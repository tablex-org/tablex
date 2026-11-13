//! The tool layer end to end over the REAL SQLite driver: the cdylib is built
//! by cargo, loaded through `engine-driver-host`, and every tool runs against
//! a temporary database file.

mod common;

use common::{load_driver, DriverProvider};
use engine_agent::schema::{spec, validate};
use engine_agent::testing::{connection, FakeCatalog, ScriptedApprover};
use engine_agent::{
    read_entries, AgentAccess, ApprovalDecision, AuditConfig, AuditLog, CancelToken,
    ConnectionKind, Limits, ToolLayer, ToolName, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ID: &str = "lite";
const SEED: &str = "\
CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, bio TEXT);
CREATE TABLE orders(id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, total REAL, note TEXT);
CREATE INDEX orders_user ON orders(user_id);
CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100;
INSERT INTO users(email, bio) VALUES ('a@x.io', 'first'), ('b@x.io', NULL), ('c@x.io', 'third');
INSERT INTO orders(user_id, total, note) VALUES (1, 50.5, 'small'), (1, 250, 'big'), (2, 10, NULL);
CREATE TABLE nums AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 5000) SELECT x AS n, 'row ' || x AS label FROM c;
CREATE TRIGGER orders_audit AFTER DELETE ON orders BEGIN SELECT 1; END;
CREATE TRIGGER users_touch BEFORE UPDATE ON users BEGIN SELECT 1; END;
";
/// Runs far longer than any test waits, unless it is cancelled.
const LONG_QUERY: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
     WHERE x < 4000000000) SELECT count(*) FROM c";

struct Rig {
    layer: Arc<ToolLayer>,
    catalog: Arc<FakeCatalog>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig_with(
    access: Option<AgentAccess>,
    approver: Option<Arc<ScriptedApprover>>,
    limits: Limits,
    secrets: Vec<engine_agent::SecretText>,
) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agent.db");
    let handle = load_driver("dbengine-sqlite", "");
    let provider = Arc::new(DriverProvider {
        handle,
        uris: std::sync::Mutex::new([(ID.to_owned(), format!("sqlite://{}", db.display()))].into()),
        secrets,
    });
    // Seed through the driver itself, outside the policy.
    {
        let session = provider.raw_session(ID);
        let limits = Limits::default();
        session
            .run(
                &engine_agent::ports::RunRequest {
                    language: engine_agent::Language::Sql,
                    text: SEED,
                    database: None,
                    limits: &limits,
                    row_limit: 10,
                    confirmed: true,
                    read_only: false,
                },
                &CancelToken::new(),
            )
            .expect("seed the database");
        session.disconnect();
    }
    let audit_path = dir.path().join("audit").join("audit.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let catalog = FakeCatalog::new(vec![connection(ID, ConnectionKind::Sqlite, access)]);
    let mut layer = ToolLayer::new(catalog.clone(), provider, "test")
        .with_transactions()
        .with_audit(Arc::clone(&audit))
        .with_limits(limits);
    if let Some(approver) = approver {
        layer = layer.with_approver(approver);
    }
    Rig {
        layer: Arc::new(layer),
        catalog,
        audit,
        audit_path,
        _dir: dir,
    }
}

fn rig(access: AgentAccess) -> Rig {
    rig_with(Some(access), None, Limits::default(), Vec::new())
}

impl Rig {
    fn call(&self, tool: &str, args: Value) -> ToolOutcome {
        let outcome = self.layer.call(tool, args);
        // Every result validates against the tool's own output schema.
        if let (ToolOutcome::Success(value), Some(tool)) = (&outcome, ToolName::parse(tool)) {
            validate(&spec(tool).output_schema, value).unwrap_or_else(|error| {
                panic!("{tool:?} output breaks its schema: {error}\n{value}")
            });
        }
        outcome
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        match self.call(tool, args) {
            ToolOutcome::Success(value) => value,
            ToolOutcome::Failure(error) => panic!("{tool} failed: {error:?}"),
        }
    }

    fn query(&self, sql: &str) -> Value {
        self.ok(
            "execute_query",
            json!({"connection_id": ID, "statement": sql}),
        )
    }

    fn count(&self, table: &str) -> String {
        self.query(&format!("SELECT count(*) FROM {table}"))["rows"][0][0]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

#[test]
fn triggers_are_listed_and_their_source_read() {
    let rig = rig(AgentAccess::SchemaOnly);
    rig.ok("open_session", json!({"connection_id": ID}));
    // The single database is filled in; there is no schema level.
    let listed = rig.ok(
        "list_routines",
        json!({"connection_id": ID, "kind": "trigger"}),
    );
    let names: Vec<(&str, &str)> = listed["routines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap(), t["table"].as_str().unwrap()))
        .collect();
    assert_eq!(names, [("orders_audit", "orders"), ("users_touch", "users")]);
    assert_eq!(listed["routines"][0]["ref"]["kind"], "trigger");
    assert!(listed.get("next_cursor").is_none());

    let filtered = rig.ok(
        "list_routines",
        json!({"connection_id": ID, "kind": "trigger", "filter": "TOUCH"}),
    );
    assert_eq!(filtered["routines"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["routines"][0]["name"], "users_touch");
    let first = rig.ok(
        "list_routines",
        json!({"connection_id": ID, "kind": "trigger", "limit": 1}),
    );
    assert_eq!(first["routines"][0]["name"], "orders_audit");
    let second = rig.ok(
        "list_routines",
        json!({"connection_id": ID, "kind": "trigger", "limit": 1, "cursor": first["next_cursor"]}),
    );
    assert_eq!(second["routines"][0]["name"], "users_touch");
    assert!(second.get("next_cursor").is_none());

    // The listed ref reads back the trigger's own statement.
    let source = rig.ok(
        "get_routine_source",
        json!({"connection_id": ID, "ref": listed["routines"][0]["ref"]}),
    );
    assert_eq!(source["kind"], "trigger");
    assert_eq!(source["name"], "orders_audit");
    assert_eq!(source["truncated"], false);
    assert!(
        source["source"].as_str().unwrap().contains("CREATE TRIGGER orders_audit AFTER DELETE ON orders"),
        "{source}"
    );
    let by_name = rig.ok(
        "get_routine_source",
        json!({"connection_id": ID, "kind": "trigger", "name": "users_touch"}),
    );
    assert!(by_name["source"].as_str().unwrap().contains("users_touch"), "{by_name}");
    assert_eq!(
        code(&rig.call(
            "get_routine_source",
            json!({"connection_id": ID, "kind": "trigger", "name": "gone"})
        )),
        "describe.not_found"
    );

    // SQLite has triggers only.
    for kind in ["function", "procedure", "event"] {
        let none = rig.ok("list_routines", json!({"connection_id": ID, "kind": kind}));
        assert_eq!(none["routines"], json!([]), "{kind}");
    }
    assert_eq!(
        code(&rig.call(
            "get_routine_source",
            json!({"connection_id": ID, "kind": "function", "name": "f"})
        )),
        "agent.unsupported"
    );
}

#[test]
fn every_tool_end_to_end() {
    let rig = rig(AgentAccess::Read);
    let listed = rig.ok("list_connections", json!({}));
    assert_eq!(listed["connections"][0]["kind"], "sqlite");
    assert_eq!(listed["connections"][0]["access"], "read");

    let opened = rig.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(opened["language"], "sql");
    assert_eq!(opened["supports_schemas"], false);

    let databases = rig.ok("list_namespaces", json!({"connection_id": ID}));
    assert_eq!(
        databases["namespaces"],
        json!([{"name": "main", "kind": "database"}])
    );
    let schemas = rig.ok(
        "list_namespaces",
        json!({"connection_id": ID, "database": "main"}),
    );
    assert_eq!(schemas["namespaces"], json!([]));

    // The single database is filled in.
    let tables = rig.ok("list_tables", json!({"connection_id": ID}));
    let names: Vec<(&str, &str)> = tables["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap(), t["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        names,
        [
            ("big_orders", "view"),
            ("nums", "table"),
            ("orders", "table"),
            ("users", "table")
        ]
    );
    let filtered = rig.ok(
        "list_tables",
        json!({"connection_id": ID, "filter": "ORD", "limit": 1}),
    );
    assert_eq!(filtered["tables"].as_array().unwrap().len(), 1);
    assert!(filtered["next_cursor"].is_string());

    let orders = rig.ok(
        "describe_table",
        json!({"connection_id": ID, "table": "orders"}),
    );
    assert_eq!(orders["primary_key"], json!(["id"]));
    let columns: Vec<&str> = orders["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(columns, ["id", "user_id", "total", "note"]);
    assert_eq!(orders["columns"][1]["nullable"], false);
    assert_eq!(orders["foreign_keys"][0]["referenced_table"], "users");
    assert_eq!(orders["foreign_keys"][0]["on_delete"], "CASCADE");
    assert!(orders["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["name"] == "orders_user"));
    assert!(orders["ddl"]
        .as_str()
        .unwrap()
        .contains("CREATE TABLE orders"));
    let missing = rig.call(
        "describe_table",
        json!({"connection_id": ID, "table": "nope"}),
    );
    assert_eq!(code(&missing), "describe.not_found");

    let context = rig.ok("get_schema_context", json!({"connection_id": ID}));
    let text = context["text"].as_str().unwrap();
    assert!(
        text.starts_with("-- schema: main (sqlite), 4 tables\nTABLE big_orders\n"),
        "{text}"
    );
    assert!(
        text.contains("TABLE orders\n  id INTEGER NOT NULL PK"),
        "{text}"
    );
    assert!(
        text.contains("  FK (user_id) -> users(id) ON DELETE CASCADE"),
        "{text}"
    );
    assert!(text.contains("email TEXT NOT NULL UNIQUE"), "{text}");
    assert_eq!(context["within_budget"], true);
    assert_eq!(
        rig.ok("get_schema_context", json!({"connection_id": ID})),
        context,
        "stable between calls"
    );

    let rows = rig.query("SELECT id, email, bio FROM users ORDER BY id");
    assert_eq!(rows["columns"].as_array().unwrap().len(), 3);
    assert_eq!(rows["rows"][0], json!(["1", "a@x.io", "first"]));
    assert_eq!(rows["rows"][1][2], "NULL");
    assert_eq!(rows["row_count"], 3);
    assert_eq!(rows["truncated"], false);

    let plan = rig.ok(
        "explain_query",
        json!({"connection_id": ID, "statement": "SELECT * FROM orders WHERE user_id = 1"}),
    );
    assert!(plan["row_count"].as_u64().unwrap() >= 1);
    let plan_text = plan["rows"].to_string();
    assert!(
        plan_text.contains("orders_user") || plan_text.contains("SEARCH"),
        "{plan_text}"
    );

    let redis = rig.call(
        "execute_redis_command",
        json!({"connection_id": ID, "command": "GET k"}),
    );
    assert_eq!(code(&redis), "agent.unsupported");

    assert_eq!(
        rig.ok("close_session", json!({"connection_id": ID}))["closed"],
        true
    );
    assert_eq!(
        code(&rig.call(
            "execute_query",
            json!({"connection_id": ID, "statement": "SELECT 1"})
        )),
        "agent.not_connected"
    );
}

#[test]
fn schema_only_browses_but_returns_no_rows() {
    let rig = rig(AgentAccess::SchemaOnly);
    rig.ok("open_session", json!({"connection_id": ID}));
    rig.ok("list_tables", json!({"connection_id": ID}));
    rig.ok(
        "describe_table",
        json!({"connection_id": ID, "table": "users"}),
    );
    rig.ok("get_schema_context", json!({"connection_id": ID}));
    for tool in [
        (
            "execute_query",
            json!({"connection_id": ID, "statement": "SELECT * FROM users"}),
        ),
        (
            "explain_query",
            json!({"connection_id": ID, "statement": "SELECT * FROM users"}),
        ),
    ] {
        assert_eq!(code(&rig.call(tool.0, tool.1)), "agent.access_denied");
    }
}

#[test]
fn read_access_refuses_writes_and_leaves_the_data_alone() {
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let before = rig.count("users");
    for sql in [
        "INSERT INTO users(email) VALUES ('evil@x.io')",
        "UPDATE users SET bio = 'x' WHERE id = 1",
        "DELETE FROM users WHERE id = 1",
        "WITH d AS (SELECT 1) DELETE FROM users WHERE id = 2",
        "CREATE TABLE t(a)",
        "DROP TABLE users",
        "ATTACH DATABASE ':memory:' AS other",
        "REPLACE INTO users(id, email) VALUES (1, 'z')",
    ] {
        let outcome = rig.call(
            "execute_query",
            json!({"connection_id": ID, "statement": sql}),
        );
        assert_eq!(code(&outcome), "agent.write_refused", "{sql}: {outcome:?}");
    }
    assert_eq!(rig.count("users"), before);
    let scripts = rig.call(
        "execute_query",
        json!({"connection_id": ID, "statement": "SELECT 1; DELETE FROM users"}),
    );
    assert_eq!(code(&scripts), "agent.multi_statement");
    assert_eq!(rig.count("users"), before);
    // Reads with a write keyword in a string or comment are fine.
    rig.query("SELECT 'DELETE FROM users' AS note -- DROP TABLE users");
}

#[test]
fn read_write_with_an_approving_approver_performs_the_write_and_audits_it() {
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, false);
    let rig = rig_with(
        Some(AgentAccess::ReadWrite),
        Some(approver.clone()),
        Limits::default(),
        Vec::new(),
    );
    rig.ok("open_session", json!({"connection_id": ID}));
    let inserted = rig.ok(
        "execute_query",
        json!({"connection_id": ID, "statement": "INSERT INTO users(email, bio) VALUES ('d@x.io', 'fourth')"}),
    );
    assert_eq!(inserted["affected_rows"], 1);
    assert_eq!(inserted["rows"], json!([]));
    assert_eq!(rig.count("users"), "4");
    let updated = rig.ok("execute_query", json!({"connection_id": ID, "statement": "UPDATE users SET bio = 'x' WHERE email = 'd@x.io'"}));
    assert_eq!(updated["affected_rows"], 1);
    assert_eq!(approver.asked.lock().unwrap().len(), 2);
    // A denying approver blocks the same statement.
    let deny = ScriptedApprover::new(
        ApprovalDecision::Deny {
            reason: Some("no".into()),
        },
        false,
    );
    let denied = rig_with(
        Some(AgentAccess::ReadWrite),
        Some(deny),
        Limits::default(),
        Vec::new(),
    );
    denied.ok("open_session", json!({"connection_id": ID}));
    let outcome = denied.call(
        "execute_query",
        json!({"connection_id": ID, "statement": "DELETE FROM users WHERE id = 1"}),
    );
    assert_eq!(code(&outcome), "agent.write_refused");
    assert_eq!(denied.count("users"), "3");

    // The write is in the audit log, with its text and row count, and no data.
    rig.layer.close_all();
    rig.audit.flush();
    let entries = read_entries(&rig.audit_path, 100).unwrap().entries;
    let insert = entries
        .iter()
        .find(|e| {
            e.statement
                .as_deref()
                .is_some_and(|s| s.starts_with("INSERT INTO users"))
        })
        .expect("the INSERT is audited");
    assert_eq!(insert.outcome, "ok");
    assert_eq!(insert.rows, Some(1));
    assert_eq!(insert.connection_id.as_deref(), Some(ID));
    assert_eq!(insert.front_end, "test");
    let file = std::fs::read_to_string(&rig.audit_path).unwrap();
    assert!(
        !file.contains("fourth@") && !file.contains("\"rows\":[["),
        "{file}"
    );
}

#[test]
fn dangerous_statements_need_the_flag_and_an_approver_that_allows_them() {
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let rig = rig_with(
        Some(AgentAccess::ReadWrite),
        Some(approver.clone()),
        Limits::default(),
        Vec::new(),
    );
    rig.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(rig.count("orders"), "3");
    let refused = rig.call(
        "execute_query",
        json!({"connection_id": ID, "statement": "DELETE FROM orders"}),
    );
    assert_eq!(code(&refused), "agent.dangerous_refused");
    assert_eq!(rig.count("orders"), "3");
    let deleted = rig.ok(
        "execute_query",
        json!({"connection_id": ID, "statement": "DELETE FROM orders", "dangerous": true}),
    );
    assert_eq!(deleted["affected_rows"], 3);
    assert_eq!(rig.count("orders"), "0");
    assert!(approver.asked.lock().unwrap()[0]
        .1
        .as_deref()
        .unwrap()
        .contains("DELETE"));
    let dropped = rig.ok(
        "execute_query",
        json!({"connection_id": ID, "statement": "DROP TABLE nums", "dangerous": true}),
    );
    assert!(dropped["rows"].as_array().unwrap().is_empty());
    let tables = rig.ok("list_tables", json!({"connection_id": ID}));
    assert!(!tables.to_string().contains("nums"));
}

#[test]
fn a_long_statement_is_cancelled_and_the_session_survives() {
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let token = CancelToken::new();
    let worker = {
        let (layer, token) = (Arc::clone(&rig.layer), token.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let outcome = layer.call_cancellable(
                "execute_query",
                json!({"connection_id": ID, "statement": LONG_QUERY}),
                &token,
            );
            (outcome, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(400));
    token.cancel();
    let (outcome, took) = worker.join().unwrap();
    assert_eq!(code(&outcome), "agent.cancelled", "{outcome:?}");
    assert!(took < Duration::from_secs(10), "cancel took {took:?}");
    assert_eq!(rig.query("SELECT 41 + 1")["rows"][0][0], "42");
}

#[test]
fn dropping_the_guard_cancels_a_running_statement() {
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let token = CancelToken::new();
    let guard = token.cancel_on_drop();
    let worker = {
        let (layer, token) = (Arc::clone(&rig.layer), token.clone());
        std::thread::spawn(move || {
            layer.call_cancellable(
                "execute_query",
                json!({"connection_id": ID, "statement": LONG_QUERY}),
                &token,
            )
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    drop(guard);
    assert_eq!(code(&worker.join().unwrap()), "agent.cancelled");
}

#[test]
fn the_statement_timeout_comes_from_the_connection() {
    let rig = rig(AgentAccess::Read);
    rig.catalog
        .update(ID, |info| info.statement_timeout_secs = Some(1));
    rig.ok("open_session", json!({"connection_id": ID}));
    let started = Instant::now();
    let outcome = rig.call(
        "execute_query",
        json!({"connection_id": ID, "statement": LONG_QUERY}),
    );
    assert_eq!(code(&outcome), "agent.limit", "{outcome:?}");
    assert!(
        outcome.to_text().contains("1 s statement timeout"),
        "{}",
        outcome.to_text()
    );
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(rig.query("SELECT 1")["rows"][0][0], "1");
}

#[test]
fn one_statement_runs_per_session_on_the_real_driver() {
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let token = CancelToken::new();
    let worker = {
        let (layer, token) = (Arc::clone(&rig.layer), token.clone());
        std::thread::spawn(move || {
            layer.call_cancellable(
                "execute_query",
                json!({"connection_id": ID, "statement": LONG_QUERY}),
                &token,
            )
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let second = rig.call(
        "execute_query",
        json!({"connection_id": ID, "statement": "SELECT 1"}),
    );
    assert_eq!(code(&second), "agent.limit");
    // Browsing is not blocked by the running statement.
    rig.ok("list_tables", json!({"connection_id": ID}));
    token.cancel();
    assert_eq!(code(&worker.join().unwrap()), "agent.cancelled");
}

#[test]
fn row_byte_and_cell_caps_hold_against_real_results() {
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let default = rig.query("SELECT n, label FROM nums ORDER BY n");
    assert_eq!(default["row_count"], 200);
    assert_eq!(default["truncated"], true);
    assert_eq!(default["truncation"], "row_limit");
    assert_eq!(default["rows"][199][0], "200");
    let max = rig.ok("execute_query", json!({"connection_id": ID, "statement": "SELECT n FROM nums ORDER BY n", "row_limit": 100000}));
    assert_eq!(max["row_count"], 1000);
    assert_eq!(max["row_limit"], 1000);
    let exact = rig.ok("execute_query", json!({"connection_id": ID, "statement": "SELECT n FROM nums WHERE n <= 7 ORDER BY n", "row_limit": 7}));
    assert_eq!(exact["row_count"], 7);
    assert_eq!(
        exact["truncated"], false,
        "exactly the limit is not truncated"
    );

    // A 100 KiB cell is cut at 2 KiB and marked.
    let wide = rig.query("SELECT replace(hex(zeroblob(50000)), '0', 'a') AS big");
    assert_eq!(wide["cells_truncated"], 1);
    let cell = wide["rows"][0][0].as_str().unwrap();
    assert!(cell.len() < 2048 + 100, "{}", cell.len());
    assert!(cell.contains("[truncated, 100000 bytes in total]"));

    // 1000 rows of ~1.5 KiB hit the 256 KiB result cap first.
    let heavy = rig.ok(
        "execute_query",
        json!({"connection_id": ID, "statement": "SELECT n, replace(hex(zeroblob(750)), '0', 'b') AS pad FROM nums ORDER BY n", "row_limit": 1000}),
    );
    assert_eq!(heavy["truncation"], "byte_limit");
    assert!(
        heavy["row_count"].as_u64().unwrap() < 200,
        "{}",
        heavy["row_count"]
    );
    let bytes: usize = heavy["rows"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| r.as_array().unwrap())
        .map(|c| c.as_str().unwrap().len())
        .sum();
    assert!(bytes <= 256 * 1024, "{bytes}");

    // Smaller limits configured by the front end apply too.
    let small = rig_with(
        Some(AgentAccess::Read),
        None,
        Limits {
            max_rows: 5,
            default_rows: 3,
            ..Limits::default()
        },
        Vec::new(),
    );
    small.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(small.query("SELECT n FROM nums")["row_count"], 3);
    assert_eq!(
        small.ok(
            "execute_query",
            json!({"connection_id": ID, "statement": "SELECT n FROM nums", "row_limit": 50})
        )["row_count"],
        5
    );
}

#[test]
fn secrets_do_not_reach_errors_or_the_audit_log() {
    let secret = "S3cr3t-Passw0rd-Zq7".to_owned();
    let rig = rig_with(
        Some(AgentAccess::Read),
        None,
        Limits::default(),
        vec![engine_agent::SecretText::from(secret.as_str())],
    );
    rig.ok("open_session", json!({"connection_id": ID}));
    // The secret is typed into a statement and echoed back by the driver's error text.
    let outcome = rig.call(
        "execute_query",
        json!({"connection_id": ID, "statement": format!("SELECT * FROM missing_{secret}")}),
    );
    assert!(outcome.is_error(), "{outcome:?}");
    assert!(
        !outcome.to_text().contains(&secret),
        "{}",
        outcome.to_text()
    );
    rig.layer.close_all();
    rig.audit.flush();
    let log = std::fs::read_to_string(&rig.audit_path).unwrap();
    assert_eq!(
        log.lines().count(),
        2,
        "open_session and the failing statement: {log}"
    );
    assert!(!log.contains(&secret), "{log}");
}

// ---------------------------------------------------------- read-only layers

#[test]
fn reads_report_read_only_enforcement_and_blocked_calls_never_reach_sqlite() {
    let rig = rig(AgentAccess::Read);
    rig.ok("open_session", json!({"connection_id": ID}));
    let select = rig.query("SELECT count(*) FROM users");
    assert_eq!(select["read_only_enforced"], true);
    let plan = rig.ok(
        "explain_query",
        json!({"connection_id": ID, "statement": "SELECT * FROM users"}),
    );
    assert_eq!(plan["read_only_enforced"], true);
    for (statement, want) in [
        (
            "SELECT load_extension('/tmp/evil')",
            "agent.blocked_function",
        ),
        ("SELECT readfile('/etc/passwd')", "agent.blocked_function"),
        ("SELECT writefile('/tmp/x', 'y')", "agent.blocked_function"),
        (
            "ATTACH DATABASE '/tmp/tablex-agent-attach.db' AS x",
            "agent.write_refused",
        ),
        ("DETACH DATABASE x", "agent.write_refused"),
        ("PRAGMA writable_schema = ON", "agent.unsupported"),
        ("PRAGMA query_only = OFF", "agent.unsupported"),
        ("BEGIN", "agent.unsupported"),
    ] {
        let outcome = rig.call(
            "execute_query",
            json!({"connection_id": ID, "statement": statement}),
        );
        assert_eq!(code(&outcome), want, "{statement}: {outcome:?}");
    }
    assert!(!std::path::Path::new("/tmp/tablex-agent-attach.db").exists());
}

#[test]
fn the_database_itself_refuses_a_write_run_read_only_and_is_writable_again_after() {
    // The denylist and the classifier are bypassed on purpose: a raw session is
    // handed a write with `read_only` set, as if a statement had slipped past both.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("raw.db");
    let provider = DriverProvider::new(
        load_driver("dbengine-sqlite", ""),
        &[(ID, &format!("sqlite://{}", db.display()))],
    );
    let session = provider.raw_session(ID);
    let limits = Limits::default();
    let run = |text: &str, read_only: bool| {
        session.run(
            &engine_agent::ports::RunRequest {
                language: engine_agent::Language::Sql,
                text,
                database: None,
                limits: &limits,
                row_limit: 10,
                confirmed: true,
                read_only,
            },
            &CancelToken::new(),
        )
    };
    run("CREATE TABLE t(a INTEGER)", false).unwrap();
    run("INSERT INTO t VALUES (1)", false).unwrap();
    for write in [
        "INSERT INTO t VALUES (2)",
        "UPDATE t SET a = 5",
        "DELETE FROM t",
        "CREATE TABLE u(b)",
        "DROP TABLE t",
    ] {
        let error = run(write, true).unwrap_err();
        assert_eq!(error.code(), "agent.write_refused", "{write}: {error:?}");
    }
    // Nothing changed, a read runs and reports enforcement, and the connection is writable again.
    let read = run("SELECT a FROM t", true).unwrap();
    assert_eq!(read.rows, vec![vec!["1".to_owned()]]);
    assert!(read.read_only_enforced);
    let write = run("INSERT INTO t VALUES (3)", false).unwrap();
    assert_eq!(write.affected_rows, Some(1));
    assert!(!write.read_only_enforced);
    assert_eq!(
        run("SELECT count(*) FROM t", false).unwrap().rows[0][0],
        "2"
    );
}

// ------------------------------------------------------------------ transactions

fn writer() -> Rig {
    rig_with(
        Some(AgentAccess::ReadWrite),
        Some(ScriptedApprover::new(ApprovalDecision::Approve, false)),
        Limits::default(),
        Vec::new(),
    )
}

fn begin(rig: &Rig) -> String {
    rig.ok("begin_transaction", json!({"connection_id": ID}))["transaction_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn in_tx(rig: &Rig, tx: &str, sql: &str) -> ToolOutcome {
    rig.call(
        "execute_query",
        json!({"connection_id": ID, "transaction_id": tx, "statement": sql}),
    )
}

fn count_in(rig: &Rig, tx: &str, table: &str) -> String {
    match in_tx(rig, tx, &format!("SELECT count(*) FROM {table}")) {
        ToolOutcome::Success(value) => value["rows"][0][0].as_str().unwrap().to_owned(),
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
}

#[test]
fn a_rolled_back_transaction_leaves_no_row_and_a_second_session_never_saw_it() {
    let rig = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    let insert = in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('tx@x.io')");
    assert!(!insert.is_error(), "{insert:?}");
    // Inside the transaction, a read (run read-only: `PRAGMA query_only` does not end it) sees the row.
    let read = in_tx(&rig, &tx, "SELECT email FROM users WHERE email = 'tx@x.io'");
    match &read {
        ToolOutcome::Success(value) => {
            assert_eq!(value["rows"], json!([["tx@x.io"]]));
            assert_eq!(value["read_only_enforced"], true);
        }
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
    assert_eq!(count_in(&rig, &tx, "users"), "4");
    // The write after a read is still in the same transaction (the read-only scope did not end it).
    let second = in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('tx2@x.io')");
    assert!(!second.is_error(), "{second:?}");
    assert_eq!(count_in(&rig, &tx, "users"), "5");
    // The connection's shared session is another database connection: it sees nothing yet.
    assert_eq!(rig.count("users"), "3");
    let rolled = rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rolled["rolled_back"], true);
    assert_eq!(rolled["changes"], 2);
    assert_eq!(rig.count("users"), "3");
    let gone = in_tx(&rig, &tx, "SELECT 1");
    assert_eq!(code(&gone), "agent.transaction_gone");
    // The transaction's session is closed: the file is not locked, a new write goes through.
    let write = rig.query("INSERT INTO users(email) VALUES ('after@x.io')");
    assert_eq!(write["affected_rows"], 1);
}

#[test]
fn a_committed_transaction_keeps_its_rows_after_the_commit_is_approved() {
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, false);
    let rig = rig_with(
        Some(AgentAccess::ReadWrite),
        Some(approver.clone()),
        Limits::default(),
        Vec::new(),
    );
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('keep@x.io')");
    in_tx(&rig, &tx, "UPDATE users SET bio = 'edited' WHERE email = 'keep@x.io'");
    assert_eq!(rig.count("users"), "3", "not visible before the commit");
    let committed = rig.ok("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(committed["committed"], true);
    assert_eq!(committed["changes"], 2);
    // The question listed both writes.
    let asked = approver.transactions.lock().unwrap().clone();
    let (id, writes) = asked.last().unwrap().clone().unwrap();
    assert_eq!(id, tx);
    assert_eq!(
        writes.unwrap(),
        [
            "INSERT INTO users(email) VALUES ('keep@x.io')",
            "UPDATE users SET bio = 'edited' WHERE email = 'keep@x.io'"
        ]
    );
    assert_eq!(rig.count("users"), "4");
    assert_eq!(
        rig.query("SELECT bio FROM users WHERE email = 'keep@x.io'")["rows"],
        json!([["edited"]])
    );
    // Ended: nothing is left to roll back.
    assert_eq!(
        rig.ok("rollback_transaction", json!({"transaction_id": tx}))["rolled_back"],
        false
    );
}

#[test]
fn a_declined_commit_leaves_the_rows_uncommitted_until_rolled_back() {
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, false);
    *approver.commit_decision.lock().unwrap() = Some(ApprovalDecision::Deny { reason: None });
    let rig = rig_with(
        Some(AgentAccess::ReadWrite),
        Some(approver),
        Limits::default(),
        Vec::new(),
    );
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('no@x.io')");
    let declined = rig.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&declined), "agent.write_refused");
    assert_eq!(rig.count("users"), "3");
    assert_eq!(count_in(&rig, &tx, "users"), "4", "the transaction is still open");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rig.count("users"), "3");
}

#[test]
fn insert_or_rollback_ends_a_sqlite_transaction_and_the_layer_treats_it_as_aborted() {
    let rig = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    let first = in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('first@x.io')");
    assert!(!first.is_error(), "{first:?}");
    // SQLite rolls the WHOLE transaction back by itself here and returns to autocommit.
    let dup = in_tx(&rig, &tx, "INSERT OR ROLLBACK INTO users(email) VALUES ('a@x.io')");
    assert!(dup.is_error(), "{dup:?}");
    match &dup {
        ToolOutcome::Failure(error) => {
            assert!(error.message.contains("is aborted; roll it back"), "{error:?}");
            assert!(!error.retryable);
        }
        ToolOutcome::Success(_) => unreachable!(),
    }
    // A next write would run in autocommit and commit by itself: it is refused instead.
    let next = in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('second@x.io')");
    assert_eq!(code(&next), "agent.transaction_aborted", "{next:?}");
    assert_eq!(code(&in_tx(&rig, &tx, "SELECT 1")), "agent.transaction_aborted");
    assert_eq!(
        code(&rig.call("commit_transaction", json!({"transaction_id": tx}))),
        "agent.transaction_aborted"
    );
    // Nothing was committed, by the first write, the failed one or the refused one.
    assert_eq!(rig.count("users"), "3");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rig.count("users"), "3");
    // A new transaction works.
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('third@x.io')");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rig.count("users"), "3");
}

#[test]
fn a_trigger_that_raises_rollback_aborts_the_layers_transaction() {
    let rig = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    // The trigger is created outside the policy (the layer takes one statement, and a trigger
    // body has several), through a session of its own on the same file.
    {
        let db = rig._dir.path().join("agent.db");
        let provider = DriverProvider::new(
            load_driver("dbengine-sqlite", ""),
            &[(ID, &format!("sqlite://{}", db.display()))],
        );
        let session = provider.raw_session(ID);
        let limits = Limits::default();
        session
            .run(
                &engine_agent::ports::RunRequest {
                    language: engine_agent::Language::Sql,
                    text: "CREATE TRIGGER users_guard BEFORE INSERT ON users WHEN NEW.email = 'bad@x.io' \
                           BEGIN SELECT RAISE(ROLLBACK, 'refused by the trigger'); END",
                    database: None,
                    limits: &limits,
                    row_limit: 10,
                    confirmed: true,
                    read_only: false,
                },
                &CancelToken::new(),
            )
            .expect("create the trigger");
        session.disconnect();
    }
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('before@x.io')");
    let raised = in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('bad@x.io')");
    assert!(raised.is_error(), "{raised:?}");
    let next = in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('after@x.io')");
    assert_eq!(code(&next), "agent.transaction_aborted", "{next:?}");
    assert_eq!(rig.count("users"), "3", "the rollback took the first row, and nothing committed 'after'");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rig.count("users"), "3");
}

#[test]
fn a_read_in_a_transaction_cannot_write_and_session_statements_are_refused() {
    let rig = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    // The policy lets a SELECT through as a read; the driver's read-only scope would stop a
    // hidden write. Session-state statements never get that far.
    for statement in ["COMMIT", "ROLLBACK", "BEGIN", "PRAGMA query_only = OFF", "SAVEPOINT a"] {
        assert_eq!(
            code(&in_tx(&rig, &tx, statement)),
            "agent.unsupported",
            "{statement}"
        );
    }
    // The transaction survived all of that.
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('still@x.io')");
    assert_eq!(count_in(&rig, &tx, "users"), "4");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rig.count("users"), "3");
}

#[test]
fn closing_the_session_or_the_layer_rolls_a_real_transaction_back() {
    let rig = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('gone@x.io')");
    rig.ok("close_session", json!({"connection_id": ID}));
    rig.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(rig.count("users"), "3");
    // Shutdown with a transaction open.
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('gone2@x.io')");
    rig.layer.close_all();
    rig.ok("open_session", json!({"connection_id": ID}));
    assert_eq!(rig.count("users"), "3");
}

#[test]
fn a_real_transaction_is_not_expired_before_the_idle_time() {
    let rig = writer();
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('idle@x.io')");
    // The default idle time is 5 minutes (the expiry itself is tested against fakes).
    assert_eq!(rig.layer.expire_idle_transactions(), 0);
    assert_eq!(count_in(&rig, &tx, "users"), "4");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
}

#[test]
fn a_long_write_interrupted_inside_a_transaction_aborts_it_and_commits_nothing() {
    let rig = writer();
    rig.ok("open_session", json!({"connection_id": ID}));
    let tx = begin(&rig);
    in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('before@x.io')");
    let token = CancelToken::new();
    let long = {
        let (layer, token, tx) = (Arc::clone(&rig.layer), token.clone(), tx.clone());
        std::thread::spawn(move || {
            layer.call_cancellable(
                "execute_query",
                json!({"connection_id": ID, "transaction_id": tx, "statement":
                    "INSERT INTO nums(n, label) WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
                     WHERE x < 4000000000) SELECT x, 'w' FROM c"}),
                &token,
            )
        })
    };
    std::thread::sleep(Duration::from_millis(400));
    token.cancel();
    let interrupted = long.join().unwrap();
    assert!(interrupted.is_error(), "{interrupted:?}");
    // SQLite may have rolled the transaction back by itself; the layer does not trust it.
    let next = in_tx(&rig, &tx, "INSERT INTO users(email) VALUES ('after@x.io')");
    assert_eq!(code(&next), "agent.transaction_aborted", "{next:?}");
    assert_eq!(
        code(&rig.call("commit_transaction", json!({"transaction_id": tx}))),
        "agent.transaction_aborted"
    );
    assert_eq!(rig.count("users"), "3");
    assert_eq!(rig.count("nums"), "5000");
    rig.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rig.count("users"), "3");
}
