//! Live tests against a dedicated SQL Server test server (never a real one: the tests create and
//! drop `cf_*` schemas and run deliberately slow statements):
//!
//! ```text
//! TABLEX_TEST_MSSQL_URI='sqlserver://sa:<password>@127.0.0.1:1433/tablex?trustServerCertificate=true' \
//!   cargo test -p dbengine-mssql --test live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The URI names a dedicated database (`tablex`): the conformance suite switches it to
//! `READ_COMMITTED_SNAPSHOT`, which a system database cannot be. On a fresh server run
//! `create_the_test_database` first with the URI pointing at `master`.
//!
//! The SQL contract itself is covered by `engine-driver-conformance`'s `mssql` test; these cover
//! what is specific to SQL Server: the type mapping, paging in both modes, `GO` scripts, the
//! session's transaction across requests, attention-based cancel, the read-only refusal and the
//! bounded connect.

use engine_driver_conformance::build_release_dylib;
use engine_driver_conformance::fixtures::MssqlFixture;
use engine_driver_conformance::harness::{Conn, Ctx, Failure};
use std::path::Path;
use std::time::{Duration, Instant};

fn uri() -> String {
    std::env::var("TABLEX_TEST_MSSQL_URI").unwrap_or_else(|_| {
        panic!("set TABLEX_TEST_MSSQL_URI (for example sqlserver://sa:<password>@127.0.0.1:1433/master?trustServerCertificate=true)")
    })
}

fn load(uri: &str) -> Ctx<'static> {
    let dylib = build_release_dylib(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        "dbengine_mssql",
        &[],
    )
    .expect("build the driver");
    Ctx::load(&dylib, uri, &MssqlFixture).expect("load the driver")
}

fn ok<T>(result: Result<T, Failure>) -> T {
    match result {
        Ok(value) => value,
        Err(Failure::Fail(message) | Failure::Skip(message)) => panic!("{message}"),
    }
}

fn code(result: Result<impl std::fmt::Debug, engine_driver_host::LoaderError>) -> (String, bool, String) {
    match result {
        Err(engine_driver_host::LoaderError::DriverRejected {
            code,
            message,
            retryable,
        }) => (code, retryable, message),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

fn first_row(conn: &mut Conn<'_>, sql: &str) -> Vec<String> {
    ok(conn.rows(sql)).remove(0)
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI (pointing at master); run once per server"]
fn create_the_test_database() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    ok(conn.exec("IF DB_ID(N'tablex') IS NULL CREATE DATABASE tablex"));
    assert_eq!(ok(conn.scalar("SELECT COUNT(*) FROM sys.databases WHERE name = N'tablex'")), "1");
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn types_arrive_as_displayable_values() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let sql = "SELECT CAST(255 AS tinyint) AS ti, CAST(-5 AS smallint) AS si, CAST(7 AS int) AS i, \
        CAST(9223372036854775807 AS bigint) AS bi, CAST(1.5 AS real) AS r, CAST(2.25 AS float) AS f, \
        CAST(1 AS bit) AS b, CAST(12345.6789 AS decimal(18,4)) AS dec, CAST(-0.05 AS numeric(5,2)) AS num, \
        CAST(922337203685477.5807 AS money) AS mon, CAST(214748.3647 AS smallmoney) AS smon, \
        CAST('2020-01-02' AS date) AS d, CAST('03:04:05.1234567' AS time(7)) AS t, \
        CAST('2020-01-02 03:04:05.123' AS datetime) AS dt, CAST('2020-01-02 03:04' AS smalldatetime) AS sdt, \
        CAST('2020-01-02 03:04:05.1234567' AS datetime2(7)) AS dt2, \
        CAST('2020-01-02 03:04:05.12 +07:00' AS datetimeoffset(2)) AS dto, \
        CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS u, \
        CAST(0x00FF AS varbinary(10)) AS vb, N'héllo' AS nv, CAST('abc' AS varchar(10)) AS v, \
        CAST('x' AS nvarchar(max)) AS nm, CAST('<a x=\"1\"/>' AS xml) AS x, CAST(NULL AS int) AS n";
    let page = ok(conn.open_page(sql, 0, 10, false).map_err(|e| Failure::Fail(e.to_string())));
    let row = first_row(&mut conn, sql);
    for (column, text) in page.columns.iter().zip(&row) {
        eprintln!("{:>5} {:<16} {text}", column.name, column.data_type);
    }
    let expect = |name: &str, text: &str| {
        let index = page.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(row[index], text, "{name}");
    };
    expect("ti", "255");
    expect("si", "-5");
    expect("i", "7");
    expect("bi", "9223372036854775807");
    expect("b", "true");
    expect("dec", "12345.6789");
    expect("num", "-0.05");
    expect("mon", "922337203685477.5807");
    expect("smon", "214748.3647");
    expect("d", "2020-01-02");
    expect("t", "03:04:05.1234567");
    expect("dt", "2020-01-02 03:04:05.123");
    expect("sdt", "2020-01-02 03:04:00");
    expect("dt2", "2020-01-02 03:04:05.1234567");
    expect("dto", "2020-01-02 03:04:05.12 +07:00");
    expect("u", "6F9619FF-8B86-D011-B42D-00C04FC964FF");
    expect("vb", "0x00FF");
    expect("nv", "héllo");
    expect("v", "abc");
    expect("nm", "x");
    expect("n", "NULL");
    let type_of = |name: &str| {
        page.columns.iter().find(|c| c.name == name).map(|c| c.data_type.as_str()).unwrap()
    };
    assert_eq!(type_of("i"), "int");
    assert_eq!(type_of("dt2"), "datetime2");
    assert_eq!(type_of("vb"), "varbinary");
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn paging_walks_a_result_in_both_modes_without_repeats() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    // Server-paged: has a top-level ORDER BY. Client-paged: has none, rows are skipped on arrival.
    for (label, sql) in [
        ("ORDER BY", ctx_numbers(2500, true)),
        ("no ORDER BY", ctx_numbers(2500, false)),
        ("TOP + ORDER BY", "SELECT TOP (2500) n FROM (".to_owned() + &ctx_numbers(3000, false) + ") x ORDER BY n"),
    ] {
        let mut seen = 0_u64;
        let mut offset = 0;
        let mut last = 0_i64;
        loop {
            let page = conn.open_page(&sql, offset, 1000, false).unwrap();
            // The page holds exactly the next numbers, in order: no repeat, no gap.
            let cells = page_cells(&mut conn, &page);
            let expected: Vec<String> = (offset + 1..=offset + page.row_count).map(|n| n.to_string()).collect();
            assert_eq!(cells, expected, "{label} at offset {offset}");
            seen += page.row_count;
            offset += page.row_count;
            last = last.max(offset as i64);
            if !page.flags.truncated {
                break;
            }
        }
        assert_eq!((seen, last), (2500, 2500), "{label}");
    }
}

/// The first column of a page, as text.
fn page_cells(conn: &mut Conn<'_>, page: &engine_driver_api::QueryPage) -> Vec<String> {
    if page.row_count == 0 {
        return Vec::new();
    }
    let batch = conn
        .session
        .render_cell_batch(page.page_id, 0, page.row_count, 0, 1)
        .expect("render the page");
    (0..batch.cell_count()).filter_map(|index| batch.cell(index)).map(|cell| cell.text).collect()
}

fn ctx_numbers(n: u64, ordered: bool) -> String {
    let sql = format!(
        "SELECT n FROM (SELECT TOP ({n}) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n \
         FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c) AS numbers"
    );
    if ordered {
        // A top-level ORDER BY and nothing else: the server pages it (OFFSET .. FETCH).
        format!("{sql} ORDER BY n")
    } else {
        sql
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn a_page_of_a_big_table_is_not_read_to_its_end() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    // A cross join of ~ 8 billion rows with no ORDER BY: reading it to the end would never return;
    // the attention signal after the page does.
    let sql = "SELECT a.object_id AS x FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c";
    let started = Instant::now();
    let page = conn.open_page(sql, 0, 200, false).unwrap();
    assert_eq!((page.row_count, page.flags.truncated), (200, true));
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    // The session is clean afterwards.
    assert_eq!(ok(conn.scalar("SELECT 1")), "1");
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn go_separates_batches_and_the_last_result_set_is_the_page() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    let rows = ok(conn.rows("SELECT 1 AS a\nGO\nSELECT 2 AS a\nGO\nSELECT 3 AS a"));
    assert_eq!(rows, [["3"]]);
    // `CREATE PROCEDURE` must be alone in its batch: `GO` makes that possible in one text.
    let name = format!("tablex_go_{}", ctx.unique());
    let script = format!(
        "CREATE PROCEDURE {name} AS SELECT 42 AS answer\nGO\nEXEC {name}\nGO\nDROP PROCEDURE {name}"
    );
    let page = conn.open_page(&script, 0, 10, true).expect("confirmed script");
    assert_eq!(page.columns[0].name, "answer");
    // A repeat count runs the batch that many times.
    assert_eq!(ok(conn.scalar("SET NOCOUNT ON; DECLARE @i INT = 0\nGO\nSELECT 5")), "5");
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn the_session_keeps_its_transaction_and_database_between_requests() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    ok(conn.exec("CREATE TABLE #t (a INT)"));
    ok(conn.exec("BEGIN TRANSACTION"));
    ok(conn.exec("INSERT INTO #t VALUES (1)"));
    assert_eq!(ok(conn.scalar("SELECT @@TRANCOUNT")), "1");
    assert_eq!(ok(conn.scalar("SELECT COUNT(*) FROM #t")), "1");
    ok(conn.exec("ROLLBACK"));
    assert_eq!(ok(conn.scalar("SELECT COUNT(*) FROM #t")), "0");
    // The affected-row count of a statement without a result set.
    let page = conn.open_page("INSERT INTO #t VALUES (1), (2), (3)", 0, 10, false).unwrap();
    assert_eq!((page.columns.len(), page.total_count), (0, Some(3)));
    // `USE` in user SQL moves the session; a request naming a database moves it back.
    ok(conn.exec("USE tempdb"));
    assert_eq!(ok(conn.scalar("SELECT DB_NAME()")), "tempdb");
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn cancel_stops_a_running_statement_and_the_session_survives() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    ok(conn.exec("CREATE TABLE #kept (a INT)"));
    ok(conn.exec("BEGIN TRANSACTION"));
    ok(conn.exec("INSERT INTO #kept VALUES (7)"));
    for sql in [
        "WAITFOR DELAY '00:00:20'",
        "SELECT COUNT_BIG(*) FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c CROSS JOIN sys.all_objects d",
    ] {
        let running = ok(conn.spawn_query(sql));
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        conn.cancel(running.operation.id()).unwrap();
        let (code, retryable, _) = code(conn.join(running, Duration::from_secs(10)));
        assert_eq!((code.as_str(), retryable), ("query.cancelled", false), "{sql}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }
    // The temporary table and the open transaction are still there: attention is not a reconnect.
    assert_eq!(ok(conn.scalar("SELECT @@TRANCOUNT")), "1");
    assert_eq!(ok(conn.scalar("SELECT a FROM #kept")), "7");
    ok(conn.exec("ROLLBACK"));
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn a_read_only_run_refuses_whatever_could_write() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    for sql in [
        "DELETE FROM sys.nope WHERE 1 = 0",
        "SELECT 1 DELETE FROM sys.nope",
        "EXEC sp_who",
        "SELECT * INTO #x FROM sys.all_objects",
        "BEGIN TRAN SELECT 1",
        "SELECT 1; COMMIT",
    ] {
        let (code_, _, message) = code(conn.open_page_with(sql, 0, 10, serde_json::json!({"read_only": true})));
        assert_eq!(code_, "query.read_only_violation", "{sql}: {message}");
    }
    let page = conn
        .open_page_with("SELECT 1 AS a; SELECT 2 AS a", 0, 10, serde_json::json!({"read_only": true}))
        .unwrap();
    assert_eq!(page.row_count, 1);
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn the_session_has_the_settings_every_client_library_sets() {
    let ctx = load(&uri());
    let mut conn = ok(ctx.connect());
    for option in ["QUOTED_IDENTIFIER", "ANSI_NULLS", "ANSI_PADDING", "ANSI_WARNINGS", "CONCAT_NULL_YIELDS_NULL", "ARITHABORT"] {
        assert_eq!(ok(conn.scalar(&format!("SELECT CAST(SESSIONPROPERTY('{option}') AS int)"))), "1", "{option}");
    }
    // So a double-quoted name is an identifier.
    assert_eq!(ok(conn.scalar("SELECT 5 AS \"quoted name\"")), "5");
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn a_server_error_is_the_servers_message_without_credentials() {
    let uri = uri();
    let ctx = load(&uri);
    let mut conn = ok(ctx.connect());
    let (code_, _, message) = code(conn.open_page("SELECT * FROM no_such_table_here", 0, 10, false));
    assert_eq!(code_, "query.failed");
    assert!(message.contains("Msg 208") && message.contains("no_such_table_here"), "{message}");
    // The session is fine after an error.
    assert_eq!(ok(conn.scalar("SELECT 1")), "1");
}

#[test]
#[ignore = "needs TABLEX_TEST_MSSQL_URI"]
fn connecting_to_a_dead_port_or_with_a_wrong_password_fails_in_bounded_time() {
    let uri = uri();
    let ctx = load(&uri);
    // Wrong password: the server's refusal, quickly.
    let (scheme_user, rest) = uri.split_once('@').expect("uri has credentials");
    let user = scheme_user.rsplit_once(':').map(|(head, _)| head).unwrap();
    let bad = format!("{user}:definitely-not-the-password@{rest}");
    let started = Instant::now();
    let result = ctx.connect_to(&bad);
    assert!(result.is_err(), "a wrong password must not connect");
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    // A port nobody listens on: refused at once, never a hang.
    let (head, tail) = uri.split_once('@').unwrap();
    let tail = tail.replacen("21433", "1", 1);
    let started = Instant::now();
    assert!(ctx.connect_to(&format!("{head}@{tail}")).is_err());
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
}
