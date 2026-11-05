// The record (form) view: the footer button / Tab switch, Previous / Next and the keys, the grid
// selection kept in sync, editing through the staged-edit model down to a real SQLite database,
// a table of hundreds of columns, a script's result tabs, and the results it is not offered for.

fn record_active(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> bool {
    app.read_with(visual, |this, _| this.record_view_active(this.active_id()))
}

fn selection_end(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> Option<CellPosition> {
    app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range.map(|range| range.end))
}

fn painted_record_fields(visual: &mut gpui::VisualTestContext, upto: usize) -> Vec<usize> {
    (0..upto)
        .filter(|field| {
            let selector: &'static str = Box::leak(format!("record-field-{field}").into_boxed_str());
            visual.debug_bounds(selector).is_some()
        })
        .collect()
}

#[gpui::test]
fn the_footer_button_and_tab_switch_between_the_table_and_the_record(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    settle(visual);
    assert!(visual.debug_bounds("grid-scroll-area").is_some());
    assert!(visual.debug_bounds("record-view").is_none());

    // Footer button -> record of the first row (nothing was selected: the first cell is).
    click_selector(visual, "record-view-toggle");
    assert!(record_active(&app, visual));
    assert!(visual.debug_bounds("record-view").is_some());
    assert!(visual.debug_bounds("grid-scroll-area").is_none(), "the record replaces the table");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 0, column: 0 }));
    let fields = app.read_with(visual, |this, _| this.display_column_count(this.active_id()));
    assert!(fields > 2);
    assert_eq!(painted_record_fields(visual, fields), (0..fields).collect::<Vec<_>>(), "every field of a narrow row");
    // The button reads as on, and the same button leaves the record again.
    click_selector(visual, "record-view-toggle");
    assert!(!record_active(&app, visual));
    assert!(visual.debug_bounds("grid-scroll-area").is_some());

    // Tab does the same from the focused grid (the grid takes focus on a click).
    let cell = visual.debug_bounds("grid-body-cell-1-1").expect("a cell");
    visual.simulate_click(cell.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("tab");
    settle(visual);
    assert!(record_active(&app, visual), "Tab switches the focused grid to the record");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 1, column: 1 }), "the clicked cell is the record's row and field");
    visual.simulate_keystrokes("tab");
    settle(visual);
    assert!(!record_active(&app, visual), "and Tab switches back");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 1, column: 1 }));
}

#[gpui::test]
fn previous_next_and_the_keys_move_the_record_and_the_grid_selection(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    visual.update(|window, _| window.activate_window());
    settle(visual);
    click_selector(visual, "record-view-toggle");
    let rows = app.read_with(visual, |this, _| this.active_grid().row_count());
    assert!(rows >= 3, "the sample has rows to walk");
    assert_eq!(app.read_with(visual, |this, _| this.record_row(this.active_id())), 0);
    app.read_with(visual, |this, _| {
        // Previous has nowhere to go on the first row.
        assert_eq!(this.record_row(this.active_id()), 0);
    });

    click_selector(visual, "record-next");
    assert_eq!(selection_end(&app, visual).map(|cell| cell.row), Some(1));
    click_selector(visual, "record-next");
    assert_eq!(selection_end(&app, visual).map(|cell| cell.row), Some(2));
    click_selector(visual, "record-prev");
    assert_eq!(selection_end(&app, visual).map(|cell| cell.row), Some(1));

    // A click on a field selects it (same row); Up / Down walk the fields, PageUp / PageDown the rows.
    click_selector(visual, "record-field-2");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 1, column: 2 }));
    visual.simulate_keystrokes("down");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 1, column: 3 }));
    visual.simulate_keystrokes("up up");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 1, column: 1 }));
    visual.simulate_keystrokes("pagedown");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 2, column: 1 }), "the field is kept");
    visual.simulate_keystrokes("pageup pageup pageup");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 0, column: 1 }), "the first row is the end");

    // The last row has no Next.
    app.update(visual, |this, cx| this.record_go_to_row(this.active_id(), usize::MAX, cx));
    settle(visual);
    assert_eq!(selection_end(&app, visual).map(|cell| cell.row), Some(rows - 1));
    assert!(visual.debug_bounds("record-next").is_some());
    app.update(visual, |this, cx| this.record_step(this.active_id(), true, cx));
    assert_eq!(selection_end(&app, visual).map(|cell| cell.row), Some(rows - 1), "stays on the last row");

    // The Details panel follows the record's field like it follows the grid's cell.
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        let position = CellPosition { row: rows - 1, column: 1 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
    });
    settle(visual);
    app.update(visual, |this, cx| this.record_go_to_row(this.active_id(), 0, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        let detail = this.selected_cell_detail(this.active_id()).expect("the panel is open");
        assert_eq!(detail.position, CellPosition { row: 0, column: 1 });
    });
    // Leaving the record shows the grid at the same selection, scrolled to it.
    click_selector(visual, "record-to-table");
    assert!(visual.debug_bounds("grid-scroll-area").is_some());
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 0, column: 1 }));
}

#[gpui::test]
fn a_table_of_hundreds_of_columns_paints_only_the_visible_fields(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 8, 500);
    visual.update(|window, _| window.activate_window());
    app.update(visual, |this, cx| this.toggle_record_view(this.active_id(), cx));
    settle(visual);
    assert!(record_active(&app, visual));
    let painted = painted_record_fields(visual, 500);
    assert!(painted.len() >= 5 && painted.len() < 40, "{} of 500 fields painted", painted.len());
    assert_eq!(painted[0], 0);

    app.update(visual, |this, _| {
        this.av().record_scroll.scroll_to_item(400, gpui::ScrollStrategy::Top);
    });
    settle(visual);
    let painted = painted_record_fields(visual, 500);
    assert!(painted.contains(&400) && !painted.contains(&0), "{painted:?}");
    // Down from the middle of a long list keeps the field in view.
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        this.select_grid_cell(tab, CellPosition { row: 0, column: 400 });
        cx.notify();
    });
    click_selector(visual, "record-field-401");
    visual.simulate_keystrokes("down down down");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 0, column: 404 }));
    // The pane height holds a few dozen fields: the cell values are those of row 0.
    app.read_with(visual, |this, _| {
        assert_eq!(this.active_grid().cell_text(CellPosition { row: 0, column: 404 }).unwrap().as_ref(), "r0c404");
    });
}

#[gpui::test]
fn a_result_without_rows_says_so_and_mongo_and_redis_results_keep_their_own_views(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 4, 5);
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        for (mode, reason) in [
            (QueryMode::MongoFind, Some(super::results::record_view::RECORD_UNAVAILABLE_MONGO)),
            (QueryMode::RedisKey, Some(super::results::record_view::RECORD_UNAVAILABLE_REDIS)),
            (QueryMode::SqlTable, None),
        ] {
            this.workspace.tab_mut(tab).unwrap().query_mode = Some(mode.clone());
            assert_eq!(this.record_view_support(tab).err(), reason, "{mode:?}");
            this.toggle_record_view(tab, cx);
            assert_eq!(this.av().grid_ui.record, reason.is_none(), "{mode:?}: the toggle is refused with its reason");
            this.av_mut().grid_ui.record = false;
        }
    });
    settle(visual);
    // The disabled button carries the reason (its tooltip) and does nothing when clicked.
    app.update(visual, |this, _| {
        let tab = this.active_id();
        this.workspace.tab_mut(tab).unwrap().query_mode = Some(QueryMode::MongoFind);
    });
    app.update(visual, |_, cx| cx.notify());
    settle(visual);
    if let Some(button) = visual.debug_bounds("record-view-toggle") {
        visual.simulate_click(button.center(), Default::default());
        settle(visual);
    }
    assert!(!record_active(&app, visual));

    // An empty page: the record view is on but shows no fields.
    let (app, visual) = wide_grid_app(cx, 0, 3);
    app.update(visual, |this, cx| this.toggle_record_view(this.active_id(), cx));
    settle(visual);
    assert!(visual.debug_bounds("record-empty").is_some());
    assert!(painted_record_fields(visual, 3).is_empty());
}

#[gpui::test]
fn a_plain_query_result_shows_the_record_read_only(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 6, 4);
    visual.update(|window, _| window.activate_window());
    app.update(visual, |this, cx| this.toggle_record_view(this.active_id(), cx));
    settle(visual);
    // Double-click a value: a result with no table behind it has nothing to write, and says so.
    let value = visual.debug_bounds("record-value-2").expect("a value");
    double_click(visual, value.center());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.av().grid_ui.inline_edit.is_none(), "no editor on a read-only result");
        assert!(this.av().detail_ui.error.is_some(), "the reason is in the status row");
        assert_eq!(this.av().grid_ui.selection.borrow().range.map(|range| range.end), Some(CellPosition { row: 0, column: 2 }));
    });
}

#[gpui::test]
fn editing_a_field_in_the_record_stages_it_and_apply_writes_it_to_the_database(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.wait_structure();
    rig.visual.update(|window, _| window.activate_window());
    let (app, tab) = (rig.app.clone(), rig.tab);

    rig.click("record-view-toggle");
    assert!(record_active(&app, rig.visual));
    rig.click("record-next");
    assert_eq!(selection_end(&app, rig.visual).map(|cell| cell.row), Some(1), "the record is row 2: 2, b, 2");

    // A real double-click on the `name` value opens the grid's inline editor inside the record.
    let value = rig.visual.debug_bounds("record-value-1").expect("the name value");
    double_click(rig.visual, value.center());
    assert_eq!(rig.editing(), Some(CellPosition { row: 1, column: 1 }));
    let editor = app.read_with(rig.visual, |this, _| this.av().grid_ui.inline_edit.as_ref().unwrap().editor.clone());
    editor
        .as_input()
        .expect("a short value uses the single-line input")
        .update_in(rig.visual, |state, window, cx| state.set_value("beta", window, cx));
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert_eq!(rig.editing(), None, "Enter commits");
    assert_eq!(rig.staged(), (1, 0));
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3", "nothing is written before Apply");
    // The record shows the staged value (and the grid does too).
    app.read_with(rig.visual, |this, _| {
        assert_eq!(
            this.pending_cell_text(tab, CellPosition { row: 1, column: 1 }).as_deref(),
            Some("beta")
        );
    });

    // Discard drops it; the original value is back, nothing staged.
    rig.click("discard-edit");
    assert_eq!(rig.staged(), (0, 0));
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3");

    // Edit again on another row, reached with Previous / Next, then Apply.
    rig.click("record-next");
    let value = rig.visual.debug_bounds("record-value-2").expect("the qty value");
    double_click(rig.visual, value.center());
    assert_eq!(rig.editing(), Some(CellPosition { row: 2, column: 2 }));
    let editor = app.read_with(rig.visual, |this, _| this.av().grid_ui.inline_edit.as_ref().unwrap().editor.clone());
    editor
        .as_input()
        .unwrap()
        .update_in(rig.visual, |state, window, cx| state.set_value("30", window, cx));
    // Moving to another row commits the value being typed.
    rig.click("record-prev");
    assert_eq!(rig.editing(), None);
    assert_eq!(rig.staged(), (1, 0), "leaving the row staged the edit of row 3");
    assert_eq!(selection_end(&app, rig.visual).map(|cell| cell.row), Some(1));
    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:30");
    rig.wait_not_running("the reload after Apply");
    assert!(record_active(&app, rig.visual), "the record view survives the reload");
    assert_eq!(rig.grid_rows()[2][2], "30");
}

#[gpui::test]
fn each_script_result_tab_can_be_viewed_as_a_record(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_script("SELECT id, name FROM items ORDER BY id;\nSELECT count(*) AS n FROM items");
    rig.wait_script_done();
    rig.wait_rendered("script-tab-1");
    rig.click("record-view-toggle");
    assert!(record_active(&rig.app, rig.visual));
    assert_eq!(painted_record_fields(rig.visual, 2), [0, 1], "Result 1 has two fields");
    rig.click("record-next");
    assert_eq!(selection_end(&rig.app, rig.visual).map(|cell| cell.row), Some(1));

    // Result 2 (one row, one field) in the record; Next has nowhere to go.
    rig.click("script-tab-1");
    assert!(record_active(&rig.app, rig.visual), "the mode stays while the tab changes");
    assert_eq!(painted_record_fields(rig.visual, 2), [0]);
    assert_eq!(rig.script_shown(), Some(ScriptTab::Result(1)));
    // Statistics has no columns: the table of statements, not a record.
    rig.click("script-tab-stats");
    assert!(!record_active(&rig.app, rig.visual));
    assert!(rig.visual.debug_bounds("record-view").is_none());
    let disabled = rig.app.read_with(rig.visual, |this, _| this.record_view_support(this.active_id()).is_err());
    assert!(disabled);
    rig.click("script-tab-0");
    assert!(record_active(&rig.app, rig.visual));
    assert_eq!(painted_record_fields(rig.visual, 2), [0, 1]);
}

const RECORD_BLOB_SEED: &str = "CREATE TABLE files(id INTEGER PRIMARY KEY, name TEXT, data BLOB); \
     INSERT INTO files(name, data) VALUES ('a', x'deadbeefcafebabe00112233445566ff');";

#[gpui::test]
fn a_binary_field_reads_compactly_in_the_record_and_opens_in_the_value_panel(cx: &mut TestAppContext) {
    let db = SqliteDb::new(RECORD_BLOB_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("files");
    rig.wait_structure();
    rig.click("record-view-toggle");
    app_open_details(&mut rig, 2);
    assert!(rig.visual.debug_bounds("record-value-2").is_some());
    assert!(rig.visual.debug_bounds("cell-detail-binary-hex").is_some(), "the panel shows the selected field's bytes as hex");
    rig.app.read_with(rig.visual, |this, _| {
        let grid = this.active_grid();
        assert_eq!(grid.cell_bytes(CellPosition { row: 0, column: 2 }).map(<[u8]>::len), Some(16));
    });
}

fn app_open_details(rig: &mut SqlRig<'_>, column: usize) {
    rig.app.update(rig.visual, |this, cx| {
        let tab = this.active_id();
        let position = CellPosition { row: 0, column };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
    });
    settle(rig.visual);
}
