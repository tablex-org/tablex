//! "Don't save passwords" in the connection model: a process-wide switch, so it has a test binary
//! of its own.

use engine_connect::key_home::{passwords_saved, set_passwords_saved};
use engine_connect::records::SavedConnection;
use engine_storage::connection::SavedConnectionRecord;

fn record() -> SavedConnectionRecord {
    SavedConnectionRecord {
        id: "7A1B2C3D-0000-4000-8000-0000000000AA".into(),
        name: "c".into(),
        uri: "postgresql://alice@db.example.com:5432/app".into(),
        db_type: "postgresql".into(),
        color_hex: "#34C759".into(),
        save_password: true,
        save_proxy_password: false,
        network_settings_json: "{}".into(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: String::new(),
    }
}

#[test]
fn no_save_makes_every_connection_ask_for_its_password_and_the_record_is_untouched() {
    assert!(passwords_saved());
    let saved = SavedConnection::from_record(&record());
    assert!(saved.save_password);
    assert!(!saved.expects_password());

    set_passwords_saved(false);
    let asking = SavedConnection::from_record(&record());
    assert!(!asking.save_password);
    assert!(asking.expects_password(), "the prompt for a password is the existing flow");
    assert!(record().save_password, "the record keeps the flag for when saving is back on");

    set_passwords_saved(true);
    assert!(SavedConnection::from_record(&record()).save_password);
}

/// By hand, in a Linux session with no Secret Service and a temporary `TABLEX_DATA_DIR`
/// (`KEY_HOME_PHASE` = `save`, then `read` in a new process, then `nosave`):
/// `cargo test -p engine-connect --test key_home -- --ignored real_file_home`.
#[cfg(unix)]
#[test]
#[ignore = "needs a session with no Secret Service and its own TABLEX_DATA_DIR"]
fn real_file_home_round_trip_across_processes() {
    use engine_connect::credentials::{default_store, key_store, CredentialError, CredentialStore, SecretKind};
    use engine_connect::key_home::{Home, KEY_FILE};
    let phase = std::env::var("KEY_HOME_PHASE").expect("KEY_HOME_PHASE");
    // The id of a saved connection, when the app is to read the secret back.
    let account = std::env::var("KEY_HOME_ACCOUNT").unwrap_or_else(|_| "conn-1".into());
    let store = key_store();
    let dir = store.key_path().unwrap().parent().unwrap().to_owned();
    match phase.as_str() {
        "save" => {
            assert!(store.needs_choice(), "no Secret Service, nothing chosen: the app asks");
            assert_eq!(
                default_store().write(SecretKind::Database, &account, "hunter2"),
                Err(CredentialError::NoKeyring),
                "before the answer nothing is saved, and the error is the typed one"
            );
            store.switch(Home::File).unwrap();
            assert!(!store.needs_choice());
            default_store().write(SecretKind::Database, &account, "hunter2").unwrap();
            default_store().write(SecretKind::Proxy, &account, "proxy-pw").unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join(KEY_FILE)), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }
        "read" => {
            assert_eq!(store.home().unwrap(), Home::File);
            assert!(!store.needs_choice(), "asked once");
            assert_eq!(default_store().load(SecretKind::Database, &account).unwrap().as_deref(), Some("hunter2"));
            assert_eq!(default_store().load(SecretKind::Proxy, &account).unwrap().as_deref(), Some("proxy-pw"));
        }
        "nosave" => {
            store.switch(Home::NoSave).unwrap();
            assert_eq!(default_store().write(SecretKind::Database, "other", "x"), Err(CredentialError::NoKeyring));
            // The key stays where it was: the saved password is back after switching to the file.
            store.switch(Home::File).unwrap();
            assert_eq!(default_store().load(SecretKind::Database, &account).unwrap().as_deref(), Some("hunter2"));
        }
        other => panic!("unknown phase {other}"),
    }
}
