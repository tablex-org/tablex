// SQL table-data tabs: "Columns k/N" show / hide popover.

fn shown_columns(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    app.read_with(visual, |this, _| {
        (0..this.display_column_count(this.active_id()))
            .filter_map(|display| {
                let source = this.source_column_for_display(this.active_id(), display)?;
                this.active_grid().column_name(source).map(str::to_string)
            })
            .collect()
    })
}

fn open_columns_picker(visual: &mut gpui::VisualTestContext) {
    let button = visual.debug_bounds("sql-columns-button").expect("columns button");
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("sql-column-panel").is_some(), "picker opens");
}

fn query_generation(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> u64 {
    app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().query.generation())
}

const ALL_COLUMNS: [&str; 8] = [
    "id", "name", "status", "active", "created", "score", "big_number", "payload",
];

#[gpui::test]
fn columns_popover_hides_and_shows_columns_without_requerying(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    assert_eq!(shown_columns(&app, visual), ALL_COLUMNS);
    let generation = query_generation(&app, visual, tab_id);
    open_columns_picker(visual);
    for selector in [
        "sql-column-header",
        "sql-column-search",
        "sql-column-list",
        "sql-column-footer",
        "sql-column-count",
        "sql-column-row-0",
        "sql-column-checkbox-0",
        "sql-column-row-7",
    ] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(visual.debug_bounds("sql-column-tabs").is_none(), "no Visible / Projection tabs for SQL");
    assert!(visual.debug_bounds("grid-header-cell-7").is_some());

    // Uncheck `name` (row 1): its header leaves the grid immediately.
    click_selector(visual, "sql-column-row-1");
    assert_eq!(
        shown_columns(&app, visual),
        ["id", "status", "active", "created", "score", "big_number", "payload"]
    );
    assert!(visual.debug_bounds("grid-header-cell-6").is_some());
    assert!(visual.debug_bounds("grid-header-cell-7").is_none(), "one header fewer");
    assert!(visual.debug_bounds("sql-column-panel").is_some(), "toggling keeps the popover open");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.table_hidden_columns, ["name"]);
        // Display only: no query ran, the statement still selects every column.
        assert_eq!(this.workspace.runtime(tab_id).unwrap().query.generation(), generation);
        assert_eq!(this.sql_table_statement(tab).unwrap(), "SELECT * FROM \"public\".\"orders\"");
        assert_eq!(this.active_grid().column_count(), 8, "the result still has every column");
    });

    // Selection / copy / cell detail / sort follow the visible columns.
    app.update(visual, |this, cx| {
        let last = this.display_column_count(this.active_id()) - 1;
        this.av().grid_ui.selection.borrow_mut().select(crate::grid::CellPosition { row: 0, column: 0 });
        this.av().grid_ui
            .selection
            .borrow_mut()
            .extend_to(crate::grid::CellPosition { row: 1, column: last });
        let range = this.av().grid_ui.selection.borrow().range.unwrap();
        let data = this.collect_copy_context_range(this.active_id(), range).expect("copy data");
        assert_eq!(data.columns, ["id", "status", "active", "created", "score", "big_number", "payload"]);
        let tsv = data.format(super::results::copy::CopyFormat::TsvWithHeaders).unwrap();
        let header = tsv.lines().next().unwrap();
        assert_eq!(header.split('\t').count(), 7, "{header}");
        assert!(!header.contains("name"), "hidden column is not copied: {header}");
        // Cell detail of display column 1 is `status`, not the hidden `name`.
        this.select_grid_cell(this.active_id(), crate::grid::CellPosition { row: 0, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.selected_cell_detail(this.active_id()).unwrap().column_name, "status");
    });
    // Header sort of display column 1 sorts by `status`.
    visual.simulate_keystrokes("escape");
    settle(visual);
    let header = visual.debug_bounds("grid-header-cell-1").expect("status header");
    visual.simulate_click(header.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().applied_order_by, "\"status\" ASC");
    });
    // Sorting re-ran the query (a header sort always does); hidden stays hidden.
    assert_eq!(shown_columns(&app, visual).len(), 7);
    assert_eq!(app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().table_hidden_columns.clone()), ["name"]);

    // Re-check `name`: all columns are back, no re-query for the toggle itself.
    open_columns_picker(visual);
    let generation = query_generation(&app, visual, tab_id);
    click_selector(visual, "sql-column-row-1");
    assert_eq!(shown_columns(&app, visual), ALL_COLUMNS);
    assert_eq!(query_generation(&app, visual, tab_id), generation);
    assert!(visual.debug_bounds("grid-header-cell-7").is_some());
}

#[gpui::test]
fn columns_popover_search_filters_rows_and_toggles_the_filtered_column(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    open_columns_picker(visual);
    click_selector(visual, "sql-column-search");
    visual.simulate_input("big");
    settle(visual);
    assert!(visual.debug_bounds("sql-column-row-0").is_some());
    assert!(visual.debug_bounds("sql-column-row-1").is_none(), "only `big_number` matches");
    // Row 0 of the filtered list is `big_number`, not `id`.
    click_selector(visual, "sql-column-row-0");
    let shown = shown_columns(&app, visual);
    assert!(!shown.contains(&"big_number".to_string()) && shown.contains(&"id".to_string()), "{shown:?}");
    assert_eq!(shown.len(), 7);
    // The count in the header is over the full list, not the filtered rows.
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().unwrap().table_hidden_columns,
            ["big_number"]
        );
    });
    assert_eq!(
        super::results::column_picker::columns_button_label(7, 8),
        "Columns 7/8"
    );
}

#[gpui::test]
fn columns_popover_select_all_and_none_keep_one_column(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    open_columns_picker(visual);
    click_selector(visual, "sql-column-select-none");
    assert_eq!(shown_columns(&app, visual), ["id"], "Select none keeps the first column");
    // The last visible column cannot be hidden.
    click_selector(visual, "sql-column-row-0");
    assert_eq!(shown_columns(&app, visual), ["id"]);
    // Showing another and hiding `id` works.
    click_selector(visual, "sql-column-row-2");
    assert_eq!(shown_columns(&app, visual), ["id", "status"]);
    click_selector(visual, "sql-column-row-0");
    assert_eq!(shown_columns(&app, visual), ["status"]);
    click_selector(visual, "sql-column-select-all");
    assert_eq!(shown_columns(&app, visual), ALL_COLUMNS);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.active_tab().unwrap().table_hidden_columns.is_empty());
    });
}

#[gpui::test]
fn hidden_columns_persist_per_tab_and_apply_to_the_restored_result(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    open_columns_picker(visual);
    click_selector(visual, "sql-column-row-1");
    click_selector(visual, "sql-column-row-4");
    let snapshot = app.read_with(visual, |this, _| this.workspace.snapshot());
    let saved = snapshot.tabs.iter().find(|tab| tab.collection == "orders").unwrap();
    assert_eq!(saved.table_hidden_columns, ["name", "created"]);

    // On-disk format round trip, then restore into the running workspace and
    // re-run: the restored tab shows the same columns.
    let json = crate::gpui_session::encode(&snapshot).expect("encode");
    let decoded: crate::gpui_session::GpuiSession = serde_json::from_slice(&json).expect("decode");
    app.update(visual, |this, cx| {
        this.workspace.restore_snapshot(&decoded, |_| true);
        this.sql_table_ui = Default::default();
        this.reset_active_view();
        this.run_active_tab_query(cx);
    });
    settle(visual);
    assert_eq!(
        shown_columns(&app, visual),
        ["id", "status", "active", "score", "big_number", "payload"]
    );
    assert!(visual.debug_bounds("grid-header-cell-5").is_some());
    assert!(visual.debug_bounds("grid-header-cell-6").is_none());
    // A second tab keeps its own (empty) hidden set.
    app.update(visual, |this, cx| {
        let id = this.workspace.open_query_tab_with_metadata(
            "orders-2",
            crate::workspace::TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                query_mode: Some(QueryMode::SqlTable),
                ..Default::default()
            },
        );
        assert!(this.workspace.tab(id).unwrap().table_hidden_columns.is_empty());
        cx.notify();
    });
}

#[gpui::test]
fn properties_lists_every_column_including_hidden_ones(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        this.toggle_sql_column(tab_id, "name".into(), cx);
        this.toggle_sql_column(tab_id, "payload".into(), cx);
    });
    settle(visual);
    app.update(visual, |this, cx| this.show_columns_presentation(tab_id, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        let items = this.build_column_items(this.active_id());
        assert_eq!(items.len(), 8, "Properties describes the whole table");
        let hidden: Vec<&str> = items
            .iter()
            .filter(|item| item.hidden)
            .map(|item| item.name.as_str())
            .collect();
        assert_eq!(hidden, ["name", "payload"]);
        // Shown columns keep their display index; hidden ones get placeholders
        // beyond the grid's column count so nothing can select them.
        let shown_indices: Vec<usize> = items.iter().filter(|i| !i.hidden).map(|i| i.display_index).collect();
        assert_eq!(shown_indices, [0, 1, 2, 3, 4, 5]);
        assert!(items.iter().filter(|i| i.hidden).all(|i| i.display_index >= this.display_column_count(this.active_id())));
        let mut unique: Vec<usize> = items.iter().map(|i| i.display_index).collect();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 8, "display indices stay unique");
    });
    assert!(visual.debug_bounds("columns-view-row").is_some());
    // Clicking a hidden row does not select it; arrow keys skip it.
    app.update(visual, |this, cx| {
        let hidden_row = this.build_column_items(this.active_id()).into_iter().find(|i| i.hidden).unwrap();
        this.select_column_row(this.active_id(), hidden_row.display_index, cx);
        assert_eq!(this.workspace.runtime(tab_id).unwrap().columns_panel.selected, None);
    });
}

#[gpui::test]
fn column_visibility_is_locked_while_changes_are_staged(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), crate::grid::CellPosition { row: 0, column: 1 });
        let edits = this.prepare_context_edits(this.active_id(), Some("renamed")).expect("editable name cell");
        this.stage_context_edits(this.active_id(), edits, Some("renamed"), cx).expect("staged");
    });
    settle(visual);
    open_columns_picker(visual);
    click_selector(visual, "sql-column-row-0");
    click_selector(visual, "sql-column-select-none");
    assert_eq!(shown_columns(&app, visual), ALL_COLUMNS, "pending edits are keyed by display column");
    app.read_with(visual, |this, _| {
        assert!(this.workspace.active_tab().unwrap().table_hidden_columns.is_empty());
        assert_eq!(this.workspace.runtime(tab_id).unwrap().pending_edits.len(), 1);
    });
}

#[gpui::test]
fn selection_stats_follow_the_visible_columns(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    let stats_of_display_column = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, column: usize| {
        app.update(visual, |this, _| {
            this.av().grid_ui.selection.borrow_mut().select(crate::grid::CellPosition { row: 0, column });
            this.av().grid_ui
                .selection
                .borrow_mut()
                .extend_to(crate::grid::CellPosition { row: 3, column });
            this.selection_stats(tab_id).map(|stats| stats.label())
        })
    };
    // Display column 4 is `created` (dates: nothing numeric) with all columns shown.
    let before = stats_of_display_column(&app, visual, 4).expect("stats");
    assert!(!before.contains("SUM"), "{before}");
    // Hide `name`: display column 4 now reads `score` (0, 0.1, 0.2, 0.3).
    app.update(visual, |this, cx| this.toggle_sql_column(tab_id, "name".into(), cx));
    settle(visual);
    let after = stats_of_display_column(&app, visual, 4).expect("stats");
    assert!(after.contains("SUM: ≈0.6"), "stats must read the displayed column, not a stale cache: {after}");
}
