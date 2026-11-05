//! A panicking backend must produce an error, never unwind into the host
//! (which would abort the process). Every case here panics inside a backend
//! method, so an escaping unwind would fail (abort) this test binary.

mod common;

use common::*;
use serde_json::json;

fn request(value: serde_json::Value) -> std::ffi::CString {
    cstring(&value)
}

#[test]
fn panicking_connect_returns_an_error_envelope() {
    let driver = driver();
    assert_eq!(
        error_code(&connect(&driver, "mock://panic")),
        "connect.internal"
    );
    // The driver is still usable afterwards.
    assert!(connect_ok(&driver) > 0);
}

#[test]
fn panicking_navigator_returns_an_error_and_keeps_the_connection() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let panicking = request(
        json!({"parent": "panic", "database": null, "payload": null, "cursor": null, "limit": 1}),
    );
    // SAFETY: `panicking` is a live NUL-terminated string.
    let envelope = take(unsafe { driver.list_nav_nodes(connection, panicking.as_ptr()) });
    assert_eq!(error_code(&envelope), "navigator.internal");
    assert_eq!(
        ok_value(query(&driver, connection, 1, "rows", 1))["row_count"],
        1
    );
}

#[test]
fn panicking_query_releases_its_operation_and_leaves_no_page() {
    let driver = driver();
    let connection = connect_ok(&driver);
    for mode in ["panic_before_activate", "panic_active"] {
        assert_eq!(
            error_code(&query(&driver, connection, 5, mode, 0)),
            "query.internal"
        );
        // The unwind dropped the operation guard: the same id registers again.
        let page = ok_value(query(&driver, connection, 5, "rows", 2));
        assert_eq!(page["row_count"], 2);
        let page_id = page["page_id"].as_u64().unwrap();
        assert_eq!(ok_value(take(driver.release_page(page_id))), json!({}));
    }
}

#[test]
fn panicking_cell_renderer_reports_through_out_error() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let page_id = ok_value(query(&driver, connection, 1, "boom", 3))["page_id"]
        .as_u64()
        .unwrap();
    let (status, batch, error) = render(&driver, page_id, (0, 3), (0, 2));
    assert_eq!(status, 1);
    assert!(batch.is_null());
    assert_eq!(error_code(&error), "render.internal");
    // Rows before the panicking cell still render, and the page is intact.
    let (status, batch, _) = render(&driver, page_id, (0, 1), (0, 2));
    assert_eq!(status, 0);
    // SAFETY: freed exactly once.
    unsafe { driver.free_cell_text_batch(batch) };
    // A null `out_error` on the panic path must not be dereferenced.
    // SAFETY: `out_batch` is a writable local; null `out_error` is allowed.
    let status = unsafe {
        let mut batch = std::ptr::null_mut();
        driver.render_cell_batch(page_id, 0, 3, 0, 2, &mut batch, std::ptr::null_mut())
    };
    assert_eq!(status, 1);
}

#[test]
fn panicking_mutation_returns_an_error_and_releases_the_operation() {
    let driver = driver();
    let connection = connect_ok(&driver);
    let panicking = request(json!({
        "operation_id": 9, "kind": "panic", "database": null, "object": null,
        "identity": null, "payload": null,
    }));
    // SAFETY: `panicking` is a live NUL-terminated string.
    let envelope = take(unsafe { driver.mutate(connection, panicking.as_ptr()) });
    assert_eq!(error_code(&envelope), "mutation.internal");
    let fine = request(json!({
        "operation_id": 9, "kind": "insert", "database": null, "object": null,
        "identity": null, "payload": null,
    }));
    // SAFETY: as above.
    let envelope = take(unsafe { driver.mutate(connection, fine.as_ptr()) });
    assert_eq!(ok_value(envelope)["affected_rows"], 2);
}

#[test]
fn panicking_interrupt_does_not_wedge_the_connection() {
    let driver = driver();
    let connection = ok_value(connect(&driver, "mock://interrupt-panics"))["connection_id"]
        .as_u64()
        .unwrap();
    let cancel = |op: u64| {
        let cancel = request(json!({ "operation_id": op }));
        // SAFETY: `cancel` is a live NUL-terminated string.
        take(unsafe { driver.cancel_operation(connection, cancel.as_ptr()) })
    };
    std::thread::scope(|scope| {
        let running = scope.spawn(|| query(&driver, connection, 3, "spin", 0));
        wait_until_spinning(3);
        // The canceller panics (after setting the interrupt flag): error
        // envelope, no unwind into the host.
        assert_eq!(error_code(&cancel(3)), "operation.internal");
        assert_eq!(error_code(&running.join().unwrap()), "query.cancelled");
    });
    // No kit lock was held across the panic: the connection works and cancel
    // still answers.
    assert_eq!(
        ok_value(query(&driver, connection, 4, "rows", 1))["row_count"],
        1
    );
    assert_eq!(ok_value(cancel(1234)), json!({"cancelled": false}));
}

#[test]
fn macro_exported_entry_points_are_guarded_too() {
    let request = request(json!({"uri": "mock://panic"}));
    // SAFETY: `request` is a live NUL-terminated string.
    let envelope = take(unsafe { dbengine_connect(request.as_ptr()) });
    assert_eq!(error_code(&envelope), "connect.internal");
}
