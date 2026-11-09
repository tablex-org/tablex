#[test]
fn connection_dialog_width_is_capped_by_viewport() {
    assert_eq!(connection_dialog_width(px(1200.0), true), px(880.0));
    assert_eq!(connection_dialog_width(px(700.0), true), px(668.0));
    assert_eq!(connection_dialog_width(px(700.0), false), px(560.0));
}

#[test]
fn connection_dialog_height_reserves_viewport_for_title_and_footer() {
    assert_eq!(connection_dialog_height(px(1200.0)), px(600.0));
    assert_eq!(connection_dialog_height(px(800.0)), px(600.0));
    assert_eq!(connection_dialog_height(px(600.0)), px(420.0));
}

#[test]
fn runtime_uri_for_form_prefers_a_password_embedded_in_the_uri() {
    let form = ConnectionFormState {
        name: "Updated credentials".to_string(),
        kind: ConnectionKind::Postgres,
        uri: "postgresql://alice:new%40password@localhost/app".to_string(),
        save_password: true,
        use_uri: true,
        ..Default::default()
    };
    let original = engine_storage::connection::SavedConnectionRecord {
        id: "11111111-1111-1111-1111-111111111111".to_string(),
        name: "Updated credentials".to_string(),
        uri: "postgresql://alice@localhost/app".to_string(),
        db_type: crate::connections::DB_TYPE_POSTGRESQL.to_string(),
        color_hex: "#34C759".to_string(),
        save_password: true,
        save_proxy_password: false,
        network_settings_json: "{}".to_string(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: String::new(),
    };

    assert_eq!(
        runtime_uri_for_form(&form, Some(&original)).unwrap().expose_for_test(),
        "postgresql://alice:new%40password@localhost/app"
    );
}

#[gpui::test]
fn opened_dialog_is_mounted_in_the_window(cx: &mut TestAppContext) {
    cx.update(|app| {
            gpui_component::init(app);
            // Dialog slide-in is a wall-clock animation; end it immediately.
            app.set_reduce_motion(true);
        });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    cx.update(|window, app| {
        window.open_dialog(app, |dialog, _, _| {
            dialog
                .title("Test Dialog")
                .child(div().debug_selector(|| "test-dialog-content".to_string()).child("Dialog content"))
        });
    });
    app.update(cx, |_, cx| cx.notify());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("test-dialog-content").is_some(),
        "open_dialog registered state but no dialog content was rendered"
    );
}

#[gpui::test]
fn connection_dialog_uses_picker_and_config_widths(cx: &mut TestAppContext) {
    cx.update(|app| {
            gpui_component::init(app);
            // Dialog slide-in is a wall-clock animation; end it immediately.
            app.set_reduce_motion(true);
        });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.open_connection_dialog(None, window, cx);
        });
    });
    // Dialog centers from the previous frame's size: settle two frames.
    for _ in 0..2 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    let picker = cx
        .debug_bounds("connection-picker")
        .expect("connection picker should render");
    assert!(picker.size.width >= px(800.0));
    assert!(picker.size.height >= px(580.0));
    assert!(cx.debug_bounds("connection-picker-selected").is_some());
    let footer = cx
        .debug_bounds("connection-picker-footer")
        .expect("full-width footer");
    assert!(footer.size.width > picker.size.width);
    assert_eq!(footer.size.height, px(60.0));
    let next = cx
        .debug_bounds("connection-picker-next")
        .expect("picker next button should render");
    assert!(
        footer.contains(&next.center()),
        "Next must remain inside its footer: next={next:?}, footer={footer:?}, selected={:?}, dialog={:?}",
        cx.debug_bounds("connection-picker-selected"),
        cx.debug_bounds("dialog-0")
    );
    cx.simulate_click(next.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let config = cx
        .debug_bounds("connection-config")
        .expect("connection config should render");
    assert!(config.size.width <= px(520.0));
    assert!(config.size.height >= px(580.0));
    assert!(cx.debug_bounds("test-connection").is_some());
    assert!(cx.debug_bounds("connection-save").is_some());
    assert!(cx.debug_bounds("select-visible-databases").is_some());
    assert!(cx.debug_bounds("save-and-connect").is_none(), "the new-connection footer shows just Test + Save");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("connection-config").is_none(), "Escape closes the connection dialog");
}

#[gpui::test]
fn picker_next_stays_disabled_when_search_hides_selection(cx: &mut TestAppContext) {
    cx.update(|app| {
            gpui_component::init(app);
            // Dialog slide-in is a wall-clock animation; end it immediately.
            app.set_reduce_motion(true);
        });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.open_connection_dialog(None, window, cx);
        });
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let search = cx
        .debug_bounds("connection-picker-search")
        .expect("picker search should render");
    cx.simulate_click(search.center(), Default::default());
    cx.simulate_keystrokes("SQLite");
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let next = cx
        .debug_bounds("connection-picker-next")
        .expect("picker next button should render");
    cx.simulate_click(next.center(), Default::default());
    cx.simulate_keystrokes("enter");
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert!(cx.debug_bounds("connection-picker").is_some());
}

#[gpui::test]
fn reopening_connection_dialog_rejects_stale_test_results_and_duplicate_tests(
    cx: &mut TestAppContext,
) {
    cx.update(|app| {
            gpui_component::init(app);
            // Dialog slide-in is a wall-clock animation; end it immediately.
            app.set_reduce_motion(true);
        });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.connection_ui.test_generation = 7;
            this.connection_ui.test_in_flight.set(true);
            this.start_connection_test(
                ConnectionFormState::new(ConnectionKind::Postgres),
                None,
                window.window_handle(),
                Default::default(),
                cx,
            );
            assert_eq!(this.connection_ui.test_generation, 7);
            assert!(this.connection_ui.test_in_flight.get());

            this.invalidate_connection_test();
            assert_eq!(this.connection_ui.test_generation, 8);
            assert!(!this.connection_ui.test_in_flight.get());
            assert!(!this.finish_connection_test(7));

            this.connection_ui.test_generation = 7;
            this.connection_ui.test_in_flight.set(true);
            this.open_connection_dialog(None, window, cx);
            assert_eq!(this.connection_ui.test_generation, 8);
            assert!(!this.connection_ui.test_in_flight.get());
            this.connection_ui.test_in_flight.set(true);
            assert!(!this.finish_connection_test(7));
            assert!(this.connection_ui.test_in_flight.get());
        });
    });
}

#[gpui::test]
fn scenario_save_and_connect_cancels_delayed_test_result(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        // The dialog slides in on a wall-clock transition; clicks in these
        // tests must hit its settled position (as upstream dialog tests do).
        app.set_reduce_motion(true);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "new-connection",
        "--ui-fixture",
        "sample",
    ])
    .unwrap()
    .unwrap();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.open_connection_dialog(None, window, cx);
        });
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let next = cx.debug_bounds("connection-picker-next").unwrap();
    cx.simulate_click(next.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let name = cx.debug_bounds("connection-config-name").unwrap();
    cx.simulate_click(name.center(), Default::default());
    cx.simulate_keystrokes("Scenario connection");
    let host = cx.debug_bounds("connection-config-host").unwrap();
    cx.simulate_click(host.center(), Default::default());
    cx.simulate_keystrokes("localhost");

    let test = cx.debug_bounds("test-connection").unwrap();
    cx.simulate_click(test.center(), Default::default());
    cx.run_until_parked();
    app.update(cx, |this, _| {
        assert!(this.connection_ui.test_in_flight.get());
        assert_eq!(this.scenario_ui.service.as_ref().unwrap().test_count(), 1);
    });

    let save = cx.debug_bounds("connection-save").unwrap();
    cx.simulate_click(save.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.update(cx, |this, _| assert!(!this.connection_ui.test_in_flight.get()));

    cx.executor().advance_clock(Duration::from_millis(701));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.update(cx, |this, _| {
        assert_eq!(this.scenario_ui.test_terminal_count, 0)
    });
}

#[gpui::test]
fn save_and_connect_reconnects_an_existing_session_for_uri_override(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(|app| {
            gpui_component::init(app);
            // Dialog slide-in is a wall-clock animation; end it immediately.
            app.set_reduce_motion(true);
        });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_isolated);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    app.update(cx, |this, cx| {
        let saved = SavedConnection {
            id: uuid::Uuid::nil(),
            name: "Local database".to_string(),
            uri: "postgresql://localhost/app".to_string(),
            kind: ConnectionKind::Postgres,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        };
        let connection = saved.id.to_string();
        this.saved_connections = vec![saved];
        let attempt = this
            .sessions
            .begin_connect(&connection)
            .expect("connect should start");
        assert!(this.sessions.finish_connect(&connection, attempt, Ok(())));

        this.ensure_connected_with_uri(
            connection.clone(),
            Some("postgresql://localhost/updated".into()),
            None,
            cx,
        );

        assert_eq!(
            this.sessions.status(&connection),
            ConnectionStatus::Connecting
        );
    });
}

#[gpui::test]
fn save_and_connect_replaces_a_connecting_session_for_uri_override(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(|app| {
            gpui_component::init(app);
            // Dialog slide-in is a wall-clock animation; end it immediately.
            app.set_reduce_motion(true);
        });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_isolated);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    app.update(cx, |this, cx| {
        let saved = SavedConnection {
            id: uuid::Uuid::nil(),
            name: "Local database".to_string(),
            uri: "postgresql://localhost/app".to_string(),
            kind: ConnectionKind::Postgres,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        };
        let connection = saved.id.to_string();
        this.saved_connections = vec![saved];
        assert!(this.sessions.begin_connect(&connection).is_some());
        this.navigator_ui
            .pending_loads
            .insert(connection.clone(), Vec::new());

        this.ensure_connected_with_uri(
            connection.clone(),
            Some("not-a-connection-uri".into()),
            None,
            cx,
        );
        assert!(!this.navigator_ui.pending_loads.contains_key(&connection));
        assert_eq!(
            this.sessions.status(&connection),
            ConnectionStatus::Connecting
        );
    });
}

fn scenario_dialog_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        // The dialog slides in on a wall-clock transition; settle instantly.
        app.set_reduce_motion(true);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "new-connection",
        "--ui-fixture",
        "sample",
    ])
    .unwrap()
    .unwrap();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });
    (app, cx)
}

/// gpui-component's `Dialog` centers itself from the size measured on the
/// previous frame, so a freshly opened or re-stepped dialog needs a second
/// frame before its controls sit where they will stay. Clicking on the first
/// frame's bounds would miss.
fn dialog_settle(cx: &mut gpui::VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn dialog_click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    dialog_settle(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Default::default());
    dialog_settle(cx);
}

#[gpui::test]
fn dialog_test_button_reports_success_and_failure_inline(cx: &mut TestAppContext) {
    use crate::connection_dialog::TestStatus;
    let (app, cx) = scenario_dialog_app(cx);
    for (name, expected) in [
        ("orders", true),
        ("fixture-error", false),
        ("fixture-timeout", false),
    ] {
        let status = std::rc::Rc::new(std::cell::RefCell::new(TestStatus::Idle));
        let status_for_call = status.clone();
        let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
        form.name = name.into();
        form.host = "localhost".into();
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.start_connection_test(
                    form.clone(),
                    None,
                    window.window_handle(),
                    status_for_call.clone(),
                    cx,
                );
            });
        });
        assert_eq!(*status.borrow(), TestStatus::Running);
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        let current = status.borrow().clone();
        match (&current, expected) {
            (TestStatus::Success { .. }, true) => {}
            (TestStatus::Failure(message), false) => {
                assert!(message.contains("fixture"), "{message}");
            }
            (other, _) => panic!("unexpected status for {name}: {other:?}"),
        }
    }
    // Invalid form (bad URL) fails inline without running anything.
    let status = std::rc::Rc::new(std::cell::RefCell::new(TestStatus::Idle));
    let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
    form.use_uri = true;
    form.uri = "mysql://x".into();
    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.start_connection_test(form.clone(), None, window.window_handle(), status.clone(), cx);
        });
    });
    assert!(matches!(&*status.borrow(), TestStatus::Failure(_)));
}

#[gpui::test]
fn dialog_footer_shows_inline_status_and_save_persists_color(cx: &mut TestAppContext) {
    let (app, cx) = scenario_dialog_app(cx);
    cx.update(|window, cx| {
        app.update(cx, |this, cx| this.open_connection_dialog(None, window, cx));
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    dialog_click(cx, "connection-picker-next");

    // Footer: filter (disabled for a new connection), Test, Save; no Cancel.
    for selector in ["select-visible-databases", "test-connection", "connection-save"] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    let save = cx.debug_bounds("connection-save").unwrap();
    let test = cx.debug_bounds("test-connection").unwrap();
    let filter = cx.debug_bounds("select-visible-databases").unwrap();
    assert!(filter.right() <= test.left() && test.right() <= save.left(), "right-aligned order");
    assert!(save.size.width < px(120.0), "Save is compact, not stretched");

    dialog_click(cx, "connection-config-name");
    cx.simulate_input("fixture-error");
    dialog_click(cx, "connection-config-host");
    cx.simulate_input("db.local");
    // Test: inline result, no modal.
    dialog_click(cx, "test-connection");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("connection-test-status").is_some(), "status label next to the buttons");
    assert!(cx.debug_bounds("dialog-layer").is_some(), "dialog stays open");

    // Choose the red swatch, rename to a saveable name, Save.
    dialog_click(cx, "connection-color-4");
    dialog_click(cx, "connection-config-name");
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_input("Red one");
    dialog_click(cx, "connection-save");
    app.update(cx, |this, _| {
        let service = this.scenario_ui.service.as_ref().unwrap();
        let record = service.last_saved_record().expect("saved record");
        assert_eq!(record.name, "Red one");
        assert_eq!(record.color_hex, "#ef4444");
        assert!(record.uri.contains("db.local"), "{}", record.uri);
    });
    assert!(cx.debug_bounds("connection-config").is_none(), "Save closes the dialog");
}

/// Opens the scenario dialog on the config step for `kind_selector`'s entry.
fn open_config_step(
    app: &gpui::Entity<AppRoot>,
    cx: &mut gpui::VisualTestContext,
    picker_entry: Option<&'static str>,
) -> crate::connection_dialog::ConnectionDialogInputs {
    cx.update(|window, cx| {
        app.update(cx, |this, cx| this.open_connection_dialog(None, window, cx));
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    if let Some(entry) = picker_entry {
        dialog_click(cx, entry);
    }
    dialog_click(cx, "connection-picker-next");
    app.read_with(cx, |this, _| this.connection_ui.last_inputs.clone().expect("dialog inputs"))
}

#[gpui::test]
fn config_tabs_switch_and_sqlite_hides_network_tabs(cx: &mut TestAppContext) {
    use crate::connection_dialog::ConfigTab;
    let (app, cx) = scenario_dialog_app(cx);
    let inputs = open_config_step(&app, cx, None);
    assert_eq!(inputs.tab.get(), ConfigTab::Connection);
    for selector in [
        "connection-config-tab-connection",
        "connection-config-tab-tls",
        "connection-config-tab-tunnel",
        "connection-config-tab-advanced",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(cx.debug_bounds("connection-config-url").is_some());
    dialog_click(cx, "connection-config-tab-tls");
    assert_eq!(inputs.tab.get(), ConfigTab::Tls);
    assert!(cx.debug_bounds("connection-config-tls-notice").is_none(), "TLS is supported: no stub notice");
    assert!(cx.debug_bounds("connection-config-ssl-mode").is_some());
    assert!(cx.debug_bounds("connection-config-url").is_none(), "Connection fields hidden");
    dialog_click(cx, "connection-config-tab-tunnel");
    assert!(cx.debug_bounds("connection-config-tunnel-notice").is_some());
    dialog_click(cx, "connection-config-tab-advanced");
    assert!(cx.debug_bounds("connection-config-timeout").is_some());
    assert!(cx.debug_bounds("connection-config-options").is_some(), "pin lives in Advanced");
    let row = cx.debug_bounds("connection-config-options").unwrap();
    assert!(!inputs.is_pinned.get());
    cx.simulate_click(
        gpui::point(row.left() + px(110.0 + 12.0 + 8.0), row.center().y),
        Default::default(),
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(inputs.is_pinned.get(), "pin checkbox toggles");
    dialog_click(cx, "connection-config-tab-connection");
    assert!(cx.debug_bounds("connection-config-host").is_some());
    assert!(cx.debug_bounds("connection-config-options").is_none());
}

#[gpui::test]
fn sqlite_config_has_only_connection_and_advanced_tabs(cx: &mut TestAppContext) {
    let (app, cx) = scenario_dialog_app(cx);
    cx.update(|window, cx| {
        app.update(cx, |this, cx| this.open_connection_dialog(None, window, cx));
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    dialog_click(cx, "connection-picker-category-lightweight");
    dialog_click(cx, "connection-picker-sqlite");
    dialog_click(cx, "connection-picker-next");
    assert!(cx.debug_bounds("connection-config-tab-advanced").is_some());
    assert!(cx.debug_bounds("connection-config-tab-tls").is_none());
    assert!(cx.debug_bounds("connection-config-tab-tunnel").is_none());
    assert!(cx.debug_bounds("connection-config-file").is_some());
    assert!(cx.debug_bounds("connection-browse-file").is_some());
    assert!(cx.debug_bounds("connection-config-url").is_none());
}

#[gpui::test]
fn url_link_fills_fields_password_eye_and_save_checkbox_work(cx: &mut TestAppContext) {
    let (app, cx) = scenario_dialog_app(cx);
    let inputs = open_config_step(&app, cx, None);
    cx.update(|window, app| {
        inputs.uri.update(app, |state, cx| {
            state.set_value(
                "postgresql://alice:s3cret@db.example.com:6543/shop?sslmode=require&connect_timeout=9",
                window,
                cx,
            )
        });
    });
    dialog_click(cx, "apply-connection-url");
    cx.read(|app| {
        assert_eq!(inputs.host.read(app).value().as_ref(), "db.example.com");
        assert_eq!(inputs.port.read(app).value().as_ref(), "6543");
        assert_eq!(inputs.username.read(app).value().as_ref(), "alice");
        assert_eq!(inputs.password.read(app).value().as_ref(), "s3cret");
        assert_eq!(inputs.database.read(app).value().as_ref(), "shop");
        // `sslmode` moved to the TLS tab; the URL params keep only what is left.
        assert_eq!(inputs.uri_options.read(app).value().as_ref(), "");
        assert_eq!(inputs.tls.read(app).mode, crate::connection_tls::TlsMode::Require);
        assert_eq!(inputs.connect_timeout.read(app).value().as_ref(), "9");
    });
    assert!(cx.debug_bounds("connection-config-url-error").is_none());
    cx.update(|window, app| {
        inputs.uri.update(app, |state, cx| state.set_value("mysql://nope", window, cx))
    });
    dialog_click(cx, "apply-connection-url");
    assert!(cx.debug_bounds("connection-config-url-error").is_some(), "invalid URL shows inline");

    // Password eye toggles masking.
    assert!(inputs.password.read_with(cx, |state, _| state.presentation().is_masked()));
    let field = cx.debug_bounds("connection-config-password").unwrap();
    let eye = gpui::point(field.right() - px(16.0), field.center().y);
    cx.simulate_click(eye, Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(!inputs.password.read_with(cx, |state, _| state.presentation().is_masked()), "eye reveals");
    cx.simulate_click(eye, Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(inputs.password.read_with(cx, |state, _| state.presentation().is_masked()));

    // Save-password checkbox.
    assert!(inputs.save_password.get());
    let row = cx.debug_bounds("connection-config-save-password").unwrap();
    cx.simulate_click(
        gpui::point(row.left() + px(110.0 + 12.0 + 8.0), row.center().y),
        Default::default(),
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(!inputs.save_password.get(), "checkbox unchecks");
    assert!(cx.debug_bounds("connection-save-password-help").is_some(), "help icon");
}

#[gpui::test]
fn color_swatch_click_selects_and_none_clears(cx: &mut TestAppContext) {
    let (app, cx) = scenario_dialog_app(cx);
    let inputs = open_config_step(&app, cx, None);
    assert_eq!(inputs.color.borrow().as_deref(), Some(""), "new connection starts with none");
    dialog_click(cx, "connection-color-4");
    assert_eq!(inputs.color.borrow().as_deref(), Some("#ef4444"), "red");
    dialog_click(cx, "connection-color-5");
    assert_eq!(inputs.color.borrow().as_deref(), Some("#3b82f6"), "blue");
    dialog_click(cx, "connection-color-0");
    assert_eq!(inputs.color.borrow().as_deref(), Some(""), "none swatch clears");
    dialog_click(cx, "connection-custom-color");
    assert!(cx.debug_bounds("connection-custom-color").is_some());
}

#[gpui::test]
fn the_driver_store_is_in_the_app_folder_like_the_one_tablex_mcp_opens(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_isolated);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });
    let base = app.update(cx, |this, _| this.driver_ui.store.base_directory().to_path_buf());
    // `tablex-mcp` resolves it with the same `engine_connect::drivers::resolve_driver_data_dir`.
    assert_eq!(Some(base.clone()), crate::platform::app_data_dir());
    assert!(base.ends_with("tablex"), "{}", base.display());
}
