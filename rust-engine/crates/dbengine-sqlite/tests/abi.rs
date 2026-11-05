//! End-to-end checks of the SQLite driver through its exported C symbols.

use dbengine_sqlite::*;
use serde_json::{json, Value};
use std::ffi::{c_char, CStr, CString};

fn take(ptr: *mut c_char) -> Value {
    assert!(!ptr.is_null());
    // SAFETY: `ptr` is a NUL-terminated string the driver just returned.
    let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
    // SAFETY: `ptr` came from this driver's allocator and is freed once.
    unsafe { dbengine_free_string(ptr) };
    serde_json::from_str(&text).unwrap()
}

fn connect() -> u64 {
    let request = CString::new(json!({"uri": "sqlite://:memory:"}).to_string()).unwrap();
    // SAFETY: `request` is a live NUL-terminated string.
    let envelope = take(unsafe { dbengine_connect(request.as_ptr()) });
    assert_eq!(envelope["ok"], true, "{envelope}");
    envelope["value"]["connection_id"].as_u64().unwrap()
}

fn query(connection: u64, operation_id: u64, sql: &str) -> Value {
    let request = CString::new(
        json!({
            "kind": "sql", "operation_id": operation_id, "database": null, "object": null,
            "offset": 0, "limit": 0, "payload": {"sql": sql},
        })
        .to_string(),
    )
    .unwrap();
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { dbengine_open_query(connection, request.as_ptr()) })
}

#[test]
fn concurrent_queries_on_one_connection_never_see_an_active_operation_error() {
    let connection = connect();
    assert_eq!(query(connection, 1, "CREATE TABLE t(x)")["ok"], true);
    std::thread::scope(|scope| {
        for thread in 0..4_u64 {
            scope.spawn(move || {
                for i in 0..200_u64 {
                    let operation_id = 1000 + thread * 10_000 + i;
                    let envelope = query(connection, operation_id, "SELECT 1 AS one");
                    assert_eq!(envelope["ok"], true, "thread {thread} op {i}: {envelope}");
                    let page_id = envelope["value"]["page_id"].as_u64().unwrap();
                    take(dbengine_release_page(page_id));
                }
            });
        }
    });
}

#[test]
fn ddl_and_dml_through_open_query_succeed() {
    let connection = connect();
    let ddl = query(connection, 1, "CREATE TABLE t(x INTEGER)");
    assert_eq!(ddl["ok"], true, "{ddl}");
    assert_eq!(ddl["value"]["columns"], json!([]));
    assert_eq!(ddl["value"]["row_count"], 0);
    let insert = query(connection, 2, "INSERT INTO t VALUES (1), (2), (3)");
    assert_eq!(insert["ok"], true, "{insert}");
    assert_eq!(insert["value"]["total_count"], 3);
    assert_eq!(insert["value"]["total_is_exact"], true);
    let select = query(connection, 3, "SELECT x FROM t ORDER BY x");
    assert_eq!(select["value"]["row_count"], 3);
    // The empty page can be leased and released like any other.
    let page_id = ddl["value"]["page_id"].as_u64().unwrap();
    let lease = dbengine_acquire_arrow_page(connection, page_id, 0, 10);
    assert_eq!((lease.row_count, lease.column_count), (0, 0));
    // SAFETY: the lease's own release callback, called once with its pointers.
    unsafe { (lease.release.unwrap())(lease.schema, lease.array, lease.release_context) };
}

#[test]
fn cancel_interrupts_a_running_statement_and_the_connection_recovers() {
    let connection = connect();
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4000000000) SELECT count(*) FROM c";
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| query(connection, 50, sql));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let request = CString::new(json!({"operation_id": 50}).to_string()).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        let cancelled = take(unsafe { dbengine_cancel_operation(connection, request.as_ptr()) });
        assert_eq!(cancelled["value"]["cancelled"], true, "{cancelled}");
        let envelope = running.join().unwrap();
        assert_eq!(envelope["ok"], false);
        // A requested cancel is `query.cancelled` and not retryable.
        assert_eq!(envelope["error"]["code"], "query.cancelled", "{envelope}");
        assert_eq!(envelope["error"]["retryable"], false);
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("interrupt"),
            "{envelope}"
        );
    });
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(query(connection, 51, "SELECT 1")["ok"], true);
}

/// A cancel that lands after the operation became active but before its statement runs (the
/// `read_only` pragmas come first) must still stop it: `sqlite3_interrupt` alone is a no-op while
/// no statement is running. The moment of the cancel is swept across the start of the operation,
/// so every phase (not registered yet, pending, active before the statement, running) is hit.
#[test]
fn a_cancel_at_any_moment_of_an_operation_stops_it() {
    let connection = connect();
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4000000000) SELECT count(*) FROM c";
    let cancel = |operation_id: u64| {
        let request = CString::new(json!({"operation_id": operation_id}).to_string()).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { dbengine_cancel_operation(connection, request.as_ptr()) })
    };
    let mut lost = Vec::new();
    for round in 0..400_u64 {
        let operation_id = 1000 + round;
        std::thread::scope(|scope| {
            let running = scope.spawn(move || {
                query_with(
                    connection,
                    operation_id,
                    sql,
                    0,
                    0,
                    json!({"read_only": true}),
                )
            });
            // Sweep the moment of the cancel across the start of the operation (0 .. 2 ms).
            let wait = std::time::Duration::from_micros(round * 5);
            let sent = std::time::Instant::now();
            while sent.elapsed() < wait {
                std::hint::spin_loop();
            }
            // Cancel until the driver says it knew the operation (pending or active), or the
            // operation ended because an earlier cancel was remembered for it.
            while !running.is_finished() && cancel(operation_id)["value"]["cancelled"] != true {
                std::hint::spin_loop();
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !running.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if !running.is_finished() {
                lost.push(round);
                // Stop the runaway statement so the test can report.
                while !running.is_finished() {
                    cancel(operation_id);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            let envelope = running.join().unwrap();
            assert_eq!(envelope["error"]["code"], "query.cancelled", "{envelope}");
        });
    }
    assert!(
        lost.is_empty(),
        "cancels that did not stop the statement: rounds {lost:?}"
    );
    // The connection is as it was: readable, and writable (the `read_only` pragma was restored
    // after every cancelled round).
    assert_eq!(query(connection, 1, "SELECT 1")["ok"], true);
    let create = query(connection, 2, "CREATE TABLE after_sweep(x)");
    assert_eq!(create["ok"], true, "{create}");
}

/// A script of many statements, each too short for the progress handler to ever look: a cancel
/// between two of them stops the script before the next one starts.
#[test]
fn a_cancel_between_the_statements_of_a_script_stops_it() {
    let connection = connect();
    assert_eq!(query(connection, 1, "CREATE TABLE s(x)")["ok"], true);
    let script = "INSERT INTO s VALUES (1);".repeat(200_000);
    let cancel = |operation_id: u64| {
        let request = CString::new(json!({"operation_id": operation_id}).to_string()).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { dbengine_cancel_operation(connection, request.as_ptr()) })
    };
    for round in 0..40_u64 {
        let operation_id = 100 + round;
        let script = script.as_str();
        std::thread::scope(|scope| {
            let running = scope.spawn(move || query(connection, operation_id, script));
            // Past the start of the operation, well before 200 000 statements can finish.
            std::thread::sleep(std::time::Duration::from_micros(2_000 + round * 250));
            while !running.is_finished() && cancel(operation_id)["value"]["cancelled"] != true {
                std::hint::spin_loop();
            }
            let envelope = running.join().unwrap();
            assert_eq!(
                envelope["error"]["code"], "query.cancelled",
                "round {round}: {envelope}"
            );
        });
    }
    let count = query(connection, 2, "SELECT count(*) FROM s");
    assert_eq!(count["ok"], true, "{count}");
}

fn query_with(
    connection: u64,
    operation_id: u64,
    sql: &str,
    offset: u64,
    limit: u64,
    payload: Value,
) -> Value {
    let mut payload = payload;
    payload["sql"] = json!(sql);
    let request = CString::new(
        json!({
            "kind": "sql", "operation_id": operation_id, "database": null, "object": null,
            "offset": offset, "limit": limit, "payload": payload,
        })
        .to_string(),
    )
    .unwrap();
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { dbengine_open_query(connection, request.as_ptr()) })
}

#[test]
fn paging_confirmation_and_not_pageable_rules() {
    let connection = connect();
    assert_eq!(
        query(connection, 1, "CREATE TABLE t(x INTEGER)")["ok"],
        true
    );
    for (id, offset, limit, rows, truncated) in [
        (2, 0, 2, 2, true),
        (3, 0, 3, 3, false),
        (4, 2, 5, 1, false),
        (5, 3, 5, 0, false),
    ] {
        let sql = "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 3) SELECT n FROM s";
        let page = query_with(connection, id, sql, offset, limit, json!({}));
        assert_eq!(
            page["value"]["row_count"], rows,
            "offset {offset} limit {limit}: {page}"
        );
        assert_eq!(page["value"]["flags"]["truncated"], truncated, "{page}");
        assert_eq!(page["value"]["total_count"], Value::Null);
        assert_eq!(page["value"]["total_is_exact"], false);
    }
    // A write runs once; paging it is refused and never re-runs it.
    let insert = query_with(connection, 10, "INSERT INTO t VALUES (1)", 0, 10, json!({}));
    assert_eq!(insert["ok"], true, "{insert}");
    assert_eq!(insert["value"]["flags"]["truncated"], false);
    let paged = query_with(connection, 11, "INSERT INTO t VALUES (2)", 5, 10, json!({}));
    assert_eq!(paged["error"]["code"], "query.not_pageable", "{paged}");
    let count = query(connection, 12, "SELECT COUNT(*) FROM t");
    assert_eq!(count["value"]["row_count"], 1);
    // Dangerous statements need confirmation, and do not run without it.
    let refused = query_with(connection, 13, "DELETE FROM t", 0, 10, json!({}));
    assert_eq!(
        refused["error"]["code"], "query.confirmation_required",
        "{refused}"
    );
    assert!(!refused["error"]["message"].as_str().unwrap().is_empty());
    let still = query(connection, 14, "SELECT x FROM t");
    assert_eq!(still["value"]["row_count"], 1);
    let confirmed = query_with(
        connection,
        15,
        "DELETE FROM t",
        0,
        10,
        json!({"confirmed": true}),
    );
    assert_eq!(confirmed["ok"], true, "{confirmed}");
    assert_eq!(confirmed["value"]["total_count"], 1);
    // limit 0 is the default page (200 rows), and the page is cut with a lookahead.
    let sql = "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 250) SELECT n FROM s";
    let page = query_with(connection, 16, sql, 0, 0, json!({}));
    assert_eq!(page["value"]["row_count"], 200, "{page}");
    assert_eq!(page["value"]["flags"]["truncated"], true);
    // A script returns the rows of its last statement.
    let script = query(
        connection,
        17,
        "CREATE TABLE u(y); INSERT INTO u VALUES (7); SELECT y FROM u",
    );
    assert_eq!(script["value"]["row_count"], 1, "{script}");
    // A script that creates a trigger runs as one unit.
    let trigger = query(
        connection,
        18,
        "CREATE TRIGGER u_ins AFTER INSERT ON u BEGIN UPDATE u SET y = 0 WHERE 0; END",
    );
    assert_eq!(trigger["ok"], true, "{trigger}");
}

#[test]
fn mutations_and_describe_through_the_exported_symbols() {
    let connection = connect();
    assert_eq!(
        query(
            connection,
            1,
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)"
        )["ok"],
        true
    );
    let mutate = |operation_id: u64, kind: &str, payload: Value| {
        let request = CString::new(
            json!({
                "operation_id": operation_id, "kind": kind, "database": null, "object": "t",
                "identity": {}, "payload": payload,
            })
            .to_string(),
        )
        .unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { dbengine_mutate(connection, request.as_ptr()) })
    };
    let inserted = mutate(
        2,
        "insert",
        json!({"op": "insert", "values": {"id": 1, "name": "a"}}),
    );
    assert_eq!(inserted["value"]["affected_rows"], 1, "{inserted}");
    let missing = mutate(
        3,
        "update",
        json!({"op": "update", "key": {"id": 9}, "values": {"name": "z"}}),
    );
    assert_eq!(
        missing["error"]["code"], "mutation.no_rows_affected",
        "{missing}"
    );
    let unknown = mutate(4, "frobnicate", json!({}));
    assert_eq!(
        unknown["error"]["code"], "mutation.unsupported",
        "{unknown}"
    );
    let duplicate = mutate(5, "insert", json!({"op": "insert", "values": {"id": 1}}));
    assert_eq!(duplicate["error"]["code"], "mutation.failed", "{duplicate}");

    let describe = CString::new(json!({"database": "main", "object": "t"}).to_string()).unwrap();
    // SAFETY: `describe` is a live NUL-terminated string.
    let described = take(unsafe { dbengine_describe_object(connection, describe.as_ptr()) });
    assert_eq!(described["ok"], true, "{described}");
    assert_eq!(described["value"]["columns"][0]["name"], "id");
    assert_eq!(described["value"]["columns"][0]["primary_key"], true);
    assert_eq!(described["value"]["keys"][0]["columns"], json!(["id"]));
    let unknown = CString::new(json!({"object": "nope"}).to_string()).unwrap();
    // SAFETY: `unknown` is a live NUL-terminated string.
    let unknown = take(unsafe { dbengine_describe_object(connection, unknown.as_ptr()) });
    assert_eq!(unknown["error"]["code"], "describe.not_found", "{unknown}");
}

#[test]
fn bracket_identifiers_do_not_hide_a_dangerous_statement() {
    let connection = connect();
    assert_eq!(
        query(connection, 1, "CREATE TABLE t([x'] INTEGER)")["ok"],
        true
    );
    assert_eq!(
        query(connection, 2, "INSERT INTO t VALUES (1), (2)")["ok"],
        true
    );
    // The `'` inside `[x']` opens no string: the DELETE after it is seen.
    let sql = "SELECT [x'] FROM t; DELETE FROM t";
    let refused = query_with(connection, 3, sql, 0, 10, json!({}));
    assert_eq!(
        refused["error"]["code"], "query.confirmation_required",
        "{refused}"
    );
    // Nothing of the script ran (not even the leading SELECT's effects), the rows are intact.
    let count = query(connection, 4, "SELECT COUNT(*) FROM t");
    assert_eq!(count["value"]["row_count"], 1);
    let rows = query(connection, 5, "SELECT [x'] FROM t");
    assert_eq!(rows["value"]["row_count"], 2, "{rows}");
    let confirmed = query_with(connection, 6, sql, 0, 10, json!({"confirmed": true}));
    assert_eq!(confirmed["ok"], true, "{confirmed}");
    let left = query(connection, 7, "SELECT [x'] FROM t");
    assert_eq!(left["value"]["row_count"], 0, "{left}");
}

#[test]
fn scripts_return_the_last_result_set_and_run_every_statement_once() {
    let connection = connect();
    assert_eq!(
        query(connection, 1, "CREATE TABLE t(x INTEGER)")["ok"],
        true
    );
    // A trailing write leaves the earlier SELECT's columns.
    let page = query(connection, 2, "SELECT 1 AS a; INSERT INTO t VALUES (1)");
    assert_eq!(page["value"]["columns"][0]["name"], "a", "{page}");
    assert_eq!(page["value"]["row_count"], 1);
    // The last of several result sets wins.
    let page = query(
        connection,
        3,
        "SELECT 1 AS a; SELECT 2 AS b, 3 AS c; INSERT INTO t VALUES (2)",
    );
    assert_eq!(
        page["value"]["columns"].as_array().unwrap().len(),
        2,
        "{page}"
    );
    assert_eq!(page["value"]["columns"][0]["name"], "b");
    // Writes only: a zero-column page whose total is the rows written.
    let page = query(
        connection,
        4,
        "INSERT INTO t VALUES (3); INSERT INTO t VALUES (4)",
    );
    assert_eq!(page["value"]["columns"], json!([]), "{page}");
    assert_eq!(page["value"]["total_count"], 2);
    // RETURNING is a result set too, and its write ran once.
    let page = query(
        connection,
        5,
        "INSERT INTO t VALUES (5) RETURNING x; INSERT INTO t VALUES (6)",
    );
    assert_eq!(page["value"]["columns"][0]["name"], "x", "{page}");
    let count = query(connection, 6, "SELECT COUNT(*) AS n FROM t");
    assert_eq!(count["value"]["row_count"], 1);
    let rows = query(connection, 7, "SELECT x FROM t ORDER BY x");
    assert_eq!(
        rows["value"]["row_count"], 6,
        "each statement ran exactly once: {rows}"
    );
    // A trigger body (with `;` inside) survives inside a script and fires.
    let page = query(
        connection,
        8,
        "CREATE TABLE log(v); \
         CREATE TRIGGER t_ins AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.x); INSERT INTO log VALUES (-1); END; \
         INSERT INTO t VALUES (7); SELECT COUNT(*) AS logged FROM log",
    );
    assert_eq!(page["ok"], true, "{page}");
    assert_eq!(page["value"]["columns"][0]["name"], "logged");
    let logged = {
        let page_id = page["value"]["page_id"].as_u64().unwrap();
        take(dbengine_release_page(page_id));
        query(connection, 9, "SELECT COUNT(*) FROM log")
    };
    assert_eq!(logged["value"]["row_count"], 1);
}

#[test]
fn run_to_end_steps_every_row_of_a_read_and_keeps_only_the_page() {
    let capabilities = take(dbengine_capabilities());
    assert!(capabilities["features"].as_array().unwrap().contains(&json!("query.run_to_end")), "{capabilities}");
    let connection = connect();
    // Row 1000 raises an error (`abs` of the smallest integer): a read that stops after the page never reaches it.
    let sql = "SELECT CASE WHEN x = 1000 THEN abs(-9223372036854775808) ELSE x END AS x FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000) SELECT x FROM c)";
    let paged = query_with(connection, 1, sql, 0, 2, json!({}));
    assert_eq!(paged["ok"], true, "a page of two rows stops stepping: {paged}");
    assert_eq!(paged["value"]["row_count"], 2);
    let page_id = paged["value"]["page_id"].as_u64().unwrap();
    take(dbengine_release_page(page_id));
    let to_end = query_with(connection, 2, sql, 0, 2, json!({"run_to_end": true}));
    assert_eq!(to_end["ok"], false, "every row is stepped, the late error is seen: {to_end}");
    assert!(to_end["error"]["message"].as_str().unwrap().contains("overflow"), "{to_end}");
    // Without the error the page is the first rows and nothing says there are more.
    let fine = query_with(connection, 3, "SELECT x FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 50) SELECT x FROM c)", 0, 3, json!({"run_to_end": true}));
    assert_eq!((fine["value"]["row_count"].as_u64(), fine["value"]["flags"]["truncated"].as_bool()), (Some(3), Some(false)), "{fine}");
}

#[test]
fn read_only_requests_run_under_query_only_and_restore_the_connection() {
    let capabilities = take(dbengine_capabilities());
    assert!(
        capabilities["features"]
            .as_array()
            .unwrap()
            .contains(&json!("query.read_only")),
        "{capabilities}"
    );
    let connection = connect();
    assert_eq!(
        query(connection, 1, "CREATE TABLE t(x INTEGER)")["ok"],
        true
    );
    assert_eq!(query(connection, 2, "INSERT INTO t VALUES (1)")["ok"], true);
    let read_only = |operation_id: u64, sql: &str| {
        query_with(
            connection,
            operation_id,
            sql,
            0,
            0,
            json!({"read_only": true, "confirmed": true}),
        )
    };
    // A read runs.
    let select = read_only(3, "SELECT x FROM t");
    assert_eq!(select["ok"], true, "{select}");
    assert_eq!(select["value"]["row_count"], 1);
    // Every kind of write is refused with a typed code, and changes nothing.
    for (id, write) in [
        (4, "INSERT INTO t VALUES (2)"),
        (5, "UPDATE t SET x = 9 WHERE x = 1"),
        (6, "DELETE FROM t WHERE x = 1"),
        (7, "CREATE TABLE u(y)"),
        (8, "DROP TABLE t"),
    ] {
        let refused = read_only(id, write);
        assert_eq!(refused["ok"], false, "{write}: {refused}");
        assert_eq!(
            refused["error"]["code"], "query.read_only_violation",
            "{write}: {refused}"
        );
    }
    // `query_only` was switched back: an ordinary write works, and nothing was lost.
    assert_eq!(query(connection, 9, "INSERT INTO t VALUES (3)")["ok"], true);
    let count = query(connection, 10, "SELECT count(*) AS n FROM t");
    assert_eq!(count["value"]["row_count"], 1);
    // The user's own `query_only = ON` is preserved across a read-only request.
    assert_eq!(query(connection, 11, "PRAGMA query_only = ON")["ok"], true);
    assert_eq!(read_only(12, "SELECT x FROM t")["ok"], true);
    let still_on = query(connection, 13, "INSERT INTO t VALUES (4)");
    assert_eq!(
        still_on["error"]["code"], "query.read_only_violation",
        "{still_on}"
    );
}

/// `PRAGMA query_only` is the READ ONLY scope; the text must not be able to switch it off
/// or end the transaction around it. Refused up front with the read-only code, nothing runs.
#[test]
fn read_only_requests_refuse_text_that_could_leave_the_scope() {
    let connection = connect();
    assert_eq!(
        query(connection, 1, "CREATE TABLE t(x INTEGER)")["ok"],
        true
    );
    assert_eq!(
        query(connection, 2, "INSERT INTO t VALUES (1), (2)")["ok"],
        true
    );
    let read_only = |operation_id: u64, sql: &str| {
        query_with(
            connection,
            operation_id,
            sql,
            0,
            0,
            json!({"read_only": true, "confirmed": true}),
        )
    };
    let count = |operation_id: u64| {
        let result = query(connection, operation_id, "SELECT count(*) AS n FROM t");
        let page_id = result["value"]["page_id"].as_u64().unwrap();
        take(dbengine_release_page(page_id));
        result
    };
    for (id, sql) in [
        (10, "PRAGMA query_only = OFF; DELETE FROM t"),
        (
            11,
            "SELECT 1; PRAGMA query_only = 0; DELETE FROM t WHERE x = 1",
        ),
        (12, "SELECT 1; COMMIT; DELETE FROM t WHERE x = 1"),
        (13, "COMMIT"),
        (14, "BEGIN"),
        (15, "BEGIN IMMEDIATE; DELETE FROM t WHERE x = 1; COMMIT"),
        (16, "SELECT 1; END"),
        (17, "SAVEPOINT s"),
        (18, "SELECT 1; RELEASE s"),
        (19, "SELECT 1; ROLLBACK TO s"),
        (20, "/* x */ COMMIT; DELETE FROM t WHERE x = 1"),
        (21, "SELECT 1; INSERT INTO t VALUES (9)"),
        // Statements that need no transaction, which `query_only` does not stop.
        (22, "VACUUM INTO '/tmp/tablex_ro_vacuum_probe.db'"),
        (23, "VACUUM"),
        (24, "ATTACH '/tmp/tablex_ro_attach_probe.db' AS probe"),
        (25, "PRAGMA query_only = OFF"),
        (26, "DELETE FROM t WHERE x = 1"),
    ] {
        let refused = read_only(id, sql);
        assert_eq!(refused["ok"], false, "{sql}: {refused}");
        assert_eq!(
            refused["error"]["code"], "query.read_only_violation",
            "{sql}: {refused}"
        );
    }
    assert!(!std::path::Path::new("/tmp/tablex_ro_vacuum_probe.db").exists());
    assert!(!std::path::Path::new("/tmp/tablex_ro_attach_probe.db").exists());
    assert_eq!(count(30)["value"]["row_count"], 1);
    // Nothing ran: both rows are there, and `query_only` is still off for ordinary writes.
    let rows = query(connection, 31, "SELECT x FROM t ORDER BY x");
    assert_eq!(rows["value"]["row_count"], 2, "{rows}");
    take(dbengine_release_page(
        rows["value"]["page_id"].as_u64().unwrap(),
    ));
    // Plain reads still run: one statement, a script of reads, text that only looks like more.
    for (id, sql) in [
        (40, "SELECT x FROM t"),
        (41, "SELECT 1; SELECT x FROM t ORDER BY x"),
        (42, "EXPLAIN QUERY PLAN SELECT x FROM t"),
        (43, "SELECT 'COMMIT; DELETE FROM t', \"END\" FROM t"),
        (44, "SELECT 1 /* ; COMMIT; */ -- ; END"),
    ] {
        let page = read_only(id, sql);
        assert_eq!(page["ok"], true, "{sql}: {page}");
        take(dbengine_release_page(
            page["value"]["page_id"].as_u64().unwrap(),
        ));
    }
    assert_eq!(
        query(connection, 50, "DELETE FROM t WHERE x = 1")["ok"],
        true
    );
}

/// What the app receives for a column with several storage classes, and what an
/// edit does to each cell's class, through the exported symbols.
#[test]
fn mixed_columns_carry_each_cells_class_and_edits_keep_it() {
    use arrow_array::ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema};
    use arrow_array::{Array, StructArray, UnionArray};

    let connection = connect();
    assert_eq!(
        query(
            connection,
            1,
            "CREATE TABLE m(id INTEGER PRIMARY KEY, v, b BLOB, f BOOLEAN, r REAL); \
             INSERT INTO m VALUES (1, 1, X'0AFF', 1, 1.5), (2, '1', X'00', 0, 2.5), (3, X'AB', NULL, NULL, 3.5), \
                                  (4, 1.0, NULL, NULL, NULL), (5, NULL, NULL, NULL, NULL)"
        )["ok"],
        true
    );
    let mutate = |operation_id: u64, payload: Value| {
        let request = CString::new(
            json!({
                "operation_id": operation_id, "kind": "update", "database": null, "object": "m",
                "identity": {}, "payload": payload,
            })
            .to_string(),
        )
        .unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { dbengine_mutate(connection, request.as_ptr()) })
    };
    let cell_classes = |operation_id: u64, sql: &str| -> (Value, Vec<i8>, Vec<String>) {
        let page = query(connection, operation_id, sql);
        let page_id = page["value"]["page_id"].as_u64().unwrap();
        let lease = dbengine_acquire_arrow_page(connection, page_id, 0, 100);
        // SAFETY: the lease owns boxed FFI structs, moved out exactly once.
        let (array, schema) = unsafe {
            (
                FFI_ArrowArray::from_raw(lease.array.cast()),
                FFI_ArrowSchema::from_raw(lease.schema.cast()),
            )
        };
        // SAFETY: both structs describe the same exported array.
        let data = unsafe { from_ffi(array, &schema) }.unwrap();
        let columns = StructArray::from(data);
        let column = columns.column(0).clone();
        // A column of one class keeps its native type and has no class list.
        let classes = column
            .as_any()
            .downcast_ref::<UnionArray>()
            .map(|union| (0..union.len()).map(|row| union.type_id(row)).collect())
            .unwrap_or_default();
        let texts = (0..column.len())
            .map(|row| engine_driver_kit::default_cell_text(&column, row))
            .collect();
        take(dbengine_release_page(page_id));
        (page["value"]["columns"].clone(), classes, texts)
    };

    let (columns, classes, texts) = cell_classes(2, "SELECT v FROM m ORDER BY id");
    assert_eq!(columns[0]["type"], "mixed");
    // INTEGER 1, TEXT '1', BLOB, REAL 1.0, NULL: all tellable apart.
    assert_eq!(classes, [0, 2, 3, 1, 0]);
    assert_eq!(texts, ["1", "1", "X'AB'", "1", "NULL"]);

    // INTEGER stays INTEGER, TEXT stays TEXT, BLOB takes hex, REAL stays REAL.
    for (operation, key, value) in [(3, 1, "43"), (4, 2, "44"), (5, 3, "0xCAFE"), (6, 4, "2.5")] {
        let done = mutate(
            operation,
            json!({"op": "update", "key": {"id": key}, "values": {"v": value}}),
        );
        assert_eq!(done["ok"], true, "{done}");
    }
    let (_, classes, texts) = cell_classes(7, "SELECT v FROM m ORDER BY id");
    assert_eq!(classes, [0, 2, 3, 1, 0]);
    assert_eq!(texts, ["43", "44", "X'CAFE'", "2.5", "NULL"]);
    // Anything else for a BLOB cell is refused, typed.
    let refused = mutate(
        8,
        json!({"op": "update", "key": {"id": 3}, "values": {"v": "hello"}}),
    );
    assert_eq!(
        refused["error"]["code"], "mutation.invalid_value",
        "{refused}"
    );
    // The BLOB column's key is the hex text, in a mixed column too.
    let by_blob = mutate(
        9,
        json!({"op": "update", "key": {"v": "0xcafe"}, "values": {"r": "inf"}}),
    );
    assert_eq!(by_blob["ok"], true, "{by_blob}");
    let (_, classes, texts) = cell_classes(10, "SELECT r FROM m ORDER BY id");
    assert_eq!(texts, ["1.5", "2.5", "inf", "NULL", "NULL"]);
    assert!(
        classes.is_empty(),
        "a REAL column stays a plain Float64 column"
    );
    // BOOLEAN: stored 1/0, shown and accepted as such, true/false too.
    let done = mutate(
        11,
        json!({"op": "update", "key": {"id": 1}, "values": {"f": "false"}}),
    );
    assert_eq!(done["ok"], true, "{done}");
    let page = query(connection, 12, "SELECT f FROM m WHERE id = 1");
    assert_eq!(page["value"]["columns"][0]["type"], "int64");
    // Text that is not UTF-8 is never repaired: it is its own class.
    assert_eq!(
        query(
            connection,
            13,
            "CREATE TABLE bad(x); INSERT INTO bad VALUES (CAST(X'63E9' AS TEXT))"
        )["ok"],
        true
    );
    let (_, classes, _) = cell_classes(14, "SELECT x FROM bad");
    assert_eq!(classes, [4]);
}
