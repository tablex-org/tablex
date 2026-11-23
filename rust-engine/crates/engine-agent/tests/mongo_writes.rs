//! `mongo_insert`, `mongo_update` and `mongo_delete` against a fake server: the access they need,
//! the question put to the user, what is (not) sent, the commands built and the reply read.

mod common;

use bson::{doc, Bson};
use common::recorder::Recorder;
use engine_agent::testing::{connection, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{
    read_entries, AgentAccess, AgentError, ApprovalDecision, AuditConfig, AuditLog, CancelToken,
    ConnectionKind, ConnectionTypeInfo, Language, ToolLayer, ToolName, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const MONGO: &str = "mongo1";
const SQL: &str = "sql1";

struct Rig {
    layer: ToolLayer,
    mongo: Arc<FakeSession>,
    catalog: Arc<FakeCatalog>,
    approver: Arc<Recorder>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(access: Option<AgentAccess>, approver: Recorder) -> Rig {
    rig_typed(access, approver, ConnectionTypeInfo::default())
}

fn rig_typed(
    access: Option<AgentAccess>,
    approver: Recorder,
    connection_type: ConnectionTypeInfo,
) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let mongo = FakeSession::new(ConnectionKind::MongoDb).arc();
    let provider = FakeProvider::new();
    provider.with_session(MONGO, Arc::clone(&mongo));
    provider.with_session(SQL, FakeSession::new(ConnectionKind::PostgreSql).arc());
    let mut mongo_info = connection(MONGO, ConnectionKind::MongoDb, access);
    mongo_info.connection_type = connection_type;
    let catalog = FakeCatalog::new(vec![
        mongo_info,
        connection(SQL, ConnectionKind::PostgreSql, access),
    ]);
    let approver = approver.arc();
    let layer = ToolLayer::new(catalog.clone(), provider, "test")
        .with_audit(Arc::clone(&audit))
        .with_approver(approver.clone());
    let rig = Rig {
        layer,
        mongo,
        catalog,
        approver,
        audit,
        audit_path,
        _dir: dir,
    };
    // The server acknowledges whatever is written.
    rig.reply(|command| {
        let name = command.as_object().unwrap().keys().next().unwrap().as_str();
        let reply = match name {
            "insert" => doc! { "n": command["documents"].as_array().unwrap().len() as i32, "ok": 1.0 },
            "update" => doc! { "n": 1, "nModified": 1, "ok": 1.0 },
            "delete" => doc! { "n": 1, "ok": 1.0 },
            "count" => doc! { "n": 7, "ok": 1.0 },
            other => panic!("unexpected command {other}"),
        };
        Bson::Document(reply).into_canonical_extjson()
    });
    rig
}

fn approving() -> Recorder {
    Recorder::new(ApprovalDecision::Approve)
}

impl Rig {
    fn reply(&self, answer: impl Fn(&Value) -> Value + Send + Sync + 'static) {
        self.mongo.set_mongo(move |_, command, _| Ok(answer(command)));
    }

    fn open(&self) {
        let opened = self.layer.call("open_session", json!({"connection_id": MONGO}));
        assert!(!opened.is_error(), "{opened:?}");
    }

    fn call(&self, tool: &str, args: Value) -> ToolOutcome {
        self.layer.call(tool, args)
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        match self.call(tool, args) {
            ToolOutcome::Success(value) => value,
            ToolOutcome::Failure(error) => panic!("{tool} failed: {error:?}"),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.mongo
            .mongo_calls()
            .iter()
            .map(|(_, command)| command.to_string())
            .collect()
    }

    fn sent(&self) -> usize {
        self.mongo.mongo_calls().len()
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn args(extra: Value) -> Value {
    let mut request = json!({"connection_id": MONGO, "database": "shop", "collection": "orders"});
    request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    request
}

fn insert() -> Value {
    args(json!({"documents": [{"sku": "a-1", "stock": 4}]}))
}

fn update() -> Value {
    args(json!({"filter": {"status": "open"}, "update": {"$set": {"status": "paid"}}}))
}

fn delete() -> Value {
    args(json!({"filter": {"status": "cancelled"}}))
}

fn writes() -> [(&'static str, Value); 3] {
    [
        ("mongo_insert", insert()),
        ("mongo_update", update()),
        ("mongo_delete", delete()),
    ]
}

// ------------------------------------------------------------------ access

#[test]
fn the_write_tools_need_read_write_access() {
    for tool in [ToolName::MongoInsert, ToolName::MongoUpdate, ToolName::MongoDelete] {
        assert_eq!(tool.required_access(), AgentAccess::ReadWrite);
        assert!(tool.is_mongo_write());
    }
    for (access, allowed) in [
        (None, false),
        (Some(AgentAccess::Off), false),
        (Some(AgentAccess::SchemaOnly), false),
        (Some(AgentAccess::Read), false),
        (Some(AgentAccess::ReadWrite), true),
    ] {
        for (tool, request) in writes() {
            let rig = rig(access, approving());
            if access.is_some_and(|access| access >= AgentAccess::SchemaOnly) {
                rig.open();
            }
            let outcome = rig.call(tool, request);
            if allowed {
                assert!(!outcome.is_error(), "{tool}: {outcome:?}");
                assert_eq!(rig.sent(), 1, "{tool}");
            } else {
                assert_eq!(code(&outcome), "agent.access_denied", "{tool} with {access:?}");
                assert!(
                    outcome.to_text().contains(&format!("`{tool}` needs `read_write`")),
                    "{}",
                    outcome.to_text()
                );
                assert_eq!(rig.sent(), 0, "{tool} with {access:?}");
                assert!(rig.approver.asked().is_empty(), "nobody is asked about a denied write");
            }
        }
    }
}

#[test]
fn a_read_only_connection_type_caps_the_access_at_read() {
    let read_only = ConnectionTypeInfo {
        name: "Read-only".into(),
        read_only: true,
        ..ConnectionTypeInfo::default()
    };
    for (tool, request) in writes() {
        let rig = rig_typed(Some(AgentAccess::ReadWrite), approving(), read_only.clone());
        rig.open();
        assert_eq!(code(&rig.call(tool, request)), "agent.access_denied", "{tool}");
        assert_eq!(rig.sent(), 0);
        assert!(rig.approver.asked().is_empty());
    }
}

#[test]
fn lowering_the_access_stops_the_writes_at_once() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.ok("mongo_delete", delete());
    rig.catalog
        .update(MONGO, |info| info.agent_access = Some(AgentAccess::Read));
    assert_eq!(code(&rig.call("mongo_delete", delete())), "agent.access_denied");
    assert_eq!(rig.sent(), 1);
}

#[test]
fn a_production_connection_is_refused_by_a_front_end_that_says_so_and_nothing_is_sent() {
    let production = ConnectionTypeInfo {
        name: "Production".into(),
        production: true,
        ..ConnectionTypeInfo::default()
    };
    for (tool, request) in writes() {
        let mut approver = approving();
        approver.refuse_production = true;
        let rig = rig_typed(Some(AgentAccess::ReadWrite), approver, production.clone());
        rig.open();
        let outcome = rig.call(tool, request);
        assert_eq!(code(&outcome), "agent.write_refused", "{tool}");
        assert!(outcome.to_text().contains("the user was not asked"), "{}", outcome.to_text());
        assert_eq!(rig.sent(), 0, "{tool}");
        // The layer told the approver it is a production connection.
        assert!(rig.approver.asked()[0].production);
    }
}

#[test]
fn a_switch_that_is_off_refuses_before_and_after_the_answer_and_nothing_is_sent() {
    // Off when asked: the approver refuses (nobody is asked).
    let refusing = Recorder::new(ApprovalDecision::Refuse {
        reason: "changes through the MCP server are turned off".into(),
    });
    let rig = rig(Some(AgentAccess::ReadWrite), refusing);
    rig.open();
    let outcome = rig.call("mongo_update", update());
    assert_eq!(code(&outcome), "agent.write_refused");
    assert!(outcome.to_text().contains("turned off"), "{}", outcome.to_text());
    assert_eq!(rig.sent(), 0);

    // Turned off while the question was open: the answer is yes, `still_allowed` says no.
    let rig = self::rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    let approver = Arc::clone(&rig.approver);
    *rig.approver.meanwhile.lock().unwrap() = Some(Box::new(move || {
        *approver.still_allowed.lock().unwrap() = Err("the switch was turned off".into());
    }));
    let outcome = rig.call("mongo_insert", insert());
    assert_eq!(code(&outcome), "agent.write_refused");
    let text = outcome.to_text();
    assert!(text.contains("approved, but it can no longer run"), "{text}");
    assert!(text.contains("the switch was turned off"), "{text}");
    assert_eq!(rig.sent(), 0, "an approval that no longer holds sends nothing");
    rig.audit.flush();
    let entries = read_entries(&rig.audit_path, 100).unwrap().entries;
    let entry = entries.iter().find(|entry| entry.tool == "mongo_insert").unwrap();
    assert_eq!(
        entry.approval.as_deref(),
        Some("approved, then refused: the switch was turned off")
    );
    assert!(entries.iter().all(|entry| entry.outcome != "started"));
}

#[test]
fn a_declined_or_unanswered_question_sends_nothing() {
    for (decision, expected, text) in [
        (
            ApprovalDecision::Deny { reason: Some("not now".into()) },
            "agent.write_refused",
            "did not approve this MongoDB command: not now",
        ),
        (
            ApprovalDecision::Deny { reason: None },
            "agent.write_refused",
            "did not approve",
        ),
        (
            ApprovalDecision::Unanswered { reason: "the client failed".into() },
            "agent.write_refused",
            "the client failed",
        ),
    ] {
        for (tool, request) in writes() {
            let rig = rig(Some(AgentAccess::ReadWrite), Recorder::new(decision.clone()));
            rig.open();
            let outcome = rig.call(tool, request);
            assert_eq!(code(&outcome), expected, "{tool}");
            assert!(outcome.to_text().contains(text), "{tool}: {}", outcome.to_text());
            assert_eq!(rig.sent(), 0, "{tool}");
            assert_eq!(rig.approver.asked().len(), 1);
        }
    }
}

#[test]
fn an_answer_that_arrives_after_the_access_was_lowered_sends_nothing() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    let catalog = Arc::clone(&rig.catalog);
    *rig.approver.meanwhile.lock().unwrap() = Some(Box::new(move || {
        catalog.update(MONGO, |info| info.agent_access = Some(AgentAccess::Read));
    }));
    let outcome = rig.call("mongo_delete", delete());
    assert_eq!(code(&outcome), "agent.access_denied", "{outcome:?}");
    assert_eq!(rig.sent(), 0);
}

#[test]
fn a_call_cancelled_while_the_question_is_open_sends_nothing() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    let token = CancelToken::new();
    let hook = token.clone();
    *rig.approver.meanwhile.lock().unwrap() = Some(Box::new(move || hook.cancel_detached()));
    let outcome = rig.layer.call_cancellable("mongo_insert", insert(), &token);
    assert_eq!(code(&outcome), "agent.cancelled", "{outcome:?}");
    assert_eq!(rig.sent(), 0);
}

// ------------------------------------------------------------------ the question

#[test]
fn the_user_sees_the_database_the_collection_and_the_exact_command() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.ok(
        "mongo_update",
        args(json!({
            "filter": {"_id": {"$oid": "65a1b2c3d4e5f60718293a4b"}},
            "update": {"$set": {"paid_at": {"$date": "2024-01-31T00:00:00Z"}}},
        })),
    );
    let asked = rig.approver.asked();
    assert_eq!(asked.len(), 1);
    let question = &asked[0];
    assert_eq!(question.language, Language::Mongo);
    assert_eq!(question.database.as_deref(), Some("shop"));
    assert_eq!(question.collection.as_deref(), Some("orders"));
    assert_eq!(question.script, None);
    assert_eq!(question.transaction, None);
    assert_eq!(question.dangerous, None);
    assert!(!question.production);
    // The text is readable (several lines) and is exactly the command that was sent.
    assert!(question.statement.contains('\n'), "{}", question.statement);
    let shown: Value = serde_json::from_str(&question.statement).unwrap();
    assert_eq!(shown, rig.mongo.mongo_calls()[0].1);
    assert_eq!(shown["update"], "orders");
    assert_eq!(shown["updates"][0]["u"]["$set"]["paid_at"], json!({"$date": "2024-01-31T00:00:00Z"}));
}

#[test]
fn every_write_is_asked_about_and_a_read_is_not() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    for (tool, request) in writes() {
        rig.ok(tool, request);
    }
    assert_eq!(rig.approver.asked().len(), 3);
    rig.ok("mongo_count", args(json!({})));
    assert_eq!(rig.approver.asked().len(), 3, "a count asks nothing");
}

// ------------------------------------------------------------------ commands

#[test]
fn the_commands_are_built_in_the_shape_the_server_takes() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.ok(
        "mongo_insert",
        args(json!({"documents": [{"sku": "a-1", "n": 1}, {"sku": "b-2", "n": 2}]})),
    );
    rig.ok("mongo_update", update());
    rig.ok(
        "mongo_update",
        args(json!({
            "filter": {"status": "open"},
            "pipeline": [{"$set": {"total": {"$sum": "$lines"}}}],
            "many": true,
            "upsert": true,
        })),
    );
    rig.ok("mongo_delete", delete());
    rig.ok("mongo_delete", args(json!({"filter": {"status": "cancelled"}, "many": true})));
    assert_eq!(
        rig.commands(),
        [
            r#"{"insert":"orders","documents":[{"sku":"a-1","n":1},{"sku":"b-2","n":2}],"ordered":true}"#,
            r#"{"update":"orders","updates":[{"q":{"status":"open"},"u":{"$set":{"status":"paid"}},"multi":false,"upsert":false}],"ordered":true}"#,
            r#"{"update":"orders","updates":[{"q":{"status":"open"},"u":[{"$set":{"total":{"$sum":"$lines"}}}],"multi":true,"upsert":true}],"ordered":true}"#,
            r#"{"delete":"orders","deletes":[{"q":{"status":"cancelled"},"limit":1}],"ordered":true}"#,
            r#"{"delete":"orders","deletes":[{"q":{"status":"cancelled"},"limit":0}],"ordered":true}"#,
        ]
    );
    // Sent to the database named, through the write method of the session, with the statement
    // timeout as the server's time limit; a read goes through the read-only one.
    assert!(rig.mongo.mongo_calls().iter().all(|(database, _)| database == "shop"));
    assert!(rig.mongo.mongo_writes.lock().unwrap().iter().all(|write| *write));
    assert!(rig.mongo.mongo_max_times.lock().unwrap().iter().all(|time| time.as_secs() == 30));
    rig.ok("mongo_count", args(json!({})));
    assert_eq!(rig.mongo.mongo_writes.lock().unwrap().last(), Some(&false));
}

#[test]
fn an_empty_filter_is_refused_unless_allow_all_and_then_it_is_dangerous() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    for (tool, mut request) in [("mongo_update", update()), ("mongo_delete", delete())] {
        request["filter"] = json!({});
        let outcome = rig.call(tool, request.clone());
        assert_eq!(code(&outcome), "agent.invalid_arguments", "{tool}");
        assert!(outcome.to_text().contains("allow_all"), "{}", outcome.to_text());
        // `allow_all` false is no allowance.
        request["allow_all"] = json!(false);
        assert_eq!(code(&rig.call(tool, request.clone())), "agent.invalid_arguments");
        assert_eq!(rig.sent(), 0);
        assert!(rig.approver.asked().is_empty(), "nobody is asked about a mistake");

        // With `allow_all` the front end that never runs dangerous statements refuses it ...
        request["allow_all"] = json!(true);
        let outcome = rig.call(tool, request);
        assert_eq!(code(&outcome), "agent.dangerous_refused", "{tool}");
        assert!(outcome.to_text().contains("does not run dangerous"), "{}", outcome.to_text());
        assert_eq!(rig.sent(), 0);
        assert!(rig.approver.asked().is_empty());
    }

    // ... one that asks the user about them (the app) puts it to them with the reason.
    let mut app = approving();
    app.allow_dangerous = true;
    let rig = self::rig(Some(AgentAccess::ReadWrite), app);
    rig.open();
    rig.ok("mongo_delete", args(json!({"filter": {}, "many": true, "allow_all": true})));
    let asked = rig.approver.asked();
    assert!(asked[0].dangerous.as_deref().unwrap().contains("every document"), "{asked:?}");
    assert_eq!(
        rig.commands(),
        [r#"{"delete":"orders","deletes":[{"q":{},"limit":0}],"ordered":true}"#]
    );
    // A non-empty filter with `allow_all` is an ordinary write.
    rig.ok("mongo_delete", args(json!({"filter": {"a": 1}, "allow_all": true})));
    assert_eq!(rig.approver.asked()[1].dangerous, None);
}

#[test]
fn bad_requests_are_refused_before_anyone_is_asked() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    let too_many: Vec<Value> = (0..1001).map(|n| json!({"n": n})).collect();
    let cases = [
        ("mongo_insert", args(json!({"documents": []}))),
        ("mongo_insert", args(json!({"documents": too_many}))),
        ("mongo_insert", args(json!({"documents": [1]}))),
        ("mongo_insert", args(json!({"documents": "x"}))),
        ("mongo_insert", args(json!({}))),
        ("mongo_update", args(json!({"filter": {"a": 1}}))),
        ("mongo_update", args(json!({"filter": {"a": 1}, "update": {"$set": {"a": 2}}, "pipeline": [{"$set": {"a": 2}}]}))),
        ("mongo_update", args(json!({"filter": {"a": 1}, "update": {}}))),
        ("mongo_update", args(json!({"filter": {"a": 1}, "update": {"a": 2}}))),
        ("mongo_update", args(json!({"filter": {"a": 1}, "pipeline": []}))),
        ("mongo_update", args(json!({"update": {"$set": {"a": 2}}}))),
        ("mongo_update", args(json!({"filter": "x", "update": {"$set": {"a": 2}}}))),
        ("mongo_update", args(json!({"filter": {"a": 1}, "update": {"$set": {"a": 2}}, "many": "yes"}))),
        ("mongo_delete", args(json!({}))),
        ("mongo_delete", args(json!({"filter": [1]}))),
        ("mongo_delete", args(json!({"filter": {"a": 1}, "surprise": 1}))),
        ("mongo_delete", args(json!({"filter": {"a": 1}, "collection": ""}))),
        ("mongo_delete", args(json!({"filter": {"a": 1}, "database": ""}))),
    ];
    for (tool, request) in cases {
        let outcome = rig.call(tool, request.clone());
        assert_eq!(code(&outcome), "agent.invalid_arguments", "{tool} {request}: {outcome:?}");
    }
    assert_eq!(rig.sent(), 0);
    assert!(rig.approver.asked().is_empty());
    // Exactly 1000 documents is fine.
    let thousand: Vec<Value> = (0..1000).map(|n| json!({"n": n})).collect();
    let outcome = rig.call("mongo_insert", args(json!({"documents": thousand})));
    assert!(!outcome.is_error(), "{outcome:?}");
}

#[test]
fn a_transaction_id_is_refused_for_mongodb_with_a_clear_message() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    for (tool, request) in writes() {
        let mut with_id = request;
        with_id["transaction_id"] = json!("tx-1");
        let outcome = rig.call(tool, with_id);
        assert_eq!(code(&outcome), "agent.unsupported", "{tool}");
        let text = outcome.to_text();
        assert!(text.contains("transaction_id") && text.contains("PostgreSQL and SQLite"), "{text}");
    }
    assert_eq!(rig.sent(), 0);
    assert!(rig.approver.asked().is_empty());
}

#[test]
fn the_tools_are_for_mongodb_connections() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.ok("open_session", json!({"connection_id": SQL}));
    for (tool, mut request) in writes() {
        request["connection_id"] = json!(SQL);
        let outcome = rig.call(tool, request);
        assert_eq!(code(&outcome), "agent.unsupported", "{tool}");
        assert!(outcome.to_text().contains("use execute_query"), "{}", outcome.to_text());
    }
    assert!(rig.approver.asked().is_empty());
}

// ------------------------------------------------------------------ the reply

#[test]
fn the_reply_reports_n_modified_upserted_and_write_errors() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.reply(|_| {
        Bson::Document(doc! {
            "n": 3_i64,
            "nModified": 2,
            "upserted": [{ "index": 0, "_id": bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap() }],
            "writeErrors": [{ "index": 1, "code": 11000, "errmsg": "duplicate key" }],
            "ok": 1.0,
        })
        .into_canonical_extjson()
    });
    let result = rig.ok("mongo_update", update());
    assert_eq!(result["n"], 3);
    assert_eq!(result["n_modified"], 2);
    assert_eq!(result["upserted"], json!([{"index": 0, "_id": {"$oid": "65a1b2c3d4e5f60718293a4b"}}]));
    assert_eq!(result["write_errors"], json!([{"index": 1, "code": 11000, "errmsg": "duplicate key"}]));
    assert!(result["elapsed_ms"].is_u64());

    // A reply without them keeps them out.
    rig.reply(|_| Bson::Document(doc! { "n": 2, "ok": 1.0 }).into_canonical_extjson());
    let result = rig.ok("mongo_insert", insert());
    assert_eq!(result["n"], 2);
    for key in ["n_modified", "upserted", "write_errors"] {
        assert!(result.get(key).is_none(), "{key}");
    }
    let result = rig.ok("mongo_delete", delete());
    assert_eq!(result["n"], 2);
}

#[test]
fn a_command_the_server_rejects_is_an_error_and_a_bad_reply_is_malformed() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.reply(|_| {
        Bson::Document(doc! { "ok": 0.0, "errmsg": "not authorized", "code": 13, "codeName": "Unauthorized" })
            .into_canonical_extjson()
    });
    let outcome = rig.call("mongo_delete", delete());
    assert_eq!(code(&outcome), "query.failed");
    assert!(outcome.to_text().contains("Unauthorized: not authorized"), "{}", outcome.to_text());
    rig.reply(|_| json!([1]));
    assert_eq!(code(&rig.call("mongo_delete", delete())), "query.malformed");
    rig.reply(|_| Bson::Document(doc! { "ok": 1.0 }).into_canonical_extjson());
    assert_eq!(code(&rig.call("mongo_delete", delete())), "query.malformed");
    rig.mongo.set_mongo(|_, _, _| Err(AgentError::driver("connection.lost", "gone", true)));
    assert_eq!(code(&rig.call("mongo_delete", delete())), "connection.lost");
}

#[test]
fn a_session_that_cannot_write_says_so() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    // No script at all: the fake answers `agent.unsupported`, as a driver that is too old does.
    *rig.mongo.mongo_script.lock().unwrap() = None;
    assert_eq!(code(&rig.call("mongo_insert", insert())), "agent.unsupported");
}

// ------------------------------------------------------------------ audit

#[test]
fn a_write_is_audited_before_it_is_sent_and_again_with_its_outcome() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    // The server sees the "started" line already on disk.
    let path = rig.audit_path.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let audit = Arc::clone(&rig.audit);
    rig.mongo.set_mongo(move |_, _, _| {
        audit.flush();
        log.lock().unwrap().extend(read_entries(&path, 100).unwrap().entries);
        Ok(Bson::Document(doc! { "n": 1, "ok": 1.0 }).into_canonical_extjson())
    });
    rig.ok("mongo_insert", insert());
    let before: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.tool == "mongo_insert")
        .cloned()
        .collect();
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!(before[0].tool, "mongo_insert");
    assert_eq!(before[0].outcome, "started");
    assert_eq!(before[0].approval.as_deref(), Some("approved"));
    rig.audit.flush();
    let entries = read_entries(&rig.audit_path, 100).unwrap().entries;
    let started = entries.iter().filter(|entry| entry.outcome == "started").count();
    assert_eq!(started, 1);
    let last = entries
        .iter()
        .find(|entry| entry.tool == "mongo_insert" && entry.outcome == "ok")
        .unwrap();
    assert_eq!(last.rows, Some(1));
    assert_eq!(last.approval.as_deref(), Some("approved"));
    assert!(
        last.statement.as_deref().unwrap().starts_with(r#"shop {"insert":"orders""#),
        "{:?}",
        last.statement
    );
}

#[cfg(unix)]
#[test]
fn a_write_that_cannot_be_audited_is_not_sent() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.ok("mongo_insert", insert());
    // Let the writer finish the records of that write first, then make the path unusable.
    rig.audit.flush();
    std::fs::remove_file(&rig.audit_path).unwrap();
    std::fs::create_dir(&rig.audit_path).unwrap();
    let sent = rig.sent();
    // The first refusal comes from the write-ahead record; once the log is known to be failing,
    // the next ones are refused before the user is even asked.
    for _ in 0..2 {
        let outcome = rig.call("mongo_insert", insert());
        assert_eq!(code(&outcome), "agent.audit_unavailable", "{outcome:?}");
    }
    assert_eq!(rig.sent(), sent, "the writes never reached the server");
    // Reads proceed.
    assert!(!rig.call("mongo_count", args(json!({}))).is_error());
}
