// "Reset Saved Passwords…": the command palette action, its confirmation, and what it clears. The
// OS credential store is the test build's in-memory one (nothing here reaches the Keychain) and
// the app database is the test process's temporary data dir.

/// An app whose credential store holds a database password, an SSH password and passphrase and the
/// AI key, with a saved connection and a history entry in the app database.
fn reset_rig(
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<AppRoot>,
    &mut gpui::VisualTestContext,
    std::sync::Arc<connection_ui::MemoryCredentials>,
    String,
) {
    use crate::credentials::CredentialStore as _;
    cx.dispatcher.allow_parking();
    let (app, visual) = palette_app(cx);
    let keys = std::sync::Arc::new(connection_ui::MemoryCredentials::default());
    let id = format!("reset-{}", uuid::Uuid::new_v4());
    for (kind, account, secret) in [
        (crate::credentials::SecretKind::Database, id.to_uppercase(), "db-pw"),
        (crate::credentials::SecretKind::SshPassword, id.to_uppercase(), "ssh-pw"),
        (crate::credentials::SecretKind::SshPassphrase, id.to_uppercase(), "ssh-phrase"),
        (crate::credentials::SecretKind::AiApiKey, "anthropic".to_owned(), "sk-test"),
    ] {
        keys.write(kind, &account, secret).unwrap();
    }
    let for_app: std::sync::Arc<dyn crate::credentials::CredentialStore + Send + Sync> = keys.clone();
    app.update(visual, |this, _| this.connection_ui.credentials = for_app);
    engine_storage::connection::upsert_connection(&engine_storage::connection::SavedConnectionRecord {
        id: id.clone(),
        name: "Kept".into(),
        uri: "postgresql://u@127.0.0.1:1/db".into(),
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
    })
    .unwrap();
    engine_storage::history::save_entry(&id, "db", "orders", "{}", "", "", 0, 10, 0, 1).unwrap();
    (app, visual, keys, id)
}

fn reset_secrets(keys: &connection_ui::MemoryCredentials, id: &str) -> usize {
    use crate::credentials::CredentialStore as _;
    [
        (crate::credentials::SecretKind::Database, id.to_uppercase()),
        (crate::credentials::SecretKind::SshPassword, id.to_uppercase()),
        (crate::credentials::SecretKind::SshPassphrase, id.to_uppercase()),
        (crate::credentials::SecretKind::AiApiKey, "anthropic".to_owned()),
    ]
    .iter()
    .filter(|(kind, account)| keys.load(*kind, account).unwrap().is_some())
    .count()
}

/// Opens the palette, types the command and presses Enter on the first row.
fn reset_from_the_palette(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    palette_open(visual);
    palette_type(visual, "reset saved passwords");
    let actions = palette_titles(&palette_snapshot(app, visual), "Actions");
    assert_eq!(
        actions.first().map(String::as_str),
        Some("Reset Saved Passwords\u{2026}"),
        "the action leads its search: {actions:?}"
    );
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
}

#[gpui::test]
fn reset_saved_passwords_asks_first_cancel_changes_nothing_and_confirm_clears_only_the_secrets(cx: &mut TestAppContext) {
    let (app, visual, keys, id) = reset_rig(cx);
    let connections = app.read_with(visual, |this, _| this.saved_connections.len());

    // The palette opens the confirmation (the palette itself is gone), and Escape declines.
    reset_from_the_palette(&app, visual);
    assert!(palette_is_open(visual), "the confirmation is the one dialog open");
    assert_eq!(reset_secrets(&keys, &id), 4);
    visual.simulate_keystrokes("escape");
    palette_settle(visual);
    assert!(!palette_is_open(visual), "Cancel closes the confirmation");
    assert_eq!(reset_secrets(&keys, &id), 4, "Cancel does nothing");
    app.read_with(visual, |this, _| assert!(this.sidebar.notice.is_none()));

    // Confirm: every secret goes (off the UI thread), and the result is shown.
    reset_from_the_palette(&app, visual);
    visual.simulate_keystrokes("enter");
    let id_for_wait = id.clone();
    let waiting = keys.clone();
    wait_until(visual, "the secrets to go", move |_| reset_secrets(&waiting, &id_for_wait) == 0);
    palette_settle(visual);
    assert_eq!(reset_secrets(&keys, &id), 0);
    app.read_with(visual, |this, _| {
        assert_eq!(this.sidebar.notice.as_deref(), Some("Saved passwords were reset. Enter the passwords again."));
        assert_eq!(this.saved_connections.len(), connections, "connections are kept");
    });

    // The rest of the app database is as it was.
    let record = engine_storage::connection::get_connection(&id).unwrap().expect("the connection is kept");
    assert_eq!(record.name, "Kept");
    let history = engine_storage::history::fetch_entries(&id, "db", "orders", 10).unwrap();
    assert_eq!(history.len(), 1, "history is kept");
}

#[test]
fn the_confirmation_says_what_is_removed_and_that_it_must_be_entered_again() {
    let text = reset_passwords::RESET_WARNING;
    for needle in ["connection passwords", "SSH passwords", "passphrases", "AI API key", "enter them again", "kept"] {
        assert!(text.contains(needle), "{needle:?} in {text:?}");
    }
}

#[gpui::test]
fn a_connection_that_fails_because_the_master_key_is_gone_offers_the_reset(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = palette_app(cx);
    let key = crate::ui_fixture::POSTGRES_ID.to_owned();
    let message = engine_connect::credentials::CredentialError::MasterKeyMissing {
        secrets: 3,
        file: std::path::PathBuf::from("/data/tablex.db"),
    }
    .to_string();
    assert!(crate::credentials::suggests_reset(&message), "{message}");
    assert!(!message.contains("delete") && !message.contains("tablex.db"), "{message}");
    let open = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, failure: Option<String>| {
        let key = key.clone();
        app.update(visual, |this, cx| {
            this.sessions.disconnect(&key);
            if let Some(failure) = failure {
                let generation = this.sessions.begin_connect(&key).expect("not connecting");
                this.sessions.finish_connect(&key, generation, Err(format!("Saved password could not be read: {failure}")));
            }
            this.workspace.open_or_activate_connection_tab(key, "Postgres fixture");
            this.reset_active_view();
            cx.notify();
        });
        palette_settle(visual);
    };
    // Any other failure has Retry and Copy but no Reset.
    open(&app, visual, Some("connection refused".into()));
    assert!(visual.debug_bounds("connection-failed-retry").is_some());
    assert!(visual.debug_bounds("connection-failed-reset-passwords").is_none());
    // The unreadable master key adds it; it opens the same confirmation as the palette.
    open(&app, visual, Some(message));
    let button = visual.debug_bounds("connection-failed-reset-passwords").expect("the Reset button");
    visual.simulate_click(button.center(), Default::default());
    palette_settle(visual);
    assert!(palette_is_open(visual), "the confirmation opened");
    visual.simulate_keystrokes("escape");
    palette_settle(visual);
    assert!(!palette_is_open(visual));
}

#[gpui::test]
fn a_warning_from_the_storage_layer_is_shown_once_without_blocking(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = palette_app(cx);
    let message = format!("the saved data in /old/{}.db could not be moved into the app database", uuid::Uuid::new_v4());
    engine_storage::legacy::warn(message.clone());
    assert!(engine_storage::legacy::warnings_pending());
    app.update(visual, |this, cx| this.report_storage_warnings(cx));
    app.read_with(visual, |this, _| {
        assert_eq!(this.sidebar.notice.as_deref(), Some(message.as_str()), "under the sidebar");
        assert_eq!(this.error_log.log.export_text().matches(&message).count(), 1, "and in the Error Log");
    });
    // Nothing more to say the next frame, and no dialog was opened.
    assert!(!engine_storage::legacy::warnings_pending());
    assert!(!palette_is_open(visual));
}
