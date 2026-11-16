// Passwords of connections that do not save theirs: typed in the connection dialog or in the prompt,
// kept in memory for this run only. The database is the scripted `MiniRedis` behind the REAL Redis
// driver dylib (it sees every `AUTH`); the credential store is a fake of the test's own, so nothing
// here touches the Keychain, and the connection store is the test process's temporary data dir.

/// A thread-safe credential store of the test's own.
#[derive(Default)]
struct TestKeys(std::sync::Mutex<std::collections::HashMap<(crate::credentials::SecretKind, String), String>>);

impl TestKeys {
    fn saved(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn password_of(&self, id: &str) -> Option<String> {
        self.0.lock().unwrap().get(&(crate::credentials::SecretKind::Database, id.to_owned())).cloned()
    }
}

impl crate::credentials::CredentialStore for TestKeys {
    fn load(&self, kind: crate::credentials::SecretKind, account: &str) -> Result<Option<String>, crate::credentials::CredentialError> {
        Ok(self.0.lock().unwrap().get(&(kind, account.to_owned())).cloned())
    }
    fn write(&self, kind: crate::credentials::SecretKind, account: &str, password: &str) -> Result<(), crate::credentials::CredentialError> {
        self.0.lock().unwrap().insert((kind, account.to_owned()), password.to_owned());
        Ok(())
    }
    fn delete(&self, kind: crate::credentials::SecretKind, account: &str) -> Result<(), crate::credentials::CredentialError> {
        self.0.lock().unwrap().remove(&(kind, account.to_owned()));
        Ok(())
    }
}

const SERVER_PASSWORD: &str = "srv-pw-needle-7";

/// An app with the real Redis driver installed, a server that wants `SERVER_PASSWORD` and no
/// connection yet.
struct PasswordRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    key: String,
    redis: MiniRedis,
    keys: std::sync::Arc<TestKeys>,
    store_dir: std::path::PathBuf,
}

impl<'a> PasswordRig<'a> {
    fn new(cx: &'a mut TestAppContext, server_password: Option<&str>) -> Self {
        let dylib = built_driver("dbengine-redis", "abi-v2");
        cx.dispatcher.allow_parking();
        cx.update(|cx| cx.set_reduce_motion(true));
        let (app, visual) = sidebar_test_app(cx);
        visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
        settle(visual);
        let store_dir = std::env::temp_dir().join(format!("tablex-password-store-{}", uuid::Uuid::new_v4()));
        let store = engine_driver_host::InstalledDriverStore::new(&store_dir);
        let sha256 = driver_sha256(&dylib);
        store
            .save(engine_driver_host::InstalledDriverRecord {
                id: "redis".into(),
                version: "0.0.0-test".into(),
                path: dylib,
                sha256,
                team_id: "TEST".into(),
                installed_at: "2026-10-11T00:00:00Z".into(),
                app_abi_version: 2,
                uri_schemes: vec!["redis".into(), "rediss".into()],
                license: "MIT".into(),
                source_repo: String::new(),
                disabled: false,
                ..Default::default()
            })
            .unwrap();
        let keys = std::sync::Arc::new(TestKeys::default());
        let keys_for_app: std::sync::Arc<dyn crate::credentials::CredentialStore + Send + Sync> = keys.clone();
        app.update(visual, |this, _| {
            this.scenario_ui.service = None;
            this.driver_ui.store = std::sync::Arc::new(store.clone());
            this.driver_ui.installed = drivers::DriverManagerUi::installed_ids(&store);
            this.connection_ui.credentials = keys_for_app;
        });
        Self { app, visual, key: String::new(), redis: MiniRedis::start_requiring(server_password), keys, store_dir }
    }

    /// A saved Redis connection (`user` empty: no user), with a record in the connection store when
    /// `stored`, that saves no password.
    fn add_connection(&mut self, user: &str, stored: bool) -> String {
        let id = uuid::Uuid::new_v4();
        let authority = if user.is_empty() { String::new() } else { format!("{user}@") };
        let mut form = ConnectionFormState {
            id: Some(id),
            name: format!("Cache {id}"),
            kind: ConnectionKind::Redis,
            uri: format!("redis://{authority}127.0.0.1:{}", self.redis.addr.port()),
            use_uri: true,
            save_password: false,
            ..Default::default()
        };
        form.parse_uri_fields();
        let record = form.to_record(1.0, None).unwrap();
        assert!(!record.save_password);
        if stored {
            engine_storage::connection::upsert_connection(&record).unwrap();
        }
        let saved = SavedConnection::from_record(&record);
        self.app.update(self.visual, |this, cx| {
            this.saved_connections.push(saved);
            this.navigator.set_connections(this.saved_connections.clone());
            this.sync_sidebar_connections(cx);
        });
        self.key = id.to_string();
        self.key.clone()
    }

    fn connect(&mut self) {
        let key = self.key.clone();
        self.app.update(self.visual, |this, cx| this.ensure_connected(key, None, cx));
        settle(self.visual);
    }

    fn status(&self) -> ConnectionStatus {
        self.app.read_with(self.visual, |this, _| this.sessions.status(&self.key))
    }

    fn wait_status(&mut self, label: &'static str, ready: impl Fn(&ConnectionStatus) -> bool) {
        let (app, key) = (self.app.clone(), self.key.clone());
        wait_until(self.visual, label, |visual| app.read_with(visual, |this, _| ready(&this.sessions.status(&key))));
        settle(self.visual);
    }

    fn wait_connected(&mut self) {
        self.wait_status("the connection", |status| matches!(status, ConnectionStatus::Connected));
    }

    fn wait_failed(&mut self) -> String {
        self.wait_status("the connect to fail", |status| matches!(status, ConnectionStatus::Failed(_)));
        let ConnectionStatus::Failed(message) = self.status() else { unreachable!() };
        message
    }

    fn wait_dialog(&mut self, selector: &'static str) {
        wait_until(self.visual, selector, |visual| {
            settle(visual);
            visual.debug_bounds(selector).is_some()
        });
    }

    fn rendered(&mut self, selector: &'static str) -> bool {
        settle(self.visual);
        self.visual.debug_bounds(selector).is_some()
    }

    fn click(&mut self, selector: &'static str) {
        settle(self.visual);
        click_debug_button(self.visual, selector);
        settle(self.visual);
    }

    /// How many password prompts have opened.
    fn prompts(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.tunnel_ui.opened_dialogs.iter().filter(|name| **name == "db-password").count())
    }

    fn remembered(&self) -> bool {
        self.app.read_with(self.visual, |this, _| this.connection_ui.session_passwords.contains(&self.key))
    }

    fn forget_session_passwords(&mut self) {
        let key = self.key.clone();
        self.app.update(self.visual, |this, _| this.connection_ui.session_passwords.clear(&key));
    }

    /// The last password the server was asked to accept.
    fn last_auth(&self) -> Option<String> {
        self.redis.auths().last().and_then(|args| args.last().cloned())
    }

    fn disconnect(&mut self) {
        let key = self.key.clone();
        self.app.update(self.visual, |this, cx| this.disconnect_connection(&key, cx));
        settle(self.visual);
    }

    fn log_text(&self) -> String {
        self.app.read_with(self.visual, |this, _| this.error_log.log.export_text())
    }

    fn errors_logged(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.error_log.log.len())
    }

    fn saved_record(&self) -> engine_storage::connection::SavedConnectionRecord {
        engine_storage::connection::get_connection(&self.key.to_uppercase()).unwrap().expect("a stored record")
    }
}

impl Drop for PasswordRig<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.store_dir);
    }
}

#[gpui::test]
fn a_password_typed_in_the_new_connection_dialog_and_not_saved_is_used_to_connect(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, Some(SERVER_PASSWORD));
    let url = format!("redis://alice@127.0.0.1:{}", rig.redis.addr.port());
    let name = format!("Typed {}", uuid::Uuid::new_v4());
    rig.app.update_in(rig.visual, |this, window, cx| this.open_connection_dialog(None, window, cx));
    settle(rig.visual);
    rig.click("connection-picker-category-docs-cache-search");
    rig.click("connection-picker-redis");
    rig.click("connection-picker-next");
    let inputs = rig.app.read_with(rig.visual, |this, _| this.connection_ui.last_inputs.clone().expect("the dialog opened"));
    rig.visual.update(|window, cx| inputs.uri.update(cx, |state, cx| state.set_value(url.clone(), window, cx)));
    rig.click("apply-connection-url");
    tls_type(rig.visual, "connection-config-name", &name);
    tls_type(rig.visual, "connection-config-password", SERVER_PASSWORD);
    // "Save password" is on for a new connection: untick it with a real click.
    assert!(inputs.save_password.get());
    let row = rig.visual.debug_bounds("connection-config-save-password").unwrap();
    rig.visual.simulate_click(gpui::point(row.left() + gpui::px(110.0 + 12.0 + 8.0), row.center().y), Default::default());
    settle(rig.visual);
    assert!(!inputs.save_password.get(), "the box is unticked");
    rig.click("connection-save");

    let saved = rig
        .app
        .read_with(rig.visual, |this, _| this.saved_connections.iter().find(|saved| saved.name == name).cloned())
        .expect("the dialog saved the connection");
    rig.key = saved.id.to_string();
    assert!(!saved.save_password, "the record does not claim a saved password");
    assert!(!rig.saved_record().save_password);
    assert!(!rig.saved_record().uri.contains(SERVER_PASSWORD), "no password in the record");
    assert_eq!(rig.keys.saved(), 0, "nothing was written to the credential store");
    assert!(rig.remembered(), "the typed password is kept for this run");

    // The bug: Save, then connect later. The typed password is used; nobody is asked.
    rig.connect();
    rig.wait_connected();
    assert_eq!(rig.prompts(), 0);
    assert_eq!(rig.last_auth().as_deref(), Some(SERVER_PASSWORD), "the server got the typed password");
    assert_eq!(rig.keys.saved(), 0);
    assert!(!rig.log_text().contains(SERVER_PASSWORD));
}

#[gpui::test]
fn after_a_relaunch_connecting_asks_for_the_password_and_enter_connects(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, Some(SERVER_PASSWORD));
    rig.add_connection("alice", false);
    assert!(!rig.remembered(), "a new run of the app knows no password");
    rig.connect();
    // The attempt waits for the answer, and nothing is sent before it.
    assert_eq!(rig.status(), ConnectionStatus::Connecting);
    rig.wait_dialog("password-prompt-input");
    assert!(rig.rendered("password-prompt-save") && rig.rendered("password-prompt-ok") && rig.rendered("password-prompt-cancel"));
    assert_eq!(rig.redis.connections(), 0);
    // The masked input has the focus: typing needs no click.
    rig.visual.simulate_input(SERVER_PASSWORD);
    rig.visual.simulate_keystrokes("enter");
    rig.wait_connected();
    assert_eq!(rig.prompts(), 1);
    assert_eq!(rig.last_auth().as_deref(), Some(SERVER_PASSWORD));
    assert!(rig.remembered());
    assert_eq!(rig.keys.saved(), 0, "not saved without the box");
    assert!(!rig.rendered("password-prompt-input"), "the prompt closed");

    // Connecting again in this run does not ask again.
    rig.disconnect();
    rig.connect();
    rig.wait_connected();
    assert_eq!(rig.prompts(), 1);
    assert!(!rig.log_text().contains(SERVER_PASSWORD));
}

#[gpui::test]
fn cancelling_the_password_prompt_leaves_the_connection_disconnected_without_an_error(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, Some(SERVER_PASSWORD));
    rig.add_connection("alice", false);
    let before = rig.errors_logged();
    for (index, escape) in [true, false].into_iter().enumerate() {
        rig.connect();
        rig.wait_dialog("password-prompt-input");
        if escape {
            rig.visual.simulate_keystrokes("escape");
        } else {
            rig.click("password-prompt-cancel");
        }
        settle(rig.visual);
        assert_eq!(rig.status(), ConnectionStatus::Disconnected, "attempt {index}");
        assert!(!rig.rendered("password-prompt-input"));
        assert!(!rig.remembered());
    }
    assert_eq!(rig.prompts(), 2, "the next connect asks again");
    assert_eq!(rig.errors_logged(), before, "no error was reported");
    assert_eq!(rig.redis.connections(), 0, "nothing was sent");
}

#[gpui::test]
fn saving_the_password_in_the_prompt_stores_it_and_the_next_connect_does_not_ask(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, Some(SERVER_PASSWORD));
    rig.add_connection("alice", true);
    rig.connect();
    rig.wait_dialog("password-prompt-input");
    // The "Save password" box: a real click on its square, left of the label.
    let row = rig.visual.debug_bounds("password-prompt-save").unwrap();
    rig.visual.simulate_click(gpui::point(row.left() + gpui::px(8.0), row.center().y), Default::default());
    settle(rig.visual);
    rig.visual.simulate_input(SERVER_PASSWORD);
    rig.click("password-prompt-ok");
    rig.wait_connected();
    assert_eq!(rig.last_auth().as_deref(), Some(SERVER_PASSWORD));
    assert_eq!(rig.keys.password_of(&rig.key.to_uppercase()).as_deref(), Some(SERVER_PASSWORD), "in the credential store");
    assert!(rig.saved_record().save_password, "the record says so");
    let saved = rig.app.read_with(rig.visual, |this, _| this.saved_connections.iter().find(|saved| saved.matches_id(&rig.key)).cloned().unwrap());
    assert!(saved.save_password, "and so does the app's list");

    // After a relaunch (nothing in memory) the saved password is read, nobody is asked.
    rig.disconnect();
    rig.forget_session_passwords();
    rig.connect();
    rig.wait_connected();
    assert_eq!(rig.prompts(), 1);
    assert_eq!(rig.redis.auths().len(), 2);
    assert_eq!(rig.last_auth().as_deref(), Some(SERVER_PASSWORD));
}

#[gpui::test]
fn a_connection_without_a_user_connects_without_a_prompt(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, None);
    rig.add_connection("", false);
    rig.connect();
    rig.wait_connected();
    assert_eq!(rig.prompts(), 0);
    assert!(rig.redis.auths().is_empty());
}

#[gpui::test]
fn another_session_and_a_reconnect_use_the_session_password_and_a_background_reconnect_never_asks(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, Some(SERVER_PASSWORD));
    rig.add_connection("alice", false);
    rig.app.update(rig.visual, |this, _| this.connection_ui.session_passwords.set(&rig.key, SERVER_PASSWORD));
    rig.connect();
    rig.wait_connected();
    assert_eq!((rig.prompts(), rig.redis.auths().len()), (0, 1));

    // A second session of the connection (a tab in manual commit, a transfer, the structure editor)
    // resolves the address itself once the live one is forgotten (an edit does that).
    let (spec, key) = (rig.key.clone(), rig.key.clone());
    let opened = rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.session_uris.remove(&key);
        let saved = this.saved_of(&spec).cloned().unwrap();
        let handle = this.driver_runtime.session_handles.get(&spec).cloned().unwrap();
        super::structure_edit::apply::SessionSpec {
            handle,
            uri: None,
            saved,
            connect_timeout: 0,
            tunnel: None,
            connector: this.connector(),
            prompts: this.tunnel_prompts(),
        }
    });
    let (session, _tunnel) = std::thread::spawn(move || opened.open()).join().unwrap().expect("the second session opens");
    drop(session);
    assert_eq!(rig.redis.auths().len(), 2, "it authenticated");
    assert_eq!(rig.last_auth().as_deref(), Some(SERVER_PASSWORD));

    // The session is lost: the reconnect uses the password, nobody is asked.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| assert!(this.mark_connection_lost(&key)));
    rig.connect();
    rig.wait_connected();
    assert_eq!((rig.prompts(), rig.redis.auths().len()), (0, 3));

    // A relaunch's recovery has no password: "Password required", and no prompt.
    rig.app.update(rig.visual, |this, _| assert!(this.mark_connection_lost(&key)));
    rig.forget_session_passwords();
    rig.app.update(rig.visual, |this, cx| this.without_prompts(|this| this.ensure_connected(key.clone(), None, cx)));
    let message = rig.wait_failed();
    assert!(crate::connections::is_missing_password_error(&message), "{message}");
    assert_eq!(rig.prompts(), 0);
    assert_eq!(rig.redis.auths().len(), 3, "nothing was sent");
    // The overview offers to enter it, next to Edit Connection.
    let (key, name) = (rig.key.clone(), "Cache".to_owned());
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.open_or_activate_connection_tab(key, name);
        this.reset_active_view();
        cx.notify();
    });
    settle(rig.visual);
    assert!(rig.rendered("connection-enter-password") && rig.rendered("connection-edit-password"));
    rig.click("connection-enter-password");
    rig.wait_dialog("password-prompt-input");
    rig.visual.simulate_input(SERVER_PASSWORD);
    rig.visual.simulate_keystrokes("enter");
    rig.wait_connected();
    assert_eq!(rig.prompts(), 1);
}

#[gpui::test]
fn a_wrong_password_fails_forgets_it_and_the_enter_password_button_asks_again(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, Some(SERVER_PASSWORD));
    rig.add_connection("alice", false);
    rig.connect();
    rig.wait_dialog("password-prompt-input");
    rig.visual.simulate_input("not-the-password-9");
    rig.visual.simulate_keystrokes("enter");
    let message = rig.wait_failed();
    assert!(!message.contains("not-the-password-9"), "{message}");
    assert!(!rig.remembered(), "a refused password is forgotten");
    assert_eq!(rig.last_auth().as_deref(), Some("not-the-password-9"));
    assert!(!rig.log_text().contains("not-the-password-9"));

    // The overview shows the failure, with Enter Password next to Retry and Edit.
    let (key, name) = (rig.key.clone(), "Cache".to_owned());
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.open_or_activate_connection_tab(key, name);
        this.reset_active_view();
        cx.notify();
    });
    settle(rig.visual);
    for selector in ["connection-enter-password", "connection-failed-retry", "connection-failed-edit"] {
        assert!(rig.rendered(selector), "{selector}");
    }
    rig.click("connection-enter-password");
    rig.wait_dialog("password-prompt-input");
    rig.visual.simulate_input(SERVER_PASSWORD);
    rig.visual.simulate_keystrokes("enter");
    rig.wait_connected();
    assert_eq!(rig.prompts(), 2);
    assert_eq!(rig.last_auth().as_deref(), Some(SERVER_PASSWORD));

    // Retry on a refused password asks again too.
    rig.disconnect();
    rig.forget_session_passwords();
    rig.connect();
    rig.wait_dialog("password-prompt-input");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert_eq!(rig.prompts(), 3);
}

#[gpui::test]
fn editing_the_user_or_host_forgets_the_session_password_and_a_rename_keeps_it(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, None);
    let id = rig.add_connection("alice", true);
    rig.app.update(rig.visual, |this, _| this.connection_ui.session_passwords.set(&id, "kept-for-now"));
    let open_edit = |rig: &mut PasswordRig| {
        let id = rig.key.clone();
        rig.app.update_in(rig.visual, |this, window, cx| {
            let saved = this.saved_connections.iter().find(|saved| saved.matches_id(&id)).cloned().unwrap();
            this.open_connection_dialog(Some(saved), window, cx);
        });
        settle(rig.visual);
    };

    // A new name: the same connection, the password stays.
    open_edit(&mut rig);
    tls_type(rig.visual, "connection-config-name", "Renamed");
    rig.click("connection-save");
    assert!(rig.remembered(), "a rename keeps it");
    assert!(!rig.rendered("connection-save"), "the dialog closed");

    // Another user: it is a different connection, the password is gone.
    open_edit(&mut rig);
    tls_type(rig.visual, "connection-config-username", "bob");
    rig.click("connection-save");
    assert!(rig.saved_record().uri.contains("bob@"), "{}", rig.saved_record().uri);
    assert!(!rig.remembered(), "another user forgets it");

    // Deleting the connection forgets it too.
    rig.app.update(rig.visual, |this, _| this.connection_ui.session_passwords.set(&id, "kept-for-now"));
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.delete_connection(&key, cx).unwrap());
    settle(rig.visual);
    assert!(!rig.remembered(), "a deleted connection forgets it");
}

#[gpui::test]
fn testing_a_stored_connection_with_a_typed_password_keeps_it_for_this_run_only(cx: &mut TestAppContext) {
    let mut rig = PasswordRig::new(cx, Some(SERVER_PASSWORD));
    let id = rig.add_connection("alice", true);
    rig.app.update_in(rig.visual, |this, window, cx| {
        let saved = this.saved_connections.iter().find(|saved| saved.matches_id(&id)).cloned().unwrap();
        this.open_connection_dialog(Some(saved), window, cx);
    });
    settle(rig.visual);
    tls_type(rig.visual, "connection-config-password", SERVER_PASSWORD);
    rig.click("test-connection");
    // The test connects with the typed password and reports it inline.
    let status = rig.app.read_with(rig.visual, |this, _| this.connection_ui.last_inputs.clone().unwrap().test_status);
    wait_until(rig.visual, "the test to report", |_| matches!(*status.borrow(), crate::connection_dialog::TestStatus::Success { .. } | crate::connection_dialog::TestStatus::Failure(_)));
    assert!(matches!(*status.borrow(), crate::connection_dialog::TestStatus::Success { .. }), "{:?}", status.borrow());
    assert_eq!(rig.last_auth().as_deref(), Some(SERVER_PASSWORD));
    assert!(rig.remembered(), "kept for this run, so connecting right after works");
    assert_eq!(rig.keys.saved(), 0, "Test saves nothing");
    assert!(!rig.saved_record().save_password);
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    rig.connect();
    rig.wait_connected();
    assert_eq!(rig.prompts(), 0);
}
