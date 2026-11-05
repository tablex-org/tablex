#[gpui::test]
fn invalid_null_edit_has_visible_feedback(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "mongo-grid",
        "--ui-fixture",
        "sample",
    ])
    .unwrap()
    .unwrap();
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let cell = visual.debug_bounds("grid-body-cell-0-2").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: cell.center(),
        modifiers: Default::default(),
        click_count: 2,
        first_mouse: false,
    });
    visual.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: cell.center(),
        modifiers: Default::default(),
        click_count: 2,
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("present");
    let mut enter = gpui::Keystroke::parse("enter").unwrap();
    enter.key_char = None;
    visual.simulate_event(gpui::KeyDownEvent {
        keystroke: enter,
        is_held: false,
        prefer_character_input: false,
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("result-status-error").is_some(),
        "invalid Enter needs visible error feedback"
    );
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert!(this
            .workspace
            .runtime(tab.id)
            .unwrap()
            .mongo
            .mutations
            .is_empty());
    });
}

#[gpui::test]
fn staged_delete_has_row_marker_and_tracks_undo_redo(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "mongo-grid",
        "--ui-fixture",
        "sample",
    ])
    .unwrap()
    .unwrap();
    let (_root, visual) = cx.add_window_view(|window, cx| {
        let app =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let cell = visual.debug_bounds("grid-body-cell-0-1").unwrap();
    visual.simulate_click(cell.center(), Default::default());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let delete = visual.debug_bounds("mongo-delete").unwrap();
    visual.simulate_click(delete.center(), Default::default());
    visual.run_until_parked();
    let confirm = visual.debug_bounds("mongo-delete-confirm").unwrap();
    visual.simulate_click(confirm.center(), Default::default());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual
            .debug_bounds("pending-delete-row-0")
            .unwrap()
            .size
            .width
            > px(0.0),
        "pending deletion must mark its row"
    );
    let undo = visual.debug_bounds("mongo-undo").unwrap();
    visual.simulate_click(undo.center(), Default::default());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert_eq!(
        visual
            .debug_bounds("pending-delete-row-0")
            .unwrap()
            .size
            .width,
        px(0.0)
    );
    let redo = visual.debug_bounds("mongo-redo").unwrap();
    visual.simulate_click(redo.center(), Default::default());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual
            .debug_bounds("pending-delete-row-0")
            .unwrap()
            .size
            .width
            > px(0.0)
    );
}

#[gpui::test]
fn mongo_fixture_double_click_opens_real_cell_editor(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "mongo-grid",
        "--ui-fixture",
        "sample",
    ])
    .unwrap()
    .unwrap();
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let cell = visual
        .debug_bounds("grid-body-cell-0-1")
        .expect("BSON name cell");
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: cell.center(),
        modifiers: Default::default(),
        click_count: 2,
        first_mouse: false,
    });
    visual.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: cell.center(),
        modifiers: Default::default(),
        click_count: 2,
    });
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        let edit = this
            .av().grid_ui
            .inline_edit
            .as_ref()
            .expect("native fixture uses the real inline editor");
        assert_eq!(edit.editor.value(cx).as_str(), "mongo-customer-000");
        let nested = (0..this.active_grid().column_count())
            .find(|column| this.active_grid().column_name(*column) == Some("nested"))
            .unwrap();
        let detail = this
            .build_cell_detail(
                this.workspace.active_tab().unwrap(),
                CellPosition {
                    row: 0,
                    column: nested,
                },
            )
            .unwrap();
        let value = engine_mongo_script::shell_value::parse_document(
            &detail.value,
            engine_mongo_script::shell_value::NumberTyping::Mongosh,
        )
        .expect("nested BSON detail is the full document in shell syntax, not a preview placeholder");
        assert_eq!(value.get_bool("ok"), Ok(true));
        // The types survive the text: an Int64 is still an Int64, an Int32 an Int32.
        assert_eq!(value.get("index"), Some(&bson::Bson::Int64(0)));
        assert_eq!(
            value.get_document("owner").unwrap().get("tier"),
            Some(&bson::Bson::Int32(2))
        );
    });
}

#[gpui::test]
fn mongo_duplicate_document_editor_has_multiline_space(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "mongo-grid",
        "--ui-fixture",
        "sample",
    ])
    .unwrap()
    .unwrap();
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let cell = visual.debug_bounds("grid-body-cell-0-1").unwrap();
    visual.simulate_click(cell.center(), Default::default());
    app.update_in(visual, |this, window, cx| {
        this.open_mongo_duplicate_row_editor(this.active_id(), window, cx)
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let editor = visual
        .debug_bounds("document-editor")
        .expect("document editor is visible");
    assert!(editor.size.height >= px(200.0));
    assert!(editor.size.width >= px(400.0));
}

#[test]
fn mongo_columns_button_label_counts_visible_columns() {
    let known_columns = vec!["_id".into(), "name".into()];
    let visible_columns = HashSet::from(["_id".into()]);

    assert_eq!(
        super::mongo_columns_button_label(&visible_columns, &known_columns),
        "Columns 1/2"
    );
}

#[test]
fn mongo_filter_element_ids_are_unique_per_tab() {
    assert_ne!(
        mongo_filter_element_id("mongo-filter-field", 1, 7),
        mongo_filter_element_id("mongo-filter-field", 2, 7),
    );
}

#[gpui::test]
fn mongo_filter_field_selection_synchronizes_cached_editor(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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

    let (tab_id, editor) = app.update_in(cx, |this, window, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.mongo.add_filter_rule("old");
        runtime
            .mongo
            .toggle_filter_menu(crate::mongo_result::MongoFilterMenu::Field(1));
        let editor = this.mongo_filter_rule_editor(tab_id, 1, "old".into(), true, window, cx);
        this.select_mongo_filter_field(tab_id, 1, "new".into(), window, cx);
        (tab_id, editor)
    });

    assert_eq!(
        editor.read_with(cx, |state, _| state.value().to_string()),
        "new"
    );
    app.read_with(cx, |this, _| {
        let mongo = &this.workspace.runtime(tab_id).expect("query runtime").mongo;
        assert_eq!(mongo.filter_rules[0].field, "new");
        assert_eq!(mongo.filter_menu, None);
    });
}

#[gpui::test]
fn mongo_filter_rule_cleanup_prevents_stale_editor_reuse(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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

    app.update_in(cx, |this, window, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.mongo.add_filter_rule("old");
        this.mongo_filter_rule_editor(tab_id, 1, "old".into(), true, window, cx);
        this.mongo_filter_rule_editor(tab_id, 1, "stale".into(), false, window, cx);

        this.remove_mongo_filter_rule(tab_id, 1, cx);

        assert!(!this
            .mongo_ui
            .filter_field_editors
            .contains_key(&(tab_id, 1)));
        assert!(!this
            .mongo_ui
            .filter_value_editors
            .contains_key(&(tab_id, 1)));
    });
}

#[test]
fn mongo_page_size_normalizes_persisted_large_limits_without_skipping_documents() {
    for requested in [500, 1000] {
        let mut workspace = crate::workspace::WorkspaceState::new();
        let (tab_id, _) = workspace.open_or_activate_collection_tab("conn", "db", "docs");
        workspace.tab_mut(tab_id).unwrap().output_limit = requested;
        workspace.runtime_mut(tab_id).unwrap().pagination =
            crate::query::PaginationState::new(requested);
        let effective =
            super::results::mongo::query::normalize_mongo_page(&mut workspace, tab_id).unwrap();
        assert_eq!(effective, 200);
        assert_eq!(
            workspace
                .tabs()
                .iter()
                .find(|tab| tab.id == tab_id)
                .unwrap()
                .output_limit,
            200
        );
        let mut ranges = Vec::new();
        loop {
            let page = &mut workspace.runtime_mut(tab_id).unwrap().pagination;
            let start = page.offset() as usize;
            let count = (750 - start).min(effective as usize);
            ranges.extend(start..start + count);
            page.set_page_result_count(count, Some(750), true);
            if !page.next() {
                break;
            }
        }
        assert_eq!(ranges, (0..750).collect::<Vec<_>>());
    }
}

/// A key press ends hover under a still pointer. Upstream gpui-component 0.7.0
/// then cleared the hovered item's highlight, so the pointer resting on the
/// open Limit menu made keyboard confirmation a no-op; 0.7.1 keeps it.
#[gpui::test]
fn limit_menu_keeps_the_hovered_item_selected_when_a_key_press_ends_hover(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.workspace
            .open_or_activate_collection_tab(crate::ui_fixture::MONGO_ID, "db", "docs");
        this.reset_active_view();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let limit = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.update(visual, |this, _| {
            this.workspace
                .active_tab()
                .expect("active tab")
                .output_limit
        })
    };
    assert_eq!(
        limit(&app, visual),
        200,
        "default Limit before the menu opens"
    );

    let control = visual
        .debug_bounds("result-limit")
        .expect("Limit control should render");
    visual.simulate_click(
        gpui::point(control.right() - gpui::px(3.0), control.center().y),
        Default::default(),
    );
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Rest the real pointer on the second menu row ("100"), which opens above
    // the footer control.
    visual.simulate_mouse_move(
        gpui::point(
            control.left() + gpui::px(3.0),
            control.top() - gpui::px(42.0),
        ),
        None,
        Default::default(),
    );
    // A non-navigation key flips the input modality to keyboard and ends the
    // pointer's hover without moving the menu selection.
    visual.simulate_keystrokes("a");
    visual.simulate_keystrokes("enter");
    visual.update(|window, _| window.refresh());
    assert_eq!(
        limit(&app, visual),
        100,
        "Enter must confirm the row the pointer selected"
    );
}

#[gpui::test]
fn mongo_output_limit_control_and_restored_tab_never_exceed_driver_page_size(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        let (tab_id, _) = this.workspace.open_or_activate_collection_tab(
            crate::ui_fixture::MONGO_ID,
            "db",
            "docs",
        );
        this.workspace.tab_mut(tab_id).unwrap().output_limit = 500;
        this.workspace.runtime_mut(tab_id).unwrap().pagination =
            crate::query::PaginationState::new(500);
        this.reset_active_view();
        assert_eq!(this.workspace.active_tab().unwrap().output_limit, 200);
        assert_eq!(
            this.workspace
                .runtime(tab_id)
                .unwrap()
                .pagination
                .page_size(),
            200
        );
        this.set_output_limit(tab_id, 1000, cx);
        assert_eq!(this.workspace.active_tab().unwrap().output_limit, 200);
        assert_eq!(
            this.workspace
                .runtime(tab_id)
                .unwrap()
                .pagination
                .page_size(),
            200
        );
    });
}

#[gpui::test]
fn mongo_filter_reset_cleans_only_target_tab_editors(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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

    app.update_in(cx, |this, window, cx| {
        let first = this.workspace.open_query_tab("First");
        let second = this.workspace.open_query_tab("Second");
        this.mongo_filter_rule_editor(first, 1, "first".into(), true, window, cx);
        this.mongo_filter_rule_editor(second, 1, "second".into(), true, window, cx);

        this.reset_mongo_filter_builder(first, cx);

        assert!(!this.mongo_ui.filter_field_editors.contains_key(&(first, 1)));
        assert!(this
            .mongo_ui
            .filter_field_editors
            .contains_key(&(second, 1)));
    });
}

#[gpui::test]
fn mongo_query_preview_opens_copies_and_closes_with_icons(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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
    app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("trans-his");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .collection = "trans-his".into();
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.query.begin();
        runtime.mongo.filter = r#"{"status":"open"}"#.into();
        runtime.mongo.sort = r#"{"_id":1}"#.into();
        runtime.mongo.project = r#"{"name":1}"#.into();
        cx.notify();
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let trigger = cx
        .debug_bounds("mongo-query-preview")
        .expect("query preview trigger should render");
    cx.simulate_click(trigger.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let panel = cx
        .debug_bounds("mongo-query-panel")
        .expect("query preview panel should render");
    assert_eq!(panel.size.width, gpui::px(520.0));
    assert_eq!(panel.size.height, gpui::px(180.0));
    assert!(cx.debug_bounds("mongo-query-command").is_some());

    let copy = cx
        .debug_bounds("mongo-query-preview-copy")
        .expect("copy icon should render");
    cx.simulate_click(copy.center(), Default::default());
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(r#"db["trans-his"].find({"status":"open"}, {"name":1}).sort({"_id":1})"#.into())
    );
    app.read_with(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        assert!(
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .mongo
                .query_preview_open
        );
    });

    let close = cx
        .debug_bounds("mongo-query-preview-close")
        .expect("close icon should render");
    cx.simulate_click(close.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        assert!(
            !this
                .workspace
                .runtime(tab_id)
                .expect("query runtime")
                .mongo
                .query_preview_open
        );
    });
}

#[gpui::test]
fn mongo_column_popover_matches_compact_layout(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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
    app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.query.begin();
        runtime.mongo.known_columns = vec!["_id".into(), "name".into()];
        runtime.mongo.visible_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_draft_columns = runtime.mongo.projection_columns.clone();
        runtime.mongo.column_open = true;
        cx.notify();
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let panel = cx
        .debug_bounds("mongo-column-panel")
        .expect("column popover should render");
    assert_eq!(panel.size.width, gpui::px(320.0));
    for selector in [
        "mongo-column-header",
        "mongo-column-tabs",
        "mongo-column-search",
        "mongo-column-list",
        "mongo-column-footer",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} should render inside the column popover"
        );
    }
    assert_eq!(
        cx.debug_bounds("mongo-column-search")
            .expect("column search should render")
            .size
            .height,
        gpui::px(28.0)
    );
    assert_eq!(
        cx.debug_bounds("mongo-column-row-0")
            .expect("column row should render")
            .size
            .height,
        gpui::px(28.0)
    );
}

#[gpui::test]
fn mongo_column_tabs_preserve_visible_and_projection_state(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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
    let tab_id = app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.query.begin();
        runtime.mongo.known_columns = vec!["_id".into(), "name".into()];
        runtime.mongo.visible_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_draft_columns = runtime.mongo.projection_columns.clone();
        runtime.mongo.column_open = true;
        cx.notify();
        tab_id
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let projection_tab = cx
        .debug_bounds("mongo-column-tab-projection")
        .expect("projection tab should render");
    cx.simulate_click(projection_tab.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        assert_eq!(
            runtime.mongo.column_tab,
            crate::mongo_result::MongoColumnTab::Projection
        );
        assert!(runtime.mongo.projection_draft_columns.contains("name"));
        assert!(runtime.mongo.visible_columns.contains("name"));
    });

    let name_row = cx
        .debug_bounds("mongo-column-row-1")
        .expect("name row should render");
    cx.simulate_click(name_row.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        assert!(!runtime.mongo.projection_draft_columns.contains("name"));
        assert!(runtime.mongo.visible_columns.contains("name"));
        assert!(runtime.mongo.project.is_empty());
    });

    let visible_tab = cx
        .debug_bounds("mongo-column-tab-visible")
        .expect("visible tab should render");
    cx.simulate_click(visible_tab.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        assert_eq!(
            runtime.mongo.column_tab,
            crate::mongo_result::MongoColumnTab::Visible
        );
        assert!(runtime.mongo.visible_columns.contains("name"));
        assert!(!runtime.mongo.projection_draft_columns.contains("name"));
    });

    let name_checkbox = cx
        .debug_bounds("mongo-column-checkbox-1")
        .expect("name checkbox should render");
    cx.simulate_click(name_checkbox.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        assert!(!this
            .workspace
            .runtime(tab_id)
            .expect("query runtime")
            .mongo
            .visible_columns
            .contains("name"));
    });
}

#[gpui::test]
fn show_all_mongo_columns_uses_current_result_columns(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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
    let tab_id = app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
            arrow_schema::Field::new("_id", arrow_schema::DataType::Utf8, false),
            arrow_schema::Field::new("name", arrow_schema::DataType::Utf8, false),
        ]));
        let batch = arrow_array::RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(arrow_array::StringArray::from(vec!["id"]))
                    as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::StringArray::from(vec!["Ada"]))
                    as arrow_array::ArrayRef,
            ],
        )
        .expect("valid test batch");
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        runtime.mongo.known_columns = vec!["_id".into(), "name".into()];
        runtime.mongo.visible_columns = HashSet::from(["_id".into()]);
        runtime.mongo.projection_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_draft_columns = HashSet::from(["_id".into()]);
        this.show_all_mongo_columns(tab_id, cx);
        tab_id
    });

    let visible_columns = cx.cx.read(|app_context| {
        app.read(app_context)
            .workspace
            .runtime(tab_id)
            .expect("query runtime")
            .mongo
            .visible_columns
            .clone()
    });
    assert_eq!(
        visible_columns,
        HashSet::from(["_id".into(), "name".into()])
    );
}

#[gpui::test]
fn mongo_toolbar_controls_fit_inside_toolbar(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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
    app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.query.begin();
        runtime.mongo.known_columns = vec!["_id".into(), "name".into()];
        runtime.mongo.visible_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_draft_columns = runtime.mongo.projection_columns.clone();
        cx.notify();
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let toolbar = cx
        .debug_bounds("mongo-result-toolbar")
        .expect("Mongo toolbar should render");
    for selector in [
        "mongo-view-switch",
        "mongo-query-preview",
        "mongo-query-capsule",
        "mongo-filter-label",
        "mongo-filter-field",
        "mongo-filter-sort-handle",
        "mongo-sort-field",
        "mongo-sort-expand",
        "mongo-columns",
        "mongo-columns-button",
        "mongo-find-expand",
        "mongo-run",
    ] {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} should render"));
        assert!(
            bounds.top() >= toolbar.top() + gpui::px(2.0),
            "{selector} should not touch the toolbar top edge"
        );
        assert!(
            bounds.bottom() <= toolbar.bottom() - gpui::px(2.0),
            "{selector} should not touch the toolbar bottom edge"
        );
        let max_height = if selector == "mongo-query-capsule" {
            gpui::px(32.0)
        } else {
            gpui::px(26.0)
        };
        assert!(
            bounds.size.height <= max_height,
            "{selector} should use the compact toolbar height"
        );
        assert!(
            bounds.left() >= toolbar.left(),
            "{selector} should stay inside the toolbar horizontally"
        );
        assert!(
            bounds.right() <= toolbar.right(),
            "{selector} should stay inside the toolbar horizontally"
        );
        if selector == "mongo-filter-field" || selector == "mongo-sort-field" {
            assert!(
                bounds.size.width >= gpui::px(120.0),
                "{selector} should keep the minimum field width"
            );
        }
        if selector == "mongo-columns-button" {
            assert!(
                bounds.size.width >= gpui::px(70.0),
                "Columns label should have enough width for its count"
            );
        }
    }
    assert!(
        cx.debug_bounds("mongo-sort-clear").is_some(),
        "sort clear should render for the default non-empty sort"
    );
    let capsule = cx
        .debug_bounds("mongo-query-capsule")
        .expect("Mongo query capsule should render");
    let columns = cx
        .debug_bounds("mongo-columns")
        .expect("Mongo columns control should render");
    let footer = cx
        .debug_bounds("result-action-footer")
        .expect("unified result footer should render");
    assert!(
        footer.top() >= toolbar.bottom(),
        "footer should be below the result toolbar"
    );
    assert_eq!(footer.size.height, px(54.0));
    let action_strip = cx
        .debug_bounds("result-action-strip")
        .expect("action strip should render");
    let status_row = cx
        .debug_bounds("result-status-row")
        .expect("status row should render");
    assert_eq!(action_strip.size.height, px(32.0));
    assert_eq!(status_row.size.height, px(22.0));
    assert_eq!(status_row.top(), action_strip.bottom());
    for selector in [
        "result-refresh",
        "result-status-summary",
        "result-status-auto",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} should render in the unified result footer"
        );
    }
    assert!(
        cx.debug_bounds("result-more").is_none(),
        "the result footer must not contain a More control"
    );
    assert!(
        cx.debug_bounds("mongo-commit").is_some(),
        "Mongo commit control should render"
    );
    assert!(
        columns.left() >= capsule.right(),
        "Columns control should sit outside the query capsule"
    );
    assert!(
        columns.right() <= toolbar.right(),
        "Columns control should stay inside the top toolbar"
    );
    for selector in ["mongo-filter-toolbar-text", "mongo-query-toolbar-text"] {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} should render"));
        assert!(
            bounds.size.height <= gpui::px(14.0),
            "{selector} should use SORT-sized typography: {:?}",
            bounds.size.height
        );
    }

    cx.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(800.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    for selector in [
        "result-limit",
        "result-export",
        "mongo-add-row",
        "edit-cell",
        "mongo-commit",
        "mongo-rollback",
        "mongo-delete",
        "mongo-undo",
        "mongo-redo",
        "page-previous",
        "page-next",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} should render in the unified result footer"
        );
    }
    let group_order = [
        "result-group-refresh",
        "result-group-commit",
        "result-group-row",
        "result-group-history",
        "result-group-pagination",
        "result-group-output",
    ]
    .into_iter()
    .map(|selector| {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} should render"))
    })
    .collect::<Vec<_>>();
    for pair in group_order.windows(2) {
        assert!(
            pair[0].right() <= pair[1].left(),
            "footer groups must remain in the approved order"
        );
    }
    assert!(
        cx.debug_bounds("result-status-error").is_none(),
        "idle Mongo results should not show an error in the status row"
    );
    let limit_control = cx
        .debug_bounds("result-limit")
        .expect("Limit control should render");
    cx.simulate_click(
        gpui::point(
            limit_control.right() - gpui::px(3.0),
            limit_control.center().y,
        ),
        Default::default(),
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    cx.update(|window, _| window.refresh());
    let mut limit = 0;
    app.update(cx, |this, _| {
        limit = this
            .workspace
            .active_tab()
            .expect("active tab")
            .output_limit;
    });
    assert_eq!(limit, 100, "Limit should select the requested output limit");
    cx.simulate_keystrokes("escape");
    // Default sidebar is 260px (was 280px), so
    // the window must be 20px narrower to keep the same constrained main
    // width that forces the footer overflow.
    cx.simulate_resize(gpui::size(gpui::px(880.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let compact_footer = cx
        .debug_bounds("result-action-footer")
        .expect("compact footer should render");
    let compact_limit = cx
        .debug_bounds("result-limit")
        .expect("compact Limit control should render");
    assert!(
        compact_limit.right() > compact_footer.right(),
        "compact output controls should extend into the scrollable action strip"
    );

    app.update(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        this.workspace
            .runtime_mut(tab_id)
            .expect("query runtime")
            .auto_refresh_seconds = Some(5);
    });
    let refresh = cx
        .debug_bounds("result-refresh-dropdown")
        .expect("Refresh control should render");
    cx.simulate_mouse_down(
        gpui::point(refresh.right() - gpui::px(3.0), refresh.center().y),
        gpui::MouseButton::Left,
        Default::default(),
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    let mut auto_refresh = Some(5);
    app.update(cx, |this, _| {
        auto_refresh = this
            .workspace
            .active_tab()
            .and_then(|tab| this.workspace.auto_refresh_seconds(tab.id));
    });
    assert_eq!(
        auto_refresh, None,
        "Refresh menu Stop should clear the cadence"
    );

    cx.simulate_resize(gpui::size(gpui::px(500.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let tiny_footer = cx
        .debug_bounds("result-action-footer")
        .expect("tiny footer should render");
    let tiny_strip = cx
        .debug_bounds("result-action-strip")
        .expect("tiny action strip should render");
    let action_content = cx
        .debug_bounds("result-action-content")
        .expect("action content should render");
    let max_offset = app.update(cx, |this, _| this.av().result_action_scroll.max_offset());
    assert!(
        f32::from(max_offset.x) > 0.0,
        "narrow action strip should expose horizontal scrolling"
    );
    assert!(
        action_content.right() > tiny_strip.right(),
        "narrow action content should not hide controls"
    );
    assert_eq!(
        app.update(cx, |this, _| this.av().result_action_scroll.offset().x),
        px(0.0)
    );
    cx.simulate_event(ScrollEvent {
        position: tiny_strip.center(),
        delta: ScrollDelta::Pixels(Point::new(gpui::px(-1000.0), gpui::px(0.0))),
        touch_phase: gpui::TouchPhase::Moved,
        modifiers: Default::default(),
        source: gpui::ScrollSource::Pointer,
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let scrolled_offset = app.update(cx, |this, _| this.av().result_action_scroll.offset().x);
    assert!(
        f32::from(scrolled_offset) < 0.0,
        "narrow action strip should move horizontally when scrolled"
    );
    for selector in ["result-export", "result-limit"] {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} should remain reachable by scrolling"));
        assert!(
            bounds.left() >= tiny_strip.left() && bounds.right() <= tiny_strip.right(),
            "{selector} should be fully reachable after scrolling: {bounds:?}, strip {tiny_strip:?}, offset {scrolled_offset:?}, max {max_offset:?}"
        );
    }
    assert!(
        tiny_footer.bottom() >= cx.debug_bounds("result-status-row").unwrap().bottom(),
        "status row should remain below the scrolling action strip"
    );
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    app.update(cx, |this, cx| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        this.workspace
            .runtime_mut(tab_id)
            .expect("query runtime")
            .mongo
            .set_find_expanded(true);
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let panel = cx
        .debug_bounds("mongo-find-panel")
        .expect("expanded Find panel should render");
    assert_eq!(panel.size.width, gpui::px(520.0));
    assert_eq!(panel.size.height, gpui::px(220.0));
    for selector in [
        "mongo-find-header",
        "mongo-find-editor-area",
        "mongo-find-footer",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} should render inside the expanded Find panel"
        );
    }

    app.update_in(cx, |this, window, cx| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        this.set_mongo_find_expanded(tab_id, false, window, cx);
        this.set_mongo_sort_expanded(tab_id, true, window, cx);
        this.set_mongo_query_preview_open(tab_id, true, cx);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let sort_panel = cx
        .debug_bounds("mongo-sort-panel")
        .expect("expanded Sort panel should render");
    assert_eq!(sort_panel.size.width, gpui::px(520.0));
    assert_eq!(sort_panel.size.height, gpui::px(220.0));
    for selector in [
        "mongo-sort-header",
        "mongo-sort-editor-area",
        "mongo-sort-footer",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} should render inside the expanded Sort panel"
        );
    }
    assert!(
        cx.debug_bounds("mongo-query-panel").is_some(),
        "query preview panel should render"
    );
}

#[gpui::test]
fn expanded_find_editor_inserts_newline_on_shift_enter(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        app.bind_keys([
            KeyBinding::new("enter", MongoFindEnter, Some("mongo-find > Input")),
            shortcuts::key_binding(
                app,
                "secondary-enter",
                MongoFindEnter,
                Some("mongo-find > Input"),
            ),
        ]);
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
    let tab_id = app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        this.workspace
            .runtime_mut(tab_id)
            .expect("query runtime")
            .mongo
            .set_find_expanded(true);
        cx.notify();
        tab_id
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let editor = cx.cx.read(|app_context| {
        app.read(app_context)
            .mongo_ui
            .expanded_find_editors
            .get(&tab_id)
            .cloned()
            .expect("expanded Find editor should be created")
    });
    app.update_in(cx, |_, window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value("{}", window, cx);
            state.focus(window, cx);
        });
    });
    cx.simulate_keystrokes("shift-enter");

    let value = cx.cx.read(|app_context| editor.read(app_context).value());
    assert_eq!(value.as_ref(), "\n{}");

    let filter = cx.cx.read(|app_context| {
        app.read(app_context)
            .workspace
            .runtime(tab_id)
            .expect("query runtime")
            .mongo
            .filter
            .clone()
    });
    assert_eq!(filter, "\n{}");

    app.update_in(cx, |this, window, cx| {
        this.set_mongo_find_expanded(tab_id, false, window, cx);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let (filter, compact_value) = cx.cx.read(|app_context| {
        let this = app.read(app_context);
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        let compact = this
            .mongo_ui
            .filter_editors
            .get(&tab_id)
            .expect("compact Find editor")
            .read(app_context)
            .value();
        (runtime.mongo.filter.clone(), compact)
    });
    assert_eq!(filter, "\n{}");
    assert_eq!(compact_value.as_ref(), " {}");

    app.update_in(cx, |this, window, cx| {
        this.set_mongo_find_expanded(tab_id, true, window, cx);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let value = cx.cx.read(|app_context| editor.read(app_context).value());
    assert_eq!(value.as_ref(), "\n{}");

    app.update_in(cx, |_, window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value("{}", window, cx);
            state.focus(window, cx);
        });
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_keystrokes("shift-enter");

    let value = cx.cx.read(|app_context| editor.read(app_context).value());
    assert_eq!(value.as_ref(), "\n");

    app.update_in(cx, |_, window, cx| {
        editor.update(cx, |state, cx| state.focus(window, cx));
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_input("{}");
    cx.simulate_keystrokes("enter");

    let (value, failed) = cx.cx.read(|app_context| {
        let value = editor.read(app_context).value();
        let failed = matches!(
            app.read(app_context)
                .workspace
                .runtime(tab_id)
                .expect("query runtime")
                .query
                .status(),
            crate::query::QueryStatus::Failed(_)
        );
        (value, failed)
    });
    assert_eq!(value.as_ref(), "{}");
    assert!(failed, "plain Enter should execute the Find query");

    app.update(cx, |this, cx| {
        this.workspace
            .runtime_mut(tab_id)
            .expect("query runtime")
            .query = crate::query::QueryState::default();
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let enter = gpui::Keystroke::parse("enter").expect("enter keystroke");
    cx.simulate_event(gpui::KeyDownEvent {
        keystroke: enter.clone(),
        is_held: true,
        prefer_character_input: false,
    });
    let idle = cx.cx.read(|app_context| {
        matches!(
            app.read(app_context)
                .workspace
                .runtime(tab_id)
                .expect("query runtime")
                .query
                .status(),
            crate::query::QueryStatus::Idle
        )
    });
    assert!(idle, "held Enter should not rerun the Find query");
    cx.simulate_event(gpui::KeyUpEvent { keystroke: enter });
    cx.simulate_keystrokes("enter");
    let failed = cx.cx.read(|app_context| {
        matches!(
            app.read(app_context)
                .workspace
                .runtime(tab_id)
                .expect("query runtime")
                .query
                .status(),
            crate::query::QueryStatus::Failed(_)
        )
    });
    assert!(failed, "Enter should work again after key release");
    cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").expect("enter keystroke"),
    });

    app.update(cx, |this, cx| {
        this.workspace
            .runtime_mut(tab_id)
            .expect("query runtime")
            .query = crate::query::QueryState::default();
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-enter");
    let failed = cx.cx.read(|app_context| {
        matches!(
            app.read(app_context)
                .workspace
                .runtime(tab_id)
                .expect("query runtime")
                .query
                .status(),
            crate::query::QueryStatus::Failed(_)
        )
    });
    assert!(failed, "Cmd+Enter should execute the Find query");
}

#[gpui::test]
fn mongo_filter_popover_matches_compact_layout(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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
    app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.query.begin();
        runtime.mongo.known_columns = vec!["_id".into(), "name".into()];
        runtime.mongo.add_filter_rule("_id");
        runtime.mongo.add_filter_rule("name");
        runtime.mongo.filter_rules[0].mode = crate::mongo_result::MongoFilterMode::NotEquals;
        runtime.mongo.filter_rules[1].mode =
            crate::mongo_result::MongoFilterMode::GreaterThanOrEqual;
        cx.notify();
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert!(cx.debug_bounds("mongo-filter-panel").is_none());
    let filter = cx
        .debug_bounds("mongo-filter-label")
        .expect("filter trigger should render");
    cx.simulate_click(filter.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert_eq!(
        cx.debug_bounds("mongo-filter-panel")
            .expect("filter popover should render")
            .size
            .width,
        gpui::px(468.0)
    );
    assert!(
        cx.debug_bounds("mongo-filter-overlay").is_some(),
        "filter panel should render in root overlay layer"
    );
    for selector in [
        "mongo-filter-header",
        "mongo-filter-rule-row",
        "mongo-filter-conjunction-row",
        "mongo-filter-footer",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} should render inside filter popover"
        );
    }
    assert_eq!(
        cx.debug_bounds("mongo-filter-rule-row")
            .expect("filter rule should render")
            .size
            .height,
        gpui::px(32.0)
    );
    assert_eq!(
        cx.debug_bounds("mongo-filter-conjunction-row")
            .expect("conjunction row should render")
            .size
            .height,
        gpui::px(20.0)
    );
    assert_eq!(
        cx.debug_bounds("mongo-filter-mode-control")
            .expect("mode control should render")
            .size
            .width,
        gpui::px(94.0)
    );
    assert_eq!(
        cx.debug_bounds("mongo-filter-field-control-1")
            .expect("field control should render")
            .size
            .height,
        gpui::px(32.0)
    );
    assert!(
        cx.debug_bounds("mongo-filter-field-label-1").is_some(),
        "field control should use the shared dropdown label"
    );
    assert_eq!(
        cx.debug_bounds("mongo-filter-type-control-1")
            .expect("type control should render")
            .size
            .width,
        gpui::px(88.0)
    );
    assert_eq!(
        cx.debug_bounds("mongo-filter-value-control-1")
            .expect("value control should render")
            .size
            .width,
        gpui::px(104.0)
    );
    let mode_label_width = cx
        .debug_bounds("mongo-filter-mode-label-1")
        .expect("mode label should render")
        .size
        .width;
    assert!(
        mode_label_width <= gpui::px(68.0),
        "long operator label must leave room for the caret: {mode_label_width:?}"
    );
    assert!(
        cx.debug_bounds("mongo-filter-mode-label-2")
            .expect("second mode label should render")
            .size
            .width
            <= gpui::px(68.0),
        "long comparison label must leave room for the caret"
    );

    let value = cx
        .debug_bounds("mongo-filter-value-control-1")
        .expect("value input should render");
    cx.simulate_click(value.center(), Default::default());
    cx.simulate_input("draft");
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        assert_eq!(
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .mongo
                .filter_rules[0]
                .value,
            "draft"
        );
    });

    let overlay = cx
        .debug_bounds("mongo-filter-overlay")
        .expect("filter overlay should render");
    assert!(overlay.contains(&Point::new(px(850.0), px(550.0))));
    cx.simulate_click(Point::new(px(850.0), px(550.0)), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        assert!(
            !this
                .workspace
                .runtime(tab_id)
                .expect("query runtime")
                .mongo
                .filter_open
        );
    });
    let filter = cx
        .debug_bounds("mongo-filter-label")
        .expect("filter trigger should remain available");
    cx.simulate_click(filter.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("mongo-filter-panel").is_some());
    app.read_with(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        assert_eq!(
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .mongo
                .filter_rules[0]
                .value,
            "draft"
        );
    });

    let mode = cx
        .debug_bounds("mongo-filter-mode-control")
        .expect("operator dropdown should render");
    cx.simulate_click(mode.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("mongo-filter-panel").is_some());

    let value_type = cx
        .debug_bounds("mongo-filter-type-control-1")
        .expect("type dropdown should render");
    cx.simulate_click(value_type.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("mongo-filter-panel").is_some());

    cx.simulate_click(
        Point::new(
            value_type.left() + px(48.0),
            value_type.top() + px(45.0 + 2.0 * 28.0),
        ),
        Default::default(),
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        assert!(runtime.mongo.filter_open);
        assert_eq!(
            runtime.mongo.filter_rules[0].value_type,
            crate::mongo_result::MongoFilterValueType::Number
        );
    });

    let value_type = cx
        .debug_bounds("mongo-filter-type-control-1")
        .expect("type dropdown should remain available");
    cx.simulate_click(value_type.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.simulate_click(
        Point::new(
            value_type.left() + px(48.0),
            value_type.top() + px(45.0 + 9.0 * 28.0),
        ),
        Default::default(),
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let tab_id = this.workspace.active_tab().expect("active tab").id;
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        assert!(runtime.mongo.filter_open);
        assert_eq!(
            runtime.mongo.filter_rules[0].value_type,
            crate::mongo_result::MongoFilterValueType::Json
        );
    });
}

#[gpui::test]
fn mongo_view_switch_is_a_segmented_document_then_table_group(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    let group = visual.debug_bounds("mongo-view-switch").expect("switch");
    let document = visual.debug_bounds("mongo-view-document").expect("document button");
    let table = visual.debug_bounds("mongo-view-table").expect("table button");
    // Order: Braces (Document) first, Table2 second; 20x20 buttons in a
    // 22px-high bordered group with a 1px divider.
    assert!(document.right() <= table.left(), "Document left of Table");
    assert_eq!(document.size, gpui::size(gpui::px(20.0), gpui::px(20.0)));
    assert_eq!(table.size, gpui::size(gpui::px(20.0), gpui::px(20.0)));
    assert_eq!(group.size.height, gpui::px(22.0));
    assert_eq!(group.size.width, gpui::px(43.0), "1px border x2 + 2x20 + 1px divider");
    let mode = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            let tab = this.workspace.active_tab().unwrap();
            this.workspace.runtime(tab.id).unwrap().mongo.view
        })
    };
    assert_eq!(mode(&app, visual), crate::mongo_result::MongoResultView::Table);
    visual.simulate_click(document.center(), Default::default());
    settle(visual);
    assert_eq!(mode(&app, visual), crate::mongo_result::MongoResultView::Document);
    let table = visual.debug_bounds("mongo-view-table").unwrap();
    visual.simulate_click(table.center(), Default::default());
    settle(visual);
    assert_eq!(mode(&app, visual), crate::mongo_result::MongoResultView::Table);
}

#[gpui::test]
fn the_mongo_run_button_stays_pinned_in_a_narrow_pane(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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
    app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.query.begin();
        runtime.mongo.known_columns = vec!["_id".into(), "name".into()];
        runtime.mongo.visible_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_columns = HashSet::from(["_id".into(), "name".into()]);
        runtime.mongo.projection_draft_columns = runtime.mongo.projection_columns.clone();
        cx.notify();
    });
    for width in [900.0, 620.0, 520.0, 440.0] {
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(600.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let capsule = cx.debug_bounds("mongo-query-capsule").expect("capsule");
        let run = cx.debug_bounds("mongo-run").expect("run");
        let columns = cx.debug_bounds("mongo-columns-button").expect("columns button");
        assert!(run.left() >= capsule.left() && run.right() <= capsule.right(), "{width}: Run inside the capsule");
        assert!(capsule.right() - run.right() <= gpui::px(12.0), "{width}: Run at the capsule's right edge");
        assert!(run.right() <= columns.left(), "{width}: Run left of Columns");
        let fields = cx.debug_bounds("mongo-query-fields").expect("fields lane");
        assert!(fields.right() <= run.left(), "{width}: fields stay left of Run");
    }
}

/// Hiding grid columns is a client-side view change: the document view must
/// still show every selected document in full, with no driver error.
#[gpui::test]
fn document_view_shows_the_full_document_after_hiding_columns(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    let (tab_id, columns) = app.read_with(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let known = this.workspace.runtime(tab_id).unwrap().mongo.known_columns.clone();
        (tab_id, known)
    });
    assert!(columns.len() > 1, "the fixture has several columns: {columns:?}");

    let button = visual.debug_bounds("mongo-columns-button").expect("Columns button");
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
    // Uncheck everything but the last column through the real popover rows.
    for (index, name) in columns[..columns.len() - 1].iter().enumerate() {
        let selector: &'static str = Box::leak(format!("mongo-column-row-{index}").into_boxed_str());
        // The picker list is virtualized: rows past its viewport are not
        // rendered, so the remainder is toggled through the same handler.
        if let Some(row) = visual.debug_bounds(selector) {
            visual.simulate_click(row.center(), Default::default());
        } else {
            let name = name.clone();
            app.update(visual, |this, cx| this.toggle_mongo_column(tab_id, name, cx));
        }
        settle(visual);
    }
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.visible_columns.len(), 1);
    });

    let document = visual.debug_bounds("mongo-view-document").expect("document button");
    visual.simulate_click(document.center(), Default::default());
    for _ in 0..5 {
        settle(visual);
    }
    app.read_with(visual, |this, _| {
        let inspector = &this.workspace.runtime(tab_id).unwrap().mongo.inspector;
        assert_eq!(inspector.error, None, "no driver error for a hidden-column view");
        let snapshot = inspector.snapshot.as_ref().expect("the selected document loaded");
        let document = snapshot.document.as_object().expect("document object");
        assert!(
            columns.iter().all(|column| document.contains_key(column)),
            "full document {document:?} must hold every column {columns:?}"
        );
    });
}

/// Drops a live test's own database when it goes out of scope: when the test ends, also when an assertion fails.
struct DropDatabase {
    session: std::sync::Arc<engine_driver_host::SharedDriverSession>,
    database: String,
}

impl Drop for DropDatabase {
    fn drop(&mut self) {
        if let Ok(operation) = self.session.begin_operation() {
            let _ = self.session.mutate(
                &operation,
                engine_driver_api::MutationRequest {
                    operation_id: crate::mongo_result::STAGED_OPERATION_ID,
                    kind: "drop_database".into(),
                    database: Some(self.database.clone()),
                    object: None,
                    identity: serde_json::Value::Null,
                    payload: serde_json::Value::Null,
                },
            );
        }
    }
}

/// The scenario Mongo find tab, re-bound to the real MongoDB driver loaded
/// through the host (opt-in: `TABLEX_TEST_MONGO_URI` is a live server and
/// `TABLEX_TEST_MONGO_DYLIB` the driver built with `--features abi-v2`),
/// with 200 sixteen-field documents seeded and the find run for real. The database they are seeded in goes with the returned
/// guard.
fn live_mongo_find_tab(
    cx: &mut TestAppContext,
) -> Option<(gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64, DropDatabase)> {
    let uri = std::env::var("TABLEX_TEST_MONGO_URI").ok()?;
    let dylib = std::env::var("TABLEX_TEST_MONGO_DYLIB").ok()?;
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.dispatcher.allow_parking();
    settle(visual);
    let staged = std::env::temp_dir().join(format!("tablex-live-mongo-{}.dylib", std::process::id()));
    std::fs::copy(dylib, &staged).expect("copy the mongo driver");
    let handle = engine_driver_host::DriverLoader.load(&staged).expect("mongo driver loads");
    let mut session = handle
        .connect(&serde_json::json!({"uri": uri, "name": "live"}).to_string())
        .expect("connect to the live MongoDB");
    let database = format!("tablex_gpui_live_{}", std::process::id());
    for id in 0..200 {
        let mut document = serde_json::Map::new();
        document.insert("_id".into(), id.into());
        document.insert("transId".into(), format!("TX-{id:04}").into());
        for field in 0..14 {
            document.insert(format!("field{field:02}"), (id * 100 + field).into());
        }
        session
            .mutate(&engine_driver_api::MutationRequest {
                operation_id: engine_driver_api::OperationId(1),
                kind: "insert".into(),
                database: Some(database.clone()),
                object: Some("payments".into()),
                identity: serde_json::Value::Null,
                payload: document.into(),
            })
            .expect("seed a document");
    }
    let (tab_id, cleanup) = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.scenario_ui.service = None;
        let connection = this.workspace.tab(tab_id).unwrap().connection_id.clone();
        let engine_id = session.connection_id();
        let capabilities = std::sync::Arc::new(handle.capabilities().clone());
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        let cleanup = DropDatabase { session: session.clone(), database: database.clone() };
        this.driver_runtime.sessions.insert(connection.clone(), session);
        this.driver_runtime.connection_ids.insert(connection.clone(), engine_id);
        this.driver_runtime.capabilities.insert(connection, capabilities);
        this.driver_ui.handles.insert("mongodb".into(), handle);
        let tab = this.workspace.tab_mut(tab_id).unwrap();
        tab.database = database;
        tab.collection = "payments".into();
        tab.output_limit = 200;
        this.run_tab_query(tab_id, cx);
        (tab_id, cleanup)
    });
    for _ in 0..100 {
        settle(visual);
        let loaded = app.read_with(visual, |this, _| {
            this.workspace.runtime(tab_id).unwrap().driver_page_id.is_some()
        });
        if loaded {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    settle(visual);
    Some((app, visual, tab_id, cleanup))
}

#[gpui::test]
fn live_document_view_shows_the_full_document_after_hiding_columns_and_a_dropped_page(cx: &mut TestAppContext) {
    let Some((app, visual, tab_id, _cleanup)) = live_mongo_find_tab(cx) else {
        eprintln!("live mongo test skipped: TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB not set");
        return;
    };
    let rows = app.read_with(visual, |this, _| this.grid(tab_id).row_count());
    assert_eq!(rows, 200, "the live find returned the seeded documents");
    let columns = app.read_with(visual, |this, _| {
        let grid = this.grid(tab_id);
        (0..grid.column_count())
            .filter_map(|column| grid.column_name(column).map(str::to_string))
            .collect::<Vec<_>>()
    });
    assert!(columns.contains(&"transId".to_string()) && columns.len() > 1, "{columns:?}");

    let button = visual.debug_bounds("mongo-columns-button").expect("Columns button");
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
    // Start from the user's state (every column visible); keep only transId.
    app.update(visual, |this, cx| this.show_all_mongo_columns(tab_id, cx));
    for name in columns.iter().filter(|name| name.as_str() != "transId").cloned() {
        app.update(visual, |this, cx| this.toggle_mongo_column(tab_id, name, cx));
    }
    settle(visual);
    app.read_with(visual, |this, _| {
        let visible = &this.workspace.runtime(tab_id).unwrap().mongo.visible_columns;
        assert_eq!(visible.iter().cloned().collect::<Vec<_>>(), ["transId"]);
    });

    // Another tab commits on the same connection: the driver drops every page
    // of the connection while this tab keeps showing the rows it holds.
    let stale = app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().driver_page_id);
    let session = app.read_with(visual, |this, _| {
        let connection = &this.workspace.tab(tab_id).unwrap().connection_id;
        this.driver_runtime.sessions[connection].clone()
    });
    let operation = session.begin_operation().expect("operation");
    let database = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().database.clone());
    let result = session
        .mutate(
            &operation,
            engine_driver_api::MutationRequest {
                operation_id: crate::mongo_result::STAGED_OPERATION_ID,
                kind: "insert".into(),
                database: Some(database),
                object: Some("payments".into()),
                identity: serde_json::Value::Null,
                payload: serde_json::json!({"_id": 1000, "transId": "TX-OTHER"}),
            },
        )
        .expect("another tab's commit");
    assert!(result.invalidated_page_ids.contains(&stale.unwrap()), "{result:?}");
    drop(operation);

    let document = visual.debug_bounds("mongo-view-document").expect("document button");
    visual.simulate_click(document.center(), Default::default());
    // Opening a document of the dropped page re-runs the find for a new one.
    app.update(visual, |this, cx| this.select_mongo_document(tab_id, 3, cx));
    for _ in 0..40 {
        settle(visual);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_ne!(runtime.driver_page_id, stale, "the tab holds a current page");
        let inspector = &runtime.mongo.inspector;
        assert_eq!(inspector.error, None, "no driver error for a hidden-column view");
        let snapshot = inspector.snapshot.as_ref().expect("the selected document loaded");
        let document = snapshot.document.as_object().expect("document object");
        assert!(
            columns.iter().all(|column| document.contains_key(column)),
            "full document {document:?} must hold every column {columns:?}"
        );
    });
}

#[gpui::test]
fn edit_menu_copy_copies_the_selected_grid_cell_like_the_grid_shortcut(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario =
        UiScenario::from_args(["tablex-gpui", "--ui-scenario", "mongo-grid", "--ui-fixture", "sample"])
            .unwrap()
            .unwrap();
    let (_root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    visual.update(|_, cx| super::actions::bind_keys(cx));
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let clipboard = |visual: &mut gpui::VisualTestContext| {
        visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
    };
    let cell = visual.debug_bounds("grid-body-cell-0-1").expect("name cell");
    visual.simulate_click(cell.center(), Default::default());
    visual.run_until_parked();

    // The grid's own shortcut is the reference value.
    visual.simulate_keystrokes("secondary-c");
    let by_shortcut = clipboard(visual).expect("the shortcut copies the cell");
    assert!(by_shortcut.contains("mongo-customer-000"), "{by_shortcut}");

    visual.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("stale".into())));
    // Edit > Copy dispatches the OS copy action to the focused element.
    visual.dispatch_action(gpui_component::input::Copy);
    visual.run_until_parked();
    assert_eq!(clipboard(visual), Some(by_shortcut), "Edit > Copy copies the same cell");
}

/// First visible text of the `name` column of the tab's grid.
fn first_name(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab_id: u64) -> Option<String> {
    app.read_with(visual, |this, _| {
        let grid = this.grid(tab_id);
        let column = (0..grid.column_count()).find(|column| grid.column_name(*column) == Some("name"))?;
        grid.cell_text(CellPosition { row: 0, column }).map(|text| text.to_string())
    })
}

fn click_debug(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is on screen"));
    visual.simulate_click(bounds.center(), Default::default());
    for _ in 0..3 {
        settle(visual);
    }
}

/// A blank sort means "no sort": clearing it (the X button, or typing it away)
/// must leave Execute enabled, and Execute must run with no sort.
#[gpui::test]
fn clearing_the_sort_keeps_execute_enabled_and_runs_unsorted(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    let tab_id = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    let natural = first_name(&app, visual, tab_id).expect("first row");

    // Type a descending sort into the SORT input and run it.
    let editor = app.update_in(visual, |this, window, cx| this.mongo_sort_editor(tab_id, window, cx));
    app.update_in(visual, |_, window, cx| editor.update(cx, |state, cx| state.focus(window, cx)));
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(r#"{"name":-1}"#);
    settle(visual);
    click_debug(visual, "mongo-run");
    let sorted = first_name(&app, visual, tab_id).expect("first sorted row");
    assert_ne!(sorted, natural, "the sort changed the order (fixture precondition)");

    // The X button clears the sort; Execute stays enabled and runs unsorted.
    click_debug(visual, "mongo-sort-clear");
    app.read_with(visual, |this, _| assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.sort, ""));
    click_debug(visual, "mongo-run");
    assert_eq!(first_name(&app, visual, tab_id).as_deref(), Some(natural.as_str()), "Execute ran with no sort");

    // Typing the sort away by hand behaves the same.
    app.update_in(visual, |_, window, cx| editor.update(cx, |state, cx| state.focus(window, cx)));
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(r#"{"name":-1}"#);
    settle(visual);
    click_debug(visual, "mongo-run");
    assert_eq!(first_name(&app, visual, tab_id).as_deref(), Some(sorted.as_str()));
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_keystrokes("backspace");
    settle(visual);
    app.read_with(visual, |this, _| assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.sort, ""));
    click_debug(visual, "mongo-run");
    assert_eq!(first_name(&app, visual, tab_id).as_deref(), Some(natural.as_str()));
}

/// A blank filter is the empty filter: Execute stays enabled and runs.
#[gpui::test]
fn a_blank_filter_keeps_execute_enabled(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    let tab_id = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    let natural = first_name(&app, visual, tab_id).expect("first row");
    let editor = app.update_in(visual, |this, window, cx| this.mongo_filter_editor(tab_id, window, cx));
    app.update_in(visual, |_, window, cx| editor.update(cx, |state, cx| state.focus(window, cx)));
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(r#"{"name":"no such name"}"#);
    settle(visual);
    click_debug(visual, "mongo-run");
    assert_ne!(first_name(&app, visual, tab_id).as_deref(), Some(natural.as_str()), "the filter applied");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_keystrokes("backspace");
    settle(visual);
    app.read_with(visual, |this, _| assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.filter, ""));
    click_debug(visual, "mongo-run");
    assert_eq!(first_name(&app, visual, tab_id).as_deref(), Some(natural.as_str()), "a blank filter selects everything");
}

#[test]
fn blank_filter_sort_and_projection_mean_none_in_the_request() {
    for blank in ["", "  ", "\n"] {
        assert!(mongo_json_object_is_valid(blank), "{blank:?} is valid");
        let payload = mongo_query_payload(blank, blank, blank);
        assert_eq!(payload["filter"], "{}");
        assert_eq!(payload["sort"], "");
        assert_eq!(payload["project"], "");
    }
    assert!(!mongo_json_object_is_valid("{ nope"));
    let payload = mongo_query_payload(r#"{"a":1}"#, r#" {"b":-1} "#, "");
    assert_eq!((payload["filter"].as_str(), payload["sort"].as_str()), (Some(r#"{"a":1}"#), Some(r#"{"b":-1}"#)));
}

/// Live Mongo tab on the driver named by `TABLEX_TEST_MONGO_DYLIB` (any build:
/// the installed 0.1.0 one has no page leases, the current one has) with five
/// documents in `tablex_gpui_empty_live.items`. `None` when the live environment is not configured.
fn live_mongo_tab(
    cx: &mut TestAppContext,
) -> Option<(gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64)> {
    let uri = std::env::var("TABLEX_TEST_MONGO_URI").ok()?;
    let dylib = std::env::var("TABLEX_TEST_MONGO_DYLIB").ok()?;
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.dispatcher.allow_parking();
    settle(visual);
    let staged = std::env::temp_dir().join(format!("tablex-live-mongo-empty-{}.dylib", std::process::id()));
    std::fs::copy(dylib, &staged).expect("copy the mongo driver");
    let handle = engine_driver_host::DriverLoader.load(&staged).expect("mongo driver loads");
    let mut session = handle
        .connect(&serde_json::json!({"uri": uri, "name": "live"}).to_string())
        .expect("connect to the live MongoDB");
    // A fixed database: a driver without `mutation.insert` (the installed 0.1.0 one) reads a
    // pre-seeded one, e.g. `docker exec <mongo> mongosh --eval 'db.getSiblingDB("tablex_gpui_empty_live").items.insertMany([{_id:0},{_id:1},{_id:2},{_id:3},{_id:4}])'`.
    let database = "tablex_gpui_empty_live".to_string();
    let can_seed = handle.capabilities().has(engine_driver_api::Capability::MutationInsert);
    for id in (0..5).filter(|_| can_seed) {
        // Re-runs hit duplicate keys: the documents are already there.
        let _ = session
            .mutate(&engine_driver_api::MutationRequest {
                operation_id: engine_driver_api::OperationId(1),
                kind: "insert".into(),
                database: Some(database.clone()),
                object: Some("items".into()),
                identity: serde_json::Value::Null,
                payload: serde_json::json!({"_id": id, "name": format!("item-{id}")}),
            });
    }
    let tab_id = app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.scenario_ui.service = None;
        let connection = this.workspace.tab(tab_id).unwrap().connection_id.clone();
        let engine_id = session.connection_id();
        let capabilities = std::sync::Arc::new(handle.capabilities().clone());
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        this.driver_runtime.sessions.insert(connection.clone(), session);
        this.driver_runtime.connection_ids.insert(connection.clone(), engine_id);
        this.driver_runtime.capabilities.insert(connection, capabilities);
        this.driver_ui.handles.insert("mongodb".into(), handle);
        let tab = this.workspace.tab_mut(tab_id).unwrap();
        tab.database = database;
        tab.collection = "items".into();
        tab.output_limit = 200;
        tab_id
    });
    Some((app, visual, tab_id))
}

/// Runs the tab's find with `filter` / `sort` on `collection` through the Execute path and
/// returns the finished status.
fn live_run(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
    collection: &str,
    filter: &str,
    sort: &str,
) -> crate::query::QueryStatus {
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab_id).unwrap().collection = collection.into();
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        runtime.mongo.filter = filter.into();
        runtime.mongo.sort = sort.into();
        runtime.mongo.mark_filter_draft();
        runtime.pagination.reset();
        this.run_tab_query_explicit(tab_id, cx);
    });
    for _ in 0..200 {
        settle(visual);
        let status = app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().query.status().clone());
        if !matches!(status, crate::query::QueryStatus::Running { .. } | crate::query::QueryStatus::Idle) {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the live find did not finish");
}

/// Empty sort, empty filter, empty collection and a filter matching nothing all run to a
/// (possibly empty) result: none of them is an Arrow "row count or at least one column" error.
#[gpui::test]
#[ignore = "live: needs TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB and the pre-seeded tablex_gpui_empty_live.items"]
fn live_mongo_blank_inputs_and_empty_results_run_without_an_error(cx: &mut TestAppContext) {
    let Some((app, visual, tab_id)) = live_mongo_tab(cx) else {
        eprintln!("live mongo test skipped: TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB not set");
        return;
    };
    let cases: [(&str, &str, &str, usize); 7] = [
        ("items", "{}", r#"{"_id":1}"#, 5),
        ("items", "{}", "", 5),
        ("items", "", "", 5),
        ("items", "  ", "  ", 5),
        ("items", r#"{"_id": 999}"#, "", 0),
        ("no_such_collection", "{}", "", 0),
        ("no_such_collection", "{}", r#"{"_id":1}"#, 0),
    ];
    for (collection, filter, sort, rows) in cases {
        let status = live_run(&app, visual, tab_id, collection, filter, sort);
        assert_eq!(
            status,
            crate::query::QueryStatus::Ready { rows },
            "{collection} filter {filter:?} sort {sort:?}"
        );
        assert_eq!(app.read_with(visual, |this, _| this.grid(tab_id).row_count()), rows);
    }
}

/// The reported sequence through the UI: a default `_id` sort, cleared with the X button, then
/// Execute (on a result that is empty, as in the report).
#[gpui::test]
#[ignore = "live: needs TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB and the pre-seeded tablex_gpui_empty_live.items"]
fn live_mongo_clear_sort_then_execute_on_an_empty_result(cx: &mut TestAppContext) {
    let Some((app, visual, tab_id)) = live_mongo_tab(cx) else {
        eprintln!("live mongo test skipped: TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB not set");
        return;
    };
    let status = live_run(&app, visual, tab_id, "items", "{}", r#"{"_id":1}"#);
    assert_eq!(status, crate::query::QueryStatus::Ready { rows: 5 });
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab_id).unwrap().collection = "no_such_collection".into();
    });
    click_debug(visual, "mongo-sort-clear");
    click_debug(visual, "mongo-run");
    for _ in 0..200 {
        settle(visual);
        let running = app.read_with(visual, |this, _| {
            matches!(this.workspace.runtime(tab_id).unwrap().query.status(), crate::query::QueryStatus::Running { .. })
        });
        if !running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let (status, sort) = app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        (runtime.query.status().clone(), runtime.mongo.sort.clone())
    });
    assert_eq!(sort, "");
    assert_eq!(status, crate::query::QueryStatus::Ready { rows: 0 }, "Execute ran and the empty result is not an error");
}
