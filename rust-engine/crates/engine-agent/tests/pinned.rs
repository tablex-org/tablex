//! Pinned sessions through the tool layer, against in-memory fakes: `open_session` with `pinned`,
//! `session_id` on the statement tools, what `USE` and `SET` may do in one, what no statement may
//! do in one, the caller that owns it, the idle timeout and the audit line.

use engine_agent::limits::RowCollector;
use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeProvider, FakeSession, ScriptedApprover};
use engine_agent::{
    read_entries, AgentAccess, AgentError, ApprovalDecision, AuditConfig, AuditLog, CancelToken, ConnectionKind,
    ToolLayer, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const PG: &str = "pg1";
const MY: &str = "my1";

struct Rig {
    layer: ToolLayer,
    catalog: Arc<FakeCatalog>,
    approver: Arc<ScriptedApprover>,
    /// Per connection: the shared session, then the sessions later opens get, in order.
    shared: Arc<FakeSession>,
    pinned: Vec<Arc<FakeSession>>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig_for(id: &'static str, kind: ConnectionKind, access: AgentAccess, idle: Option<Duration>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let provider = FakeProvider::new();
    let shared = FakeSession::new(kind).arc();
    provider.with_session(id, Arc::clone(&shared));
    let pinned: Vec<Arc<FakeSession>> = (0..20).map(|_| FakeSession::new(kind).arc()).collect();
    for session in &pinned {
        provider.with_extra_session(id, Arc::clone(session));
    }
    let catalog = FakeCatalog::new(vec![connection(id, kind, Some(access))]);
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let mut layer = ToolLayer::new(catalog.clone(), provider, "test")
        .with_audit(Arc::clone(&audit))
        .with_approver(approver.clone())
        .with_transactions()
        .with_pinned_sessions();
    if let Some(idle) = idle {
        layer = layer.with_session_idle_timeout(idle);
    }
    Rig {
        layer,
        catalog,
        approver,
        shared,
        pinned,
        audit,
        audit_path,
        _dir: dir,
    }
}

fn rig(id: &'static str, kind: ConnectionKind) -> Rig {
    rig_for(id, kind, AgentAccess::ReadWrite, None)
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

impl Rig {
    fn call(&self, tool: &str, args: Value) -> ToolOutcome {
        self.layer.call(tool, args)
    }

    fn call_as(&self, owner: &str, tool: &str, args: Value) -> ToolOutcome {
        self.layer.call_as(owner, tool, args, &CancelToken::new())
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        match self.call(tool, args) {
            ToolOutcome::Success(value) => value,
            ToolOutcome::Failure(error) => panic!("{tool} failed: {error:?}"),
        }
    }

    /// Opens the shared session, then a pinned one: its id.
    fn pin(&self, id: &str) -> String {
        self.ok("open_session", json!({"connection_id": id}));
        self.pin_only(id)
    }

    fn pin_only(&self, id: &str) -> String {
        self.ok("open_session", json!({"connection_id": id, "pinned": true}))["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn run(&self, id: &str, session: Option<&str>, statement: &str) -> ToolOutcome {
        let mut args = json!({"connection_id": id, "statement": statement});
        if let Some(session) = session {
            args["session_id"] = json!(session);
        }
        self.call("execute_query", args)
    }

    fn ran(session: &Arc<FakeSession>) -> Vec<String> {
        session.runs.lock().unwrap().iter().map(|(text, _)| text.clone()).collect()
    }

    fn audit(&self) -> Vec<engine_agent::AuditEntry> {
        self.audit.flush();
        read_entries(&self.audit_path, 10_000).unwrap().entries
    }
}

// ------------------------------------------------------------------ opening

#[test]
fn a_pinned_session_is_a_connection_of_its_own_with_an_unguessable_id() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.ok("open_session", json!({"connection_id": PG}));
    let first = rig.ok("open_session", json!({"connection_id": PG, "pinned": true}));
    let second = rig.ok("open_session", json!({"connection_id": PG, "pinned": true}));
    let (a, b) = (first["session_id"].as_str().unwrap(), second["session_id"].as_str().unwrap());
    assert_ne!(a, b);
    for id in [a, b] {
        // "ses-" and 128 random bits in hex.
        let hex = id.strip_prefix("ses-").expect(id);
        assert_eq!(hex.len(), 32, "{id}");
        assert!(hex.chars().all(|ch| ch.is_ascii_hexdigit()), "{id}");
    }
    assert_eq!(first["already_open"], false);
    assert_eq!(first["idle_timeout_secs"], 600);
    assert_eq!(first["kind"], "postgresql");
    // Each one has its own physical session; the shared one is not either of them.
    assert!(rig.layer.is_open(PG));
    assert_eq!(rig.shared.disconnects.load(Ordering::SeqCst), 0);
    let tx = rig.ok("begin_transaction", json!({"connection_id": PG}))["transaction_id"].as_str().unwrap().to_owned();
    assert!(tx.starts_with("tx-") && tx.len() == 3 + 32, "{tx}");
}

#[test]
fn pinned_sessions_exist_only_where_the_front_end_offers_them() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::new();
    provider.with_session(PG, FakeSession::new(ConnectionKind::PostgreSql).arc());
    let catalog = FakeCatalog::new(vec![connection(PG, ConnectionKind::PostgreSql, Some(AgentAccess::ReadWrite))]);
    let plain = ToolLayer::new(catalog, provider, "test");
    drop(dir);
    let outcome = plain.call("open_session", json!({"connection_id": PG, "pinned": true}));
    assert_eq!(code(&outcome), "agent.invalid_arguments", "{outcome:?}");
    let outcome = plain.call("execute_query", json!({"connection_id": PG, "statement": "SELECT 1", "session_id": "ses-x"}));
    assert_eq!(code(&outcome), "agent.invalid_arguments", "{outcome:?}");
    let specs = plain.tools();
    let open = specs.iter().find(|spec| spec.name == "open_session").unwrap();
    assert!(open.input_schema["properties"].get("pinned").is_none());
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let open = rig.layer.tools().iter().find(|spec| spec.name == "open_session").unwrap().clone();
    assert!(open.input_schema["properties"].get("pinned").is_some());
    assert!(open.description.contains("everything it held") || open.description.contains("drops the connection"));
}

#[test]
fn a_pinned_session_needs_access_read_and_at_most_four_are_open() {
    for access in [AgentAccess::Off, AgentAccess::SchemaOnly] {
        let rig = rig_for(PG, ConnectionKind::PostgreSql, access, None);
        let outcome = rig.call("open_session", json!({"connection_id": PG, "pinned": true}));
        assert_eq!(code(&outcome), "agent.access_denied", "{access:?}");
    }
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.ok("open_session", json!({"connection_id": PG}));
    let ids: Vec<String> = (0..4).map(|_| rig.pin_only(PG)).collect();
    let fifth = rig.call("open_session", json!({"connection_id": PG, "pinned": true}));
    assert_eq!(code(&fifth), "agent.limit", "{fifth:?}");
    // The cap is the client's own: another client still opens one.
    let other = rig.call_as("b", "open_session", json!({"connection_id": PG, "pinned": true}));
    assert_eq!(code(&other), "ok", "{other:?}");
    // Closing one makes room.
    let closed = rig.ok("close_session", json!({"session_id": ids[0]}));
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["session_id"], ids[0].as_str());
    rig.pin_only(PG);
}

// ------------------------------------------------- USE and SET: where they run

#[test]
fn set_and_use_run_on_the_pinned_connection_and_nowhere_else() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let one = rig.pin(PG);
    let two = rig.pin_only(PG);
    rig.ok("execute_query", json!({"connection_id": PG, "session_id": one, "statement": "SET search_path TO app, public"}));
    rig.ok("execute_query", json!({"connection_id": PG, "session_id": one, "statement": "SELECT 1"}));
    rig.ok("execute_query", json!({"connection_id": PG, "session_id": two, "statement": "SET TIME ZONE 'UTC'"}));
    // Each statement went to the connection of its session: the shared one and the other
    // session never saw them.
    assert_eq!(Rig::ran(&rig.pinned[0]), ["SET search_path TO app, public", "SELECT 1"]);
    assert_eq!(Rig::ran(&rig.pinned[1]), ["SET TIME ZONE 'UTC'"]);
    assert!(Rig::ran(&rig.shared).is_empty());
    // A SET is not wrapped in the READ ONLY scope (it would be undone with it); a read is.
    assert_eq!(*rig.pinned[0].read_only_runs.lock().unwrap(), [false, true]);
    // Nobody was asked: a SET changes the session and nothing else.
    assert!(rig.approver.asked.lock().unwrap().is_empty());
}

#[test]
fn without_a_session_use_and_set_stay_refused_and_the_message_says_how() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.ok("open_session", json!({"connection_id": PG}));
    for statement in ["SET search_path TO app", "SET TIME ZONE 'UTC'", "USE shop"] {
        let outcome = rig.run(PG, None, statement);
        assert_eq!(code(&outcome), "agent.unsupported", "{statement}");
        assert!(outcome.to_text().contains("pinned"), "{}", outcome.to_text());
    }
    assert!(Rig::ran(&rig.shared).is_empty());
    // The same without the feature: no hint to a feature that is not there.
    let plain_provider = FakeProvider::new();
    plain_provider.with_session(PG, FakeSession::new(ConnectionKind::PostgreSql).arc());
    let catalog = FakeCatalog::new(vec![connection(PG, ConnectionKind::PostgreSql, Some(AgentAccess::ReadWrite))]);
    let plain = ToolLayer::new(catalog, plain_provider, "test");
    plain.call("open_session", json!({"connection_id": PG}));
    let outcome = plain.call("execute_query", json!({"connection_id": PG, "statement": "SET search_path TO app"}));
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(!outcome.to_text().contains("pinned"), "{}", outcome.to_text());
}

#[test]
fn use_and_set_of_mysql_and_clickhouse_run_in_a_session() {
    for (id, kind, statements) in [
        (MY, ConnectionKind::MySql, vec!["USE shop", "SET time_zone = '+00:00'", "SET @n = 5", "SET NAMES utf8mb4", "SET SESSION lock_wait_timeout = 5"]),
        (MY, ConnectionKind::MariaDb, vec!["USE `shop`", "SET max_statement_time = 5"]),
        ("ch1", ConnectionKind::ClickHouse, vec!["USE analytics", "SET session_timezone = 'UTC'"]),
    ] {
        let rig = rig(id, kind);
        let session = rig.pin(id);
        for statement in &statements {
            let outcome = rig.run(id, Some(&session), statement);
            assert_eq!(code(&outcome), "ok", "{kind:?}: {statement}: {outcome:?}");
        }
        assert_eq!(Rig::ran(&rig.pinned[0]), statements);
        assert!(rig.pinned[0].read_only_runs.lock().unwrap().iter().all(|read_only| !read_only));
    }
}

#[test]
fn a_set_of_lock_timeout_is_the_users_own_setting_and_is_not_touched() {
    // Without `lock_timeout_secs` nothing in the layer sets or restores a lock timeout: the session
    // keeps what the model set.
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let session = rig.pin(PG);
    for statement in ["SET lock_timeout = '2s'", "SELECT 1", "SET lock_timeout = '9s'", "RESET lock_timeout"] {
        let outcome = rig.run(PG, Some(&session), statement);
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
    assert_eq!(
        Rig::ran(&rig.pinned[0]),
        ["SET lock_timeout = '2s'", "SELECT 1", "SET lock_timeout = '9s'", "RESET lock_timeout"]
    );
}

/// A session whose statements succeed and which reads its settings: `lock_timeout` as `2s`, the
/// MySQL lock waits as `(31536000, 50)`; `fail` names a statement that fails.
fn settings(session: &FakeSession, fail: Option<&'static str>) {
    session.set_behavior(Behavior::Script(Box::new(move |request, _| {
        if fail == Some(request.text) {
            return Err(AgentError::NotConnected("the connection was lost".into()));
        }
        let mut collector = RowCollector::new(Vec::new(), request.row_limit, request.limits);
        if request.text.starts_with("SELECT current_setting") {
            let _ = collector.push_row(["2s".to_owned()]);
        } else if request.text.starts_with("SELECT @@SESSION") {
            let _ = collector.push_row(["31536000".to_owned(), "50".to_owned()]);
        }
        Ok(collector.finish(false, Some(0)))
    })));
}

fn with_lock_timeout(rig: &Rig, id: &str, session: &str, statement: &str) -> ToolOutcome {
    rig.call(
        "execute_query",
        json!({"connection_id": id, "session_id": session, "statement": statement, "lock_timeout_secs": 5}),
    )
}

#[test]
fn lock_timeout_secs_runs_on_the_pinned_connection_and_leaves_the_sessions_own_setting() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    settings(&rig.pinned[0], None);
    let session = rig.pin(PG);
    // A statement a transaction block can hold: SET LOCAL ends with the block, so the `SET` the
    // model made (here read as 2s) is untouched. Nothing goes to the shared session.
    let outcome = with_lock_timeout(&rig, PG, &session, "ALTER TABLE t ADD COLUMN c text");
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    assert_eq!(
        Rig::ran(&rig.pinned[0]),
        ["BEGIN; SET LOCAL lock_timeout = '5s'", "ALTER TABLE t ADD COLUMN c text", "COMMIT"]
    );
    assert!(Rig::ran(&rig.shared).is_empty());
    rig.pinned[0].runs.lock().unwrap().clear();
    // One that cannot: the session's own value (2s) goes back, not the default.
    let outcome = with_lock_timeout(&rig, PG, &session, "CREATE INDEX CONCURRENTLY i ON t (a)");
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    assert_eq!(
        Rig::ran(&rig.pinned[0]),
        [
            "SELECT current_setting('lock_timeout')",
            "SET lock_timeout = '5s'",
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            "SET lock_timeout = '2s'",
        ]
    );
    // The session is still there.
    assert_eq!(code(&rig.run(PG, Some(&session), "SELECT 1")), "ok");
}

#[test]
fn lock_timeout_secs_puts_the_mysql_session_settings_back_in_a_pinned_session() {
    let rig = rig(MY, ConnectionKind::MySql);
    settings(&rig.pinned[0], None);
    let session = rig.pin(MY);
    let outcome = with_lock_timeout(&rig, MY, &session, "ALTER TABLE t ADD COLUMN c int");
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    assert_eq!(
        Rig::ran(&rig.pinned[0]),
        [
            "SELECT @@SESSION.lock_wait_timeout, @@SESSION.innodb_lock_wait_timeout",
            "SET SESSION lock_wait_timeout = 5, SESSION innodb_lock_wait_timeout = 5",
            "ALTER TABLE t ADD COLUMN c int",
            "SET SESSION lock_wait_timeout = 31536000, SESSION innodb_lock_wait_timeout = 50",
        ]
    );
    assert!(Rig::ran(&rig.shared).is_empty());
}

#[test]
fn a_pinned_session_whose_lock_timeout_cannot_be_taken_off_is_closed() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    settings(&rig.pinned[0], Some("SET lock_timeout = '2s'"));
    let session = rig.pin(PG);
    let outcome = with_lock_timeout(&rig, PG, &session, "VACUUM t");
    assert_eq!(code(&outcome), "agent.not_connected", "{outcome:?}");
    let ToolOutcome::Failure(error) = &outcome else { unreachable!() };
    assert!(error.message.contains("pinned session was closed"), "{}", error.message);
    // Its connection is gone, the shared session is not, and the id is no longer valid.
    assert!(rig.pinned[0].disconnects.load(Ordering::SeqCst) >= 1);
    assert_eq!(rig.shared.disconnects.load(Ordering::SeqCst), 0);
    let again = rig.run(PG, Some(&session), "SELECT 1");
    assert_eq!(code(&again), "agent.not_connected", "{again:?}");
    assert_eq!(code(&rig.run(PG, None, "SELECT 1")), "ok");
}

// ------------------------------------------------------- what never runs in one

const ESCALATIONS_POSTGRES: [&str; 11] = [
    "SET ROLE admin",
    "SET SESSION AUTHORIZATION postgres",
    "RESET ROLE",
    "SET SESSION default_transaction_read_only = off",
    "SET transaction_read_only = off",
    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
    "SET LOCAL ROLE admin",
    "RESET SESSION AUTHORIZATION",
    "SET session_replication_role = replica",
    "SET row_security = off",
    "RESET ALL",
];

const ESCALATIONS_MYSQL: [&str; 14] = [
    "SET SESSION TRANSACTION READ WRITE",
    "SET @@session.transaction_read_only = 0",
    "SET sql_log_bin = 0",
    "SET GLOBAL max_connections = 1",
    "SET PERSIST max_connections = 1",
    "SET @@global.max_connections = 1",
    "SET ROLE admin",
    "SET PASSWORD = 'x'",
    "SET autocommit = 0",
    "SET sql_mode = ''",
    "SET foreign_key_checks = 0",
    "SET time_zone = 'UTC', GLOBAL max_connections = 1",
    "SET DEFAULT ROLE admin TO u",
    "SET tx_read_only = 0",
];

#[test]
fn every_statement_that_changes_identity_privilege_or_the_read_only_mode_is_refused_at_every_access() {
    for access in [AgentAccess::Read, AgentAccess::ReadWrite] {
        for (id, kind, statements) in [
            (PG, ConnectionKind::PostgreSql, ESCALATIONS_POSTGRES.to_vec()),
            (MY, ConnectionKind::MySql, ESCALATIONS_MYSQL.to_vec()),
            (MY, ConnectionKind::MariaDb, ESCALATIONS_MYSQL.to_vec()),
            ("ch1", ConnectionKind::ClickHouse, vec!["SET readonly = 0", "SET allow_ddl = 1", "SET ROLE admin", "SET allow_introspection_functions = 1"]),
        ] {
            let rig = rig_for(id, kind, access, None);
            let session = rig.pin(id);
            for statement in &statements {
                let outcome = rig.run(id, Some(&session), statement);
                assert_eq!(code(&outcome), "agent.dangerous_refused", "{access:?} {kind:?}: {statement}: {outcome:?}");
                assert!(outcome.to_text().contains("do not retry"), "{}", outcome.to_text());
            }
            assert!(Rig::ran(&rig.pinned[0]).is_empty(), "{kind:?}: {:?}", Rig::ran(&rig.pinned[0]));
            assert!(Rig::ran(&rig.shared).is_empty());
            assert!(rig.approver.asked.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn a_setting_that_is_not_on_the_list_and_the_other_session_words_are_refused_too() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let session = rig.pin(PG);
    for statement in ["SET work_mem = '1GB'", "SET application_name = 'x'", "DISCARD ALL", "BEGIN", "COMMIT", "LISTEN x", "PREPARE p AS SELECT 1", "DO $$ begin end $$", "SAVEPOINT a"] {
        let outcome = rig.run(PG, Some(&session), statement);
        assert_eq!(code(&outcome), "agent.unsupported", "{statement}: {outcome:?}");
    }
    assert!(Rig::ran(&rig.pinned[0]).is_empty());
    // A versioned comment can hide a word from a reader that skips comments.
    let rig = rig_for(MY, ConnectionKind::MySql, AgentAccess::ReadWrite, None);
    let session = rig.pin(MY);
    let outcome = rig.run(MY, Some(&session), "SET /*!50000 GLOBAL */ max_connections = 1");
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(Rig::ran(&rig.pinned[0]).is_empty());
}

#[test]
fn a_session_statement_is_refused_in_a_script_a_transaction_and_below_read() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let session = rig.pin(PG);
    // A script keeps refusing them (run `USE` and `SET` with execute_query).
    let outcome = rig.call("execute_batch", json!({"connection_id": PG, "session_id": session, "script": "SET search_path TO a; SELECT 1"}));
    assert_eq!(code(&outcome), "agent.unsupported", "{outcome:?}");
    // A session and a transaction are not mixed.
    let tx = rig.ok("begin_transaction", json!({"connection_id": PG}))["transaction_id"].as_str().unwrap().to_owned();
    let outcome = rig.call("execute_query", json!({"connection_id": PG, "session_id": session, "transaction_id": tx, "statement": "SELECT 1"}));
    assert_eq!(code(&outcome), "agent.invalid_arguments", "{outcome:?}");
    assert!(outcome.to_text().contains("not both"), "{}", outcome.to_text());
    // Lowered below `read`: the session is closed, and nothing runs.
    rig.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::SchemaOnly));
    let outcome = rig.run(PG, Some(&session), "SET search_path TO a");
    assert_eq!(code(&outcome), "agent.access_denied", "{outcome:?}");
    rig.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    let outcome = rig.run(PG, Some(&session), "SELECT 1");
    assert!(outcome.to_text().contains("unknown session"), "{}", outcome.to_text());
    assert!(rig.pinned[0].disconnects.load(Ordering::SeqCst) >= 1);
}

// ------------------------------------------- the policy applies inside a session

#[test]
fn reads_stay_read_only_and_writes_are_approved_inside_a_session() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let session = rig.pin(PG);
    let read = rig.run(PG, Some(&session), "SELECT * FROM orders");
    assert_eq!(code(&read), "ok");
    assert_eq!(*rig.pinned[0].read_only_runs.lock().unwrap(), [true]);
    // A write is put to the user, exactly as outside a session.
    let write = rig.run(PG, Some(&session), "UPDATE orders SET paid = true WHERE id = 1");
    assert_eq!(code(&write), "ok", "{write:?}");
    assert_eq!(rig.approver.asked.lock().unwrap().len(), 1);
    // Dangerous needs the flag; a declined question runs nothing.
    let drop = rig.run(PG, Some(&session), "DROP TABLE orders");
    assert_eq!(code(&drop), "agent.dangerous_refused");
    let ran = Rig::ran(&rig.pinned[0]);
    assert_eq!(ran.len(), 2, "{ran:?}");
    // At `read` a write is refused and a read still runs.
    rig.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::Read));
    let write = rig.run(PG, Some(&session), "DELETE FROM orders WHERE id = 1");
    assert_eq!(code(&write), "agent.write_refused");
    assert_eq!(code(&rig.run(PG, Some(&session), "SELECT 1")), "ok");
    assert_eq!(code(&rig.run(PG, Some(&session), "SET search_path TO a")), "ok");
}

#[test]
fn every_statement_tool_takes_a_session_id_and_runs_on_that_connection() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let session = rig.pin(PG);
    rig.ok("explain_query", json!({"connection_id": PG, "session_id": session, "statement": "SELECT 1"}));
    rig.ok("execute_batch", json!({"connection_id": PG, "session_id": session, "script": "SELECT 1; SELECT 2"}));
    let ran = Rig::ran(&rig.pinned[0]);
    assert_eq!(ran, ["EXPLAIN SELECT 1", "SELECT 1", "SELECT 2"]);
    assert!(Rig::ran(&rig.shared).is_empty());
    // A statement of another connection's session, or of a session id that does not exist.
    let outcome = rig.run(PG, Some("ses-00000000000000000000000000000000"), "SELECT 1");
    assert_eq!(code(&outcome), "agent.not_connected");
    assert!(outcome.to_text().contains("unknown session"), "{}", outcome.to_text());
}

// ------------------------------------------------------ who may use one, and when

#[test]
fn another_client_cannot_use_see_or_close_a_session_or_a_transaction() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.layer.call_as("a", "open_session", json!({"connection_id": PG}), &CancelToken::new());
    let session = rig.call_as("a", "open_session", json!({"connection_id": PG, "pinned": true}));
    let ToolOutcome::Success(session) = session else { panic!("{session:?}") };
    let session = session["session_id"].as_str().unwrap().to_owned();
    // Client A uses it.
    let used = rig.call_as("a", "execute_query", json!({"connection_id": PG, "session_id": session, "statement": "SET search_path TO a"}));
    assert_eq!(code(&used), "ok", "{used:?}");
    // Client B is told it does not exist, whatever it tries.
    for (tool, args) in [
        ("execute_query", json!({"connection_id": PG, "session_id": session, "statement": "SELECT 1"})),
        ("execute_batch", json!({"connection_id": PG, "session_id": session, "script": "SELECT 1"})),
        ("explain_query", json!({"connection_id": PG, "session_id": session, "statement": "SELECT 1"})),
        ("close_session", json!({"session_id": session})),
    ] {
        let outcome = rig.call_as("b", tool, args);
        assert_eq!(code(&outcome), "agent.not_connected", "{tool}: {outcome:?}");
        assert!(outcome.to_text().contains("unknown session"), "{tool}: {}", outcome.to_text());
    }
    // It is the same answer as for an id nobody has.
    let nobody = rig.call_as("b", "close_session", json!({"session_id": "ses-00000000000000000000000000000000"}));
    assert_eq!(nobody.to_text().replace("00000000000000000000000000000000", "X"), rig.call_as("b", "close_session", json!({"session_id": session})).to_text().replace(&session, "ses-X"));
    // B's own closing of the connection's shared session does not end A's pinned session.
    rig.call_as("b", "close_session", json!({"connection_id": PG}));
    let still = rig.call_as("a", "execute_query", json!({"connection_id": PG, "session_id": session, "statement": "SELECT 1"}));
    assert_eq!(code(&still), "ok", "{still:?}");
    // A closes it.
    let closed = rig.call_as("a", "close_session", json!({"session_id": session}));
    assert_eq!(code(&closed), "ok");
}

#[test]
fn another_client_cannot_use_commit_or_roll_back_a_transaction() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.call_as("a", "open_session", json!({"connection_id": PG}));
    let began = rig.call_as("a", "begin_transaction", json!({"connection_id": PG}));
    let ToolOutcome::Success(began) = began else { panic!("{began:?}") };
    let tx = began["transaction_id"].as_str().unwrap().to_owned();
    let ran = rig.call_as("a", "execute_query", json!({"connection_id": PG, "transaction_id": tx, "statement": "INSERT INTO a VALUES (1)"}));
    assert_eq!(code(&ran), "ok", "{ran:?}");
    for (tool, args) in [
        ("execute_query", json!({"connection_id": PG, "transaction_id": tx, "statement": "SELECT 1"})),
        ("execute_batch", json!({"connection_id": PG, "transaction_id": tx, "script": "SELECT 1"})),
        ("commit_transaction", json!({"transaction_id": tx})),
        ("rollback_transaction", json!({"transaction_id": tx})),
    ] {
        let outcome = rig.call_as("b", tool, args);
        assert_eq!(code(&outcome), "agent.transaction_gone", "{tool}: {outcome:?}");
        assert!(outcome.to_text().contains("no open transaction"), "{tool}: {}", outcome.to_text());
    }
    // B cannot begin a second one on the connection while A holds one, and A's is untouched.
    let busy = rig.call_as("b", "begin_transaction", json!({"connection_id": PG}));
    assert_eq!(code(&busy), "agent.limit");
    let committed = rig.call_as("a", "commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&committed), "ok", "{committed:?}");
    // The id of a pinned session is not a transaction, and the other way round.
    let session = rig.ok("open_session", json!({"connection_id": PG, "pinned": true}))["session_id"].as_str().unwrap().to_owned();
    assert_eq!(code(&rig.call("commit_transaction", json!({"transaction_id": session}))), "agent.transaction_gone");
    let tx2 = rig.ok("begin_transaction", json!({"connection_id": PG}))["transaction_id"].as_str().unwrap().to_owned();
    let outcome = rig.run(PG, Some(&tx2), "SELECT 1");
    assert!(outcome.to_text().contains("unknown session"), "{}", outcome.to_text());
}

#[test]
fn a_session_nobody_calls_for_the_idle_timeout_is_closed_and_its_connection_dropped() {
    let rig = rig_for(PG, ConnectionKind::PostgreSql, AgentAccess::ReadWrite, Some(Duration::from_millis(60)));
    let session = rig.pin(PG);
    assert_eq!(code(&rig.run(PG, Some(&session), "SET search_path TO a")), "ok");
    assert_eq!(rig.layer.expire_idle_transactions(), 0, "just used");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.layer.expire_idle_transactions(), 1);
    assert!(rig.pinned[0].disconnects.load(Ordering::SeqCst) >= 1, "the connection is dropped");
    let outcome = rig.run(PG, Some(&session), "SELECT 1");
    assert_eq!(code(&outcome), "agent.not_connected");
    assert!(outcome.to_text().contains("unknown session") && outcome.to_text().contains("without a call"), "{}", outcome.to_text());
    // A new session starts clean: it is another connection.
    let fresh = rig.pin_only(PG);
    assert_ne!(fresh, session);
    assert!(Rig::ran(&rig.pinned[1]).is_empty());
    // The idle check also runs at the start of every call.
    std::thread::sleep(Duration::from_millis(150));
    let outcome = rig.run(PG, Some(&fresh), "SELECT 1");
    assert!(outcome.to_text().contains("unknown session"), "{}", outcome.to_text());
}

#[test]
fn closing_a_session_drops_its_connection_and_the_shared_one_stays() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let session = rig.pin(PG);
    rig.ok("execute_query", json!({"connection_id": PG, "session_id": session, "statement": "SET search_path TO a"}));
    let closed = rig.ok("close_session", json!({"session_id": session}));
    assert_eq!(closed["closed"], true);
    assert!(rig.pinned[0].disconnects.load(Ordering::SeqCst) >= 1);
    assert_eq!(rig.shared.disconnects.load(Ordering::SeqCst), 0);
    // Gone for good.
    let outcome = rig.run(PG, Some(&session), "SELECT 1");
    assert!(outcome.to_text().contains("unknown session"), "{}", outcome.to_text());
    // `close_session` takes a connection or a session, not both and not neither.
    for args in [json!({}), json!({"connection_id": PG, "session_id": session})] {
        assert_eq!(code(&rig.call("close_session", args)), "agent.invalid_arguments");
    }
    // Closing the connection's session ends the caller's pinned sessions with it.
    let another = rig.pin_only(PG);
    rig.ok("close_session", json!({"connection_id": PG}));
    assert!(rig.run(PG, Some(&another), "SELECT 1").to_text().contains("unknown session"));
}

#[test]
fn an_edited_or_removed_connection_ends_its_sessions() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.catalog.update(PG, |info| info.revision = engine_agent::Revision::new(1));
    let session = rig.pin(PG);
    assert_eq!(code(&rig.run(PG, Some(&session), "SELECT 1")), "ok");
    rig.catalog.update(PG, |info| info.revision = engine_agent::Revision::new(99));
    let outcome = rig.run(PG, Some(&session), "SELECT 1");
    assert_eq!(code(&outcome), "agent.not_connected", "{outcome:?}");
    assert!(outcome.to_text().contains("edited"), "{}", outcome.to_text());
    assert!(rig.pinned[0].disconnects.load(Ordering::SeqCst) >= 1);
}

// ---------------------------------------------------------------------- the audit

#[test]
fn the_audit_log_records_the_session_and_every_session_statement() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    let session = rig.pin(PG);
    rig.ok("execute_query", json!({"connection_id": PG, "session_id": session, "statement": "SET search_path TO a"}));
    rig.ok("execute_query", json!({"connection_id": PG, "session_id": session, "statement": "SELECT 1"}));
    let refused = rig.run(PG, Some(&session), "SET ROLE admin");
    assert_eq!(code(&refused), "agent.dangerous_refused");
    rig.ok("close_session", json!({"session_id": session}));
    let entries = rig.audit();
    let in_session: Vec<(&str, &str)> = entries
        .iter()
        .filter(|entry| entry.session_id.as_deref() == Some(session.as_str()))
        .map(|entry| (entry.statement.as_deref().unwrap_or(""), entry.outcome.as_str()))
        .collect();
    assert!(in_session.contains(&("SET search_path TO a", "ok")), "{in_session:?}");
    assert!(in_session.contains(&("SELECT 1", "ok")), "{in_session:?}");
    assert!(in_session.contains(&("SET ROLE admin", "agent.dangerous_refused")), "{in_session:?}");
    // The open and the close are in it too, and a call outside a session has none.
    assert!(entries.iter().any(|entry| entry.tool == "open_session" && entry.session_id.as_deref() == Some(session.as_str())));
    assert!(entries.iter().any(|entry| entry.tool == "close_session" && entry.session_id.as_deref() == Some(session.as_str())));
    assert!(entries.iter().filter(|entry| entry.tool == "list_connections").all(|entry| entry.session_id.is_none()));
}

// --------------------------------------------- the databases of the connection

#[test]
fn use_stays_inside_the_databases_the_user_allowed() {
    let rig = rig(MY, ConnectionKind::MySql);
    rig.catalog.update(MY, |info| info.allowed_databases = Some(vec!["shop".into(), "reports".into()]));
    let session = rig.pin(MY);
    let ok = |statement: &str| {
        rig.call("execute_query", json!({"connection_id": MY, "session_id": session, "database": "shop", "statement": statement}))
    };
    assert_eq!(code(&ok("USE reports")), "ok");
    let outside = ok("USE secrets");
    assert_eq!(code(&outside), "agent.access_denied", "{outside:?}");
    assert!(outside.to_text().contains("secrets") && outside.to_text().contains("shop"), "{}", outside.to_text());
    assert_eq!(Rig::ran(&rig.pinned[0]), ["USE reports"]);
    // The database argument too: outside is refused, none is asked for.
    let outside = rig.call("execute_query", json!({"connection_id": MY, "session_id": session, "database": "secrets", "statement": "SELECT 1"}));
    assert_eq!(code(&outside), "agent.access_denied");
    let none = rig.call("execute_query", json!({"connection_id": MY, "session_id": session, "statement": "SELECT 1"}));
    assert_eq!(code(&none), "agent.access_denied");
    assert!(none.to_text().contains("`database`"), "{}", none.to_text());
}

// ------------------------------------------------- what a pinned SET may say, and how it is read

#[test]
fn a_pinned_mysql_set_runs_a_literal_and_never_an_expression() {
    for kind in [ConnectionKind::MySql, ConnectionKind::MariaDb] {
        for access in [AgentAccess::Read, AgentAccess::ReadWrite] {
            let rig = rig_for(MY, kind, access, None);
            let session = rig.pin(MY);
            // A stored function that writes, a subquery, an operator: none runs, nobody is asked,
            // and no audit line says a write started.
            for statement in [
                "SET @n = app.next_invoice_no()",
                "SET @n = next_invoice_no()",
                "SET @n = (SELECT 1)",
                "SET @n = 1 + 1",
                "SET time_zone = (SELECT 'UTC')",
            ] {
                let outcome = rig.run(MY, Some(&session), statement);
                assert_eq!(code(&outcome), "agent.unsupported", "{kind:?} {access:?}: {statement}: {outcome:?}");
                assert!(outcome.to_text().contains("literal"), "{}", outcome.to_text());
            }
            assert!(Rig::ran(&rig.pinned[0]).is_empty(), "{:?}", Rig::ran(&rig.pinned[0]));
            assert!(rig.approver.asked.lock().unwrap().is_empty());
            for statement in ["SET @n = 5", "SET @n = 'x', @m = -1.5, @k = @n, time_zone = DEFAULT"] {
                assert_eq!(code(&rig.run(MY, Some(&session), statement)), "ok", "{statement}");
            }
        }
    }
}

#[test]
fn the_attack_strings_of_the_review_are_refused_in_a_pinned_session() {
    let mut rig_pg = rig(PG, ConnectionKind::PostgreSql);
    let session = rig_pg.pin(PG);
    // PostgreSQL nests block comments: this is `SET ROLE admin` for the server.
    for statement in [
        "SET /* /* */ search_path TO x /* */ */ ROLE admin",
        "SET /* /* */ search_path TO x /* */ */ SESSION AUTHORIZATION postgres",
        "SET /* /* */ search_path TO x /* */ */ default_transaction_read_only = off",
        "SET /* /* */ search_path TO x /* */ */ session_replication_role = replica",
        "SET /* /* */ search_path TO x /* */ */ row_security = off",
        "SET search_path TO a; SET ROLE admin",
    ] {
        let outcome = rig_pg.run(PG, Some(&session), statement);
        assert!(matches!(code(&outcome), "agent.dangerous_refused" | "agent.unsupported" | "agent.multi_statement"), "{statement}: {outcome:?}");
    }
    assert!(Rig::ran(&rig_pg.pinned[0]).is_empty(), "{:?}", Rig::ran(&rig_pg.pinned[0]));
    rig_pg = rig(PG, ConnectionKind::PostgreSql);
    let session = rig_pg.pin(PG);
    // `set_config` through a SELECT is still the blocked function it was.
    let outcome = rig_pg.run(PG, Some(&session), "SELECT set_config('role', 'admin', false)");
    assert_eq!(code(&outcome), "agent.blocked_function", "{outcome:?}");

    for kind in [ConnectionKind::MySql, ConnectionKind::MariaDb] {
        let rig = rig(MY, kind);
        let session = rig.pin(MY);
        for (statement, expected) in [
            ("SET NAMES utf8mb4, foreign_key_checks = 0", "agent.dangerous_refused"),
            ("SET NAMES utf8mb4, GLOBAL read_only = 0", "agent.dangerous_refused"),
            ("SET NAMES utf8mb4, sql_log_bin = 0", "agent.dangerous_refused"),
            ("SET CHARACTER SET utf8mb4, foreign_key_checks = 0", "agent.dangerous_refused"),
            ("SET @a = 1--1, GLOBAL max_connections = 1", "agent.dangerous_refused"),
            (r#"SET @x = "\" , @y = ", foreign_key_checks = 0, @z = "x""#, "agent.dangerous_refused"),
            ("SET NAMES gbk", "agent.unsupported"),
            ("SET CHARACTER SET sjis", "agent.unsupported"),
            ("SET character_set_client = gbk", "agent.unsupported"),
            ("SET character_set_connection = big5", "agent.unsupported"),
        ] {
            let outcome = rig.run(MY, Some(&session), statement);
            assert_eq!(code(&outcome), expected, "{kind:?}: {statement}: {outcome:?}");
        }
        assert!(Rig::ran(&rig.pinned[0]).is_empty(), "{kind:?}: {:?}", Rig::ran(&rig.pinned[0]));
        for statement in ["SET NAMES utf8mb4", "SET NAMES latin1 COLLATE latin1_swedish_ci", "SET character_set_client = utf8"] {
            assert_eq!(code(&rig.run(MY, Some(&session), statement)), "ok", "{statement}");
        }
    }
}

#[test]
fn use_is_judged_by_the_name_as_written() {
    let rig = rig(MY, ConnectionKind::MySql);
    rig.catalog.update(MY, |info| info.allowed_databases = Some(vec!["shop".into()]));
    let session = rig.pin(MY);
    let run = |statement: &str| {
        rig.call("execute_query", json!({"connection_id": MY, "session_id": session, "database": "shop", "statement": statement}))
    };
    // On a server that tells `Shop` from `shop`, this is another database.
    assert_eq!(code(&run("USE Shop")), "agent.access_denied");
    assert_eq!(code(&run("USE SHOP")), "agent.access_denied");
    assert_eq!(code(&run("USE shop")), "ok");
    assert_eq!(Rig::ran(&rig.pinned[0]), ["USE shop"]);
}

#[test]
fn the_approval_card_of_a_pinned_session_names_the_database_of_its_use() {
    let rig = rig(MY, ConnectionKind::MySql);
    let session = rig.pin(MY);
    let write = |statement: &str| {
        rig.call("execute_query", json!({"connection_id": MY, "session_id": session, "statement": statement}))
    };
    assert_eq!(code(&write("USE other")), "ok");
    assert_eq!(code(&write("UPDATE t SET a = 1 WHERE id = 1")), "ok");
    assert_eq!(rig.approver.databases.lock().unwrap().last().cloned().flatten().as_deref(), Some("other"));
    // A later USE moves it; a statement that names its database keeps that one on the card.
    assert_eq!(code(&write("USE `third`")), "ok");
    assert_eq!(code(&write("UPDATE t SET a = 2 WHERE id = 1")), "ok");
    assert_eq!(rig.approver.databases.lock().unwrap().last().cloned().flatten().as_deref(), Some("third"));
    let named = rig.call("execute_query", json!({"connection_id": MY, "session_id": session, "database": "named", "statement": "UPDATE t SET a = 3 WHERE id = 1"}));
    assert_eq!(code(&named), "ok");
    assert_eq!(rig.approver.databases.lock().unwrap().last().cloned().flatten().as_deref(), Some("named"));
}

// ------------------------------------------------------------- caps across clients

#[test]
fn pinned_sessions_and_transactions_share_a_cap_across_every_client() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.ok("open_session", json!({"connection_id": PG}));
    // Four clients hold four pinned sessions each: sixteen.
    for owner in ["a", "b", "c", "d"] {
        for _ in 0..4 {
            let opened = rig.call_as(owner, "open_session", json!({"connection_id": PG, "pinned": true}));
            assert_eq!(code(&opened), "ok", "{owner}: {opened:?}");
        }
    }
    // A fifth client, under its own cap of four, is refused: the room is all taken.
    let opened = rig.call_as("e", "open_session", json!({"connection_id": PG, "pinned": true}));
    assert_eq!(code(&opened), "agent.limit", "{opened:?}");
    assert!(opened.to_text().contains("16"), "{}", opened.to_text());
    let begun = rig.call_as("e", "begin_transaction", json!({"connection_id": PG}));
    assert_eq!(code(&begun), "agent.limit", "{begun:?}");
    // One that ends frees room for it.
    rig.layer.end_client("a");
    assert_eq!(code(&rig.call_as("e", "open_session", json!({"connection_id": PG, "pinned": true}))), "ok");
}

#[test]
fn a_client_that_is_gone_takes_its_pinned_sessions_with_it() {
    let rig = rig(PG, ConnectionKind::PostgreSql);
    rig.ok("open_session", json!({"connection_id": PG}));
    let mine = rig.call_as("a", "open_session", json!({"connection_id": PG, "pinned": true}));
    let id = mine.to_json()["session_id"].as_str().unwrap().to_owned();
    assert_eq!(code(&rig.call_as("b", "open_session", json!({"connection_id": PG, "pinned": true}))), "ok");
    rig.layer.end_client("a");
    assert!(rig.pinned[0].disconnects.load(Ordering::SeqCst) >= 1);
    assert_eq!(rig.pinned[1].disconnects.load(Ordering::SeqCst), 0);
    let used = rig.call_as("a", "execute_query", json!({"connection_id": PG, "session_id": id, "statement": "SELECT 1"}));
    assert_eq!(code(&used), "agent.not_connected", "{used:?}");
}
