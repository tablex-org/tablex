//! `add_connection` and `duplicate_connection` while "Don't save passwords" is on: the connection
//! is made without a saved password and the result says so (the switch is process-wide, so this
//! has its own test binary).

mod support;

use engine_connect::credentials::{CredentialStore, SecretKind};
use engine_storage::connection::list_connections;
use serde_json::json;
use support::*;

const PASSWORD: &str = "Zq81-hunter2-p@ss/w0rd";

#[tokio::test]
async fn without_saved_passwords_an_add_or_copy_is_made_without_one_and_says_so() {
    let fixture = Fixture::with_notify(None);
    fixture.set_mcp_writes(true);
    let keys = fixture.keys.clone();
    let asker = Asker::answering(true, Ok(accepted(true)));
    let seen = std::sync::Arc::clone(&asker.seen);
    let harness = Harness::start_with(fixture, asker).await;
    let secret = |id: &str| keys.load(SecretKind::Database, &id.to_uppercase()).unwrap();
    let args = |name: &str| {
        json!({
            "name": name, "kind": "postgresql", "host": "db.local", "port": 5432,
            "database": "shop", "user": "app", "password": PASSWORD, "connection_type": "test"
        })
    };

    // An original saved while passwords are saved.
    engine_connect::key_home::set_passwords_saved(true);
    let body = structured(&harness.call("add_connection", args("nosave-original")).await);
    assert_eq!(body["password_saved"], true);
    let original = body["connection_id"].as_str().unwrap().to_owned();
    assert_eq!(secret(&original).as_deref(), Some(PASSWORD));

    engine_connect::key_home::set_passwords_saved(false);
    // Add with a password: added, none saved, said so (to the user and in the result).
    let result = harness.call("add_connection", args("nosave-added")).await;
    let body = structured(&result);
    assert_eq!(body["password_saved"], false);
    assert!(body["warning"].as_str().unwrap().contains("Don't save passwords"), "{body}");
    assert_eq!(secret(body["connection_id"].as_str().unwrap()), None);
    assert!(seen.asked().last().unwrap().0.contains("added without one"));
    let record = list_connections().unwrap().into_iter().find(|record| record.name == "nosave-added").unwrap();
    assert!(!record.save_password);

    // Duplicate of a connection whose password is saved: copied, without the password.
    let body = structured(
        &harness.call("duplicate_connection", json!({"connection_id": original, "name": "nosave-copy"})).await,
    );
    assert_eq!(body["password_saved"], false);
    assert!(body["warning"].as_str().unwrap().contains("Don't save passwords"), "{body}");
    assert_eq!(secret(body["connection_id"].as_str().unwrap()), None);
    assert_eq!(secret(&original).as_deref(), Some(PASSWORD), "the original keeps its secret");
    assert!(seen.asked().last().unwrap().0.contains("the copy has none"));
    assert!(!text(&result).contains(PASSWORD));
}
