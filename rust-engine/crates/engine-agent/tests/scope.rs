//! The reach of a call: the cell window, the databases and tools the user allowed, what
//! `list_connections` and `list_tables` say about a connection and its tables, the schema digest
//! of a selection of tables and of a MongoDB collection read from a sample, and transactions on
//! MySQL and MariaDB.

use bson::{doc, Bson};
use engine_agent::table::TableDescription;
use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeProvider, FakeSession, ScriptedApprover};
use engine_agent::{
    AgentAccess, ApprovalDecision, ConnectionKind, ToolLayer, ToolOutcome, TxControl,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const SQL: &str = "sql1";
const MONGO: &str = "mongo1";
const MY: &str = "my1";

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn ok(outcome: ToolOutcome) -> Value {
    match outcome {
        ToolOutcome::Success(value) => value,
        ToolOutcome::Failure(error) => panic!("failed: {error:?}"),
    }
}

struct Rig {
    layer: ToolLayer,
    catalog: Arc<FakeCatalog>,
    approver: Arc<ScriptedApprover>,
    sessions: Vec<(String, Arc<FakeSession>)>,
}

fn rig(connections: &[(&'static str, ConnectionKind)], access: AgentAccess) -> Rig {
    let provider = FakeProvider::new();
    let mut infos = Vec::new();
    let mut sessions = Vec::new();
    for (id, kind) in connections {
        let mut session = FakeSession::new(*kind);
        session.databases = vec!["shop".into(), "reports".into(), "secrets".into()];
        let session = session.arc();
        provider.with_session(id, Arc::clone(&session));
        // The session a transaction or a pinned session gets: `<id>#2`.
        let extra = FakeSession::new(*kind).arc();
        provider.with_extra_session(id, Arc::clone(&extra));
        sessions.push((format!("{id}#2"), extra));
        sessions.push(((*id).to_owned(), session));
        infos.push(connection(id, *kind, Some(access)));
    }
    let catalog = FakeCatalog::new(infos);
    let approver = ScriptedApprover::new(ApprovalDecision::Approve, true);
    let layer = ToolLayer::new(catalog.clone(), provider, "test")
        .with_approver(approver.clone())
        .with_transactions()
        .with_pinned_sessions();
    Rig {
        layer,
        catalog,
        approver,
        sessions,
    }
}

impl Rig {
    fn session(&self, id: &str) -> &Arc<FakeSession> {
        &self.sessions.iter().find(|(name, _)| name == id).unwrap().1
    }

    fn open(&self, id: &str) {
        ok(self.layer.call("open_session", json!({"connection_id": id})));
    }

    fn call(&self, tool: &str, args: Value) -> ToolOutcome {
        self.layer.call(tool, args)
    }
}

// ------------------------------------------------------------------ the cell window

#[test]
fn a_long_cell_is_read_in_windows_by_execute_query_and_execute_batch() {
    let rig = rig(&[(SQL, ConnectionKind::PostgreSql)], AgentAccess::Read);
    rig.session(SQL).set_behavior(Behavior::WideCell { bytes: 10_000 });
    rig.open(SQL);
    let cell = |args: Value| -> String {
        let mut request = json!({"connection_id": SQL, "statement": "SELECT big FROM t"});
        request.as_object_mut().unwrap().extend(args.as_object().unwrap().clone());
        ok(rig.call("execute_query", request))["rows"][0][0].as_str().unwrap().to_owned()
    };
    // No window: the start, cut at the cell limit and marked.
    let plain = cell(json!({}));
    assert!(plain.ends_with("[truncated, 10000 bytes in total]"), "{plain}");
    // A window: exactly those characters, and where the next one starts.
    let window = cell(json!({"cell_char_offset": 100, "cell_char_limit": 50}));
    assert_eq!(window, format!("{}… [chars 100-150 of 10000; next cell_char_offset 150]", "w".repeat(50)));
    // Only an offset: the default width (the cell limit).
    let tail = cell(json!({"cell_char_offset": 9_000}));
    assert_eq!(tail, format!("{}… [chars 9000-10000 of 10000]", "w".repeat(1000)));
    // Only a limit: from the start. Clamped to 4000 and to at least 1.
    let head = cell(json!({"cell_char_limit": 999_999}));
    assert!(head.starts_with(&"w".repeat(4000)) && head.contains("chars 0-4000 of 10000; next cell_char_offset 4000"), "{}", &head[4000..]);
    assert!(cell(json!({"cell_char_limit": 0})).contains("chars 0-1 of 10000"));
    // Past the end: nothing, still said.
    assert_eq!(cell(json!({"cell_char_offset": 20_000, "cell_char_limit": 10})), "… [chars 10000-10000 of 10000]");
    // The batch takes the same window for every statement.
    let batch = ok(rig.call(
        "execute_batch",
        json!({"connection_id": SQL, "script": "SELECT 1; SELECT 2", "cell_char_offset": 10, "cell_char_limit": 5}),
    ));
    for statement in batch["statements"].as_array().unwrap() {
        assert_eq!(
            statement["result"]["rows"][0][0],
            format!("{}… [chars 10-15 of 10000; next cell_char_offset 15]", "w".repeat(5))
        );
    }
    // The cells of a later call are not windowed by an earlier one.
    assert!(cell(json!({})).ends_with("[truncated, 10000 bytes in total]"));
}

#[test]
fn a_string_of_a_mongodb_document_is_windowed_too() {
    let rig = rig(&[(MONGO, ConnectionKind::MongoDb)], AgentAccess::Read);
    rig.session(MONGO).set_mongo(|_, _, _| {
        let reply = doc! {"cursor": {"id": 0_i64, "firstBatch": [{"_id": 1, "text": "abcdefghij"}]}, "ok": 1.0};
        Ok(Bson::Document(reply).into_canonical_extjson())
    });
    rig.open(MONGO);
    let result = ok(rig.call(
        "execute_query",
        json!({"connection_id": MONGO, "database": "shop", "statement": "db.notes.find({})", "cell_char_offset": 2, "cell_char_limit": 3}),
    ));
    let document: Value = serde_json::from_str(result["rows"][0][0].as_str().unwrap()).unwrap();
    assert_eq!(document["text"], "cde… [chars 2-5 of 10; next cell_char_offset 5]");
    assert_eq!(result["cells_truncated"], 1);
}

// --------------------------------------------------- filters that match everything

fn mongo_rig() -> Rig {
    let rig = rig(&[(MONGO, ConnectionKind::MongoDb)], AgentAccess::ReadWrite);
    rig.session(MONGO).set_mongo(|_, command, _| {
        let name = command.as_object().unwrap().keys().next().unwrap().as_str();
        let reply = match name {
            "update" => doc! {"n": 1, "nModified": 1, "ok": 1.0},
            "delete" => doc! {"n": 1, "ok": 1.0},
            other => panic!("unexpected {other}"),
        };
        Ok(Bson::Document(reply).into_canonical_extjson())
    });
    rig.open(MONGO);
    rig
}

#[test]
fn the_mongodb_tools_refuse_a_filter_that_matches_everything_unless_allowed_and_then_it_is_dangerous() {
    let rig = mongo_rig();
    let sent = || rig.session(MONGO).mongo_calls().len();
    let filters = [
        json!({"_id": {"$exists": true}}),
        json!({"$where": "this.a > 1"}),
        json!({"$expr": {"$gt": ["$a", 0]}}),
        json!({"$or": [{"a": 1}, {"a": {"$ne": 1}}]}),
        json!({"$or": [{"a": {"$exists": true}}, {"a": {"$exists": false}}]}),
        json!({"_id": {"$gte": {"$minKey": 1}}}),
        json!({"$nor": [{"a": 1}]}),
        json!({}),
    ];
    for filter in &filters {
        for (tool, extra) in [
            ("mongo_delete", json!({"many": true})),
            ("mongo_update", json!({"update": {"$set": {"a": 1}}})),
        ] {
            let mut args = json!({"connection_id": MONGO, "database": "shop", "collection": "orders", "filter": filter});
            args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let before = sent();
            let outcome = rig.call(tool, args.clone());
            assert_eq!(code(&outcome), "agent.invalid_arguments", "{tool} {filter}: {outcome:?}");
            assert!(outcome.to_text().contains("allow_all"), "{}", outcome.to_text());
            assert_eq!(sent(), before, "nothing was sent");
            args["allow_all"] = json!(true);
            let asked = rig.approver.asked.lock().unwrap().len();
            let outcome = rig.call(tool, args);
            assert_eq!(code(&outcome), "ok", "{tool} {filter}: {outcome:?}");
            let questions = rig.approver.asked.lock().unwrap();
            assert_eq!(questions.len(), asked + 1);
            assert!(questions.last().unwrap().1.is_some(), "the danger is stated: {filter}");
        }
    }
    // Filters that name documents are ordinary writes.
    for filter in [json!({"_id": 5}), json!({"_id": {"$in": [1, 2]}}), json!({"a": {"$exists": true}}), json!({"$or": [{"a": 1}, {"b": 2}]}), json!({"status": "open", "_id": {"$exists": true}})] {
        let outcome = rig.call(
            "mongo_delete",
            json!({"connection_id": MONGO, "database": "shop", "collection": "orders", "filter": filter, "many": true}),
        );
        assert_eq!(code(&outcome), "ok", "{filter}: {outcome:?}");
        assert!(rig.approver.asked.lock().unwrap().last().unwrap().1.is_none(), "{filter}");
    }
}

#[test]
fn a_shell_change_of_many_documents_on_such_a_filter_needs_the_flag() {
    let rig = mongo_rig();
    for statement in [
        "db.orders.deleteMany({_id: {$exists: true}})",
        "db.orders.updateMany({$where: 'true'}, {$set: {a: 1}})",
        "db.orders.deleteMany({$expr: {$eq: [1, 1]}})",
        "db.orders.deleteMany({$or: [{a: 1}, {a: {$ne: 1}}]})",
    ] {
        let outcome = rig.call("execute_query", json!({"connection_id": MONGO, "database": "shop", "statement": statement}));
        assert_eq!(code(&outcome), "agent.dangerous_refused", "{statement}: {outcome:?}");
        let flagged = rig.call("execute_query", json!({"connection_id": MONGO, "database": "shop", "statement": statement, "dangerous": true}));
        assert_eq!(code(&flagged), "ok", "{statement}: {flagged:?}");
    }
    // One document is not that.
    let one = rig.call("execute_query", json!({"connection_id": MONGO, "database": "shop", "statement": "db.orders.deleteOne({_id: {$exists: true}})"}));
    assert_eq!(code(&one), "ok", "{one:?}");
}

// ------------------------------------------------------------ the tools the user allows

#[test]
fn a_tool_outside_the_users_allowlist_is_refused_and_not_listed() {
    let allowed: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    let provider = FakeProvider::new();
    provider.with_session(SQL, FakeSession::new(ConnectionKind::PostgreSql).arc());
    let catalog = FakeCatalog::new(vec![connection(SQL, ConnectionKind::PostgreSql, Some(AgentAccess::ReadWrite))]);
    let shared = Arc::clone(&allowed);
    let layer = ToolLayer::new(catalog, provider, "test")
        .with_approver(ScriptedApprover::new(ApprovalDecision::Approve, true))
        .with_tool_allowlist(Arc::new(move || shared.lock().unwrap().clone()));
    // Unset: every tool.
    assert!(layer.tool_allowed("execute_query"));
    ok(layer.call("list_connections", json!({})));
    // Set: only those, read at every call.
    *allowed.lock().unwrap() = Some(vec!["list_connections".into(), "open_session".into(), "list_tables".into()]);
    ok(layer.call("list_connections", json!({})));
    ok(layer.call("open_session", json!({"connection_id": SQL})));
    ok(layer.call("list_tables", json!({"connection_id": SQL})));
    for (tool, args) in [
        ("execute_query", json!({"connection_id": SQL, "statement": "SELECT 1"})),
        ("describe_table", json!({"connection_id": SQL, "table": "t"})),
        ("close_session", json!({"connection_id": SQL})),
    ] {
        let outcome = layer.call(tool, args);
        assert_eq!(code(&outcome), "agent.access_denied", "{tool}");
        assert!(outcome.to_text().contains("ai.mcp_tools") && outcome.to_text().contains(tool), "{}", outcome.to_text());
        assert!(!layer.tool_allowed(tool));
    }
    // Nothing allowed: nothing runs. An unknown tool is still unknown.
    *allowed.lock().unwrap() = Some(Vec::new());
    assert_eq!(code(&layer.call("list_connections", json!({}))), "agent.access_denied");
    assert_eq!(code(&layer.call("nope", json!({}))), "agent.unknown_tool");
    // Back to all.
    *allowed.lock().unwrap() = None;
    ok(layer.call("list_connections", json!({})));
}

// -------------------------------------------------------- the databases of a connection

#[test]
fn a_connection_limited_to_some_databases_keeps_every_tool_inside_them() {
    let rig = rig(&[(SQL, ConnectionKind::PostgreSql), (MONGO, ConnectionKind::MongoDb), (MY, ConnectionKind::MySql)], AgentAccess::ReadWrite);
    rig.session(SQL).set_behavior(Behavior::Numbers { count: 2 });
    for id in [SQL, MONGO, MY] {
        rig.catalog.update(id, |info| info.allowed_databases = Some(vec!["shop".into(), "reports".into()]));
        rig.open(id);
    }
    // Inside: ordinary.
    for tool in ["execute_query", "explain_query"] {
        let outcome = rig.call(tool, json!({"connection_id": SQL, "database": "shop", "statement": "SELECT 1"}));
        assert_eq!(code(&outcome), "ok", "{tool}: {outcome:?}");
    }
    // Outside, or none named: refused, naming what is allowed, before anything is sent.
    let before = rig.session(SQL).run_count();
    for (tool, args) in [
        ("execute_query", json!({"connection_id": SQL, "database": "secrets", "statement": "SELECT 1"})),
        ("execute_query", json!({"connection_id": SQL, "statement": "SELECT 1"})),
        ("execute_batch", json!({"connection_id": SQL, "database": "secrets", "script": "SELECT 1"})),
        ("explain_query", json!({"connection_id": SQL, "database": "secrets", "statement": "SELECT 1"})),
        ("describe_table", json!({"connection_id": SQL, "database": "secrets", "table": "t"})),
        ("list_tables", json!({"connection_id": SQL, "database": "secrets"})),
        ("list_tables", json!({"connection_id": SQL})),
        ("get_schema_context", json!({"connection_id": SQL, "database": "secrets"})),
        ("list_routines", json!({"connection_id": SQL, "database": "secrets", "kind": "function"})),
        ("list_namespaces", json!({"connection_id": SQL, "database": "secrets"})),
        ("mongo_find", json!({"connection_id": MONGO, "database": "secrets", "collection": "c"})),
        ("mongo_count", json!({"connection_id": MONGO, "database": "secrets", "collection": "c"})),
        ("mongo_insert", json!({"connection_id": MONGO, "database": "secrets", "collection": "c", "documents": [{"a": 1}]})),
        ("execute_query", json!({"connection_id": MONGO, "database": "secrets", "statement": "db.c.find({})"})),
        ("execute_query", json!({"connection_id": MONGO, "statement": "db.c.find({})"})),
    ] {
        let outcome = rig.call(tool, args);
        assert_eq!(code(&outcome), "agent.access_denied", "{tool}: {outcome:?}");
        let text = outcome.to_text();
        assert!(text.contains("`shop`") && text.contains("`reports`") && text.contains("agent.databases"), "{tool}: {text}");
    }
    assert_eq!(rig.session(SQL).run_count(), before);
    assert!(rig.session(MONGO).mongo_calls().is_empty());
    // The list of databases shows only the allowed ones, and a connection that lists them says so.
    let namespaces = ok(rig.call("list_namespaces", json!({"connection_id": SQL})));
    let names: Vec<&str> = namespaces["namespaces"].as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["shop", "reports"], "{namespaces}");
    let connections = ok(rig.call("list_connections", json!({})));
    let listed = connections["connections"].as_array().unwrap().iter().find(|c| c["id"] == SQL).unwrap();
    assert_eq!(listed["allowed_databases"], json!(["shop", "reports"]));
    // The user's wording, a connection at `off` is listed by name only: nothing about its databases.
    rig.catalog.update(SQL, |info| info.agent_access = Some(AgentAccess::Off));
    let connections = ok(rig.call("list_connections", json!({})));
    let listed = connections["connections"].as_array().unwrap().iter().find(|c| c["id"] == SQL).unwrap();
    assert!(listed.get("allowed_databases").is_none() && listed.get("endpoint").is_none());
    let outcome = rig.call("execute_query", json!({"connection_id": SQL, "database": "secrets", "statement": "SELECT 1"}));
    assert!(!outcome.to_text().contains("reports"), "an off connection does not name its databases: {}", outcome.to_text());
}

#[test]
fn a_mongodb_shell_command_cannot_leave_the_allowed_databases_through_getsiblingdb() {
    let rig = rig(&[(MONGO, ConnectionKind::MongoDb)], AgentAccess::ReadWrite);
    rig.session(MONGO).set_mongo(|_, _, _| {
        Ok(Bson::Document(doc! {"cursor": {"firstBatch": [], "id": 0_i64, "ns": "shop.users"}, "ok": 1.0}).into_canonical_extjson())
    });
    rig.catalog.update(MONGO, |info| info.allowed_databases = Some(vec!["shop".into()]));
    rig.open(MONGO);
    for statement in [
        "db.getSiblingDB('secret').users.find({})",
        "db.getSiblingDB(\"secret\").getCollection('users').getIndexes()",
        "db.getSiblingDB('secret').users.deleteMany({\"a\": 1})",
    ] {
        let outcome = rig.call("execute_query", json!({"connection_id": MONGO, "database": "shop", "statement": statement}));
        assert_eq!(code(&outcome), "agent.access_denied", "{statement}: {outcome:?}");
        assert!(outcome.to_text().contains("secret") && outcome.to_text().contains("`shop`"), "{}", outcome.to_text());
    }
    assert!(rig.session(MONGO).mongo_calls().is_empty(), "nothing was sent");
    // The allowed one, by the argument and by the text.
    for statement in ["db.users.find({})", "db.getSiblingDB('shop').users.find({})"] {
        let outcome = rig.call("execute_query", json!({"connection_id": MONGO, "database": "shop", "statement": statement}));
        assert_eq!(code(&outcome), "ok", "{statement}: {outcome:?}");
    }
}

#[test]
fn a_mongodb_pipeline_cannot_name_a_database_outside_the_allowed_ones() {
    let rig = rig(&[(MONGO, ConnectionKind::MongoDb)], AgentAccess::ReadWrite);
    rig.session(MONGO).set_mongo(|_, _, _| {
        Ok(Bson::Document(doc! {"cursor": {"firstBatch": [], "id": 0_i64, "ns": "shop.orders"}, "ok": 1.0}).into_canonical_extjson())
    });
    rig.catalog.update(MONGO, |info| info.allowed_databases = Some(vec!["shop".into()]));
    rig.open(MONGO);
    for statement in [
        "db.orders.aggregate([{$match: {}}, {$out: {db: 'secret', coll: 'x'}}])",
        "db.orders.aggregate([{$merge: {into: {db: 'secret', coll: 'x'}}}])",
        "db.orders.aggregate([{$lookup: {from: {db: 'secret', coll: 'users'}, localField: 'a', foreignField: 'b', as: 'u'}}])",
        "db.orders.aggregate([{$unionWith: {coll: {db: 'secret', coll: 'users'}}}])",
        "db.orders.aggregate([{$facet: {f: [{$lookup: {from: {db: 'secret', coll: 'u'}, pipeline: []}}]}}])",
        "db.orders.aggregate([{$out: {db: {$literal: 'shop'}, coll: 'x'}}])",
    ] {
        // `dangerous` is given so that the flag a `$out` or `$merge` needs is not what refuses it.
        let outcome = rig.call(
            "execute_query",
            json!({"connection_id": MONGO, "database": "shop", "statement": statement, "dangerous": true}),
        );
        assert_eq!(code(&outcome), "agent.access_denied", "{statement}: {outcome:?}");
        assert!(outcome.to_text().contains("`shop`"), "{}", outcome.to_text());
    }
    // The tool path reads another database through `$lookup` the same way.
    let outcome = rig.call(
        "mongo_aggregate",
        json!({"connection_id": MONGO, "database": "shop", "collection": "orders", "pipeline": [
            {"$lookup": {"from": {"db": "secret", "coll": "users"}, "localField": "a", "foreignField": "b", "as": "u"}}
        ]}),
    );
    assert_eq!(code(&outcome), "agent.access_denied", "{outcome:?}");
    assert!(rig.session(MONGO).mongo_calls().is_empty(), "nothing was sent");
    // The allowed database, and a pipeline that names none, run.
    let outcome = rig.call(
        "execute_query",
        json!({"connection_id": MONGO, "database": "shop", "statement":
            "db.orders.aggregate([{$lookup: {from: {db: 'shop', coll: 'users'}, localField: 'a', foreignField: 'b', as: 'u'}}])"}),
    );
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
    let outcome = rig.call("mongo_aggregate", json!({"connection_id": MONGO, "database": "shop", "collection": "orders", "pipeline": [{"$match": {}}]}));
    assert_eq!(code(&outcome), "ok", "{outcome:?}");
}

#[test]
fn list_connections_says_where_a_connection_is_what_folder_it_is_in_and_the_users_note() {
    let rig = rig(&[(SQL, ConnectionKind::PostgreSql), ("hidden", ConnectionKind::MySql)], AgentAccess::Read);
    rig.catalog.update(SQL, |info| {
        info.endpoint = Some("db.internal:5432/shop".into());
        info.group = vec!["Work".into(), "Shop".into()];
        info.notes = Some("the orders database; read replica".into());
    });
    rig.catalog.update("hidden", |info| {
        info.agent_access = Some(AgentAccess::Off);
        info.endpoint = Some("secret.internal:3306/x".into());
        info.group = vec!["Private".into()];
        info.notes = Some("do not show".into());
    });
    let connections = ok(rig.call("list_connections", json!({})));
    let list = connections["connections"].as_array().unwrap();
    let shown = list.iter().find(|c| c["id"] == SQL).unwrap();
    assert_eq!(shown["endpoint"], "db.internal:5432/shop");
    assert_eq!(shown["group"], json!(["Work", "Shop"]));
    assert_eq!(shown["notes"], "the orders database; read replica");
    let hidden = list.iter().find(|c| c["id"] == "hidden").unwrap();
    for field in ["endpoint", "group", "notes"] {
        assert!(hidden.get(field).is_none(), "{field}: {hidden}");
    }
    assert!(!connections.to_string().contains("secret.internal"));
    // And it keeps to the output schema.
    engine_agent::schema::validate(&engine_agent::schema::spec(engine_agent::ToolName::ListConnections).output_schema, &connections).unwrap();
}

// ---------------------------------------------------------------- table comments

#[test]
fn list_tables_carries_the_comment_of_each_table() {
    let rig = rig(&[(SQL, ConnectionKind::PostgreSql)], AgentAccess::SchemaOnly);
    let description = |name: &str| -> TableDescription {
        serde_json::from_value(json!({"name": name, "columns": [], "primary_key": [], "unique_columns": [], "indexes": [], "foreign_keys": []})).unwrap()
    };
    *rig.session(SQL).tables.lock().unwrap() = vec![description("orders"), description("users"), description("tmp")];
    rig.session(SQL).table_comments.lock().unwrap().extend([
        ("orders".to_owned(), "One row per order.\nPaid or not.".to_owned()),
        ("users".to_owned(), "x".repeat(5000)),
    ]);
    rig.open(SQL);
    let listed = ok(rig.call("list_tables", json!({"connection_id": SQL, "database": "shop"})));
    let tables = listed["tables"].as_array().unwrap();
    let by = |name: &str| tables.iter().find(|t| t["name"] == name).unwrap();
    assert_eq!(by("orders")["comment"], "One row per order.\\nPaid or not.", "one line, the newline spelled out");
    assert!(by("users")["comment"].as_str().unwrap().chars().count() <= 500, "capped");
    assert!(by("tmp").get("comment").is_none());
    engine_agent::schema::validate(&engine_agent::schema::spec(engine_agent::ToolName::ListTables).output_schema, &listed).unwrap();
}

// -------------------------------------------------------------- schema digests

fn table(name: &str, columns: &[(&str, &str)]) -> TableDescription {
    let columns: Vec<Value> = columns
        .iter()
        .map(|(column, kind)| json!({"name": column, "type": kind, "nullable": true, "primary_key": *column == "id"}))
        .collect();
    serde_json::from_value(json!({"name": name, "columns": columns, "primary_key": ["id"], "unique_columns": [], "indexes": [], "foreign_keys": []})).unwrap()
}

#[test]
fn a_digest_can_be_limited_to_named_tables_and_to_a_number_of_them() {
    let rig = rig(&[(SQL, ConnectionKind::PostgreSql)], AgentAccess::SchemaOnly);
    *rig.session(SQL).tables.lock().unwrap() = (0..30).map(|n| table(&format!("t{n:02}"), &[("id", "int"), ("name", "text")])).collect();
    rig.open(SQL);
    let digest = |args: Value| {
        let mut request = json!({"connection_id": SQL, "database": "shop"});
        request.as_object_mut().unwrap().extend(args.as_object().unwrap().clone());
        ok(rig.call("get_schema_context", request))
    };
    assert_eq!(digest(json!({}))["tables_included"], 30);
    // Named tables only; a name that is not a table is reported, not an error.
    let named = digest(json!({"tables": ["t03", "t01", "nope", "t03"]}));
    assert_eq!(named["tables_included"], 2, "{named}");
    let text = named["text"].as_str().unwrap();
    assert!(text.contains("t01") && text.contains("t03") && !text.contains("t02"), "{text}");
    assert_eq!(named["omitted_tables"], json!(["nope"]));
    // A number: 1 to 20, clamped.
    assert_eq!(digest(json!({"max_tables": 5}))["tables_included"], 5);
    assert_eq!(digest(json!({"max_tables": 0}))["tables_included"], 1);
    assert_eq!(digest(json!({"max_tables": 500}))["tables_included"], 20);
    let capped = digest(json!({"max_tables": 3}));
    assert_eq!(capped["tables_total"], 30);
    assert_eq!(capped["omitted_tables"].as_array().unwrap().len(), 27);
    // Names are checked like every name the model gives.
    let bad = rig.call("get_schema_context", json!({"connection_id": SQL, "database": "shop", "tables": ["a\u{202e}b"]}));
    assert_eq!(code(&bad), "agent.invalid_arguments");
    let many: Vec<String> = (0..51).map(|n| format!("t{n}")).collect();
    assert_eq!(code(&rig.call("get_schema_context", json!({"connection_id": SQL, "database": "shop", "tables": many}))), "agent.invalid_arguments");
}

fn sampled_mongo(access: AgentAccess) -> Rig {
    let rig = rig(&[(MONGO, ConnectionKind::MongoDb)], access);
    rig.session(MONGO).set_mongo(|_, command, _| {
        let name = command.as_object().unwrap().keys().next().unwrap().as_str();
        assert_eq!(name, "find", "only a sample is read");
        let collection = command["find"].as_str().unwrap();
        let documents: Vec<bson::Document> = match collection {
            "orders" => vec![
                doc! {"_id": 1, "status": "paid", "total": 10, "customer": {"id": 7, "name": "a"}, "tags": ["x", "y"]},
                doc! {"_id": 2, "status": "open", "total": 12.5, "customer": {"id": 8}, "note": Bson::Null},
                doc! {"_id": 3, "status": "open", "total": 3},
            ],
            "empty" => vec![],
            "forbidden" => return Err(engine_agent::AgentError::driver("query.failed", "not authorized", false)),
            other => panic!("unexpected {other}"),
        };
        let reply = doc! {"cursor": {"id": 0_i64, "firstBatch": documents}, "ok": 1.0};
        Ok(Bson::Document(reply).into_canonical_extjson())
    });
    rig
}

#[test]
fn a_mongodb_digest_reads_the_fields_of_each_collection_from_a_sample_and_needs_read() {
    let rig = sampled_mongo(AgentAccess::Read);
    // The collections the fake lists.
    *rig.session(MONGO).tables.lock().unwrap() = ["orders", "empty", "forbidden"].iter().map(|name| table(name, &[])).collect();
    rig.open(MONGO);
    let digest = ok(rig.call("get_schema_context", json!({"connection_id": MONGO, "database": "shop"})));
    let text = digest["text"].as_str().unwrap();
    assert!(text.contains("orders") && text.contains("status") && text.contains("customer.id") && text.contains("string"), "{text}");
    assert!(text.contains("_id"), "{text}");
    // A field in only some documents is nullable; one that mixes types lists them.
    assert!(text.contains("int|double") || text.contains("double|int"), "{text}");
    assert_eq!(digest["tables_total"], 3);
    assert_eq!(digest["omitted_tables"], json!(["forbidden"]), "one collection that cannot be read does not sink the digest: {digest}");
    // Only `find` with a limit was sent, to the database asked for, read-only, one per collection.
    let sent = rig.session(MONGO).mongo_calls();
    assert_eq!(sent.len(), 3);
    assert!(sent.iter().all(|(database, command)| database == "shop" && command["limit"] == 20 && command["singleBatch"] == true), "{sent:?}");
    assert!(rig.session(MONGO).mongo_writes.lock().unwrap().iter().all(|write| !write));
    // `tables` and `max_tables` apply, and the digest keeps to its budget.
    let one = ok(rig.call("get_schema_context", json!({"connection_id": MONGO, "database": "shop", "tables": ["orders"]})));
    assert_eq!(one["tables_included"], 1);
    let small = ok(rig.call("get_schema_context", json!({"connection_id": MONGO, "database": "shop", "tables": ["orders"], "budget": 20})));
    assert_eq!(small["budget"], 20);
    assert!(small["estimated_tokens"].as_u64().unwrap() <= small["text"].as_str().unwrap().len() as u64);
}

#[test]
fn a_mongodb_digest_samples_documents_so_it_is_refused_below_read() {
    let rig = sampled_mongo(AgentAccess::SchemaOnly);
    rig.open(MONGO);
    let outcome = rig.call("get_schema_context", json!({"connection_id": MONGO, "database": "shop"}));
    assert_eq!(code(&outcome), "agent.access_denied", "{outcome:?}");
    assert!(outcome.to_text().contains("sample of its documents"), "{}", outcome.to_text());
    assert!(rig.session(MONGO).mongo_calls().is_empty());
}

// ---------------------------------------------------- transactions on MySQL and MariaDB

#[test]
fn a_transaction_on_mysql_and_mariadb_begins_runs_asks_for_the_commit_and_rolls_back() {
    for kind in [ConnectionKind::MySql, ConnectionKind::MariaDb] {
        let rig = rig(&[(MY, kind)], AgentAccess::ReadWrite);
        rig.open(MY);
        let began = ok(rig.call("begin_transaction", json!({"connection_id": MY})));
        let tx = began["transaction_id"].as_str().unwrap().to_owned();
        let run = |statement: &str| rig.call("execute_query", json!({"connection_id": MY, "transaction_id": tx, "statement": statement}));
        // A read inside a transaction runs in it as it is: the server cannot narrow a transaction
        // that is open to READ ONLY, so the driver is not asked to.
        assert_eq!(code(&run("SELECT * FROM a")), "ok", "{kind:?}");
        assert_eq!(code(&run("INSERT INTO a VALUES (1)")), "ok", "{kind:?}");
        assert_eq!(code(&run("UPDATE a SET b = 2 WHERE id = 1")), "ok");
        // What commits by itself is refused, whatever the access.
        for statement in ["CREATE TABLE b (id int)", "ALTER TABLE a ADD COLUMN c int", "DROP TABLE a", "TRUNCATE a", "RENAME TABLE a TO c", "GRANT SELECT ON a TO u", "ANALYZE TABLE a", "LOCK TABLES a READ", "BEGIN", "SET autocommit = 0"] {
            let outcome = run(statement);
            assert_ne!(code(&outcome), "ok", "{statement}");
        }
        for statement in ["CREATE TABLE b (id int)", "ALTER TABLE a ADD COLUMN c int", "DROP TABLE a", "TRUNCATE a", "RENAME TABLE a TO c", "GRANT SELECT ON a TO u", "ANALYZE TABLE a"] {
            let outcome = run(statement);
            assert_eq!(code(&outcome), "agent.unsupported", "{statement}: {outcome:?}");
            assert!(outcome.to_text().contains("COMMIT"), "{}", outcome.to_text());
        }
        let committed = ok(rig.call("commit_transaction", json!({"transaction_id": tx})));
        assert_eq!(committed["committed"], true, "{kind:?}: {committed}");
        assert_eq!(committed["changes"], 2);
        // The commit question listed both changes.
        let asked = rig.approver.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 3, "two writes and the commit: {asked:?}");
        // And a rollback.
        let began = ok(rig.call("begin_transaction", json!({"connection_id": MY})));
        let again = began["transaction_id"].as_str().unwrap().to_owned();
        assert_eq!(code(&rig.call("execute_query", json!({"connection_id": MY, "transaction_id": again, "statement": "DELETE FROM a WHERE id = 1"}))), "ok");
        let rolled = ok(rig.call("rollback_transaction", json!({"transaction_id": again})));
        assert_eq!(rolled["rolled_back"], true);
    }
}

#[test]
fn the_control_statements_of_a_mysql_transaction_reach_its_own_session() {
    let provider = FakeProvider::new();
    let shared = FakeSession::new(ConnectionKind::MariaDb).arc();
    let own = FakeSession::new(ConnectionKind::MariaDb).arc();
    provider.with_session(MY, Arc::clone(&shared));
    provider.with_extra_session(MY, Arc::clone(&own));
    let catalog = FakeCatalog::new(vec![connection(MY, ConnectionKind::MariaDb, Some(AgentAccess::ReadWrite))]);
    let layer = ToolLayer::new(catalog, provider, "test")
        .with_approver(ScriptedApprover::new(ApprovalDecision::Approve, true))
        .with_transactions();
    ok(layer.call("open_session", json!({"connection_id": MY})));
    let tx = ok(layer.call("begin_transaction", json!({"connection_id": MY})))["transaction_id"].as_str().unwrap().to_owned();
    ok(layer.call("execute_query", json!({"connection_id": MY, "transaction_id": tx, "statement": "SELECT 1"})));
    ok(layer.call("execute_query", json!({"connection_id": MY, "transaction_id": tx, "statement": "INSERT INTO a VALUES (1)"})));
    ok(layer.call("commit_transaction", json!({"transaction_id": tx})));
    assert_eq!(own.control_calls(), [TxControl::Begin, TxControl::Commit]);
    assert_eq!(*own.read_only_runs.lock().unwrap(), [false, false], "no READ ONLY scope inside a MySQL transaction");
    assert!(shared.runs.lock().unwrap().is_empty() && shared.control_calls().is_empty());
}

// ---------------------------------------------------------------------------- misc

#[test]
fn the_tool_specs_describe_the_new_parameters() {
    let specs = engine_agent::schema::tool_specs();
    let find = |name: &str| specs.iter().find(|spec| spec.name == name).unwrap();
    for tool in ["execute_query", "execute_batch"] {
        let properties = &find(tool).input_schema["properties"];
        assert!(properties.get("cell_char_offset").is_some() && properties.get("cell_char_limit").is_some(), "{tool}");
    }
    let context = &find("get_schema_context").input_schema["properties"];
    assert!(context.get("tables").is_some() && context.get("max_tables").is_some());
    assert!(find("get_schema_context").description.contains("sample"));
    for tool in ["execute_query", "execute_batch", "explain_query", "execute_redis_command", "mongo_find"] {
        assert!(find(tool).input_schema["properties"].get("session_id").is_some(), "{tool}");
    }
    assert!(find("close_session").input_schema["required"].as_array().unwrap().is_empty());
    assert!(find("open_session").description.contains("PINNED SESSION"));
}

#[test]
fn the_mongodb_and_redis_tools_run_on_a_pinned_session_too() {
    let rig = rig(&[(MONGO, ConnectionKind::MongoDb), ("rd", ConnectionKind::Redis)], AgentAccess::ReadWrite);
    let script = |_: &str, command: &Value, _: &engine_agent::CancelToken| {
        let name = command.as_object().unwrap().keys().next().unwrap().as_str().to_owned();
        let reply = match name.as_str() {
            "find" => doc! {"cursor": {"id": 0_i64, "firstBatch": [{"_id": 1}]}, "ok": 1.0},
            _ => doc! {"n": 1, "ok": 1.0},
        };
        Ok(Bson::Document(reply).into_canonical_extjson())
    };
    rig.session(MONGO).set_mongo(script);
    rig.session(&format!("{MONGO}#2")).set_mongo(script);
    rig.open(MONGO);
    let session = ok(rig.call("open_session", json!({"connection_id": MONGO, "pinned": true})))["session_id"].as_str().unwrap().to_owned();
    let found = ok(rig.call("mongo_find", json!({"connection_id": MONGO, "database": "shop", "collection": "c", "session_id": session})));
    assert_eq!(found["document_count"], 1);
    let shown = ok(rig.call("execute_query", json!({"connection_id": MONGO, "database": "shop", "statement": "db.c.countDocuments({})", "session_id": session})));
    assert_eq!(shown["rows"], json!([["1"]]));
    // The shared session of the connection never saw them; the pinned one did.
    assert!(rig.session(MONGO).mongo_calls().is_empty());
    assert_eq!(rig.session(&format!("{MONGO}#2")).mongo_calls().len(), 2);
    // A write is approved as ever.
    let before = rig.approver.asked.lock().unwrap().len();
    ok(rig.call("mongo_insert", json!({"connection_id": MONGO, "database": "shop", "collection": "c", "documents": [{"a": 1}], "session_id": session})));
    assert_eq!(rig.approver.asked.lock().unwrap().len(), before + 1);
    // Redis: a command runs on the pinned connection; SELECT stays refused (use `database`).
    rig.open("rd");
    let pinned = ok(rig.call("open_session", json!({"connection_id": "rd", "pinned": true})))["session_id"].as_str().unwrap().to_owned();
    let get = rig.call("execute_redis_command", json!({"connection_id": "rd", "command": "GET k", "session_id": pinned}));
    assert_eq!(code(&get), "ok", "{get:?}");
    assert_eq!(rig.session("rd#2").run_count(), 1);
    assert_eq!(rig.session("rd").run_count(), 0);
    let select = rig.call("execute_redis_command", json!({"connection_id": "rd", "command": "SELECT 3", "session_id": pinned}));
    assert_eq!(code(&select), "agent.unsupported", "{select:?}");
}
