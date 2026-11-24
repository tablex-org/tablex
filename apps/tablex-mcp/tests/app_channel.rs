//! `open_table` and `execute_and_show` end to end: the real server and tool layer over the real
//! socket client, against a fake app listening on a temporary socket. Nothing here touches a
//! running tablex or the user's data dir.

// Windows has no app channel yet: the tests that need a listening fake app are unix only.
#![cfg_attr(windows, allow(dead_code, unused_imports))]

mod support;

use engine_connect::app_channel::{AppClient, Listener, Reply, Request};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use support::*;
use tablex_mcp::app_port::AppChannel;

/// A fake tablex: records what it is asked and answers with `reply`.
struct FakeApp {
    _dir: tempfile::TempDir,
    path: PathBuf,
    seen: Arc<Mutex<Vec<Request>>>,
    _listener: Listener,
}

impl FakeApp {
    fn listening(reply: Reply) -> Self {
        let dir = tempfile::Builder::new().prefix("tx").tempdir().unwrap();
        let path = dir.path().join("app.sock");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let listener = Listener::bind(
            &path,
            Arc::new(move |request| {
                log.lock().unwrap().push(request);
                reply.clone()
            }),
        )
        .unwrap();
        Self {
            _dir: dir,
            path,
            seen,
            _listener: listener,
        }
    }

    fn channel(&self) -> Arc<AppChannel> {
        Arc::new(AppChannel::at(AppClient::new(self.path.clone())))
    }

    fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

fn error_message(result: &rmcp::model::CallToolResult) -> String {
    let body: Value = serde_json::from_str(&text(result)).unwrap();
    body["error"]["message"].as_str().unwrap().to_owned()
}

// The app channel has no Windows implementation yet (no listener to fake).
#[cfg(unix)]
#[tokio::test]
async fn open_table_sends_the_object_to_the_app_and_reports_its_answer() {
    let app = FakeApp::listening(Reply::ok());
    let harness = Harness::start(Fixture::with_app(app.channel())).await;
    let conn = harness.fixture.connection(Some("schema"));
    let result = harness
        .call(
            "open_table",
            json!({"connection_id": conn.id, "database": "main", "schema": "main", "object": "items"}),
        )
        .await;
    let body = structured(&result);
    assert_eq!(body["requested"], true);
    assert_eq!(body["object"], "items");
    let requests = app.requests();
    assert_eq!(requests.len(), 1);
    match &requests[0] {
        Request::OpenObject {
            connection_id,
            database,
            schema,
            object,
        } => {
            assert!(connection_id.eq_ignore_ascii_case(&conn.id), "{connection_id}");
            assert_eq!(database.as_deref(), Some("main"));
            assert_eq!(schema.as_deref(), Some("main"));
            assert_eq!(object, "items");
        }
        other => panic!("{other:?}"),
    }
}

// The app channel has no Windows implementation yet (no listener to fake).
#[cfg(unix)]
#[tokio::test]
async fn execute_and_show_sends_the_query_and_defaults_to_running_it() {
    let app = FakeApp::listening(Reply::ok());
    let harness = Harness::start(Fixture::with_app(app.channel())).await;
    let conn = harness.fixture.connection(Some("read"));
    let result = harness
        .call(
            "execute_and_show",
            json!({"connection_id": conn.id, "query": "SELECT * FROM items", "database": "main"}),
        )
        .await;
    assert_eq!(structured(&result)["run"], true);
    let result = harness
        .call(
            "execute_and_show",
            json!({"connection_id": conn.id, "query": "DELETE FROM items", "run": false}),
        )
        .await;
    assert_eq!(structured(&result), json!({"connection_id": structured(&result)["connection_id"], "shown": true, "run": false}));
    let requests = app.requests();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        &requests[0],
        Request::ShowQuery { database: Some(db), text, run: true, .. } if db == "main" && text == "SELECT * FROM items"
    ));
    assert!(matches!(
        &requests[1],
        Request::ShowQuery { database: None, text, run: false, .. } if text == "DELETE FROM items"
    ));
    // A change is not run from here, and the app never hears of it.
    let result = harness
        .call(
            "execute_and_show",
            json!({"connection_id": conn.id, "query": "DELETE FROM items WHERE id = 1"}),
        )
        .await;
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(app.requests().len(), 2);
}

// The app channel has no Windows implementation yet (no listener to fake).
#[cfg(unix)]
#[tokio::test]
async fn access_below_the_need_is_refused_before_the_app_is_asked() {
    let app = FakeApp::listening(Reply::ok());
    let harness = Harness::start(Fixture::with_app(app.channel())).await;
    let off = harness.fixture.connection(Some("off"));
    let unset = harness.fixture.connection(None);
    let schema_only = harness.fixture.connection(Some("schema"));
    for id in [&off.id, &unset.id] {
        let result = harness
            .call("open_table", json!({"connection_id": id, "database": "main", "object": "items"}))
            .await;
        assert_eq!(code(&result), "agent.access_denied");
    }
    for id in [&off.id, &unset.id, &schema_only.id] {
        let result = harness
            .call("execute_and_show", json!({"connection_id": id, "query": "SELECT 1"}))
            .await;
        assert_eq!(code(&result), "agent.access_denied");
        assert!(error_message(&result).contains("`read`"));
    }
    assert!(app.requests().is_empty());
}

#[tokio::test]
async fn an_app_that_is_not_running_gives_a_clear_error() {
    let dir = tempfile::Builder::new().prefix("tx").tempdir().unwrap();
    let nobody = Arc::new(AppChannel::at(AppClient::new(dir.path().join("app.sock"))));
    let harness = Harness::start(Fixture::with_app(nobody)).await;
    let conn = harness.fixture.connection(Some("read"));
    for (tool, args) in [
        ("open_table", json!({"connection_id": conn.id, "database": "main", "object": "items"})),
        ("execute_and_show", json!({"connection_id": conn.id, "query": "SELECT 1"})),
    ] {
        let result = harness.call(tool, args).await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(code(&result), "agent.app_unavailable");
        let message = error_message(&result);
        if cfg!(unix) {
            assert!(message.contains("Open tablex and try again"), "{message}");
        } else {
            assert!(message.contains("not on Windows"), "{message}");
        }
    }
}

// The app channel has no Windows implementation yet (no listener to fake).
#[cfg(unix)]
#[tokio::test]
async fn what_the_app_refuses_is_reported_with_its_reason() {
    let app = FakeApp::listening(Reply::error("no saved connection `x` in tablex"));
    let harness = Harness::start(Fixture::with_app(app.channel())).await;
    let conn = harness.fixture.connection(Some("read"));
    let result = harness
        .call("open_table", json!({"connection_id": conn.id, "database": "main", "object": "items"}))
        .await;
    assert_eq!(code(&result), "agent.app_refused");
    assert!(error_message(&result).contains("no saved connection"));
}

// The app channel has no Windows implementation yet (no listener to fake).
#[cfg(unix)]
#[tokio::test]
async fn the_default_layer_reaches_the_socket_in_the_data_dir() {
    // `build_layer` resolves the socket of `<TABLEX_DATA_DIR>/tablex` (in the data dir, or for a
    // deep temp dir like this test's, a short one named after it); the fixture's data dir is private
    // to this test binary, and this is the only test that listens there.
    // The fixture goes first: it points `TABLEX_DATA_DIR` at the private dir, and without that the
    // path below would be the real user's socket.
    let data_dir = support::shared().data_dir();
    let path = engine_connect::app_channel::socket_path().expect("a data dir");
    let in_data_dir = path.starts_with(data_dir);
    let named_after_it = path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("tablex-"));
    assert!(in_data_dir || named_after_it, "{path:?}");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let _listener = Listener::bind(
        &path,
        Arc::new(move |request| {
            log.lock().unwrap().push(request);
            Reply::ok()
        }),
    )
    .unwrap();
    let harness = Harness::start(Fixture::new()).await;
    let conn = harness.fixture.connection(Some("read"));
    let result = harness
        .call("execute_and_show", json!({"connection_id": conn.id, "query": "SELECT 1"}))
        .await;
    assert_eq!(structured(&result)["shown"], true);
    assert_eq!(seen.lock().unwrap().len(), 1);
}
