// Command palette (Cmd+K / Cmd+P): index sources, ranking through the real
// list, keyboard and pointer flows. Production theme + `Root` dialog layer.

fn palette_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    // The app-level Cmd+K handler targets the active window.
    visual.update(|window, _| window.activate_window());
    for label in ["Postgres fixture", "warehouse", "public", "Tables"] {
        palette_expand(&app, visual, label);
    }
    (app, visual)
}

fn palette_expand(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    label: &str,
) {
    let row = app.update(visual, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("fixture navigator row {label}"))
    });
    app.update(visual, |this, cx| this.handle_navigator_disclosure(&row, cx));
    visual.run_until_parked();
}

fn palette_settle(visual: &mut gpui::VisualTestContext) {
    for _ in 0..3 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
}

fn palette_open(visual: &mut gpui::VisualTestContext) {
    visual.simulate_keystrokes("secondary-k");
    palette_settle(visual);
}

fn palette_is_open(visual: &mut gpui::VisualTestContext) -> bool {
    visual.update(|window, cx| window.has_active_dialog(cx))
}

fn palette_snapshot(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> Vec<(&'static str, Vec<String>)> {
    app.read_with(visual, |this, cx| this.palette_snapshot_for_test(cx))
}

fn palette_type(visual: &mut gpui::VisualTestContext, text: &str) {
    visual.simulate_input(text);
    palette_settle(visual);
}

fn palette_titles(snapshot: &[(&'static str, Vec<String>)], section: &str) -> Vec<String> {
    snapshot
        .iter()
        .find(|(title, _)| *title == section)
        .map(|(_, rows)| rows.clone())
        .unwrap_or_default()
}

#[gpui::test]
fn palette_index_is_built_from_every_loaded_source_and_cached_by_revision(
    cx: &mut TestAppContext,
) {
    let (app, visual) = palette_app(cx);
    // Tabs, saved scripts and a saved pipeline.
    app.update(visual, |this, cx| {
        this.workspace.open_query_tab_with_metadata(
            "Orders report",
            TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                ..Default::default()
            },
        );
        let token = this.saved_sql_ui.begin(crate::ui_fixture::POSTGRES_ID, 0);
        this.saved_sql_ui.finish(
            crate::ui_fixture::POSTGRES_ID,
            token,
            0,
            Ok(vec![crate::saved_sql::SavedSqlEntry {
                path: "/tmp/monthly.sql".into(),
                title: "Monthly revenue".into(),
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                dialect: "sql".into(),
                updated_at: 0.0,
            }]),
        );
        this.saved_aggregates.insert(
            (crate::ui_fixture::MONGO_ID.into(), "app".into()),
            vec![engine_storage::aggregate::SavedAggregate {
                id: "agg-1".into(),
                conn_uuid: crate::ui_fixture::MONGO_ID.into(),
                database: "app".into(),
                collection: "users".into(),
                name: "Active users".into(),
                kind: "aggregation".into(),
                pipeline_json: "[]".into(),
                created_at: 0.0,
                updated_at: 0.0,
                options_json: None,
            }],
        );
        cx.notify();
    });
    let (found, first) = app.update(visual, |this, _| this.palette_index_for_test());
    for expected in [
        "Tab: Orders report",
        "Connection: Postgres fixture",
        "Action: New Query",
        "Database: warehouse",
        "Schema: public",
        "Table: orders",
        "Script: Monthly revenue",
        "Pipeline: Active users",
    ] {
        assert!(
            found.iter().any(|entry| entry == expected),
            "missing {expected}; index has {} entries",
            found.len()
        );
    }

    // Unchanged sources: the big catalog segment is reused as-is.
    let (_, second) = app.update(visual, |this, _| this.palette_index_for_test());
    assert_eq!(first, second);

    // A catalog change (Views group loaded) bumps the revision and rebuilds it.
    let views = app.update(visual, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .find(|row| row.label == "Views")
            .expect("views group")
    });
    app.update(visual, |this, cx| this.handle_navigator_disclosure(&views, cx));
    visual.run_until_parked();
    let (_, third) = app.update(visual, |this, _| this.palette_index_for_test());
    assert_ne!(first, third);
}

#[gpui::test]
fn palette_index_lists_mongo_collections_already_loaded_in_the_navigator(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_expand(&app, visual, "Mongo fixture");
    palette_expand(&app, visual, "fixture_db_0");
    let (found, _) = app.update(visual, |this, _| this.palette_index_for_test());
    assert!(found.iter().any(|entry| entry == "Database: fixture_db_0"));
    assert!(found
        .iter()
        .any(|entry| entry == "Collection: fixture_collection_00"));
}

#[gpui::test]
fn cmd_k_opens_typing_ranks_and_down_enter_opens_the_table_tab(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    assert!(!palette_is_open(visual));
    palette_open(visual);
    assert!(palette_is_open(visual), "Cmd+K opens the palette dialog");
    assert!(
        visual.debug_bounds("palette-row-0-0").is_some(),
        "idle list renders its first row"
    );
    let idle = palette_snapshot(&app, visual);
    assert!(!palette_titles(&idle, "Actions").is_empty());
    assert!(!palette_titles(&idle, "Connections").is_empty());

    palette_type(visual, "fixture_table_0");
    let ranked = palette_snapshot(&app, visual);
    let tables = palette_titles(&ranked, "Tables");
    // Nine prefix hits keep tree order; the looser subsequence hits follow.
    assert_eq!(
        tables[..9],
        (1..=9).map(|n| format!("fixture_table_0{n}")).collect::<Vec<_>>(),
        "same-tier ties keep tree order: {ranked:?}"
    );
    assert_eq!(ranked[0].0, "Tables", "best hits lead: {ranked:?}");
    // The first hit is highlighted; Down moves to the second; Enter opens it.
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    assert!(!palette_is_open(visual), "Enter closes the palette");
    let expected = tables[1].clone();
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.title, expected);
        assert_eq!(tab.query_mode, Some(QueryMode::SqlTable));
        assert_eq!(tab.database, "warehouse");
        assert_eq!(tab.schema.as_deref(), Some("public"));
    });
}

#[gpui::test]
fn escape_cmd_k_and_click_outside_close_the_palette_and_cmd_p_opens_it(cx: &mut TestAppContext) {
    let (_app, visual) = palette_app(cx);
    palette_open(visual);
    assert!(palette_is_open(visual));
    visual.simulate_keystrokes("escape");
    palette_settle(visual);
    assert!(!palette_is_open(visual), "Escape closes");

    visual.simulate_keystrokes("secondary-p");
    palette_settle(visual);
    assert!(palette_is_open(visual), "Cmd+P is an alias");
    visual.simulate_keystrokes("secondary-k");
    palette_settle(visual);
    assert!(!palette_is_open(visual), "Cmd+K toggles it closed");

    palette_open(visual);
    assert!(palette_is_open(visual));
    // Below the title bar, far from the centered panel: the backdrop.
    visual.simulate_click(
        gpui::point(gpui::px(8.0), gpui::px(800.0)),
        Default::default(),
    );
    palette_settle(visual);
    assert!(!palette_is_open(visual), "click outside closes");
}

#[gpui::test]
fn recent_section_lists_the_last_activated_items_newest_first(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_open(visual);
    assert!(
        palette_titles(&palette_snapshot(&app, visual), "Recent").is_empty(),
        "nothing recent yet"
    );
    palette_type(visual, "orders");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);

    palette_open(visual);
    assert_eq!(
        palette_titles(&palette_snapshot(&app, visual), "Recent"),
        vec!["orders".to_string()]
    );
    // An action goes through the same path and lands in front.
    palette_type(visual, "toggle sidebar");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| assert!(this.shell.sidebar_collapsed));

    palette_open(visual);
    assert_eq!(
        palette_titles(&palette_snapshot(&app, visual), "Recent"),
        vec!["Toggle Sidebar".to_string(), "orders".to_string()]
    );
    // Activating a recent row works from the idle list.
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| assert!(!this.shell.sidebar_collapsed));
}

#[gpui::test]
fn palette_actions_reuse_existing_operations(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    palette_open(visual);
    palette_type(visual, "new query");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), before + 1);
        assert_eq!(this.workspace.active_tab().unwrap().title, "Query");
    });

    // A palette-selected connection opens its connection tab.
    palette_open(visual);
    palette_type(visual, "postgres fixture");
    let snapshot = palette_snapshot(&app, visual);
    assert_eq!(snapshot[0].0, "Connections", "naming a connection ranks it first: {snapshot:?}");
    assert_eq!(palette_titles(&snapshot, "Connections")[0], "Postgres fixture");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.kind, crate::workspace::TabKind::Connection);
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
    });

    // And an open tab is activated from the tabs section.
    let orders_tab = app.update(visual, |this, _| {
        this.workspace.open_query_tab_with_metadata(
            "Zeta scratch",
            TabMetadata::default(),
        )
    });
    app.update(visual, |this, cx| {
        let first = this.workspace.tabs()[0].id;
        this.activate_tab_ui(first, cx);
    });
    palette_open(visual);
    palette_type(visual, "zeta");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().id, orders_tab)
    });
}

#[gpui::test]
fn palette_click_activates_a_row_and_empty_query_results_say_so(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_open(visual);
    palette_type(visual, "zzzzqqq");
    assert!(palette_snapshot(&app, visual).is_empty());
    assert!(visual.debug_bounds("palette-empty").is_some());

    // Replace the query: select-all + type.
    visual.simulate_keystrokes("secondary-a");
    palette_type(visual, "orders");
    let row = visual
        .debug_bounds("palette-row-0-0")
        .expect("first result row is rendered");
    visual.simulate_click(row.center(), Default::default());
    palette_settle(visual);
    assert!(!palette_is_open(visual));
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().title, "orders");
    });
}

#[gpui::test]
fn palette_opens_databases_and_collections_like_the_sidebar(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_expand(&app, visual, "Mongo fixture");
    palette_expand(&app, visual, "fixture_db_0");

    palette_open(visual);
    palette_type(visual, "fixture_collection_00");
    let snapshot = palette_snapshot(&app, visual);
    assert_eq!(snapshot[0].0, "Collections", "{snapshot:?}");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.collection, "fixture_collection_00");
        assert_eq!(tab.connection_id, crate::ui_fixture::MONGO_ID);
        assert_eq!(tab.query_mode, Some(QueryMode::MongoFind));
    });

    palette_open(visual);
    palette_type(visual, "warehouse");
    let snapshot = palette_snapshot(&app, visual);
    assert_eq!(snapshot[0], ("Databases", vec!["warehouse".to_string()]));
    // Enter reveals the database in the sidebar (no tab); Shift+Enter keeps
    // the scoped New Query.
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs, "Enter opens no tab");
        assert!(matches!(this.navigator.selected(),
            Some(NavigatorNodeId::Database { database, .. }) if database == "warehouse"));
    });

    palette_open(visual);
    palette_type(visual, "warehouse");
    visual.simulate_keystrokes("shift-enter");
    palette_settle(visual);
    assert!(!palette_is_open(visual));
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.title, "Query");
        assert_eq!(tab.database, "warehouse");
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
    });
}

/// Fixture tree with only the connection expanded, so databases are indexed
/// but collapsed, and the sidebar hidden.
fn palette_collapsed_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.update(|window, _| window.activate_window());
    palette_expand(&app, visual, "Postgres fixture");
    app.update(visual, |this, cx| {
        this.shell.sidebar_collapsed = true;
        cx.notify();
    });
    (app, visual)
}

fn palette_row_state(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    label: &str,
) -> Option<bool> {
    app.read_with(visual, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .find(|row| row.label == label)
            .map(|row| row.expanded)
    })
}

#[gpui::test]
fn palette_enter_on_a_database_reveals_expands_and_selects_it_in_the_sidebar(
    cx: &mut TestAppContext,
) {
    let (app, visual) = palette_collapsed_app(cx);
    assert_eq!(palette_row_state(&app, visual, "warehouse"), Some(false));
    assert_eq!(palette_row_state(&app, visual, "public"), None);
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());

    palette_open(visual);
    palette_type(visual, "warehouse");
    let snapshot = palette_snapshot(&app, visual);
    assert_eq!(snapshot[0], ("Databases", vec!["warehouse".to_string()]));
    visual.simulate_keystrokes("enter");
    palette_settle(visual);

    assert!(!palette_is_open(visual));
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs, "Enter opens no query tab");
        assert!(!this.shell.sidebar_collapsed, "the sidebar is shown");
        assert!(this.sidebar.locate_error.is_none(), "{:?}", this.sidebar.locate_error);
        assert!(matches!(this.navigator.selected(),
            Some(NavigatorNodeId::Database { database, .. }) if database == "warehouse"));
    });
    assert_eq!(palette_row_state(&app, visual, "warehouse"), Some(true));
    assert!(palette_row_state(&app, visual, "public").is_some(), "its children are loaded");
}

#[gpui::test]
fn palette_enter_on_a_schema_expands_its_ancestors_and_the_schema(cx: &mut TestAppContext) {
    let (app, visual) = palette_collapsed_app(cx);
    // Only the database level is loaded; the schema is not indexed yet.
    palette_expand(&app, visual, "warehouse");
    // Collapse the database again: the schema must be re-revealed through it.
    palette_expand(&app, visual, "warehouse");
    assert_eq!(palette_row_state(&app, visual, "warehouse"), Some(false));
    assert_eq!(palette_row_state(&app, visual, "public"), None);

    palette_open(visual);
    palette_type(visual, "public");
    let snapshot = palette_snapshot(&app, visual);
    assert_eq!(snapshot[0].0, "Schemas", "{snapshot:?}");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);

    app.read_with(visual, |this, _| {
        assert!(this.sidebar.locate_error.is_none(), "{:?}", this.sidebar.locate_error);
        assert!(matches!(this.navigator.selected(),
            Some(NavigatorNodeId::Schema { schema, .. }) if schema == "public"));
    });
    assert_eq!(palette_row_state(&app, visual, "warehouse"), Some(true));
    assert_eq!(palette_row_state(&app, visual, "public"), Some(true));
    assert!(palette_row_state(&app, visual, "Tables").is_some(), "schema children shown");
}

#[gpui::test]
fn palette_shift_enter_on_a_schema_opens_the_scoped_new_query(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_open(visual);
    palette_type(visual, "public");
    visual.simulate_keystrokes("shift-enter");
    palette_settle(visual);
    assert!(!palette_is_open(visual));
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.title, "Query");
        assert_eq!(tab.database, "warehouse");
        assert_eq!(tab.schema.as_deref(), Some("public"));
    });
}

#[gpui::test]
fn palette_shift_enter_on_a_table_still_opens_the_table_tab(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_open(visual);
    palette_type(visual, "orders");
    visual.simulate_keystrokes("shift-enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().title, "orders");
    });
}

#[gpui::test]
fn palette_height_fits_its_rows_up_to_the_cap_and_zero_hits_are_compact(cx: &mut TestAppContext) {
    let (_app, visual) = palette_app(cx);
    let height = |visual: &mut gpui::VisualTestContext| {
        f32::from(visual.debug_bounds("palette-list").expect("palette panel").size.height)
    };
    palette_open(visual);
    let idle = height(visual);
    assert_eq!(idle, 440.0, "the idle list fills the cap");

    palette_type(visual, "orders");
    let one = height(visual);
    // search strip (40) + one section header (26) + one 32px row.
    assert_eq!(one, 98.0);
    let row = visual.debug_bounds("palette-row-0-0").unwrap();
    let panel = visual.debug_bounds("palette-list").unwrap();
    assert!(row.bottom() <= panel.bottom(), "the row sits inside the panel");

    visual.simulate_keystrokes("secondary-a");
    palette_type(visual, "zzzzqqq");
    let none = height(visual);
    assert_eq!(none, 84.0, "40 search strip + 44 'No matching items' row");
    assert!(visual.debug_bounds("palette-empty").is_some());

    visual.simulate_keystrokes("secondary-a");
    palette_type(visual, "fixture_table");
    assert_eq!(height(visual), 440.0, "many hits are capped");
}

#[gpui::test]
fn palette_enter_on_a_mongo_database_reveals_it_without_opening_a_tab(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_expand(&app, visual, "Mongo fixture");
    palette_expand(&app, visual, "fixture_db_0");
    // Collapse it again so the reveal has to expand the database itself.
    palette_expand(&app, visual, "fixture_db_0");
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    palette_open(visual);
    palette_type(visual, "fixture_db_0");
    let snapshot = palette_snapshot(&app, visual);
    assert_eq!(snapshot[0].0, "Databases", "{snapshot:?}");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs);
        assert!(this.sidebar.locate_error.is_none(), "{:?}", this.sidebar.locate_error);
        let selected = this.navigator.selected().expect("a selected node");
        let provider = this.navigator.provider_node(selected).expect("provider node");
        assert_eq!(provider.kind, "database");
    });
    assert_eq!(palette_row_state(&app, visual, "fixture_db_0"), Some(true));
}

#[gpui::test]
fn palette_reaches_about_and_quit_where_no_native_menu_bar_exists(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_open(visual);
    palette_type(visual, "about");
    let found = palette_snapshot(&app, visual);
    assert!(palette_titles(&found, "Actions").contains(&"About TableX".to_string()));
    visual.update(|window, cx| window.close_dialog(cx));
    palette_settle(visual);
    palette_open(visual);
    palette_type(visual, "quit");
    let found = palette_snapshot(&app, visual);
    assert!(palette_titles(&found, "Actions").contains(&"Quit TableX".to_string()));
}

#[gpui::test]
fn about_menu_action_opens_the_about_dialog(cx: &mut TestAppContext) {
    let (_app, visual) = palette_app(cx);
    visual.update(|_, cx| menus::finish_launch(cx, |_| {}));
    assert!(!palette_is_open(visual));
    visual.dispatch_action(menus::About);
    palette_settle(visual);
    assert!(palette_is_open(visual), "About opens a dialog");
}

#[gpui::test]
fn driver_manager_menu_action_opens_the_driver_manager_tab(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    visual.update(|_, cx| menus::finish_launch(cx, |_| {}));
    let kind = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.active_tab().map(|tab| tab.kind))
    };
    assert_ne!(kind(visual), Some(TabKind::DriverManager));
    visual.dispatch_action(menus::DriverManager);
    palette_settle(visual);
    assert_eq!(kind(visual), Some(TabKind::DriverManager), "the menu item opens the tab");
}
