//! The tool layer against in-memory fakes: dispatch errors, the policy matrix,
//! limits, cancellation, audit and redaction.

use engine_agent::testing::{
    connection, Behavior, FakeCatalog, FakeProvider, FakeSession, ScriptedApprover,
};
use engine_agent::{
    read_entries, AgentAccess, AgentError, ApprovalDecision, AuditConfig, AuditLog, CancelToken,
    ConnectionInfo, ConnectionKind, ConnectionTypeInfo, Limits, ToolLayer, ToolOutcome,
};
use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SQL: &str = "sql1";
const REDIS: &str = "redis1";

struct Fixture {
    layer: Arc<ToolLayer>,
    catalog: Arc<FakeCatalog>,
    sql: Arc<FakeSession>,
    redis: Arc<FakeSession>,
    provider: Arc<FakeProvider>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn connection_type(read_only: bool, production: bool) -> ConnectionTypeInfo {
    ConnectionTypeInfo {
        name: match (read_only, production) {
            (false, false) => "Development",
            (true, false) => "Read-only",
            (false, true) => "Production",
            (true, true) => "Read-only production",
        }
        .into(),
        read_only,
        production,
        default_agent_access: None,
    }
}

fn with_type(mut info: ConnectionInfo, read_only: bool, production: bool) -> ConnectionInfo {
    info.connection_type = connection_type(read_only, production);
    info
}

fn fixture_with(
    setting: Option<AgentAccess>,
    read_only: bool,
    production: bool,
    approver: Option<Arc<ScriptedApprover>>,
    limits: Limits,
    sql_secrets: Vec<engine_agent::SecretText>,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let mut sql = FakeSession::new(ConnectionKind::PostgreSql);
    sql.secrets = sql_secrets;
    sql.databases = vec!["app".into(), "other".into()];
    sql.schemas = vec!["public".into()];
    sql.info.supports_schemas = true;
    sql.routines = Mutex::new(seeded_routines());
    let sql = sql.arc();
    let redis = FakeSession::new(ConnectionKind::Redis).arc();
    let provider = FakeProvider::new();
    provider.with_session(SQL, Arc::clone(&sql));
    provider.with_session(REDIS, Arc::clone(&redis));
    let catalog = FakeCatalog::new(vec![
        with_type(
            connection(SQL, ConnectionKind::PostgreSql, setting),
            read_only,
            production,
        ),
        with_type(
            connection(REDIS, ConnectionKind::Redis, setting),
            read_only,
            production,
        ),
    ]);
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let mut layer = ToolLayer::new(catalog.clone(), provider.clone(), "test")
        .with_audit(Arc::clone(&audit))
        .with_limits(limits);
    if let Some(approver) = approver {
        layer = layer.with_approver(approver);
    }
    Fixture {
        layer: Arc::new(layer),
        catalog,
        sql,
        redis,
        provider,
        audit,
        audit_path,
        _dir: dir,
    }
}

fn routine(
    kind: SchemaObjectKind,
    name: &str,
    arguments: Option<&str>,
    table: Option<&str>,
) -> SchemaObjectRef {
    SchemaObjectRef {
        database: Some("app".into()),
        schema: Some("public".into()),
        kind,
        name: name.into(),
        arguments: arguments.map(str::to_owned),
        table: table.map(str::to_owned),
        identity: arguments.map(|text| format!("oid:{text}")),
    }
}

fn seeded_routines() -> Vec<(SchemaObjectRef, String)> {
    use SchemaObjectKind::{Function, Procedure, Trigger};
    vec![
        (
            routine(Function, "add", Some("integer, integer"), None),
            "CREATE FUNCTION public.add(a integer, b integer) RETURNS integer AS 'select a + b'".into(),
        ),
        (
            routine(Function, "add", Some("text, text"), None),
            "CREATE FUNCTION public.add(a text, b text) RETURNS text AS 'select a || b'".into(),
        ),
        (
            routine(Function, "Audit_Fn", Some(""), None),
            "CREATE FUNCTION public.\"Audit_Fn\"() RETURNS trigger".into(),
        ),
        (
            routine(Function, "refresh_totals", Some(""), None),
            "CREATE FUNCTION public.refresh_totals() RETURNS void".into(),
        ),
        (
            routine(Procedure, "purge_old", Some("integer"), None),
            "CREATE PROCEDURE public.purge_old(days integer)".into(),
        ),
        (
            routine(Trigger, "audit_orders", None, Some("orders")),
            "CREATE TRIGGER audit_orders AFTER INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION public.\"Audit_Fn\"()".into(),
        ),
    ]
}

fn fixture(setting: Option<AgentAccess>) -> Fixture {
    fixture_with(setting, false, false, None, Limits::default(), Vec::new())
}

impl Fixture {
    fn call(&self, tool: &str, args: Value) -> ToolOutcome {
        self.layer.call(tool, args)
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        match self.call(tool, args) {
            ToolOutcome::Success(value) => value,
            ToolOutcome::Failure(error) => panic!("{tool} failed: {error:?}"),
        }
    }

    fn open_all(&self) {
        self.ok("open_session", json!({"connection_id": SQL}));
        self.ok("open_session", json!({"connection_id": REDIS}));
    }

    /// Everything recorded so far, newest first: the writer is flushed first, so
    /// the result never depends on how far the writer thread has got.
    fn audit(&self) -> Vec<engine_agent::AuditEntry> {
        self.audit.flush();
        read_entries(&self.audit_path, 10_000).unwrap().entries
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

// ------------------------------------------------------------- dispatch

#[test]
fn unknown_tools_and_bad_arguments_get_typed_errors() {
    let fx = fixture(Some(AgentAccess::ReadWrite));
    assert_eq!(
        code(&fx.call("drop_everything", json!({}))),
        "agent.unknown_tool"
    );
    assert_eq!(code(&fx.call("", json!({}))), "agent.unknown_tool");
    // Malformed JSON text, a non-object, an unknown field, a wrong type, a missing field.
    assert_eq!(
        code(&fx.layer.call_json("execute_query", "{not json")),
        "agent.invalid_arguments"
    );
    assert_eq!(
        code(&fx.layer.call_json("execute_query", "[1,2]")),
        "agent.invalid_arguments"
    );
    assert_eq!(
        code(&fx.call("execute_query", json!("SELECT 1"))),
        "agent.invalid_arguments"
    );
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1", "evil": true})
        )),
        "agent.invalid_arguments"
    );
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": 7})
        )),
        "agent.invalid_arguments"
    );
    assert_eq!(
        code(&fx.call("execute_query", json!({"connection_id": SQL}))),
        "agent.invalid_arguments"
    );
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": "nope", "statement": "SELECT 1"})
        )),
        "agent.invalid_arguments"
    );
    // Empty arguments are fine for a tool with none.
    assert_eq!(code(&fx.layer.call_json("list_connections", "")), "ok");
    assert_eq!(code(&fx.layer.call_json("list_connections", "{}")), "ok");
    let message = match fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "x", "evil": 1}),
    ) {
        ToolOutcome::Failure(error) => error.message,
        ToolOutcome::Success(_) => unreachable!(),
    };
    assert!(message.contains("unknown field `evil`"), "{message}");
    assert_eq!(fx.sql.run_count(), 0);
}

#[test]
fn errors_serialise_for_the_model() {
    let fx = fixture(None);
    let outcome = fx.call("open_session", json!({"connection_id": SQL}));
    let json = outcome.to_json();
    assert_eq!(json["error"]["code"], "agent.access_denied");
    assert_eq!(json["error"]["retryable"], false);
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("agent access"));
    assert!(outcome.is_error());
}

#[test]
fn list_connections_never_carries_a_uri_or_secret() {
    let fx = fixture_with(
        Some(AgentAccess::Read),
        false,
        true,
        None,
        Limits::default(),
        Vec::new(),
    );
    let listed = fx.ok("list_connections", json!({}));
    let text = listed.to_string();
    for needle in ["uri", "password", "postgres://", "://"] {
        assert!(!text.contains(needle), "{needle} in {text}");
    }
    let first = &listed["connections"][0];
    assert_eq!(first["connection_type"], "Production");
    assert_eq!(first["production"], true);
    assert_eq!(first["access"], "read");
    assert_eq!(first["open"], false);
    let mut keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "access",
            "configured_access",
            "connection_type",
            "id",
            "kind",
            "name",
            "open",
            "production",
            "read_only"
        ]
    );
}

// ------------------------------------------------------- the policy matrix

/// Expected outcome of every tool case at each effective access
/// (off, schema_only, read, read_write).
fn expected(case: &str, effective: usize) -> &'static str {
    const D: &str = "agent.access_denied";
    const W: &str = "agent.write_refused";
    const X: &str = "agent.dangerous_refused";
    const M: &str = "agent.multi_statement";
    const OK: &str = "ok";
    let row: [&str; 4] = match case {
        "open_session" | "list_namespaces" | "list_tables" | "describe_table"
        | "get_schema_context" | "list_routines" | "get_routine_source" => [D, OK, OK, OK],
        "select" | "explain" | "redis_get" | "redis_scan" => [D, D, OK, OK],
        "insert" | "redis_set" => [D, D, W, OK],
        "drop" | "redis_flushdb" => [D, D, W, X],
        "drop_flagged" => [D, D, W, OK],
        "script" => [D, D, M, M],
        "redis_keys" => [D, D, X, X],
        other => panic!("{other}"),
    };
    row[effective]
}

fn matrix_calls() -> Vec<(&'static str, &'static str, Value)> {
    let sql = |statement: &str| json!({"connection_id": SQL, "statement": statement});
    let redis = |command: &str| json!({"connection_id": REDIS, "command": command});
    vec![
        (
            "open_session",
            "open_session",
            json!({"connection_id": SQL}),
        ),
        (
            "list_namespaces",
            "list_namespaces",
            json!({"connection_id": SQL}),
        ),
        (
            "list_tables",
            "list_tables",
            json!({"connection_id": SQL, "database": "app", "schema": "public"}),
        ),
        (
            "describe_table",
            "describe_table",
            json!({"connection_id": SQL, "table": "t", "database": "app", "schema": "public"}),
        ),
        (
            "get_schema_context",
            "get_schema_context",
            json!({"connection_id": SQL, "database": "app", "schema": "public"}),
        ),
        (
            "list_routines",
            "list_routines",
            json!({"connection_id": SQL, "kind": "function", "database": "app", "schema": "public"}),
        ),
        (
            "get_routine_source",
            "get_routine_source",
            json!({"connection_id": SQL, "kind": "trigger", "name": "audit_orders", "table": "orders", "database": "app", "schema": "public"}),
        ),
        ("select", "execute_query", sql("SELECT 1")),
        ("explain", "explain_query", sql("SELECT 1")),
        ("insert", "execute_query", sql("INSERT INTO t VALUES (1)")),
        ("drop", "execute_query", sql("DROP TABLE t")),
        (
            "drop_flagged",
            "execute_query",
            json!({"connection_id": SQL, "statement": "DROP TABLE t", "dangerous": true}),
        ),
        ("script", "execute_query", sql("SELECT 1; SELECT 2")),
        ("redis_get", "execute_redis_command", redis("GET k")),
        ("redis_scan", "execute_redis_command", redis("SCAN 0")),
        ("redis_set", "execute_redis_command", redis("SET k v")),
        ("redis_flushdb", "execute_redis_command", redis("FLUSHDB")),
        ("redis_keys", "execute_redis_command", redis("KEYS *")),
    ]
}

#[test]
fn access_by_connection_type_by_tool_by_statement_class() {
    // Hand-written effective access for (setting, read_only, production):
    // columns of `settings` x [plain, read-only, production, read-only production].
    let settings: [(Option<AgentAccess>, [usize; 4]); 5] = [
        (None, [0, 0, 0, 0]),
        (Some(AgentAccess::Off), [0, 0, 0, 0]),
        (Some(AgentAccess::SchemaOnly), [1, 1, 1, 1]),
        (Some(AgentAccess::Read), [2, 2, 2, 2]),
        (Some(AgentAccess::ReadWrite), [3, 2, 3, 2]),
    ];
    let types = [(false, false), (true, false), (false, true), (true, true)];
    for (setting, effective_by_type) in settings {
        for ((read_only, production), effective) in types.into_iter().zip(effective_by_type) {
            let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
            let fx = fixture_with(
                setting,
                read_only,
                production,
                Some(approver),
                Limits::default(),
                Vec::new(),
            );
            seed_tables(&fx, 1);
            fx.sql.tables.lock().unwrap()[0].name = "t".into();
            for (case, tool, args) in matrix_calls() {
                // Calls on an open session need it open (only possible from schema_only up).
                if effective >= 1 {
                    fx.open_all();
                }
                let before = fx.sql.run_count() + fx.redis.run_count();
                let outcome = fx.call(tool, args);
                let want = if case == "open_session" && effective >= 1 {
                    "ok"
                } else {
                    expected(case, effective)
                };
                assert_eq!(
                    code(&outcome),
                    want,
                    "{case}: setting {setting:?}, read_only {read_only}, production {production} (effective {effective}): {outcome:?}"
                );
                let ran = fx.sql.run_count() + fx.redis.run_count() - before;
                assert_eq!(
                    ran,
                    usize::from(
                        want == "ok"
                            && !matches!(
                                case,
                                "open_session"
                                    | "list_namespaces"
                                    | "list_tables"
                                    | "describe_table"
                                    | "get_schema_context"
                                    | "list_routines"
                                    | "get_routine_source"
                            )
                    ),
                    "{case}: what reached the driver"
                );
                fx.layer.close_all();
            }
            // list_connections and close_session are never denied.
            assert_eq!(code(&fx.call("list_connections", json!({}))), "ok");
            assert_eq!(
                code(&fx.call("close_session", json!({"connection_id": SQL}))),
                "ok"
            );
        }
    }
}

#[test]
fn a_denied_call_never_opens_a_session() {
    let fx = fixture(Some(AgentAccess::Off));
    let outcome = fx.call("open_session", json!({"connection_id": SQL}));
    assert_eq!(code(&outcome), "agent.access_denied");
    assert_eq!(fx.provider.opened.load(Ordering::SeqCst), 0);
    // Production without a setting says why.
    let fx = fixture_with(None, false, true, None, Limits::default(), Vec::new());
    let message = match fx.call("open_session", json!({"connection_id": SQL})) {
        ToolOutcome::Failure(error) => error.message,
        ToolOutcome::Success(_) => unreachable!(),
    };
    assert!(message.contains("production"), "{message}");
    let fx = fixture_with(
        Some(AgentAccess::ReadWrite),
        true,
        false,
        None,
        Limits::default(),
        Vec::new(),
    );
    fx.open_all();
    let message = match fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "DELETE FROM t WHERE id = 1"}),
    ) {
        ToolOutcome::Failure(error) => error.message,
        ToolOutcome::Success(_) => unreachable!(),
    };
    assert!(message.contains("write"), "{message}");
}

#[test]
fn lowering_the_access_takes_effect_on_the_next_call() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1"})
        )),
        "ok"
    );
    fx.catalog.update(SQL, |info| {
        info.agent_access = Some(AgentAccess::SchemaOnly)
    });
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1"})
        )),
        "agent.access_denied"
    );
    assert_eq!(
        code(&fx.call(
            "list_tables",
            json!({"connection_id": SQL, "database": "app", "schema": "public"})
        )),
        "ok"
    );
    fx.catalog.update(SQL, |info| info.agent_access = None);
    assert_eq!(
        code(&fx.call(
            "list_tables",
            json!({"connection_id": SQL, "database": "app", "schema": "public"})
        )),
        "agent.access_denied"
    );
}

// -------------------------------------------------------------- approvals

#[test]
fn writes_ask_the_approver_with_the_exact_statement() {
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let fx = fixture_with(
        Some(AgentAccess::ReadWrite),
        false,
        false,
        Some(approver.clone()),
        Limits::default(),
        Vec::new(),
    );
    fx.open_all();
    fx.sql.set_behavior(Behavior::Affected(1));
    let result = fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(result["affected_rows"], 1);
    assert_eq!(result["rows"], json!([]));
    assert_eq!(
        approver.asked.lock().unwrap().as_slice(),
        [("UPDATE t SET a = 1 WHERE id = 3".to_owned(), None)]
    );
    // A read does not ask.
    fx.sql.set_behavior(Behavior::Numbers { count: 2 });
    fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1"}),
    );
    assert_eq!(approver.asked.lock().unwrap().len(), 1);
    // The driver was told the user confirmed only for the dangerous case below.
    assert!(!fx.sql.runs.lock().unwrap()[0].1);
}

#[test]
fn a_denied_write_is_refused_and_never_reaches_the_driver() {
    let deny = ScriptedApprover::new(
        ApprovalDecision::Deny {
            reason: Some("not now".into()),
        },
        true,
    );
    let fx = fixture_with(
        Some(AgentAccess::ReadWrite),
        false,
        false,
        Some(deny),
        Limits::default(),
        Vec::new(),
    );
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "INSERT INTO t VALUES (1)"}),
    );
    assert_eq!(code(&outcome), "agent.write_refused");
    assert!(outcome.to_text().contains("not now"));
    assert_eq!(fx.sql.run_count(), 0);
    // The default approver refuses every write.
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "INSERT INTO t VALUES (1)"}),
    );
    assert_eq!(code(&outcome), "agent.write_refused");
    assert_eq!(fx.sql.run_count(), 0);
}

#[test]
fn dangerous_statements_need_the_flag_and_an_approver_that_allows_them() {
    let drop = |flagged: Option<bool>| {
        let mut args = json!({"connection_id": SQL, "statement": "DELETE FROM t"});
        if let Some(flag) = flagged {
            args["dangerous"] = json!(flag);
        }
        args
    };
    // MCP mode: approves writes, never dangerous ones.
    let mcp = ScriptedApprover::new(ApprovalDecision::Approve, false);
    let fx = fixture_with(
        Some(AgentAccess::ReadWrite),
        false,
        false,
        Some(mcp.clone()),
        Limits::default(),
        Vec::new(),
    );
    fx.open_all();
    assert_eq!(
        code(&fx.call("execute_query", drop(None))),
        "agent.dangerous_refused"
    );
    assert_eq!(
        code(&fx.call("execute_query", drop(Some(false)))),
        "agent.dangerous_refused"
    );
    assert_eq!(
        code(&fx.call("execute_query", drop(Some(true)))),
        "agent.dangerous_refused"
    );
    assert!(
        mcp.asked.lock().unwrap().is_empty(),
        "never asked for a dangerous statement"
    );
    assert_eq!(fx.sql.run_count(), 0);
    // A front end that never allows them says so at once and does not suggest the flag; the
    // retry with the flag gets the same answer, so there is nothing to retry.
    for flagged in [None, Some(true)] {
        let message = fx.call("execute_query", drop(flagged)).to_text();
        assert!(
            message.contains("test never runs dangerous statements"),
            "{message}"
        );
        assert!(message.contains("do not retry"), "{message}");
        assert!(!message.contains("dangerous=true"), "{message}");
    }
    // App mode: the flag makes the approver ask, with the reason, and the driver gets `confirmed`.
    let app = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let fx = fixture_with(
        Some(AgentAccess::ReadWrite),
        false,
        false,
        Some(app.clone()),
        Limits::default(),
        Vec::new(),
    );
    fx.open_all();
    assert_eq!(
        code(&fx.call("execute_query", drop(None))),
        "agent.dangerous_refused"
    );
    assert_eq!(app.asked.lock().unwrap().len(), 0);
    // A front end that asks about them still tells the model how to ask.
    let message = fx.call("execute_query", drop(None)).to_text();
    assert!(
        message.contains("call again with dangerous=true"),
        "{message}"
    );
    fx.sql.set_behavior(Behavior::Affected(9));
    assert_eq!(code(&fx.call("execute_query", drop(Some(true)))), "ok");
    let asked = app.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].1.as_deref().unwrap().contains("DELETE"));
    assert_eq!(
        fx.sql.runs.lock().unwrap().as_slice(),
        [("DELETE FROM t".to_owned(), true)]
    );
}

// ------------------------------------------------------------ statements

#[test]
fn explain_is_built_per_dialect_and_judged_like_any_statement() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    fx.ok(
        "explain_query",
        json!({"connection_id": SQL, "statement": "SELECT * FROM t"}),
    );
    assert_eq!(fx.sql.runs.lock().unwrap()[0].0, "EXPLAIN SELECT * FROM t");
    for (statement, want) in [
        ("EXPLAIN SELECT 1", "agent.invalid_arguments"),
        ("describe t", "agent.invalid_arguments"),
        ("SELECT 1; DROP TABLE t", "agent.multi_statement"),
        ("ANALYZE SELECT 1", "agent.write_refused"),
        ("", "agent.invalid_arguments"),
    ] {
        let outcome = fx.call(
            "explain_query",
            json!({"connection_id": SQL, "statement": statement}),
        );
        assert_eq!(code(&outcome), want, "{statement}");
    }
    assert_eq!(fx.sql.run_count(), 1, "only the plain EXPLAIN ran");
}

#[test]
fn language_mismatches_are_unsupported() {
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": REDIS, "statement": "SELECT 1"})
        )),
        "agent.unsupported"
    );
    assert_eq!(
        code(&fx.call(
            "execute_redis_command",
            json!({"connection_id": SQL, "command": "GET k"})
        )),
        "agent.unsupported"
    );
    assert_eq!(
        code(&fx.call("list_tables", json!({"connection_id": REDIS}))),
        "agent.unsupported"
    );
    assert_eq!(
        code(&fx.call(
            "describe_table",
            json!({"connection_id": REDIS, "table": "t"})
        )),
        "agent.unsupported"
    );
    // Redis databases can be listed.
    assert_eq!(
        code(&fx.call("list_namespaces", json!({"connection_id": REDIS}))),
        "ok"
    );
}

#[test]
fn redis_commands_are_one_line_and_blocked_verbs_stay_blocked() {
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    let redis = |command: &str| json!({"connection_id": REDIS, "command": command});
    assert_eq!(
        code(&fx.call("execute_redis_command", redis("GET a\nGET b"))),
        "agent.multi_statement"
    );
    assert_eq!(
        code(&fx.call("execute_redis_command", redis("   "))),
        "agent.invalid_arguments"
    );
    for blocked in [
        "CONFIG SET dir /tmp",
        "EVAL 1 0",
        "FLUSHALL",
        "script flush",
        "\"keys\" *",
    ] {
        assert_eq!(
            code(&fx.call("execute_redis_command", redis(blocked))),
            "agent.dangerous_refused",
            "{blocked}"
        );
    }
    assert_eq!(fx.redis.run_count(), 0);
}

#[test]
fn driver_errors_pass_through_with_secrets_redacted() {
    let secret = "Zq7-s3cr3t-pw".to_owned();
    let fx = fixture_with(
        Some(AgentAccess::Read),
        false,
        false,
        None,
        Limits::default(),
        vec![engine_agent::SecretText::from(secret.as_str())],
    );
    fx.open_all();
    fx.sql.set_behavior(Behavior::Fail(AgentError::driver(
        "query.failed",
        format!(
            "connect to postgres://admin:{secret}@db.internal/app failed; password={secret} host=x"
        ),
        true,
    )));
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1"}),
    );
    assert_eq!(code(&outcome), "query.failed");
    let text = outcome.to_text();
    assert!(!text.contains(&secret), "{text}");
    assert!(!text.contains("admin:"), "{text}");
    assert!(text.contains("db.internal"), "{text}");
    assert_eq!(outcome.to_json()["error"]["retryable"], true);
    // Even a secret inside the model's own statement text is scrubbed from the audit.
    fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": format!("SELECT '{secret}'")}),
    );
}

#[test]
fn a_lost_connection_asks_for_open_session_again() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    fx.sql.set_behavior(Behavior::Fail(AgentError::NotConnected(
        "the connection was lost".into(),
    )));
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1"}),
    );
    assert_eq!(code(&outcome), "agent.not_connected");
    let outcome = fx.call("list_tables", json!({"connection_id": "redis1"}));
    assert_eq!(code(&outcome), "agent.unsupported");
}

// -------------------------------------------------------------- sessions

#[test]
fn sessions_open_once_close_cleanly_and_are_required() {
    let fx = fixture(Some(AgentAccess::Read));
    let outcome = fx.call(
        "list_tables",
        json!({"connection_id": SQL, "database": "app", "schema": "public"}),
    );
    assert_eq!(code(&outcome), "agent.not_connected");
    let first = fx.ok("open_session", json!({"connection_id": SQL}));
    assert_eq!(first["already_open"], false);
    assert_eq!(first["access"], "read");
    assert_eq!(first["language"], "sql");
    assert_eq!(first["statement_timeout_secs"], 30);
    let second = fx.ok("open_session", json!({"connection_id": SQL}));
    assert_eq!(second["already_open"], true);
    assert_eq!(fx.provider.opened.load(Ordering::SeqCst), 1);
    let listed = fx.ok("list_connections", json!({}));
    let open: Vec<(&str, bool)> = listed["connections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap(), c["open"].as_bool().unwrap()))
        .collect();
    assert_eq!(open, [(REDIS, false), (SQL, true)]);
    assert_eq!(
        fx.ok("close_session", json!({"connection_id": SQL}))["closed"],
        true
    );
    assert_eq!(
        fx.ok("close_session", json!({"connection_id": SQL}))["closed"],
        false
    );
    assert_eq!(fx.sql.disconnects.load(Ordering::SeqCst), 1);
    assert_eq!(fx.provider.closed.load(Ordering::SeqCst), 1);
    // A failed open leaves nothing behind and reports the driver's error.
    fx.provider.fail_open(AgentError::driver(
        "connect.failed",
        "refused by postgres://u:pw@h/db",
        false,
    ));
    let outcome = fx.call("open_session", json!({"connection_id": SQL}));
    assert_eq!(code(&outcome), "connect.failed");
    assert!(!outcome.to_text().contains("u:pw"));
    assert!(!fx.layer.is_open(SQL));
    // Dropping the layer closes what is open.
    fx.ok("open_session", json!({"connection_id": SQL}));
    let sql = Arc::clone(&fx.sql);
    drop(fx);
    assert_eq!(sql.disconnects.load(Ordering::SeqCst), 2);
}

#[test]
fn namespaces_and_tables_page_and_fill_in_the_obvious() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    for name in ["orders", "users", "audit_log", "Zebra"] {
        fx.sql
            .tables
            .lock()
            .unwrap()
            .push(engine_agent::table::TableDescription {
                name: name.into(),
                database: None,
                schema: Some("public".into()),
                columns: Vec::new(),
                primary_key: Vec::new(),
                unique_columns: Vec::new(),
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
                ddl: None,
                ddl_truncated: false,
            });
    }
    let databases = fx.ok("list_namespaces", json!({"connection_id": SQL}));
    assert_eq!(databases["namespaces"].as_array().unwrap().len(), 2);
    assert_eq!(
        databases["namespaces"][0],
        json!({"name": "app", "kind": "database"})
    );
    let schemas = fx.ok(
        "list_namespaces",
        json!({"connection_id": SQL, "database": "app"}),
    );
    assert_eq!(
        schemas["namespaces"][0],
        json!({"name": "public", "kind": "schema"})
    );
    // Two databases and a schema level: both must be named.
    assert_eq!(
        code(&fx.call("list_tables", json!({"connection_id": SQL}))),
        "agent.invalid_arguments"
    );
    assert_eq!(
        code(&fx.call(
            "list_tables",
            json!({"connection_id": SQL, "database": "app"})
        )),
        "agent.invalid_arguments"
    );
    let page = fx.ok(
        "list_tables",
        json!({"connection_id": SQL, "database": "app", "schema": "public", "limit": 3}),
    );
    let names: Vec<&str> = page["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Zebra", "audit_log", "orders"]);
    let cursor = page["next_cursor"].as_str().unwrap();
    let rest = fx.ok("list_tables", json!({"connection_id": SQL, "database": "app", "schema": "public", "limit": 3, "cursor": cursor}));
    assert_eq!(rest["tables"], json!([{"name": "users", "kind": "table"}]));
    assert!(rest.get("next_cursor").is_none());
    let filtered = fx.ok(
        "list_tables",
        json!({"connection_id": SQL, "database": "app", "schema": "public", "filter": "ORD"}),
    );
    assert_eq!(
        filtered["tables"],
        json!([{"name": "orders", "kind": "table"}])
    );
}

// ----------------------------------------------------------------- limits

#[test]
fn rows_are_capped_and_truncation_is_reported() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    fx.sql.set_behavior(Behavior::Numbers { count: 5000 });
    let default = fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT n FROM big"}),
    );
    assert_eq!(default["row_count"], 200);
    assert_eq!(default["row_limit"], 200);
    assert_eq!(default["truncated"], true);
    assert_eq!(default["truncation"], "row_limit");
    let big = fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT n FROM big", "row_limit": 99999}),
    );
    assert_eq!(big["row_limit"], 1000);
    assert_eq!(big["row_count"], 1000);
    let few = fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT n FROM big", "row_limit": 3}),
    );
    assert_eq!(few["rows"], json!([["1"], ["2"], ["3"]]));
    fx.sql.set_behavior(Behavior::Numbers { count: 2 });
    let small = fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT n FROM small"}),
    );
    assert_eq!(small["truncated"], false);
    assert!(small.get("truncation").is_none());
}

#[test]
fn wide_cells_and_big_results_are_cut() {
    let limits = Limits {
        max_result_bytes: 1000,
        max_cell_bytes: 100,
        ..Limits::default()
    };
    let fx = fixture_with(
        Some(AgentAccess::Read),
        false,
        false,
        None,
        limits,
        Vec::new(),
    );
    fx.open_all();
    fx.sql.set_behavior(Behavior::WideCell { bytes: 10_000 });
    let wide = fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT big"}),
    );
    assert_eq!(wide["cells_truncated"], 1);
    let cell = wide["rows"][0][0].as_str().unwrap();
    assert!(
        cell.len() < 200 && cell.contains("[truncated, 10000 bytes in total]"),
        "{cell}"
    );
    fx.sql.set_behavior(Behavior::Numbers { count: 5000 });
    let many = fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT n", "row_limit": 1000}),
    );
    assert_eq!(many["truncation"], "byte_limit");
    assert_eq!(many["truncated"], true);
    assert!(many["row_count"].as_u64().unwrap() < 1000);
    // Statements over the length limit are refused before any parsing.
    let huge = "SELECT 1 /*".to_owned() + &"x".repeat(70_000) + "*/";
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": huge})
        )),
        "agent.limit"
    );
}

// ------------------------------------------------------------ cancellation

fn run_in_thread(
    fx: &Fixture,
    token: CancelToken,
    args: Value,
) -> std::thread::JoinHandle<ToolOutcome> {
    let layer = Arc::clone(&fx.layer);
    std::thread::spawn(move || layer.call_cancellable("execute_query", args, &token))
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_running_statement_cancels_and_the_session_stays_usable() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    fx.sql.set_behavior(Behavior::BlockUntilCancelled);
    let token = CancelToken::new();
    let running = run_in_thread(
        &fx,
        token.clone(),
        json!({"connection_id": SQL, "statement": "SELECT slow()"}),
    );
    wait_until("the statement to start", || fx.sql.run_count() == 1);
    token.cancel();
    let outcome = running.join().unwrap();
    assert_eq!(code(&outcome), "agent.cancelled", "{outcome:?}");
    fx.sql.set_behavior(Behavior::Numbers { count: 1 });
    assert_eq!(
        code(&fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1"})
        )),
        "ok"
    );
}

#[test]
fn dropping_the_cancel_guard_cancels_the_statement() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    fx.sql.set_behavior(Behavior::BlockUntilCancelled);
    let token = CancelToken::new();
    let guard = token.cancel_on_drop();
    let running = run_in_thread(
        &fx,
        token,
        json!({"connection_id": SQL, "statement": "SELECT slow()"}),
    );
    wait_until("the statement to start", || fx.sql.run_count() == 1);
    drop(guard);
    assert_eq!(code(&running.join().unwrap()), "agent.cancelled");
}

#[test]
fn the_statement_timeout_cancels_and_says_so() {
    let limits = Limits {
        statement_timeout: Duration::from_millis(150),
        ..Limits::default()
    };
    let fx = fixture_with(
        Some(AgentAccess::Read),
        false,
        false,
        None,
        limits,
        Vec::new(),
    );
    fx.open_all();
    fx.sql.set_behavior(Behavior::BlockUntilCancelled);
    let started = Instant::now();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT slow()"}),
    );
    assert_eq!(code(&outcome), "agent.limit");
    assert!(
        outcome.to_text().contains("statement timeout"),
        "{}",
        outcome.to_text()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // The connection's own timeout wins over the default.
    fx.catalog
        .update(SQL, |info| info.statement_timeout_secs = Some(1));
    let started = Instant::now();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT slow()"}),
    );
    assert_eq!(code(&outcome), "agent.limit");
    assert!(started.elapsed() >= Duration::from_millis(900));
}

#[test]
fn only_one_statement_runs_per_session() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    fx.sql.set_behavior(Behavior::BlockUntilCancelled);
    let token = CancelToken::new();
    let running = run_in_thread(
        &fx,
        token.clone(),
        json!({"connection_id": SQL, "statement": "SELECT slow()"}),
    );
    wait_until("the statement to start", || fx.sql.run_count() == 1);
    let second = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 2"}),
    );
    assert_eq!(code(&second), "agent.limit");
    assert!(second.to_text().contains("another statement"));
    // Another connection is not blocked, and neither is browsing this one.
    assert_eq!(
        code(&fx.call(
            "execute_redis_command",
            json!({"connection_id": REDIS, "command": "GET k"})
        )),
        "ok"
    );
    assert_eq!(
        code(&fx.call("list_namespaces", json!({"connection_id": SQL}))),
        "ok"
    );
    token.cancel();
    assert_eq!(code(&running.join().unwrap()), "agent.cancelled");
    assert_eq!(fx.sql.run_count(), 1, "the refused statement never ran");
}

#[test]
fn cancelling_while_the_approval_is_open_is_a_cancel() {
    struct Hanging;
    impl engine_agent::WriteApprover for Hanging {
        fn approve(
            &self,
            _: &engine_agent::WriteApproval<'_>,
            cancel: &CancelToken,
        ) -> ApprovalDecision {
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
            ApprovalDecision::Deny { reason: None }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let _ = dir;
    let fx = fixture(Some(AgentAccess::ReadWrite));
    let layer = Arc::new(
        ToolLayer::new(fx.catalog.clone(), fx.provider.clone(), "test")
            .with_approver(Arc::new(Hanging)),
    );
    layer.call("open_session", json!({"connection_id": SQL}));
    let token = CancelToken::new();
    let worker = {
        let (layer, token) = (Arc::clone(&layer), token.clone());
        std::thread::spawn(move || {
            layer.call_cancellable(
                "execute_query",
                json!({"connection_id": SQL, "statement": "INSERT INTO t VALUES (1)"}),
                &token,
            )
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    token.cancel();
    assert_eq!(code(&worker.join().unwrap()), "agent.cancelled");
    assert_eq!(fx.sql.run_count(), 0);
}

// ------------------------------------------------------------ schema tool

fn column(
    name: &str,
    ty: &str,
    pk: bool,
    comment: Option<&str>,
) -> engine_agent::table::ColumnDescription {
    engine_agent::table::ColumnDescription {
        name: name.into(),
        data_type: ty.into(),
        nullable: !pk,
        primary_key: pk,
        default: None,
        comment: comment.map(str::to_owned),
    }
}

fn seed_tables(fx: &Fixture, count: usize) {
    let mut tables = fx.sql.tables.lock().unwrap();
    for n in 0..count {
        tables.push(engine_agent::table::TableDescription {
            name: format!("t{n:03}"),
            database: None,
            schema: Some("public".into()),
            columns: vec![
                column("id", "bigint", true, None),
                column("note", "text", false, Some("free text")),
            ],
            primary_key: vec!["id".into()],
            unique_columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            ddl: None,
            ddl_truncated: false,
        });
    }
}

#[test]
fn the_schema_context_tool_is_deterministic_and_respects_the_budget() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    seed_tables(&fx, 12);
    let args =
        json!({"connection_id": SQL, "database": "app", "schema": "public", "budget": 100_000});
    let full = fx.ok("get_schema_context", args.clone());
    let again = fx.ok("get_schema_context", args);
    assert_eq!(full, again);
    assert_eq!(full["tables_included"], 12);
    assert!(full["text"]
        .as_str()
        .unwrap()
        .starts_with("-- schema: app.public (postgresql), 12 tables\nTABLE t000\n"));
    let tight = fx.ok(
        "get_schema_context",
        json!({"connection_id": SQL, "database": "app", "schema": "public", "budget": 120}),
    );
    assert_eq!(tight["within_budget"], true);
    assert!(tight["tables_included"].as_u64().unwrap() < 12);
    assert!(tight["estimated_tokens"].as_u64().unwrap() <= 120);
    assert!(tight["text"].as_str().unwrap().contains("-- omitted:"));
    // Tables beyond the describe cap are listed as not shown rather than dropped silently.
    let limits = Limits {
        max_context_tables: 5,
        ..Limits::default()
    };
    let capped = fixture_with(
        Some(AgentAccess::SchemaOnly),
        false,
        false,
        None,
        limits,
        Vec::new(),
    );
    capped.open_all();
    seed_tables(&capped, 8);
    let result = capped.ok(
        "get_schema_context",
        json!({"connection_id": SQL, "database": "app", "schema": "public", "budget": 100_000}),
    );
    assert_eq!(result["tables_total"], 8);
    assert_eq!(result["tables_included"], 5);
    assert_eq!(result["omitted_tables"], json!(["t005", "t006", "t007"]));
}

// ----------------------------------------------------------- audit + redaction

#[test]
fn every_call_is_audited_without_secrets_or_data() {
    let secret = "Zq7-s3cr3t-pw".to_owned();
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, false);
    let fx = fixture_with(
        Some(AgentAccess::ReadWrite),
        false,
        false,
        Some(approver),
        Limits::default(),
        vec![engine_agent::SecretText::from(secret.as_str())],
    );
    fx.open_all();
    fx.sql.set_behavior(Behavior::Numbers { count: 3 });
    fx.call("list_connections", json!({}));
    fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT n FROM t"}),
    );
    fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "CREATE ROLE bob PASSWORD 'hunter2hunter2'"}),
    );
    fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "DROP TABLE t"}),
    );
    fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": format!("SELECT '{secret}'")}),
    );
    fx.sql.set_behavior(Behavior::Fail(AgentError::driver(
        "query.failed",
        format!("could not connect to postgres://u:{secret}@h/db"),
        false,
    )));
    fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1"}),
    );
    fx.call("nonsense_tool", json!({}));
    fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1", "surprise": 1}),
    );
    fx.layer.close_all();
    // The audit is flushed by `audit()`: 2 opens, list_connections, the 5 statements,
    // the write-ahead line of the approved CREATE ROLE, the unknown tool and the bad arguments.
    let entries = fx.audit();
    assert_eq!(entries.len(), 11, "{entries:#?}");
    let mut by_time: Vec<_> = entries.iter().rev().collect();
    by_time.sort_by(|a, b| a.time.cmp(&b.time));
    assert!(entries.iter().all(|e| e.front_end == "test"));
    let find = |tool: &str, outcome: &str| {
        entries
            .iter()
            .filter(|e| e.tool == tool && e.outcome == outcome)
            .count()
    };
    assert_eq!(find("open_session", "ok"), 2);
    assert_eq!(find("list_connections", "ok"), 1);
    assert_eq!(
        find("execute_query", "ok"),
        3,
        "select, create role (approved) and the secret select"
    );
    assert_eq!(find("execute_query", "agent.dangerous_refused"), 1);
    assert_eq!(find("execute_query", "query.failed"), 1);
    assert_eq!(find("execute_query", "agent.invalid_arguments"), 1);
    assert_eq!(find("nonsense_tool", "agent.unknown_tool"), 1);
    let select = entries
        .iter()
        .find(|e| e.statement.as_deref() == Some("SELECT n FROM t"))
        .unwrap();
    assert_eq!(select.rows, Some(3));
    assert_eq!(select.connection_id.as_deref(), Some(SQL));
    // Scan every byte of every audit file for anything that must not be there.
    let mut text = String::new();
    for entry in std::fs::read_dir(fx.audit_path.parent().unwrap()).unwrap() {
        text.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    for needle in [secret.as_str(), "hunter2hunter2", "u:Zq7", "postgres://"] {
        assert!(
            !text.contains(needle),
            "{needle} leaked into the audit log:\n{text}"
        );
    }
    assert!(text.contains("PASSWORD '***'"), "{text}");
    // Result data never appears: the numbers the fake returns are only counted.
    assert!(!text.contains("\"rows\":[["));
}

// ---- sessions follow the connection's access ---------------------------------------------

fn closed(fx: &Fixture) -> usize {
    fx.provider.closed.load(std::sync::atomic::Ordering::SeqCst)
}

#[test]
fn lowering_access_to_off_closes_the_open_session_and_its_front_end_state_on_the_next_call() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.ok("open_session", json!({"connection_id": SQL}));
    assert!(fx.layer.is_open(SQL));
    fx.catalog
        .update(SQL, |info| info.agent_access = Some(AgentAccess::Off));
    let outcome = fx.call("list_tables", json!({"connection_id": SQL}));
    assert_eq!(outcome.code(), Some("agent.access_denied"));
    assert!(
        !fx.layer.is_open(SQL),
        "the session did not outlive the permission"
    );
    assert_eq!(
        closed(&fx),
        1,
        "the front end was told, so it can drop the tunnel"
    );
    // Listing alone also notices.
    fx.catalog
        .update(SQL, |info| info.agent_access = Some(AgentAccess::Read));
    fx.ok("open_session", json!({"connection_id": SQL}));
    fx.catalog
        .update(SQL, |info| info.agent_access = Some(AgentAccess::Off));
    fx.ok("list_connections", json!({}));
    assert!(!fx.layer.is_open(SQL));
    assert_eq!(closed(&fx), 2);
}

#[test]
fn a_connection_that_vanished_closes_its_session_and_revoke_is_idempotent() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.ok("open_session", json!({"connection_id": REDIS}));
    fx.catalog.update(REDIS, |info| info.id = "renamed".into());
    let outcome = fx.call("list_namespaces", json!({"connection_id": REDIS}));
    assert_eq!(outcome.code(), Some("agent.invalid_arguments"));
    assert!(!fx.layer.is_open(REDIS));
    assert_eq!(closed(&fx), 1);
    // An explicit revoke by the front end (edit / delete) works the same, and twice is fine.
    fx.ok("open_session", json!({"connection_id": SQL}));
    fx.layer.revoke(SQL);
    fx.layer.revoke(SQL);
    assert!(!fx.layer.is_open(SQL));
    assert_eq!(closed(&fx), 2);
}

// ---- correction 2: revisions, concurrent opens, statements in flight ------------------------

#[test]
fn an_edited_connection_closes_its_session_and_the_next_call_says_to_reopen() {
    use engine_agent::Revision;
    let fx = fixture(Some(AgentAccess::Read));
    fx.catalog
        .update(SQL, |info| info.revision = Revision::new(1));
    fx.ok("open_session", json!({"connection_id": SQL}));
    assert!(fx.layer.is_open(SQL));
    // The same revision keeps the session; the model never sees a revision.
    let listed = fx.ok("list_connections", json!({}));
    assert!(
        !listed.to_string().to_lowercase().contains("revision"),
        "{listed}"
    );
    assert!(fx.layer.is_open(SQL));
    let info = engine_agent::ConnectionCatalog::get(&*fx.catalog, SQL).unwrap();
    assert!(
        format!("{info:?}").contains("Revision(<opaque>)"),
        "it is never printed"
    );
    assert!(!serde_json::to_string(&info)
        .unwrap()
        .to_lowercase()
        .contains("revision"));
    // The connection is edited (another address): the session to the old target is closed.
    fx.catalog
        .update(SQL, |info| info.revision = Revision::new(2));
    let outcome = fx.call("list_tables", json!({"connection_id": SQL}));
    assert_eq!(outcome.code(), Some("agent.not_connected"));
    assert!(!fx.layer.is_open(SQL));
    assert_eq!(
        closed(&fx),
        1,
        "the provider was told, so it drops the tunnel"
    );
    // open_session simply connects anew, under the new revision, and works.
    let reopened = fx.ok("open_session", json!({"connection_id": SQL}));
    assert_eq!(reopened["already_open"], false);
    assert_eq!(
        fx.provider.opened.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    fx.ok("list_namespaces", json!({"connection_id": SQL}));
    // Listing alone also notices an edit.
    fx.catalog
        .update(SQL, |info| info.revision = Revision::new(3));
    fx.ok("list_connections", json!({}));
    assert!(!fx.layer.is_open(SQL));
    // An unknown revision (a catalog that has none) is never compared.
    fx.catalog
        .update(SQL, |info| info.revision = Revision::UNKNOWN);
    fx.ok("open_session", json!({"connection_id": SQL}));
    fx.catalog
        .update(SQL, |info| info.revision = Revision::new(9));
    assert!(fx.layer.is_open(SQL));
    fx.ok("list_namespaces", json!({"connection_id": SQL}));
}

/// Opens block until released, and count how many are in flight at once.
struct SlowProvider {
    session: Arc<engine_agent::testing::FakeSession>,
    opens: std::sync::atomic::AtomicUsize,
    in_flight: std::sync::atomic::AtomicUsize,
    max_in_flight: std::sync::atomic::AtomicUsize,
    closes: std::sync::atomic::AtomicUsize,
    first_in: (std::sync::Mutex<bool>, std::sync::Condvar),
    second_started: std::sync::atomic::AtomicBool,
}

impl engine_agent::SessionProvider for SlowProvider {
    fn open(
        &self,
        _connection: &engine_agent::ConnectionInfo,
    ) -> Result<Arc<dyn engine_agent::Session>, engine_agent::AgentError> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.max_in_flight.fetch_max(now, SeqCst);
        if self.opens.fetch_add(1, SeqCst) == 0 {
            // The first connect: tell the test, then wait until the second caller has started
            // (and, for a layer that does not serialize, until it had the chance to get in here).
            *self.first_in.0.lock().unwrap() = true;
            self.first_in.1.notify_all();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !self.second_started.load(SeqCst) && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            let grace = std::time::Instant::now() + std::time::Duration::from_millis(40);
            while self.in_flight.load(SeqCst) < 2 && std::time::Instant::now() < grace {
                std::thread::yield_now();
            }
        }
        self.in_flight.fetch_sub(1, SeqCst);
        let session: Arc<dyn engine_agent::Session> = Arc::clone(&self.session) as _;
        Ok(session)
    }

    fn close(&self, _connection: &engine_agent::ConnectionInfo) {
        self.closes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn two_concurrent_opens_of_one_connection_connect_once_and_keep_the_survivors_tunnel() {
    use std::sync::atomic::Ordering::SeqCst;
    for round in 0..20 {
        let catalog =
            engine_agent::testing::FakeCatalog::new(vec![engine_agent::testing::connection(
                SQL,
                ConnectionKind::PostgreSql,
                Some(AgentAccess::Read),
            )]);
        let provider = Arc::new(SlowProvider {
            session: engine_agent::testing::FakeSession::new(ConnectionKind::PostgreSql).arc(),
            opens: 0.into(),
            in_flight: 0.into(),
            max_in_flight: 0.into(),
            closes: 0.into(),
            first_in: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
            second_started: false.into(),
        });
        let layer = Arc::new(ToolLayer::new(catalog, provider.clone(), "test"));
        let first = {
            let layer = Arc::clone(&layer);
            std::thread::spawn(move || layer.call("open_session", json!({"connection_id": SQL})))
        };
        {
            let (flag, signal) = &provider.first_in;
            let mut entered = flag.lock().unwrap();
            while !*entered {
                entered = signal.wait(entered).unwrap();
            }
        }
        let second = {
            let layer = Arc::clone(&layer);
            let provider = Arc::clone(&provider);
            std::thread::spawn(move || {
                provider.second_started.store(true, SeqCst);
                layer.call("open_session", json!({"connection_id": SQL}))
            })
        };
        let (first, second) = (first.join().unwrap(), second.join().unwrap());
        assert!(
            !first.is_error() && !second.is_error(),
            "round {round}: {first:?} {second:?}"
        );
        assert_eq!(provider.opens.load(SeqCst), 1, "round {round}: one connect");
        assert_eq!(
            provider.max_in_flight.load(SeqCst),
            1,
            "round {round}: never two at once"
        );
        assert_eq!(
            provider.closes.load(SeqCst),
            0,
            "round {round}: the survivor keeps its tunnel"
        );
        let already: Vec<bool> = [&first, &second]
            .iter()
            .map(|outcome| match outcome {
                ToolOutcome::Success(value) => value["already_open"].as_bool().unwrap(),
                ToolOutcome::Failure(_) => unreachable!(),
            })
            .collect();
        assert_eq!(
            already.iter().filter(|open| **open).count(),
            1,
            "round {round}: {already:?}"
        );
        let listed = layer.call("list_namespaces", json!({"connection_id": SQL}));
        assert!(
            !listed.is_error(),
            "round {round}: the session works afterwards"
        );
    }
}

#[test]
fn revoking_cancels_the_statement_that_is_running() {
    use engine_agent::testing::Behavior;
    let fx = fixture(Some(AgentAccess::Read));
    fx.ok("open_session", json!({"connection_id": SQL}));
    fx.sql.set_behavior(Behavior::BlockUntilCancelled);
    let running = {
        let layer = Arc::clone(&fx.layer);
        std::thread::spawn(move || {
            layer.call(
                "execute_query",
                json!({"connection_id": SQL, "statement": "SELECT pg_sleep(60)"}),
            )
        })
    };
    // The statement reached the session and is blocked in it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fx.sql.run_count() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(fx.sql.run_count(), 1, "the statement is in flight");
    fx.layer.revoke(SQL);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(running.join().unwrap());
    });
    let outcome = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("revoke cancelled the running statement");
    assert_eq!(outcome.code(), Some("agent.cancelled"));
    assert!(!fx.layer.is_open(SQL));
    assert_eq!(
        fx.sql.disconnects.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

/// Sessions are kept under the catalog's spelling of an id; a caller may use another one.
struct Lowercase(Arc<FakeCatalog>);

impl engine_agent::ConnectionCatalog for Lowercase {
    fn list(&self) -> Vec<ConnectionInfo> {
        self.0.list()
    }

    fn canonical_id(&self, id: &str) -> String {
        id.to_lowercase()
    }
}

#[test]
fn every_spelling_of_an_id_reaches_the_same_session() {
    let provider = FakeProvider::new();
    provider.with_session("sql1", FakeSession::new(ConnectionKind::PostgreSql).arc());
    let catalog = FakeCatalog::new(vec![connection(
        "sql1",
        ConnectionKind::PostgreSql,
        Some(AgentAccess::Read),
    )]);
    let layer = ToolLayer::new(Arc::new(Lowercase(catalog)), provider.clone(), "test");
    assert_eq!(
        layer
            .call("open_session", json!({"connection_id": "SQL1"}))
            .code(),
        None,
        "a call may spell the id as the store does"
    );
    for spelling in ["sql1", "SQL1", "Sql1"] {
        assert!(layer.is_open(spelling), "is_open({spelling})");
    }
    let again = layer.call("open_session", json!({"connection_id": "sql1"}));
    let ToolOutcome::Success(again) = again else {
        panic!("{again:?}")
    };
    assert_eq!(again["already_open"], true, "one session, not two");
    assert_eq!(provider.opened.load(Ordering::SeqCst), 1);
    layer.revoke("SQL1");
    assert!(
        !layer.is_open("sql1"),
        "revoke with the other spelling closes it"
    );
    assert_eq!(provider.closed.load(Ordering::SeqCst), 1);
    layer.call("open_session", json!({"connection_id": "sql1"}));
    layer.revoke("sql1");
    assert!(!layer.is_open("SQL1"));
}

#[test]
fn a_call_that_panics_still_leaves_an_audit_line_without_the_payload() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.ok("open_session", json!({"connection_id": SQL}));
    fx.sql.set_behavior(Behavior::Script(Box::new(|_, _| {
        panic!("payload-with-connection-text-4471")
    })));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1"}),
        )
    }));
    assert!(caught.is_err(), "the panic goes on to the front end");
    let line = fx
        .audit()
        .into_iter()
        .find(|entry| entry.tool == "execute_query")
        .expect("the call was audited");
    assert_eq!(line.outcome, "agent.internal");
    assert_eq!(line.connection_id.as_deref(), Some(SQL));
    assert_eq!(line.statement.as_deref(), Some("SELECT 1"));
    let file = std::fs::read_to_string(&fx.audit_path).unwrap();
    assert!(
        !file.contains("payload-with-connection-text-4471"),
        "{file}"
    );
    // The layer is still usable.
    fx.sql.set_behavior(Behavior::Numbers { count: 1 });
    fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1"}),
    );
}

// ------------------------------------------------- A2a correction 1: what an approval sees

/// An approver that remembers the whole request and then does `act`.
struct Probe {
    seen: std::sync::Mutex<Vec<(String, bool, engine_agent::Language)>>,
    act: Box<dyn Fn(&CancelToken) -> ApprovalDecision + Send + Sync>,
    /// What `still_allowed` answers after a yes.
    veto: Option<String>,
}

impl Probe {
    fn new(act: impl Fn(&CancelToken) -> ApprovalDecision + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            seen: Default::default(),
            act: Box::new(act),
            veto: None,
        })
    }

    /// Says yes, and finds afterwards that the write may no longer run.
    fn approving_then_vetoing(reason: &str) -> Arc<Self> {
        Arc::new(Self {
            seen: Default::default(),
            act: Box::new(|_| ApprovalDecision::Approve),
            veto: Some(reason.to_owned()),
        })
    }
}

impl engine_agent::WriteApprover for Probe {
    fn approve(
        &self,
        request: &engine_agent::WriteApproval<'_>,
        cancel: &CancelToken,
    ) -> ApprovalDecision {
        self.seen.lock().unwrap().push((
            request.connection_type.to_owned(),
            request.production,
            request.language,
        ));
        (self.act)(cancel)
    }

    fn still_allowed(&self, _request: &engine_agent::WriteApproval<'_>) -> Result<(), String> {
        self.veto.clone().map_or(Ok(()), Err)
    }

    fn allows_dangerous(&self) -> bool {
        true
    }
}

fn with_probe(
    setting: AgentAccess,
    production: bool,
    approver: Arc<Probe>,
) -> (Fixture, Arc<Probe>) {
    // `fixture_with` takes a ScriptedApprover; build the layer around the probe instead.
    let fx = fixture_with(
        Some(setting),
        false,
        production,
        None,
        Limits::default(),
        Vec::new(),
    );
    let layer = ToolLayer::new(fx.catalog.clone(), fx.provider.clone(), "test")
        .with_audit(Arc::clone(&fx.audit))
        .with_approver(approver.clone());
    (
        Fixture {
            layer: Arc::new(layer),
            catalog: fx.catalog.clone(),
            sql: fx.sql.clone(),
            redis: fx.redis.clone(),
            provider: fx.provider.clone(),
            audit: fx.audit.clone(),
            audit_path: fx.audit_path.clone(),
            _dir: fx._dir,
        },
        approver,
    )
}

#[test]
fn a_bare_carriage_return_is_refused_before_anything_runs_and_crlf_is_not() {
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    // PostgreSQL ends a `--` comment at the `\r`; the lexer and every editor only at `\n`.
    let sneaky = "DELETE FROM users WHERE id = 1 -- note\r OR true";
    for (tool, args) in [
        (
            "execute_query",
            json!({"connection_id": SQL, "statement": sneaky}),
        ),
        (
            "explain_query",
            json!({"connection_id": SQL, "statement": sneaky}),
        ),
        (
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1\r"}),
        ),
        (
            "execute_redis_command",
            json!({"connection_id": REDIS, "command": "GET a\rb"}),
        ),
    ] {
        let outcome = fx.call(tool, args);
        assert_eq!(code(&outcome), "agent.invalid_arguments", "{tool}");
        let ToolOutcome::Failure(error) = outcome else {
            panic!("{tool} succeeded")
        };
        assert!(
            error.message.contains("carriage return"),
            "{}",
            error.message
        );
    }
    assert_eq!(fx.sql.run_count(), 0);
    assert_eq!(fx.redis.run_count(), 0);
    // A Windows line break is two characters that belong together: fine.
    fx.sql.set_behavior(Behavior::Numbers { count: 1 });
    fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1\r\nFROM t\r\n"}),
    );
    assert_eq!(fx.sql.run_count(), 1);
}

#[test]
fn the_approver_is_told_the_type_the_policy_used_and_the_language() {
    let probe = Probe::new(|_| ApprovalDecision::Approve);
    let (fx, probe) = with_probe(AgentAccess::ReadWrite, true, probe);
    fx.open_all();
    fx.sql.set_behavior(Behavior::Affected(1));
    fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(
        probe.seen.lock().unwrap().as_slice(),
        [("Production".to_owned(), true, engine_agent::Language::Sql)]
    );
}

#[test]
fn a_connection_made_read_only_while_the_question_waited_cannot_be_written_to() {
    let catalog_slot: Arc<std::sync::Mutex<Option<Arc<FakeCatalog>>>> = Default::default();
    let hook = Arc::clone(&catalog_slot);
    // The user edits the connection type while the card is open, then clicks Run.
    let probe = Probe::new(move |_| {
        if let Some(catalog) = hook.lock().unwrap().as_ref() {
            catalog.update(SQL, |info| info.connection_type.read_only = true);
        }
        ApprovalDecision::Approve
    });
    let (fx, _) = with_probe(AgentAccess::ReadWrite, false, probe);
    *catalog_slot.lock().unwrap() = Some(fx.catalog.clone());
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(code(&outcome), "agent.access_denied", "{outcome:?}");
    assert_eq!(fx.sql.run_count(), 0, "nothing reached the driver");
    let entries = fx.audit();
    assert!(
        entries.iter().any(|e| e.tool == "execute_query"
            && e.outcome == "agent.access_denied"
            && e.approval
                .as_deref()
                .is_some_and(|a| a.starts_with("approved, then refused"))),
        "{entries:#?}"
    );
    assert!(
        !entries.iter().any(|e| e.outcome == "started"),
        "no write-ahead line for a write that did not start"
    );
}

#[test]
fn a_level_lowered_or_a_connection_edited_while_the_question_waited_refuses_the_write() {
    for edit in ["lowered", "edited", "removed"] {
        let slot: Arc<std::sync::Mutex<Option<Arc<FakeCatalog>>>> = Default::default();
        let hook = Arc::clone(&slot);
        let probe = Probe::new(move |_| {
            if let Some(catalog) = hook.lock().unwrap().as_ref() {
                match edit {
                    "lowered" => {
                        catalog.update(SQL, |info| info.agent_access = Some(AgentAccess::Read))
                    }
                    "edited" => catalog.update(SQL, |info| {
                        info.revision = engine_agent::ports::Revision::new(9)
                    }),
                    _ => catalog.update(SQL, |info| info.id = "gone".into()),
                }
            }
            ApprovalDecision::Approve
        });
        let (fx, _) = with_probe(AgentAccess::ReadWrite, false, probe);
        *slot.lock().unwrap() = Some(fx.catalog.clone());
        fx.catalog.update(SQL, |info| {
            info.revision = engine_agent::ports::Revision::new(1)
        });
        fx.open_all();
        let outcome = fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
        );
        let want = match edit {
            "lowered" => "agent.access_denied",
            _ => "agent.not_connected",
        };
        assert_eq!(code(&outcome), want, "{edit}: {outcome:?}");
        assert_eq!(fx.sql.run_count(), 0, "{edit}");
        let line = fx.audit();
        let line = line.iter().find(|e| e.tool == "execute_query").unwrap();
        assert_eq!(line.outcome, want, "{edit}: the record and the code agree");
        assert_eq!(
            line.approval.as_deref(),
            Some(match edit {
                "lowered" => "approved, then refused: the access was lowered",
                "edited" => "approved, then refused: the connection was edited",
                _ => "approved, then refused: the connection was removed",
            }),
            "{edit}"
        );
    }
}

#[test]
fn the_audit_line_says_what_the_approver_decided() {
    // Approved: the write-ahead line and the final one both say so.
    let (fx, _) = with_probe(
        AgentAccess::ReadWrite,
        false,
        Probe::new(|_| ApprovalDecision::Approve),
    );
    fx.open_all();
    fx.sql.set_behavior(Behavior::Affected(1));
    fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    let entries = fx.audit();
    let writes: Vec<_> = entries
        .iter()
        .filter(|e| e.tool == "execute_query")
        .map(|e| (e.outcome.as_str(), e.approval.as_deref()))
        .collect();
    assert_eq!(
        writes,
        [("ok", Some("approved")), ("started", Some("approved"))],
        "newest first"
    );

    // Declined: the model still sees write_refused; the log says it was the user.
    let (fx, _) = with_probe(
        AgentAccess::ReadWrite,
        false,
        Probe::new(|_| ApprovalDecision::Deny {
            reason: Some("the user declined".into()),
        }),
    );
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(code(&outcome), "agent.write_refused");
    let entries = fx.audit();
    let line = entries.iter().find(|e| e.tool == "execute_query").unwrap();
    assert_eq!(line.outcome, "agent.write_refused");
    assert_eq!(
        line.approval.as_deref(),
        Some("declined: the user declined")
    );

    // Cancelled while it waited.
    let (fx, _) = with_probe(
        AgentAccess::ReadWrite,
        false,
        Probe::new(|cancel| {
            cancel.cancel();
            ApprovalDecision::Deny { reason: None }
        }),
    );
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(code(&outcome), "agent.cancelled");
    let entries = fx.audit();
    let line = entries.iter().find(|e| e.tool == "execute_query").unwrap();
    assert_eq!(line.approval.as_deref(), Some("cancelled"));

    // No usable answer: not a person's no. Audited and worded as what it is.
    let (fx, _) = with_probe(
        AgentAccess::ReadWrite,
        false,
        Probe::new(|_| ApprovalDecision::Unanswered {
            reason: "no usable answer came back".into(),
        }),
    );
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(code(&outcome), "agent.write_refused");
    assert_eq!(
        outcome.to_text(),
        r#"{"error":{"code":"agent.write_refused","message":"this SQL statement was not run: no usable answer came back","retryable":false}}"#
    );
    let entries = fx.audit();
    let line = entries.iter().find(|e| e.tool == "execute_query").unwrap();
    assert_eq!(
        line.approval.as_deref(),
        Some("unanswered: no usable answer came back")
    );
    assert_eq!(fx.sql.run_count(), 0);

    // A yes, and then the front end's own condition no longer holds: approved, then refused, and
    // the model is told it was approved but cannot run.
    let (fx, _) = with_probe(
        AgentAccess::ReadWrite,
        false,
        Probe::approving_then_vetoing("the switch is off"),
    );
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(code(&outcome), "agent.write_refused");
    assert!(
        outcome
            .to_text()
            .contains("the change was approved, but it can no longer run: the switch is off"),
        "{}",
        outcome.to_text()
    );
    let entries = fx.audit();
    let line = entries.iter().find(|e| e.tool == "execute_query").unwrap();
    assert_eq!(
        line.approval.as_deref(),
        Some("approved, then refused: the switch is off")
    );
    assert_eq!(fx.sql.run_count(), 0, "nothing reached the driver");
    assert!(
        entries.iter().all(|e| e.outcome != "started"),
        "no write-ahead line for a write that did not start"
    );

    // Nothing was asked of a read: no approval on its line.
    let (fx, _) = with_probe(
        AgentAccess::ReadWrite,
        false,
        Probe::new(|_| ApprovalDecision::Approve),
    );
    fx.open_all();
    fx.ok(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 1"}),
    );
    assert!(fx.audit().iter().all(|e| e.approval.is_none()));
}

#[test]
fn the_audit_tells_a_persons_no_from_a_front_end_that_never_asked_and_a_cancel_after_the_yes() {
    // A front end that cannot ask (the default approver): `refused`, not `declined`.
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(code(&outcome), "agent.write_refused");
    let entries = fx.audit();
    let line = entries.iter().find(|e| e.tool == "execute_query").unwrap();
    assert_eq!(
        line.approval.as_deref(),
        Some("refused: this front end cannot ask the user to approve writes")
    );
    // A person said yes and the call was cancelled before the write started.
    let (fx, _) = with_probe(
        AgentAccess::ReadWrite,
        false,
        Probe::new(|cancel| {
            cancel.cancel();
            ApprovalDecision::Approve
        }),
    );
    fx.open_all();
    let outcome = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert_eq!(code(&outcome), "agent.cancelled");
    let entries = fx.audit();
    let line = entries.iter().find(|e| e.tool == "execute_query").unwrap();
    assert_eq!(line.approval.as_deref(), Some("approved, then cancelled"));
    assert_eq!(fx.sql.run_count(), 0);
}

#[test]
fn a_database_name_with_a_character_that_has_no_shape_is_refused_by_every_statement_tool() {
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    for (tool, args) in [
        (
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1", "database": "prod\u{200B}"}),
        ),
        (
            "explain_query",
            json!({"connection_id": SQL, "statement": "SELECT 1", "database": "pr\u{202E}od"}),
        ),
        (
            "execute_redis_command",
            json!({"connection_id": REDIS, "command": "GET a", "database": "0\u{E0041}"}),
        ),
    ] {
        let outcome = fx.call(tool, args);
        assert_eq!(code(&outcome), "agent.invalid_arguments", "{tool}");
        let ToolOutcome::Failure(error) = outcome else {
            panic!("{tool} succeeded")
        };
        assert!(error.message.contains("no shape"), "{}", error.message);
    }
    assert_eq!(fx.sql.run_count(), 0);
    assert_eq!(fx.redis.run_count(), 0);
}

#[test]
fn a_database_that_cannot_be_named_is_said_to_be_unusable_not_to_be_reworded() {
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    let long = "d".repeat(300);
    for (name, why) in [
        ("prod\u{200B}".to_owned(), "no shape"),
        (long.clone(), "300 characters"),
    ] {
        let ToolOutcome::Failure(error) = fx.call(
            "execute_query",
            json!({"connection_id": SQL, "statement": "SELECT 1", "database": name}),
        ) else {
            panic!("a database that cannot be shown ran")
        };
        assert_eq!(error.code, "agent.invalid_arguments");
        assert!(error.message.contains(why), "{}", error.message);
        assert!(
            error
                .message
                .contains("cannot be used by the statement tools"),
            "{}",
            error.message
        );
        assert!(
            !error.message.contains("visible characters only"),
            "it must not ask for the impossible: {}",
            error.message
        );
        assert!(
            !error.message.contains(&long),
            "the long name is not echoed back"
        );
    }
    // A name of exactly the cap is fine.
    let exact = "d".repeat(256);
    let outcome = fx.call(
        "explain_query",
        json!({"connection_id": SQL, "statement": "SELECT 1", "database": exact}),
    );
    assert!(
        !matches!(&outcome, ToolOutcome::Failure(e) if e.code == "agent.invalid_arguments"),
        "{outcome:?}"
    );
    assert_eq!(fx.sql.run_count(), 1, "only the exact-cap name got through");
}

#[test]
fn the_database_the_layer_fills_in_on_the_card_is_spelled_like_every_other_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut sql = FakeSession::new(ConnectionKind::PostgreSql);
    sql.databases = vec!["pro\u{200C}d".into()];
    let sql = sql.arc();
    let provider = FakeProvider::new();
    provider.with_session(SQL, Arc::clone(&sql));
    let catalog = FakeCatalog::new(vec![connection(
        SQL,
        ConnectionKind::PostgreSql,
        Some(AgentAccess::ReadWrite),
    )]);
    let audit =
        Arc::new(AuditLog::open(dir.path().join("audit.jsonl"), AuditConfig::default()).unwrap());
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let layer = ToolLayer::new(catalog, provider, "test")
        .with_audit(audit)
        .with_approver(approver.clone());
    let opened = layer.call("open_session", json!({"connection_id": SQL}));
    assert!(matches!(opened, ToolOutcome::Success(_)), "{opened:?}");
    let outcome = layer.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "UPDATE t SET a = 1 WHERE id = 3"}),
    );
    assert!(matches!(outcome, ToolOutcome::Success(_)), "{outcome:?}");
    assert_eq!(
        approver.databases.lock().unwrap().as_slice(),
        [Some("pro\\u{200C}d".to_owned())],
        "the same spelling the tools show the model, not a name with the character dropped"
    );
}

#[test]
fn the_carriage_return_error_says_how_to_store_one() {
    let fx = fixture(Some(AgentAccess::ReadWrite));
    fx.open_all();
    let ToolOutcome::Failure(error) = fx.call(
        "execute_query",
        json!({"connection_id": SQL, "statement": "SELECT 'a\rb'"}),
    ) else {
        panic!("a bare CR ran")
    };
    for hint in ["E'\\r'", "chr(13)", "CHAR(13)", "not a raw character"] {
        assert!(error.message.contains(hint), "{hint}: {}", error.message);
    }
}

#[test]
fn a_name_with_a_zero_width_joiner_reaches_the_model_spelled_not_altered() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    let persian = "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}";
    let heart = "heart\u{2764}\u{FE0F}";
    use engine_agent::table::{ColumnDescription, TableDescription};
    fx.sql.tables.lock().unwrap().push(TableDescription {
        name: persian.to_owned(),
        database: Some("app".into()),
        schema: Some("public".into()),
        columns: vec![ColumnDescription {
            name: heart.to_owned(),
            data_type: "text".into(),
            nullable: true,
            primary_key: false,
            default: None,
            comment: None,
        }],
        primary_key: vec![],
        unique_columns: vec![],
        indexes: vec![],
        foreign_keys: vec![],
        ddl: None,
        ddl_truncated: false,
    });
    let described = fx.ok(
        "describe_table",
        json!({"connection_id": SQL, "table": persian, "database": "app", "schema": "public"}),
    );
    assert_eq!(
        described["name"],
        "\u{645}\u{6CC}\\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}"
    );
    assert_eq!(described["columns"][0]["name"], "heart\u{2764}\\u{FE0F}");
    let context = fx.ok(
        "get_schema_context",
        json!({"connection_id": SQL, "database": "app", "schema": "public"}),
    );
    let digest = context["text"].to_string();
    assert!(
        digest.contains("200C") && digest.contains("FE0F"),
        "{digest}"
    );
}

// ------------------------------------------------------------ routines

fn routine_names(listed: &Value) -> Vec<&str> {
    listed["routines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect()
}

fn list_functions(fx: &Fixture, extra: Value) -> Value {
    let mut args = json!({"connection_id": SQL, "kind": "function", "database": "app", "schema": "public"});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    fx.ok("list_routines", args)
}

#[test]
fn routine_tools_need_schema_access_and_a_session() {
    let list = json!({"connection_id": SQL, "kind": "function", "database": "app", "schema": "public"});
    let source = json!({"connection_id": SQL, "kind": "trigger", "name": "audit_orders", "table": "orders", "database": "app", "schema": "public"});
    for setting in [None, Some(AgentAccess::Off)] {
        let fx = fixture(setting);
        for (tool, args) in [("list_routines", list.clone()), ("get_routine_source", source.clone())] {
            let outcome = fx.call(tool, args);
            assert_eq!(code(&outcome), "agent.access_denied", "{tool}: {outcome:?}");
        }
        assert!(fx.sql.routine_lookups.lock().unwrap().is_empty());
    }
    for setting in [AgentAccess::SchemaOnly, AgentAccess::Read, AgentAccess::ReadWrite] {
        let fx = fixture(Some(setting));
        fx.open_all();
        assert_eq!(code(&fx.call("list_routines", list.clone())), "ok", "{setting:?}");
        assert_eq!(code(&fx.call("get_routine_source", source.clone())), "ok", "{setting:?}");
    }
}

#[test]
fn routine_tools_refuse_redis_with_a_clear_message() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    for (tool, args) in [
        ("list_routines", json!({"connection_id": REDIS, "kind": "function"})),
        ("get_routine_source", json!({"connection_id": REDIS, "kind": "function", "name": "f"})),
    ] {
        let outcome = fx.call(tool, args);
        assert_eq!(code(&outcome), "agent.unsupported", "{tool}");
        assert!(outcome.to_text().contains("Redis"), "{}", outcome.to_text());
    }
}

#[test]
fn routine_tools_refuse_mongodb() {
    let fx = fixture(Some(AgentAccess::Read));
    let mongo = FakeSession::new(ConnectionKind::MongoDb).arc();
    fx.provider.with_session("mongo1", mongo);
    let catalog = FakeCatalog::new(vec![connection("mongo1", ConnectionKind::MongoDb, Some(AgentAccess::Read))]);
    let layer = ToolLayer::new(catalog, fx.provider.clone(), "test");
    layer.call("open_session", json!({"connection_id": "mongo1"}));
    for (tool, args) in [
        ("list_routines", json!({"connection_id": "mongo1", "kind": "trigger"})),
        ("get_routine_source", json!({"connection_id": "mongo1", "kind": "trigger", "name": "t"})),
    ] {
        let outcome = layer.call(tool, args);
        assert_eq!(code(&outcome), "agent.unsupported", "{tool}");
        assert!(outcome.to_text().contains("mongodb"), "{}", outcome.to_text());
    }
}

#[test]
fn routine_requests_are_validated() {
    let fx = fixture(Some(AgentAccess::Read));
    fx.open_all();
    let invalid = |tool: &str, args: Value| code(&fx.call(tool, args)).to_owned();
    // `kind` is required and limited to the four kinds.
    assert_eq!(invalid("list_routines", json!({"connection_id": SQL})), "agent.invalid_arguments");
    assert_eq!(
        invalid("list_routines", json!({"connection_id": SQL, "kind": "sequence"})),
        "agent.invalid_arguments"
    );
    // Neither a ref nor kind + name; or both.
    assert_eq!(invalid("get_routine_source", json!({"connection_id": SQL})), "agent.invalid_arguments");
    assert_eq!(
        invalid("get_routine_source", json!({"connection_id": SQL, "kind": "function"})),
        "agent.invalid_arguments"
    );
    let reference = json!({"kind": "function", "name": "add"});
    assert_eq!(
        invalid("get_routine_source", json!({"connection_id": SQL, "ref": reference, "name": "add"})),
        "agent.invalid_arguments"
    );
    // A ref of a kind this tool does not cover.
    assert_eq!(
        invalid("get_routine_source", json!({"connection_id": SQL, "ref": {"kind": "sequence", "name": "s"}})),
        "agent.invalid_arguments"
    );
    // Two databases and a schema level: both must be named.
    assert_eq!(
        invalid("list_routines", json!({"connection_id": SQL, "kind": "function"})),
        "agent.invalid_arguments"
    );
    assert_eq!(
        invalid("get_routine_source", json!({"connection_id": SQL, "kind": "function", "name": "add"})),
        "agent.invalid_arguments"
    );
}

#[test]
fn list_routines_filters_pages_and_keeps_overloads_apart() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    let all = list_functions(&fx, json!({}));
    assert_eq!(routine_names(&all), ["Audit_Fn", "add", "add", "refresh_totals"]);
    assert!(all.get("next_cursor").is_none());
    let overloads = all["routines"].as_array().unwrap();
    assert_eq!(overloads[1]["arguments"], "integer, integer");
    assert_eq!(overloads[2]["arguments"], "text, text");
    assert_eq!(overloads[1]["kind"], "function");
    assert_eq!(
        overloads[1]["ref"],
        json!({"database": "app", "schema": "public", "kind": "function", "name": "add",
               "arguments": "integer, integer", "identity": "oid:integer, integer"})
    );

    let first = list_functions(&fx, json!({"limit": 3}));
    assert_eq!(routine_names(&first), ["Audit_Fn", "add", "add"]);
    let cursor = first["next_cursor"].as_str().unwrap();
    let rest = list_functions(&fx, json!({"limit": 3, "cursor": cursor}));
    assert_eq!(routine_names(&rest), ["refresh_totals"]);
    assert!(rest.get("next_cursor").is_none());

    // Case-insensitive substring, like list_tables.
    assert_eq!(routine_names(&list_functions(&fx, json!({"filter": "AUDIT"}))), ["Audit_Fn"]);
    assert!(routine_names(&list_functions(&fx, json!({"filter": "nothing"}))).is_empty());

    // Each kind lists its own objects; a trigger names its table.
    let triggers = fx.ok(
        "list_routines",
        json!({"connection_id": SQL, "kind": "trigger", "database": "app", "schema": "public"}),
    );
    assert_eq!(routine_names(&triggers), ["audit_orders"]);
    assert_eq!(triggers["routines"][0]["table"], "orders");
    let procedures = fx.ok(
        "list_routines",
        json!({"connection_id": SQL, "kind": "procedure", "database": "app", "schema": "public"}),
    );
    assert_eq!(routine_names(&procedures), ["purge_old"]);
    let events = fx.ok(
        "list_routines",
        json!({"connection_id": SQL, "kind": "event", "database": "app", "schema": "public"}),
    );
    assert_eq!(events["routines"], json!([]));
}

#[test]
fn a_listed_ref_round_trips_to_its_source() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    let all = list_functions(&fx, json!({}));
    for (index, wanted) in [(1, "a integer"), (2, "a text")] {
        let item = &all["routines"][index];
        let source = fx.ok("get_routine_source", json!({"connection_id": SQL, "ref": item["ref"]}));
        assert_eq!(source["kind"], "function");
        assert_eq!(source["name"], "add");
        assert_eq!(source["truncated"], false);
        assert!(source["source"].as_str().unwrap().contains(wanted), "{source}");
    }
    // What reached the session is the very reference the listing returned.
    let lookups = fx.sql.routine_lookups.lock().unwrap();
    assert_eq!(lookups.len(), 2);
    for (lookup, index) in lookups.iter().zip([1, 2]) {
        assert_eq!(serde_json::to_value(lookup).unwrap(), all["routines"][index]["ref"]);
    }
}

#[test]
fn a_routine_can_be_named_by_kind_and_name() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    let source = fx.ok(
        "get_routine_source",
        json!({"connection_id": SQL, "kind": "function", "name": "add", "arguments": "text, text",
               "database": "app", "schema": "public"}),
    );
    assert!(source["source"].as_str().unwrap().contains("a text"), "{source}");
    let trigger = fx.ok(
        "get_routine_source",
        json!({"connection_id": SQL, "kind": "trigger", "name": "audit_orders", "table": "orders",
               "database": "app", "schema": "public"}),
    );
    assert!(trigger["source"].as_str().unwrap().starts_with("CREATE TRIGGER"), "{trigger}");
    let lookup = fx.sql.routine_lookups.lock().unwrap()[1].clone();
    assert_eq!(lookup.table.as_deref(), Some("orders"));
    assert_eq!(lookup.database.as_deref(), Some("app"));
    assert_eq!(lookup.identity, None);
}

#[test]
fn a_missing_routine_is_not_found() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    let outcome = fx.call(
        "get_routine_source",
        json!({"connection_id": SQL, "kind": "function", "name": "gone", "database": "app", "schema": "public"}),
    );
    assert_eq!(code(&outcome), "describe.not_found", "{outcome:?}");
}

#[test]
fn a_long_source_is_cut_to_the_ddl_limit_and_says_so() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    let limit = Limits::default().max_ddl_bytes;
    fx.sql.routines.lock().unwrap().push((
        routine(SchemaObjectKind::Function, "huge", Some(""), None),
        format!("CREATE FUNCTION huge() AS $$ {} $$", "x".repeat(limit * 3)),
    ));
    let source = fx.ok(
        "get_routine_source",
        json!({"connection_id": SQL, "kind": "function", "name": "huge", "arguments": "", "database": "app", "schema": "public"}),
    );
    assert_eq!(source["truncated"], true);
    let text = source["source"].as_str().unwrap();
    assert!(text.starts_with("CREATE FUNCTION huge()"), "{text}");
    assert!(text.contains("[truncated, "), "{text}");
    assert!(text.len() < limit + 100, "{} bytes", text.len());
    // A source within the limit is whole.
    let short = fx.ok(
        "get_routine_source",
        json!({"connection_id": SQL, "kind": "procedure", "name": "purge_old", "arguments": "integer", "database": "app", "schema": "public"}),
    );
    assert_eq!(short["truncated"], false);
}

#[test]
fn secrets_in_a_routine_source_are_masked() {
    let secret = "Zq7-s3cr3t-pw";
    let fx = fixture_with(
        Some(AgentAccess::SchemaOnly),
        false,
        false,
        None,
        Limits::default(),
        vec![engine_agent::SecretText::from(secret)],
    );
    fx.open_all();
    fx.sql.routines.lock().unwrap().push((
        routine(SchemaObjectKind::Function, "leaky", Some(""), None),
        format!(
            "CREATE FUNCTION leaky() AS $$ connect('postgres://admin:other-pw@db.internal/app'); \
             connect('host=x password={secret}'); select '{secret}' $$"
        ),
    ));
    let outcome = fx.call(
        "get_routine_source",
        json!({"connection_id": SQL, "kind": "function", "name": "leaky", "arguments": "", "database": "app", "schema": "public"}),
    );
    let text = outcome.to_text();
    assert!(!text.contains(secret), "{text}");
    assert!(!text.contains("other-pw"), "{text}");
    assert!(text.contains("db.internal"), "{text}");
}

#[test]
fn routine_calls_are_audited_without_the_source() {
    let fx = fixture(Some(AgentAccess::SchemaOnly));
    fx.open_all();
    list_functions(&fx, json!({}));
    fx.ok(
        "get_routine_source",
        json!({"connection_id": SQL, "kind": "trigger", "name": "audit_orders", "table": "orders", "database": "app", "schema": "public"}),
    );
    fx.call("get_routine_source", json!({"connection_id": SQL, "kind": "function", "name": "gone", "database": "app", "schema": "public"}));
    let entries = fx.audit();
    let line = |tool: &str| entries.iter().filter(|entry| entry.tool == tool).collect::<Vec<_>>();
    assert_eq!(line("list_routines").len(), 1);
    assert_eq!(line("list_routines")[0].outcome, "ok");
    let sources = line("get_routine_source");
    assert_eq!(sources.len(), 2);
    assert!(sources.iter().any(|entry| entry.outcome == "ok"));
    assert!(sources.iter().any(|entry| entry.outcome == "describe.not_found"));
    assert!(sources.iter().all(|entry| entry.connection_id.as_deref() == Some(SQL)));
    assert!(sources.iter().all(|entry| entry.statement.is_none()));
}
