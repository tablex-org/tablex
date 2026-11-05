// Cmd+F find bar for the results grid. Interaction tests drive real
// keystrokes / clicks through a `VisualTestContext`; the ignored perf tests
// (run on demand, release) report the 100k x 20 scan time and prove the
// per-frame highlight lookup allocates nothing:
//
//   cargo test --release -p tablex-gpui grid_find_perf -- --ignored --nocapture --test-threads=1

use crate::grid::CellPosition as FindCell;

const FIND_ROWS: usize = 60;
const FIND_COLUMNS: usize = 20;

fn find_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = wide_grid_app(cx, FIND_ROWS, FIND_COLUMNS);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    (app, visual)
}

/// Lets the 150 ms debounce fire, the background scan finish and the result
/// render (twice: scrolling settles on the second pass).
fn find_flush(visual: &mut gpui::VisualTestContext) {
    visual
        .executor()
        .advance_clock(Duration::from_millis(200));
    settle(visual);
}

fn find_open(visual: &mut gpui::VisualTestContext) {
    visual.simulate_keystrokes("secondary-f");
    settle(visual);
}

fn find_type(visual: &mut gpui::VisualTestContext, text: &str) {
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    find_flush(visual);
}

fn find_counter(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> String {
    app.read_with(visual, |this, _| this.av().grid_ui.find.counter_text())
}

fn find_selected(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> Option<FindCell> {
    app.read_with(visual, |this, _| {
        this.av().grid_ui.selection.borrow().range.map(|range| range.start)
    })
}

fn find_click(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be painted"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

fn find_input_focused(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> bool {
    visual.update(|window, cx| {
        app.read(cx)
            .av().grid_ui
            .find
            .input
            .as_ref()
            .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
    })
}

fn find_grid_focused(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> bool {
    visual.update(|window, cx| app.read(cx).av().grid_is_focused(window))
}

fn find_cell_in_view(visual: &mut gpui::VisualTestContext, row: usize, column: usize) -> bool {
    let grid = visual.debug_bounds("grid-scroll-area").expect("grid area");
    // Selectors are `&'static str`; leaking a few test strings is harmless.
    let selector: &'static str = Box::leak(format!("grid-body-cell-{row}-{column}").into_boxed_str());
    visual
        .debug_bounds(selector)
        .is_some_and(|cell| grid.contains(&cell.center()))
}

#[gpui::test]
fn cmd_f_opens_a_top_right_find_bar_and_focuses_its_input(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    assert!(visual.debug_bounds("grid-find-bar").is_none());

    // Nothing focused yet: the result tab being active is enough.
    find_open(visual);
    let bar = visual.debug_bounds("grid-find-bar").expect("find bar painted");
    let grid = visual.debug_bounds("grid-scroll-area").expect("grid area");
    assert!(bar.right() <= grid.right(), "inside the grid area");
    assert!(
        bar.left() > grid.center().x,
        "anchored to the right half of the grid"
    );
    assert!(
        bar.top() >= grid.top() && bar.top() < grid.top() + gpui::px(80.0),
        "anchored to the top of the grid"
    );
    // The bar overlays the header band: no data row is covered.
    let header = visual.debug_bounds("grid-header-cell-0").expect("header cell");
    assert!(
        bar.top() >= header.top() && bar.bottom() <= header.bottom(),
        "bar {bar:?} should sit inside the header band {header:?}"
    );
    let first_row = visual.debug_bounds("grid-body-cell-0-0").expect("first data row");
    assert!(
        !bar.intersects(&first_row) && bar.bottom() <= first_row.top(),
        "bar {bar:?} must not cover the first data row {first_row:?}"
    );
    for part in [
        "grid-find-expand",
        "grid-find-input",
        "grid-find-count",
        "grid-find-prev",
        "grid-find-next",
        "grid-find-close",
    ] {
        assert!(visual.debug_bounds(part).is_some(), "{part} should be painted");
    }
    assert!(find_input_focused(&app, visual), "Cmd+F focuses the field");
    // Options are collapsed until the expander is used.
    assert!(visual.debug_bounds("grid-find-match-case").is_none());
}

#[gpui::test]
fn cmd_f_from_the_focused_grid_opens_the_bar_and_a_second_cmd_f_selects_the_text(
    cx: &mut TestAppContext,
) {
    let (app, visual) = find_app(cx);
    let cell = visual.debug_bounds("grid-body-cell-2-2").unwrap();
    visual.simulate_click(cell.center(), Default::default());
    settle(visual);
    assert!(find_grid_focused(&app, visual));

    find_open(visual);
    assert!(visual.debug_bounds("grid-find-bar").is_some());
    assert!(find_input_focused(&app, visual));

    visual.simulate_input("r5c1");
    // Cmd+F inside the field is not swallowed by the Input's own `Search`
    // action: it re-selects the text so typing replaces it.
    find_open(visual);
    assert!(find_input_focused(&app, visual));
    visual.simulate_input("zz");
    let text = app.read_with(visual, |this, cx| {
        this.av().grid_ui.find.input.as_ref().unwrap().read(cx).value().to_string()
    });
    assert_eq!(text, "zz", "Cmd+F in the field selects its text");
    visual.simulate_input("r5c1");
    // Focus leaves the field (grid click), then Cmd+F brings it back with the
    // text selected so typing replaces it.
    let cell = visual.debug_bounds("grid-body-cell-1-1").unwrap();
    visual.simulate_click(cell.center(), Default::default());
    settle(visual);
    assert!(!find_input_focused(&app, visual));
    find_open(visual);
    assert!(find_input_focused(&app, visual));
    visual.simulate_input("c2");
    let text = app.read_with(visual, |this, cx| {
        this.av().grid_ui.find.input.as_ref().unwrap().read(cx).value().to_string()
    });
    assert_eq!(text, "c2", "the existing text was selected and replaced");
}

#[gpui::test]
fn cmd_f_does_nothing_when_the_active_tab_shows_no_grid(cx: &mut TestAppContext) {
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
        root.read(cx)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });
    visual.update(|window, _| window.activate_window());
    settle(visual);
    find_open(visual);
    assert!(visual.debug_bounds("grid-find-bar").is_none());
    assert!(!app.read_with(visual, |this, _| this.av().grid_ui.find.open));
}

#[gpui::test]
fn typing_highlights_matching_cells_after_the_debounce_and_counts_them(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    find_open(visual);

    visual.simulate_input("r5c1");
    // Within the 150 ms debounce nothing is searched yet.
    app.read_with(visual, |this, _| {
        assert!(this.av().grid_ui.find.query.is_empty());
        assert_eq!(this.av().grid_ui.find.matches.total(), 0);
    });
    find_flush(visual);

    // r5c1 and r5c10..r5c19: the matches are substring, row-major.
    app.read_with(visual, |this, _| {
        let find = &this.av().grid_ui.find;
        assert_eq!(find.query, "r5c1");
        assert!(find.matches.columns.is_empty());
        let expected: Vec<FindCell> = std::iter::once(1)
            .chain(10..20)
            .map(|column| FindCell { row: 5, column })
            .collect();
        assert_eq!(find.matches.cells, expected);
        assert_eq!(find.matches.row_cells(5), expected.as_slice());
        assert!(find.matches.row_cells(4).is_empty());
        assert_eq!(find.current_cell(), Some(expected[0]));
    });
    assert_eq!(find_counter(&app, visual), "1 / 11");
    // The first match is the selected cell and is on screen.
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 5, column: 1 }));
    assert!(find_cell_in_view(visual, 5, 1));

    // A column-name match highlights the header (kind "column").
    find_type(visual, "col1");
    app.read_with(visual, |this, _| {
        let find = &this.av().grid_ui.find;
        assert_eq!(find.matches.columns, [1, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19]);
        assert!(find.matches.cells.is_empty(), "no cell contains `col1`");
        assert!(find.matches.is_header_match(10) && !find.matches.is_header_match(2));
    });
    assert_eq!(find_counter(&app, visual), "1 / 11");
    assert!(visual.debug_bounds("grid-header-cell-1").is_some());

    // Emptying the field clears immediately (no debounce wait).
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_keystrokes("backspace");
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.av().grid_ui.find.query.is_empty());
        assert_eq!(this.av().grid_ui.find.matches.total(), 0);
    });
    assert_eq!(find_counter(&app, visual), "");
}

#[gpui::test]
fn enter_shift_enter_and_the_arrow_buttons_wrap_select_and_scroll_far_matches(
    cx: &mut TestAppContext,
) {
    let (app, visual) = find_app(cx);
    find_open(visual);
    // Column 19 of every row: 60 matches, the last far below and far right.
    find_type(visual, "c19");
    assert_eq!(find_counter(&app, visual), "1 / 60");
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 0, column: 19 }));
    assert!(
        find_cell_in_view(visual, 0, 19),
        "the first match scrolled right into view"
    );
    let offset = app.read_with(visual, |this, _| this.av().grid_horizontal_scroll.offset());
    assert!(f32::from(offset.x) < 0.0, "scrolled horizontally: {offset:?}");

    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(find_counter(&app, visual), "2 / 60");
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 1, column: 19 }));

    // Shift+Enter steps back and wraps from the first match to the last,
    // which is 59 rows down: scrolled into view vertically too.
    visual.simulate_keystrokes("shift-enter");
    visual.simulate_keystrokes("shift-enter");
    settle(visual);
    assert_eq!(find_counter(&app, visual), "60 / 60");
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 59, column: 19 }));
    assert!(find_cell_in_view(visual, 59, 19), "far match scrolled into view");
    let y = app.read_with(visual, |this, _| this.av().grid_scroll.0.borrow().base_handle.offset().y);
    assert!(f32::from(y) < 0.0, "scrolled vertically: {y:?}");

    // The next button wraps forward to the first match.
    find_click(visual, "grid-find-next");
    assert_eq!(find_counter(&app, visual), "1 / 60");
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 0, column: 19 }));
    assert!(find_cell_in_view(visual, 0, 19));
    // The previous button wraps back to the last one.
    find_click(visual, "grid-find-prev");
    assert_eq!(find_counter(&app, visual), "60 / 60");
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 59, column: 19 }));
    assert!(find_cell_in_view(visual, 59, 19));
}

#[gpui::test]
fn a_header_match_scrolls_its_column_into_view(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    find_open(visual);
    find_type(visual, "col19");
    assert_eq!(find_counter(&app, visual), "1 / 1");
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.find.current_target(),
            Some(super::results::find::FindTarget::Column(19))
        );
    });
    assert!(
        visual.debug_bounds("grid-header-cell-19").is_some(),
        "the matching header was scrolled into the culled column window"
    );
}

#[gpui::test]
fn match_case_and_whole_cell_toggles_rerun_the_search(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    find_open(visual);
    find_type(visual, "R5C1");
    assert_eq!(find_counter(&app, visual), "1 / 11", "case-insensitive by default");

    find_click(visual, "grid-find-expand");
    assert!(visual.debug_bounds("grid-find-match-case").is_some());
    assert!(visual.debug_bounds("grid-find-whole-cell").is_some());

    find_click(visual, "grid-find-match-case");
    find_flush(visual);
    assert_eq!(find_counter(&app, visual), "0 / 0", "`R5C1` != `r5c1` with Match case");
    app.read_with(visual, |this, _| assert!(this.av().grid_ui.find.options.match_case));

    find_type(visual, "r5c1");
    assert_eq!(find_counter(&app, visual), "1 / 11");
    find_click(visual, "grid-find-whole-cell");
    find_flush(visual);
    assert_eq!(find_counter(&app, visual), "1 / 1", "Whole cell: only `r5c1`");
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 5, column: 1 }));

    // Toggling off restores the substring behaviour.
    find_click(visual, "grid-find-whole-cell");
    find_click(visual, "grid-find-match-case");
    find_flush(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.av().grid_ui.find.options, crate::grid::GridSearchOptions::default());
    });
    assert_eq!(find_counter(&app, visual), "1 / 11");
}

#[gpui::test]
fn escape_and_the_close_button_close_the_bar_and_return_focus_to_the_grid(
    cx: &mut TestAppContext,
) {
    let (app, visual) = find_app(cx);
    find_open(visual);
    find_type(visual, "c19");
    assert_eq!(find_counter(&app, visual), "1 / 60");

    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("grid-find-bar").is_none());
    assert!(find_grid_focused(&app, visual), "focus returns to the grid");
    app.read_with(visual, |this, cx| {
        let find = &this.av().grid_ui.find;
        assert!(!find.open && find.query.is_empty() && find.matches.total() == 0);
        assert_eq!(find.input.as_ref().unwrap().read(cx).value().as_ref(), "");
    });

    // Reopening starts clean; the × button closes too.
    find_open(visual);
    assert!(visual.debug_bounds("grid-find-bar").is_some());
    find_type(visual, "r1");
    find_click(visual, "grid-find-close");
    assert!(visual.debug_bounds("grid-find-bar").is_none());
    assert!(find_grid_focused(&app, visual));
}

#[gpui::test]
fn the_bar_only_occludes_its_own_bounds(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    find_open(visual);
    let bar = visual.debug_bounds("grid-find-bar").unwrap();

    // Outside the bar the grid keeps its clicks: select a cell, focus moves.
    let cell = visual.debug_bounds("grid-body-cell-3-0").unwrap();
    assert!(!bar.contains(&cell.center()));
    visual.simulate_click(cell.center(), Default::default());
    settle(visual);
    assert_eq!(find_selected(&app, visual), Some(FindCell { row: 3, column: 0 }));
    assert!(find_grid_focused(&app, visual));

    // A click on the bar's own padding never reaches the cell underneath.
    let under_bar = bar.origin + gpui::point(gpui::px(2.0), gpui::px(2.0));
    visual.simulate_click(under_bar, Default::default());
    settle(visual);
    assert_eq!(
        find_selected(&app, visual),
        Some(FindCell { row: 3, column: 0 }),
        "no click-through to the grid under the bar"
    );
}

#[gpui::test]
fn new_results_rerun_the_search_and_keep_the_query(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    find_open(visual);
    find_type(visual, "r5c1");
    assert_eq!(find_counter(&app, visual), "1 / 11");
    let stale = app.read_with(visual, |this, _| this.av().grid_ui.find.generation);

    // A refresh/new query replaces the page: 3 rows x 2 columns.
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("a", arrow_schema::DataType::Utf8, true),
        arrow_schema::Field::new("b", arrow_schema::DataType::Utf8, true),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::StringArray::from(vec!["x", "y", "z"]))
                as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::StringArray::from(vec!["q", "q", "R5C1 here"]))
                as arrow_array::ArrayRef,
        ],
    )
    .unwrap();
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
    });
    settle(visual);
    settle(visual);

    app.read_with(visual, |this, _| {
        let find = &this.av().grid_ui.find;
        assert_eq!(find.query, "r5c1", "the query is kept");
        assert_eq!(find.matches.cells, [FindCell { row: 2, column: 1 }]);
        assert!(find.generation > stale);
    });
    assert_eq!(find_counter(&app, visual), "1 / 1");
    let input_text = app.read_with(visual, |this, cx| {
        this.av().grid_ui.find.input.as_ref().unwrap().read(cx).value().to_string()
    });
    assert_eq!(input_text, "r5c1");

    // A scan superseded by a newer one is dropped, not applied.
    app.update(visual, |this, cx| {
        this.finish_find_scan(
            this.active_id(),
            super::results::find::FindScan {
                generation: stale,
                matches: super::results::find::FindMatches::default(),
                cancelled: false,
                elapsed: Duration::ZERO,
            },
            true,
            cx,
        );
    });
    assert_eq!(find_counter(&app, visual), "1 / 1", "stale generation ignored");
}

#[gpui::test]
fn search_follows_column_visibility_and_order(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    // Display columns: [source 3, source 1]; every other column is hidden.
    app.update(visual, |this, cx| {
        this.av_mut().grid_ui.visible_columns = vec![3, 1];
        this.av_mut().grid_ui.layout = crate::grid::ColumnLayout::new(2);
        cx.notify();
    });
    settle(visual);
    find_open(visual);

    // `c5` exists only in a hidden column.
    find_type(visual, "r2c5");
    assert_eq!(find_counter(&app, visual), "0 / 0");
    // `c1` is display column 1 (source 1); display column 0 is `r?c3`.
    find_type(visual, "r2c1");
    app.read_with(visual, |this, _| {
        assert_eq!(this.av().grid_ui.find.matches.cells, [FindCell { row: 2, column: 1 }]);
    });
    // Header names are matched in display order too: `col3` is display 0.
    find_type(visual, "col3");
    app.read_with(visual, |this, _| assert_eq!(this.av().grid_ui.find.matches.columns, [0]));
    find_type(visual, "col2");
    assert_eq!(find_counter(&app, visual), "0 / 0", "hidden header does not match");

    // Changing the visible columns under an open search re-runs it.
    find_type(visual, "col5");
    assert_eq!(find_counter(&app, visual), "0 / 0");
    app.update(visual, |this, cx| {
        this.av_mut().grid_ui.visible_columns = vec![5, 0];
        this.av_mut().grid_ui.layout = crate::grid::ColumnLayout::new(2);
        cx.notify();
    });
    settle(visual);
    settle(visual);
    app.read_with(visual, |this, _| assert_eq!(this.av().grid_ui.find.matches.columns, [0]));
}

#[gpui::test]
fn the_counter_caps_at_the_match_limit_and_shows_a_plus(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    app.update(visual, |this, _| this.av_mut().grid_ui.find.max_matches = 5);
    find_open(visual);
    find_type(visual, "c19");
    assert_eq!(find_counter(&app, visual), "1 / 5+");
    app.read_with(visual, |this, _| {
        assert_eq!(this.av().grid_ui.find.matches.cells.len(), 5);
        assert!(this.av().grid_ui.find.matches.capped);
    });
    // Navigation wraps within the capped set.
    visual.simulate_keystrokes("shift-enter");
    settle(visual);
    assert_eq!(find_counter(&app, visual), "5 / 5+");

    // Exactly at the cap is not "more": 5 cells total (c19 rows 0..5 only
    // when the whole column is not scanned further) -> use a unique query.
    find_type(visual, "r1c19");
    assert_eq!(find_counter(&app, visual), "1 / 1");
}

// ---- on-demand performance measurements (release) -----------------------

fn find_bench_mixed_batch(rows: usize) -> arrow_array::RecordBatch {
    use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    let mut fields = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();
    for column in 0..20usize {
        match column % 5 {
            0 | 1 => {
                fields.push(Field::new(format!("text{column}"), DataType::Utf8, true));
                arrays.push(std::sync::Arc::new(StringArray::from_iter(
                    (0..rows).map(|row| Some(format!("row {row} col {column}"))),
                )));
            }
            2 => {
                fields.push(Field::new(format!("int{column}"), DataType::Int64, true));
                arrays.push(std::sync::Arc::new(Int64Array::from_iter_values(
                    (0..rows as i64).map(|row| row * 7 + column as i64),
                )));
            }
            3 => {
                fields.push(Field::new(format!("float{column}"), DataType::Float64, true));
                arrays.push(std::sync::Arc::new(Float64Array::from_iter_values(
                    (0..rows).map(|row| row as f64 * 0.25 + column as f64),
                )));
            }
            _ => {
                fields.push(Field::new(format!("flag{column}"), DataType::Boolean, true));
                arrays.push(std::sync::Arc::new(BooleanArray::from_iter(
                    (0..rows).map(|row| Some(row % 3 == 0)),
                )));
            }
        }
    }
    arrow_array::RecordBatch::try_new(std::sync::Arc::new(Schema::new(fields)), arrays).unwrap()
}

#[test]
#[ignore = "on-demand performance measurement; run with --release --ignored --nocapture"]
fn grid_find_perf_scan_100k_rows_by_20_columns() {
    use crate::grid::{GridSearchOptions, GridSearchQuery, GridSearchStatus};
    const ROWS: usize = 100_000;
    let columns: Vec<usize> = (0..FIND_COLUMNS).collect();
    let utf8 = crate::grid::GridState::new(GridContentSource::Arrow(
        crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(wide_grid_batch(
            ROWS, FIND_COLUMNS,
        ))]),
    ));
    let mixed = crate::grid::GridState::new(GridContentSource::Arrow(
        crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(
            find_bench_mixed_batch(ROWS),
        )]),
    ));
    let never = std::sync::atomic::AtomicBool::new(false);
    let cases: [(&str, &str, GridSearchOptions, &crate::grid::GridState<GridContentSource>); 8] = [
        ("utf8 miss (full scan)", "zzz", GridSearchOptions::default(), &utf8),
        ("utf8 1 hit, ci", "R99999C19", GridSearchOptions::default(), &utf8),
        (
            "utf8 1 hit, match case",
            "r99999c19",
            GridSearchOptions { match_case: true, whole_cell: false },
            &utf8,
        ),
        (
            "utf8 1 hit, whole cell",
            "r99999c19",
            GridSearchOptions { match_case: false, whole_cell: true },
            &utf8,
        ),
        ("utf8 many hits (cap 10000)", "c1", GridSearchOptions::default(), &utf8),
        ("mixed types miss (full scan)", "zzz", GridSearchOptions::default(), &mixed),
        ("mixed types 1 hit (number)", "700010", GridSearchOptions::default(), &mixed),
        ("mixed types non-ascii needle", "ÄÖ", GridSearchOptions::default(), &mixed),
    ];
    eprintln!("find scan, {ROWS} rows x {FIND_COLUMNS} cols = {} cells", ROWS * FIND_COLUMNS);
    eprintln!("{:<32} {:>9} {:>8} {:>10} {:>10}", "case", "ms", "matches", "status", "allocs");
    for (label, text, options, grid) in cases {
        let query = GridSearchQuery::new(text, options).unwrap();
        let before = crate::grid_bench::allocation_count();
        let started = Instant::now();
        let (cells, status) = grid.search_cells(&query, &columns, 10_001, &never);
        let elapsed = started.elapsed();
        let allocs = crate::grid_bench::allocation_count() - before;
        eprintln!(
            "{:<32} {:>9.2} {:>8} {:>10} {:>10}",
            label,
            elapsed.as_secs_f64() * 1000.0,
            cells.len(),
            format!("{status:?}"),
            allocs
        );
        assert!(status != GridSearchStatus::Cancelled);
        // Scanning allocates only its reader table, reused buffers and the
        // growing result vector - never per cell.
        assert!(allocs < 200, "scan allocated {allocs} times for {label}");
    }

    // Cancellation: a flag set before the scan stops it at the first poll.
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let query = GridSearchQuery::new("zzz", GridSearchOptions::default()).unwrap();
    let started = Instant::now();
    let (_, status) = utf8.search_cells(&query, &columns, 10_001, &cancel);
    eprintln!(
        "cancelled scan returns in {:.3} ms ({status:?})",
        started.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(status, GridSearchStatus::Cancelled);
}

#[test]
#[ignore = "on-demand performance measurement; run with --release --ignored --nocapture --test-threads=1"]
fn grid_find_perf_highlight_lookup_allocates_nothing() {
    use crate::grid::{GridSearchOptions, GridSearchQuery};
    const ROWS: usize = 100_000;
    let columns: Vec<usize> = (0..FIND_COLUMNS).collect();
    let grid = crate::grid::GridState::new(GridContentSource::Arrow(
        crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(wide_grid_batch(
            ROWS, FIND_COLUMNS,
        ))]),
    ));
    let never = std::sync::atomic::AtomicBool::new(false);
    let query = GridSearchQuery::new("c1", GridSearchOptions::default()).unwrap();
    let (cells, _) = grid.search_cells(&query, &columns, 10_000, &never);
    let matches = super::results::find::FindMatches {
        columns: vec![1, 10],
        cells,
        capped: true,
    };
    assert_eq!(matches.cells.len(), 10_000);
    let mut state = super::results::find::FindState::default();
    state.matches = matches;
    state.current = Some(4);

    // The exact per-cell work render_result_row does, for EVERY cell of the
    // page: the row slice, the column binary search and the current compare.
    let before = crate::grid_bench::allocation_count();
    let started = Instant::now();
    let mut hits = 0usize;
    let mut current_hits = 0usize;
    let current = state.current_cell();
    for row in 0..ROWS {
        let find_row = state.matches.row_cells(row);
        for column in 0..FIND_COLUMNS {
            let is_match = !find_row.is_empty()
                && find_row
                    .binary_search_by_key(&column, |cell| cell.column)
                    .is_ok();
            if is_match {
                hits += 1;
                if current == Some(FindCell { row, column }) {
                    current_hits += 1;
                }
            }
        }
    }
    let elapsed = started.elapsed();
    let allocs = crate::grid_bench::allocation_count() - before;
    let per_cell_ns = elapsed.as_nanos() as f64 / (ROWS * FIND_COLUMNS) as f64;
    eprintln!(
        "highlight lookup over {} cells: {:.2} ms total, {:.1} ns/cell, {} allocations, {} hits",
        ROWS * FIND_COLUMNS,
        elapsed.as_secs_f64() * 1000.0,
        per_cell_ns,
        allocs,
        hits
    );
    assert_eq!(hits, 10_000);
    assert_eq!(current_hits, 1);
    assert_eq!(allocs, 0, "the per-frame highlight lookup must not allocate");
}

#[gpui::test]
#[ignore = "on-demand allocation measurement; run with --release --ignored --nocapture --test-threads=1"]
fn grid_find_perf_render_allocations_with_an_active_search(cx: &mut TestAppContext) {
    let baseline = measure_grid_redraws(cx, 60, 20);
    let (app, visual) = wide_grid_app(cx, 60, 20);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    find_open(visual);
    find_type(visual, "c1");
    app.read_with(visual, |this, _| assert!(this.av().grid_ui.find.matches.total() > 50));
    const REDRAWS: u64 = 10;
    for _ in 0..3 {
        app.update(visual, |_, cx| cx.notify());
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let allocs_before = crate::grid_bench::allocation_count();
    for _ in 0..REDRAWS {
        app.update(visual, |_, cx| cx.notify());
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let with_find = (crate::grid_bench::allocation_count() - allocs_before) / REDRAWS;
    eprintln!(
        "allocations per redraw, 60x20 grid: without find {} / with find bar + highlights {}",
        baseline.0, with_find
    );
}
