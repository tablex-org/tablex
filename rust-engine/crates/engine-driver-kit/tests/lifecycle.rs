//! Full ABI lifecycle through the kit with a mock backend.

mod common;

use arrow_array::ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::{Array, Int64Array, StringArray, StructArray};
use common::*;
use engine_driver_kit::api::{CArrowPageLease, CCellText};
use engine_driver_kit::{Driver, ExportedSymbols};
use serde_json::json;

fn import(lease: &CArrowPageLease) -> StructArray {
    assert!(!lease.array.is_null() && !lease.schema.is_null());
    // SAFETY: the lease owns boxed FFI structs; `from_raw` moves the contents
    // out and leaves empty structs behind for `release` to free.
    let (array, schema) = unsafe {
        (
            FFI_ArrowArray::from_raw(lease.array.cast()),
            FFI_ArrowSchema::from_raw(lease.schema.cast()),
        )
    };
    // SAFETY: array and schema were exported together by the kit.
    let data = unsafe { from_ffi(array, &schema) }.expect("valid Arrow C data");
    StructArray::from(data)
}

fn release(lease: CArrowPageLease) {
    let release = lease.release.expect("lease has a release callback");
    // SAFETY: called once with the lease's own pointers.
    unsafe { release(lease.schema, lease.array, lease.release_context) };
}

#[test]
fn init_validates_exports_against_capabilities() {
    let ok = driver();
    assert_eq!(ok_value(take(ok.init())), json!({}));
    let mismatched = Driver::<Mock>::new(ExportedSymbols::NONE);
    assert_eq!(
        error_code(&take(mismatched.init())),
        "init.capability_mismatch"
    );
}

#[test]
fn page_lifecycle_is_zero_copy_and_scoped_to_its_connection() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page = ok_value(query(&driver, connection, 1, "rows", 5));
    let page_id = page["page_id"].as_u64().unwrap();
    assert_eq!(page["row_count"], 5);
    assert_eq!(page["total_count"], 100);
    assert_eq!(page["total_is_exact"], true);
    assert_eq!(page["columns"][1]["name"], "name");
    assert_eq!(page["columns"][1]["type"], "utf8");

    let lease = driver.acquire_arrow_page(connection, page_id, 1, 3);
    assert_eq!(
        (lease.row_count, lease.column_count, lease.generation),
        (3, 2, 1)
    );
    let rows = import(&lease);
    let ids = rows
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(ids.values().to_vec(), vec![1, 2, 3]);
    let names = rows
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(names.value(0), "row1");
    assert!(names.is_null(1));
    release(lease);

    // Clamped window, wrong connection, unknown page: empty or short leases.
    let tail = driver.acquire_arrow_page(connection, page_id, 4, 100);
    assert_eq!(tail.row_count, 1);
    release(tail);
    for lease in [
        driver.acquire_arrow_page(connection + 1, page_id, 0, 1),
        driver.acquire_arrow_page(connection, page_id + 1, 0, 1),
    ] {
        assert!(lease.array.is_null() && lease.release.is_none());
    }

    assert_eq!(ok_value(take(driver.release_page(page_id))), json!({}));
    assert_eq!(
        error_code(&take(driver.release_page(page_id))),
        "release.page_not_found"
    );
    assert!(driver
        .acquire_arrow_page(connection, page_id, 0, 1)
        .array
        .is_null());
}

#[test]
fn cell_batches_render_row_major_and_free_cleanly() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page_id = ok_value(query(&driver, connection, 1, "rows", 5))["page_id"]
        .as_u64()
        .unwrap();

    let (status, batch, error) = render(&driver, page_id, (1, 2), (0, 5));
    assert_eq!((status, &error), (0, &serde_json::Value::Null));
    // SAFETY: `batch` was just returned by render_cell_batch and is freed below.
    let (cells, text) = unsafe {
        let batch = &*batch;
        (
            std::slice::from_raw_parts(batch.cells, batch.cell_count as usize).to_vec(),
            std::slice::from_raw_parts(batch.text_buffer, batch.text_buffer_len as usize).to_vec(),
        )
    };
    let text = String::from_utf8(text).unwrap();
    let cell_text = |cell: &CCellText| {
        text[cell.offset as usize..(cell.offset + cell.length) as usize].to_owned()
    };
    assert_eq!(
        cells
            .iter()
            .map(|c| (c.row, c.column, cell_text(c)))
            .collect::<Vec<_>>(),
        vec![
            (1, 0, "1".to_owned()),
            (1, 1, "row1".to_owned()),
            (2, 0, "2".to_owned()),
            (2, 1, "NULL".to_owned()),
        ]
    );
    // The null bit: row 2's name is SQL NULL, not the text `NULL`.
    assert_eq!(
        cells.iter().map(|c| c.flags).collect::<Vec<_>>(),
        vec![0, 0, 0, engine_driver_api::CELL_FLAG_NULL]
    );
    // SAFETY: freed exactly once.
    unsafe { driver.free_cell_text_batch(batch) };

    // Empty window: null buffers, still freeable. Null is a no-op.
    let (status, empty, _) = render(&driver, page_id, (99, 5), (0, 2));
    assert_eq!(status, 0);
    // SAFETY: `empty` is a live batch; null pointers mark empty buffers.
    unsafe {
        assert_eq!((*empty).cell_count, 0);
        assert!((*empty).cells.is_null() && (*empty).text_buffer.is_null());
        driver.free_cell_text_batch(empty);
        driver.free_cell_text_batch(std::ptr::null_mut());
    }

    let (status, none, error) = render(&driver, page_id + 9, (0, 1), (0, 1));
    assert_eq!(status, 1);
    assert!(none.is_null());
    assert_eq!(error_code(&error), "render.page_not_found");
    // A null out pointer is rejected without touching anything.
    // SAFETY: null `out_batch` is the documented rejection path.
    let status = unsafe {
        driver.render_cell_batch(
            page_id,
            0,
            1,
            0,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(status, 1);
}

#[test]
fn inspect_row_reports_default_read_only_fields() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page_id = ok_value(query(&driver, connection, 1, "rows", 5))["page_id"]
        .as_u64()
        .unwrap();
    let row = ok_value(take(driver.inspect_row(page_id, 2)));
    assert_eq!(row["page_id"], page_id);
    assert_eq!(row["row_index"], 2);
    assert_eq!(row["fields"][0]["display_text"], "2");
    assert_eq!(row["fields"][1]["display_text"], "NULL");
    // SQL NULL is a JSON null; the text of a non-null cell stays a string.
    assert!(row["fields"][1]["value"].is_null());
    assert_eq!(row["fields"][0]["value"], "2");
    assert_eq!(row["fields"][1]["editable"], false);
    assert_eq!(
        error_code(&take(driver.inspect_row(page_id, 5))),
        "inspect.row_not_found"
    );
    assert_eq!(
        error_code(&take(driver.inspect_row(page_id + 1, 0))),
        "inspect.page_not_found"
    );
}

#[test]
fn disconnect_drops_the_connections_pages() {
    let driver = driver();
    let first = connect_ok(&driver);
    let second = connect_ok(&driver);
    let page_a = ok_value(query(&driver, first, 1, "rows", 2))["page_id"]
        .as_u64()
        .unwrap();
    let page_b = ok_value(query(&driver, second, 2, "rows", 2))["page_id"]
        .as_u64()
        .unwrap();
    assert_eq!(ok_value(take(driver.disconnect(first))), json!({}));
    assert!(driver
        .acquire_arrow_page(first, page_a, 0, 1)
        .array
        .is_null());
    let alive = driver.acquire_arrow_page(second, page_b, 0, 1);
    assert_eq!(alive.row_count, 1);
    release(alive);
    assert_eq!(
        error_code(&take(driver.disconnect(first))),
        "disconnect.not_found"
    );
    assert_eq!(
        error_code(&query(&driver, first, 3, "rows", 1)),
        "query.not_connected"
    );
}

#[test]
fn shutdown_clears_every_connection_and_page() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page_id = ok_value(query(&driver, connection, 1, "rows", 2))["page_id"]
        .as_u64()
        .unwrap();
    assert_eq!(ok_value(take(driver.shutdown())), json!({}));
    assert!(driver
        .acquire_arrow_page(connection, page_id, 0, 1)
        .array
        .is_null());
    assert_eq!(
        error_code(&query(&driver, connection, 2, "rows", 1)),
        "query.not_connected"
    );
}

#[test]
fn connect_and_request_errors_keep_their_codes() {
    let driver = driver();
    assert_eq!(
        error_code(&connect(&driver, "mock://fail")),
        "connect.bad_request"
    );
    // SAFETY: null and non-JSON requests are the documented bad-request paths.
    unsafe {
        assert_eq!(
            error_code(&take(driver.connect(std::ptr::null()))),
            "connect.bad_request"
        );
        let bad = std::ffi::CString::new("{").unwrap();
        assert_eq!(
            error_code(&take(driver.open_query(1, bad.as_ptr()))),
            "query.bad_request"
        );
    }
    let connection = connect_ok(&driver);
    let request = cstring(
        &json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 10}),
    );
    // SAFETY: `request` is a live NUL-terminated string.
    let nav = ok_value(take(unsafe {
        driver.list_nav_nodes(connection, request.as_ptr())
    }));
    assert_eq!(nav["next_cursor"], "next");
    // SAFETY: as above.
    let missing = take(unsafe { driver.list_nav_nodes(connection + 7, request.as_ptr()) });
    assert_eq!(error_code(&missing), "navigator.not_connected");
}

#[test]
fn mutate_invalidates_pages_unless_the_backend_keeps_them() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let other = connect_ok(&driver);
    let page = |driver: &Driver<Mock>, connection, op| {
        ok_value(query(driver, connection, op, "rows", 2))["page_id"]
            .as_u64()
            .unwrap()
    };
    let (a, b, c) = (
        page(&driver, connection, 1),
        page(&driver, connection, 2),
        page(&driver, other, 3),
    );
    let mutate = |kind: &str, op: u64| {
        let request = cstring(&json!({
            "operation_id": op, "kind": kind, "database": null, "object": "t",
            "identity": null, "payload": null,
        }));
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { driver.mutate(connection, request.as_ptr()) })
    };
    let kept = ok_value(mutate("keep", 10));
    assert_eq!(
        (
            kept["affected_rows"].clone(),
            kept["invalidated_page_ids"].clone()
        ),
        (json!(1), json!([]))
    );
    let result = ok_value(mutate("insert", 11));
    assert_eq!(result["affected_rows"], 2);
    assert_eq!(result["invalidated_page_ids"], json!([a, b]));
    assert!(driver
        .acquire_arrow_page(connection, a, 0, 1)
        .array
        .is_null());
    let survivor = driver.acquire_arrow_page(other, c, 0, 1);
    assert_eq!(survivor.row_count, 1);
    release(survivor);
}

#[test]
fn export_and_describe_hooks_receive_page_and_json() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page_id = ok_value(query(&driver, connection, 1, "rows", 4))["page_id"]
        .as_u64()
        .unwrap();
    let export = |page: serde_json::Value| {
        let request = cstring(&json!({
            "operation_id": 20, "format": "json", "destination": "/dev/null",
            "page_id": page, "query": null, "options": null,
        }));
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { driver.export(connection, request.as_ptr()) })
    };
    let result = ok_value(export(json!(page_id)));
    assert_eq!(
        (
            result["rows_written"].clone(),
            result["bytes_written"].clone()
        ),
        (json!(4), json!("orders".len()))
    );
    assert_eq!(
        error_code(&export(json!(page_id + 50))),
        "export.page_not_found"
    );
    assert_eq!(
        error_code(&export(serde_json::Value::Null)),
        "export.failed"
    );

    let request = cstring(&json!({"object": "orders"}));
    // SAFETY: `request` is a live NUL-terminated string.
    let described = take(unsafe { driver.describe_object(connection, request.as_ptr()) });
    assert_eq!(ok_value(described), json!({"described": "orders"}));
}

fn cancel(driver: &Driver<Mock>, connection: u64, operation_id: u64) -> serde_json::Value {
    let request = cstring(&json!({ "operation_id": operation_id }));
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { driver.cancel_operation(connection, request.as_ptr()) })
}

#[test]
fn cancel_interrupts_the_active_operation() {
    let driver = driver();
    let connection = connect_ok(&driver);
    std::thread::scope(|scope| {
        let running = scope.spawn(|| query(&driver, connection, 77, "spin", 0));
        wait_until_spinning(77);
        assert_eq!(
            ok_value(cancel(&driver, connection, 77)),
            json!({"cancelled": true})
        );
        let envelope = running.join().unwrap();
        assert_eq!(error_code(&envelope), "query.cancelled");
    });
    // The connection still works afterwards.
    assert_eq!(
        ok_value(query(&driver, connection, 78, "rows", 1))["row_count"],
        1
    );
}

#[test]
fn early_cancel_starts_the_operation_cancelled() {
    let driver = driver();
    let connection = connect_ok(&driver);
    // The host may cancel right after open, before the operation registered.
    assert_eq!(
        ok_value(cancel(&driver, connection, 90)),
        json!({"cancelled": false})
    );
    let envelope = query(&driver, connection, 90, "rows", 1);
    assert_eq!(error_code(&envelope), "query.cancelled");
    assert_eq!(envelope["error"]["message"], "operation cancelled");
    // Consumed once: the id can run afterwards.
    assert_eq!(
        ok_value(query(&driver, connection, 90, "rows", 1))["row_count"],
        1
    );
}

#[test]
fn blocking_canceller_does_not_block_the_connection() {
    let driver = driver();
    let connection = ok_value(connect(&driver, "mock://blocking-cancel"))["connection_id"]
        .as_u64()
        .unwrap();
    CANCEL_GATE.store(false, std::sync::atomic::Ordering::SeqCst);
    std::thread::scope(|scope| {
        let running = scope.spawn(|| query(&driver, connection, 100, "spin", 0));
        wait_until_spinning(100);
        let cancelling = scope.spawn(|| cancel(&driver, connection, 100));
        // The canceller interrupts the statement, then blocks (a network
        // cancel). The query still ends and the connection is usable at once.
        assert_eq!(error_code(&running.join().unwrap()), "query.cancelled");
        assert_eq!(
            ok_value(query(&driver, connection, 101, "rows", 1))["row_count"],
            1
        );
        assert_eq!(
            ok_value(cancel(&driver, connection, 999))["cancelled"],
            false
        );
        assert!(
            !cancelling.is_finished(),
            "the canceller should still be blocked"
        );
        CANCEL_GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            ok_value(cancelling.join().unwrap()),
            json!({"cancelled": true})
        );
    });
}

#[test]
fn cancel_after_the_operation_finished_does_not_hit_the_next_one() {
    let driver = driver();
    let connection = connect_ok(&driver);
    assert_eq!(
        ok_value(query(&driver, connection, 110, "rows", 1))["row_count"],
        1
    );
    std::thread::scope(|scope| {
        let running = scope.spawn(|| query(&driver, connection, 111, "spin", 0));
        wait_until_spinning(111);
        // Operation 110 is over: this must not interrupt operation 111.
        assert_eq!(
            ok_value(cancel(&driver, connection, 110)),
            json!({"cancelled": false})
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !running.is_finished(),
            "a stale cancel interrupted the next operation"
        );
        assert_eq!(
            ok_value(cancel(&driver, connection, 111)),
            json!({"cancelled": true})
        );
        assert_eq!(error_code(&running.join().unwrap()), "query.cancelled");
    });
}

#[test]
fn disconnect_cancels_the_running_query() {
    let driver = driver();
    let connection = connect_ok(&driver);
    std::thread::scope(|scope| {
        let running = scope.spawn(|| query(&driver, connection, 120, "spin", 0));
        wait_until_spinning(120);
        assert_eq!(ok_value(take(driver.disconnect(connection))), json!({}));
        assert_eq!(error_code(&running.join().unwrap()), "query.cancelled");
    });
}

#[test]
fn concurrent_open_query_on_one_connection_never_sees_an_active_operation_error() {
    let driver = driver();
    let connection = connect_ok(&driver);
    std::thread::scope(|scope| {
        for thread in 0..2_u64 {
            let driver = &driver;
            scope.spawn(move || {
                for i in 0..300_u64 {
                    let operation_id = 10_000 * (thread + 1) + i;
                    let envelope = query(driver, connection, operation_id, "rows", 2);
                    assert_eq!(envelope["ok"], true, "thread {thread} op {i}: {envelope}");
                    let page_id = envelope["value"]["page_id"].as_u64().unwrap();
                    take(driver.release_page(page_id));
                }
            });
        }
    });
}

#[test]
fn unrequested_interrupts_are_reported_as_retryable_and_requested_ones_are_not() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let envelope = query(&driver, connection, 140, "server_kill", 0);
    assert_eq!(error_code(&envelope), "query.interrupted");
    assert_eq!(envelope["error"]["retryable"], true);
}

#[test]
fn an_operation_waiting_for_the_connection_fails_once_disconnect_began() {
    let driver = driver();
    let connection = connect_ok(&driver);
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| query(&driver, connection, 130, "gate", 0));
        wait_until_gated(130); // registered, not yet activated
        assert_eq!(ok_value(take(driver.disconnect(connection))), json!({}));
        release_gate(130);
        let envelope = waiting.join().unwrap();
        assert_eq!(error_code(&envelope), "query.not_connected");
        assert_eq!(envelope["error"]["message"], "connection closed");
    });
}

#[test]
fn queries_racing_a_disconnect_complete_before_it_or_fail_never_run_after_it() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let disconnected = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..3_u64)
            .map(|thread| {
                let (driver, disconnected) = (&driver, &disconnected);
                scope.spawn(move || {
                    for i in 0.. {
                        let late = disconnected.load(std::sync::atomic::Ordering::SeqCst);
                        let operation_id = 1_000_000 * (thread + 1) + i;
                        let envelope = query_with(
                            driver,
                            connection,
                            operation_id,
                            1,
                            json!({"mode": "rows", "late": late}),
                        );
                        if envelope["ok"] != true {
                            let code = error_code(&envelope);
                            assert!(code == "query.not_connected", "{envelope}");
                            return;
                        }
                    }
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(ok_value(take(driver.disconnect(connection))), json!({}));
        disconnected.store(true, std::sync::atomic::Ordering::SeqCst);
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert_eq!(LATE_RUNS.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn a_cancel_for_a_finished_id_does_not_poison_its_reuse() {
    let driver = driver();
    let connection = connect_ok(&driver);
    assert_eq!(
        ok_value(query(&driver, connection, 150, "rows", 1))["row_count"],
        1
    );
    assert_eq!(
        ok_value(cancel(&driver, connection, 150)),
        json!({"cancelled": false})
    );
    assert_eq!(
        ok_value(query(&driver, connection, 150, "rows", 1))["row_count"],
        1
    );
}

#[test]
fn statements_without_columns_produce_an_empty_page() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page = ok_value(query(&driver, connection, 1, "ddl", 0));
    let page_id = page["page_id"].as_u64().unwrap();
    assert_eq!(page["columns"], json!([]));
    assert_eq!(
        (page["row_count"].clone(), page["total_count"].clone()),
        (json!(0), json!(3))
    );
    let lease = driver.acquire_arrow_page(connection, page_id, 0, u64::MAX);
    assert_eq!((lease.row_count, lease.column_count), (0, 0));
    assert!(!lease.array.is_null());
    release(lease);
    let (status, batch, _) = render(&driver, page_id, (0, 10), (0, 10));
    assert_eq!(status, 0);
    // SAFETY: `batch` is a live batch freed once.
    unsafe {
        assert_eq!((*batch).cell_count, 0);
        driver.free_cell_text_batch(batch);
    }
    assert_eq!(
        error_code(&take(driver.inspect_row(page_id, 0))),
        "inspect.row_not_found"
    );
}

#[test]
fn leases_outlive_the_page_that_produced_them() {
    for removal in ["release_page", "disconnect", "shutdown"] {
        let driver = driver();
        let connection = connect_ok(&driver);
        let page_id = ok_value(query(&driver, connection, 1, "rows", 4))["page_id"]
            .as_u64()
            .unwrap();
        let lease = driver.acquire_arrow_page(connection, page_id, 0, 4);
        match removal {
            "release_page" => drop(take(driver.release_page(page_id))),
            "disconnect" => drop(take(driver.disconnect(connection))),
            _ => drop(take(driver.shutdown())),
        }
        // The page is gone, but the lease still owns its buffers.
        let rows = import(&lease);
        let ids = rows
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(ids.values().to_vec(), vec![0, 1, 2, 3], "{removal}");
        release(lease);
        assert!(
            driver
                .acquire_arrow_page(connection, page_id, 0, 1)
                .array
                .is_null(),
            "{removal}"
        );
    }
}

#[test]
fn u64_max_ranges_are_clamped() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page_id = ok_value(query(&driver, connection, 1, "rows", 5))["page_id"]
        .as_u64()
        .unwrap();
    let lease = driver.acquire_arrow_page(connection, page_id, u64::MAX, u64::MAX);
    assert_eq!(lease.row_count, 0);
    release(lease);
    let lease = driver.acquire_arrow_page(connection, page_id, 3, u64::MAX);
    assert_eq!(lease.row_count, 2);
    release(lease);
    let lease = driver.acquire_arrow_page(connection, page_id, 0, u64::MAX);
    assert_eq!((lease.row_count, lease.column_count), (5, 2));
    release(lease);
    for (rows, columns, expected) in [
        ((0, u64::MAX), (0, u64::MAX), 10),
        ((u64::MAX, u64::MAX), (0, 1), 0),
        ((0, 1), (u64::MAX, u64::MAX), 0),
        ((4, u64::MAX), (1, u64::MAX), 1),
    ] {
        let (status, batch, error) = render(&driver, page_id, rows, columns);
        assert_eq!(status, 0, "{error}");
        // SAFETY: `batch` is a live batch freed once.
        unsafe {
            assert_eq!((*batch).cell_count, expected);
            driver.free_cell_text_batch(batch);
        }
    }
}

#[test]
fn macro_exported_symbols_serve_a_session() {
    assert_eq!(dbengine_abi_version(), 2);
    let caps = take(dbengine_capabilities());
    assert_eq!(caps["driver"], "mock");
    assert_eq!(ok_value(take(dbengine_init())), json!({}));
    let request = cstring(&json!({"uri": "mock://ok"}));
    // SAFETY: live NUL-terminated request strings throughout.
    unsafe {
        let id = ok_value(take_exported(dbengine_connect(request.as_ptr())))["connection_id"]
            .as_u64()
            .unwrap();
        let query = cstring(&json!({
            "kind": "sql", "operation_id": 1, "database": null, "object": null,
            "offset": 0, "limit": 3, "payload": {"mode": "rows"},
        }));
        let page = ok_value(take_exported(dbengine_open_query(id, query.as_ptr())));
        let page_id = page["page_id"].as_u64().unwrap();
        let table = &*dbengine_arrow_function_table();
        assert_eq!(table.abi_version, 1);
        let lease = (table.acquire_page.unwrap())(id, page_id, 0, 3);
        assert_eq!(lease.row_count, 3);
        (lease.release.unwrap())(lease.schema, lease.array, lease.release_context);
        let lease = dbengine_acquire_arrow_page(id, page_id, 0, 1);
        assert_eq!(lease.row_count, 1);
        (lease.release.unwrap())(lease.schema, lease.array, lease.release_context);
        assert_eq!(
            ok_value(take_exported(dbengine_inspect_row(page_id, 0)))["row_index"],
            0
        );
        assert_eq!(
            ok_value(take_exported(dbengine_release_page(page_id))),
            json!({})
        );
        assert_eq!(ok_value(take_exported(dbengine_disconnect(id))), json!({}));
    }
}

/// Like `take`, but frees through the macro-exported `dbengine_free_string`.
fn take_exported(ptr: *mut std::ffi::c_char) -> serde_json::Value {
    assert!(!ptr.is_null());
    // SAFETY: `ptr` is a NUL-terminated string an exported entry point returned.
    let text = unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_str()
        .unwrap()
        .to_owned();
    // SAFETY: `ptr` came from this driver's string allocator and is freed once.
    unsafe { dbengine_free_string(ptr) };
    serde_json::from_str(&text).unwrap()
}
