// SQL table-data tabs (WHERE / ORDER BY bar, no editor).

fn sql_table_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
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
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    let find_row = |app: &gpui::Entity<AppRoot>, label: &str, visual: &mut gpui::VisualTestContext| {
        app.update(visual, |this, _| {
            this.navigator
                .visible_rows()
                .into_iter()
                .find(|row| row.label == label)
                .expect("fixture navigator row")
        })
    };
    for label in ["Postgres fixture", "warehouse", "public", "Tables"] {
        let row = find_row(&app, label, visual);
        app.update(visual, |this, cx| this.handle_navigator_disclosure(&row, cx));
        visual.run_until_parked();
    }
    let orders = find_row(&app, "orders", visual);
    app.update(visual, |this, cx| this.handle_navigator_open(&orders, cx));
    settle(visual);
    let tab_id = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    (app, visual, tab_id)
}

fn sql_table_type(visual: &mut gpui::VisualTestContext, field_selector: &'static str, text: &str) {
    let field = visual.debug_bounds(field_selector).expect("bar field");
    // The input fills the field right of its label; click near the middle.
    visual.simulate_click(field.center(), Default::default());
    visual.simulate_input(text);
    settle(visual);
}

fn sql_table_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| this.active_grid().row_count())
}

fn sql_table_first_id(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> String {
    app.read_with(visual, |this, _| {
        this.active_grid()
            .full_cell_text(crate::grid::CellPosition { row: 0, column: 0 })
            .unwrap_or_default()
    })
}

#[gpui::test]
fn sidebar_table_opens_a_table_data_tab_with_where_order_by_bar_and_no_editor(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = sql_table_app(cx);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::SqlTable));
        assert_eq!(tab.title, "orders");
        assert_eq!(
            (tab.connection_id.as_str(), tab.database.as_str(), tab.schema.as_deref(), tab.collection.as_str()),
            (crate::ui_fixture::POSTGRES_ID, "warehouse", Some("public"), "orders")
        );
        assert_eq!(tab.sql, "SELECT * FROM \"public\".\"orders\"");
        assert_eq!((tab.table_where.as_str(), tab.table_order_by.as_str()), ("", ""));
        assert!(this.active_grid().row_count() > 0, "the table opens already loaded");
    });
    // Bar present with both labelled fields, run, and the Mongo toolbar shape.
    let toolbar = visual.debug_bounds("sql-result-toolbar").expect("toolbar");
    assert_eq!(toolbar.size.height, gpui::px(theme::RESULT_TOOLBAR_HEIGHT));
    let capsule = visual.debug_bounds("sql-query-capsule").expect("capsule");
    assert_eq!(capsule.size.height, gpui::px(30.0));
    for selector in [
        "sql-where-field",
        "sql-order-field",
        "sql-where-label",
        "sql-order-label",
        "sql-run",
        "sql-query-preview",
        "sql-field-handle",
    ] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    let where_field = visual.debug_bounds("sql-where-field").unwrap();
    let order_field = visual.debug_bounds("sql-order-field").unwrap();
    assert!(where_field.right() <= order_field.left(), "WHERE left of ORDER BY");
    assert!(order_field.right() <= visual.debug_bounds("sql-run").unwrap().left());
    // Columns control sits right of the capsule, like the Mongo toolbar.
    let columns_button = visual.debug_bounds("sql-columns-button").expect("columns button");
    assert!(columns_button.left() >= capsule.right(), "right of the capsule");
    // The button opens the show / hide popover (like Mongo's), not Properties.
    visual.simulate_click(columns_button.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("sql-column-panel").is_some(), "Columns opens the picker");
    assert!(visual.debug_bounds("columns-view").is_none(), "Properties stays on the rail");
    let columns_button = visual.debug_bounds("sql-columns-button").unwrap();
    visual.simulate_click(columns_button.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("sql-column-panel").is_none(), "second click closes the picker");
    // Properties is still one click away on the left rail.
    let rail_columns = visual.debug_bounds("properties-rail-button").expect("rail Columns");
    visual.simulate_click(rail_columns.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("columns-view").is_some(), "rail Columns opens Properties");
    let rail_grid = visual.debug_bounds("columns-rail-grid-button").unwrap();
    visual.simulate_click(rail_grid.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("columns-view").is_none(), "Grid returns to the grid");
    // No SQL editor / Save / Results header / Mongo bar.
    assert!(visual.debug_bounds("save-sql-script").is_none(), "no SQL editor");
    assert!(visual.debug_bounds("run-query").is_none(), "no title-bar Run");
    assert!(visual.debug_bounds("mongo-result-toolbar").is_none());
    assert!(visual.debug_bounds("grid-scroll-area").is_some());

    // Re-opening the same object reuses the tab.
    let count = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let orders = app.update(visual, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .find(|row| row.label == "orders")
            .unwrap()
    });
    app.update(visual, |this, cx| this.handle_navigator_open(&orders, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), count);
        assert_eq!(this.workspace.active_tab().unwrap().id, tab_id);
    });

    // New Query for the same object keeps the editor.
    let id = orders.id.clone();
    app.update(visual, |this, cx| this.new_query_for_sidebar_node(&id, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_ne!(tab.id, tab_id);
        assert_ne!(tab.query_mode, Some(QueryMode::SqlTable));
    });
    assert!(visual.debug_bounds("save-sql-script").is_some(), "New Query keeps the editor");
    assert!(visual.debug_bounds("sql-result-toolbar").is_none());
}

#[gpui::test]
fn where_and_order_by_run_against_the_fixture_engine(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    let total = sql_table_rows(&app, visual);
    assert!(total > 0);

    // Typing in WHERE + Enter runs the filtered query.
    sql_table_type(visual, "sql-where-field", "id > 200");
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().table_where, "id > 200");
    });
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(sql_table_rows(&app, visual), 40, "ids 201..=240");
    assert_eq!(sql_table_first_id(&app, visual), "201");
    assert!(visual.debug_bounds("sql-where-clear").is_some(), "clear x appears with text");
    assert!(visual.debug_bounds("sql-query-error").is_none());

    // ORDER BY + Enter re-orders the filtered rows.
    sql_table_type(visual, "sql-order-field", "id DESC");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(sql_table_rows(&app, visual), 40);
    assert_eq!(sql_table_first_id(&app, visual), "240");

    // The generated statement is quoted and combines both fragments.
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(
            this.sql_table_statement(tab).unwrap(),
            "SELECT * FROM \"public\".\"orders\"\nWHERE id > 200\nORDER BY id DESC"
        );
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert!(matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. }));
    });

    // Clearing ORDER BY restores the default order (and keeps the filter).
    let clear = visual.debug_bounds("sql-order-clear").expect("order clear");
    visual.simulate_click(clear.center(), Default::default());
    settle(visual);
    assert_eq!(sql_table_first_id(&app, visual), "201");
    // Clearing WHERE restores all rows.
    let clear = visual.debug_bounds("sql-where-clear").expect("where clear");
    visual.simulate_click(clear.center(), Default::default());
    settle(visual);
    assert_eq!(sql_table_rows(&app, visual), total);
    assert_eq!(sql_table_first_id(&app, visual), "1");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!((tab.table_where.as_str(), tab.table_order_by.as_str()), ("", ""));
    });
    assert!(visual.debug_bounds("sql-where-clear").is_none(), "clear hides when empty");
}

fn sql_table_replace_text(
    visual: &mut gpui::VisualTestContext,
    field_selector: &'static str,
    text: &str,
) {
    let field = visual.debug_bounds(field_selector).expect("bar field");
    visual.simulate_click(field.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

#[gpui::test]
fn paging_and_refresh_use_the_applied_where_not_the_live_editor_text(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    // Apply `id > 10` (230 rows over a 200-row page: ids 11..=210, then 211..=240).
    sql_table_replace_text(visual, "sql-where-field", "id > 10");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(sql_table_rows(&app, visual), 200);
    assert_eq!(sql_table_first_id(&app, visual), "11");

    // Type a different WHERE and do NOT run it.
    sql_table_replace_text(visual, "sql-where-field", "id > 230");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.table_where, "id > 230", "live buffer follows typing");
        assert_eq!(tab.applied_where, "id > 10", "typing never changes the applied query");
    });

    // Next page pages the OLD applied query (ids 211..=240), not `id > 230`.
    let next = visual.debug_bounds("page-next").expect("next page button");
    visual.simulate_click(next.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.runtime(tab_id).unwrap().pagination.offset(), 200);
    });
    assert_eq!(sql_table_rows(&app, visual), 30, "page 2 of `id > 10`");
    assert_eq!(sql_table_first_id(&app, visual), "211");

    // Refresh / auto-refresh re-run the same applied query on the same page.
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    settle(visual);
    assert_eq!(sql_table_rows(&app, visual), 30);
    assert_eq!(sql_table_first_id(&app, visual), "211");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!((tab.table_where.as_str(), tab.applied_where.as_str()), ("id > 230", "id > 10"));
    });

    // Enter applies the draft and returns to page 1 of the new query.
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().applied_where, "id > 230");
        assert_eq!(this.workspace.runtime(tab_id).unwrap().pagination.offset(), 0);
    });
    assert_eq!(sql_table_rows(&app, visual), 10, "ids 231..=240");
    assert_eq!(sql_table_first_id(&app, visual), "231");
}

#[gpui::test]
fn clearing_or_sorting_applies_only_that_field_and_the_session_stores_the_applied_query(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = sql_table_app(cx);
    sql_table_replace_text(visual, "sql-where-field", "id > 200");
    sql_table_replace_text(visual, "sql-order-field", "id DESC");
    // Nothing applied yet: only the drafts exist.
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!((tab.applied_where.as_str(), tab.applied_order_by.as_str()), ("", ""));
    });
    // Header sort applies ORDER BY only; the unsent WHERE draft stays a draft.
    let header = visual.debug_bounds("grid-header-cell-0").expect("id header");
    visual.simulate_click(header.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.applied_order_by, "\"id\" ASC");
        assert_eq!(tab.applied_where, "");
        assert_eq!(tab.table_where, "id > 200");
        // Session persistence stores the applied query plus the unsent draft.
        let session = this.workspace.snapshot();
        let saved = session.tabs.iter().find(|saved| saved.collection == "orders").unwrap();
        assert_eq!(saved.table_where, "");
        assert_eq!(saved.table_order_by, "\"id\" ASC");
        assert_eq!(saved.table_where_draft.as_deref(), Some("id > 200"));
        assert_eq!(saved.table_order_by_draft, None);
    });
    // A full 200-row page (not the 40 rows `id > 200` would give).
    assert_eq!(sql_table_rows(&app, visual), 200, "WHERE draft was not applied");

    // Restoring that snapshot brings back the applied query and the draft.
    let restored = app.read_with(visual, |this, _| this.workspace.snapshot());
    let mut other = crate::workspace::WorkspaceState::default();
    other.restore_snapshot(&restored, |_| true);
    let tab = other.tabs().iter().find(|tab| tab.collection == "orders").unwrap();
    assert_eq!(
        (tab.applied_where.as_str(), tab.table_where.as_str(), tab.applied_order_by.as_str()),
        ("", "id > 200", "\"id\" ASC")
    );
    let _ = tab_id;
}

#[gpui::test]
fn semicolon_fragments_are_rejected_inline_without_running(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    let generation = app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().query.generation()
    });
    sql_table_type(visual, "sql-where-field", "id > 1; DROP TABLE orders");
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        let error = runtime.sql_table.error.as_deref().expect("validation error");
        assert!(error.contains("';'"), "{error}");
        assert_eq!(runtime.query.generation(), generation, "nothing was run");
    });
    assert!(visual.debug_bounds("sql-query-error").is_some(), "danger state in the bar");

    // A ';' inside a string literal is fine; editing clears the error.
    let field = visual.debug_bounds("sql-where-field").unwrap();
    visual.simulate_click(field.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("name = 'a;b'");
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.runtime(tab_id).unwrap().sql_table.error.is_none());
    });
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert!(runtime.sql_table.error.is_none());
        assert!(runtime.query.generation() > generation, "ran with the quoted ';'");
    });
}

#[gpui::test]
fn engine_errors_show_in_the_bar_like_invalid_mongo_json(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    sql_table_type(visual, "sql-where-field", "nope = 1");
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        match runtime.query.status() {
            crate::query::QueryStatus::Failed(message) => {
                assert!(message.contains("column \"nope\" does not exist"), "{message}")
            }
            other => panic!("expected failed query, got {other:?}"),
        }
    });
    assert!(visual.debug_bounds("sql-query-error").is_some());
    // A corrected filter recovers.
    let field = visual.debug_bounds("sql-where-field").unwrap();
    visual.simulate_click(field.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("id = 3");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(visual.debug_bounds("sql-query-error").is_none());
    assert_eq!(sql_table_rows(&app, visual), 1);
}

#[gpui::test]
fn header_click_cycles_order_by_and_reruns(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    assert_eq!(sql_table_first_id(&app, visual), "1");
    // Column 2 is `status` in the fixture (nullable text, NULL last on ASC).
    let header = visual.debug_bounds("grid-header-cell-2").expect("status header");
    visual.simulate_click(header.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.table_order_by, "\"status\" ASC");
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.pagination.offset(), 0);
    });
    let ascending = app.read_with(visual, |this, _| {
        this.active_grid()
            .full_cell_text(crate::grid::CellPosition { row: 0, column: 2 })
    });
    assert_eq!(ascending.as_deref(), Some("active"), "NULL status rows sort last ascending");
    assert_eq!(sql_table_first_id(&app, visual), "3");
    // The ORDER BY input mirrors the header sort.
    let input_value = app.read_with(visual, |this, cx| {
        this.sql_table_ui
            .input_value(tab_id, results::sql_table::SqlField::OrderBy, cx)
    });
    assert_eq!(input_value.as_deref(), Some("\"status\" ASC"));

    visual.simulate_click(header.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().table_order_by, "\"status\" DESC");
    });
    // Descending puts the NULL-status rows (ids 1, 12, 23, ...) first, in
    // their original order; the default order would have id 2 second.
    let second_id = app.read_with(visual, |this, _| {
        this.active_grid()
            .full_cell_text(crate::grid::CellPosition { row: 1, column: 0 })
    });
    assert_eq!(second_id.as_deref(), Some("12"));

    // Third click clears ORDER BY: default order returns.
    visual.simulate_click(header.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().table_order_by, "");
    });
    assert_eq!(sql_table_first_id(&app, visual), "1");

    // Resize handle drag/click does not sort.
    let resize_edge = gpui::point(header.right() - gpui::px(3.0), header.center().y);
    visual.simulate_click(resize_edge, Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().table_order_by, "");
    });
}

#[gpui::test]
fn table_data_tab_keeps_editing_properties_and_footer(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        assert!(this.sql_sort_available(this.active_id()), "row editing/copy/sort are enabled");
        this.select_grid_cell(this.active_id(), crate::grid::CellPosition { row: 0, column: 1 });
        let edits = this.prepare_context_edits(this.active_id(), Some("renamed")).expect("editable name cell");
        let staged = this.stage_context_edits(this.active_id(), edits, Some("renamed"), cx).expect("staged");
        assert_eq!(staged, 1);
        assert_eq!(this.workspace.runtime(tab_id).unwrap().pending_edits.len(), 1);
    });
    settle(visual);
    // Footer + pagination still render for table-data tabs.
    assert!(visual.debug_bounds("page-next").is_some() || visual.debug_bounds("page-previous").is_some());
    // Pending edits block a re-run exactly like SQL result tabs.
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    app.read_with(visual, |this, _| {
        assert!(this.av().detail_ui.error.as_deref().is_some_and(|e| e.contains("Apply or discard")));
    });

    // Properties (Columns) presentation and the right panel open normally.
    app.update(visual, |this, cx| {
        this.av_mut().detail_ui.error = None;
        this.show_columns_presentation(tab_id, cx)
    });
    settle(visual);
    assert!(visual.debug_bounds("columns-view").is_some());
    assert!(visual.debug_bounds("properties-tab-strip").is_some());
    assert!(visual.debug_bounds("sql-result-toolbar").is_some(), "bar stays above Properties");
    app.update(visual, |this, cx| this.show_grid_presentation(tab_id, cx));
    settle(visual);
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), crate::grid::CellPosition { row: 0, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
    });
    settle(visual);
    assert!(visual.debug_bounds("cell-detail-header").is_some());
}

#[gpui::test]
fn restored_table_data_tab_keeps_where_and_order_by(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_table_app(cx);
    let restored = vec![crate::session::RestoredTab {
        source_id: "t1".into(),
        active: true,
        kind: "queryDraft".into(),
        connection_id: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: Some("public".into()),
        collection: "orders".into(),
        query_mode: Some(QueryMode::SqlTable),
        title: "orders".into(),
        sql: String::new(),
        output_limit: 100,
        is_pinned: false,
        where_clause: "id > 10 AND status = 'active'".into(),
        order_by: "created DESC".into(),
    }];
    app.update(visual, |this, cx| {
        let connections = this.saved_connections.clone();
        super::workspace::restore_tabs_into(&mut this.workspace, &connections, restored);
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::SqlTable));
        assert_eq!(tab.table_where, "id > 10 AND status = 'active'");
        assert_eq!(tab.table_order_by, "created DESC");
        assert_eq!(tab.sql, "SELECT * FROM \"public\".\"orders\"");
    });
    // The bar shows the restored fragments (single-line inputs).
    let (where_value, order_value) = app.read_with(visual, |this, cx| {
        (
            this.sql_table_ui.input_value(
                this.workspace.active_tab().unwrap().id,
                results::sql_table::SqlField::Where,
                cx,
            ),
            this.sql_table_ui.input_value(
                this.workspace.active_tab().unwrap().id,
                results::sql_table::SqlField::OrderBy,
                cx,
            ),
        )
    });
    assert_eq!(where_value.as_deref(), Some("id > 10 AND status = 'active'"));
    assert_eq!(order_value.as_deref(), Some("created DESC"));
    // Running applies both fragments.
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    settle(visual);
    let first = sql_table_first_id(&app, visual);
    assert_eq!(first, "239", "created DESC over active ids > 10");
}

fn navigator_snapshot(this: &AppRoot) -> Vec<(NavigatorNodeId, bool)> {
    this.navigator
        .visible_rows()
        .into_iter()
        .map(|row| (row.id, row.expanded))
        .collect()
}

#[gpui::test]
fn deleting_or_adding_a_connection_keeps_other_connections_expanded(cx: &mut TestAppContext) {
    // Postgres is expanded down to `orders` (selected by the open). The
    // post-delete refresh (`apply_saved_connections`, exactly what
    // `delete_connection` / add / edit run after storage changes) must not
    // collapse it. Scenario mode blocks the storage-backed delete itself.
    let (app, visual, _tab) = sql_table_app(cx);
    let postgres_id = crate::ui_fixture::POSTGRES_ID;
    let full = app.read_with(visual, |this, _| this.saved_connections.clone());
    assert_eq!(full.len(), 3);
    let before = app.read_with(visual, |this, _| navigator_snapshot(this));
    let postgres_rows = |rows: &[(NavigatorNodeId, bool)]| {
        rows.iter()
            .filter(|(id, _)| match id {
                NavigatorNodeId::Connection { connection }
                | NavigatorNodeId::Database { connection, .. }
                | NavigatorNodeId::Schema { connection, .. }
                | NavigatorNodeId::ObjectGroup { connection, .. }
                | NavigatorNodeId::Object { connection, .. }
                | NavigatorNodeId::SchemaObject { connection, .. }
                | NavigatorNodeId::Column { connection, .. }
                | NavigatorNodeId::Provider { connection, .. } => connection == postgres_id,
                NavigatorNodeId::Group { .. } => false,
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let before_postgres = postgres_rows(&before);
    assert!(before_postgres.len() > 6, "Postgres tree is open");
    let selected = app.read_with(visual, |this, _| this.navigator.selected().cloned());
    assert!(matches!(selected, Some(NavigatorNodeId::Object { .. })));

    // Delete the Mongo fixture connection.
    let without_mongo: Vec<_> = full
        .iter()
        .filter(|saved| saved.id.to_string() != crate::ui_fixture::MONGO_ID)
        .cloned()
        .collect();
    app.update(visual, |this, cx| {
        this.sidebar.path = None; // never touch real sidebar preferences
        this.apply_saved_connections(without_mongo, cx);
    });
    settle(visual);
    let after = app.read_with(visual, |this, _| navigator_snapshot(this));
    assert_eq!(postgres_rows(&after), before_postgres, "Postgres stays expanded");
    assert_eq!(after.len(), before.len() - 1, "only the Mongo row disappeared");
    assert_eq!(
        app.read_with(visual, |this, _| this.navigator.selected().cloned()),
        selected,
        "selection survives"
    );
    assert!(visual.debug_bounds("sql-result-toolbar").is_some(), "open table tab untouched");

    // Add it back: still nothing collapses, the new row starts collapsed.
    app.update(visual, |this, cx| this.apply_saved_connections(full.clone(), cx));
    settle(visual);
    let readded = app.read_with(visual, |this, _| navigator_snapshot(this));
    assert_eq!(postgres_rows(&readded), before_postgres);
    let mongo = readded
        .iter()
        .find(|(id, _)| matches!(id, NavigatorNodeId::Connection { connection } if connection == crate::ui_fixture::MONGO_ID))
        .expect("Mongo row is back");
    assert!(!mongo.1, "re-added connection is collapsed");

    // Deleting the Postgres connection itself removes only its subtree and
    // moves selection to a neighbour.
    let without_postgres: Vec<_> = full
        .iter()
        .filter(|saved| saved.id.to_string() != postgres_id)
        .cloned()
        .collect();
    app.update(visual, |this, cx| this.apply_saved_connections(without_postgres, cx));
    settle(visual);
    let gone = app.read_with(visual, |this, _| navigator_snapshot(this));
    assert!(postgres_rows(&gone).is_empty());
    assert_eq!(gone.len(), 2);
    let selected = app.read_with(visual, |this, _| this.navigator.selected().cloned());
    assert!(
        matches!(selected, Some(NavigatorNodeId::Connection { .. })),
        "selection moved to a sibling connection: {selected:?}"
    );
}

#[gpui::test]
fn clearing_while_changes_are_staged_keeps_the_pager_on_the_shown_page(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    sql_table_replace_text(visual, "sql-where-field", "id > 10");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let next = visual.debug_bounds("page-next").expect("next page button");
    visual.simulate_click(next.center(), Default::default());
    settle(visual);
    assert_eq!(sql_table_first_id(&app, visual), "211");

    // Stage an edit: re-runs are blocked until it is applied or discarded.
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), crate::grid::CellPosition { row: 0, column: 1 });
        let edits = this.prepare_context_edits(this.active_id(), Some("renamed")).expect("editable name cell");
        assert_eq!(this.stage_context_edits(this.active_id(), edits, Some("renamed"), cx).expect("staged"), 1);
    });
    settle(visual);

    let clear = visual.debug_bounds("sql-where-clear").expect("WHERE clear button");
    visual.simulate_click(clear.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.applied_where, "id > 10", "nothing is applied while changes are staged");
        assert_eq!(
            this.workspace.runtime(tab_id).unwrap().pagination.offset(),
            200,
            "the pager must keep describing the page the grid still shows"
        );
    });
    assert_eq!(sql_table_first_id(&app, visual), "211");
}

/// A narrow pane (split view, small window) must keep Run inside the capsule,
/// pinned to its right edge and left of the Columns control: the WHERE /
/// ORDER BY fields shrink and clip inside their own lane instead.
#[gpui::test]
fn the_run_button_stays_pinned_in_a_narrow_pane(cx: &mut TestAppContext) {
    let (_app, visual, _tab) = sql_table_app(cx);
    for width in [1400.0, 760.0, 600.0, 520.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
        visual.update(|window, _| window.refresh());
        settle(visual);
        let capsule = visual.debug_bounds("sql-query-capsule").expect("capsule");
        let run = visual.debug_bounds("sql-run").expect("run");
        let columns = visual.debug_bounds("sql-columns-button").expect("columns button");
        assert!(run.left() >= capsule.left() && run.right() <= capsule.right(), "{width}: Run inside the capsule");
        assert!(capsule.right() - run.right() <= gpui::px(12.0), "{width}: Run at the capsule's right edge");
        assert!(run.right() <= columns.left(), "{width}: Run left of Columns");
        let fields = visual.debug_bounds("sql-query-fields").expect("fields lane");
        assert!(fields.right() <= run.left(), "{width}: fields stay left of Run");
    }
}

/// A ClickHouse table named ``a\` UNION ALL SELECT ... -- `` would turn the tab's SELECT into another
/// one (a backslash escapes inside backticks there): the tab refuses to build a statement and says why.
#[gpui::test]
fn a_clickhouse_table_name_with_a_backslash_is_refused_in_the_tab_and_nothing_runs(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    let generation = app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().query.generation());
    app.update(visual, |this, cx| {
        let connection = this.workspace.tab(tab_id).unwrap().connection_id.clone();
        let mut capabilities = engine_driver_api::DriverCapabilities::new("clickhouse", "ClickHouse", Vec::new());
        capabilities.sql_dialect = Some(engine_driver_api::SqlDialectInfo {
            identifier_quote: "`".into(),
            string_literal_quote: "'".into(),
            supports_schemas: false,
            database_qualified_names: true,
            like_requires_text_cast: false,
            string_backslash_escapes: true,
            double_quote_identifiers: true,
            bracket_identifiers: false,
        });
        this.driver_runtime.capabilities.insert(connection, std::sync::Arc::new(capabilities));
        this.workspace.tab_mut(tab_id).unwrap().collection = "a\\` UNION ALL SELECT secret FROM vault -- ".into();
        this.run_active_tab_query(cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        let error = runtime.sql_table.error.as_deref().expect("the tab says why");
        assert!(error.contains("backslash") && error.contains("UNION ALL"), "{error}");
        assert_eq!(runtime.query.generation(), generation, "nothing was run");
    });
    assert!(visual.debug_bounds("sql-query-error").is_some(), "danger state in the bar");
}
