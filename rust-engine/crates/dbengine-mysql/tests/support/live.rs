//! Live ABI v2 tests shared by `dbengine-mysql` and `dbengine-mariadb`.
//!
//! Each crate's `tests/live_v2.rs` defines `CFG` and includes this file as
//! `mod live`. The tests are `#[ignore]` and need a server:
//!
//! ```text
//! TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//! TABLEX_TEST_MARIADB_URI=mariadb://tablex:tablex@127.0.0.1:53307/tablex \
//!   cargo test -p dbengine-mysql -p dbengine-mariadb --test live_v2 -- --ignored --nocapture --test-threads=1
//! ```
//!
//! They build the release `--features abi-v2` dylib and drive it through the
//! real host loader (`engine_driver_host::DriverLoader`); `describe_object`
//! and the raw error envelopes (the host drops `retryable`) go through the
//! exported C symbols found with `dlsym`.
//!
//! Setup, verification and cleanup use a separate `mysql_async` connection, so
//! a driver bug cannot hide behind itself. Every table is unique per test and
//! dropped at the end; the privileged parts (triggers on MySQL, `CREATE
//! DATABASE`) use the root account of the dedicated container
//! (`TABLEX_TEST_*_ROOT_URI`, default: the same host with `root:tablex`).

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type, UInt64Type};
use arrow_array::{Array, RecordBatch};
use engine_driver_api::sql::{
    ConstraintKind, SqlBatch, SqlColumnNode, SqlObjectRef, SqlRowChange, SqlRowOp,
    SqlTableIdentity, TableProperties,
};
use engine_driver_api::{CancelRequest, Capability, MutationRequest, OperationId, QueryPage};
use engine_driver_host::{DriverHandle, DriverLoader, DriverSession, LoaderError, QueryRunner};
use mysql_async::prelude::Queryable;
use serde_json::{json, Value};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub struct Cfg {
    pub crate_dir: &'static str,
    pub dylib: &'static str,
    pub uri_env: &'static str,
    pub root_uri_env: &'static str,
    pub driver: &'static str,
    pub display_name: &'static str,
    pub scheme: &'static str,
    /// Grid type label of a `JSON` column in a result set. MariaDB aliases JSON
    /// to LONGTEXT and sends it as a text BLOB, so it cannot be told from text.
    pub json_label: &'static str,
}

use super::CFG;

// ---------------------------------------------------------------- environment

fn uri() -> String {
    std::env::var(CFG.uri_env).unwrap_or_else(|_| {
        panic!(
            "set {} (for example {}://tablex:tablex@127.0.0.1:53306/tablex)",
            CFG.uri_env, CFG.scheme
        )
    })
}

fn root_uri() -> String {
    std::env::var(CFG.root_uri_env).unwrap_or_else(|_| {
        let uri = uri();
        let (scheme, rest) = uri.split_once("://").expect("URI scheme");
        let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        format!("{scheme}://root:tablex@{host}")
    })
}

fn database_name() -> String {
    let uri = uri();
    let rest = uri.split_once("://").expect("URI scheme").1;
    rest.split_once('/')
        .map(|(_, path)| path.split('?').next().unwrap_or_default().to_owned())
        .filter(|name| !name.is_empty())
        .expect("the test URI names a database")
}

fn unique(tag: &str) -> String {
    format!("p3_{}_{}_{}", CFG.driver, tag, std::process::id())
}

// ----------------------------------------------------------------- driver load

struct Env {
    handle: Arc<DriverHandle>,
    raw: Raw,
    _staging: tempfile::TempDir,
}

fn build_dylib() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(CFG.crate_dir)
        .join("Cargo.toml");
    let target_dir = std::env::temp_dir().join("tablex-p3-live-target");
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "--features",
            "abi-v2",
            "--manifest-path",
        ])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .expect("run cargo build");
    assert!(
        output.status.success(),
        "driver build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    target_dir.join("release").join(CFG.dylib)
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let source = build_dylib();
        let staging = tempfile::TempDir::new().expect("staging dir");
        let staged = staging.path().join(CFG.dylib);
        std::fs::copy(&source, &staged).expect("stage driver");
        let handle = DriverLoader
            .load(&staged)
            .expect("load the driver through the host");
        let raw = Raw::open(&staged);
        Env {
            handle,
            raw,
            _staging: staging,
        }
    })
}

fn connect_with(uri: &str) -> Result<DriverSession, LoaderError> {
    env().handle.connect(&json!({ "uri": uri }).to_string())
}

fn connect() -> DriverSession {
    connect_with(&uri()).expect("connect")
}

fn connect_root() -> DriverSession {
    connect_with(&root_uri()).expect("connect as root")
}

/// The driver's exported C symbols (`dlsym` on the image the host loaded: the
/// same statics, the same connections).
struct Raw {
    lib: *mut c_void,
}

// SAFETY: the image handle is only used for `dlsym`; the driver's entry points
// are thread-safe by the ABI contract.
unsafe impl Send for Raw {}
unsafe impl Sync for Raw {}

type JsonFn = unsafe extern "C" fn(u64, *const c_char) -> *mut c_char;

impl Raw {
    fn open(path: &Path) -> Self {
        let path = CString::new(path.to_str().expect("utf-8 path")).unwrap();
        // SAFETY: valid NUL-terminated path.
        let lib = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(!lib.is_null(), "dlopen failed");
        Self { lib }
    }

    /// Calls an exported `(connection_id, request_json) -> envelope` symbol.
    fn call(&self, symbol: &str, connection_id: u64, request: &Value) -> Value {
        let name = CString::new(symbol).unwrap();
        // SAFETY: valid handle and NUL-terminated name.
        let address = unsafe { libc::dlsym(self.lib, name.as_ptr()) };
        assert!(!address.is_null(), "{symbol} is not exported");
        // SAFETY: every `(u64, *const c_char) -> *mut c_char` export has this ABI.
        let function: JsonFn = unsafe { std::mem::transmute(address) };
        let request = CString::new(request.to_string()).unwrap();
        // SAFETY: valid request string for the duration of the call.
        let response = unsafe { function(connection_id, request.as_ptr()) };
        assert!(!response.is_null(), "{symbol} returned null");
        // SAFETY: the driver returned a NUL-terminated string it owns.
        let text = unsafe { CStr::from_ptr(response) }
            .to_str()
            .unwrap()
            .to_owned();
        let free = CString::new("dbengine_free_string").unwrap();
        // SAFETY: `dbengine_free_string` is a required export.
        let free_address = unsafe { libc::dlsym(self.lib, free.as_ptr()) };
        // SAFETY: matches the exported signature; `response` is freed once.
        unsafe {
            let free_fn: unsafe extern "C" fn(*mut c_char) = std::mem::transmute(free_address);
            free_fn(response);
        }
        serde_json::from_str(&text).expect("envelope JSON")
    }
}

// --------------------------------------------------------------------- admin

fn admin_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("admin runtime")
}

fn admin_opts() -> mysql_async::Opts {
    let normalized = format!("mysql://{}", root_uri().split_once("://").unwrap().1);
    mysql_async::Opts::from_url(&normalized).expect("root URI")
}

/// Runs statements as root on a connection of its own.
fn admin_exec(statements: &[&str]) {
    admin_runtime().block_on(async {
        let mut conn = mysql_async::Conn::new(admin_opts())
            .await
            .expect("admin connect");
        for statement in statements {
            conn.query_drop(*statement)
                .await
                .unwrap_or_else(|error| panic!("admin `{statement}`: {error}"));
        }
        let _ = conn.disconnect().await;
    });
}

/// Rows of text (NULL is `None`) through a connection of its own.
fn admin_rows(sql: &str) -> Vec<Vec<Option<String>>> {
    admin_runtime().block_on(async {
        let mut conn = mysql_async::Conn::new(admin_opts())
            .await
            .expect("admin connect");
        let rows: Vec<mysql_async::Row> = conn
            .query(sql)
            .await
            .unwrap_or_else(|error| panic!("admin `{sql}`: {error}"));
        let _ = conn.disconnect().await;
        rows.into_iter()
            .map(|row| {
                (0..row.len())
                    .map(|index| row.get::<Option<String>, _>(index).flatten())
                    .collect()
            })
            .collect()
    })
}

/// Drops the listed tables (and views) when the test ends, even on panic.
struct Cleanup(Vec<String>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let database = database_name();
        for table in &self.0 {
            let _ = std::panic::catch_unwind(|| {
                admin_exec(&[
                    &format!("DROP VIEW IF EXISTS `{database}`.`{table}`"),
                    &format!("DROP TABLE IF EXISTS `{database}`.`{table}`"),
                ]);
            });
        }
    }
}

// -------------------------------------------------------------------- queries

fn query_request(
    operation: OperationId,
    sql: &str,
    database: Option<&str>,
    offset: u64,
    limit: u64,
) -> String {
    json!({
        "kind": "sql",
        "operation_id": operation.0,
        "database": database,
        "object": null,
        "offset": offset,
        "limit": limit,
        "payload": { "sql": sql },
    })
    .to_string()
}

struct Fetched {
    page: QueryPage,
    batch: RecordBatch,
}

fn run(
    session: &mut DriverSession,
    sql: &str,
    offset: u64,
    limit: u64,
) -> Result<QueryPage, LoaderError> {
    run_with(session, sql, offset, limit, false)
}

/// `run`, with `payload.confirmed` set as given.
fn run_with(
    session: &mut DriverSession,
    sql: &str,
    offset: u64,
    limit: u64,
    confirmed: bool,
) -> Result<QueryPage, LoaderError> {
    let operation = session.begin_operation()?;
    let mut request: Value = serde_json::from_str(&query_request(
        operation.id(),
        sql,
        Some(&database_name()),
        offset,
        limit,
    ))
    .expect("request JSON");
    if confirmed {
        request["payload"]["confirmed"] = json!(true);
    }
    session.open_query(&request.to_string())
}

/// Runs `sql`, imports the page as Arrow through the host lease, releases it.
fn fetch(session: &mut DriverSession, sql: &str, offset: u64, limit: u64) -> Fetched {
    let page = run(session, sql, offset, limit).unwrap_or_else(|error| panic!("`{sql}`: {error}"));
    let lease = session
        .acquire_arrow_page(page.page_id, 0, page.row_count)
        .expect("arrow lease");
    // SAFETY: the lease comes from the driver's `dbengine_acquire_arrow_page`.
    let imported = unsafe { lease.into_record_batch() }.expect("import Arrow page");
    let batch = imported.batch.clone();
    drop(imported);
    session.release_page(page.page_id).expect("release page");
    Fetched { page, batch }
}

fn ids(batch: &RecordBatch, column: usize) -> Vec<i64> {
    batch
        .column(column)
        .as_primitive::<Int64Type>()
        .values()
        .to_vec()
}

fn thread_id(session: &mut DriverSession) -> u64 {
    let fetched = fetch(session, "SELECT CAST(CONNECTION_ID() AS CHAR)", 0, 1);
    fetched
        .batch
        .column(0)
        .as_string::<i32>()
        .value(0)
        .parse()
        .expect("thread id")
}

fn rejected(error: LoaderError) -> (String, String) {
    match error {
        LoaderError::DriverRejected { code, message, .. } => (code, message),
        other => panic!("expected a driver rejection, got {other}"),
    }
}

fn nav(
    session: &mut DriverSession,
    payload: Option<Value>,
    parent: Option<&str>,
    cursor: Option<&str>,
    limit: u64,
) -> engine_driver_api::NavigatorPage {
    let database = payload
        .as_ref()
        .and_then(|payload| payload.get("database"))
        .cloned();
    session
        .list_nav_nodes(
            &json!({
                "parent": parent,
                "database": database,
                "payload": payload,
                "cursor": cursor,
                "limit": limit,
            })
            .to_string(),
        )
        .expect("navigator")
}

fn mutation(
    kind: &str,
    database: Option<&str>,
    object: Option<&str>,
    identity: Value,
    payload: Value,
) -> impl FnOnce(OperationId) -> MutationRequest {
    let (kind, database, object) = (
        kind.to_owned(),
        database.map(str::to_owned),
        object.map(str::to_owned),
    );
    move |operation_id| MutationRequest {
        operation_id,
        kind,
        database,
        object,
        identity,
        payload,
    }
}

fn mutate(
    session: &mut DriverSession,
    build: impl FnOnce(OperationId) -> MutationRequest,
) -> Result<engine_driver_api::MutationResult, LoaderError> {
    let operation = session.begin_operation().expect("operation");
    session.mutate(&build(operation.id()))
}

fn object_of(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => panic!("expected a JSON object, got {other}"),
    }
}

/// A shared-contract row change; `key` and `values` are JSON objects (or null).
fn change(op: SqlRowOp, key: Value, values: Value) -> SqlRowChange {
    SqlRowChange {
        op,
        key: object_of(key),
        values: object_of(values),
    }
}

/// A single-op mutation: kind = the change's op, identity = `SqlTableIdentity`,
/// payload = the `SqlRowChange`.
fn apply(
    session: &mut DriverSession,
    table: &str,
    change: SqlRowChange,
) -> Result<engine_driver_api::MutationResult, LoaderError> {
    let database = database_name();
    mutate(
        session,
        mutation(
            change.op.kind(),
            Some(&database),
            Some(table),
            serde_json::to_value(SqlTableIdentity::default()).unwrap(),
            serde_json::to_value(&change).unwrap(),
        ),
    )
}

fn apply_batch(
    session: &mut DriverSession,
    table: &str,
    changes: Vec<SqlRowChange>,
) -> Result<engine_driver_api::MutationResult, LoaderError> {
    let database = database_name();
    mutate(
        session,
        mutation(
            "batch",
            Some(&database),
            Some(table),
            serde_json::to_value(SqlTableIdentity::default()).unwrap(),
            serde_json::to_value(SqlBatch { changes }).unwrap(),
        ),
    )
}

// ---------------------------------------------------------------------- tests

#[test]
#[ignore = "needs a live server (see module docs)"]
fn capabilities_connect_and_disconnect() {
    let capabilities = env().handle.capabilities();
    assert_eq!(capabilities.driver, CFG.driver);
    assert_eq!(capabilities.display_name, CFG.display_name);
    assert_eq!(capabilities.uri_schemes, [CFG.scheme]);
    assert_eq!(capabilities.abi_version, 2);
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
    assert_eq!(capabilities.editors, ["sql"]);
    let mut kinds = vec![
        "database", "table", "view", "function", "procedure", "trigger", "event", "column",
    ];
    if CFG.driver == "mariadb" {
        kinds.push("sequence");
    }
    assert_eq!(capabilities.navigator_kinds, kinds);
    let dialect = capabilities.sql_dialect.as_ref().expect("sql_dialect");
    assert_eq!(
        (
            dialect.identifier_quote.as_str(),
            dialect.string_literal_quote.as_str()
        ),
        ("`", "'")
    );
    assert!(!dialect.supports_schemas && dialect.database_qualified_names);
    println!(
        "capabilities: {}",
        serde_json::to_string(capabilities).unwrap()
    );

    // Connect, use, disconnect (twice), use after disconnect.
    let mut session = connect();
    let fetched = fetch(&mut session, "SELECT 1 AS one", 0, 10);
    assert_eq!(ids(&fetched.batch, 0), [1]);
    session.disconnect().expect("disconnect");
    session.disconnect().expect("second disconnect is a no-op");
    assert!(run(&mut session, "SELECT 1", 0, 10).is_err());

    // Failures: wrong password never echoes the secret; wrong scheme; garbage.
    let uri = uri();
    let (before, _) = uri.rsplit_once('@').expect("credentials in URI");
    let user = before
        .rsplit_once("://")
        .unwrap()
        .1
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    let host = uri.rsplit_once('@').unwrap().1;
    let bad = format!("{}://{user}:wrong-secret-pw@{host}", CFG.scheme);
    let error = match connect_with(&bad) {
        Ok(_) => panic!("a wrong password must be refused"),
        Err(error) => error,
    };
    let (code, message) = rejected(error);
    assert_eq!(code, "connect.failed");
    assert!(!message.contains("wrong-secret-pw"), "{message}");
    let (code, _) = rejected(match connect_with("postgres://u:p@h/db") {
        Ok(_) => panic!("a foreign scheme must be refused"),
        Err(error) => error,
    });
    assert_eq!(code, "connect.bad_request");
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn navigator_lists_databases_objects_and_columns_with_cursor_paging() {
    let database = database_name();
    let tables: Vec<String> = (1..=5).map(|n| unique(&format!("nav{n}"))).collect();
    let view = unique("navview");
    let _cleanup = Cleanup(tables.iter().cloned().chain([view.clone()]).collect());
    for table in &tables {
        admin_exec(&[&format!(
            "CREATE TABLE `{database}`.`{table}` (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) NULL, qty INT NOT NULL DEFAULT 0)"
        )]);
    }
    admin_exec(&[&format!(
        "CREATE VIEW `{database}`.`{view}` AS SELECT id FROM `{database}`.`{}`",
        tables[0]
    )]);
    let mut session = connect();

    // Databases: paged (one per page) and unpaged agree. The test account sees
    // at least `information_schema` and its own database.
    let all = nav(&mut session, None, None, None, 0);
    assert!(all.next_cursor.is_none());
    let names: Vec<_> = all.nodes.iter().map(|node| node.label.clone()).collect();
    assert!(names.contains(&database) && names.contains(&"information_schema".to_owned()));
    assert!(all
        .nodes
        .iter()
        .all(|node| node.kind == "database" && node.has_children));
    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = nav(&mut session, None, None, cursor.as_deref(), 1);
        assert_eq!(page.nodes.len(), 1);
        paged.extend(page.nodes.iter().map(|node| node.label.clone()));
        pages += 1;
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(pages >= 2, "limit 1 must produce several pages");
    assert_eq!(paged, names);

    // Tables and views of the database, three per page.
    let parent = &all
        .nodes
        .iter()
        .find(|node| node.label == database)
        .unwrap()
        .clone();
    let mut found = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = nav(
            &mut session,
            Some(parent.payload.clone()),
            Some(&parent.id),
            cursor.as_deref(),
            3,
        );
        found.extend(page.nodes.clone());
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    for table in &tables {
        let node = found
            .iter()
            .find(|node| &node.label == table)
            .expect("table listed");
        assert_eq!(node.kind, "table");
        assert!(node.has_children);
        // Table nodes carry exactly a SqlObjectRef.
        assert_eq!(
            node.payload,
            json!({ "database": database, "object": table })
        );
        let object: SqlObjectRef = serde_json::from_value(node.payload.clone()).unwrap();
        assert_eq!(object.object, *table);
        assert_eq!(object.database.as_deref(), Some(database.as_str()));
        assert_eq!(object.schema, None);
    }
    let view_node = found
        .iter()
        .find(|node| node.label == view)
        .expect("view listed");
    assert_eq!(view_node.kind, "view");
    let mut labels: Vec<_> = found.iter().map(|node| node.label.clone()).collect();
    let listed = labels.len();
    labels.sort();
    labels.dedup();
    assert_eq!(
        labels.len(),
        listed,
        "paging must not repeat or skip objects"
    );

    // Columns of one table, two per page; payload carries type / PK / nullability.
    let table_node = found.iter().find(|node| node.label == tables[0]).unwrap();
    let first = nav(
        &mut session,
        Some(table_node.payload.clone()),
        Some(&table_node.id),
        None,
        2,
    );
    assert_eq!(first.nodes.len(), 2);
    let cursor = first.next_cursor.clone().expect("a third column follows");
    let second = nav(
        &mut session,
        Some(table_node.payload.clone()),
        Some(&table_node.id),
        Some(&cursor),
        2,
    );
    assert_eq!(second.nodes.len(), 1);
    assert!(second.next_cursor.is_none());
    let columns: Vec<_> = first.nodes.iter().chain(&second.nodes).collect();
    assert_eq!(
        columns
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>(),
        ["id", "name", "qty"]
    );
    assert!(columns
        .iter()
        .all(|node| node.kind == "column" && !node.has_children));
    // Column nodes carry a SqlColumnNode that points back at the table.
    let typed: Vec<SqlColumnNode> = columns
        .iter()
        .map(|node| serde_json::from_value(node.payload.clone()).expect("SqlColumnNode"))
        .collect();
    for node in &typed {
        assert_eq!(node.object.object, tables[0]);
        assert_eq!(node.object.database.as_deref(), Some(database.as_str()));
        assert_eq!(node.object.schema, None);
        assert_eq!(node.column, node.details.name);
    }
    assert!(typed[0].details.primary_key && !typed[0].details.nullable);
    assert!(typed[1].details.nullable && !typed[1].details.primary_key);
    // Echoing the typed object payload back expands the table.
    let echoed = nav(
        &mut session,
        Some(serde_json::to_value(&typed[0].object).unwrap()),
        None,
        None,
        0,
    );
    assert_eq!(echoed.nodes.len(), 3);
    assert_eq!(columns[0].payload["primary_key"], true);
    assert_eq!(columns[0].payload["nullable"], false);
    assert_eq!(columns[1].payload["nullable"], true);
    assert!(columns[0].payload["data_type"]
        .as_str()
        .unwrap()
        .starts_with("int"));
    assert_eq!(columns[1].subtitle.as_deref(), Some("varchar(20)"));
    // The node id alone is enough to expand a table.
    let by_id = nav(&mut session, None, Some(&table_node.id), None, 0);
    assert_eq!(by_id.nodes.len(), 3);
    session.disconnect().unwrap();
}

/// `run`, with `payload.run_to_end` (capability `query.run_to_end`).
fn run_to_end(session: &mut DriverSession, sql: &str, limit: u64) -> Result<QueryPage, LoaderError> {
    let operation = session.begin_operation()?;
    let mut request: Value = serde_json::from_str(&query_request(operation.id(), sql, Some(&database_name()), 0, limit)).expect("request JSON");
    request["payload"]["run_to_end"] = json!(true);
    session.open_query(&request.to_string())
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn run_to_end_runs_a_select_for_every_row_keeps_the_page_and_is_never_truncated() {
    let database = database_name();
    let table = unique("toend");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT NOT NULL PRIMARY KEY)"),
        &format!("INSERT INTO `{database}`.`{table}` VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)"),
    ]);
    let mut session = connect();
    assert!(env().handle.capabilities().has(Capability::QueryRunToEnd), "the driver advertises query.run_to_end");
    run_to_end(&mut session, "SET @n = 0", 3).expect("SET");
    // A page of 3 rows, no LIMIT in the statement: the variable counts the rows the server produced.
    let page = run_to_end(&mut session, &format!("SELECT @n := @n + 1 AS n FROM `{table}`"), 3).expect("SELECT");
    assert_eq!(page.row_count, 3);
    assert!(!page.flags.truncated, "a statement run to its end has no next page");
    let counted = fetch(&mut session, "SELECT CAST(@n AS SIGNED) AS n", 0, 10);
    assert_eq!(counted.batch.column(0).as_primitive::<Int64Type>().value(0), 10, "the SELECT ran for every row");
    // The same statement paged as usual stops after its page (a LIMIT is appended): the other way round.
    run(&mut session, "SET @n = 0", 0, 3).expect("SET");
    let paged = fetch(&mut session, &format!("SELECT @n := @n + 1 AS n FROM `{table}`"), 0, 3);
    assert!(paged.page.flags.truncated);
    let seen = fetch(&mut session, "SELECT CAST(@n AS SIGNED) AS n", 0, 10);
    assert!(seen.batch.column(0).as_primitive::<Int64Type>().value(0) < 10);
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn query_pages_have_native_arrow_types_and_offset_limit() {
    let database = database_name();
    let table = unique("types");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (
           id INT NOT NULL PRIMARY KEY, name VARCHAR(40), amount DECIMAL(20,6), created DATETIME(3),
           flag TINYINT(1), small TINYINT, doc JSON, note TEXT NULL, big BIGINT UNSIGNED,
           ratio DOUBLE, bin BLOB, code CHAR(3), day DATE)"
    )]);
    let rows: Vec<String> = (1..=30)
        .map(|i| {
            let note = if i % 5 == 0 { "NULL".to_owned() } else { format!("'note {i}'") };
            format!(
                "({i}, 'name {i}', {i}.5, '2024-02-29 23:59:59.123', {}, -{i}, '{{\"i\": {i}}}', {note}, {}, {}.25, UNHEX('00FF'), 'ab', '2024-02-29')",
                i % 2,
                u64::MAX - i,
                i
            )
        })
        .collect();
    admin_exec(&[&format!(
        "INSERT INTO `{database}`.`{table}` VALUES {}",
        rows.join(",")
    )]);

    let mut session = connect();
    let select = format!("SELECT * FROM `{table}` ORDER BY id");
    let first = fetch(&mut session, &select, 0, 10);
    assert_eq!(first.page.row_count, 10);
    assert!(
        first.page.flags.truncated && first.page.flags.read_only && !first.page.flags.cancelled
    );
    assert_eq!(first.page.total_count, None);
    let labels: Vec<_> = first
        .page
        .columns
        .iter()
        .map(|column| column.data_type.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "int",
            "varchar",
            "decimal",
            "datetime",
            // TINYINT(1) is a number, whatever the page holds.
            "tinyint",
            "tinyint",
            CFG.json_label,
            "text",
            "bigint unsigned",
            "double",
            "blob",
            "char",
            "date"
        ]
    );
    assert_eq!(first.page.columns[0].name, "id");
    assert!(!first.page.columns[0].nullable && first.page.columns[1].nullable);
    let types: Vec<String> = first
        .batch
        .schema()
        .fields()
        .iter()
        .map(|field| field.data_type().to_string())
        .collect();
    assert_eq!(
        types,
        [
            "Int64", "Utf8", "Utf8", "Utf8", "Int64", "Int64", "Utf8", "Utf8", "UInt64", "Float64",
            "Binary", "Utf8", "Utf8"
        ],
        "DECIMAL and DATETIME stay Utf8 (no Arrow Decimal)"
    );
    let batch = &first.batch;
    assert_eq!(ids(batch, 0)[..3], [1, 2, 3]);
    assert_eq!(batch.column(1).as_string::<i32>().value(0), "name 1");
    assert_eq!(batch.column(2).as_string::<i32>().value(0), "1.500000");
    assert_eq!(
        batch.column(3).as_string::<i32>().value(0),
        "2024-02-29 23:59:59.123"
    );
    assert_eq!(batch.column(4).as_primitive::<Int64Type>().value(0), 1);
    assert_eq!(batch.column(4).as_primitive::<Int64Type>().value(1), 0);
    assert_eq!(batch.column(5).as_primitive::<Int64Type>().value(0), -1);
    assert_eq!(batch.column(6).as_string::<i32>().value(0), "{\"i\": 1}");
    assert!(batch.column(7).is_null(4), "NULL stays NULL (row id 5)");
    assert_eq!(batch.column(7).as_string::<i32>().value(0), "note 1");
    assert_eq!(
        batch.column(8).as_primitive::<UInt64Type>().value(0),
        u64::MAX - 1
    );
    assert_eq!(batch.column(9).as_primitive::<Float64Type>().value(0), 1.25);
    assert_eq!(batch.column(10).as_binary::<i32>().value(0), &[0x00, 0xff]);
    assert_eq!(batch.column(11).as_string::<i32>().value(0), "ab");
    assert_eq!(batch.column(12).as_string::<i32>().value(0), "2024-02-29");

    // Cell text as the grid renders it.
    let page = run(&mut session, &select, 0, 10).unwrap();
    let cells = session
        .render_cell_batch(page.page_id, 0, 5, 0, 13)
        .expect("render cells");
    let text = |row: u64, column: u64| cells.cell(row * 13 + column).unwrap().text;
    assert_eq!(text(0, 2), "1.500000");
    assert_eq!(text(0, 4), "1");
    assert_eq!(text(0, 8), (u64::MAX - 1).to_string());
    assert_eq!(text(0, 10), "X'00FF'");
    assert_eq!(text(4, 7), "NULL");
    drop(cells);
    session.release_page(page.page_id).unwrap();

    // Offset/limit, server-side: page boundaries and the lookahead flag.
    for (offset, limit, expect_rows, truncated, first_id) in [
        (10, 10, 10, true, 11),
        (20, 10, 10, false, 21),
        (25, 10, 5, false, 26),
        (0, 30, 30, false, 1),
        (0, 29, 29, true, 1),
    ] {
        let fetched = fetch(&mut session, &select, offset, limit);
        assert_eq!(
            fetched.page.row_count, expect_rows,
            "offset {offset} limit {limit}"
        );
        assert_eq!(
            fetched.page.flags.truncated, truncated,
            "offset {offset} limit {limit}"
        );
        assert_eq!(ids(&fetched.batch, 0)[0], first_id);
    }
    let empty = fetch(&mut session, &select, 100, 10);
    assert_eq!(empty.page.row_count, 0);
    assert_eq!(
        empty.page.columns.len(),
        13,
        "an empty page keeps its columns"
    );
    let unlimited = fetch(&mut session, &select, 0, 0);
    assert_eq!(unlimited.page.row_count, 30);

    // A statement with its own LIMIT is paged client-side.
    let limited = format!("SELECT id FROM `{table}` ORDER BY id LIMIT 12");
    let page = fetch(&mut session, &limited, 4, 5);
    assert_eq!(ids(&page.batch, 0), [5, 6, 7, 8, 9]);
    assert!(page.page.flags.truncated);
    let page = fetch(&mut session, &limited, 10, 5);
    assert_eq!(ids(&page.batch, 0), [11, 12]);
    assert!(!page.page.flags.truncated);
    // CTE, UNION and a trailing comment still page server-side.
    let cte = format!(
        "WITH c AS (SELECT id FROM `{table}`) SELECT id FROM c ORDER BY id -- trailing comment"
    );
    assert_eq!(ids(&fetch(&mut session, &cte, 28, 5).batch, 0), [29, 30]);
    let union = format!("SELECT id FROM `{table}` WHERE id <= 3 UNION ALL SELECT id FROM `{table}` WHERE id > 28 ORDER BY id");
    assert_eq!(ids(&fetch(&mut session, &union, 2, 2).batch, 0), [3, 29]);
    // SHOW pages client-side too.
    // (The test account sees at least two databases.)
    let show = fetch(&mut session, "SHOW DATABASES", 1, 1);
    assert_eq!(show.page.row_count, 1);
    let first_show = fetch(&mut session, "SHOW DATABASES", 0, 1);
    assert_eq!(first_show.page.row_count, 1);
    assert!(first_show.page.flags.truncated);
    assert_ne!(
        show.batch.column(0).as_string::<i32>().value(0),
        first_show.batch.column(0).as_string::<i32>().value(0)
    );

    // Default database tracking, including a `USE` inside the SQL.
    let operation = session.begin_operation().unwrap();
    let in_schema = |session: &mut DriverSession, database: &str, sql: &str, op: OperationId| {
        let page = session
            .open_query(&query_request(op, sql, Some(database), 0, 10))
            .unwrap();
        let lease = session
            .acquire_arrow_page(page.page_id, 0, page.row_count)
            .unwrap();
        // SAFETY: lease from the driver's Arrow export.
        let batch = unsafe { lease.into_record_batch() }.unwrap().batch;
        session.release_page(page.page_id).unwrap();
        batch.column(0).as_string::<i32>().value(0).to_owned()
    };
    assert_eq!(
        in_schema(
            &mut session,
            "information_schema",
            "SELECT DATABASE()",
            operation.id()
        ),
        "information_schema"
    );
    let operation = session.begin_operation().unwrap();
    assert_eq!(
        in_schema(&mut session, &database, "SELECT DATABASE()", operation.id()),
        database
    );
    let operation = session.begin_operation().unwrap();
    let _ = run(&mut session, "USE information_schema", 0, 10).unwrap();
    assert_eq!(
        in_schema(&mut session, &database, "SELECT DATABASE()", operation.id()),
        database
    );

    // Errors keep the server's wording and a plain (non-retryable) failure code.
    let (code, message) = rejected(run(&mut session, "SELEC 1", 0, 10).unwrap_err());
    assert_eq!(code, "query.failed");
    assert!(message.contains("1064"), "{message}");
    // The connection is still usable after an error.
    assert_eq!(ids(&fetch(&mut session, "SELECT 2", 0, 10).batch, 0), [2]);
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn unusual_result_columns_do_not_break_a_page() {
    let mut session = connect();
    let fetched = fetch(
        &mut session,
        "SELECT NULL AS n, 1.5 AS d, 1e0 AS f, b'101' AS bits, CAST('x' AS BINARY) AS bin, \
         ST_GeomFromText('POINT(1 2)') AS geo, TIME '10:00:00' AS t, 18446744073709551615 AS ubig, \
         -9223372036854775808 AS minbig, 'héllo wörld ✓' AS u, YEAR('2024-05-06') AS y",
        0,
        10,
    );
    let batch = &fetched.batch;
    assert_eq!(batch.num_rows(), 1);
    assert!(batch.column(0).is_null(0), "SELECT NULL");
    assert_eq!(batch.column(1).as_string::<i32>().value(0), "1.5");
    assert_eq!(batch.column(2).as_primitive::<Float64Type>().value(0), 1.0);
    // A bit literal in an expression is a binary string (not a BIT column);
    // GEOMETRY is WKT.
    assert_eq!(batch.column(3).as_binary::<i32>().value(0), &[0x05]);
    assert_eq!(batch.column(4).as_binary::<i32>().value(0), b"x");
    assert_eq!(batch.column(5).as_string::<i32>().value(0), "POINT (1 2)");
    assert_eq!(batch.column(6).as_string::<i32>().value(0), "10:00:00");
    assert_eq!(
        batch.column(7).as_primitive::<UInt64Type>().value(0),
        u64::MAX
    );
    assert_eq!(
        batch.column(8).as_primitive::<Int64Type>().value(0),
        i64::MIN
    );
    assert_eq!(batch.column(9).as_string::<i32>().value(0), "héllo wörld ✓");
    assert_eq!(batch.column(10).as_primitive::<Int64Type>().value(0), 2024);
    // The grid can render every one of them.
    let page = run(
        &mut session,
        "SELECT NULL, b'101', ST_GeomFromText('POINT(1 2)'), 18446744073709551615",
        0,
        10,
    )
    .unwrap();
    let cells = session.render_cell_batch(page.page_id, 0, 1, 0, 4).unwrap();
    assert_eq!(cells.cell(0).unwrap().text, "NULL");
    assert_eq!(cells.cell(1).unwrap().text, "X'05'");
    assert_eq!(cells.cell(2).unwrap().text, "POINT (1 2)");
    assert_eq!(cells.cell(3).unwrap().text, u64::MAX.to_string());
    drop(cells);
    session.release_page(page.page_id).unwrap();
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_large_page_is_built_in_one_pass() {
    let database = database_name();
    let table = unique("big");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (a INT, b VARCHAR(64), c VARCHAR(64), d VARCHAR(32), e DECIMAL(12,2))"),
        &format!(
            "INSERT INTO `{database}`.`{table}` \
             SELECT x.ORDINAL_POSITION, x.COLUMN_NAME, x.TABLE_NAME, x.DATA_TYPE, x.ORDINAL_POSITION / 3 \
             FROM information_schema.COLUMNS x CROSS JOIN (SELECT 1 FROM information_schema.COLUMNS LIMIT 200) y LIMIT 100000"
        ),
    ]);
    let mut session = connect();
    let sql = format!("SELECT * FROM `{table}`");
    // Warm the connection (statement parsing, buffers) once.
    let _ = fetch(&mut session, &sql, 0, 10);
    let started = Instant::now();
    let fetched = fetch(&mut session, &sql, 0, 100_000);
    let took = started.elapsed();
    println!(
        "large page: {} rows x {} columns in {took:?} ({:.0} rows/s) including server time and Arrow lease",
        fetched.page.row_count,
        fetched.batch.num_columns(),
        fetched.page.row_count as f64 / took.as_secs_f64()
    );
    assert_eq!(fetched.page.row_count, 100_000);
    assert!(!fetched.page.flags.truncated);
    assert!(took < Duration::from_secs(10), "{took:?}");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn ddl_and_dml_return_zero_column_pages_with_affected_rows() {
    let database = database_name();
    let table = unique("ddl");
    let _cleanup = Cleanup(vec![table.clone()]);
    let mut session = connect();
    let expect = |session: &mut DriverSession, sql: &str, affected: u64| {
        let page = run(session, sql, 0, 100).unwrap_or_else(|error| panic!("{sql}: {error}"));
        assert!(page.columns.is_empty(), "{sql}");
        assert_eq!(page.row_count, 0, "{sql}");
        assert_eq!(page.total_count, Some(affected), "{sql}");
        assert!(page.total_is_exact, "{sql}");
        session.release_page(page.page_id).unwrap();
    };
    expect(
        &mut session,
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, v INT)"),
        0,
    );
    expect(
        &mut session,
        &format!("INSERT INTO `{table}` VALUES (1, 10), (2, 20), (3, 30)"),
        3,
    );
    expect(
        &mut session,
        &format!("UPDATE `{table}` SET v = v + 1 WHERE id <= 2"),
        2,
    );
    expect(
        &mut session,
        &format!("DELETE FROM `{table}` WHERE id = 3"),
        1,
    );
    // Multi-statement scripts sum the affected rows and still return the first result set if any.
    expect(
        &mut session,
        &format!(
            "INSERT INTO `{table}` VALUES (4, 40); INSERT INTO `{table}` VALUES (5, 50), (6, 60)"
        ),
        3,
    );
    let mixed = fetch(
        &mut session,
        &format!("INSERT INTO `{table}` VALUES (7, 70); SELECT COUNT(*) FROM `{table}`"),
        0,
        10,
    );
    assert_eq!(mixed.page.row_count, 1);
    // DROP is dangerous: refused unless confirmed.
    let (code, reason) =
        rejected(run(&mut session, &format!("DROP TABLE `{table}`"), 0, 10).unwrap_err());
    assert_eq!(code, "query.confirmation_required");
    assert!(!reason.is_empty());
    let dropped = run_with(&mut session, &format!("DROP TABLE `{table}`"), 0, 10, true).unwrap();
    assert!(dropped.columns.is_empty());
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn cancel_stops_a_running_statement_and_the_connection_stays_usable() {
    let mut session = connect();
    let connection_id = session.connection_id();
    for sql in [
        "SELECT SLEEP(10)",
        // A statement that errors (not "returns 1") when killed.
        "SELECT COUNT(*) FROM information_schema.COLUMNS a, information_schema.COLUMNS b, information_schema.COLUMNS c",
    ] {
        let operation = session.begin_operation().unwrap();
        let runner: QueryRunner = session.query_runner().unwrap();
        let request = query_request(operation.id(), sql, Some(&database_name()), 0, 10);
        let started = Instant::now();
        let worker = std::thread::spawn(move || runner.open_query(&request));
        std::thread::sleep(Duration::from_millis(600));
        let cancelled_at = Instant::now();
        env()
            .handle
            .cancel_operation(connection_id, &CancelRequest { operation_id: operation.id() })
            .expect("cancel_operation");
        let result = worker.join().unwrap();
        let took = cancelled_at.elapsed();
        println!("cancel `{sql}`: returned {took:?} after the cancel, {:?} total", started.elapsed());
        assert!(took < Duration::from_secs(2), "cancel took {took:?}");
        let (code, message) = rejected(result.expect_err("a cancelled statement is an error"));
        assert_eq!(code, "query.cancelled");
        assert!(message.contains("cancelled"), "{message}");
        // Nothing of it survives on the connection.
        assert_eq!(ids(&fetch(&mut session, "SELECT 5", 0, 10).batch, 0), [5]);
        assert_eq!(fetch(&mut session, "SELECT CAST(SLEEP(0.2) AS SIGNED)", 0, 10).page.row_count, 1);
    }
    session.disconnect().unwrap();
}

/// What `KILL QUERY` does when it reaches an idle connection (the cancel
/// arriving after its statement finished): it must not hit the next statement.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_kill_query_between_statements_does_not_kill_the_next_statement() {
    let mut session = connect();
    let thread_id = thread_id(&mut session);
    for round in 0..25 {
        admin_exec(&[&format!("KILL QUERY {thread_id}")]);
        // SLEEP returns 0 when it ran to completion and 1 when it was interrupted.
        let slept = fetch(
            &mut session,
            "SELECT CAST(SLEEP(0.2) AS SIGNED) AS slept, 7 AS seven",
            0,
            10,
        );
        assert_eq!(
            slept.batch.column(1).as_primitive::<Int64Type>().value(0),
            7,
            "round {round}"
        );
        let slept = slept.batch.column(0).as_primitive::<Int64Type>().value(0);
        assert_eq!(
            slept, 0,
            "round {round}: a kill that arrived while idle interrupted the next statement"
        );
    }
    // And through the driver's own cancel path: cancelling an operation that
    // already finished is ignored, the next statement is unaffected.
    for round in 0..10 {
        let operation = session.begin_operation().unwrap();
        let page = session
            .open_query(&query_request(
                operation.id(),
                "SELECT 1",
                Some(&database_name()),
                0,
                10,
            ))
            .unwrap();
        session.release_page(page.page_id).unwrap();
        env()
            .handle
            .cancel_operation(
                session.connection_id(),
                &CancelRequest {
                    operation_id: operation.id(),
                },
            )
            .unwrap();
        let slept = fetch(&mut session, "SELECT CAST(SLEEP(0.2) AS SIGNED)", 0, 10);
        assert_eq!(
            slept.batch.column(0).as_primitive::<Int64Type>().value(0),
            0,
            "round {round}"
        );
    }
    session.disconnect().unwrap();
}

/// The kit decides "operation A is active" and only then calls the driver's
/// cancel, which has to open a second connection. A may be over and operation B
/// running by the time the `KILL QUERY` is ready: the driver re-checks which
/// operation is running right before it sends the kill, so B survives.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_cancel_that_lands_after_its_operation_ended_never_kills_the_next_one() {
    let mut session = connect();
    let connection_id = session.connection_id();
    let (mut cancelled, mut completed) = (0, 0);
    for round in 0..60_u64 {
        let operation = session.begin_operation().unwrap();
        let runner = session.query_runner().unwrap();
        let request = query_request(
            operation.id(),
            "SELECT CAST(SLEEP(0.02) AS SIGNED)",
            Some(&database_name()),
            0,
            10,
        );
        let operation_id = operation.id();
        let worker = std::thread::spawn(move || {
            let _operation = operation;
            runner.open_query(&request)
        });
        let canceller = std::thread::spawn(move || {
            // Around the end of the 20 ms statement.
            std::thread::sleep(Duration::from_millis(12 + (round % 9) * 2));
            let _ = env()
                .handle
                .cancel_operation(connection_id, &CancelRequest { operation_id });
        });
        match worker.join().unwrap() {
            Ok(page) => {
                completed += 1;
                // Not a query we hold a lease for: just drop it.
                let _ = session.release_page(page.page_id);
            }
            Err(error) => {
                let (code, message) = rejected(error);
                assert_eq!(code, "query.cancelled", "{message}");
                assert!(message.contains("cancelled"), "{message}");
                cancelled += 1;
            }
        }
        // The next operation starts while the cancel may still be in flight.
        let sentinel = fetch(&mut session, "SELECT CAST(SLEEP(0.15) AS SIGNED)", 0, 10);
        assert_eq!(
            sentinel
                .batch
                .column(0)
                .as_primitive::<Int64Type>()
                .value(0),
            0,
            "round {round}: the next statement was interrupted by a stale cancel"
        );
        canceller.join().unwrap();
    }
    println!("stale-cancel stress: {cancelled} cancelled, {completed} completed, sentinel never interrupted");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn an_interrupt_nobody_asked_for_is_a_retryable_query_interrupted() {
    let mut session = connect();
    let connection_id = session.connection_id();
    let thread_id = thread_id(&mut session);
    let request = json!({
        "kind": "sql",
        "operation_id": 900_001,
        "database": database_name(),
        "object": null,
        "offset": 0,
        "limit": 10,
        "payload": { "sql": "SELECT COUNT(*) FROM information_schema.COLUMNS a, information_schema.COLUMNS b, information_schema.COLUMNS c" },
    });
    let worker = std::thread::spawn(move || {
        env()
            .raw
            .call("dbengine_open_query", connection_id, &request)
    });
    std::thread::sleep(Duration::from_millis(600));
    admin_exec(&[&format!("KILL QUERY {thread_id}")]);
    let envelope = worker.join().unwrap();
    assert_eq!(envelope["ok"], false, "{envelope}");
    assert_eq!(envelope["error"]["code"], "query.interrupted", "{envelope}");
    assert_eq!(envelope["error"]["retryable"], true, "{envelope}");
    // Retrying works.
    assert_eq!(ids(&fetch(&mut session, "SELECT 9", 0, 10).batch, 0), [9]);
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn mutations_are_transactional_and_roll_back_on_error() {
    use SqlRowOp::{Delete, Insert, Update};
    let database = database_name();
    let table = unique("mut");
    let nopk = unique("nopk");
    let _cleanup = Cleanup(vec![table.clone(), nopk.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, code VARCHAR(10) NOT NULL UNIQUE, qty INT NOT NULL DEFAULT 0, note VARCHAR(80) NULL)"),
        &format!("CREATE TABLE `{database}`.`{nopk}` (k INT NOT NULL, v INT NOT NULL)"),
        &format!("INSERT INTO `{database}`.`{nopk}` VALUES (1, 1), (1, 2)"),
    ]);
    let mut session = connect();
    let state = |sql: String| admin_rows(&sql);

    // insert (values are bound, never spliced into SQL).
    let evil = "x'); DROP TABLE t; -- \\";
    let inserted = apply(
        &mut session,
        &table,
        change(
            Insert,
            json!(null),
            json!({"id": 1, "code": "a", "qty": 5, "note": evil}),
        ),
    )
    .unwrap();
    assert_eq!(inserted.affected_rows, 1);
    assert_eq!(
        state(format!(
            "SELECT qty, note FROM `{database}`.`{table}` WHERE id = 1"
        )),
        [[Some("5".into()), Some(evil.into())]]
    );
    // A duplicate fails and changes nothing.
    let (code, message) = rejected(
        apply(
            &mut session,
            &table,
            change(Insert, json!(null), json!({"id": 2, "code": "a"})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.failed");
    assert!(message.contains("1062"), "{message}");
    assert_eq!(
        state(format!("SELECT COUNT(*) FROM `{database}`.`{table}`")),
        [[Some("1".into())]]
    );

    // update by key.
    let updated = apply(
        &mut session,
        &table,
        change(Update, json!({"id": 1}), json!({"qty": 9, "note": null})),
    )
    .unwrap();
    assert_eq!(updated.affected_rows, 1);
    assert_eq!(
        state(format!(
            "SELECT qty, note FROM `{database}`.`{table}` WHERE id = 1"
        )),
        [[Some("9".into()), None]]
    );
    // Setting a column to the value it has is still "the row was found".
    assert_eq!(
        apply(
            &mut session,
            &table,
            change(Update, json!({"id": 1}), json!({"qty": 9}))
        )
        .unwrap()
        .affected_rows,
        1
    );
    // A key that matches nothing is an error (and the connection stays clean).
    for (op, values) in [(Update, json!({"qty": 1})), (Delete, json!(null))] {
        let (code, message) = rejected(
            apply(&mut session, &table, change(op, json!({"id": 99}), values)).unwrap_err(),
        );
        assert_eq!(code, "mutation.no_rows_affected");
        assert!(message.contains("rolled back"), "{message}");
    }
    // NOT NULL violation rolls back.
    let (_, message) = rejected(
        apply(
            &mut session,
            &table,
            change(Update, json!({"id": 1}), json!({"qty": null})),
        )
        .unwrap_err(),
    );
    assert!(message.contains("qty"), "{message}");
    assert_eq!(
        state(format!(
            "SELECT qty FROM `{database}`.`{table}` WHERE id = 1"
        )),
        [[Some("9".into())]]
    );

    // A key that is not unique matches two rows: the change is rolled back.
    let (code, message) = rejected(
        apply(
            &mut session,
            &nopk,
            change(Update, json!({"k": 1}), json!({"v": 100})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.identity_not_unique");
    assert!(message.contains("rolled back"), "{message}");
    assert_eq!(
        state(format!("SELECT v FROM `{database}`.`{nopk}` ORDER BY v")),
        [[Some("1".into())], [Some("2".into())]]
    );
    let (code, _) = rejected(
        apply(
            &mut session,
            &nopk,
            change(Delete, json!({"k": 1}), json!(null)),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.identity_not_unique");
    assert_eq!(
        state(format!("SELECT COUNT(*) FROM `{database}`.`{nopk}`")),
        [[Some("2".into())]]
    );
    // (Each successful change is visible to the admin connection, so no
    // transaction was left open by the failed ones.)

    // Unsafe or malformed requests are refused before they reach the server.
    let bad = |session: &mut DriverSession, kind: &str, identity: Value, payload: Value| {
        let database = database_name();
        let error = mutate(
            session,
            mutation(kind, Some(&database), Some(&table), identity, payload),
        )
        .unwrap_err();
        let (code, message) = rejected(error);
        assert_eq!(code, "mutation.bad_request", "{kind}: {message}");
    };
    let identity = json!({});
    bad(
        &mut session,
        "update",
        identity.clone(),
        json!({"op": "update", "values": {"qty": 1}}),
    );
    bad(
        &mut session,
        "update",
        identity.clone(),
        json!({"op": "update", "key": {}, "values": {"qty": 1}}),
    );
    bad(
        &mut session,
        "update",
        identity.clone(),
        json!({"op": "update", "key": {"id": 1}}),
    );
    bad(
        &mut session,
        "delete",
        identity.clone(),
        json!({"op": "delete"}),
    );
    // The payload op must be the kind.
    bad(
        &mut session,
        "update",
        identity.clone(),
        json!({"op": "delete", "key": {"id": 1}}),
    );
    bad(
        &mut session,
        "insert",
        identity.clone(),
        json!({"values": {"id": 5}}),
    );
    bad(
        &mut session,
        "insert",
        json!("not an identity"),
        json!({"op": "insert"}),
    );

    // `schema` in the identity is ignored (MySQL has no schema level).
    let database_for_schema = database_name();
    let with_schema = mutate(
        &mut session,
        mutation(
            "insert",
            Some(&database_for_schema),
            Some(&table),
            json!({"schema": "ignored"}),
            serde_json::to_value(change(Insert, json!(null), json!({"id": 4, "code": "d"})))
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(with_schema.affected_rows, 1);
    apply(
        &mut session,
        &table,
        change(Delete, json!({"id": 4}), json!(null)),
    )
    .unwrap();

    // Open pages are invalidated by a write.
    let page = run(&mut session, &format!("SELECT * FROM `{table}`"), 0, 10).unwrap();
    let written = apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 3, "code": "c"})),
    )
    .unwrap();
    assert_eq!(written.invalidated_page_ids, [page.page_id]);

    // delete by key.
    for id in [3, 1] {
        assert_eq!(
            apply(
                &mut session,
                &table,
                change(Delete, json!({"id": id}), json!(null))
            )
            .unwrap()
            .affected_rows,
            1
        );
    }
    assert_eq!(
        state(format!("SELECT COUNT(*) FROM `{database}`.`{table}`")),
        [[Some("0".into())]]
    );

    // A kind the driver does not advertise never reaches it.
    assert!(matches!(
        mutate(
            &mut session,
            mutation("drop_database", Some(&database), None, json!({}), json!({}))
        ),
        Err(LoaderError::UnsupportedFeature { .. })
    ));
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn composite_keys_address_exactly_one_row() {
    use SqlRowOp::{Delete, Insert, Update};
    let database = database_name();
    let table = unique("comp");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (tenant INT NOT NULL, id INT NOT NULL, name VARCHAR(20) NOT NULL, PRIMARY KEY (tenant, id))"
    )]);
    let mut session = connect();
    let names = || {
        admin_rows(&format!(
            "SELECT tenant, id, name FROM `{database}`.`{table}` ORDER BY tenant, id"
        ))
    };
    for (tenant, id) in [(1, 1), (1, 2), (2, 1)] {
        apply(
            &mut session,
            &table,
            change(
                Insert,
                json!(null),
                json!({"tenant": tenant, "id": id, "name": format!("n{tenant}{id}")}),
            ),
        )
        .unwrap();
    }
    // Both key columns pick one row.
    let updated = apply(
        &mut session,
        &table,
        change(
            Update,
            json!({"tenant": 1, "id": 2}),
            json!({"name": "changed"}),
        ),
    )
    .unwrap();
    assert_eq!(updated.affected_rows, 1);
    assert_eq!(
        names(),
        [
            [Some("1".into()), Some("1".into()), Some("n11".into())],
            [Some("1".into()), Some("2".into()), Some("changed".into())],
            [Some("2".into()), Some("1".into()), Some("n21".into())],
        ]
    );
    // Half a key matches two rows: refused and rolled back.
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(Update, json!({"tenant": 1}), json!({"name": "x"})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.identity_not_unique");
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(Delete, json!({"id": 1}), json!(null)),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.identity_not_unique");
    assert_eq!(names().len(), 3);
    // A composite delete removes only its row.
    assert_eq!(
        apply(
            &mut session,
            &table,
            change(Delete, json!({"tenant": 2, "id": 1}), json!(null))
        )
        .unwrap()
        .affected_rows,
        1
    );
    assert_eq!(names().len(), 2);
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(Delete, json!({"tenant": 2, "id": 1}), json!(null)),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.no_rows_affected");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn batches_apply_in_order_in_one_transaction_all_or_nothing() {
    use SqlRowOp::{Delete, Insert, Update};
    let database = database_name();
    let table = unique("batch");
    let nopk = unique("bnopk");
    let _cleanup = Cleanup(vec![table.clone(), nopk.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, code VARCHAR(10) NOT NULL UNIQUE, qty INT NOT NULL DEFAULT 0)"),
        &format!("CREATE TABLE `{database}`.`{nopk}` (k INT NOT NULL, v INT NOT NULL)"),
        &format!("INSERT INTO `{database}`.`{nopk}` VALUES (1, 1), (1, 2)"),
    ]);
    let mut session = connect();
    assert!(session.capabilities().has(Capability::MutationBatch));
    let rows = || {
        admin_rows(&format!(
            "SELECT id, code, qty FROM `{database}`.`{table}` ORDER BY id"
        ))
    };
    let text = |value: &str| Some(value.to_owned());

    // Mixed ops, in order: the update and the delete see the inserts before them.
    let page = run(&mut session, &format!("SELECT * FROM `{table}`"), 0, 10).unwrap();
    let applied = apply_batch(
        &mut session,
        &table,
        vec![
            change(Insert, json!(null), json!({"id": 1, "code": "a"})),
            change(Insert, json!(null), json!({"id": 2, "code": "b"})),
            change(Update, json!({"id": 1}), json!({"qty": 7})),
            change(Delete, json!({"id": 2}), json!(null)),
        ],
    )
    .unwrap();
    assert_eq!(applied.affected_rows, 4);
    assert_eq!(applied.invalidated_page_ids, [page.page_id]);
    assert_eq!(rows(), [[text("1"), text("a"), text("7")]]);

    // A server error on the last change rolls back the earlier ones too.
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &table,
            vec![
                change(Insert, json!(null), json!({"id": 10, "code": "x"})),
                change(Update, json!({"id": 1}), json!({"qty": 8})),
                change(Insert, json!(null), json!({"id": 11, "code": "a"})),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.failed");
    assert!(message.contains("1062"), "{message}");
    assert_eq!(rows(), [[text("1"), text("a"), text("7")]]);

    // A key that matches nothing fails the whole batch and names the change.
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &table,
            vec![
                change(Insert, json!(null), json!({"id": 12, "code": "y"})),
                change(Delete, json!({"id": 404}), json!(null)),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.no_rows_affected");
    assert!(message.contains("change 1"), "{message}");
    assert_eq!(rows(), [[text("1"), text("a"), text("7")]]);

    // A key that matches several rows too.
    let (code, _) = rejected(
        apply_batch(
            &mut session,
            &nopk,
            vec![
                change(Insert, json!(null), json!({"k": 9, "v": 9})),
                change(Update, json!({"k": 1}), json!({"v": 50})),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.identity_not_unique");
    assert_eq!(
        admin_rows(&format!(
            "SELECT k, v FROM `{database}`.`{nopk}` ORDER BY v"
        )),
        [[text("1"), text("1")], [text("1"), text("2")]]
    );

    // A malformed change is refused before anything runs.
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &table,
            vec![
                change(Insert, json!(null), json!({"id": 13, "code": "z"})),
                change(Update, json!({}), json!({"qty": 1})),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.bad_request");
    assert!(message.contains("change 1"), "{message}");
    let (code, _) = rejected(apply_batch(&mut session, &table, vec![]).unwrap_err());
    assert_eq!(code, "mutation.bad_request");
    assert_eq!(rows(), [[text("1"), text("a"), text("7")]]);

    // The connection holds no open transaction afterwards.
    apply(
        &mut session,
        &table,
        change(Delete, json!({"id": 1}), json!(null)),
    )
    .unwrap();
    assert!(rows().is_empty());
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn create_database_works_with_privileges_and_fails_cleanly_without() {
    let name = unique("db");
    struct DropDatabase(String);
    impl Drop for DropDatabase {
        fn drop(&mut self) {
            let _ = std::panic::catch_unwind(|| {
                admin_exec(&[&format!("DROP DATABASE IF EXISTS `{}`", self.0)])
            });
        }
    }
    let _cleanup = DropDatabase(name.clone());

    // The unprivileged test account may not create databases.
    let mut limited = connect();
    let error = mutate(
        &mut limited,
        mutation(
            "create_database",
            Some(&name),
            None,
            json!(null),
            json!(null),
        ),
    )
    .unwrap_err();
    let (code, message) = rejected(error);
    assert_eq!(code, "mutation.create_database.rejected");
    assert!(message.contains("denied"), "{message}");
    limited.disconnect().unwrap();

    let mut root = connect_root();
    let created = mutate(
        &mut root,
        mutation(
            "create_database",
            Some(&name),
            None,
            json!(null),
            json!(null),
        ),
    )
    .unwrap();
    assert_eq!(created.affected_rows, 0);
    assert_eq!(
        admin_rows(&format!(
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{name}'"
        )),
        [[Some(name.clone())]]
    );
    let (code, message) = rejected(
        mutate(
            &mut root,
            mutation(
                "create_database",
                Some(&name),
                None,
                json!(null),
                json!(null),
            ),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.create_database.rejected");
    assert!(message.contains("1007"), "{message}");
    // The new database shows up in the navigator.
    let all = nav(&mut root, None, None, None, 0);
    assert!(all.nodes.iter().any(|node| node.label == name));
    // The payload takes no options.
    let (code, _) = rejected(
        mutate(
            &mut root,
            mutation(
                "create_database",
                Some(&name),
                None,
                json!(null),
                json!({"charset": "utf8mb4"}),
            ),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.bad_request");
    root.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn describe_object_reports_columns_keys_foreign_keys_indexes_references_triggers_and_ddl() {
    let database = database_name();
    let parent = unique("dparent");
    let child = unique("dchild");
    let view = unique("dview");
    let _cleanup = Cleanup(vec![view.clone(), child.clone(), parent.clone()]);
    let trigger = unique("trg");
    // FOREIGN KEY and CHECK names are unique per schema on MySQL.
    let (fk, check_name) = (unique("fk"), unique("chk"));
    admin_exec(&[
        &format!("DROP TRIGGER IF EXISTS `{database}`.`{trigger}`"),
        &format!(
            "CREATE TABLE `{database}`.`{parent}` (
               id INT NOT NULL AUTO_INCREMENT, code VARCHAR(20) NOT NULL DEFAULT 'abc', qty INT DEFAULT NULL,
               price DECIMAL(10,2) DEFAULT 1.50, created DATETIME DEFAULT CURRENT_TIMESTAMP,
               doubled INT AS (qty * 2) STORED,
               PRIMARY KEY (id), UNIQUE KEY uq_code (code), KEY idx_qty_code (qty, code),
               CONSTRAINT `{check_name}` CHECK (qty IS NULL OR qty >= 0)) ENGINE=InnoDB"
        ),
        &format!(
            "CREATE TABLE `{database}`.`{child}` (cid INT PRIMARY KEY, pid INT,
               CONSTRAINT `{fk}` FOREIGN KEY (pid) REFERENCES `{parent}`(id) ON DELETE CASCADE ON UPDATE RESTRICT)"
        ),
        // Triggers need SUPER on MySQL with binary logging: created as root.
        &format!(
            "CREATE TRIGGER `{database}`.`{trigger}` BEFORE INSERT ON `{database}`.`{parent}` FOR EACH ROW SET NEW.qty = IFNULL(NEW.qty, 0)"
        ),
        &format!("CREATE VIEW `{database}`.`{view}` AS SELECT id, code FROM `{database}`.`{parent}`"),
    ]);
    let mut session = connect();
    let connection_id = session.connection_id();
    let describe = |object: &str| {
        env().raw.call(
            "dbengine_describe_object",
            connection_id,
            &serde_json::to_value(SqlObjectRef {
                database: Some(database.clone()),
                schema: None,
                object: object.to_owned(),
            })
            .unwrap(),
        )
    };

    let envelope = describe(&parent);
    assert_eq!(envelope["ok"], true, "{envelope}");
    let table = &envelope["value"];
    // The reply is the shared TableProperties.
    let typed: TableProperties =
        serde_json::from_value(table.clone()).expect("reply deserializes into TableProperties");
    assert_eq!(typed.columns.len(), 6);
    assert_eq!(
        typed.keys.as_ref().unwrap()[0].kind,
        ConstraintKind::PrimaryKey
    );
    assert!(typed
        .keys
        .as_ref()
        .unwrap()
        .iter()
        .any(|key| key.kind == ConstraintKind::Check));
    assert!(typed.foreign_keys.as_ref().unwrap().is_empty());
    assert_eq!(typed.references.as_ref().unwrap().len(), 1);
    assert_eq!(typed.triggers.as_ref().unwrap().len(), 1);
    assert!(typed.ddl.as_deref().unwrap().starts_with("CREATE TABLE"));
    println!(
        "describe({parent}) = {}",
        serde_json::to_string_pretty(table).unwrap()
    );
    for field in [
        "columns",
        "keys",
        "foreign_keys",
        "indexes",
        "references",
        "triggers",
        "ddl",
    ] {
        assert!(table.get(field).is_some(), "{field}");
    }
    let columns = table["columns"].as_array().unwrap();
    let column = |name: &str| {
        columns
            .iter()
            .find(|column| column["name"] == name)
            .unwrap_or_else(|| panic!("column {name}"))
    };
    assert_eq!(columns.len(), 6);
    assert_eq!(column("id")["primary_key"], true);
    assert_eq!(column("id")["nullable"], false);
    assert_eq!(column("id")["auto_increment"], true);
    assert_eq!(column("code")["default_expression"], "'abc'");
    assert_eq!(column("price")["default_expression"], "1.50");
    assert!(column("created")["default_expression"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase()
        .starts_with("current_timestamp"));
    assert_eq!(column("doubled")["is_generated"], true);
    assert!(column("doubled")["default_expression"]
        .as_str()
        .unwrap()
        .contains("qty"));
    assert_eq!(column("qty")["is_generated"], false);
    assert!(column("price")["data_type"]
        .as_str()
        .unwrap()
        .starts_with("decimal(10,2)"));

    let keys = table["keys"].as_array().unwrap();
    assert_eq!(keys[0]["kind"], "primary_key");
    assert_eq!(keys[0]["columns"], json!(["id"]));
    assert!(keys.iter().any(|key| key["name"] == "uq_code"
        && key["kind"] == "unique"
        && key["columns"] == json!(["code"])));
    let check = keys
        .iter()
        .find(|key| key["kind"] == "check")
        .expect("check constraint");
    assert_eq!(check["name"], check_name);
    assert!(check["definition"].as_str().unwrap().contains("qty"));

    let indexes = table["indexes"].as_array().unwrap();
    assert_eq!(indexes[0]["name"], "PRIMARY");
    assert_eq!(indexes[0]["primary"], true);
    let composite = indexes
        .iter()
        .find(|index| index["name"] == "idx_qty_code")
        .expect("composite index");
    assert_eq!(composite["columns"], json!(["qty", "code"]));
    assert_eq!(composite["unique"], false);
    assert_eq!(composite["method"], "btree");
    assert!(indexes
        .iter()
        .any(|index| index["name"] == "uq_code" && index["unique"] == true));

    assert_eq!(table["foreign_keys"], json!([]));
    let references = table["references"].as_array().unwrap();
    assert_eq!(references.len(), 1, "{references:?}");
    assert_eq!(references[0]["name"], fk);
    assert_eq!(references[0]["table"], child);
    assert_eq!(references[0]["referenced_table"], parent);
    assert_eq!(references[0]["columns"], json!(["pid"]));

    let triggers = table["triggers"].as_array().unwrap();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0]["name"], trigger);
    assert_eq!(triggers[0]["timing"], "BEFORE");
    assert_eq!(triggers[0]["events"], json!(["INSERT"]));
    assert!(triggers[0]["definition"]
        .as_str()
        .unwrap()
        .contains("IFNULL"));

    let ddl = table["ddl"].as_str().expect("ddl");
    assert!(
        ddl.starts_with("CREATE TABLE") && ddl.contains(&parent),
        "{ddl}"
    );

    // The child owns the foreign key.
    let child_value = describe(&child);
    let child_table = &child_value["value"];
    let foreign_keys = child_table["foreign_keys"].as_array().unwrap();
    assert_eq!(foreign_keys.len(), 1);
    assert_eq!(foreign_keys[0]["name"], fk);
    assert_eq!(foreign_keys[0]["referenced_table"], parent);
    assert_eq!(foreign_keys[0]["referenced_columns"], json!(["id"]));
    assert_eq!(foreign_keys[0]["on_delete"], "CASCADE");
    assert_eq!(foreign_keys[0]["on_update"], "RESTRICT");
    assert_eq!(child_table["references"], json!([]));

    // A view: columns and its definition, no constraints.
    let view_value = describe(&view);
    assert_eq!(view_value["ok"], true, "{view_value}");
    assert_eq!(view_value["value"]["columns"].as_array().unwrap().len(), 2);
    assert!(view_value["value"]["ddl"]
        .as_str()
        .unwrap()
        .contains("VIEW"));
    assert_eq!(view_value["value"]["keys"], json!([]));

    // Errors.
    let missing = describe("p3_does_not_exist");
    assert_eq!(missing["error"]["code"], "describe.not_found");
    let bad = env().raw.call(
        "dbengine_describe_object",
        connection_id,
        &json!({"database": database}),
    );
    assert_eq!(bad["error"]["code"], "describe.bad_request");
    // The old `table` alias is gone: the request is a SqlObjectRef.
    let alias = env().raw.call(
        "dbengine_describe_object",
        connection_id,
        &json!({"database": database, "table": parent}),
    );
    assert_eq!(alias["error"]["code"], "describe.bad_request");
    // No database: the connection's current one.
    let current = env().raw.call(
        "dbengine_describe_object",
        connection_id,
        &json!({"object": parent}),
    );
    assert_eq!(current["ok"], true, "{current}");
    assert_eq!(current["value"], envelope["value"]);
    session.disconnect().unwrap();
    admin_exec(&[&format!("DROP TRIGGER IF EXISTS `{database}`.`{trigger}`")]);
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn disconnect_during_a_running_query_cancels_it() {
    let marker = unique("disc");
    let mut session = connect();
    let operation = session.begin_operation().unwrap();
    let runner = session.query_runner().unwrap();
    let request = query_request(
        operation.id(),
        &format!("SELECT /* {marker} */ SLEEP(10)"),
        Some(&database_name()),
        0,
        10,
    );
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(600));
    // The server sees the statement.
    let running = || {
        admin_rows(&format!(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE INFO LIKE '%{marker}%' AND INFO NOT LIKE '%PROCESSLIST%'"
        ))[0][0]
            .clone()
            .unwrap()
    };
    assert_eq!(running(), "1");
    let started = Instant::now();
    session.disconnect().expect("disconnect");
    let result = worker.join().unwrap();
    let took = started.elapsed();
    println!("disconnect during SLEEP(10): disconnect + query unwound in {took:?}");
    assert!(took < Duration::from_secs(4), "{took:?}");
    assert!(
        result.is_err(),
        "the interrupted query must not report success"
    );
    // No statement or connection of ours is left running on the server.
    let deadline = Instant::now() + Duration::from_secs(3);
    while running() != "0" {
        assert!(
            Instant::now() < deadline,
            "the statement is still running on the server"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn concurrent_operations_on_one_connection_are_serialized() {
    let mut session = connect();
    let connection_id = session.connection_id();
    let mut workers = Vec::new();
    for index in 0..4_i64 {
        let operation = session.begin_operation().unwrap();
        let runner = session.query_runner().unwrap();
        let request = query_request(
            operation.id(),
            &format!("SELECT SLEEP(0.2), {index}"),
            Some(&database_name()),
            0,
            10,
        );
        workers.push(std::thread::spawn(move || {
            let _operation = operation;
            runner.open_query(&request)
        }));
    }
    let started = Instant::now();
    for worker in workers {
        worker.join().unwrap().expect("each statement completes");
    }
    // Four 0.2 s statements through one client take at least 0.8 s.
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "{:?}",
        started.elapsed()
    );
    let _ = connection_id;
    session.disconnect().unwrap();
}

// ------------------------------------------------- shared SQL contract rules

#[test]
#[ignore = "needs a live server (see module docs)"]
fn paging_follows_the_contract_default_cap_and_not_pageable() {
    let database = database_name();
    let table = unique("pg");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (a INT NOT NULL AUTO_INCREMENT PRIMARY KEY, b VARCHAR(64))"),
        &format!(
            "INSERT INTO `{database}`.`{table}` (b) \
             SELECT x.COLUMN_NAME FROM information_schema.COLUMNS x \
             CROSS JOIN (SELECT 1 FROM information_schema.COLUMNS LIMIT 5) y LIMIT 300"
        ),
    ]);
    let mut session = connect();
    let select = format!("SELECT * FROM `{table}` ORDER BY a");

    // limit 0 is the default page (200), not "unbounded".
    let page = run(&mut session, &select, 0, 0).unwrap();
    assert_eq!(page.row_count, 200);
    assert!(page.flags.truncated);
    session.release_page(page.page_id).unwrap();
    // An oversized limit is capped, never refused; the whole table fits.
    let page = run(&mut session, &select, 0, 5_000_000).unwrap();
    assert_eq!(page.row_count, 300);
    assert!(!page.flags.truncated);
    session.release_page(page.page_id).unwrap();
    // The client-side skip path (SHOW) is bounded too.
    let page = run(&mut session, "SHOW DATABASES", 0, 1).unwrap();
    assert_eq!(page.row_count, 1);
    session.release_page(page.page_id).unwrap();

    // Anything but a single pure read runs once: offset > 0 is refused and the
    // statement never runs.
    let count =
        || admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`"))[0][0].clone();
    let insert = format!("INSERT INTO `{database}`.`{table}` (b) VALUES ('once')");
    let page = run(&mut session, &insert, 0, 10).unwrap();
    assert!(!page.flags.truncated);
    session.release_page(page.page_id).unwrap();
    assert_eq!(count().as_deref(), Some("301"));
    for (sql, offset) in [
        (insert.as_str(), 10),
        ("SELECT 1; SELECT 2", 1),
        ("USE information_schema", 1),
    ] {
        let (code, message) = rejected(run(&mut session, sql, offset, 10).unwrap_err());
        assert_eq!(code, "query.not_pageable", "{sql}: {message}");
    }
    assert_eq!(
        count().as_deref(),
        Some("301"),
        "a refused statement must not run"
    );
    // A script at offset 0 runs once and is not truncated.
    let page = run(&mut session, "SELECT 1; SELECT 2", 0, 10).unwrap();
    assert!(!page.flags.truncated);
    session.release_page(page.page_id).unwrap();
    // REPLACE()/INSERT() the string functions are reads and page normally.
    let page = run(&mut session, "SELECT REPLACE('abc', 'b', 'x') AS r", 0, 10).unwrap();
    assert_eq!(page.row_count, 1);
    session.release_page(page.page_id).unwrap();
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn dangerous_statements_need_confirmation() {
    let database = database_name();
    let table = unique("danger");
    let spare = unique("dangerspare");
    let _cleanup = Cleanup(vec![table.clone(), spare.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, qty INT)"),
        &format!("CREATE TABLE `{database}`.`{spare}` (id INT)"),
        &format!("INSERT INTO `{database}`.`{table}` VALUES (1, 1), (2, 2), (3, 3)"),
    ]);
    let mut session = connect();
    let rows = || admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`"))[0][0].clone();
    for sql in [
        format!("DELETE FROM `{table}`"),
        format!("UPDATE `{table}` SET qty = 0"),
        format!("UPDATE `{table}` SET qty = (SELECT 1 WHERE 1 = 1)"),
        format!("-- looks harmless\nTRUNCATE TABLE `{table}`"),
        format!("/* c */ DROP TABLE `{spare}`"),
        format!("SELECT 1; DROP TABLE `{spare}`"),
        format!("ALTER TABLE `{table}` ADD COLUMN extra INT"),
        "GRANT SELECT ON *.* TO 'p3_nobody'@'localhost'".to_owned(),
        "CREATE DATABASE p3_must_not_exist".to_owned(),
    ] {
        let (code, reason) = rejected(run(&mut session, &sql, 0, 10).unwrap_err());
        assert_eq!(code, "query.confirmation_required", "{sql}");
        assert!(
            !reason.trim().is_empty(),
            "{sql}: the refusal must explain why"
        );
    }
    assert_eq!(
        rows().as_deref(),
        Some("3"),
        "a refused statement must not run"
    );
    assert!(
        admin_rows(&format!("SHOW TABLES FROM `{database}` LIKE '{spare}'")).len() == 1,
        "the unconfirmed DROP must not have run"
    );
    // Harmless statements, and keywords inside strings and comments, run as is.
    for sql in [
        format!("UPDATE `{table}` SET qty = 5 WHERE id = 2"),
        "SELECT 'DROP TABLE x; DELETE FROM y'".to_owned(),
        "SELECT 1 /* DROP TABLE x */".to_owned(),
        "SELECT REPLACE('a', 'a', 'b')".to_owned(),
        format!("DELETE FROM `{table}` WHERE id IN (SELECT 99)"),
        format!("DELETE FROM `{table}` WHERE id = 3"),
    ] {
        let page = run(&mut session, &sql, 0, 10)
            .unwrap_or_else(|error| panic!("`{sql}` must not need confirmation: {error}"));
        session.release_page(page.page_id).unwrap();
    }
    assert_eq!(rows().as_deref(), Some("2"));
    // Confirmed: runs.
    let page = run_with(&mut session, &format!("DELETE FROM `{table}`"), 0, 10, true).unwrap();
    session.release_page(page.page_id).unwrap();
    assert_eq!(rows().as_deref(), Some("0"));
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn use_inside_user_sql_never_leaks_into_the_next_request() {
    let database = database_name();
    let mut session = connect();
    let current = |session: &mut DriverSession| {
        let fetched = fetch(session, "SELECT DATABASE()", 0, 10);
        fetched
            .batch
            .column(0)
            .as_string::<i32>()
            .value(0)
            .to_owned()
    };
    assert_eq!(current(&mut session), database);
    for sql in [
        "USE information_schema",
        "-- note\nUSE information_schema",
        "# note\nUSE information_schema",
        "/* note */ use `information_schema`",
        "/*!50000 USE information_schema */",
        "SELECT 1; USE information_schema",
        "  /* a */ -- b\n  USE information_schema ; SELECT 2",
    ] {
        let page = run(&mut session, sql, 0, 10)
            .unwrap_or_else(|error| panic!("`{}`: {error}", sql.replace('\n', " ")));
        session.release_page(page.page_id).unwrap();
        // The next request names the original database and must run there.
        assert_eq!(
            current(&mut session),
            database,
            "after `{}` the request for `{database}` ran elsewhere",
            sql.replace('\n', " ")
        );
    }
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn mutations_join_a_user_transaction_through_a_savepoint() {
    use SqlRowOp::{Insert, Update};
    let database = database_name();
    let table = unique("utx");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, v INT NOT NULL)"
    )]);
    let mut session = connect();
    let count = |session: &mut DriverSession, other: bool| -> String {
        if other {
            admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`"))[0][0]
                .clone()
                .unwrap()
        } else {
            let fetched = fetch(
                session,
                &format!("SELECT CAST(COUNT(*) AS CHAR) FROM `{table}`"),
                0,
                1,
            );
            fetched
                .batch
                .column(0)
                .as_string::<i32>()
                .value(0)
                .to_owned()
        }
    };
    let release = |session: &mut DriverSession, page: QueryPage| {
        session.release_page(page.page_id).unwrap();
    };

    // Rolled back by the user: the mutation was part of the transaction.
    let page = run(&mut session, "START TRANSACTION", 0, 10).unwrap();
    release(&mut session, page);
    let page = run(
        &mut session,
        &format!("INSERT INTO `{table}` VALUES (1, 1)"),
        0,
        10,
    )
    .unwrap();
    release(&mut session, page);
    apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 2, "v": 2})),
    )
    .unwrap();
    // Failed mutations roll back to their savepoint only.
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(Update, json!({"id": 99}), json!({"v": 1})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.no_rows_affected");
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(Insert, json!(null), json!({"id": 2, "v": 3})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.failed");
    assert_eq!(
        count(&mut session, false),
        "2",
        "the user's work and the good mutation remain"
    );
    assert_eq!(count(&mut session, true), "0", "nothing is committed yet");
    // A batch joins the transaction too, and is all-or-nothing within it.
    let (code, _) = rejected(
        apply_batch(
            &mut session,
            &table,
            vec![
                change(Insert, json!(null), json!({"id": 10, "v": 1})),
                change(Insert, json!(null), json!({"id": 1, "v": 1})),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.failed");
    assert_eq!(count(&mut session, false), "2");
    let page = run(&mut session, "ROLLBACK", 0, 10).unwrap();
    release(&mut session, page);
    assert_eq!(
        count(&mut session, false),
        "0",
        "the user's ROLLBACK undoes the mutation too"
    );

    // Committed by the user: persists, invisible to others until then.
    let page = run(&mut session, "START TRANSACTION", 0, 10).unwrap();
    release(&mut session, page);
    apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 5, "v": 5})),
    )
    .unwrap();
    assert_eq!(
        count(&mut session, true),
        "0",
        "not visible to others before the user's COMMIT"
    );
    let page = run(&mut session, "COMMIT", 0, 10).unwrap();
    release(&mut session, page);
    assert_eq!(count(&mut session, true), "1");

    // Outside a user transaction a mutation commits on its own (visible at once).
    apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 6, "v": 6})),
    )
    .unwrap();
    assert_eq!(count(&mut session, true), "2");
    // After the user's transaction ended the connection is back to normal: a
    // second START TRANSACTION / mutation / ROLLBACK round works the same way.
    let page = run(&mut session, "START TRANSACTION", 0, 10).unwrap();
    release(&mut session, page);
    apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 7, "v": 7})),
    )
    .unwrap();
    let page = run(&mut session, "ROLLBACK", 0, 10).unwrap();
    release(&mut session, page);
    assert_eq!(count(&mut session, true), "2");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_connection_lost_mid_mutation_is_closed_for_good() {
    let database = database_name();
    let table = unique("lost");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY)"
    )]);
    let mut session = connect();
    let id = thread_id(&mut session);
    admin_exec(&[&format!("KILL {id}")]);
    // The mutation cannot even begin; the driver closes the dead connection
    // instead of leaving a session in an unknown state behind.
    let error = apply(
        &mut session,
        &table,
        change(SqlRowOp::Insert, json!(null), json!({"id": 1})),
    )
    .unwrap_err();
    let (code, message) = rejected(error);
    assert_eq!(
        code, "mutation.not_connected",
        "the first failure already says the connection is lost: {message}"
    );
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(SqlRowOp::Insert, json!(null), json!({"id": 1})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.not_connected");
    let (code, _) = rejected(run(&mut session, "SELECT 1", 0, 10).unwrap_err());
    assert_eq!(code, "query.not_connected");
    // Catalog calls do not serve a connection that was closed as untrustworthy.
    let connection_id = session.connection_id();
    let listed = env().raw.call(
        "dbengine_list_nav_nodes",
        connection_id,
        &json!({"parent": null, "database": database, "payload": {"database": database}, "cursor": null, "limit": 0}),
    );
    assert_eq!(
        listed["error"]["code"], "navigator.not_connected",
        "{listed}"
    );
    let described = env().raw.call(
        "dbengine_describe_object",
        connection_id,
        &json!({"database": database, "object": table}),
    );
    assert_eq!(
        described["error"]["code"], "describe.not_connected",
        "{described}"
    );
    let _ = session.disconnect();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn catalog_calls_do_not_wait_behind_a_running_query() {
    let database = database_name();
    let table = unique("meta");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, name VARCHAR(10))"
    )]);
    let mut session = connect();
    let connection_id = session.connection_id();
    let operation = session.begin_operation().unwrap();
    let runner = session.query_runner().unwrap();
    let request = query_request(operation.id(), "SELECT SLEEP(6)", Some(&database), 0, 10);
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(600));

    let started = Instant::now();
    let page = nav(
        &mut session,
        Some(json!({"database": database})),
        None,
        None,
        0,
    );
    assert!(page.nodes.iter().any(|node| node.label == table));
    let described = env().raw.call(
        "dbengine_describe_object",
        connection_id,
        &json!({"database": database, "object": table}),
    );
    assert_eq!(described["ok"], true, "{described}");
    // A describe without a database uses the connection's current one.
    let described = env().raw.call(
        "dbengine_describe_object",
        connection_id,
        &json!({"object": table}),
    );
    assert_eq!(described["ok"], true, "{described}");
    let took = started.elapsed();
    println!("navigator + two describes during SLEEP(6): {took:?}");
    assert!(
        took < Duration::from_secs(3),
        "catalog calls waited {took:?} behind the query"
    );
    env()
        .handle
        .cancel_operation(
            connection_id,
            &CancelRequest {
                operation_id: operation.id(),
            },
        )
        .unwrap();
    let _ = worker.join().unwrap();
    session.disconnect().unwrap();
    // After disconnect the catalog connection is gone too.
    let closed = env().raw.call(
        "dbengine_describe_object",
        connection_id,
        &json!({"object": table}),
    );
    assert_eq!(closed["ok"], false);
}

/// Drops the routines, trigger, event and sequence a test created, even on panic.
struct DropObjects(Vec<String>);

impl Drop for DropObjects {
    fn drop(&mut self) {
        for statement in &self.0 {
            let _ = std::panic::catch_unwind(|| admin_exec(&[statement]));
        }
    }
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn schema_objects_list_by_kind_and_show_their_source() {
    use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef, SchemaObjectSource};
    let database = database_name();
    let table = unique("so_t");
    let (function, procedure, trigger, event) =
        (unique("so_f"), unique("so_p"), unique("so_trg"), unique("so_ev"));
    let sequence = unique("so_seq");
    let _cleanup = Cleanup(vec![table.clone()]);
    let mut drops = vec![
        format!("DROP FUNCTION IF EXISTS `{database}`.`{function}`"),
        format!("DROP PROCEDURE IF EXISTS `{database}`.`{procedure}`"),
        format!("DROP TRIGGER IF EXISTS `{database}`.`{trigger}`"),
        format!("DROP EVENT IF EXISTS `{database}`.`{event}`"),
    ];
    if CFG.driver == "mariadb" {
        drops.push(format!("DROP SEQUENCE IF EXISTS `{database}`.`{sequence}`"));
    }
    let _drops = DropObjects(drops);
    // The server prints a routine's source to its definer (or an account with
    // SHOW_ROUTINE): root creates them on behalf of the test account.
    let test_user = uri().split_once("://").unwrap().1.split(':').next().unwrap().to_owned();
    let host = admin_rows(&format!("SELECT host FROM mysql.user WHERE user = '{test_user}' LIMIT 1"))
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next().flatten())
        .unwrap_or_else(|| "%".into());
    let definer = format!("`{test_user}`@`{host}`");
    let mut session = connect();
    let mut statements = vec![
        format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, qty INT)"),
        format!("CREATE DEFINER = {definer} FUNCTION `{database}`.`{function}`(a INT) RETURNS INT DETERMINISTIC RETURN a + 1"),
        format!("CREATE DEFINER = {definer} PROCEDURE `{database}`.`{procedure}`(IN n INT) SELECT n"),
        format!(
            "CREATE TRIGGER `{database}`.`{trigger}` BEFORE INSERT ON `{database}`.`{table}` FOR EACH ROW SET NEW.qty = IFNULL(NEW.qty, 0)"
        ),
        format!(
            "CREATE EVENT `{database}`.`{event}` ON SCHEDULE EVERY 1 DAY STARTS CURRENT_TIMESTAMP + INTERVAL 1 DAY DO SELECT 1"
        ),
    ];
    if CFG.driver == "mariadb" {
        statements.push(format!("CREATE SEQUENCE `{database}`.`{sequence}` START WITH 7 INCREMENT BY 2"));
    }
    admin_exec(&statements.iter().map(String::as_str).collect::<Vec<_>>());

    let list = |session: &mut DriverSession, kind: &str, filter: Option<&str>| {
        let mut payload = json!({"database": database, "kind": kind});
        if let Some(filter) = filter {
            payload["filter"] = json!(filter);
        }
        nav(session, Some(payload), Some(&format!("database:{database}")), None, 500)
    };
    let source_of = |session: &mut DriverSession, node: &engine_driver_api::NavigatorNode| {
        serde_json::from_value::<SchemaObjectSource>(
            session.describe_object(&node.payload).expect("source"),
        )
        .expect("typed source")
        .source
    };
    let ours = |page: &engine_driver_api::NavigatorPage, name: &str| {
        page.nodes.iter().position(|node| node.label.starts_with(name))
    };

    let functions = list(&mut session, "function", Some(&function));
    assert_eq!(functions.nodes.len(), 1);
    assert_eq!((functions.nodes[0].kind.as_str(), functions.nodes[0].has_children), ("function", false));
    let text = source_of(&mut session, &functions.nodes[0]);
    assert!(text.contains("CREATE") && text.contains("FUNCTION") && text.contains(&function) && text.ends_with(';'), "{text}");
    assert!(list(&mut session, "function", Some("no_such_routine_zz")).nodes.is_empty());

    let procedures = list(&mut session, "procedure", Some(&procedure));
    assert_eq!(procedures.nodes.len(), 1);
    assert!(source_of(&mut session, &procedures.nodes[0]).contains("PROCEDURE"));
    assert!(ours(&list(&mut session, "function", None), &procedure).is_none(), "a procedure is not a function");

    let triggers = list(&mut session, "trigger", Some(&trigger));
    assert_eq!(triggers.nodes.len(), 1);
    assert_eq!(triggers.nodes[0].label, format!("{trigger} on {table}"));
    let text = source_of(&mut session, &triggers.nodes[0]);
    assert!(text.contains("TRIGGER") && text.contains("IFNULL(NEW.qty, 0)"), "{text}");

    let events = list(&mut session, "event", Some(&event));
    assert_eq!(events.nodes.len(), 1);
    assert!(source_of(&mut session, &events.nodes[0]).contains("CREATE"));

    let sequences = list(&mut session, "sequence", Some(&sequence));
    if CFG.driver == "mariadb" {
        // A sequence is table-like: it describes and lists columns like a table, and its
        // source is asked for with a `SchemaObjectRef` built from its name.
        assert_eq!(sequences.nodes.len(), 1);
        let node = &sequences.nodes[0];
        assert_eq!((node.kind.as_str(), node.has_children), ("sequence", true));
        let typed: SqlObjectRef = serde_json::from_value(node.payload.clone()).expect("a table-like payload");
        assert_eq!(typed.object, sequence);
        let described: TableProperties =
            serde_json::from_value(session.describe_object(&node.payload).expect("describe")).unwrap();
        assert!(!described.columns.is_empty(), "a sequence has columns");
        let columns = nav(&mut session, Some(node.payload.clone()), Some(&node.id), None, 100);
        assert!(!columns.nodes.is_empty());
        let text = serde_json::from_value::<SchemaObjectSource>(
            session
                .describe_object(&json!({"database": database, "kind": "sequence", "name": sequence}))
                .expect("source"),
        )
        .unwrap()
        .source;
        assert!(text.contains("SEQUENCE") && text.contains("start with 7"), "{text}");
        // And the table listing does not repeat it.
        let tables = nav(&mut session, Some(json!({"database": database})), Some(&format!("database:{database}")), None, 500);
        assert!(ours(&tables, &sequence).is_none(), "the table listing leaves sequences to their own group");
    } else {
        assert!(sequences.nodes.is_empty(), "MySQL has no sequences");
    }
    // Kinds the family does not have list nothing.
    assert!(list(&mut session, "type", None).nodes.is_empty());
    assert!(list(&mut session, "materialized_view", None).nodes.is_empty());

    // Gone is not-found, an unknown kind is refused, and a hostile name only finds nothing.
    let reference = |kind: SchemaObjectKind, name: &str| {
        serde_json::to_value(SchemaObjectRef {
            database: Some(database.clone()),
            schema: None,
            kind,
            name: name.to_owned(),
            arguments: None,
            table: None,
            identity: None,
        })
        .unwrap()
    };
    let described = |session: &DriverSession, kind, name: &str| match session.describe_object(&reference(kind, name)) {
        Err(LoaderError::DriverRejected { code, .. }) => code,
        other => panic!("expected a rejection, got {other:?}"),
    };
    assert_eq!(described(&session, SchemaObjectKind::Function, "no_such_routine_zz"), "describe.not_found");
    assert_eq!(described(&session, SchemaObjectKind::Trigger, "x`; DROP TABLE t; --"), "describe.not_found");
    assert_eq!(described(&session, SchemaObjectKind::Type, "mood"), "describe.bad_request");
    // A database that does not exist is not-found too (server error 1049), not a failure.
    let elsewhere = serde_json::to_value(SchemaObjectRef {
        database: Some("no_such_db_zz".into()),
        schema: None,
        kind: SchemaObjectKind::Function,
        name: "f".into(),
        arguments: None,
        table: None,
        identity: None,
    })
    .unwrap();
    assert!(matches!(
        session.describe_object(&elsewhere),
        Err(LoaderError::DriverRejected { ref code, .. }) if code == "describe.not_found"
    ));
    let bad = session.list_nav_nodes(
        &json!({"parent": null, "database": database, "payload": {"database": database, "kind": "package"}, "cursor": null, "limit": 10})
            .to_string(),
    );
    assert!(matches!(bad, Err(LoaderError::DriverRejected { ref code, .. }) if code == "navigator.bad_request"), "{bad:?}");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn navigator_filter_describe_action_and_default_limit() {
    let database = database_name();
    let tables: Vec<String> = ["alpha", "alphabet", "beta"]
        .iter()
        .map(|n| unique(n))
        .collect();
    let _cleanup = Cleanup(tables.clone());
    for table in &tables {
        admin_exec(&[&format!("CREATE TABLE `{database}`.`{table}` (id INT)")]);
    }
    let mut session = connect();
    let list = |session: &mut DriverSession, filter: &str| {
        let page = nav(
            session,
            Some(json!({"database": database, "filter": filter})),
            None,
            None,
            0,
        );
        page.nodes
    };
    // Case-insensitive substring of the name; the other tests' tables share the prefix.
    let upper = list(&mut session, "ALPHA");
    let names: Vec<_> = upper.iter().map(|node| node.label.as_str()).collect();
    assert!(
        names.contains(&tables[0].as_str()) && names.contains(&tables[1].as_str()),
        "{names:?}"
    );
    assert!(!names.contains(&tables[2].as_str()));
    assert!(upper
        .iter()
        .all(|node| node.actions.iter().any(|a| a == "describe")));
    // `_` and `%` are literal characters, not wildcards.
    assert!(list(&mut session, "%").is_empty());
    assert!(list(&mut session, "p3_%alpha").is_empty());
    assert!(!list(&mut session, "p3_").is_empty());
    // limit 0 is the default page (500): small catalogs come back whole.
    let all = nav(
        &mut session,
        Some(json!({"database": database})),
        None,
        None,
        0,
    );
    assert!(all.next_cursor.is_none());
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn decimal_digits_and_null_keys_survive_mutations() {
    use SqlRowOp::{Delete, Insert, Update};
    let database = database_name();
    let table = unique("dec");
    let notes = unique("nullkey");
    let _cleanup = Cleanup(vec![table.clone(), notes.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, d DECIMAL(30,20))"),
        &format!(
            "CREATE TABLE `{database}`.`{notes}` (owner VARCHAR(20) NULL, body VARCHAR(20) NULL)"
        ),
        &format!("INSERT INTO `{database}`.`{notes}` VALUES (NULL, 'a'), ('x', 'b')"),
    ]);
    let mut session = connect();
    // A JSON number reaches DECIMAL as its digits, not as the nearest double.
    apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 1, "d": 0.1})),
    )
    .unwrap();
    assert_eq!(
        admin_rows(&format!(
            "SELECT CAST(d AS CHAR) FROM `{database}`.`{table}`"
        )),
        [[Some("0.10000000000000000000".into())]]
    );
    // A null key value matches the row whose column IS NULL.
    apply(
        &mut session,
        &notes,
        change(Update, json!({"owner": null}), json!({"body": "z"})),
    )
    .unwrap();
    assert_eq!(
        admin_rows(&format!(
            "SELECT owner, body FROM `{database}`.`{notes}` ORDER BY body"
        )),
        [
            [Some("x".into()), Some("b".into())],
            [None, Some("z".into())]
        ]
    );
    apply(
        &mut session,
        &notes,
        change(Delete, json!({"owner": null}), json!(null)),
    )
    .unwrap();
    assert_eq!(
        admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{notes}`")),
        [[Some("1".into())]]
    );
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_script_returns_its_last_result_set_and_runs_every_statement_once() {
    let database = database_name();
    let table = unique("script");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY)"
    )]);
    let mut session = connect();
    let count = || {
        admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`"))[0][0]
            .clone()
            .unwrap()
    };
    let shape = |session: &mut DriverSession, sql: &str, limit: u64| {
        let page = run(session, sql, 0, limit).unwrap_or_else(|error| panic!("`{sql}`: {error}"));
        session.release_page(page.page_id).unwrap();
        (
            page.columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            page.row_count,
            page.flags.truncated,
        )
    };
    // The LAST statement that produces a result set wins.
    assert_eq!(
        shape(&mut session, "SELECT 1 AS a; SELECT 2 AS b", 10),
        (vec!["b".to_owned()], 1, false)
    );
    // A trailing write has no result set: the last SELECT's rows are returned.
    assert_eq!(
        shape(
            &mut session,
            &format!("SELECT 1 AS a; INSERT INTO `{table}` VALUES (1)"),
            10
        ),
        (vec!["a".to_owned()], 1, false)
    );
    // Only writes: a zero-column page.
    let writes = format!("INSERT INTO `{table}` VALUES (2); INSERT INTO `{table}` VALUES (3)");
    assert_eq!(shape(&mut session, &writes, 10), (Vec::new(), 0, false));
    assert_eq!(count(), "3", "every statement ran exactly once");
    // A write between two selects does not hide the last one.
    assert_eq!(
        shape(
            &mut session,
            &format!("SELECT 1 AS a; INSERT INTO `{table}` VALUES (4); SELECT COUNT(*) AS n FROM `{table}`"),
            10
        ),
        (vec!["n".to_owned()], 1, false)
    );
    // A script is not pageable: rows past the page are dropped and the page is
    // not truncated (there is no next page of a script).
    assert_eq!(
        shape(
            &mut session,
            "SELECT 1 AS a UNION ALL SELECT 2 UNION ALL SELECT 3; SELECT 4 AS b UNION ALL SELECT 5 UNION ALL SELECT 6",
            2
        ),
        (vec!["b".to_owned()], 2, false)
    );
    // The same cut-off for a single pageable read stays exact.
    assert_eq!(
        shape(
            &mut session,
            "SELECT 4 AS b UNION ALL SELECT 5 UNION ALL SELECT 6",
            2
        ),
        (vec!["b".to_owned()], 2, true)
    );
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn transaction_state_comes_from_the_server_after_an_error() {
    use SqlRowOp::Insert;
    let database = database_name();
    let table = unique("txs");
    let locks = unique("txlock");
    let filler = unique("txfill");
    let _cleanup = Cleanup(vec![table.clone(), locks.clone(), filler.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY)"),
        &format!("CREATE TABLE `{database}`.`{locks}` (id INT PRIMARY KEY, v INT)"),
        &format!("INSERT INTO `{database}`.`{locks}` VALUES (1, 0), (2, 0)"),
        &format!("CREATE TABLE `{database}`.`{filler}` (n INT)"),
    ]);
    let rows = || {
        admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`"))[0][0]
            .clone()
            .unwrap()
    };
    let mut session = connect();
    let release = |session: &mut DriverSession, sql: &str| {
        let page = run(session, sql, 0, 10).unwrap_or_else(|error| panic!("`{sql}`: {error}"));
        session.release_page(page.page_id).unwrap();
    };

    // 1. `COMMIT; START TRANSACTION; <failing statement>`: the script ended with a
    //    transaction open, whatever order its statement text suggests.
    release(&mut session, "START TRANSACTION");
    let failing = format!("COMMIT; START TRANSACTION; INSERT INTO `{table}_missing` VALUES (1)");
    run(&mut session, &failing, 0, 10).expect_err("the last statement fails");
    apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 1})),
    )
    .unwrap();
    assert_eq!(rows(), "0", "the mutation joined the open transaction");
    release(&mut session, "ROLLBACK");
    assert_eq!(rows(), "0", "the user's ROLLBACK undid the mutation");

    // 2. A deadlock rolls the user's transaction back: the session is no longer
    //    in one, and a mutation must commit on its own (not try to join).
    release(&mut session, "START TRANSACTION");
    release(
        &mut session,
        &format!("UPDATE `{locks}` SET v = 1 WHERE id = 1"),
    );
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (locks_b, filler_b, database_b) = (locks.clone(), filler.clone(), database.clone());
    let other = std::thread::spawn(move || {
        admin_runtime().block_on(async {
            let mut conn = mysql_async::Conn::new(admin_opts()).await.expect("connect");
            conn.query_drop("START TRANSACTION").await.unwrap();
            conn.query_drop(format!(
                "UPDATE `{database_b}`.`{locks_b}` SET v = 2 WHERE id = 2"
            ))
            .await
            .unwrap();
            // A heavy transaction, so InnoDB picks the other one as the victim.
            conn.query_drop(format!(
                "INSERT INTO `{database_b}`.`{filler_b}` SELECT ORDINAL_POSITION \
                 FROM information_schema.COLUMNS LIMIT 1000"
            ))
            .await
            .unwrap();
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            let _ = conn
                .query_drop(format!(
                    "UPDATE `{database_b}`.`{locks_b}` SET v = 2 WHERE id = 1"
                ))
                .await;
            let _ = conn.query_drop("COMMIT").await;
            let _ = conn.disconnect().await;
        });
    });
    ready_rx.recv().unwrap();
    let operation = session.begin_operation().unwrap();
    let runner = session.query_runner().unwrap();
    let request = query_request(
        operation.id(),
        &format!("UPDATE `{locks}` SET v = 3 WHERE id = 2"),
        Some(&database),
        0,
        10,
    );
    let victim = std::thread::spawn(move || {
        let _operation = operation;
        runner.open_query(&request)
    });
    std::thread::sleep(Duration::from_millis(600));
    go_tx.send(()).unwrap();
    let outcome = victim.join().unwrap();
    other.join().unwrap();
    let (_, message) = rejected(outcome.expect_err("this transaction is the deadlock victim"));
    assert!(message.contains("1213"), "{message}");
    // The mutation commits by itself: visible to others at once, no savepoint.
    apply(
        &mut session,
        &table,
        change(Insert, json!(null), json!({"id": 2})),
    )
    .unwrap();
    assert_eq!(
        rows(),
        "1",
        "outside a transaction the mutation commits itself"
    );
    // And the connection is healthy.
    assert_eq!(ids(&fetch(&mut session, "SELECT 5", 0, 10).batch, 0), [5]);
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn paging_reads_sql_with_the_sessions_sql_mode() {
    let mut session = connect();
    let mode = |session: &mut DriverSession, mode: &str| {
        let sql =
            format!("SET SESSION sql_mode = CONCAT_WS(',', NULLIF(@@sql_mode, ''), '{mode}')");
        let page = run(session, &sql, 0, 10).unwrap();
        session.release_page(page.page_id).unwrap();
    };
    // ANSI_QUOTES: `"a\"` is an identifier (no escapes inside it), so the LIMIT
    // after it is the statement's own. Reading it as a string hid that LIMIT and
    // appended a second one (a syntax error).
    mode(&mut session, "ANSI_QUOTES");
    let fetched = fetch(&mut session, "SELECT 1 AS \"a\\\" LIMIT 5", 0, 10);
    assert_eq!(fetched.page.row_count, 1);
    assert_eq!(fetched.page.columns[0].name, "a\\");
    // NO_BACKSLASH_ESCAPES: the backslash ends the string.
    mode(&mut session, "NO_BACKSLASH_ESCAPES");
    let fetched = fetch(&mut session, "SELECT 'a\\' AS x LIMIT 5", 0, 10);
    assert_eq!(fetched.page.row_count, 1);
    // The paged form (no LIMIT of its own) works in both modes too.
    let fetched = fetch(&mut session, "SELECT 'a\\' AS x", 0, 10);
    assert_eq!(fetched.page.row_count, 1);
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn an_unknown_transaction_state_fails_the_mutation_and_starts_nothing() {
    let database = database_name();
    let table = unique("unk");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY)"
    )]);
    let mut session = connect();
    let id = thread_id(&mut session);
    let page = run(&mut session, "START TRANSACTION", 0, 10).unwrap();
    session.release_page(page.page_id).unwrap();
    let page = run(
        &mut session,
        &format!("INSERT INTO `{table}` VALUES (1)"),
        0,
        10,
    )
    .unwrap();
    session.release_page(page.page_id).unwrap();
    // A failing statement leaves the driver without a fresh server answer, so the
    // mutation has to ask (`DO 0`) ...
    run(
        &mut session,
        &format!("INSERT INTO `{table}_missing` VALUES (1)"),
        0,
        10,
    )
    .expect_err("the table does not exist");
    // ... and the connection dies before it can.
    admin_exec(&[&format!("KILL {id}")]);
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(SqlRowOp::Insert, json!(null), json!({"id": 2})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.not_connected");
    // The lost connection is closed for good, and nothing was written or committed.
    let (code, _) = rejected(run(&mut session, "SELECT 1", 0, 10).unwrap_err());
    assert_eq!(code, "query.not_connected");
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(SqlRowOp::Insert, json!(null), json!({"id": 3})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.not_connected");
    assert_eq!(
        admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`")),
        [[Some("0".into())]]
    );
    let _ = session.disconnect();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn re_pages_of_a_read_run_read_only_so_hidden_writes_do_not_repeat() {
    struct DropFunction(String);
    impl Drop for DropFunction {
        fn drop(&mut self) {
            let _ = std::panic::catch_unwind(|| {
                admin_exec(&[&format!("DROP FUNCTION IF EXISTS {}", self.0)])
            });
        }
    }
    let database = database_name();
    let log = unique("rolog");
    let function = format!("`{database}`.`{}`", unique("bump"));
    let _cleanup = Cleanup(vec![log.clone()]);
    let _drop_function = DropFunction(function.clone());
    // (DETERMINISTIC only satisfies MySQL's binary-log check for stored functions;
    // the function writes, which is the point.)
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{log}` (id INT AUTO_INCREMENT PRIMARY KEY)"),
        &format!("DROP FUNCTION IF EXISTS {function}"),
        &format!(
            "CREATE FUNCTION {function}() RETURNS INT DETERMINISTIC MODIFIES SQL DATA \
             BEGIN INSERT INTO `{database}`.`{log}` VALUES (NULL); RETURN 1; END"
        ),
    ]);
    let logged = || {
        admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{log}`"))[0][0]
            .clone()
            .unwrap()
    };
    let read = format!(
        "SELECT {function}() AS x FROM (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4) t"
    );
    let mut session = connect();
    // The first page runs as the user wrote it (the function does its write).
    let page = run(&mut session, &read, 0, 2).unwrap();
    session.release_page(page.page_id).unwrap();
    let after_first = logged();
    assert_ne!(after_first, "0", "the function ran at offset 0");
    // A re-page runs read-only: the hidden write fails instead of repeating.
    let (code, message) = rejected(run(&mut session, &read, 2, 2).unwrap_err());
    assert_eq!(code, "query.failed");
    assert!(message.contains("1792"), "{message}");
    assert_eq!(logged(), after_first, "the re-page must not have written");
    // The failed re-page left no transaction behind.
    assert_eq!(ids(&fetch(&mut session, "SELECT 7", 0, 10).batch, 0), [7]);
    // Inside a transaction of the user's the statement just runs there.
    let page = run(&mut session, "START TRANSACTION", 0, 10).unwrap();
    session.release_page(page.page_id).unwrap();
    let page = run(&mut session, &read, 2, 2).unwrap();
    session.release_page(page.page_id).unwrap();
    let page = run(&mut session, "ROLLBACK", 0, 10).unwrap();
    session.release_page(page.page_id).unwrap();
    assert_eq!(logged(), after_first, "the user's ROLLBACK undid it");
    // Plain re-pages are unaffected.
    let fetched = fetch(
        &mut session,
        "SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3",
        1,
        1,
    );
    assert_eq!(ids(&fetched.batch, 0), [2]);
    assert!(fetched.page.flags.truncated);
    session.disconnect().unwrap();
}

/// `payload.read_only` request for `sql`.
fn run_read_only(session: &mut DriverSession, sql: &str) -> Result<QueryPage, LoaderError> {
    let operation = session.begin_operation().expect("operation");
    session.open_query(&read_request(operation.id(), sql, true))
}

/// A READ ONLY run is a scope the text must not be able to leave: `COMMIT` (or `SET
/// autocommit`, `START TRANSACTION`, `SET TRANSACTION READ WRITE` ...) ends it, and what
/// follows runs as a normal write. The reported probe makes a client lexer with the
/// default options see ONE statement where an `ANSI_QUOTES` server sees three.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn read_only_text_cannot_leave_its_scope_with_transaction_control() {
    let database = database_name();
    let table = format!("tablex_ro_{}_{}", CFG.driver, std::process::id());
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!("DROP TABLE IF EXISTS `{database}`.`{table}`"),
        &format!(
            "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, note VARCHAR(20)) ENGINE=InnoDB"
        ),
        &format!("INSERT INTO `{database}`.`{table}` VALUES (1, 'a'), (2, 'b'), (3, 'c')"),
    ]);
    let contents = || {
        admin_rows(&format!(
            "SELECT id, note FROM `{database}`.`{table}` ORDER BY id"
        ))
    };
    let before = contents();
    assert_eq!(before.len(), 3);
    let mut session = connect();
    let release = |session: &mut DriverSession, page: QueryPage| {
        session.release_page(page.page_id).expect("release page");
    };
    let refused = |session: &mut DriverSession, sql: &str| {
        let (code, message) = match run_read_only(session, sql) {
            Ok(_) => panic!("read-only text was accepted: {sql}"),
            Err(error) => rejected(error),
        };
        assert_eq!(code, "query.read_only_violation", "{sql}: {message}");
        assert_eq!(contents(), before, "nothing changed after: {sql}");
        message
    };
    let accepted = |session: &mut DriverSession, sql: &str| {
        let page = run_read_only(session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        session.release_page(page.page_id).expect("release page");
        assert_eq!(contents(), before, "nothing changed after: {sql}");
    };
    let t = format!("`{table}`");

    // Default sql_mode: transaction control and scripts with a non-read are refused.
    for sql in [
        format!("SELECT 1; COMMIT; DELETE FROM {t} WHERE id = 1"),
        format!("COMMIT; DELETE FROM {t} WHERE id = 1"),
        format!("SELECT 1; /*! COMMIT */; DELETE FROM {t} WHERE id = 1"),
        "/*!50000 COMMIT */".to_owned(),
        "SELECT 1; ROLLBACK".to_owned(),
        "START TRANSACTION".to_owned(),
        "BEGIN".to_owned(),
        "SET autocommit = 1".to_owned(),
        "SELECT 1; SET @@session.autocommit = 1".to_owned(),
        "SET TRANSACTION READ WRITE".to_owned(),
        "SAVEPOINT s".to_owned(),
        "SELECT 1; RELEASE SAVEPOINT s".to_owned(),
        format!("SELECT 1; DELETE FROM {t} WHERE id = 1"),
        format!("SELECT 1; SET @x = 1; DELETE FROM {t} WHERE id = 1"),
    ] {
        refused(&mut session, &sql);
    }
    // A single write is refused up front too.
    refused(&mut session, &format!("DELETE FROM {t} WHERE id = 1"));
    let message = refused(&mut session, &format!("SELECT 1; COMMIT; DELETE FROM {t}"));
    assert!(message.contains("statement 2"), "{message}");

    // Plain reads still run: one statement, EXPLAIN, a script of plain reads.
    accepted(&mut session, &format!("SELECT COUNT(*) FROM {t}"));
    accepted(
        &mut session,
        &format!("EXPLAIN SELECT * FROM {t} WHERE id = 1"),
    );
    accepted(&mut session, "SELECT 'COMMIT; DELETE FROM x'");
    accepted(&mut session, "SELECT 1 /* ; COMMIT; */");
    accepted(
        &mut session,
        &format!("SELECT 1; SELECT id FROM {t} ORDER BY id"),
    );
    let explain_analyze = if CFG.driver == "mariadb" {
        format!("ANALYZE FORMAT=JSON SELECT * FROM {t}")
    } else {
        format!("EXPLAIN ANALYZE SELECT * FROM {t}")
    };
    accepted(&mut session, &explain_analyze);
    // The refusals left no transaction behind: the session is clean.
    assert_eq!(ids(&fetch(&mut session, "SELECT 7", 0, 10).batch, 0), [7]);

    // The reported probe, in a session whose sql_mode has ANSI_QUOTES: one statement to
    // a lexer with the default options, three to the server.
    let page = run(
        &mut session,
        "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',ANSI_QUOTES')",
        0,
        10,
    )
    .unwrap();
    release(&mut session, page);
    let probe = format!(
        r#"SELECT "a\" FROM (SELECT 1 AS "a\") x; COMMIT; DELETE FROM {t} WHERE id=1; -- ""#
    );
    refused(&mut session, &probe);
    accepted(
        &mut session,
        &format!(r#"SELECT "id" FROM {t} ORDER BY "id""#),
    );
    // NO_BACKSLASH_ESCAPES: `'\'` is a whole string, the rest is real SQL.
    let page = run(
        &mut session,
        "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',NO_BACKSLASH_ESCAPES')",
        0,
        10,
    )
    .unwrap();
    release(&mut session, page);
    refused(
        &mut session,
        &format!(r"SELECT '\'; COMMIT; DELETE FROM {t} WHERE id = 1"),
    );

    // Control: the same probe run without `read_only` really does delete the row, so the
    // refusals above are what protected it.
    let page = run(
        &mut session,
        "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',ANSI_QUOTES')",
        0,
        10,
    )
    .unwrap();
    release(&mut session, page);
    let page = run(&mut session, &probe, 0, 10).expect("unprotected probe");
    release(&mut session, page);
    assert_eq!(
        contents().len(),
        2,
        "without read_only the probe deleted a row"
    );
    session.disconnect().unwrap();
}

/// `START TRANSACTION READ ONLY` implicitly COMMITs a transaction that is already open, so a
/// READ ONLY run on a session with pending writes (`SET autocommit = 0`, `BEGIN`) is refused
/// before anything is sent: the pending row stays pending and a `ROLLBACK` still removes it.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn read_only_run_is_refused_instead_of_committing_an_open_transaction() {
    let database = database_name();
    let table = unique("ro_open");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!("DROP TABLE IF EXISTS `{database}`.`{table}`"),
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"),
    ]);
    let committed = || {
        admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`"))[0][0]
            .clone()
            .unwrap()
    };
    let mut session = connect();
    for opener in ["SET autocommit = 0", "BEGIN"] {
        let page = run(&mut session, opener, 0, 10).unwrap();
        session.release_page(page.page_id).unwrap();
        let page = run(
            &mut session,
            &format!("INSERT INTO `{table}` VALUES (1)"),
            0,
            10,
        )
        .unwrap();
        session.release_page(page.page_id).unwrap();
        let (code, message) = rejected(run_read_only(&mut session, "SELECT 1").unwrap_err());
        assert_eq!(code, "query.read_only_unavailable", "{opener}: {message}");
        assert!(message.contains("transaction is open"), "{message}");
        assert_eq!(committed(), "0", "{opener}: the refused run committed");
        let page = run(&mut session, "ROLLBACK", 0, 10).unwrap();
        session.release_page(page.page_id).unwrap();
        assert_eq!(committed(), "0", "{opener}: the row survived the ROLLBACK");
        // Nothing pending any more: the same run goes through.
        let page = run_read_only(&mut session, "SELECT 1").expect("read only after ROLLBACK");
        session.release_page(page.page_id).unwrap();
    }
    session.disconnect().unwrap();
}

/// A versioned comment (`/*!99999 ... */`) is code only to a server at or above its version;
/// to an older one it is a comment closed by the first `*/`. The classifier must read it
/// with the SERVER's version, or `SELECT 1 /*!99999 ' */ ; COMMIT; DELETE ...; -- '` is one
/// statement to it and three to the server. Also what `CALL` and admin statements may do.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn read_only_text_is_lexed_with_the_servers_version_and_admin_statements_are_refused() {
    struct DropProcedure(String);
    impl Drop for DropProcedure {
        fn drop(&mut self) {
            let _ = std::panic::catch_unwind(|| {
                admin_exec(&[&format!("DROP PROCEDURE IF EXISTS {}", self.0)])
            });
        }
    }
    let database = database_name();
    let table = format!("tablex_ro_ver_{}_{}", CFG.driver, std::process::id());
    let procedure = format!(
        "`{database}`.`tablex_ro_proc_{}_{}`",
        CFG.driver,
        std::process::id()
    );
    let _cleanup = Cleanup(vec![table.clone()]);
    let _drop_procedure = DropProcedure(procedure.clone());
    let t = format!("`{table}`");
    admin_exec(&[
        &format!("DROP TABLE IF EXISTS `{database}`.{t}"),
        &format!("CREATE TABLE `{database}`.{t} (id INT PRIMARY KEY) ENGINE=InnoDB"),
        &format!("INSERT INTO `{database}`.{t} VALUES (1), (2), (3)"),
        &format!("DROP PROCEDURE IF EXISTS {procedure}"),
        &format!(
            "CREATE PROCEDURE {procedure}() MODIFIES SQL DATA \
             BEGIN COMMIT; DELETE FROM `{database}`.{t} WHERE id = 3; END"
        ),
    ]);
    let ids_now = || {
        admin_rows(&format!("SELECT id FROM `{database}`.{t} ORDER BY id"))
            .into_iter()
            .map(|row| row[0].clone().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids_now(), ["1", "2", "3"]);
    // The ids the table must hold; a control run that deletes updates it.
    let expected = std::cell::RefCell::new(vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]);
    let mut session = connect();
    let refused = |session: &mut DriverSession, sql: &str| {
        let (code, message) = match run_read_only(session, sql) {
            Ok(_) => panic!("read-only text was accepted: {sql}"),
            Err(error) => rejected(error),
        };
        assert_eq!(code, "query.read_only_violation", "{sql}: {message}");
        assert_eq!(
            ids_now(),
            *expected.borrow(),
            "nothing changed after: {sql}"
        );
        message
    };
    // Written as MySQL and MariaDB each read it: a comment above the server's version.
    let hidden = format!("SELECT 1 /*!99999 ' */ ; COMMIT; DELETE FROM {t} WHERE id = 1; -- '");
    let hidden_maria =
        format!("SELECT 1 /*M!999999 ' */ ; COMMIT; DELETE FROM {t} WHERE id = 1; -- '");
    for sql in [&hidden, &hidden_maria] {
        let message = refused(&mut session, sql);
        assert!(message.contains("statement 2"), "{message}");
    }
    // Run at or below the server's version, a versioned comment is code.
    refused(&mut session, "/*!50000 COMMIT */");
    refused(
        &mut session,
        &format!("SELECT 1 /*!50000 ; DELETE FROM {t} WHERE id = 1 */"),
    );
    // Above it, it is a comment: what follows is the read the server runs.
    for sql in [
        format!("SELECT id FROM {t} /*!99999 ; DELETE FROM {t} WHERE id = 1 */ ORDER BY id"),
        "SELECT 1 /*M!999999 ; COMMIT */".to_owned(),
    ] {
        let page =
            run_read_only(&mut session, &sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        session.release_page(page.page_id).unwrap();
        assert_eq!(ids_now(), ["1", "2", "3"]);
    }
    // For a normal Run the hidden DELETE (no WHERE) needs confirmation.
    let unconfirmed = format!("SELECT 1 /*!99999 ' */ ; DELETE FROM {t}; -- '");
    let (code, message) = rejected(run(&mut session, &unconfirmed, 0, 10).unwrap_err());
    assert_eq!(code, "query.confirmation_required", "{message}");
    assert_eq!(ids_now(), ["1", "2", "3"]);
    // Control: without `read_only` the server really does read the probe as three
    // statements, so the refusals above are what protected the row.
    let page = run(&mut session, &hidden, 0, 10).expect("unprotected probe");
    session.release_page(page.page_id).unwrap();
    assert_eq!(
        ids_now(),
        ["2", "3"],
        "without read_only the probe deleted a row"
    );
    *expected.borrow_mut() = vec!["2".to_owned(), "3".to_owned()];

    // A procedure may COMMIT and carry on outside the READ ONLY transaction; admin
    // statements need no transaction at all. None of them runs.
    let call = format!("CALL {procedure}()");
    refused(&mut session, &call);
    refused(&mut session, "KILL QUERY 0");
    refused(&mut session, "FLUSH TABLES");
    refused(&mut session, "SET GLOBAL sort_buffer_size = 262144");
    refused(&mut session, "SET @@global.sort_buffer_size = 262144");
    refused(&mut session, "EXECUTE nothing_prepared");
    // Control: the procedure really deletes when it is allowed to run.
    let page = run(&mut session, &call, 0, 10).expect("unprotected CALL");
    session.release_page(page.page_id).unwrap();
    assert_eq!(ids_now(), ["2"], "the procedure ran without read_only");
    *expected.borrow_mut() = vec!["2".to_owned()];

    // Two versioned comments: the first (90000) is a comment on both servers, the second
    // (50000) is code, so the server runs `SELECT 1 ; COMMIT; DELETE ...`. A reading that
    // takes the first as code swallows the rest inside a string and sees one statement.
    admin_exec(&[&format!("INSERT INTO `{database}`.{t} VALUES (4)")]);
    *expected.borrow_mut() = vec!["2".to_owned(), "4".to_owned()];
    let two =
        format!("SELECT 1 /*!90000 ' */ /*!50000 */ ; COMMIT; DELETE FROM {t} WHERE id = 4; -- '");
    let message = refused(&mut session, &two);
    assert!(message.contains("statement 2"), "{message}");
    // Seven digits or more are read differently by MySQL and MariaDB: refused as ambiguous.
    let seven = format!(
        "SELECT 1 /*!9999999 ' */ /*!0000000 */ ; COMMIT; DELETE FROM {t} WHERE id = 4; -- '"
    );
    let message = refused(&mut session, &seven);
    assert!(message.contains("versioned comment"), "{message}");
    // Control: without `read_only` the server runs the two-comment probe's DELETE.
    let page = run(&mut session, &two, 0, 10).expect("unprotected two-comment probe");
    session.release_page(page.page_id).unwrap();
    assert_eq!(
        ids_now(),
        ["2"],
        "without read_only the probe deleted the row"
    );
    *expected.borrow_mut() = vec!["2".to_owned()];

    // `INTO OUTFILE` needs no transaction, and a READ ONLY one does not stop it. Measured in
    // the test containers (root, `/var/lib/mysql-files` on MySQL 8.4.11, `/tmp` on MariaDB
    // 11.8.9): MySQL's `EXPLAIN ANALYZE SELECT 1 INTO OUTFILE` writes NO file, MariaDB's
    // `ANALYZE SELECT 1 INTO OUTFILE` and `ANALYZE FORMAT=JSON ... INTO DUMPFILE` DO. Both
    // are refused regardless, so the file never appears (`LOAD_FILE` is NULL).
    let dir = admin_rows("SELECT IFNULL(@@secure_file_priv, '')")[0][0]
        .clone()
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| "/tmp/".to_owned());
    let outfile = format!("{dir}tablex_ro_{}_{}.out", CFG.driver, std::process::id());
    let dumpfile = format!("{dir}tablex_ro_{}_{}.dump", CFG.driver, std::process::id());
    for sql in [
        format!("EXPLAIN ANALYZE SELECT 1 INTO OUTFILE '{outfile}'"),
        format!("EXPLAIN SELECT 1 INTO OUTFILE '{outfile}'"),
        format!("ANALYZE SELECT 1 INTO OUTFILE '{outfile}'"),
        format!("ANALYZE FORMAT=JSON SELECT 1 INTO DUMPFILE '{dumpfile}'"),
        format!("EXPLAIN ANALYZE SELECT 1 INTO DUMPFILE '{dumpfile}'"),
        format!("SELECT 1 INTO OUTFILE '{outfile}'"),
    ] {
        refused(&mut session, &sql);
        for file in [&outfile, &dumpfile] {
            let loaded = admin_rows(&format!("SELECT LOAD_FILE('{file}') IS NULL"));
            assert_eq!(
                loaded[0][0].as_deref(),
                Some("1"),
                "{file} exists after: {sql}"
            );
        }
    }
    if CFG.driver == "mariadb" {
        // `sql_mode = MSSQL`: `[x' ]` is one quoted identifier to the server, a string that
        // never ends to a lexer that does not know brackets (it would see one statement).
        let page = run(&mut session, "SET SESSION sql_mode = 'MSSQL'", 0, 10).unwrap();
        session.release_page(page.page_id).unwrap();
        let bracket = format!("SELECT 1 AS [x' ] ; COMMIT; DELETE FROM {t} WHERE id = 2; -- '");
        let message = refused(&mut session, &bracket);
        assert!(message.contains("statement 2"), "{message}");
        // `;` and `]]` inside brackets do not split: this is one read.
        let page = run_read_only(&mut session, "SELECT 1 AS [a; COMMIT; b]]c]").unwrap();
        session.release_page(page.page_id).unwrap();
        let page = run(&mut session, &bracket, 0, 10).expect("unprotected probe");
        session.release_page(page.page_id).unwrap();
        assert!(
            ids_now().is_empty(),
            "without read_only the probe deleted the row"
        );
    }
    session.disconnect().unwrap();
}

// ------------------------------------------------------------------------ TLS
//
// Env-gated like the rest. `rust-engine/scripts/test-tls-servers.sh start` runs the TLS servers (this
// driver's `tablex-test-{mysql,mariadb}-tls`, and `-notls` for the `PREFERRED` fallback); `source` its env
// file. The existing plain-credential server (`CFG.uri_env`) auto-generates a self-signed certificate, so it
// always offers TLS.

struct TlsServer {
    port: String,
    user: String,
    password: String,
    db: String,
    x509_user: String,
    ca: String,
    other_ca: String,
    name: String,
    /// (certificate, key) in PKCS#8, PKCS#1 and SEC1 encodings.
    client: [(String, String); 3],
    encrypted_key: String,
}

fn tls_server() -> Option<TlsServer> {
    let prefix = format!("TABLEX_TEST_{}_TLS_", CFG.driver.to_uppercase());
    let Ok(port) = std::env::var(format!("{prefix}PORT")) else {
        eprintln!("skipped: {prefix}PORT is not set (test-tls-servers.sh)");
        return None;
    };
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let own = |name: &str| var(&format!("{prefix}{name}"));
    Some(TlsServer {
        port,
        user: own("USER"),
        password: own("PASSWORD"),
        db: own("DB"),
        x509_user: own("X509_USER"),
        ca: var("TABLEX_TEST_TLS_CA"),
        other_ca: var("TABLEX_TEST_TLS_OTHER_CA"),
        name: var("TABLEX_TEST_TLS_SERVER_NAME"),
        client: [
            (
                var("TABLEX_TEST_TLS_CLIENT_CERT"),
                var("TABLEX_TEST_TLS_CLIENT_KEY"),
            ),
            (
                var("TABLEX_TEST_TLS_CLIENT_CERT_RSA"),
                var("TABLEX_TEST_TLS_CLIENT_KEY_RSA"),
            ),
            (
                var("TABLEX_TEST_TLS_CLIENT_CERT_EC"),
                var("TABLEX_TEST_TLS_CLIENT_KEY_EC"),
            ),
        ],
        encrypted_key: var("TABLEX_TEST_TLS_CLIENT_KEY_ENCRYPTED"),
    })
}

impl TlsServer {
    fn uri_for(&self, user: &str, host: &str, query: &str) -> String {
        format!(
            "{}://{user}:{}@{host}:{}/{}?{query}",
            CFG.scheme, self.password, self.port, self.db
        )
    }

    fn uri(&self, host: &str, query: &str) -> String {
        self.uri_for(&self.user, host, query)
    }
}

/// `uri_env`'s server with `query` appended.
fn plain_uri_with(query: &str) -> String {
    let base = uri();
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}{query}")
}

fn notls_uri() -> Option<String> {
    let name = format!("TABLEX_TEST_{}_NOTLS_URI", CFG.driver.to_uppercase());
    let uri = std::env::var(&name).ok();
    if uri.is_none() {
        eprintln!("skipped: {name} is not set (test-tls-servers.sh)");
    }
    uri
}

/// The cipher the SERVER reports for this session; empty when it is not encrypted.
fn tls_cipher(session: &mut DriverSession) -> String {
    let fetched = fetch(session, "SHOW SESSION STATUS LIKE 'Ssl_cipher'", 0, 10);
    assert_eq!(fetched.page.row_count, 1);
    fetched
        .batch
        .column(1)
        .as_string::<i32>()
        .value(0)
        .to_owned()
}

/// The `(code, message)` the driver refuses `uri` with.
fn refused(uri: &str) -> (String, String) {
    match connect_with(uri) {
        Err(error) => rejected(error),
        Ok(_) => panic!("the driver connected"),
    }
}

/// Connects; `Ok(encrypted)` or the driver's error code.
fn connect_outcome(uri: &str) -> Result<bool, String> {
    match connect_with(uri) {
        Ok(mut session) => {
            let encrypted = !tls_cipher(&mut session).is_empty();
            session.disconnect().unwrap();
            Ok(encrypted)
        }
        Err(error) => Err(rejected(error).0),
    }
}

#[test]
#[ignore = "needs test-tls-servers.sh (TABLEX_TEST_*_TLS_*)"]
fn tls_ssl_mode_matrix_against_the_tls_server() {
    let Some(t) = tls_server() else { return };
    let (ca, other, name) = (&t.ca, &t.other_ca, &t.name);
    // (host, query, expected): Ok(true) = connected and encrypted, Err(code) = refused with that code.
    let cases: Vec<(&str, String, Result<bool, &str>)> = vec![
        (
            "127.0.0.1",
            "ssl-mode=DISABLED".into(),
            Err("connect.tls_required"),
        ),
        (
            "127.0.0.1",
            "sslmode=disable".into(),
            Err("connect.tls_required"),
        ),
        ("127.0.0.1", "sslmode=allow".into(), Ok(true)),
        ("127.0.0.1", "ssl-mode=PREFERRED".into(), Ok(true)),
        ("127.0.0.1", String::new(), Ok(true)), // default: PREFERRED
        ("127.0.0.1", "ssl-mode=REQUIRED".into(), Ok(true)),
        (
            "127.0.0.1",
            format!("ssl-mode=REQUIRED&ssl-ca={ca}"),
            Ok(true),
        ),
        (
            "127.0.0.1",
            format!("ssl-mode=REQUIRED&ssl-ca={other}"),
            Err("connect.tls_certificate"),
        ),
        // PREFERRED never downgrades after a certificate failure.
        (
            "127.0.0.1",
            format!("ssl-mode=PREFERRED&ssl-ca={other}"),
            Err("connect.tls_certificate"),
        ),
        // VERIFY_CA: right chain, any name.
        (
            "127.0.0.1",
            format!("ssl-mode=VERIFY_CA&ssl-ca={ca}"),
            Ok(true),
        ),
        (
            "127.0.0.1",
            format!("ssl-mode=VERIFY_CA&ssl-ca={other}"),
            Err("connect.tls_certificate"),
        ),
        // VERIFY_CA never uses public roots: it needs the CA file (B1).
        (
            "127.0.0.1",
            "ssl-mode=VERIFY_CA".into(),
            Err("connect.tls_config"),
        ),
        // N8: the tunnel's `tls-server-name` is only a hint under VERIFY_CA.
        (
            "127.0.0.1",
            format!("ssl-mode=VERIFY_CA&ssl-ca={ca}&tls-server-name=wrong.tablex.test"),
            Ok(true),
        ),
        // Neither the built-in roots nor the OS store know the throwaway CA.
        (
            "localhost",
            "ssl-mode=VERIFY_IDENTITY".into(),
            Err("connect.tls_certificate"),
        ),
        // VERIFY_IDENTITY: chain and name.
        (
            "localhost",
            format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}"),
            Ok(true),
        ),
        (
            "127.0.0.1",
            format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}"),
            Err("connect.tls_certificate"),
        ),
        (
            "localhost",
            format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={other}"),
            Err("connect.tls_certificate"),
        ),
        // The tunnel hook: socket to 127.0.0.1, certificate checked for the real name.
        (
            "127.0.0.1",
            format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}&tls-server-name={name}"),
            Ok(true),
        ),
        (
            "127.0.0.1",
            format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}&tls-server-name=wrong.tablex.test"),
            Err("connect.tls_certificate"),
        ),
        // mysql_async's own switches keep working.
        (
            "127.0.0.1",
            "require_ssl=true&verify_ca=false".into(),
            Ok(true),
        ),
        (
            "127.0.0.1",
            "require_ssl=true".into(),
            Err("connect.tls_certificate"),
        ),
        // ... except `verify_identity=false` without a CA file: VERIFY_CA against public roots.
        (
            "127.0.0.1",
            "require_ssl=true&verify_identity=false".into(),
            Err("connect.tls_config"),
        ),
        // Configuration errors.
        (
            "127.0.0.1",
            "ssl-mode=MAYBE".into(),
            Err("connect.tls_config"),
        ),
        (
            "127.0.0.1",
            "ssl-mode=VERIFY_CA&ssl-ca=/nonexistent/ca.pem".into(),
            Err("connect.tls_config"),
        ),
    ];
    for (host, query, expected) in cases {
        let outcome = connect_outcome(&t.uri(host, &query));
        let expected = expected.map_err(str::to_owned);
        if outcome != expected && outcome.is_err() {
            eprintln!("refused with: {}", refused(&t.uri(host, &query)).1);
        }
        assert_eq!(outcome, expected, "host={host} {query}");
    }
}

#[test]
#[ignore = "needs test-tls-servers.sh (TABLEX_TEST_*_TLS_*)"]
fn tls_certificate_errors_name_their_reason() {
    let Some(t) = tls_server() else { return };
    let message = |host: &str, query: &str| match connect_with(&t.uri(host, query)) {
        Err(error) => rejected(error).1,
        Ok(_) => panic!("{host} {query} connected"),
    };
    let unknown = message(
        "localhost",
        &format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={}", t.other_ca),
    );
    assert!(
        unknown.contains("unknown issuer") && unknown.contains("ssl-ca"),
        "{unknown}"
    );
    let mismatch = message(
        "127.0.0.1",
        &format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={}", t.ca),
    );
    assert!(
        mismatch.contains("name mismatch") && mismatch.contains("tls-server-name"),
        "{mismatch}"
    );
    let disabled = message("127.0.0.1", "ssl-mode=DISABLED");
    assert!(
        disabled.contains("requires TLS") && disabled.contains("DISABLED"),
        "{disabled}"
    );
    for text in [&unknown, &mismatch, &disabled] {
        assert!(!text.contains(&t.password), "{text}");
    }
}

#[test]
#[ignore = "needs test-tls-servers.sh (TABLEX_TEST_*_TLS_*)"]
fn tls_ssl_mode_matrix_against_servers_with_and_without_tls() {
    let Some(t) = tls_server() else { return };
    let Some(notls) = notls_uri() else { return };
    let with = |base: &str, query: &str| {
        let separator = if base.contains('?') { '&' } else { '?' };
        format!("{base}{separator}{query}")
    };
    // The existing server offers TLS with a self-signed certificate.
    let existing = |query: &str| connect_outcome(&plain_uri_with(query));
    assert_eq!(existing("ssl-mode=DISABLED"), Ok(false));
    assert_eq!(
        existing("ssl-mode=PREFERRED"),
        Ok(true),
        "PREFERRED upgrades when TLS is offered"
    );
    assert_eq!(existing(""), Ok(true));
    assert_eq!(existing("ssl-mode=REQUIRED"), Ok(true));
    assert_eq!(
        existing("ssl-mode=VERIFY_IDENTITY"),
        Err("connect.tls_certificate".to_owned())
    );
    // `allow` with a wrong password reports the password error, not a TLS one.
    let wrong = |base: &str| {
        let uri = with(
            &base.replacen("://tablex:", "://tablex:wrong-", 1),
            "sslmode=allow",
        );
        refused(&uri).1
    };
    for base in [uri(), notls.clone()] {
        let message = wrong(&base);
        assert!(message.contains("Access denied"), "{message}");
    }
    // A server without TLS: PREFERRED (and allow) fall back to clear text, nothing stricter does.
    let plain = |query: &str| connect_outcome(&with(&notls, query));
    assert_eq!(plain("ssl-mode=DISABLED"), Ok(false));
    assert_eq!(
        plain("ssl-mode=PREFERRED"),
        Ok(false),
        "falls back only because the server offers no TLS"
    );
    assert_eq!(plain(""), Ok(false));
    assert_eq!(plain("sslmode=allow"), Ok(false));
    for mode in [
        "REQUIRED".to_owned(),
        format!("VERIFY_CA&ssl-ca={}", t.ca),
        "VERIFY_IDENTITY".to_owned(),
    ] {
        assert_eq!(
            plain(&format!("ssl-mode={mode}")),
            Err("connect.tls_required".to_owned()),
            "{mode}"
        );
    }
    assert_eq!(
        plain("require_ssl=true"),
        Err("connect.tls_required".to_owned())
    );
}

#[test]
#[ignore = "needs test-tls-servers.sh (TABLEX_TEST_*_TLS_*)"]
fn tls_client_certificate_authentication() {
    let Some(t) = tls_server() else { return };
    for (cert, key) in &t.client {
        let uri = t.uri_for(
            &t.x509_user,
            "localhost",
            &format!(
                "ssl-mode=VERIFY_IDENTITY&ssl-ca={}&ssl-cert={cert}&ssl-key={key}",
                t.ca
            ),
        );
        let mut session = connect_with(&uri).unwrap_or_else(|e| panic!("{key}: {e}"));
        let fetched = fetch(&mut session, "SELECT CURRENT_USER()", 0, 1);
        let user = fetched
            .batch
            .column(0)
            .as_string::<i32>()
            .value(0)
            .to_owned();
        assert!(user.starts_with(&t.x509_user), "{user}");
        assert!(!tls_cipher(&mut session).is_empty());
        session.disconnect().unwrap();
    }
    // REQUIRE X509 without a certificate: the server refuses.
    let anonymous = t.uri_for(
        &t.x509_user,
        "localhost",
        &format!("ssl-mode=REQUIRED&ssl-ca={}", t.ca),
    );
    assert!(connect_with(&anonymous).is_err());
    // A password-protected key is a typed "not supported" and does not echo anything.
    let encrypted = t.uri_for(
        &t.x509_user,
        "localhost",
        &format!(
            "ssl-mode=REQUIRED&ssl-cert={}&ssl-key={}",
            t.client[0].0, t.encrypted_key
        ),
    );
    let (code, message) = refused(&encrypted);
    assert_eq!(code, "connect.tls_config");
    assert!(message.contains("password protected"), "{message}");
    // A certificate and a key that do not belong together.
    let mismatched = t.uri_for(
        &t.x509_user,
        "localhost",
        &format!(
            "ssl-mode=REQUIRED&ssl-cert={}&ssl-key={}",
            t.client[0].0, t.client[1].1
        ),
    );
    assert_eq!(refused(&mismatched).0, "connect.tls_config");
}

#[test]
#[ignore = "needs test-tls-servers.sh (TABLEX_TEST_*_TLS_*)"]
fn tls_session_pages_and_cancels() {
    let Some(t) = tls_server() else { return };
    // The tunnel shape: socket to 127.0.0.1, certificate verified for the real name.
    let uri = t.uri(
        "127.0.0.1",
        &format!(
            "ssl-mode=VERIFY_IDENTITY&ssl-ca={}&tls-server-name={}",
            t.ca, t.name
        ),
    );
    let mut session = connect_with(&uri).expect("connect over TLS");
    assert!(!tls_cipher(&mut session).is_empty());
    let table = unique("tlspage");
    let setup = run_with(
        &mut session,
        &format!(
            "CREATE TABLE `{table}` AS SELECT CAST(ROW_NUMBER() OVER () AS SIGNED) AS id, x.COLUMN_NAME AS name \
             FROM information_schema.COLUMNS x CROSS JOIN information_schema.COLUMNS y LIMIT 25000"
        ),
        0,
        10,
        true,
    )
    .expect("create table");
    session.release_page(setup.page_id).unwrap();
    // First, a middle and the last partial page.
    for (offset, limit, first, rows) in [
        (0, 1000, 1, 1000),
        (12_000, 1000, 12_001, 1000),
        (24_500, 1000, 24_501, 500),
    ] {
        let fetched = fetch(
            &mut session,
            &format!("SELECT id, name FROM `{table}` ORDER BY id"),
            offset,
            limit,
        );
        assert_eq!(fetched.page.row_count, rows, "offset {offset}");
        assert_eq!(ids(&fetched.batch, 0).first(), Some(&first));
    }
    // Navigator (the catalog connection is a second TLS connection).
    let listing = nav(&mut session, None, None, None, 50);
    assert!(!listing.nodes.is_empty());

    // Cancel: the server refuses clear text, so a `KILL QUERY` that works was sent over TLS,
    // and the server's own handshake counter says so too.
    let handshakes = |session: &mut DriverSession| -> u64 {
        let fetched = fetch(
            session,
            "SHOW GLOBAL STATUS LIKE 'Ssl_finished_accepts'",
            0,
            10,
        );
        fetched
            .batch
            .column(1)
            .as_string::<i32>()
            .value(0)
            .parse()
            .expect("counter")
    };
    let before = handshakes(&mut session);
    let connection_id = session.connection_id();
    let operation = session.begin_operation().unwrap();
    let runner: QueryRunner = session.query_runner().unwrap();
    let request = query_request(operation.id(), "SELECT SLEEP(20)", Some(&t.db), 0, 10);
    let started = Instant::now();
    let worker = std::thread::spawn(move || runner.open_query(&request));
    std::thread::sleep(Duration::from_millis(600));
    env()
        .handle
        .cancel_operation(
            connection_id,
            &CancelRequest {
                operation_id: operation.id(),
            },
        )
        .expect("cancel_operation");
    let (code, _) = rejected(worker.join().unwrap().expect_err("cancelled"));
    assert_eq!(code, "query.cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    drop(operation);
    assert_eq!(ids(&fetch(&mut session, "SELECT 5", 0, 10).batch, 0), [5]);
    let after = handshakes(&mut session);
    assert!(
        after > before,
        "the KILL connection did a TLS handshake ({before} -> {after})"
    );

    let drop_page =
        run_with(&mut session, &format!("DROP TABLE `{table}`"), 0, 10, true).expect("drop");
    session.release_page(drop_page.page_id).unwrap();
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server and test-tls-servers.sh"]
fn tls_throughput_versus_plain() {
    let Some(_) = tls_server() else { return };
    let database = database_name();
    let table = unique("tlsrate");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (a INT, b VARCHAR(64), c VARCHAR(64), d VARCHAR(32), e DECIMAL(12,2))"),
        &format!(
            "INSERT INTO `{database}`.`{table}` \
             SELECT x.ORDINAL_POSITION, x.COLUMN_NAME, x.TABLE_NAME, x.DATA_TYPE, x.ORDINAL_POSITION / 3 \
             FROM information_schema.COLUMNS x CROSS JOIN (SELECT 1 FROM information_schema.COLUMNS LIMIT 200) y LIMIT 100000"
        ),
    ]);
    // Same (TLS-capable) server, clear text vs TLS: best of 5 after a warm-up.
    let measure = |query: &str| {
        let mut session = connect_with(&plain_uri_with(query)).expect("connect");
        let sql = format!("SELECT * FROM `{table}`");
        let _ = fetch(&mut session, &sql, 0, 10);
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let started = Instant::now();
            let fetched = fetch(&mut session, &sql, 0, 100_000);
            best = best.min(started.elapsed());
            assert_eq!(fetched.page.row_count, 100_000);
        }
        let encrypted = !tls_cipher(&mut session).is_empty();
        session.disconnect().unwrap();
        (best, encrypted)
    };
    let (plain, plain_encrypted) = measure("ssl-mode=DISABLED");
    let (tls, tls_encrypted) = measure("ssl-mode=REQUIRED");
    assert!(!plain_encrypted && tls_encrypted);
    let rate = |t: Duration| 100_000.0 / t.as_secs_f64();
    println!(
        "THROUGHPUT {}: plain {:.0} rows/s ({plain:?}); tls {:.0} rows/s ({tls:?}); tls/plain = {:.2}",
        CFG.driver,
        rate(plain),
        rate(tls),
        rate(tls) / rate(plain)
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
    let mut request: Value = serde_json::from_str(&query_request(
        operation,
        sql,
        Some(&database_name()),
        0,
        10,
    ))
    .expect("request JSON");
    request["payload"]["read_only"] = json!(read_only);
    request["payload"]["confirmed"] = json!(true);
    request.to_string()
}

/// What one swept round did.
#[derive(Debug)]
struct Round {
    /// `ok`, or the driver's error code.
    outcome: String,
    /// The driver's message when it rejected the call.
    detail: String,
    /// From the cancel call to the return of the cancelled call.
    latency: Duration,
    /// The call was still running `CANCEL_DEADLINE` after the cancel.
    lost: bool,
}

/// Runs `work` on its own thread, cancels operation `id` after `delay` (the
/// cancel is delivered by this thread, like the host does), and waits.
fn cancel_after<T: Send>(
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
        let _ = env().handle.cancel_operation(connection_id, &cancel);
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
                    let _ = env().handle.cancel_operation(connection_id, &cancel);
                }
            }
        };
        let (outcome, detail) = match worker.join().expect("worker thread") {
            Ok(()) => ("ok".to_owned(), String::new()),
            Err(error) => rejected(error),
        };
        Round {
            outcome,
            detail,
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
            let wrong = format!(
                "round {round} (cancel after {delay:?}): {}: {}",
                result.outcome, result.detail
            );
            eprintln!("  wrong: {wrong}");
            self.wrong.push(wrong);
        }
    }

    fn report(&self, what: &str) {
        eprintln!(
            "K1 {} {what}: {} rounds, {} lost, {} wrong outcome, {} unusable after, worst cancel latency {:?}",
            CFG.driver,
            self.rounds,
            self.lost,
            self.wrong.len(),
            self.unusable.len(),
            self.worst
        );
    }

    fn is_clean(&self) -> bool {
        self.lost == 0
            && self.wrong.is_empty()
            && self.unusable.is_empty()
            && self.worst < CANCEL_DEADLINE
    }
}

/// The connection answers a read and takes a write, and the write is *committed*
/// (a second connection counts the rows): a session left inside a read-write
/// transaction would pass its own three statements. `committed` counts the inserts
/// that succeeded.
fn usable(session: &mut DriverSession, table: &str, committed: &mut usize) -> Result<(), String> {
    let database = database_name();
    for sql in [
        "SELECT 1".to_owned(),
        format!("INSERT INTO `{database}`.`{table}` VALUES (1)"),
        format!("SELECT COUNT(*) FROM `{database}`.`{table}`"),
    ] {
        match run_with(session, &sql, 0, 10, true) {
            Ok(page) => {
                let _ = session.release_page(page.page_id);
            }
            Err(error) => return Err(format!("`{sql}`: {error}")),
        }
        if sql.starts_with("INSERT") {
            *committed += 1;
        }
    }
    let seen = admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`"));
    if seen == [[Some(committed.to_string())]] {
        Ok(())
    } else {
        Err(format!(
            "a second connection sees {seen:?} committed rows, {committed} were inserted (the session is left in a transaction)"
        ))
    }
}

/// The time span the sweep covers: a few times the preparatory phase of a
/// read-only query (`SELECT 1` end to end) plus the cancel's own delivery.
fn sweep_span(session: &mut DriverSession) -> Duration {
    let mut times = (0..25)
        .map(|_| {
            let operation = session.begin_operation().unwrap();
            let started = Instant::now();
            let page = session
                .open_query(&read_request(operation.id(), "SELECT 1", true))
                .unwrap();
            let took = started.elapsed();
            session.release_page(page.page_id).unwrap();
            took
        })
        .collect::<Vec<_>>();
    times.sort();
    let select_one = times[times.len() / 2];
    let span = Duration::from_micros(env_number("TABLEX_SWEEP_SPAN_US", 0))
        .max(select_one * 4)
        .max(Duration::from_millis(4));
    eprintln!(
        "K1 {} sweep: median `SELECT 1` {select_one:?}, sweep span {span:?}",
        CFG.driver
    );
    span
}

fn sweep_queries(
    session: &mut DriverSession,
    table: &str,
    committed: &mut usize,
    sql: &str,
    read_only: bool,
    span: Duration,
) -> Sweep {
    let rounds = env_number("TABLEX_SWEEP_ROUNDS", 150) as usize;
    let mut sweep = Sweep::default();
    for round in 0..rounds {
        let delay = span.mul_f64(round as f64 / rounds as f64);
        let operation = session.begin_operation().unwrap();
        let runner = session.query_runner().unwrap();
        let request = read_request(operation.id(), sql, read_only);
        let result = cancel_after(session.connection_id(), operation.id(), delay, || {
            runner.open_query(&request)
        });
        drop(operation);
        sweep.record(round, delay, &result, "query.cancelled");
        if let Err(message) = usable(session, table, committed) {
            sweep.unusable.push(format!("round {round}: {message}"));
        }
    }
    sweep
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_cancel_at_any_moment_of_a_query_stops_it() {
    let database = database_name();
    let table = unique("sweep");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!("CREATE TABLE `{database}`.`{table}` (n INT)")]);
    let mut session = connect();
    let mut committed = 0;
    let span = sweep_span(&mut session);
    let mut failures = Vec::new();
    for (what, sql) in [
        ("sleep", "SELECT SLEEP(30)"),
        (
            "cpu bound",
            "SELECT COUNT(*) FROM information_schema.COLUMNS a, information_schema.COLUMNS b, information_schema.COLUMNS c",
        ),
    ] {
        for read_only in [true, false] {
            let label = format!("{what}, read_only={read_only}");
            let sweep = sweep_queries(&mut session, &table, &mut committed, sql, read_only, span);
            sweep.report(&label);
            if !sweep.is_clean() {
                failures.push(format!(
                    "{label}: {} lost, wrong {:?}, unusable {:?}, worst {:?}",
                    sweep.lost, sweep.wrong, sweep.unusable, sweep.worst
                ));
            }
        }
    }
    session.disconnect().unwrap();
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Holds a row lock on a connection of its own, so a mutation of that row
/// waits (a genuinely blocked statement, which a kill ends with an error).
#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_cancel_at_any_moment_of_a_mutation_stops_it() {
    let database = database_name();
    let table = unique("sweepm");
    let scratch = unique("sweepw");
    let _cleanup = Cleanup(vec![table.clone(), scratch.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, v INT NOT NULL)"),
        &format!("INSERT INTO `{database}`.`{table}` VALUES (1, 0)"),
        &format!("CREATE TABLE `{database}`.`{scratch}` (n INT)"),
    ]);
    let runtime = admin_runtime();
    let mut locker = runtime
        .block_on(mysql_async::Conn::new(admin_opts()))
        .expect("locker connect");
    runtime
        .block_on(async {
            locker.query_drop("START TRANSACTION").await?;
            locker
                .query_drop(format!(
                    "SELECT * FROM `{database}`.`{table}` WHERE id = 1 FOR UPDATE"
                ))
                .await
        })
        .expect("lock the row");

    let mut session = connect();
    let span = sweep_span(&mut session);
    let rounds = env_number("TABLEX_SWEEP_ROUNDS", 150) as usize;
    let mut committed = 0;
    let mut sweep = Sweep::default();
    let connection_id = session.connection_id();
    for round in 0..rounds {
        let delay = span.mul_f64(round as f64 / rounds as f64);
        let operation = session.begin_operation().unwrap();
        let id = operation.id();
        let request = mutation(
            "update",
            Some(&database),
            Some(&table),
            json!({}),
            json!({"op": "update", "key": {"id": 1}, "values": {"v": round + 1}}),
        )(id);
        let result = cancel_after(connection_id, id, delay, || session.mutate(&request));
        drop(operation);
        sweep.record(round, delay, &result, "mutation.cancelled");
        if let Err(message) = usable(&mut session, &scratch, &mut committed) {
            sweep.unusable.push(format!("round {round}: {message}"));
        }
    }
    sweep.report("mutation (row lock held elsewhere)");
    runtime
        .block_on(locker.query_drop("ROLLBACK"))
        .expect("release the lock");
    let _ = runtime.block_on(locker.disconnect());
    // Nothing of a cancelled mutation was applied.
    assert_eq!(
        admin_rows(&format!(
            "SELECT v FROM `{database}`.`{table}` WHERE id = 1"
        )),
        [[Some("0".to_owned())]]
    );
    session.disconnect().unwrap();
    assert!(
        sweep.is_clean(),
        "{} lost, wrong {:?}, unusable {:?}, worst {:?}",
        sweep.lost,
        sweep.wrong,
        sweep.unusable,
        sweep.worst
    );
}

/// A cancel for operation X that lands late must never fail the operation Y
/// that follows on the same connection.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_late_cancel_never_fails_the_operation_that_follows() {
    let mut session = connect();
    let connection_id = session.connection_id();
    let rounds = env_number("TABLEX_SWEEP_ROUNDS", 300) as usize;
    let (mut cancelled, mut completed) = (0, 0);
    let mut following: std::collections::BTreeMap<String, usize> = Default::default();
    for round in 0..rounds {
        let read_only = round % 2 == 0;
        let operation = session.begin_operation().unwrap();
        let id = operation.id();
        let runner = session.query_runner().unwrap();
        let request = read_request(id, "SELECT CAST(SLEEP(0.02) AS SIGNED)", read_only);
        let worker = std::thread::spawn(move || runner.open_query(&request));
        let canceller = std::thread::spawn(move || {
            // Around the end of the 20 ms statement and just after it.
            std::thread::sleep(Duration::from_micros(12_000 + (round as u64 % 40) * 450));
            let _ = env()
                .handle
                .cancel_operation(connection_id, &CancelRequest { operation_id: id });
        });
        match worker.join().unwrap() {
            Ok(page) => {
                completed += 1;
                let _ = session.release_page(page.page_id);
            }
            Err(error) => {
                assert_eq!(rejected(error).0, "query.cancelled");
                cancelled += 1;
            }
        }
        drop(operation);
        // The next operation starts while the cancel may still be on its way.
        let next = session.begin_operation().unwrap();
        let outcome = match session.open_query(&read_request(
            next.id(),
            "SELECT CAST(SLEEP(0.15) AS SIGNED)",
            read_only,
        )) {
            // SLEEP returns 1 when it was interrupted.
            Ok(page) => {
                let lease = session
                    .acquire_arrow_page(page.page_id, 0, page.row_count)
                    .expect("arrow lease");
                // SAFETY: the lease comes from the driver's Arrow export.
                let imported = unsafe { lease.into_record_batch() }.expect("import Arrow page");
                let slept = imported
                    .batch
                    .column(0)
                    .as_primitive::<Int64Type>()
                    .value(0);
                drop(imported);
                let _ = session.release_page(page.page_id);
                if slept == 0 {
                    "ok"
                } else {
                    "sleep interrupted"
                }
                .to_owned()
            }
            Err(error) => rejected(error).0,
        };
        *following.entry(outcome).or_default() += 1;
        canceller.join().unwrap();
    }
    eprintln!(
        "K1 {} late cancel: X {cancelled} cancelled / {completed} completed; Y outcomes {following:?}",
        CFG.driver
    );
    session.disconnect().unwrap();
    // The late window was exercised: some X were cancelled, some finished first.
    assert!(
        cancelled > 0 && completed > 0,
        "X: {cancelled} cancelled / {completed} completed; the cancel delay misses the end of X"
    );
    assert_eq!(following.len(), 1, "{following:?}");
    assert!(following.contains_key("ok"), "{following:?}");
}

/// A batch of many small statements: a `KILL QUERY` that reaches the thread
/// between two statements is discarded by the server, so the cancel has to be
/// noticed by the driver itself, at the next statement.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_cancel_during_a_long_batch_mutation_stops_it_at_the_next_statement() {
    let database = database_name();
    let table = unique("sweepb");
    let scratch = unique("sweepbw");
    let _cleanup = Cleanup(vec![table.clone(), scratch.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT AUTO_INCREMENT PRIMARY KEY, v INT NOT NULL)"),
        &format!("CREATE TABLE `{database}`.`{scratch}` (n INT)"),
    ]);
    let mut session = connect();
    let connection_id = session.connection_id();
    // An insert and an update of the row it made, over and over: runs of plain inserts are joined into multi-row
    // statements (a thousand rows each), which would make the batch too short to cancel in the middle of.
    let changes: Vec<Value> = (0..env_number("TABLEX_BATCH_CHANGES", 20000) / 2)
        .map(|id| id + 1)
        .flat_map(|id| {
            [
                json!({"op": "insert", "values": {"id": id, "v": id}}),
                json!({"op": "update", "key": {"id": id}, "values": {"v": id + 1}}),
            ]
        })
        .collect();
    // How long the whole batch takes when nobody cancels it.
    let uncancelled = {
        let started = Instant::now();
        let result = mutate(
            &mut session,
            mutation(
                "batch",
                Some(&database),
                Some(&table),
                json!({}),
                json!({ "changes": changes }),
            ),
        );
        result.expect("the batch runs");
        admin_exec(&[&format!("TRUNCATE TABLE `{database}`.`{table}`")]);
        started.elapsed()
    };
    eprintln!(
        "K1 {} batch of {} changes (insert + update pairs) takes {uncancelled:?}",
        CFG.driver,
        changes.len()
    );
    let rounds = env_number("TABLEX_SWEEP_ROUNDS", 24) as usize;
    // The test only tells a stopped batch from a finished one if the whole batch
    // outlasts the cancel's delay and the deadline.
    let longest_delay = Duration::from_millis(60 + 17 * rounds as u64);
    assert!(
        uncancelled > CANCEL_DEADLINE + longest_delay,
        "the batch takes {uncancelled:?}: raise TABLEX_BATCH_CHANGES above {} changes",
        changes.len()
    );
    let mut committed = 0;
    let mut sweep = Sweep::default();
    for round in 0..rounds {
        let delay = Duration::from_millis(60 + 17 * round as u64);
        let operation = session.begin_operation().unwrap();
        let id = operation.id();
        let request = mutation(
            "batch",
            Some(&database),
            Some(&table),
            json!({}),
            json!({ "changes": changes }),
        )(id);
        let result = cancel_after(connection_id, id, delay, || session.mutate(&request));
        drop(operation);
        sweep.record(round, delay, &result, "mutation.cancelled");
        if let Err(message) = usable(&mut session, &scratch, &mut committed) {
            sweep.unusable.push(format!("round {round}: {message}"));
        }
    }
    sweep.report("long batch mutation");
    assert_eq!(
        admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`")),
        [[Some("0".to_owned())]],
        "a cancelled batch is rolled back as a whole"
    );
    session.disconnect().unwrap();
    assert!(
        sweep.is_clean(),
        "{} lost, wrong {:?}, unusable {:?}, worst {:?}",
        sweep.lost,
        sweep.wrong,
        sweep.unusable,
        sweep.worst
    );
}

/// What the cancel bookkeeping costs when nobody cancels: `SELECT 1` x 2000 through
/// the driver (the median of five passes, microseconds per call).
#[test]
#[ignore = "needs a live server (see module docs)"]
fn select_one_cost_when_nobody_cancels() {
    let mut session = connect();
    for read_only in [false, true] {
        let mut passes = (0..5)
            .map(|_| {
                let started = Instant::now();
                for _ in 0..2000 {
                    let operation = session.begin_operation().unwrap();
                    let page = session
                        .open_query(&read_request(operation.id(), "SELECT 1", read_only))
                        .unwrap();
                    session.release_page(page.page_id).unwrap();
                }
                started.elapsed().as_micros() as f64 / 2000.0
            })
            .collect::<Vec<_>>();
        passes.sort_by(f64::total_cmp);
        eprintln!(
            "cost {} read_only={read_only}: {:.1} us per SELECT 1 (median of 5 x 2000; passes {passes:.1?})",
            CFG.driver, passes[2]
        );
    }
    session.disconnect().unwrap();
}

// ------------------------------------------------------------- value fidelity
//
// What the app receives and what it may send back for the column kinds that a
// display string cannot carry: BIT, TINYINT(1), non-UTF-8 text, GEOMETRY,
// BINARY / VARBINARY / BLOB, and a change on a table without transactions.

fn rejected_code(result: Result<engine_driver_api::MutationResult, LoaderError>) -> String {
    rejected(result.unwrap_err()).0
}

/// `SELECT ...` as text, one row, one column.
fn scalar(sql: &str) -> Option<String> {
    admin_rows(sql).remove(0).remove(0)
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn bit_columns_are_unsigned_integers_that_can_be_edited() {
    let database = database_name();
    let table = unique("bit");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!(
            "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, b1 BIT(1), b8 BIT(8), b64 BIT(64))"
        ),
        &format!(
            "INSERT INTO `{database}`.`{table}` VALUES (1, b'1', b'01000001', b'{}'), (2, b'0', NULL, b'1')",
            "1".repeat(64)
        ),
    ]);
    let mut session = connect();
    let select = format!("SELECT * FROM `{table}` ORDER BY id");
    let fetched = fetch(&mut session, &select, 0, 10);
    let labels: Vec<_> = fetched
        .page
        .columns
        .iter()
        .map(|column| column.data_type.as_str())
        .collect();
    assert_eq!(labels, ["int", "bit(1)", "bit(8)", "bit(64)"]);
    let batch = &fetched.batch;
    for column in 1..4 {
        assert_eq!(batch.column(column).data_type().to_string(), "UInt64");
    }
    let value =
        |column: usize, row: usize| batch.column(column).as_primitive::<UInt64Type>().value(row);
    // BIT(1) is 0/1 and BIT(8) 65 is the number 65, not the letter `A`.
    assert_eq!((value(1, 0), value(1, 1)), (1, 0));
    assert_eq!(value(2, 0), 65);
    assert!(batch.column(2).is_null(1));
    assert_eq!(value(3, 0), u64::MAX);

    // Edits: `1`, `0`, `65`, `b'0101'` are accepted, anything else is refused.
    let update = |session: &mut DriverSession, values: Value| {
        apply(
            session,
            &table,
            change(SqlRowOp::Update, json!({"id": 1}), values),
        )
    };
    update(
        &mut session,
        json!({"b1": "0", "b8": "b'0101'", "b64": "18446744073709551614"}),
    )
    .unwrap();
    assert_eq!(
        admin_rows(&format!(
            "SELECT b1+0, b8+0, b64+0 FROM `{database}`.`{table}` WHERE id = 1"
        )),
        [[
            Some("0".into()),
            Some("5".into()),
            Some("18446744073709551614".into())
        ]]
    );
    update(&mut session, json!({"b1": 1, "b8": "65"})).unwrap();
    assert_eq!(
        scalar(&format!(
            "SELECT b8+0 FROM `{database}`.`{table}` WHERE id = 1"
        ))
        .as_deref(),
        Some("65")
    );
    for bad in ["2", "abc", "b'12'", "256", "-1", ""] {
        let column = if bad == "256" {
            "b8"
        } else if bad == "2" {
            "b1"
        } else {
            "b8"
        };
        assert_eq!(
            rejected_code(update(&mut session, json!({column: bad}))),
            "mutation.invalid_value",
            "{bad}"
        );
    }
    // A BIT column is a valid key and takes an insert.
    apply(
        &mut session,
        &table,
        change(
            SqlRowOp::Update,
            json!({"id": 2, "b1": "0"}),
            json!({"b8": "3"}),
        ),
    )
    .unwrap();
    apply(
        &mut session,
        &table,
        change(
            SqlRowOp::Insert,
            json!(null),
            json!({"id": 3, "b1": "1", "b8": 7}),
        ),
    )
    .unwrap();
    assert_eq!(
        admin_rows(&format!(
            "SELECT b1+0, b8+0 FROM `{database}`.`{table}` WHERE id IN (2, 3) ORDER BY id"
        )),
        [
            [Some("0".into()), Some("3".into())],
            [Some("1".into()), Some("7".into())]
        ]
    );
    // And it reads back as the number it was written as.
    let again = fetch(&mut session, &select, 0, 10);
    assert_eq!(
        again.batch.column(2).as_primitive::<UInt64Type>().value(0),
        65
    );
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn tinyint_one_is_an_integer_column_with_and_without_other_values() {
    let database = database_name();
    let table = unique("tiny");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, flag TINYINT(1), small TINYINT)"),
        &format!("INSERT INTO `{database}`.`{table}` VALUES (1, 1, 1), (2, 0, 0), (3, NULL, NULL)"),
    ]);
    let mut session = connect();
    let select = format!("SELECT * FROM `{table}` ORDER BY id");
    // Only 0, 1 and NULL: still an integer column.
    let only_flags = fetch(&mut session, &select, 0, 10);
    assert_eq!(only_flags.page.columns[1].data_type, "tinyint");
    assert_eq!(only_flags.batch.column(1).data_type().to_string(), "Int64");
    assert_eq!(
        only_flags
            .batch
            .column(1)
            .as_primitive::<Int64Type>()
            .value(0),
        1
    );
    // The describe type is an integer type too: result and describe agree.
    let described = env().raw.call(
        "dbengine_describe_object",
        session.connection_id(),
        &json!({"database": database, "object": table}),
    );
    let data_type = described["value"]["columns"][1]["data_type"]
        .as_str()
        .unwrap();
    assert!(data_type.starts_with("tinyint"), "{data_type}");
    // A value other than 0 and 1 changes nothing about the type.
    apply(
        &mut session,
        &table,
        change(SqlRowOp::Update, json!({"id": 1}), json!({"flag": 5})),
    )
    .unwrap();
    apply(
        &mut session,
        &table,
        change(SqlRowOp::Update, json!({"id": 2}), json!({"flag": "-7"})),
    )
    .unwrap();
    assert_eq!(
        rejected_code(apply(
            &mut session,
            &table,
            change(SqlRowOp::Update, json!({"id": 2}), json!({"flag": "true"}))
        )),
        "mutation.failed"
    );
    let mixed = fetch(&mut session, &select, 0, 10);
    assert_eq!(mixed.page.columns[1].data_type, "tinyint");
    assert_eq!(mixed.batch.column(1).data_type().to_string(), "Int64");
    assert_eq!(
        mixed.batch.column(1).as_primitive::<Int64Type>().values()[..2],
        [5, -7]
    );
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn text_is_utf8_in_any_column_charset_and_never_repaired() {
    let database = database_name();
    let table = unique("charset");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!(
            "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY,
               l1 VARCHAR(20) CHARACTER SET latin1, u8 VARCHAR(20) CHARACTER SET utf8mb4)"
        ),
        // 0xE9 is `é` in latin1.
        &format!(
            "INSERT INTO `{database}`.`{table}` VALUES (1, CONVERT(UNHEX('636166E9') USING latin1), 'caf\u{e9} \u{2713}')"
        ),
    ]);
    let mut session = connect();
    let select = format!("SELECT l1, u8 FROM `{table}`");
    let text = |fetched: &Fetched| {
        (
            fetched
                .batch
                .column(0)
                .as_string::<i32>()
                .value(0)
                .to_owned(),
            fetched
                .batch
                .column(1)
                .as_string::<i32>()
                .value(0)
                .to_owned(),
        )
    };
    assert_eq!(
        text(&fetch(&mut session, &select, 0, 10)),
        ("caf\u{e9}".into(), "caf\u{e9} \u{2713}".into())
    );
    // User SQL that switches the session to latin1 is undone before the next request.
    run(&mut session, "SET NAMES latin1", 0, 10).unwrap();
    assert_eq!(
        text(&fetch(&mut session, &select, 0, 10)),
        ("caf\u{e9}".into(), "caf\u{e9} \u{2713}".into())
    );
    let names = fetch(
        &mut session,
        "SELECT @@character_set_client, @@character_set_results, @@character_set_connection",
        0,
        1,
    );
    for column in 0..3 {
        assert_eq!(
            names.batch.column(column).as_string::<i32>().value(0),
            "utf8mb4"
        );
    }

    // Bytes that are not UTF-8 (a latin1 `é` sent to a client that asked for
    // latin1 results) are never replaced by U+FFFD: the column arrives as bytes
    // under a label that says so.
    let script = format!("SET character_set_results = latin1; SELECT id, l1 FROM `{table}`");
    let invalid = fetch(&mut session, &script, 0, 10);
    assert_eq!(invalid.page.columns[1].data_type, "varchar (invalid utf-8)");
    assert_eq!(invalid.batch.column(1).data_type().to_string(), "Binary");
    assert_eq!(
        invalid.batch.column(1).as_binary::<i32>().value(0),
        b"caf\xe9"
    );
    // The session is back on utf8mb4 for the next request.
    assert_eq!(text(&fetch(&mut session, &select, 0, 10)).0, "caf\u{e9}");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn geometry_columns_are_wkt_and_refuse_text_edits() {
    let database = database_name();
    let table = unique("geo");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!(
            "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, p POINT, l LINESTRING, g POLYGON, n VARCHAR(10))"
        ),
        &format!(
            "INSERT INTO `{database}`.`{table}` VALUES
               (1, ST_GeomFromText('POINT(1 2)'), ST_GeomFromText('LINESTRING(0 0, 1 1, 2 0.5)'),
                ST_GeomFromText('POLYGON((0 0, 4 0, 4 4, 0 0), (1 1, 2 1, 2 2, 1 1))'), 'a'),
               (2, ST_GeomFromText('POINT(-1.5 3)', 3857), NULL, NULL, 'b')"
        ),
    ]);
    let mut session = connect();
    let fetched = fetch(
        &mut session,
        &format!("SELECT * FROM `{table}` ORDER BY id"),
        0,
        10,
    );
    let labels: Vec<_> = fetched
        .page
        .columns
        .iter()
        .map(|column| column.data_type.as_str())
        .collect();
    assert_eq!(
        labels,
        ["int", "geometry", "geometry", "geometry", "varchar"]
    );
    let wkt =
        |column: usize, row: usize| fetched.batch.column(column).as_string::<i32>().value(row);
    assert_eq!(wkt(1, 0), "POINT (1 2)");
    assert_eq!(wkt(2, 0), "LINESTRING (0 0, 1 1, 2 0.5)");
    assert_eq!(
        wkt(3, 0),
        "POLYGON ((0 0, 4 0, 4 4, 0 0), (1 1, 2 1, 2 2, 1 1))"
    );
    assert_eq!(wkt(1, 1), "SRID=3857;POINT (-1.5 3)");
    assert!(fetched.batch.column(2).is_null(1));
    // Edits through text are refused, in a value and in a key; NULL and the other
    // columns of the row can be written.
    for column in ["p", "l", "g"] {
        assert_eq!(
            rejected_code(apply(
                &mut session,
                &table,
                change(
                    SqlRowOp::Update,
                    json!({"id": 1}),
                    json!({column: "POINT (9 9)"})
                )
            )),
            "mutation.value_not_editable"
        );
    }
    assert_eq!(
        rejected_code(apply(
            &mut session,
            &table,
            change(
                SqlRowOp::Update,
                json!({"p": "POINT (1 2)"}),
                json!({"n": "z"})
            )
        )),
        "mutation.value_not_editable"
    );
    assert_eq!(
        scalar(&format!(
            "SELECT ST_AsText(p) FROM `{database}`.`{table}` WHERE id = 1"
        ))
        .as_deref(),
        Some("POINT(1 2)")
    );
    apply(
        &mut session,
        &table,
        change(SqlRowOp::Update, json!({"id": 1}), json!({"n": "z"})),
    )
    .unwrap();
    apply(
        &mut session,
        &table,
        change(SqlRowOp::Update, json!({"id": 2}), json!({"p": null})),
    )
    .unwrap();
    assert_eq!(
        scalar(&format!(
            "SELECT COUNT(*) FROM `{database}`.`{table}` WHERE p IS NULL"
        ))
        .as_deref(),
        Some("1")
    );
    // A geometry the reader does not know is marked hex, never text.
    let page = fetch(
        &mut session,
        "SELECT ST_GeomFromText('GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1))')",
        0,
        1,
    );
    assert_eq!(
        page.batch.column(0).as_string::<i32>().value(0),
        "GEOMETRYCOLLECTION (POINT (1 2), LINESTRING (0 0, 1 1))"
    );
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn binary_columns_round_trip_in_0x_hex_and_refuse_other_text() {
    let database = database_name();
    let table = unique("bin");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[
        &format!(
            "CREATE TABLE `{database}`.`{table}` (id BINARY(16) PRIMARY KEY, v VARBINARY(16), b BLOB, name VARCHAR(20))"
        ),
        &format!(
            "INSERT INTO `{database}`.`{table}` VALUES
               (UNHEX('00112233445566778899AABBCCDDEEFF'), UNHEX('0102'), UNHEX('00FF'), 'one'),
               (UNHEX('FFEEDDCCBBAA99887766554433221100'), NULL, NULL, 'two')"
        ),
    ]);
    let key = "0x00112233445566778899aabbccddeeff";
    let other_key = "0xFFEEDDCCBBAA99887766554433221100";
    let mut session = connect();
    let fetched = fetch(
        &mut session,
        &format!("SELECT * FROM `{table}` ORDER BY name"),
        0,
        10,
    );
    let labels: Vec<_> = fetched
        .page
        .columns
        .iter()
        .map(|c| c.data_type.as_str())
        .collect();
    assert_eq!(labels, ["binary", "varbinary", "blob", "varchar"]);
    for column in 0..3 {
        assert_eq!(
            fetched.batch.column(column).data_type().to_string(),
            "Binary"
        );
    }
    assert_eq!(
        fetched.batch.column(0).as_binary::<i32>().value(0).len(),
        16
    );
    let hex = |column: &str, id: &str| {
        scalar(&format!(
            "SELECT HEX({column}) FROM `{database}`.`{table}` WHERE name = '{id}'"
        ))
    };
    // A row with a binary primary key is editable: value, VARBINARY, BLOB.
    apply(
        &mut session,
        &table,
        change(
            SqlRowOp::Update,
            json!({"id": key}),
            json!({"v": "0xDEADBEEF", "b": "0x", "name": "uno"}),
        ),
    )
    .unwrap();
    assert_eq!(hex("v", "uno").as_deref(), Some("DEADBEEF"));
    assert_eq!(hex("b", "uno").as_deref(), Some(""));
    // A byte that is not valid UTF-8 survives the round trip (bound as bytes).
    apply(
        &mut session,
        &table,
        change(
            SqlRowOp::Update,
            json!({"id": other_key}),
            json!({"v": "0xff00fe", "b": "0xC328"}),
        ),
    )
    .unwrap();
    assert_eq!(hex("v", "two").as_deref(), Some("FF00FE"));
    assert_eq!(hex("b", "two").as_deref(), Some("C328"));
    // Insert and delete by a binary key.
    apply(
        &mut session,
        &table,
        change(
            SqlRowOp::Insert,
            json!(null),
            json!({"id": "0x0f0e0d0c0b0a09080706050403020100", "v": "0x41", "name": "new"}),
        ),
    )
    .unwrap();
    assert_eq!(hex("v", "new").as_deref(), Some("41"));
    apply(
        &mut session,
        &table,
        change(
            SqlRowOp::Delete,
            json!({"id": "0x0F0E0D0C0B0A09080706050403020100"}),
            json!(null),
        ),
    )
    .unwrap();
    assert_eq!(
        scalar(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`")).as_deref(),
        Some("2")
    );
    // Any other text for a binary column is refused, in a value and in a key; the
    // character string `hello` is never bound.
    for bad in ["hello", "0xABC", "X'AB'", "0xZZ", "00ff", "\\x00ff"] {
        assert_eq!(
            rejected_code(apply(
                &mut session,
                &table,
                change(SqlRowOp::Update, json!({"id": key}), json!({"v": bad}))
            )),
            "mutation.invalid_value",
            "value {bad}"
        );
        assert_eq!(
            rejected_code(apply(
                &mut session,
                &table,
                change(SqlRowOp::Update, json!({"id": bad}), json!({"name": "x"}))
            )),
            "mutation.invalid_value",
            "key {bad}"
        );
    }
    assert_eq!(hex("v", "uno").as_deref(), Some("DEADBEEF"));
    // A batch is refused as a whole, naming the change.
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &table,
            vec![
                change(SqlRowOp::Update, json!({"id": key}), json!({"name": "dos"})),
                change(SqlRowOp::Update, json!({"id": key}), json!({"v": "nope"})),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.invalid_value");
    assert!(message.starts_with("change 1:"), "{message}");
    assert_eq!(hex("v", "uno").as_deref(), Some("DEADBEEF"));
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_table_without_transactions_is_changed_one_row_at_a_time_and_reported_truthfully() {
    let database = database_name();
    let myisam = unique("myisam");
    let innodb = unique("innodb");
    let _cleanup = Cleanup(vec![myisam.clone(), innodb.clone()]);
    for (table, engine) in [(&myisam, "MyISAM"), (&innodb, "InnoDB")] {
        admin_exec(&[
            &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, v INT, k INT) ENGINE={engine}"),
            &format!("INSERT INTO `{database}`.`{table}` VALUES (1, 0, 5), (2, 0, 5), (3, 0, 6)"),
        ]);
    }
    let values = |table: &str| {
        scalar(&format!(
            "SELECT GROUP_CONCAT(v ORDER BY id) FROM `{database}`.`{table}`"
        ))
        .unwrap()
    };
    let mut session = connect();
    let batch = |table: &str, session: &mut DriverSession| {
        apply_batch(
            session,
            table,
            vec![
                change(SqlRowOp::Update, json!({"id": 1}), json!({"v": 10})),
                change(SqlRowOp::Update, json!({"id": 2}), json!({"v": 20})),
                change(SqlRowOp::Update, json!({"id": 99}), json!({"v": 30})),
                change(SqlRowOp::Update, json!({"id": 3}), json!({"v": 40})),
            ],
        )
    };
    // InnoDB: all or nothing, and it says it was rolled back.
    let (code, message) = rejected(batch(&innodb, &mut session).unwrap_err());
    assert_eq!(code, "mutation.no_rows_affected");
    assert!(
        message.starts_with("change 2:") && message.contains("rolled back"),
        "{message}"
    );
    assert_eq!(values(&innodb), "0,0,0");
    // MyISAM: the first two changes are applied and STAY applied; the failure is
    // `partially_applied` and names what was applied, failed and not attempted.
    let (code, message) = rejected(batch(&myisam, &mut session).unwrap_err());
    assert_eq!(code, "mutation.partially_applied");
    assert!(
        message.contains("applied: [0, 1]; failed: [2]; unknown: []; not attempted: [3]"),
        "{message}"
    );
    assert!(
        message.contains("NOT rolled back") && message.contains("MyISAM"),
        "{message}"
    );
    assert!(!message.contains("was rolled back"), "{message}");
    assert!(message.contains("affected no row"), "{message}");
    assert_eq!(values(&myisam), "10,20,0");
    // A failing first change applied nothing: the plain error, no partial report.
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &myisam,
            vec![
                change(SqlRowOp::Update, json!({"id": 99}), json!({"v": 1})),
                change(SqlRowOp::Update, json!({"id": 3}), json!({"v": 2})),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.no_rows_affected");
    assert!(!message.contains("rolled back"), "{message}");
    assert_eq!(values(&myisam), "10,20,0");
    // A key that matches several rows is refused before anything is written.
    let (code, _) = rejected(
        apply(
            &mut session,
            &myisam,
            change(SqlRowOp::Update, json!({"k": 5}), json!({"v": 77})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.identity_not_unique");
    let (code, _) = rejected(
        apply(
            &mut session,
            &myisam,
            change(SqlRowOp::Delete, json!({"k": 5}), json!(null)),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.identity_not_unique");
    assert_eq!(values(&myisam), "10,20,0");
    assert_eq!(
        scalar(&format!("SELECT COUNT(*) FROM `{database}`.`{myisam}`")).as_deref(),
        Some("3")
    );
    // A good batch, a duplicate key (a server error mid-batch) and a delete.
    let result = apply_batch(
        &mut session,
        &myisam,
        vec![
            change(
                SqlRowOp::Insert,
                json!(null),
                json!({"id": 4, "v": 4, "k": 9}),
            ),
            change(SqlRowOp::Update, json!({"id": 4}), json!({"v": 5})),
            change(SqlRowOp::Delete, json!({"id": 3}), json!(null)),
        ],
    )
    .unwrap();
    assert_eq!(result.affected_rows, 3);
    assert_eq!(values(&myisam), "10,20,5");
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &myisam,
            vec![
                change(SqlRowOp::Update, json!({"id": 1}), json!({"v": 11})),
                change(SqlRowOp::Insert, json!(null), json!({"id": 2, "v": 1})),
            ],
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.partially_applied");
    assert!(
        message.contains("applied: [0]; failed: [1]; unknown: []; not attempted: []"),
        "{message}"
    );
    assert!(message.contains("Duplicate"), "{message}");
    assert_eq!(values(&myisam), "11,20,5");
    // Single changes work as before.
    apply(
        &mut session,
        &myisam,
        change(SqlRowOp::Update, json!({"id": 1}), json!({"v": 12})),
    )
    .unwrap();
    assert_eq!(values(&myisam), "12,20,5");
    session.disconnect().unwrap();
}

/// `dbengine_connect` called directly: the reply carries the session's dialect.
fn raw_connect(uri: &str) -> Value {
    let name = CString::new("dbengine_connect").unwrap();
    // SAFETY: valid handle and NUL-terminated name.
    let address = unsafe { libc::dlsym(env().raw.lib, name.as_ptr()) };
    assert!(!address.is_null());
    // SAFETY: `dbengine_connect` is `(*const c_char) -> *mut c_char`.
    let function: unsafe extern "C" fn(*const c_char) -> *mut c_char =
        unsafe { std::mem::transmute(address) };
    let request = CString::new(json!({"uri": uri}).to_string()).unwrap();
    // SAFETY: valid request for the call.
    let response = unsafe { function(request.as_ptr()) };
    // SAFETY: NUL-terminated string the driver owns until freed.
    let text = unsafe { CStr::from_ptr(response) }
        .to_str()
        .unwrap()
        .to_owned();
    let free = CString::new("dbengine_free_string").unwrap();
    // SAFETY: required export with this signature; freed once.
    unsafe {
        let address = libc::dlsym(env().raw.lib, free.as_ptr());
        let free: unsafe extern "C" fn(*mut c_char) = std::mem::transmute(address);
        free(response);
    }
    serde_json::from_str(&text).expect("envelope")
}

fn raw_disconnect(connection_id: u64) {
    env()
        .raw
        .call("dbengine_disconnect", connection_id, &json!({}));
}

/// Restores the server's GLOBAL `sql_mode` on drop.
struct RestoreSqlMode(String);

impl RestoreSqlMode {
    /// Remembers the current mode and makes `extra` part of it for the next sessions.
    fn extend_with(extra: &str) -> Self {
        let previous = scalar("SELECT @@GLOBAL.sql_mode").unwrap_or_default();
        let restore = Self(previous.clone());
        let mode = if previous.is_empty() {
            extra.to_owned()
        } else {
            format!("{previous},{extra}")
        };
        admin_exec(&[&format!("SET GLOBAL sql_mode = '{mode}'")]);
        restore
    }
}

impl Drop for RestoreSqlMode {
    fn drop(&mut self) {
        admin_exec(&[&format!("SET GLOBAL sql_mode = '{}'", self.0)]);
    }
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn the_connect_reply_publishes_the_sessions_backslash_escape_mode() {
    let default = raw_connect(&uri());
    assert_eq!(
        default["value"]["sql_dialect"]["string_backslash_escapes"], true,
        "{default}"
    );
    assert_eq!(default["value"]["sql_dialect"]["identifier_quote"], "`");
    raw_disconnect(default["value"]["connection_id"].as_u64().unwrap());

    // A server whose default mode is NO_BACKSLASH_ESCAPES (restored afterwards).
    let _restore = RestoreSqlMode::extend_with("NO_BACKSLASH_ESCAPES");
    let reply = raw_connect(&uri());
    assert_eq!(
        reply["value"]["sql_dialect"]["string_backslash_escapes"], false,
        "{reply}"
    );
    raw_disconnect(reply["value"]["connection_id"].as_u64().unwrap());
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn the_connect_reply_publishes_the_sessions_identifier_quoting() {
    let default = raw_connect(&uri());
    let dialect = &default["value"]["sql_dialect"];
    assert_eq!(dialect["double_quote_identifiers"], false, "{default}");
    assert_eq!(dialect["bracket_identifiers"], false, "{default}");
    raw_disconnect(default["value"]["connection_id"].as_u64().unwrap());

    // ANSI_QUOTES makes `"…"` an identifier; `[name]` stays a syntax error (restored afterwards).
    {
        let _restore = RestoreSqlMode::extend_with("ANSI_QUOTES");
        let reply = raw_connect(&uri());
        let dialect = &reply["value"]["sql_dialect"];
        assert_eq!(dialect["double_quote_identifiers"], true, "{reply}");
        assert_eq!(dialect["bracket_identifiers"], false, "{reply}");
        assert_eq!(dialect["string_backslash_escapes"], true, "{reply}");
        raw_disconnect(reply["value"]["connection_id"].as_u64().unwrap());
    }

    // `ANSI` is a combination that includes ANSI_QUOTES.
    {
        let _restore = RestoreSqlMode::extend_with("ANSI");
        let reply = raw_connect(&uri());
        assert_eq!(
            reply["value"]["sql_dialect"]["double_quote_identifiers"], true,
            "{reply}"
        );
        raw_disconnect(reply["value"]["connection_id"].as_u64().unwrap());
    }

    // MSSQL exists on MariaDB only; it adds `[name]` identifiers.
    if CFG.driver == "mariadb" {
        let _restore = RestoreSqlMode::extend_with("MSSQL");
        let reply = raw_connect(&uri());
        let dialect = &reply["value"]["sql_dialect"];
        assert_eq!(dialect["bracket_identifiers"], true, "{reply}");
        assert_eq!(dialect["double_quote_identifiers"], true, "{reply}");
        raw_disconnect(reply["value"]["connection_id"].as_u64().unwrap());
    }
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn tables_that_differ_only_in_case_are_never_mixed_up() {
    if scalar("SELECT @@lower_case_table_names").as_deref() != Some("0") {
        eprintln!("skipped: lower_case_table_names is not 0 on this server");
        return;
    }
    let database = database_name();
    let lower = unique("ci");
    let upper = lower.to_uppercase();
    let _cleanup = Cleanup(vec![lower.clone(), upper.clone()]);
    admin_exec(&[
        // `lower` has a binary key on a transactional table, `upper` a text one on MyISAM.
        &format!("CREATE TABLE `{database}`.`{lower}` (id BINARY(2) PRIMARY KEY, v VARBINARY(8)) ENGINE=InnoDB"),
        &format!("CREATE TABLE `{database}`.`{upper}` (id VARCHAR(8) PRIMARY KEY, v VARCHAR(8)) ENGINE=MyISAM"),
        &format!("INSERT INTO `{database}`.`{lower}` VALUES (UNHEX('0001'), UNHEX('41'))"),
        &format!("INSERT INTO `{database}`.`{upper}` VALUES ('k', 'a')"),
    ]);
    let mut session = connect();
    // The binary column is bound as bytes whichever table is asked for, in either order.
    for _ in 0..2 {
        apply(
            &mut session,
            &lower,
            change(
                SqlRowOp::Update,
                json!({"id": "0x0001"}),
                json!({"v": "0x4142"}),
            ),
        )
        .unwrap();
        apply(
            &mut session,
            &upper,
            change(SqlRowOp::Update, json!({"id": "k"}), json!({"v": "0x41"})),
        )
        .unwrap();
    }
    assert_eq!(
        scalar(&format!("SELECT HEX(v) FROM `{database}`.`{lower}`")).as_deref(),
        Some("4142")
    );
    // The text column keeps the text `0x41` (it is not a binary column).
    assert_eq!(
        scalar(&format!("SELECT v FROM `{database}`.`{upper}`")).as_deref(),
        Some("0x41")
    );
    // The other table's text column is not mistaken for binary: `hello` is refused only for `lower`.
    assert_eq!(
        rejected_code(apply(
            &mut session,
            &lower,
            change(
                SqlRowOp::Update,
                json!({"id": "0x0001"}),
                json!({"v": "hello"})
            )
        )),
        "mutation.invalid_value"
    );
    apply(
        &mut session,
        &upper,
        change(SqlRowOp::Update, json!({"id": "k"}), json!({"v": "hello"})),
    )
    .unwrap();
    // Engines are not mixed up either: the MyISAM table reports a partial apply,
    // the InnoDB one a rollback.
    let failing = |table: &str| {
        let key = if table == upper {
            json!("k")
        } else {
            json!("0x0001")
        };
        vec![
            change(SqlRowOp::Update, json!({"id": key}), json!({"v": "0x41"})),
            change(SqlRowOp::Update, json!({"id": "zz"}), json!({"v": "0x41"})),
        ]
    };
    let (code, _) = rejected(apply_batch(&mut session, &upper, failing(&upper)).unwrap_err());
    assert_eq!(code, "mutation.partially_applied");
    let (code, message) = rejected(apply_batch(&mut session, &lower, failing(&lower)).unwrap_err());
    assert_eq!(code, "mutation.invalid_value", "{message}");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_change_cut_off_on_a_table_without_transactions_is_unknown_not_failed() {
    let database = database_name();
    let table = unique("cut");
    let _cleanup = Cleanup(vec![table.clone()]);
    let trigger = unique("cuttrg");
    admin_exec(&[
        &format!("DROP TRIGGER IF EXISTS `{database}`.`{trigger}`"),
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, v INT) ENGINE=MyISAM"),
        &format!("INSERT INTO `{database}`.`{table}` VALUES (1, 0)"),
        // Inserting id 2 takes a while: time to cut the connection in the middle of it.
        &format!(
            "CREATE TRIGGER `{database}`.`{trigger}` BEFORE INSERT ON `{database}`.`{table}` \
             FOR EACH ROW SET NEW.v = IF(NEW.id = 2, SLEEP(8), NEW.v)"
        ),
    ]);
    let mut session = connect();
    let id = thread_id(&mut session);
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        admin_exec(&[&format!("KILL CONNECTION {id}")]);
    });
    let started = Instant::now();
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &table,
            vec![
                change(SqlRowOp::Update, json!({"id": 1}), json!({"v": 5})),
                change(SqlRowOp::Insert, json!(null), json!({"id": 2, "v": 1})),
                change(SqlRowOp::Insert, json!(null), json!({"id": 3, "v": 1})),
            ],
        )
        .unwrap_err(),
    );
    killer.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "the kill cut the statement short"
    );
    assert_eq!(code, "mutation.partially_applied", "{message}");
    // Change 0 was applied; change 1 was cut off (it may or may not have been
    // applied): unknown, not failed; change 2 was never attempted.
    assert!(
        message.ends_with("applied: [0]; failed: []; unknown: [1]; not attempted: [2]"),
        "{message}"
    );
    assert!(
        message.contains("may or may not have been applied"),
        "{message}"
    );
    assert_eq!(
        scalar(&format!(
            "SELECT v FROM `{database}`.`{table}` WHERE id = 1"
        ))
        .as_deref(),
        Some("5")
    );
    // The connection is closed for good, as after any lost connection.
    let (code, _) = rejected(
        apply(
            &mut session,
            &table,
            change(SqlRowOp::Update, json!({"id": 1}), json!({"v": 6})),
        )
        .unwrap_err(),
    );
    assert_eq!(code, "mutation.not_connected");
    let _ = session.disconnect();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_view_is_changed_one_change_at_a_time_and_never_reported_as_rolled_back() {
    let database = database_name();
    let table = unique("vbase");
    let view = unique("vview");
    let _cleanup = Cleanup(vec![view.clone(), table.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, v INT) ENGINE=InnoDB"),
        &format!("INSERT INTO `{database}`.`{table}` VALUES (1, 0), (2, 0)"),
        &format!("CREATE VIEW `{database}`.`{view}` AS SELECT id, v FROM `{database}`.`{table}`"),
    ]);
    let mut session = connect();
    let (code, message) = rejected(
        apply_batch(
            &mut session,
            &view,
            vec![
                change(SqlRowOp::Update, json!({"id": 1}), json!({"v": 7})),
                change(SqlRowOp::Update, json!({"id": 99}), json!({"v": 8})),
            ],
        )
        .unwrap_err(),
    );
    // The engine of a view is unknown: nothing is claimed rolled back.
    assert_eq!(code, "mutation.partially_applied", "{message}");
    assert!(
        message.contains("no known transactional engine"),
        "{message}"
    );
    assert!(
        message.ends_with("applied: [0]; failed: [1]; unknown: []; not attempted: []"),
        "{message}"
    );
    assert_eq!(
        scalar(&format!(
            "SELECT v FROM `{database}`.`{table}` WHERE id = 1"
        ))
        .as_deref(),
        Some("7")
    );
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_batch_of_inserts_is_joined_into_multi_row_statements_and_stays_all_or_nothing() {
    let database = database_name();
    let table = unique("multi");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!(
        "CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, name VARCHAR(80) NOT NULL, price DECIMAL(12,2), at DATETIME NULL, flag TINYINT(1), blob_ BLOB NULL) ENGINE=InnoDB"
    )]);
    let mut session = connect();
    let count = || scalar(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`")).unwrap();
    let insert = |id: i64| {
        change(
            SqlRowOp::Insert,
            json!(null),
            json!({"id": id, "name": format!("it's #{id}; DROP TABLE x; --"), "price": format!("{id}.50"), "at": "2024-01-02 03:04:05", "flag": id % 2 == 0, "blob_": null}),
        )
    };

    // 2500 rows: three statements (1000 + 1000 + 500) in one transaction.
    let result = apply_batch(&mut session, &table, (1..=2500).map(insert).collect()).expect("a batch of inserts");
    assert_eq!(result.affected_rows, 2500);
    assert_eq!(count(), "2500");
    assert_eq!(scalar(&format!("SELECT name FROM `{database}`.`{table}` WHERE id = 1234")).unwrap(), "it's #1234; DROP TABLE x; --");
    assert_eq!(scalar(&format!("SELECT price FROM `{database}`.`{table}` WHERE id = 2500")).unwrap(), "2500.50");
    assert_eq!(scalar(&format!("SELECT COUNT(*) FROM `{database}`.`{table}` WHERE flag = 1")).unwrap(), "1250");

    // A duplicate in the middle of the second statement rolls everything back, and names the changes of the statement.
    let mut changes: Vec<SqlRowChange> = (3000..=4500).map(insert).collect();
    changes[1200] = insert(7);
    let (code, message) = rejected(apply_batch(&mut session, &table, changes).unwrap_err());
    assert_eq!(code, "mutation.failed");
    assert!(message.contains("1062"), "{message}");
    assert_eq!(count(), "2500");

    // Mixed with an update and a delete: order kept, one transaction.
    let result = apply_batch(
        &mut session,
        &table,
        vec![
            insert(5000),
            insert(5001),
            change(SqlRowOp::Update, json!({"id": 5000}), json!({"name": "renamed"})),
            insert(5002),
            change(SqlRowOp::Delete, json!({"id": 1}), json!(null)),
        ],
    )
    .expect("a mixed batch");
    assert_eq!(result.affected_rows, 5);
    assert_eq!(count(), "2502");
    assert_eq!(scalar(&format!("SELECT name FROM `{database}`.`{table}` WHERE id = 5000")).unwrap(), "renamed");
    session.disconnect().unwrap();
}

#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_session_without_strict_mode_never_joins_inserts_so_a_null_in_a_not_null_column_is_refused() {
    let database = database_name();
    let table = unique("strict");
    let _cleanup = Cleanup(vec![table.clone()]);
    admin_exec(&[&format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, n INT NOT NULL)")]);
    let mut session = connect();
    let row = |id: i64, n: Value| change(SqlRowOp::Insert, json!(null), json!({"id": id, "n": n}));
    let count = || scalar(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`")).unwrap();
    // Not strict: a multi-row INSERT would store 0 for the NULL (and only warn). Alone, each insert fails on 1048.
    let page = run_with(&mut session, "SET SESSION sql_mode = 'NO_ENGINE_SUBSTITUTION'", 0, 10, true).expect("set the mode");
    session.release_page(page.page_id).unwrap();
    let (code, message) = rejected(apply_batch(&mut session, &table, vec![row(1, json!(1)), row(2, json!(null)), row(3, json!(3))]).unwrap_err());
    assert_eq!(code, "mutation.failed");
    assert!(message.contains("1048"), "{message}");
    assert_eq!(count(), "0", "nothing was stored, and certainly no implicit 0");
    // A strict session joins them, and the server refuses the NULL there too.
    let page = run_with(&mut session, "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'", 0, 10, true).expect("set the mode");
    session.release_page(page.page_id).unwrap();
    let (code, _) = rejected(apply_batch(&mut session, &table, vec![row(1, json!(1)), row(2, json!(null)), row(3, json!(3))]).unwrap_err());
    assert_eq!(code, "mutation.failed");
    assert_eq!(count(), "0");
    let result = apply_batch(&mut session, &table, vec![row(1, json!(1)), row(2, json!(2))]).expect("good rows");
    assert_eq!(result.affected_rows, 2);
    session.disconnect().unwrap();
}

/// A cancel that lands inside one joined multi-row statement (a `BEFORE INSERT` trigger that sleeps 4 ms a row makes
/// the statement at least 1.2 s long whatever the CPU; a killed `SLEEP` returns early, so the statement may still end
/// without an error, and the driver's own cancel check then rolls it back): the whole batch is rolled back and the
/// session is usable.
#[test]
#[ignore = "needs a live server (see module docs)"]
fn a_cancel_inside_a_joined_insert_rolls_the_whole_batch_back() {
    let database = database_name();
    let table = unique("joincancel");
    let scratch = unique("joincancelw");
    let _cleanup = Cleanup(vec![table.clone(), scratch.clone()]);
    admin_exec(&[
        &format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, v INT NOT NULL)"),
        &format!("CREATE TABLE `{database}`.`{scratch}` (n INT)"),
        &format!("CREATE TRIGGER `{database}`.`{table}_slow` BEFORE INSERT ON `{database}`.`{table}` FOR EACH ROW SET @spent = SLEEP(0.004)"),
    ]);
    // Only a strict session joins inserts; on a server configured otherwise this test would not reach the joined path.
    let mode = admin_rows("SELECT @@GLOBAL.sql_mode");
    assert!(
        mode[0][0].as_deref().is_some_and(|mode| mode.contains("STRICT_")),
        "the server's sql_mode must be strict for this test: {mode:?}"
    );
    let mut session = connect();
    let connection_id = session.connection_id();
    // 300 rows are one joined statement of at least 1.2 s. The server may finish the statement it is in before it
    // honours the kill (a trigger body does not look at it between rows), so the cancel is looked at by the driver
    // when the statement returns, and the batch is rolled back either way.
    let changes: Vec<Value> = (1..=300).map(|id| json!({"op": "insert", "values": {"id": id, "v": id}})).collect();
    let mut committed = 0;
    for delay in [Duration::from_millis(200), Duration::from_millis(600)] {
        let operation = session.begin_operation().unwrap();
        let id = operation.id();
        let request = mutation("batch", Some(&database), Some(&table), json!({}), json!({ "changes": changes }))(id);
        let round = cancel_after(connection_id, id, delay, || session.mutate(&request));
        drop(operation);
        assert_eq!(round.outcome, "mutation.cancelled", "{round:?}");
        assert!(!round.lost && round.latency < CANCEL_DEADLINE, "{round:?}");
        assert_eq!(
            admin_rows(&format!("SELECT COUNT(*) FROM `{database}`.`{table}`")),
            [[Some("0".to_owned())]],
            "a cancelled batch is rolled back as a whole"
        );
        usable(&mut session, &scratch, &mut committed).expect("the session is usable after the cancel");
    }
    session.disconnect().unwrap();
}

