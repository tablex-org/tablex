//! `execute_batch` against fake sessions: the script is split and judged statement by statement,
//! the user is asked once, statements run in order with the limits of `execute_query`, and a
//! script in a transaction is committed or rolled back as a whole.

mod common;

use common::recorder::Recorder;
use engine_agent::limits::RowCollector;
use engine_agent::result::{QueryResult, ResultColumn};
use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{
    read_entries, AgentAccess, AgentError, ApprovalDecision, AuditConfig, AuditLog, ConnectionInfo,
    ConnectionKind, ConnectionTypeInfo, Limits, ToolLayer, ToolOutcome, TxControl,
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

const PG: &str = "pg1";

struct Rig {
    layer: ToolLayer,
    catalog: Arc<FakeCatalog>,
    provider: Arc<FakeProvider>,
    approver: Arc<Recorder>,
    /// The connection's shared session, and the one a transaction gets.
    shared: Arc<FakeSession>,
    own: Arc<FakeSession>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

struct Options {
    kind: ConnectionKind,
    access: Option<AgentAccess>,
    connection_type: ConnectionTypeInfo,
    transactions: bool,
    limits: Limits,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            kind: ConnectionKind::PostgreSql,
            access: Some(AgentAccess::ReadWrite),
            connection_type: ConnectionTypeInfo::default(),
            transactions: true,
            limits: Limits::default(),
        }
    }
}

/// What the fake database does: a statement with `FAIL` in it fails, a `SELECT` returns the
/// numbers 1 to 3, anything else changes two rows.
fn database(request: &engine_agent::ports::RunRequest<'_>) -> Result<QueryResult, AgentError> {
    if request.text.contains("FAIL") {
        return Err(AgentError::driver("query.failed", format!("boom in `{}`", request.text), false));
    }
    if request.text.starts_with("SELECT") {
        let columns = vec![ResultColumn {
            name: "n".into(),
            data_type: "integer".into(),
        }];
        let mut collector = RowCollector::new(columns, request.row_limit, request.limits);
        let wanted = request.text.trim_start_matches("SELECT ").parse::<usize>().unwrap_or(3);
        for n in 1..=wanted {
            if collector.push_row([n.to_string()]).is_break() {
                return Ok(collector.finish(true, None));
            }
        }
        return Ok(collector.finish(false, None));
    }
    Ok(RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(2)))
}

fn rig(options: Options, approver: Recorder) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let session = || {
        let session = FakeSession::new(options.kind).arc();
        session.set_behavior(Behavior::Script(Box::new(|request, _| database(request))));
        session
    };
    let (shared, own) = (session(), session());
    let provider = FakeProvider::new();
    provider.with_session(PG, Arc::clone(&shared));
    provider.with_extra_session(PG, Arc::clone(&own));
    let mut info: ConnectionInfo = connection(PG, options.kind, options.access);
    info.connection_type = options.connection_type;
    let catalog = FakeCatalog::new(vec![info]);
    let approver = approver.arc();
    let mut layer = ToolLayer::new(catalog.clone(), provider.clone(), "test")
        .with_audit(Arc::clone(&audit))
        .with_approver(approver.clone())
        .with_limits(options.limits);
    if options.transactions {
        layer = layer.with_transactions();
    }
    Rig {
        layer,
        catalog,
        provider,
        approver,
        shared,
        own,
        audit,
        audit_path,
        _dir: dir,
    }
}

fn approving() -> Recorder {
    Recorder::new(ApprovalDecision::Approve)
}

impl Rig {
    fn open(&self) {
        let opened = self.layer.call("open_session", json!({"connection_id": PG}));
        assert!(!opened.is_error(), "{opened:?}");
    }

    fn call(&self, args: Value) -> ToolOutcome {
        self.layer.call("execute_batch", args)
    }

    fn ok(&self, args: Value) -> Value {
        match self.call(args) {
            ToolOutcome::Success(value) => value,
            ToolOutcome::Failure(error) => panic!("execute_batch failed: {error:?}"),
        }
    }

    /// The statements that reached `session`, in order.
    fn ran(session: &FakeSession) -> Vec<String> {
        session.runs.lock().unwrap().iter().map(|(text, _)| text.clone()).collect()
    }

    fn shared_ran(&self) -> Vec<String> {
        Self::ran(&self.shared)
    }

    fn own_ran(&self) -> Vec<String> {
        Self::ran(&self.own)
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn script(text: &str) -> Value {
    json!({"connection_id": PG, "script": text})
}

fn with(mut request: Value, extra: Value) -> Value {
    request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    request
}

// ------------------------------------------------------------------ running

#[test]
fn the_statements_run_in_order_with_a_result_each() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let result = rig.ok(script(
        "SELECT 3; INSERT INTO items (name) VALUES ('a;b'); -- note\nSELECT 2;",
    ));
    assert_eq!(
        rig.shared_ran(),
        ["SELECT 3", "INSERT INTO items (name) VALUES ('a;b')", "-- note\nSELECT 2"]
    );
    assert_eq!(result["statement_count"], 3);
    assert_eq!(result["ok"], true);
    assert_eq!(result["skipped"], 0);
    assert_eq!(result["changes"], 1);
    assert_eq!(result["affected_rows"], 2);
    assert!(result.get("transaction").is_none());
    let statements = result["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[0]["index"], 1);
    assert_eq!(statements[0]["statement"], "SELECT 3");
    assert_eq!(statements[0]["result"]["rows"], json!([["1"], ["2"], ["3"]]));
    assert_eq!(statements[1]["result"]["affected_rows"], 2);
    assert_eq!(statements[2]["index"], 3);
    assert!(statements.iter().all(|statement| statement["ok"] == true));
    // Reads run read-only, the write does not.
    assert_eq!(*rig.shared.read_only_runs.lock().unwrap(), [true, false, true]);
    // Nothing went to a session of its own.
    assert!(rig.own_ran().is_empty());
    assert!(rig.shared.control_calls().is_empty());
}

#[test]
fn a_script_that_only_reads_needs_read_access_and_asks_nobody() {
    let rig = rig(
        Options {
            access: Some(AgentAccess::Read),
            ..Options::default()
        },
        approving(),
    );
    rig.open();
    let result = rig.ok(script("SELECT 1; SELECT 2"));
    assert_eq!(result["statements"].as_array().unwrap().len(), 2);
    assert_eq!(result["changes"], 0);
    assert!(result.get("affected_rows").is_none());
    assert!(rig.approver.asked().is_empty());
}

#[test]
fn each_statement_has_the_row_limit_of_execute_query_and_all_share_the_size_limit() {
    let rig = rig(
        Options {
            limits: Limits {
                max_result_bytes: 100,
                ..Limits::default()
            },
            ..Options::default()
        },
        approving(),
    );
    rig.open();
    // 40 numbers are about 70 bytes; cut to 5 rows by `row_limit`.
    let result = rig.ok(with(script("SELECT 40; SELECT 40"), json!({"row_limit": 5})));
    let first = &result["statements"][0]["result"];
    assert_eq!(first["row_count"], 5);
    assert_eq!(first["row_limit"], 5);
    assert_eq!(first["truncated"], true);
    assert_eq!(first["truncation"], "row_limit");
    // Without a row limit the first statement (71 bytes) uses most of the script's 100, and the
    // second gets what is left, and at least its first row.
    let result = rig.ok(script("SELECT 40; SELECT 40"));
    let (first, second) = (&result["statements"][0]["result"], &result["statements"][1]["result"]);
    assert_eq!(first["row_count"], 40);
    assert_eq!(first["truncated"], false);
    assert_eq!(second["truncation"], "byte_limit");
    assert!((1..40).contains(&second["row_count"].as_u64().unwrap()), "{second}");
}

#[test]
fn it_stops_at_the_first_failure_unless_told_to_go_on() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let result = rig.ok(script("SELECT 1; INSERT FAIL; INSERT INTO a VALUES (1); SELECT 2"));
    assert_eq!(rig.shared_ran(), ["SELECT 1", "INSERT FAIL"]);
    assert_eq!(result["ok"], false);
    assert_eq!(result["statement_count"], 4);
    assert_eq!(result["skipped"], 2);
    let statements = result["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 2);
    assert_eq!(statements[1]["index"], 2);
    assert_eq!(statements[1]["ok"], false);
    assert_eq!(statements[1]["error"]["code"], "query.failed");
    assert!(statements[1]["error"]["message"].as_str().unwrap().contains("boom"));
    assert!(statements[1].get("result").is_none());

    let rig = self::rig(Options::default(), approving());
    rig.open();
    let result = rig.ok(with(
        script("SELECT 1; INSERT FAIL; INSERT INTO a VALUES (1); SELECT 2"),
        json!({"continue_on_error": true}),
    ));
    assert_eq!(
        rig.shared_ran(),
        ["SELECT 1", "INSERT FAIL", "INSERT INTO a VALUES (1)", "SELECT 2"]
    );
    assert_eq!(result["ok"], false);
    assert_eq!(result["skipped"], 0);
    let ok: Vec<bool> = result["statements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|statement| statement["ok"].as_bool().unwrap())
        .collect();
    assert_eq!(ok, [true, false, true, true]);
    assert_eq!(result["changes"], 1, "the failed write did not take effect");
}

#[test]
fn an_error_message_has_the_connections_secrets_masked() {
    let mut session = FakeSession::new(ConnectionKind::PostgreSql);
    session.secrets = vec![engine_agent::SecretText::new("hunter2-secret")];
    session.set_behavior(Behavior::Fail(AgentError::driver(
        "query.failed",
        "auth failed for hunter2-secret",
        false,
    )));
    let session = session.arc();
    let provider = FakeProvider::new();
    provider.with_session(PG, session);
    let catalog = FakeCatalog::new(vec![connection(PG, ConnectionKind::PostgreSql, Some(AgentAccess::Read))]);
    let layer = ToolLayer::new(catalog, provider, "test");
    layer.call("open_session", json!({"connection_id": PG}));
    let ToolOutcome::Success(result) = layer.call("execute_batch", script("SELECT 1")) else {
        panic!("a failed statement is a result")
    };
    let message = result["statements"][0]["error"]["message"].as_str().unwrap();
    assert!(!message.contains("hunter2-secret"), "{message}");
}

// ------------------------------------------------------------------ the one question

#[test]
fn one_question_lists_every_write_and_none_of_the_reads() {
    let rig = rig(Options::default(), approving());
    rig.open();
    rig.ok(script(
        "SELECT 1; INSERT INTO items (name) VALUES ('a'); UPDATE orders SET status = 'paid' WHERE id = 1; \
         SELECT 2; CREATE TABLE audit_copy (id int)",
    ));
    let asked = rig.approver.asked();
    assert_eq!(asked.len(), 1, "one question for the whole script");
    assert_eq!(asked[0].script, Some(3));
    assert_eq!(asked[0].language, engine_agent::Language::Sql);
    assert_eq!(
        asked[0].statement,
        "INSERT INTO items (name) VALUES ('a');\n\n\
         UPDATE orders SET status = 'paid' WHERE id = 1;\n\n\
         CREATE TABLE audit_copy (id int);"
    );
    assert!(asked[0].dangerous.is_none());
    assert!(asked[0].transaction.is_none());
    assert_eq!(rig.shared_ran().len(), 5);
}

#[test]
fn nothing_runs_before_the_answer_and_a_no_runs_nothing() {
    for decision in [
        ApprovalDecision::Deny { reason: Some("no".into()) },
        ApprovalDecision::Deny { reason: None },
        ApprovalDecision::Unanswered { reason: "no answer".into() },
        ApprovalDecision::Refuse { reason: "this front end cannot ask".into() },
    ] {
        let rig = rig(Options::default(), Recorder::new(decision));
        rig.open();
        // The first statement is a read: it must not run ahead of the question either.
        let outcome = rig.call(script("SELECT 1; INSERT INTO a VALUES (1); SELECT 2"));
        assert_eq!(code(&outcome), "agent.write_refused", "{outcome:?}");
        assert!(outcome.to_text().contains("script"), "{}", outcome.to_text());
        assert!(rig.shared_ran().is_empty(), "nothing ran");
        assert_eq!(rig.approver.asked().len(), 1);
    }
}

#[test]
fn a_production_connection_a_switch_that_is_off_and_a_lowered_access_stop_the_script() {
    // Production, refused without asking.
    let mut approver = approving();
    approver.refuse_production = true;
    let rig = rig(
        Options {
            connection_type: ConnectionTypeInfo {
                name: "Production".into(),
                production: true,
                ..ConnectionTypeInfo::default()
            },
            ..Options::default()
        },
        approver,
    );
    rig.open();
    let outcome = rig.call(script("SELECT 1; INSERT INTO a VALUES (1)"));
    assert_eq!(code(&outcome), "agent.write_refused", "{outcome:?}");
    assert!(rig.shared_ran().is_empty());
    assert!(rig.approver.asked()[0].production);

    // The switch is turned off while the question is open.
    let rig = self::rig(Options::default(), approving());
    rig.open();
    let approver = Arc::clone(&rig.approver);
    *rig.approver.meanwhile.lock().unwrap() = Some(Box::new(move || {
        *approver.still_allowed.lock().unwrap() = Err("the switch was turned off".into());
    }));
    let outcome = rig.call(script("SELECT 1; INSERT INTO a VALUES (1)"));
    assert_eq!(code(&outcome), "agent.write_refused", "{outcome:?}");
    assert!(outcome.to_text().contains("can no longer run"), "{}", outcome.to_text());
    assert!(rig.shared_ran().is_empty());

    // The access is lowered while the question is open.
    let rig = self::rig(Options::default(), approving());
    rig.open();
    let catalog = Arc::clone(&rig.catalog);
    *rig.approver.meanwhile.lock().unwrap() = Some(Box::new(move || {
        catalog.update(PG, |info| info.agent_access = Some(AgentAccess::Read));
    }));
    let outcome = rig.call(script("INSERT INTO a VALUES (1)"));
    assert_eq!(code(&outcome), "agent.access_denied", "{outcome:?}");
    assert!(rig.shared_ran().is_empty());
}

#[test]
fn a_switch_turned_off_while_the_script_runs_stops_the_next_change() {
    let rig = rig(Options::default(), approving());
    rig.open();
    // The first change runs; the user turns the switch off meanwhile.
    let approver = Arc::clone(&rig.approver);
    rig.shared.set_behavior(Behavior::Script(Box::new(move |request, _| {
        if request.text.contains("first") {
            *approver.still_allowed.lock().unwrap() = Err("the switch was turned off".into());
        }
        database(request)
    })));
    let result = rig.ok(with(
        script("INSERT INTO a VALUES ('first'); SELECT 1; INSERT INTO a VALUES ('second'); SELECT 2"),
        // Even then a change that is no longer allowed ends the script.
        json!({"continue_on_error": true}),
    ));
    assert_eq!(
        rig.shared_ran(),
        ["INSERT INTO a VALUES ('first')", "SELECT 1"],
        "the second change never ran, nor what came after it"
    );
    assert_eq!(result["skipped"], 1);
    let statements = result["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[2]["ok"], false);
    assert_eq!(statements[2]["error"]["code"], "agent.write_refused");
    assert_eq!(result["changes"], 1);
}

// ------------------------------------------------------------------ refused as a whole

#[test]
fn a_statement_that_is_not_allowed_refuses_the_whole_script_before_anything_runs() {
    let rig = rig(Options::default(), approving());
    rig.open();
    for (text, expected, mention) in [
        // Dangerous: DROP, and a DELETE without WHERE.
        ("INSERT INTO a VALUES (1); DROP TABLE a", "agent.dangerous_refused", "statement 2"),
        ("SELECT 1; DELETE FROM a", "agent.dangerous_refused", "statement 2"),
        // Transaction and session state.
        ("INSERT INTO a VALUES (1); BEGIN; INSERT INTO a VALUES (2); COMMIT", "agent.unsupported", "statement 2"),
        ("SET search_path = x; SELECT 1", "agent.unsupported", "statement 1"),
        // A function that reaches outside the tables.
        ("SELECT 1; SELECT pg_read_file('/etc/passwd')", "agent.blocked_function", "pg_read_file"),
        // Nothing to run, and too much.
        ("", "agent.invalid_arguments", "empty"),
        ("-- only a comment", "agent.invalid_arguments", "empty"),
        (&"SELECT 1;".repeat(51), "agent.limit", "51 statements"),
    ] {
        let outcome = rig.call(script(text));
        assert_eq!(code(&outcome), expected, "{text}: {outcome:?}");
        assert!(outcome.to_text().contains(mention), "{text}: {}", outcome.to_text());
        assert!(rig.shared_ran().is_empty(), "{text}: nothing ran");
        assert!(rig.approver.asked().is_empty(), "{text}: nobody was asked");
    }
    // Fifty statements are fine.
    assert!(!rig.call(script(&"SELECT 1;".repeat(50))).is_error());
}

#[test]
fn a_dangerous_statement_is_refused_even_by_a_front_end_that_asks_about_them() {
    let mut app = approving();
    app.allow_dangerous = true;
    let rig = rig(Options::default(), app);
    rig.open();
    let outcome = rig.call(script("INSERT INTO a VALUES (1); TRUNCATE a"));
    assert_eq!(code(&outcome), "agent.dangerous_refused");
    assert!(outcome.to_text().contains("execute_query"), "{}", outcome.to_text());
    assert!(rig.shared_ran().is_empty());
    assert!(rig.approver.asked().is_empty());
}

#[test]
fn additive_ddl_in_a_script_is_an_ordinary_write_asked_once() {
    let rig = rig(Options::default(), approving());
    rig.open();
    rig.ok(script(
        "CREATE TABLE audit_copy (id int); ALTER TABLE items ADD COLUMN biz_domain text; \
         CREATE INDEX CONCURRENTLY i ON items (name); COMMENT ON TABLE items IS 'x'",
    ));
    let asked = rig.approver.asked();
    assert_eq!(asked.len(), 1, "one question for the whole script");
    assert_eq!(asked[0].script, Some(4));
    assert!(asked[0].dangerous.is_none());
    // The driver's own check asks to confirm the ALTER; the approval of the script covers it.
    let runs = rig.shared.runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 4);
    assert_eq!(
        runs.iter().map(|(_, confirmed)| *confirmed).collect::<Vec<_>>(),
        [false, true, false, false]
    );
    // A destructive statement still refuses the whole script.
    let outcome = rig.call(script("ALTER TABLE items ADD c int; ALTER TABLE items DROP COLUMN c"));
    assert_eq!(code(&outcome), "agent.dangerous_refused");
    assert!(outcome.to_text().contains("statement 2"), "{}", outcome.to_text());
    assert_eq!(rig.shared_ran().len(), 4, "nothing more ran");
    assert_eq!(rig.approver.asked().len(), 1);
}

#[test]
fn writes_need_read_write_and_a_read_only_type_caps_it() {
    for (access, connection_type) in [
        (Some(AgentAccess::Read), ConnectionTypeInfo::default()),
        (
            Some(AgentAccess::ReadWrite),
            ConnectionTypeInfo {
                name: "Read-only".into(),
                read_only: true,
                ..ConnectionTypeInfo::default()
            },
        ),
    ] {
        let rig = rig(
            Options {
                access,
                connection_type,
                ..Options::default()
            },
            approving(),
        );
        rig.open();
        let outcome = rig.call(script("SELECT 1; INSERT INTO a VALUES (1)"));
        assert_eq!(code(&outcome), "agent.write_refused", "{outcome:?}");
        assert!(outcome.to_text().contains("statement 2"), "{}", outcome.to_text());
        assert!(rig.shared_ran().is_empty());
        assert!(rig.approver.asked().is_empty());
    }
    // Below read, the tool is not for this connection at all.
    let rig = rig(
        Options {
            access: Some(AgentAccess::SchemaOnly),
            ..Options::default()
        },
        approving(),
    );
    rig.open();
    assert_eq!(code(&rig.call(script("SELECT 1"))), "agent.access_denied");
}

#[test]
fn a_script_is_for_sql_connections() {
    let mut session = FakeSession::new(ConnectionKind::MongoDb);
    session.databases = vec!["shop".into()];
    let provider = FakeProvider::new();
    provider.with_session("m", session.arc());
    let catalog = FakeCatalog::new(vec![connection("m", ConnectionKind::MongoDb, Some(AgentAccess::ReadWrite))]);
    let layer = ToolLayer::new(catalog, provider, "test");
    layer.call("open_session", json!({"connection_id": "m"}));
    let outcome = layer.call("execute_batch", json!({"connection_id": "m", "script": "SELECT 1"}));
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(outcome.to_text().contains("mongo_insert"), "{}", outcome.to_text());
}

#[test]
fn a_statement_that_is_a_script_in_itself_cannot_get_through() {
    // The split is the drivers' own; a piece is also classified alone and must be one statement.
    let rig = rig(Options::default(), approving());
    rig.open();
    let result = rig.ok(script("SELECT 'a;b'; SELECT \"c;d\" /* ; */"));
    assert_eq!(rig.shared_ran(), ["SELECT 'a;b'", "SELECT \"c;d\" /* ; */"]);
    assert_eq!(result["statement_count"], 2);
}

// ------------------------------------------------------------------ a transaction of its own

#[test]
fn use_transaction_begins_runs_everything_and_commits_with_the_one_approval() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let result = rig.ok(with(
        script("INSERT INTO a VALUES (1); SELECT 2; UPDATE a SET b = 2 WHERE id = 1"),
        json!({"use_transaction": true}),
    ));
    assert_eq!(result["ok"], true);
    assert_eq!(result["transaction"], "committed");
    assert_eq!(result["changes"], 2);
    assert_eq!(result["affected_rows"], 4);
    // The session of its own took BEGIN, the statements, COMMIT; the shared one saw nothing.
    assert_eq!(
        rig.own.control_calls(),
        [TxControl::Begin, TxControl::Commit]
    );
    assert_eq!(
        rig.own_ran(),
        ["INSERT INTO a VALUES (1)", "SELECT 2", "UPDATE a SET b = 2 WHERE id = 1"]
    );
    assert!(rig.shared_ran().is_empty());
    assert!(rig.shared.control_calls().is_empty());
    // One question, with both changes, and no question about the commit.
    let asked = rig.approver.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].script, Some(2));
    assert!(asked[0].transaction.is_none());
    assert!(asked[0].statement.contains("INSERT INTO a") && asked[0].statement.contains("UPDATE a"));
    // The transaction's session was closed after the commit.
    assert!(rig.own.disconnects.load(Ordering::SeqCst) >= 1);
    // Both changes were audited before they ran, and the commit.
    rig.audit.flush();
    let entries = read_entries(&rig.audit_path, 100).unwrap().entries;
    let started: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.outcome == "started")
        .map(|entry| entry.tool.as_str())
        .collect();
    assert_eq!(started.iter().filter(|tool| **tool == "execute_batch").count(), 2);
    assert_eq!(started.iter().filter(|tool| **tool == "commit_transaction").count(), 1);
    let end = entries.iter().find(|entry| entry.tool == "execute_batch" && entry.outcome == "ok").unwrap();
    assert_eq!(end.rows, Some(4));
}

#[test]
fn a_failure_in_a_transaction_rolls_everything_back_and_names_the_statement() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let result = rig.ok(with(
        script("INSERT INTO a VALUES (1); INSERT FAIL; INSERT INTO a VALUES (3)"),
        json!({"use_transaction": true}),
    ));
    assert_eq!(result["ok"], false);
    assert_eq!(result["transaction"], "rolled_back");
    assert_eq!(result["changes"], 0, "nothing took effect");
    assert!(result.get("affected_rows").is_none());
    assert_eq!(result["skipped"], 1);
    let statements = result["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 2);
    assert_eq!(statements[1]["index"], 2);
    assert_eq!(statements[1]["ok"], false);
    assert_eq!(statements[1]["error"]["code"], "query.failed");
    assert_eq!(rig.own_ran(), ["INSERT INTO a VALUES (1)", "INSERT FAIL"]);
    assert_eq!(rig.own.control_calls(), [TxControl::Begin, TxControl::Rollback]);
    assert!(rig.shared_ran().is_empty());
    assert!(rig.own.disconnects.load(Ordering::SeqCst) >= 1);
    // The transaction is gone: no id is left open for the model to trip over.
    let outcome = rig.layer.call("commit_transaction", json!({"transaction_id": "tx-1"}));
    assert_eq!(code(&outcome), "agent.transaction_gone");
}

#[test]
fn a_switch_turned_off_mid_script_rolls_a_transaction_back() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let approver = Arc::clone(&rig.approver);
    rig.own.set_behavior(Behavior::Script(Box::new(move |request, _| {
        if request.text.contains("first") {
            *approver.still_allowed.lock().unwrap() = Err("the switch was turned off".into());
        }
        database(request)
    })));
    let result = rig.ok(with(
        script("INSERT INTO a VALUES ('first'); INSERT INTO a VALUES ('second')"),
        json!({"use_transaction": true}),
    ));
    assert_eq!(result["transaction"], "rolled_back");
    assert_eq!(result["changes"], 0);
    assert_eq!(rig.own_ran(), ["INSERT INTO a VALUES ('first')"]);
    assert_eq!(rig.own.control_calls(), [TxControl::Begin, TxControl::Rollback]);
}

#[test]
fn use_transaction_is_refused_where_the_script_cannot_be_atomic() {
    let call = |kind: ConnectionKind, extra: Value, text: &str| {
        let rig = rig(
            Options {
                kind,
                ..Options::default()
            },
            approving(),
        );
        rig.open();
        let outcome = rig.call(with(script(text), extra));
        (outcome, rig)
    };
    let tx = json!({"use_transaction": true});
    // MySQL and MariaDB commit implicitly at DDL: a script that holds one is refused as a whole,
    // and one of plain changes is a transaction like any other.
    for kind in [ConnectionKind::MySql, ConnectionKind::MariaDb] {
        let (outcome, rig) = call(kind, tx.clone(), "INSERT INTO a VALUES (1); CREATE TABLE b (id int)");
        assert_eq!(code(&outcome), "agent.unsupported", "{kind:?}");
        assert!(outcome.to_text().contains("COMMIT") && outcome.to_text().contains("statement 2"), "{}", outcome.to_text());
        assert!(rig.shared_ran().is_empty() && rig.own_ran().is_empty());
        assert!(rig.approver.asked().is_empty(), "nobody is asked about what cannot run");
        let (outcome, rig) = call(kind, tx.clone(), "INSERT INTO a VALUES (1); UPDATE a SET b = 2 WHERE id = 1");
        assert_eq!(code(&outcome), "ok", "{kind:?}: {outcome:?}");
        assert_eq!(rig.own.control_calls(), [TxControl::Begin, TxControl::Commit]);
        assert_eq!(rig.approver.asked().len(), 1);
    }
    let (outcome, rig) = call(ConnectionKind::ClickHouse, tx.clone(), "INSERT INTO a VALUES (1)");
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(rig.approver.asked().is_empty());
    // A script that only reads has nothing to commit.
    let (outcome, rig) = call(ConnectionKind::PostgreSql, tx.clone(), "SELECT 1; SELECT 2");
    assert_eq!(code(&outcome), "agent.invalid_arguments");
    assert!(outcome.to_text().contains("only reads"), "{}", outcome.to_text());
    assert!(rig.shared_ran().is_empty());
    // The two ways to use a transaction exclude each other, and so does continue_on_error.
    let (outcome, _) = call(
        ConnectionKind::PostgreSql,
        json!({"use_transaction": true, "continue_on_error": true}),
        "INSERT INTO a VALUES (1)",
    );
    assert_eq!(code(&outcome), "agent.invalid_arguments");
    assert!(outcome.to_text().contains("continue_on_error"), "{}", outcome.to_text());
    let (outcome, _) = call(
        ConnectionKind::PostgreSql,
        json!({"use_transaction": true, "transaction_id": "tx-1"}),
        "INSERT INTO a VALUES (1)",
    );
    assert_eq!(code(&outcome), "agent.invalid_arguments");
    assert!(outcome.to_text().contains("cannot be combined"), "{}", outcome.to_text());
    // `use_transaction: false` is a script without one.
    let (outcome, rig) = call(
        ConnectionKind::MySql,
        json!({"use_transaction": false}),
        "INSERT INTO a VALUES (1)",
    );
    assert!(!outcome.is_error(), "{outcome:?}");
    assert_eq!(rig.shared_ran(), ["INSERT INTO a VALUES (1)"]);
}

#[test]
fn a_provider_that_cannot_give_a_second_session_refuses_use_transaction() {
    let rig = rig(Options::default(), approving());
    rig.open();
    *rig.provider.transactions_unavailable.lock().unwrap() =
        Some("this database lives in memory for one session only".into());
    let outcome = rig.call(with(script("INSERT INTO a VALUES (1)"), json!({"use_transaction": true})));
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(outcome.to_text().contains("in memory"), "{}", outcome.to_text());
    assert!(rig.shared_ran().is_empty());
}

#[test]
fn use_transaction_is_refused_on_a_front_end_without_transactions() {
    let rig = rig(
        Options {
            transactions: false,
            ..Options::default()
        },
        approving(),
    );
    rig.open();
    // The schema has no such property, so the request is refused as an invalid one.
    let outcome = rig.call(with(script("INSERT INTO a VALUES (1)"), json!({"use_transaction": true})));
    assert_eq!(code(&outcome), "agent.invalid_arguments");
    assert!(rig.shared_ran().is_empty());
}

// ------------------------------------------------------------------ an open transaction

#[test]
fn a_script_in_an_open_transaction_is_not_committed_and_its_changes_wait_for_the_commit_question() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let begun = rig.layer.call("begin_transaction", json!({"connection_id": PG}));
    let ToolOutcome::Success(begun) = begun else {
        panic!("{begun:?}")
    };
    let id = begun["transaction_id"].as_str().unwrap().to_owned();
    let result = rig.ok(with(
        script("INSERT INTO a VALUES (1); SELECT 1; UPDATE a SET b = 2 WHERE id = 1"),
        json!({"transaction_id": id}),
    ));
    assert_eq!(result["ok"], true);
    assert!(result.get("transaction").is_none(), "the model's transaction is still open");
    assert_eq!(rig.own.control_calls(), [TxControl::Begin]);
    assert_eq!(rig.own_ran().len(), 3);
    assert!(rig.shared_ran().is_empty());
    let asked = rig.approver.asked();
    assert_eq!(asked[0].script, Some(2));
    assert_eq!(asked[0].transaction.as_deref(), Some(id.as_str()));
    // The commit question lists what the script changed, then the commit goes through.
    let committed = rig.layer.call("commit_transaction", json!({"transaction_id": id}));
    assert!(!committed.is_error(), "{committed:?}");
    assert_eq!(rig.approver.asked().len(), 2);
    assert_eq!(rig.own.control_calls(), [TxControl::Begin, TxControl::Commit]);

    // A failing statement aborts the open transaction: only a rollback is left.
    let rig = self::rig(Options::default(), approving());
    rig.open();
    let ToolOutcome::Success(begun) = rig.layer.call("begin_transaction", json!({"connection_id": PG})) else {
        panic!()
    };
    let id = begun["transaction_id"].as_str().unwrap().to_owned();
    let result = rig.ok(with(
        script("INSERT INTO a VALUES (1); INSERT FAIL; INSERT INTO a VALUES (3)"),
        json!({"transaction_id": id}),
    ));
    assert_eq!(result["ok"], false);
    assert_eq!(result["skipped"], 1);
    assert_eq!(result["statements"][1]["error"]["code"], "query.failed");
    let again = rig.call(with(script("SELECT 1"), json!({"transaction_id": id})));
    assert_eq!(code(&again), "agent.transaction_aborted");
}

#[test]
fn a_transaction_id_is_checked_like_in_execute_query() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let outcome = rig.call(with(script("SELECT 1"), json!({"transaction_id": "tx-9"})));
    assert_eq!(code(&outcome), "agent.transaction_gone");
    let ToolOutcome::Success(begun) = rig.layer.call("begin_transaction", json!({"connection_id": PG})) else {
        panic!()
    };
    let id = begun["transaction_id"].as_str().unwrap().to_owned();
    let outcome = rig.call(with(
        script("SELECT 1"),
        json!({"transaction_id": id, "continue_on_error": true}),
    ));
    assert_eq!(code(&outcome), "agent.invalid_arguments");
    // COPY is not undone by a rollback.
    let outcome = rig.call(with(script("COPY a TO STDOUT"), json!({"transaction_id": id})));
    assert_eq!(code(&outcome), "agent.unsupported", "{outcome:?}");
    assert!(outcome.to_text().contains("COPY"), "{}", outcome.to_text());
    assert!(rig.own_ran().is_empty());
}

#[test]
fn a_script_that_would_make_the_commit_unreviewable_is_refused_up_front() {
    let rig = rig(Options::default(), approving());
    rig.open();
    let ToolOutcome::Success(begun) = rig.layer.call("begin_transaction", json!({"connection_id": PG})) else {
        panic!()
    };
    let id = begun["transaction_id"].as_str().unwrap().to_owned();
    // About 9000 bytes of changes: more than a commit question can show (8000).
    let many: String = (0..20)
        .map(|n| format!("INSERT INTO a VALUES ({n}, '{}');", "x".repeat(450)))
        .collect();
    let outcome = rig.call(with(script(&many), json!({"transaction_id": id})));
    assert_eq!(code(&outcome), "agent.limit", "{outcome:?}");
    assert!(outcome.to_text().contains("could not be shown"), "{}", outcome.to_text());
    assert!(rig.own_ran().is_empty());
    assert!(rig.approver.asked().is_empty());
}
