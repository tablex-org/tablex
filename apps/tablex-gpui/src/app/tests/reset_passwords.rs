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
        key_file: None,
        stray_key_file: None,
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

#[gpui::test]
fn a_reset_and_a_move_of_the_key_never_run_at_the_same_time(cx: &mut TestAppContext) {
    let (app, visual, keys, id) = reset_rig(cx);
    // A move of the key runs: the reset is refused and touches nothing.
    app.update_in(visual, |this, window, cx| {
        this.connection_ui.key_home.changing = true;
        this.reset_saved_passwords(window, cx);
    });
    visual.run_until_parked();
    assert_eq!(reset_secrets(&keys, &id), 4);
    app.read_with(visual, |this, _| assert!(this.connection_ui.key_home.changing, "the move still owns the flag"));
    // Nothing runs: the reset takes the flag (a move is refused meanwhile) and gives it back.
    app.update_in(visual, |this, window, cx| {
        this.connection_ui.key_home.changing = false;
        this.reset_saved_passwords(window, cx);
        assert!(this.connection_ui.key_home.changing, "held while the reset runs");
    });
    let waiting = keys.clone();
    wait_until(visual, "the secrets to go", move |_| reset_secrets(&waiting, &id) == 0);
    palette_settle(visual);
    app.read_with(visual, |this, _| assert!(!this.connection_ui.key_home.changing));
}

#[gpui::test]
fn choosing_a_key_home_while_the_key_is_being_moved_says_so_instead_of_returning_silently(cx: &mut TestAppContext) {
    let (app, visual, _keys, _id) = reset_rig(cx);
    let notifications = |visual: &mut gpui::VisualTestContext| visual.update(|window, cx| window.notifications(cx).len());
    let before = notifications(visual);
    app.update_in(visual, |this, window, cx| {
        this.connection_ui.key_home.changing = true;
        this.choose_key_home(engine_connect::key_home::Home::NoSave, window, cx);
    });
    visual.run_until_parked();
    assert_eq!(notifications(visual), before + 1, "the refusal is said, as the reset says it");
    app.read_with(visual, |this, _| assert!(this.connection_ui.key_home.changing, "the running move still owns the flag"));
}

/// A saved connection with a password in `keys`, and a form that edits it (rename, new colour and,
/// with `moves`, the host), as the dialog builds them.
fn saved_connection_edit(
    keys: &std::sync::Arc<connection_ui::MemoryCredentials>,
    save_password: bool,
    moves: bool,
) -> (
    ConnectionFormState,
    engine_storage::connection::SavedConnectionRecord,
) {
    use crate::credentials::{CredentialStore as _, SecretKind};
    let id = uuid::Uuid::new_v4();
    let mut form = ConnectionFormState {
        id: Some(id),
        name: "Before".into(),
        kind: ConnectionKind::Postgres,
        uri: "postgresql://u@127.0.0.1:1/db".into(),
        use_uri: true,
        save_password: true,
        ..Default::default()
    };
    form.parse_uri_fields();
    form.password = "db-pw".into();
    let original = form.to_record_saving(1.0, None, true).unwrap();
    form.password.clear();
    engine_storage::connection::upsert_connection(&original).unwrap();
    keys.write(SecretKind::Database, &original.id, "db-pw").unwrap();
    assert!(original.save_password);
    form.name = "After".into();
    form.color_hex = Some("#ef4444".into());
    form.use_uri = false;
    if moves {
        form.host = "other.invalid".into();
    }
    form.save_password = save_password;
    (form, original)
}

fn edited_connection_keeps_password(saving: bool, form_saves: bool, moves: bool) -> (bool, bool) {
    use crate::credentials::{CredentialStore as _, SecretKind};
    let keys = std::sync::Arc::new(connection_ui::MemoryCredentials::default());
    let (mut form, original) = saved_connection_edit(&keys, form_saves, moves);
    let store: std::sync::Arc<dyn crate::credentials::CredentialStore + Send + Sync> = keys.clone();
    let record = form.to_record_saving(2.0, Some(&original), saving).unwrap();
    assert_eq!(record.name, "After");
    connection_ui::save_connection_record(&form, record, Some(&original), &store, saving).unwrap();
    let stored = engine_storage::connection::get_connection(&original.id).unwrap().unwrap();
    (
        stored.save_password,
        keys.load(SecretKind::Database, &original.id).unwrap().is_some(),
    )
}

#[test]
fn editing_a_connection_while_passwords_are_not_saved_keeps_its_saved_password() {
    // "Don't save passwords" (this session, or the saved choice: both read as `saving == false`):
    // whatever the form says, an edit neither clears the secret nor the flag that points to it.
    for form_saves in [true, false] {
        assert_eq!(edited_connection_keeps_password(false, form_saves, false), (true, true), "form_saves={form_saves}");
    }
}

#[test]
fn moving_a_connection_while_passwords_are_not_saved_deletes_the_old_servers_password() {
    // A new server must not inherit the secret of the old one when passwords are saved again.
    for form_saves in [true, false] {
        assert_eq!(edited_connection_keeps_password(false, form_saves, true), (false, false), "form_saves={form_saves}");
    }
}

#[test]
fn unchecking_save_password_while_passwords_are_saved_still_clears_the_secret() {
    assert_eq!(edited_connection_keeps_password(true, false, true), (false, false));
    // The host moved and nothing was typed: the old password is not sent to the new server.
    assert_eq!(edited_connection_keeps_password(true, true, true), (false, false));
}

#[test]
fn without_saving_ssh_secrets_are_neither_written_nor_cleared_unless_their_hop_moved() {
    use crate::credentials::{CredentialStore as _, SecretKind};
    use crate::ssh_tunnel::{SshAuthMethod, SshSecretInput, SshSettings};
    let old = SshSettings {
        enabled: true,
        host: "bastion.example.com".into(),
        port: "2222".into(),
        user: "deploy".into(),
        auth: SshAuthMethod::Password,
        save_secrets: true,
        ..SshSettings::default()
    };
    let run = |edit: &dyn Fn(&mut SshSettings)| -> Option<String> {
        let keys = std::sync::Arc::new(connection_ui::MemoryCredentials::default());
        let (mut form, mut original) = saved_connection_edit(&keys, true, false);
        original.network_settings_json = old.write_into(&original.network_settings_json);
        engine_storage::connection::upsert_connection(&original).unwrap();
        keys.write(SecretKind::SshPassword, &original.id, "saved-ssh").unwrap();
        let mut new = old.clone();
        edit(&mut new);
        form.ssh = Some(new);
        form.ssh_secrets = SshSecretInput::new(engine_net::Secret::new("typed-ssh"), engine_net::Secret::default());
        let store: std::sync::Arc<dyn crate::credentials::CredentialStore + Send + Sync> = keys.clone();
        let record = form.to_record_saving(2.0, Some(&original), false).unwrap();
        connection_ui::save_connection_record(&form, record, Some(&original), &store, false).unwrap();
        keys.load(SecretKind::SshPassword, &original.id).unwrap()
    };
    // Unchanged route, "save" unchecked, a secret typed: the earlier run's secret stays as it was.
    assert_eq!(run(&|new| new.save_secrets = false).as_deref(), Some("saved-ssh"));
    assert_eq!(run(&|_| {}).as_deref(), Some("saved-ssh"));
    // Another bastion: no secret for it, and the old server's is gone.
    assert_eq!(run(&|new| new.host = "other.example.com".into()), None);
}
