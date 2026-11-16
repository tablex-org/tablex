// Which databases a connection shows ("Show only the connection's database"), databases the user
// holds no CONNECT privilege on, and "Connect as Another User...": real clicks, hovers and typing
// in the sidebar and the connection dialog, on the production theme. The connection store is the
// test process's temporary data dir and the credential store a fake of the test's own: nothing
// here touches the Keychain or the user's TableX data.

/// A thread-safe credential store of the test's own.
#[derive(Default)]
struct ScopeKeys(std::sync::Mutex<std::collections::HashMap<(crate::credentials::SecretKind, String), String>>);

impl ScopeKeys {
    fn entries(&self) -> std::collections::BTreeMap<String, String> {
        self.0.lock().unwrap().iter().map(|((kind, account), secret)| (format!("{kind:?}:{account}"), secret.clone())).collect()
    }
}

impl crate::credentials::CredentialStore for ScopeKeys {
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

const SCOPE_HOST: &str = "db.example.invalid";

/// An app on the real connection store (the test process's temporary one), with the fake keys and a
/// sidebar file of its own.
fn scope_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, std::sync::Arc<ScopeKeys>, std::path::PathBuf) {
    cx.dispatcher.allow_parking();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = sidebar_test_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(1000.0)));
    settle(visual);
    let keys = std::sync::Arc::new(ScopeKeys::default());
    let keys_for_app: std::sync::Arc<dyn crate::credentials::CredentialStore + Send + Sync> = keys.clone();
    let sidebar = std::env::temp_dir().join(format!("tablex-scope-sidebar-{}.json", uuid::Uuid::new_v4()));
    let path = sidebar.clone();
    app.update(visual, |this, cx| {
        this.scenario_ui.service = None;
        this.connection_ui.credentials = keys_for_app;
        this.sidebar.path = Some(path);
        this.apply_saved_connections(Vec::new(), cx);
    });
    settle(visual);
    (app, visual, keys, sidebar)
}

/// A stored PostgreSQL connection (its record is in the store, the app has reloaded). `overrides`
/// is the record's `overrides_json` as an older or newer version of the app wrote it.
fn scope_add(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, name: &str, uri: &str, overrides: &str) -> String {
    let id = uuid::Uuid::new_v4();
    let mut form = ConnectionFormState { id: Some(id), name: name.into(), kind: ConnectionKind::Postgres, uri: uri.into(), use_uri: true, save_password: false, ..Default::default() };
    form.parse_uri_fields();
    let mut record = form.to_record(1.0, None).unwrap();
    record.overrides_json = overrides.into();
    engine_storage::connection::upsert_connection(&record).unwrap();
    app.update(visual, |this, cx| this.refresh_saved_connections(cx));
    settle(visual);
    id.to_string()
}

fn scope_record(id: &str) -> engine_storage::connection::SavedConnectionRecord {
    engine_storage::connection::get_connection(&id.to_uppercase()).unwrap().expect("the record is stored")
}

fn scope_id_of(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, name: &str) -> String {
    app.read_with(visual, |this, _| this.saved_connections.iter().find(|saved| saved.name == name).map(|saved| saved.id.to_string()).unwrap_or_else(|| panic!("{name} is saved")))
}

/// The databases of `connection` as a driver lists them (`can_connect` as it sends it: absent,
/// or `false`), loaded the way a finished load is applied.
fn scope_load_databases(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str, databases: &[(&str, Option<bool>)]) {
    let nodes: Vec<engine_driver_api::NavigatorNode> = databases
        .iter()
        .map(|(name, can_connect)| {
            let mut payload = serde_json::json!({ "database": name });
            if let Some(can_connect) = can_connect {
                payload["can_connect"] = serde_json::json!(can_connect);
            }
            engine_driver_api::NavigatorNode {
                id: format!("database:{name}"),
                kind: "database".into(),
                label: (*name).into(),
                subtitle: None,
                icon: Some("database".into()),
                has_children: true,
                payload,
                actions: vec!["refresh".into()],
            }
        })
        .collect();
    let connection = connection.to_owned();
    app.update(visual, |this, cx| {
        let root = NavigatorNodeId::Connection { connection };
        this.navigator.reset_node(&root);
        let request = this.navigator.toggle_expansion(&root).request.expect("the databases load");
        let children = crate::navigator::sql_driver::catalog_children(&request, &nodes);
        let generation = this.navigator.load_generation(&request).unwrap();
        this.navigator.apply_load(&request, generation, Ok(children));
        this.navigator_ui.generation = this.navigator.generation();
        cx.notify();
    });
    settle(visual);
}

/// The database rows the sidebar shows under `connection`.
fn scope_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str) -> Vec<String> {
    app.read_with(visual, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .filter(|row| matches!(&row.id, NavigatorNodeId::Database { connection: owner, .. } if owner == connection))
            .map(|row| row.label)
            .collect()
    })
}

fn scope_row_index(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str, database: &str) -> usize {
    let id = NavigatorNodeId::Database { connection: connection.to_owned(), database: database.to_owned() };
    app.read_with(visual, |this, _| this.navigator.visible_rows().iter().position(|row| row.id == id).unwrap_or_else(|| panic!("{database} is listed")))
}

/// Opens the dialog of a new connection (`None`) or an edit and returns its inputs on the fields.
fn scope_dialog(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, edit: Option<&str>) -> ConnectionDialogInputs {
    let edit = edit.map(str::to_owned);
    app.update_in(visual, |this, window, cx| {
        let saved = edit.as_ref().map(|id| this.saved_connections.iter().find(|saved| saved.matches_id(id)).cloned().expect("a saved connection"));
        this.open_connection_dialog(saved, window, cx);
    });
    for _ in 0..2 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let inputs = app.read_with(visual, |this, _| this.connection_ui.last_inputs.clone().expect("the dialog opened"));
    if edit.is_none() {
        // The kind is the default (PostgreSQL): on to the fields.
        inputs.step.set(crate::connection_dialog::ConnectionDialogStep::Config);
        settle(visual);
    }
    inputs
}

fn scope_only(inputs: &ConnectionDialogInputs, visual: &mut gpui::VisualTestContext) -> bool {
    visual.update(|_, cx| inputs.read_profile(cx).overrides.only_connection_database())
}

/// A click on the "Show only the connection's database" checkbox (the box is the first thing of its row).
fn scope_click_checkbox(visual: &mut gpui::VisualTestContext) {
    let row = visual.debug_bounds("connection-config-only-database").expect("the checkbox row is rendered");
    visual.simulate_click(gpui::point(row.origin.x + gpui::px(110.0 + 12.0 + 8.0), row.origin.y + gpui::px(10.0)), Default::default());
    settle(visual);
}

fn scope_value(inputs: &ConnectionDialogInputs, visual: &mut gpui::VisualTestContext, field: fn(&ConnectionDialogInputs) -> &gpui::Entity<gpui_component::input::InputState>) -> String {
    visual.update(|_, cx| field(inputs).read(cx).value().to_string())
}

#[gpui::test]
fn a_new_connection_with_a_database_lists_only_it_and_show_all_lists_every_database(cx: &mut TestAppContext) {
    let (app, visual, _keys, sidebar) = scope_app(cx);
    let name = format!("Reports app {}", uuid::Uuid::new_v4());
    let inputs = scope_dialog(&app, visual, None);

    // The option is on the Connection tab, off while no database is named.
    assert!(rendered(visual, "connection-config-only-database".into()));
    assert!(!scope_only(&inputs, visual), "no database named: nothing to narrow to");
    tls_type(visual, "connection-config-name", &name);
    tls_type(visual, "connection-config-host", SCOPE_HOST);
    tls_type(visual, "connection-config-username", "alice");
    assert!(!scope_only(&inputs, visual));
    // Naming a database turns it on, as typed.
    tls_type(visual, "connection-config-database", "app_db");
    assert!(scope_only(&inputs, visual), "a new connection that names a database shows only it");
    // The checkbox is a real control: a click turns it off and on again.
    scope_click_checkbox(visual);
    assert!(!scope_only(&inputs, visual), "unchecked");
    scope_click_checkbox(visual);
    assert!(scope_only(&inputs, visual), "checked");
    // Once the user has chosen, the field no longer decides.
    scope_click_checkbox(visual);
    tls_type(visual, "connection-config-database", "app_db2");
    assert!(!scope_only(&inputs, visual), "the user's choice stands when the database is edited");
    tls_type(visual, "connection-config-database", "app_db");
    scope_click_checkbox(visual);
    assert!(scope_only(&inputs, visual));

    click(visual, "connection-save".into());
    let id = scope_id_of(&app, visual, &name);
    assert_eq!(scope_record(&id).overrides_json, r#"{"navigator.only_connection_database":true}"#, "stored with the connection");

    // The navigator lists that database alone, whatever else the server has.
    scope_load_databases(&app, visual, &id, &[("app_db", None), ("reports", None), ("warehouse", None)]);
    assert_eq!(scope_rows(&app, visual, &id), ["app_db"]);

    // It survives a reload of everything: the records read back, and the dialog shows it on.
    let records = engine_storage::connection::list_connections().unwrap();
    let reloaded = records.iter().map(SavedConnection::from_record).collect::<Vec<_>>();
    let scopes = super::connection_policy::ConnectionPolicy::from_records(&records).database_scopes(&reloaded);
    assert_eq!(scopes.get(&id).map(String::as_str), Some("app_db"));
    let edit = scope_dialog(&app, visual, Some(&id));
    assert!(scope_only(&edit, visual), "reopened on");

    // Show all databases: unchecked and saved, every database is listed again.
    scope_click_checkbox(visual);
    assert!(!scope_only(&edit, visual));
    click(visual, "connection-save".into());
    assert_eq!(scope_record(&id).overrides_json, "", "nothing left to store");
    assert_eq!(scope_rows(&app, visual, &id), ["app_db", "reports", "warehouse"]);
    let again = scope_dialog(&app, visual, Some(&id));
    assert!(!scope_only(&again, visual), "reopened off");
    // And it can be turned on for an existing connection by hand.
    scope_click_checkbox(visual);
    click(visual, "connection-save".into());
    assert_eq!(scope_rows(&app, visual, &id), ["app_db"]);

    let _ = std::fs::remove_file(sidebar);
}

#[gpui::test]
fn an_existing_connection_without_the_option_keeps_showing_every_database(cx: &mut TestAppContext) {
    let (app, visual, _keys, sidebar) = scope_app(cx);
    // Saved before the option existed: no field, with and without other overrides.
    let plain = scope_add(&app, visual, "Saved earlier", &format!("postgresql://bob@{SCOPE_HOST}/app_db"), "");
    let tuned = scope_add(&app, visual, "Saved earlier, tuned", &format!("postgresql://bob@{SCOPE_HOST}/app_db"), r#"{"results.page_size":50}"#);
    // The option on, but no database named: nothing to narrow to.
    let unnamed = scope_add(&app, visual, "Saved without a database", &format!("postgresql://bob@{SCOPE_HOST}"), r#"{"navigator.only_connection_database":true}"#);
    let narrowed = scope_add(&app, visual, "Saved narrowed", &format!("postgresql://bob@{SCOPE_HOST}/reports"), r#"{"navigator.only_connection_database":true}"#);
    for id in [&plain, &tuned, &unnamed, &narrowed] {
        scope_load_databases(&app, visual, id, &[("app_db", None), ("reports", None), ("warehouse", None)]);
    }
    assert_eq!(scope_rows(&app, visual, &plain), ["app_db", "reports", "warehouse"]);
    assert_eq!(scope_rows(&app, visual, &tuned), ["app_db", "reports", "warehouse"]);
    assert_eq!(scope_rows(&app, visual, &unnamed), ["app_db", "reports", "warehouse"]);
    assert_eq!(scope_rows(&app, visual, &narrowed), ["reports"], "the one that asked for it");

    // Editing the older connection does not turn it on, not even by naming a database.
    let inputs = scope_dialog(&app, visual, Some(&plain));
    assert!(!scope_only(&inputs, visual));
    tls_type(visual, "connection-config-database", "warehouse");
    assert!(!scope_only(&inputs, visual), "an existing connection stays as it was");
    click(visual, "connection-save".into());
    assert_eq!(scope_record(&plain).overrides_json, "");
    // (A new address is a new tree: the databases load again.)
    scope_load_databases(&app, visual, &plain, &[("app_db", None), ("reports", None), ("warehouse", None)]);
    assert_eq!(scope_rows(&app, visual, &plain), ["app_db", "reports", "warehouse"]);

    // Other overrides survive turning the option on.
    let inputs = scope_dialog(&app, visual, Some(&tuned));
    scope_click_checkbox(visual);
    assert!(scope_only(&inputs, visual));
    click(visual, "connection-save".into());
    let stored = scope_record(&tuned).overrides_json;
    assert!(stored.contains(r#""results.page_size":50"#) && stored.contains(r#""navigator.only_connection_database":true"#), "{stored}");

    // Kinds without a database level do not offer it.
    let inputs = scope_dialog(&app, visual, Some(&plain));
    inputs.kind.set(ConnectionKind::Mysql);
    settle(visual);
    assert!(rendered(visual, "connection-config-only-database".into()), "MySQL lists databases too");
    inputs.kind.set(ConnectionKind::Mongodb);
    settle(visual);
    assert!(!rendered(visual, "connection-config-only-database".into()), "MongoDB does not");
    assert!(!scope_only(&inputs, visual));

    let _ = std::fs::remove_file(sidebar);
}

#[test]
fn only_the_kinds_with_databases_and_a_named_one_are_scoped() {
    let record = |kind: ConnectionKind, uri: &str, overrides: &str| {
        let mut form = ConnectionFormState { id: Some(uuid::Uuid::new_v4()), name: "c".into(), kind, uri: uri.into(), use_uri: true, ..Default::default() };
        form.parse_uri_fields();
        let mut record = form.to_record(1.0, None).unwrap();
        record.overrides_json = overrides.into();
        record
    };
    let on = r#"{"navigator.only_connection_database":true}"#;
    let records = [
        record(ConnectionKind::Postgres, "postgresql://u@h.example.invalid/app_db", on),
        record(ConnectionKind::Mysql, "mysql://u@h.example.invalid/shop", on),
        record(ConnectionKind::Mariadb, "mariadb://u@h.example.invalid/shop", on),
        record(ConnectionKind::Postgres, "postgresql://u@h.example.invalid/spaced%20name", on),
        record(ConnectionKind::Postgres, "postgresql://u@h.example.invalid", on),
        record(ConnectionKind::Mongodb, "mongodb://u@h.example.invalid/analytics", on),
        record(ConnectionKind::Postgres, "postgresql://u@h.example.invalid/off", ""),
        record(ConnectionKind::Postgres, "postgresql://u@h.example.invalid/off2", r#"{"navigator.only_connection_database":false}"#),
        record(ConnectionKind::Postgres, "postgresql://u@h.example.invalid/off3", r#"{"navigator.only_connection_database":"yes"}"#),
    ];
    let saved: Vec<SavedConnection> = records.iter().map(SavedConnection::from_record).collect();
    let scopes = super::connection_policy::ConnectionPolicy::from_records(&records).database_scopes(&saved);
    let of = |index: usize| scopes.get(&saved[index].id.to_string()).map(String::as_str);
    assert_eq!(
        (of(0), of(1), of(2), of(3), of(4), of(5), of(6), of(7), of(8)),
        (Some("app_db"), Some("shop"), Some("shop"), Some("spaced name"), None, None, None, None, None)
    );
}

// ---- databases without CONNECT ----------------------------------------------------------------

/// The sidebar scenario with the Postgres fixture connection (its user `alice`) and its databases
/// loaded as a driver lists them: one of which `alice` may not connect to.
fn no_connect_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = ns_app(cx);
    app.update(visual, |this, cx| {
        for saved in this.saved_connections.iter_mut().filter(|saved| saved.matches_id(crate::ui_fixture::POSTGRES_ID)) {
            saved.uri = format!("postgres://alice@{SCOPE_HOST}/warehouse");
        }
        this.navigator.set_connections(this.saved_connections.clone());
        cx.notify();
    });
    scope_load_databases(&app, visual, crate::ui_fixture::POSTGRES_ID, &[("warehouse", None), ("reports", Some(false)), ("shared", Some(true))]);
    (app, visual)
}

const NO_CONNECT_TIP: &str = "sidebar-no-connect-tooltip No CONNECT privilege for alice";

fn hover_row(visual: &mut gpui::VisualTestContext, index: usize) {
    visual.simulate_mouse_move(gpui::point(gpui::px(2.0), gpui::px(2.0)), None, Default::default());
    settle(visual);
    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{index}"))).expect("the row is drawn");
    visual.simulate_mouse_move(gpui::point(row.left() + gpui::px(80.0), row.center().y), None, Default::default());
    visual.executor().advance_clock(Duration::from_millis(700));
    settle(visual);
}

#[gpui::test]
fn a_database_without_connect_says_so_in_a_tooltip_and_a_click_connects_nothing(cx: &mut TestAppContext) {
    let (app, visual) = no_connect_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let (warehouse, reports, shared) = (scope_row_index(&app, visual, pg, "warehouse"), scope_row_index(&app, visual, pg, "reports"), scope_row_index(&app, visual, pg, "shared"));
    let reports_id = NavigatorNodeId::Database { connection: pg.into(), database: "reports".into() };
    assert_eq!(scope_rows(&app, visual, pg), ["warehouse", "reports", "shared"], "every database is listed, the closed one too");
    assert!(app.read_with(visual, |this, _| this.navigator.database_denied(&reports_id)));

    // Only the closed one explains itself; a payload without the field, or with `true`, is open.
    for open in [warehouse, shared] {
        hover_row(visual, open);
        assert!(!ns_has(visual, NO_CONNECT_TIP.into()), "row {open} is connectable");
    }
    hover_row(visual, reports);
    assert!(ns_has(visual, NO_CONNECT_TIP.into()), "the tooltip names the user");
    visual.simulate_mouse_move(gpui::point(gpui::px(2.0), gpui::px(2.0)), None, Default::default());
    settle(visual);
    assert!(!ns_has(visual, NO_CONNECT_TIP.into()), "and goes with the pointer");

    // A click on it starts no connect and no load; it opens the panel with the way out.
    assert_eq!(ns_status(&app, visual), ConnectionStatus::Disconnected);
    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{reports}"))).unwrap();
    visual.simulate_click(gpui::point(row.left() + gpui::px(80.0), row.center().y), Default::default());
    settle(visual);
    assert_eq!(ns_status(&app, visual), ConnectionStatus::Disconnected, "no doomed connect");
    let (state, expanded, open) = app.read_with(visual, |this, _| {
        (
            this.navigator.load_state(&reports_id).cloned(),
            this.navigator.visible_rows().iter().any(|row| row.id == reports_id && row.expanded),
            this.navigator_ui.problem_open.clone(),
        )
    });
    assert!(matches!(&state, Some(crate::navigator::NodeLoadState::Failed(text)) if text.contains("no CONNECT privilege for alice")), "{state:?}");
    assert!(!expanded, "nothing opened");
    assert_eq!(open, Some(reports_id.clone()), "the panel is open");
    assert!(ns_has(visual, format!("sidebar-problem-popover-{reports}")));
    assert!(ns_has(visual, format!("sidebar-problem-another-user-{reports}")), "with Connect as Another User");

    // The double click and the chevron of the closed one do the same; an open one connects.
    app.update(visual, |this, cx| {
        let row = this.navigator.visible_rows().into_iter().find(|row| row.id == reports_id).unwrap();
        this.handle_navigator_disclosure(&row, cx);
    });
    settle(visual);
    assert_eq!(ns_status(&app, visual), ConnectionStatus::Disconnected);
    visual.simulate_keystrokes("escape");
    settle(visual);
    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{warehouse}"))).unwrap();
    visual.simulate_click(gpui::point(row.left() + gpui::px(80.0), row.center().y), Default::default());
    settle(visual);
    assert_eq!(ns_status(&app, visual), ConnectionStatus::Connecting, "a database the user may open starts the connect");
}

#[gpui::test]
fn the_panel_of_a_closed_database_opens_the_dialog_of_another_user(cx: &mut TestAppContext) {
    let (app, visual) = no_connect_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let reports = scope_row_index(&app, visual, pg, "reports");
    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{reports}"))).unwrap();
    visual.simulate_click(gpui::point(row.left() + gpui::px(80.0), row.center().y), Default::default());
    settle(visual);
    ns_click(visual, format!("sidebar-problem-another-user-{reports}"));
    let inputs = app.read_with(visual, |this, _| this.connection_ui.last_inputs.clone()).expect("the dialog opened");
    assert!(!inputs.is_picker(), "on the fields, the server is known");
    assert_eq!(scope_value(&inputs, visual, |inputs| &inputs.database), "reports");
    assert_eq!(scope_value(&inputs, visual, |inputs| &inputs.username), "", "no user");
    assert_eq!(scope_value(&inputs, visual, |inputs| &inputs.password), "", "no password");
    assert!(!ns_has(visual, format!("sidebar-problem-popover-{reports}")), "the panel closed");
}

// ---- the error panel --------------------------------------------------------------------------

/// Opens `database` of the Postgres fixture and fails it with `error`, as the driver's answer lands.
fn fail_database(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, database: &str, error: &str) -> usize {
    let pg = crate::ui_fixture::POSTGRES_ID;
    let id = NavigatorNodeId::Database { connection: pg.into(), database: database.into() };
    let error = error.to_owned();
    app.update(visual, |this, cx| {
        this.navigator.reset_node(&id);
        let request = this.navigator.toggle_expansion(&id).request.expect("the load");
        let generation = this.navigator.load_generation(&request).unwrap();
        this.navigator.apply_load(&request, generation, Err(error));
        this.navigator_ui.generation = this.navigator.generation();
        cx.notify();
    });
    settle(visual);
    scope_row_index(app, visual, pg, database)
}

#[gpui::test]
fn a_refused_database_offers_another_user_and_other_failures_do_not(cx: &mut TestAppContext) {
    let (app, visual) = no_connect_app(cx);
    for (error, offered) in [
        ("permission denied for database warehouse", true),
        ("FATAL: permission denied for database \"warehouse\"\nDETAIL: User does not have CONNECT privilege.", true),
        ("password authentication failed for user \"alice\"", true),
        ("Access denied for user 'alice'@'10.0.0.5' (using password: YES)", true),
        ("no pg_hba.conf entry for host \"10.0.0.5\", user \"alice\", database \"warehouse\"", true),
        ("connection refused: is the server running on that host?", false),
        ("timed out waiting for the server", false),
        ("relation \"orders\" does not exist", false),
    ] {
        let index = fail_database(&app, visual, "warehouse", error);
        assert!(ns_has(visual, format!("sidebar-problem-{index}")), "{error}: the failure marker");
        ns_click(visual, format!("sidebar-problem-{index}"));
        assert!(ns_has(visual, format!("sidebar-problem-popover-{index}")), "{error}");
        assert_eq!(ns_has(visual, format!("sidebar-problem-another-user-{index}")), offered, "{error}");
        visual.simulate_keystrokes("escape");
        settle(visual);
    }

    // The button of the panel opens the dialog on that database.
    let index = fail_database(&app, visual, "warehouse", "permission denied for database warehouse");
    ns_click(visual, format!("sidebar-problem-{index}"));
    ns_click(visual, format!("sidebar-problem-another-user-{index}"));
    let inputs = app.read_with(visual, |this, _| this.connection_ui.last_inputs.clone()).expect("the dialog opened");
    assert_eq!(scope_value(&inputs, visual, |inputs| &inputs.database), "warehouse");
    assert!(!ns_has(visual, format!("sidebar-problem-popover-{index}")), "the panel closed behind it");
    visual.simulate_keystrokes("escape");
    settle(visual);

    // Not for a failed connection: that row has its own Retry.
    let pg = crate::ui_fixture::POSTGRES_ID;
    let connection_row = scope_connection_index(&app, visual, pg);
    ns_set(&app, visual, ConnectionStatus::Failed("password authentication failed for user \"alice\"".into()));
    ns_click(visual, format!("sidebar-problem-{connection_row}"));
    assert!(ns_has(visual, format!("sidebar-problem-popover-{connection_row}")));
    assert!(!ns_has(visual, format!("sidebar-problem-another-user-{connection_row}")), "a connection that failed to open has its own Retry");
}

fn scope_connection_index(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str) -> usize {
    let id = NavigatorNodeId::Connection { connection: connection.to_owned() };
    app.read_with(visual, |this, _| this.navigator.visible_rows().iter().position(|row| row.id == id).expect("the connection row"))
}

// ---- Connect as Another User ------------------------------------------------------------------

#[test]
fn another_user_starts_from_the_servers_address_without_its_user() {
    let record = {
        let mut form = ConnectionFormState {
            id: Some(uuid::Uuid::new_v4()),
            name: "Main server".into(),
            kind: ConnectionKind::Postgres,
            uri: format!("postgresql://alice:s3cret@{SCOPE_HOST}:5433/app_db?sslmode=require&connect_timeout=7"),
            use_uri: true,
            save_password: true,
            is_pinned: true,
            ..Default::default()
        };
        form.parse_uri_fields();
        form.to_record(1.0, None).unwrap()
    };
    let form = ConnectionFormState::another_user_of(&SavedConnection::from_record(&record), "reports");
    assert_eq!(form.id, None, "a new connection");
    assert_eq!((form.host.as_str(), form.port.as_str(), form.database.as_str()), (SCOPE_HOST, "5433", "reports"));
    assert_eq!(form.kind, ConnectionKind::Postgres);
    assert_eq!((form.username.as_str(), form.password.as_str()), ("", ""), "no user, no password");
    assert!(!form.is_pinned && form.save_password);
    assert_eq!(form.name, format!("reports ({SCOPE_HOST})"));
    assert_eq!(form.connect_timeout, "7");
    assert_eq!(form.tls.mode, ConnectionFormState::from_saved(&SavedConnection::from_record(&record)).tls.mode, "the TLS settings are the source's");
    for text in [&form.uri, &form.name] {
        assert!(!text.contains("alice") && !text.contains("s3cret"), "{text}");
    }
    assert!(form.uri.contains(&format!("@{SCOPE_HOST}")) || form.uri.contains(&format!("//{SCOPE_HOST}")), "{}", form.uri);
    assert!(form.uri.ends_with("sslmode=require") || form.uri.contains("sslmode=require"), "{}", form.uri);
}

#[gpui::test]
fn connect_as_another_user_opens_a_prefilled_dialog_and_saves_a_second_connection(cx: &mut TestAppContext) {
    let (app, visual, keys, sidebar) = scope_app(cx);
    let source_name = format!("Main server {}", uuid::Uuid::new_v4());

    // The source: a TLS connection of alice through a bastion, in a group, with a type and its own
    // password and a bastion password in the credential store.
    let ssh = SshSettings { enabled: true, host: "bastion.example.invalid".into(), user: "jump".into(), ..Default::default() };
    let source = scope_add(
        &app,
        visual,
        &source_name,
        &format!("postgresql://alice@{SCOPE_HOST}:5433/app_db?sslmode=require&connect_timeout=7"),
        r#"{"results.page_size":50,"agent.access":"read_write"}"#,
    );
    let mut record = scope_record(&source);
    record.network_settings_json = ssh.write_into(&record.network_settings_json);
    record.connection_type = crate::settings::connection::PRODUCTION.into();
    record.save_password = true;
    engine_storage::connection::upsert_connection(&record).unwrap();
    let source_key = record.id.clone();
    crate::credentials::CredentialStore::write(&*keys, crate::credentials::SecretKind::Database, &source_key, "source-secret").unwrap();
    crate::credentials::CredentialStore::write(&*keys, crate::credentials::SecretKind::Proxy, &source_key, "bastion-secret").unwrap();
    app.update(visual, |this, cx| {
        this.refresh_saved_connections(cx);
        this.change_sidebar_preferences(
            |prefs| {
                prefs.add_group("team", "Team", None)?;
                prefs.move_connection(&source, Some("team"), 0)
            },
            cx,
        )
        .unwrap();
    });
    scope_load_databases(&app, visual, &source, &[("app_db", None), ("reports", None)]);
    let secrets_before = keys.entries();
    assert_eq!(secrets_before.len(), 2);

    // The database's context menu has the entry; choosing it opens the dialog.
    let reports = scope_row_index(&app, visual, &source, "reports");
    right_click_row(visual, reports);
    assert!(rendered(visual, "database-menu-another-user".into()), "the entry is in the database's context menu");
    click(visual, "database-menu-another-user".into());
    let inputs = app.read_with(visual, |this, _| this.connection_ui.last_inputs.clone()).expect("the dialog opened");
    visual.update(|_, cx| assert!(inputs.read_form(cx, Default::default()).id.is_none()));
    assert!(!inputs.is_picker(), "on the fields: the server is already chosen");
    assert!(rendered(visual, "connection-config-name".into()));

    // What is copied: the address, the options, the TLS and tunnel settings, the type, the group.
    let field = |visual: &mut gpui::VisualTestContext, field: fn(&ConnectionDialogInputs) -> &gpui::Entity<gpui_component::input::InputState>| scope_value(&inputs, visual, field);
    assert_eq!(field(visual, |inputs| &inputs.host), SCOPE_HOST);
    assert_eq!(field(visual, |inputs| &inputs.port), "5433");
    assert_eq!(field(visual, |inputs| &inputs.database), "reports");
    assert_eq!(field(visual, |inputs| &inputs.name), format!("reports ({SCOPE_HOST})"));
    assert_eq!(field(visual, |inputs| &inputs.connect_timeout), "7");
    assert_eq!(inputs.kind.get(), ConnectionKind::Postgres);
    let form = visual.update(|_, cx| inputs.read_form(cx, ConnectionFormState::new(ConnectionKind::Postgres)));
    let source_tls = ConnectionFormState::from_saved(&app.read_with(visual, |this, _| this.saved_connections.iter().find(|saved| saved.matches_id(&source)).cloned().unwrap())).tls;
    assert_eq!(form.tls, source_tls);
    assert_ne!(form.tls, Default::default(), "the source has TLS settings to copy");
    let tunnel = form.ssh.clone().unwrap();
    assert!(tunnel.enabled && tunnel.host == "bastion.example.invalid" && tunnel.user == "jump", "{tunnel:?}");
    assert!(form.ssh_secrets.secrets(&tunnel).is_empty(), "no tunnel secret is copied");
    let profile = form.profile.clone().unwrap();
    assert_eq!(profile.type_id, crate::settings::connection::PRODUCTION, "the connection type");
    assert!(profile.overrides.only_connection_database(), "shows only this database");
    assert!(profile.overrides.get("results.page_size").is_some(), "the other overrides come along");
    assert!(profile.overrides.get(crate::settings::connection::AGENT_ACCESS).is_none(), "agent access starts from the type's default");

    // What is not: the user, the password, anything in the credential store, the URL's user.
    assert_eq!(field(visual, |inputs| &inputs.username), "");
    assert_eq!(field(visual, |inputs| &inputs.password), "");
    assert!(!field(visual, |inputs| &inputs.uri).contains("alice"));
    assert_eq!(keys.entries(), secrets_before, "nothing was written to the credential store");
    // The User field has the focus: typing goes there.
    let focused = visual.update(|window, cx| inputs.username.read(cx).focus_handle(cx).is_focused(window));
    assert!(focused, "the User field is focused");
    visual.simulate_input("carol");
    settle(visual);
    assert_eq!(field(visual, |inputs| &inputs.username), "carol");
    assert_eq!(field(visual, |inputs| &inputs.password), "");

    // Save: a second connection; the source is untouched.
    click(visual, "connection-save".into());
    let new_name = format!("reports ({SCOPE_HOST})");
    let created = scope_id_of(&app, visual, &new_name);
    assert_ne!(created, source);
    let (count, new_uri, source_uri) = app.read_with(visual, |this, _| {
        (
            this.saved_connections.iter().filter(|saved| saved.matches_id(&source) || saved.matches_id(&created)).count(),
            this.saved_connections.iter().find(|saved| saved.matches_id(&created)).unwrap().uri.clone(),
            this.saved_connections.iter().find(|saved| saved.matches_id(&source)).unwrap().uri.clone(),
        )
    });
    assert_eq!(count, 2, "both connections exist");
    assert!(new_uri.contains("carol@") && new_uri.contains(&format!("{SCOPE_HOST}:5433/reports")) && new_uri.contains("sslmode=require"), "{new_uri}");
    assert!(source_uri.contains("alice@") && source_uri.contains("/app_db"), "{source_uri}");
    let stored = scope_record(&created);
    assert!(!stored.save_password, "no password was typed, so none is saved");
    assert_eq!(stored.connection_type, crate::settings::connection::PRODUCTION);
    assert!(stored.overrides_json.contains(r#""navigator.only_connection_database":true"#) && !stored.overrides_json.contains("agent.access"), "{}", stored.overrides_json);
    assert!(stored.network_settings_json.contains("bastion.example.invalid"), "{}", stored.network_settings_json);
    assert_eq!(keys.entries(), secrets_before, "still nothing for the new connection: no secret was copied");
    let group = app.read_with(visual, |this, _| this.navigator.preferences().connections.iter().find(|item| item.id == created).and_then(|item| item.group.clone()));
    assert_eq!(group.as_deref(), Some("team"), "placed in the group of the source");

    // It lists only `reports`, the source still its own database.
    scope_load_databases(&app, visual, &created, &[("app_db", None), ("reports", None), ("warehouse", None)]);
    assert_eq!(scope_rows(&app, visual, &created), ["reports"]);

    let _ = std::fs::remove_file(sidebar);
}

#[gpui::test]
fn the_database_menu_entry_is_only_for_servers_with_databases(cx: &mut TestAppContext) {
    let (app, visual) = no_connect_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    // A SQL database row of a connection that lists databases has it ...
    let warehouse = scope_row_index(&app, visual, pg, "warehouse");
    right_click_row(visual, warehouse);
    assert!(rendered(visual, "database-menu-another-user".into()));
    visual.simulate_keystrokes("escape");
    settle(visual);
    // ... a table does not.
    app.update(visual, |this, cx| {
        let id = NavigatorNodeId::Database { connection: pg.into(), database: "warehouse".into() };
        let request = this.navigator.toggle_expansion(&id).request.expect("the load");
        let generation = this.navigator.load_generation(&request).unwrap();
        this.navigator.apply_load(&request, generation, Ok(vec![crate::navigator::schema_child(pg, "warehouse", "public")]));
        cx.notify();
    });
    settle(visual);
    let schema = app.read_with(visual, |this, _| this.navigator.visible_rows().iter().position(|row| matches!(&row.id, NavigatorNodeId::Schema { .. })).unwrap());
    right_click_row(visual, schema);
    assert!(!rendered(visual, "database-menu-another-user".into()), "a schema is not a database");
}
