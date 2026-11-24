// Big values and big pages in the results grid: the inline editor of a huge
// cell, keyboard paging over a large page, select all, and a page export
// larger than the clipboard limit.

/// A one-tab result `id` (integer key) + `memo` (text) holding `memo(row)`.
fn big_page_app(
    cx: &mut TestAppContext,
    rows: usize,
    memo: impl Fn(usize) -> String,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = visual.cx.read(|cx| {
        root.read(cx).view().clone().downcast::<AppRoot>().expect("root holds AppRoot")
    });
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("memo", arrow_schema::DataType::Utf8, false),
    ]));
    let ids = arrow_array::Int64Array::from_iter_values(0..rows as i64);
    let memos = arrow_array::StringArray::from_iter_values((0..rows).map(memo));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(ids) as arrow_array::ArrayRef,
            std::sync::Arc::new(memos) as arrow_array::ArrayRef,
        ],
    )
    .expect("valid batch");
    visual.simulate_resize(gpui::size(px(900.0), px(600.0)));
    visual.update(|window, _| window.activate_window());
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.open_ran_query_tab("test");
        this.workspace.runtime_mut(tab_id).expect("query runtime").structure_columns = vec![
            crate::catalog::CatalogColumn {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
            crate::catalog::CatalogColumn {
                name: "memo".into(),
                data_type: "text".into(),
                nullable: false,
                primary_key: false,
                ..Default::default()
            },
        ];
        this.reset_active_view();
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    (app, visual)
}

fn pending_memo(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, row: usize) -> Option<String> {
    app.read_with(visual, |this, _| {
        let tab = this.active_id();
        this.workspace
            .runtime(tab)
            .and_then(|runtime| runtime.pending_cell_text(CellPosition { row, column: 1 }).map(|text| text.to_string()))
    })
}

fn open_memo_editor(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, row: usize) {
    app.update_in(visual, |this, window, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row, column: 1 });
        this.open_cell_editor(this.active_id(), window, cx);
    });
    visual.run_until_parked();
}

#[gpui::test]
fn a_huge_cell_is_edited_in_the_value_panel_and_saved_whole(cx: &mut TestAppContext) {
    let json = format!(
        "[{}]",
        (0..60_000).map(|i| format!("{{\"k\":{i}}}")).collect::<Vec<_>>().join(",")
    );
    assert!(json.len() > 600_000);
    let (app, visual) = big_page_app(cx, 2, |_| json.clone());
    open_memo_editor(&app, visual, 0);
    app.read_with(visual, |this, cx| {
        let view = this.view(this.active_id());
        assert!(view.grid_ui.inline_edit.is_none(), "no inline textarea is built for the big value");
        let edit = view.detail_ui.edit.as_ref().expect("the Value panel's editor opened");
        assert_eq!(edit.position, CellPosition { row: 0, column: 1 });
        assert_eq!(edit.editor.value(cx).as_ref(), json, "the editor holds the whole value");
        assert!(this.workspace.runtime(this.active_id()).unwrap().cell_detail.open, "the panel is open");
    });
    let edited = format!("{json} ");
    app.update_in(visual, |this, window, cx| {
        let edit = this.view(this.active_id()).detail_ui.edit.as_ref().unwrap();
        edit.editor.set_value(&edited, window, cx);
        this.commit_cell_detail_editor(this.active_id(), cx);
    });
    assert_eq!(pending_memo(&app, visual, 0).as_deref(), Some(edited.as_str()), "saved exactly, nothing cut");
    assert_eq!(pending_memo(&app, visual, 1), None);
}

#[gpui::test]
fn the_inline_editor_keeps_values_up_to_the_size_limit(cx: &mut TestAppContext) {
    use crate::app::results::editing::INLINE_EDIT_MAX_BYTES;
    let (app, visual) = big_page_app(cx, 2, |row| {
        if row == 0 { "a".repeat(INLINE_EDIT_MAX_BYTES) } else { "a".repeat(INLINE_EDIT_MAX_BYTES + 1) }
    });
    open_memo_editor(&app, visual, 0);
    app.read_with(visual, |this, cx| {
        let view = this.view(this.active_id());
        let edit = view.grid_ui.inline_edit.as_ref().expect("a value at the limit opens in its cell");
        assert!(edit.floating, "a long line takes the textarea without being measured");
        assert_eq!(edit.editor.value(cx).len(), INLINE_EDIT_MAX_BYTES);
        assert!(view.detail_ui.edit.is_none());
    });
    app.update_in(visual, |this, window, cx| this.cancel_inline_edit(this.active_id(), window, cx));
    open_memo_editor(&app, visual, 1);
    app.read_with(visual, |this, _| {
        let view = this.view(this.active_id());
        assert!(view.grid_ui.inline_edit.is_none());
        assert!(view.detail_ui.edit.is_some(), "one byte over the limit goes to the panel");
    });
}

fn cell_selector(row: usize) -> &'static str {
    Box::leak(format!("grid-body-cell-{row}-0").into_boxed_str())
}

fn selected_end(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> CellPosition {
    app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range.unwrap().end)
}

fn list_offset_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> f32 {
    app.read_with(visual, |this, _| {
        -f32::from(this.av().grid_scroll.0.borrow().base_handle.offset().y) / theme::ROW_HEIGHT
    })
}

#[gpui::test]
fn page_keys_home_end_and_select_all_move_through_a_100k_row_page(cx: &mut TestAppContext) {
    const ROWS: usize = 100_000;
    let (app, visual) = big_page_app(cx, ROWS, |row| format!("m{row}"));
    visual.simulate_resize(gpui::size(px(900.0), px(1400.0)));
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let first = visual.debug_bounds("grid-body-cell-0-0").expect("first cell");
    visual.simulate_click(first.center(), Default::default());
    visual.run_until_parked();
    assert_eq!(selected_end(&app, visual), CellPosition { row: 0, column: 0 });
    let page = app.read_with(visual, |this, _| this.av().grid_page_rows());
    assert!(page > 5, "a screenful of rows, got {page}");

    visual.simulate_keystrokes("pagedown");
    visual.run_until_parked();
    assert_eq!(selected_end(&app, visual), CellPosition { row: page, column: 0 });
    visual.simulate_keystrokes("pagedown");
    visual.run_until_parked();
    assert_eq!(selected_end(&app, visual), CellPosition { row: 2 * page, column: 0 });
    let offset = list_offset_rows(&app, visual);
    assert!(offset > page as f32 - 1.0, "the list scrolled with the selection, offset {offset}");
    assert!(visual.debug_bounds(cell_selector(2 * page)).is_some(), "the selected row is drawn");
    visual.simulate_keystrokes("pageup");
    visual.run_until_parked();
    assert_eq!(selected_end(&app, visual), CellPosition { row: page, column: 0 });

    visual.simulate_keystrokes("end");
    assert_eq!(selected_end(&app, visual), CellPosition { row: page, column: 1 }, "End: last column of the row");
    visual.simulate_keystrokes("home");
    assert_eq!(selected_end(&app, visual), CellPosition { row: page, column: 0 }, "Home: first column of the row");

    visual.simulate_keystrokes("secondary-end");
    visual.run_until_parked();
    assert_eq!(selected_end(&app, visual), CellPosition { row: ROWS - 1, column: 0 });
    let offset = list_offset_rows(&app, visual);
    assert!(offset > (ROWS - page - 2) as f32, "scrolled to the end, offset {offset}");
    assert!(visual.debug_bounds(cell_selector(ROWS - 1)).is_some());
    visual.simulate_keystrokes("pagedown");
    assert_eq!(selected_end(&app, visual).row, ROWS - 1, "PageDown stops at the last row");

    visual.simulate_keystrokes("secondary-home");
    visual.run_until_parked();
    assert_eq!(selected_end(&app, visual), CellPosition { row: 0, column: 0 });
    assert_eq!(list_offset_rows(&app, visual), 0.0);

    // Shift extends the range from the anchor.
    visual.simulate_keystrokes("shift-pagedown");
    app.read_with(visual, |this, _| {
        let range = this.av().grid_ui.selection.borrow().range.unwrap().normalized();
        assert_eq!((range.start.row, range.end.row), (0, page));
    });

    // Select all.
    visual.simulate_keystrokes("secondary-a");
    visual.run_until_parked();
    app.read_with(visual, |this, _| {
        let range = this.av().grid_ui.selection.borrow().range.unwrap().normalized();
        assert_eq!(range.start, CellPosition { row: 0, column: 0 });
        assert_eq!(range.end, CellPosition { row: ROWS - 1, column: 1 });
    });
}

#[gpui::test]
fn the_vertical_scrollbar_drags_the_list(cx: &mut TestAppContext) {
    let (app, visual) = big_page_app(cx, 100_000, |row| format!("m{row}"));
    let strip = visual.debug_bounds("grid-vertical-scrollbar").expect("the vertical scrollbar");
    let body = visual.debug_bounds("grid-body-cell-0-1").expect("a cell");
    assert!(strip.origin.x > body.origin.x, "the bar is at the right edge");
    assert_eq!(list_offset_rows(&app, visual), 0.0);
    let grab = gpui::point(strip.origin.x + strip.size.width - px(6.0), strip.origin.y + px(10.0));
    visual.simulate_event(gpui::MouseMoveEvent { position: grab, pressed_button: None, modifiers: Default::default() });
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: grab,
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    let to = gpui::point(grab.x, strip.origin.y + strip.size.height / 2.0);
    visual.simulate_mouse_move(to, MouseButton::Left, Default::default());
    visual.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: to,
        modifiers: Default::default(),
        click_count: 1,
    });
    visual.run_until_parked();
    let offset = list_offset_rows(&app, visual);
    assert!(offset > 20_000.0, "dragging the thumb to the middle scrolls to about row 50k, offset {offset}");
}

#[gpui::test]
fn export_of_a_page_over_the_copy_limit_goes_to_the_file(cx: &mut TestAppContext) {
    use crate::app::results::copy::COPY_MAX_BYTES;
    // 200 rows of 100 KB: 20 MB, over the 16 MiB clipboard limit.
    let memo = |row: usize| format!("{row}-{}", "x".repeat(100_000));
    let (app, visual) = big_page_app(cx, 200, memo);
    let dir = std::env::temp_dir().join(format!("tablex-big-export-{}", uuid::Uuid::new_v4()));
    app.update(visual, |this, _| std::sync::Arc::make_mut(&mut this.settings).results.export.folder = dir.display().to_string());
    app.read_with(visual, |this, _| {
        let tab = this.active_id();
        let all = crate::grid::CellRange {
            start: CellPosition { row: 0, column: 0 },
            end: CellPosition { row: 199, column: 1 },
        };
        let error = this.collect_copy_context_range(tab, all).err().expect("a copy is limited");
        assert!(error.contains("16 MiB"), "{error}");
    });
    for (format, extension) in [
        (results::export::ContextExportFormat::CsvWithHeaders, "csv"),
        (results::export::ContextExportFormat::Json, "json"),
    ] {
        app.update_in(visual, |this, window, cx| this.export_context_selection(this.active_id(), format, true, window, cx));
        // The click has returned: the file is written by the background executor, not by the handler.
        assert!(
            !dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none(),
            "{extension}: nothing is written before the UI thread is free again"
        );
        visual.run_until_parked();
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == extension))
            .unwrap_or_else(|| panic!("a .{extension} file in the export folder"));
        let size = std::fs::metadata(&file).unwrap().len() as usize;
        assert!(size > COPY_MAX_BYTES, "{extension}: {size} bytes written");
        let text = std::fs::read_to_string(&file).unwrap();
        if extension == "csv" {
            assert!(text.starts_with("id,memo\n0,0-xxxx"));
            assert_eq!(text.lines().count(), 201, "a header and every row");
            assert!(text.ends_with(&format!("199,199-{}", "x".repeat(100_000))), "the last row is whole");
        } else {
            let rows: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(rows.as_array().unwrap().len(), 200);
        }
        std::fs::remove_file(file).unwrap();
    }
    std::fs::remove_dir_all(dir).unwrap();
}
