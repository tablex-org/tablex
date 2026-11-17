//! `add_connection`, `duplicate_connection` and `remove_connection` end to end: the real server
//! and tool layer over a temporary connection store (`TABLEX_DATA_DIR`), an in-memory credential
//! store and a fake app on a temporary socket. Every call is put to the user through MCP
//! elicitation; nothing touches the user's Keychain, data directory or a running TableX.

mod support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use engine_connect::app_channel::{AppClient, Listener, Reply, Request};
use engine_connect::credentials::{CredentialStore, SecretKind};
use engine_storage::connection::{get_connection, list_connections, SavedConnectionRecord};
use rmcp::model::{CallToolResult, ElicitResult, ElicitationAction, ErrorData as McpError};
use serde_json::{json, Value};
use support::*;

const PASSWORD: &str = "Zq81-hunter2-p@ss/w0rd";

fn unique(prefix: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    format!("{prefix}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A fake TableX that records what it is told.
struct FakeApp {
    _dir: tempfile::TempDir,
    path: PathBuf,
    seen: Arc<Mutex<Vec<Request>>>,
    _listener: Listener,
}

impl FakeApp {
    fn listening() -> Self {
        let dir = tempfile::Builder::new().prefix("tx").tempdir().unwrap();
        let path = dir.path().join("app.sock");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let listener = Listener::bind(
            &path,
            Arc::new(move |request| {
                log.lock().unwrap().push(request);
                Reply::ok()
            }),
        )
        .unwrap();
        Self { _dir: dir, path, seen, _listener: listener }
    }

    fn client(&self) -> AppClient {
        AppClient::new(self.path.clone())
    }

    fn changed(&self) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|request| matches!(request, Request::ConnectionsChanged {}))
            .count()
    }
}

struct Rig {
    harness: Harness<Asker>,
    seen: Arc<Seen>,
    app: FakeApp,
}

/// A server whose client answers every question with `answer` (`capability`: it declares the
/// elicitation capability), with the app's switch `switch`.
async fn rig(capability: bool, answer: Result<ElicitResult, McpError>, switch: bool) -> Rig {
    let app = FakeApp::listening();
    let fixture = Fixture::with_notify(Some(app.client()));
    fixture.set_mcp_writes(switch);
    let asker = Asker::answering(capability, answer);
    let seen = Arc::clone(&asker.seen);
    Rig { harness: Harness::start_with(fixture, asker).await, seen, app }
}

async fn approving() -> Rig {
    rig(true, Ok(accepted(true)), true).await
}

fn stored(name: &str) -> Option<SavedConnectionRecord> {
    list_connections().unwrap().into_iter().find(|record| record.name == name)
}

fn error_message(result: &CallToolResult) -> String {
    let body: Value = serde_json::from_str(&text(result)).unwrap();
    body["error"]["message"].as_str().unwrap().to_owned()
}

fn add_args(name: &str) -> Value {
    json!({
        "name": name, "kind": "postgresql", "host": "db.local", "port": 5432,
        "database": "shop", "user": "app", "password": PASSWORD, "connection_type": "test",
        "options": {"tls": "require"}
    })
}

impl Rig {
    fn secret(&self, id: &str) -> Option<String> {
        self.harness.fixture.keys.load(SecretKind::Database, &id.to_uppercase()).unwrap()
    }

    /// Everything a log or a result could carry, as one text.
    fn everything(&self, result: &CallToolResult) -> String {
        let entries = serde_json::to_string(&self.harness.fixture.audit_entries()).unwrap();
        let questions = serde_json::to_string(&self.seen.asked()).unwrap();
        format!("{} {} {entries} {questions}", text(result), serde_json::to_string(result).unwrap())
    }

    async fn add(&self, args: Value) -> CallToolResult {
        self.harness.call("add_connection", args).await
    }
}

// ------------------------------------------------------------------------------------------ add

#[tokio::test]
async fn an_approved_add_stores_the_record_and_keeps_the_password_in_the_secret_store_only() {
    let rig = approving().await;
    let name = unique("shop-pg");
    let result = rig.add(add_args(&name)).await;
    let body = structured(&result);
    let id = body["connection_id"].as_str().unwrap().to_owned();
    assert_eq!(body["password_saved"], true);
    assert_eq!(body["agent_access"], "off");
    assert_eq!(body["app_notified"], true);
    // The postgresql driver is not in the test driver store: said as a warning, not a failure.
    assert!(body["warning"].as_str().unwrap().contains("driver is not installed"), "{body}");
    assert_eq!(rig.app.changed(), 1);

    let record = stored(&name).expect("the record is stored");
    assert_eq!(record.id, id.to_uppercase());
    assert_eq!(record.uri, "postgresql://app@db.local:5432/shop?sslmode=require");
    assert_eq!(record.db_type, "postgresql");
    assert_eq!(record.connection_type, "test");
    assert!(record.save_password && !record.is_pinned);
    assert_eq!((record.network_settings_json.as_str(), record.overrides_json.as_str()), ("{}", ""));
    assert_eq!(get_connection(&record.id).unwrap().unwrap(), record);
    // The password is in the secret store, under the connection's id, and nowhere else.
    assert_eq!(rig.secret(&id).as_deref(), Some(PASSWORD));
    assert!(!serde_json::to_string(&record).unwrap().contains(PASSWORD));
    assert!(!rig.everything(&result).contains(PASSWORD));
    assert!(!rig.everything(&result).contains("Zq81"));

    // One question, about this change: what is added, never the password.
    let asked = rig.seen.asked();
    assert_eq!(asked.len(), 1);
    let (message, schema) = &asked[0];
    for part in [
        "ADD", &name, "postgresql", "db.local", "5432", "shop", "app", "Test", "sslmode=require",
        "saved in TableX's credential store",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
    assert!(!message.contains("PRODUCTION"));
    assert_eq!(schema["properties"]["run"]["title"], "Make this change to your connections");
    assert_eq!(schema["properties"]["run"]["default"], false);

    // The audit line names the change and never a password or the full URI with credentials.
    let entries = rig.harness.fixture.audit_entries();
    let entry = entries.iter().find(|entry| entry.tool == "add_connection").unwrap();
    assert_eq!(entry.outcome, "ok");
    assert_eq!(entry.approval.as_deref(), Some("approved"));
    assert_eq!(entry.connection_id.as_deref(), Some(id.as_str()));
    let statement = entry.statement.as_deref().unwrap();
    // (The redactor masks the user in an address and the value of `password=`: still safe.)
    assert!(statement.contains(&name) && statement.contains("add_connection") && statement.contains("db.local:5432/shop"), "{statement}");
    assert!(!statement.contains(PASSWORD), "{statement}");

    // The same store is what the tool layer reads: the connection is listed, with no access yet.
    let listed = structured(&rig.harness.call("list_connections", json!({})).await);
    let entry = listed["connections"].as_array().unwrap().iter().find(|entry| entry["id"] == id).unwrap();
    assert_eq!(entry["access"], "off");
}

#[tokio::test]
async fn an_add_without_a_password_saves_none_and_sqlite_takes_a_path() {
    let rig = approving().await;
    let name = unique("local-sqlite");
    let result = rig
        .add(json!({"name": name, "kind": "sqlite", "path": "/tmp/shop-orders.db"}))
        .await;
    let body = structured(&result);
    assert_eq!(body["password_saved"], false);
    let record = stored(&name).unwrap();
    assert_eq!(record.uri, "sqlite:///tmp/shop-orders.db");
    assert_eq!(record.connection_type, "development");
    assert!(!record.save_password);
    assert_eq!(rig.secret(body["connection_id"].as_str().unwrap()), None);
    assert!(rig.seen.asked()[0].0.contains("none is saved"));
}

#[tokio::test]
async fn a_uri_without_a_password_is_taken_and_a_connection_type_may_be_named() {
    let rig = approving().await;
    let name = unique("shop-uri");
    rig.add(json!({"name": name, "kind": "mongodb", "uri": "MONGODB://app@db.local/shop?authSource=admin", "connection_type": "Production"}))
        .await;
    let record = stored(&name).unwrap();
    assert_eq!(record.uri, "mongodb://app@db.local/shop?authSource=admin");
    assert_eq!(record.connection_type, "production");
    // A production connection is said to be one in the question.
    assert!(rig.seen.asked()[0].0.contains("PRODUCTION"), "{}", rig.seen.asked()[0].0);
}

#[tokio::test]
async fn a_declined_dismissed_or_malformed_answer_changes_nothing() {
    let answers: [Result<ElicitResult, McpError>; 5] = [
        Ok(accepted(false)),
        Ok(ElicitResult::new(ElicitationAction::Decline)),
        Ok(ElicitResult::new(ElicitationAction::Cancel)),
        Ok(ElicitResult::new(ElicitationAction::Accept).with_content(json!({"run": "true"}))),
        Err(McpError::internal_error("the client failed", None)),
    ];
    for answer in answers {
        let rig = rig(true, answer, true).await;
        let name = unique("declined");
        let result = rig.add(add_args(&name)).await;
        assert_eq!(code(&result), "agent.write_refused");
        assert!(stored(&name).is_none());
        assert_eq!(rig.seen.asked().len(), 1);
        assert_eq!(rig.app.changed(), 0);
        assert!(!rig.everything(&result).contains(PASSWORD));
        let entries = rig.harness.fixture.audit_entries();
        assert_ne!(entries[0].approval.as_deref(), Some("approved"));
        assert_ne!(entries[0].outcome, "ok");
        assert!(rig.harness.fixture.keys.loaded_accounts().is_empty());
        // Nothing was saved in the secret store either.
        for record in list_connections().unwrap() {
            assert_eq!(rig.secret(&record.id), None);
        }
    }
}

#[tokio::test]
async fn a_client_that_cannot_ask_is_refused_and_so_is_an_app_with_the_switch_off() {
    // No elicitation capability.
    for rig in [rig(false, Ok(accepted(true)), true).await] {
        let name = unique("no-elicit");
        let result = rig.add(add_args(&name)).await;
        assert_eq!(code(&result), "agent.write_refused");
        assert!(error_message(&result).contains("cannot ask the user"), "{}", error_message(&result));
        assert!(stored(&name).is_none());
        assert!(rig.seen.asked().is_empty());
        assert!(!error_message(&result).contains(PASSWORD));
    }
    // A plain client that never heard of elicitation.
    let fixture = Fixture::new();
    fixture.set_mcp_writes(true);
    let harness = Harness::start(fixture).await;
    let name = unique("plain-client");
    let result = harness.call("add_connection", add_args(&name)).await;
    assert_eq!(code(&result), "agent.write_refused");
    assert!(stored(&name).is_none());
    // The switch is off: refused before anything is asked.
    let rig = rig(true, Ok(accepted(true)), false).await;
    let name = unique("switch-off");
    let result = rig.add(add_args(&name)).await;
    assert_eq!(code(&result), "agent.write_refused");
    assert!(error_message(&result).contains("turned off in TableX"), "{}", error_message(&result));
    assert!(stored(&name).is_none());
    assert!(rig.seen.asked().is_empty());
}

#[tokio::test]
async fn invalid_input_is_refused_before_anything_is_asked() {
    let rig = approving().await;
    let name = unique("invalid");
    let base = |change: Value| {
        let mut args = add_args(&name);
        for (key, value) in change.as_object().unwrap() {
            if value.is_null() {
                args.as_object_mut().unwrap().remove(key);
            } else {
                args[key] = value.clone();
            }
        }
        args
    };
    let cases = [
        ("an unknown kind", base(json!({"kind": "oracle"}))),
        ("port 0", base(json!({"port": 0}))),
        ("port too big", base(json!({"port": 65536}))),
        ("a negative port", base(json!({"port": -5}))),
        ("a port as text", base(json!({"port": "5432"}))),
        ("an empty name", base(json!({"name": "   "}))),
        ("a long name", base(json!({"name": "n".repeat(129)}))),
        ("a name with a line break", base(json!({"name": "a\nb"}))),
        ("a name with a direction change", base(json!({"name": "a\u{202e}b"}))),
        ("a host with a path", base(json!({"host": "db.local/evil"}))),
        ("no host", base(json!({"host": null}))),
        ("a uri and fields", base(json!({"uri": "postgresql://db.local/shop"}))),
        ("an unknown type", base(json!({"connection_type": "staging"}))),
        ("an unknown tls mode", base(json!({"options": {"tls": "maybe"}}))),
        ("an ssh tunnel", base(json!({"ssh": {"host": "bastion"}}))),
        ("an empty password", base(json!({"password": ""}))),
        ("a password with a NUL", base(json!({"password": "a\u{0}b"}))),
    ];
    for (what, args) in cases {
        let result = rig.add(args).await;
        assert_eq!(code(&result), "agent.invalid_arguments", "{what}: {}", text(&result));
        assert!(!everything(&rig, &result).contains(PASSWORD), "{what}");
    }
    let uris = [
        ("a password in the uri", "postgresql://app:hunter2@db.local/shop"),
        ("an unsafe option", "postgresql://app@db.local/shop?sslrootcert=/etc/ca.pem"),
        ("another kind's scheme", "mysql://app@db.local/shop"),
        ("not a uri", "db.local"),
    ];
    for (what, uri) in uris {
        let result = rig.add(json!({"name": name, "kind": "postgresql", "uri": uri})).await;
        assert_eq!(code(&result), "agent.invalid_arguments", "{what}: {}", text(&result));
        assert!(!text(&result).contains("hunter2"), "{what}");
    }
    let relative = rig.add(json!({"name": name, "kind": "sqlite", "path": "shop.db"})).await;
    assert_eq!(code(&relative), "agent.invalid_arguments");
    assert!(stored(&name).is_none());
    assert!(rig.seen.asked().is_empty());
}

fn everything(rig: &Rig, result: &CallToolResult) -> String {
    rig.everything(result)
}

#[tokio::test]
async fn a_name_that_is_taken_is_refused_whatever_its_case() {
    let rig = approving().await;
    let name = unique("taken");
    structured(&rig.add(add_args(&name)).await);
    let asked = rig.seen.asked().len();
    for taken in [name.clone(), name.to_uppercase(), format!("  {name} ")] {
        let result = rig.add(add_args(&taken)).await;
        assert_eq!(code(&result), "agent.invalid_arguments", "{taken}");
        assert!(error_message(&result).contains("already exists"));
    }
    // A saved connection of the fixture is as good as one added through the tool.
    let existing = rig.harness.fixture.connection(None);
    let existing_name = list_connections().unwrap().into_iter().find(|r| r.id == existing.id).unwrap().name;
    assert_eq!(code(&rig.add(add_args(&existing_name)).await), "agent.invalid_arguments");
    assert_eq!(rig.seen.asked().len(), asked);
    assert_eq!(list_connections().unwrap().iter().filter(|r| r.name == name).count(), 1);
}

#[tokio::test]
async fn an_app_that_is_not_running_does_not_fail_the_call() {
    for notify in [
        None,
        Some(AppClient::new(std::env::temp_dir().join("tablex-no-such-app-socket.sock"))),
    ] {
        let fixture = Fixture::with_notify(notify);
        fixture.set_mcp_writes(true);
        let harness = Harness::start_with(fixture, Asker::answering(true, Ok(accepted(true)))).await;
        let name = unique("no-app");
        let body = structured(&harness.call("add_connection", add_args(&name)).await);
        assert_eq!(body["app_notified"], false);
        assert!(stored(&name).is_some());
    }
}

#[tokio::test]
async fn three_declines_close_the_connection_tools_for_the_session() {
    let rig = rig(true, Ok(accepted(false)), true).await;
    for _ in 0..3 {
        let result = rig.add(add_args(&unique("nagging"))).await;
        assert!(error_message(&result).contains("did not approve"), "{}", error_message(&result));
    }
    let result = rig.add(add_args(&unique("nagging"))).await;
    assert_eq!(code(&result), "agent.write_refused");
    assert!(error_message(&result).contains("declined"), "{}", error_message(&result));
    assert_eq!(rig.seen.asked().len(), 3);
}

// ------------------------------------------------------------------------------------ duplicate

#[tokio::test]
async fn a_copy_has_the_same_address_and_type_and_the_password_only_when_the_original_has_one() {
    let rig = approving().await;
    // An original with a saved password (added through the tool) and one without.
    let with = unique("orig-with-pw");
    let with_id = structured(&rig.add(add_args(&with)).await)["connection_id"].as_str().unwrap().to_owned();
    let without = unique("orig-without-pw");
    let without_id = structured(
        &rig.add(json!({"name": without, "kind": "mysql", "host": "db.local", "database": "orders", "user": "app"})).await,
    )["connection_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let copy_name = unique("copy-with-pw");
    let body = structured(&rig.harness.call("duplicate_connection", json!({"connection_id": with_id, "name": copy_name})).await);
    let copy_id = body["connection_id"].as_str().unwrap().to_owned();
    assert_ne!(copy_id, with_id);
    assert_eq!((body["password_saved"].clone(), body["agent_access"].clone()), (json!(true), json!("off")));
    let (original, copy) = (stored(&with).unwrap(), stored(&copy_name).unwrap());
    assert_eq!(copy.uri, original.uri);
    assert_eq!((copy.db_type.as_str(), copy.connection_type.as_str()), (original.db_type.as_str(), original.connection_type.as_str()));
    assert!(copy.save_password && !copy.is_pinned);
    assert_eq!(copy.id, copy_id.to_uppercase());
    assert_eq!(rig.secret(&copy_id).as_deref(), Some(PASSWORD));
    assert_eq!(rig.secret(&with_id).as_deref(), Some(PASSWORD));
    let question = rig.seen.asked().last().unwrap().0.clone();
    for part in ["COPY", &with, &copy_name, "the saved password of the original is copied"] {
        assert!(question.contains(part), "{part}: {question}");
    }
    assert!(!question.contains(PASSWORD));

    let copy_name = unique("copy-without-pw");
    let body = structured(&rig.harness.call("duplicate_connection", json!({"connection_id": without_id, "name": copy_name})).await);
    let copy = stored(&copy_name).unwrap();
    assert_eq!(body["password_saved"], false);
    assert!(!copy.save_password);
    assert_eq!(copy.uri, stored(&without).unwrap().uri);
    assert_eq!(rig.secret(body["connection_id"].as_str().unwrap()), None);
    assert!(rig.seen.asked().last().unwrap().0.contains("none (the original has none saved)"));
    assert!(!rig.everything(&rig.harness.call("list_connections", json!({})).await).contains(PASSWORD));
}

#[tokio::test]
async fn a_copy_does_not_get_the_originals_access_setting() {
    let rig = approving().await;
    let original = rig.harness.fixture.connection(Some("read"));
    let name = unique("copy-of-read");
    let body = structured(&rig.harness.call("duplicate_connection", json!({"connection_id": original.id, "name": name})).await);
    let copy = stored(&name).unwrap();
    assert!(!copy.overrides_json.contains("agent.access"), "{}", copy.overrides_json);
    // Its type (none here) gives agents nothing: the copy is off, the original is not.
    assert_eq!(body["agent_access"], "off");
    let listed = structured(&rig.harness.call("list_connections", json!({})).await);
    let access = |id: &str| listed["connections"].as_array().unwrap().iter().find(|e| e["id"] == id).unwrap()["access"].clone();
    assert_eq!(access(&original.id), "read");
    assert_eq!(access(body["connection_id"].as_str().unwrap()), "off");
}

#[tokio::test]
async fn a_copy_is_refused_for_a_name_in_use_or_an_unknown_connection_and_asks_nothing() {
    let rig = approving().await;
    let original = rig.harness.fixture.connection(Some("read"));
    let original_name = list_connections().unwrap().into_iter().find(|r| r.id == original.id).unwrap().name;
    for (id, name) in [(original.id.as_str(), original_name.as_str()), (original.id.as_str(), ""), ("no-such-id", "copy-x")] {
        let result = rig.harness.call("duplicate_connection", json!({"connection_id": id, "name": name})).await;
        assert_eq!(code(&result), "agent.invalid_arguments", "{id} {name}");
    }
    assert!(rig.seen.asked().is_empty());
}

#[tokio::test]
async fn a_declined_copy_stores_nothing() {
    let rig = rig(true, Ok(ElicitResult::new(ElicitationAction::Decline)), true).await;
    let original = rig.harness.fixture.connection(Some("read"));
    let name = unique("declined-copy");
    let result = rig.harness.call("duplicate_connection", json!({"connection_id": original.id, "name": name})).await;
    assert_eq!(code(&result), "agent.write_refused");
    assert!(stored(&name).is_none());
    assert_eq!(rig.app.changed(), 0);
}

// ------------------------------------------------------------------------------------- remove

#[tokio::test]
async fn a_removal_deletes_the_record_its_secrets_and_pins_and_closes_the_session() {
    let rig = approving().await;
    let fixture = &rig.harness.fixture;
    let conn = fixture.connection(Some("read"));
    let record = list_connections().unwrap().into_iter().find(|r| r.id == conn.id).unwrap();
    // Secrets under both spellings of the id, and a pinned node.
    for account in [conn.id.to_uppercase(), conn.id.clone()] {
        fixture.keys.write(SecretKind::Database, &account, PASSWORD).unwrap();
        fixture.keys.write(SecretKind::SshPassword, &account, PASSWORD).unwrap();
    }
    let db = engine_storage::app_db::default_path();
    engine_storage::pinned_nodes::pin(&db, &conn.id, "db/main").unwrap();
    assert_eq!(engine_storage::pinned_nodes::list(&db, &conn.id).unwrap(), ["db/main"]);
    // The agent has a session open on it.
    structured(&rig.harness.call("open_session", json!({"connection_id": conn.id})).await);
    assert!(fixture.layer.is_open(&conn.id));

    let result = rig.harness.call("remove_connection", json!({"connection_id": conn.id})).await;
    let body = structured(&result);
    assert_eq!(body, json!({"connection_id": conn.id, "app_notified": true}));
    assert!(stored(&record.name).is_none());
    assert!(get_connection(&conn.id).unwrap().is_none());
    for account in [conn.id.to_uppercase(), conn.id.clone()] {
        for kind in [SecretKind::Database, SecretKind::SshPassword] {
            assert_eq!(fixture.keys.load(kind, &account).unwrap(), None, "{kind:?} {account}");
        }
    }
    assert!(engine_storage::pinned_nodes::list(&db, &conn.id).unwrap().is_empty());
    assert!(!fixture.layer.is_open(&conn.id));
    assert_eq!(rig.app.changed(), 1);
    let question = rig.seen.asked().last().unwrap().0.clone();
    assert!(question.contains("REMOVE") && question.contains(&record.name), "{question}");
    assert!(!question.contains("PRODUCTION"));
    let entries = fixture.audit_entries();
    let entry = entries.iter().find(|entry| entry.tool == "remove_connection").unwrap();
    assert_eq!((entry.outcome.as_str(), entry.approval.as_deref()), ("ok", Some("approved")));
    assert!(!rig.everything(&result).contains(PASSWORD));
}

#[tokio::test]
async fn the_question_for_a_production_connection_says_so() {
    let rig = approving().await;
    let conn = rig.harness.fixture.typed_connection(Some("read"), "production");
    structured(&rig.harness.call("remove_connection", json!({"connection_id": conn.id})).await);
    assert!(rig.seen.asked()[0].0.contains("WARNING: this is a PRODUCTION connection"), "{}", rig.seen.asked()[0].0);
    assert!(get_connection(&conn.id).unwrap().is_none());
}

#[tokio::test]
async fn a_declined_removal_or_an_unknown_connection_changes_nothing() {
    let rig = rig(true, Ok(ElicitResult::new(ElicitationAction::Decline)), true).await;
    let fixture = &rig.harness.fixture;
    let conn = fixture.connection(Some("read"));
    fixture.keys.write(SecretKind::Database, &conn.id.to_uppercase(), PASSWORD).unwrap();
    structured(&rig.harness.call("open_session", json!({"connection_id": conn.id})).await);
    let result = rig.harness.call("remove_connection", json!({"connection_id": conn.id})).await;
    assert_eq!(code(&result), "agent.write_refused");
    assert!(get_connection(&conn.id).unwrap().is_some());
    assert_eq!(rig.secret(&conn.id).as_deref(), Some(PASSWORD));
    assert!(fixture.layer.is_open(&conn.id));
    assert_eq!(rig.app.changed(), 0);
    let asked = rig.seen.asked().len();
    let unknown = rig.harness.call("remove_connection", json!({"connection_id": "no-such-id"})).await;
    assert_eq!(code(&unknown), "agent.invalid_arguments");
    assert_eq!(rig.seen.asked().len(), asked);
}

#[tokio::test]
async fn a_client_that_cannot_ask_cannot_remove_or_copy() {
    let rig = rig(false, Ok(accepted(true)), true).await;
    let conn = rig.harness.fixture.connection(Some("read"));
    for (tool, args) in [
        ("remove_connection", json!({"connection_id": conn.id})),
        ("duplicate_connection", json!({"connection_id": conn.id, "name": unique("no-ask-copy")})),
    ] {
        let result = rig.harness.call(tool, args).await;
        assert_eq!(code(&result), "agent.write_refused", "{tool}");
        assert!(error_message(&result).contains("cannot ask the user"));
    }
    assert!(get_connection(&conn.id).unwrap().is_some());
}

#[tokio::test]
async fn cancelling_the_request_while_the_question_waits_adds_nothing_even_if_it_is_answered_late() {
    let (asker, mut desk) = Asker::manual(true);
    let app = FakeApp::listening();
    let fixture = Fixture::with_notify(Some(app.client()));
    fixture.set_mcp_writes(true);
    let harness = Harness::start_with(fixture, asker).await;
    let name = unique("cancelled");
    let call = harness.start_call("add_connection", add_args(&name)).await;
    let question = within("a question", desk.recv()).await.expect("the question arrives");
    assert!(question.message.contains(&name));
    call.cancel(Some("test".into())).await.unwrap();
    poll("the withdrawn question", async || question.cancelled.is_cancelled().then_some(())).await;
    let entry = poll("the cancelled call in the audit log", async || {
        harness
            .fixture
            .audit_entries()
            .into_iter()
            .find(|entry| entry.tool == "add_connection" && entry.outcome == "agent.cancelled")
    })
    .await;
    assert_eq!(entry.approval.as_deref(), Some("cancelled"));
    question.accept(true);
    assert!(stored(&name).is_none());
    assert_eq!(app.changed(), 0);
}
