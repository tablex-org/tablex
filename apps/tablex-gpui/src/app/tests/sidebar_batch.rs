#[gpui::test]
fn selected_connection_batch_group_move_hide_and_create_persist_once_and_release_selection(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-batch-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let first = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        let second = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        let group = this.create_group_from_selected_connections("Selected", cx).unwrap();
        assert!(!this.navigator.connection_multi_mode());
        assert!(this.navigator.selected_ids().is_empty());
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        assert_eq!(saved.groups.len(), 1);
        assert_eq!(saved.connections.iter().filter(|item| item.group.as_deref() == Some(&group)).count(), 2);
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        this.hide_selected_sidebar_connections(true, cx).unwrap();
        assert_eq!(this.navigator.visible_row_count(), 2, "group plus unselected SQLite");
        assert!(this.navigator.selected_ids().is_empty());
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        assert_eq!(saved.connections.iter().filter(|item| item.hidden).count(), 2);
        this.navigator.set_preferences(Default::default()).unwrap();
        this.reload_sidebar_preferences(cx).unwrap();
        assert_eq!(this.navigator.visible_row_count(), 2, "restart reload keeps the two connections hidden");
        this.update_sidebar_search(crate::navigator::SearchOptions { show_hidden: true, ..Default::default() }, cx);
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        this.move_selected_sidebar_connections(None, cx).unwrap();
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        let mut moved: Vec<_> = saved.connections.iter().filter(|item| item.id == crate::ui_fixture::MONGO_ID || item.id == crate::ui_fixture::POSTGRES_ID).collect();
        moved.sort_by_key(|item| item.order);
        assert_eq!(moved.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), [crate::ui_fixture::MONGO_ID, crate::ui_fixture::POSTGRES_ID]);
        assert!(moved.iter().all(|item| item.group.is_none()));
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        this.hide_selected_sidebar_connections(false, cx).unwrap();
        this.update_sidebar_search(Default::default(), cx);
        assert_eq!(this.navigator.visible_row_count(), 4, "all connections are restored alongside the empty group");
        assert_eq!(crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().connections.iter().filter(|item| item.hidden).count(), 0);
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn moving_expanded_connections_to_ungrouped_preserves_cached_tree_and_load_state(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-expanded-move-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Stored", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&group), 0, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, Some(&group), 1, cx).unwrap();

        let connection = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        let database = NavigatorNodeId::Database { connection: crate::ui_fixture::MONGO_ID.into(), database: "fixture_db".into() };
        let schema = NavigatorNodeId::Schema { connection: crate::ui_fixture::MONGO_ID.into(), database: "fixture_db".into(), schema: "public".into() };
        let outcome = this.navigator.toggle_expansion(&connection);
        this.navigator.apply_load(&outcome.request.unwrap(), this.navigator.generation(), Ok(vec![crate::navigator::database_child(crate::ui_fixture::MONGO_ID, "fixture_db")]));
        let outcome = this.navigator.toggle_expansion(&database);
        this.navigator.apply_load(&outcome.request.unwrap(), this.navigator.generation(), Ok(vec![crate::navigator::schema_child(crate::ui_fixture::MONGO_ID, "fixture_db", "public")]));
        this.navigator.toggle_expansion(&schema);

        let before: Vec<_> = this.navigator.visible_rows().into_iter().filter(|row| matches!(&row.id,
            NavigatorNodeId::Connection { connection: id } if id == crate::ui_fixture::MONGO_ID
        ) || matches!(&row.id,
            NavigatorNodeId::Database { connection: id, .. }
            | NavigatorNodeId::Schema { connection: id, .. }
            | NavigatorNodeId::ObjectGroup { connection: id, .. } if id == crate::ui_fixture::MONGO_ID
        )).map(|row| (row.id, row.expanded, row.load)).collect();
        assert!(before.iter().any(|(id, expanded, _)| id == &schema && *expanded));

        let first = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        let second = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        let snapshot = this.selected_sidebar_connection_ids().unwrap();
        assert_eq!(snapshot, [crate::ui_fixture::MONGO_ID, crate::ui_fixture::POSTGRES_ID]);
        assert_eq!(this.move_selected_sidebar_connections(None, cx).unwrap(), 2);

        let after: Vec<_> = this.navigator.visible_rows().into_iter().filter(|row| matches!(&row.id,
            NavigatorNodeId::Connection { connection: id } if id == crate::ui_fixture::MONGO_ID
        ) || matches!(&row.id,
            NavigatorNodeId::Database { connection: id, .. }
            | NavigatorNodeId::Schema { connection: id, .. }
            | NavigatorNodeId::ObjectGroup { connection: id, .. } if id == crate::ui_fixture::MONGO_ID
        )).map(|row| (row.id, row.expanded, row.load)).collect();
        assert_eq!(after, before, "moving a connection must not reset its expanded, loaded descendants");
        assert!(!this.navigator.connection_multi_mode(), "a completed batch move releases the selection");
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        assert!(saved.connections.iter().filter(|item| snapshot.contains(&item.id)).all(|item| item.group.is_none()));
        let mut moved: Vec<_> = saved.connections.iter().filter(|item| snapshot.contains(&item.id)).collect();
        moved.sort_by_key(|item| item.order);
        assert_eq!(moved.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), snapshot.iter().map(String::as_str).collect::<Vec<_>>());
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn batch_delete_rechecks_exact_selection_and_never_cleans_credentials_after_storage_failure(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-batch-delete-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Stored", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&group), 0, cx).unwrap();
        let first = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        let second = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        let snapshot = this.selected_sidebar_connection_ids().unwrap();
        let mut cleanup_count = 0;
        assert!(this.delete_selected_connections_with(&snapshot, |_| Err("disk failure".into()), |_| { cleanup_count += 1; Ok(()) }, cx).is_err());
        assert_eq!(cleanup_count, 0);
        assert_eq!(this.saved_connections.len(), 3);
        assert_eq!(this.navigator.selected_connection_count(), 2);
        this.navigator.select_visible(&second, false, false);
        let mut storage_called = false;
        assert!(this.delete_selected_connections_with(&snapshot, |_| { storage_called = true; Ok(()) }, |_| Ok(()), cx).is_err());
        assert!(!storage_called, "stale confirmation must not delete another selection");
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        let mut cleaned = Vec::new();
        let deleted = this.delete_selected_connections_with(&snapshot, |ids| { assert_eq!(ids.len(), 2); Ok(()) }, |id| { cleaned.push(id.to_string()); if cleaned.len() == 1 { Err("keychain locked".into()) } else { Ok(()) } }, cx).unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(cleaned.len(), 2);
        assert_eq!(this.saved_connections.len(), 1);
        assert_eq!(this.navigator.selected_connection_count(), 0);
        assert!(this.sidebar.notice.as_deref().is_some_and(|text| text.contains("keychain locked")));
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        assert_eq!(saved.groups.iter().map(|group| group.name.as_str()).collect::<Vec<_>>(), ["Stored"], "deleting saved connections must not delete groups or database data");
        assert!(saved.connections.iter().all(|item| item.id != crate::ui_fixture::MONGO_ID && item.id != crate::ui_fixture::POSTGRES_ID));
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn async_delete_completion_reconciles_committed_snapshot_without_clearing_new_selection(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-async-delete-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Stored", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&group), 0, cx).unwrap();
        let snapshot = vec![crate::ui_fixture::MONGO_ID.into(), crate::ui_fixture::POSTGRES_ID.into()];
        let retained = NavigatorNodeId::Connection { connection: crate::ui_fixture::SQLITE_ID.into() };
        this.navigator.select_visible(&retained, false, false);
        this.apply_deleted_connections_after_commit(&snapshot, Vec::new(), cx);
        assert_eq!(this.navigator.selected(), Some(&retained));
        assert_eq!(this.saved_connections.len(), 1);
        assert!(crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().connections.iter().all(|item| item.id != crate::ui_fixture::MONGO_ID));
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn mixed_selected_nodes_never_trigger_connection_batch_actions(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        let mut prefs = crate::sidebar_preferences::SidebarPreferences::default();
        prefs.add_group("work", "Work", None).unwrap();
        this.navigator.set_preferences(prefs).unwrap();
        this.navigator.select_visible(&NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() }, false, false);
        this.navigator.select_visible(&NavigatorNodeId::Group { id: "work".into() }, true, false);
        assert!(!this.navigator.connection_multi_mode());
        assert!(this.selected_sidebar_connection_ids().is_err());
        let mut storage_called = false;
        assert!(this.delete_selected_connections_with(&[crate::ui_fixture::MONGO_ID.into()], |_| { storage_called = true; Ok(()) }, |_| Ok(()), cx).is_err());
        assert!(!storage_called);
    });
}

#[gpui::test]
fn committed_batch_delete_reconciles_memory_and_warns_when_preferences_cannot_save(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-reconcile-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Stored", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&group), 0, cx).unwrap();
        let original = std::fs::read(&path).unwrap();
        this.sidebar.path = Some(path.join("cannot-write"));
        let id = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        this.navigator.select_visible(&id, true, false);
        let snapshot = this.selected_sidebar_connection_ids().unwrap();
        this.delete_selected_connections_with(&snapshot, |_| Ok(()), |_| Ok(()), cx).unwrap();
        assert!(this.navigator.preferences().connections.iter().all(|entry| entry.id != crate::ui_fixture::MONGO_ID));
        assert!(this.sidebar.notice.as_deref().is_some_and(|notice| notice.contains("not saved")));
        assert_eq!(std::fs::read(&path).unwrap(), original, "failed save must preserve old file");
        this.sidebar.path = Some(path.clone());
        this.create_sidebar_group("Next mutation", None, cx).unwrap();
        assert!(crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().connections.iter().all(|entry| entry.id != crate::ui_fixture::MONGO_ID));
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn verified_connection_refresh_reconciles_stale_placement_from_previous_partial_commit(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-stale-reconcile-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let deleted_id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        this.change_sidebar_preferences(|prefs| prefs.move_connection(deleted_id, None, 0), cx).unwrap();
        assert!(crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().connections.iter().any(|item| item.id == deleted_id));
        this.reconcile_sidebar_placements(cx);
        assert!(crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().connections.iter().all(|item| item.id != deleted_id));
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn selected_connection_config_copy_contains_all_targets_and_no_credentials(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        for (index, id) in [crate::ui_fixture::MONGO_ID, crate::ui_fixture::POSTGRES_ID].into_iter().enumerate() {
            this.navigator.select_visible(&NavigatorNodeId::Connection { connection: id.into() }, index != 0, false);
        }
        let selected = this.selected_sidebar_connection_ids().unwrap();
        this.copy_selected_sidebar_configs_with(&selected, |id| Ok(engine_storage::connection::SavedConnectionRecord {
            id: id.into(), name: format!("Config {id}"), uri: "postgresql://user:secret@localhost/app?accessToken=secret".into(),
            db_type: crate::connections::DB_TYPE_POSTGRESQL.into(), color_hex: "#123ABC".into(), save_password: true,
            save_proxy_password: true, network_settings_json: r#"{"proxy":{"password":"proxy-secret"}}"#.into(),
            is_pinned: false, last_connected: None, created_at: 1.0, updated_at: 1.0,
            connection_type: String::new(), overrides_json: String::new(),
        }), cx).unwrap();
        let text = cx.read_from_clipboard().and_then(|item| item.text()).unwrap();
        assert!(!text.contains("secret"));
        let imported = crate::connections::stage_connection_configs(text.as_bytes(), 2.0).unwrap();
        assert_eq!(imported.len(), 2);
        let mut prefs = crate::sidebar_preferences::SidebarPreferences::default();
        prefs.add_group("work", "Work", None).unwrap();
        this.navigator.set_preferences(prefs).unwrap();
        this.navigator.select_visible(&NavigatorNodeId::Group { id: "work".into() }, true, false);
        assert!(this.copy_selected_sidebar_configs_with(&selected, |_| panic!("mixed selection must reject before fetch"), cx).is_err());
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).unwrap(), text);
    });
}

#[gpui::test]
fn batch_drag_moves_selected_connections_before_unplaced_target_in_stable_order(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-multidrag-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let a = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
        let b = NavigatorNodeId::Connection { connection: crate::ui_fixture::SQLITE_ID.into() };
        this.navigator.select_visible(&a, false, false);
        this.navigator.select_visible(&b, true, false);
        let drag = super::navigator_ui::SidebarDrag { id: a.clone(), ids: Some(std::sync::Arc::from(vec![a, b].into_boxed_slice())) };
        let target = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        this.drop_sidebar_drag(&drag, &target, cx);
        let labels: Vec<_> = this.navigator.visible_rows().iter().take(3).map(|row| row.label.clone()).collect();
        assert_eq!(labels, ["Postgres fixture", "SQLite fixture", "Mongo fixture"]);
        assert!(!this.navigator.connection_multi_mode());
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        assert_eq!(saved.connections.iter().filter(|item| item.group.is_none()).count(), 3);
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn batch_drag_of_group_and_descendant_moves_only_the_group_and_rejects_stale_payload(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-groupdrag-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Parent", None, cx).unwrap();
        let destination = this.create_sidebar_group("Destination", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&group), 0, cx).unwrap();
        let root = NavigatorNodeId::Group { id: group.clone() };
        let child = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
        this.navigator.select_visible(&root, false, false);
        this.navigator.select_visible(&child, true, false);
        let drag = super::navigator_ui::SidebarDrag { id: root.clone(), ids: Some(std::sync::Arc::from(vec![root.clone(), child].into_boxed_slice())) };
        let target = NavigatorNodeId::Group { id: destination.clone() };
        this.drop_sidebar_drag(&drag, &target, cx);
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        assert_eq!(saved.groups.iter().find(|item| item.id == group).unwrap().parent.as_deref(), Some(destination.as_str()));
        assert_eq!(saved.connections.iter().find(|item| item.id == crate::ui_fixture::MONGO_ID).unwrap().group.as_deref(), Some(group.as_str()));
        this.navigator.select_visible(&root, false, false);
        let original = std::fs::read(&path).unwrap();
        this.drop_sidebar_drag(&drag, &NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() }, cx);
        assert_eq!(std::fs::read(&path).unwrap(), original, "stale multi payload cannot move nodes");
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn batch_drag_two_groups_reparents_together_in_visual_order(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-group-multidrag-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let a = this.create_sidebar_group("A", None, cx).unwrap();
        let b = this.create_sidebar_group("B", None, cx).unwrap();
        let target = this.create_sidebar_group("Target", None, cx).unwrap();
        let first = NavigatorNodeId::Group { id: a.clone() };
        let second = NavigatorNodeId::Group { id: b.clone() };
        this.navigator.select_visible(&first, false, false);
        this.navigator.select_visible(&second, true, false);
        let drag = super::navigator_ui::SidebarDrag { id: first.clone(), ids: Some(std::sync::Arc::from(vec![first, second].into_boxed_slice())) };
        this.drop_sidebar_drag(&drag, &NavigatorNodeId::Group { id: target.clone() }, cx);
        let mut moved: Vec<_> = this.navigator.preferences().groups.iter().filter(|group| group.parent.as_deref() == Some(&target)).collect();
        moved.sort_by_key(|group| group.order);
        assert_eq!(moved.iter().map(|group| group.name.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().groups.len(), 3);
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn selected_connection_context_menu_hides_entire_explicit_selection(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-batch-menu-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, _| { this.sidebar.path = Some(path.clone()); });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let first = visual.debug_bounds("sidebar-row-0").expect("first connection row");
    let third = visual.debug_bounds("sidebar-row-2").expect("third connection row");
    let modifier = visual.update(|_, cx| gpui::Modifiers::secondary_key(cx));
    visual.simulate_click(first.center(), modifier);
    visual.simulate_click(third.center(), modifier);
    visual.run_until_parked();
    app.update(visual, |this, _| assert_eq!(this.navigator.selected_connection_count(), 2));
    visual.simulate_mouse_down(first.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(first.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    // The menu opens on "N selected connections", a label the keys skip.
    for _ in 0..3 { visual.simulate_keystrokes("down"); }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let saved = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
        assert!(saved.connections.iter().any(|item| item.id == crate::ui_fixture::MONGO_ID && item.hidden), "prefs={:?} notice={:?} selection={:?}", saved.connections, this.sidebar.notice, this.navigator.selected_ids());
        assert!(saved.connections.iter().any(|item| item.id == crate::ui_fixture::SQLITE_ID && item.hidden));
        assert!(!this.navigator.connection_multi_mode());
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn selecting_group_connections_cascades_nested_members_and_respects_visible_filter(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-group-select-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let parent = this.create_sidebar_group("Parent", None, cx).unwrap();
        let child = this.create_sidebar_group("Child", Some(&parent), cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&child), 0, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, Some(&parent), 1, cx).unwrap();
        let row = this.navigator.visible_row_at(this.navigator.visible_index(&NavigatorNodeId::Group { id: parent.clone() }).unwrap()).unwrap();
        this.handle_navigator_disclosure(&row, cx);
        assert_eq!(this.navigator.visible_row_count(), 2, "collapsed parent and outside SQLite");
        assert_eq!(this.toggle_sidebar_group_connections(&parent, cx).unwrap(), 2);
        assert_eq!(this.selected_sidebar_connection_ids().unwrap(), [crate::ui_fixture::MONGO_ID, crate::ui_fixture::POSTGRES_ID]);
        assert_eq!(this.toggle_sidebar_group_connections(&parent, cx).unwrap(), 0);
        this.update_sidebar_search(crate::navigator::SearchOptions { query: "Mongo".into(), ..Default::default() }, cx);
        assert_eq!(this.toggle_sidebar_group_connections(&parent, cx).unwrap(), 1);
        assert_eq!(this.selected_sidebar_connection_ids().unwrap(), [crate::ui_fixture::MONGO_ID]);
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn group_context_menu_selects_nested_connections_from_collapsed_group(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-group-menu-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        let group = this.create_sidebar_group("Selected", None, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::MONGO_ID, Some(&group), 0, cx).unwrap();
        this.move_sidebar_connection(crate::ui_fixture::POSTGRES_ID, Some(&group), 1, cx).unwrap();
        let row = this.navigator.visible_row_at(0).unwrap();
        this.handle_navigator_disclosure(&row, cx);
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let group = visual.debug_bounds("sidebar-row-0").expect("collapsed group row");
    visual.simulate_mouse_down(group.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(group.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    for _ in 0..2 { visual.simulate_keystrokes("down"); }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.navigator.selected_connection_count(), 2);
        assert!(this.navigator.connection_multi_mode());
        assert!(this.navigator.visible_row_at(0).unwrap().expanded);
    });
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn selected_delete_menu_opens_confirmation_without_deleting_before_acceptance(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let first = visual.debug_bounds("sidebar-row-0").unwrap();
    let third = visual.debug_bounds("sidebar-row-2").unwrap();
    let modifier = visual.update(|_, cx| gpui::Modifiers::secondary_key(cx));
    visual.simulate_click(first.center(), modifier);
    visual.simulate_click(third.center(), modifier);
    visual.simulate_mouse_down(first.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(first.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    // The menu opens on "N selected connections", a label the keys skip.
    for _ in 0..4 { visual.simulate_keystrokes("down"); }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    assert!(visual.debug_bounds("dialog-layer").is_some(), "destructive action requires confirmation");
    app.update(visual, |this, _| {
        assert_eq!(this.saved_connections.len(), 3);
        assert_eq!(this.navigator.selected_connection_count(), 2);
    });
}
