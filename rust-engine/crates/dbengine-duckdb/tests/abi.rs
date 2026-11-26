//! End-to-end checks of the DuckDB driver through its exported C symbols: what the conformance
//! suite (which loads the release dylib) does not reach: in-memory and read-only connections, one
//! file shared by several connections, the user's transaction, partial batches, cancels.

use dbengine_duckdb::*;
use serde_json::{json, Value};
use std::ffi::{c_char, CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};

fn take(ptr: *mut c_char) -> Value {
    assert!(!ptr.is_null());
    // SAFETY: `ptr` is a NUL-terminated string the driver just returned.
    let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
    // SAFETY: `ptr` came from this driver's allocator and is freed once.
    unsafe { dbengine_free_string(ptr) };
    serde_json::from_str(&text).unwrap()
}

fn connect_to(uri: &str) -> Value {
    let request = CString::new(json!({ "uri": uri }).to_string()).unwrap();
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { dbengine_connect(request.as_ptr()) })
}

fn connect(uri: &str) -> u64 {
    let envelope = connect_to(uri);
    assert_eq!(envelope["ok"], true, "{envelope}");
    envelope["value"]["connection_id"].as_u64().unwrap()
}

static OPERATION: AtomicU64 = AtomicU64::new(1);

fn next_operation() -> u64 {
    OPERATION.fetch_add(1, Ordering::Relaxed)
}

fn query_with(connection: u64, sql: &str, offset: u64, limit: u64, payload: Value) -> Value {
    let mut payload = payload;
    payload["sql"] = json!(sql);
    let request = CString::new(
        json!({
            "kind": "sql", "operation_id": next_operation(), "database": null, "object": null,
            "offset": offset, "limit": limit, "payload": payload,
        })
        .to_string(),
    )
    .unwrap();
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { dbengine_open_query(connection, request.as_ptr()) })
}

fn query(connection: u64, sql: &str) -> Value {
    query_with(connection, sql, 0, 0, json!({}))
}

fn ok(envelope: Value) -> Value {
    assert_eq!(envelope["ok"], true, "{envelope}");
    envelope["value"].clone()
}

fn rows(connection: u64, sql: &str) -> u64 {
    ok(query(connection, sql))["row_count"].as_u64().unwrap()
}

fn mutate(connection: u64, kind: &str, change: Value) -> Value {
    let request = CString::new(
        json!({
            "operation_id": next_operation(), "kind": kind, "database": null, "object": "t",
            "identity": {}, "payload": change,
        })
        .to_string(),
    )
    .unwrap();
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { dbengine_mutate(connection, request.as_ptr()) })
}

fn insert(connection: u64, id: i64) -> Value {
    mutate(connection, "insert", json!({"op": "insert", "values": {"id": id, "note": "n"}}))
}

fn file_uri(dir: &tempfile::TempDir, name: &str) -> String {
    format!("duckdb://{}", dir.path().join(name).display())
}

#[test]
fn memory_databases_are_private_to_their_connection() {
    let one = connect("duckdb://:memory:");
    let two = connect("duckdb://:memory:");
    ok(query(one, "CREATE TABLE t (id INT)"));
    let missing = query(two, "SELECT * FROM t");
    assert_eq!(missing["ok"], false, "{missing}");
    assert_eq!(rows(one, "SELECT * FROM t"), 0);
}

#[test]
fn bad_uris_are_connect_bad_request() {
    for uri in ["duckdb://", "sqlite:///x.db", "duckdb:///x%zz.db"] {
        let envelope = connect_to(uri);
        assert_eq!(envelope["ok"], false, "{uri}");
        assert_eq!(envelope["error"]["code"], "connect.bad_request", "{uri}: {envelope}");
    }
}

#[test]
fn connections_to_one_file_share_it_and_see_committed_data() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = file_uri(&dir, "shared.duckdb");
    let one = connect(&uri);
    let two = connect(&uri);
    ok(query(one, "CREATE TABLE t (id INT, note VARCHAR)"));
    ok(query(one, "INSERT INTO t VALUES (1, 'a')"));
    assert_eq!(rows(two, "SELECT * FROM t"), 1);
    // An uncommitted write is not visible to the other connection.
    ok(query(one, "BEGIN"));
    ok(query(one, "INSERT INTO t VALUES (2, 'b')"));
    assert_eq!(rows(two, "SELECT * FROM t"), 1);
    ok(query(one, "COMMIT"));
    assert_eq!(rows(two, "SELECT * FROM t"), 2);
    // The file is closed when the last connection is: a later connection reads what was written.
    take(dbengine_disconnect(one));
    take(dbengine_disconnect(two));
    let three = connect(&uri);
    assert_eq!(rows(three, "SELECT * FROM t"), 2);
}

#[test]
fn a_read_only_connection_reads_and_refuses_writes() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = file_uri(&dir, "ro.duckdb");
    let writer = connect(&path);
    ok(query(writer, "CREATE TABLE t (id INT, note VARCHAR)"));
    ok(query(writer, "INSERT INTO t VALUES (1, 'a')"));
    take(dbengine_disconnect(writer));

    let reader = connect(&format!("{path}?mode=ro"));
    assert_eq!(rows(reader, "SELECT * FROM t"), 1);
    let refused = query(reader, "INSERT INTO t VALUES (2, 'b')");
    assert_eq!(refused["error"]["code"], "query.read_only_violation", "{refused}");
    // A mutation is refused too, whole.
    assert_eq!(insert(reader, 5)["ok"], false);
    assert_eq!(rows(reader, "SELECT * FROM t"), 1);
    // The same file cannot be open both ways at once.
    let both = connect_to(&path);
    assert_eq!(both["ok"], false, "{both}");
    take(dbengine_disconnect(reader));

    // A file that does not exist is not created by a read-only connection.
    let missing = connect_to(&format!("{}?mode=ro", file_uri(&dir, "none.duckdb")));
    assert_eq!(missing["ok"], false, "{missing}");
    assert!(!dir.path().join("none.duckdb").exists());
}

#[test]
fn a_read_only_request_cannot_write_or_leave_its_scope() {
    let connection = connect("duckdb://:memory:");
    ok(query(connection, "CREATE TABLE t (id INT)"));
    let read_only = |sql: &str| query_with(connection, sql, 0, 0, json!({"read_only": true}));
    assert_eq!(ok(read_only("SELECT * FROM t"))["row_count"], 0);
    for sql in [
        "INSERT INTO t VALUES (1)",
        "COMMIT; INSERT INTO t VALUES (1)",
        "SELECT 1; DELETE FROM t WHERE id = 1",
        "ATTACH ':memory:' AS other",
        "COPY t TO '/tmp/never-written.csv'",
    ] {
        let refused = read_only(sql);
        assert_eq!(refused["error"]["code"], "query.read_only_violation", "{sql}: {refused}");
    }
    // A function that writes is stopped by the scope itself.
    assert_eq!(rows(connection, "SELECT * FROM t"), 0);
    // The scope is gone afterwards: the connection writes again.
    ok(query(connection, "INSERT INTO t VALUES (1)"));
    assert_eq!(rows(connection, "SELECT * FROM t"), 1);
}

#[test]
fn the_user_transaction_is_followed_and_a_mutation_joins_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = file_uri(&dir, "txn.duckdb");
    let connection = connect(&uri);
    let other = connect(&uri);
    ok(query(connection, "CREATE TABLE t (id INT PRIMARY KEY, note VARCHAR)"));

    // Rolled back by the user: the mutation was part of the transaction.
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (1, 'user')"));
    ok(insert(connection, 2));
    // A change that fails before it writes leaves the transaction usable.
    let missing = mutate(
        connection,
        "update",
        json!({"op": "update", "key": {"id": 99}, "values": {"note": "x"}}),
    );
    assert_eq!(missing["error"]["code"], "mutation.no_rows_affected", "{missing}");
    assert_eq!(rows(connection, "SELECT * FROM t"), 2);
    assert_eq!(rows(other, "SELECT * FROM t"), 0);
    ok(query(connection, "ROLLBACK"));
    assert_eq!(rows(connection, "SELECT * FROM t"), 0);

    // Committed by the user: persists.
    ok(query(connection, "BEGIN"));
    ok(insert(connection, 3));
    assert_eq!(rows(other, "SELECT * FROM t"), 0);
    ok(query(connection, "COMMIT"));
    assert_eq!(rows(other, "SELECT * FROM t"), 1);

    // Outside a transaction a mutation commits on its own.
    ok(insert(connection, 4));
    assert_eq!(rows(other, "SELECT * FROM t"), 2);
}

#[test]
fn a_batch_that_stops_inside_the_users_transaction_says_what_was_applied() {
    let connection = connect("duckdb://:memory:");
    ok(query(connection, "CREATE TABLE t (id INT PRIMARY KEY, note VARCHAR)"));
    let batch = json!({"changes": [
        {"op": "insert", "values": {"id": 1, "note": "a"}},
        {"op": "insert", "values": {"id": 2, "note": "b"}},
        {"op": "update", "key": {"id": 99}, "values": {"note": "x"}},
        {"op": "insert", "values": {"id": 3, "note": "c"}},
    ]});
    // On its own it is all or nothing.
    let failed = mutate(connection, "batch", batch.clone());
    assert_eq!(failed["error"]["code"], "mutation.no_rows_affected", "{failed}");
    assert_eq!(rows(connection, "SELECT * FROM t"), 0);
    // Inside the user's transaction the first two stay in it, and the error lists them.
    ok(query(connection, "BEGIN"));
    let partial = mutate(connection, "batch", batch);
    assert_eq!(partial["error"]["code"], "mutation.partially_applied", "{partial}");
    let message = partial["error"]["message"].as_str().unwrap();
    assert!(message.contains("applied: changes 0, 1") && message.contains("not attempted: 3"), "{message}");
    assert_eq!(rows(connection, "SELECT * FROM t"), 2);
    ok(query(connection, "ROLLBACK"));
    assert_eq!(rows(connection, "SELECT * FROM t"), 0);
}

#[test]
fn mutations_write_every_type_from_text_and_blobs_from_hex() {
    let connection = connect("duckdb://:memory:");
    ok(query(
        connection,
        "CREATE TABLE t (id INT PRIMARY KEY, d DATE, h HUGEINT, l INT[], b BLOB, note VARCHAR)",
    ));
    ok(mutate(
        connection,
        "insert",
        json!({"op": "insert", "values": {
            "id": 1, "d": "2024-02-29", "h": "123456789012345678901234567890",
            "l": [1, 2, 3], "b": "0xDEad", "note": {"k": "v"},
        }}),
    ));
    assert_eq!(
        rows(
            connection,
            "SELECT * FROM t WHERE d = DATE '2024-02-29' AND h = 123456789012345678901234567890::HUGEINT \
             AND len(l) = 3 AND b = '\\xDE\\xAD'::BLOB AND note = '{\"k\":\"v\"}'",
        ),
        1
    );
    // A BLOB key is hex too, and a bad hex value writes nothing.
    ok(mutate(
        connection,
        "update",
        json!({"op": "update", "key": {"b": "0xdead"}, "values": {"note": "by blob"}}),
    ));
    let bad = mutate(
        connection,
        "update",
        json!({"op": "update", "key": {"id": 1}, "values": {"b": "xyz"}}),
    );
    assert_eq!(bad["error"]["code"], "mutation.invalid_value", "{bad}");
    assert_eq!(rows(connection, "SELECT * FROM t WHERE note = 'by blob'"), 1);
    // A value DuckDB cannot cast is a failure that writes nothing.
    let cast = mutate(
        connection,
        "update",
        json!({"op": "update", "key": {"id": 1}, "values": {"d": "not a date"}}),
    );
    assert_eq!(cast["ok"], false, "{cast}");
    assert_eq!(rows(connection, "SELECT * FROM t WHERE d = DATE '2024-02-29'"), 1);
}

#[test]
fn scripts_return_the_last_result_set_and_count_what_dml_changed() {
    let connection = connect("duckdb://:memory:");
    let create = ok(query(connection, "CREATE TABLE t (x INT); INSERT INTO t VALUES (1), (2)"));
    assert_eq!(create["columns"], json!([]));
    assert_eq!(create["total_count"], 2);
    let last = ok(query(connection, "SELECT 1 AS a; INSERT INTO t VALUES (3); SELECT x FROM t ORDER BY x"));
    assert_eq!(last["row_count"], 3);
    assert_eq!(last["columns"][0]["name"], "x");
    assert_eq!(last["columns"][0]["type"], "INTEGER");
    let first = ok(query(connection, "SELECT 1 AS a; INSERT INTO t VALUES (4)"));
    assert_eq!(first["columns"][0]["name"], "a");
    // Paging is for a single read; the write ran once.
    let paged = query_with(connection, "INSERT INTO t VALUES (5)", 3, 10, json!({}));
    assert_eq!(paged["error"]["code"], "query.not_pageable", "{paged}");
    assert_eq!(rows(connection, "SELECT * FROM t"), 4);
    // `run_to_end` keeps `limit` rows but reads them all.
    let run = query_with(connection, "SELECT x FROM t", 0, 2, json!({"run_to_end": true}));
    assert_eq!(run["value"]["row_count"], 2, "{run}");
    assert_eq!(run["value"]["flags"]["truncated"], false);
}

#[test]
fn nested_values_show_as_text_in_the_cell_batch_path() {
    let connection = connect("duckdb://:memory:");
    let page = ok(query(
        connection,
        "SELECT [1, 2] AS l, {'a': 1} AS s, 1.5::DECIMAL(5,2) AS d, TIMESTAMPTZ '2024-01-02 03:04:05+00' AS t",
    ));
    let types: Vec<_> = page["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(types, ["INTEGER[]", "STRUCT", "DECIMAL(5,2)", "TIMESTAMP WITH TIME ZONE"]);
    let inspected = take(dbengine_inspect_row(page["page_id"].as_u64().unwrap(), 0));
    let fields = &inspected["value"]["fields"];
    assert!(fields[0]["display_text"].as_str().unwrap().contains('2'), "{inspected}");
    assert_eq!(fields[2]["display_text"], "1.50");
    assert_eq!(fields[3]["display_text"], "2024-01-02T03:04:05Z");
}

#[test]
fn cancel_stops_a_running_statement_and_the_connection_recovers() {
    let connection = connect("duckdb://:memory:");
    let sql = "SELECT sum(a.range * b.range) FROM range(10000000) a, range(10000000) b";
    let started = std::time::Instant::now();
    let operation_id = next_operation();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| {
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
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let request = CString::new(json!({"operation_id": operation_id}).to_string()).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        let cancelled = take(unsafe { dbengine_cancel_operation(connection, request.as_ptr()) });
        assert_eq!(cancelled["value"]["cancelled"], true, "{cancelled}");
        let envelope = running.join().unwrap();
        assert_eq!(envelope["error"]["code"], "query.cancelled", "{envelope}");
        assert_eq!(envelope["error"]["retryable"], false);
    });
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(rows(connection, "SELECT 1"), 1);
}

/// A cancel that lands at any moment around the start of an operation (before it is active,
/// before its statement runs, while it runs) stops it: none is lost.
#[test]
fn a_cancel_at_any_moment_of_an_operation_stops_it() {
    let connection = connect("duckdb://:memory:");
    let sql = "SELECT sum(a.range * b.range) FROM range(10000000) a, range(10000000) b";
    let cancel = |operation_id: u64| {
        let request = CString::new(json!({"operation_id": operation_id}).to_string()).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { dbengine_cancel_operation(connection, request.as_ptr()) })
    };
    for round in 0..60_u64 {
        let operation_id = next_operation();
        std::thread::scope(|scope| {
            let running = scope.spawn(move || {
                let request = CString::new(
                    json!({
                        "kind": "sql", "operation_id": operation_id, "database": null,
                        "object": null, "offset": 0, "limit": 0,
                        "payload": {"sql": sql, "read_only": true},
                    })
                    .to_string(),
                )
                .unwrap();
                // SAFETY: `request` is a live NUL-terminated string.
                take(unsafe { dbengine_open_query(connection, request.as_ptr()) })
            });
            let wait = std::time::Duration::from_micros(round * 40);
            let sent = std::time::Instant::now();
            while sent.elapsed() < wait {
                std::hint::spin_loop();
            }
            while !running.is_finished() && cancel(operation_id)["value"]["cancelled"] != true {
                std::hint::spin_loop();
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !running.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(running.is_finished(), "round {round}: the cancel was lost");
            let envelope = running.join().unwrap();
            assert_eq!(envelope["error"]["code"], "query.cancelled", "round {round}: {envelope}");
        });
    }
    // The read-only scope was rolled back after every cancelled round: the connection writes.
    ok(query(connection, "CREATE TABLE after_sweep (x INT)"));
}

#[test]
fn the_navigator_lists_catalogs_schemas_tables_and_views_and_describe_reads_them_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let connection = connect(&file_uri(&dir, "shop.duckdb"));
    ok(query_with(
        connection,
        "CREATE TABLE items (tenant INT NOT NULL, id INT NOT NULL, name VARCHAR DEFAULT 'x', \
           PRIMARY KEY (tenant, id), UNIQUE (name)); \
         CREATE TABLE refs (id INT PRIMARY KEY, tenant INT, item_id INT, \
           FOREIGN KEY (tenant, item_id) REFERENCES items (tenant, id)); \
         CREATE VIEW v AS SELECT * FROM items; \
         CREATE SCHEMA extra; CREATE TABLE extra.t (a INT); \
         CREATE INDEX ix ON items (name); \
         COMMENT ON COLUMN items.name IS 'the name'",
        0,
        0,
        json!({"confirmed": true}),
    ));
    let list = |request: Value| {
        let request = CString::new(request.to_string()).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        ok(take(unsafe { dbengine_list_nav_nodes(connection, request.as_ptr()) }))
    };
    let labels = |page: &Value| -> Vec<String> {
        page["nodes"].as_array().unwrap().iter().map(|n| n["label"].as_str().unwrap().to_owned()).collect()
    };
    let databases = list(json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 0}));
    assert_eq!(labels(&databases), ["shop"]);
    let schemas = list(json!({"parent": "database:shop", "database": "shop", "payload": {"database": "shop"}, "cursor": null, "limit": 0}));
    assert_eq!(labels(&schemas), ["extra", "main"]);
    let objects = list(json!({"parent": null, "database": "shop", "payload": {"database": "shop", "schema": "main"}, "cursor": null, "limit": 0}));
    assert_eq!(labels(&objects), ["items", "refs", "v"]);
    let kinds: Vec<_> = objects["nodes"].as_array().unwrap().iter().map(|n| n["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["table", "table", "view"]);
    // The filter is a literal substring, ignoring case; a page continues from its cursor.
    let filtered = list(json!({"parent": null, "database": "shop", "payload": {"database": "shop", "schema": "main", "filter": "ITE"}, "cursor": null, "limit": 0}));
    assert_eq!(labels(&filtered), ["items"]);
    let first = list(json!({"parent": null, "database": "shop", "payload": {"database": "shop", "schema": "main"}, "cursor": null, "limit": 2}));
    assert_eq!(labels(&first), ["items", "refs"]);
    let second = list(json!({"parent": null, "database": "shop", "payload": {"database": "shop", "schema": "main"}, "cursor": first["next_cursor"], "limit": 2}));
    assert_eq!(labels(&second), ["v"]);
    assert_eq!(second["next_cursor"], Value::Null);
    // Kinds DuckDB's navigator does not list are empty pages; an unknown one is refused.
    let functions = list(json!({"parent": null, "database": "shop", "payload": {"database": "shop", "schema": "main", "kind": "function"}, "cursor": null, "limit": 0}));
    assert!(labels(&functions).is_empty());

    let describe = |object: &str| {
        let request = CString::new(json!({"database": "shop", "schema": "main", "object": object}).to_string()).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        take(unsafe { dbengine_describe_object(connection, request.as_ptr()) })
    };
    let items = ok(describe("items"));
    assert_eq!(items["columns"][2]["comment"], "the name");
    assert_eq!(items["columns"][2]["default_expression"], "'x'");
    assert_eq!(items["columns"][0]["primary_key"], true);
    assert!(items["keys"].as_array().unwrap().iter().any(|k| k["kind"] == "primary_key" && k["columns"] == json!(["tenant", "id"])));
    assert!(items["keys"].as_array().unwrap().iter().any(|k| k["kind"] == "unique"));
    assert_eq!(items["indexes"][0]["name"], "ix");
    assert_eq!(items["indexes"][0]["columns"], json!(["name"]));
    assert_eq!(items["references"][0]["table"], "refs");
    assert!(items["ddl"].as_str().unwrap().starts_with("CREATE TABLE items"));
    let refs = ok(describe("refs"));
    assert_eq!(refs["foreign_keys"][0]["referenced_table"], "items");
    assert_eq!(refs["foreign_keys"][0]["referenced_columns"], json!(["tenant", "id"]));
    assert!(ok(describe("v"))["ddl"].as_str().unwrap().starts_with("CREATE VIEW v"));
    assert_eq!(describe("nope")["error"]["code"], "describe.not_found");
}

#[test]
fn another_attached_database_is_listed_but_not_connectable() {
    let connection = connect("duckdb://:memory:");
    ok(query(connection, "ATTACH ':memory:' AS scratch"));
    let request = CString::new(json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 0}).to_string()).unwrap();
    // SAFETY: `request` is a live NUL-terminated string.
    let page = ok(take(unsafe { dbengine_list_nav_nodes(connection, request.as_ptr()) }));
    let nodes = page["nodes"].as_array().unwrap();
    let node = |name: &str| nodes.iter().find(|n| n["label"] == name).unwrap_or_else(|| panic!("{name} in {page}"));
    assert_eq!(node("memory")["payload"], json!({"database": "memory"}));
    assert_eq!(node("scratch")["payload"], json!({"database": "scratch", "can_connect": false}));
}

#[test]
fn an_instance_without_external_access_reads_and_writes_no_file_and_cannot_be_unlocked() {
    let dir = tempfile::TempDir::new().unwrap();
    let csv = dir.path().join("secret.csv");
    std::fs::write(&csv, "a\n1\n").unwrap();
    let csv = csv.display().to_string();
    let out = dir.path().join("out.csv");

    // With file access (the default) the same text reads the file.
    let open = connect("duckdb://:memory:");
    assert_eq!(rows(open, &format!("SELECT * FROM read_csv('{csv}')")), 1);
    assert_eq!(rows(open, &format!("SELECT * FROM '{csv}'")), 1);

    for uri in ["duckdb://:memory:?external_access=off".to_owned(), format!("{}?external_access=off", file_uri(&dir, "locked.duckdb"))] {
        let locked = connect(&uri);
        // The database itself works.
        ok(query(locked, "CREATE TABLE t (id INT)"));
        ok(query(locked, "INSERT INTO t VALUES (1), (2)"));
        assert_eq!(rows(locked, "SELECT * FROM t"), 2);
        // Files and URLs do not, whatever the spelling.
        for sql in [
            format!("SELECT * FROM read_csv('{csv}')"),
            format!("SELECT * FROM read_text('{csv}')"),
            format!("SELECT * FROM '{csv}'"),
            format!("SELECT * FROM \"{csv}\""),
            format!("COPY t TO '{}'", out.display()),
            format!("COPY t FROM '{csv}'"),
            format!("ATTACH '{csv}' AS other"),
            format!("EXPORT DATABASE '{}'", dir.path().join("export").display()),
            "INSTALL httpfs".to_owned(),
            "LOAD httpfs".to_owned(),
            "SELECT * FROM 'https://example.invalid/x.csv'".to_owned(),
        ] {
            let failed = query(locked, &sql);
            assert_eq!(failed["ok"], false, "{sql}: {failed}");
        }
        assert!(!out.exists());
        // Nothing can undo it: the setting and the whole configuration are locked.
        for sql in ["SET enable_external_access = true", "SET threads = 1", "SET autoinstall_known_extensions = true"] {
            let failed = query(locked, sql);
            assert_eq!(failed["ok"], false, "{sql}: {failed}");
        }
        // A read-only scope on top of it runs as before.
        let page = ok(query_with(locked, "SELECT * FROM t", 0, 0, json!({"read_only": true})));
        assert_eq!(page["row_count"], 2);
        take(dbengine_disconnect(locked));
    }
}

#[test]
fn a_file_is_open_with_or_without_external_access_never_both() {
    let dir = tempfile::TempDir::new().unwrap();
    let plain = file_uri(&dir, "mixed.duckdb");
    let locked = format!("{plain}?external_access=off");
    let user = connect(&plain);
    // The app's connection keeps file access: a locked one cannot share its instance.
    let refused = connect_to(&locked);
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(refused["error"]["message"].as_str().unwrap().contains("already open for writing"), "{refused}");
    take(dbengine_disconnect(user));
    // The other way round: an agent's locked instance is never widened for a later connection.
    let agent = connect(&locked);
    let refused = connect_to(&plain);
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(refused["error"]["message"].as_str().unwrap().contains("without file access"), "{refused}");
    // Another locked connection shares it.
    let second = connect(&locked);
    ok(query(agent, "CREATE TABLE t (id INT)"));
    assert_eq!(rows(second, "SELECT * FROM t"), 0);
}

#[test]
fn a_begin_inside_the_users_transaction_is_not_run_and_keeps_it_usable() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = file_uri(&dir, "nested.duckdb");
    let connection = connect(&uri);
    let other = connect(&uri);
    ok(query(connection, "CREATE TABLE t (id INT PRIMARY KEY, note VARCHAR)"));

    // The app sends a BEGIN before every statement of a manual-commit tab.
    for (id, begin) in [(1, "BEGIN"), (2, "BEGIN"), (3, "BEGIN TRANSACTION"), (4, "START TRANSACTION")] {
        ok(query(connection, begin));
        ok(query(connection, &format!("INSERT INTO t VALUES ({id}, 'n')")));
    }
    assert_eq!(rows(connection, "SELECT * FROM t"), 4, "the transaction is still open and usable");
    assert_eq!(rows(other, "SELECT * FROM t"), 0);
    // One COMMIT ends the one transaction.
    ok(query(connection, "COMMIT"));
    assert_eq!(rows(other, "SELECT * FROM t"), 4);
    assert_eq!(query(connection, "COMMIT")["ok"], false, "no second transaction was opened");

    // In a script, and the rollback path.
    ok(query(connection, "BEGIN; INSERT INTO t VALUES (5, 'n'); BEGIN; INSERT INTO t VALUES (6, 'n')"));
    assert_eq!(rows(connection, "SELECT * FROM t"), 6);
    ok(query(connection, "ROLLBACK"));
    assert_eq!(rows(connection, "SELECT * FROM t"), 4);
    // Outside a transaction a BEGIN runs again.
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (7, 'n')"));
    assert_eq!(rows(other, "SELECT * FROM t"), 4, "a transaction is open again");
    ok(query(connection, "COMMIT"));
    assert_eq!(rows(other, "SELECT * FROM t"), 5);
}

#[test]
fn a_failed_statement_aborts_the_transaction_and_the_next_begin_changes_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = file_uri(&dir, "aborted.duckdb");
    let connection = connect(&uri);
    let other = connect(&uri);
    ok(query(connection, "CREATE TABLE t (id INT PRIMARY KEY, note VARCHAR)"));
    ok(query(connection, "INSERT INTO t VALUES (1, 'kept')"));

    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (2, 'lost')"));
    ok(query(connection, "BEGIN"));
    assert_eq!(query(connection, "INSERT INTO t VALUES (1, 'duplicate')")["ok"], false);
    // As on PostgreSQL: everything but the end of the transaction is refused until then.
    ok(query(connection, "BEGIN"));
    let refused = query(connection, "SELECT * FROM t");
    assert!(refused["error"]["message"].as_str().unwrap().contains("transaction is aborted"), "{refused}");
    ok(query(connection, "ROLLBACK"));
    assert_eq!(rows(connection, "SELECT * FROM t"), 1);
    assert_eq!(rows(other, "SELECT * FROM t"), 1);
    // The next BEGIN opens a transaction again.
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (3, 'n')"));
    assert_eq!(rows(other, "SELECT * FROM t"), 1);
    ok(query(connection, "COMMIT"));
    assert_eq!(rows(other, "SELECT * FROM t"), 2);

    // COMMIT of an aborted transaction rolls it back (DuckDB says nothing) and ends it.
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (4, 'lost')"));
    assert_eq!(query(connection, "SELEC 1")["ok"], false);
    ok(query(connection, "COMMIT"));
    assert_eq!(rows(other, "SELECT * FROM t"), 2);
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (5, 'n')"));
    assert_eq!(rows(other, "SELECT * FROM t"), 2, "a BEGIN after it opens a transaction");
    ok(query(connection, "ROLLBACK"));
}

/// What the lexer takes for one statement and DuckDB splits in several must not run its first
/// statements unseen: the attack runs `COMMIT` inside the READ ONLY scope, then a `DELETE`.
#[test]
fn text_the_lexer_and_duckdb_split_differently_runs_nothing_read_only() {
    let connection = connect("duckdb://:memory:");
    ok(query(connection, "CREATE TABLE orders (id INT)"));
    ok(query(connection, "INSERT INTO orders VALUES (1), (2)"));
    let read_only = |sql: &str| query_with(connection, sql, 0, 0, json!({"read_only": true}));
    for sql in [
        // A backtick name hides the `'` from a PostgreSQL lexer: DuckDB sees four statements.
        "SELECT 1 AS `'`; COMMIT; DELETE FROM orders; SELECT 1 AS `'`",
        "SELECT 1 AS a`;`; COMMIT; DELETE FROM orders; SELECT 1 AS `b`",
        "SELECT 1 AS `a``'`; COMMIT; DELETE FROM orders; SELECT 1 AS `'`",
    ] {
        let refused = read_only(sql);
        assert_eq!(refused["ok"], false, "{sql}: {refused}");
    }
    assert_eq!(rows(connection, "SELECT * FROM orders"), 2, "nothing ran");
    // The same text is the user's own script outside the read-only scope: four statements, in order.
    ok(query(connection, "SELECT 1 AS `'`; INSERT INTO orders VALUES (3); SELECT 1 AS `'`"));
    assert_eq!(rows(connection, "SELECT * FROM orders"), 3);
}

/// A bare `START`/`START WORK`, a parse error in a `COMMIT`, and an `EXECUTE` of a prepared
/// `COMMIT` leave the driver's idea of the transaction in step with DuckDB's.
#[test]
fn the_transaction_flag_follows_what_duckdb_did_not_what_the_text_said() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = file_uri(&dir, "flag.duckdb");
    let connection = connect(&uri);
    let other = connect(&uri);
    ok(query(connection, "CREATE TABLE t (id INT PRIMARY KEY)"));
    let open = |at: u64| rows(other, "SELECT * FROM t") == at;

    // `START TRANSACTION`, `START WORK` or bare `START`: whichever DuckDB takes opens one, and the
    // BEGIN the app sends before the next statement must not abort it.
    let mut id = 0;
    for start in ["START TRANSACTION", "START WORK", "START"] {
        let started = query(connection, start);
        eprintln!("{start}: ok={}", started["ok"]);
        if started["ok"] != true {
            continue;
        }
        id += 1;
        ok(query(connection, &format!("INSERT INTO t VALUES ({id})")));
        ok(query(connection, "BEGIN"));
        ok(query(connection, &format!("INSERT INTO t VALUES ({})", id + 100)));
        assert!(open(0), "{start}: still one open transaction");
        ok(query(connection, "ROLLBACK"));
        assert!(open(0), "{start}: rolled back");
    }
    assert!(id >= 1, "DuckDB takes at least `START TRANSACTION`");

    // A `COMMIT` that does not parse ends nothing: DuckDB aborts the transaction on the error and it
    // stays open, so the app's next BEGIN must not run (it would be refused as nested, which is
    // another error), and only the end of the transaction is accepted.
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (10)"));
    assert_eq!(query(connection, "COMMIT x y z")["ok"], false);
    ok(query(connection, "BEGIN"));
    let refused = query(connection, "INSERT INTO t VALUES (11)");
    assert!(refused["error"]["message"].as_str().unwrap().contains("transaction is aborted"), "{refused}");
    ok(query(connection, "ROLLBACK"));
    assert!(open(0), "nothing of it was kept");
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (11)"));
    assert!(open(0), "a BEGIN after the ROLLBACK opened a transaction");
    ok(query(connection, "COMMIT"));
    assert!(open(1));

    // A transaction that really ends by a COMMIT of the aborted one: the next BEGIN opens a new one.
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (20)"));
    assert_eq!(query(connection, "INSERT INTO t VALUES (11)")["ok"], false);
    ok(query(connection, "COMMIT"));
    assert!(open(1), "the aborted transaction was rolled back");
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (21)"));
    assert!(open(1), "a BEGIN after the failing COMMIT opened a transaction");
    ok(query(connection, "COMMIT"));
    assert!(open(2));

    // A prepared COMMIT ends the transaction without the text saying so.
    if query(connection, "PREPARE end_it AS COMMIT")["ok"] == true {
        ok(query(connection, "BEGIN"));
        ok(query(connection, "INSERT INTO t VALUES (30)"));
        ok(query(connection, "EXECUTE end_it"));
        assert!(open(3), "committed by the EXECUTE");
        ok(query(connection, "BEGIN"));
        ok(query(connection, "INSERT INTO t VALUES (31)"));
        assert!(open(3), "a BEGIN after it opens a transaction, not skipped as nested");
        ok(query(connection, "ROLLBACK"));
        assert!(open(3));
    } else {
        eprintln!("PREPARE of a COMMIT is refused by DuckDB");
    }
}

/// Inside a READ ONLY transaction of the user's own, only the engine is left to stop `COPY ... TO`
/// and `ATTACH`; what it does, and that a locked instance refuses both either way.
#[test]
fn read_only_transactions_and_locked_instances_against_copy_and_attach() {
    let dir = tempfile::TempDir::new().unwrap();
    for locked in [false, true] {
        let suffix = if locked { "?external_access=off" } else { "" };
        let connection = connect(&format!("{}{suffix}", file_uri(&dir, if locked { "l.duckdb" } else { "o.duckdb" })));
        ok(query(connection, "CREATE TABLE t (id INT)"));
        ok(query(connection, "INSERT INTO t VALUES (1)"));
        let copy_out = dir.path().join(if locked { "locked.csv" } else { "open.csv" });
        let attached = dir.path().join(if locked { "locked-other.duckdb" } else { "open-other.duckdb" });
        ok(query(connection, "BEGIN TRANSACTION READ ONLY"));
        let copy = query(connection, &format!("COPY t TO '{}'", copy_out.display()));
        let attach = query(connection, &format!("ATTACH '{}' AS other", attached.display()));
        eprintln!("locked={locked}: COPY ok={} ATTACH ok={}", copy["ok"], attach["ok"]);
        if locked {
            assert_eq!(copy["ok"], false, "{copy}");
            assert_eq!(attach["ok"], false, "{attach}");
            assert!(!copy_out.exists() && !attached.exists());
        }
        let _ = query(connection, "ROLLBACK");
    }
}

/// DuckDB rewrites some Unicode spaces to ASCII ones before it tokenizes; the lexer reads them as
/// word bytes. In the user's own transaction (where the READ ONLY scope does not apply and the
/// lexer's refusal is all there is) this text is one pageable read to the lexer and a `DELETE`
/// after a CTE to DuckDB.
#[test]
fn a_unicode_space_in_code_cannot_hide_a_delete_in_a_read_only_request() {
    let connection = connect("duckdb://:memory:");
    ok(query(connection, "CREATE TABLE orders (id INT)"));
    ok(query(connection, "INSERT INTO orders VALUES (1), (2)"));
    let read_only = |sql: &str| query_with(connection, sql, 0, 0, json!({"read_only": true}));
    let attacks = [
        "WITH c AS (SELECT \u{a0}e'\\' AS a, x ') DELETE FROM orders --') SELECT * FROM c",
        "WITH c AS (SELECT \u{2003}e'\\' AS a, x ') DELETE FROM orders --') SELECT * FROM c",
        "WITH c AS (SELECT \u{feff}e'\\' AS a, x ') DELETE FROM orders --') SELECT * FROM c",
    ];
    // Inside a transaction of the user's own, and outside one.
    for in_transaction in [true, false] {
        if in_transaction {
            ok(query(connection, "BEGIN"));
        }
        for sql in attacks {
            let refused = read_only(sql);
            assert_eq!(refused["ok"], false, "{sql:?}: {refused}");
        }
        if in_transaction {
            ok(query(connection, "COMMIT"));
        }
        assert_eq!(rows(connection, "SELECT * FROM orders"), 2, "nothing ran (in a transaction: {in_transaction})");
    }
    // The text is read as DuckDB reads it, so a real Unicode space between words is a space.
    let page = ok(read_only("SELECT\u{a0}count(*) AS n\u{3000}FROM\u{2003}orders"));
    assert_eq!(page["row_count"], 1);
    ok(query(connection, "SELECT\u{a0}1 AS a;\u{a0}SELECT 2 AS b"));
    // Inside a quote it stays what it is.
    let page = ok(read_only("SELECT 'a\u{a0}b' AS s"));
    assert_eq!(page["row_count"], 1);
}

/// DuckDB runs its Unicode-space rewrite again on each statement slice, from the plain state. DuckDB's
/// scan takes the `'` in `/* ' */` for a quote, so the whole text needs no rewrite and the second
/// slice does: to the lexer both are reads, to DuckDB the second one is a `DELETE`.
#[test]
fn a_unicode_space_that_only_a_later_statement_needs_is_refused_too() {
    let connection = connect("duckdb://:memory:");
    ok(query(connection, "CREATE TABLE t (id INT)"));
    ok(query(connection, "INSERT INTO t VALUES (1), (2)"));
    let sql = "/* ' */ SELECT 1; WITH c AS (SELECT \u{a0}e'\\' AS a, x ') DELETE FROM t --') SELECT * FROM c";
    // Not in a transaction of the user's: no READ ONLY scope, no confirmation given.
    let refused = query(connection, sql);
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(rows(connection, "SELECT * FROM t"), 2, "nothing ran");
    // In the user's transaction a read-only request has only the lexer's refusal.
    ok(query(connection, "BEGIN"));
    let refused = query_with(connection, sql, 0, 0, json!({"read_only": true}));
    assert_eq!(refused["ok"], false, "{refused}");
    ok(query(connection, "COMMIT"));
    assert_eq!(rows(connection, "SELECT * FROM t"), 2, "nothing ran");
}

/// A macro of the file called `txid_current` must not answer the driver's question whether a
/// transaction is open: it would say "open" for ever and the app's next `BEGIN` would be skipped,
/// leaving its change in autocommit.
#[test]
fn a_macro_called_txid_current_cannot_make_the_driver_skip_a_begin() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = file_uri(&dir, "txid.duckdb");
    let connection = connect(&uri);
    let other = connect(&uri);
    ok(query(connection, "CREATE TABLE t (id INT)"));
    ok(query(connection, "CREATE MACRO txid_current() AS 1"));
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (1)"));
    ok(query(connection, "COMMIT"));
    assert_eq!(rows(other, "SELECT * FROM t"), 1);
    // The app sends a BEGIN before a statement of a manual-commit tab: it must run.
    ok(query(connection, "BEGIN"));
    ok(query(connection, "INSERT INTO t VALUES (2)"));
    assert_eq!(rows(other, "SELECT * FROM t"), 1, "the change is inside a transaction, not autocommitted");
    ok(query(connection, "ROLLBACK"));
    assert_eq!(rows(other, "SELECT * FROM t"), 1, "rolled back: no change was left");
}

/// The driver says in its connect reply whether the instance has file access off and a locked
/// configuration: from its own open and a qualified `system.main` probe, never from a name that the
/// database file can define a macro for.
#[test]
fn the_connect_reply_reports_a_locked_instance_and_a_macro_in_the_file_cannot_change_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let locked_of = |uri: &str| {
        let reply = connect_to(uri);
        assert_eq!(reply["ok"], true, "{reply}");
        let value = &reply["value"];
        take(dbengine_disconnect(value["connection_id"].as_u64().unwrap()));
        value["file_access_locked"].as_bool()
    };
    let plain = file_uri(&dir, "plain.duckdb");
    assert_eq!(locked_of("duckdb://:memory:"), None, "an open instance reports nothing");
    assert_eq!(locked_of("duckdb://:memory:?external_access=off"), Some(true));
    assert_eq!(locked_of(&plain), None);
    assert_eq!(locked_of(&format!("{plain}?external_access=off")), Some(true));
    assert_eq!(locked_of(&format!("{plain}?mode=ro&external_access=off")), Some(true));

    // A macro with the name of the function that a bare probe would call, answering "locked" and
    // "no file access" whatever the instance is.
    let liar = file_uri(&dir, "liar.duckdb");
    let writer = connect(&liar);
    ok(query(writer, "CREATE MACRO current_setting(name) AS CASE name WHEN 'enable_external_access' THEN false ELSE true END"));
    let shadowed = ok(query(writer, "SELECT current_setting('enable_external_access') AS e"));
    assert_eq!(shadowed["row_count"], 1);
    take(dbengine_disconnect(writer));
    assert_eq!(locked_of(&liar), None, "an unlocked instance is not locked because a macro says so");
    assert_eq!(locked_of(&format!("{liar}?external_access=off")), Some(true));

    // The other way round: a macro that claims file access is on does not unlock what is locked.
    let denier = file_uri(&dir, "denier.duckdb");
    let writer = connect(&denier);
    ok(query(writer, "CREATE MACRO current_setting(name) AS CASE name WHEN 'enable_external_access' THEN true ELSE false END"));
    take(dbengine_disconnect(writer));
    assert_eq!(locked_of(&format!("{denier}?external_access=off")), Some(true));
}
