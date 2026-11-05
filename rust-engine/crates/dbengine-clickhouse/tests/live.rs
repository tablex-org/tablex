//! Live tests against a dedicated ClickHouse test server (never a real one: the
//! tests create and drop `cf_*` databases):
//!
//! ```text
//! TABLEX_TEST_CLICKHOUSE_URI=clickhouse://tablex:tablex@127.0.0.1:58123/tablex \
//!   cargo test -p dbengine-clickhouse --test live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The SQL contract itself is covered by `engine-driver-conformance`'s
//! `clickhouse` test; these cover what is specific to ClickHouse.

use engine_driver_conformance::build_release_dylib;
use engine_driver_conformance::fixtures::ClickHouseFixture;
use engine_driver_conformance::harness::{Conn, Ctx, Failure};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

/// The dedicated test server. Every test here is `#[ignore]`d, so asking for one
/// (`-- --ignored`) without the server is an error, not a silent pass.
fn uri() -> String {
    std::env::var("TABLEX_TEST_CLICKHOUSE_URI").unwrap_or_else(|_| {
        panic!("set TABLEX_TEST_CLICKHOUSE_URI (for example clickhouse://tablex:tablex@127.0.0.1:58123/tablex)")
    })
}

fn load(uri: &str) -> Ctx<'static> {
    let dylib = build_release_dylib(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        "dbengine_clickhouse",
        &[],
    )
    .expect("build the driver");
    Ctx::load(&dylib, uri, &ClickHouseFixture).expect("load the driver")
}

fn ok<T>(result: Result<T, Failure>) -> T {
    match result {
        Ok(value) => value,
        Err(Failure::Fail(message) | Failure::Skip(message)) => panic!("{message}"),
    }
}

fn code(
    result: Result<impl std::fmt::Debug, engine_driver_host::LoaderError>,
) -> (String, bool, String) {
    match result {
        Err(engine_driver_host::LoaderError::DriverRejected {
            code,
            message,
            retryable,
        }) => (code, retryable, message),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn types_arrive_as_displayable_values() {
    let uri = uri();
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    let sql = "SELECT toInt8(-1) AS i8, toUInt64(18446744073709551615) AS u64, toFloat32(1.5) AS f32, \
        toDecimal64(1.25, 2) AS dec, 'héllo' AS s, toFixedString('ab', 4) AS fs, toDate('2020-01-02') AS d, \
        toDate32('1960-01-02') AS d32, toDateTime('2020-01-02 03:04:05', 'UTC') AS dt, \
        toDateTime64('2020-01-02 03:04:05.123', 3, 'UTC') AS dt64, \
        toUUID('c91bf7e0-92ee-4169-ac36-4ec41d2d4d54') AS u, toIPv4('1.2.3.4') AS ip4, toIPv6('::1') AS ip6, \
        CAST('b' AS Enum8('a' = 1, 'b' = 2)) AS e, CAST('x' AS LowCardinality(String)) AS lc, \
        CAST(NULL AS Nullable(Int32)) AS n, [1, 2] AS arr, map('k', 1) AS m, tuple(1, 'a') AS tp, true AS bo, \
        toInt128(-5) AS i128, toUInt256(7) AS u256, unhex('ff00') AS bin, [toDateTime('2020-01-02 03:04:05', 'UTC')] AS dts";
    let page = ok(conn
        .open_page(sql, 0, 10, false)
        .map_err(|e| Failure::Fail(e.to_string())));
    let labels: Vec<_> = page
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.data_type.as_str()))
        .collect();
    eprintln!("{labels:?}");
    let row = &ok(conn.rows(sql))[0];
    for (column, text) in page.columns.iter().zip(row) {
        eprintln!("{:>5} {:<12} {text}", column.name, column.data_type);
    }
    let expect = |name: &str, text: &str| {
        let index = page.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(row[index], text, "{name}");
    };
    expect("i8", "-1");
    expect("u64", "18446744073709551615");
    expect("dec", "1.25");
    expect("s", "héllo");
    expect("fs", "ab");
    expect("d", "2020-01-02");
    expect("d32", "1960-01-02");
    expect("u", "c91bf7e0-92ee-4169-ac36-4ec41d2d4d54");
    expect("ip4", "1.2.3.4");
    expect("ip6", "::1");
    expect("e", "b");
    expect("lc", "x");
    expect("n", "NULL");
    expect("i128", "-5");
    expect("u256", "7");
    expect("bo", "true");
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn hundred_thousand_rows_page_and_time() {
    let uri = uri();
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    let sql = "SELECT number AS id, toString(number) AS label, number * 0.5 AS score, \
               toDateTime('2020-01-01 00:00:00', 'UTC') + number AS at FROM numbers(250000) ORDER BY id";
    for round in 0..3 {
        let started = Instant::now();
        let page = conn.open_page(sql, 0, 100_000, false).unwrap();
        let took = started.elapsed();
        assert_eq!((page.row_count, page.flags.truncated), (100_000, true));
        eprintln!("round {round}: 100000 rows x 4 columns in {took:?}");
    }
    // Paging walks the whole result, one lookahead row per page, no repeats.
    let mut seen = 0;
    let mut offset = 0;
    loop {
        let page = conn.open_page(sql, offset, 100_000, false).unwrap();
        let first = ok(conn.rows(&format!("SELECT id FROM ({sql}) LIMIT 1 OFFSET {offset}")));
        assert_eq!(first[0][0], offset.to_string());
        seen += page.row_count;
        offset += page.row_count;
        if !page.flags.truncated {
            break;
        }
    }
    assert_eq!(seen, 250_000);
    // A client-paged statement (its own LIMIT) skips rows as they arrive.
    let page = conn
        .open_page(
            "SELECT number FROM numbers(1000) ORDER BY number LIMIT 500",
            450,
            100,
            false,
        )
        .unwrap();
    assert_eq!((page.row_count, page.flags.truncated), (50, false));
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn cancel_stops_a_running_query_and_the_connection_survives() {
    let uri = uri();
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    for sql in [
        "SELECT sleep(3)",
        "SELECT count() FROM numbers(1000000000000) WHERE cityHash64(number) % 1000003 = 7 SETTINGS max_execution_time = 20, max_threads = 1",
    ] {
        let running = ok(conn.spawn_query(sql));
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        conn.cancel(running.operation.id()).unwrap();
        let (code, retryable, _) = code(conn.join(running, Duration::from_secs(10)));
        eprintln!("cancel of `{}...`: {code} after {:?}", sql.chars().take(20).collect::<String>(), started.elapsed());
        assert_eq!((code.as_str(), retryable), ("query.cancelled", false));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(ok(conn.scalar("SELECT 1")), "1");
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn scripts_share_one_http_session_and_formats_are_refused() {
    let uri = uri();
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    assert_eq!(
        ok(conn.scalar("SET max_threads = 3; SELECT getSetting('max_threads')")),
        "3"
    );
    // The session outlives the request.
    assert_eq!(ok(conn.scalar("SELECT getSetting('max_threads')")), "3");
    let (code_, _, message) = code(conn.open_page("SELECT 1 FORMAT JSON", 0, 10, false));
    assert_eq!(code_, "query.failed");
    assert!(message.contains("FORMAT"), "{message}");
    // A syntax error is the server's message, without credentials.
    let (_, _, message) = code(conn.open_page("SELEC 1", 0, 10, false));
    assert!(
        message.contains("SYNTAX_ERROR") && !message.contains("tablex:tablex"),
        "{message}"
    );
    // An empty result keeps its columns.
    let page = conn
        .open_page("SELECT 1 AS a WHERE 0", 0, 10, false)
        .unwrap();
    assert_eq!((page.row_count, page.columns.len()), (0, 1));
    // ClickHouse-only dangerous statements.
    for sql in [
        "OPTIMIZE TABLE system.one FINAL",
        "DETACH TABLE nope",
        "SYSTEM FLUSH LOGS",
        "RENAME TABLE a TO b",
    ] {
        assert_eq!(
            code(conn.open_page(sql, 0, 10, false)).0,
            "query.confirmation_required",
            "{sql}"
        );
    }
}

/// `payload.read_only` text is refused up front when it controls a transaction or is a
/// script with a statement that is not a plain read; plain reads and a lone write (which
/// the server's `readonly = 2` refuses) behave as before. Nothing changes in the table.
#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn read_only_text_is_refused_when_it_controls_a_transaction_or_mixes_in_a_write() {
    struct DropTable(String);
    impl Drop for DropTable {
        fn drop(&mut self) {
            if let Ok(mut conn) = load(&self.0).connect() {
                let _ = conn.exec("DROP TABLE IF EXISTS tablex_ro_ch");
            }
        }
    }
    let uri = uri();
    let _cleanup = DropTable(uri.clone());
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    ok(conn.exec("DROP TABLE IF EXISTS tablex_ro_ch"));
    ok(conn.exec("CREATE TABLE tablex_ro_ch (id UInt32) ENGINE = MergeTree ORDER BY id"));
    ok(conn.exec("INSERT INTO tablex_ro_ch VALUES (1), (2), (3)"));
    let read_only = |conn: &mut Conn, sql: &str| {
        let operation = conn.session.begin_operation().unwrap();
        conn.session.open_query(
            &json!({
                "kind": "sql", "operation_id": operation.id(), "database": null, "object": null,
                "offset": 0, "limit": 10, "payload": { "sql": sql, "read_only": true, "confirmed": true }
            })
            .to_string(),
        )
    };
    let count = |conn: &mut Conn| ok(conn.scalar("SELECT count() FROM tablex_ro_ch"));
    assert_eq!(count(&mut conn), "3");
    for sql in [
        "SELECT 1; COMMIT; INSERT INTO tablex_ro_ch VALUES (9)",
        "COMMIT",
        "BEGIN TRANSACTION",
        "SELECT 1; ROLLBACK",
        "SET autocommit = 1",
        "SELECT 1; SET max_threads = 1",
        "SELECT 1; INSERT INTO tablex_ro_ch VALUES (9)",
        "INSERT INTO tablex_ro_ch VALUES (9); SELECT 1",
    ] {
        let (code_, _, message) = code(read_only(&mut conn, sql));
        assert_eq!(code_, "query.read_only_violation", "{sql}: {message}");
        assert_eq!(count(&mut conn), "3", "nothing changed after: {sql}");
    }
    // A lone write is the server's to refuse (`readonly = 2`).
    assert!(read_only(&mut conn, "INSERT INTO tablex_ro_ch VALUES (9)").is_err());
    assert_eq!(count(&mut conn), "3");
    // Plain reads still run: one statement, a script of reads, EXPLAIN.
    for sql in [
        "SELECT count() FROM tablex_ro_ch",
        "SELECT 1; SELECT id FROM tablex_ro_ch ORDER BY id",
        "EXPLAIN SELECT * FROM tablex_ro_ch",
        "SELECT 'COMMIT; INSERT INTO tablex_ro_ch VALUES (9)'",
    ] {
        let page = read_only(&mut conn, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        conn.session.release_page(page.page_id).unwrap();
    }
    assert_eq!(count(&mut conn), "3");
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn navigator_describe_create_database_and_batch_edit() {
    let uri = uri();
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    let name = format!("cf_live_{}", ctx.unique());
    let cleanup = ctx.fixture; // the fixture's idempotent drop
    let _drop = scopeguard::Drop(&ctx, cleanup.drop_database(&name));

    // create_database
    let create = |conn: &mut Conn, database: &str| {
        let operation = conn.session.begin_operation().unwrap();
        conn.session.mutate(&engine_driver_api::MutationRequest {
            operation_id: operation.id(),
            kind: "create_database".into(),
            database: Some(database.into()),
            object: None,
            identity: Value::Null,
            payload: Value::Null,
        })
    };
    assert_eq!(create(&mut conn, &name).unwrap().affected_rows, 0);
    assert_eq!(
        code(create(&mut conn, &name)).0,
        "mutation.create_database.rejected"
    );

    for sql in [
        format!("CREATE TABLE {name}.events (day Date, tenant Int32, id Int32, label String, note Nullable(String), \
                 INDEX idx_label label TYPE bloom_filter GRANULARITY 4) ENGINE = MergeTree PARTITION BY toYYYYMM(day) \
                 ORDER BY (tenant, id) TTL day + INTERVAL 100 YEAR"),
        format!("CREATE TABLE {name}.mem (id Int32, v Int32) ENGINE = Memory"),
        format!("CREATE VIEW {name}.v AS SELECT id FROM {name}.events"),
        format!("CREATE TABLE {name}.sink (id Int32) ENGINE = MergeTree ORDER BY id"),
        format!("CREATE MATERIALIZED VIEW {name}.mv TO {name}.sink AS SELECT id FROM {name}.events"),
    ] {
        ok(conn.exec(&sql));
    }

    // navigator
    let nodes = conn
        .nav(&json!({"parent": null, "database": name, "payload": {"database": name}, "cursor": null, "limit": 0}))
        .unwrap()
        .nodes;
    let kinds: Vec<_> = nodes
        .iter()
        .map(|n| (n.label.as_str(), n.kind.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("events", "table"),
            ("mem", "table"),
            ("mv", "materialized_view"),
            ("sink", "table"),
            ("v", "view")
        ]
    );
    let databases = conn
        .nav(
            &json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 0}),
        )
        .unwrap();
    assert!(databases.nodes.iter().any(|n| n.label == name));
    let filtered = conn
        .nav(&json!({"parent": null, "database": null, "payload": {"filter": "CF_LIVE"}, "cursor": null, "limit": 0}))
        .unwrap();
    assert!(filtered
        .nodes
        .iter()
        .all(|n| n.label.to_lowercase().contains("cf_live")));

    // describe: engine facts, keys, skipping index, DDL
    let described = conn
        .describe(&json!({"database": name, "object": "events"}))
        .unwrap();
    let keys: Vec<_> = described["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| {
            (
                k["name"].as_str().unwrap().to_owned(),
                k["definition"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    eprintln!("{keys:?}");
    assert!(keys.contains(&("PRIMARY".into(), "tenant, id".into())));
    assert!(keys.contains(&("PARTITION BY".into(), "toYYYYMM(day)".into())));
    assert!(
        keys.iter()
            .any(|(n, d)| n == "TTL" && d.contains("toIntervalYear(100)")),
        "{keys:?}"
    );
    assert_eq!(described["indexes"][0]["name"], "idx_label");
    assert!(described["indexes"][0]["method"]
        .as_str()
        .unwrap()
        .starts_with("bloom_filter GRANULARITY 4"));
    let ddl = described["ddl"].as_str().unwrap();
    assert!(
        ddl.starts_with("-- engine: MergeTree") && ddl.contains("CREATE TABLE"),
        "{ddl}"
    );
    let view = conn
        .describe(&json!({"database": name, "object": "v"}))
        .unwrap();
    assert!(view["ddl"].as_str().unwrap().contains("CREATE VIEW"));

    // batch edit on MergeTree: insert + update + delete, one insert statement
    let mutate = |conn: &mut Conn, table: &str, kind: &str, payload: Value| {
        let operation = conn.session.begin_operation().unwrap();
        conn.session.mutate(&engine_driver_api::MutationRequest {
            operation_id: operation.id(),
            kind: kind.into(),
            database: Some(name.clone()),
            object: Some(table.into()),
            identity: json!({}),
            payload,
        })
    };
    let batch = mutate(
        &mut conn,
        "events",
        "batch",
        json!({"changes": [
            {"op": "insert", "values": {"day": "2024-05-01", "tenant": 1, "id": 1, "label": "a", "note": null}},
            {"op": "insert", "values": {"day": "2024-05-01", "tenant": 1, "id": 2, "label": "b", "note": "n"}},
            {"op": "insert", "values": {"day": "2024-05-02", "tenant": 2, "id": 1, "label": "c"}},
            {"op": "update", "key": {"tenant": 1, "id": 2}, "values": {"label": "B2", "note": null}},
            {"op": "delete", "key": {"tenant": 2, "id": 1}},
        ]}),
    );
    assert_eq!(batch.unwrap().affected_rows, 5);
    assert_eq!(
        ok(conn.rows(&format!(
            "SELECT tenant, id, label, note FROM {name}.events ORDER BY tenant, id"
        ))),
        [["1", "1", "a", "NULL"], ["1", "2", "B2", "NULL"]]
    );
    // Update an existing row (an ALTER with mutations_sync = 2) and refuse key columns.
    let updated = mutate(
        &mut conn,
        "events",
        "update",
        json!({"op": "update", "key": {"tenant": 1, "id": 1}, "values": {"label": "o'brien\\x"}}),
    );
    assert_eq!(updated.unwrap().affected_rows, 1);
    assert_eq!(
        ok(conn.scalar(&format!("SELECT label FROM {name}.events WHERE id = 1"))),
        "o'brien\\x"
    );
    let key_update = mutate(
        &mut conn,
        "events",
        "update",
        json!({"op": "update", "key": {"id": 1}, "values": {"tenant": 9}}),
    );
    assert_eq!(code(key_update).0, "mutation.unsupported");
    // A failing change in a batch changes nothing.
    let failing = mutate(
        &mut conn,
        "events",
        "batch",
        json!({"changes": [
            {"op": "insert", "values": {"day": "2024-05-03", "tenant": 3, "id": 1, "label": "x"}},
            {"op": "delete", "key": {"tenant": 3, "id": 99}},
        ]}),
    );
    assert_eq!(code(failing).0, "mutation.no_rows_affected");
    assert_eq!(
        ok(conn.scalar(&format!(
            "SELECT count() FROM {name}.events WHERE tenant = 3"
        ))),
        "0"
    );
    // Tables that cannot be mutated by key say so; inserts still work.
    let memory_update = mutate(
        &mut conn,
        "mem",
        "update",
        json!({"op": "update", "key": {"id": 1}, "values": {"v": 1}}),
    );
    assert_eq!(code(memory_update).0, "mutation.unsupported");
    let memory_insert = mutate(
        &mut conn,
        "mem",
        "insert",
        json!({"op": "insert", "values": {"id": 1, "v": 2}}),
    );
    assert_eq!(memory_insert.unwrap().affected_rows, 1);
    // Defaults row.
    let defaults = mutate(
        &mut conn,
        "mem",
        "insert",
        json!({"op": "insert", "values": {}}),
    );
    assert_eq!(defaults.unwrap().affected_rows, 1);
    assert_eq!(
        ok(conn.scalar(&format!("SELECT count() FROM {name}.mem"))),
        "2"
    );
}

mod scopeguard {
    use super::Ctx;
    /// Runs cleanup statements when dropped, even if the test panics.
    pub struct Drop<'a>(pub &'a Ctx<'a>, pub Vec<String>);
    impl std::ops::Drop for Drop<'_> {
        fn drop(&mut self) {
            self.0.cleanup(&self.1);
        }
    }
}

/// A cancel delivered at any moment of an operation stops it: the cancel is
/// swept across the start of a long statement (`read_only` or not); a lost
/// cancel shows as a call still running three seconds after it.
#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn a_cancel_at_any_moment_of_a_query_stops_it() {
    use engine_driver_api::{CancelRequest, OperationId};
    let uri = uri();
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    let rounds: usize = std::env::var("TABLEX_SWEEP_ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(150);
    let sql = "SELECT count() FROM numbers(1000000000000) WHERE cityHash64(number) % 1000003 = 7 SETTINGS max_execution_time = 20, max_threads = 1, max_block_size = 4096";
    let request = |id: OperationId, read_only: bool| {
        json!({
            "kind": "sql", "operation_id": id, "database": null, "object": null,
            "offset": 0, "limit": 10, "payload": { "sql": sql, "read_only": read_only }
        })
        .to_string()
    };
    // The span: a few times what a trivial statement takes end to end.
    let mut times = (0..15)
        .map(|_| {
            let started = Instant::now();
            ok(conn.scalar("SELECT 1"));
            started.elapsed()
        })
        .collect::<Vec<_>>();
    times.sort();
    let span = (times[times.len() / 2] * 4).max(Duration::from_millis(4));
    eprintln!(
        "K1 clickhouse sweep: median `SELECT 1` {:?}, span {span:?}",
        times[times.len() / 2]
    );
    let mut failures = Vec::new();
    for read_only in [true, false] {
        let (mut lost, mut wrong, mut unusable, mut worst) =
            (0, Vec::new(), Vec::new(), Duration::ZERO);
        let mut server_memory = 0;
        for round in 0..rounds {
            let delay = span.mul_f64(round as f64 / rounds as f64);
            let operation = conn.session.begin_operation().unwrap();
            let id = operation.id();
            let runner = conn.session.query_runner().unwrap();
            let body = request(id, read_only);
            let connection_id = conn.session.connection_id();
            let (outcome, latency, was_lost) = std::thread::scope(|scope| {
                let (done, finished) = std::sync::mpsc::channel();
                let worker = scope.spawn(move || {
                    let result = runner.open_query(&body);
                    let _ = done.send(Instant::now());
                    result.map(|_| ())
                });
                let target = Instant::now() + delay;
                while Instant::now() < target {
                    std::hint::spin_loop();
                }
                let cancelled_at = Instant::now();
                let cancel = CancelRequest { operation_id: id };
                if let Err(error) = ctx.handle.cancel_operation(connection_id, &cancel) {
                    eprintln!("  round {round}: the cancel call failed: {error}");
                }
                let mut was_lost = false;
                let finished_at = loop {
                    let budget = if was_lost {
                        Duration::from_millis(200)
                    } else {
                        Duration::from_secs(3).saturating_sub(cancelled_at.elapsed())
                    };
                    match finished.recv_timeout(budget) {
                        Ok(at) => break at,
                        Err(_) => {
                            was_lost = true;
                            let _ = ctx.handle.cancel_operation(connection_id, &cancel);
                        }
                    }
                };
                let outcome = match worker.join().unwrap() {
                    Ok(()) => "ok".to_owned(),
                    Err(error) => {
                        let (code, _, message) = code(Err::<(), _>(error));
                        if code == "query.cancelled" {
                            code
                        } else {
                            format!("{code}: {message}")
                        }
                    }
                };
                (
                    outcome,
                    finished_at.saturating_duration_since(cancelled_at),
                    was_lost,
                )
            });
            drop(operation);
            worst = worst.max(latency);
            if was_lost {
                lost += 1;
                eprintln!("  lost: round {round}, cancel after {delay:?}");
            }
            // The test container's memory limit sits just above what the server
            // holds when idle: a statement it picks to stop is its noise, not a cancel.
            if outcome.contains("MEMORY_LIMIT_EXCEEDED") {
                server_memory += 1;
            } else if outcome != "query.cancelled" {
                wrong.push(format!("round {round}: {outcome}"));
            }
            let mut answer = conn.scalar("SELECT 1");
            if matches!(&answer, Err(Failure::Fail(message)) if message.contains("MEMORY_LIMIT_EXCEEDED"))
            {
                server_memory += 1;
                std::thread::sleep(Duration::from_millis(300));
                answer = conn.scalar("SELECT 1");
            }
            match answer {
                Ok(text) if text == "1" => {}
                other => unusable.push(format!("round {round}: {other:?}")),
            }
        }
        eprintln!(
            "K1 clickhouse read_only={read_only}: {rounds} rounds, {lost} lost, {} wrong outcome, {} unusable after, worst cancel latency {worst:?} ({server_memory} server memory-limit errors, not counted)",
            wrong.len(),
            unusable.len()
        );
        if lost > 0 || !wrong.is_empty() || !unusable.is_empty() || worst >= Duration::from_secs(3)
        {
            failures.push(format!("read_only={read_only}: {lost} lost, wrong {wrong:?}, unusable {unusable:?}, worst {worst:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
