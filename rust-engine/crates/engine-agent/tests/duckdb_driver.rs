//! The tool layer end to end over the REAL DuckDB driver: the cdylib is built by cargo, loaded
//! through `engine-driver-host`, and every tool runs against a temporary database file. The
//! agent's connection is opened the way the standalone MCP server opens it, without external
//! access (`?external_access=off`); the engine's own refusal is checked beside the classifier's.

mod common;

use common::{load_driver, DriverProvider};
use engine_agent::schema::{spec, validate};
use engine_agent::testing::{connection, FakeCatalog, ScriptedApprover};
use engine_agent::{
    AgentAccess, ApprovalDecision, CancelToken, ConnectionKind, Limits, Session, ToolLayer,
    ToolName, ToolOutcome,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ID: &str = "duck";
const SEED: &str = "\
CREATE TABLE users(id INTEGER PRIMARY KEY, email VARCHAR NOT NULL UNIQUE, bio VARCHAR);
CREATE TABLE orders(id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), total DOUBLE, note VARCHAR);
CREATE INDEX orders_user ON orders(user_id);
CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100;
CREATE SCHEMA sales;
CREATE TABLE sales.regions(id INTEGER, name VARCHAR);
INSERT INTO users VALUES (1, 'a@x.io', 'first'), (2, 'b@x.io', NULL), (3, 'c@x.io', 'third');
INSERT INTO orders VALUES (1, 1, 50.5, 'small'), (2, 1, 250, 'big'), (3, 2, 10, NULL);
INSERT INTO sales.regions VALUES (1, 'north');
";

struct Rig {
    layer: Arc<ToolLayer>,
    provider: Arc<DriverProvider>,
    dir: tempfile::TempDir,
}

/// `locked`: the agent's connection asks for an instance without external access.
fn rig_with(access: AgentAccess, approver: Option<Arc<ScriptedApprover>>, locked: bool) -> Rig {
    rig_with_layer(access, approver, locked, |layer| layer)
}

fn rig_with_layer(
    access: AgentAccess,
    approver: Option<Arc<ScriptedApprover>>,
    locked: bool,
    configure: impl FnOnce(ToolLayer) -> ToolLayer,
) -> Rig {
    rig_seeded(access, approver, locked, "", configure)
}

/// [`rig_with_layer`] with `extra` run after the seed.
fn rig_seeded(
    access: AgentAccess,
    approver: Option<Arc<ScriptedApprover>>,
    locked: bool,
    extra: &str,
    configure: impl FnOnce(ToolLayer) -> ToolLayer,
) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agent.duckdb");
    let uri = format!("duckdb://{}", db.display());
    let handle = load_driver("dbengine-duckdb", "");
    let provider = DriverProvider::new(
        handle,
        &[
            (ID, &if locked { format!("{uri}?external_access=off") } else { uri.clone() }),
            ("seed", &uri),
        ],
    );
    // Seed through the driver itself, outside the policy, and close that session: the agent's
    // session is the only one on the file afterwards, so it decides how the instance opens.
    let session = provider.raw_session("seed");
    run_raw(&*session, &format!("{SEED}{extra}")).expect("seed the database");
    session.disconnect();
    let catalog = FakeCatalog::new(vec![connection(ID, ConnectionKind::DuckDb, Some(access))]);
    let mut layer = ToolLayer::new(catalog, provider.clone(), "test")
        .with_transactions()
        .with_limits(Limits::default());
    if let Some(approver) = approver {
        layer = layer.with_approver(approver);
    }
    Rig { layer: Arc::new(configure(layer)), provider, dir }
}

fn rig(access: AgentAccess) -> Rig {
    rig_with(access, None, true)
}

fn writer() -> (Rig, Arc<ScriptedApprover>) {
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    (rig_with(AgentAccess::ReadWrite, Some(approver.clone()), true), approver)
}

/// A statement straight to a session, past the tool layer and its classifier.
fn run_raw(
    session: &dyn Session,
    sql: &str,
) -> Result<engine_agent::result::QueryResult, engine_agent::AgentError> {
    session.run(
        &engine_agent::ports::RunRequest {
            language: engine_agent::Language::Sql,
            text: sql,
            database: None,
            limits: &Limits::default(),
            row_limit: 10,
            confirmed: true,
            read_only: false,
        },
        &CancelToken::new(),
    )
}

impl Rig {
    fn call(&self, tool: &str, args: Value) -> ToolOutcome {
        let outcome = self.layer.call(tool, args);
        // Every result validates against the tool's own output schema.
        if let (ToolOutcome::Success(value), Some(tool)) = (&outcome, ToolName::parse(tool)) {
            validate(&spec(tool).output_schema, value)
                .unwrap_or_else(|error| panic!("{tool:?} output breaks its schema: {error}\n{value}"));
        }
        outcome
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        match self.call(tool, args) {
            ToolOutcome::Success(value) => value,
            ToolOutcome::Failure(error) => panic!("{tool} failed: {error:?}"),
        }
    }

    fn sql(&self, sql: &str) -> ToolOutcome {
        self.call("execute_query", json!({"connection_id": ID, "statement": sql}))
    }

    fn query(&self, sql: &str) -> Value {
        self.ok("execute_query", json!({"connection_id": ID, "statement": sql}))
    }

    fn count(&self, table: &str) -> String {
        self.query(&format!("SELECT count(*) FROM {table}"))["rows"][0][0].as_str().unwrap().to_owned()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn message(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Failure(error) => error.message.clone(),
        ToolOutcome::Success(value) => value.to_string(),
    }
}

fn write_file(path: &Path, text: &str) -> String {
    std::fs::write(path, text).unwrap();
    path.display().to_string()
}

#[test]
fn every_tool_end_to_end() {
    let rig = rig(AgentAccess::Read);
    let listed = rig.ok("list_connections", json!({}));
    assert_eq!(listed["connections"][0]["kind"], "duckdb");
    assert_eq!(listed["connections"][0]["access"], "read");

    let opened = rig.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(opened["language"], "sql");
    assert_eq!(opened["supports_schemas"], true);
    assert_eq!(opened["kind"], "duckdb");

    // The database is the file's name; its schemas are listed below it, like PostgreSQL's.
    let databases = rig.ok("list_namespaces", json!({"connection_id": ID}));
    assert_eq!(databases["namespaces"], json!([{"name": "agent", "kind": "database"}]));
    let schemas = rig.ok("list_namespaces", json!({"connection_id": ID, "database": "agent"}));
    assert_eq!(
        schemas["namespaces"],
        json!([{"name": "main", "kind": "schema"}, {"name": "sales", "kind": "schema"}])
    );
    let missing_schema = rig.call("list_tables", json!({"connection_id": ID, "database": "agent"}));
    assert_eq!(code(&missing_schema), "agent.invalid_arguments");

    let at = |schema: &str| json!({"connection_id": ID, "database": "agent", "schema": schema});
    let tables = rig.ok("list_tables", at("main"));
    let names: Vec<(&str, &str)> = tables["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap(), t["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(names, [("big_orders", "view"), ("orders", "table"), ("users", "table")]);
    let sales = rig.ok("list_tables", at("sales"));
    assert_eq!(sales["tables"], json!([{"name": "regions", "kind": "table"}]));
    let filtered = rig.ok(
        "list_tables",
        json!({"connection_id": ID, "database": "agent", "schema": "main", "filter": "ORD", "limit": 1}),
    );
    assert_eq!(filtered["tables"].as_array().unwrap().len(), 1);
    assert!(filtered["next_cursor"].is_string());

    let mut describe = at("main");
    describe["table"] = json!("orders");
    let orders = rig.ok("describe_table", describe.clone());
    assert_eq!(orders["primary_key"], json!(["id"]));
    let columns: Vec<&str> =
        orders["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(columns, ["id", "user_id", "total", "note"]);
    assert_eq!(orders["columns"][1]["nullable"], false);
    assert_eq!(orders["foreign_keys"][0]["referenced_table"], "users");
    assert!(orders["indexes"].as_array().unwrap().iter().any(|i| i["name"] == "orders_user"));
    assert!(orders["ddl"].as_str().unwrap().contains("CREATE TABLE orders"));
    describe["table"] = json!("nope");
    assert_eq!(code(&rig.call("describe_table", describe)), "describe.not_found");

    let context = rig.ok("get_schema_context", at("main"));
    let text = context["text"].as_str().unwrap();
    assert!(text.starts_with("-- schema: agent.main (duckdb), 3 tables\nTABLE big_orders\n"), "{text}");
    assert!(text.contains("TABLE orders\n  id INTEGER NOT NULL PK"), "{text}");
    assert!(text.contains("  FK (user_id) -> users(id)"), "{text}");
    assert!(text.contains("email VARCHAR NOT NULL UNIQUE"), "{text}");
    assert_eq!(context["within_budget"], true);

    let rows = rig.query("SELECT id, email, bio FROM users ORDER BY id");
    assert_eq!(rows["columns"].as_array().unwrap().len(), 3);
    assert_eq!(rows["rows"][0], json!(["1", "a@x.io", "first"]));
    assert_eq!(rows["rows"][1][2], "NULL");
    assert_eq!(rows["row_count"], 3);
    assert_eq!(rows["truncated"], false);
    // DuckDB's own query forms are reads: `SHOW`, `DESCRIBE`, `WITH`, a table function of the catalog.
    for sql in [
        "SHOW TABLES",
        "DESCRIBE users",
        "WITH big AS (SELECT * FROM orders WHERE total > 100) SELECT count(*) FROM big",
        "SELECT table_name FROM duckdb_tables() ORDER BY 1",
        "SELECT * FROM sales.regions",
    ] {
        let read = rig.query(sql);
        assert_eq!(read["read_only_enforced"], true, "{sql}");
    }

    let plan = rig.ok(
        "explain_query",
        json!({"connection_id": ID, "statement": "SELECT * FROM orders WHERE user_id = 1"}),
    );
    assert_eq!(plan["read_only_enforced"], true);
    let plan_text = plan["rows"].to_string();
    assert!(plan_text.contains("orders") && plan_text.contains("physical_plan"), "{plan_text}");
    assert_eq!(
        code(&rig.call("explain_query", json!({"connection_id": ID, "statement": "EXPLAIN SELECT 1"}))),
        "agent.invalid_arguments"
    );

    // DuckDB has no stored routines the tools list: functions and the like come back empty.
    let routines = rig.ok(
        "list_routines",
        json!({"connection_id": ID, "database": "agent", "schema": "main", "kind": "function"}),
    );
    assert_eq!(routines["routines"], json!([]));
    assert_eq!(
        code(&rig.call(
            "get_routine_source",
            json!({"connection_id": ID, "database": "agent", "schema": "main", "kind": "function", "name": "f"})
        )),
        "agent.unsupported"
    );
    assert_eq!(
        code(&rig.call("execute_redis_command", json!({"connection_id": ID, "command": "GET k"}))),
        "agent.unsupported"
    );

    assert_eq!(rig.ok("close_session", json!({"connection_id": ID}))["closed"], true);
    assert_eq!(
        code(&rig.call("execute_query", json!({"connection_id": ID, "statement": "SELECT 1"}))),
        "agent.not_connected"
    );
}

#[test]
fn schema_only_browses_but_returns_no_rows() {
    let rig = rig(AgentAccess::SchemaOnly);
    rig.ok("open_session", json!({"connection_id": ID}));
    let at = json!({"connection_id": ID, "database": "agent", "schema": "main"});
    rig.ok("list_tables", at.clone());
    rig.ok(
        "describe_table",
        json!({"connection_id": ID, "database": "agent", "schema": "main", "table": "users"}),
    );
    rig.ok("get_schema_context", at);
    for statement in [
        "SELECT * FROM users",
        "SELECT * FROM read_text('/etc/passwd')",
        "COPY users TO '/tmp/x.csv'",
        "ATTACH '/other.db'",
    ] {
        for tool in ["execute_query", "explain_query"] {
            let outcome = rig.call(tool, json!({"connection_id": ID, "statement": statement}));
            assert_eq!(code(&outcome), "agent.access_denied", "{tool}: {statement}");
        }
    }
}

#[test]
fn read_access_refuses_writes_and_session_statements_and_leaves_the_data_alone() {
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let before = rig.count("users");
    for sql in [
        "INSERT INTO users VALUES (9, 'evil@x.io', NULL)",
        "INSERT OR REPLACE INTO users VALUES (1, 'z@x.io', NULL)",
        "INSERT INTO users BY NAME SELECT 9 AS id, 'e@x.io' AS email",
        "UPDATE users SET bio = 'x' WHERE id = 1",
        "DELETE FROM users WHERE id = 1",
        "WITH d AS (SELECT 1) DELETE FROM users WHERE id = 2",
        "CREATE TABLE t(a INTEGER)",
        "CREATE TABLE t AS SELECT * FROM users",
        "CREATE OR REPLACE TABLE users AS SELECT 1 AS id",
        "DROP TABLE users",
        "TRUNCATE users",
        "CHECKPOINT",
        "FORCE CHECKPOINT",
        "VACUUM",
        "CALL pragma_version()",
        "CALL dbgen(sf = 1)",
        "EXPLAIN ANALYZE INSERT INTO users VALUES (9, 'e@x.io', NULL)",
    ] {
        let outcome = rig.sql(sql);
        assert_eq!(code(&outcome), "agent.write_refused", "{sql}: {outcome:?}");
    }
    for sql in [
        "SET enable_external_access = true",
        "SET threads = 1",
        "RESET threads",
        "PRAGMA database_list",
        "USE agent.sales",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
    ] {
        let outcome = rig.sql(sql);
        assert_eq!(code(&outcome), "agent.unsupported", "{sql}: {outcome:?}");
    }
    assert_eq!(rig.count("users"), before);
    assert_eq!(code(&rig.sql("SELECT 1; DELETE FROM users")), "agent.multi_statement");
    assert_eq!(rig.count("users"), before);
    // Words of a statement in a string or a comment are only text.
    rig.query("SELECT 'DELETE FROM users; COPY users TO ''/tmp/x''' AS note -- DROP TABLE users");
}

/// Everything an agent could use to read the user's files, write one, or reach out, in the forms
/// an attack takes. `{csv}` is a real file, `{out}` a path that must stay free, `{dir}` a folder.
const ATTACKS: &[&str] = &[
    "SELECT * FROM read_text('/etc/passwd')",
    "SELECT * FROM read_text('{csv}')",
    "SELECT * FROM read_blob('{csv}')",
    "SELECT * FROM read_csv('{csv}')",
    "SELECT * FROM READ_CSV_AUTO('{csv}')",
    "SELECT * FROM read_json('{csv}')",
    "SELECT * FROM read_ndjson('{csv}')",
    "SELECT * FROM read_json_auto('{csv}')",
    "SELECT * FROM read_parquet('{csv}')",
    "SELECT * FROM parquet_scan('{csv}')",
    "SELECT * FROM parquet_metadata('{csv}')",
    "SELECT * FROM parquet_schema('{csv}')",
    "SELECT * FROM glob('/Users/*/.ssh/*')",
    "SELECT * FROM sniff_csv('{csv}')",
    "SELECT * FROM delta_scan('{csv}')",
    "SELECT * FROM iceberg_scan('{csv}')",
    "SELECT * FROM query_table('{csv}')",
    "SELECT * FROM query('SELECT 1')",
    "SELECT * FROM main.\"read_csv\"('{csv}')",
    "SELECT * FROM read_csv /* hide */ ('{csv}')",
    "FROM '{csv}'",
    "SELECT * FROM '{csv}'",
    "SELECT * FROM '/Users/x/.ssh/id_rsa'",
    "SELECT * FROM 'https://evil.invalid/x.csv'",
    "SELECT * FROM 's3://bucket/x.parquet'",
    "SELECT * FROM \"{csv}\"",
    "select * from\n'{csv}' as t",
    "SELECT * FROM users, '{csv}'",
    "SELECT * FROM users JOIN '{csv}' ON true",
    "SELECT * FROM (SELECT * FROM '{csv}') t",
    "WITH t AS (SELECT * FROM read_csv('{csv}')) SELECT * FROM t",
    "SELECT (SELECT count(*) FROM '{csv}')",
    "SELECT 1 WHERE EXISTS (SELECT 1 FROM read_text('{csv}'))",
    "DESCRIBE '{csv}'",
    "SUMMARIZE '{csv}'",
    "SELECT getenv('HOME')",
    "SELECT * FROM duckdb_secrets()",
    "COPY users TO '{out}'",
    "COPY users TO '{out}' (FORMAT parquet)",
    "COPY (SELECT * FROM users) TO '{out}'",
    "COPY users FROM '{csv}'",
    "copy users from '{csv}' (header)",
    "EXPLAIN ANALYZE COPY users TO '{out}'",
    "EXPORT DATABASE '{dir}'",
    "IMPORT DATABASE '{dir}'",
    "ATTACH '{out}' AS other",
    "ATTACH '{out}' AS other (READ_ONLY)",
    "DETACH other",
    "INSTALL httpfs",
    "FORCE INSTALL httpfs",
    "LOAD httpfs",
    "UPDATE EXTENSIONS",
    "CREATE SECRET s (TYPE s3, KEY_ID 'a', SECRET 'b')",
    "CREATE OR REPLACE PERSISTENT SECRET s (TYPE s3, KEY_ID 'a', SECRET 'b')",
    "DROP SECRET s",
    "CREATE TABLE t AS SELECT * FROM read_csv('{csv}')",
    "CREATE VIEW v AS SELECT * FROM '{csv}'",
    "CREATE MACRO m(f) AS TABLE SELECT * FROM read_csv(f)",
    "INSERT INTO users SELECT * FROM read_csv('{csv}')",
    "CALL read_csv('{csv}')",
];

fn attack(text: &str, rig: &Rig, csv: &str) -> String {
    text.replace("{csv}", csv)
        .replace("{out}", &rig.path("out.dat").display().to_string())
        .replace("{dir}", &rig.path("export").display().to_string())
}

#[test]
fn file_and_environment_access_is_refused_at_every_access_level_and_nothing_is_written() {
    // `locked: false` is the app's assistant, which shares the user's instance: it runs no SQL at all
    // (see the test of that below), so the classifier is only asked on a locked one here.
    for locked in [true] {
        for access in [AgentAccess::Read, AgentAccess::ReadWrite] {
            let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
            let rig = rig_with(access, Some(approver.clone()), locked);
            let csv = write_file(&rig.path("secret.csv"), "a\n1\n");
            rig.ok("open_session", json!({"connection_id": ID}));
            for text in ATTACKS {
                let statement = attack(text, &rig, &csv);
                for flagged in [false, true] {
                    let outcome = rig.call(
                        "execute_query",
                        json!({"connection_id": ID, "statement": statement, "dangerous": flagged}),
                    );
                    assert_eq!(
                        code(&outcome),
                        "agent.blocked_function",
                        "{access} locked={locked}: {statement}: {outcome:?}"
                    );
                }
                let outcome = rig.call("explain_query", json!({"connection_id": ID, "statement": statement}));
                assert!(
                    matches!(code(&outcome), "agent.blocked_function" | "agent.invalid_arguments"),
                    "{access}: explain {statement}: {outcome:?}"
                );
                let batch = rig.call(
                    "execute_batch",
                    json!({"connection_id": ID, "script": format!("SELECT 1; {statement}")}),
                );
                assert_eq!(code(&batch), "agent.blocked_function", "{access}: batch {statement}: {batch:?}");
            }
            // The approver was never asked, and nothing reached the file system.
            assert!(approver.asked.lock().unwrap().is_empty(), "{access} locked={locked}");
            assert!(!rig.path("out.dat").exists() && !rig.path("export").exists());
            assert_eq!(rig.count("users"), "3");
        }
    }
    // The refusal says what to do instead.
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let refusal = message(&rig.sql("SELECT * FROM read_text('/etc/passwd')"));
    assert!(
        refusal.contains("`read_text` is blocked for agents") && refusal.contains("open a file in tablex"),
        "{refusal}"
    );
}

#[test]
fn the_engine_refuses_files_to_an_agents_session_even_when_the_classifier_is_bypassed() {
    let rig = rig(AgentAccess::Read);
    let csv = write_file(&rig.path("secret.csv"), "a\n1\n");
    // A session that asks for no external access (what the agent's connection does), run past the
    // classifier: the engine itself says no, for every route to a file.
    let locked = rig.provider.raw_session(ID);
    for text in ATTACKS.iter().filter(|text| !text.contains("{dir}") && !text.contains("query(")) {
        let statement = attack(text, &rig, &csv);
        let outcome = run_raw(&*locked, &statement);
        // Only a statement about no file (`getenv`, `duckdb_secrets()`, a macro that is only
        // defined, `DETACH` of nothing) may still run, and none of them reads or writes one.
        if outcome.is_ok() {
            assert!(
                ["getenv", "duckdb_secrets", "CREATE MACRO"].iter().any(|word| statement.contains(word)),
                "the engine ran: {statement}"
            );
        }
    }
    assert!(!rig.path("out.dat").exists());
    // Nothing can switch it off again.
    for sql in [
        "SET enable_external_access = true",
        "SET autoinstall_known_extensions = true",
        "RESET enable_external_access",
    ] {
        assert!(run_raw(&*locked, sql).is_err(), "{sql}");
    }
    // The database itself is fine.
    assert_eq!(run_raw(&*locked, "SELECT count(*) FROM users").unwrap().rows[0][0], "3");
    locked.disconnect();
    // The same text on a session with file access (the user's own) does read the file: the
    // refusal above is the agent's, not a broken file.
    let user = rig.provider.raw_session("seed");
    let read = run_raw(&*user, &format!("SELECT * FROM read_csv('{csv}')")).expect("a user session reads the file");
    assert_eq!(read.rows, [["1"]]);
    user.disconnect();
}

/// tablex's Open Data File makes a view over the user's file; a file database the user keeps such a
/// view in must not become a way for an agent to read the file. The agent's session is locked, and the
/// engine refuses the file when the view is read (CSV, JSON and Parquet alike).
#[test]
fn a_view_over_a_file_in_the_users_database_cannot_be_read_by_an_agents_session() {
    let rig = rig(AgentAccess::ReadWrite);
    let csv = write_file(&rig.path("people.csv"), "name,age\nann,30\nbob,41\n");
    let (json, parquet) = (rig.path("people.json").display().to_string(), rig.path("people.parquet").display().to_string());
    // The user's own session (with file access) writes the other two formats and makes the views.
    let user = rig.provider.raw_session("seed");
    for sql in [
        format!("COPY (SELECT * FROM read_csv_auto('{csv}')) TO '{json}' (FORMAT json)"),
        format!("COPY (SELECT * FROM read_csv_auto('{csv}')) TO '{parquet}' (FORMAT parquet)"),
        format!("CREATE VIEW people_csv AS SELECT * FROM read_csv_auto('{csv}')"),
        format!("CREATE VIEW people_json AS SELECT * FROM read_json_auto('{json}')"),
        format!("CREATE VIEW people_parquet AS SELECT * FROM read_parquet('{parquet}')"),
    ] {
        run_raw(&*user, &sql).unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    for view in ["people_csv", "people_json", "people_parquet"] {
        let read = run_raw(&*user, &format!("SELECT count(*) FROM {view}")).unwrap();
        assert_eq!(read.rows[0][0], "2", "the user reads {view}");
    }
    user.disconnect();

    // The agent's session: a plain SELECT of a view passes the classifier, and the engine says no.
    rig.ok("open_session", json!({"connection_id": ID}));
    for view in ["people_csv", "people_json", "people_parquet"] {
        for statement in [format!("SELECT * FROM {view}"), format!("SELECT count(*) FROM {view}")] {
            let outcome = rig.sql(&statement);
            assert!(matches!(outcome, ToolOutcome::Failure(_)), "{statement}: {outcome:?}");
            let text = message(&outcome);
            assert!(
                text.contains("disabled by configuration") || text.contains("external access"),
                "the engine's own refusal, not a bypass: {statement}: {text}"
            );
            assert!(text.contains("Permission Error"), "a permission error, not a row or another failure: {text}");
        }
    }
    // The same session reads its own tables as before.
    assert_eq!(rig.count("users"), "3");
}

#[test]
fn read_write_runs_the_changes_the_user_approves() {
    let (rig, approver) = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let inserted = rig.query("INSERT INTO users VALUES (4, 'd@x.io', 'fourth')");
    assert_eq!(inserted["affected_rows"], 1);
    assert_eq!(rig.count("users"), "4");
    rig.query("CREATE TABLE kv (k INTEGER PRIMARY KEY, v VARCHAR)");
    rig.query("INSERT INTO kv VALUES (1, 'a')");
    assert_eq!(rig.query("INSERT OR REPLACE INTO kv VALUES (1, 'b')")["affected_rows"], 1);
    assert_eq!(rig.query("SELECT v FROM kv")["rows"], json!([["b"]]));
    assert_eq!(rig.query("UPDATE users SET bio = 'x' WHERE email = 'd@x.io'")["affected_rows"], 1);
    assert_eq!(rig.query("CREATE TABLE copy_of_users AS SELECT * FROM users")["affected_rows"], 4);
    assert_eq!(rig.count("copy_of_users"), "4");
    // Additive DDL needs no dangerous flag, nor does a checkpoint.
    for sql in [
        "ALTER TABLE copy_of_users ADD COLUMN age INTEGER",
        "CREATE SCHEMA extra",
        "CREATE INDEX copy_email ON copy_of_users (email)",
        "CHECKPOINT",
    ] {
        rig.query(sql);
    }
    assert_eq!(approver.asked.lock().unwrap().len(), 10);
    // What destroys data or structure needs the flag.
    for sql in ["DELETE FROM copy_of_users", "DROP TABLE copy_of_users", "CREATE OR REPLACE TABLE users AS SELECT 1 AS id"] {
        assert_eq!(code(&rig.sql(sql)), "agent.dangerous_refused", "{sql}");
    }
    let deleted = rig.ok(
        "execute_query",
        json!({"connection_id": ID, "statement": "DELETE FROM copy_of_users", "dangerous": true}),
    );
    assert_eq!(deleted["affected_rows"], 4);
    // The tables the agent made are in the catalog it browses.
    let tables = rig.ok("list_tables", json!({"connection_id": ID, "database": "agent", "schema": "main"}));
    assert!(tables.to_string().contains("copy_of_users"));
    // A change the user denies does not run.
    let deny = ScriptedApprover::new(ApprovalDecision::Deny { reason: Some("no".into()) }, false);
    let denied = rig_with(AgentAccess::ReadWrite, Some(deny), true);
    denied.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(code(&denied.sql("DELETE FROM users WHERE id = 1")), "agent.write_refused");
    assert_eq!(denied.count("users"), "3");
}

fn begin(rig: &Rig) -> String {
    rig.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"].as_str().unwrap().to_owned()
}

fn in_tx(rig: &Rig, tx: &str, sql: &str) -> ToolOutcome {
    rig.call("execute_query", json!({"connection_id": ID, "transaction_id": tx, "statement": sql}))
}

fn count_in(rig: &Rig, tx: &str, table: &str) -> String {
    match in_tx(rig, tx, &format!("SELECT count(*) FROM {table}")) {
        ToolOutcome::Success(value) => value["rows"][0][0].as_str().unwrap().to_owned(),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
}

#[test]
fn a_transaction_is_rolled_back_or_committed_on_a_session_of_its_own() {
    let (rig, _approver) = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    assert!(!in_tx(&rig, &tx, "INSERT INTO users VALUES (5, 'tx@x.io', NULL)").is_error());
    // A read inside it sees the row (run in the transaction as it is), the shared session does not.
    match in_tx(&rig, &tx, "SELECT email FROM users WHERE id = 5") {
        ToolOutcome::Success(value) => assert_eq!(value["rows"], json!([["tx@x.io"]])),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
    assert_eq!(count_in(&rig, &tx, "users"), "4");
    assert_eq!(rig.count("users"), "3");
    // The files and the transaction's own end stay out of reach inside it too.
    for sql in ["COPY users TO '/tmp/tablex-agent.csv'", "SELECT * FROM read_text('/etc/passwd')", "SELECT * FROM '/etc/passwd'"] {
        let refused = in_tx(&rig, &tx, sql);
        assert!(matches!(code(&refused), "agent.unsupported" | "agent.blocked_function"), "{sql}: {refused:?}");
    }
    assert_eq!(code(&in_tx(&rig, &tx, "COMMIT")), "agent.unsupported");
    let rolled = rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rolled["rolled_back"], true);
    assert_eq!(rolled["changes"], 1);
    assert_eq!(rig.count("users"), "3");
    assert_eq!(code(&in_tx(&rig, &tx, "SELECT 1")), "agent.transaction_gone");

    let tx = begin(&rig);
    assert!(!in_tx(&rig, &tx, "INSERT INTO users VALUES (6, 'kept@x.io', NULL)").is_error());
    let committed = rig.ok("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(committed["committed"], true);
    assert_eq!(rig.count("users"), "4");
}

#[test]
fn a_batch_in_a_transaction_commits_or_rolls_back_as_one() {
    let (rig, approver) = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let run = |script: &str| {
        rig.call("execute_batch", json!({"connection_id": ID, "script": script, "use_transaction": true}))
    };
    let committed = run(
        "INSERT INTO users VALUES (7, 'b1@x.io', NULL); INSERT INTO users VALUES (8, 'b2@x.io', NULL); \
         SELECT count(*) FROM users",
    );
    assert!(!committed.is_error(), "{committed:?}");
    assert_eq!(rig.count("users"), "5");
    // The second statement fails (a duplicate key): the first is rolled back with it.
    let failed = run("INSERT INTO users VALUES (9, 'b3@x.io', NULL); INSERT INTO users VALUES (7, 'dup@x.io', NULL)");
    match &failed {
        ToolOutcome::Success(value) => assert_eq!(value["transaction"], "rolled_back", "{value}"),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
    assert_eq!(rig.count("users"), "5");
    assert_eq!(approver.asked.lock().unwrap().len(), 2, "one approval per script");
    // A script with a file in it is refused whole, before anything runs.
    let refused = run("INSERT INTO users VALUES (10, 'z@x.io', NULL); COPY users TO '/tmp/x.csv'");
    assert!(matches!(code(&refused), "agent.unsupported" | "agent.blocked_function"), "{refused:?}");
    assert_eq!(rig.count("users"), "5");
}

#[test]
fn a_pinned_session_reads_but_takes_no_set_use_or_file() {
    let rig = rig_with_layer(AgentAccess::Read, None, true, ToolLayer::with_pinned_sessions);
    let pinned = rig.ok("open_session", json!({"connection_id": ID, "pinned": true}));
    let session = pinned["session_id"].as_str().unwrap().to_owned();
    let run = |sql: &str| {
        rig.call("execute_query", json!({"connection_id": ID, "session_id": session, "statement": sql}))
    };
    assert!(!run("SELECT count(*) FROM users").is_error());
    for sql in [
        "SET enable_external_access = true",
        "SET threads = 1",
        "USE agent.sales",
        "PRAGMA enable_profiling",
        "ATTACH '/other.db'",
        "SELECT * FROM read_text('/etc/passwd')",
    ] {
        let outcome = run(sql);
        assert!(matches!(code(&outcome), "agent.unsupported" | "agent.blocked_function"), "{sql}: {outcome:?}");
    }
    rig.ok("close_session", json!({"session_id": session}));
}

/// What the classifier's denylist forgot or could forget: SQL carried in a string and run on a
/// connection of its own, log and profile files, the places of DuckDB's grammar where a string is a
/// file, and the statements of DuckDB 2.0 that open or register something. `{csv}` is a real file
/// that holds a marker, `{out}` a path that must stay free.
const BYPASSES: &[&str] = &[
    "SELECT * FROM json_execute_serialized_sql(json_serialize_sql('SELECT * FROM read_text(''{csv}'')'))",
    "SELECT json_execute_serialized_sql(json_serialize_sql('SELECT * FROM read_text(''{csv}'')'))",
    "CALL enable_logging(storage = 'file', storage_path = '{out}')",
    "SELECT * FROM enable_logging(storage = 'file', storage_path = '{out}')",
    "CALL enable_profiling(format = 'json', save_location = '{out}')",
    "SELECT * FROM disable_logging()",
    "PRAGMA enable_profiling = 'json'",
    "PRAGMA profiling_output = '{out}'",
    "SELECT * FROM a: '{csv}'",
    "SELECT * FROM users, b: '{csv}'",
    "SHOW '{csv}'",
    "PIVOT_WIDER '{csv}' ON a USING sum(b)",
    "PIVOT_LONGER '{csv}' ON a INTO NAME n VALUE v",
    "SELECT * FROM users JOIN BY (TYPE inner) '{csv}'",
    "MERGE INTO users USING '{csv}' ON true WHEN MATCHED THEN DELETE",
    "SELECT * FROM data.csv",
    "SELECT * FROM \"a\".\"csv\"",
    "WITH x AS (COPY users TO '{out}') SELECT 1",
    "WITH x AS MATERIALIZED (ATTACH '{out}' AS o) SELECT 1",
    "WITH x AS (INSTALL httpfs) SELECT 1",
    "((ATTACH '{out}' AS o))",
    "CONNECT '{out}' (TYPE duckdb)",
    "DISCONNECT",
    "CREATE EXTENSION REPOSITORY r URL 'https://evil.invalid'",
    "CREATE OR REPLACE EXTENSION REPOSITORY r URL 'https://evil.invalid'",
    "CREATE EXTERNAL RESOURCE r (TYPE http)",
    "REGISTER EXTERNAL RESOURCE r",
    "DESTROY EXTERNAL RESOURCE r",
    "ATTACH TO EXTERNAL RESOURCE r",
    "CONNECT TO EXTERNAL RESOURCE r",
    // The lexer of a PostgreSQL parser reads the `'` in a backtick name as a string, so a statement
    // that follows is hidden from it.
    "SELECT 1 AS `'`; SELECT * FROM read_text('{csv}'); SELECT 1 AS `'`",
    "SELECT 1 AS `'`; COPY users TO '{out}'; SELECT 1 AS `'`",
];

const MARKER: &str = "TOP-SECRET-MARKER";

#[test]
fn what_a_denylist_could_forget_is_blocked_by_the_classifier_and_the_engine_on_a_locked_instance() {
    let rig = rig(AgentAccess::Read);
    let csv = write_file(&rig.path("secret.csv"), &format!("a\n{MARKER}\n"));
    rig.ok("open_session", json!({"connection_id": ID}));
    let raw = rig.provider.raw_session(ID);
    for text in BYPASSES {
        let statement = attack(text, &rig, &csv);
        // The classifier, at the tool.
        let outcome = rig.call("execute_query", json!({"connection_id": ID, "statement": statement}));
        assert!(
            matches!(code(&outcome), "agent.blocked_function" | "agent.unsupported" | "agent.multi_statement"),
            "{statement}: {outcome:?}"
        );
        assert!(!message(&outcome).contains(MARKER), "{statement}");
        // The engine, past the classifier: nothing is read or written, whatever it answers.
        if let Ok(result) = run_raw(&*raw, &statement) {
            assert!(!format!("{:?}", result.rows).contains(MARKER), "the engine read the file: {statement}");
        }
        assert!(!rig.path("out.dat").exists(), "{statement}");
    }
    assert_eq!(rig.count("users"), "3");
}

#[test]
fn an_unlocked_duckdb_session_runs_no_sql_for_an_agent_and_still_browses() {
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let rig = rig_with_layer(AgentAccess::ReadWrite, Some(approver.clone()), false, ToolLayer::with_pinned_sessions);
    let csv = write_file(&rig.path("secret.csv"), &format!("a\n{MARKER}\n"));
    let shown = |outcome: &ToolOutcome| {
        assert_eq!(code(outcome), "agent.unsupported", "{outcome:?}");
        let text = message(outcome);
        assert!(text.contains("MCP client") && text.contains("file access"), "{text}");
        assert!(!text.contains(MARKER));
    };
    rig.ok("open_session", json!({"connection_id": ID}));
    // Every statement tool, with a harmless statement and with every attack.
    let statements: Vec<String> = ["SELECT 1", "SELECT count(*) FROM users"]
        .iter()
        .map(|text| (*text).to_owned())
        .chain(ATTACKS.iter().chain(BYPASSES).map(|text| attack(text, &rig, &csv)))
        .collect();
    for statement in &statements {
        shown(&rig.call("execute_query", json!({"connection_id": ID, "statement": statement})));
        shown(&rig.call("explain_query", json!({"connection_id": ID, "statement": statement})));
        shown(&rig.call("execute_batch", json!({"connection_id": ID, "script": format!("{statement}; SELECT 1")})));
        shown(&rig.call("execute_batch", json!({"connection_id": ID, "script": statement, "use_transaction": true})));
    }
    // A transaction and a pinned session are not opened at all.
    shown(&rig.call("begin_transaction", json!({"connection_id": ID})));
    shown(&rig.call("open_session", json!({"connection_id": ID, "pinned": true})));
    assert!(approver.asked.lock().unwrap().is_empty());
    assert!(!rig.path("out.dat").exists() && !rig.path("export").exists());
    // The attack of a backtick name has no more way in.
    shown(&rig.sql("SELECT 1 AS `'`; COMMIT; DELETE FROM users; SELECT 1 AS `'`"));
    // What goes through the driver's own catalog calls, not agent SQL, still works.
    let at = json!({"connection_id": ID, "database": "agent", "schema": "main"});
    let tables = rig.ok("list_tables", at.clone());
    assert!(tables.to_string().contains("users"), "{tables}");
    let mut describe = at.clone();
    describe["table"] = json!("users");
    let described = rig.ok("describe_table", describe);
    assert!(described.to_string().contains("email"), "{described}");
    rig.ok("get_schema_context", at);
    rig.ok("list_namespaces", json!({"connection_id": ID}));
    // And nothing changed.
    let user = rig.provider.raw_session("seed");
    assert_eq!(run_raw(&*user, "SELECT count(*) FROM users").unwrap().rows[0][0], "3");
    user.disconnect();
}

/// A text the SQL lexer takes for one statement and DuckDB splits in several, past the classifier
/// and the READ ONLY scope: the backtick attack, then a `COMMIT` and a `DELETE` of the user's data.
#[test]
fn a_backtick_name_cannot_smuggle_a_commit_and_a_delete_past_the_read_scope() {
    let attack = "SELECT 1 AS `'`; COMMIT; DELETE FROM users; SELECT 1 AS `'`";
    // The tool layer on a locked instance, at every access level.
    for access in [AgentAccess::Read, AgentAccess::ReadWrite] {
        let rig = rig(access);
        rig.ok("open_session", json!({"connection_id": ID}));
        let outcome = rig.sql(attack);
        assert_eq!(code(&outcome), "agent.multi_statement", "{access}: {outcome:?}");
        let batch = rig.call("execute_batch", json!({"connection_id": ID, "script": attack}));
        assert!(batch.is_error(), "{access}: {batch:?}");
        assert_eq!(rig.count("users"), "3", "{access}");
    }
    // The driver itself, in its READ ONLY scope, however the text got there: refused, nothing ran.
    let rig = rig(AgentAccess::Read);
    let raw = rig.provider.raw_session(ID);
    let request = engine_agent::ports::RunRequest {
        language: engine_agent::Language::Sql,
        text: attack,
        database: None,
        limits: &Limits::default(),
        row_limit: 10,
        confirmed: false,
        read_only: true,
    };
    assert!(raw.run(&request, &CancelToken::new()).is_err());
    assert_eq!(run_raw(&*raw, "SELECT count(*) FROM users").unwrap().rows[0][0], "3");
}

/// DuckDB rewrites some Unicode spaces to ASCII ones before it reads a statement, and the lexer
/// takes them for word bytes: this text is one read to the lexer and a `DELETE` after a CTE to
/// DuckDB. An agent has no use for such a character in code, so it is refused at every access level,
/// also inside a transaction (where no READ ONLY scope stands behind the classifier).
#[test]
fn a_unicode_space_cannot_hide_a_delete_in_a_read_for_an_agent() {
    let attack = "WITH c AS (SELECT \u{a0}e'\\' AS a, x ') DELETE FROM users --') SELECT * FROM c";
    for access in [AgentAccess::Read, AgentAccess::ReadWrite] {
        let rig = rig_with(access, Some(ScriptedApprover::new(ApprovalDecision::Approve, true)), true);
        rig.ok("open_session", json!({"connection_id": ID}));
        for tool in ["execute_query", "explain_query"] {
            let outcome = rig.call(tool, json!({"connection_id": ID, "statement": attack}));
            assert_eq!(code(&outcome), "agent.unsupported", "{access} {tool}: {outcome:?}");
            assert!(message(&outcome).contains("Unicode space"), "{}", message(&outcome));
        }
        let batch = rig.call("execute_batch", json!({"connection_id": ID, "script": format!("{attack}; SELECT 1")}));
        assert!(batch.is_error(), "{access}: {batch:?}");
        assert_eq!(rig.count("users"), "3", "{access}");
    }
    // Inside a transaction.
    let (rig, approver) = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    let outcome = in_tx(&rig, &tx, attack);
    assert!(outcome.is_error(), "{outcome:?}");
    assert_eq!(count_in(&rig, &tx, "users"), "3");
    assert!(approver.asked.lock().unwrap().is_empty(), "the user was not asked about it");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rig.count("users"), "3");
    // The ASCII twin is an honest read and runs; a Unicode space in a string is data.
    assert_eq!(rig.query("SELECT 'a\u{a0}b' AS s")["rows"][0][0], "a\u{a0}b");
    let honest = "WITH c AS (SELECT 1 AS a) SELECT * FROM c";
    assert_eq!(rig.query(honest)["rows"][0][0], "1");
}

/// A database file that defines a macro called `current_setting` answers a bare call of the
/// function with whatever it likes. The agent trusts the driver's own report of the lock, so a file
/// cannot make an unlocked instance look locked (the assistant stays refused), and a locked one
/// stays usable whatever the file defines.
#[test]
fn a_macro_in_the_file_cannot_make_an_unlocked_instance_look_locked() {
    let liar = "CREATE MACRO current_setting(name) AS CASE name WHEN 'enable_external_access' THEN false ELSE true END;";
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let rig = rig_seeded(AgentAccess::ReadWrite, Some(approver.clone()), false, liar, ToolLayer::with_pinned_sessions);
    rig.ok("open_session", json!({"connection_id": ID}));
    // The macro really answers in place of the setting on this file.
    let user = rig.provider.raw_session("seed");
    let answered = run_raw(&*user, "SELECT current_setting('enable_external_access')").unwrap();
    assert_eq!(answered.rows[0][0], "false", "the macro shadows the built-in function: {answered:?}");
    user.disconnect();
    for statement in ["SELECT 1", "SELECT count(*) FROM users"] {
        let outcome = rig.sql(statement);
        assert_eq!(code(&outcome), "agent.unsupported", "{statement}: {outcome:?}");
        assert!(message(&outcome).contains("file access"), "{}", message(&outcome));
    }
    assert!(rig.call("begin_transaction", json!({"connection_id": ID})).is_error());
    assert!(approver.asked.lock().unwrap().is_empty());
    // Browsing still works.
    rig.ok("list_namespaces", json!({"connection_id": ID}));

    // A locked instance of such a file runs the agent's SQL.
    let locked = rig_seeded(AgentAccess::Read, None, true, liar, |layer| layer);
    locked.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(locked.count("users"), "3");
    let denier = "CREATE MACRO current_setting(name) AS CASE name WHEN 'enable_external_access' THEN true ELSE false END;";
    let locked = rig_seeded(AgentAccess::Read, None, true, denier, |layer| layer);
    locked.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(locked.count("users"), "3");
}
