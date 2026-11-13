//! Explicit transactions through the tool layer, against in-memory fakes: gating, routing of
//! statements to the transaction's own session, the commit question, and every way a transaction
//! must end in a rollback.

use engine_agent::testing::{
    connection, Behavior, FakeCatalog, FakeProvider, FakeSession, ScriptedApprover,
};
use engine_agent::{
    read_entries, AgentAccess, AgentError, ApprovalDecision, AuditConfig, AuditLog, CancelToken,
    ConnectionKind, ToolLayer, ToolOutcome, TxControl, WriteApproval, WriteApprover,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PG: &str = "pg1";
const LITE: &str = "lite1";

/// A connection's shared session and the session its transaction gets.
struct Sessions {
    shared: Arc<FakeSession>,
    own: Arc<FakeSession>,
}

struct Fixture {
    layer: Arc<ToolLayer>,
    catalog: Arc<FakeCatalog>,
    provider: Arc<FakeProvider>,
    approver: Arc<ScriptedApprover>,
    sessions: HashMap<String, Sessions>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

struct Options {
    connections: Vec<(&'static str, ConnectionKind)>,
    access: AgentAccess,
    decision: ApprovalDecision,
    idle: Option<Duration>,
    transactions: bool,
    approver: Option<Arc<dyn WriteApprover>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            connections: vec![(PG, ConnectionKind::PostgreSql)],
            access: AgentAccess::ReadWrite,
            decision: ApprovalDecision::Approve,
            idle: None,
            transactions: true,
            approver: None,
        }
    }
}

fn fixture(options: Options) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let provider = FakeProvider::new();
    let mut sessions = HashMap::new();
    let mut infos = Vec::new();
    for (id, kind) in &options.connections {
        let shared = FakeSession::new(*kind).arc();
        let own = FakeSession::new(*kind).arc();
        provider.with_session(id, Arc::clone(&shared));
        provider.with_extra_session(id, Arc::clone(&own));
        sessions.insert((*id).to_owned(), Sessions { shared, own });
        infos.push(connection(id, *kind, Some(options.access)));
    }
    let catalog = FakeCatalog::new(infos);
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let approver = ScriptedApprover::new(options.decision, false);
    let mut layer = ToolLayer::new(catalog.clone(), provider.clone(), "test")
        .with_audit(Arc::clone(&audit));
    layer = match options.approver {
        Some(custom) => layer.with_approver(custom),
        None => layer.with_approver(approver.clone()),
    };
    if options.transactions {
        layer = layer.with_transactions();
    }
    if let Some(idle) = options.idle {
        layer = layer.with_transaction_idle_timeout(idle);
    }
    Fixture {
        layer: Arc::new(layer),
        catalog,
        provider,
        approver,
        sessions,
        audit,
        audit_path,
        _dir: dir,
    }
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

    /// Opens the connection's shared session (the first open of a connection gets the main fake
    /// session) and begins a transaction (which gets the connection's extra one).
    fn begin(&self, id: &str) -> String {
        self.ok("open_session", json!({"connection_id": id}));
        self.begin_only(id)
    }

    fn begin_only(&self, id: &str) -> String {
        self.ok("begin_transaction", json!({"connection_id": id}))["transaction_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn run_in(&self, connection: &str, tx: &str, statement: &str) -> ToolOutcome {
        self.call(
            "execute_query",
            json!({"connection_id": connection, "transaction_id": tx, "statement": statement}),
        )
    }

    fn own(&self, id: &str) -> &Arc<FakeSession> {
        &self.sessions[id].own
    }

    fn shared(&self, id: &str) -> &Arc<FakeSession> {
        &self.sessions[id].shared
    }

    fn audit(&self) -> Vec<engine_agent::AuditEntry> {
        self.audit.flush();
        read_entries(&self.audit_path, 10_000).unwrap().entries
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

// ------------------------------------------------------------------ off by default

#[test]
fn the_tools_do_not_exist_unless_the_front_end_turns_them_on() {
    let off = fixture(Options {
        transactions: false,
        ..Options::default()
    });
    let names: Vec<&str> = off.layer.tools().iter().map(|tool| tool.name).collect();
    assert_eq!(names.len(), 12);
    for tool in ["begin_transaction", "commit_transaction", "rollback_transaction"] {
        assert!(!names.contains(&tool), "{tool}");
        assert_eq!(
            code(&off.call(tool, json!({"connection_id": PG, "transaction_id": "tx-1"}))),
            "agent.unknown_tool"
        );
    }
    let execute = off
        .layer
        .tools()
        .iter()
        .find(|tool| tool.name == "execute_query")
        .unwrap();
    assert!(execute.input_schema["properties"].get("transaction_id").is_none());
    // The argument is refused by the schema, never reaches a transaction lookup.
    off.ok("open_session", json!({"connection_id": PG}));
    assert_eq!(
        code(&off.run_in(PG, "tx-1", "SELECT 1")),
        "agent.invalid_arguments"
    );

    let on = fixture(Options::default());
    let names: Vec<&str> = on.layer.tools().iter().map(|tool| tool.name).collect();
    assert_eq!(names.len(), 15);
    for tool in ["begin_transaction", "commit_transaction", "rollback_transaction"] {
        assert!(names.contains(&tool), "{tool}");
    }
    let execute = on
        .layer
        .tools()
        .iter()
        .find(|tool| tool.name == "execute_query")
        .unwrap();
    assert!(execute.input_schema["properties"].get("transaction_id").is_some());
}

// ------------------------------------------------------------------ begin

#[test]
fn begin_opens_a_session_of_its_own_and_sends_begin_on_it() {
    let fx = fixture(Options::default());
    fx.ok("open_session", json!({"connection_id": PG}));
    let result = fx.ok("begin_transaction", json!({"connection_id": PG}));
    assert_eq!(result["connection_id"], PG);
    assert_eq!(result["idle_timeout_secs"], 300);
    assert!(result["transaction_id"].as_str().unwrap().starts_with("tx-"));
    assert_eq!(fx.provider.opened.load(Ordering::SeqCst), 2);
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin]);
    assert!(fx.shared(PG).control_calls().is_empty());
    // The model-facing shape validates against the declared schema.
    let spec = engine_agent::schema::spec(engine_agent::ToolName::BeginTransaction);
    engine_agent::schema::validate(&spec.output_schema, &result).unwrap();
}

#[test]
fn begin_needs_read_write_and_an_approver_that_would_take_writes() {
    let read = fixture(Options {
        access: AgentAccess::Read,
        ..Options::default()
    });
    assert_eq!(
        code(&read.call("begin_transaction", json!({"connection_id": PG}))),
        "agent.access_denied"
    );
    assert_eq!(read.provider.opened.load(Ordering::SeqCst), 0);

    // An approver that refuses without asking (switch off, production, no elicitation).
    let refusing = fixture(Options {
        decision: ApprovalDecision::Refuse {
            reason: "changes through MCP are turned off".into(),
        },
        ..Options::default()
    });
    let outcome = refusing.call("begin_transaction", json!({"connection_id": PG}));
    assert_eq!(code(&outcome), "agent.write_refused");
    assert!(message(&outcome).contains("changes through MCP are turned off"));
    assert_eq!(refusing.provider.opened.load(Ordering::SeqCst), 0, "no second session was opened");
    assert!(refusing
        .audit()
        .iter()
        .any(|entry| entry.tool == "begin_transaction"
            && entry.approval.as_deref().is_some_and(|a| a.starts_with("refused"))));

    // The default approver refuses every write: so it refuses a transaction too.
    let default = fixture(Options {
        approver: Some(Arc::new(engine_agent::RefuseWrites)),
        ..Options::default()
    });
    assert_eq!(
        code(&default.call("begin_transaction", json!({"connection_id": PG}))),
        "agent.write_refused"
    );
}

#[test]
fn only_postgresql_and_sqlite_connections_without_a_read_only_type_can_begin() {
    let fx = fixture(Options {
        connections: vec![
            (PG, ConnectionKind::PostgreSql),
            (LITE, ConnectionKind::Sqlite),
            ("my", ConnectionKind::MySql),
            ("ch", ConnectionKind::ClickHouse),
            ("rd", ConnectionKind::Redis),
        ],
        ..Options::default()
    });
    for id in ["my", "ch", "rd"] {
        let outcome = fx.call("begin_transaction", json!({"connection_id": id}));
        assert_eq!(code(&outcome), "agent.unsupported", "{id}");
        assert!(message(&outcome).contains("available on PostgreSQL and SQLite"), "{id}");
        assert!(fx.own(id).control_calls().is_empty());
    }
    // A read-only type refuses BEGIN: unsupported (its access is capped at `read` anyway).
    fx.catalog.update(PG, |info| info.connection_type.read_only = true);
    let outcome = fx.call("begin_transaction", json!({"connection_id": PG}));
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(message(&outcome).contains("read-only"));
    fx.catalog.update(PG, |info| info.connection_type.read_only = false);
    // A SQLite database that exists for one session only.
    *fx.provider.transactions_unavailable.lock().unwrap() =
        Some("an in-memory database is not shared with a second session".into());
    let outcome = fx.call("begin_transaction", json!({"connection_id": LITE}));
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(message(&outcome).contains("in-memory"));
    *fx.provider.transactions_unavailable.lock().unwrap() = None;
    fx.begin(PG);
    fx.begin(LITE);
    // A connection with access `off` is not described at all.
    fx.catalog.update("my", |info| info.agent_access = Some(AgentAccess::Off));
    assert_eq!(
        code(&fx.call("begin_transaction", json!({"connection_id": "my"}))),
        "agent.access_denied"
    );
}

#[test]
fn one_transaction_per_connection_and_four_in_all() {
    let ids = ["c1", "c2", "c3", "c4", "c5"];
    let fx = fixture(Options {
        connections: ids.iter().map(|id| (*id, ConnectionKind::PostgreSql)).collect(),
        ..Options::default()
    });
    let first = fx.begin("c1");
    let again = fx.call("begin_transaction", json!({"connection_id": "c1"}));
    assert_eq!(code(&again), "agent.limit");
    assert!(message(&again).contains(&first));
    for id in &ids[1..4] {
        fx.begin(id);
    }
    let fifth = fx.call("begin_transaction", json!({"connection_id": "c5"}));
    assert_eq!(code(&fifth), "agent.limit");
    assert!(message(&fifth).contains("4 transactions"));
    // The refused begin opened nothing.
    assert!(fx.own("c5").control_calls().is_empty());
    // A slot is free again after a rollback.
    fx.ok("rollback_transaction", json!({"transaction_id": first}));
    fx.begin("c5");
}

// ------------------------------------------------------------------ statements

#[test]
fn statements_run_on_the_transactions_session_reads_stay_read_only() {
    let fx = fixture(Options::default());
    fx.ok("open_session", json!({"connection_id": PG}));
    let tx = fx.begin(PG);
    let read = fx.run_in(PG, &tx, "SELECT 1");
    assert!(!read.is_error(), "{read:?}");
    // The shared session never saw it; the transaction's session ran it read-only.
    assert_eq!(fx.shared(PG).run_count(), 0);
    assert_eq!(fx.own(PG).run_count(), 1);
    assert_eq!(*fx.own(PG).read_only_runs.lock().unwrap(), [true]);
    match read {
        ToolOutcome::Success(value) => assert_eq!(value["read_only_enforced"], true),
        ToolOutcome::Failure(_) => unreachable!(),
    }
    // Without the id a statement still goes to the shared session, in autocommit.
    fx.ok(
        "execute_query",
        json!({"connection_id": PG, "statement": "SELECT 2"}),
    );
    assert_eq!(fx.shared(PG).run_count(), 1);
    assert_eq!(fx.own(PG).run_count(), 1);
}

#[test]
fn a_write_in_a_transaction_is_approved_per_statement_and_says_so() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    let outcome = fx.run_in(PG, &tx, "INSERT INTO t VALUES (1)");
    assert!(!outcome.is_error(), "{outcome:?}");
    let asked = fx.approver.transactions.lock().unwrap().clone();
    assert_eq!(asked, [Some((tx.clone(), None))]);
    assert_eq!(
        *fx.approver.asked.lock().unwrap(),
        [("INSERT INTO t VALUES (1)".to_owned(), None)]
    );
    assert_eq!(fx.own(PG).run_count(), 1);
    assert_eq!(*fx.own(PG).read_only_runs.lock().unwrap(), [false]);
    assert_eq!(fx.shared(PG).run_count(), 0);
    // The audit line of the write says it is inside a transaction and not committed.
    let started = fx
        .audit()
        .into_iter()
        .find(|entry| entry.outcome == "started")
        .unwrap();
    assert!(started.approval.unwrap().contains(&tx));
    // Session-state statements are still refused, in a transaction too.
    for statement in ["COMMIT", "BEGIN", "SET x = 1", "ROLLBACK", "SAVEPOINT a"] {
        assert_eq!(
            code(&fx.run_in(PG, &tx, statement)),
            "agent.unsupported",
            "{statement}"
        );
    }
    assert_eq!(fx.own(PG).run_count(), 1);
}

#[test]
fn a_declined_write_is_not_recorded_and_not_run() {
    let fx = fixture(Options {
        decision: ApprovalDecision::Deny { reason: None },
        ..Options::default()
    });
    let tx = fx.begin(PG);
    assert_eq!(
        code(&fx.run_in(PG, &tx, "DELETE FROM t WHERE id = 1")),
        "agent.write_refused"
    );
    assert_eq!(fx.own(PG).run_count(), 0);
    // Nothing to commit: commit asks no one.
    let before = fx.approver.asked.lock().unwrap().len();
    let committed = fx.ok("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(committed["changes"], 0);
    assert_eq!(fx.approver.asked.lock().unwrap().len(), before);
}

#[test]
fn an_unknown_expired_or_foreign_transaction_id_is_refused() {
    let fx = fixture(Options {
        connections: vec![(PG, ConnectionKind::PostgreSql), (LITE, ConnectionKind::Sqlite)],
        ..Options::default()
    });
    let missing = fx.run_in(PG, "tx-99", "SELECT 1");
    assert_eq!(code(&missing), "agent.transaction_gone");
    assert!(message(&missing).contains("tx-99"));
    assert_eq!(
        code(&fx.call("commit_transaction", json!({"transaction_id": "tx-99"}))),
        "agent.transaction_gone"
    );
    assert_eq!(
        code(&fx.call("rollback_transaction", json!({"transaction_id": "tx-99"}))),
        "agent.transaction_gone"
    );
    let tx = fx.begin(PG);
    // Another connection's id.
    let foreign = fx.run_in(LITE, &tx, "SELECT 1");
    assert_eq!(code(&foreign), "agent.invalid_arguments");
    assert!(message(&foreign).contains("another connection"));
    assert_eq!(fx.own(PG).run_count(), 0);
    // A database argument would leave the PostgreSQL transaction's connection.
    let other_database = fx.call(
        "execute_query",
        json!({"connection_id": PG, "transaction_id": tx, "statement": "SELECT 1", "database": "other"}),
    );
    assert_eq!(code(&other_database), "agent.invalid_arguments");
    assert_eq!(fx.own(PG).run_count(), 0);
}

#[test]
fn a_failed_statement_aborts_a_postgresql_transaction_and_only_rollback_is_left() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Fail(AgentError::driver(
        "query.failed",
        "relation \"nope\" does not exist",
        false,
    )));
    let failed = fx.run_in(PG, &tx, "INSERT INTO nope VALUES (1)");
    assert_eq!(code(&failed), "query.failed");
    assert!(message(&failed).contains("is aborted; roll it back"), "{}", message(&failed));
    // Everything else is refused without reaching the server, even a read.
    let runs = fx.own(PG).run_count();
    let read = fx.run_in(PG, &tx, "SELECT 1");
    assert_eq!(code(&read), "agent.transaction_aborted");
    assert_eq!(fx.own(PG).run_count(), runs);
    let commit = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&commit), "agent.transaction_aborted");
    assert!(!fx.own(PG).control_calls().contains(&TxControl::Commit));
    let rolled = fx.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rolled["rolled_back"], true);
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
}

#[test]
fn a_failed_statement_aborts_a_sqlite_transaction_too() {
    // SQLite may have rolled the whole transaction back by itself (INSERT OR ROLLBACK,
    // RAISE(ROLLBACK), an interrupted write): the next write would autocommit. So only rollback is left.
    let fx = fixture(Options {
        connections: vec![(LITE, ConnectionKind::Sqlite)],
        ..Options::default()
    });
    let tx = fx.begin(LITE);
    fx.own(LITE).set_behavior(Behavior::Fail(AgentError::driver(
        "query.failed",
        "UNIQUE constraint failed",
        true,
    )));
    let failed = fx.run_in(LITE, &tx, "INSERT INTO t VALUES (1)");
    assert_eq!(code(&failed), "query.failed");
    assert!(message(&failed).contains("is aborted; roll it back"), "{}", message(&failed));
    // The transaction is over, so a retry cannot help even when the driver said it might.
    match &failed {
        ToolOutcome::Failure(error) => assert!(!error.retryable),
        ToolOutcome::Success(_) => unreachable!(),
    }
    fx.own(LITE).set_behavior(Behavior::Affected(1));
    let runs = fx.own(LITE).run_count();
    assert_eq!(code(&fx.run_in(LITE, &tx, "INSERT INTO t VALUES (2)")), "agent.transaction_aborted");
    assert_eq!(code(&fx.run_in(LITE, &tx, "SELECT 1")), "agent.transaction_aborted");
    assert_eq!(fx.own(LITE).run_count(), runs, "nothing reached the session");
    assert_eq!(
        code(&fx.call("commit_transaction", json!({"transaction_id": tx}))),
        "agent.transaction_aborted"
    );
    assert_eq!(fx.ok("rollback_transaction", json!({"transaction_id": tx}))["rolled_back"], true);
}

#[test]
fn a_cancelled_or_timed_out_statement_aborts_the_transaction() {
    let fx = fixture(Options {
        connections: vec![(LITE, ConnectionKind::Sqlite)],
        ..Options::default()
    });
    let tx = fx.begin(LITE);
    fx.own(LITE).set_behavior(Behavior::Fail(AgentError::Cancelled));
    assert_eq!(code(&fx.run_in(LITE, &tx, "INSERT INTO t VALUES (1)")), "agent.cancelled");
    assert_eq!(code(&fx.run_in(LITE, &tx, "SELECT 1")), "agent.transaction_aborted");
}

#[test]
fn a_transaction_holds_only_as_many_changes_as_the_user_can_review() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    for n in 0..50 {
        let outcome = fx.run_in(PG, &tx, &format!("INSERT INTO t VALUES ({n})"));
        assert!(!outcome.is_error(), "{n}: {outcome:?}");
    }
    let outcome = fx.run_in(PG, &tx, "INSERT INTO t VALUES (50)");
    assert_eq!(code(&outcome), "agent.limit");
    assert!(message(&outcome).contains("commit_transaction"));
    assert_eq!(fx.own(PG).run_count(), 50);
    // Reads are not changes.
    fx.own(PG).set_behavior(Behavior::Numbers { count: 1 });
    assert!(!fx.run_in(PG, &tx, "SELECT 1").is_error());
}

// ------------------------------------------------------------------ commit and rollback

#[test]
fn commit_asks_with_the_list_of_writes_then_closes_the_session() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    fx.run_in(PG, &tx, "INSERT INTO t VALUES (1)");
    fx.own(PG).set_behavior(Behavior::Numbers { count: 1 });
    fx.run_in(PG, &tx, "SELECT * FROM t");
    fx.own(PG).set_behavior(Behavior::Affected(2));
    fx.run_in(PG, &tx, "UPDATE t SET a = 2 WHERE id = 1");
    let result = fx.ok("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(result, json!({"transaction_id": tx, "committed": true, "changes": 2}));
    let spec = engine_agent::schema::spec(engine_agent::ToolName::CommitTransaction);
    engine_agent::schema::validate(&spec.output_schema, &result).unwrap();
    // The question named the transaction and listed exactly the writes, not the read.
    let asked = fx.approver.transactions.lock().unwrap().clone();
    assert_eq!(
        asked.last().unwrap(),
        &Some((
            tx.clone(),
            Some(vec![
                "INSERT INTO t VALUES (1)".to_owned(),
                "UPDATE t SET a = 2 WHERE id = 1".to_owned()
            ])
        ))
    );
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Commit]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
    // The id is spent.
    let again = fx.run_in(PG, &tx, "SELECT 1");
    assert_eq!(code(&again), "agent.transaction_gone");
    assert!(message(&again).contains("already committed"));
    assert_eq!(
        code(&fx.call("commit_transaction", json!({"transaction_id": tx}))),
        "agent.transaction_gone"
    );
    // A new transaction can begin on the connection.
    fx.provider.with_extra_session(PG, FakeSession::new(ConnectionKind::PostgreSql).arc());
    fx.begin(PG);
}

#[test]
fn commit_without_changes_asks_nobody() {
    let fx = fixture(Options {
        decision: ApprovalDecision::Deny { reason: None },
        ..Options::default()
    });
    let tx = fx.begin(PG);
    fx.run_in(PG, &tx, "SELECT 1");
    let result = fx.ok("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(result["committed"], true);
    assert_eq!(result["changes"], 0);
    assert!(fx.approver.asked.lock().unwrap().is_empty());
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Commit]);
}

#[test]
fn a_declined_commit_keeps_the_transaction_open_for_a_rollback() {
    let fx = fixture(Options::default());
    *fx.approver.commit_decision.lock().unwrap() = Some(ApprovalDecision::Deny { reason: None });
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    fx.run_in(PG, &tx, "INSERT INTO t VALUES (1)");
    let declined = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&declined), "agent.write_refused");
    assert!(message(&declined).contains("still open"), "{}", message(&declined));
    assert!(message(&declined).contains("rollback_transaction"));
    // Nothing was committed, the session is still there and usable.
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 0);
    fx.own(PG).set_behavior(Behavior::Numbers { count: 1 });
    assert!(!fx.run_in(PG, &tx, "SELECT 1").is_error());
    let rolled = fx.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rolled, json!({"transaction_id": tx, "rolled_back": true, "changes": 1}));
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
    let spec = engine_agent::schema::spec(engine_agent::ToolName::RollbackTransaction);
    engine_agent::schema::validate(&spec.output_schema, &rolled).unwrap();
}

/// Approves the writes, and turns the switch off when the commit is asked.
struct SwitchOffAtCommit {
    allowed: std::sync::atomic::AtomicBool,
}

impl WriteApprover for SwitchOffAtCommit {
    fn approve(&self, request: &WriteApproval<'_>, _cancel: &CancelToken) -> ApprovalDecision {
        if request.transaction.is_some_and(|t| t.commit.is_some()) {
            // The user flips the switch while the question is open, then says yes.
            self.allowed.store(false, Ordering::SeqCst);
        }
        ApprovalDecision::Approve
    }

    fn still_allowed(&self, _request: &WriteApproval<'_>) -> Result<(), String> {
        if self.allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("changes through the MCP server are turned off".into())
        }
    }
}

#[test]
fn the_commit_is_rechecked_after_the_yes() {
    let approver = Arc::new(SwitchOffAtCommit {
        allowed: std::sync::atomic::AtomicBool::new(true),
    });
    let fx = fixture(Options {
        approver: Some(approver),
        ..Options::default()
    });
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    fx.run_in(PG, &tx, "INSERT INTO t VALUES (1)");
    let outcome = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&outcome), "agent.write_refused");
    assert!(message(&outcome).contains("turned off"));
    assert!(message(&outcome).contains("still open"));
    assert!(!fx.own(PG).control_calls().contains(&TxControl::Commit));
    assert!(fx
        .audit()
        .iter()
        .any(|entry| entry.tool == "commit_transaction"
            && entry.approval.as_deref().is_some_and(|a| a.starts_with("approved, then refused"))));
    fx.ok("rollback_transaction", json!({"transaction_id": tx}));
}

#[test]
fn a_connection_edited_or_lowered_before_the_commit_rolls_the_transaction_back() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    fx.run_in(PG, &tx, "INSERT INTO t VALUES (1)");
    fx.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::Read));
    let outcome = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&outcome), "agent.access_denied");
    assert!(message(&outcome).contains("rolled back"));
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
    let again = fx.call("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(again.to_json()["rolled_back"], false);
}

#[test]
fn rolling_back_a_transaction_that_is_gone_is_fine() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.ok("rollback_transaction", json!({"transaction_id": tx}));
    let again = fx.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(again, json!({"transaction_id": tx, "rolled_back": false, "changes": 0}));
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
}

#[test]
fn a_failed_commit_closes_the_session_and_says_the_state_is_not_certain() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    fx.run_in(PG, &tx, "INSERT INTO t VALUES (1)");
    *fx.own(PG).control_error.lock().unwrap() =
        Some(AgentError::driver("query.not_connected", "connection reset", false));
    let outcome = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&outcome), "transaction.commit_failed");
    assert!(message(&outcome).contains("closed"));
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
    let gone = fx.run_in(PG, &tx, "SELECT 1");
    assert_eq!(code(&gone), "agent.transaction_gone");
    assert!(message(&gone).contains("COMMIT did not complete"));
}

// ------------------------------------------------------------------ always rolled back

#[test]
fn close_session_rolls_the_connections_transaction_back() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.ok("close_session", json!({"connection_id": PG}));
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
    let gone = fx.run_in(PG, &tx, "SELECT 1");
    assert_eq!(code(&gone), "agent.transaction_gone");
    assert!(message(&gone).contains("session was closed"));
    // The connection can have a transaction again.
    let fresh = FakeSession::new(ConnectionKind::PostgreSql).arc();
    fx.provider.with_extra_session(PG, Arc::clone(&fresh));
    fx.begin_only(PG);
    assert_eq!(fresh.control_calls(), [TxControl::Begin]);
}

#[test]
fn revoke_rolls_the_connections_transaction_back() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.layer.revoke(PG);
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
    let again = fx.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(again["rolled_back"], false);
}

#[test]
fn lowering_the_access_rolls_the_transaction_back_at_the_next_call() {
    let fx = fixture(Options {
        connections: vec![(PG, ConnectionKind::PostgreSql), (LITE, ConnectionKind::Sqlite)],
        ..Options::default()
    });
    let tx = fx.begin(LITE);
    fx.catalog.update(LITE, |info| info.agent_access = Some(AgentAccess::Read));
    let outcome = fx.run_in(LITE, &tx, "SELECT 1");
    assert_eq!(code(&outcome), "agent.transaction_gone");
    assert!(message(&outcome).contains("access was lowered"), "{}", message(&outcome));
    assert_eq!(fx.own(LITE).control_calls(), [TxControl::Begin, TxControl::Rollback]);
    // A connection switched off is revoked on the next call, transaction included.
    let tx = fx.begin(PG);
    fx.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::Off));
    assert_eq!(
        code(&fx.call("list_namespaces", json!({"connection_id": PG}))),
        "agent.access_denied"
    );
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert_eq!(fx.ok("rollback_transaction", json!({"transaction_id": tx}))["rolled_back"], false);
}

#[test]
fn close_all_and_dropping_the_layer_roll_everything_back() {
    let fx = fixture(Options {
        connections: vec![(PG, ConnectionKind::PostgreSql), (LITE, ConnectionKind::Sqlite)],
        ..Options::default()
    });
    fx.begin(PG);
    fx.begin(LITE);
    fx.layer.close_all();
    for id in [PG, LITE] {
        assert_eq!(fx.own(id).control_calls(), [TxControl::Begin, TxControl::Rollback], "{id}");
        assert_eq!(fx.own(id).disconnects.load(Ordering::SeqCst), 1, "{id}");
    }
    // Dropped without close_all: the layer's Drop does it.
    let fx = fixture(Options::default());
    fx.begin(PG);
    let own = Arc::clone(fx.own(PG));
    drop(fx);
    assert_eq!(own.control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert_eq!(own.disconnects.load(Ordering::SeqCst), 1);
}

#[test]
fn a_server_that_does_not_answer_the_rollback_is_dropped_after_a_bounded_wait() {
    let fx = fixture(Options::default());
    fx.begin(PG);
    fx.own(PG).control_hangs.store(true, Ordering::SeqCst);
    let started = Instant::now();
    fx.layer.close_all();
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(2), "{waited:?}");
    assert!(waited < Duration::from_secs(8), "{waited:?}");
    // The session was closed all the same: the server ends the transaction when it drops.
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
}

#[test]
fn an_idle_transaction_is_rolled_back_and_the_model_is_told_why() {
    let fx = fixture(Options {
        idle: Some(Duration::from_millis(60)),
        ..Options::default()
    });
    // Cheap with nothing open.
    assert_eq!(fx.layer.expire_idle_transactions(), 0);
    let tx = fx.begin(PG);
    assert_eq!(fx.layer.expire_idle_transactions(), 0, "not idle yet");
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(fx.layer.expire_idle_transactions(), 1);
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
    let gone = fx.run_in(PG, &tx, "SELECT 1");
    assert_eq!(code(&gone), "agent.transaction_gone");
    assert!(message(&gone).contains("rolled back after 60 ms idle"), "{}", message(&gone));
    assert_eq!(fx.layer.expire_idle_transactions(), 0);
    // The expiry is in the audit log.
    assert!(fx.audit().iter().any(|entry| entry.tool == "expire_transaction"
        && entry.statement.as_deref().is_some_and(|s| s.contains("idle timeout"))));
}

#[test]
fn every_tool_call_sweeps_idle_transactions_first_and_a_call_restarts_the_clock() {
    let fx = fixture(Options {
        connections: vec![(PG, ConnectionKind::PostgreSql), (LITE, ConnectionKind::Sqlite)],
        idle: Some(Duration::from_millis(500)),
        ..Options::default()
    });
    let kept = fx.begin(PG);
    let idle = fx.begin(LITE);
    for _ in 0..7 {
        std::thread::sleep(Duration::from_millis(100));
        // Calls on `kept` keep it alive.
        assert!(!fx.run_in(PG, &kept, "SELECT 1").is_error());
    }
    // An unrelated call expired the idle one on its way in: its session is closed, without a
    // ROLLBACK round trip that a server that does not answer could hold the call up with.
    let outcome = fx.call("list_connections", json!({}));
    assert!(!outcome.is_error());
    assert_eq!(fx.own(LITE).control_calls(), [TxControl::Begin]);
    assert_eq!(fx.own(LITE).disconnects.load(Ordering::SeqCst), 1);
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin]);
    assert_eq!(code(&fx.run_in(LITE, &idle, "SELECT 1")), "agent.transaction_gone");
}

#[test]
fn a_transaction_with_a_statement_running_is_not_expired() {
    let fx = fixture(Options {
        idle: Some(Duration::from_millis(50)),
        ..Options::default()
    });
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::BlockUntilCancelled);
    let token = CancelToken::new();
    let running = {
        let layer = Arc::clone(&fx.layer);
        let token = token.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            layer.call_cancellable(
                "execute_query",
                json!({"connection_id": PG, "transaction_id": tx, "statement": "SELECT 1"}),
                &token,
            )
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.own(PG).run_count() == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(fx.layer.expire_idle_transactions(), 0);
    token.cancel();
    assert_eq!(code(&running.join().unwrap()), "agent.cancelled");
    fx.ok("rollback_transaction", json!({"transaction_id": tx}));
}

#[test]
fn a_rollback_cancels_the_statement_that_is_running_in_the_transaction() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::BlockUntilCancelled);
    let running = {
        let layer = Arc::clone(&fx.layer);
        let tx = tx.clone();
        std::thread::spawn(move || {
            layer.call(
                "execute_query",
                json!({"connection_id": PG, "transaction_id": tx, "statement": "SELECT 1"}),
            )
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.own(PG).run_count() == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let rolled = fx.ok("rollback_transaction", json!({"transaction_id": tx}));
    assert_eq!(rolled["rolled_back"], true);
    assert_eq!(code(&running.join().unwrap()), "agent.cancelled");
    // No ROLLBACK queued behind the running statement: the session is closed, which ends it.
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin]);
    assert_eq!(fx.own(PG).disconnects.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------------------------ audit

#[test]
fn begin_commit_and_rollback_leave_audit_lines_without_extra_statement_text() {
    let fx = fixture(Options {
        connections: vec![(PG, ConnectionKind::PostgreSql), (LITE, ConnectionKind::Sqlite)],
        ..Options::default()
    });
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    fx.run_in(PG, &tx, "INSERT INTO t VALUES (1)");
    fx.ok("commit_transaction", json!({"transaction_id": tx}));
    let other = fx.begin(LITE);
    fx.ok("rollback_transaction", json!({"transaction_id": other}));
    let entries = fx.audit();
    let line = |tool: &str, outcome: &str, connection: &str| {
        entries
            .iter()
            .find(|entry| {
                entry.tool == tool
                    && entry.outcome == outcome
                    && entry.connection_id.as_deref() == Some(connection)
            })
            .unwrap_or_else(|| panic!("no {tool} {outcome} line for {connection} in {entries:#?}"))
    };
    assert_eq!(line("begin_transaction", "ok", PG).statement.as_deref(), Some("BEGIN"));
    let started = line("commit_transaction", "started", PG);
    assert_eq!(started.statement.as_deref(), Some("COMMIT"));
    assert_eq!(started.rows, Some(1));
    let done = line("commit_transaction", "ok", PG);
    assert_eq!(done.statement.as_deref(), Some("COMMIT"));
    assert_eq!(done.approval.as_deref(), Some("approved"));
    assert_eq!(line("rollback_transaction", "ok", LITE).statement.as_deref(), Some("ROLLBACK"));
}

// ------------------------------------------------------------------ the commit window

#[derive(Default)]
struct Latch {
    set: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}

impl Latch {
    fn open(&self) {
        *self.set.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut set = self.set.lock().unwrap();
        while !*set {
            let (next, timeout) = self
                .changed
                .wait_timeout(set, Duration::from_secs(20))
                .unwrap();
            assert!(!timeout.timed_out(), "a latch was never opened");
            set = next;
        }
    }
}

/// Approves everything, but holds a question (a statement containing `HOLD`, or the commit) until
/// the test lets it go.
#[derive(Default)]
struct Holding {
    statement_asked: Latch,
    statement_go: Latch,
    commit_asked: Latch,
    commit_go: Latch,
}

impl WriteApprover for Holding {
    fn approve(&self, request: &WriteApproval<'_>, _cancel: &CancelToken) -> ApprovalDecision {
        if request.transaction.is_some_and(|t| t.commit.is_some()) {
            self.commit_asked.open();
            self.commit_go.wait();
        } else if request.statement.contains("HOLD") {
            self.statement_asked.open();
            self.statement_go.wait();
        }
        ApprovalDecision::Approve
    }
}

fn holding_fixture() -> (Fixture, Arc<Holding>, String) {
    let holding = Arc::new(Holding::default());
    let fx = fixture(Options {
        approver: Some(holding.clone()),
        ..Options::default()
    });
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    assert!(!fx.run_in(PG, &tx, "INSERT INTO t VALUES ('A')").is_error());
    (fx, holding, tx)
}

fn commit_in_thread(fx: &Fixture, tx: &str) -> std::thread::JoinHandle<ToolOutcome> {
    let layer = Arc::clone(&fx.layer);
    let tx = tx.to_owned();
    std::thread::spawn(move || layer.call("commit_transaction", json!({"transaction_id": tx})))
}

#[test]
fn no_statement_runs_in_a_transaction_while_its_commit_question_is_open() {
    let (fx, holding, tx) = holding_fixture();
    let commit = commit_in_thread(&fx, &tx);
    holding.commit_asked.wait();
    // A write and a read are both refused until the commit is decided.
    for statement in ["INSERT INTO t VALUES ('B')", "SELECT 1"] {
        let outcome = fx.run_in(PG, &tx, statement);
        assert_eq!(code(&outcome), "agent.limit", "{statement}");
        assert!(message(&outcome).contains("waiting for the user's answer"), "{}", message(&outcome));
    }
    assert_eq!(fx.own(PG).run_count(), 1);
    // A second commit does not ask again either.
    let second = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&second), "agent.limit");
    holding.commit_go.open();
    let committed = commit.join().unwrap();
    assert_eq!(committed.to_json()["changes"], 1, "{committed:?}");
    assert_eq!(fx.own(PG).control_calls(), [TxControl::Begin, TxControl::Commit]);
    assert_eq!(fx.own(PG).run_count(), 1, "only the approved write ran");
}

#[test]
fn a_write_approved_after_the_commit_question_opened_is_refused_and_never_runs() {
    let (fx, holding, tx) = holding_fixture();
    // B is waiting for its approval when the commit takes its list.
    let b = {
        let layer = Arc::clone(&fx.layer);
        let tx = tx.clone();
        std::thread::spawn(move || {
            layer.call(
                "execute_query",
                json!({"connection_id": PG, "transaction_id": tx, "statement": "INSERT INTO t VALUES ('HOLD B')"}),
            )
        })
    };
    holding.statement_asked.wait();
    let commit = commit_in_thread(&fx, &tx);
    holding.commit_asked.wait();
    // The user approves B's change after the commit question was put: B must not run unlisted.
    holding.statement_go.open();
    let b = b.join().unwrap();
    assert_eq!(code(&b), "agent.limit", "{b:?}");
    assert_eq!(fx.own(PG).run_count(), 1);
    holding.commit_go.open();
    let committed = commit.join().unwrap();
    assert_eq!(committed.to_json()["changes"], 1, "{committed:?}");
    assert_eq!(fx.own(PG).run_count(), 1);
}

#[test]
fn a_statement_that_fails_during_the_commit_question_stops_the_commit() {
    let (fx, holding, tx) = holding_fixture();
    // A read that started before the commit and fails while the question is open.
    let release = Arc::new(Latch::default());
    let started = Arc::new(Latch::default());
    {
        let (release, started) = (Arc::clone(&release), Arc::clone(&started));
        fx.own(PG).set_behavior(Behavior::Script(Box::new(move |_, _| {
            started.open();
            release.wait();
            Err(AgentError::driver("query.failed", "boom", false))
        })));
    }
    let read = {
        let layer = Arc::clone(&fx.layer);
        let tx = tx.clone();
        std::thread::spawn(move || {
            layer.call(
                "execute_query",
                json!({"connection_id": PG, "transaction_id": tx, "statement": "SELECT 1"}),
            )
        })
    };
    started.wait();
    let commit = commit_in_thread(&fx, &tx);
    holding.commit_asked.wait();
    release.open();
    assert_eq!(code(&read.join().unwrap()), "query.failed");
    holding.commit_go.open();
    // The server would turn the COMMIT of an aborted transaction into a ROLLBACK and say nothing.
    let outcome = commit.join().unwrap();
    assert_eq!(code(&outcome), "agent.transaction_aborted", "{outcome:?}");
    assert!(!fx.own(PG).control_calls().contains(&TxControl::Commit));
    fx.ok("rollback_transaction", json!({"transaction_id": tx}));
}

#[test]
fn a_statement_still_running_when_the_commit_is_approved_stops_it_and_keeps_the_transaction() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    fx.run_in(PG, &tx, "INSERT INTO t VALUES ('A')");
    fx.own(PG).set_behavior(Behavior::BlockUntilCancelled);
    let token = CancelToken::new();
    let read = {
        let (layer, token, tx) = (Arc::clone(&fx.layer), token.clone(), tx.clone());
        std::thread::spawn(move || {
            layer.call_cancellable(
                "execute_query",
                json!({"connection_id": PG, "transaction_id": tx, "statement": "SELECT 1"}),
                &token,
            )
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.own(PG).run_count() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let outcome = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&outcome), "agent.limit", "{outcome:?}");
    assert!(message(&outcome).contains("still running"), "{}", message(&outcome));
    assert!(message(&outcome).contains("still open"));
    assert!(!fx.own(PG).control_calls().contains(&TxControl::Commit));
    token.cancel();
    read.join().unwrap();
    fx.ok("rollback_transaction", json!({"transaction_id": tx}));
}

// ------------------------------------------------------------------ what a commit can show

/// Approves everything; a commit of more than two statements cannot be shown.
struct ShowsTwo;

impl WriteApprover for ShowsTwo {
    fn approve(&self, _request: &WriteApproval<'_>, _cancel: &CancelToken) -> ApprovalDecision {
        ApprovalDecision::Approve
    }

    fn commit_reviewable(&self, statements: &[String]) -> Result<(), String> {
        if statements.len() > 2 {
            Err("more than two changes cannot be shown".into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn a_write_that_would_make_the_commit_unreviewable_is_refused_before_it_is_asked() {
    let fx = fixture(Options {
        approver: Some(Arc::new(ShowsTwo)),
        ..Options::default()
    });
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::Affected(1));
    for n in 0..2 {
        assert!(!fx.run_in(PG, &tx, &format!("INSERT INTO t VALUES ({n})")).is_error());
    }
    let third = fx.run_in(PG, &tx, "INSERT INTO t VALUES (3)");
    assert_eq!(code(&third), "agent.limit");
    assert!(message(&third).contains("more than two changes cannot be shown"), "{}", message(&third));
    assert!(message(&third).contains("commit it with commit_transaction or roll it back")
        || message(&third).contains("Commit it with commit_transaction or roll it back"));
    assert_eq!(fx.own(PG).run_count(), 2, "never asked, never run");
    // The two that were approved can still be committed.
    assert_eq!(fx.ok("commit_transaction", json!({"transaction_id": tx}))["changes"], 2);
}

#[test]
fn copy_is_refused_inside_a_transaction() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    for statement in [
        "COPY t TO '/tmp/out.csv'",
        "COPY t FROM '/tmp/in.csv'",
        "COPY t TO PROGRAM 'cat'",
        "/* c */ copy (SELECT 1) TO '/tmp/x'",
    ] {
        let outcome = fx.run_in(PG, &tx, statement);
        assert_eq!(code(&outcome), "agent.unsupported", "{statement}");
        assert!(message(&outcome).contains("COPY"), "{statement}");
    }
    assert_eq!(fx.own(PG).run_count(), 0);
    // The refusal does not abort the transaction.
    assert!(!fx.run_in(PG, &tx, "SELECT 1").is_error());
}

// ------------------------------------------------------------------ shutdown

#[test]
fn no_transaction_is_begun_while_close_all_is_running() {
    let fx = fixture(Options {
        connections: vec![(PG, ConnectionKind::PostgreSql), (LITE, ConnectionKind::Sqlite)],
        ..Options::default()
    });
    fx.begin(PG);
    fx.own(PG).control_hangs.store(true, Ordering::SeqCst);
    fx.ok("open_session", json!({"connection_id": LITE}));
    let closer = {
        let layer = Arc::clone(&fx.layer);
        std::thread::spawn(move || layer.close_all())
    };
    // close_all is stuck on the PostgreSQL rollback (about 3 s).
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fx.own(PG).control_calls().contains(&TxControl::Rollback) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let refused = fx.call("begin_transaction", json!({"connection_id": LITE}));
    assert_eq!(code(&refused), "agent.not_connected");
    assert!(message(&refused).contains("shutting down"));
    assert!(fx.own(LITE).control_calls().is_empty(), "no session was opened for it");
    closer.join().unwrap();
    // Once it is done the layer is usable again.
    fx.ok("open_session", json!({"connection_id": LITE}));
    fx.begin_only(LITE);
}

#[test]
fn a_panic_inside_the_driver_aborts_the_transaction() {
    for statement in ["INSERT INTO t VALUES (1)", "SELECT 1"] {
        let fx = fixture(Options::default());
        let tx = fx.begin(PG);
        fx.own(PG).set_behavior(Behavior::Script(Box::new(|_, _| panic!("the driver panicked"))));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fx.run_in(PG, &tx, statement)
        }));
        assert!(panicked.is_err(), "{statement}");
        // The caller (the MCP server) caught the panic; the transaction must not be usable on.
        fx.own(PG).set_behavior(Behavior::Affected(1));
        let runs = fx.own(PG).run_count();
        assert_eq!(code(&fx.run_in(PG, &tx, "INSERT INTO t VALUES (2)")), "agent.transaction_aborted", "{statement}");
        assert_eq!(code(&fx.call("commit_transaction", json!({"transaction_id": tx}))), "agent.transaction_aborted");
        assert_eq!(fx.own(PG).run_count(), runs);
        assert!(!fx.own(PG).control_calls().contains(&TxControl::Commit));
        assert_eq!(fx.ok("rollback_transaction", json!({"transaction_id": tx}))["rolled_back"], true);
    }
}

#[test]
fn a_write_still_running_when_the_commit_is_approved_stops_the_commit() {
    let fx = fixture(Options::default());
    let tx = fx.begin(PG);
    fx.own(PG).set_behavior(Behavior::BlockUntilCancelled);
    let token = CancelToken::new();
    let write = {
        let (layer, token, tx) = (Arc::clone(&fx.layer), token.clone(), tx.clone());
        std::thread::spawn(move || {
            layer.call_cancellable(
                "execute_query",
                json!({"connection_id": PG, "transaction_id": tx, "statement": "INSERT INTO t VALUES ('slow')"}),
                &token,
            )
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.own(PG).run_count() < 1 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    // The write is approved, recorded and running: it is on the commit's list, so the commit is asked
    // and approved, and then refused because the write has not finished.
    let outcome = fx.call("commit_transaction", json!({"transaction_id": tx}));
    assert_eq!(code(&outcome), "agent.limit", "{outcome:?}");
    assert!(message(&outcome).contains("still running"), "{}", message(&outcome));
    assert!(message(&outcome).contains("still open"));
    assert!(!fx.own(PG).control_calls().contains(&TxControl::Commit));
    token.cancel();
    assert_eq!(code(&write.join().unwrap()), "agent.cancelled");
    // Cancelled, so aborted: only a rollback is left.
    assert_eq!(code(&fx.call("commit_transaction", json!({"transaction_id": tx}))), "agent.transaction_aborted");
    fx.ok("rollback_transaction", json!({"transaction_id": tx}));
}
