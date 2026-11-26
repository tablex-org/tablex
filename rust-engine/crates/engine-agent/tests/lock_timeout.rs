//! `lock_timeout_secs` of `execute_query` and `execute_batch` against fake sessions: what is sent
//! to the session and in which order, that the session is put back on every path, that nothing
//! extra is sent without the parameter, and what the agent is told. The live counterpart (a real
//! lock held by another session) is `tests/live.rs`.

mod common;

use common::recorder::Recorder;
use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::limits::RowCollector;
use engine_agent::{
    AgentAccess, AgentError, ApprovalDecision, ConnectionKind, Limits, ToolLayer, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const ID: &str = "c1";

struct Rig {
    layer: ToolLayer,
    provider: Arc<FakeProvider>,
    session: Arc<FakeSession>,
    own: Arc<FakeSession>,
    approver: Arc<Recorder>,
}

/// A statement timeout well above every `lock_timeout_secs` the tests use.
fn rig(kind: ConnectionKind, access: AgentAccess) -> Rig {
    rig_with(
        kind,
        access,
        Limits {
            statement_timeout: Duration::from_secs(900),
            ..Limits::default()
        },
    )
}

fn rig_with(kind: ConnectionKind, access: AgentAccess, limits: Limits) -> Rig {
    let session = FakeSession::new(kind).arc();
    let own = FakeSession::new(kind).arc();
    let provider = FakeProvider::new();
    provider.with_session(ID, Arc::clone(&session));
    provider.with_extra_session(ID, Arc::clone(&own));
    let catalog = FakeCatalog::new(vec![connection(ID, kind, Some(access))]);
    let approver = Recorder::new(ApprovalDecision::Approve).arc();
    let layer = ToolLayer::new(catalog, provider.clone(), "test")
        .with_transactions()
        .with_limits(limits)
        .with_approver(approver.clone());
    let opened = layer.call("open_session", json!({"connection_id": ID}));
    assert!(!opened.is_error(), "{opened:?}");
    Rig {
        layer,
        provider,
        session,
        own,
        approver,
    }
}

fn ran(session: &FakeSession) -> Vec<String> {
    session.runs.lock().unwrap().iter().map(|(text, _)| text.clone()).collect()
}

fn query(statement: &str, extra: Value) -> Value {
    let mut args = json!({"connection_id": ID, "statement": statement});
    args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    args
}

fn batch(script: &str, extra: Value) -> Value {
    let mut args = json!({"connection_id": ID, "script": script});
    args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    args
}

fn failure(outcome: ToolOutcome) -> engine_agent::ToolError {
    match outcome {
        ToolOutcome::Failure(error) => error,
        ToolOutcome::Success(value) => panic!("expected a failure, got {value}"),
    }
}

const ALTER: &str = "ALTER TABLE t ADD COLUMN c text";

/// A session whose statements succeed, and which reads its lock wait settings as
/// `(31536000, 50)`; `fail` names a statement that fails with the given error.
fn script(session: &FakeSession, fail: Option<(&'static str, AgentError)>) {
    session.set_behavior(Behavior::Script(Box::new(move |request, _| {
        if let Some((text, error)) = &fail {
            if request.text == *text {
                return Err(error.clone());
            }
        }
        let mut collector = RowCollector::new(Vec::new(), request.row_limit, request.limits);
        if request.text.starts_with("SELECT @@SESSION") {
            let _ = collector.push_row(["31536000".to_owned(), "50".to_owned()]);
        }
        Ok(collector.finish(false, Some(0)))
    })));
}

#[test]
fn without_the_parameter_the_statement_is_the_only_round_trip() {
    for kind in [ConnectionKind::PostgreSql, ConnectionKind::MySql, ConnectionKind::MariaDb] {
        let rig = rig(kind, AgentAccess::ReadWrite);
        assert!(!rig.layer.call("execute_query", query(ALTER, json!({}))).is_error());
        assert_eq!(ran(&rig.session), [ALTER], "{kind:?}");
        let out = rig.layer.call("execute_batch", batch(&format!("{ALTER}; SELECT 1"), json!({})));
        assert!(!out.is_error(), "{out:?}");
        assert_eq!(ran(&rig.session), [ALTER, ALTER, "SELECT 1"], "{kind:?}");
    }
}

#[test]
fn postgresql_runs_the_statement_in_a_transaction_of_its_own() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    let out = rig.layer.call("execute_query", query(ALTER, json!({"lock_timeout_secs": 5})));
    assert!(!out.is_error(), "{out:?}");
    assert_eq!(
        ran(&rig.session),
        ["BEGIN; SET LOCAL lock_timeout = '5s'", ALTER, "COMMIT"]
    );
    // The statement was approved as the plain write it is, once.
    assert_eq!(rig.approver.asked().len(), 1);
    rig.session.runs.lock().unwrap().clear();
    let out = rig.layer.call("execute_query", query("SELECT 1", json!({"lock_timeout_secs": 600})));
    assert!(!out.is_error(), "{out:?}");
    assert_eq!(
        ran(&rig.session),
        ["BEGIN; SET LOCAL lock_timeout = '600s'", "SELECT 1", "COMMIT"]
    );
    // The database of the call goes to every step: PostgreSQL keeps a session per database.
    rig.session.runs.lock().unwrap().clear();
    let args = query(ALTER, json!({"lock_timeout_secs": 2, "database": "other"}));
    assert!(!rig.layer.call("execute_query", args).is_error());
    assert_eq!(ran(&rig.session).len(), 3);
}

#[test]
fn a_statement_that_cannot_run_in_a_transaction_block_keeps_set_and_reset() {
    for statement in [
        "CREATE INDEX CONCURRENTLY i ON t (a)",
        "CREATE DATABASE scratch",
    ] {
        let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
        let out = rig.layer.call("execute_query", query(statement, json!({"lock_timeout_secs": 5})));
        // CREATE DATABASE is additive too; either way it is a plain write.
        assert!(!out.is_error(), "{statement}: {out:?}");
        assert_eq!(
            ran(&rig.session),
            ["SET lock_timeout = '5s'", statement, "RESET lock_timeout"],
            "{statement}"
        );
    }
}

#[test]
fn mysql_and_mariadb_read_set_and_restore_both_settings_as_numbers() {
    for kind in [ConnectionKind::MySql, ConnectionKind::MariaDb] {
        let rig = rig(kind, AgentAccess::ReadWrite);
        script(&rig.session, None);
        let out = rig.layer.call("execute_query", query(ALTER, json!({"lock_timeout_secs": 7})));
        assert!(!out.is_error(), "{out:?}");
        assert_eq!(
            ran(&rig.session),
            [
                "SELECT @@SESSION.lock_wait_timeout, @@SESSION.innodb_lock_wait_timeout",
                "SET SESSION lock_wait_timeout = 7, SESSION innodb_lock_wait_timeout = 7",
                ALTER,
                "SET SESSION lock_wait_timeout = 31536000, SESSION innodb_lock_wait_timeout = 50",
            ],
            "{kind:?}"
        );
    }
}

#[test]
fn a_user_variable_assignment_is_refused_like_set() {
    for kind in [ConnectionKind::MySql, ConnectionKind::MariaDb] {
        let rig = rig(kind, AgentAccess::ReadWrite);
        for statement in [
            "SELECT @tablex_lock_wait_timeout := 31536000",
            "SELECT id INTO @a FROM t",
        ] {
            let outcome = failure(rig.layer.call("execute_query", query(statement, json!({}))));
            assert_eq!(outcome.code, "agent.unsupported", "{statement}");
            assert!(outcome.message.contains(statement), "{}", outcome.message);
        }
        assert!(ran(&rig.session).is_empty());
    }
}

#[test]
fn the_transaction_is_rolled_back_when_the_statement_fails_or_is_cancelled() {
    for (error, code) in [
        (
            AgentError::driver("query.failed", "ERROR: canceling statement due to lock timeout", false),
            "query.failed",
        ),
        (AgentError::Cancelled, "agent.cancelled"),
    ] {
        let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
        script(&rig.session, Some((ALTER, error.clone())));
        let outcome = failure(rig.layer.call(
            "execute_query",
            query(ALTER, json!({"lock_timeout_secs": 3})),
        ));
        assert_eq!(outcome.code, code);
        assert_eq!(
            ran(&rig.session),
            ["BEGIN; SET LOCAL lock_timeout = '3s'", ALTER, "ROLLBACK"],
            "after {error:?}"
        );
        assert_eq!(rig.session.disconnects.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn a_lock_timeout_tells_the_agent_to_retry_later() {
    for (kind, message) in [
        (ConnectionKind::PostgreSql, "ERROR: canceling statement due to lock timeout"),
        (
            ConnectionKind::MariaDb,
            "Lock wait timeout exceeded; try restarting transaction (error 1205, SQLSTATE HY000)",
        ),
    ] {
        let rig = rig(kind, AgentAccess::ReadWrite);
        script(&rig.session, Some((ALTER, AgentError::driver("query.failed", message, false))));
        let outcome = failure(rig.layer.call(
            "execute_query",
            query(ALTER, json!({"lock_timeout_secs": 4})),
        ));
        assert!(outcome.message.contains("The lock timeout was hit"), "{}", outcome.message);
        assert!(outcome.message.contains("Retry later"), "{}", outcome.message);
        assert!(outcome.retryable);
        // Without the parameter the error is the driver's own.
        let outcome = failure(rig.layer.call("execute_query", query(ALTER, json!({}))));
        assert!(!outcome.message.contains("lock timeout was hit"), "{}", outcome.message);
    }
}

#[test]
fn a_commit_with_no_answer_closes_the_session_through_the_layer() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    script(
        &rig.session,
        Some(("COMMIT", AgentError::NotConnected("the connection was lost".into()))),
    );
    let outcome = failure(rig.layer.call(
        "execute_query",
        query(ALTER, json!({"lock_timeout_secs": 2})),
    ));
    assert_eq!(outcome.code, "agent.not_connected");
    assert!(outcome.message.contains("could not commit"), "{}", outcome.message);
    assert!(outcome.message.contains("session was closed"), "{}", outcome.message);
    // Closed for real: the slot is gone, the provider was told (a tunnel goes with it), and the
    // next open starts from nothing instead of finding a dead one.
    assert!(!rig.layer.is_open(ID));
    assert_eq!(rig.provider.closed.load(Ordering::SeqCst), 1);
    assert_eq!(rig.session.disconnects.load(Ordering::SeqCst), 1);
    let reopened = rig.layer.call("open_session", json!({"connection_id": ID}));
    assert!(!reopened.is_error(), "{reopened:?}");
    assert!(rig.layer.is_open(ID));
}

#[test]
fn a_set_and_reset_that_cannot_be_reset_closes_the_session_too() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    let statement = "CREATE INDEX CONCURRENTLY i ON t (a)";
    script(
        &rig.session,
        Some(("RESET lock_timeout", AgentError::driver("query.failed", "gone", false))),
    );
    let outcome = failure(rig.layer.call(
        "execute_query",
        query(statement, json!({"lock_timeout_secs": 2})),
    ));
    assert_eq!(outcome.code, "agent.not_connected");
    assert!(outcome.message.contains("session was closed"), "{}", outcome.message);
    assert!(!rig.layer.is_open(ID));
    assert_eq!(rig.provider.closed.load(Ordering::SeqCst), 1);
}

#[test]
fn a_lift_that_fails_inside_a_transaction_aborts_it_and_the_commit_says_so() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    script(
        &rig.own,
        Some((
            "SET LOCAL lock_timeout = DEFAULT",
            AgentError::driver("query.failed", "current transaction is aborted", false),
        )),
    );
    let ToolOutcome::Success(begun) = rig.layer.call("begin_transaction", json!({"connection_id": ID})) else {
        panic!("begin failed")
    };
    let id = begun["transaction_id"].as_str().unwrap().to_owned();
    // The statement itself succeeded, but the timeout could not be lifted afterwards.
    let outcome = failure(rig.layer.call(
        "execute_query",
        query(ALTER, json!({"transaction_id": id, "lock_timeout_secs": 5})),
    ));
    assert_eq!(outcome.code, "query.lock_timeout_not_lifted");
    assert!(outcome.message.contains("aborted") && outcome.message.contains("rollback_transaction"), "{}", outcome.message);
    assert!(!outcome.retryable);
    // PostgreSQL turns a COMMIT of an aborted transaction into a silent ROLLBACK: it must not be
    // sent, and the approved write must not be reported as committed.
    let commit = failure(rig.layer.call("commit_transaction", json!({"transaction_id": id})));
    assert_eq!(commit.code, "agent.transaction_aborted");
    assert!(!ran(&rig.own).iter().any(|text| text == "COMMIT"));
    let rolled = rig.layer.call("rollback_transaction", json!({"transaction_id": id}));
    assert!(!rolled.is_error(), "{rolled:?}");
}

#[test]
fn a_lock_timeout_inside_a_transaction_says_to_roll_back() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    script(
        &rig.own,
        Some((ALTER, AgentError::driver("query.failed", "ERROR: canceling statement due to lock timeout", false))),
    );
    let ToolOutcome::Success(begun) = rig.layer.call("begin_transaction", json!({"connection_id": ID})) else {
        panic!("begin failed")
    };
    let id = begun["transaction_id"].as_str().unwrap().to_owned();
    let outcome = failure(rig.layer.call(
        "execute_query",
        query(ALTER, json!({"transaction_id": id, "lock_timeout_secs": 5})),
    ));
    assert!(outcome.message.contains("roll it back, then retry the transaction"), "{}", outcome.message);
    assert!(!outcome.message.contains("Retry later"), "{}", outcome.message);
    assert!(!outcome.retryable);
    // Nothing was sent after the failure: the transaction only accepts a rollback.
    assert_eq!(ran(&rig.own), ["SET LOCAL lock_timeout = '5s'", ALTER]);
}

#[test]
fn a_bad_value_or_an_engine_without_the_setting_is_refused_before_anything_runs() {
    let rig_pg = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    for value in [json!(0), json!(601), json!(-5), json!(100_000_000_000_i64)] {
        let outcome = failure(rig_pg.layer.call("execute_query", query(ALTER, json!({"lock_timeout_secs": value}))));
        assert_eq!(outcome.code, "agent.invalid_arguments", "{value}");
        assert!(outcome.message.contains("from 1 to 600"), "{}", outcome.message);
        let outcome = failure(rig_pg.layer.call("execute_batch", batch(ALTER, json!({"lock_timeout_secs": value}))));
        assert_eq!(outcome.code, "agent.invalid_arguments", "{value}");
    }
    // Not a whole number: the schema says so.
    for value in [json!("5"), json!(1.5)] {
        let outcome = failure(rig_pg.layer.call("execute_query", query(ALTER, json!({"lock_timeout_secs": value}))));
        assert_eq!(outcome.code, "agent.invalid_arguments", "{value}");
    }
    assert!(ran(&rig_pg.session).is_empty());
    assert!(rig_pg.approver.asked().is_empty());

    for kind in [ConnectionKind::Sqlite, ConnectionKind::ClickHouse, ConnectionKind::MongoDb, ConnectionKind::Redis] {
        let rig = rig(kind, AgentAccess::ReadWrite);
        for (tool, args) in [
            ("execute_query", query("SELECT 1", json!({"lock_timeout_secs": 5}))),
            ("execute_batch", batch("SELECT 1", json!({"lock_timeout_secs": 5}))),
        ] {
            let outcome = failure(rig.layer.call(tool, args));
            assert_eq!(outcome.code, "agent.unsupported", "{kind:?} {tool}");
            assert!(
                outcome.message.contains("PostgreSQL, MySQL and MariaDB"),
                "{}",
                outcome.message
            );
            assert!(outcome.message.contains(kind.driver_id()), "{}", outcome.message);
        }
        assert!(ran(&rig.session).is_empty(), "{kind:?}");
    }
}

#[test]
fn a_timeout_the_statement_timeout_would_cut_off_is_refused_with_its_name() {
    // The default statement timeout is 30 s.
    let rig = rig_with(ConnectionKind::PostgreSql, AgentAccess::ReadWrite, Limits::default());
    for tool_args in [
        ("execute_query", query(ALTER, json!({"lock_timeout_secs": 30}))),
        ("execute_query", query(ALTER, json!({"lock_timeout_secs": 600}))),
        ("execute_batch", batch(ALTER, json!({"lock_timeout_secs": 31}))),
    ] {
        let outcome = failure(rig.layer.call(tool_args.0, tool_args.1.clone()));
        assert_eq!(outcome.code, "agent.invalid_arguments", "{:?}", tool_args.1);
        assert!(outcome.message.contains("statement timeout (30 s)"), "{}", outcome.message);
        assert!(outcome.message.contains("smaller value"), "{}", outcome.message);
    }
    assert!(ran(&rig.session).is_empty());
    assert!(rig.approver.asked().is_empty());
    // Just below it is fine.
    let out = rig.layer.call("execute_query", query(ALTER, json!({"lock_timeout_secs": 29})));
    assert!(!out.is_error(), "{out:?}");
}

#[test]
fn the_connections_own_statement_timeout_is_the_one_compared() {
    let session = FakeSession::new(ConnectionKind::MariaDb).arc();
    let provider = FakeProvider::new();
    provider.with_session(ID, session.clone());
    let mut info = connection(ID, ConnectionKind::MariaDb, Some(AgentAccess::ReadWrite));
    info.statement_timeout_secs = Some(10);
    let layer = ToolLayer::new(FakeCatalog::new(vec![info]), provider, "test")
        .with_approver(Recorder::new(ApprovalDecision::Approve).arc());
    assert!(!layer.call("open_session", json!({"connection_id": ID})).is_error());
    let outcome = failure(layer.call("execute_query", query("SELECT 1", json!({"lock_timeout_secs": 10}))));
    assert!(outcome.message.contains("statement timeout (10 s)"), "{}", outcome.message);
    assert!(session.runs.lock().unwrap().is_empty());
}

#[test]
fn a_script_applies_it_around_each_statement() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    let out = rig.layer.call(
        "execute_batch",
        batch(&format!("{ALTER}; SELECT 1"), json!({"lock_timeout_secs": 5})),
    );
    assert!(!out.is_error(), "{out:?}");
    assert_eq!(
        ran(&rig.session),
        [
            "BEGIN; SET LOCAL lock_timeout = '5s'", ALTER, "COMMIT",
            "BEGIN; SET LOCAL lock_timeout = '5s'", "SELECT 1", "COMMIT",
        ]
    );
    // Asked once, for the one write.
    assert_eq!(rig.approver.asked().len(), 1);
}

#[test]
fn inside_a_transaction_it_is_local_to_the_call_and_lifted_after_it() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    let ToolOutcome::Success(begun) = rig.layer.call("begin_transaction", json!({"connection_id": ID})) else {
        panic!("begin failed")
    };
    let id = begun["transaction_id"].as_str().unwrap().to_owned();
    let out = rig.layer.call(
        "execute_query",
        query(ALTER, json!({"transaction_id": id, "lock_timeout_secs": 5})),
    );
    assert!(!out.is_error(), "{out:?}");
    assert_eq!(
        ran(&rig.own),
        ["SET LOCAL lock_timeout = '5s'", ALTER, "SET LOCAL lock_timeout = DEFAULT"]
    );
    assert!(ran(&rig.session).is_empty());
    // A script that is a transaction of its own.
    let rig = self::rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    let out = rig.layer.call(
        "execute_batch",
        batch(ALTER, json!({"lock_timeout_secs": 5, "use_transaction": true})),
    );
    assert!(!out.is_error(), "{out:?}");
    assert_eq!(
        ran(&rig.own),
        ["SET LOCAL lock_timeout = '5s'", ALTER, "SET LOCAL lock_timeout = DEFAULT"]
    );
}

#[test]
fn inside_a_mysql_transaction_the_settings_are_read_set_and_put_back() {
    const UPDATE: &str = "UPDATE t SET a = 1 WHERE id = 1";
    for kind in [ConnectionKind::MySql, ConnectionKind::MariaDb] {
        let rig = rig(kind, AgentAccess::ReadWrite);
        script(&rig.own, None);
        let ToolOutcome::Success(begun) = rig.layer.call("begin_transaction", json!({"connection_id": ID})) else {
            panic!("begin failed")
        };
        let id = begun["transaction_id"].as_str().unwrap().to_owned();
        let out = rig.layer.call(
            "execute_query",
            query(UPDATE, json!({"transaction_id": id, "lock_timeout_secs": 5})),
        );
        assert!(!out.is_error(), "{out:?}");
        // Only the transaction's own session is touched, and its settings go back as they were.
        assert_eq!(
            ran(&rig.own),
            [
                "SELECT @@SESSION.lock_wait_timeout, @@SESSION.innodb_lock_wait_timeout",
                "SET SESSION lock_wait_timeout = 5, SESSION innodb_lock_wait_timeout = 5",
                UPDATE,
                "SET SESSION lock_wait_timeout = 31536000, SESSION innodb_lock_wait_timeout = 50",
            ],
            "{kind:?}"
        );
        assert!(ran(&rig.session).is_empty());
    }
}

#[test]
fn the_session_state_refusal_names_what_to_drop_and_says_the_rest_is_allowed() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    let outcome = failure(rig.layer.call(
        "execute_query",
        query("SET lock_timeout = '5s'", json!({})),
    ));
    assert_eq!(outcome.code, "agent.unsupported");
    let message = outcome.message;
    assert!(message.contains("`SET lock_timeout = '5s'`"), "{message}");
    assert!(message.contains("additive DDL such as ALTER TABLE ... ADD COLUMN"), "{message}");
    assert!(message.contains("lock_timeout_secs"), "{message}");

    // A script: every such statement at once, the ALTER is not blamed.
    let outcome = failure(rig.layer.call(
        "execute_batch",
        batch("SET lock_timeout = '5s'; ALTER TABLE t ADD COLUMN c text; SET statement_timeout = 0", json!({})),
    ));
    let message = outcome.message;
    assert!(message.contains("statement 1 `SET lock_timeout = '5s'`"), "{message}");
    assert!(message.contains("statement 3 `SET statement_timeout = 0`"), "{message}");
    assert!(!message.contains("statement 2"), "{message}");
    assert!(message.contains("are allowed at access `read_write`"), "{message}");
    assert!(ran(&rig.session).is_empty());
    assert!(rig.approver.asked().is_empty());

    // At `read` the message does not promise DDL.
    let rig = self::rig(ConnectionKind::PostgreSql, AgentAccess::Read);
    let outcome = failure(rig.layer.call("execute_query", query("SET x = 1", json!({}))));
    assert!(!outcome.message.contains("ALTER TABLE"), "{}", outcome.message);
}

#[test]
fn a_server_error_at_commit_is_returned_as_it_is_and_the_session_keeps_working() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    script(
        &rig.session,
        Some((
            "COMMIT",
            AgentError::driver("query.failed", "ERROR: insert or update on table \"c\" violates foreign key constraint \"c_p\"", false),
        )),
    );
    let outcome = failure(rig.layer.call(
        "execute_query",
        query("INSERT INTO c VALUES (1)", json!({"lock_timeout_secs": 2})),
    ));
    assert_eq!(outcome.code, "query.failed");
    assert!(outcome.message.contains("violates foreign key"), "{}", outcome.message);
    assert!(rig.layer.is_open(ID), "a refused COMMIT is not a lost session");
    assert_eq!(rig.session.disconnects.load(Ordering::SeqCst), 0);
    assert_eq!(rig.provider.closed.load(Ordering::SeqCst), 0);
    // The next call works on the same session.
    script(&rig.session, None);
    rig.session.runs.lock().unwrap().clear();
    let out = rig.layer.call("execute_query", query("SELECT 1", json!({"lock_timeout_secs": 2})));
    assert!(!out.is_error(), "{out:?}");
    assert_eq!(ran(&rig.session).len(), 3);
}

#[test]
fn a_statement_refused_in_a_transaction_block_is_retried_on_the_session_path() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    let statement = "REINDEX (VERBOSE) SCHEMA app";
    let refused = std::sync::atomic::AtomicBool::new(false);
    rig.session.set_behavior(Behavior::Script(Box::new(move |request, _| {
        if request.text == statement && !refused.swap(true, Ordering::SeqCst) {
            return Err(AgentError::driver(
                "query.failed",
                "ERROR: REINDEX SCHEMA cannot run inside a transaction block",
                false,
            ));
        }
        Ok(RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(0)))
    })));
    let out = rig.layer.call("execute_query", query(statement, json!({"lock_timeout_secs": 5})));
    assert!(!out.is_error(), "{out:?}");
    assert_eq!(
        ran(&rig.session),
        [
            "BEGIN; SET LOCAL lock_timeout = '5s'",
            statement,
            "ROLLBACK",
            "SET lock_timeout = '5s'",
            statement,
            "RESET lock_timeout",
        ]
    );
    // Approved once, not once per try.
    assert_eq!(rig.approver.asked().len(), 1);
}

#[test]
fn a_panic_between_the_set_and_the_reset_closes_the_session_through_the_layer() {
    let rig = rig(ConnectionKind::PostgreSql, AgentAccess::ReadWrite);
    rig.session.set_behavior(Behavior::Script(Box::new(|request, _| {
        if request.text == ALTER {
            panic!("driver bug");
        }
        Ok(RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(0)))
    })));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.layer
            .call("execute_query", query(ALTER, json!({"lock_timeout_secs": 5})))
    }));
    assert!(caught.is_err());
    assert!(!rig.layer.is_open(ID), "the slot left the map");
    assert_eq!(rig.provider.closed.load(Ordering::SeqCst), 1, "the provider was told");
    assert_eq!(rig.session.disconnects.load(Ordering::SeqCst), 1);
    // Nothing went to the driver after the panic (no ROLLBACK, no RESET).
    assert_eq!(ran(&rig.session), ["BEGIN; SET LOCAL lock_timeout = '5s'", ALTER]);
}
