fn sidebar_test_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionTree,
        fixture: FixtureKind::Sample,
        delay: Duration::ZERO,
        timeout_delay: Duration::ZERO,
    };
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    (app, visual)
}

#[gpui::test]
fn visible_filter_controller_persists_and_rejects_stale_dialog_without_losing_colors(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-visible-filter-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let id = crate::ui_fixture::POSTGRES_ID;
        this.set_sidebar_connection_color(id, Some("#123ABC"), cx)
            .unwrap();
        let epoch = this.sessions.begin_connect(id).unwrap();
        this.sessions.finish_connect(id, epoch, Ok(()));
        let (token, _) = this.filter_enumeration.begin(id, epoch);
        this.filter_enumeration
            .finish(id, token, epoch, Ok(vec!["app".into()]));
        this.apply_visible_filter(
            id,
            crate::sidebar_preferences::VisibleObjectFilter {
                databases: Some(vec!["app".into()]),
                ..Default::default()
            },
            cx,
        )
        .unwrap();
        this.sessions.disconnect(id);
        assert!(this
            .apply_visible_filter(id, Default::default(), cx)
            .is_err());
    });
    let restored = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert_eq!(
        restored.visible_filters[crate::ui_fixture::POSTGRES_ID].databases,
        Some(vec!["app".into()])
    );
    assert_eq!(
        restored
            .connections
            .iter()
            .find(|display| display.id == crate::ui_fixture::POSTGRES_ID)
            .unwrap()
            .color
            .as_deref(),
        Some("#123ABC")
    );
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_controller_persists_groups_reloads_and_preserves_file_on_save_error(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-controller-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let id = this
            .create_sidebar_group("Work", None, cx)
            .expect("group created");
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, Some(&id), 0, cx)
            .unwrap();
        assert_eq!(this.navigator.visible_rows()[0].label, "Work");
    });
    let persisted = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert_eq!(persisted.groups[0].name, "Work");
    app.update(visual, |this, cx| {
        this.navigator.set_preferences(Default::default()).unwrap();
        this.reload_sidebar_preferences(cx).unwrap();
        assert_eq!(this.navigator.visible_rows()[0].label, "Work");
        let previous = this.navigator.preferences().clone();
        this.sidebar.path = Some(path.join("cannot-write"));
        assert!(this
            .rename_sidebar_group(&previous.groups[0].id, "Modified", cx)
            .is_err());
        assert_eq!(this.navigator.preferences(), &previous);
        assert!(this.sidebar.error.is_some());
    });
    assert_eq!(
        crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap(),
        persisted
    );
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn successful_placement_cleanup_does_not_erase_credential_cleanup_warning(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-credential-warning-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let store = crate::credentials::fake::MemoryStore {
            failing: Some(crate::credentials::SecretKind::Database),
            ..Default::default()
        };
        let error = crate::credentials::CredentialStore::delete_all(&store, "unused")
            .expect_err("actual credential deletion error");
        this.sidebar.notice = Some(format!(
            "Connection deleted, but could not clear its {} secrets: {error}",
            crate::platform::CREDENTIAL_STORE_LABEL
        ));
        this.change_sidebar_preferences(
            |prefs| {
                prefs.remove_connection(crate::ui_fixture::POSTGRES_ID);
                Ok(())
            },
            cx,
        )
        .unwrap();
        assert!(this
            .sidebar
            .notice
            .as_deref()
            .is_some_and(|text| text.contains("secrets: ")));
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_controller_rejects_group_cycle_without_changing_persisted_file(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-group-cycle-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let parent = this.create_sidebar_group("Parent", None, cx).unwrap();
        let child = this
            .create_sidebar_group("Child", Some(&parent), cx)
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(this
            .move_sidebar_group(&parent, Some(&child), 0, cx)
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(this.navigator.preferences().groups[0].parent, None);
        assert!(this
            .sidebar
            .error
            .as_deref()
            .is_some_and(|error| error.contains("cycle")));
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_color_override_survives_reload_and_invalid_color_keeps_old_value(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-color-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let id = crate::ui_fixture::POSTGRES_ID;
        this.set_sidebar_connection_color(id, Some("#123ABC"), cx)
            .unwrap();
        this.navigator.set_preferences(Default::default()).unwrap();
        this.reload_sidebar_preferences(cx).unwrap();
        assert_eq!(this.sidebar_color(id), Some("#123ABC"));
        assert!(this
            .set_sidebar_connection_color(id, Some("not-color"), cx)
            .is_err());
        assert_eq!(this.sidebar_color(id), Some("#123ABC"));
        this.set_sidebar_connection_color(id, Some("#ABC123"), cx)
            .expect("valid retry should work without reloading");
        assert_eq!(this.sidebar_color(id), Some("#ABC123"));
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn corrupt_sidebar_file_blocks_writes_without_overwriting_and_allows_repair(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-corrupt-{}.json",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, b"not json").unwrap();
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        assert!(this.reload_sidebar_preferences(cx).is_err());
        assert!(this
            .create_sidebar_group("Do not overwrite", None, cx)
            .is_err());
        assert!(this
            .sidebar
            .error
            .as_deref()
            .is_some_and(|message| message.contains("read")));
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
        std::fs::write(&path, b"{}").unwrap();
        this.reload_sidebar_preferences(cx).unwrap();
        this.create_sidebar_group("Repaired", None, cx).unwrap();
        assert_eq!(this.navigator.visible_rows()[0].label, "Repaired");
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_search_editor_and_session_events_update_real_navigator(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            let editor = this.sidebar_search_input(window, cx);
            editor.update(cx, |state, cx| {
                state.set_value("Postgres", window, cx);
                cx.emit(gpui_component::input::InputEvent::Change);
            });
        });
    });
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        assert_eq!(this.navigator.visible_row_count(), 1);
        let mut options = this.navigator.search_options().clone();
        options.connected_only = true;
        this.update_sidebar_search(options, cx);
        assert_eq!(this.navigator.visible_row_count(), 0);
        this.start_scenario_connect(
            crate::ui_fixture::POSTGRES_ID.into(),
            "Postgres fixture".into(),
            None,
            cx,
        );
    });
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        assert_eq!(this.navigator.visible_row_count(), 1);
        this.sessions.disconnect(crate::ui_fixture::POSTGRES_ID);
        this.sync_sidebar_connections(cx);
        assert_eq!(this.navigator.visible_row_count(), 0);
    });
}

#[gpui::test]
fn invalid_sidebar_regex_reports_error_and_matches_nothing_until_fixed(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.update_sidebar_search(
            crate::navigator::SearchOptions {
                query: "Postgres".into(),
                ..Default::default()
            },
            cx,
        );
        assert_eq!(this.navigator.visible_row_count(), 1);
        this.update_sidebar_search(
            crate::navigator::SearchOptions {
                query: "[".into(),
                regex: true,
                ..Default::default()
            },
            cx,
        );
        assert!(this.sidebar.search_error.is_some());
        assert_eq!(this.navigator.visible_row_count(), 0, "an invalid pattern matches nothing");
        this.update_sidebar_search(
            crate::navigator::SearchOptions {
                query: "Sqlite".into(),
                ..Default::default()
            },
            cx,
        );
        assert!(this.sidebar.search_error.is_none());
        assert_eq!(this.navigator.visible_rows()[0].label, "SQLite fixture");
    });
}

#[gpui::test]
fn sidebar_query_command_uses_visible_target_context(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        let id = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
        };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.kind, TabKind::Query);
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
        assert!(!this.new_query_for_sidebar_node(
            &NavigatorNodeId::Connection {
                connection: "deleted".into()
            },
            cx
        ));
    });
}

#[gpui::test]
fn copy_config_target_is_disabled_for_multi_or_mixed_selection(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        let first = this.navigator.visible_rows()[0].id.clone();
        let second = this.navigator.visible_rows()[1].id.clone();
        this.navigator.select_visible(&first, false, false);
        assert!(this.sidebar_single_config_copy_target().is_some());
        this.navigator.select_visible(&second, true, false);
        assert!(this.sidebar_single_config_copy_target().is_none());
        let mut prefs = crate::sidebar_preferences::SidebarPreferences::default();
        prefs.add_group("work", "Work", None).unwrap();
        this.navigator.set_preferences(prefs).unwrap();
        this.navigator.select_visible(&first, false, false);
        this.navigator
            .select_visible(&NavigatorNodeId::Group { id: "work".into() }, true, false);
        assert!(this.sidebar_single_config_copy_target().is_none());
    });
}

#[gpui::test]
fn sidebar_mongo_query_uses_provider_payload_and_real_collection_tab(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.open_or_activate_collection_tab(
            crate::ui_fixture::MONGO_ID,
            "fixture_db_0",
            "fixture_collection_01",
        );
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        let selected = this
            .navigator
            .selected()
            .cloned()
            .expect("located collection");
        assert!(this.new_query_for_sidebar_node(&selected, cx));
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.kind, TabKind::Query);
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert_eq!(tab.database, "fixture_db_0");
        assert_eq!(tab.collection, "fixture_collection_01");
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
}

#[gpui::test]
fn new_mongo_query_from_disconnected_connection_opens_idle_script_not_sql(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let id = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert_eq!(tab.connection_id, crate::ui_fixture::MONGO_ID);
        assert!(tab.sql.is_empty());
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
}

#[gpui::test]
fn mongo_database_new_query_scopes_script_without_auto_running(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let root = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        let id = this
            .navigator
            .find_provider_child(&root, "database", "fixture_db_0", None)
            .unwrap();
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert_eq!(tab.database, "fixture_db_0");
        assert!(tab.collection.is_empty());
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
}

#[gpui::test]
fn mongo_connection_query_uses_only_uri_path_database_as_default(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        this.saved_connections
            .iter_mut()
            .find(|saved| saved.matches_id(crate::ui_fixture::MONGO_ID))
            .unwrap()
            .uri = "mongodb://fixture.invalid/warehouse%2Ddev?authSource=admin".into();
        let id = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.database, "warehouse-dev");
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
}

#[gpui::test]
fn mongo_script_editor_survives_tab_switch_without_running_query(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.seed_connection_tree_scenario();
            let node = NavigatorNodeId::Connection {
                connection: crate::ui_fixture::MONGO_ID.into(),
            };
            assert!(this.new_query_for_sidebar_node(&node, cx));
            let tab_id = this.workspace.active_tab().unwrap().id;
            this.workspace.tab_mut(tab_id).unwrap().sql = "db.users.find({})".into();
            let first = this.editor_for_tab(tab_id, window, cx);
            this.workspace.open_query_tab("Another");
            this.workspace.activate_tab(tab_id);
            let restored = this.editor_for_tab(tab_id, window, cx);
            assert_eq!(first.entity_id(), restored.entity_id());
            assert_eq!(restored.read(cx).value().to_string(), "db.users.find({})");
            assert!(matches!(
                this.workspace.runtime(tab_id).unwrap().query.status(),
                crate::query::QueryStatus::Idle
            ));
        })
    });
}

#[gpui::test]
fn mongo_script_run_rejects_a_missing_database_and_dispatches_no_shell_text_without_the_plugin(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let id = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab_id = this.workspace.active_tab().unwrap().id;
        // Not a single find: a shell script, which has no plugin here, so nothing runs (the
        // dialog that offers it needs the window; this test has none).
        this.workspace.tab_mut(tab_id).unwrap().database = "warehouse".into();
        this.workspace.tab_mut(tab_id).unwrap().sql = "SELECT * FROM users".into();
        this.run_active_tab_query(cx);
        assert!(matches!(this.workspace.runtime(tab_id).unwrap().query.status(), crate::query::QueryStatus::Idle));
        assert!(!this.query_tasks.tasks.contains_key(&tab_id));
        this.workspace.tab_mut(tab_id).unwrap().database = String::new();
        this.workspace.tab_mut(tab_id).unwrap().sql = "db.users.find({})".into();
        this.run_active_tab_query(cx);
        assert!(matches!(this.workspace.runtime(tab_id).unwrap().query.status(), crate::query::QueryStatus::Failed(_)));
        this.workspace.tab_mut(tab_id).unwrap().database = "warehouse".into();
        this.run_active_tab_query(cx);
        assert!(matches!(this.workspace.runtime(tab_id).unwrap().query.status(), crate::query::QueryStatus::Failed(error) if error.contains("UI scenario")));
        assert!(!this.query_tasks.tasks.contains_key(&tab_id));
        assert_eq!(this.workspace.active_tab().unwrap().query_mode, Some(QueryMode::MongoScript));
    });
}

#[gpui::test]
fn mongo_script_pending_connect_drops_stale_generation_and_closed_tab(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let connection = crate::ui_fixture::MONGO_ID;
        let id = NavigatorNodeId::Connection {
            connection: connection.into(),
        };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.tab_mut(tab_id).unwrap().sql = "db.items.find({})".into();
        let old = this.sessions.begin_connect(connection).unwrap();
        super::mongo_script::queue_pending_mongo(
            &mut this.workspace,
            &mut this.query_tasks,
            tab_id,
            connection,
            old,
            "db.items.find({})",
        )
        .unwrap();
        this.sessions.disconnect(connection);
        let new = this.sessions.begin_connect(connection).unwrap();
        this.sessions.finish_connect(connection, new, Ok(()));
        this.resume_pending_runs(connection, cx);
        assert!(!this.query_tasks.pending_runs.contains_key(&tab_id));
        assert!(matches!(
            this.workspace.runtime(tab_id).unwrap().query.status(),
            crate::query::QueryStatus::Failed(_)
        ));
        super::mongo_script::queue_pending_mongo(
            &mut this.workspace,
            &mut this.query_tasks,
            tab_id,
            connection,
            new,
            "db.items.find({})",
        )
        .unwrap();
        this.close_tab(tab_id, cx);
        assert!(!this.query_tasks.pending_runs.contains_key(&tab_id));
    });
}

#[test]
fn queued_mongo_run_owns_running_state_and_deduplicates_same_tab() {
    let mut workspace = crate::workspace::WorkspaceState::new();
    let tab_id = workspace.open_query_tab_with_metadata(
        "Mongo",
        crate::workspace::TabMetadata {
            connection_id: "conn".into(),
            query_mode: Some(QueryMode::MongoScript),
            ..Default::default()
        },
    );
    let mut tasks = super::workspace::QueryTaskState::default();
    let run = super::mongo_script::queue_pending_mongo(
        &mut workspace,
        &mut tasks,
        tab_id,
        "conn",
        4,
        "db.docs.find({})",
    )
    .unwrap();
    assert!(
        matches!(workspace.runtime(tab_id).unwrap().query.status(), crate::query::QueryStatus::Running { run: active } if *active == run)
    );
    assert!(super::mongo_script::queue_pending_mongo(
        &mut workspace,
        &mut tasks,
        tab_id,
        "conn",
        4,
        "db.docs.find({})"
    )
    .is_none());
    assert_eq!(tasks.pending_runs.len(), 1);
}

#[gpui::test]
fn mongo_connect_pending_run_toolbar_cancel_only_removes_its_tab(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let mut ids = (0, 0);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let connection = crate::ui_fixture::MONGO_ID;
        let epoch = this.sessions.begin_connect(connection).unwrap();
        let node = NavigatorNodeId::Connection {
            connection: connection.into(),
        };
        assert!(this.new_query_for_sidebar_node(&node, cx));
        let first = this.workspace.active_tab().unwrap().id;
        assert!(this.new_query_for_sidebar_node(&node, cx));
        let second = this.workspace.active_tab().unwrap().id;
        for tab_id in [first, second] {
            this.workspace.tab_mut(tab_id).unwrap().sql = "db.docs.find({})".into();
            super::mongo_script::queue_pending_mongo(
                &mut this.workspace,
                &mut this.query_tasks,
                tab_id,
                connection,
                epoch,
                "db.docs.find({})",
            )
            .unwrap();
        }
        this.workspace.activate_tab(first);
        this.reset_active_view();
        ids = (first, second);
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let run_button = visual
        .debug_bounds("run-query")
        .expect("pending script has a Cancel button");
    visual.simulate_click(run_button.center(), Default::default());
    app.update(visual, |this, cx| {
        assert!(matches!(
            this.workspace.runtime(ids.0).unwrap().query.status(),
            crate::query::QueryStatus::Cancelled
        ));
        assert!(matches!(
            this.workspace.runtime(ids.1).unwrap().query.status(),
            crate::query::QueryStatus::Running { .. }
        ));
        assert!(!this.query_tasks.pending_runs.contains_key(&ids.0));
        assert!(this.query_tasks.pending_runs.contains_key(&ids.1));
        assert!(matches!(
            this.sessions.status(crate::ui_fixture::MONGO_ID),
            ConnectionStatus::Connecting
        ));
        let connection = crate::ui_fixture::MONGO_ID;
        let epoch = this.sessions.generation(connection).unwrap();
        this.sessions.finish_connect(connection, epoch, Ok(()));
        this.resume_pending_runs(connection, cx);
        assert!(matches!(
            this.workspace.runtime(ids.0).unwrap().query.status(),
            crate::query::QueryStatus::Cancelled
        ));
        assert!(!this.query_tasks.pending_runs.contains_key(&ids.0));
        assert!(!this.query_tasks.tasks.contains_key(&ids.0));
    });
}

#[gpui::test]
fn mongo_script_cancel_rejects_late_result_for_same_tab(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let id = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab_id = this.workspace.active_tab().unwrap().id;
        let run = this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .query
            .begin()
            .unwrap();
        this.cancel_tab_query(tab_id, cx);
        assert!(matches!(
            this.workspace.runtime(tab_id).unwrap().query.status(),
            crate::query::QueryStatus::Cancelled
        ));
        assert!(!this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .query
            .complete(run, 3));
        assert!(!this.query_tasks.tasks.contains_key(&tab_id));
    });
}

#[gpui::test]
fn changed_mongo_script_resets_previous_query_page_before_run(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let id = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        assert!(this.new_query_for_sidebar_node(&id, cx));
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.tab_mut(tab_id).unwrap().database = "warehouse".into();
        this.workspace.tab_mut(tab_id).unwrap().sql = "db.orders.find({ active: true })".into();
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        runtime.pagination.set_result_count(200);
        assert!(runtime.pagination.next());
        runtime.last_executed_query = Some("db.orders.find({ active: false })".into());
        this.run_active_tab_query(cx);
        assert_eq!(this.workspace.runtime(tab_id).unwrap().pagination.page(), 0);
    });
}

#[gpui::test]
fn connection_clipboard_copy_never_rehydrates_or_includes_saved_password(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(AppRoot::new_isolated);
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    app.update(visual, |this, cx| {
        let id = uuid::Uuid::new_v4();
        this.saved_connections = vec![SavedConnection {
            id,
            name: "Sensitive".into(),
            uri: "postgresql://alice:secret@localhost/app".into(),
            kind: ConnectionKind::Postgres,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }];
        this.copy_connection_string(&id.to_string(), cx);
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("copied URI");
        assert!(!text.contains("secret"), "copied clipboard must be safe");
        assert_eq!(text, "postgresql://alice@localhost/app");
    });
}

#[gpui::test]
fn sidebar_keyboard_focus_moves_selection_through_virtual_rows(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let disclosure = visual
        .debug_bounds("nav-disclosure-0")
        .expect("first virtual row disclosure");
    let click = gpui::point(disclosure.center().x + px(36.0), disclosure.center().y);
    visual.simulate_click(click, Default::default());
    visual.run_until_parked();
    let previous = app.update(visual, |this, _| this.navigator.selected().cloned());
    visual.simulate_keystrokes("down");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_ne!(this.navigator.selected(), previous.as_ref());
        assert!(this.navigator.selected().is_some());
    });
}

#[gpui::test]
fn locate_active_sql_tab_expands_and_loads_path_then_selects_object(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.open_query_tab_with_metadata(
            "orders",
            TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                ..Default::default()
            },
        );
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(matches!(this.navigator.selected(), Some(NavigatorNodeId::Object { database, schema, object, .. })
            if database == "warehouse" && schema == "public" && object == "orders"));
        assert!(this.sidebar.locate_error.is_none(), "{:?}", this.sidebar.locate_error);
    });
}

#[gpui::test]
fn locate_does_not_select_stale_tab_after_switch(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        let welcome = this.workspace.active_tab().unwrap().id;
        this.workspace.open_query_tab_with_metadata(
            "orders",
            TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                ..Default::default()
            },
        );
        this.locate_active_sidebar_tab(cx);
        this.workspace.activate_tab(welcome);
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(this.navigator.selected().is_none());
    });
}

#[gpui::test]
fn locate_mongo_collection_paginates_root_and_database_without_guessing_ids(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.open_or_activate_collection_tab(
            crate::ui_fixture::MONGO_ID,
            "fixture_db_6",
            "fixture_collection_24",
        );
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let selected = this
            .navigator
            .selected()
            .cloned()
            .expect("located collection");
        let metadata = this
            .navigator
            .provider_node(&selected)
            .expect("provider payload");
        assert_eq!(metadata.payload["database"], "fixture_db_6");
        assert_eq!(metadata.payload["collection"], "fixture_collection_24");
        assert!(
            this.sidebar.locate_error.is_none(),
            "{:?}",
            this.sidebar.locate_error
        );
    });
}

#[gpui::test]
fn locate_mongo_reopens_cached_collapsed_database_and_connection_without_recursing(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let root = NavigatorNodeId::Connection {
        connection: crate::ui_fixture::MONGO_ID.into(),
    };
    app.update(visual, |this, cx| {
        let row = this.navigator.visible_row_at(0).unwrap();
        this.handle_navigator_disclosure(&row, cx);
    });
    visual.run_until_parked();
    let db = app.update(visual, |this, cx| {
        let db = this
            .navigator
            .find_provider_child(&root, "database", "fixture_db_0", None)
            .unwrap();
        let row = this
            .navigator
            .visible_row_at(this.navigator.visible_index(&db).unwrap())
            .unwrap();
        this.handle_navigator_disclosure(&row, cx);
        db
    });
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        let row = this
            .navigator
            .visible_row_at(this.navigator.visible_index(&db).unwrap())
            .unwrap();
        this.handle_navigator_disclosure(&row, cx);
        this.workspace.open_or_activate_collection_tab(
            crate::ui_fixture::MONGO_ID,
            "fixture_db_0",
            "fixture_collection_00",
        );
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        assert!(
            this.sidebar.locate_error.is_none(),
            "cached database: {:?}",
            this.sidebar.locate_error
        );
        assert!(this.navigator.selected().is_some());
        let row = this
            .navigator
            .visible_row_at(this.navigator.visible_index(&root).unwrap())
            .unwrap();
        this.handle_navigator_disclosure(&row, cx);
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            this.sidebar.locate_error.is_none(),
            "cached connection: {:?}",
            this.sidebar.locate_error
        );
        assert!(matches!(
            this.navigator.selected(),
            Some(NavigatorNodeId::Provider { .. })
        ));
    });
}

#[cfg(unix)]
#[test]
fn sidebar_scenario_prefs_guard_accepts_only_absolute_temp_files() {
    // Pure guard: no env access, so parallel tests cannot interfere.
    let valid = std::env::temp_dir().join("tablex-guard.json");
    assert_eq!(
        super::sidebar::scenario_prefs_path_from(valid.to_str().unwrap()),
        Some(valid.clone())
    );
    for rejected in [
        "relative/path.json",
        "/etc/tablex.json",
        "/definitely-not-the-temp-dir/tablex.json",
    ] {
        assert_eq!(
            super::sidebar::scenario_prefs_path_from(rejected),
            None,
            "{rejected} must be refused"
        );
    }
    let alias =
        std::env::temp_dir().join(format!("tablex-scenario-alias-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(std::env::current_dir().unwrap(), &alias).unwrap();
    assert_eq!(
        super::sidebar::scenario_prefs_path_from(alias.join("prefs.json").to_str().unwrap()),
        None
    );
    std::fs::remove_file(alias).unwrap();
    let alias = std::env::temp_dir().join(format!("tablex-temp-alias-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(std::env::temp_dir(), &alias).unwrap();
    assert_eq!(
        super::sidebar::scenario_prefs_path_from(alias.join("prefs.json").to_str().unwrap()),
        None
    );
    std::fs::remove_file(alias).unwrap();
    let alias = std::env::temp_dir().join(format!("tablex-prefs-alias-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(&valid, &alias).unwrap();
    assert_eq!(
        super::sidebar::scenario_prefs_path_from(alias.to_str().unwrap()),
        None
    );
    std::fs::remove_file(alias).unwrap();
}

#[gpui::test]
fn sidebar_hidden_state_survives_reconstruction_from_the_same_file(cx: &mut TestAppContext) {
    // Restart proof without a second process: two scenario constructions
    // share one isolated temp file via direct path assignment (no process
    // env, so parallel tests are unaffected), exactly mirroring what the
    // capture launcher arms through TABLEX_SIDEBAR_PREFS_PATH.
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-restart-{}.json",
        uuid::Uuid::new_v4()
    ));
    let (first, view1) = sidebar_test_app(cx);
    first.update(view1, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.set_sidebar_connection_hidden(crate::ui_fixture::SQLITE_ID, true, cx)
            .unwrap();
    });
    assert!(std::fs::read(&path).is_ok(), "hide wrote the isolated file");
    let (second, view2) = sidebar_test_app(cx);
    second.update(view2, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.reload_sidebar_preferences(cx).unwrap();
        assert!(
            !this
                .navigator
                .visible_rows()
                .iter()
                .any(|row| row.label == "SQLite fixture"),
            "reconstruction honors the persisted hidden flag"
        );
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_seed_colors_fall_back_without_overwriting_file_colors(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        // Simulate a prefs file that sets an explicit Postgres color, then
        // apply the display seed: explicit file colors must win.
        this.sidebar
            .color_overrides
            .insert(crate::ui_fixture::POSTGRES_ID.into(), "#FF0000".into());
        this.sidebar.seed_scenario_colors_as_fallback();
        assert_eq!(
            this.sidebar_color(crate::ui_fixture::POSTGRES_ID),
            Some("#FF0000")
        );
        assert_eq!(
            this.sidebar_color(crate::ui_fixture::MONGO_ID),
            Some("#34C759")
        );
        assert_eq!(
            this.sidebar_color(crate::ui_fixture::SQLITE_ID),
            Some("#0F80CC")
        );
    });
}

#[gpui::test]
fn sidebar_shell_starts_from_validated_saved_width(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        let width = this.navigator.preferences().resolve_sidebar_width();
        assert_eq!(width, 260.0, "scenario default has no saved width");
        assert!((this.shell.sidebar_width - width).abs() < f32::EPSILON);
    });
}

#[gpui::test]
fn sidebar_menu_new_query_opens_scoped_tab_through_real_menu(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        this.seed_connection_tree_scenario();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Seed layout: orders is visible row 21.
    let row = visual
        .debug_bounds("sidebar-row-21")
        .expect("orders row bounds");
    assert_eq!(
        app.update(visual, |this, _| this
            .navigator
            .visible_row_at(21)
            .map(|row| row.label.clone())),
        Some("orders".into())
    );
    visual.simulate_mouse_down(row.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(row.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    // Object menu: Open, Copy SQL, New Query.
    for _ in 0..3 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("menu opened a tab");
        assert_eq!(
            tab.title, "Query",
            "New Query names the tab, Enter-open reuses the label"
        );
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
        assert!(tab.sql.contains("orders"), "scoped preview SQL filled in");
    });
}

#[gpui::test]
fn sidebar_menu_hide_and_settings_restore_through_real_menus(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-menu-hide-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, _| {
        this.sidebar.path = Some(path.clone());
        this.seed_connection_tree_scenario();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Hide left the connection menu for the settings menu: left-click selects
    // the Postgres connection, then Hide/Unhide acts on the selection.
    // Seed layout: Postgres fixture is visible row 17.
    let row = visual
        .debug_bounds("sidebar-row-17")
        .expect("postgres row bounds");
    visual.simulate_click(row.center(), Default::default());
    visual.run_until_parked();
    let settings = visual
        .debug_bounds("sidebar-settings")
        .expect("settings button bounds");
    visual.simulate_click(settings.center(), Default::default());
    visual.run_until_parked();
    // Settings menu: Manual, Ascending, Descending, All, Connections,
    // Databases, Objects, Show hidden, Toggle Pin, then Hide/Unhide at the
    // 10th keystroke (the first down skips the unclickable Sort label).
    for _ in 0..10 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            !this
                .navigator
                .visible_rows()
                .iter()
                .any(|row| row.label == "Postgres fixture"),
            "Hide Connection removed the subtree"
        );
    });
    let settings = visual
        .debug_bounds("sidebar-settings")
        .expect("settings button bounds");
    visual.simulate_click(settings.center(), Default::default());
    visual.run_until_parked();
    // Show hidden is the 8th clickable row (the Sort label is skipped).
    for _ in 0..8 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        assert!(this.navigator.search_options().show_hidden);
        assert!(
            this.navigator
                .visible_rows()
                .iter()
                .any(|row| row.label == "Postgres fixture"),
            "Show hidden restores the subtree"
        );
        this.set_sidebar_connection_hidden(crate::ui_fixture::POSTGRES_ID, false, cx)
            .unwrap();
        this.update_sidebar_search(Default::default(), cx);
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_drop_handler_reorders_connection_and_persists(cx: &mut TestAppContext) {
    // The harness mouse pipeline never initiates a platform drag (no drag
    // state after down/move/up in any button combination), so this drives the
    // exact on_drop callback the native gesture calls, with real model +
    // isolated-file persistence. Menu, keyboard and click paths above prove
    // the surrounding UI event delivery end to end.
    let (app, visual) = sidebar_test_app(cx);
    let path =
        std::env::temp_dir().join(format!("tablex-sidebar-drag-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.seed_connection_tree_scenario();
        let drag = super::navigator_ui::SidebarDrag {
            id: NavigatorNodeId::Connection {
                connection: crate::ui_fixture::SQLITE_ID.into(),
            },
            ids: None,
        };
        let target = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        this.drop_sidebar_drag(&drag, &target, cx);
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let labels: Vec<_> = this
            .navigator
            .visible_rows()
            .iter()
            .take(2)
            .map(|row| row.label.clone())
            .collect();
        assert_eq!(
            labels,
            vec!["SQLite fixture".to_string(), "Mongo fixture".to_string()],
            "drop inserts the dragged connection before its target"
        );
    });
    assert!(
        std::fs::read(&path).is_ok(),
        "drop persisted to the isolated file"
    );
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_collapse_rail_reexpands_through_real_buttons(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let hide = visual
        .debug_bounds("sidebar-hide")
        .expect("hide button bounds");
    visual.simulate_click(hide.center(), Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| assert!(this.shell.sidebar_collapsed));
    let expand = visual
        .debug_bounds("expand-sidebar")
        .expect("rail expand button renders while collapsed");
    visual.simulate_click(expand.center(), Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(!this.shell.sidebar_collapsed);
        assert!(this.navigator.visible_row_count() > 0);
    });
}

#[gpui::test]
fn sidebar_regex_toggle_arms_pattern_search(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let toggle = visual
        .debug_bounds("sidebar-regex")
        .expect("regex toggle bounds");
    visual.simulate_click(toggle.center(), Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(this.navigator.search_options().regex)
    });
}

/// Native keyboard activation clicks on key *up*; `simulate_keystrokes` only
/// sends key down, so send the full press/release pair like a real keyboard.
fn press_key_with_release(visual: &mut gpui::VisualTestContext, key: &str) {
    let keystroke = shortcuts::parse_chord(key).unwrap();
    visual.simulate_event(gpui::KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    visual.simulate_event(gpui::KeyUpEvent { keystroke });
    visual.run_until_parked();
}

#[gpui::test]
fn sidebar_regex_toggle_is_keyboard_operable_from_the_search_input(cx: &mut TestAppContext) {
    // The restored official Button owns a tab stop and native Enter/Space
    // clicks: Tab from the focused input lands on it, Enter/Space toggle.
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let input = app.update_in(visual, |this, window, cx| this.sidebar_search_input(window, cx));
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.focus(window, cx));
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("tab");
    visual.run_until_parked();
    press_key_with_release(visual, "enter");
    app.read_with(visual, |this, _| {
        assert!(this.navigator.search_options().regex, "Enter on the focused toggle arms regex")
    });
    press_key_with_release(visual, "space");
    app.read_with(visual, |this, _| {
        assert!(!this.navigator.search_options().regex, "Space on the focused toggle disarms regex")
    });
}

#[gpui::test]
fn sidebar_toolbar_stays_inside_pane(cx: &mut TestAppContext) {
    // Parent-asserted containment at the default 260px width: every toolbar
    // and search-row control must paint fully inside the pane, not just
    // exist. row-0 spans the pane width. The 180px minimum is proven by
    // native capture (resizable panels snapshot size at first layout, so a
    // unit test cannot drive the narrow width through the same state path
    // as the real drag handle).
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        this.seed_connection_tree_scenario();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let pane = visual.debug_bounds("sidebar-row-0").expect("pane probe");
    assert_eq!(pane.size.width, gpui::px(crate::theme::SIDEBAR_WIDTH));
    for id in [
        "sidebar-import-export",
        "sidebar-collapse-all",
        "sidebar-new-group",
        "sidebar-refresh",
        "sidebar-hide",
        "sidebar-regex",
        "sidebar-locate",
        "sidebar-settings",
        "sidebar-connected-only",
    ] {
        // ±2px: the harness renders text wider than production, so at 260
        // the row can overflow by a pixel here while fitting natively.
        // Exact docking is asserted by native measured probes instead.
        let bounds = visual.debug_bounds(id).expect("toolbar button bounds");
        assert!(
            bounds.left() >= pane.left() - gpui::px(2.0)
                && bounds.right() <= pane.right() + gpui::px(2.0),
            "{id} spills outside the pane"
        );
    }
    // Bar alignment: trigger leads at the left padding, the action
    // group docks right (within rounding), icon siblings keep exact 1px
    // gaps, and collapse sits after the trigger with no overlap. Surplus
    // distribution (flex-1 spacer width at 260/420, truncation at 180) is
    // proven by native measured probes: the test harness renders text
    // wider than production, so absolute surplus cannot be asserted here.
    let trigger = visual.debug_bounds("sidebar-import-export").unwrap();
    let collapse = visual.debug_bounds("sidebar-collapse-all").unwrap();
    let new_group = visual.debug_bounds("sidebar-new-group").unwrap();
    let refresh = visual.debug_bounds("sidebar-refresh").unwrap();
    let hide = visual.debug_bounds("sidebar-hide").unwrap();
    assert_eq!(trigger.left(), pane.left() + gpui::px(12.0));
    let dock_gap = f32::from(pane.right() - gpui::px(12.0)) - f32::from(hide.right());
    assert!(
        dock_gap.abs() < 2.0,
        "actions not docked right, gap {dock_gap}"
    );
    assert!(
        collapse.left() >= trigger.right() + gpui::px(1.0),
        "collapse overlaps trigger"
    );
    assert_eq!(new_group.left() - collapse.right(), gpui::px(1.0));
    assert_eq!(refresh.left() - new_group.right(), gpui::px(1.0));
    assert_eq!(hide.left() - refresh.right(), gpui::px(1.0));
}

#[gpui::test]
fn sidebar_multi_toolbar_renders_actions_and_exit_clears(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        this.navigator.select_visible(&rows[0].id, false, false);
        this.navigator.select_visible(&rows[1].id, true, false);
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    for id in [
        "sidebar-multi-group",
        "sidebar-multi-move",
        "sidebar-multi-delete",
        "sidebar-multi-select-all",
        "sidebar-multi-exit",
    ] {
        assert!(
            visual.debug_bounds(id).is_some(),
            "multi toolbar shows {id}"
        );
    }
    assert!(visual.debug_bounds("sidebar-import-export").is_none());
    let exit = visual.debug_bounds("sidebar-multi-exit").unwrap();
    visual.simulate_click(exit.center(), Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(!this.navigator.connection_multi_mode());
    });
    assert!(visual.debug_bounds("sidebar-import-export").is_some());
}

#[gpui::test]
fn sidebar_multi_toolbar_select_all_and_group_dialog(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        this.navigator.select_visible(&rows[0].id, false, false);
        this.navigator.select_visible(&rows[1].id, true, false);
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let select_all = visual.debug_bounds("sidebar-multi-select-all").unwrap();
    visual.simulate_click(select_all.center(), Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.navigator.selected_connection_ids().unwrap().len(), 3);
    });
    let group = visual.debug_bounds("sidebar-multi-group").unwrap();
    visual.simulate_click(group.center(), Default::default());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("dialog-layer").is_some(),
        "group button opens the create-group dialog"
    );
}

#[cfg(feature = "ui-scenarios")]
#[gpui::test]
fn sidebar_bounds_probes_measure_visible_rows_and_toolbar(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        this.seed_connection_tree_scenario();
        // Seed layout: rows 0 and 17 are the Mongo and Postgres connections.
        let rows = this.navigator.visible_rows();
        this.navigator.select_visible(&rows[0].id, false, false);
        this.navigator.select_visible(&rows[17].id, true, false);
        assert!(this.navigator.connection_multi_mode());
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let cache = &this.scenario_ui.sidebar_bounds;
        assert!(
            cache.len() > 20,
            "probes measured the visible rows: {}",
            cache.len()
        );
        assert!(cache.contains_key("row-0"));
        for key in [
            "multi-group",
            "multi-move",
            "multi-delete",
            "multi-select-all",
            "multi-exit",
        ] {
            assert!(cache.contains_key(key), "multi toolbar probe fired: {key}");
        }
    });
}

#[gpui::test]
fn sidebar_escape_releases_multi_selection_without_losing_active_row(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            let rows = this.navigator.visible_rows();
            this.navigator.select_visible(&rows[0].id, false, false);
            this.navigator.select_visible(&rows[1].id, true, false);
            this.sidebar.focus.focus(window, cx);
            cx.notify();
        });
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.navigator.selected_ids().len(), 1);
        assert!(this.navigator.selected().is_some());
    });
}

#[gpui::test]
fn sidebar_seed_builds_representative_tinted_tree(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        this.sidebar.seed_scenario_colors();
        this.seed_connection_tree_scenario();
    });
    app.update(visual, |this, _| {
        assert_eq!(
            this.sidebar_color(crate::ui_fixture::MONGO_ID),
            Some("#34C759")
        );
        assert_eq!(
            this.sidebar_color(crate::ui_fixture::POSTGRES_ID),
            Some("#336791")
        );
        assert_eq!(
            this.sidebar_color(crate::ui_fixture::SQLITE_ID),
            Some("#0F80CC")
        );
        let rows = this.navigator.visible_rows();
        assert!(rows.len() > 20, "seeded tree shows both subtrees");
        let mongo = rows
            .iter()
            .find(|row| row.label == "Mongo fixture")
            .unwrap();
        assert!(mongo.expanded);
        assert!(rows
            .iter()
            .any(|row| row.label == "fixture_db_0" && row.expanded));
        assert!(rows.iter().any(|row| row.label == "fixture_collection_00"));
        assert!(rows.iter().any(|row| row.label == "orders"));
        let selected = this.navigator.selected().expect("seeded selection");
        assert!(
            matches!(selected, NavigatorNodeId::Provider { .. }),
            "seed selects the green-tinted mongo collection, got {selected:?}"
        );
    });
}

#[test]
fn sidebar_hex_colors_parse_and_blend_without_allocating_strings() {
    let green = super::navigator_ui::parse_hex_color("#34C759").expect("valid color");
    assert_eq!(super::navigator_ui::parse_hex_color("#34c759"), Some(green));
    assert!(super::navigator_ui::parse_hex_color("34C759").is_none());
    assert!(super::navigator_ui::parse_hex_color("#12345").is_none());
    assert!(super::navigator_ui::parse_hex_color("#ZZZZZZ").is_none());
    let tint = super::navigator_ui::tinted(green, 0.08);
    assert!((tint.r - green.r).abs() < f32::EPSILON);
    assert!((tint.a - 0.08).abs() < 1e-6);
    assert_eq!(super::navigator_ui::tinted(green, 9.0).a, 1.0);
}

#[test]
fn sidebar_rows_resolve_their_owning_connection_for_subtree_tints() {
    use super::navigator_ui::row_connection_id;
    assert_eq!(
        row_connection_id(&NavigatorNodeId::Group { id: "g".into() }),
        None
    );
    assert_eq!(
        row_connection_id(&NavigatorNodeId::Connection {
            connection: "c".into()
        }),
        Some("c")
    );
    assert_eq!(
        row_connection_id(&NavigatorNodeId::Object {
            connection: "c".into(),
            database: "d".into(),
            schema: "s".into(),
            object: "o".into(),
        }),
        Some("c")
    );
    assert_eq!(
        row_connection_id(&NavigatorNodeId::Provider {
            connection: "c".into(),
            node: "n".into(),
        }),
        Some("c")
    );
}

#[test]
fn sidebar_brand_assets_cover_every_connection_kind() {
    use super::navigator_ui::connection_brand_asset;
    use crate::connections::ConnectionKind;
    assert_eq!(
        connection_brand_asset(ConnectionKind::Postgres),
        Some("icons/postgres.svg")
    );
    assert_eq!(
        connection_brand_asset(ConnectionKind::Sqlite),
        Some("icons/sqlite.svg")
    );
    assert_eq!(
        connection_brand_asset(ConnectionKind::Mongodb),
        Some("icons/mongodb.svg")
    );
    assert_eq!(connection_brand_asset(ConnectionKind::Mysql), Some("icons/mysql-mark.svg"));
    assert_eq!(connection_brand_asset(ConnectionKind::Mariadb), Some("icons/mariadb.svg"));
    assert_eq!(connection_brand_asset(ConnectionKind::Redis), Some("icons/redis.svg"));
}

#[gpui::test]
fn sidebar_sort_and_hide_controls_reshape_visible_rows(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        let path = std::env::temp_dir().join(format!(
            "tablex-sidebar-sort-hide-{}.json",
            uuid::Uuid::new_v4()
        ));
        this.sidebar.path = Some(path.clone());
        this.sort_sidebar_connections(crate::sidebar_preferences::ConnectionSort::Descending, cx)
            .unwrap();
        assert_eq!(this.navigator.visible_rows()[0].label, "SQLite fixture");
        this.set_sidebar_connection_hidden(crate::ui_fixture::SQLITE_ID, true, cx)
            .unwrap();
        assert_eq!(this.navigator.visible_row_count(), 2);
        this.set_sidebar_connection_hidden(crate::ui_fixture::SQLITE_ID, false, cx)
            .unwrap();
        assert_eq!(this.navigator.visible_row_count(), 3);
        assert!(this.create_sidebar_group("", None, cx).is_err());
        std::fs::remove_file(path).unwrap();
    });
}

#[gpui::test]
fn hidden_connection_is_recoverable_from_scope_menu_after_reload(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-hidden-recovery-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.set_sidebar_connection_hidden(crate::ui_fixture::SQLITE_ID, true, cx)
            .unwrap();
        this.reload_sidebar_preferences(cx).unwrap();
        assert!(!this
            .navigator
            .visible_rows()
            .iter()
            .any(|row| row.label == "SQLite fixture"));
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let scope = visual
        .debug_bounds("sidebar-settings")
        .expect("settings menu remains reachable");
    visual.simulate_click(scope.center(), Default::default());
    visual.run_until_parked();
    // Show hidden is the 8th clickable row (the Sort label is skipped).
    for _ in 0..8 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        assert!(
            this.navigator.search_options().show_hidden,
            "scope menu must expose Show hidden connections"
        );
        let id = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::SQLITE_ID.into(),
        };
        assert!(this.navigator.action_target(&id).is_some());
        assert!(this
            .navigator
            .available_actions(&id, false)
            .contains(&crate::navigator::NavigatorAction::HideConnection));
        this.set_sidebar_connection_hidden(crate::ui_fixture::SQLITE_ID, false, cx)
            .unwrap();
        this.update_sidebar_search(Default::default(), cx);
        this.reload_sidebar_preferences(cx).unwrap();
        assert!(this.navigator.action_target(&id).is_some());
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn unsupported_connection_enter_cannot_connect_but_stays_editable(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let id = uuid::Uuid::from_u128(42).to_string();
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            let saved = SavedConnection {
                id: uuid::Uuid::parse_str(&id).unwrap(),
                name: "Redis disabled".into(),
                uri: "redis://localhost".into(),
                kind: ConnectionKind::Redis,
                save_password: false,
                is_pinned: false,
                disabled: true,
                ssh_tunnel: false,
            };
            this.saved_connections = vec![saved.clone()];
            this.navigator.set_connections(vec![saved]);
            let row_id = NavigatorNodeId::Connection {
                connection: id.clone(),
            };
            this.navigator.select_visible(&row_id, false, false);
            assert!(!this.navigator.supports_action(
                &row_id,
                false,
                crate::navigator::NavigatorAction::Connect
            ));
            assert!(!this.navigator.supports_action(
                &row_id,
                false,
                crate::navigator::NavigatorAction::Refresh
            ));
            assert!(this.navigator.supports_action(
                &row_id,
                false,
                crate::navigator::NavigatorAction::Edit
            ));
            this.sidebar.focus.focus(window, cx);
            cx.notify();
        });
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.sessions.status(&id), ConnectionStatus::Disconnected);
        assert_eq!(
            this.workspace.active_tab().map(|tab| tab.kind),
            Some(TabKind::Welcome)
        );
    });
}

#[gpui::test]
fn opening_sidebar_context_menu_materializes_groups_only_on_demand(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-sidebar-lazy-menu-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.create_sidebar_group("Work", None, cx).unwrap();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let disclosure = visual.debug_bounds("nav-disclosure-0").expect("group row");
    let point = gpui::point(disclosure.center().x + px(35.0), disclosure.center().y);
    visual.simulate_mouse_down(point, gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(point, gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.navigator.visible_rows()[0].label, "Work")
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn sidebar_drop_positions_insert_before_the_target_sibling(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        let path =
            std::env::temp_dir().join(format!("tablex-sidebar-drop-{}.json", uuid::Uuid::new_v4()));
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Work", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&group), 0, cx)
            .unwrap();
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, Some(&group), 1, cx)
            .unwrap();
        let dragged = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::SQLITE_ID.into(),
        };
        let (parent, position) =
            this.sidebar_drop_sibling(crate::ui_fixture::POSTGRES_ID, &dragged);
        assert_eq!(parent.as_deref(), Some(group.as_str()));
        assert_eq!(position, 1);
        this.move_sidebar_connection(
            crate::ui_fixture::SQLITE_ID,
            parent.as_deref(),
            position,
            cx,
        )
        .unwrap();
        let labels: Vec<_> = this
            .navigator
            .visible_rows()
            .iter()
            .map(|row| row.label.clone())
            .collect();
        let sqlite = labels
            .iter()
            .position(|label| label == "SQLite fixture")
            .unwrap();
        let postgres = labels
            .iter()
            .position(|label| label == "Postgres fixture")
            .unwrap();
        assert!(
            sqlite < postgres,
            "drop inserts before the target: {labels:?}"
        );
        std::fs::remove_file(path).unwrap();
    });
}

#[gpui::test]
fn sidebar_drop_reindexes_forward_backward_same_and_unplaced_siblings(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        let path = std::env::temp_dir().join(format!(
            "tablex-sidebar-drop-reindex-{}.json",
            uuid::Uuid::new_v4()
        ));
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Work", None, cx).unwrap();
        for (position, id) in [
            crate::ui_fixture::MONGO_ID,
            crate::ui_fixture::POSTGRES_ID,
            crate::ui_fixture::SQLITE_ID,
        ]
        .into_iter()
        .enumerate()
        {
            this.move_sidebar_connection(id, Some(&group), position, cx)
                .unwrap();
        }
        let node = |id: &str| NavigatorNodeId::Connection {
            connection: id.into(),
        };
        let (parent, forward) = this.sidebar_drop_sibling(
            crate::ui_fixture::SQLITE_ID,
            &node(crate::ui_fixture::MONGO_ID),
        );
        assert_eq!(forward, 1, "forward drop must account for removed source");
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, parent.as_deref(), forward, cx)
            .unwrap();
        let labels: Vec<_> = this
            .navigator
            .visible_rows()
            .into_iter()
            .map(|row| row.label)
            .collect();
        assert_eq!(
            labels,
            [
                "Work",
                "Postgres fixture",
                "Mongo fixture",
                "SQLite fixture"
            ]
        );
        let (_, backward) = this.sidebar_drop_sibling(
            crate::ui_fixture::POSTGRES_ID,
            &node(crate::ui_fixture::SQLITE_ID),
        );
        assert_eq!(backward, 0);
        this.move_sidebar_connection(crate::ui_fixture::SQLITE_ID, Some(&group), backward, cx)
            .unwrap();
        let (_, same) = this.sidebar_drop_sibling(
            crate::ui_fixture::POSTGRES_ID,
            &node(crate::ui_fixture::POSTGRES_ID),
        );
        assert_eq!(same, 1, "dropping on itself keeps the position");
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, Some(&group), same, cx)
            .unwrap();
        let labels: Vec<_> = this
            .navigator
            .visible_rows()
            .into_iter()
            .map(|row| row.label)
            .collect();
        assert_eq!(
            labels,
            [
                "Work",
                "SQLite fixture",
                "Postgres fixture",
                "Mongo fixture"
            ]
        );
        this.move_sidebar_connection(crate::ui_fixture::SQLITE_ID, None, usize::MAX, cx)
            .unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, None, usize::MAX, cx)
            .unwrap();
        let (root, cross) = this.sidebar_drop_sibling(
            crate::ui_fixture::MONGO_ID,
            &node(crate::ui_fixture::POSTGRES_ID),
        );
        assert_eq!(root, None);
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, None, cross, cx)
            .unwrap();
        let labels: Vec<_> = this
            .navigator
            .visible_rows()
            .into_iter()
            .filter(|row| row.depth == 0)
            .map(|row| row.label)
            .collect();
        assert_eq!(
            labels,
            [
                "Work",
                "SQLite fixture",
                "Postgres fixture",
                "Mongo fixture"
            ]
        );

        let mut prefs = this.navigator.preferences().clone();
        prefs.remove_connection(crate::ui_fixture::MONGO_ID);
        prefs.remove_connection(crate::ui_fixture::SQLITE_ID);
        this.navigator.set_preferences(prefs).unwrap();
        let (root, before_unplaced) = this.sidebar_drop_sibling(
            crate::ui_fixture::MONGO_ID,
            &node(crate::ui_fixture::SQLITE_ID),
        );
        assert_eq!(root, None);
        assert_eq!(
            before_unplaced, 2,
            "unplaced target must be counted after persisted siblings"
        );
        this.move_sidebar_connection(crate::ui_fixture::SQLITE_ID, None, before_unplaced, cx)
            .unwrap();
        let labels: Vec<_> = this
            .navigator
            .visible_rows()
            .into_iter()
            .filter(|row| row.depth == 0)
            .map(|row| row.label)
            .collect();
        assert_eq!(
            labels,
            [
                "Work",
                "Postgres fixture",
                "SQLite fixture",
                "Mongo fixture"
            ]
        );
        std::fs::remove_file(path).unwrap();
    });
}

fn search_query(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, text: &str) {
    let input = app.update_in(visual, |this, window, cx| this.sidebar_search_input(window, cx));
    // Programmatic `set_value` emits no Change event: mirror what typing does.
    app.update_in(visual, |this, window, cx| {
        input.update(cx, |state, cx| state.set_value(text.to_string(), window, cx));
        let mut options = this.navigator.search_options().clone();
        options.query = text.to_string();
        this.update_sidebar_search(options, cx);
    });
    visual.run_until_parked();
}

fn visible_labels(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    app.read_with(visual, |this, _| {
        this.navigator.visible_rows().into_iter().map(|row| row.label).collect()
    })
}

fn connect_postgres_fixture(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    app.update(visual, |this, cx| {
        let id = crate::ui_fixture::POSTGRES_ID;
        let epoch = this.sessions.begin_connect(id).unwrap();
        this.sessions.finish_connect(id, epoch, Ok(()));
        this.sync_sidebar_connections(cx);
    });
}

fn settle_search(visual: &mut gpui::VisualTestContext) {
    visual
        .cx
        .executor()
        .advance_clock(super::remote_search::DEBOUNCE + Duration::from_millis(10));
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
}

/// The official `Input::cleanable` button has no debug selector of its own;
/// it is the 20px xsmall icon button immediately left of the suffix cluster
/// (the suffix keeps its right edge; the clear button grows out to its left).
/// Center of `Input::cleanable`'s built-in clear button, which has no debug
/// selector. Layout in gpui-component 0.7.0 (`input/input.rs` suffix h_flex,
/// `input/clear_button.rs`): the clear button is the sibling immediately left
/// of our suffix children, separated by the suffix `gap_x` (4px for a
/// `Size::Small` input), and is an `xsmall()` Button (`size_5` = 20px square).
/// Re-check these constants when upgrading gpui-component; callers assert the
/// click actually cleared the input, so a layout change fails loudly.
fn search_clear_point(visual: &mut gpui::VisualTestContext) -> gpui::Point<gpui::Pixels> {
    const SUFFIX_GAP_SMALL: f32 = 4.0;
    const XSMALL_BUTTON_SIZE: f32 = 20.0;
    let suffix = visual.debug_bounds("sidebar-search-suffix").expect("search suffix");
    gpui::point(
        suffix.left() - gpui::px(SUFFIX_GAP_SMALL + XSMALL_BUTTON_SIZE / 2.0),
        suffix.center().y,
    )
}

#[gpui::test]
fn search_row_clear_button_and_regex_toggle_reflect_state(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let regex = visual.debug_bounds("sidebar-regex").expect("regex toggle");
    assert!(regex.size.width >= gpui::px(20.0) && regex.size.height >= gpui::px(20.0));

    search_query(&app, visual, "post");
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Real click on the component's clear button resets state AND the input.
    let input = app.update_in(visual, |this, window, cx| this.sidebar_search_input(window, cx));
    let clear_at = search_clear_point(visual);
    visual.simulate_click(clear_at, Default::default());
    visual.run_until_parked();
    input.read_with(visual, |state, _| assert_eq!(state.value().as_ref(), ""));
    app.read_with(visual, |this, _| assert!(this.navigator.search_options().query.is_empty()));

    // Regex on with an invalid pattern surfaces the error state.
    let regex = visual.debug_bounds("sidebar-regex").unwrap();
    visual.simulate_click(regex.center(), Default::default());
    search_query(&app, visual, "(");
    app.read_with(visual, |this, _| {
        assert!(this.navigator.search_options().regex);
        assert!(this.sidebar.search_error.is_some(), "invalid regex is reported");
    });
}

#[gpui::test]
fn local_search_switch_persists_and_is_disabled_under_regex(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-local-search-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, _| this.sidebar.path = Some(path.clone()));
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    app.read_with(visual, |this, _| assert!(this.navigator.preferences().local_search(), "default: local"));

    let switch = visual.debug_bounds("sidebar-global-search").expect("switch");
    visual.simulate_click(switch.center(), Default::default());
    visual.run_until_parked();
    app.read_with(visual, |this, _| assert!(!this.navigator.preferences().local_search()));
    let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert!(!saved.local_search(), "persisted to the preferences file");

    // Regex mode disables the switch: clicking it changes nothing.
    let regex = visual.debug_bounds("sidebar-regex").unwrap();
    visual.simulate_click(regex.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let switch = visual.debug_bounds("sidebar-global-search").unwrap();
    visual.simulate_click(switch.center(), Default::default());
    visual.run_until_parked();
    app.read_with(visual, |this, _| assert!(!this.navigator.preferences().local_search()));
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn connected_only_button_toggles_and_shows_the_green_dot_when_live(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(visual.debug_bounds("sidebar-connected-dot").is_none(), "ring while nothing is connected");
    connect_postgres_fixture(&app, visual);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(visual.debug_bounds("sidebar-connected-dot").is_some(), "green dot while connected");

    let button = visual.debug_bounds("sidebar-connected-only").unwrap();
    assert_eq!((button.size.width, button.size.height), (gpui::px(24.0), gpui::px(24.0)));
    visual.simulate_click(button.center(), Default::default());
    visual.run_until_parked();
    app.read_with(visual, |this, _| {
        assert!(this.navigator.search_options().connected_only);
        let labels: Vec<_> = this.navigator.visible_rows().into_iter().map(|row| row.label).collect();
        assert_eq!(labels, ["Postgres fixture"], "only live connections remain");
    });
}

fn seeded_postgres_tree(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) {
    // Postgres warehouse/public expanded with Tables loaded (Views collapsed).
    app.update(visual, |this, _| this.seed_connection_tree_scenario());
    connect_postgres_fixture(app, visual);
}

fn group_children(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> Vec<String> {
    visible_labels(app, visual)
}

#[gpui::test]
fn remote_search_filters_expanded_groups_server_side_and_restores_them(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    seeded_postgres_tree(&app, visual);
    let unfiltered = group_children(&app, visual);
    assert!(unfiltered.contains(&"orders".to_string()));
    let table_count = unfiltered.len();

    // Local mode (default): typing only filters the loaded tree; nothing reloads.
    search_query(&app, visual, "daily");
    settle_search(visual);
    app.read_with(visual, |this, _| {
        assert!(!this.navigator.has_filtered_groups());
        assert_eq!(this.navigator.loading_count(), 0);
    });
    assert!(!group_children(&app, visual).contains(&"daily_summary".to_string()));

    // Switch off: after the debounce the Views group (still collapsed and
    // unloaded) is loaded with the filter and its match is revealed; the
    // filtered Tables group no longer lists non-matching tables.
    app.update(visual, |this, cx| this.set_sidebar_local_search(false, cx));
    settle_search(visual);
    let labels = group_children(&app, visual);
    assert!(labels.contains(&"daily_summary".to_string()), "{labels:?}");
    assert!(!labels.contains(&"long_view_name_東京".to_string()), "{labels:?}");
    app.read_with(visual, |this, _| {
        assert!(this.navigator.has_filtered_groups());
        assert!(!this.sidebar.remote.loading, "search finished");
        assert_eq!(this.navigator.loading_count(), 0);
    });

    // A query that matches tables narrows the Tables group server-side.
    search_query(&app, visual, "orders");
    settle_search(visual);
    let labels = group_children(&app, visual);
    assert!(labels.contains(&"orders".to_string()));
    assert!(!labels.contains(&"daily_summary".to_string()));

    // Clearing the query restores the unfiltered children (expanded group
    // reloaded, collapsed group unloaded again).
    search_query(&app, visual, "");
    settle_search(visual);
    app.read_with(visual, |this, _| {
        assert!(!this.navigator.has_filtered_groups());
        assert!(this.sidebar.remote.filter.is_none());
        assert!(!this.sidebar.remote.loading);
    });
    assert_eq!(group_children(&app, visual).len(), table_count);
}

#[gpui::test]
fn turning_local_search_back_on_restores_the_tree(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    seeded_postgres_tree(&app, visual);
    let unfiltered = group_children(&app, visual);
    app.update(visual, |this, cx| this.set_sidebar_local_search(false, cx));
    search_query(&app, visual, "orders");
    settle_search(visual);
    app.read_with(visual, |this, _| assert!(this.navigator.has_filtered_groups()));

    app.update(visual, |this, cx| this.set_sidebar_local_search(true, cx));
    settle_search(visual);
    app.read_with(visual, |this, _| assert!(!this.navigator.has_filtered_groups()));
    search_query(&app, visual, "");
    assert_eq!(group_children(&app, visual), unfiltered);
}

#[gpui::test]
fn stale_remote_searches_are_cancelled_by_newer_input_and_regex(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    seeded_postgres_tree(&app, visual);
    app.update(visual, |this, cx| this.set_sidebar_local_search(false, cx));
    let generation = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.sidebar.remote.generation)
    };

    // Two quick edits: the first debounce timer is superseded and starts nothing.
    search_query(&app, visual, "ord");
    let first = generation(&app, visual);
    search_query(&app, visual, "daily");
    assert!(generation(&app, visual) > first);
    app.read_with(visual, |this, _| {
        assert!(!this.sidebar.remote.loading, "still debouncing");
        assert!(!this.navigator.has_filtered_groups());
    });
    settle_search(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.sidebar.remote.filter.as_deref(), Some("daily"), "only the latest query ran");
    });
    assert!(group_children(&app, visual).contains(&"daily_summary".to_string()));

    // Regex on before a new timer fires cancels the pending search and
    // restores the tree (regex is a local projection).
    app.update(visual, |this, cx| {
        let mut options = this.navigator.search_options().clone();
        options.query = "orders".into();
        this.update_sidebar_search(options.clone(), cx);
        options.regex = true;
        this.update_sidebar_search(options, cx);
    });
    settle_search(visual);
    app.read_with(visual, |this, _| {
        assert!(this.sidebar.remote.filter.is_none());
        assert!(!this.navigator.has_filtered_groups(), "restored");
        assert!(!this.sidebar.remote.loading);
    });
}

#[gpui::test]
fn invalid_regex_marks_the_input_without_an_error_block_and_keeps_the_clear_button(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let regex = visual.debug_bounds("sidebar-regex").expect("regex toggle");
    visual.simulate_click(regex.center(), Default::default());
    visual.run_until_parked();

    // Type an invalid pattern into the FOCUSED input.
    let input = app.update_in(visual, |this, window, cx| this.sidebar_search_input(window, cx));
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.focus(window, cx));
    });
    search_query(&app, visual, "(");
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();

    app.read_with(visual, |this, _| {
        // The query is kept (so the clear x has something to clear) and
        // an invalid pattern matches nothing instead of keeping stale rows.
        assert_eq!(this.navigator.search_options().query, "(");
        assert!(this.navigator.visible_rows().is_empty());
        // One concise line, no raw multi-line regex report.
        let message = this.sidebar.search_error.clone().expect("invalid state");
        assert_eq!(message, "Invalid regular expression: unclosed group");
        assert!(!message.contains('\n'));
    });
    assert!(
        visual.debug_bounds("sidebar-search-invalid").is_some(),
        "input carries the danger state"
    );
    // The component's clear x works while the pattern is invalid: one real
    // click empties the input and the query, and the danger state goes away.
    let clear_at = search_clear_point(visual);
    visual.simulate_click(clear_at, Default::default());
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    input.read_with(visual, |state, _| assert_eq!(state.value().as_ref(), ""));
    app.read_with(visual, |this, _| {
        assert!(this.navigator.search_options().query.is_empty());
        assert!(this.sidebar.search_error.is_none(), "empty query is valid again");
    });
    assert!(visual.debug_bounds("sidebar-search-invalid").is_none());
    search_query(&app, visual, "(");
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();

    // Fixing the pattern clears the invalid state and shows matches again.
    search_query(&app, visual, "post");
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(visual.debug_bounds("sidebar-search-invalid").is_none());
    app.read_with(visual, |this, _| {
        assert!(this.sidebar.search_error.is_none());
        assert!(!this.navigator.visible_rows().is_empty());
    });
}

#[gpui::test]
fn local_search_switch_is_visibly_dimmed_only_in_regex_mode(cx: &mut TestAppContext) {
    let (_app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(visual.debug_bounds("sidebar-global-search-dimmed").is_none());
    let regex = visual.debug_bounds("sidebar-regex").unwrap();
    visual.simulate_click(regex.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("sidebar-global-search-dimmed").is_some(),
        "the switch is dimmed with a not-allowed cursor while regex is on"
    );
    let regex = visual.debug_bounds("sidebar-regex").unwrap();
    visual.simulate_click(regex.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(visual.debug_bounds("sidebar-global-search-dimmed").is_none());
}
