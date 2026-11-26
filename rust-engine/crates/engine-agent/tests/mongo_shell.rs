//! `execute_query` with a mongosh-style command on a MongoDB connection, and `describe_table` on
//! a collection, against a fake server: what each command needs (access, approval, the flag for a
//! dangerous one), what is sent, and how the reply is read.

mod common;

use bson::{doc, Bson, Document};
use common::recorder::Recorder;
use engine_agent::schema::{spec, validate};
use engine_agent::testing::{connection, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{
    read_entries, AgentAccess, AgentError, ApprovalDecision, AuditConfig, AuditLog, ConnectionKind,
    ConnectionTypeInfo, Language, ToolLayer, ToolName, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::Arc;

const MONGO: &str = "mongo1";

struct Rig {
    layer: ToolLayer,
    mongo: Arc<FakeSession>,
    approver: Arc<Recorder>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn batch(documents: Vec<Document>) -> Document {
    doc! { "cursor": { "id": 0_i64, "firstBatch": documents, "ns": "shop.orders" }, "ok": 1.0 }
}

/// What the server answers to each command the tests send.
fn server(command: &Value) -> Document {
    let name = command.as_object().unwrap().keys().next().unwrap().as_str();
    match name {
        "find" => batch(vec![doc! { "_id": 1, "status": "paid" }, doc! { "_id": 2, "status": "open" }]),
        "aggregate" => batch(vec![doc! { "n": 3 }]),
        "listIndexes" => batch(vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "email": 1 }, "name": "email_1", "unique": true },
            doc! { "v": 2, "key": { "status": 1, "created": -1 }, "name": "status_1_created_-1", "expireAfterSeconds": 60 },
        ]),
        "listCollections" => batch(vec![doc! { "name": "orders", "type": "collection" }, doc! { "name": "users", "type": "collection" }]),
        "listDatabases" => doc! { "databases": [{ "name": "admin" }, { "name": "shop" }], "ok": 1.0 },
        "buildInfo" => doc! { "version": "7.0.5", "gitVersion": "x", "ok": 1.0 },
        "count" => doc! { "n": 7, "ok": 1.0 },
        "distinct" => doc! { "values": ["paid", "open"], "ok": 1.0 },
        "collStats" => doc! { "ns": "shop.orders", "size": 4096, "storageSize": 8192, "totalIndexSize": 2048, "count": 2, "ok": 1.0, "$clusterTime": { "x": 1 } },
        "dbStats" => doc! { "db": "shop", "collections": 2, "ok": 1.0 },
        "explain" => doc! { "queryPlanner": { "winningPlan": { "stage": "COLLSCAN" } }, "ok": 1.0 },
        "insert" => doc! { "n": command["documents"].as_array().unwrap().len() as i32, "ok": 1.0 },
        "update" => doc! { "n": 2, "nModified": 1, "ok": 1.0 },
        "delete" => doc! { "n": 3, "ok": 1.0 },
        "findAndModify" => doc! { "lastErrorObject": { "n": 1, "updatedExisting": true }, "value": { "_id": 1 }, "ok": 1.0 },
        "createIndexes" => doc! { "createdCollectionAutomatically": false, "numIndexesBefore": 1, "numIndexesAfter": 2, "ok": 1.0 },
        "dropIndexes" => doc! { "nIndexesWas": 2, "ok": 1.0 },
        "drop" => doc! { "ns": "shop.orders", "nIndexesWas": 1, "ok": 1.0 },
        "renameCollection" => doc! { "ok": 1.0 },
        other => panic!("unexpected command {other}"),
    }
}

fn rig(access: Option<AgentAccess>, approver: Recorder) -> Rig {
    rig_typed(access, approver, ConnectionTypeInfo::default())
}

fn rig_typed(access: Option<AgentAccess>, approver: Recorder, connection_type: ConnectionTypeInfo) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let mongo = FakeSession::new(ConnectionKind::MongoDb).arc();
    mongo.set_mongo(|_, command, _| Ok(Bson::Document(server(command)).into_canonical_extjson()));
    let provider = FakeProvider::new();
    provider.with_session(MONGO, Arc::clone(&mongo));
    let mut info = connection(MONGO, ConnectionKind::MongoDb, access);
    info.connection_type = connection_type;
    let catalog = FakeCatalog::new(vec![info]);
    let approver = approver.arc();
    let layer = ToolLayer::new(catalog, provider, "test")
        .with_audit(Arc::clone(&audit))
        .with_approver(approver.clone());
    Rig {
        layer,
        mongo,
        approver,
        audit,
        audit_path,
        _dir: dir,
    }
}

fn approving() -> Recorder {
    Recorder::new(ApprovalDecision::Approve)
}

fn approving_dangerous() -> Recorder {
    Recorder {
        allow_dangerous: true,
        ..approving()
    }
}

impl Rig {
    fn open(&self) {
        let opened = self.layer.call("open_session", json!({"connection_id": MONGO}));
        assert!(!opened.is_error(), "{opened:?}");
    }

    fn query(&self, statement: &str) -> ToolOutcome {
        self.layer.call(
            "execute_query",
            json!({"connection_id": MONGO, "database": "shop", "statement": statement}),
        )
    }

    fn query_with(&self, statement: &str, extra: Value) -> ToolOutcome {
        let mut args = json!({"connection_id": MONGO, "database": "shop", "statement": statement});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        self.layer.call("execute_query", args)
    }

    fn ok(&self, statement: &str) -> Value {
        match self.query(statement) {
            ToolOutcome::Success(value) => {
                validate(&spec(ToolName::ExecuteQuery).output_schema, &value)
                    .unwrap_or_else(|error| panic!("{statement}: the output breaks its schema: {error}\n{value}"));
                value
            }
            ToolOutcome::Failure(error) => panic!("{statement} failed: {error:?}"),
        }
    }

    fn sent(&self) -> Vec<(String, Value)> {
        self.mongo.mongo_calls()
    }

    fn names(&self) -> Vec<String> {
        self.sent()
            .iter()
            .map(|(_, command)| command.as_object().unwrap().keys().next().unwrap().clone())
            .collect()
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn row(result: &Value, at: usize) -> Value {
    serde_json::from_str(result["rows"][at][0].as_str().unwrap()).unwrap()
}

// ------------------------------------------------------------------ the policy

const READS: [&str; 15] = [
    "db.orders.find({status: 'paid'})",
    "db.orders.findOne({})",
    "db.orders.count()",
    "db.orders.countDocuments({a: 1})",
    "db.orders.estimatedDocumentCount()",
    "db.orders.distinct('status')",
    "db.orders.aggregate([{$match: {}}])",
    "db.getCollection('orders').getIndexes()",
    "db.orders.stats()",
    "db.orders.find({}).explain()",
    "show dbs",
    "show collections",
    "db.getCollectionNames()",
    "db.version()",
    "db.getSiblingDB('other').orders.find({})",
];

const WRITES: [&str; 9] = [
    "db.orders.insertOne({a: 1})",
    "db.orders.insertMany([{a: 1}, {a: 2}])",
    "db.orders.updateOne({a: 1}, {$set: {a: 2}})",
    "db.orders.replaceOne({a: 1}, {a: 2})",
    "db.orders.deleteOne({a: 1})",
    "db.orders.bulkWrite([{insertOne: {document: {a: 1}}}, {deleteOne: {filter: {a: 2}}}])",
    "db.orders.findOneAndUpdate({a: 1}, {$set: {a: 2}})",
    "db.orders.findOneAndDelete({a: 1})",
    "db.orders.createIndex({a: 1})",
];

const DANGEROUS: [&str; 8] = [
    "db.orders.dropIndex('a_1')",
    "db.orders.dropIndexes()",
    "db.orders.drop()",
    "db.orders.renameCollection('old_orders')",
    "db.orders.deleteMany({})",
    "db.orders.updateMany({}, {$set: {a: 1}})",
    "db.orders.aggregate([{$match: {}}, {$out: 'copy'}])",
    "db.orders.aggregate([{$merge: {into: 'copy'}}])",
];

const REFUSED: [&str; 4] = [
    "db.runCommand({ping: 1})",
    "db.adminCommand({listDatabases: 1})",
    "db.createUser({user: 'u', pwd: 'p', roles: []})",
    "db.getSiblingDB('x').runCommand({ping: 1})",
];

#[test]
fn every_command_at_every_access_level() {
    for (access, reads, writes, dangerous) in [
        (None, "agent.access_denied", "agent.access_denied", "agent.access_denied"),
        (Some(AgentAccess::Off), "agent.access_denied", "agent.access_denied", "agent.access_denied"),
        (Some(AgentAccess::SchemaOnly), "agent.access_denied", "agent.access_denied", "agent.access_denied"),
        (Some(AgentAccess::Read), "ok", "agent.write_refused", "agent.write_refused"),
        (Some(AgentAccess::ReadWrite), "ok", "ok", "agent.dangerous_refused"),
    ] {
        let rig = rig(access, approving_dangerous());
        if access.is_some_and(|access| access >= AgentAccess::SchemaOnly) {
            rig.open();
        }
        for (statements, expected) in [(&READS[..], reads), (&WRITES[..], writes), (&DANGEROUS[..], dangerous)] {
            for statement in statements {
                let before = rig.sent().len();
                let outcome = rig.query(statement);
                assert_eq!(code(&outcome), expected, "{access:?}: {statement}: {outcome:?}");
                if expected != "ok" {
                    assert_eq!(rig.sent().len(), before, "{access:?}: {statement} sent something");
                }
            }
        }
        // Whatever the access, runCommand and createUser never reach the server.
        for statement in REFUSED {
            let outcome = rig.query(statement);
            assert!(
                matches!(code(&outcome), "agent.access_denied" | "agent.dangerous_refused"),
                "{access:?}: {statement}: {outcome:?}"
            );
        }
        assert!(
            !rig.names().iter().any(|name| ["runCommand", "adminCommand", "createUser", "ping"].contains(&name.as_str())),
            "{:?}",
            rig.names()
        );
    }
}

#[test]
fn a_denied_write_names_the_access_and_what_would_change_it() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.open();
    let outcome = rig.query("db.orders.insertOne({a: 1})");
    let text = outcome.to_text();
    assert!(text.contains("insertOne") && text.contains("`read`") && text.contains("`read_write`"), "{text}");
    assert!(rig.approver.asked().is_empty());
}

#[test]
fn the_refused_commands_say_to_use_the_app_even_when_everything_is_allowed() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving_dangerous());
    rig.open();
    for statement in REFUSED {
        let outcome = rig.query_with(statement, json!({"dangerous": true}));
        assert_eq!(code(&outcome), "agent.dangerous_refused", "{statement}");
        assert!(outcome.to_text().contains("tablex app"), "{}", outcome.to_text());
    }
    assert!(rig.sent().is_empty());
    assert!(rig.approver.asked().is_empty());
}

#[test]
fn a_dangerous_command_needs_the_flag_and_then_the_user_is_told_the_danger() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving_dangerous());
    rig.open();
    for statement in DANGEROUS {
        let outcome = rig.query(statement);
        assert_eq!(code(&outcome), "agent.dangerous_refused", "{statement}");
        assert!(outcome.to_text().contains("dangerous=true"), "{}", outcome.to_text());
    }
    assert!(rig.sent().is_empty());
    assert!(rig.approver.asked().is_empty(), "nobody is asked about a command that was not flagged");
    for statement in DANGEROUS {
        let outcome = rig.query_with(statement, json!({"dangerous": true}));
        assert!(!outcome.is_error(), "{statement}: {outcome:?}");
    }
    let asked = rig.approver.asked();
    assert_eq!(asked.len(), DANGEROUS.len());
    assert!(asked.iter().all(|question| question.dangerous.is_some()), "{asked:?}");
    assert!(asked.iter().all(|question| question.language == Language::Mongo));
    assert_eq!(rig.sent().len(), DANGEROUS.len());
    // A plain write is asked about without a danger.
    rig.ok("db.orders.insertOne({a: 1})");
    assert_eq!(rig.approver.asked().last().unwrap().dangerous, None);
}

#[test]
fn a_front_end_that_runs_no_dangerous_command_refuses_it_at_once_flag_or_not() {
    // The MCP server on a production connection: `allows_dangerous` is false.
    let production = ConnectionTypeInfo {
        name: "Production".into(),
        production: true,
        ..ConnectionTypeInfo::default()
    };
    for flagged in [false, true] {
        let rig = rig_typed(Some(AgentAccess::ReadWrite), approving(), production.clone());
        rig.open();
        for statement in DANGEROUS {
            let outcome = rig.query_with(statement, json!({"dangerous": flagged}));
            assert_eq!(code(&outcome), "agent.dangerous_refused", "{statement}");
            let text = outcome.to_text();
            assert!(text.contains("does not run dangerous") && text.contains("production"), "{text}");
            assert!(text.contains("tablex app"), "{text}");
        }
        assert!(rig.sent().is_empty());
        assert!(rig.approver.asked().is_empty());
    }
}

#[test]
fn a_production_connection_refuses_a_plain_write_through_the_front_end_without_sending() {
    let production = ConnectionTypeInfo {
        name: "Production".into(),
        production: true,
        ..ConnectionTypeInfo::default()
    };
    let mut approver = approving();
    approver.refuse_production = true;
    let rig = rig_typed(Some(AgentAccess::ReadWrite), approver, production);
    rig.open();
    let outcome = rig.query("db.orders.insertOne({a: 1})");
    assert_eq!(code(&outcome), "agent.write_refused");
    assert!(rig.sent().is_empty());
    assert!(rig.approver.asked()[0].production);
}

#[test]
fn a_declined_question_sends_nothing() {
    let rig = rig(
        Some(AgentAccess::ReadWrite),
        Recorder::new(ApprovalDecision::Deny { reason: Some("not now".into()) }),
    );
    rig.open();
    let outcome = rig.query("db.orders.deleteOne({a: 1})");
    assert_eq!(code(&outcome), "agent.write_refused");
    assert!(outcome.to_text().contains("did not approve this MongoDB command: not now"), "{}", outcome.to_text());
    assert!(rig.sent().is_empty());
}

#[test]
fn a_read_only_connection_type_caps_a_shell_write() {
    let read_only = ConnectionTypeInfo {
        name: "Read-only".into(),
        read_only: true,
        ..ConnectionTypeInfo::default()
    };
    let rig = rig_typed(Some(AgentAccess::ReadWrite), approving(), read_only);
    rig.open();
    assert_eq!(code(&rig.query("db.orders.insertOne({a: 1})")), "agent.write_refused");
    rig.ok("db.orders.find({})");
}

// --------------------------------------------------------------------- what is sent

#[test]
fn the_card_shows_the_database_the_collection_and_the_exact_command() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.ok("db.orders.updateOne({_id: ObjectId('65a1b2c3d4e5f60718293a4b')}, {$set: {paid_at: ISODate('2024-01-31T00:00:00Z')}})");
    let asked = rig.approver.asked();
    assert_eq!(asked.len(), 1);
    let question = &asked[0];
    assert_eq!(question.database.as_deref(), Some("shop"));
    assert_eq!(question.collection.as_deref(), Some("orders"));
    let shown: Value = serde_json::from_str(&question.statement).unwrap();
    assert_eq!(shown, rig.sent()[0].1);
    assert_eq!(shown["update"], "orders");
    assert_eq!(shown["updates"][0]["q"]["_id"], json!({"$oid": "65a1b2c3d4e5f60718293a4b"}));
    assert_eq!(shown["updates"][0]["u"]["$set"]["paid_at"], json!({"$date": "2024-01-31T00:00:00Z"}));
}

#[test]
fn the_commands_are_built_in_the_shape_the_server_takes() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving_dangerous());
    rig.open();
    for statement in [
        "db.orders.find({status: 'paid'}, {total: 1}).sort({created: -1}).skip(2).limit(5)",
        "db.orders.find({}).explain('executionStats')",
        "db.orders.countDocuments({a: 1})",
        "db.orders.distinct('status', {a: 1})",
        "db.orders.getIndexes()",
        "db.orders.stats({scale: 1024})",
        "db.orders.insertMany([{a: 1}], {ordered: false})",
        "db.orders.findOneAndUpdate({a: 1}, {$inc: {n: 1}}, {returnDocument: 'after', upsert: true})",
        "db.orders.createIndex({a: 1, b: -1}, {unique: true})",
        "db.orders.dropIndex('a_1')",
        "db.orders.drop()",
    ] {
        let outcome = rig.query_with(statement, json!({"dangerous": true}));
        assert!(!outcome.is_error(), "{statement}: {outcome:?}");
    }
    let sent: Vec<String> = rig.sent().iter().map(|(_, command)| command.to_string()).collect();
    assert_eq!(
        sent,
        [
            r#"{"find":"orders","filter":{"status":"paid"},"projection":{"total":1},"sort":{"created":-1},"skip":2,"limit":5,"batchSize":5}"#,
            r#"{"explain":{"find":"orders","filter":{}},"verbosity":"executionStats"}"#,
            r#"{"count":"orders","query":{"a":1}}"#,
            r#"{"distinct":"orders","key":"status","query":{"a":1}}"#,
            r#"{"listIndexes":"orders","cursor":{"batchSize":201}}"#,
            r#"{"collStats":"orders","scale":1024}"#,
            r#"{"insert":"orders","documents":[{"a":1}],"ordered":false}"#,
            r#"{"findAndModify":"orders","query":{"a":1},"update":{"$inc":{"n":1}},"new":true,"upsert":true}"#,
            r#"{"createIndexes":"orders","indexes":[{"key":{"a":1,"b":-1},"name":"a_1_b_-1","unique":true}]}"#,
            r#"{"dropIndexes":"orders","index":"a_1"}"#,
            r#"{"drop":"orders"}"#,
        ]
    );
    // Reads go through the read-only method of the session, changes through the other one.
    let writes = rig.mongo.mongo_writes.lock().unwrap().clone();
    assert_eq!(writes, [false, false, false, false, false, false, true, true, true, true, true]);
    assert!(rig.mongo.mongo_calls().iter().all(|(database, _)| database == "shop"));
}

#[test]
fn a_find_asks_for_one_more_than_the_row_limit_and_a_smaller_limit_exactly() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.open();
    rig.ok("db.orders.find({})");
    rig.query_with("db.orders.find({})", json!({"row_limit": 50}));
    rig.ok("db.orders.find({}).limit(3)");
    rig.query_with("db.orders.find({}).limit(500)", json!({"row_limit": 100}));
    let limits: Vec<(u64, u64)> = rig
        .sent()
        .iter()
        .map(|(_, command)| (command["limit"].as_u64().unwrap(), command["batchSize"].as_u64().unwrap()))
        .collect();
    assert_eq!(limits, [(201, 201), (51, 51), (3, 3), (101, 101)]);
}

#[test]
fn an_aggregate_gets_a_final_limit_unless_it_writes() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving_dangerous());
    rig.open();
    rig.ok("db.orders.aggregate([{$match: {a: 1}}], {allowDiskUse: true})");
    rig.query_with("db.orders.aggregate([{$match: {}}, {$out: 'copy'}])", json!({"dangerous": true}));
    let sent = rig.sent();
    assert_eq!(
        sent[0].1.to_string(),
        r#"{"aggregate":"orders","pipeline":[{"$match":{"a":1}},{"$limit":201}],"cursor":{"batchSize":201},"allowDiskUse":true}"#
    );
    assert_eq!(
        sent[1].1.to_string(),
        r#"{"aggregate":"orders","pipeline":[{"$match":{}},{"$out":"copy"}],"cursor":{}}"#
    );
    assert_eq!(*rig.mongo.mongo_writes.lock().unwrap(), [false, true]);
}

#[test]
fn a_rename_runs_in_admin_with_full_namespaces() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving_dangerous());
    rig.open();
    rig.query_with("db.orders.renameCollection('old_orders')", json!({"dangerous": true}));
    assert_eq!(
        rig.sent(),
        [("admin".to_string(), json!({"renameCollection": "shop.orders", "to": "shop.old_orders"}))]
    );
    let asked = rig.approver.asked();
    assert_eq!(asked[0].database.as_deref(), Some("shop"));
    assert_eq!(asked[0].collection.as_deref(), Some("orders"));
}

#[test]
fn a_sibling_database_overrides_and_version_and_dbs_run_in_admin() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.open();
    rig.ok("db.getSiblingDB('other').orders.find({})");
    rig.ok("db.version()");
    rig.ok("show dbs");
    rig.ok("show collections");
    let databases: Vec<String> = rig.sent().iter().map(|(database, _)| database.clone()).collect();
    assert_eq!(databases, ["other", "admin", "admin", "shop"]);
}

#[test]
fn without_a_database_the_only_one_is_used_else_it_is_asked_for() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.open();
    let outcome = rig.layer.call(
        "execute_query",
        json!({"connection_id": MONGO, "statement": "db.orders.find({})"}),
    );
    assert!(!outcome.is_error(), "{outcome:?}");
    // The fake server has one database, `main`.
    assert_eq!(rig.sent()[0].0, "main");
    let outcome = rig.layer.call(
        "execute_query",
        json!({"connection_id": MONGO, "database": "", "statement": "db.orders.find({})"}),
    );
    assert_eq!(code(&outcome), "agent.invalid_arguments");
}

// ------------------------------------------------------------------------ results

#[test]
fn a_find_is_one_document_per_row_and_a_cut_is_reported() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.open();
    let result = rig.ok("db.orders.find({})");
    assert_eq!(result["columns"], json!([{"name": "document", "type": "json"}]));
    assert_eq!(result["row_count"], 2);
    assert_eq!(row(&result, 0), json!({"_id": 1, "status": "paid"}));
    assert_eq!(result["truncated"], false);
    assert_eq!(result["read_only_enforced"], true);
    // Two documents come back for a limit of one: more exist.
    let result = match rig.query_with("db.orders.find({})", json!({"row_limit": 1})) {
        ToolOutcome::Success(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(result["row_count"], 1);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["truncation"], "row_limit");
    assert_eq!(result["row_limit"], 1);
}

#[test]
fn the_other_reads_come_back_as_rows() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.open();
    let count = rig.ok("db.orders.countDocuments({})");
    assert_eq!((count["columns"][0]["name"].as_str(), count["rows"][0][0].as_str()), (Some("count"), Some("7")));
    let distinct = rig.ok("db.orders.distinct('status')");
    assert_eq!(distinct["rows"], json!([["\"paid\""], ["\"open\""]]));
    let indexes = rig.ok("db.getCollection('orders').getIndexes()");
    assert_eq!(indexes["row_count"], 3);
    assert_eq!(row(&indexes, 1)["name"], "email_1");
    assert_eq!(row(&indexes, 1)["unique"], true);
    let collections = rig.ok("show collections");
    assert_eq!(collections["row_count"], 2);
    assert_eq!(row(&collections, 0)["name"], "orders");
    let databases = rig.ok("show dbs");
    assert_eq!(databases["rows"], json!([["{\"name\":\"admin\"}"], ["{\"name\":\"shop\"}"]]));
    let version = rig.ok("db.version()");
    assert_eq!(row(&version, 0), json!({"version": "7.0.5"}));
    // The reply without the server's bookkeeping; a metric method returns its one number.
    let stats = rig.ok("db.orders.stats()");
    assert_eq!(row(&stats, 0)["storageSize"], 8192);
    assert!(row(&stats, 0).get("ok").is_none() && row(&stats, 0).get("$clusterTime").is_none());
    let size = rig.ok("db.orders.dataSize()");
    assert_eq!(row(&size, 0), json!({"size": 4096}));
    let explain = rig.ok("db.orders.find({}).explain()");
    assert_eq!(row(&explain, 0)["queryPlanner"]["winningPlan"]["stage"], "COLLSCAN");
    rig.ok("db.stats()");
}

#[test]
fn use_switches_nothing_and_sends_nothing() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.open();
    let result = rig.ok("use shop");
    assert_eq!(row(&result, 0)["database"], "shop");
    assert!(row(&result, 0)["note"].as_str().unwrap().contains("self-contained"));
    assert!(rig.sent().is_empty());
}

#[test]
fn a_write_reports_its_counts_and_a_bulk_write_adds_its_commands_up() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    let result = rig.ok("db.orders.updateMany({a: 1}, {$set: {b: 2}})");
    assert_eq!(result["affected_rows"], 1);
    assert_eq!(row(&result, 0), json!({"n": 2, "nModified": 1}));
    assert_eq!(result["read_only_enforced"], false);
    let result = rig.ok(
        "db.orders.bulkWrite([{insertOne: {document: {a: 1}}}, {insertOne: {document: {a: 2}}}, {updateOne: {filter: {a: 1}, update: {$set: {b: 1}}}}, {deleteOne: {filter: {a: 2}}}])",
    );
    assert_eq!(rig.names()[1..], ["insert", "update", "delete"]);
    // 2 inserted + 1 modified + 3 "deleted" by the fake.
    assert_eq!(row(&result, 0), json!({"n": 7, "nModified": 1}));
    assert_eq!(rig.approver.asked().len(), 2, "one question for the whole bulk write");
    let shown: Value = serde_json::from_str(&rig.approver.asked()[1].statement).unwrap();
    assert_eq!(shown.as_array().unwrap().len(), 3);
    let result = rig.ok("db.orders.findOneAndDelete({a: 1})");
    assert_eq!(row(&result, 0)["value"], json!({"_id": 1}));
}

#[test]
fn an_ordered_bulk_write_stops_at_the_first_command_with_write_errors() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.mongo.set_mongo(|_, command, _| {
        let name = command.as_object().unwrap().keys().next().unwrap().as_str();
        let reply = match name {
            "insert" => doc! { "n": 0, "writeErrors": [{ "index": 0, "code": 11000, "errmsg": "duplicate key" }], "ok": 1.0 },
            _ => doc! { "n": 1, "ok": 1.0 },
        };
        Ok(Bson::Document(reply).into_canonical_extjson())
    });
    rig.open();
    let statement = "db.orders.bulkWrite([{insertOne: {document: {a: 1}}}, {deleteOne: {filter: {a: 2}}}])";
    let result = rig.ok(statement);
    assert_eq!(rig.names(), ["insert"], "the delete was not sent");
    assert_eq!(row(&result, 0)["writeErrors"][0]["code"], 11000);
    // Unordered: the next command goes on.
    let result = rig.ok(
        "db.orders.bulkWrite([{insertOne: {document: {a: 1}}}, {deleteOne: {filter: {a: 2}}}], {ordered: false})",
    );
    assert_eq!(rig.names()[1..], ["insert", "delete"]);
    assert_eq!(row(&result, 0)["n"], 1);
}

#[test]
fn the_servers_refusal_is_an_error_with_its_name() {
    let rig = rig(Some(AgentAccess::Read), approving());
    rig.mongo.set_mongo(|_, _, _| {
        Ok(Bson::Document(doc! { "ok": 0.0, "errmsg": "unknown top level operator: $nope", "codeName": "BadValue" })
            .into_canonical_extjson())
    });
    rig.open();
    let outcome = rig.query("db.orders.find({$nope: 1})");
    assert_eq!(code(&outcome), "query.failed");
    assert!(outcome.to_text().contains("BadValue") && outcome.to_text().contains("$nope"), "{}", outcome.to_text());
}

// ------------------------------------------------------------------ bad statements

#[test]
fn what_is_not_one_known_command_is_refused_before_anything_is_sent() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving_dangerous());
    rig.open();
    for statement in [
        "SELECT * FROM orders",
        "",
        "db.orders.find({}); db.orders.drop()",
        "db.orders.find({})\ndb.orders.drop()",
        "db.orders.find({a: })",
        "db.orders.mapReduce(function() {}, function() {}, {out: 'x'})",
        "db.orders.find({}).forEach(printjson)",
        "db.dropDatabase2()",
        "sh.status()",
        "db.orders.updateOne({a: 1}, {a: 2})",
        "db.orders.insertMany([])",
    ] {
        let outcome = rig.query_with(statement, json!({"dangerous": true}));
        assert_eq!(code(&outcome), "agent.invalid_arguments", "{statement}: {outcome:?}");
        assert!(outcome.to_text().contains("MongoDB"), "{}", outcome.to_text());
    }
    assert!(rig.sent().is_empty());
    assert!(rig.approver.asked().is_empty());
    // The message lists what is supported.
    let outcome = rig.query("db.orders.nope()");
    assert!(outcome.to_text().contains("find") && outcome.to_text().contains("getIndexes"), "{}", outcome.to_text());
}

#[test]
fn a_batch_on_mongodb_points_to_execute_query() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    let outcome = rig.layer.call(
        "execute_batch",
        json!({"connection_id": MONGO, "script": "SELECT 1"}),
    );
    assert_eq!(code(&outcome), "agent.unsupported");
    assert!(outcome.to_text().contains("execute_query"), "{}", outcome.to_text());
}

// ------------------------------------------------------------------------ audit

#[test]
fn a_shell_write_is_audited_before_it_is_sent_and_a_read_is_not_recorded_as_started() {
    let rig = rig(Some(AgentAccess::ReadWrite), approving());
    rig.open();
    rig.ok("db.orders.find({})");
    rig.ok("db.orders.insertOne({a: 1})");
    rig.audit.flush();
    let entries = read_entries(&rig.audit_path, 100).unwrap().entries;
    let started: Vec<_> = entries.iter().filter(|entry| entry.outcome == "started").collect();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].tool, "execute_query");
    assert!(started[0].statement.as_deref().unwrap().contains("\"insert\":\"orders\""), "{started:?}");
    assert_eq!(started[0].approval.as_deref(), Some("approved"));
    let finished = entries.iter().filter(|entry| entry.tool == "execute_query" && entry.outcome == "ok").count();
    assert_eq!(finished, 2);
}

// --------------------------------------------------------------- describe_table

#[test]
fn describe_table_on_a_collection_lists_its_indexes_with_schema_only_access() {
    let rig = rig(Some(AgentAccess::SchemaOnly), approving());
    rig.open();
    let outcome = rig.layer.call(
        "describe_table",
        json!({"connection_id": MONGO, "database": "shop", "table": "orders"}),
    );
    let ToolOutcome::Success(description) = outcome else {
        panic!("{outcome:?}")
    };
    validate(&spec(ToolName::DescribeTable).output_schema, &description).unwrap();
    assert_eq!(description["name"], "orders");
    assert_eq!(description["database"], "shop");
    assert_eq!(description["columns"], json!([]));
    assert_eq!(description["primary_key"], json!(["_id"]));
    assert_eq!(description["unique_columns"], json!(["email"]));
    assert_eq!(
        description["indexes"],
        json!([
            {"name": "_id_", "columns": ["_id"], "unique": false},
            {"name": "email_1", "columns": ["email"], "unique": true},
            {"name": "status_1_created_-1", "columns": ["status", "created"], "unique": false},
        ])
    );
    let ddl = description["ddl"].as_str().unwrap();
    assert!(ddl.contains(r#"db.getCollection("orders").createIndex({"email":1}, {"name":"email_1","unique":true})"#), "{ddl}");
    assert!(ddl.contains("expireAfterSeconds"), "{ddl}");
    assert!(!ddl.contains("_id_"), "{ddl}");
    // Only listIndexes was sent, through the read-only method.
    assert_eq!(
        rig.sent(),
        [("shop".to_string(), json!({"listIndexes": "orders", "cursor": {"batchSize": 101}}))]
    );
    assert_eq!(*rig.mongo.mongo_writes.lock().unwrap(), [false]);
}

#[test]
fn describe_table_on_a_collection_needs_schema_only_and_a_known_database() {
    let rig = rig(Some(AgentAccess::Off), approving());
    let outcome = rig.layer.call(
        "describe_table",
        json!({"connection_id": MONGO, "database": "shop", "table": "orders"}),
    );
    assert_eq!(code(&outcome), "agent.access_denied");
    let rig = self::rig(Some(AgentAccess::SchemaOnly), approving());
    rig.open();
    // One database on the server: it is the default.
    let outcome = rig.layer.call("describe_table", json!({"connection_id": MONGO, "table": "orders"}));
    assert!(!outcome.is_error(), "{outcome:?}");
    assert_eq!(rig.sent()[0].0, "main");
}

#[test]
fn a_describe_error_from_the_server_is_passed_on() {
    let rig = rig(Some(AgentAccess::SchemaOnly), approving());
    rig.mongo.set_mongo(|_, _, _| Err(AgentError::driver("query.failed", "ns does not exist", false)));
    rig.open();
    let outcome = rig.layer.call(
        "describe_table",
        json!({"connection_id": MONGO, "database": "shop", "table": "nope"}),
    );
    assert_eq!(code(&outcome), "query.failed");
}

#[test]
fn the_tool_descriptions_tell_the_agent_that_shell_commands_work() {
    let execute = spec(ToolName::ExecuteQuery);
    for needle in ["MongoDB", "db.orders.find", "getIndexes", "runCommand", "createUser", "read_write"] {
        assert!(execute.description.contains(needle), "execute_query: {needle}");
    }
    let open = spec(ToolName::OpenSession).output_schema.to_string();
    assert!(open.contains("execute_query") && open.contains("mongo"));
    assert!(spec(ToolName::DescribeTable).description.contains("MongoDB"));
}
