//! `mongo_find`, `mongo_aggregate` and `mongo_count` against a fake server that speaks the
//! driver's command replies (canonical Extended JSON): the commands built, paging and cursor
//! cleanup, the limits, errors, access, cancel and timeout.

use bson::{doc, oid::ObjectId, Bson, DateTime, Document};
use engine_agent::testing::{connection, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{
    read_entries, AgentAccess, AgentError, AuditConfig, AuditLog, CancelToken, ConnectionKind,
    Limits, SecretText, ToolLayer, ToolName, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MONGO: &str = "mongo1";
const SQL: &str = "sql1";

/// A collection behind a fake server with a first-batch cap, like a server whose reply would
/// pass its size limit: a `find` or `aggregate` returns at most `first_batch` documents even when
/// asked for more, and leaves a cursor open while documents remain within the command's limit.
struct Server {
    documents: Vec<Document>,
    first_batch: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Next document to hand out, and where the open cursor's results end.
    next: usize,
    end: usize,
    killed: Vec<i64>,
}

const CURSOR: i64 = 77;

impl Server {
    fn new(documents: Vec<Document>) -> Arc<Self> {
        Arc::new(Self {
            documents,
            first_batch: usize::MAX,
            state: Mutex::default(),
        })
    }

    fn with_first_batch(documents: Vec<Document>, first_batch: usize) -> Arc<Self> {
        Arc::new(Self {
            documents,
            first_batch,
            state: Mutex::default(),
        })
    }

    fn reply(&self, command: &Value) -> Value {
        let name = command.as_object().unwrap().keys().next().unwrap().as_str();
        let mut state = self.state.lock().unwrap();
        let wanted = |value: &Value| value.as_u64().map_or(usize::MAX, |n| n as usize);
        let batch = |state: &mut State, key: &str, size: usize| {
            let take = size.min(state.end - state.next);
            let documents: Vec<Bson> = self.documents[state.next..state.next + take]
                .iter()
                .cloned()
                .map(Bson::Document)
                .collect();
            state.next += take;
            let id = if state.next < state.end { CURSOR } else { 0 };
            Bson::Document(doc! {
                "cursor": { key: documents, "id": id, "ns": "shop.orders" },
                "ok": 1.0,
            })
            .into_canonical_extjson()
        };
        match name {
            "find" => {
                state.next = command.get("skip").map_or(0, wanted).min(self.documents.len());
                let limit = command.get("limit").map_or(usize::MAX, wanted);
                state.end = self.documents.len().min(state.next.saturating_add(limit));
                let size = wanted(&command["batchSize"]).min(self.first_batch);
                batch(&mut state, "firstBatch", size)
            }
            "aggregate" => {
                let stages = command["pipeline"].as_array().unwrap();
                let limit = stages.last().unwrap()["$limit"].as_u64().unwrap() as usize;
                state.next = 0;
                state.end = self.documents.len().min(limit);
                let size = wanted(&command["cursor"]["batchSize"]).min(self.first_batch);
                batch(&mut state, "firstBatch", size)
            }
            "getMore" => {
                assert_eq!(command["getMore"], json!({"$numberLong": "77"}));
                assert_eq!(command["collection"], "orders");
                let size = wanted(&command["batchSize"]);
                batch(&mut state, "nextBatch", size)
            }
            "killCursors" => {
                state.killed.push(
                    command["cursors"][0]["$numberLong"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap(),
                );
                Bson::Document(doc! { "cursorsKilled": [CURSOR], "ok": 1.0 }).into_canonical_extjson()
            }
            "count" => {
                Bson::Document(doc! { "n": self.documents.len() as i32, "ok": 1.0 })
                    .into_canonical_extjson()
            }
            other => panic!("unexpected command {other}"),
        }
    }

    fn killed(&self) -> Vec<i64> {
        self.state.lock().unwrap().killed.clone()
    }
}

struct Rig {
    layer: ToolLayer,
    mongo: Arc<FakeSession>,
    catalog: Arc<FakeCatalog>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(access: Option<AgentAccess>, limits: Limits, secrets: Vec<SecretText>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let mut mongo = FakeSession::new(ConnectionKind::MongoDb);
    mongo.secrets = secrets;
    let mongo = mongo.arc();
    let sql = FakeSession::new(ConnectionKind::PostgreSql).arc();
    let provider = FakeProvider::new();
    provider.with_session(MONGO, Arc::clone(&mongo));
    provider.with_session(SQL, sql);
    let catalog = FakeCatalog::new(vec![
        connection(MONGO, ConnectionKind::MongoDb, access),
        connection(SQL, ConnectionKind::PostgreSql, access),
    ]);
    let layer = ToolLayer::new(catalog.clone(), provider, "test")
        .with_audit(Arc::clone(&audit))
        .with_limits(limits);
    Rig {
        layer,
        mongo,
        catalog,
        audit,
        audit_path,
        _dir: dir,
    }
}

impl Rig {
    fn serve(&self, server: &Arc<Server>) {
        let server = Arc::clone(server);
        self.mongo
            .set_mongo(move |_, command, _| Ok(server.reply(command)));
    }

    fn open(&self) {
        let opened = self
            .layer
            .call("open_session", json!({"connection_id": MONGO}));
        assert!(!opened.is_error(), "{opened:?}");
        assert_eq!(
            opened.to_json()["language"],
            "mongo",
            "open_session tells the model which tools to use"
        );
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

    /// The commands that reached the server, as compact JSON text (key order matters).
    fn commands(&self) -> Vec<String> {
        self.mongo
            .mongo_calls()
            .iter()
            .map(|(_, command)| command.to_string())
            .collect()
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn orders(count: usize) -> Vec<Document> {
    (0..count)
        .map(|n| doc! { "_id": n as i32, "status": "paid", "total": n as f64 * 1.5 })
        .collect()
}

fn find(args: Value) -> Value {
    let mut request = json!({"connection_id": MONGO, "database": "shop", "collection": "orders"});
    request.as_object_mut().unwrap().extend(args.as_object().unwrap().clone());
    request
}

// ------------------------------------------------------------------ commands

#[test]
fn find_sends_the_filter_projection_sort_skip_and_limit_in_the_order_given() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(30)));
    let result = rig.ok(
        "mongo_find",
        find(json!({
            "filter": {"status": "paid", "total": {"$gt": 1}},
            "projection": {"status": 1, "total": 1},
            "sort": {"total": -1, "_id": 1},
            "skip": 5,
            "limit": 10,
        })),
    );
    assert_eq!(
        rig.commands(),
        [r#"{"find":"orders","filter":{"status":"paid","total":{"$gt":1}},"projection":{"status":1,"total":1},"sort":{"total":-1,"_id":1},"skip":5,"limit":11,"batchSize":11}"#]
    );
    let (database, _) = &rig.mongo.mongo_calls()[0];
    assert_eq!(database, "shop");
    // The server's time limit is the statement timeout.
    assert_eq!(rig.mongo.mongo_max_times.lock().unwrap()[0], Duration::from_secs(30));
    assert_eq!(result["document_count"], 10);
    assert_eq!(result["limit"], 10);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["truncation"], "row_limit");
}

#[test]
fn find_without_options_sends_only_the_default_limit() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(3)));
    let result = rig.ok("mongo_find", find(json!({"skip": 0})));
    assert_eq!(
        rig.commands(),
        [r#"{"find":"orders","limit":201,"batchSize":201}"#]
    );
    assert_eq!(result["document_count"], 3);
    assert_eq!(result["limit"], 200);
    assert_eq!(result["truncated"], false);
    assert!(result.get("truncation").is_none());
}

#[test]
fn the_limit_is_capped_by_the_row_limit_of_the_layer() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(3)));
    let result = rig.ok("mongo_find", find(json!({"limit": 50000})));
    assert_eq!(result["limit"], 1000);
    assert_eq!(
        rig.commands(),
        [r#"{"find":"orders","limit":1001,"batchSize":1001}"#]
    );
    let limits = Limits {
        max_rows: 7,
        ..Limits::default()
    };
    let small = self::rig(Some(AgentAccess::Read), limits, vec![]);
    small.open();
    small.serve(&Server::new(orders(30)));
    let result = small.ok("mongo_find", find(json!({})));
    assert_eq!(result["limit"], 7);
    assert_eq!(result["document_count"], 7);
    assert_eq!(result["truncated"], true);
}

#[test]
fn aggregate_appends_a_limit_stage_and_sets_the_batch_size() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(30)));
    let pipeline = json!([
        {"$match": {"status": "paid"}},
        {"$group": {"_id": "$status", "total": {"$sum": "$total"}}},
    ]);
    let result = rig.ok(
        "mongo_aggregate",
        find(json!({"pipeline": pipeline, "limit": 4})),
    );
    assert_eq!(
        rig.commands(),
        [r#"{"aggregate":"orders","pipeline":[{"$match":{"status":"paid"}},{"$group":{"_id":"$status","total":{"$sum":"$total"}}},{"$limit":5}],"cursor":{"batchSize":5}}"#]
    );
    assert_eq!(result["document_count"], 4);
    assert_eq!(result["truncated"], true);
    // Without a limit: the default of 200 (one more is asked for).
    let again = rig.ok("mongo_aggregate", find(json!({"pipeline": []})));
    assert_eq!(again["limit"], 200);
    assert!(rig.commands()[1].contains(r#""pipeline":[{"$limit":201}]"#));
}

#[test]
fn count_sends_the_filter_as_the_query() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(12)));
    let counted = rig.ok("mongo_count", find(json!({"filter": {"status": "paid"}})));
    assert_eq!(counted["count"], 12);
    assert!(counted["elapsed_ms"].is_u64());
    rig.ok("mongo_count", find(json!({})));
    assert_eq!(
        rig.commands(),
        [
            r#"{"count":"orders","query":{"status":"paid"}}"#,
            r#"{"count":"orders"}"#
        ]
    );
}

// ------------------------------------------------------------------ results

#[test]
fn documents_come_back_as_relaxed_extended_json() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    let id = ObjectId::parse_str("65b9f0a1c2d3e4f506172839").unwrap();
    let at = DateTime::parse_rfc3339_str("2024-01-31T10:20:30Z").unwrap();
    rig.serve(&Server::new(vec![doc! {
        "_id": id,
        "placed": at,
        "items": 3_i64,
        "total": 12.5,
        "tags": ["a", "b"],
        "ship": { "city": "Oslo" },
        "note": Bson::Null,
    }]));
    let result = rig.ok("mongo_find", find(json!({})));
    assert_eq!(
        result["documents"][0],
        json!({
            "_id": {"$oid": "65b9f0a1c2d3e4f506172839"},
            "placed": {"$date": "2024-01-31T10:20:30Z"},
            "items": 3,
            "total": 12.5,
            "tags": ["a", "b"],
            "ship": {"city": "Oslo"},
            "note": null,
        })
    );
    assert_eq!(result["document_count"], 1);
}

#[test]
fn a_first_batch_smaller_than_the_limit_is_continued_with_get_more() {
    // The server hands out 2 documents per batch: the limit 3 needs a getMore for the rest, and
    // the 4th document (which only shows that the result was cut) ends the server's cursor too.
    let server = Server::with_first_batch(orders(10), 2);
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&server);
    let result = rig.ok("mongo_find", find(json!({"limit": 3})));
    assert_eq!(result["document_count"], 3);
    assert_eq!(result["truncated"], true);
    assert_eq!(
        rig.commands(),
        [
            r#"{"find":"orders","limit":4,"batchSize":4}"#,
            r#"{"getMore":{"$numberLong":"77"},"collection":"orders","batchSize":2}"#,
        ]
    );
    assert!(server.killed().is_empty(), "the server closed the cursor itself");
}

#[test]
fn an_exhausted_cursor_is_not_closed_again() {
    let server = Server::with_first_batch(orders(3), 2);
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&server);
    let result = rig.ok("mongo_find", find(json!({"limit": 5})));
    assert_eq!(result["document_count"], 3);
    assert_eq!(result["truncated"], false);
    let commands = rig.commands();
    assert_eq!(commands.len(), 2, "find and one getMore: {commands:?}");
    assert!(server.killed().is_empty());
    // A result that fits the first batch needs no getMore either.
    let rig = self::rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(2)));
    rig.ok("mongo_find", find(json!({})));
    assert_eq!(rig.commands().len(), 1);
}

#[test]
fn the_byte_limit_cuts_the_result_and_closes_the_cursor() {
    let limits = Limits {
        max_result_bytes: 100,
        ..Limits::default()
    };
    let server = Server::with_first_batch(orders(10), 4);
    let rig = rig(Some(AgentAccess::Read), limits, vec![]);
    rig.open();
    rig.serve(&server);
    let result = rig.ok("mongo_find", find(json!({"limit": 8})));
    let count = result["document_count"].as_u64().unwrap();
    assert!((1..8).contains(&count), "{result}");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["truncation"], "byte_limit");
    assert_eq!(server.killed(), [77], "the open cursor was closed");
    assert_eq!(
        rig.commands().last().unwrap(),
        r#"{"killCursors":"orders","cursors":[{"$numberLong":"77"}]}"#
    );
    let size: usize = result["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|document| document.to_string().len())
        .sum();
    assert!(size <= 100, "{size} bytes for {count} documents");
}

#[test]
fn long_strings_are_cut_and_a_document_over_the_cap_is_replaced() {
    let limits = Limits {
        max_cell_bytes: 20,
        max_result_bytes: 400,
        ..Limits::default()
    };
    let rig = rig(Some(AgentAccess::Read), limits, vec![]);
    rig.open();
    let huge: Vec<String> = (0..200).map(|n| format!("item-{n}")).collect();
    rig.serve(&Server::new(vec![
        doc! { "_id": 1, "note": "n".repeat(100) },
        doc! { "_id": 2, "items": huge },
    ]));
    let result = rig.ok("mongo_find", find(json!({})));
    let first = result["documents"][0]["note"].as_str().unwrap();
    assert!(first.starts_with(&"n".repeat(20)) && first.ends_with("[truncated, 100 bytes in total]"), "{first}");
    let second = result["documents"][1]["$truncated"].as_str().unwrap();
    assert!(second.starts_with(r#"{"_id":2,"items":["#) && second.contains("bytes in total]"), "{second}");
    assert_eq!(result["cells_truncated"], 2);
    assert_eq!(result["document_count"], 2);
    assert_eq!(result["truncated"], false);
}

// ------------------------------------------------------------------ errors

#[test]
fn a_server_refusal_is_an_error_with_its_code_name() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.mongo.set_mongo(|_, _, _| {
        Ok(Bson::Document(doc! {
            "ok": 0.0, "errmsg": "unknown top level operator: $nope", "code": 2, "codeName": "BadValue",
        })
        .into_canonical_extjson())
    });
    let outcome = rig.call("mongo_find", find(json!({"filter": {"$nope": 1}})));
    assert_eq!(code(&outcome), "query.failed", "{outcome:?}");
    assert!(
        outcome.to_text().contains("BadValue: unknown top level operator: $nope"),
        "{outcome:?}"
    );
    // The same for a count.
    assert_eq!(code(&rig.call("mongo_count", find(json!({})))), "query.failed");
    assert_eq!(rig.mongo.mongo_calls().len(), 2, "no cursor to close");
}

#[test]
fn a_server_time_limit_is_a_limit_error() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.mongo.set_mongo(|_, _, _| {
        Ok(Bson::Document(doc! {
            "ok": 0.0, "errmsg": "operation exceeded time limit", "code": 50, "codeName": "MaxTimeMSExpired",
        })
        .into_canonical_extjson())
    });
    let outcome = rig.call("mongo_count", find(json!({})));
    assert_eq!(code(&outcome), "agent.limit", "{outcome:?}");
    assert!(outcome.to_text().contains("statement timeout"));
}

#[test]
fn a_reply_that_is_no_command_reply_is_an_error() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.mongo.set_mongo(|_, _, _| Ok(json!({"ok": {"$numberDouble": "1.0"}})));
    assert_eq!(code(&rig.call("mongo_find", find(json!({})))), "query.malformed");
    rig.mongo.set_mongo(|_, _, _| Ok(json!("nothing")));
    assert_eq!(code(&rig.call("mongo_count", find(json!({})))), "query.malformed");
}

#[test]
fn a_driver_error_passes_through_and_a_secret_in_it_is_masked() {
    let secret = SecretText::new("s3cret-pw-value");
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![secret]);
    rig.open();
    rig.mongo.set_mongo(|_, _, _| {
        Err(AgentError::driver(
            "query.failed",
            "connection to user:s3cret-pw-value@db.example.test lost",
            true,
        ))
    });
    let outcome = rig.call("mongo_find", find(json!({})));
    assert_eq!(code(&outcome), "query.failed");
    assert!(!outcome.to_text().contains("s3cret-pw-value"), "{outcome:?}");
    assert!(outcome.to_text().contains("***"), "{outcome:?}");
}

#[test]
fn a_session_without_mongo_commands_says_so() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    // No script: the fake behaves like a session that does not implement the command.
    let outcome = rig.call("mongo_find", find(json!({})));
    assert_eq!(code(&outcome), "agent.unsupported", "{outcome:?}");
}

// ------------------------------------------------------------------ requests

#[test]
fn bad_requests_are_refused_before_anything_is_sent() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(1)));
    let bad = |tool: &str, args: Value| code(&rig.call(tool, args)).to_owned();
    let invalid = "agent.invalid_arguments";
    assert_eq!(bad("mongo_find", find(json!({"limit": 0}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"limit": -1}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"skip": -1}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"skip": 1.5}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"filter": "{}"}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"filter": [1]}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"sort": 5}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"projection": "name"}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"surprise": 1}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"collection": ""}))), invalid);
    assert_eq!(bad("mongo_find", find(json!({"database": ""}))), invalid);
    assert_eq!(
        bad("mongo_find", json!({"connection_id": MONGO, "collection": "orders"})),
        invalid,
        "database is required"
    );
    assert_eq!(
        bad("mongo_find", json!({"connection_id": MONGO, "database": "shop"})),
        invalid,
        "collection is required"
    );
    assert_eq!(bad("mongo_aggregate", find(json!({}))), invalid, "pipeline is required");
    assert_eq!(bad("mongo_aggregate", find(json!({"pipeline": {"$match": {}}}))), invalid);
    assert_eq!(bad("mongo_aggregate", find(json!({"pipeline": ["$match"]}))), invalid);
    assert_eq!(bad("mongo_aggregate", find(json!({"pipeline": [], "limit": 0}))), invalid);
    // A stage that is no BSON (an extended-JSON value that does not parse) is refused, not skipped: a
    // skipped stage is one whose databases were never looked at.
    assert_eq!(
        bad("mongo_aggregate", find(json!({"pipeline": [{"$match": {"_id": {"$oid": "not-an-oid"}}}]}))),
        invalid
    );
    assert_eq!(bad("mongo_count", find(json!({"filter": 1}))), invalid);
    assert_eq!(bad("mongo_count", find(json!({"limit": 1}))), invalid);
    assert!(rig.mongo.mongo_calls().is_empty(), "nothing reached the server");
    // An explicit null means "not given".
    rig.ok("mongo_find", find(json!({"filter": null, "limit": null})));
}

#[test]
fn a_request_over_the_statement_size_limit_is_refused() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(1)));
    let big = "x".repeat(70 * 1024);
    let outcome = rig.call("mongo_find", find(json!({"filter": {"name": big}})));
    assert_eq!(code(&outcome), "agent.limit", "{outcome:?}");
    assert!(rig.mongo.mongo_calls().is_empty());
}

#[test]
fn the_tools_are_for_mongodb_connections_and_the_sql_tools_point_to_them() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.ok("open_session", json!({"connection_id": SQL}));
    for tool in ["mongo_find", "mongo_count"] {
        let args = json!({"connection_id": SQL, "database": "shop", "collection": "orders"});
        let outcome = rig.call(tool, args);
        assert_eq!(code(&outcome), "agent.unsupported", "{tool}");
        assert!(outcome.to_text().contains("use execute_query"), "{}", outcome.to_text());
    }
    let outcome = rig.call("mongo_aggregate", json!({"connection_id": SQL, "database": "shop", "collection": "orders", "pipeline": []}));
    assert_eq!(code(&outcome), "agent.unsupported");
    for (tool, args) in [
        ("explain_query", json!({"connection_id": MONGO, "statement": "SELECT 1"})),
        ("list_routines", json!({"connection_id": MONGO, "kind": "function", "database": "shop"})),
    ] {
        let outcome = rig.call(tool, args);
        assert_eq!(code(&outcome), "agent.unsupported", "{tool}");
        assert!(
            outcome.to_text().contains("mongo_find, mongo_aggregate or mongo_count"),
            "{tool}: {}",
            outcome.to_text()
        );
    }
    // `execute_query` takes a shell command there, and says what it takes instead of SQL.
    let outcome = rig.call("execute_query", json!({"connection_id": MONGO, "statement": "SELECT 1"}));
    assert_eq!(code(&outcome), "agent.invalid_arguments");
    assert!(outcome.to_text().contains("starts with `db.`"), "{}", outcome.to_text());
}

// ------------------------------------------------------------------ access

fn tools() -> [(&'static str, Value); 3] {
    [
        ("mongo_find", find(json!({}))),
        ("mongo_aggregate", find(json!({"pipeline": []}))),
        ("mongo_count", find(json!({}))),
    ]
}

#[test]
fn the_tools_need_read_access() {
    for tool in [ToolName::MongoFind, ToolName::MongoAggregate, ToolName::MongoCount] {
        assert_eq!(tool.required_access(), AgentAccess::Read);
    }
    for (access, allowed) in [
        (None, false),
        (Some(AgentAccess::Off), false),
        (Some(AgentAccess::SchemaOnly), false),
        (Some(AgentAccess::Read), true),
        (Some(AgentAccess::ReadWrite), true),
    ] {
        let rig = rig(access, Limits::default(), vec![]);
        rig.serve(&Server::new(orders(2)));
        if access.is_some_and(|access| access >= AgentAccess::SchemaOnly) {
            rig.open();
        }
        for (tool, args) in tools() {
            let outcome = rig.call(tool, args);
            if allowed {
                assert!(!outcome.is_error(), "{tool} with {access:?}: {outcome:?}");
            } else {
                assert_eq!(code(&outcome), "agent.access_denied", "{tool} with {access:?}");
                let text = outcome.to_text();
                assert!(text.contains(&format!("`{tool}` needs `read` or more")), "{text}");
            }
        }
        let sent = rig.mongo.mongo_calls().len();
        assert_eq!(sent > 0, allowed, "{access:?}: {sent} commands");
    }
}

#[test]
fn lowering_the_access_stops_the_tools_at_once() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(2)));
    rig.ok("mongo_count", find(json!({})));
    rig.catalog
        .update(MONGO, |info| info.agent_access = Some(AgentAccess::SchemaOnly));
    assert_eq!(code(&rig.call("mongo_count", find(json!({})))), "agent.access_denied");
    assert_eq!(rig.mongo.mongo_calls().len(), 1);
}

#[test]
fn a_tool_without_an_open_session_asks_for_one() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    let outcome = rig.call("mongo_find", find(json!({})));
    assert_eq!(code(&outcome), "agent.not_connected");
    assert!(outcome.to_text().contains("open_session"));
    let outcome = rig.call("mongo_find", find(json!({"connection_id": "nope"})));
    assert_eq!(code(&outcome), "agent.invalid_arguments");
}

// ------------------------------------------------------------------ cancel and timeout

#[test]
fn a_cancel_stops_the_read_and_closes_the_cursor_it_left_open() {
    let rig = Arc::new(rig(Some(AgentAccess::Read), Limits::default(), vec![]));
    rig.open();
    let server = Server::with_first_batch(orders(10), 1);
    let seen = Arc::new(Mutex::new(Vec::<bool>::new()));
    {
        let (server, seen) = (Arc::clone(&server), Arc::clone(&seen));
        rig.mongo.set_mongo(move |_, command, cancel| {
            if command.get("getMore").is_some() {
                // The server does not answer until the call is cancelled.
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                return Err(AgentError::Cancelled);
            }
            seen.lock().unwrap().push(cancel.is_cancelled());
            Ok(server.reply(command))
        });
    }
    let token = CancelToken::new();
    let canceller = {
        let (rig, token) = (Arc::clone(&rig), token.clone());
        std::thread::spawn(move || {
            while !rig.commands().iter().any(|command| command.contains("getMore")) {
                std::thread::sleep(Duration::from_millis(2));
            }
            token.cancel();
        })
    };
    let outcome = rig
        .layer
        .call_cancellable("mongo_find", find(json!({"limit": 5})), &token);
    canceller.join().unwrap();
    assert_eq!(code(&outcome), "agent.cancelled", "{outcome:?}");
    let commands = rig.commands();
    assert!(commands.last().unwrap().starts_with(r#"{"killCursors":"orders""#), "{commands:?}");
    // The cleanup ran on a token of its own, not the cancelled one.
    assert_eq!(seen.lock().unwrap().as_slice(), [false, false]);
    assert_eq!(server.killed(), [77]);
    // The session is usable again.
    rig.serve(&server);
    rig.ok("mongo_count", find(json!({})));
}

#[test]
fn a_call_cancelled_before_it_starts_sends_nothing() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(1)));
    let token = CancelToken::new();
    token.cancel();
    let outcome = rig.layer.call_cancellable("mongo_count", find(json!({})), &token);
    assert_eq!(code(&outcome), "agent.cancelled");
    assert!(rig.mongo.mongo_calls().is_empty());
}

#[test]
fn the_statement_timeout_ends_a_command_that_never_answers() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.catalog
        .update(MONGO, |info| info.statement_timeout_secs = Some(1));
    rig.open();
    rig.mongo.set_mongo(|_, _, cancel| {
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(AgentError::Cancelled)
    });
    let started = Instant::now();
    let outcome = rig.call("mongo_find", find(json!({})));
    assert_eq!(code(&outcome), "agent.limit", "{outcome:?}");
    assert!(outcome.to_text().contains("1 s statement timeout"), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // The server was given the same limit.
    assert_eq!(rig.mongo.mongo_max_times.lock().unwrap()[0], Duration::from_secs(1));
}

// ------------------------------------------------------------------ audit

#[test]
fn the_audit_log_records_the_command_the_database_and_the_count() {
    let rig = rig(Some(AgentAccess::Read), Limits::default(), vec![]);
    rig.open();
    rig.serve(&Server::new(orders(4)));
    rig.ok("mongo_find", find(json!({"filter": {"status": "paid"}})));
    rig.audit.flush();
    let entries = read_entries(&rig.audit_path, 100).unwrap().entries;
    let entry = entries.iter().find(|entry| entry.tool == "mongo_find").unwrap();
    assert_eq!(entry.outcome, "ok");
    assert_eq!(entry.rows, Some(4));
    assert_eq!(entry.connection_id.as_deref(), Some(MONGO));
    assert_eq!(
        entry.statement.as_deref(),
        Some(r#"shop {"find":"orders","filter":{"status":"paid"},"limit":201,"batchSize":201}"#)
    );
    assert!(entry.approval.is_none());
}
