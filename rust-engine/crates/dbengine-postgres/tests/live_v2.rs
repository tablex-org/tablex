//! Live tests of the ABI v2 PostgreSQL driver through the real host loader.
//!
//! Each test builds the `--features abi-v2` release dylib (once per process),
//! loads a staged copy with `engine_driver_host::DriverLoader`, and talks to the
//! database named by `TABLEX_TEST_PG_URI` (a dedicated test server:
//! `postgres://tablex:tablex@127.0.0.1:55432/tablex`). Every test works in its
//! own uniquely named schema and drops it afterwards.
//!
//! ```text
//! TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//!   cargo test -p dbengine-postgres --features abi-v2 -- --ignored --nocapture --test-threads=1
//! ```
#![cfg(feature = "abi-v2")]

use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use engine_driver_host::{DriverHandle, DriverLoader, DriverSession, LoaderError};
use engine_driver_kit::api::sql::{SqlColumnNode, SqlObjectRef, SqlTableIdentity, TableProperties};
use engine_driver_kit::api::{
    CancelRequest, Capability, MutationRequest, NavigatorPage, OperationId, QueryPage,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// The dedicated test server. Every test here is `#[ignore]`d, so asking for one
/// (`-- --ignored`) without the server is an error, not a silent pass.
fn pg_uri() -> String {
    std::env::var("TABLEX_TEST_PG_URI").unwrap_or_else(|_| {
        panic!(
            "set TABLEX_TEST_PG_URI (for example postgres://tablex:tablex@127.0.0.1:55432/tablex)"
        )
    })
}

/// Builds the v2 release dylib once per test process and returns its path.
fn built_driver() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let output = Command::new(env!("CARGO"))
            .args([
                "build",
                "--release",
                "--features",
                "abi-v2",
                "--manifest-path",
            ])
            .arg(&manifest)
            .output()
            .expect("run cargo build");
        assert!(
            output.status.success(),
            "driver build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata = Command::new(env!("CARGO"))
            .args([
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
            ])
            .arg(&manifest)
            .output()
            .expect("run cargo metadata");
        let metadata: Value = serde_json::from_slice(&metadata.stdout).expect("metadata JSON");
        let target = metadata["target_directory"].as_str().expect("target dir");
        Path::new(target)
            .join("release")
            .join("libdbengine_postgres.dylib")
    })
}

fn stage(temp: &TempDir) -> PathBuf {
    let staged = temp.path().join("libdbengine_postgres.dylib");
    std::fs::copy(built_driver(), &staged).expect("stage driver");
    staged
}

static SCHEMA_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A connected session plus a private schema that is dropped on exit.
struct Harness {
    _temp: TempDir,
    handle: Arc<DriverHandle>,
    session: DriverSession,
    schema: String,
    uri: String,
}

impl Harness {
    fn new(uri: &str) -> Self {
        let temp = TempDir::new().expect("temp dir");
        let path = stage(&temp);
        let handle = DriverLoader.load(&path).expect("load PostgreSQL driver");
        let mut session = handle
            .connect(&json!({ "uri": uri }).to_string())
            .expect("connect");
        let schema = format!(
            "pgv2_{}_{}",
            std::process::id(),
            SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let page = run(&mut session, &format!("CREATE SCHEMA {schema}")).expect("create schema");
        session.release_page(page.page_id).expect("release");
        Self {
            _temp: temp,
            handle,
            session,
            schema,
            uri: uri.to_owned(),
        }
    }

    fn sql(&mut self, sql: &str) -> Result<QueryPage, LoaderError> {
        run(&mut self.session, sql)
    }

    /// Executes `sql` (with `{s}` replaced by the schema) and discards the page.
    fn exec(&mut self, sql: &str) -> QueryPage {
        let sql = sql.replace("{s}", &self.schema);
        let page = self
            .sql(&sql)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        self.session.release_page(page.page_id).expect("release");
        page
    }

    /// Runs a page query and imports its rows as an Arrow batch.
    fn query(&mut self, sql: &str, offset: u64, limit: u64) -> (QueryPage, RecordBatch) {
        let sql = sql.replace("{s}", &self.schema);
        let page = self
            .page(&sql, offset, limit)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let lease = self
            .session
            .acquire_arrow_page(page.page_id, 0, page.row_count)
            .expect("acquire arrow page");
        // SAFETY: the lease was just returned by the driver's Arrow export.
        let imported = unsafe { lease.into_record_batch() }.expect("import arrow page");
        (page, imported.batch)
    }

    fn page(&mut self, sql: &str, offset: u64, limit: u64) -> Result<QueryPage, LoaderError> {
        let operation = self.session.begin_operation().expect("operation");
        let request = json!({
            "kind": "sql", "operation_id": operation.id(), "database": null, "object": null,
            "offset": offset, "limit": limit, "payload": { "sql": sql, "confirmed": true }
        });
        self.session.open_query(&request.to_string())
    }

    fn nav(&mut self, request: Value) -> NavigatorPage {
        self.session
            .list_nav_nodes(&request.to_string())
            .unwrap_or_else(|error| panic!("{request}: {error}"))
    }

    fn mutate(
        &mut self,
        kind: &str,
        object: Option<&str>,
        identity: Value,
        payload: Value,
    ) -> Result<engine_driver_kit::api::MutationResult, LoaderError> {
        let operation = self.session.begin_operation().expect("operation");
        self.session.mutate(&MutationRequest {
            operation_id: operation.id(),
            kind: kind.into(),
            database: None,
            object: object.map(String::from),
            identity,
            payload,
        })
    }

    /// First text cell of the first row.
    fn scalar(&mut self, sql: &str) -> String {
        let (_, batch) = self.query(sql, 0, 10);
        batch.column(0).as_string::<i32>().value(0).to_owned()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let drop_sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema);
        match run(&mut self.session, &drop_sql) {
            Ok(page) => {
                let _ = self.session.release_page(page.page_id);
            }
            // The test disconnected its session: clean up over a fresh one.
            Err(_) => {
                if let Ok(mut session) =
                    self.handle.connect(&json!({ "uri": self.uri }).to_string())
                {
                    if let Ok(page) = run(&mut session, &drop_sql) {
                        let _ = session.release_page(page.page_id);
                    }
                    let _ = session.disconnect();
                }
            }
        }
        let _ = self.session.disconnect();
    }
}

fn run(session: &mut DriverSession, sql: &str) -> Result<QueryPage, LoaderError> {
    let operation = session.begin_operation()?;
    let request = json!({
        "kind": "sql", "operation_id": operation.id(), "database": null, "object": null,
        "offset": 0, "limit": 100, "payload": { "sql": sql, "confirmed": true }
    });
    session.open_query(&request.to_string())
}

fn rejected_code(error: &LoaderError) -> &str {
    match error {
        LoaderError::DriverRejected { code, .. } => code,
        other => panic!("expected DriverRejected, got {other}"),
    }
}

fn names(page: &NavigatorPage) -> Vec<String> {
    page.nodes.iter().map(|node| node.label.clone()).collect()
}

fn text(batch: &RecordBatch, column: usize, row: usize) -> String {
    batch
        .column(column)
        .as_string::<i32>()
        .value(row)
        .to_owned()
}

// ---------------------------------------------------------------------------
// capabilities, connect, disconnect
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn capabilities_connect_and_disconnect() {
    let uri = pg_uri();
    let temp = TempDir::new().unwrap();
    let handle = DriverLoader.load(&stage(&temp)).expect("load");

    let capabilities = handle.capabilities();
    assert_eq!(capabilities.driver, "postgresql");
    assert_eq!(capabilities.uri_schemes, ["postgres", "postgresql"]);
    assert_eq!(capabilities.editors, ["sql"]);
    for capability in [
        Capability::Navigator,
        Capability::QuerySql,
        Capability::QueryArrowCData,
        Capability::OperationCancel,
        Capability::MutationInsert,
        Capability::MutationUpdate,
        Capability::MutationDelete,
        Capability::MutationBatch,
        Capability::DatabaseCreate,
        Capability::MetadataTableProperties,
    ] {
        assert!(capabilities.has(capability), "{capability}");
    }
    assert!(!capabilities.has(Capability::QueryPageLease));
    assert!(capabilities.supports_mutation_kind("create_database"));
    let dialect = capabilities.sql_dialect.as_ref().expect("dialect");
    assert_eq!(
        (
            dialect.identifier_quote.as_str(),
            dialect.string_literal_quote.as_str(),
            dialect.supports_schemas,
            dialect.database_qualified_names,
            dialect.like_requires_text_cast
        ),
        ("\"", "'", true, false, true)
    );
    assert_eq!(
        capabilities.navigator_kinds,
        [
            "database",
            "schema",
            "table",
            "view",
            "materialized_view",
            "function",
            "procedure",
            "sequence",
            "trigger",
            "type",
            "column"
        ]
    );

    let mut session = handle
        .connect(&json!({ "uri": uri }).to_string())
        .expect("connect");
    let page = run(&mut session, "SELECT 1").expect("query on the connection");
    assert_eq!(page.row_count, 1);
    session.release_page(page.page_id).unwrap();
    session.disconnect().expect("disconnect");
    assert!(
        session
            .list_nav_nodes(
                r#"{"parent":null,"database":null,"payload":null,"cursor":null,"limit":10}"#
            )
            .is_err(),
        "disconnected session rejects calls"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn connect_reports_the_server_version_in_the_hosts_numeric_form() {
    let mut h = Harness::new(&pg_uri());
    // `server_version_num` is `major * 10000 + minor` from 10 on (16.2 = 160002); the host
    // keeps MySQL's `major * 10000 + minor * 100 + patch` (160200).
    let num: u32 = h.scalar("SHOW server_version_num").parse().expect("a number");
    let expected = if num >= 100_000 {
        (num / 10_000) * 10_000 + (num % 10_000) * 100
    } else {
        num
    };
    assert_eq!(h.session.server_version(), Some(expected), "server_version_num is {num}");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn connect_failures_are_typed_and_do_not_leak_credentials() {
    let uri = pg_uri();
    let temp = TempDir::new().unwrap();
    let handle = DriverLoader.load(&stage(&temp)).expect("load");

    let bad_scheme = handle
        .connect(r#"{"uri":"mysql://root:hunter2@localhost/db"}"#)
        .err()
        .expect("rejected");
    assert_eq!(rejected_code(&bad_scheme), "connect.bad_request");
    assert!(!bad_scheme.to_string().contains("hunter2"));

    // Right server, wrong password: the message must not echo the URI.
    let wrong_password = uri.replace("tablex:tablex@", "tablex:s3cr3tWRONG@");
    assert_ne!(
        wrong_password, uri,
        "the test URI must use user:password tablex:tablex"
    );
    let error = handle
        .connect(&json!({ "uri": wrong_password }).to_string())
        .err()
        .expect("authentication fails");
    assert_eq!(rejected_code(&error), "connect.failed");
    assert!(!error.to_string().contains("s3cr3tWRONG"), "{error}");
}

// ---------------------------------------------------------------------------
// navigator
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn navigator_levels_filter_and_cursor_paging() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.alpha_t (id bigint PRIMARY KEY, \"Name\" text NOT NULL DEFAULT 'x', score numeric(8,2))");
    h.exec("CREATE TABLE {s}.beta_t (id int)");
    h.exec("CREATE TABLE {s}.a_b (id int)");
    h.exec("CREATE TABLE {s}.axb (id int)");
    h.exec("CREATE TABLE {s}.pct (id int)");
    h.exec("CREATE VIEW {s}.alpha_v AS SELECT id FROM {s}.alpha_t");
    h.exec("CREATE MATERIALIZED VIEW {s}.alpha_m AS SELECT id FROM {s}.alpha_t");
    let schema = h.schema.clone();

    // Databases.
    let databases =
        h.nav(json!({"parent":null,"database":null,"payload":null,"cursor":null,"limit":100}));
    let current = h.scalar("SELECT current_database()");
    assert!(
        names(&databases).contains(&current),
        "{:?}",
        names(&databases)
    );
    let database = databases.nodes.iter().find(|n| n.label == current).unwrap();
    assert_eq!(database.kind, "database");
    assert!(database.has_children);
    assert_eq!(database.payload, json!({ "database": current }));

    // Schemas of that database (only schemas with readable objects).
    let schemas = h.nav(json!({
        "parent": database.id, "database": current, "payload": database.payload, "cursor": null, "limit": 500
    }));
    let schema_node = schemas
        .nodes
        .iter()
        .find(|n| n.label == schema)
        .expect("test schema");
    assert_eq!(schema_node.kind, "schema");
    assert_eq!(
        schema_node.payload,
        json!({"database": current, "schema": schema})
    );
    assert!(!names(&schemas).contains(&"pg_catalog".to_string()));

    // Objects: tables, views and materialized views, native order (relkind, name).
    let objects_request = |filter: Option<&str>, cursor: Option<&str>, limit: u64| {
        let mut payload = json!({"database": current, "schema": schema});
        if let Some(filter) = filter {
            payload["filter"] = json!(filter);
        }
        json!({"parent": schema_node.id, "database": current, "payload": payload, "cursor": cursor, "limit": limit})
    };
    let all = h.nav(objects_request(None, None, 100));
    let kinds: Vec<(String, String)> = all
        .nodes
        .iter()
        .map(|n| (n.kind.clone(), n.label.clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("materialized_view", "alpha_m"),
            ("table", "a_b"),
            ("table", "alpha_t"),
            ("table", "axb"),
            ("table", "beta_t"),
            ("table", "pct"),
            ("view", "alpha_v"),
        ]
        .map(|(k, n)| (k.to_string(), n.to_string()))
    );
    assert!(all.next_cursor.is_none());

    // Filtered (case-insensitive substring, LIKE wildcards are literal).
    assert_eq!(
        names(&h.nav(objects_request(Some("ALPHA"), None, 100))),
        ["alpha_m", "alpha_t", "alpha_v"]
    );
    assert_eq!(
        names(&h.nav(objects_request(Some("a_b"), None, 100))),
        ["a_b"]
    );
    assert!(names(&h.nav(objects_request(Some("%"), None, 100))).is_empty());
    assert!(names(&h.nav(objects_request(Some("zzz"), None, 100))).is_empty());

    // Cursor paging walks every object exactly once.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = h.nav(objects_request(None, cursor.as_deref(), 3));
        assert!(page.nodes.len() <= 3);
        seen.extend(names(&page));
        pages += 1;
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, names(&all));
    // A filtered cursor chain too.
    let first = h.nav(objects_request(Some("alpha"), None, 2));
    assert_eq!(names(&first), ["alpha_m", "alpha_t"]);
    let second = h.nav(objects_request(
        Some("alpha"),
        first.next_cursor.as_deref(),
        2,
    ));
    assert_eq!(names(&second), ["alpha_v"]);
    assert!(second.next_cursor.is_none());

    // Columns of a table, with the object node's payload as the request payload.
    let table = all.nodes.iter().find(|n| n.label == "alpha_t").unwrap();
    assert_eq!(
        table.payload,
        json!({"database": current, "schema": schema, "object": "alpha_t"})
    );
    let columns = h.nav(json!({
        "parent": table.id, "database": current, "payload": table.payload, "cursor": null, "limit": 100
    }));
    assert_eq!(names(&columns), ["id", "Name", "score"]);
    assert!(columns
        .nodes
        .iter()
        .all(|n| n.kind == "column" && !n.has_children));
    assert_eq!(columns.nodes[0].subtitle.as_deref(), Some("bigint"));
    assert_eq!(columns.nodes[0].payload["primary_key"], true);
    assert_eq!(columns.nodes[1].payload["default_expression"], "'x'::text");
    assert_eq!(columns.nodes[1].payload["nullable"], false);
    assert_eq!(columns.nodes[2].subtitle.as_deref(), Some("numeric(8,2)"));
    // Typed payloads: object nodes are exactly a `SqlObjectRef`, column nodes a
    // `SqlColumnNode`, and database/schema nodes carry just their names.
    for node in &all.nodes {
        assert_eq!(
            node.payload.as_object().unwrap().len(),
            3,
            "{}",
            node.payload
        );
        let reference: SqlObjectRef = serde_json::from_value(node.payload.clone()).unwrap();
        assert_eq!(reference.database.as_deref(), Some(current.as_str()));
        assert_eq!(reference.schema.as_deref(), Some(schema.as_str()));
        assert_eq!(reference.object, node.label);
    }
    for (node, expected) in columns.nodes.iter().zip(["id", "Name", "score"]) {
        let typed: SqlColumnNode = serde_json::from_value(node.payload.clone()).unwrap();
        assert_eq!(typed.column, expected);
        assert_eq!(typed.details.name, expected);
        assert_eq!(typed.object.object, "alpha_t");
        assert_eq!(typed.object.schema.as_deref(), Some(schema.as_str()));
    }
    let typed: SqlColumnNode = serde_json::from_value(columns.nodes[0].payload.clone()).unwrap();
    assert!(typed.details.primary_key && !typed.details.nullable);
    assert_eq!(typed.details.data_type, "bigint");
    // A column node's payload echoed back still lists that table's columns.
    let echoed = h.nav(json!({
        "parent": columns.nodes[0].id, "database": current, "payload": columns.nodes[0].payload, "cursor": null, "limit": 100
    }));
    assert_eq!(names(&echoed), ["id", "Name", "score"]);

    // Column paging.
    let one = h.nav(json!({"parent": table.id, "database": current, "payload": table.payload, "cursor": null, "limit": 2}));
    assert_eq!(names(&one), ["id", "Name"]);
    let rest = h.nav(json!({"parent": table.id, "database": current, "payload": table.payload, "cursor": one.next_cursor, "limit": 2}));
    assert_eq!(names(&rest), ["score"]);

    // Malformed navigation is rejected, not guessed.
    let error = h
        .session
        .list_nav_nodes(&json!({"parent":null,"database":null,"payload":{"schema":"public"},"cursor":null,"limit":10}).to_string())
        .expect_err("schema without database");
    assert_eq!(rejected_code(&error), "navigator.bad_request");
    let error = h
        .session
        .list_nav_nodes(
            &json!({"parent":null,"database":null,"payload":null,"cursor":"bogus","limit":10})
                .to_string(),
        )
        .expect_err("bad cursor");
    assert_eq!(rejected_code(&error), "navigator.bad_request");
}

// ---------------------------------------------------------------------------
// queries
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn query_page_types_nulls_and_numeric_as_text() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TYPE {s}.mood AS ENUM ('sad', 'happy')");
    h.exec("CREATE DOMAIN {s}.posint AS int CHECK (VALUE > 0)");
    h.exec(
        "CREATE TABLE {s}.types (id bigint PRIMARY KEY, i2 smallint, i4 int, f4 real, f8 double precision,
            n numeric(12,2), nbig numeric, t text, b boolean, ts timestamp, tstz timestamptz, d date,
            tm time, j json, jb jsonb, u uuid, by bytea, iv interval, arr int[], ip inet, m {s}.mood)",
    );
    h.exec(
        "INSERT INTO {s}.types VALUES
          (1, 2, 4, 1.5, 2.25, 12345.67, 123456789012345678901234567890.123456789, 'héllo ☃', true,
           '2024-01-02 03:04:05.25', '2024-01-02 03:04:05.25+00', '2024-01-02', '03:04:05',
           '{\"a\": 1}', '{\"b\":[1,2]}', '123e4567-e89b-12d3-a456-426614174000', '\\x00ff',
           '1 day 02:03:04', '{1,2,3}', '10.0.0.1', 'happy'),
          (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
           NULL, NULL, NULL, NULL, NULL, NULL)",
    );
    let (page, batch) = h.query("SELECT * FROM {s}.types ORDER BY id", 0, 100);
    assert_eq!((page.row_count, page.total_count), (2, None));
    assert!(!page.total_is_exact && !page.flags.truncated && page.flags.read_only);
    assert_eq!(page.columns.len(), 21);
    assert_eq!(page.columns[0].name, "id");
    assert_eq!(page.columns[0].data_type, "int64");
    assert_eq!(page.columns[3].data_type, "float32");
    assert_eq!(page.columns[4].data_type, "float64");
    assert_eq!(page.columns[7].data_type, "text");
    assert_eq!(page.columns[8].data_type, "boolean");

    let expect = |name: &str, kind: &DataType| {
        let index = batch.schema().index_of(name).unwrap();
        assert_eq!(batch.schema().field(index).data_type(), kind, "{name}");
    };
    for name in ["id", "i2", "i4"] {
        expect(name, &DataType::Int64);
    }
    expect("f4", &DataType::Float32);
    expect("f8", &DataType::Float64);
    expect("b", &DataType::Boolean);
    // numeric, timestamps, json, uuid, bytea, interval, arrays, inet and enums: UTF-8.
    for name in [
        "n", "nbig", "t", "ts", "tstz", "d", "tm", "j", "jb", "u", "by", "iv", "arr", "ip", "m",
    ] {
        expect(name, &DataType::Utf8);
    }
    assert_eq!(batch.num_columns(), 21);

    let col = |name: &str| batch.schema().index_of(name).unwrap();
    assert_eq!(
        batch
            .column(col("id"))
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(0),
        1
    );
    assert_eq!(
        batch
            .column(col("i2"))
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(0),
        2
    );
    assert_eq!(
        batch
            .column(col("i4"))
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(0),
        4
    );
    assert_eq!(
        batch
            .column(col("f4"))
            .as_primitive::<arrow_array::types::Float32Type>()
            .value(0),
        1.5
    );
    assert_eq!(
        batch
            .column(col("f8"))
            .as_primitive::<arrow_array::types::Float64Type>()
            .value(0),
        2.25
    );
    assert!(batch.column(col("b")).as_boolean().value(0));
    let cell = |name: &str, row: usize| text(&batch, col(name), row);
    assert_eq!(cell("n", 0), "12345.67");
    assert_eq!(cell("nbig", 0), "123456789012345678901234567890.123456789");
    assert_eq!(cell("t", 0), "héllo ☃");
    // The server's own output: the fraction loses its trailing zeros, timestamptz
    // carries the session's offset.
    assert_eq!(cell("ts", 0), "2024-01-02 03:04:05.25");
    assert_eq!(cell("tstz", 0), "2024-01-02 03:04:05.25+00");
    assert_eq!(cell("d", 0), "2024-01-02");
    assert_eq!(cell("tm", 0), "03:04:05");
    assert_eq!(cell("j", 0), "{\"a\": 1}");
    assert_eq!(cell("jb", 0), "{\"b\": [1, 2]}");
    assert_eq!(cell("u", 0), "123e4567-e89b-12d3-a456-426614174000");
    assert_eq!(cell("by", 0), "\\x00ff");
    // Types without a client-side decoder come from the server's own text output.
    assert_eq!(cell("iv", 0), "1 day 02:03:04");
    assert_eq!(cell("arr", 0), "{1,2,3}");
    // What psql prints (the inet output function): a host address has no `/32`
    // (the `::text` cast would add it).
    assert_eq!(cell("ip", 0), "10.0.0.1");
    assert_eq!(cell("m", 0), "happy");

    // Domains arrive as their base type.
    let (_, domain) = h.query("SELECT 5::{s}.posint AS d, 'sad'::{s}.mood AS m", 0, 10);
    assert_eq!(domain.schema().field(0).data_type(), &DataType::Int64);
    assert_eq!(text(&domain, 1, 0), "sad");
    for column in 1..21 {
        assert!(
            batch.column(column).is_null(1),
            "column {column} of the NULL row"
        );
    }
    assert!(!batch.column(0).is_null(1));

    // The native type name rides along in the Arrow field metadata.
    let native = |name: &str| {
        batch
            .schema()
            .field(col(name))
            .metadata()
            .get("tablex.native_type")
            .cloned()
    };
    assert_eq!(native("id").as_deref(), Some("bigint"));
    assert_eq!(native("n").as_deref(), Some("numeric"));
    assert_eq!(native("ts").as_deref(), Some("timestamp without time zone"));
    assert_eq!(native("iv").as_deref(), Some("interval"));
    assert_eq!(native("arr").as_deref(), Some("integer[]"));

    // The kit's cell rendering over the same page: text as is, NULL as NULL.
    let rendered = h
        .session
        .render_cell_batch(page.page_id, 0, 2, 0, 21)
        .expect("render");
    let find = |row: u64, column: u64| {
        (0..rendered.cell_count())
            .filter_map(|i| rendered.cell(i))
            .find(|c| c.row == row && c.column == column)
            .map(|c| c.text)
    };
    assert_eq!(find(0, col("n") as u64).as_deref(), Some("12345.67"));
    assert_eq!(find(1, col("n") as u64).as_deref(), Some("NULL"));
    drop(rendered);
    h.session.release_page(page.page_id).unwrap();
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn numeric_rendering_matches_the_servers_text_output() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let values = [
        "0",
        "0.000",
        "1",
        "-1",
        "42.00",
        "42.50",
        "-0.001",
        "0.0000000001",
        "100000000000000000000",
        "123456789012345678901234567890.123456789",
        "-99999999999999999999999999.99",
        "0.1",
        "10000",
        "10000.0001",
        "9999.9999",
        "0.00009999",
        "1234567.1234567",
        "NaN",
        "1e-30",
        "-1e30",
    ];
    let list = values.map(|v| format!("('{v}'::numeric)")).join(", ");
    let (_, batch) = h.query(
        &format!("SELECT v, v::text AS expected FROM (VALUES {list}) AS t(v) ORDER BY v::text"),
        0,
        100,
    );
    assert_eq!(batch.num_rows(), values.len());
    for row in 0..batch.num_rows() {
        assert_eq!(text(&batch, 0, row), text(&batch, 1, row), "row {row}");
    }
    // A column typmod keeps trailing zeros exactly like the server prints them.
    let (_, batch) = h.query(
        "SELECT 7.10::numeric(10,3) AS v, 7.10::numeric(10,3)::text AS e",
        0,
        10,
    );
    assert_eq!(
        (text(&batch, 0, 0), text(&batch, 1, 0)),
        ("7.100".into(), "7.100".into())
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn offset_limit_total_count_and_truncation() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let sql = "SELECT g AS id FROM generate_series(0, 24) AS g ORDER BY g";

    let (page, batch) = h.query(sql, 10, 5);
    assert_eq!(page.row_count, 5);
    assert_eq!(page.total_count, None, "the total is not known");
    assert!(!page.total_is_exact);
    assert!(page.flags.truncated, "rows 15.. exist");
    let ids = batch
        .column(0)
        .as_primitive::<arrow_array::types::Int64Type>();
    assert_eq!(
        (0..5).map(|i| ids.value(i)).collect::<Vec<_>>(),
        [10, 11, 12, 13, 14]
    );

    let (page, batch) = h.query(sql, 22, 5);
    assert_eq!(
        (page.row_count, page.total_count, page.flags.truncated),
        (3, None, false)
    );
    // Exact truncation through the lookahead row: a page that ends exactly at the
    // end of the result is not truncated.
    let (page, _) = h.query(sql, 20, 5);
    assert_eq!((page.row_count, page.flags.truncated), (5, false));
    let (page, _) = h.query(sql, 19, 5);
    assert_eq!((page.row_count, page.flags.truncated), (5, true));
    // Pages are capped at 100 000 rows.
    let (page, _) = h.query("SELECT g FROM generate_series(1, 100005) g", 0, 5_000_000);
    assert_eq!((page.row_count, page.flags.truncated), (100_000, true));
    assert_eq!(
        batch
            .column(0)
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(2),
        24
    );

    // limit 0 means the native default of 200.
    let (page, _) = h.query("SELECT g FROM generate_series(1, 250) AS g", 0, 0);
    assert_eq!((page.row_count, page.flags.truncated), (200, true));

    // An empty page keeps its typed columns.
    let (page, batch) = h.query(
        "SELECT 1::int AS id, 'x'::text AS t, 1.5::numeric AS n WHERE false",
        0,
        10,
    );
    assert_eq!((page.row_count, page.total_count), (0, None));
    assert_eq!(batch.num_rows(), 0);
    let types: Vec<DataType> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.data_type().clone())
        .collect();
    assert_eq!(types, [DataType::Int64, DataType::Utf8, DataType::Utf8]);

    // Trailing semicolon and trailing line comment are tolerated.
    let (page, _) = h.query("SELECT 1 AS one; ", 0, 10);
    assert_eq!(page.row_count, 1);
    let (page, _) = h.query("SELECT 2 AS two -- trailing comment", 0, 10);
    assert_eq!(page.row_count, 1);

    // Leading comments still page in SQL; several statements fall back to a script.
    let (page, _) = h.query(
        "-- first ten\nSELECT g FROM generate_series(1, 50) g",
        0,
        10,
    );
    assert_eq!((page.row_count, page.flags.truncated), (10, true));
    let (page, batch) = h.query("SELECT 1 AS a; SELECT 2 AS b, 3 AS c", 0, 10);
    assert_eq!((page.row_count, batch.num_columns()), (1, 2));
    // A script's integers are native, like a single statement's.
    assert_eq!(batch.schema().field(0).data_type(), &DataType::Int64);
    assert_eq!(cell_string(&batch, 0, 0), "2");

    // Duplicate column names survive the server-text path used for exotic types.
    let (_, batch) = h.query(
        "SELECT 1 AS a, interval '1 day' AS a, '{x}'::text[] AS a",
        0,
        10,
    );
    assert_eq!(batch.num_columns(), 3);
    assert_eq!(
        (text(&batch, 1, 0), text(&batch, 2, 0)),
        ("1 day".into(), "{x}".into())
    );

    // A server error is a typed failure that names the problem.
    let error = h
        .sql("SELECT * FROM {s}.nope".replace("{s}", &h.schema).as_str())
        .expect_err("missing table");
    assert_eq!(rejected_code(&error), "query.failed");
    assert!(error.to_string().contains("does not exist"), "{error}");
    // A genuine syntax error keeps the position of the user's own statement.
    let error = h.sql("SELECT FROM WHERE").expect_err("syntax error");
    assert_eq!(rejected_code(&error), "query.failed");
    assert!(error.to_string().contains("syntax error"), "{error}");
    let error = h.sql("   ").expect_err("empty sql");
    assert_eq!(rejected_code(&error), "query.bad_request");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn ddl_and_dml_return_zero_column_pages() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);

    let page = h.exec("CREATE TABLE {s}.t (id int PRIMARY KEY, v text)");
    assert!(page.columns.is_empty());
    assert_eq!(
        (page.row_count, page.total_count, page.total_is_exact),
        (0, Some(0), true)
    );

    let page = h.exec("INSERT INTO {s}.t SELECT g, 'v' || g FROM generate_series(1, 5) g");
    assert!(page.columns.is_empty());
    assert_eq!(page.total_count, Some(5), "INSERT reports affected rows");
    let page = h.exec("UPDATE {s}.t SET v = 'u' WHERE id <= 2");
    assert_eq!(page.total_count, Some(2));
    let page = h.exec("DELETE FROM {s}.t WHERE id = 5");
    assert_eq!(page.total_count, Some(1));
    assert_eq!(
        h.exec("UPDATE {s}.t SET v = 'z' WHERE id = 999")
            .total_count,
        Some(0)
    );

    // RETURNING and CTE-wrapped DML cannot be a subquery: run as written, with rows.
    let (page, batch) = h.query(
        "INSERT INTO {s}.t VALUES (10, 'ten') RETURNING id, v",
        0,
        10,
    );
    assert_eq!(page.row_count, 1);
    assert_eq!(text(&batch, 1, 0), "ten");
    let (page, batch) = h.query(
        "WITH gone AS (DELETE FROM {s}.t WHERE id = 10 RETURNING id) SELECT id FROM gone",
        0,
        10,
    );
    assert_eq!(page.row_count, 1);
    assert_eq!(
        batch
            .column(0)
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(0),
        10
    );

    // Statements that return rows without being SELECT/WITH page client-side.
    let (page, batch) = h.query("SHOW server_version", 0, 10);
    assert_eq!((page.row_count, batch.num_columns()), (1, 1));
    let (page, _) = h.query("VALUES (1), (2), (3), (4)", 1, 2);
    assert_eq!(page.row_count, 2);

    // Several statements in one request: the last result set comes back as text.
    let (page, batch) = h.query(
        "CREATE TABLE {s}.s2 (x int); INSERT INTO {s}.s2 VALUES (7); SELECT x FROM {s}.s2",
        0,
        10,
    );
    assert_eq!(page.row_count, 1);
    assert_eq!(cell_string(&batch, 0, 0), "7");
    let page = h.exec("INSERT INTO {s}.s2 VALUES (8); INSERT INTO {s}.s2 VALUES (9)");
    assert!(page.columns.is_empty());
    assert_eq!(page.total_count, Some(2));

    // The writes really happened.
    assert_eq!(
        h.scalar(
            "SELECT count(*)::text FROM {s}.t"
                .replace("{s}", &h.schema)
                .as_str()
        ),
        "4"
    );
    let page = h.exec("DROP TABLE {s}.s2");
    assert!(page.columns.is_empty());
}

// ---------------------------------------------------------------------------
// cancel, interruption, disconnect
// ---------------------------------------------------------------------------

fn sleep_request(operation: OperationId, seconds: u32) -> String {
    json!({
        "kind": "sql", "operation_id": operation, "database": null, "object": null,
        "offset": 0, "limit": 10, "payload": { "sql": format!("SELECT pg_sleep({seconds})") }
    })
    .to_string()
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn cancelling_a_running_query_returns_quickly_and_the_connection_stays_usable() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let operation = h.session.begin_operation().unwrap();
    let id = operation.id();
    let runner = h.session.query_runner().unwrap();
    let request = sleep_request(id, 10);
    let started = Instant::now();
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(400));
    h.handle
        .cancel_operation(
            h.session.connection_id(),
            &CancelRequest { operation_id: id },
        )
        .expect("cancel_operation");
    let result = worker.join().expect("query thread");
    let elapsed = started.elapsed();
    let error = result.expect_err("cancelled query fails");
    assert_eq!(rejected_code(&error), "query.cancelled", "{error}");
    assert!(elapsed < Duration::from_secs(2), "cancel took {elapsed:?}");
    drop(operation);

    // The same connection runs the next statement, and a stale cancel is harmless.
    let (page, batch) = h.query("SELECT 41 + 1 AS answer", 0, 10);
    assert_eq!(page.row_count, 1);
    assert_eq!(
        batch
            .column(0)
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(0),
        42
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_cancel_for_a_pending_operation_stops_it_before_it_runs() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    // An early cancel (the host cancels an id the driver has not seen yet) is remembered.
    let operation = h.session.begin_operation().unwrap();
    h.handle
        .cancel_operation(
            h.session.connection_id(),
            &CancelRequest {
                operation_id: operation.id(),
            },
        )
        .expect("early cancel");
    let started = Instant::now();
    let result = h.session.open_query(&sleep_request(operation.id(), 10));
    assert!(result.is_err(), "pre-cancelled operation does not run");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn an_interrupt_nobody_asked_for_is_a_retryable_query_interrupted() {
    let uri = pg_uri();
    let mut victim = Harness::new(&uri);
    let pid = victim.scalar("SELECT pg_backend_pid()::text");
    let mut killer = Harness::new(&uri);

    let operation = victim.session.begin_operation().unwrap();
    let runner = victim.session.query_runner().unwrap();
    let request = sleep_request(operation.id(), 10);
    let started = Instant::now();
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(400));
    // A different connection cancels the victim's backend: not a host cancel.
    assert_eq!(
        killer.scalar(&format!("SELECT pg_cancel_backend({pid})::text")),
        "true"
    );
    let error = worker.join().unwrap().expect_err("interrupted");
    assert_eq!(rejected_code(&error), "query.interrupted", "{error}");
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(operation);
    // The connection survives.
    assert_eq!(victim.scalar("SELECT 'alive'"), "alive");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn disconnect_cancels_a_running_query() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let operation = h.session.begin_operation().unwrap();
    let runner = h.session.query_runner().unwrap();
    let request = sleep_request(operation.id(), 10);
    let started = Instant::now();
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(400));
    h.session
        .disconnect()
        .expect("disconnect while a query runs");
    let error = worker
        .join()
        .unwrap()
        .expect_err("query ended by the disconnect");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "disconnect took {elapsed:?}: {error}"
    );
    drop(operation);
}

// ---------------------------------------------------------------------------
// mutations
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn insert_update_delete_run_in_a_transaction_and_roll_back_on_failure() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.items (id int PRIMARY KEY, name text, qty int NOT NULL DEFAULT 0, meta jsonb, price numeric(10,2), at timestamptz, ok boolean)");
    h.exec("INSERT INTO {s}.items VALUES (1, 'one', 1, NULL, NULL, NULL, NULL), (2, 'two', 2, NULL, NULL, NULL, NULL)");
    let schema = h.schema.clone();
    let identity = serde_json::to_value(SqlTableIdentity {
        schema: Some(schema.clone()),
    })
    .unwrap();
    let count = |h: &mut Harness| h.scalar(&format!("SELECT count(*)::text FROM {schema}.items"));
    let name_of = |h: &mut Harness, id: i32| {
        h.scalar(&format!(
            "SELECT coalesce(name, 'NULL') FROM {schema}.items WHERE id = {id}"
        ))
    };
    let change =
        |op: &str, key: Value, values: Value| json!({"op": op, "key": key, "values": values});

    // A page opened before the write is invalidated by it.
    let (stale, _) = h.query("SELECT * FROM {s}.items", 0, 10);

    // Values are bound parameters: awkward text and every column type go through.
    let nasty = "it's \\ \"q\" ; DROP TABLE x; --";
    let result = h
        .mutate(
            "insert",
            Some("items"),
            identity.clone(),
            change(
                "insert",
                json!({}),
                json!({
                    "id": 3, "name": nasty, "qty": 7, "meta": {"k": [1, 2]}, "price": 12.5,
                    "at": "2024-01-02 03:04:05+00", "ok": true
                }),
            ),
        )
        .expect("insert");
    assert_eq!(result.affected_rows, 1);
    assert_eq!(result.invalidated_page_ids, [stale.page_id]);
    assert_eq!(count(&mut h), "3");
    assert_eq!(name_of(&mut h, 3), nasty);
    assert_eq!(
        h.scalar(&format!(
            "SELECT meta::text FROM {schema}.items WHERE id = 3"
        )),
        "{\"k\": [1, 2]}"
    );
    assert_eq!(
        h.scalar(&format!(
            "SELECT price::text FROM {schema}.items WHERE id = 3"
        )),
        "12.50"
    );
    assert_eq!(
        h.scalar(&format!("SELECT ok::text FROM {schema}.items WHERE id = 3")),
        "true"
    );

    let result = h
        .mutate(
            "update",
            Some("items"),
            identity.clone(),
            change("update", json!({"id": 3}), json!({"name": null, "qty": 8})),
        )
        .expect("update");
    assert_eq!(result.affected_rows, 1);
    assert_eq!(name_of(&mut h, 3), "NULL");
    assert_eq!(
        h.scalar(&format!(
            "SELECT qty::text FROM {schema}.items WHERE id = 3"
        )),
        "8"
    );
    // The key may be given as a string for a numeric column (the server parses it).
    h.mutate(
        "update",
        Some("items"),
        identity.clone(),
        change("update", json!({"id": "3"}), json!({"qty": 9})),
    )
    .expect("string key");

    let result = h
        .mutate(
            "delete",
            Some("items"),
            identity.clone(),
            change("delete", json!({"id": 3}), json!({})),
        )
        .expect("delete");
    assert_eq!(result.affected_rows, 1);
    assert_eq!(count(&mut h), "2");

    // A server error is `mutation.failed` and names the cause (id is NOT NULL via PK).
    let error = h
        .mutate(
            "insert",
            Some("items"),
            identity.clone(),
            json!({"op": "insert"}),
        )
        .expect_err("id is required");
    assert_eq!(rejected_code(&error), "mutation.failed");
    assert_eq!(count(&mut h), "2");

    // A mixed batch commits atomically, in order.
    let result = h
        .mutate(
            "batch",
            Some("items"),
            identity.clone(),
            json!({"changes": [
                change("update", json!({"id": 1}), json!({"name": "uno"})),
                change("update", json!({"id": 2}), json!({"name": "dos"})),
                change("insert", json!({}), json!({"id": 4, "name": "four"})),
                change("delete", json!({"id": 4}), json!({}))
            ]}),
        )
        .expect("batch");
    assert_eq!(result.affected_rows, 4);
    assert_eq!(
        (name_of(&mut h, 1), name_of(&mut h, 2)),
        ("uno".into(), "dos".into())
    );
    assert_eq!(count(&mut h), "2");

    // A statement that affects no row rolls the whole batch back.
    let error = h
        .mutate(
            "batch",
            Some("items"),
            identity.clone(),
            json!({"changes": [
                change("insert", json!({}), json!({"id": 6, "name": "six"})),
                change("update", json!({"id": 1}), json!({"name": "CHANGED"})),
                change("update", json!({"id": 404}), json!({"name": "ghost"}))
            ]}),
        )
        .expect_err("no rows affected");
    assert_eq!(rejected_code(&error), "mutation.no_rows_affected");
    assert_eq!(name_of(&mut h, 1), "uno", "earlier statements rolled back");
    assert_eq!(count(&mut h), "2", "the insert was rolled back too");

    // A failing statement (duplicate key) rolls the batch back and says why.
    let error = h
        .mutate(
            "batch",
            Some("items"),
            identity.clone(),
            json!({"changes": [
                change("insert", json!({}), json!({"id": 5, "name": "five"})),
                change("insert", json!({}), json!({"id": 1, "name": "duplicate key"}))
            ]}),
        )
        .expect_err("duplicate key");
    assert_eq!(rejected_code(&error), "mutation.failed");
    assert!(error.to_string().contains("duplicate key"), "{error}");
    assert_eq!(count(&mut h), "2", "id 5 was rolled back");
    // No transaction is left open and the connection keeps working.
    assert_eq!(h.scalar("SELECT 'ok'"), "ok");

    // A key that matches several rows is not an identity: rolled back.
    h.exec("INSERT INTO {s}.items VALUES (10, 'dup', 1, NULL, NULL, NULL, NULL), (11, 'dup', 1, NULL, NULL, NULL, NULL)");
    let error = h
        .mutate(
            "update",
            Some("items"),
            identity.clone(),
            change("update", json!({"name": "dup"}), json!({"qty": 50})),
        )
        .expect_err("two rows match");
    assert_eq!(rejected_code(&error), "mutation.identity_not_unique");
    assert_eq!(
        h.scalar(&format!(
            "SELECT qty::text FROM {schema}.items WHERE id = 10"
        )),
        "1"
    );
    let error = h
        .mutate(
            "batch",
            Some("items"),
            identity.clone(),
            json!({"changes": [
                change("insert", json!({}), json!({"id": 12, "name": "x"})),
                change("delete", json!({"name": "dup"}), json!({}))
            ]}),
        )
        .expect_err("two rows match");
    assert_eq!(rejected_code(&error), "mutation.identity_not_unique");
    assert_eq!(count(&mut h), "4", "nothing was deleted or inserted");

    // Composite keys: every key column must match, and NULL matches with IS NULL.
    h.exec("CREATE TABLE {s}.pairs (tenant int NOT NULL, id int NOT NULL, label text, tag text, PRIMARY KEY (tenant, id))");
    h.exec("INSERT INTO {s}.pairs VALUES (1, 7, 'a', NULL), (2, 7, 'b', NULL), (1, 8, 'c', 'x')");
    let label_of = |h: &mut Harness, tenant: i32, id: i32| {
        h.scalar(&format!(
            "SELECT label FROM {schema}.pairs WHERE tenant = {tenant} AND id = {id}"
        ))
    };
    let result = h
        .mutate(
            "update",
            Some("pairs"),
            identity.clone(),
            change(
                "update",
                json!({"tenant": 2, "id": 7}),
                json!({"label": "B2"}),
            ),
        )
        .expect("composite update");
    assert_eq!(result.affected_rows, 1);
    assert_eq!(
        (label_of(&mut h, 1, 7), label_of(&mut h, 2, 7)),
        ("a".into(), "B2".into())
    );
    // Only part of the key is not unique: id 7 exists for two tenants.
    let error = h
        .mutate(
            "delete",
            Some("pairs"),
            identity.clone(),
            change("delete", json!({"id": 7}), json!({})),
        )
        .expect_err("partial key");
    assert_eq!(rejected_code(&error), "mutation.identity_not_unique");
    h.mutate(
        "delete",
        Some("pairs"),
        identity.clone(),
        change(
            "delete",
            json!({"tenant": 1, "id": 7, "tag": null}),
            json!({}),
        ),
    )
    .expect("composite delete with a NULL key part");
    assert_eq!(
        h.scalar(&format!("SELECT count(*)::text FROM {schema}.pairs")),
        "2"
    );

    // Bad requests never reach the server.
    for (kind, payload) in [
        ("update", change("update", json!({}), json!({"qty": 1}))), // empty key
        ("update", change("update", json!({"id": 1}), json!({}))),  // nothing to change
        (
            "delete",
            change("update", json!({"id": 1}), json!({"qty": 1})),
        ), // op != kind
        ("insert", Value::Null),                                    // no payload
        ("batch", json!({"changes": []})),                          // empty batch
    ] {
        let error = h
            .mutate(kind, Some("items"), identity.clone(), payload.clone())
            .expect_err("bad request");
        assert_eq!(
            rejected_code(&error),
            "mutation.bad_request",
            "{kind} {payload}"
        );
    }
    let error = h
        .mutate(
            "update",
            None,
            identity.clone(),
            change("update", json!({"id": 1}), json!({"qty": 1})),
        )
        .expect_err("no object");
    assert_eq!(rejected_code(&error), "mutation.bad_request");
    // The connection is still healthy and nothing leaked from the failures.
    let committed = h
        .mutate(
            "update",
            Some("items"),
            identity.clone(),
            change("update", json!({"id": 2}), json!({"qty": 99})),
        )
        .expect("works afterwards");
    assert_eq!(committed.affected_rows, 1);
    // Unadvertised kinds are refused by the host without calling the driver.
    let error = h
        .mutate("drop_collection", Some("items"), identity, Value::Null)
        .expect_err("unsupported");
    assert!(
        matches!(error, LoaderError::UnsupportedFeature { .. }),
        "{error}"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn create_database_succeeds_once_and_is_rejected_twice() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let name = format!("tablexv2_{}", std::process::id());
    let create = |h: &mut Harness, name: &str| {
        let operation = h.session.begin_operation().unwrap();
        h.session.mutate(&MutationRequest {
            operation_id: operation.id(),
            kind: "create_database".into(),
            database: Some(name.into()),
            object: None,
            identity: Value::Null,
            payload: Value::Null,
        })
    };
    // Clean up a leftover from an aborted earlier run.
    let _ = h
        .sql(&format!("DROP DATABASE IF EXISTS \"{name}\""))
        .map(|p| h.session.release_page(p.page_id));

    let (stale, _) = h.query("SELECT 1 AS one", 0, 10);
    let result = create(&mut h, &name).expect("create database");
    assert_eq!(result.affected_rows, 0);
    assert!(
        result.invalidated_page_ids.is_empty(),
        "a new database keeps open pages"
    );
    h.session
        .release_page(stale.page_id)
        .expect("page still valid");

    // It exists: the navigator lists it. (A connection that opened a client to it
    // would block DROP DATABASE below, so a query against another database
    // exercises the per-database client path instead.)
    let databases =
        h.nav(json!({"parent":null,"database":null,"payload":null,"cursor":null,"limit":500}));
    assert!(names(&databases).contains(&name));
    assert!(names(&databases).contains(&"postgres".to_string()));
    let operation = h.session.begin_operation().unwrap();
    let page = h
        .session
        .open_query(&json!({
            "kind": "sql", "operation_id": operation.id(), "database": "postgres", "object": null,
            "offset": 0, "limit": 10, "payload": {"sql": "SELECT current_database() AS db"}
        }).to_string())
        .expect("query another database");
    let lease = h
        .session
        .acquire_arrow_page(page.page_id, 0, page.row_count)
        .unwrap();
    // SAFETY: lease from the driver.
    let batch = unsafe { lease.into_record_batch() }.unwrap().batch;
    assert_eq!(text(&batch, 0, 0), "postgres");
    h.session.release_page(page.page_id).unwrap();

    let again = create(&mut h, &name).expect_err("duplicate database");
    assert_eq!(
        rejected_code(&again),
        "mutation.create_database.rejected",
        "{again}"
    );
    let invalid = create(&mut h, "").expect_err("empty name");
    assert_eq!(rejected_code(&invalid), "mutation.create_database.rejected");
    // Anything besides the name is a malformed request, not a server refusal.
    for (object, identity, payload) in [
        (None, Value::Null, json!({"owner": "x"})),
        (Some("t".to_string()), Value::Null, Value::Null),
        (None, json!({"schema": "s"}), Value::Null),
    ] {
        let operation = h.session.begin_operation().unwrap();
        let error = h
            .session
            .mutate(&MutationRequest {
                operation_id: operation.id(),
                kind: "create_database".into(),
                database: Some(format!("{name}_bad")),
                object,
                identity,
                payload,
            })
            .expect_err("malformed create_database");
        assert_eq!(rejected_code(&error), "mutation.bad_request", "{error}");
    }

    // Drop it again (the driver's DDL path: DROP DATABASE needs no transaction block).
    let dropped = h
        .sql(&format!("DROP DATABASE \"{name}\""))
        .expect("drop database");
    assert!(dropped.columns.is_empty());
    h.session.release_page(dropped.page_id).unwrap();
    let databases =
        h.nav(json!({"parent":null,"database":null,"payload":null,"cursor":null,"limit":500}));
    assert!(!names(&databases).contains(&name));
}

// ---------------------------------------------------------------------------
// describe_object
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn schema_objects_list_by_kind_and_show_their_source() {
    use engine_driver_kit::api::sql::{SchemaObjectKind, SchemaObjectRef, SchemaObjectSource};
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.orders (id int PRIMARY KEY, total int)");
    h.exec("CREATE FUNCTION {s}.add(a int, b int) RETURNS int LANGUAGE sql AS 'SELECT a + b'");
    h.exec("CREATE FUNCTION {s}.add(a text, b text) RETURNS text LANGUAGE sql AS 'SELECT a || b'");
    h.exec("CREATE PROCEDURE {s}.touch(n int) LANGUAGE sql AS 'SELECT n'");
    h.exec("CREATE FUNCTION {s}.audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$");
    h.exec("CREATE TRIGGER orders_audit BEFORE INSERT ON {s}.orders FOR EACH ROW EXECUTE FUNCTION {s}.audit()");
    h.exec("CREATE SEQUENCE {s}.ticket_seq AS integer INCREMENT BY 5 START WITH 100 MINVALUE 100 MAXVALUE 1000 CACHE 3 CYCLE");
    h.exec("CREATE MATERIALIZED VIEW {s}.totals AS SELECT total FROM {s}.orders");
    h.exec("CREATE TYPE {s}.mood AS ENUM ('sad', 'it''s ok', 'happy')");
    h.exec("CREATE TYPE {s}.pair AS (a int, \"Weird Name\" text)");
    h.exec("CREATE DOMAIN {s}.positive AS int NOT NULL DEFAULT 1 CONSTRAINT positive_check CHECK (VALUE > 0)");
    h.exec("CREATE TYPE {s}.floatrange AS RANGE (subtype = float8, subtype_diff = float8mi)");
    h.exec("CREATE FUNCTION {s}.feel(m {s}.mood) RETURNS int LANGUAGE sql AS 'SELECT 1'");
    let schema = h.schema.clone();
    let database = h.scalar("SELECT current_database()");

    let list = |h: &mut Harness, kind: &str, filter: Option<&str>| {
        let mut payload = json!({"database": database, "schema": schema, "kind": kind});
        if let Some(filter) = filter {
            payload["filter"] = json!(filter);
        }
        h.nav(json!({"parent": format!("schema:{database}:{schema}"), "database": database,
                     "payload": payload, "cursor": null, "limit": 100}))
    };
    let source_of = |h: &mut Harness, node: &engine_driver_kit::api::NavigatorNode| -> String {
        let reference: SchemaObjectRef = serde_json::from_value(node.payload.clone()).expect("typed payload");
        let described = h.session.describe_object(&node.payload).expect("source");
        assert_eq!(reference.schema.as_deref(), Some(schema.as_str()));
        serde_json::from_value::<SchemaObjectSource>(described).expect("typed source").source
    };

    // Overloads are two nodes with their argument types, and each shows its own source.
    let functions = list(&mut h, "function", None);
    assert_eq!(names(&functions)[..3], ["add(integer, integer)", "add(text, text)", "audit()"]);
    // A user type is printed with its schema (it is not on the search path)...
    let feel = functions.nodes.iter().find(|node| node.label.starts_with("feel(")).expect("feel");
    assert_eq!(feel.label, format!("feel({schema}.mood)"));
    // ...but the lookup goes by the type OIDs: a reference whose printed text no longer
    // matches (another `search_path`) still finds its function.
    let mut stale: SchemaObjectRef = serde_json::from_value(feel.payload.clone()).unwrap();
    assert!(stale.identity.is_some());
    stale.arguments = Some("mood".into());
    let found = h.session.describe_object(&serde_json::to_value(&stale).unwrap()).expect("found by identity");
    assert!(serde_json::from_value::<SchemaObjectSource>(found).unwrap().source.contains("feel("));
    // Without an identity the printed types are what is matched.
    stale.identity = None;
    let error = h.session.describe_object(&serde_json::to_value(&stale).unwrap()).unwrap_err();
    assert_eq!(rejected_code(&error), "describe.not_found");
    assert!(functions.nodes.iter().all(|node| node.kind == "function" && !node.has_children));
    let ints = source_of(&mut h, &functions.nodes[0]);
    let texts = source_of(&mut h, &functions.nodes[1]);
    assert!(ints.contains("a integer, b integer") && ints.contains("SELECT a + b") && ints.ends_with(';'), "{ints}");
    assert!(texts.contains("a text, b text") && texts.contains("a || b"), "{texts}");
    assert_eq!(names(&list(&mut h, "function", Some("AUD"))), ["audit()"]);

    let procedures = list(&mut h, "procedure", None);
    assert_eq!(names(&procedures), ["touch(integer)"]);
    assert!(source_of(&mut h, &procedures.nodes[0]).starts_with("CREATE OR REPLACE PROCEDURE"));

    let triggers = list(&mut h, "trigger", None);
    assert_eq!(names(&triggers), ["orders_audit on orders"]);
    let trigger = source_of(&mut h, &triggers.nodes[0]);
    assert!(trigger.starts_with("CREATE TRIGGER orders_audit BEFORE INSERT ON"), "{trigger}");

    let sequences = list(&mut h, "sequence", None);
    assert_eq!(names(&sequences), ["ticket_seq"]);
    let sequence = source_of(&mut h, &sequences.nodes[0]);
    assert!(
        sequence.contains("AS integer INCREMENT BY 5 MINVALUE 100 MAXVALUE 1000 START WITH 100 CACHE 3 CYCLE;"),
        "{sequence}"
    );

    let types = list(&mut h, "type", None);
    assert_eq!(names(&types), ["floatrange", "mood", "pair", "positive"]);
    let range = source_of(&mut h, &types.nodes[0]);
    assert!(range.contains("AS RANGE (") && range.contains("SUBTYPE = double precision") && range.contains("SUBTYPE_DIFF = float8mi"), "{range}");
    let enum_source = source_of(&mut h, &types.nodes[1]);
    assert!(enum_source.ends_with("AS ENUM ('sad', 'it''s ok', 'happy');"), "{enum_source}");
    let composite = source_of(&mut h, &types.nodes[2]);
    assert!(composite.contains("a integer") && composite.contains("\"Weird Name\" text"), "{composite}");
    let domain = source_of(&mut h, &types.nodes[3]);
    assert!(
        domain.contains("AS integer DEFAULT 1 NOT NULL") && domain.contains("CONSTRAINT positive_check CHECK ((VALUE > 0))"),
        "{domain}"
    );

    // A materialized view lists as a table-like node and shows its definition.
    let matviews = list(&mut h, "materialized_view", None);
    assert_eq!(matviews.nodes.iter().map(|node| (node.kind.as_str(), node.label.as_str())).collect::<Vec<_>>(), [("materialized_view", "totals")]);
    let reference = SchemaObjectRef {
        database: Some(database.clone()),
        schema: Some(schema.clone()),
        kind: SchemaObjectKind::MaterializedView,
        name: "totals".into(),
        arguments: None,
        table: None,
        identity: None,
    };
    let described = h.session.describe_object(&serde_json::to_value(&reference).unwrap()).expect("matview source");
    let matview = serde_json::from_value::<SchemaObjectSource>(described).unwrap().source;
    assert!(matview.starts_with("CREATE MATERIALIZED VIEW") && matview.contains("SELECT") && matview.ends_with("WITH DATA;"), "{matview}");

    // The plain listing is unchanged, a vanished object is not-found and a bad kind is refused.
    let missing = SchemaObjectRef { name: "gone".into(), kind: SchemaObjectKind::Sequence, ..reference.clone() };
    let error = h.session.describe_object(&serde_json::to_value(&missing).unwrap()).unwrap_err();
    assert_eq!(rejected_code(&error), "describe.not_found");
    let bad = h.session.list_nav_nodes(
        &json!({"parent": null, "database": database, "payload": {"database": database, "schema": schema, "kind": "package"},
                "cursor": null, "limit": 10})
        .to_string(),
    );
    assert_eq!(rejected_code(&bad.unwrap_err()), "navigator.bad_request");
    // Names are bound, never spliced: a hostile one finds nothing and breaks nothing.
    let hostile = SchemaObjectRef { name: "x'; DROP TABLE orders; --".into(), kind: SchemaObjectKind::Trigger, table: Some("orders".into()), ..reference };
    let error = h.session.describe_object(&serde_json::to_value(&hostile).unwrap()).unwrap_err();
    assert_eq!(rejected_code(&error), "describe.not_found");
    assert_eq!(h.scalar("SELECT count(*)::text FROM {s}.orders"), "0");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn describe_object_returns_table_properties() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.parent (id bigint PRIMARY KEY, label text UNIQUE)");
    h.exec(
        "CREATE TABLE {s}.child (id serial PRIMARY KEY, parent_id bigint NOT NULL REFERENCES {s}.parent(id) ON DELETE CASCADE,
            qty int CHECK (qty > 0), note text DEFAULT 'n')",
    );
    h.exec("CREATE INDEX child_note_idx ON {s}.child USING btree (note)");
    h.exec("COMMENT ON TABLE {s}.child IS 'a child'");
    h.exec("CREATE FUNCTION {s}.audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$");
    h.exec("CREATE TRIGGER child_audit AFTER INSERT OR UPDATE ON {s}.child FOR EACH ROW EXECUTE FUNCTION {s}.audit()");
    h.exec("CREATE VIEW {s}.child_v AS SELECT id FROM {s}.child");
    let schema = h.schema.clone();

    let describe = |h: &mut Harness, object: &str| {
        h.session
            .describe_object(&json!({"schema": schema, "object": object}))
    };

    let child = describe(&mut h, "child").expect("describe child");
    // The document deserializes into the shared typed contract.
    let typed: TableProperties =
        serde_json::from_value(child.clone()).expect("typed TableProperties");
    assert_eq!(typed.columns.len(), 4);
    assert_eq!(
        typed.keys.as_ref().unwrap()[0].kind,
        engine_driver_kit::api::sql::ConstraintKind::PrimaryKey
    );
    assert_eq!(typed.foreign_keys.as_ref().unwrap().len(), 1);
    assert_eq!(typed.triggers.as_ref().unwrap()[0].enabled, Some(true));
    assert!(typed.ddl.as_deref().unwrap().starts_with("CREATE TABLE"));
    let columns = child["columns"].as_array().unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["id", "parent_id", "qty", "note"]
    );
    assert_eq!(columns[0]["primary_key"], true);
    assert_eq!(columns[0]["auto_increment"], true);
    assert_eq!(columns[1]["nullable"], false);
    assert_eq!(columns[3]["default_expression"], "'n'::text");

    let keys = child["keys"].as_array().unwrap();
    let key_kinds: Vec<(&str, &str)> = keys
        .iter()
        .map(|k| (k["kind"].as_str().unwrap(), k["name"].as_str().unwrap()))
        .collect();
    assert_eq!(key_kinds[0], ("primary_key", "child_pkey"));
    assert!(key_kinds.iter().any(|(kind, _)| *kind == "check"));
    assert_eq!(keys[0]["columns"], json!(["id"]));

    let fks = child["foreign_keys"].as_array().unwrap();
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0]["columns"], json!(["parent_id"]));
    assert_eq!(fks[0]["referenced_table"], format!("{schema}.parent"));
    assert_eq!(fks[0]["referenced_columns"], json!(["id"]));
    assert_eq!(fks[0]["on_delete"], "CASCADE");
    assert_eq!(fks[0]["on_update"], "NO ACTION");

    let indexes = child["indexes"].as_array().unwrap();
    let idx = indexes
        .iter()
        .find(|i| i["name"] == "child_note_idx")
        .expect("index");
    assert_eq!(idx["method"], "btree");
    assert_eq!(idx["unique"], false);
    assert_eq!(idx["columns"], json!(["note"]));
    assert!(indexes.iter().any(|i| i["primary"] == true));

    let triggers = child["triggers"].as_array().unwrap();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0]["name"], "child_audit");
    assert_eq!(triggers[0]["timing"], "AFTER");
    assert_eq!(triggers[0]["enabled"], true);
    let mut events: Vec<&str> = triggers[0]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    events.sort_unstable();
    assert_eq!(events, ["INSERT", "UPDATE"]);

    let ddl = child["ddl"].as_str().unwrap();
    assert!(
        ddl.starts_with(&format!("CREATE TABLE \"{schema}\".\"child\" (")),
        "{ddl}"
    );
    assert!(ddl.contains("CREATE INDEX child_note_idx"), "{ddl}");
    assert!(ddl.contains("CREATE TRIGGER child_audit"), "{ddl}");
    assert!(ddl.contains("COMMENT ON TABLE"), "{ddl}");
    assert_eq!(child["references"], json!([]));

    // Foreign keys of other tables that point here.
    let parent = describe(&mut h, "parent").expect("describe parent");
    let references = parent["references"].as_array().unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["table"], format!("{schema}.child"));
    assert_eq!(
        references[0]["referenced_table"],
        format!("{schema}.parent")
    );
    assert_eq!(
        parent["keys"].as_array().unwrap().len(),
        2,
        "primary key and unique"
    );

    // Views describe through their definition.
    let view = describe(&mut h, "child_v").expect("describe view");
    assert!(view["ddl"]
        .as_str()
        .unwrap()
        .starts_with(&format!("CREATE VIEW \"{schema}\".\"child_v\" AS")));

    // Unknown objects and malformed requests fail with codes.
    let error = describe(&mut h, "nope").expect_err("missing object");
    assert_eq!(rejected_code(&error), "describe.not_found");
    let error = h
        .session
        .describe_object(&json!({"schema": schema}))
        .expect_err("no object");
    assert_eq!(rejected_code(&error), "describe.bad_request");
}

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn init_after_shutdown_in_the_same_image_works() {
    let uri = pg_uri();
    let temp = TempDir::new().unwrap();
    let path = stage(&temp);
    for round in 0..3 {
        let handle = DriverLoader.load(&path).expect("load");
        let mut session = handle
            .connect(&json!({ "uri": uri }).to_string())
            .unwrap_or_else(|error| panic!("round {round}: {error}"));
        let page = run(&mut session, "SELECT 1").expect("query");
        session.release_page(page.page_id).unwrap();
        session.disconnect().unwrap();
        // shutdown stops the runtime; the next load's init builds a new one.
        handle.shutdown().expect("shutdown");
        drop(handle);
    }
}

// ---------------------------------------------------------------------------
// review corrections: B1 (database names), B2, B3, B5, cancel codes, metadata
// ---------------------------------------------------------------------------

/// A raw request that lets a test omit `payload.confirmed`.
fn query_request(
    operation: OperationId,
    sql: &str,
    offset: u64,
    limit: u64,
    confirmed: Option<bool>,
) -> String {
    let mut payload = json!({ "sql": sql });
    if let Some(confirmed) = confirmed {
        payload["confirmed"] = json!(confirmed);
    }
    json!({
        "kind": "sql", "operation_id": operation, "database": null, "object": null,
        "offset": offset, "limit": limit, "payload": payload
    })
    .to_string()
}

/// Drops the named databases over a fresh session when the test ends, even if
/// it failed.
struct DropDatabases {
    uri: String,
    handle: Arc<DriverHandle>,
    names: Vec<String>,
}

impl Drop for DropDatabases {
    fn drop(&mut self) {
        let Ok(mut session) = self.handle.connect(&json!({ "uri": self.uri }).to_string()) else {
            return;
        };
        for name in &self.names {
            let sql = format!("DROP DATABASE IF EXISTS \"{}\"", name.replace('"', "\"\""));
            if let Ok(page) = run(&mut session, &sql) {
                let _ = session.release_page(page.page_id);
            }
        }
        let _ = session.disconnect();
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn hostile_database_names_select_that_database_and_never_redirect() {
    let uri = pg_uri();
    let temp = TempDir::new().unwrap();
    let handle = DriverLoader.load(&stage(&temp)).expect("load");
    let pid = std::process::id();
    // Names that would change a connection string if they were pasted into one:
    // a query component (`?hostaddr=` would send the credentials to another
    // host), percent escapes, a fragment, a path separator, a space.
    let names: Vec<String> = [
        format!("x?hostaddr=203.0.113.5&p{pid}"),
        format!("hostaddr=203.0.113.5 p{pid}"),
        format!("a%20b%2Fc p{pid}"),
        format!("h#frag p{pid}"),
        format!("sl/ash p{pid}"),
        format!("sp ace p{pid}"),
        format!("q\"uote p{pid}"),
        format!("x&host=evil.example p{pid}"),
    ]
    .into();
    let _cleanup = DropDatabases {
        uri: uri.clone(),
        handle: Arc::clone(&handle),
        names: names.clone(),
    };
    let mut session = handle
        .connect(&json!({ "uri": uri }).to_string())
        .expect("connect");

    for name in &names {
        let operation = session.begin_operation().unwrap();
        session
            .mutate(&MutationRequest {
                operation_id: operation.id(),
                kind: "create_database".into(),
                database: Some(name.clone()),
                object: None,
                identity: Value::Null,
                payload: Value::Null,
            })
            .unwrap_or_else(|error| panic!("create {name:?}: {error}"));
    }
    let started = Instant::now();
    for name in &names {
        // A query on that database really runs there.
        let operation = session.begin_operation().unwrap();
        let page = session
            .open_query(
                &json!({
                    "kind": "sql", "operation_id": operation.id(), "database": name, "object": null,
                    "offset": 0, "limit": 10, "payload": {"sql": "SELECT current_database()"}
                })
                .to_string(),
            )
            .unwrap_or_else(|error| panic!("query on {name:?}: {error}"));
        let lease = session
            .acquire_arrow_page(page.page_id, 0, page.row_count)
            .unwrap();
        // SAFETY: the lease was just returned by the driver's Arrow export.
        let batch = unsafe { lease.into_record_batch() }.unwrap().batch;
        assert_eq!(text(&batch, 0, 0), *name);
        session.release_page(page.page_id).unwrap();
        // The navigator (metadata connection) opens the same database too.
        let nav = session
            .list_nav_nodes(
                &json!({"parent": null, "database": name, "payload": {"database": name}, "cursor": null, "limit": 10}).to_string(),
            )
            .unwrap_or_else(|error| panic!("navigator on {name:?}: {error}"));
        assert!(nav.nodes.iter().all(|node| node.kind == "schema"));
    }
    // No attempt to reach 203.0.113.5 (it would hang until a TCP timeout).
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    // The listed databases include every name verbatim.
    let operation = session.begin_operation().unwrap();
    let _ = operation;
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn dangerous_statements_need_confirmation_and_writes_are_not_pageable() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.t (id int PRIMARY KEY, v int)");
    h.exec("INSERT INTO {s}.t VALUES (1, 1), (2, 2), (3, 3)");
    let schema = h.schema.clone();
    let raw = |h: &mut Harness, sql: &str, offset: u64, limit: u64, confirmed: Option<bool>| {
        let operation = h.session.begin_operation().unwrap();
        h.session.open_query(&query_request(
            operation.id(),
            sql,
            offset,
            limit,
            confirmed,
        ))
    };

    for sql in [
        format!("DROP TABLE {schema}.t"),
        format!("TRUNCATE {schema}.t"),
        format!("ALTER TABLE {schema}.t ADD COLUMN x int"),
        format!("DELETE FROM {schema}.t"),
        format!("UPDATE {schema}.t SET v = 0"),
        "GRANT SELECT ON ALL TABLES IN SCHEMA public TO PUBLIC".to_string(),
        "CREATE DATABASE never_created_by_this_test".to_string(),
        format!("SELECT 1; DROP TABLE {schema}.t"),
        // PostgreSQL ends a `--` comment at a carriage return too: the DELETE is a
        // statement of its own, not part of the comment.
        format!("SELECT 1 --x\r; DELETE FROM {schema}.t"),
    ] {
        for confirmed in [None, Some(false)] {
            let error = raw(&mut h, &sql, 0, 10, confirmed).expect_err(&sql);
            assert_eq!(
                rejected_code(&error),
                "query.confirmation_required",
                "{sql}"
            );
            assert!(!error.to_string().is_empty());
        }
    }
    // Nothing ran: all three rows and the table are still there.
    assert_eq!(
        h.scalar(&format!("SELECT count(*)::text FROM {schema}.t")),
        "3"
    );
    // Confirmed, it runs.
    let page = raw(
        &mut h,
        &format!("DELETE FROM {schema}.t WHERE id = 3"),
        0,
        10,
        None,
    )
    .expect("has WHERE");
    assert_eq!(page.row_count, 0);
    h.session.release_page(page.page_id).unwrap();
    let page = raw(
        &mut h,
        &format!("DELETE FROM {schema}.t"),
        0,
        10,
        Some(true),
    )
    .expect("confirmed");
    h.session.release_page(page.page_id).unwrap();
    assert_eq!(
        h.scalar(&format!("SELECT count(*)::text FROM {schema}.t")),
        "0"
    );

    // A write runs once at offset 0 and is refused at any other offset.
    let page = raw(
        &mut h,
        &format!("INSERT INTO {schema}.t VALUES (10, 1) RETURNING id"),
        0,
        10,
        Some(true),
    )
    .expect("write");
    assert!(!page.flags.truncated);
    h.session.release_page(page.page_id).unwrap();
    for sql in [
        format!("INSERT INTO {schema}.t VALUES (11, 1) RETURNING id"),
        format!("DELETE FROM {schema}.t WHERE id = 10"),
        format!("WITH d AS (DELETE FROM {schema}.t WHERE id = 10 RETURNING id) SELECT * FROM d"),
        "SELECT 1; SELECT 2".to_string(),
    ] {
        let error = raw(&mut h, &sql, 3, 10, Some(true)).expect_err(&sql);
        assert_eq!(rejected_code(&error), "query.not_pageable", "{sql}");
    }
    assert_eq!(
        h.scalar(&format!("SELECT count(*)::text FROM {schema}.t")),
        "1",
        "refused requests ran nothing"
    );
    // Pure reads page at any offset, SHOW and EXPLAIN included.
    let (page, _) = h.query("SHOW server_version", 0, 5);
    assert_eq!(page.row_count, 1);
    let page = raw(&mut h, "EXPLAIN SELECT 1", 2, 5, None).expect("EXPLAIN pages client-side");
    assert!(!page.flags.truncated);
    h.session.release_page(page.page_id).unwrap();

    // A write that returns more rows than a page says so and discards the rest.
    let page = raw(
        &mut h,
        &format!(
            "INSERT INTO {schema}.t SELECT g, 0 FROM generate_series(100, 109) g RETURNING id"
        ),
        0,
        4,
        Some(true),
    )
    .expect("write with RETURNING");
    assert_eq!(
        (page.row_count, page.flags.truncated),
        (4, false),
        "a non-pageable page never offers a next page, rows beyond the limit are dropped"
    );
    h.session.release_page(page.page_id).unwrap();
    assert_eq!(
        h.scalar(&format!("SELECT count(*)::text FROM {schema}.t")),
        "11",
        "all ten rows were inserted once"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn scripts_are_streamed_and_keep_at_most_one_page() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    // The last result set wins and is cut to one page (+ lookahead flag); a
    // million-row set is never held.
    let (page, batch) = h.query(
        "SELECT 1 AS a; SELECT g AS n, g::text AS t FROM generate_series(1, 1000000) g",
        0,
        50,
    );
    assert_eq!(
        (page.row_count, page.flags.truncated),
        (50, false),
        "a script page never offers a next page"
    );
    assert_eq!(batch.num_columns(), 2);
    assert_eq!(cell_string(&batch, 0, 49), "50");
    let (page, _) = h.query("SELECT 1; SELECT g FROM generate_series(1, 5) g", 0, 5);
    assert_eq!((page.row_count, page.flags.truncated), (5, false));
    // A script without a result set reports the rows affected.
    h.exec("CREATE TABLE {s}.s (x int)");
    let page = h.exec("INSERT INTO {s}.s VALUES (1), (2); INSERT INTO {s}.s VALUES (3)");
    assert!(page.columns.is_empty());
    assert_eq!(page.total_count, Some(3));
    // A real syntax error in a single statement is reported as one (SQLSTATE
    // 42601), not mistaken for a script.
    let error = h.sql("SELECT FROM WHERE").expect_err("syntax error");
    assert!(error.to_string().contains("syntax error"), "{error}");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn as_written_statements_render_exotic_types_as_text_not_placeholders() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.x (id int PRIMARY KEY, iv interval, arr int[], ip inet, tags text[], r int4range)");
    let (page, batch) = h.query(
        "INSERT INTO {s}.x VALUES (1, '1 day 02:03:04', '{1,2,3}', '10.0.0.1', '{a,\"b c\"}', '[1,5)') RETURNING id, iv, arr, ip, tags, r",
        0,
        10,
    );
    assert_eq!(page.row_count, 1);
    let cells: Vec<String> = (1..6).map(|c| text(&batch, c, 0)).collect();
    assert_eq!(
        cells,
        [
            "1 day 02:03:04",
            "{1,2,3}",
            "10.0.0.1",
            "{a,\"b c\"}",
            "[1,5)"
        ]
    );
    assert!(cells.iter().all(|cell| !cell.contains("<value>")));
    // The row was written exactly once.
    assert_eq!(h.scalar("SELECT count(*)::text FROM {s}.x"), "1");
    // UPDATE ... RETURNING and a CTE-less VALUES too.
    let (_, batch) = h.query(
        "UPDATE {s}.x SET iv = '2 days' WHERE id = 1 RETURNING iv, arr",
        0,
        10,
    );
    assert_eq!(
        (text(&batch, 0, 0), text(&batch, 1, 0)),
        ("2 days".into(), "{1,2,3}".into())
    );
    let (_, batch) = h.query("VALUES (interval '1 hour', ARRAY[1,2])", 0, 10);
    assert_eq!(
        (text(&batch, 0, 0), text(&batch, 1, 0)),
        ("01:00:00".into(), "{1,2}".into())
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn mutations_join_a_user_transaction_through_a_savepoint() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.t (id int PRIMARY KEY, v text)");
    let schema = h.schema.clone();
    let identity = serde_json::to_value(SqlTableIdentity {
        schema: Some(schema.clone()),
    })
    .unwrap();
    let insert = |id: i64| json!({"op": "insert", "values": {"id": id, "v": "m"}});
    let count = |h: &mut Harness| h.scalar(&format!("SELECT count(*)::text FROM {schema}.t"));

    // Inside the user's transaction: pending, rolled back with it.
    h.exec("BEGIN");
    h.exec("INSERT INTO {s}.t VALUES (1, 'user')");
    h.mutate("insert", Some("t"), identity.clone(), insert(2))
        .expect("joins the transaction");
    // A failing mutation undoes only itself and leaves the user's work open.
    let error = h
        .mutate("insert", Some("t"), identity.clone(), insert(2))
        .expect_err("duplicate");
    assert_eq!(rejected_code(&error), "mutation.failed");
    let error = h
        .mutate(
            "update",
            Some("t"),
            identity.clone(),
            json!({"op": "update", "key": {"id": 99}, "values": {"v": "x"}}),
        )
        .expect_err("no such row");
    assert_eq!(rejected_code(&error), "mutation.no_rows_affected");
    assert_eq!(
        count(&mut h),
        "2",
        "the user's row and the first mutation are still pending"
    );
    // A batch that fails midway is undone as a whole, user's work untouched.
    let error = h
        .mutate(
            "batch",
            Some("t"),
            identity.clone(),
            json!({"changes": [insert(3), insert(1)]}),
        )
        .expect_err("duplicate key in the batch");
    assert_eq!(rejected_code(&error), "mutation.failed");
    assert_eq!(count(&mut h), "2");
    h.exec("ROLLBACK");
    assert_eq!(
        count(&mut h),
        "0",
        "the user's ROLLBACK undid the joined mutation"
    );

    // Committed by the user: persists.
    h.exec("BEGIN");
    h.mutate("insert", Some("t"), identity.clone(), insert(5))
        .expect("joins");
    h.exec("COMMIT");
    assert_eq!(count(&mut h), "1");
    // Outside a transaction a mutation commits on its own.
    h.mutate("insert", Some("t"), identity.clone(), insert(6))
        .expect("own transaction");
    h.exec("BEGIN");
    h.exec("ROLLBACK");
    assert_eq!(
        count(&mut h),
        "2",
        "an autocommitted mutation survives a later ROLLBACK"
    );
    // A transaction that already failed rejects the mutation without being rolled back.
    h.exec("BEGIN");
    assert!(h.sql("SELECT 1/0").is_err());
    assert!(h
        .mutate("insert", Some("t"), identity.clone(), insert(7))
        .is_err());
    h.exec("ROLLBACK");
    assert_eq!(count(&mut h), "2");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_failed_rollback_marks_the_connection_broken() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.t (id int PRIMARY KEY, v text)");
    h.exec("INSERT INTO {s}.t VALUES (1, 'a')");
    // The trigger kills the session's own backend in the middle of the mutation,
    // so the statement fails and the ROLLBACK that follows cannot succeed.
    h.exec("CREATE FUNCTION {s}.kill_me() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); PERFORM pg_sleep(2); RETURN NEW; END $$");
    h.exec(
        "CREATE TRIGGER t_kill BEFORE UPDATE ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.kill_me()",
    );
    let identity = serde_json::to_value(SqlTableIdentity {
        schema: Some(h.schema.clone()),
    })
    .unwrap();
    let error = h
        .mutate(
            "update",
            Some("t"),
            identity.clone(),
            json!({"op": "update", "key": {"id": 1}, "values": {"v": "b"}}),
        )
        .expect_err("the backend was terminated");
    assert!(
        matches!(error, LoaderError::DriverRejected { .. }),
        "{error}"
    );

    // Later calls on the broken connection fail with `<area>.not_connected`.
    let error = h
        .mutate(
            "update",
            Some("t"),
            identity.clone(),
            json!({"op": "update", "key": {"id": 1}, "values": {"v": "c"}}),
        )
        .expect_err("broken");
    assert_eq!(rejected_code(&error), "mutation.not_connected");
    let error = h.sql("SELECT 1").expect_err("broken");
    assert_eq!(rejected_code(&error), "query.not_connected");
    let error = h
        .session
        .list_nav_nodes(
            &json!({"parent":null,"database":null,"payload":null,"cursor":null,"limit":10})
                .to_string(),
        )
        .expect_err("broken");
    assert_eq!(rejected_code(&error), "navigator.not_connected");
    let error = h
        .session
        .describe_object(&json!({"object": "t"}))
        .expect_err("broken");
    assert_eq!(rejected_code(&error), "describe.not_connected");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_cancelled_mutation_reports_mutation_cancelled_and_rolls_back() {
    let uri = pg_uri();
    let temp = TempDir::new().unwrap();
    let handle = DriverLoader.load(&stage(&temp)).expect("load");
    let mut session = handle
        .connect(&json!({ "uri": uri }).to_string())
        .expect("connect");
    let schema = format!("pgv2_{}_cm", std::process::id());
    for sql in [
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.t (id int PRIMARY KEY)"),
        format!("CREATE FUNCTION {schema}.slow() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(10); RETURN NEW; END $$"),
        format!("CREATE TRIGGER t_slow BEFORE INSERT ON {schema}.t FOR EACH ROW EXECUTE FUNCTION {schema}.slow()"),
    ] {
        let page = run(&mut session, &sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        session.release_page(page.page_id).unwrap();
    }
    let operation = session.begin_operation().unwrap();
    let id = operation.id();
    let connection_id = session.connection_id();
    let started = Instant::now();
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            session.mutate(&MutationRequest {
                operation_id: id,
                kind: "insert".into(),
                database: None,
                object: Some("t".into()),
                identity: json!({"schema": schema}),
                payload: json!({"op": "insert", "values": {"id": 1}}),
            })
        });
        std::thread::sleep(Duration::from_millis(500));
        handle
            .cancel_operation(connection_id, &CancelRequest { operation_id: id })
            .expect("cancel");
        worker.join().unwrap()
    });
    let error = result.expect_err("cancelled");
    assert_eq!(rejected_code(&error), "mutation.cancelled", "{error}");
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(operation);
    // Rolled back, connection usable, no transaction left open.
    let page = run(&mut session, &format!("DROP SCHEMA {schema} CASCADE"))
        .expect("drop (also proves no open transaction)");
    session.release_page(page.page_id).unwrap();
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn catalog_calls_do_not_wait_behind_a_running_user_query() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.t (id int PRIMARY KEY)");
    let schema = h.schema.clone();
    let database = h.scalar("SELECT current_database()");
    let operation = h.session.begin_operation().unwrap();
    let id = operation.id();
    let runner = h.session.query_runner().unwrap();
    let request = sleep_request(id, 10);
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    let nav = h.nav(json!({
        "parent": null, "database": database,
        "payload": {"database": database, "schema": schema}, "cursor": null, "limit": 10
    }));
    assert_eq!(names(&nav), ["t"]);
    let properties = h
        .session
        .describe_object(&json!({"schema": schema, "object": "t"}))
        .expect("describe during a running query");
    assert_eq!(properties["columns"].as_array().unwrap().len(), 1);
    let waited = started.elapsed();
    h.handle
        .cancel_operation(
            h.session.connection_id(),
            &CancelRequest { operation_id: id },
        )
        .expect("cancel");
    let _ = worker.join().unwrap();
    drop(operation);
    assert!(
        waited < Duration::from_secs(4),
        "catalog calls waited {waited:?}"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn re_paging_a_read_is_read_only_so_side_effects_cannot_repeat() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE SEQUENCE {s}.sq");
    h.exec("CREATE TABLE {s}.log (n int)");
    h.exec("CREATE FUNCTION {s}.w() RETURNS int LANGUAGE sql AS $$ INSERT INTO {s}.log VALUES (1) RETURNING n $$");
    h.exec("CREATE TABLE {s}.t (id int PRIMARY KEY)");
    h.exec("INSERT INTO {s}.t SELECT generate_series(1, 5)");
    let schema = h.schema.clone();
    let raw = |h: &mut Harness, sql: &str, offset: u64| {
        let operation = h.session.begin_operation().unwrap();
        h.session
            .open_query(&query_request(operation.id(), sql, offset, 2, Some(true)))
    };
    let last_value =
        |h: &mut Harness| h.scalar(&format!("SELECT last_value::text FROM {schema}.sq"));
    let logged = |h: &mut Harness| h.scalar(&format!("SELECT count(*)::text FROM {schema}.log"));

    // Offset 0 runs the read once, side effects included (that is the user's query).
    let nextval = format!("SELECT nextval('{schema}.sq') FROM generate_series(1, 5)");
    let page = raw(&mut h, &nextval, 0).expect("first page");
    assert_eq!(page.row_count, 2);
    h.session.release_page(page.page_id).unwrap();
    // The lookahead row means 3 values were drawn (2 + 1); remember where we are.
    let after_first = last_value(&mut h);
    // Re-paging runs READ ONLY: the write is refused, nothing repeats.
    let error = raw(&mut h, &nextval, 2).expect_err("nextval is a write");
    assert_eq!(rejected_code(&error), "query.not_pageable", "{error}");
    assert_eq!(
        last_value(&mut h),
        after_first,
        "the sequence did not advance"
    );

    let writer = format!("SELECT {schema}.w() FROM generate_series(1, 5)");
    let page = raw(&mut h, &writer, 0).expect("first page runs the function");
    h.session.release_page(page.page_id).unwrap();
    let after_first = logged(&mut h);
    let error = raw(&mut h, &writer, 2).expect_err("the function writes");
    assert_eq!(rejected_code(&error), "query.not_pageable", "{error}");
    assert_eq!(
        logged(&mut h),
        after_first,
        "the function did not write again"
    );

    // Locking reads are writes for READ ONLY purposes.
    let page = raw(
        &mut h,
        &format!("SELECT * FROM {schema}.t ORDER BY id FOR UPDATE"),
        0,
    )
    .expect("offset 0");
    h.session.release_page(page.page_id).unwrap();
    let error = raw(
        &mut h,
        &format!("SELECT * FROM {schema}.t ORDER BY id FOR UPDATE"),
        2,
    )
    .expect_err("FOR UPDATE");
    assert_eq!(rejected_code(&error), "query.not_pageable", "{error}");

    // Plain reads still page at any offset (and leave the session clean).
    let (page, batch) = h.query("SELECT id FROM {s}.t ORDER BY id", 2, 2);
    assert_eq!((page.row_count, page.flags.truncated), (2, true));
    assert_eq!(text_of_int(&batch), [3, 4]);
    let (page, _) = h.query("SELECT id FROM {s}.t ORDER BY id", 4, 2);
    assert_eq!((page.row_count, page.flags.truncated), (1, false));
    let (page, _) = h.query("SHOW server_version", 0, 5);
    assert_eq!(page.row_count, 1);
    let identity = serde_json::to_value(SqlTableIdentity {
        schema: Some(schema.clone()),
    })
    .unwrap();
    h.mutate(
        "insert",
        Some("t"),
        identity.clone(),
        json!({"op": "insert", "values": {"id": 6}}),
    )
    .expect("no read-only transaction is left behind");

    // Inside the user's own transaction: its mode cannot change, so the re-page
    // uses a savepoint with transaction_read_only on; the user's work survives.
    h.exec("BEGIN");
    h.exec("INSERT INTO {s}.t VALUES (7)");
    let (page, _) = h.query("SELECT id FROM {s}.t ORDER BY id", 5, 2);
    assert_eq!(
        page.row_count, 2,
        "the user's uncommitted row is visible to the page"
    );
    let in_txn_sequence = last_value(&mut h);
    let error = raw(&mut h, &nextval, 2).expect_err("still read-only inside a user transaction");
    assert_eq!(rejected_code(&error), "query.not_pageable", "{error}");
    assert_eq!(
        last_value(&mut h),
        in_txn_sequence,
        "the sequence did not advance inside the user transaction either"
    );
    h.exec("INSERT INTO {s}.t VALUES (8)");
    h.exec("COMMIT");
    assert_eq!(
        h.scalar(&format!("SELECT count(*)::text FROM {schema}.t")),
        "8",
        "the user's transaction committed intact"
    );
}

/// A `payload.read_only` request for `sql` (`{s}` is the test schema).
fn run_read_only(h: &mut Harness, sql: &str) -> Result<QueryPage, LoaderError> {
    let sql = sql.replace("{s}", &h.schema);
    let operation = h.session.begin_operation().expect("operation");
    h.session
        .open_query(&read_request(operation.id(), &sql, true))
}

/// A READ ONLY run is a scope the text must not be able to leave: `COMMIT` (or `END`,
/// `SET TRANSACTION READ WRITE`, `RELEASE` ...) ends it and what follows runs as a normal
/// write. Also the session's `standard_conforming_strings = off` must not make the server
/// split the text where the classifier did not.
#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn read_only_text_cannot_leave_its_scope_with_transaction_control() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.tablex_ro_t (id int PRIMARY KEY, note text)");
    h.exec("INSERT INTO {s}.tablex_ro_t VALUES (1, 'a'), (2, 'b'), (3, 'c')");
    let contents = |h: &mut Harness| {
        h.scalar("SELECT string_agg(id::text || note, ',' ORDER BY id) FROM {s}.tablex_ro_t")
    };
    let before = contents(&mut h);
    assert_eq!(before, "1a,2b,3c");
    let refused = |h: &mut Harness, sql: &str| {
        let error = match run_read_only(h, sql) {
            Ok(_) => panic!("read-only text was accepted: {sql}"),
            Err(error) => error,
        };
        assert_eq!(
            rejected_code(&error),
            "query.read_only_violation",
            "{sql}: {error}"
        );
        assert_eq!(contents(h), before, "nothing changed after: {sql}");
        error.to_string()
    };
    let accepted = |h: &mut Harness, sql: &str| {
        let page = run_read_only(h, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        h.session.release_page(page.page_id).expect("release");
        assert_eq!(contents(h), before, "nothing changed after: {sql}");
    };
    let refusals = [
        "SELECT 1; COMMIT; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
        "COMMIT; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
        "SELECT 1; END; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
        "SELECT 1; ABORT",
        "SELECT 1; ROLLBACK",
        "SELECT 1; BEGIN; DELETE FROM {s}.tablex_ro_t WHERE id = 1; COMMIT",
        "START TRANSACTION",
        "COMMIT",
        "SET TRANSACTION READ WRITE",
        "SELECT 1; SET LOCAL transaction_read_only = off; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        "SELECT 1; SET autocommit = on",
        "SELECT 1; RELEASE SAVEPOINT tablex_read",
        "SELECT 1; ROLLBACK TO SAVEPOINT tablex_read; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
        "SAVEPOINT s",
        "SELECT 1; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
        "SELECT 1; INSERT INTO {s}.tablex_ro_t VALUES (9, 'z')",
        // `--` ends at `\r` on PostgreSQL: the DELETE is a second statement.
        "SELECT 1 --x\r; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
    ];
    for sql in refusals {
        refused(&mut h, sql);
    }
    // A single write is refused up front too.
    refused(&mut h, "DELETE FROM {s}.tablex_ro_t WHERE id = 1");
    let message = refused(&mut h, "SELECT 1; COMMIT; DELETE FROM {s}.tablex_ro_t");
    assert!(message.contains("statement 2"), "{message}");

    // Plain reads still run: one statement, EXPLAIN [ANALYZE], a script of plain reads,
    // and text that only looks like a script.
    for sql in [
        "SELECT count(*) FROM {s}.tablex_ro_t",
        "EXPLAIN SELECT * FROM {s}.tablex_ro_t WHERE id = 1",
        "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM {s}.tablex_ro_t",
        "SELECT 1; SELECT id FROM {s}.tablex_ro_t ORDER BY id",
        "SELECT 'COMMIT; DELETE FROM x', E'\\'; COMMIT;'",
        "SELECT $$; COMMIT; DELETE FROM x$$",
        "SELECT 1 /* a /* ; COMMIT; */ */",
        "SHOW server_version",
    ] {
        accepted(&mut h, sql);
    }
    // The refusals left no transaction behind: the session is clean.
    h.exec("CREATE TABLE {s}.tablex_ro_probe (n int)");

    // Inside the user's own transaction (savepoint scope) the same rules hold, and the
    // user's transaction survives.
    h.exec("BEGIN");
    refused(
        &mut h,
        "SELECT 1; COMMIT; DELETE FROM {s}.tablex_ro_t WHERE id = 1",
    );
    refused(&mut h, "DELETE FROM {s}.tablex_ro_t WHERE id = 1");
    accepted(&mut h, "SELECT 1; SELECT id FROM {s}.tablex_ro_t");
    h.exec("INSERT INTO {s}.tablex_ro_probe VALUES (1)");
    h.exec("ROLLBACK");
    assert_eq!(
        h.scalar("SELECT count(*)::text FROM {s}.tablex_ro_probe"),
        "0"
    );

    // `standard_conforming_strings = off` in the session: the server would read `\'` as an
    // escaped quote and split this text into three statements. A read-only run reads it as
    // the classifier did (one string), so the `COMMIT; DELETE` stays inside it.
    h.exec("SET standard_conforming_strings = off");
    let probe = "SELECT 1; SELECT 'x\\''; COMMIT; DELETE FROM {s}.tablex_ro_t WHERE id = 1; --'";
    let page = run_read_only(&mut h, probe).expect("one statement under read_only");
    h.session.release_page(page.page_id).expect("release");
    assert_eq!(
        contents(&mut h),
        before,
        "the string swallowed the COMMIT and DELETE"
    );
    assert_eq!(
        h.scalar("SHOW standard_conforming_strings"),
        "off",
        "the session's own setting is back after the read-only run"
    );
    // Control: without `read_only` that session really runs the DELETE.
    h.exec(probe);
    assert_eq!(
        contents(&mut h),
        "2b,3c",
        "without read_only the probe deleted a row"
    );
    h.exec("SET standard_conforming_strings = on");
}

/// `DO` and `CALL` are refused under `read_only` (they are not reads), and could not end the
/// driver's transaction anyway: inside a transaction block the server refuses a `COMMIT`
/// in a `DO` block or procedure.
#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn read_only_refuses_do_and_call_and_the_server_refuses_their_commit_in_a_transaction_block() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.tablex_ro_t (id int PRIMARY KEY)");
    h.exec("INSERT INTO {s}.tablex_ro_t VALUES (1), (2)");
    h.exec(
        "CREATE PROCEDURE {s}.tablex_ro_commit() LANGUAGE plpgsql AS $$ BEGIN \
         COMMIT; DELETE FROM {s}.tablex_ro_t WHERE id = 1; END $$",
    );
    let count = |h: &mut Harness| h.scalar("SELECT count(*)::text FROM {s}.tablex_ro_t");
    for sql in [
        "DO $$ BEGIN COMMIT; DELETE FROM {s}.tablex_ro_t WHERE id = 1; END $$",
        "CALL {s}.tablex_ro_commit()",
    ] {
        let error = run_read_only(&mut h, sql).expect_err(sql);
        assert_eq!(
            rejected_code(&error),
            "query.read_only_violation",
            "{sql}: {error}"
        );
        assert_eq!(count(&mut h), "2", "nothing changed after: {sql}");
    }
    // Not read-only but in a transaction block (what the driver's scope is): the COMMIT
    // inside is refused by the server, and the DELETE after it never runs.
    for sql in [
        "DO $$ BEGIN COMMIT; DELETE FROM {s}.tablex_ro_t WHERE id = 1; END $$",
        "CALL {s}.tablex_ro_commit()",
    ] {
        h.exec("BEGIN READ ONLY");
        let error = h
            .sql(&sql.replace("{s}", &h.schema.clone()))
            .expect_err(sql);
        assert!(
            error
                .to_string()
                .contains("invalid transaction termination"),
            "{sql}: {error}"
        );
        h.exec("ROLLBACK");
        assert_eq!(count(&mut h), "2", "nothing changed after: {sql}");
    }
}

fn text_of_int(batch: &RecordBatch) -> Vec<i64> {
    let ids = batch
        .column(0)
        .as_primitive::<arrow_array::types::Int64Type>();
    (0..batch.num_rows()).map(|row| ids.value(row)).collect()
}

// ---------------------------------------------------------------------------
// TLS through the real ABI (rust-engine/scripts/test-tls-servers.sh start;
// `source` its env file; the plain server is TABLEX_TEST_PG_URI)
// ---------------------------------------------------------------------------

/// URI of the TLS test server that verifies the certificate for `db.tablex.test`
/// while the socket goes to 127.0.0.1 (`hostaddr`, as the SSH tunnel will do).
fn tls_uri() -> Option<String> {
    let Ok(port) = std::env::var("TABLEX_TEST_PG_TLS_PORT") else {
        eprintln!("skipped: TABLEX_TEST_PG_TLS_PORT is not set (test-tls-servers.sh)");
        return None;
    };
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    Some(format!(
        "postgres://{}:{}@{}:{port}/{}?sslmode=verify-full&sslrootcert={}&hostaddr=127.0.0.1",
        var("TABLEX_TEST_PG_TLS_USER"),
        var("TABLEX_TEST_PG_TLS_PASSWORD"),
        var("TABLEX_TEST_TLS_SERVER_NAME"),
        var("TABLEX_TEST_PG_TLS_DB"),
        var("TABLEX_TEST_TLS_CA"),
    ))
}

#[test]
#[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
fn a_tls_session_runs_queries_and_pages() {
    let Some(uri) = tls_uri() else { return };
    let mut h = Harness::new(&uri);
    assert_eq!(
        h.scalar("SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()"),
        "true",
        "the session is encrypted"
    );
    h.exec("CREATE TABLE {s}.t AS SELECT g::bigint AS id, md5(g::text) AS h FROM generate_series(1, 25000) g");
    // First page, a middle page and the last partial page of the same table.
    for (offset, limit, first, rows) in [
        (0, 1000, 1, 1000),
        (12_000, 1000, 12_001, 1000),
        (24_500, 1000, 24_501, 500),
    ] {
        let (page, batch) = h.query("SELECT id, h FROM {s}.t ORDER BY id", offset, limit);
        assert_eq!(page.row_count, rows, "offset {offset}");
        let ids = text_of_int(&batch);
        assert_eq!(ids.first(), Some(&first));
        assert_eq!(ids.len() as u64, rows);
    }
    // A catalog request runs on the second (metadata) connection: also TLS, same settings.
    let schemas =
        h.nav(json!({"parent":null,"database":null,"payload":null,"cursor":null,"limit":100}));
    assert!(!schemas.nodes.is_empty());
}

#[test]
#[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
fn cancelling_a_running_query_works_over_tls() {
    let Some(uri) = tls_uri() else { return };
    let mut h = Harness::new(&uri);
    let operation = h.session.begin_operation().unwrap();
    let id = operation.id();
    let runner = h.session.query_runner().unwrap();
    let request = sleep_request(id, 10);
    let started = Instant::now();
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(400));
    h.handle
        .cancel_operation(
            h.session.connection_id(),
            &CancelRequest { operation_id: id },
        )
        .expect("cancel_operation");
    let error = worker.join().expect("query thread").expect_err("cancelled");
    assert_eq!(rejected_code(&error), "query.cancelled", "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    drop(operation);
    assert_eq!(h.scalar("SELECT 'alive'"), "alive");
}

#[test]
#[ignore = "needs test-tls-servers.sh and TABLEX_TEST_PG_URI"]
fn throughput_over_tls_versus_plain() {
    let plain = pg_uri();
    let Some(tls) = tls_uri() else {
        return;
    };
    // 100k rows x (bigint + 32-char text + 40-char text), one page, best of 5 after a warm-up.
    let sql = "SELECT g::bigint AS id, md5(g::text) AS a, repeat('x', 40) AS b FROM generate_series(1, 100000) g";
    let measure = |uri: &str| {
        let mut h = Harness::new(uri);
        let mut best = Duration::MAX;
        for round in 0..6 {
            let started = Instant::now();
            let (page, _) = h.query(sql, 0, 100_000);
            let took = started.elapsed();
            assert_eq!(page.row_count, 100_000);
            if round > 0 {
                best = best.min(took);
            }
        }
        best
    };
    let plain_time = measure(&plain);
    let tls_time = measure(&tls);
    let rate = |t: Duration| 100_000.0 / t.as_secs_f64();
    eprintln!(
        "THROUGHPUT plain: {:.0} rows/s ({plain_time:?}); tls: {:.0} rows/s ({tls_time:?}); tls/plain = {:.2}",
        rate(plain_time),
        rate(tls_time),
        rate(tls_time) / rate(plain_time)
    );
}

// ---------------------------------------------------------------------------
// K1: a cancel delivered at ANY moment of an operation stops it
// ---------------------------------------------------------------------------
//
// A cancel is swept across the start of an operation (not registered, pending,
// active before its first round trip, between the preparatory round trips,
// statement sent but not executing, executing). A lost cancel shows as the call
// still running after `CANCEL_DEADLINE`; the sweep then cancels again so the
// round ends, and counts the round as lost.

const CANCEL_DEADLINE: Duration = Duration::from_secs(3);

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn read_request(operation: OperationId, sql: &str, read_only: bool) -> String {
    json!({
        "kind": "sql", "operation_id": operation, "database": null, "object": null,
        "offset": 0, "limit": 10, "payload": { "sql": sql, "read_only": read_only }
    })
    .to_string()
}

/// What one swept round did.
struct Round {
    /// `ok`, or the driver's error code.
    outcome: String,
    /// From the cancel call to the return of the cancelled call.
    latency: Duration,
    /// The call was still running `CANCEL_DEADLINE` after the cancel.
    lost: bool,
}

/// Runs `work` on its own thread, cancels operation `id` after `delay` (the
/// cancel is delivered by this thread, like the host does), and waits.
fn cancel_after<T: Send>(
    handle: &Arc<DriverHandle>,
    connection_id: u64,
    id: OperationId,
    delay: Duration,
    work: impl FnOnce() -> Result<T, LoaderError> + Send,
) -> Round {
    std::thread::scope(|scope| {
        let (done, finished) = std::sync::mpsc::channel();
        let worker = scope.spawn(move || {
            let result = work();
            let _ = done.send(Instant::now());
            result.map(|_| ())
        });
        let target = Instant::now() + delay;
        while Instant::now() < target {
            std::hint::spin_loop();
        }
        let cancelled_at = Instant::now();
        let cancel = CancelRequest { operation_id: id };
        let _ = handle.cancel_operation(connection_id, &cancel);
        let mut lost = false;
        let finished_at = loop {
            let budget = if lost {
                Duration::from_millis(200)
            } else {
                CANCEL_DEADLINE.saturating_sub(cancelled_at.elapsed())
            };
            match finished.recv_timeout(budget) {
                Ok(at) => break at,
                Err(_) => {
                    lost = true;
                    let _ = handle.cancel_operation(connection_id, &cancel);
                }
            }
        };
        let outcome = match worker.join().expect("worker thread") {
            Ok(()) => "ok".to_owned(),
            Err(error) => rejected_code(&error).to_owned(),
        };
        Round {
            outcome,
            latency: finished_at.saturating_duration_since(cancelled_at),
            lost,
        }
    })
}

/// The sweep's bookkeeping.
#[derive(Default)]
struct Sweep {
    rounds: usize,
    lost: usize,
    wrong: Vec<String>,
    unusable: Vec<String>,
    worst: Duration,
}

impl Sweep {
    fn record(&mut self, round: usize, delay: Duration, result: &Round, expected: &str) {
        self.rounds += 1;
        self.worst = self.worst.max(result.latency);
        if result.lost {
            self.lost += 1;
            eprintln!("  lost: round {round}, cancel after {delay:?}");
        }
        if result.outcome != expected {
            eprintln!(
                "  wrong outcome: round {round}, cancel after {delay:?}: {}",
                result.outcome
            );
            self.wrong
                .push(format!("round {round}: {}", result.outcome));
        }
    }

    fn report(&self, what: &str) {
        eprintln!(
            "K1 {what}: {} rounds, {} lost, {} wrong outcome, {} unusable after, worst cancel latency {:?}",
            self.rounds,
            self.lost,
            self.wrong.len(),
            self.unusable.len(),
            self.worst
        );
    }

    fn assert_clean(&self, what: &str) {
        assert_eq!(self.lost, 0, "{what}: lost cancels");
        assert!(self.wrong.is_empty(), "{what}: {:?}", self.wrong);
        assert!(self.unusable.is_empty(), "{what}: {:?}", self.unusable);
        assert!(self.worst < CANCEL_DEADLINE, "{what}: {:?}", self.worst);
    }
}

/// Checks a swept session from a second one: the session answers a read and takes
/// a write, and the write is *committed* - a session left inside a read-write
/// transaction would pass its own three statements.
struct Checker {
    other: Harness,
    /// Inserts that succeeded on the swept session.
    committed: usize,
}

impl Checker {
    fn new(uri: &str) -> Self {
        Self {
            other: Harness::new(uri),
            committed: 0,
        }
    }

    fn usable(&mut self, h: &mut Harness) -> Result<(), String> {
        for sql in [
            "SELECT 1",
            "INSERT INTO {s}.sweep VALUES (1)",
            "SELECT count(*) FROM {s}.sweep",
        ] {
            let sql = sql.replace("{s}", &h.schema);
            match h.sql(&sql) {
                Ok(page) => {
                    let _ = h.session.release_page(page.page_id);
                }
                Err(error) => return Err(format!("`{sql}`: {error}")),
            }
            if sql.starts_with("INSERT") {
                self.committed += 1;
            }
        }
        let seen = self
            .other
            .scalar(&format!("SELECT count(*)::text FROM {}.sweep", h.schema));
        if seen == self.committed.to_string() {
            Ok(())
        } else {
            Err(format!(
                "a second connection sees {seen} committed rows, {} were inserted (the session is left in a transaction)",
                self.committed
            ))
        }
    }
}

/// The time span the sweep covers: a few times the preparatory phase of a
/// read-only query (`SELECT 1` end to end) plus the cancel's own delivery.
fn sweep_span(h: &mut Harness) -> Duration {
    let mut times = (0..25)
        .map(|_| {
            let operation = h.session.begin_operation().unwrap();
            let started = Instant::now();
            let page = h
                .session
                .open_query(&read_request(operation.id(), "SELECT 1", true))
                .unwrap();
            let took = started.elapsed();
            h.session.release_page(page.page_id).unwrap();
            took
        })
        .collect::<Vec<_>>();
    times.sort();
    let select_one = times[times.len() / 2];
    let span = Duration::from_micros(env_number("TABLEX_SWEEP_SPAN_US", 0))
        .max(select_one * 4)
        .max(Duration::from_millis(4));
    eprintln!("K1 sweep: median `SELECT 1` {select_one:?}, sweep span {span:?}");
    span
}

fn sweep_queries(
    h: &mut Harness,
    checker: &mut Checker,
    sql: &str,
    read_only: bool,
    span: Duration,
) -> Sweep {
    let rounds = env_number("TABLEX_SWEEP_ROUNDS", 150) as usize;
    let mut sweep = Sweep::default();
    for round in 0..rounds {
        let delay = span.mul_f64(round as f64 / rounds as f64);
        let operation = h.session.begin_operation().unwrap();
        let runner = h.session.query_runner().unwrap();
        let request = read_request(operation.id(), sql, read_only);
        let result = cancel_after(
            &h.handle,
            h.session.connection_id(),
            operation.id(),
            delay,
            || runner.open_query(&request),
        );
        drop(operation);
        sweep.record(round, delay, &result, "query.cancelled");
        if let Err(message) = checker.usable(h) {
            eprintln!("  unusable: round {round}, cancel after {delay:?}: {message}");
            sweep.unusable.push(format!("round {round}: {message}"));
        }
    }
    sweep
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_cancel_at_any_moment_of_a_query_stops_it() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.sweep (n int)");
    let mut checker = Checker::new(&uri);
    let span = sweep_span(&mut h);
    let mut failures = Vec::new();
    for (what, sql) in [
        ("pg_sleep", "SELECT pg_sleep(30)"),
        (
            "cpu bound",
            "SELECT count(*) FROM generate_series(1, 3000000000::bigint)",
        ),
    ] {
        for read_only in [true, false] {
            let label = format!("{what}, read_only={read_only}");
            let sweep = sweep_queries(&mut h, &mut checker, sql, read_only, span);
            sweep.report(&label);
            if sweep.lost > 0 || !sweep.wrong.is_empty() || !sweep.unusable.is_empty() {
                failures.push((label, sweep));
            }
        }
    }
    for (label, sweep) in &failures {
        sweep.assert_clean(label);
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_cancel_at_any_moment_of_a_mutation_stops_it() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.sweep (n int)");
    h.exec("CREATE TABLE {s}.m (id int PRIMARY KEY)");
    h.exec("CREATE FUNCTION {s}.slow() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(30); RETURN NEW; END $$");
    h.exec("CREATE TRIGGER m_slow BEFORE INSERT ON {s}.m FOR EACH ROW EXECUTE FUNCTION {s}.slow()");
    let mut checker = Checker::new(&uri);
    let span = sweep_span(&mut h);
    let rounds = env_number("TABLEX_SWEEP_ROUNDS", 150) as usize;
    let mut sweep = Sweep::default();
    let (handle, connection_id) = (Arc::clone(&h.handle), h.session.connection_id());
    for round in 0..rounds {
        let delay = span.mul_f64(round as f64 / rounds as f64);
        let operation = h.session.begin_operation().unwrap();
        let id = operation.id();
        let request = MutationRequest {
            operation_id: id,
            kind: "insert".into(),
            database: None,
            object: Some("m".into()),
            identity: json!({"schema": h.schema}),
            payload: json!({"op": "insert", "values": {"id": round}}),
        };
        let result = cancel_after(&handle, connection_id, id, delay, || {
            h.session.mutate(&request)
        });
        drop(operation);
        sweep.record(round, delay, &result, "mutation.cancelled");
        if let Err(message) = checker.usable(&mut h) {
            eprintln!("  unusable: round {round}, cancel after {delay:?}: {message}");
            sweep.unusable.push(format!("round {round}: {message}"));
        }
    }
    sweep.report("mutation (trigger sleeps 30 s)");
    assert_eq!(
        h.scalar(
            "SELECT count(*)::text FROM {s}.m"
                .replace("{s}", &h.schema)
                .as_str()
        ),
        "0"
    );
    sweep.assert_clean("mutation");
}

/// A cancel for operation X that lands late must never fail the operation Y
/// that follows on the same connection.
#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_late_cancel_never_fails_the_operation_that_follows() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let rounds = env_number("TABLEX_SWEEP_ROUNDS", 300) as usize;
    let connection_id = h.session.connection_id();
    let (mut cancelled, mut completed) = (0, 0);
    let mut following: std::collections::BTreeMap<String, usize> = Default::default();
    for round in 0..rounds {
        let read_only = round % 2 == 0;
        let operation = h.session.begin_operation().unwrap();
        let id = operation.id();
        let runner = h.session.query_runner().unwrap();
        let request = read_request(id, "SELECT pg_sleep(0.02)", read_only);
        let worker = std::thread::spawn(move || runner.open_query(&request));
        let handle = Arc::clone(&h.handle);
        let canceller = std::thread::spawn(move || {
            // Around the end of the 20 ms statement and just after it.
            std::thread::sleep(Duration::from_micros(12_000 + (round as u64 % 40) * 450));
            let _ = handle.cancel_operation(connection_id, &CancelRequest { operation_id: id });
        });
        match worker.join().unwrap() {
            Ok(page) => {
                completed += 1;
                let _ = h.session.release_page(page.page_id);
            }
            Err(error) => {
                assert_eq!(rejected_code(&error), "query.cancelled", "{error}");
                cancelled += 1;
            }
        }
        drop(operation);
        // The next operation starts while the cancel may still be on its way.
        let next = h.session.begin_operation().unwrap();
        let outcome =
            match h
                .session
                .open_query(&read_request(next.id(), "SELECT pg_sleep(0.15)", read_only))
            {
                Ok(page) => {
                    let _ = h.session.release_page(page.page_id);
                    "ok".to_owned()
                }
                Err(error) => rejected_code(&error).to_owned(),
            };
        *following.entry(outcome).or_default() += 1;
        canceller.join().unwrap();
    }
    eprintln!(
        "K1 late cancel: X {cancelled} cancelled / {completed} completed; Y outcomes {following:?}"
    );
    // The late window was exercised: some X were cancelled, some finished first.
    assert!(
        cancelled > 0 && completed > 0,
        "X: {cancelled} cancelled / {completed} completed; the cancel delay misses the end of X"
    );
    assert_eq!(following.len(), 1, "{following:?}");
    assert!(following.contains_key("ok"), "{following:?}");
}

/// What the cancel bookkeeping costs when nobody cancels: `SELECT 1` x 2000 through
/// the driver (the median of five passes, microseconds per call).
#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn select_one_cost_when_nobody_cancels() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    for read_only in [false, true] {
        let mut passes = (0..5)
            .map(|_| {
                let started = Instant::now();
                for _ in 0..2000 {
                    let operation = h.session.begin_operation().unwrap();
                    let page = h
                        .session
                        .open_query(&read_request(operation.id(), "SELECT 1", read_only))
                        .unwrap();
                    h.session.release_page(page.page_id).unwrap();
                }
                started.elapsed().as_micros() as f64 / 2000.0
            })
            .collect::<Vec<_>>();
        passes.sort_by(f64::total_cmp);
        eprintln!("cost postgresql read_only={read_only}: {:.1} us per SELECT 1 (median of 5 x 2000; passes {passes:.1?})", passes[2]);
    }
}

// ---------------------------------------------------------------------------
// value correctness: one text form per type, in every path
// ---------------------------------------------------------------------------

/// The text of one cell as the app would see it (`NULL` for SQL NULL).
fn cell_string(batch: &RecordBatch, column: usize, row: usize) -> String {
    let array = batch.column(column);
    if array.is_null(row) {
        return "NULL".to_owned();
    }
    arrow::util::display::array_value_to_string(array, row).expect("displayable cell")
}

/// Every pair of columns `(x, x::text)` after `skip` leading columns must read
/// the same in every row: the cell is the server's own text, never a placeholder.
fn assert_pairs_equal(batch: &RecordBatch, skip: usize, what: &str) {
    assert!(batch.num_rows() > 0, "{what}: no rows");
    assert_eq!(
        (batch.num_columns() - skip) % 2,
        0,
        "{what}: columns come in pairs"
    );
    for pair in (skip..batch.num_columns()).step_by(2) {
        let name = batch.schema().field(pair).name().clone();
        for row in 0..batch.num_rows() {
            let (shown, server) = (
                cell_string(batch, pair, row),
                cell_string(batch, pair + 1, row),
            );
            assert_eq!(shown, server, "{what}: column {name}, row {row}");
            assert!(
                !shown.contains("<value>"),
                "{what}: {name} is a placeholder"
            );
        }
    }
}

fn pair_list(columns: &[&str]) -> String {
    columns
        .iter()
        .map(|column| format!("{column}, {column}::text AS {column}_t"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn bytea_reads_in_full_and_the_same_text_writes_back_identical_bytes() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let identity = json!({ "schema": h.schema });
    h.exec("CREATE TABLE {s}.b (k bytea PRIMARY KEY, v bytea, orig bytea)");
    // Empty, NUL, a lone backslash, non-UTF-8, 70 bytes (past the old 64-byte
    // preview) and 1 MiB.
    h.exec(
        "INSERT INTO {s}.b SELECT k, v, v FROM (VALUES
           ('\\x'::bytea, '\\x'::bytea),
           ('\\x00'::bytea, '\\x00'::bytea),
           ('\\x5c'::bytea, decode('5c5c00ff', 'hex')),
           ('\\xfffe80'::bytea, decode(repeat('ab', 70), 'hex')),
           (decode(repeat('0a', 70), 'hex'), decode(repeat('0a5cff00', 262144), 'hex'))
         ) t(k, v)",
    );
    let (_, batch) = h.query(
        "SELECT k, v, k::text AS kt, v::text AS vt, length(v) AS n FROM {s}.b ORDER BY length(v), k",
        0,
        100,
    );
    assert_eq!(batch.num_rows(), 5);
    let mut sizes = Vec::new();
    for row in 0..5 {
        let (key, value) = (text(&batch, 0, row), text(&batch, 1, row));
        assert_eq!(key, text(&batch, 2, row), "key text, row {row}");
        assert_eq!(value, text(&batch, 3, row), "value text, row {row}");
        let length = batch
            .column(4)
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(row) as usize;
        assert!(
            value.starts_with("\\x") && !value.contains("..."),
            "row {row}"
        );
        assert_eq!(value.len(), 2 + 2 * length, "never cut, row {row}");
        sizes.push(length);
    }
    assert_eq!(sizes, [0, 1, 4, 70, 1 << 20]);

    // Cell edit: lose the bytes, then write back the text that was read.
    h.exec("UPDATE {s}.b SET v = NULL");
    for row in 0..5 {
        let result = h
            .mutate(
                "update",
                Some("b"),
                identity.clone(),
                json!({"op": "update", "key": {"k": text(&batch, 0, row)}, "values": {"v": text(&batch, 1, row)}}),
            )
            .unwrap_or_else(|error| panic!("update row {row}: {error}"));
        assert_eq!(
            result.affected_rows, 1,
            "the bytea key matched exactly one row, row {row}"
        );
    }
    assert_eq!(
        h.scalar("SELECT (count(*) = 5 AND bool_and(v = orig))::text FROM {s}.b"),
        "true",
        "the stored bytes are identical"
    );

    // Clone Row: the full text of the 1 MiB value inserts the same bytes under a new key.
    h.mutate(
        "insert",
        Some("b"),
        identity.clone(),
        json!({"op": "insert", "values": {"k": "\\x01", "v": text(&batch, 1, 4), "orig": text(&batch, 1, 4)}}),
    )
    .expect("clone");
    assert_eq!(
        h.scalar("SELECT (v = orig AND length(v) = 1048576)::text FROM {s}.b WHERE k = '\\x01'"),
        "true"
    );
    // A bytea key deletes exactly its row (NUL, backslash and 0xff in the key).
    for key in ["\\x00", "\\x5c", "\\xfffe80", "\\x"] {
        h.mutate(
            "delete",
            Some("b"),
            identity.clone(),
            json!({"op": "delete", "key": {"k": key}}),
        )
        .unwrap_or_else(|error| panic!("delete {key}: {error}"));
    }
    assert_eq!(h.scalar("SELECT count(*)::text FROM {s}.b"), "2");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn real_columns_are_single_precision_in_every_path() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.f (id int PRIMARY KEY, r real)");
    h.exec(
        "INSERT INTO {s}.f VALUES (1, 0.1), (2, 3.14), (3, '1e-45'), (4, '3.4028235e38'),
           (5, 'NaN'), (6, 'Infinity'), (7, '-Infinity'), (8, NULL), (9, 16777217)",
    );
    // The same column through the binary path, the simple protocol (an interval
    // forces the server-text path) and a script.
    let paged = "SELECT r, r::text AS t FROM {s}.f ORDER BY id";
    let simple = "SELECT r, r::text AS t, '1 day'::interval AS iv FROM {s}.f ORDER BY id";
    let script = "SELECT 1; SELECT r, r::text AS t FROM {s}.f ORDER BY id";
    for (what, sql) in [("paged", paged), ("simple", simple), ("script", script)] {
        let (page, batch) = h.query(sql, 0, 100);
        assert_eq!(page.columns[0].data_type, "float32", "{what}");
        assert_eq!(
            batch.schema().field(0).data_type(),
            &DataType::Float32,
            "{what}"
        );
        assert_eq!(
            batch
                .schema()
                .field(0)
                .metadata()
                .get("tablex.native_type")
                .map(String::as_str),
            Some("real"),
            "{what}"
        );
        let singles = batch
            .column(0)
            .as_primitive::<arrow_array::types::Float32Type>();
        for row in 0..batch.num_rows() {
            if row == 7 {
                assert!(singles.is_null(row), "{what}");
                continue;
            }
            // The server's text reads back to exactly the value we hold.
            let server: f32 = text(&batch, 1, row).parse().expect("server real text");
            let held = singles.value(row);
            assert!(
                held.to_bits() == server.to_bits() || (held.is_nan() && server.is_nan()),
                "{what}: row {row}: {held:?} vs server {}",
                text(&batch, 1, row)
            );
        }
        assert_eq!(singles.value(0), 0.1f32, "{what}");
        assert_eq!(singles.value(8), 16_777_216.0f32, "{what}");
        // The kit's cell text is the single-precision text: not 0.10000000149011612.
        let rendered = h
            .session
            .render_cell_batch(page.page_id, 0, 9, 0, 1)
            .expect("render");
        let cell = |row: u64| {
            (0..rendered.cell_count())
                .filter_map(|i| rendered.cell(i))
                .find(|c| c.row == row && c.column == 0)
                .map(|c| c.text)
        };
        assert_eq!(cell(0).as_deref(), Some("0.1"), "{what}");
        assert_eq!(cell(1).as_deref(), Some("3.14"), "{what}");
        assert_eq!(cell(7).as_deref(), Some("NULL"), "{what}");
        drop(rendered);
        h.session.release_page(page.page_id).unwrap();
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn types_without_a_binary_decoder_read_as_psql_prints_them_in_every_path() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("SET TimeZone = 'Asia/Kolkata'");
    h.exec("CREATE TYPE {s}.mood AS ENUM ('sad', 'happy')");
    h.exec("CREATE TYPE {s}.pair AS (a int, b text)");
    h.exec("CREATE DOMAIN {s}.span AS interval");
    h.exec(
        "CREATE TABLE {s}.x (id int PRIMARY KEY, iv interval, arr int[], ip inet, mn money,
           tz timetz, r int4range, c {s}.pair, m {s}.mood, ts timestamptz, sp {s}.span, rc regclass,
           bits bit(5), tsv tsvector, pt point, ia text[])",
    );
    let columns = [
        "iv", "arr", "ip", "mn", "tz", "r", "c", "m", "ts", "sp", "rc", "bits", "tsv", "pt", "ia",
    ];
    let list = pair_list(&columns);
    let row = |id: i32| {
        format!(
            "({id}, '1 year 2 mons 3 days 04:05:06.7', '{{1,NULL,3}}', '10.0.0.1/24', '12.50',
              '12:34:56.5+05:30', '[1,5)', '(1,\"x y\")', 'happy', '2024-01-02 03:04:05.25+00',
              '2 days', 'pg_class', B'10101', 'a:1 b:2', '(1,2)', '{{\"a b\",NULL,\"c,d\"}}')"
        )
    };
    h.exec(&format!("INSERT INTO {{s}}.x VALUES {}", row(1)));
    h.exec(
        "INSERT INTO {s}.x (id) VALUES (2)", // a row of NULLs
    );

    // Paged select (binary decoders for the rest, the server's text for these).
    let (_, batch) = h.query(
        &format!("SELECT id, {list} FROM {{s}}.x ORDER BY id"),
        0,
        100,
    );
    assert_eq!(batch.schema().field(0).data_type(), &DataType::Int64);
    assert_pairs_equal(&batch, 1, "paged select");
    assert_eq!(cell_string(&batch, 1, 0), "1 year 2 mons 3 days 04:05:06.7");
    assert_eq!(cell_string(&batch, 3, 0), "{1,NULL,3}");
    assert_eq!(cell_string(&batch, 5, 0), "10.0.0.1/24");
    assert_eq!(cell_string(&batch, 9, 0), "12:34:56.5+05:30");
    assert_eq!(cell_string(&batch, 11, 0), "[1,5)");
    assert_eq!(cell_string(&batch, 15, 0), "happy");
    assert_eq!(cell_string(&batch, 17, 0), "2024-01-02 08:34:05.25+05:30");
    assert_eq!(cell_string(&batch, 1, 1), "NULL");

    // A CTE with a data-modifying statement cannot be wrapped: it used to show
    // `<value>` for every one of these.
    let (_, batch) = h.query(
        &format!(
            "WITH ins AS (INSERT INTO {{s}}.x VALUES {} RETURNING id, {list}) SELECT * FROM ins",
            row(100)
        ),
        0,
        100,
    );
    assert_pairs_equal(&batch, 1, "CTE with INSERT");
    assert_eq!(cell_string(&batch, 0, 0), "100");
    let (_, batch) = h.query(
        &format!("WITH u AS (UPDATE {{s}}.x SET iv = iv WHERE id < 100 RETURNING id, {list}) SELECT * FROM u"),
        0,
        100,
    );
    assert_pairs_equal(&batch, 1, "CTE with UPDATE");
    assert_eq!(batch.num_rows(), 2);
    // INSERT ... RETURNING is no longer rewritten into a WITH: it runs once.
    let (_, batch) = h.query(
        &format!(
            "INSERT INTO {{s}}.x VALUES {} RETURNING id, {list}",
            row(101)
        ),
        0,
        100,
    );
    assert_pairs_equal(&batch, 1, "INSERT RETURNING");
    // The statement ran exactly once each.
    assert_eq!(h.scalar("SELECT count(*)::text FROM {s}.x"), "4");

    // EXECUTE of a prepared statement.
    h.exec(&format!(
        "PREPARE px AS SELECT id, {list} FROM {{s}}.x ORDER BY id"
    ));
    let (_, batch) = h.query("EXECUTE px", 0, 100);
    assert_pairs_equal(&batch, 1, "EXECUTE");
    assert_eq!(batch.num_rows(), 4);
    h.exec("DEALLOCATE px");

    // FETCH from a cursor (inside a transaction), a read that is not wrappable.
    h.exec("BEGIN");
    h.exec(&format!(
        "DECLARE cx CURSOR FOR SELECT id, {list} FROM {{s}}.x ORDER BY id"
    ));
    let (_, batch) = h.query("FETCH ALL FROM cx", 0, 100);
    h.exec("COMMIT");
    assert_pairs_equal(&batch, 1, "FETCH");
    assert_eq!(batch.num_rows(), 4);

    // A script's last result set (simple protocol end to end).
    let (_, batch) = h.query(
        &format!("SELECT 1; SELECT id, {list} FROM {{s}}.x ORDER BY id"),
        0,
        100,
    );
    assert_pairs_equal(&batch, 1, "script");
    assert_eq!(
        batch.schema().field(0).data_type(),
        &DataType::Int64,
        "typed after the script"
    );
    assert_eq!(cell_string(&batch, 17, 0), "2024-01-02 08:34:05.25+05:30");

    // A host address reads the same in every path: no `/32` (inet's output form).
    let hosts =
        "'10.0.0.1'::inet AS a, '::1'::inet AS b, '10.0.0.1/24'::inet AS c, NULL::inet AS d";
    let (_, paged) = h.query(&format!("SELECT {hosts}"), 0, 10);
    let (_, script) = h.query(&format!("SELECT 1; SELECT {hosts}"), 0, 10);
    for batch in [&paged, &script] {
        let cells: Vec<String> = (0..4).map(|c| cell_string(batch, c, 0)).collect();
        assert_eq!(cells, ["10.0.0.1", "::1", "10.0.0.1/24", "NULL"]);
    }

    // Pseudo-types do not fail the query.
    let (_, batch) = h.query(
        "SELECT pg_sleep(0) AS v, ROW(1, 'a') AS r, 'unknown'::unknown AS u",
        0,
        10,
    );
    assert_eq!(
        (
            cell_string(&batch, 0, 0),
            cell_string(&batch, 1, 0),
            cell_string(&batch, 2, 0)
        ),
        (String::new(), "(1,a)".to_owned(), "unknown".to_owned())
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn temporal_text_is_the_servers_in_every_path_and_time_zone() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.tt (id serial PRIMARY KEY, d date, tm time, ts timestamp, tz timestamptz, ttz timetz)");
    let cases: [(&str, &[&str]); 5] = [
        (
            "d",
            &[
                "2024-01-02",
                "0044-03-15 BC",
                "4713-01-01 BC",
                "5874897-12-31",
                "infinity",
                "-infinity",
                "0001-01-01",
                "0001-12-31 BC",
            ],
        ),
        (
            "tm",
            &[
                "00:00:00",
                "12:34:56.25",
                "24:00:00",
                "23:59:59.999999",
                "01:02:03.1",
                "00:00:00.000001",
                "10:00:00.050",
            ],
        ),
        (
            "ts",
            &[
                "2024-01-02 03:04:05.25",
                "0044-03-15 12:00:00.25 BC",
                "10000-01-01 00:00:00.5",
                "infinity",
                "-infinity",
                "1999-12-31 23:59:59.999999",
                "2000-01-01 00:00:00.000001",
                "4713-01-01 00:00:00 BC",
                "2024-01-02 03:04:05.100",
            ],
        ),
        (
            "tz",
            &[
                "2024-01-02 03:04:05.25+00",
                "0044-03-15 12:00:00+00 BC",
                "2024-11-03 05:30:00+00",
                "2024-11-03 06:30:00+00",
                "2024-03-10 07:30:00+00",
                "infinity",
                "-infinity",
                "10000-01-01 00:00:00.5+00",
                "2000-01-01 00:00:00.000001+00",
            ],
        ),
        (
            "ttz",
            &[
                "12:34:56.5+05:30",
                "00:00:00+00",
                "23:59:59.999999-03",
                "12:00:00+13:45",
            ],
        ),
    ];
    for (column, literals) in cases {
        for literal in literals {
            h.exec(&format!(
                "INSERT INTO {{s}}.tt ({column}) VALUES ('{literal}')"
            ));
        }
    }
    let list = pair_list(&["d", "tm", "ts", "tz", "ttz"]);
    for zone in [
        "UTC",
        "Asia/Kolkata",
        "America/New_York",
        "Europe/Amsterdam",
        "Pacific/Chatham",
    ] {
        h.exec(&format!("SET TimeZone = '{zone}'"));
        let (_, paged) = h.query(&format!("SELECT {list} FROM {{s}}.tt ORDER BY id"), 0, 100);
        assert_pairs_equal(&paged, 0, &format!("paged / {zone}"));
        let (_, script) = h.query(
            &format!("SELECT 1; SELECT {list} FROM {{s}}.tt ORDER BY id"),
            0,
            100,
        );
        assert_pairs_equal(&script, 0, &format!("script / {zone}"));
        let (_, cte) = h.query(
            &format!("WITH u AS (UPDATE {{s}}.tt SET id = id RETURNING {list}) SELECT * FROM u"),
            0,
            100,
        );
        assert_pairs_equal(&cte, 0, &format!("CTE / {zone}"));
        // The paged and script pages show the same cells row for row.
        for column in 0..paged.num_columns() {
            for row in 0..paged.num_rows() {
                assert_eq!(
                    cell_string(&paged, column, row),
                    cell_string(&script, column, row),
                    "{zone}"
                );
            }
        }
        if zone == "Asia/Kolkata" {
            let find = |name: &str, shown: &str| {
                let at = paged.schema().index_of(name).unwrap();
                (0..paged.num_rows()).any(|row| cell_string(&paged, at, row) == shown)
            };
            assert!(find("tz", "2024-01-02 08:34:05.25+05:30"));
            assert!(find("ts", "0044-03-15 12:00:00.25 BC"));
            assert!(find("ts", "10000-01-01 00:00:00.5"));
            assert!(find("d", "0044-03-15 BC"));
            assert!(find("tm", "24:00:00"));
            assert!(find("tz", "infinity"));
        }
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn an_edited_timestamptz_keeps_its_instant_in_any_session_time_zone() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let identity = json!({ "schema": h.schema });
    h.exec("CREATE TABLE {s}.e (at timestamptz PRIMARY KEY, copy timestamptz)");
    h.exec(
        "INSERT INTO {s}.e (at) VALUES ('2024-03-10 01:30:00.123456+00'), ('2024-11-03 05:30:00+00'),
           ('2024-11-03 06:30:00+00'), ('0044-03-15 12:00:00+00 BC'), ('2000-01-01 00:00:00.000001+00')",
    );
    for zone in [
        "Asia/Kolkata",
        "America/New_York",
        "Europe/Amsterdam",
        "UTC",
    ] {
        h.exec(&format!("SET TimeZone = '{zone}'"));
        h.exec("UPDATE {s}.e SET copy = NULL");
        let (_, batch) = h.query("SELECT at, at::text AS t FROM {s}.e ORDER BY at", 0, 100);
        assert_eq!(batch.num_rows(), 5);
        for row in 0..5 {
            let shown = text(&batch, 0, row);
            assert_eq!(shown, text(&batch, 1, row), "{zone}");
            assert!(
                shown.contains('+') || shown.contains('-'),
                "{zone}: {shown} has a numeric offset"
            );
            // Edit the cell with the very text that was shown, keyed by that text.
            let result = h
                .mutate(
                    "update",
                    Some("e"),
                    identity.clone(),
                    json!({"op": "update", "key": {"at": shown}, "values": {"copy": shown}}),
                )
                .unwrap_or_else(|error| panic!("{zone} row {row} ({shown}): {error}"));
            assert_eq!(
                result.affected_rows, 1,
                "{zone}: the timestamptz key matched one row"
            );
        }
        assert_eq!(
            h.scalar("SELECT (count(*) = 5 AND bool_and(at = copy))::text FROM {s}.e"),
            "true",
            "{zone}: same instants"
        );
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn booleans_floats_and_integers_are_native_in_every_path() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let columns =
        "true AS b, false AS c, NULL::boolean AS n, 0.1::real AS r, 7::int AS i, 'x'::text AS t";
    let (_, paged) = h.query(&format!("SELECT {columns}"), 0, 10);
    let (_, script) = h.query(&format!("SELECT 1; SELECT {columns}"), 0, 10);
    h.exec("CREATE TABLE {s}.one (id int)");
    let (_, forced) = h.query(
        &format!("WITH x AS (INSERT INTO {{s}}.one VALUES (1) RETURNING id) SELECT {columns}, '1 day'::interval AS iv FROM x"),
        0,
        10,
    );
    for (what, batch) in [("paged", &paged), ("script", &script), ("simple", &forced)] {
        let kinds: Vec<DataType> = (0..6)
            .map(|c| batch.schema().field(c).data_type().clone())
            .collect();
        assert_eq!(
            kinds,
            [
                DataType::Boolean,
                DataType::Boolean,
                DataType::Boolean,
                DataType::Float32,
                DataType::Int64,
                DataType::Utf8
            ],
            "{what}"
        );
        let cells: Vec<String> = (0..6).map(|c| cell_string(batch, c, 0)).collect();
        assert_eq!(cells, ["true", "false", "NULL", "0.1", "7", "x"], "{what}");
    }
    // A later statement that is not a read (here a CREATE) leaves the script's
    // columns as the server's text: a boolean reads `t`. Documented limit.
    let name = format!("zz_{}", std::process::id());
    let (_, untyped) = h.query(
        &format!("SELECT true AS b; CREATE TEMP TABLE {name} (x int)"),
        0,
        10,
    );
    assert_eq!(untyped.schema().field(0).data_type(), &DataType::Utf8);
    assert_eq!(cell_string(&untyped, 0, 0), "t");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_decimal_sent_as_a_json_number_that_a_double_may_have_rounded_is_refused() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let identity = json!({ "schema": h.schema });
    h.exec("CREATE TABLE {s}.n (id int PRIMARY KEY, v numeric, f double precision, t text)");
    let insert = |h: &mut Harness, id: i32, values: Value| {
        h.mutate(
            "insert",
            Some("n"),
            identity.clone(),
            json!({"op": "insert", "values": values}),
        )
        .map(|_| id)
    };
    // 0.1 + 0.2 as a JSON number is already a double (0.30000000000000004).
    let error = insert(&mut h, 1, json!({"id": 1, "v": 0.1 + 0.2})).expect_err("refused");
    assert!(
        error.to_string().contains("mutation.invalid_value")
            || error.to_string().contains("JSON string"),
        "{error}"
    );
    assert_eq!(
        h.scalar("SELECT count(*)::text FROM {s}.n"),
        "0",
        "nothing was written"
    );
    // The same digits as a string are stored exactly.
    insert(&mut h, 2, json!({"id": 2, "v": "0.30000000000000004"})).expect("string");
    insert(
        &mut h,
        3,
        json!({"id": 3, "v": "123456789012345678901234567890.123456789"}),
    )
    .expect("big string");
    // Integers of any size, and doubles that read back from 15 digits, are exact.
    insert(&mut h, 4, json!({"id": 4, "v": 9_007_199_254_740_993_i64})).expect("integer");
    insert(&mut h, 5, json!({"id": 5, "v": 12.5})).expect("short double");
    // A double column takes a double.
    insert(&mut h, 6, json!({"id": 6, "f": 0.1 + 0.2})).expect("double column");
    let stored = |h: &mut Harness, id: i32| {
        h.scalar(&format!("SELECT v::text FROM {{s}}.n WHERE id = {id}"))
    };
    assert_eq!(stored(&mut h, 2), "0.30000000000000004");
    assert_eq!(
        stored(&mut h, 3),
        "123456789012345678901234567890.123456789"
    );
    assert_eq!(stored(&mut h, 4), "9007199254740993");
    assert_eq!(stored(&mut h, 5), "12.5");
    assert_eq!(
        h.scalar("SELECT f::text FROM {s}.n WHERE id = 6"),
        "0.30000000000000004"
    );
}

// ---------------------------------------------------------------------------
// text the server does not send as UTF-8, and settings the user changes
// ---------------------------------------------------------------------------

/// Runs `sql` on `database` (not the connection's default) and imports the page.
fn query_on(
    h: &mut Harness,
    database: &str,
    sql: &str,
) -> Result<(QueryPage, RecordBatch), LoaderError> {
    let operation = h.session.begin_operation().expect("operation");
    let page = h.session.open_query(
        &json!({
            "kind": "sql", "operation_id": operation.id(), "database": database, "object": null,
            "offset": 0, "limit": 100, "payload": { "sql": sql, "confirmed": true }
        })
        .to_string(),
    )?;
    let lease = h
        .session
        .acquire_arrow_page(page.page_id, 0, page.row_count)
        .expect("acquire arrow page");
    // SAFETY: the lease was just returned by the driver's Arrow export.
    let imported = unsafe { lease.into_record_batch() }.expect("import arrow page");
    h.session.release_page(page.page_id).expect("release");
    Ok((page, imported.batch))
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server, superuser)"]
fn text_that_is_not_utf8_never_panics_and_arrives_as_bytes() {
    let uri = pg_uri();
    // Declared first, so it drops last: after the harness closed its sessions.
    let _drop;
    let mut h = Harness::new(&uri);
    let name = format!("tablex_ascii_{}", std::process::id());
    _drop = DropDatabases {
        uri: uri.clone(),
        handle: Arc::clone(&h.handle),
        names: vec![name.clone()],
    };

    // Text arrives as non-UTF-8 when the session's client_encoding is not UTF-8
    // (the server converts, or for SQL_ASCII passes the bytes through). The
    // driver keeps UTF8 (a `set_config` inside the very statement is what gets
    // past that).
    let latin1 = "set_config('client_encoding', 'LATIN1', false) AS c, 'caf' || chr(233) AS note";
    // Binary path: the column is bytes exactly as sent, typed so, read-only.
    let (page, batch) = h.query(&format!("SELECT {latin1}"), 0, 10);
    assert_eq!(batch.schema().field(1).data_type(), &DataType::Binary);
    assert_eq!(page.columns[1].data_type, "binary");
    assert_eq!(
        batch
            .schema()
            .field(1)
            .metadata()
            .get("tablex.native_type")
            .map(String::as_str),
        Some("text (invalid utf-8)")
    );
    assert_eq!(batch.column(1).as_binary::<i32>().value(0), b"caf\xe9");
    // Only the column that held such a value is bytes.
    assert_eq!(batch.schema().field(0).data_type(), &DataType::Utf8);
    // The next statement is back on UTF-8: text is text again.
    let (_, after) = h.query(
        "SELECT 'caf' || chr(233) AS note, current_setting('client_encoding') AS e",
        0,
        10,
    );
    assert_eq!(
        (text(&after, 0, 0), text(&after, 1, 0)),
        ("caf\u{e9}".into(), "UTF8".into())
    );

    // The simple protocol hands over text only: no panic, a typed error that names
    // the column, and the connection and the user's transaction survive.
    h.exec("BEGIN");
    h.exec("CREATE TABLE {s}.kept (id int)");
    h.exec("INSERT INTO {s}.kept VALUES (1)");
    for sql in [
        format!("SELECT 1; SELECT {latin1}"),
        format!("WITH u AS (UPDATE {{s}}.kept SET id = id RETURNING id, '1 day'::interval AS iv, {latin1}) SELECT * FROM u"),
    ] {
        let error = h.sql(&sql.replace("{s}", &h.schema.clone())).expect_err(&sql);
        assert_eq!(rejected_code(&error), "query.invalid_utf8", "{sql}: {error}");
        assert!(error.to_string().contains("note"), "{error}");
        assert_eq!(h.scalar("SELECT current_setting('client_encoding')"), "UTF8");
    }
    assert_eq!(
        h.scalar("SELECT count(*)::text FROM {s}.kept"),
        "1",
        "the user's transaction is intact"
    );
    h.exec("COMMIT");

    // A SQL_ASCII database: the server itself refuses to send its non-UTF-8 text to
    // a UTF-8 client (an error, not a panic); with the session in SQL_ASCII the
    // bytes arrive untouched, as a binary column.
    h.exec(&format!(
        "CREATE DATABASE \"{name}\" ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0"
    ));
    let on = |h: &mut Harness, sql: &str| query_on(h, &name, sql);
    on(
        &mut h,
        "CREATE TABLE t (id int PRIMARY KEY, note text, tz timestamptz)",
    )
    .unwrap();
    on(
        &mut h,
        "INSERT INTO t VALUES (1, 'caf' || chr(233) || ' ' || chr(255), '2024-01-02 03:04:05+00'), (2, 'ascii', NULL)",
    )
    .unwrap();
    let error = on(&mut h, "SELECT id, note FROM t ORDER BY id").expect_err("server refuses");
    assert!(
        error.to_string().contains("invalid byte sequence"),
        "{error}"
    );
    let (_, fine) = on(&mut h, "SELECT note FROM t WHERE id = 2").unwrap();
    assert_eq!(text(&fine, 0, 0), "ascii", "the connection is usable");
    let raw = "SELECT set_config('client_encoding', 'SQL_ASCII', false) AS c, id, note, tz FROM t ORDER BY id";
    let (_, batch) = on(&mut h, raw).unwrap();
    let bytes = batch.column(2).as_binary::<i32>();
    assert_eq!(
        (bytes.value(0), bytes.value(1)),
        (&b"caf\xe9 \xff"[..], &b"ascii"[..])
    );
    // tz is printed by the server next to the bad text (the `::text` re-select).
    assert_eq!(cell_string(&batch, 3, 0), "2024-01-02 03:04:05+00");
    // Back on UTF-8 afterwards.
    let (_, e) = on(&mut h, "SELECT current_setting('client_encoding')").unwrap();
    assert_eq!(text(&e, 0, 0), "UTF8");
    // The navigator (metadata connection) lists the SQL_ASCII database's objects.
    let nav = h
        .session
        .list_nav_nodes(
            &json!({"parent": null, "database": name, "payload": {"database": name}, "cursor": null, "limit": 10}).to_string(),
        )
        .expect("navigator on a SQL_ASCII database");
    assert!(nav.nodes.iter().all(|node| node.kind == "schema"));
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn session_settings_the_user_changes_do_not_change_what_is_read_or_written() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    let identity = json!({ "schema": h.schema });
    // Every connection starts with the settings the driver parses text under.
    assert_eq!(h.scalar("SHOW extra_float_digits"), "3");
    assert!(h.scalar("SHOW DateStyle").starts_with("ISO"));
    assert_eq!(h.scalar("SHOW IntervalStyle"), "postgres");

    h.exec("CREATE TABLE {s}.v (id int PRIMARY KEY, f8 double precision, f4 real, d date, ts timestamp, iv interval, orig8 double precision)");
    h.exec(
        "INSERT INTO {s}.v VALUES
           (1, 0.1::float8 + 0.2::float8, 0.1, '2024-01-02', '2024-01-02 03:04:05.25', '1 year 2 mons 3 days 04:05:06.7', 0.1::float8 + 0.2::float8),
           (2, 1e300, '3.4028235e38', '2024-12-01', '0044-03-15 12:00:00 BC', '-1 days +02:03:04', 1e300),
           (3, 5e-324, '1e-45', 'infinity', 'infinity', '00:00:00', 5e-324),
           (4, 'NaN', 'NaN', '-infinity', '-infinity', '10 years', 'NaN')",
    );
    // The user changes everything the text depends on, in their own statements.
    h.exec("SET extra_float_digits = 0");
    h.exec("SET DateStyle = 'SQL, DMY'");
    h.exec("SET IntervalStyle = 'iso_8601'");

    let check = |batch: &RecordBatch, what: &str| {
        let f8 = batch
            .column(batch.schema().index_of("f8").unwrap())
            .as_primitive::<arrow_array::types::Float64Type>();
        let f4 = batch
            .column(batch.schema().index_of("f4").unwrap())
            .as_primitive::<arrow_array::types::Float32Type>();
        let ids = batch
            .column(batch.schema().index_of("id").unwrap())
            .as_primitive::<arrow_array::types::Int64Type>();
        let date = batch.schema().index_of("d").unwrap();
        let stamp = batch.schema().index_of("ts").unwrap();
        let interval = batch.schema().index_of("iv").unwrap();
        assert_eq!(batch.num_rows(), 4, "{what}");
        for row in 0..4 {
            let (f64_bits, f32_bits) = match ids.value(row) {
                1 => ((0.1f64 + 0.2f64).to_bits(), 0.1f32.to_bits()),
                2 => (1e300f64.to_bits(), 3.4028235e38f32.to_bits()),
                3 => (5e-324f64.to_bits(), 1e-45f32.to_bits()),
                _ => (f64::NAN.to_bits(), f32::NAN.to_bits()),
            };
            if ids.value(row) == 4 {
                assert!(f8.value(row).is_nan() && f4.value(row).is_nan(), "{what}");
            } else {
                assert_eq!(f8.value(row).to_bits(), f64_bits, "{what}: f8 row {row}");
                assert_eq!(f4.value(row).to_bits(), f32_bits, "{what}: f4 row {row}");
            }
        }
        let shown = |column: usize| {
            (0..4)
                .map(|row| cell_string(batch, column, row))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shown(date),
            ["2024-01-02", "2024-12-01", "infinity", "-infinity"],
            "{what}"
        );
        assert_eq!(
            shown(stamp),
            [
                "2024-01-02 03:04:05.25",
                "0044-03-15 12:00:00 BC",
                "infinity",
                "-infinity"
            ],
            "{what}"
        );
        assert_eq!(
            shown(interval),
            [
                "1 year 2 mons 3 days 04:05:06.7",
                "-1 days +02:03:04",
                "00:00:00",
                "10 years"
            ],
            "{what}"
        );
    };
    let columns = "id, f8, f4, d, ts, iv";
    // The binary path (the interval is re-selected as text).
    let (_, batch) = h.query(
        &format!("SELECT {columns} FROM {{s}}.v ORDER BY id"),
        0,
        100,
    );
    check(&batch, "paged select");
    // The settings were put back for that statement.
    assert_eq!(h.scalar("SHOW extra_float_digits"), "3");
    // A script (simple protocol), also one that changes them itself first.
    h.exec("SET extra_float_digits = 0");
    let (_, batch) = h.query(
        &format!("SELECT 1; SELECT {columns} FROM {{s}}.v ORDER BY id"),
        0,
        100,
    );
    check(&batch, "script");
    let (_, batch) = h.query(
        &format!("SET extra_float_digits = 0; SET DateStyle = 'SQL, DMY'; SET IntervalStyle = 'iso_8601'; SELECT {columns} FROM {{s}}.v ORDER BY id"),
        0,
        100,
    );
    check(&batch, "script that sets them");
    let (_, batch) = h.query(
        "SET extra_float_digits = 0; SELECT 0.1::float8 + 0.2::float8 AS f, DATE '2024-01-02' AS d",
        0,
        10,
    );
    assert_eq!(
        batch
            .column(0)
            .as_primitive::<arrow_array::types::Float64Type>()
            .value(0),
        0.1 + 0.2
    );
    assert_eq!(cell_string(&batch, 1, 0), "2024-01-02");
    // A statement only the simple protocol runs (a CTE with DML).
    h.exec("SET extra_float_digits = 0");
    h.exec("SET DateStyle = 'SQL, DMY'");
    let (_, batch) = h.query(
        &format!("WITH u AS (UPDATE {{s}}.v SET id = id RETURNING {columns}) SELECT * FROM u ORDER BY id"),
        0,
        100,
    );
    check(&batch, "CTE with UPDATE");
    // select_config can change them too.
    h.exec("SELECT set_config('extra_float_digits', '0', false)");
    let (_, batch) = h.query(
        &format!("SELECT {columns} FROM {{s}}.v ORDER BY id"),
        0,
        100,
    );
    check(&batch, "after set_config");

    // An edit round trip after those SETs: the text that was read, written back, and
    // used as a key, stores the same values.
    h.exec("SET extra_float_digits = 0");
    h.exec("SET DateStyle = 'SQL, DMY'");
    h.exec("UPDATE {s}.v SET f8 = NULL");
    let (_, batch) = h.query(
        "SELECT id, f8::text AS f8t, d::text AS dt, orig8 FROM {s}.v WHERE id <= 3 ORDER BY id",
        0,
        100,
    );
    for row in 0..3 {
        let id = batch
            .column(0)
            .as_primitive::<arrow_array::types::Int64Type>()
            .value(row);
        // The value the driver read (binary, exact) as text: Rust's shortest form.
        let exact = batch
            .column(3)
            .as_primitive::<arrow_array::types::Float64Type>()
            .value(row);
        h.mutate(
            "update",
            Some("v"),
            identity.clone(),
            json!({"op": "update", "key": {"id": id, "d": text(&batch, 2, row)}, "values": {"f8": exact.to_string()}}),
        )
        .unwrap_or_else(|error| panic!("edit row {id}: {error}"));
    }
    assert_eq!(
        h.scalar("SELECT (count(*) FILTER (WHERE id <= 3 AND f8 = orig8) = 3)::text FROM {s}.v"),
        "true"
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server, superuser)"]
fn catalog_readers_report_non_utf8_names_instead_of_panicking() {
    let uri = pg_uri();
    // Declared first, so it drops last: after the harness closed its sessions.
    let _drop;
    let mut h = Harness::new(&uri);
    let name = format!("tablex_ascii_cat_{}", std::process::id());
    _drop = DropDatabases {
        uri: uri.clone(),
        handle: Arc::clone(&h.handle),
        names: vec![name.clone()],
    };
    h.exec(&format!(
        "CREATE DATABASE \"{name}\" ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0"
    ));
    let on = |h: &mut Harness, sql: &str| query_on(h, &name, sql).map(drop);
    on(&mut h, "CREATE TABLE ok_t (id int PRIMARY KEY, v text)").unwrap();
    on(&mut h, "CREATE TABLE clean_t (id int PRIMARY KEY, v text)").unwrap();
    // A table whose name, and a column whose comment, are not UTF-8.
    on(
        &mut h,
        "DO $$ BEGIN EXECUTE 'CREATE TABLE ' || quote_ident('caf' || chr(233)) || ' (id int)'; END $$",
    )
    .unwrap();
    on(
        &mut h,
        "DO $$ BEGIN EXECUTE 'COMMENT ON COLUMN ok_t.v IS ' || quote_literal('x' || chr(233)); END $$",
    )
    .unwrap();

    // Databases and schemas list fine (their names are ASCII).
    let databases = h.nav(
        json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 500}),
    );
    assert!(names(&databases).contains(&name));
    let schemas = h.nav(json!({
        "parent": format!("database:{name}"), "database": name,
        "payload": {"database": name}, "cursor": null, "limit": 500
    }));
    let public = schemas
        .nodes
        .iter()
        .find(|n| n.label == "public")
        .expect("public");
    // The table list holds a name the server will not send as UTF-8: the request
    // fails with the server's message (no panic, no table silently left out) and
    // the metadata connection stays usable.
    let objects = json!({
        "parent": public.id, "database": name,
        "payload": {"database": name, "schema": "public"}, "cursor": null, "limit": 100
    });
    let error = h
        .session
        .list_nav_nodes(&objects.to_string())
        .expect_err("a non-UTF-8 table name cannot be listed");
    assert!(
        error.to_string().contains("invalid byte sequence"),
        "{error}"
    );
    assert!(!names(&h.nav(json!({
        "parent": format!("database:{name}"), "database": name,
        "payload": {"database": name}, "cursor": null, "limit": 500
    })))
    .is_empty());

    // Describe: a clean table works; one with a non-UTF-8 comment fails visibly.
    let described = h
        .session
        .describe_object(&json!({"database": name, "schema": "public", "object": "clean_t"}))
        .expect("clean table");
    assert_eq!(described["columns"].as_array().map(Vec::len), Some(2));
    let error = h
        .session
        .describe_object(&json!({"database": name, "schema": "public", "object": "ok_t"}))
        .expect_err("a non-UTF-8 comment cannot be read");
    assert!(
        error.to_string().contains("invalid byte sequence"),
        "{error}"
    );
    // The user's session is untouched and the metadata connection still answers.
    assert_eq!(h.scalar("SELECT 1::text"), "1");
    h.session
        .describe_object(&json!({"database": name, "schema": "public", "object": "clean_t"}))
        .expect("describe again");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn the_driver_settings_are_set_after_connecting_and_a_uris_own_options_survive() {
    let uri = pg_uri();
    // A URI that carries its own startup `options`: applied as written, and the
    // driver's settings still hold (they are not sent as options).
    let with_options = format!("{uri}?options=-c%20search_path%3Dpg_catalog");
    let mut h = Harness::new(&with_options);
    assert_eq!(h.scalar("SHOW search_path"), "pg_catalog");
    assert_eq!(h.scalar("SHOW extra_float_digits"), "3");
    assert!(h.scalar("SHOW DateStyle").starts_with("ISO"));
    assert_eq!(h.scalar("SHOW IntervalStyle"), "postgres");
    // A database other than the default gets its own client, set up the same way.
    let (_, other) = query_on(
        &mut h,
        "postgres",
        "SELECT current_setting('extra_float_digits') AS f, current_setting('IntervalStyle') AS i, current_database() AS d",
    )
    .expect("other database");
    assert_eq!(
        (text(&other, 0, 0), text(&other, 1, 0), text(&other, 2, 0)),
        ("3".into(), "postgres".into(), "postgres".into())
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_batch_of_inserts_goes_as_multi_row_statements_and_stays_all_or_nothing() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.rows (id int PRIMARY KEY, name text, price numeric(12,2), at timestamptz, meta jsonb, flag boolean, note text)");
    let schema = h.schema.clone();
    let identity = serde_json::to_value(SqlTableIdentity { schema: Some(schema.clone()) }).unwrap();
    let count = |h: &mut Harness| h.scalar(&format!("SELECT count(*)::text FROM {schema}.rows"));
    let insert = |id: i64| {
        json!({"op": "insert", "values": {
            "id": id, "name": format!("it's #{id}; DROP TABLE x; --"), "price": format!("{id}.50"),
            "at": "2024-01-02 03:04:05+00", "meta": {"k": [id]}, "flag": id % 2 == 0, "note": null
        }})
    };

    // 2500 rows: three statements (1000 + 1000 + 500), every value typed by its column.
    let changes: Vec<Value> = (1..=2500).map(insert).collect();
    let result = h.mutate("batch", Some("rows"), identity.clone(), json!({ "changes": changes })).expect("a batch of inserts");
    assert_eq!(result.affected_rows, 2500);
    assert_eq!(count(&mut h), "2500");
    assert_eq!(h.scalar(&format!("SELECT name FROM {schema}.rows WHERE id = 1234")), "it's #1234; DROP TABLE x; --");
    assert_eq!(h.scalar(&format!("SELECT price::text FROM {schema}.rows WHERE id = 2500")), "2500.50");
    assert_eq!(h.scalar(&format!("SELECT meta::text FROM {schema}.rows WHERE id = 7")), "{\"k\": [7]}");
    assert_eq!(h.scalar(&format!("SELECT count(*)::text FROM {schema}.rows WHERE flag AND note IS NULL")), "1250");

    // A duplicate in the middle of the second chunk rolls every chunk back.
    let mut changes: Vec<Value> = (3000..=4500).map(insert).collect();
    changes[1200] = insert(7);
    let error = h.mutate("batch", Some("rows"), identity.clone(), json!({ "changes": changes })).expect_err("a duplicate key");
    assert_eq!(rejected_code(&error), "mutation.failed", "{error}");
    assert_eq!(count(&mut h), "2500", "nothing of the batch stays");

    // Mixed with an update and a delete, the order is kept and the batch is still one transaction.
    let mixed = json!({"changes": [
        insert(5000), insert(5001),
        {"op": "update", "key": {"id": 5000}, "values": {"name": "renamed"}},
        insert(5002),
        {"op": "delete", "key": {"id": 1}},
    ]});
    let result = h.mutate("batch", Some("rows"), identity.clone(), mixed).expect("a mixed batch");
    assert_eq!(result.affected_rows, 5);
    assert_eq!(count(&mut h), "2502");
    assert_eq!(h.scalar(&format!("SELECT name FROM {schema}.rows WHERE id = 5000")), "renamed");

    // A JSON number that a double cannot hold is refused for an exact decimal column, in a joined statement too.
    // Both rows write the same columns, so they go as one joined statement and the check runs on it.
    let mut inexact = insert(6001);
    inexact["values"]["price"] = json!(0.30000000000000004);
    let rounded = json!({"changes": [insert(6000), inexact]});
    let error = h.mutate("batch", Some("rows"), identity, rounded).expect_err("inexact decimal");
    assert_eq!(rejected_code(&error), "mutation.invalid_value", "{error}");
    assert!(error.to_string().contains("column \"price\" of row 2"), "the message names the column and the row: {error}");
    assert_eq!(count(&mut h), "2502");
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_joined_insert_that_a_rule_or_a_trigger_partly_swallows_is_refused_and_rolled_back() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.ruled (id int PRIMARY KEY)");
    h.exec("CREATE RULE skip_negative AS ON INSERT TO {s}.ruled WHERE NEW.id < 0 DO INSTEAD NOTHING");
    h.exec("CREATE TABLE {s}.triggered (id int PRIMARY KEY)");
    h.exec("CREATE FUNCTION {s}.skip_negative() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id < 0 THEN RETURN NULL; END IF; RETURN NEW; END $$");
    h.exec("CREATE TRIGGER skip BEFORE INSERT ON {s}.triggered FOR EACH ROW EXECUTE FUNCTION {s}.skip_negative()");
    let schema = h.schema.clone();
    let identity = serde_json::to_value(SqlTableIdentity { schema: Some(schema.clone()) }).unwrap();
    let insert = |id: i64| json!({"op": "insert", "values": {"id": id}});
    for table in ["ruled", "triggered"] {
        let count = |h: &mut Harness| h.scalar(&format!("SELECT count(*)::text FROM {schema}.{table}"));
        // The server reports one row for a statement of two: that must not pass as a success.
        for ids in [vec![1, -1], vec![1, 2, -3], vec![-1, -2, 3]] {
            let changes: Vec<Value> = ids.iter().map(|id| insert(*id)).collect();
            let error = h.mutate("batch", Some(table), identity.clone(), json!({ "changes": changes })).expect_err("a row was swallowed");
            assert_eq!(rejected_code(&error), "mutation.no_rows_affected", "{table} {ids:?}: {error}");
            assert!(error.to_string().contains(&format!("of {} rows", ids.len())), "{error}");
            assert_eq!(count(&mut h), "0", "{table} {ids:?}: the whole batch is rolled back");
        }
        // Rows nothing swallows go in, a single swallowed row stays the error it was.
        let result = h.mutate("batch", Some(table), identity.clone(), json!({ "changes": [insert(1), insert(2)] })).expect("two good rows");
        assert_eq!(result.affected_rows, 2);
        let error = h.mutate("insert", Some(table), identity.clone(), insert(-5)).expect_err("a single swallowed row");
        assert_eq!(rejected_code(&error), "mutation.no_rows_affected");
        assert_eq!(count(&mut h), "2");
    }
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
fn a_cancel_inside_a_joined_insert_rolls_the_whole_batch_back_and_the_session_stays_usable() {
    let uri = pg_uri();
    let mut h = Harness::new(&uri);
    h.exec("CREATE TABLE {s}.slow (id int PRIMARY KEY)");
    h.exec("CREATE FUNCTION {s}.nap() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.004); RETURN NEW; END $$");
    h.exec("CREATE TRIGGER nap BEFORE INSERT ON {s}.slow FOR EACH ROW EXECUTE FUNCTION {s}.nap()");
    let schema = h.schema.clone();
    let identity = serde_json::to_value(SqlTableIdentity { schema: Some(schema.clone()) }).unwrap();
    // 1000 rows are one joined statement of about four seconds.
    let changes: Vec<Value> = (1..=1000).map(|id| json!({"op": "insert", "values": {"id": id}})).collect();
    for delay in [300, 900] {
        let operation = h.session.begin_operation().unwrap();
        let id = operation.id();
        let connection_id = h.session.connection_id();
        let started = Instant::now();
        let result = std::thread::scope(|scope| {
            let session = &mut h.session;
            let (identity, changes) = (identity.clone(), changes.clone());
            let worker = scope.spawn(move || {
                session.mutate(&MutationRequest {
                    operation_id: id,
                    kind: "batch".into(),
                    database: None,
                    object: Some("slow".into()),
                    identity,
                    payload: json!({ "changes": changes }),
                })
            });
            std::thread::sleep(Duration::from_millis(delay));
            h.handle.cancel_operation(connection_id, &CancelRequest { operation_id: id }).expect("cancel");
            worker.join().unwrap()
        });
        let error = result.expect_err("cancelled inside the joined statement");
        assert_eq!(rejected_code(&error), "mutation.cancelled", "{error}");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        drop(operation);
        assert_eq!(h.scalar(&format!("SELECT count(*)::text FROM {schema}.slow")), "0", "the whole batch is rolled back");
    }
    // No transaction is left open: a write from this session commits and is seen from another.
    h.exec("INSERT INTO {s}.slow(id) VALUES (5000)");
}

