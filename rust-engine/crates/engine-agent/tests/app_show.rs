//! `open_table` and `execute_and_show`: only on a layer built with an app port, access gating,
//! what reaches the app, and the rule that only one read is run from here.

use engine_agent::testing::{connection, FakeCatalog, FakeProvider};
use engine_agent::{AgentAccess, AgentError, AppShow, ConnectionKind, ToolLayer, ToolOutcome};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const PG: &str = "pg1";
const MONGO: &str = "mongo1";
const REDIS: &str = "redis1";

#[derive(Debug, PartialEq, Eq)]
enum Shown {
    Open(String, String, Option<String>, String),
    Query(String, Option<String>, String, bool),
}

struct FakeApp {
    shown: Mutex<Vec<Shown>>,
    /// What the app answers (`None`: yes).
    answer: Mutex<Option<AgentError>>,
}

impl FakeApp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shown: Mutex::new(Vec::new()),
            answer: Mutex::new(None),
        })
    }

    fn reply(&self) -> Result<(), AgentError> {
        match &*self.answer.lock().unwrap() {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

impl AppShow for FakeApp {
    fn open_object(
        &self,
        connection_id: &str,
        database: &str,
        schema: Option<&str>,
        object: &str,
    ) -> Result<(), AgentError> {
        self.shown.lock().unwrap().push(Shown::Open(
            connection_id.into(),
            database.into(),
            schema.map(str::to_owned),
            object.into(),
        ));
        self.reply()
    }

    fn show_query(
        &self,
        connection_id: &str,
        database: Option<&str>,
        text: &str,
        run: bool,
    ) -> Result<(), AgentError> {
        self.shown.lock().unwrap().push(Shown::Query(
            connection_id.into(),
            database.map(str::to_owned),
            text.into(),
            run,
        ));
        self.reply()
    }
}

fn layer(access: AgentAccess, app: Option<&Arc<FakeApp>>) -> ToolLayer {
    let catalog = FakeCatalog::new(vec![
        connection(PG, ConnectionKind::PostgreSql, Some(access)),
        connection(MONGO, ConnectionKind::MongoDb, Some(access)),
        connection(REDIS, ConnectionKind::Redis, Some(access)),
    ]);
    let layer = ToolLayer::new(catalog, FakeProvider::new(), "test");
    match app {
        Some(app) => layer.with_app(app.clone()),
        None => layer,
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn ok(outcome: ToolOutcome) -> Value {
    match outcome {
        ToolOutcome::Success(value) => value,
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
}

fn names(layer: &ToolLayer) -> Vec<&'static str> {
    layer.tools().iter().map(|tool| tool.name).collect()
}

#[test]
fn the_tools_exist_only_on_a_layer_with_an_app() {
    let without = layer(AgentAccess::ReadWrite, None);
    assert_eq!(names(&without).len(), 19);
    for tool in ["open_table", "execute_and_show"] {
        assert!(!names(&without).contains(&tool));
        assert_eq!(
            code(&without.call(tool, json!({"connection_id": PG, "query": "SELECT 1", "database": "shop", "object": "orders"}))),
            "agent.unknown_tool"
        );
    }
    let app = FakeApp::new();
    let with = layer(AgentAccess::ReadWrite, Some(&app));
    assert_eq!(names(&with).len(), 21);
    assert_eq!(&names(&with)[19..], ["open_table", "execute_and_show"]);
    // With transactions too, in either order.
    let both = layer(AgentAccess::ReadWrite, Some(&app)).with_transactions();
    assert_eq!(names(&both).len(), 24);
    assert!(app.shown.lock().unwrap().is_empty());
}

#[test]
fn open_table_needs_schema_only_and_sends_the_object_to_the_app() {
    let app = FakeApp::new();
    let args = json!({"connection_id": PG, "database": "shop", "schema": "public", "object": "orders"});
    let off = layer(AgentAccess::Off, Some(&app));
    assert_eq!(code(&off.call("open_table", args.clone())), "agent.access_denied");
    assert!(app.shown.lock().unwrap().is_empty());
    let layer = layer(AgentAccess::SchemaOnly, Some(&app));
    let result = ok(layer.call("open_table", args));
    assert_eq!(result, json!({"connection_id": PG, "object": "orders", "requested": true}));
    // The schema is optional.
    ok(layer.call("open_table", json!({"connection_id": MONGO, "database": "shop", "object": "items"})));
    assert_eq!(
        *app.shown.lock().unwrap(),
        [
            Shown::Open(PG.into(), "shop".into(), Some("public".into()), "orders".into()),
            Shown::Open(MONGO.into(), "shop".into(), None, "items".into()),
        ]
    );
    // Missing and unknown arguments, and an unknown connection.
    for bad in [
        json!({"connection_id": PG, "object": "orders"}),
        json!({"connection_id": PG, "database": "shop", "object": "orders", "x": 1}),
        json!({"connection_id": "nope", "database": "shop", "object": "orders"}),
        json!({"connection_id": PG, "database": "shop", "object": "or\u{200b}ders"}),
    ] {
        assert_eq!(code(&layer.call("open_table", bad)), "agent.invalid_arguments");
    }
    assert_eq!(app.shown.lock().unwrap().len(), 2);
}

#[test]
fn execute_and_show_needs_read_and_runs_one_read() {
    let app = FakeApp::new();
    let args = json!({"connection_id": PG, "query": "SELECT * FROM orders", "database": "shop"});
    for access in [AgentAccess::Off, AgentAccess::SchemaOnly] {
        let layer = layer(access, Some(&app));
        assert_eq!(code(&layer.call("execute_and_show", args.clone())), "agent.access_denied", "{access:?}");
    }
    assert!(app.shown.lock().unwrap().is_empty());
    let layer = layer(AgentAccess::Read, Some(&app));
    // `run` defaults to true.
    let result = ok(layer.call("execute_and_show", args.clone()));
    assert_eq!(result, json!({"connection_id": PG, "shown": true, "run": true}));
    let result = ok(layer.call(
        "execute_and_show",
        json!({"connection_id": PG, "query": "SELECT 1", "run": false}),
    ));
    assert_eq!(result["run"], false);
    assert_eq!(
        *app.shown.lock().unwrap(),
        [
            Shown::Query(PG.into(), Some("shop".into()), "SELECT * FROM orders".into(), true),
            Shown::Query(PG.into(), None, "SELECT 1".into(), false),
        ]
    );
}

#[test]
fn a_query_that_would_change_data_is_only_shown_never_run() {
    for access in [AgentAccess::Read, AgentAccess::ReadWrite] {
        let app = FakeApp::new();
        let layer = layer(access, Some(&app));
        for query in [
            "DELETE FROM orders WHERE id = 1",
            "UPDATE orders SET paid = true",
            "DROP TABLE orders",
            "SELECT 1; SELECT 2",
            "BEGIN",
            "SELECT pg_read_file('/etc/passwd')",
        ] {
            let outcome = layer.call("execute_and_show", json!({"connection_id": PG, "query": query}));
            assert!(
                matches!(code(&outcome), "agent.write_refused" | "agent.dangerous_refused" | "agent.multi_statement" | "agent.unsupported" | "agent.blocked_function"),
                "{access:?} {query}: {}",
                code(&outcome)
            );
            // Shown for the user to run: fine, and the app is told not to run it.
            ok(layer.call("execute_and_show", json!({"connection_id": PG, "query": query, "run": false})));
        }
        assert!(app.shown.lock().unwrap().iter().all(|shown| matches!(shown, Shown::Query(_, _, _, false))));
        // MongoDB cannot be judged: shown only.
        let outcome = layer.call("execute_and_show", json!({"connection_id": MONGO, "query": "db.items.find({})"}));
        assert_eq!(code(&outcome), "agent.write_refused");
        // Redis: a read runs, a write does not.
        ok(layer.call("execute_and_show", json!({"connection_id": REDIS, "query": "GET k"})));
        let outcome = layer.call("execute_and_show", json!({"connection_id": REDIS, "query": "SET k v"}));
        assert_eq!(code(&outcome), "agent.write_refused");
    }
}

#[test]
fn an_empty_or_malformed_query_is_refused_before_the_app_hears_of_it() {
    let app = FakeApp::new();
    let layer = layer(AgentAccess::ReadWrite, Some(&app));
    for args in [
        json!({"connection_id": PG, "query": "   "}),
        json!({"connection_id": PG, "query": "SELECT 1\u{0}"}),
        json!({"connection_id": PG}),
        json!({"connection_id": PG, "query": "SELECT 1", "run": "yes"}),
        json!({"connection_id": PG, "query": "SELECT 1", "statement": "x"}),
    ] {
        assert_eq!(code(&layer.call("execute_and_show", args)), "agent.invalid_arguments");
    }
    assert!(app.shown.lock().unwrap().is_empty());
}

#[test]
fn what_the_app_says_is_passed_on_with_its_own_code() {
    let app = FakeApp::new();
    let layer = layer(AgentAccess::Read, Some(&app));
    *app.answer.lock().unwrap() = Some(AgentError::AppUnavailable(
        "tablex is not running. Open tablex and try again.".into(),
    ));
    for (tool, args) in [
        ("open_table", json!({"connection_id": PG, "database": "shop", "object": "orders"})),
        ("execute_and_show", json!({"connection_id": PG, "query": "SELECT 1"})),
    ] {
        match layer.call(tool, args) {
            ToolOutcome::Failure(error) => {
                assert_eq!(error.code, "agent.app_unavailable");
                assert!(error.message.contains("Open tablex and try again"));
            }
            other => panic!("{other:?}"),
        }
    }
    *app.answer.lock().unwrap() = Some(AgentError::AppRefused("no saved connection".into()));
    let outcome = layer.call("execute_and_show", json!({"connection_id": PG, "query": "SELECT 1"}));
    assert_eq!(code(&outcome), "agent.app_refused");
}
