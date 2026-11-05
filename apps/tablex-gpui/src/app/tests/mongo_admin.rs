// Real-interaction proofs for the MongoDB sidebar menus and admin dialogs.
// Everything runs through the production context menu, dialogs and inputs
// against the scenario fixture, whose catalog answers the admin operations
// like the driver does (see `ui_fixture::MongoFixtureCatalog`).

const MONGO_COLLECTION_MENU: [&str; 10] = [
    "mongo-menu-copy-name",
    "mongo-menu-view-data",
    "mongo-menu-view-new-tab",
    "mongo-menu-new-query",
    "mongo-menu-new-aggregation",
    "mongo-menu-manage-indexes",
    "mongo-menu-rename",
    "mongo-menu-clone",
    "mongo-menu-refresh",
    "mongo-menu-drop-collection",
];

const MONGO_DATABASE_MENU: [&str; 5] = [
    "mongo-menu-copy-name",
    "mongo-menu-new-query",
    "mongo-menu-set-default-database",
    "mongo-menu-refresh",
    "mongo-menu-drop-database",
];

fn mongo_admin_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.dispatcher.allow_parking();
    // Dialog open animations run on wall-clock time; under a loaded test
    // machine they would move the controls between measuring and clicking.
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let id = crate::ui_fixture::MONGO_ID;
        let epoch = this.sessions.begin_connect(id).unwrap();
        this.sessions.finish_connect(id, epoch, Ok(()));
        cx.notify();
    });
    settle(visual);
    (app, visual)
}

fn sidebar_labels(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> Vec<String> {
    app.read_with(visual, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .map(|row| row.label)
            .collect()
    })
}

fn mongo_menu_ys(visual: &mut gpui::VisualTestContext, selectors: &[&'static str]) -> Vec<Option<f32>> {
    selectors
        .iter()
        .map(|selector| {
            visual
                .debug_bounds(selector)
                .map(|bounds| f32::from(bounds.origin.y))
        })
        .collect()
}

fn right_click_label(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    label: &str,
) {
    let index = connection_row_index(app, visual, label);
    right_click_row(visual, index);
}

/// Expands a visible sidebar row through the production disclosure path.
fn expand_sidebar_row(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, label: &str) {
    app.update(visual, |this, cx| {
        let row = this
            .navigator
            .visible_rows()
            .into_iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("{label} is visible"));
        this.handle_navigator_disclosure(&row, cx);
    });
    settle(visual);
}

fn mongo_fixture_operations(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> Vec<String> {
    app.read_with(visual, |this, _| {
        this.scenario_ui
            .service
            .as_ref()
            .unwrap()
            .fixture()
            .mongo_admin_operations()
            .to_vec()
    })
}

fn set_mongo_admin_features(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    features: &'static [&'static str],
) {
    app.update(visual, |this, _| {
        this.scenario_ui
            .service
            .as_mut()
            .unwrap()
            .fixture_mut()
            .set_mongo_admin_features(features);
    });
}

fn dialog_is_open(visual: &mut gpui::VisualTestContext) -> bool {
    visual.debug_bounds("mongo-name-input").is_some()
        || visual.debug_bounds("mongo-drop-confirm-input").is_some()
        || visual.debug_bounds("mongo-index-table").is_some()
}

#[gpui::test]
fn mongo_collection_menu_has_entries_in_order_with_a_separator_before_drop(
    cx: &mut TestAppContext,
) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_00");
    let ys = mongo_menu_ys(visual, &MONGO_COLLECTION_MENU);
    assert!(
        ys.iter().all(Option::is_some),
        "every collection entry is rendered: {ys:?}"
    );
    let ys: Vec<f32> = ys.into_iter().flatten().collect();
    assert!(
        ys.windows(2).all(|pair| pair[0] < pair[1]),
        "entries keep their order: {ys:?}"
    );
    let step = |index: usize| ys[index + 1] - ys[index];
    assert!(
        step(0) > step(1) && step(8) > step(7),
        "separators follow Copy Name and precede Drop Collection: {ys:?}"
    );
    for absent in [
        "mongo-menu-set-default-database",
        "mongo-menu-clear-default-database",
        "mongo-menu-drop-database",
    ] {
        assert!(
            visual.debug_bounds(absent).is_none(),
            "{absent} is a database entry"
        );
    }
}

#[gpui::test]
fn mongo_database_menu_has_its_entries_and_no_collection_actions(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_db_0");
    let ys = mongo_menu_ys(visual, &MONGO_DATABASE_MENU);
    assert!(
        ys.iter().all(Option::is_some),
        "every database entry is rendered: {ys:?}"
    );
    let ys: Vec<f32> = ys.into_iter().flatten().collect();
    assert!(ys.windows(2).all(|pair| pair[0] < pair[1]), "{ys:?}");
    assert!(
        (ys[4] - ys[3]) > (ys[3] - ys[2]),
        "a separator precedes Drop Database: {ys:?}"
    );
    for absent in [
        "mongo-menu-view-data",
        "mongo-menu-rename",
        "mongo-menu-clone",
        "mongo-menu-manage-indexes",
        "mongo-menu-drop-collection",
    ] {
        assert!(visual.debug_bounds(absent).is_none(), "{absent}");
    }
}

#[gpui::test]
fn mongo_menu_gates_admin_entries_on_driver_capabilities(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    // Only the index group is advertised: Rename / Clone / Drop Collection are
    // present but disabled, Manage Indexes still opens.
    set_mongo_admin_features(&app, visual, &["admin.indexes"]);
    for (selector, expects_dialog) in [
        ("mongo-menu-rename", false),
        ("mongo-menu-clone", false),
        ("mongo-menu-drop-collection", false),
        ("mongo-menu-manage-indexes", true),
    ] {
        right_click_label(&app, visual, "fixture_collection_00");
        assert!(
            visual.debug_bounds(selector).is_some(),
            "{selector} stays visible when unavailable"
        );
        click_debug_button(visual, selector);
        settle(visual);
        assert_eq!(dialog_is_open(visual), expects_dialog, "{selector}");
        if expects_dialog {
            click_debug_button(visual, "mongo-index-close");
        }
    }
    assert!(mongo_fixture_operations(&app, visual).is_empty());

    // A database menu without admin.database cannot open Drop Database.
    set_mongo_admin_features(&app, visual, &[]);
    right_click_label(&app, visual, "fixture_db_0");
    click_debug_button(visual, "mongo-menu-drop-database");
    settle(visual);
    assert!(!dialog_is_open(visual));

    // Disconnected connections advertise nothing at all.
    set_mongo_admin_features(
        &app,
        visual,
        &["admin.collection", "admin.indexes", "admin.database"],
    );
    app.update(visual, |this, _| {
        this.sessions.disconnect(crate::ui_fixture::MONGO_ID);
    });
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.mongo_admin_caps(crate::ui_fixture::MONGO_ID),
            crate::mongo_admin::MongoAdminCaps::default()
        );
    });
}

#[gpui::test]
fn mongo_menu_copy_name_view_data_new_tab_and_new_query_use_the_node(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-copy-name");
    let copied = visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some("fixture_collection_00"));

    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-view-data");
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-view-data");
    app.read_with(visual, |this, _| {
        let data_tabs: Vec<_> = this
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.query_mode.is_mongo_find())
            .collect();
        assert_eq!(data_tabs.len(), 1, "View Data reuses the open tab");
        assert_eq!(data_tabs[0].collection, "fixture_collection_00");
        assert_eq!(data_tabs[0].database, "fixture_db_0");
    });
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-view-new-tab");
    app.read_with(visual, |this, _| {
        let count = this
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.query_mode.is_mongo_find())
            .count();
        assert_eq!(count, 2, "View in New Tab always opens another tab");
    });

    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-query");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert_eq!(tab.database, "fixture_db_0");
        assert_eq!(tab.collection, "fixture_collection_00");
    });
}

#[gpui::test]
fn mongo_rename_dialog_validates_then_renames_sidebar_and_open_tab(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_01");
    click_debug_button(visual, "mongo-menu-view-data");
    right_click_label(&app, visual, "fixture_collection_01");
    click_debug_button(visual, "mongo-menu-rename");
    settle(visual);
    assert!(visual.debug_bounds("mongo-name-input").is_some(), "dialog opens");

    // Invalid and duplicate names are refused inline; nothing is dispatched.
    // No click first: the dialog must hand focus to the name field itself.
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("bad$name");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(visual.debug_bounds("mongo-dialog-error").is_some());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("fixture_collection_02");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(
        visual.debug_bounds("mongo-dialog-error").is_some(),
        "an existing target is refused by the server and shown in the dialog"
    );
    assert!(dialog_is_open(visual));
    assert!(mongo_fixture_operations(&app, visual).is_empty());

    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("aaa_renamed");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(!dialog_is_open(visual), "success closes the dialog");
    assert_eq!(
        mongo_fixture_operations(&app, visual),
        ["rename_collection"]
    );
    let labels = sidebar_labels(&app, visual);
    assert!(labels.contains(&"aaa_renamed".to_string()), "{labels:?}");
    assert!(!labels.contains(&"fixture_collection_01".to_string()));
    app.read_with(visual, |this, _| {
        let tab = this
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.query_mode.is_mongo_find())
            .expect("data tab survives the rename");
        assert_eq!(tab.collection, "aaa_renamed");
        assert_eq!(tab.title, "aaa_renamed");
        assert!(this
            .sidebar
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("renamed to \"aaa_renamed\"")));
        // Other databases keep their loaded state: only db_0 was reloaded.
        assert!(this.navigator.visible_rows().iter().any(|row| {
            row.label == "fixture_db_0" && row.expanded
        }));
    });
}

#[gpui::test]
fn mongo_clone_dialog_copies_into_a_new_collection_with_the_indexes_option(
    cx: &mut TestAppContext,
) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_03");
    click_debug_button(visual, "mongo-menu-clone");
    settle(visual);
    assert!(visual.debug_bounds("mongo-clone-include-indexes").is_some());
    // The default name is "<source>_copy"; use a name that sorts first.
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("aaa_clone");
    click_debug_button(visual, "mongo-clone-include-indexes");
    click_debug_button(visual, "mongo-name-submit");
    settle(visual);
    assert!(!dialog_is_open(visual));
    assert_eq!(mongo_fixture_operations(&app, visual), ["clone_collection"]);
    assert!(sidebar_labels(&app, visual).contains(&"aaa_clone".to_string()));
    app.read_with(visual, |this, _| {
        let service = this.scenario_ui.service.as_ref().unwrap();
        // The include-indexes box was unchecked: only _id_ was copied.
        let indexes = service
            .fixture()
            .list_mongo_indexes(crate::ui_fixture::MONGO_ID, "fixture_db_0", "aaa_clone")
            .unwrap();
        assert_eq!(indexes.len(), 1);
        assert!(indexes[0].is_id());
        assert!(this
            .sidebar
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("cloned to \"aaa_clone\"")));
    });
}

#[gpui::test]
fn mongo_drop_collection_requires_the_exact_typed_name_and_closes_its_data_tab(
    cx: &mut TestAppContext,
) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_02");
    click_debug_button(visual, "mongo-menu-view-data");
    right_click_label(&app, visual, "fixture_collection_02");
    click_debug_button(visual, "mongo-menu-new-query");
    right_click_label(&app, visual, "fixture_collection_02");
    click_debug_button(visual, "mongo-menu-drop-collection");
    settle(visual);
    assert!(visual.debug_bounds("mongo-drop-confirm-input").is_some());

    // Disabled until the exact name is typed (case and padding matter).
    click_debug_button(visual, "mongo-drop-submit");
    assert!(mongo_fixture_operations(&app, visual).is_empty());
    for wrong in ["fixture_collection_0", "FIXTURE_COLLECTION_02", "fixture_collection_02 "] {
        visual.simulate_keystrokes("secondary-a");
        visual.simulate_input(wrong);
        settle(visual);
        click_debug_button(visual, "mongo-drop-submit");
        visual.simulate_keystrokes("enter");
        settle(visual);
        assert!(
            mongo_fixture_operations(&app, visual).is_empty(),
            "{wrong:?} must not confirm the drop"
        );
        assert!(dialog_is_open(visual));
    }
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("fixture_collection_02");
    settle(visual);
    click_debug_button(visual, "mongo-drop-submit");
    settle(visual);
    assert!(!dialog_is_open(visual));
    assert_eq!(mongo_fixture_operations(&app, visual), ["drop_collection"]);
    assert!(!sidebar_labels(&app, visual).contains(&"fixture_collection_02".to_string()));
    app.read_with(visual, |this, _| {
        assert!(
            !this
                .workspace
                .tabs()
                .iter()
                .any(|tab| tab.query_mode.is_mongo_find()
                    && tab.collection == "fixture_collection_02"),
            "the dropped collection's data tab is closed"
        );
        assert!(
            this.workspace.tabs().iter().any(|tab| {
                tab.query_mode.is_mongo_script()
                    && tab.collection == "fixture_collection_02"
            }),
            "script tabs hold user text and are kept"
        );
        assert!(this
            .sidebar
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("dropped") && notice.contains("1 script tab")));
    });
}

#[gpui::test]
fn mongo_drop_database_requires_the_name_and_keeps_other_expanded_databases(
    cx: &mut TestAppContext,
) {
    let (app, visual) = mongo_admin_app(cx);
    // Expand a second database so its state can be checked after the drop.
    expand_sidebar_row(&app, visual, "fixture_db_1");
    let path = std::env::temp_dir().join(format!(
        "tablex-mongo-drop-db-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.set_mongo_default_database(crate::ui_fixture::MONGO_ID, Some("fixture_db_0"), cx)
            .unwrap();
    });
    right_click_label(&app, visual, "fixture_db_0");
    click_debug_button(visual, "mongo-menu-drop-database");
    settle(visual);
    click_debug_button(visual, "mongo-drop-submit");
    assert!(mongo_fixture_operations(&app, visual).is_empty());
    visual.simulate_input("fixture_db_0");
    settle(visual);
    click_debug_button(visual, "mongo-drop-submit");
    settle(visual);
    assert!(!dialog_is_open(visual));
    assert_eq!(mongo_fixture_operations(&app, visual), ["drop_database"]);
    let labels = sidebar_labels(&app, visual);
    assert!(!labels.contains(&"fixture_db_0".to_string()), "{labels:?}");
    app.read_with(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        let second = rows.iter().find(|row| row.label == "fixture_db_1").unwrap();
        assert!(second.expanded, "other databases keep their expansion");
        assert!(
            rows.iter().any(|row| row.label == "fixture_collection_00"),
            "the second database's collections stay loaded"
        );
        assert_eq!(
            this.mongo_default_database(crate::ui_fixture::MONGO_ID),
            None,
            "dropping the default database clears the preference"
        );
    });
    let _ = std::fs::remove_file(path);
}

#[gpui::test]
fn mongo_system_databases_cannot_be_dropped(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    let target = app.read_with(visual, |this, _| {
        let root = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        let id = this
            .navigator
            .find_provider_child(&root, "database", "fixture_db_0", None)
            .unwrap();
        let mut target = this.mongo_node_target(&id).unwrap();
        // The fixture has no "admin" row; retarget the resolved node.
        target.database = "admin".into();
        target
    });
    visual.update(|window, cx| {
        app.update(cx, |this, cx| this.open_mongo_drop_dialog(target, window, cx));
    });
    settle(visual);
    assert!(!dialog_is_open(visual), "no confirmation is offered for admin");
    assert!(mongo_fixture_operations(&app, visual).is_empty());
}

#[gpui::test]
fn mongo_set_default_database_persists_and_scopes_connection_queries(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-mongo-default-db-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, _| this.sidebar.path = Some(path.clone()));
    right_click_label(&app, visual, "fixture_db_0");
    click_debug_button(visual, "mongo-menu-set-default-database");
    let stored = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert_eq!(
        stored.default_database(crate::ui_fixture::MONGO_ID),
        Some("fixture_db_0")
    );
    app.update(visual, |this, cx| {
        let id = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        assert_eq!(this.workspace.active_tab().unwrap().database, "fixture_db_0");
    });
    // The entry flips to Clear, and clearing persists too.
    right_click_label(&app, visual, "fixture_db_0");
    assert!(visual.debug_bounds("mongo-menu-set-default-database").is_none());
    click_debug_button(visual, "mongo-menu-clear-default-database");
    let cleared = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert_eq!(cleared.default_database(crate::ui_fixture::MONGO_ID), None);
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn mongo_refresh_reloads_only_the_database_and_keeps_other_expansion(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    expand_sidebar_row(&app, visual, "fixture_db_1");
    app.update(visual, |this, cx| {
        // Server-side change the sidebar does not know about yet.
        this.scenario_ui
            .service
            .as_mut()
            .unwrap()
            .fixture_mut()
            .apply_mongo_admin(
                crate::ui_fixture::MONGO_ID,
                "fixture_db_0",
                &crate::mongo_admin::MongoAdminOp::RenameCollection {
                    from: "fixture_collection_01".into(),
                    to: "aaa_server_side".into(),
                },
            )
            .unwrap();
        cx.notify();
    });
    settle(visual);
    assert!(!sidebar_labels(&app, visual).contains(&"aaa_server_side".to_string()));
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-refresh");
    settle(visual);
    let labels = sidebar_labels(&app, visual);
    assert!(labels.contains(&"aaa_server_side".to_string()), "{labels:?}");
    app.read_with(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        assert!(rows.iter().any(|row| row.label == "fixture_db_1" && row.expanded));
        assert!(rows.iter().any(|row| row.label == "fixture_db_0" && row.expanded));
    });
}

#[gpui::test]
fn mongo_index_manager_lists_creates_and_drops_indexes(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-manage-indexes");
    settle(visual);
    assert!(visual.debug_bounds("mongo-index-table").is_some());
    for (row, expected) in [(0, "_id_"), (1, "email_1"), (2, "created_at_1")] {
        let selector: &'static str =
            Box::leak(format!("mongo-index-cell-{row}-0").into_boxed_str());
        assert!(
            visual.debug_bounds(selector).is_some(),
            "row {row} ({expected}) is listed"
        );
    }
    assert!(visual.debug_bounds("mongo-index-cell-3-0").is_none());

    // The _id_ index can be neither selected for dropping nor dropped.
    click_debug_button(visual, "mongo-index-cell-0-0");
    click_debug_button(visual, "mongo-index-drop");
    assert!(mongo_fixture_operations(&app, visual).is_empty());

    // Create: validation first, then a real compound-free index.
    click_debug_button(visual, "mongo-index-create");
    settle(visual);
    assert!(visual.debug_bounds("mongo-index-form").is_some());
    click_debug_button(visual, "mongo-index-form-submit");
    assert!(
        visual.debug_bounds("mongo-index-form-error").is_some(),
        "an index without a field is refused"
    );
    assert!(mongo_fixture_operations(&app, visual).is_empty());
    click_debug_button(visual, "mongo-index-field-0");
    visual.simulate_input("status");
    // Direction Select: open it, pick the second option (Descending).
    click_debug_button(visual, "mongo-index-type-0");
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    click_debug_button(visual, "mongo-index-unique");
    click_debug_button(visual, "mongo-index-form-submit");
    settle(visual);
    assert_eq!(mongo_fixture_operations(&app, visual), ["create_index"]);
    assert!(
        visual.debug_bounds("mongo-index-form").is_none(),
        "the form closes after a successful create"
    );
    assert!(
        visual.debug_bounds("mongo-index-cell-3-0").is_some(),
        "the list reloads with the new index"
    );
    app.read_with(visual, |this, _| {
        let indexes = this
            .scenario_ui
            .service
            .as_ref()
            .unwrap()
            .fixture()
            .list_mongo_indexes(crate::ui_fixture::MONGO_ID, "fixture_db_0", "fixture_collection_00")
            .unwrap();
        let created = indexes.last().unwrap();
        assert_eq!(created.name, "status_-1", "Descending was selected");
        assert!(created.unique);
    });

    // Drop it through the confirmation dialog.
    click_debug_button(visual, "mongo-index-cell-3-0");
    click_debug_button(visual, "mongo-index-drop");
    settle(visual);
    assert_eq!(
        mongo_fixture_operations(&app, visual),
        ["create_index"],
        "nothing is dropped before the confirmation"
    );
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(
        mongo_fixture_operations(&app, visual),
        ["create_index", "drop_index"]
    );
    assert!(visual.debug_bounds("mongo-index-cell-3-0").is_none());
}

#[gpui::test]
fn mongo_index_manager_ttl_input_is_validated_before_dispatch(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-manage-indexes");
    settle(visual);
    click_debug_button(visual, "mongo-index-create");
    settle(visual);
    click_debug_button(visual, "mongo-index-field-0");
    visual.simulate_input("expires");
    click_debug_button(visual, "mongo-index-ttl");
    visual.simulate_input("soon");
    click_debug_button(visual, "mongo-index-form-submit");
    assert!(visual.debug_bounds("mongo-index-form-error").is_some());
    assert!(mongo_fixture_operations(&app, visual).is_empty());
}

#[gpui::test]
fn mongo_admin_failure_stays_in_the_dialog_without_touching_the_sidebar(
    cx: &mut TestAppContext,
) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_04");
    click_debug_button(visual, "mongo-menu-drop-collection");
    // The collection disappears behind the dialog's back.
    app.update(visual, |this, _| {
        this.scenario_ui
            .service
            .as_mut()
            .unwrap()
            .fixture_mut()
            .apply_mongo_admin(
                crate::ui_fixture::MONGO_ID,
                "fixture_db_0",
                &crate::mongo_admin::MongoAdminOp::DropCollection {
                    collection: "fixture_collection_04".into(),
                },
            )
            .unwrap();
    });
    visual.simulate_input("fixture_collection_04");
    settle(visual);
    click_debug_button(visual, "mongo-drop-submit");
    settle(visual);
    assert!(dialog_is_open(visual), "the dialog stays open on failure");
    assert!(visual.debug_bounds("mongo-dialog-error").is_some());
    assert!(
        sidebar_labels(&app, visual).contains(&"fixture_collection_04".to_string()),
        "a failed drop leaves the sidebar alone"
    );
}

#[gpui::test]
fn mongo_sidebar_cmd_c_copies_the_selected_node_name(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    visual.update(|_, cx| super::actions::bind_keys(cx));
    for (label, expected) in [
        ("fixture_collection_00", "fixture_collection_00"),
        ("fixture_db_0", "fixture_db_0"),
    ] {
        let index = connection_row_index(&app, visual, label);
        let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
        let row = visual.debug_bounds(selector).expect("row bounds");
        // A single left click selects the row and focuses the sidebar.
        visual.simulate_click(row.center(), Default::default());
        settle(visual);
        visual.simulate_keystrokes("secondary-c");
        let copied = visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some(expected), "{label}");
    }
}
