// Sidebar connection tools: Reconnect, Disconnect all, Go to object, the remembered tree and the
// Sidebar settings (double-click, expand on connect, group by database type). Real interactions:
// the real menus by keyboard, the real shortcut, the real dialog and rows; the SQLite rigs run the
// real driver behind the real connector.

fn tools_save_settings(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    change: impl FnOnce(&mut crate::settings::Settings),
) {
    app.update(visual, |this, cx| {
        let mut next = (**this.settings).clone();
        change(&mut next);
        this.apply_settings(next, None, cx);
    });
    settle(visual);
}

fn tools_saved(id: uuid::Uuid, name: &str, kind: ConnectionKind, uri: &str) -> SavedConnection {
    SavedConnection {
        id,
        name: name.into(),
        uri: uri.into(),
        kind,
        save_password: false,
        is_pinned: false,
        disabled: false,
        ssh_tunnel: false,
    }
}

fn tools_mark_connected(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &str) {
    app.update(visual, |this, cx| {
        let epoch = this.sessions.begin_connect(id).unwrap();
        this.sessions.finish_connect(id, epoch, Ok(()));
        this.sync_sidebar_connections(cx);
    });
}

fn tools_status(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &str) -> ConnectionStatus {
    app.read_with(visual, |this, _| this.sessions.status(id))
}

fn tools_double_click(visual: &mut gpui::VisualTestContext, point: gpui::Point<gpui::Pixels>) {
    for count in [1usize, 2] {
        visual.simulate_event(gpui::MouseDownEvent {
            position: point,
            modifiers: Default::default(),
            button: gpui::MouseButton::Left,
            click_count: count,
            first_mouse: false,
        });
        visual.simulate_event(gpui::MouseUpEvent {
            position: point,
            modifiers: Default::default(),
            button: gpui::MouseButton::Left,
            click_count: count,
        });
        visual.run_until_parked();
    }
    settle(visual);
}

fn tools_wait_status(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &str,
    label: &'static str,
    expected: impl Fn(&ConnectionStatus) -> bool,
) {
    let (app, id) = (app.clone(), id.to_string());
    wait_until(visual, label, |visual| app.read_with(visual, |this, _| expected(&this.sessions.status(&id))));
    settle(visual);
}

// ---- Reconnect ------------------------------------------------------------------------------

#[gpui::test]
fn reconnect_from_the_menu_drops_the_session_connects_again_and_keeps_the_tab_and_the_tree(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = install_store(nav_rig_sqlite(cx, &db), ConnectionKind::Sqlite, &["sqlite"]);
    let key = rig.key.clone();
    rig.run("SELECT name FROM items ORDER BY id");
    rig.wait_not_running("the first run");
    assert_eq!(rig.rows(), 5);
    // The tree has the connection and its database open.
    rig.expand(&connection_node(&key));
    rig.expand(&database_node(&key, "main"));
    let old = rig.live_session().expect("a live session");

    // The real menu of the connected row: Pin, Close Connection, Reconnect.
    let index = connection_row_index(&rig.app, rig.visual, "driver-test");
    right_click_row(rig.visual, index);
    for _ in 0..3 {
        rig.visual.simulate_keystrokes("down");
    }
    rig.visual.simulate_keystrokes("enter");
    rig.visual.run_until_parked();

    let (app, id, previous) = (rig.app.clone(), key.clone(), old.clone());
    wait_until(rig.visual, "the new session", |visual| {
        app.read_with(visual, |this, _| {
            this.sessions.is_connected(&id)
                && this
                    .driver_runtime
                    .sessions
                    .get(&id)
                    .is_some_and(|session| !std::sync::Arc::ptr_eq(session, &previous))
        })
    });
    settle(rig.visual);

    // What was open comes back as the nodes load again.
    let (app, id) = (rig.app.clone(), key.clone());
    wait_until(rig.visual, "the tree back", |visual| {
        app.read_with(visual, |this, _| {
            this.navigator.is_expanded(&connection_node(&id))
                && this.navigator.is_expanded(&database_node(&id, "main"))
                && this.navigator.has_loaded_children(&database_node(&id, "main"))
        })
    });
    // The open tab runs on the new session.
    rig.run("SELECT name FROM items WHERE id <= 2 ORDER BY id");
    rig.wait_not_running("the run after the reconnect");
    assert_eq!(rig.rows(), 2);
}

#[gpui::test]
fn reconnect_is_offered_only_to_a_connected_connection_and_waits_for_a_transaction_answer(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let id = crate::ui_fixture::POSTGRES_ID;
    app.read_with(visual, |this, _| {
        assert!(!this.can_reconnect(id), "disconnected: Connect is what the menu shows");
        assert!(this.reconnect_target().is_none());
    });
    app.update(visual, |this, _| {
        let _ = this.sessions.begin_connect(id).unwrap();
    });
    app.read_with(visual, |this, _| assert!(!this.can_reconnect(id), "disabled while connecting"));
    app.update(visual, |this, _| {
        let epoch = this.sessions.begin_connect(id);
        assert!(epoch.is_none());
        this.sessions.disconnect(id);
    });
    tools_mark_connected(&app, visual, id);
    app.update(visual, |this, _| {
        this.navigator.select(NavigatorNodeId::Connection { connection: id.into() });
        assert!(this.can_reconnect(id));
        assert_eq!(this.reconnect_target().as_deref(), Some(id));
    });
    // The palette lists Reconnect and Disconnect All only now.
    let titles = app.update(visual, |this, _| this.palette_action_titles());
    assert!(titles.contains(&"Reconnect".to_string()), "{titles:?}");
    assert!(titles.contains(&"Disconnect All".to_string()), "{titles:?}");
}

// ---- Disconnect all -------------------------------------------------------------------------

#[gpui::test]
fn disconnect_all_in_a_group_touches_only_that_group_and_asks_about_open_transactions(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    let other_id = uuid::Uuid::new_v4();
    let other = other_id.to_string();
    let path = std::env::temp_dir().join(format!("tablex-tools-groups-{}.json", uuid::Uuid::new_v4()));
    let (real_group, other_group) = rig.app.update(rig.visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.saved_connections
            .push(tools_saved(other_id, "other", ConnectionKind::Postgres, "postgres://u@h/db"));
        this.navigator.set_connections(this.saved_connections.clone());
        let real = this.create_sidebar_group("Real", None, cx).unwrap();
        let elsewhere = this.create_sidebar_group("Elsewhere", None, cx).unwrap();
        this.move_sidebar_connection(&key, Some(&real), 0, cx).unwrap();
        this.move_sidebar_connection(&other, Some(&elsewhere), 0, cx).unwrap();
        (real, elsewhere)
    });
    tools_mark_connected(&rig.app, rig.visual, &other);
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.connected_in_scope(&navigator_tools::DisconnectScope::Group(real_group.clone())), std::slice::from_ref(&key));
        assert_eq!(this.connected_in_scope(&navigator_tools::DisconnectScope::Group(other_group.clone())), std::slice::from_ref(&other));
        assert_eq!(this.connected_in_scope(&navigator_tools::DisconnectScope::All).len(), 2);
    });

    // The real group menu of "Elsewhere" (nothing of it has a transaction): New Subgroup,
    // Select group connections, Rename, Move to Group, Disconnect All in Group.
    let index = connection_row_index(&rig.app, rig.visual, "Elsewhere");
    right_click_row(rig.visual, index);
    for _ in 0..5 {
        rig.visual.simulate_keystrokes("down");
    }
    rig.visual.simulate_keystrokes("enter");
    rig.visual.run_until_parked();
    settle(rig.visual);
    assert_eq!(tools_status(&rig.app, rig.visual, &other), ConnectionStatus::Disconnected);
    assert!(matches!(tools_status(&rig.app, rig.visual, &key), ConnectionStatus::Connected), "the other group is untouched");

    // The connection of "Real" has an open transaction: the same question as for one connection.
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('group')");
    tools_mark_connected(&rig.app, rig.visual, &other);
    let asked = rig.app.update_in(rig.visual, |this, window, cx| {
        this.request_disconnect_all(navigator_tools::DisconnectScope::Group(real_group.clone()), window, cx)
    });
    assert_eq!(asked, 1);
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "asked, nothing dropped silently");
    rig.click("tx-guard-cancel");
    assert!(matches!(tools_status(&rig.app, rig.visual, &key), ConnectionStatus::Connected));
    assert_eq!(rig.pending(a), 1, "Cancel keeps the transaction");
    rig.app.update_in(rig.visual, |this, window, cx| {
        this.request_disconnect_all(navigator_tools::DisconnectScope::Group(real_group.clone()), window, cx)
    });
    settle(rig.visual);
    rig.click("tx-guard-rollback");
    tools_wait_status(&rig.app, rig.visual, &key, "the group's disconnect", |status| {
        matches!(status, ConnectionStatus::Disconnected)
    });
    assert!(matches!(tools_status(&rig.app, rig.visual, &other), ConnectionStatus::Connected), "the other group stays");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'group'"), "0", "rolled back, not committed");

    // Disconnect All from the sidebar's options menu: the last item.
    click_debug_button(rig.visual, "sidebar-settings");
    settle(rig.visual);
    rig.visual.simulate_keystrokes("up");
    rig.visual.simulate_keystrokes("enter");
    rig.visual.run_until_parked();
    settle(rig.visual);
    assert_eq!(tools_status(&rig.app, rig.visual, &other), ConnectionStatus::Disconnected);
    let _ = std::fs::remove_file(path);
}

#[gpui::test]
fn disconnect_all_is_a_palette_action_that_goes_through_the_per_connection_path(cx: &mut TestAppContext) {
    let (app, visual) = nav_click_app(cx);
    visual.update(|window, _| window.activate_window());
    for id in [crate::ui_fixture::POSTGRES_ID, crate::ui_fixture::SQLITE_ID] {
        tools_mark_connected(&app, visual, id);
    }
    palette_open(visual);
    palette_type(visual, "disconnect all");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    for id in [crate::ui_fixture::POSTGRES_ID, crate::ui_fixture::SQLITE_ID] {
        assert_eq!(tools_status(&app, visual, id), ConnectionStatus::Disconnected);
    }
}

// ---- Go to object ---------------------------------------------------------------------------

fn goto_open(visual: &mut gpui::VisualTestContext) {
    visual.simulate_keystrokes(&primary("shift-d"));
    settle(visual);
}

fn goto_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<(&'static str, String, String)> {
    app.read_with(visual, |this, cx| this.goto_rows_for_test(cx))
}

#[gpui::test]
fn go_to_object_has_its_shortcut_and_finds_loaded_and_unloaded_objects_then_reveals_and_opens(
    cx: &mut TestAppContext,
) {
    let (app, visual) = nav_click_app(cx);
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        window.activate_window();
    });
    app.update(visual, |this, _| this.seed_connection_tree_scenario());
    connect_postgres_fixture(&app, visual);
    settle(visual);

    // The chord is registered under its id, for the platform's primary key.
    let label = shortcuts::label(&Default::default(), "navigation.go_to_object").expect("a chord");
    assert!(label.contains('D'), "{label}");
    assert!(!palette_is_open(visual));
    goto_open(visual);
    assert!(palette_is_open(visual), "the shortcut opens the dialog");
    assert!(visual.debug_bounds("goto-hint").is_some(), "idle: says what to type");

    // The sidebar's tree is what it was; the dialog and its search never alter it.
    let tree_before = app.read_with(visual, |this, _| this.navigator.visible_rows());
    // A loaded table, found at once with its path.
    visual.simulate_input("orders");
    settle(visual);
    let rows = goto_rows(&app, visual);
    let orders = rows.iter().find(|(badge, title, _)| *badge == "Table" && title == "orders").expect("orders");
    assert!(orders.2.contains("Postgres fixture") && orders.2.contains("warehouse.public"), "{orders:?}");

    // Escape closes it and leaves the search state clean.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!palette_is_open(visual));
    app.read_with(visual, |this, _| {
        assert!(this.goto_filter_for_test().is_none());
        assert_eq!(this.navigator.visible_rows(), tree_before);
    });

    // A view nothing loaded yet: found by the remote search after the debounce.
    goto_open(visual);
    visual.simulate_input("daily_sum");
    settle_search(visual);
    let rows = goto_rows(&app, visual);
    let view = rows
        .iter()
        .find(|(badge, title, _)| *badge == "View" && title == "daily_summary")
        .unwrap_or_else(|| panic!("the remote search found the view: {rows:?}"));
    assert!(view.2.contains("Postgres fixture"), "{view:?}");
    app.read_with(visual, |this, _| {
        assert_eq!(this.goto_filter_for_test().as_deref(), Some("daily_sum"), "the dialog's own search ran");
        assert!(this.sidebar.remote.filter.is_none(), "not the sidebar's");
        assert!(!this.navigator.has_filtered_groups(), "no group of the sidebar was narrowed");
        assert_eq!(this.navigator.visible_rows(), tree_before, "the sidebar shows what it showed");
        assert_eq!(this.navigator.loading_count(), 0);
    });

    // Enter reveals it in the tree and opens it; the search is undone.
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    visual.simulate_keystrokes("enter");
    settle_search(visual);
    settle(visual);
    assert!(!palette_is_open(visual));
    app.read_with(visual, |this, _| {
        assert!(matches!(this.navigator.selected(), Some(NavigatorNodeId::Object { object, .. }) if object == "daily_summary"));
        assert_eq!(this.workspace.tabs().len(), tabs + 1, "a data tab opened");
        let tab = this.workspace.active_tab().expect("the opened tab");
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
        assert!(tab.title.contains("daily_summary"), "{}", tab.title);
        assert!(this.goto_filter_for_test().is_none(), "the search ended with the dialog");
        assert!(!this.navigator.has_filtered_groups());
    });
    let labels = visible_labels(&app, visual);
    assert!(labels.contains(&"daily_summary".to_string()), "revealed in the tree: {labels:?}");
}

#[gpui::test]
fn go_to_object_finds_a_mongodb_collection_and_opens_its_tab(cx: &mut TestAppContext) {
    let (app, visual) = nav_click_app(cx);
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        window.activate_window();
    });
    connect_mongo_fixture(&app, visual);
    let tree_before = app.read_with(visual, |this, _| this.navigator.visible_rows());
    // Only a name matches: the connection is called "Mongo fixture", its collections are not hits.
    goto_open(visual);
    visual.simulate_input("mongo");
    settle_search(visual);
    assert!(goto_rows(&app, visual).is_empty(), "{:?}", goto_rows(&app, visual));
    visual.simulate_keystrokes("escape");
    settle(visual);
    goto_open(visual);
    visual.simulate_input("collection_17");
    settle_search(visual);
    app.read_with(visual, |this, _| assert_eq!(this.navigator.visible_rows(), tree_before));
    let rows = goto_rows(&app, visual);
    assert!(
        rows.iter().any(|(badge, title, path)| *badge == "Collection"
            && title == "fixture_collection_17"
            && path.contains("Mongo fixture")),
        "{rows:?}"
    );
    visual.simulate_keystrokes("enter");
    settle_search(visual);
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("a collection tab");
        assert_eq!(tab.collection, "fixture_collection_17");
        assert_eq!(tab.connection_id, crate::ui_fixture::MONGO_ID);
    });
}

// ---- The remembered tree --------------------------------------------------------------------

fn tools_session_path() -> std::path::PathBuf {
    tb_session_dir().join("tablex.db")
}

#[gpui::test]
fn the_open_nodes_round_trip_through_the_session_file_and_come_back_when_the_connection_connects(
    cx: &mut TestAppContext,
) {
    let (app, visual) = nav_click_app(cx);
    let path = tools_session_path();
    app.update(visual, |this, _| {
        this.shell.session.path = Some(path.clone());
        this.seed_connection_tree_scenario();
    });
    connect_postgres_fixture(&app, visual);
    settle(visual);
    let postgres = crate::ui_fixture::POSTGRES_ID;
    assert!(visible_labels(&app, visual).contains(&"orders".to_string()));
    app.update(visual, |this, _| this.flush_session_blocking());
    let saved = tb_read_session(&path);
    let entry = saved.tree.iter().find(|tree| tree.connection_id == postgres).expect("a tree entry");
    assert!(entry.nodes.len() >= 4 && entry.nodes.len() <= 200, "{}", entry.nodes.len());
    assert!(saved.tree.iter().all(|tree| app.read_with(visual, |this, _| {
        this.saved_connections.iter().any(|saved| saved.matches_id(&tree.connection_id))
    })));

    // A relaunch: the tree is closed, the session file is read, nothing connects by itself.
    app.update(visual, |this, cx| {
        this.disconnect_connection(postgres, cx);
        this.navigator.set_remembered_tree(Vec::new());
        this.collapse_navigator(cx);
        this.restore_workspace_session(true, |_, _| {}, cx);
    });
    settle(visual);
    assert!(!visible_labels(&app, visual).contains(&"orders".to_string()), "closed after the relaunch");
    app.read_with(visual, |this, _| {
        assert!(this.navigator.has_remembered_tree(postgres), "kept for the connection");
        assert!(matches!(this.sessions.status(postgres), ConnectionStatus::Disconnected), "never connected for it");
    });

    // The connection connects (the user's click): the nodes open as they load.
    app.update(visual, |this, cx| this.ensure_connected(postgres.into(), None, cx));
    tools_wait_status(&app, visual, postgres, "the connect", |status| matches!(status, ConnectionStatus::Connected));
    settle(visual);
    assert!(visible_labels(&app, visual).contains(&"orders".to_string()), "the tree is back");
    app.read_with(visual, |this, _| assert!(!this.navigator.has_remembered_tree(postgres)));
}

#[gpui::test]
fn the_remembered_tree_ignores_missing_connections_follows_its_setting_and_never_connects(
    cx: &mut TestAppContext,
) {
    let (app, visual) = nav_click_app(cx);
    let path = tools_session_path();
    let postgres = crate::ui_fixture::POSTGRES_ID;
    app.update(visual, |this, _| {
        this.shell.session.path = Some(path.clone());
        this.seed_connection_tree_scenario();
    });
    connect_postgres_fixture(&app, visual);
    // The setting off: nothing is saved of the tree.
    tools_save_settings(&app, visual, |settings| settings.navigation.restore_tree_state = false);
    app.update(visual, |this, _| this.flush_session_blocking());
    assert!(tb_read_session(&path).tree.is_empty());
    tools_save_settings(&app, visual, |settings| settings.navigation.restore_tree_state = true);
    app.update(visual, |this, _| this.flush_session_blocking());
    assert!(!tb_read_session(&path).tree.is_empty());

    // A file that names a connection that is gone, and a node this version cannot read.
    let mut session = tb_read_session(&path);
    session.tree.push(crate::gpui_session::SessionTree {
        connection_id: "00000000-0000-4000-8000-0000000000ff".into(),
        nodes: vec![serde_json::json!({"Connection": {"connection": "00000000-0000-4000-8000-0000000000ff"}})],
    });
    session.tree.iter_mut().find(|tree| tree.connection_id == postgres).unwrap().nodes.push(serde_json::json!({"FromTheFuture": 1}));
    crate::gpui_session::save(&path, &crate::gpui_session::encode(&session).unwrap()).unwrap();
    app.update(visual, |this, cx| {
        this.disconnect_connection(postgres, cx);
        this.navigator.set_remembered_tree(Vec::new());
        this.restore_workspace_session(true, |_, _| {}, cx);
        assert!(!this.navigator.has_remembered_tree("00000000-0000-4000-8000-0000000000ff"));
        assert!(this.navigator.has_remembered_tree(postgres), "the readable nodes are kept");
    });
    // Setting off at the relaunch: the file is read, the tree is not remembered.
    tools_save_settings(&app, visual, |settings| settings.navigation.restore_tree_state = false);
    app.update(visual, |this, cx| {
        this.navigator.set_remembered_tree(Vec::new());
        this.restore_workspace_session(true, |_, _| {}, cx);
        assert!(!this.navigator.has_remembered_tree(postgres));
        assert!(matches!(this.sessions.status(postgres), ConnectionStatus::Disconnected));
    });
}

// ---- Sidebar settings -----------------------------------------------------------------------

#[gpui::test]
fn the_sidebar_settings_are_registered_with_their_defaults(cx: &mut TestAppContext) {
    let _ = cx;
    let defaults = crate::settings::Settings::default();
    assert_eq!(defaults.navigation.connection_double_click, crate::settings::ConnectionDoubleClick::Expand);
    assert!(!defaults.navigation.expand_on_connect);
    assert!(!defaults.navigation.group_by_driver);
    assert!(defaults.navigation.restore_tree_state);
    for id in [
        "navigation.connection_double_click",
        "navigation.expand_on_connect",
        "navigation.group_by_driver",
        "navigation.restore_tree_state",
    ] {
        let descriptor = crate::settings::registry::find(id).unwrap_or_else(|| panic!("{id}"));
        assert_eq!(descriptor.group, "Sidebar", "{id}");
    }
}

#[gpui::test]
fn double_clicking_a_connection_follows_the_setting(cx: &mut TestAppContext) {
    use crate::settings::ConnectionDoubleClick;
    let (app, visual) = nav_click_app(cx);
    let postgres = crate::ui_fixture::POSTGRES_ID;

    // Connect or disconnect: one click only selects, two connect, two more disconnect.
    tools_save_settings(&app, visual, |settings| {
        settings.navigation.connection_double_click = ConnectionDoubleClick::Connect;
    });
    let (index, _) = nav_row_state(&app, visual, "Postgres fixture");
    let point = nav_label_point(visual, index);
    visual.simulate_click(point, Default::default());
    settle(visual);
    assert!(!nav_row_state(&app, visual, "Postgres fixture").1, "a single click does not expand");
    assert_eq!(tools_status(&app, visual, postgres), ConnectionStatus::Disconnected);
    tools_double_click(visual, point);
    tools_wait_status(&app, visual, postgres, "the double-click connect", |status| {
        matches!(status, ConnectionStatus::Connected)
    });
    tools_double_click(visual, point);
    tools_wait_status(&app, visual, postgres, "the double-click disconnect", |status| {
        matches!(status, ConnectionStatus::Disconnected)
    });

    // Open a new SQL editor: a tab of the connection.
    tools_save_settings(&app, visual, |settings| {
        settings.navigation.connection_double_click = ConnectionDoubleClick::NewQuery;
    });
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let (index, _) = nav_row_state(&app, visual, "Postgres fixture");
    let point = nav_label_point(visual, index);
    tools_double_click(visual, point);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs + 1);
        assert_eq!(this.workspace.active_tab().unwrap().connection_id, postgres);
        assert!(!this.navigator.is_expanded(&connection_node(postgres)));
    });

    // The default keeps the old behaviour: the click expands (checked by
    // `double_click_on_a_connection_toggles_once_net`).
    tools_save_settings(&app, visual, |settings| {
        settings.navigation.connection_double_click = ConnectionDoubleClick::Expand;
    });
    let (index, _) = nav_row_state(&app, visual, "Postgres fixture");
    let point = nav_label_point(visual, index);
    visual.simulate_click(point, Default::default());
    settle(visual);
    assert!(nav_row_state(&app, visual, "Postgres fixture").1);
}

#[gpui::test]
fn expand_on_connect_opens_the_first_level_of_a_connection_the_user_connected(cx: &mut TestAppContext) {
    let (app, visual) = nav_click_app(cx);
    let (postgres, sqlite) = (crate::ui_fixture::POSTGRES_ID, crate::ui_fixture::SQLITE_ID);
    // Off (the default): connecting by hand leaves the row closed.
    app.update(visual, |this, cx| this.open_connection_target(postgres, cx));
    tools_wait_status(&app, visual, postgres, "the connect", |status| matches!(status, ConnectionStatus::Connected));
    settle(visual);
    assert!(!nav_row_state(&app, visual, "Postgres fixture").1, "closed");
    assert!(!visible_labels(&app, visual).contains(&"warehouse".to_string()));

    // On: it opens its databases once up.
    tools_save_settings(&app, visual, |settings| settings.navigation.expand_on_connect = true);
    app.update(visual, |this, cx| this.open_connection_target(sqlite, cx));
    tools_wait_status(&app, visual, sqlite, "the connect", |status| matches!(status, ConnectionStatus::Connected));
    settle(visual);
    let (index, expanded) = nav_row_state(&app, visual, "SQLite fixture");
    assert!(expanded, "opened on connect");
    assert!(visible_labels(&app, visual).len() > index + 1, "its first level is listed");

    // A connection nobody asked for (a restored tab, a load) never opens by itself.
    app.update(visual, |this, cx| this.ensure_connected(crate::ui_fixture::MONGO_ID.into(), None, cx));
    tools_wait_status(&app, visual, crate::ui_fixture::MONGO_ID, "the connect", |status| {
        matches!(status, ConnectionStatus::Connected)
    });
    settle(visual);
    assert!(!app.read_with(visual, |this, _| this.navigator.is_expanded(&connection_node(crate::ui_fixture::MONGO_ID))));
}

#[gpui::test]
fn group_by_driver_renders_a_section_per_type_with_the_user_groups_inside(cx: &mut TestAppContext) {
    let (app, visual) = nav_click_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-tools-driver-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Team", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, Some(&group), 0, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::SQLITE_ID, Some(&group), 1, cx).unwrap();
    });
    let flat = app.read_with(visual, |this, _| this.navigator.visible_rows().len());
    tools_save_settings(&app, visual, |settings| settings.navigation.group_by_driver = true);
    let rows = app.read_with(visual, |this, _| this.navigator.visible_rows());
    let sections: Vec<&str> = rows
        .iter()
        .filter(|row| matches!(row.id, NavigatorNodeId::Driver { .. }))
        .map(|row| row.label.as_str())
        .collect();
    assert!(sections.starts_with(&["PostgreSQL", "SQLite"]), "{sections:?}");
    assert!(sections.contains(&"MongoDB"), "{sections:?}");
    let position = |label: &str| rows.iter().position(|row| row.label == label).unwrap_or_else(|| panic!("{label}"));
    assert_eq!(rows[position("PostgreSQL")].depth, 0);
    // "Team" shows inside the sections of the types it holds, with its connection below it.
    assert_eq!(rows[position("PostgreSQL") + 1].label, "Team");
    assert_eq!(rows[position("PostgreSQL") + 2].label, "Postgres fixture");
    assert_eq!(rows[position("PostgreSQL") + 2].depth, 2);
    assert_eq!(rows.iter().filter(|row| row.label == "Team").count(), 2);

    // A click on a section collapses it, as on a group.
    let (index, expanded) = nav_row_state(&app, visual, "PostgreSQL");
    assert!(expanded);
    let point = nav_label_point(visual, index);
    visual.simulate_click(point, Default::default());
    settle(visual);
    assert!(!nav_row_state(&app, visual, "PostgreSQL").1);
    assert!(!visible_labels(&app, visual).contains(&"Postgres fixture".to_string()));

    tools_save_settings(&app, visual, |settings| settings.navigation.group_by_driver = false);
    assert_eq!(app.read_with(visual, |this, _| this.navigator.visible_rows().len()), flat, "the saved layout again");
    let _ = std::fs::remove_file(path);
}

#[gpui::test]
fn the_palette_lists_go_to_object_with_its_shortcut_like_the_other_actions(cx: &mut TestAppContext) {
    let _ = cx;
    let detail = command_palette::StaticAction::GoToObject.detail(&Default::default()).to_string();
    let label = shortcuts::label(&Default::default(), "navigation.go_to_object").expect("a chord");
    assert!(detail.ends_with(&label), "{detail}");
    // Short enough for the row to show the whole hint next to the longest hinted action.
    let longest = command_palette::StaticAction::NextTabGroup.detail(&Default::default()).to_string();
    assert!(detail.chars().count() <= longest.chars().count(), "{detail} / {longest}");
}
