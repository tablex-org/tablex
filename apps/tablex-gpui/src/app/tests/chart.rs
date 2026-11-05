// Charts from a result: the Chart presentation (rail button, settings, the five chart types, export) driven
// through the real window, the production theme and the real Select / Combobox components.

use crate::chart_data::{self as cd, Aggregation, ChartData, ChartKind};

/// region (6 values, repeats), month (Date32), amount, qty, team (2 values).
fn sales_batch(rows: usize) -> arrow_array::RecordBatch {
    use arrow_array::{ArrayRef, Date32Array, Float64Array, Int64Array, StringArray};
    let regions = ["north", "south", "east", "west", "central", "island"];
    let region: ArrayRef = Arc::new(StringArray::from_iter((0..rows).map(|i| Some(regions[i % regions.len()]))));
    let month: ArrayRef = Arc::new(Date32Array::from_iter_values((0..rows).map(|i| 19_000 + (i % 24) as i32 * 30)));
    let amount: ArrayRef = Arc::new(Float64Array::from_iter((0..rows).map(|i| Some(10.0 + ((i * 37) % 90) as f64))));
    let qty: ArrayRef = Arc::new(Int64Array::from_iter((0..rows).map(|i| Some(1 + (i % 7) as i64))));
    let team: ArrayRef = Arc::new(StringArray::from_iter((0..rows).map(|i| Some(if i % 3 == 0 { "red" } else { "blue" }))));
    let schema = Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("region", arrow_schema::DataType::Utf8, true),
        arrow_schema::Field::new("month", arrow_schema::DataType::Date32, true),
        arrow_schema::Field::new("amount", arrow_schema::DataType::Float64, true),
        arrow_schema::Field::new("qty", arrow_schema::DataType::Int64, true),
        arrow_schema::Field::new("team", arrow_schema::DataType::Utf8, true),
    ]));
    arrow_array::RecordBatch::try_new(schema, vec![region, month, amount, qty, team]).expect("valid batch")
}

fn chart_app(cx: &mut TestAppContext, batch: arrow_array::RecordBatch) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    chart_app_source(cx, crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(batch)]))
}

fn chart_app_source(cx: &mut TestAppContext, source: crate::grid_arrow::ArrowGridSource) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    // The UI fixture, never the user's own connections.
    cx.dispatcher.allow_parking();
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(760.0)));
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(source)));
        this.reset_active_view();
        cx.notify();
    });
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    settle(visual);
    (app, visual)
}

fn chart_state<R>(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, read: impl FnOnce(&results::chart::ChartTabState) -> R) -> R {
    app.read_with(visual, |this, _| read(&this.av().chart))
}

fn painted_quads(visual: &mut gpui::VisualTestContext) -> Vec<gpui::Quad> {
    visual.update(|window, _| window.painted_quads())
}

fn spec_of(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> cd::ChartSpec {
    chart_state(app, visual, |s| s.spec_for_test().cloned()).expect("a spec")
}

fn scene_of(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> std::rc::Rc<results::chart::ChartScene> {
    match chart_state(app, visual, |s| s.outcome_for_test().cloned()) {
        Some(Ok(scene)) => scene,
        other => panic!("expected a chart, got {:?}", other.map(|o| o.map(|_| ()))),
    }
}

fn issue_of(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Option<cd::ChartIssue> {
    match chart_state(app, visual, |s| s.outcome_for_test().cloned()) {
        Some(Err(issue)) => Some(issue),
        _ => None,
    }
}

fn show_chart(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    if !app.read_with(visual, |this, _| this.chart_presentation_active(this.active_id())) {
        click_selector(visual, "chart-rail-button");
    }
    settle(visual);
}

fn set_spec(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, edit: impl FnOnce(&mut cd::ChartSpec)) {
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        this.view_mut(tab).chart.edit_spec_for_test(edit);
        this.notify_tab(tab, cx);
    });
    settle(visual);
}

/// Frames until the menu under `trigger` has faded in completely, or until it is clear there is none. The
/// fade runs on the executor's clock, which only moves when the test advances it.
fn wait_for_menu(visual: &mut gpui::VisualTestContext, trigger: gpui::Bounds<gpui::Pixels>, menu_width: f32, must_exist: bool) -> Vec<gpui::Quad> {
    let mut quads = Vec::new();
    for attempt in 0..200 {
        settle(visual);
        quads = painted_quads(visual);
        let panel = quads.iter().rev().find(|q| {
            (q.bounds.size.width.0 / 2.0 - menu_width).abs() < 1.0
                && q.bounds.origin.y.0 / 2.0 >= trigger.bottom().as_f32() - 4.0
                && q.bounds.origin.y.0 / 2.0 <= trigger.bottom().as_f32() + 12.0
                && q.background.as_solid().is_some_and(|c| c.a > 0.5)
        });
        match panel {
            Some(panel) if panel.background.as_solid().is_some_and(|c| c.a > 0.99) => break,
            None if attempt >= 2 && !must_exist => break,
            _ => visual.executor().advance_clock(std::time::Duration::from_millis(16)),
        }
    }
    quads
}

/// The open dropdown under `trigger`, read from what is painted: the menu panel (a solid, opaque quad of the
/// menu's width just below the trigger), how many option rows it holds and the row the cursor is on.
#[derive(Debug, PartialEq)]
struct OpenMenu {
    rows: usize,
    cursor: Option<usize>,
}

fn open_menu(visual: &mut gpui::VisualTestContext, trigger: &'static str, menu_width: f32, searchable: bool) -> Option<OpenMenu> {
    let trigger = visual.debug_bounds(trigger)?;
    let scale = 2.0f32;
    let quads = wait_for_menu(visual, trigger, menu_width, false);
    let panel = quads.iter().rev().find(|q| {
        let width = q.bounds.size.width.0 / scale;
        (width - menu_width).abs() < 1.0
            && q.bounds.origin.y.0 / scale >= trigger.bottom().as_f32() - 1.0
            && q.bounds.origin.y.0 / scale <= trigger.bottom().as_f32() + 12.0
            && q.background.as_solid().is_some_and(|c| c.a > 0.99)
    })?;
    let top = panel.bounds.origin.y.0 / scale;
    let height = panel.bounds.size.height.0 / scale;
    let rows = ((height - 7.0) / SELECT_ITEM_PITCH).round() as usize - usize::from(searchable);
    let first = top + 4.0 + if searchable { SELECT_ITEM_PITCH } else { 0.0 };
    let cursor = quads
        .iter()
        .rfind(|q| {
            let (x, y) = (q.bounds.origin.x.0 / scale, q.bounds.origin.y.0 / scale);
            (q.bounds.size.width.0 / scale - (menu_width - 8.0)).abs() < 1.5
                && y >= first - 3.0
                && y < top + height
                && x >= panel.bounds.origin.x.0 / scale
                && q.background.as_solid().is_some_and(|c| c.a > 0.9)
        })
        .map(|q| ((q.bounds.origin.y.0 / scale - first) / SELECT_ITEM_PITCH).round() as usize);
    Some(OpenMenu { rows, cursor })
}

#[gpui::test]
fn the_chart_button_swaps_the_grid_for_a_chart_and_back_keeping_the_grid_as_it_was(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    assert!(visual.debug_bounds("grid-scroll-area").is_some());
    assert!(visual.debug_bounds("chart-pane").is_none());
    // The grid is scrolled and a cell is selected before the chart is shown.
    scroll_wide_grid(&app, visual, None, Some(2_000.0));
    app.update(visual, |this, _| this.av().grid_ui.selection.borrow_mut().select(CellPosition { row: 77, column: 2 }));
    let offset_before = app.read_with(visual, |this, _| this.av().grid_scroll.0.borrow().base_handle.offset());
    assert!(offset_before.y < gpui::px(-1_000.0), "the grid scrolled: {offset_before:?}");

    click_selector(visual, "chart-rail-button");
    assert!(visual.debug_bounds("chart-pane").is_some(), "the chart replaces the grid");
    assert!(visual.debug_bounds("grid-scroll-area").is_none());
    assert!(app.read_with(visual, |this, _| this.chart_presentation_active(this.active_id())));
    let tooltip_state = visual.debug_bounds("chart-rail-button").expect("the rail button");
    assert!(tooltip_state.size.width > gpui::px(0.0));

    // The same button leaves the chart; the grid is as it was.
    click_selector(visual, "chart-rail-button");
    assert!(visual.debug_bounds("chart-pane").is_none());
    assert!(visual.debug_bounds("grid-scroll-area").is_some());
    let offset_after = app.read_with(visual, |this, _| this.av().grid_scroll.0.borrow().base_handle.offset());
    assert_eq!(offset_after, offset_before, "the grid keeps its scroll");
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 77, column: 2 }), "and its selection");

    // The rail's Grid button is the other way back.
    click_selector(visual, "chart-rail-button");
    assert!(visual.debug_bounds("chart-pane").is_some());
    click_selector(visual, "columns-rail-grid-button");
    assert!(visual.debug_bounds("grid-scroll-area").is_some() && visual.debug_bounds("chart-pane").is_none());
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 77, column: 2 }));

    // The record toggle also takes the chart's place.
    click_selector(visual, "chart-rail-button");
    click_selector(visual, "record-view-toggle");
    assert!(visual.debug_bounds("chart-pane").is_none(), "the record view replaces the chart");
    assert!(record_active(&app, visual));

    // Leaving the chart frees its data: nothing is held for a hidden chart.
    chart_state(&app, visual, |s| assert!(s.source_for_test().is_none()));
}

#[gpui::test]
fn chart_defaults_pick_the_first_text_column_and_the_first_number_and_sum_repeating_categories(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    let spec = spec_of(&app, visual);
    assert_eq!(spec.kind, ChartKind::Bar);
    assert_eq!(spec.x, 0, "region: the first text column");
    assert_eq!(spec.values, vec![2], "amount: the first numeric column");
    assert_eq!(spec.aggregation, Aggregation::Sum, "regions repeat");
    let scene = scene_of(&app, visual);
    assert_eq!(scene.model.title, "SUM(amount) by region");
    // Unique categories need no aggregation.
    let unique = {
        use arrow_array::{ArrayRef, Float64Array, StringArray};
        let ids: Vec<String> = (0..40).map(|i| format!("id{i}")).collect();
        let k: ArrayRef = Arc::new(StringArray::from_iter(ids.iter().map(|s| Some(s.as_str()))));
        let v: ArrayRef = Arc::new(Float64Array::from_iter((0..40).map(|i| Some(f64::from(i)))));
        arrow_array::RecordBatch::try_from_iter(vec![("k", k), ("v", v)]).unwrap()
    };
    let (app2, visual2) = chart_app(cx, unique);
    show_chart(&app2, visual2);
    let spec = spec_of(&app2, visual2);
    assert_eq!((spec.x, spec.values.clone(), spec.aggregation), (0, vec![1], Aggregation::None));
}

#[gpui::test]
fn the_chart_selects_open_show_their_options_apply_a_pick_dismiss_and_reopen_on_it(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    assert_eq!(open_menu(visual, "chart-kind", 140.0, false), None, "closed at first");

    // Type: five options, the cursor on the current one; a click picks Area and closes the menu.
    click_selector(visual, "chart-kind");
    assert_eq!(open_menu(visual, "chart-kind", 140.0, false), Some(OpenMenu { rows: 5, cursor: Some(0) }));
    click_select_option(visual, "chart-kind", 22.0, 2);
    assert_eq!(spec_of(&app, visual).kind, ChartKind::Area);
    assert_eq!(open_menu(visual, "chart-kind", 140.0, false), None, "a pick dismisses the menu");
    assert_eq!(scene_of(&app, visual).model.kind, ChartKind::Area, "the chart follows");
    click_selector(visual, "chart-kind");
    assert_eq!(open_menu(visual, "chart-kind", 140.0, false), Some(OpenMenu { rows: 5, cursor: Some(2) }), "reopened on the selection");
    // Escape dismisses without changing anything, even after the cursor moved.
    visual.simulate_keystrokes("down down");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(open_menu(visual, "chart-kind", 140.0, false), None);
    assert_eq!(spec_of(&app, visual).kind, ChartKind::Area);
    assert!(visual.debug_bounds("chart-pane").is_some(), "Escape closes the menu, not the chart");
    click_selector(visual, "chart-kind");
    assert_eq!(open_menu(visual, "chart-kind", 140.0, false), Some(OpenMenu { rows: 5, cursor: Some(2) }));
    visual.simulate_keystrokes("escape");
    settle(visual);

    // Keyboard: open, move, Enter.
    pick_with_keys(visual, "chart-kind", 2);
    assert_eq!(spec_of(&app, visual).kind, ChartKind::Pie);
    pick_with_keys(visual, "chart-kind", -4);
    assert_eq!(spec_of(&app, visual).kind, ChartKind::Bar);

    // Aggregate: six options.
    click_selector(visual, "chart-aggregation");
    assert_eq!(open_menu(visual, "chart-aggregation", 140.0, false), Some(OpenMenu { rows: 6, cursor: Some(2) }), "Sum is the default here");
    click_select_option(visual, "chart-aggregation", 22.0, 3);
    assert_eq!(spec_of(&app, visual).aggregation, Aggregation::Avg);
    assert_eq!(scene_of(&app, visual).model.y_title, "AVG(amount)");
    click_selector(visual, "chart-aggregation");
    assert_eq!(open_menu(visual, "chart-aggregation", 140.0, false), Some(OpenMenu { rows: 6, cursor: Some(3) }));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(open_menu(visual, "chart-aggregation", 140.0, false), None);

    // X: a searchable list of the five columns.
    click_selector(visual, "chart-x");
    assert_eq!(open_menu(visual, "chart-x", 240.0, true), Some(OpenMenu { rows: 5, cursor: Some(0) }));
    click_select_option(visual, "chart-x", 48.7, 4);
    assert_eq!(spec_of(&app, visual).x, 4, "team");
    assert_eq!(scene_of(&app, visual).model.x_title, "team");
    assert_eq!(open_menu(visual, "chart-x", 240.0, true), None);
    click_selector(visual, "chart-x");
    assert_eq!(open_menu(visual, "chart-x", 240.0, true), Some(OpenMenu { rows: 5, cursor: Some(4) }));
    visual.simulate_keystrokes("escape");
    settle(visual);

    // Series by: None + the five columns; pick `region` -> a series per region.
    click_selector(visual, "chart-series");
    assert_eq!(open_menu(visual, "chart-series", 240.0, true), Some(OpenMenu { rows: 6, cursor: Some(0) }));
    click_select_option(visual, "chart-series", 48.7, 1);
    assert_eq!(spec_of(&app, visual).series_by, Some(0));
    assert_eq!(scene_of(&app, visual).model.series_names().len(), 6, "one series per region");
    click_selector(visual, "chart-series");
    assert_eq!(open_menu(visual, "chart-series", 240.0, true), Some(OpenMenu { rows: 6, cursor: Some(1) }));
    visual.simulate_keystrokes("escape");
    settle(visual);

    // A scatter plots numbers or dates along X: choosing it moves X off the text column, and the X
    // select shows the column it moved to. The "series by" select is off for a type that has no series.
    set_spec(&app, visual, |spec| spec.series_by = None);
    pick_with_keys(visual, "chart-kind", 3);
    let spec = spec_of(&app, visual);
    assert_eq!((spec.kind, spec.x), (ChartKind::Scatter, 1), "month: the first date column");
    click_selector(visual, "chart-x");
    assert_eq!(open_menu(visual, "chart-x", 240.0, true), Some(OpenMenu { rows: 5, cursor: Some(1) }));
    visual.simulate_keystrokes("escape");
    settle(visual);
    let before = spec_of(&app, visual);
    click_selector(visual, "chart-series");
    assert_eq!(open_menu(visual, "chart-series", 240.0, true), None, "disabled: a scatter has no series column");
    assert_eq!(spec_of(&app, visual), before);
}

#[gpui::test]
fn the_values_combobox_toggles_columns_keeps_its_menu_open_and_remembers_them(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    assert_eq!(spec_of(&app, visual).values, vec![2]);
    click_selector(visual, "chart-values");
    let trigger = visual.debug_bounds("chart-values").unwrap();
    // The popover fades in; click it once it is at rest.
    wait_for_menu(visual, trigger, 240.0, true);
    // Rows of the popover: a searchable list under the trigger (the search field first); toggle `qty` (the 4th column).
    let at = gpui::point(trigger.center().x, trigger.bottom() + gpui::px(48.7 + SELECT_ITEM_PITCH * 3.0));
    visual.simulate_click(at, Default::default());
    settle(visual);
    assert_eq!(spec_of(&app, visual).values, vec![2, 3], "the column is added, the order kept");
    assert_eq!(scene_of(&app, visual).model.series_names().len(), 2, "one series per value column");
    // Multi: the popover stays open for the next toggle; clicking the same row removes it.
    visual.simulate_click(at, Default::default());
    settle(visual);
    assert_eq!(spec_of(&app, visual).values, vec![2]);
    visual.simulate_click(at, Default::default());
    settle(visual);
    assert_eq!(spec_of(&app, visual).values, vec![2, 3]);
    // Escape dismisses it; the chart pane stays; reopening still shows both as selected.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("chart-pane").is_some());
    click_selector(visual, "chart-values");
    assert_eq!(app.read_with(visual, |this, cx| this.av().chart.values_selection_for_test(cx)), vec![2, 3], "reopened with both selected");
    visual.simulate_keystrokes("escape");
    settle(visual);

    // Count needs no value column: the combobox is disabled and the chart counts rows.
    set_spec(&app, visual, |spec| spec.aggregation = Aggregation::Count);
    assert_eq!(scene_of(&app, visual).model.y_title, "COUNT(*)");
    click_selector(visual, "chart-values");
    assert_eq!(app.read_with(visual, |this, cx| this.av().chart.values_selection_for_test(cx)), vec![2, 3]);
    let still = visual.debug_bounds("chart-values").unwrap();
    visual.simulate_click(gpui::point(still.center().x, still.bottom() + gpui::px(48.7)), Default::default());
    settle(visual);
    assert_eq!(spec_of(&app, visual).values, vec![2, 3], "a disabled combobox changes nothing");
}

fn series_hsla(rgba: gpui::Rgba) -> gpui::Hsla {
    rgba.into()
}

fn near(a: gpui::Hsla, b: gpui::Hsla) -> bool {
    (a.h - b.h).abs() < 0.01 && (a.s - b.s).abs() < 0.02 && (a.l - b.l).abs() < 0.02
}

/// Painted quads of `color` (ignoring alpha) whose width is `width` logical pixels.
fn marks(visual: &mut gpui::VisualTestContext, color: gpui::Hsla, width: f32) -> usize {
    painted_quads(visual)
        .iter()
        .filter(|q| (q.bounds.size.width.0 / 2.0 - width).abs() < 0.6 && q.background.as_solid().is_some_and(|c| near(c, color)))
        .count()
}

fn hover(visual: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    visual.simulate_event(gpui::MouseMoveEvent { position: at, pressed_button: None, modifiers: Default::default() });
    settle(visual);
    settle(visual);
}

#[gpui::test]
fn each_chart_type_paints_its_marks_in_the_theme_colors(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    let (c1, c2) = (series_hsla(theme::chart_1()), series_hsla(theme::chart_2()));

    // Bar: one 30px bar per region, in chart_1.
    assert_eq!(marks(visual, c1, 30.0), 6, "six regions, six bars");
    let plot = visual.debug_bounds("chart-plot").expect("the plot");
    assert!(plot.size.width > gpui::px(600.0) && plot.size.height > gpui::px(150.0), "{plot:?}");

    // Bar grouped by a column: a bar per (region, team) cell, each team its own theme color, side by side.
    set_spec(&app, visual, |spec| spec.series_by = Some(4));
    assert_eq!(scene_of(&app, visual).model.series_names().iter().map(|n| n.to_string()).collect::<Vec<_>>(), ["red", "blue"]);
    let painted = painted_quads(visual);
    let bars = |color: gpui::Hsla| painted.iter().filter(|q| q.background.as_solid().is_some_and(|c| near(c, color) && c.a > 0.99) && q.bounds.size.height.0 > 40.0 && q.bounds.size.width.0 > 20.0 && q.bounds.size.width.0 < 200.0).count();
    assert_eq!(bars(c1), 2, "red: north and west");
    assert_eq!(bars(c2), 4, "blue: the other four");
    set_spec(&app, visual, |spec| spec.series_by = None);

    // Line: one series, drawn as the area chart's straight outline; the grid is painted.
    set_spec(&app, visual, |spec| spec.kind = ChartKind::Line);
    assert_eq!(scene_of(&app, visual).model.kind, ChartKind::Line);
    assert!(marks(visual, series_hsla(theme::border()), 4.0) > 0, "the dashed grid is painted");

    // Area: one series, and with two the second takes chart_2 in the legend.
    set_spec(&app, visual, |spec| spec.kind = ChartKind::Area);
    assert_eq!(scene_of(&app, visual).model.kind, ChartKind::Area);
    assert!(marks(visual, series_hsla(theme::border()), 4.0) > 0, "the area chart paints its grid");
    set_spec(&app, visual, |spec| spec.values = vec![2, 3]);
    assert_eq!(marks(visual, c1, 10.0), 1, "legend swatch of series 1");
    assert_eq!(marks(visual, c2, 10.0), 1, "legend swatch of series 2");

    // Scatter: the raw rows, one dot each (600 <= the cap).
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Scatter;
        spec.x = 1;
        spec.values = vec![2];
        spec.aggregation = Aggregation::None;
    });
    assert_eq!(marks(visual, c1, 7.0), 600, "{} dots", marks(visual, c1, 7.0));

    // Pie: a legend swatch per slice.
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Pie;
        spec.x = 0;
        spec.aggregation = Aggregation::Sum;
    });
    let ChartData::Slices(slices) = &scene_of(&app, visual).model.data else { panic!("slices") };
    assert_eq!(slices.len(), 6);
    let palette = [theme::chart_1(), theme::chart_2(), theme::chart_3(), theme::chart_4(), theme::chart_5()];
    for color in palette {
        assert_eq!(marks(visual, series_hsla(color), 10.0), 1, "a swatch in {color:?}");
    }
}

#[gpui::test]
fn the_chart_follows_the_theme(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    let dark = series_hsla(theme::chart_1());
    assert_eq!(marks(visual, dark, 30.0), 6);
    visual.update(|_, cx| theme::apply(gpui_component::ThemeMode::Light, cx));
    settle(visual);
    let light = series_hsla(theme::chart_1());
    assert!(!near(dark, light), "the palettes differ");
    assert_eq!(marks(visual, light, 30.0), 6, "bars repainted in the light chart_1");
    assert_eq!(marks(visual, dark, 30.0), 0);
    visual.update(|_, cx| theme::apply(gpui_component::ThemeMode::Dark, cx));
    settle(visual);
}

#[gpui::test]
fn hovering_a_mark_shows_the_tooltip_for_official_and_custom_charts(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    let tooltip = |visual: &mut gpui::VisualTestContext| painted_quads(visual).len();
    // Official bar chart: hover the first bar.
    let plot = visual.debug_bounds("chart-plot").unwrap();
    let rest = tooltip(visual);
    let bar = painted_quads(visual).into_iter().find(|q| near(q.background.as_solid().unwrap_or_default(), series_hsla(theme::chart_1())) && (q.bounds.size.width.0 / 2.0 - 30.0).abs() < 0.6).expect("a bar");
    let over = gpui::point(gpui::px(bar.bounds.origin.x.0 / 2.0 + 15.0), gpui::px(bar.bounds.origin.y.0 / 2.0 + 20.0));
    hover(visual, over);
    assert!(tooltip(visual) > rest, "a tooltip (and the highlight band) is painted over the bar");
    hover(visual, gpui::point(plot.left() - gpui::px(100.0), plot.top()));
    hover(visual, gpui::point(plot.left() - gpui::px(100.0), plot.top() + gpui::px(1.0)));
    assert_eq!(tooltip(visual), rest, "and gone when the pointer leaves");

    // Grouped bars (custom plot): the same.
    set_spec(&app, visual, |spec| spec.series_by = Some(4));
    let rest = tooltip(visual);
    let bar = painted_quads(visual).into_iter().find(|q| near(q.background.as_solid().unwrap_or_default(), series_hsla(theme::chart_2())) && q.bounds.size.height.0 > 40.0 && q.bounds.size.width.0 > 20.0 && q.bounds.size.width.0 < 200.0).expect("a bar");
    hover(visual, gpui::point(gpui::px(bar.bounds.origin.x.0 / 2.0 + 5.0), gpui::px(bar.bounds.origin.y.0 / 2.0 + 20.0)));
    assert!(tooltip(visual) > rest, "grouped bars show a tooltip");

    // Scatter (custom plot): hover a dot.
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Scatter;
        spec.x = 1;
        spec.series_by = None;
        spec.aggregation = Aggregation::None;
    });
    hover(visual, gpui::point(plot.left() - gpui::px(100.0), plot.top()));
    let rest = tooltip(visual);
    let dot = painted_quads(visual).into_iter().find(|q| (q.bounds.size.width.0 / 2.0 - 7.0).abs() < 0.6 && q.background.as_solid().is_some_and(|c| near(c, series_hsla(theme::chart_1())))).expect("a dot");
    hover(visual, gpui::point(gpui::px(dot.bounds.origin.x.0 / 2.0 + 3.5), gpui::px(dot.bounds.origin.y.0 / 2.0 + 3.5)));
    assert!(tooltip(visual) > rest, "the nearest dot gets a tooltip and a crosshair");
}

#[gpui::test]
fn empty_non_numeric_and_too_wide_results_say_why_there_is_no_chart(cx: &mut TestAppContext) {
    // No numeric column.
    let words = {
        use arrow_array::{ArrayRef, StringArray};
        let a: ArrayRef = Arc::new(StringArray::from(vec![Some("a"), Some("b")]));
        let b: ArrayRef = Arc::new(StringArray::from(vec![Some("1"), Some("2")]));
        arrow_array::RecordBatch::try_from_iter(vec![("a", a), ("b", b)]).unwrap()
    };
    let (app, visual) = chart_app(cx, words);
    show_chart(&app, visual);
    assert_eq!(issue_of(&app, visual), Some(cd::ChartIssue::NoNumericColumn));
    assert!(visual.debug_bounds("chart-message").is_some(), "the pane says so");
    assert!(visual.debug_bounds("chart-plot").is_none());
    // Text that parses as numbers is refused, not converted.
    set_spec(&app, visual, |spec| {
        spec.values = vec![1];
        spec.aggregation = Aggregation::Sum;
    });
    assert_eq!(issue_of(&app, visual), Some(cd::ChartIssue::NotNumeric { column: "b".into() }));
    // Count needs no numbers.
    set_spec(&app, visual, |spec| spec.aggregation = Aggregation::Count);
    assert!(issue_of(&app, visual).is_none());
    assert!(visual.debug_bounds("chart-plot").is_some());
}

#[gpui::test]
fn an_empty_result_and_too_many_series_each_get_their_message(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(0));
    show_chart(&app, visual);
    assert_eq!(issue_of(&app, visual), Some(cd::ChartIssue::NoRows));
    assert!(visual.debug_bounds("chart-message").is_some());

    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    // 24 months as the series column: more series than a chart can tell apart.
    set_spec(&app, visual, |spec| spec.series_by = Some(1));
    assert!(matches!(issue_of(&app, visual), Some(cd::ChartIssue::TooManySeries { max: 12, .. })), "{:?}", issue_of(&app, visual));
    assert!(visual.debug_bounds("chart-message").is_some());
    // The export buttons are inert without a chart.
    click_selector(visual, "chart-export-png");
    let pending = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| visual.simulate_new_path_selection(|_| None)));
    assert!(pending.is_err(), "no save dialog is opened without a chart");
}

#[gpui::test]
fn a_chart_not_offered_for_a_tab_without_a_result(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(Vec::new()))));
        this.reset_active_view();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.active_id();
        assert_eq!(this.chart_support(tab), Err("No result to chart"));
        assert!(!this.chart_presentation_active(tab));
    });
    app.update(visual, |this, cx| this.toggle_chart_presentation(this.active_id(), cx));
    settle(visual);
    assert!(visual.debug_bounds("chart-pane").is_none(), "nothing to chart: the toggle does nothing");
}

#[gpui::test]
fn settings_survive_a_rerun_with_the_same_columns_and_restart_with_other_columns(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    set_spec(&app, visual, |spec| spec.kind = ChartKind::Line);
    let controls = chart_state(&app, visual, |s| s.controls_for_test());
    let prepares = chart_state(&app, visual, |s| s.prepares);

    // The same query run again: new rows, same columns -> same settings, new chart.
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(sales_batch(12))]))));
        cx.notify();
    });
    settle(visual);
    assert_eq!(spec_of(&app, visual).kind, ChartKind::Line);
    assert_eq!(chart_state(&app, visual, |s| s.controls_for_test()), controls, "the controls are kept");
    assert!(chart_state(&app, visual, |s| s.prepares) > prepares, "and the new page is charted");
    assert_eq!(scene_of(&app, visual).model.drawn_points(), 6, "twelve rows, six regions");

    // The same page drawn again is not charted again.
    let prepares = chart_state(&app, visual, |s| s.prepares);
    for _ in 0..3 {
        app.update(visual, |_, cx| cx.notify());
        settle(visual);
    }
    assert_eq!(chart_state(&app, visual, |s| s.prepares), prepares, "an unchanged request is not recomputed");

    // Other columns: the spec starts over (the chart type is kept), with new controls.
    let other = {
        use arrow_array::{ArrayRef, Int64Array, StringArray};
        let a: ArrayRef = Arc::new(StringArray::from(vec![Some("x"), Some("y")]));
        let b: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), Some(2)]));
        arrow_array::RecordBatch::try_from_iter(vec![("name", a), ("n", b)]).unwrap()
    };
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(other)]))));
        cx.notify();
    });
    settle(visual);
    let spec = spec_of(&app, visual);
    assert_eq!((spec.kind, spec.x, spec.values.clone()), (ChartKind::Line, 0, vec![1]));
    assert_ne!(chart_state(&app, visual, |s| s.controls_for_test()), controls);
    assert_eq!(scene_of(&app, visual).model.title, "n by name");
}

#[gpui::test]
fn a_late_answer_for_replaced_inputs_is_never_shown(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    assert!(scene_of(&app, visual).model.title.starts_with("SUM"));
    let tab = app.read_with(visual, |this, _| this.active_id());
    fn model_for(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, aggregation: Aggregation) -> Result<cd::ChartModel, cd::ChartIssue> {
        let page = chart_state(app, visual, |s| s.source_for_test().cloned()).unwrap();
        let mut spec = spec_of(app, visual);
        spec.aggregation = aggregation;
        cd::prepare(&page, &spec, 512, &std::sync::atomic::AtomicBool::new(false))
    }
    fn edit(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64, aggregation: Aggregation) {
        app.update(visual, |this, cx| {
            this.view_mut(tab).chart.edit_spec_for_test(|spec| spec.aggregation = aggregation);
            this.notify_tab(tab, cx);
        });
        visual.update(|window, _| window.refresh());
    }
    let (avg_model, max_model) = (model_for(&app, visual, Aggregation::Avg), model_for(&app, visual, Aggregation::Max));

    // Change the inputs and draw a frame, but let nothing run: the request is in flight.
    edit(&app, visual, tab, Aggregation::Avg);
    let first = chart_state(&app, visual, |s| s.generation_for_test());
    assert!(chart_state(&app, visual, |s| s.outcome_for_test().is_none()), "the old chart is gone while the new one is prepared");
    assert!(visual.debug_bounds("chart-plot").is_none(), "and not drawn");
    // Inputs change again before the first answer arrives.
    edit(&app, visual, tab, Aggregation::Max);
    let second = chart_state(&app, visual, |s| s.generation_for_test());
    assert!(second > first);

    // The first request answers late: dropped.
    app.update(visual, |this, cx| this.finish_chart(tab, first, avg_model.clone(), cx));
    assert!(chart_state(&app, visual, |s| s.outcome_for_test().is_none()), "an answer for replaced inputs is ignored");
    // The current one lands.
    app.update(visual, |this, cx| this.finish_chart(tab, second, max_model.clone(), cx));
    assert_eq!(scene_of(&app, visual).model.y_title, "MAX(amount)");
    // The task that was really in flight finishes later and changes nothing either.
    settle(visual);
    assert_eq!(scene_of(&app, visual).model.y_title, "MAX(amount)");
    assert_eq!(chart_state(&app, visual, |s| s.generation_for_test()), second);
}

#[gpui::test]
fn the_last_of_several_quick_changes_is_the_chart_that_ends_up_shown(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(2_000));
    show_chart(&app, visual);
    let tab = app.read_with(visual, |this, _| this.active_id());
    for aggregation in [Aggregation::Avg, Aggregation::Min, Aggregation::Max, Aggregation::Sum, Aggregation::Max] {
        app.update(visual, |this, cx| {
            this.view_mut(tab).chart.edit_spec_for_test(|spec| spec.aggregation = aggregation);
            this.notify_tab(tab, cx);
        });
        visual.update(|window, _| window.refresh());
    }
    settle(visual);
    assert_eq!(scene_of(&app, visual).model.y_title, "MAX(amount)");
}

#[gpui::test]
fn a_chart_is_computed_off_the_frame_and_not_while_the_grid_shows(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(50_000));
    assert_eq!(chart_state(&app, visual, |s| s.prepares), 0, "nothing is prepared while the grid shows");
    app.update(visual, |this, cx| this.toggle_chart_presentation(this.active_id(), cx));
    visual.update(|window, _| window.refresh());
    // The frame that switched to Chart started the computation and returned without a chart.
    assert_eq!(chart_state(&app, visual, |s| s.prepares), 1);
    assert!(chart_state(&app, visual, |s| s.outcome_for_test().is_none()), "the frame did not wait for the rows");
    assert!(visual.debug_bounds("chart-message").is_some(), "it shows that the chart is being prepared");
    settle(visual);
    assert!(scene_of(&app, visual).model.drawn_points() > 0);
}

fn read_png(path: &std::path::Path) -> image::RgbaImage {
    image::load_from_memory(&std::fs::read(path).expect("the file was written")).expect("a valid PNG").to_rgba8()
}

fn rgb8(color: gpui::Rgba) -> [u8; 3] {
    [(color.r * 255.0).round() as u8, (color.g * 255.0).round() as u8, (color.b * 255.0).round() as u8]
}

#[gpui::test]
fn exporting_writes_a_valid_png_and_an_svg_in_the_colors_on_screen(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    let dir = tempfile::tempdir().unwrap();

    for (mode, name) in [(gpui_component::ThemeMode::Dark, "dark"), (gpui_component::ThemeMode::Light, "light")] {
        visual.update(|_, cx| theme::apply(mode, cx));
        settle(visual);
        // PNG.
        let png = dir.path().join(format!("bars-{name}.png"));
        let chosen = png.clone();
        click_selector(visual, "chart-export-png");
        visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
        visual.run_until_parked();
        settle(visual);
        let image = read_png(&png);
        assert_eq!((image.width(), image.height()), (1920, 1080), "{name}: 960x540 at 2x");
        let background = rgb8(theme::background());
        assert_eq!(image.get_pixel(3, 3).0[..3], background, "{name}: the theme background");
        let drawn = image.pixels().filter(|p| p.0[..3] != background).count();
        assert!(drawn > 10_000, "{name}: {drawn} pixels differ from the background");
        let first = rgb8(theme::chart_1());
        let bars = image.pixels().filter(|p| p.0[..3] == first).count();
        assert!(bars > 5_000, "{name}: {bars} pixels in the first series color");
        // SVG.
        let svg = dir.path().join(format!("bars-{name}.svg"));
        let chosen = svg.clone();
        click_selector(visual, "chart-export-svg");
        visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
        visual.run_until_parked();
        settle(visual);
        let text = std::fs::read_to_string(&svg).expect("the SVG was written");
        let tree = resvg::usvg::Tree::from_str(&text, &resvg::usvg::Options::default()).expect("usvg parses it");
        assert_eq!((tree.size().width(), tree.size().height()), (960.0, 540.0));
        assert!(text.contains(&format!("#{:02x}{:02x}{:02x}", first[0], first[1], first[2])), "{name}: the series color");
        assert!(text.contains("SUM(amount) by region"));
        let notice = chart_state(&app, visual, |s| s.notice_for_test().cloned()).expect("a notice");
        assert!(!notice.1 && notice.0.starts_with("Saved SVG to "), "{notice:?}");
    }
    visual.update(|_, cx| theme::apply(gpui_component::ThemeMode::Dark, cx));
}

#[gpui::test]
fn every_chart_type_exports_a_decodable_png_with_something_drawn(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    let dir = tempfile::tempdir().unwrap();
    let style = results::chart::svg_style();
    for kind in ChartKind::ALL {
        set_spec(&app, visual, |spec| {
            spec.kind = kind;
            spec.x = if kind == ChartKind::Scatter { 1 } else { 0 };
            spec.aggregation = if kind == ChartKind::Scatter { Aggregation::None } else { Aggregation::Sum };
            spec.values = vec![2, 3];
        });
        let model = scene_of(&app, visual).model.clone();
        for ext in [results::chart::ChartExportFormat::Png, results::chart::ChartExportFormat::Svg] {
            let path = dir.path().join(format!("{kind:?}.{}", if ext == results::chart::ChartExportFormat::Png { "png" } else { "svg" }));
            results::chart::write_chart(&model, &style, ext, &path).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
            if ext == results::chart::ChartExportFormat::Png {
                let image = read_png(&path);
                assert_eq!((image.width(), image.height()), (1920, 1080), "{kind:?}");
                let bg = style.background;
                let drawn = image.pixels().filter(|p| p.0[..3] != bg).count();
                assert!(drawn > 8_000, "{kind:?}: {drawn} pixels drawn");
            } else {
                resvg::usvg::Tree::from_str(&std::fs::read_to_string(&path).unwrap(), &resvg::usvg::Options::default()).expect("usvg parses it");
            }
        }
    }
}

#[gpui::test]
fn a_failed_export_says_so_and_a_cancelled_dialog_does_nothing(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    click_selector(visual, "chart-export-png");
    visual.simulate_new_path_selection(|_| None);
    visual.run_until_parked();
    settle(visual);
    assert!(chart_state(&app, visual, |s| s.notice_for_test().cloned()).is_none(), "cancelling the dialog is not an error");

    let missing = std::path::PathBuf::from("/nonexistent-tablex-dir/deeper/chart.png");
    let chosen = missing.clone();
    click_selector(visual, "chart-export-png");
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    visual.run_until_parked();
    settle(visual);
    let (text, error) = chart_state(&app, visual, |s| s.notice_for_test().cloned()).expect("the failure is reported");
    assert!(error && text.starts_with("Could not save the chart to /nonexistent-tablex-dir/deeper/chart.png"), "{text}");
    assert!(visual.debug_bounds("chart-notice").is_some(), "and shown under the chart");
    assert!(!missing.exists());
}

// ---- performance (on demand) ----------------------------------------------------------------------------------
//
//   cargo test --release -p tablex-gpui chart_render_allocations -- --ignored --nocapture --test-threads=1
//   cargo test --release -p tablex-gpui chart_prepare_one_million_rows -- --ignored --nocapture --test-threads=1

fn tab_renders(tab: u64) -> u64 {
    results::surface::RENDERS.with(|renders| renders.borrow().get(&tab).copied().unwrap_or(0))
}

/// Allocations, bytes and result-area renders per frame over `frames` frames made by `frame`.
fn measure_frames(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    frames: u64,
    mut frame: impl FnMut(&gpui::Entity<AppRoot>, &mut gpui::VisualTestContext),
) -> (u64, u64, f64) {
    let tab = app.read_with(visual, |this, _| this.active_id());
    for _ in 0..3 {
        frame(app, visual);
    }
    let (allocs, bytes, renders) = (crate::grid_bench::allocation_count(), crate::grid_bench::allocation_bytes(), tab_renders(tab));
    for _ in 0..frames {
        frame(app, visual);
    }
    (
        (crate::grid_bench::allocation_count() - allocs) / frames,
        (crate::grid_bench::allocation_bytes() - bytes) / frames,
        (tab_renders(tab) - renders) as f64 / frames as f64,
    )
}

#[gpui::test]
#[ignore = "on-demand allocation measurement; run with --release --ignored --nocapture"]
fn chart_render_allocations(cx: &mut TestAppContext) {
    const FRAMES: u64 = 20;
    let (app, visual) = chart_app(cx, sales_batch(100_000));
    let idle = |_: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    };
    let redraw = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.update(visual, |_, cx| cx.notify());
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    };
    let grid_idle = measure_frames(&app, visual, FRAMES, idle);
    let grid_redraw = measure_frames(&app, visual, FRAMES, redraw);
    let mut rows = vec![("Grid", grid_idle, grid_redraw)];
    for kind in ChartKind::ALL {
        set_spec(&app, visual, |spec| {
            spec.kind = kind;
            spec.x = if kind == ChartKind::Scatter { 1 } else { 0 };
            spec.aggregation = if kind == ChartKind::Scatter { Aggregation::None } else { Aggregation::Sum };
        });
        app.update(visual, |this, cx| {
            let tab = this.active_id();
            if !this.chart_presentation_active(tab) {
                this.toggle_chart_presentation(tab, cx);
            }
        });
        settle(visual);
        let idle_row = measure_frames(&app, visual, FRAMES, idle);
        let redraw_row = measure_frames(&app, visual, FRAMES, redraw);
        rows.push((kind.label(), idle_row, redraw_row));
    }
    eprintln!("per frame (mean of {FRAMES}, after 3 warm-up frames)           idle refresh                      full redraw (AppRoot notified)");
    eprintln!("{:<10} {:>10} {:>12} {:>8}   {:>10} {:>12} {:>8}", "mode", "allocs", "bytes", "renders", "allocs", "bytes", "renders");
    for (name, idle, redraw) in &rows {
        eprintln!("{name:<10} {:>10} {:>12} {:>8.2}   {:>10} {:>12} {:>8.2}", idle.0, idle.1, idle.2, redraw.0, redraw.1, redraw.2);
    }
}

#[test]
#[ignore = "on-demand timing; run with --release --ignored --nocapture"]
fn chart_prepare_one_million_rows() {
    use arrow_array::{ArrayRef, Date32Array, Float64Array, StringArray};
    use std::time::Instant;
    let rows = 1_000_000usize;
    let names: Vec<String> = (0..1_000).map(|i| format!("customer-{i:04}")).collect();
    let region: ArrayRef = Arc::new(StringArray::from_iter((0..rows).map(|i| Some(names[(i * 7919) % names.len()].as_str()))));
    let day: ArrayRef = Arc::new(Date32Array::from_iter_values((0..rows).map(|i| 18_000 + ((i * 31) % 3_650) as i32)));
    let amount: ArrayRef = Arc::new(Float64Array::from_iter((0..rows).map(|i| Some(((i * 37) % 9_000) as f64 / 7.0))));
    let seq: ArrayRef = Arc::new(Float64Array::from_iter_values((0..rows).map(|i| i as f64)));
    let batch = arrow_array::RecordBatch::try_from_iter(vec![("customer", region), ("day", day), ("amount", amount), ("seq", seq)]).unwrap();
    let batches: Vec<_> = (0..10).map(|i| Arc::new(batch.slice(i * 100_000, 100_000))).collect();
    let source = cd::ChartSource::new(&batches);
    assert_eq!(source.rows(), rows);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let cases: [(&str, cd::ChartSpec); 6] = [
        ("bar   SUM(amount) by customer (1,000 groups)", cd::ChartSpec { kind: ChartKind::Bar, x: 0, values: vec![2], aggregation: Aggregation::Sum, series_by: None }),
        ("line  SUM(amount) by day (3,650 groups)", cd::ChartSpec { kind: ChartKind::Line, x: 1, values: vec![2], aggregation: Aggregation::Sum, series_by: None }),
        ("area  raw amount by seq (1M points -> 2,560)", cd::ChartSpec { kind: ChartKind::Area, x: 3, values: vec![2], aggregation: Aggregation::None, series_by: None }),
        ("scatter raw amount by seq (1M -> 5,000)", cd::ChartSpec { kind: ChartKind::Scatter, x: 3, values: vec![2], aggregation: Aggregation::None, series_by: None }),
        ("pie   SUM(amount) by customer (top 11 + Other)", cd::ChartSpec { kind: ChartKind::Pie, x: 0, values: vec![2], aggregation: Aggregation::Sum, series_by: None }),
        ("bar   COUNT(*) by customer (1,000 groups)", cd::ChartSpec { kind: ChartKind::Bar, x: 0, values: vec![], aggregation: Aggregation::Count, series_by: None }),
    ];
    eprintln!("prepare over {rows} rows (10 batches), single thread");
    for (name, spec) in cases {
        let started = Instant::now();
        let model = cd::prepare(&source, &spec, 2_560, &cancel).expect("prepared");
        eprintln!("{:>8.1} ms  {name}: {} points drawn, notes {:?}", started.elapsed().as_secs_f64() * 1e3, model.drawn_points(), model.notes.iter().map(ToString::to_string).collect::<Vec<_>>());
    }
}

// ---- results as the real drivers deliver them -------------------------------------------------------------------

fn native(name: &str, data_type: arrow_schema::DataType, native_type: &str) -> arrow_schema::Field {
    arrow_schema::Field::new(name, data_type, true)
        .with_metadata(std::collections::HashMap::from([(crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_owned(), native_type.to_owned())]))
}

/// A page shaped as the PostgreSQL driver builds one: integers and floats typed, everything else `Utf8`
/// with the type's name in the field metadata (`numeric`, `date`, `timestamp with time zone`...).
fn postgres_shaped_page() -> crate::grid_arrow::ArrowGridSource {
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::DataType::{Int64, Utf8};
    let region = ["north", "south", "east"];
    let rows = 9usize;
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(region[i % 3])))),
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(format!("{}.{}5", i, i % 4))))),
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(format!("2024-01-{:02}", 1 + (rows - i) % 9))))),
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(format!("2024-03-{:02} 10:00:00+00", 1 + (rows - i) % 9))))),
        Arc::new(Int64Array::from_iter_values((0..rows as i64).map(|i| i * 10))),
    ];
    let schema = Arc::new(arrow_schema::Schema::new(vec![
        native("region", Utf8, "text"),
        native("avg_x", Utf8, "numeric"),
        native("day", Utf8, "date"),
        native("ts", Utf8, "timestamp with time zone"),
        native("n", Int64, "bigint"),
    ]));
    crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(arrow_array::RecordBatch::try_new(schema, columns).unwrap())])
}

/// A page shaped as the MySQL / MariaDB driver builds one: DECIMAL, DATE and DATETIME are `Utf8` with no field
/// metadata at all; the type names travel in the page's column list.
fn mysql_shaped_page() -> crate::grid_arrow::ArrowGridSource {
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::{DataType::{Int64, Utf8}, Field};
    let region = ["north", "south", "east"];
    let rows = 9usize;
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(region[i % 3])))),
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(format!("{}", 3 + i))))),
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(format!("2024-01-{:02}", 1 + (rows - i) % 9))))),
        Arc::new(StringArray::from_iter((0..rows).map(|i| Some(format!("2024-03-{:02} 10:00:00", 1 + (rows - i) % 9))))),
        Arc::new(Int64Array::from_iter_values((0..rows as i64).map(|i| i * 10))),
    ];
    let schema = Arc::new(arrow_schema::Schema::new(vec![
        Field::new("region", Utf8, true),
        Field::new("total", Utf8, true),
        Field::new("day", Utf8, true),
        Field::new("at", Utf8, true),
        Field::new("n", Int64, true),
    ]));
    crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(arrow_array::RecordBatch::try_new(schema, columns).unwrap())])
        .with_driver_types(["varchar", "decimal", "date", "datetime", "bigint"].map(str::to_owned))
}

fn drivers_text_columns_are_charted(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, value: usize, day: usize, at: usize) {
    show_chart(app, visual);
    // The defaults already see the numeric column behind the text and the repeating region.
    let spec = spec_of(app, visual);
    assert_eq!((spec.x, spec.values.clone(), spec.aggregation), (0, vec![value], Aggregation::Sum));
    let scene = scene_of(app, visual);
    assert_eq!(scene.model.series_names().len(), 1);
    assert!(scene.model.drawn_points() == 3, "three regions: {:?}", scene.model.title);
    // A date and a timestamp column order chronologically along X of a line.
    for column in [day, at] {
        set_spec(app, visual, |spec| {
            spec.kind = ChartKind::Line;
            spec.x = column;
            spec.aggregation = Aggregation::Sum;
        });
        let scene = scene_of(app, visual);
        let ChartData::Categories(data) = &scene.model.data else { panic!("categories") };
        let sorted: Vec<String> = {
            let mut labels: Vec<String> = data.labels.iter().map(ToString::to_string).collect();
            labels.sort();
            labels
        };
        assert_eq!(data.labels.iter().map(ToString::to_string).collect::<Vec<_>>(), sorted, "chronological");
        assert_eq!(data.labels.len(), 9, "nine distinct days");
    }
    // As a scatter's X too.
    set_spec(app, visual, |spec| {
        spec.kind = ChartKind::Scatter;
        spec.x = at;
        spec.aggregation = Aggregation::None;
    });
    assert!(matches!(&scene_of(app, visual).model.data, ChartData::Points(points) if points.x_temporal));
    // And the numeric text is numbers: a value column in the Select is not refused.
    assert!(issue_of(app, visual).is_none());
}

#[gpui::test]
fn postgres_shaped_pages_chart_numeric_and_date_text_columns(cx: &mut TestAppContext) {
    let (app, visual) = chart_app_source(cx, postgres_shaped_page());
    drivers_text_columns_are_charted(&app, visual, 1, 2, 3);
    // Real text is still refused as a value, whatever it looks like.
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Bar;
        spec.x = 1;
        spec.values = vec![0];
        spec.aggregation = Aggregation::Sum;
    });
    assert_eq!(issue_of(&app, visual), Some(cd::ChartIssue::NotNumeric { column: "region".into() }));
}

#[gpui::test]
fn mysql_shaped_pages_chart_decimal_and_date_text_columns_by_the_drivers_type_names(cx: &mut TestAppContext) {
    let (app, visual) = chart_app_source(cx, mysql_shaped_page());
    drivers_text_columns_are_charted(&app, visual, 1, 2, 3);
}

#[gpui::test]
fn a_column_whose_catalog_type_lands_after_the_chart_is_shown_is_classed_again(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, StringArray};
    // A result whose driver says nothing of the types: `total` and `day` arrive as text. The catalog of the table is read
    // after the chart opened and says what they are.
    let region: ArrayRef = Arc::new(StringArray::from(vec!["north", "south", "north", "east"]));
    let total: ArrayRef = Arc::new(StringArray::from(vec!["10.5", "4", "20.25", "7"]));
    let day: ArrayRef = Arc::new(StringArray::from(vec!["2024-01-01", "2024-01-02", "2024-01-03", "2024-01-04"]));
    let (app, visual) = chart_app(cx, arrow_array::RecordBatch::try_from_iter(vec![("region", region), ("total", total), ("day", day)]).unwrap());
    show_chart(&app, visual);
    let classes = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> Vec<cd::ColumnClass> {
        chart_state(app, visual, |s| s.source_for_test().map(|source| source.columns().iter().map(|c| c.class).collect()).unwrap_or_default())
    };
    assert_eq!(classes(&app, visual), [cd::ColumnClass::Text; 3], "no type is known: text");
    assert_eq!(issue_of(&app, visual), Some(cd::ChartIssue::NoNumericColumn));
    let prepares = chart_state(&app, visual, |s| s.prepares);

    // The catalog lands.
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        let column = |name: &str, data_type: &str| crate::catalog::CatalogColumn { name: name.into(), data_type: data_type.into(), ..Default::default() };
        this.workspace.runtime_mut(tab).expect("the tab").set_structure_columns(vec![column("region", "text"), column("total", "numeric(10,2)"), column("day", "date")]);
        this.notify_tab(tab, cx);
    });
    settle(visual);
    assert_eq!(classes(&app, visual), [cd::ColumnClass::Text, cd::ColumnClass::Numeric, cd::ColumnClass::Temporal], "classed again by the catalog");
    assert!(chart_state(&app, visual, |s| s.prepares) > prepares, "and the chart is prepared again");
    // The defaults the user had not touched are made over: the number is the value, and the chart is drawn.
    let spec = spec_of(&app, visual);
    assert_eq!((spec.x, spec.values.clone()), (0, vec![1]));
    assert!(issue_of(&app, visual).is_none());
    assert_eq!(scene_of(&app, visual).model.series_names().len(), 1);

    // A spec the user changed stays when the catalog changes again.
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Line;
        spec.x = 2;
        spec.aggregation = Aggregation::Max;
    });
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        let runtime = this.workspace.runtime_mut(tab).expect("the tab");
        let mut columns = std::mem::take(&mut runtime.structure_columns);
        columns.push(crate::catalog::CatalogColumn { name: "other".into(), data_type: "int".into(), ..Default::default() });
        runtime.set_structure_columns(columns);
        this.notify_tab(tab, cx);
    });
    settle(visual);
    let kept = spec_of(&app, visual);
    assert_eq!((kept.kind, kept.x, kept.aggregation), (ChartKind::Line, 2, Aggregation::Max));
}

#[gpui::test]
fn a_catalog_replaced_by_another_of_the_same_size_at_the_same_address_classes_the_columns_again(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, StringArray};
    let region: ArrayRef = Arc::new(StringArray::from(vec!["north", "south", "north", "east"]));
    let total: ArrayRef = Arc::new(StringArray::from(vec!["10.5", "4", "20.25", "7"]));
    let day: ArrayRef = Arc::new(StringArray::from(vec!["2024-01-01", "2024-01-02", "2024-01-03", "2024-01-04"]));
    let (app, visual) = chart_app(cx, arrow_array::RecordBatch::try_from_iter(vec![("region", region), ("total", total), ("day", day)]).unwrap());
    show_chart(&app, visual);
    let classes = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> Vec<cd::ColumnClass> {
        chart_state(app, visual, |s| s.source_for_test().map(|source| source.columns().iter().map(|c| c.class).collect()).unwrap_or_default())
    };
    // A catalog replaced by one of the same length, in the buffer of the first (what an address and a length cannot tell apart):
    // `kinds` are the types the tab's table has for the three columns, written the way a catalog load writes them.
    let replace = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, kinds: [&str; 3]| -> usize {
        app.update(visual, |this, cx| {
            let tab = this.active_id();
            let runtime = this.workspace.runtime_mut(tab).expect("the tab");
            let mut columns = std::mem::take(&mut runtime.structure_columns);
            columns.clear();
            columns.extend(["region", "total", "day"].into_iter().zip(kinds).map(|(name, data_type)| crate::catalog::CatalogColumn { name: name.into(), data_type: data_type.into(), ..Default::default() }));
            let (address, length) = (columns.as_ptr() as usize, columns.len());
            runtime.set_structure_columns(columns);
            assert_eq!(length, 3);
            this.notify_tab(tab, cx);
            address
        })
    };
    let first = replace(&app, visual, ["text", "numeric(10,2)", "date"]);
    settle(visual);
    assert_eq!(classes(&app, visual), [cd::ColumnClass::Text, cd::ColumnClass::Numeric, cd::ColumnClass::Temporal]);
    assert!(issue_of(&app, visual).is_none());
    // Another catalog of the same size at the same address: the columns are not numbers or dates any more.
    let second = replace(&app, visual, ["text", "text", "text"]);
    settle(visual);
    assert_eq!(second, first, "the same buffer: an address and a length see nothing");
    assert_eq!(classes(&app, visual), [cd::ColumnClass::Text; 3], "classed again by the catalog that replaced the first");
    assert_eq!(issue_of(&app, visual), Some(cd::ChartIssue::NoNumericColumn), "and the chart follows it");
    // And back: the first catalog again (A, B, A).
    replace(&app, visual, ["text", "numeric(10,2)", "date"]);
    settle(visual);
    assert_eq!(classes(&app, visual), [cd::ColumnClass::Text, cd::ColumnClass::Numeric, cd::ColumnClass::Temporal]);
    assert!(issue_of(&app, visual).is_none());
}

#[gpui::test]
fn the_catalog_the_navigator_loads_moves_a_chart_that_was_shown_before_it_landed(cx: &mut TestAppContext) {
    // The SQLite driver labels every text column `text` and a driver's label is what the chart classes by, so the catalog cannot
    // turn `day` into a date here; what this shows is that the load a table tab starts reaches the chart on screen: its catalog
    // moves, and the chart takes the page and prepares it again.
    let db = SqliteDb::new("CREATE TABLE sales(region TEXT, day DATE, total INTEGER); INSERT INTO sales VALUES ('north', '2024-01-01', 4), ('south', '2024-01-02', 9), ('east', '2024-01-03', 6);");
    let mut rig = SqlRig::sqlite(cx, &db);
    let tab = rig.tab;
    // A result of the SQL editor has no catalog.
    rig.run_on(tab, "SELECT region, day, total FROM sales");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { rows: 3 }), "{:?}", rig.status());
    rig.app.update(rig.visual, |this, cx| this.toggle_chart_presentation(tab, cx));
    live_chart_ready(&mut rig, "the chart before the catalog");
    let (generation, prepares) = (rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().structure_generation), chart_state(&rig.app, rig.visual, |s| s.prepares));
    assert_eq!(chart_state(&rig.app, rig.visual, |s| s.catalog_for_test()), generation);

    // The tab becomes the table's own, which reads its catalog behind the rows on screen through the navigator (the load a table
    // tab starts when its rows arrive).
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        let target = this.workspace.tab_mut(tab).unwrap();
        target.database = "main".into();
        target.schema = Some("main".into());
        target.collection = "sales".into();
        target.query_mode = Some(QueryMode::SqlTable);
        assert_eq!(target.connection_id, key);
        this.ensure_table_structure(tab, cx);
    });
    let app = rig.app.clone();
    wait_until(rig.visual, "the catalog to land", |visual| {
        visual.update(|window, _| window.refresh());
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().structure_generation > generation)
    });
    settle(rig.visual);
    let landed = rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().structure_columns.iter().map(|c| (c.name.clone(), c.data_type.to_lowercase())).collect::<Vec<_>>());
    assert_eq!(landed, [("region".to_owned(), "text".to_owned()), ("day".to_owned(), "date".to_owned()), ("total".to_owned(), "integer".to_owned())], "the driver's own catalog");
    let now = rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().structure_generation);
    assert_eq!(chart_state(&rig.app, rig.visual, |s| s.catalog_for_test()), now, "the chart on screen has the catalog that landed");
    assert!(chart_state(&rig.app, rig.visual, |s| s.prepares) > prepares, "and prepared its chart again");
}

#[gpui::test]
fn find_does_not_open_over_a_chart_and_the_grid_selection_stays(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    app.update(visual, |this, _| this.av().grid_ui.selection.borrow_mut().select(CellPosition { row: 4, column: 1 }));
    show_chart(&app, visual);
    visual.simulate_keystrokes("secondary-f");
    settle(visual);
    assert!(!app.read_with(visual, |this, _| this.av().grid_ui.find.open), "no find bar over a chart");
    assert!(visual.debug_bounds("chart-pane").is_some());
    assert_eq!(selection_end(&app, visual), Some(CellPosition { row: 4, column: 1 }), "the grid selection did not move");
    // Back on the grid, Cmd+F finds again.
    click_selector(visual, "chart-rail-button");
    visual.simulate_keystrokes("secondary-f");
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.av().grid_ui.find.open));
}

#[gpui::test]
fn picking_a_value_column_keeps_what_was_typed_in_the_combobox_search(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    click_selector(visual, "chart-values");
    let trigger = visual.debug_bounds("chart-values").unwrap();
    wait_for_menu(visual, trigger, 240.0, true);
    visual.simulate_input("qty");
    for _ in 0..50 {
        settle(visual);
        if app.read_with(visual, |this, cx| this.av().chart.values_query_for_test(cx)) == "qty" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    settle(visual);
    settle(visual);
    assert_eq!(app.read_with(visual, |this, cx| this.av().chart.values_query_for_test(cx)), "qty");
    // Pick the first row under the search field (which column it is depends on how far the list has filtered).
    visual.simulate_click(gpui::point(trigger.center().x, trigger.bottom() + gpui::px(48.7)), Default::default());
    settle(visual);
    let values = spec_of(&app, visual).values;
    assert!(values.len() == 2 && values.contains(&2), "a column was added: {values:?}");
    assert_eq!(app.read_with(visual, |this, cx| this.av().chart.values_query_for_test(cx)), "qty", "the search text survives the pick");
}

#[gpui::test]
fn leaving_chart_or_closing_the_tab_stops_the_computation_in_flight(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(50_000));
    let tab = app.read_with(visual, |this, _| this.active_id());
    // Start a computation and draw a frame without letting it run.
    app.update(visual, |this, cx| this.toggle_chart_presentation(tab, cx));
    visual.update(|window, _| window.refresh());
    let flag = chart_state(&app, visual, |s| s.pending_cancel_for_test()).expect("a computation is in flight");
    assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
    // Back to the grid: the next frame releases the chart, and the work is told to stop.
    app.update(visual, |this, cx| this.toggle_chart_presentation(tab, cx));
    visual.update(|window, _| window.refresh());
    assert!(flag.load(std::sync::atomic::Ordering::Relaxed), "leaving Chart cancels the work");
    assert!(chart_state(&app, visual, |s| s.pending_cancel_for_test()).is_none());
    settle(visual);
    assert!(chart_state(&app, visual, |s| s.outcome_for_test().is_none()), "and nothing is kept");

    // Closing the tab does the same.
    app.update(visual, |this, cx| this.toggle_chart_presentation(tab, cx));
    visual.update(|window, _| window.refresh());
    let flag = chart_state(&app, visual, |s| s.pending_cancel_for_test()).expect("in flight again");
    assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
    app.update(visual, |this, cx| this.close_tab(tab, cx));
    assert!(flag.load(std::sync::atomic::Ordering::Relaxed), "closing the tab cancels the work");
}

#[gpui::test]
fn a_chart_of_one_page_of_a_bigger_result_says_so(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    let tab = app.read_with(visual, |this, _| this.active_id());
    app.update(visual, |this, cx| {
        // The result is whole: the driver reports no next page.
        this.workspace.runtime_mut(tab).unwrap().pagination.set_driver_page(None, false, false);
        cx.notify();
    });
    show_chart(&app, visual);
    assert!(!scene_of(&app, visual).model.notes.iter().any(|n| matches!(n, cd::ChartNote::LoadedOnly { .. })), "a whole result says nothing");

    // The driver says another page exists: a new page arrives and the chart carries the note.
    app.update(visual, |this, cx| {
        this.workspace.runtime_mut(tab).unwrap().pagination.set_driver_page(None, false, true);
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(sales_batch(200))]))));
        cx.notify();
    });
    settle(visual);
    let scene = scene_of(&app, visual);
    assert!(scene.model.notes.contains(&cd::ChartNote::LoadedOnly { rows: 200 }), "{:?}", scene.model.notes);
    assert!(visual.debug_bounds("chart-notes").is_some(), "the note is shown under the chart");
}

#[gpui::test]
fn choosing_scatter_does_not_move_x_onto_a_column_that_is_a_value(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    // `month` (a date) and `amount` are the candidates for X; `month` is already a value here.
    set_spec(&app, visual, |spec| spec.values = vec![1, 3]);
    pick_with_keys(visual, "chart-kind", 3);
    let spec = spec_of(&app, visual);
    assert_eq!(spec.kind, ChartKind::Scatter);
    assert_eq!(spec.x, 2, "amount: the first number or date that is not a value");
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) ----------------------------------
//
//   TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//   TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//   TABLEX_TEST_MARIADB_URI=mariadb://tablex:tablex@127.0.0.1:53307/tablex \
//     cargo test -q -j 6 -p tablex-gpui --bin tablex-gpui -- --ignored --test-threads=1 chart_live
//
// Each query goes through the real driver into Chart mode and must be charted, not refused. Only VALUES lists and
// SELECT literals: nothing is created, nothing is left behind.

struct LiveChart {
    /// Two groups with a numeric aggregate that is text on the wire (`AVG(...)::numeric`, `SUM(int)`), as `[label, value]`.
    numeric_sql: &'static str,
    numeric_expect: [(&'static str, f64); 2],
    /// Rows `(x, v)` with `v` = 1, 2, 3 in order of `x`, `x` a date or timestamp that is text on the wire.
    time_queries: &'static [&'static str],
}

fn live_chart_ready(rig: &mut SqlRig<'_>, title: &'static str) -> std::rc::Rc<results::chart::ChartScene> {
    let app = rig.app.clone();
    wait_until(rig.visual, title, |visual| {
        visual.update(|window, _| window.refresh());
        app.read_with(visual, |this, _| this.av().chart.outcome_for_test().is_some())
    });
    scene_of(&rig.app, rig.visual)
}

fn live_chart(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str, live: &LiveChart) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let tab = rig.tab;

    // A numeric aggregate: the default chart is a bar of the groups, not a refusal.
    rig.run_on(tab, live.numeric_sql);
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    rig.app.update(rig.visual, |this, cx| this.toggle_chart_presentation(tab, cx));
    let scene = live_chart_ready(&mut rig, "the numeric chart");
    let ChartData::Categories(data) = &scene.model.data else { panic!("categories: {:?}", scene.model.data) };
    assert_eq!(
        data.labels.iter().map(ToString::to_string).collect::<Vec<_>>(),
        live.numeric_expect.map(|(label, _)| label.to_owned())
    );
    assert_eq!(data.series[0].values, live.numeric_expect.map(|(_, value)| Some(value)), "{}", scene.model.title);
    assert!(issue_of(&rig.app, rig.visual).is_none());

    // Dates and timestamps that arrive as text: ordered chronologically along a line, and also as a scatter's X.
    for (i, sql) in live.time_queries.iter().enumerate() {
        rig.run_on(tab, sql);
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{sql}: {:?}", rig.status());
        // The result has new columns: the chart starts over on them (the chart type is kept).
        let columns = rig.app.read_with(rig.visual, |this, _| this.active_grid().column_count());
        assert_eq!(columns, 2, "{sql}");
        rig.app.update(rig.visual, |this, cx| {
            this.view_mut(tab).chart.edit_spec_for_test(|spec| {
                spec.kind = ChartKind::Line;
                spec.x = 0;
                spec.values = vec![1];
                spec.aggregation = Aggregation::None;
            });
            this.notify_tab(tab, cx);
        });
        let scene = live_chart_ready(&mut rig, "the time chart");
        let ChartData::Categories(data) = &scene.model.data else { panic!("categories") };
        assert_eq!(data.series[0].values, vec![Some(1.0), Some(2.0), Some(3.0)], "{sql}: oldest first");
        let labels: Vec<String> = data.labels.iter().map(ToString::to_string).collect();
        assert!(labels.iter().all(|l| l.starts_with("2024-01-")), "{labels:?}");
        rig.app.update(rig.visual, |this, cx| {
            this.view_mut(tab).chart.edit_spec_for_test(|spec| spec.kind = ChartKind::Scatter);
            this.notify_tab(tab, cx);
        });
        let scene = live_chart_ready(&mut rig, "the scatter");
        assert!(matches!(&scene.model.data, ChartData::Points(points) if points.x_temporal && points.series[0].points.len() == 3), "query {i}");
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn chart_live_postgres_charts_numeric_and_timestamptz_text(cx: &mut TestAppContext) {
    live_chart(
        cx,
        ConnectionKind::Postgres,
        ("dbengine-postgres", "abi-v2"),
        "TABLEX_TEST_PG_URI",
        &LiveChart {
            numeric_sql: "SELECT r AS region, AVG(x)::numeric AS avg_x FROM (VALUES ('north', 1), ('north', 2), ('south', 5)) v(r, x) GROUP BY r ORDER BY r",
            numeric_expect: [("north", 1.5), ("south", 5.0)],
            time_queries: &[
                "SELECT ts, v FROM (VALUES ('2024-01-03 10:00:00+00'::timestamptz, 3), ('2024-01-01 10:00:00+00'::timestamptz, 1), ('2024-01-02 12:00:00+02'::timestamptz, 2)) t(ts, v) ORDER BY v DESC",
                "SELECT d, v FROM (VALUES (DATE '2024-01-03', 3), (DATE '2024-01-01', 1), (DATE '2024-01-02', 2)) t(d, v) ORDER BY v DESC",
            ],
        },
    );
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn chart_live_postgres_sum_of_bigint_is_a_number(cx: &mut TestAppContext) {
    live_chart(
        cx,
        ConnectionKind::Postgres,
        ("dbengine-postgres", "abi-v2"),
        "TABLEX_TEST_PG_URI",
        &LiveChart {
            numeric_sql: "SELECT r AS region, SUM(x::bigint) AS total FROM (VALUES ('north', 1), ('north', 2), ('south', 5)) v(r, x) GROUP BY r ORDER BY r",
            numeric_expect: [("north", 3.0), ("south", 5.0)],
            time_queries: &[],
        },
    );
}

const MYSQL_LIVE: LiveChart = LiveChart {
    numeric_sql: "SELECT g, SUM(n) AS total FROM (SELECT 'a' AS g, 1 AS n UNION ALL SELECT 'a', 2 UNION ALL SELECT 'b', 5) t GROUP BY g ORDER BY g",
    numeric_expect: [("a", 3.0), ("b", 5.0)],
    time_queries: &[
        "SELECT d, v FROM (SELECT CAST('2024-01-03' AS DATE) AS d, 3 AS v UNION ALL SELECT CAST('2024-01-01' AS DATE), 1 UNION ALL SELECT CAST('2024-01-02' AS DATE), 2) t ORDER BY v DESC",
        "SELECT d, v FROM (SELECT CAST('2024-01-03 10:00:00' AS DATETIME) AS d, 3 AS v UNION ALL SELECT CAST('2024-01-01 10:00:00' AS DATETIME), 1 UNION ALL SELECT CAST('2024-01-02 10:00:00' AS DATETIME), 2) t ORDER BY v DESC",
    ],
};

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn chart_live_mysql_charts_sum_of_int_and_date_text(cx: &mut TestAppContext) {
    live_chart(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI", &MYSQL_LIVE);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn chart_live_mariadb_charts_sum_of_int_and_date_text(cx: &mut TestAppContext) {
    live_chart(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI", &MYSQL_LIVE);
}

#[gpui::test]
fn only_line_and_area_are_prepared_again_when_the_window_is_resized(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    let prepares = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| chart_state(app, visual, |s| s.prepares);
    for (kind, again) in [(ChartKind::Bar, false), (ChartKind::Pie, false), (ChartKind::Line, true), (ChartKind::Area, true)] {
        set_spec(&app, visual, |spec| spec.kind = kind);
        let before = prepares(&app, visual);
        // From the narrowest to the widest budget step (the budget is twice the width, in steps of 512).
        for width in [500.0, 1_900.0] {
            visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(760.0)));
            settle(visual);
        }
        assert_eq!(prepares(&app, visual) > before + 1, again, "{kind:?}: {} -> {}", before, prepares(&app, visual));
    }
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(760.0)));
}

/// The hover dots an official chart paints (their halos, 20px, one per series) as `(series color hue, centre y)`.
fn hover_halos(visual: &mut gpui::VisualTestContext) -> Vec<(f32, f32)> {
    let mut halos: Vec<(f32, f32)> = painted_quads(visual)
        .iter()
        .filter(|q| (q.bounds.size.width.0 / 2.0 - 20.0).abs() < 0.6 && q.background.as_solid().is_some_and(|c| (c.a - 0.2).abs() < 0.05))
        .map(|q| (q.background.as_solid().unwrap().h, q.bounds.origin.y.0 / 2.0 + 10.0))
        .collect();
    halos.sort_by(|a, b| a.1.total_cmp(&b.1));
    halos
}

/// The tooltip's dots are placed by the same scales as the line: for one category the dots' vertical gaps are in
/// the ratio of the values' gaps, whatever the plot's size and offset.
fn category_of_dots(halos: &[(f32, f32)], series: &[Vec<f64>]) -> Option<usize> {
    let hue = |color: gpui::Rgba| gpui::Hsla::from(color).h;
    // Series 1..=3 take chart_1..chart_3.
    let y_of = |colour: f32| halos.iter().find(|(h, _)| (h - colour).abs() < 0.01).map(|(_, y)| *y);
    let ys = [y_of(hue(theme::chart_1()))?, y_of(hue(theme::chart_2()))?, y_of(hue(theme::chart_3()))?];
    let measured = (ys[0] - ys[1]) / (ys[1] - ys[2]);
    (0..series[0].len()).find(|&i| {
        let (a, b, c) = (series[0][i], series[1][i], series[2][i]);
        let expected = ((a - b) / (b - c)) as f32;
        // Both the ratio and the direction (a higher value is a higher dot: a smaller y).
        (expected - measured).abs() < 0.03 * expected.abs().max(1.0) && (ys[0] < ys[1]) == (a > b) && (ys[1] < ys[2]) == (b > c)
    })
}

#[gpui::test]
fn lines_and_areas_paint_the_values_where_their_scales_put_them(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, Float64Array, StringArray};
    let labels = ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"];
    let a = vec![10.0, 20.0, 30.0, 25.0, 12.0, 40.0, 18.0, 22.0];
    let b = vec![5.0, 9.0, 6.0, 20.0, 11.0, 3.0, 14.0, 8.0];
    let c = vec![1.0, 2.0, 12.0, 4.0, 3.0, 28.0, 7.0, 2.0];
    let column = |values: &[f64]| -> ArrayRef { Arc::new(Float64Array::from(values.to_vec())) };
    let k: ArrayRef = Arc::new(StringArray::from_iter(labels.iter().map(|l| Some(*l))));
    let batch = arrow_array::RecordBatch::try_from_iter(vec![("k", k), ("a", column(&a)), ("b", column(&b)), ("c", column(&c))]).unwrap();
    // The hover dots glide on a spring that runs on the wall clock: snap them.
    cx.update(|app| app.set_reduce_motion(true));
    let (app, visual) = chart_app(cx, batch);
    show_chart(&app, visual);
    let series = vec![a, b, c];
    for kind in [ChartKind::Line, ChartKind::Area] {
        set_spec(&app, visual, |spec| {
            spec.kind = kind;
            spec.x = 0;
            spec.values = vec![1, 2, 3];
            spec.aggregation = Aggregation::None;
        });
        let plot = visual.debug_bounds("chart-plot").expect("the plot");
        // Rest: no hover dots.
        hover(visual, gpui::point(plot.left() - gpui::px(80.0), plot.top()));
        assert!(hover_halos(visual).is_empty(), "{kind:?}: nothing hovered");
        // Three places along X land on three different categories, each with its three values painted in proportion.
        let mut seen = Vec::new();
        for fraction in [0.3, 0.62, 0.93] {
            hover(visual, gpui::point(plot.left() + plot.size.width * fraction, plot.center().y));
            let halos = hover_halos(visual);
            assert_eq!(halos.len(), 3, "{kind:?} @{fraction}: a dot per series");
            let category = category_of_dots(&halos, &series).unwrap_or_else(|| panic!("{kind:?} @{fraction}: the dots {halos:?} match no category's values"));
            seen.push(category);
        }
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{kind:?}: left to right is category order: {seen:?}");
    }
}

/// The painted quads of `color` (ignoring alpha) that are `width` logical pixels wide, as `(x, y)` of their centres, left to right.
fn mark_centres(visual: &mut gpui::VisualTestContext, color: gpui::Hsla, width: f32) -> Vec<(f32, f32)> {
    let mut centres: Vec<(f32, f32)> = painted_quads(visual)
        .iter()
        .filter(|q| (q.bounds.size.width.0 / 2.0 - width).abs() < 0.6 && q.background.as_solid().is_some_and(|c| near(c, color)))
        .map(|q| ((q.bounds.origin.x.0 + q.bounds.size.width.0 / 2.0) / 2.0, (q.bounds.origin.y.0 + q.bounds.size.height.0 / 2.0) / 2.0))
        .collect();
    centres.sort_by(|a, b| a.0.total_cmp(&b.0));
    centres
}

#[gpui::test]
fn a_missing_mean_breaks_the_line_on_screen_as_in_the_export_and_a_missing_sum_does_not(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, Float64Array, Int64Array, StringArray};
    // x 1, 2, 3 by series a / b: "b" has no row at x = 2, so its mean there is missing.
    let x: ArrayRef = Arc::new(Int64Array::from(vec![1, 2, 3, 1, 3]));
    let by: ArrayRef = Arc::new(StringArray::from(vec!["a", "a", "a", "b", "b"]));
    let v: ArrayRef = Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0, 5.0, 7.0]));
    let (app, visual) = chart_app(cx, arrow_array::RecordBatch::try_from_iter(vec![("x", x), ("by", by), ("v", v)]).unwrap());
    show_chart(&app, visual);
    let blue = series_hsla(theme::chart_2());
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Line;
        spec.x = 0;
        spec.values = vec![2];
        spec.aggregation = Aggregation::Avg;
        spec.series_by = Some(1);
    });
    let plot = visual.debug_bounds("chart-plot").expect("the plot");
    // The piece of "a" is a line; each of the two lone values of "b" is a 3px dot (a line cannot show a single point), the first
    // at the left end of the plot and the second at its right end, 5 and 7 of the way up the same axis.
    let dots = mark_centres(visual, blue, 3.0);
    assert_eq!(dots.len(), 2, "two pieces of one point: {dots:?}");
    assert!(dots[0].0 < plot.left().as_f32() + plot.size.width.as_f32() * 0.15 && dots[1].0 > plot.right().as_f32() - plot.size.width.as_f32() * 0.08, "{dots:?} in {plot:?}");
    assert!(dots[1].1 < dots[0].1, "7 is higher than 5: {dots:?}");
    let first_series = mark_centres(visual, series_hsla(theme::chart_1()), 3.0);
    assert!(first_series.is_empty(), "the first series is one whole line: {first_series:?}");
    // The same data as a sum: a missing sum is 0 and the line is whole, so there is no lone dot.
    set_spec(&app, visual, |spec| spec.aggregation = Aggregation::Sum);
    assert!(mark_centres(visual, blue, 3.0).is_empty());
    // An area breaks the same way.
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Area;
        spec.aggregation = Aggregation::Avg;
    });
    assert_eq!(mark_centres(visual, blue, 3.0).len(), 2);
}

/// x 1, 2, 3 by series a / b: "b" has no row at x = 2, so its mean there is missing (a gap) and its sum is 0 drawn as a value.
fn gap_chart(cx: &mut TestAppContext, kind: ChartKind) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    use arrow_array::{ArrayRef, Float64Array, Int64Array, StringArray};
    let x: ArrayRef = Arc::new(Int64Array::from(vec![1, 2, 3, 1, 3]));
    let by: ArrayRef = Arc::new(StringArray::from(vec!["a", "a", "a", "b", "b"]));
    let v: ArrayRef = Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0, 5.0, 7.0]));
    // The hover dots glide on a spring that runs on the wall clock: snap them.
    cx.update(|app| app.set_reduce_motion(true));
    let (app, visual) = chart_app(cx, arrow_array::RecordBatch::try_from_iter(vec![("x", x), ("by", by), ("v", v)]).unwrap());
    show_chart(&app, visual);
    set_spec(&app, visual, |spec| {
        spec.kind = kind;
        spec.x = 0;
        spec.values = vec![2];
        spec.aggregation = Aggregation::Avg;
        spec.series_by = Some(1);
    });
    (app, visual)
}

/// The mouse over category `index` of the three the gap chart has (the plot's left part is the label gutter), at the plot's middle height.
fn hover_category(visual: &mut gpui::VisualTestContext, index: usize) {
    let plot = visual.debug_bounds("chart-plot").expect("the plot");
    hover(visual, gpui::point(plot.left() + plot.size.width * [0.06, 0.5, 0.97][index], plot.center().y));
}

#[gpui::test]
fn hovering_a_gap_in_a_line_or_an_area_shows_one_dot_fewer_and_no_data_for_the_missing_series(cx: &mut TestAppContext) {
    for kind in [ChartKind::Line, ChartKind::Area] {
        let (app, visual) = gap_chart(cx, kind);
        let plot = visual.debug_bounds("chart-plot").expect("the plot");
        hover(visual, gpui::point(plot.left() - gpui::px(80.0), plot.top()));
        assert!(hover_halos(visual).is_empty(), "{kind:?}: nothing hovered");
        // Over x = 1 both series have a value: a dot each, and their values in the rows.
        hover_category(visual, 0);
        assert_eq!(hover_halos(visual).len(), 2, "{kind:?}: a dot per series");
        assert_eq!(results::chart::ChartScene::last_line_tooltip(), [("a".to_owned(), "1".to_owned()), ("b".to_owned(), "5".to_owned())], "{kind:?}");
        // Over x = 2 the mean of "b" is missing: its line is broken there, so it gets no dot, and its row says there was no data
        // (not 0, which is where a gap is not drawn).
        hover_category(visual, 1);
        let halos = hover_halos(visual);
        assert_eq!(halos.len(), 1, "{kind:?}: one dot fewer over the gap: {halos:?}");
        assert_eq!(results::chart::ChartScene::last_line_tooltip(), [("a".to_owned(), "2".to_owned()), ("b".to_owned(), "no data".to_owned())], "{kind:?}");
        // Back over x = 3 both again.
        hover_category(visual, 2);
        assert_eq!(hover_halos(visual).len(), 2, "{kind:?}");
        assert_eq!(results::chart::ChartScene::last_line_tooltip()[1], ("b".to_owned(), "7".to_owned()), "{kind:?}");
        // A sum has no gap: the missing sum is drawn as 0 and keeps its dot, but its row still says there was no data.
        set_spec(&app, visual, |spec| spec.aggregation = Aggregation::Sum);
        hover_category(visual, 1);
        assert_eq!(hover_halos(visual).len(), 2, "{kind:?}: the sum is drawn at 0");
        assert_eq!(results::chart::ChartScene::last_line_tooltip()[1], ("b".to_owned(), "no data".to_owned()), "{kind:?}");
    }
}

#[gpui::test]
fn a_hover_works_no_layout_out_of_a_line_chart_again_and_a_resize_does(cx: &mut TestAppContext) {
    let (_app, visual) = gap_chart(cx, ChartKind::Line);
    let layouts = results::chart::ChartScene::line_layouts_worked_out;
    let painted = layouts();
    // Moving the mouse over the chart renders and paints it again each time; none of it moves a point.
    for index in [0, 1, 2, 1, 0] {
        hover_category(visual, index);
    }
    assert_eq!(layouts(), painted, "the hovers worked out no geometry");
    // The window gets another size: the plot is laid out again, once, and then stays.
    visual.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(700.0)));
    settle(visual);
    let resized = layouts();
    assert!(resized > painted, "a new size is a new layout");
    hover_category(visual, 1);
    hover_category(visual, 2);
    assert_eq!(layouts(), resized);
    // Back to the first size: laid out again, since only the last size is kept.
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(760.0)));
    settle(visual);
    assert!(layouts() > resized, "the first size back is laid out again");
}

#[gpui::test]
fn a_scatter_paints_every_point_where_its_scales_put_it(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, Float64Array};
    // Known points: x 0, 1, 2, 4 and y 10, 40, 20, 30.
    let xs = [0.0, 1.0, 2.0, 4.0];
    let ys = [10.0, 40.0, 20.0, 30.0];
    let x: ArrayRef = Arc::new(Float64Array::from(xs.to_vec()));
    let y: ArrayRef = Arc::new(Float64Array::from(ys.to_vec()));
    let (app, visual) = chart_app(cx, arrow_array::RecordBatch::try_from_iter(vec![("x", x), ("y", y)]).unwrap());
    show_chart(&app, visual);
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Scatter;
        spec.x = 0;
        spec.values = vec![1];
        spec.aggregation = Aggregation::None;
    });
    let plot = visual.debug_bounds("chart-plot").expect("the plot");
    let dots = mark_centres(visual, series_hsla(theme::chart_1()), 7.0);
    assert_eq!(dots.len(), 4, "{dots:?}");
    // Left to right is the order of x, and the places are in proportion to the values: the pixels between the dots of x = 0
    // and x = 4 are shared out by x, and the pixels up from the lowest dot by y.
    let span = dots[3].0 - dots[0].0;
    for (dot, x) in dots.iter().zip(xs) {
        assert!(((dot.0 - dots[0].0) / span - (x / 4.0) as f32).abs() < 0.01, "x = {x} at {dot:?} of {dots:?}");
    }
    let (lowest, highest) = (dots[0].1, dots[1].1);
    for (dot, y) in dots.iter().zip(ys) {
        let fraction = (lowest - dot.1) / (lowest - highest);
        assert!((fraction - (y as f32 - 10.0) / 30.0).abs() < 0.01, "y = {y} at {dot:?} of {dots:?}");
    }
    // All of them inside the plot, the axes' room kept apart.
    assert!(dots.iter().all(|d| d.0 > plot.left().as_f32() && d.0 < plot.right().as_f32() && d.1 > plot.top().as_f32() && d.1 < plot.bottom().as_f32()), "{dots:?} in {plot:?}");
}

#[gpui::test]
fn a_pie_paints_its_wedges_where_the_values_put_them(cx: &mut TestAppContext) {
    use arrow_array::{ArrayRef, Float64Array, StringArray};
    let names = ["w", "x", "y", "z"];
    let values = [40.0, 30.0, 20.0, 10.0];
    let k: ArrayRef = Arc::new(StringArray::from_iter(names.iter().map(|n| Some(*n))));
    let v: ArrayRef = Arc::new(Float64Array::from(values.to_vec()));
    cx.update(|app| app.set_reduce_motion(true));
    let (app, visual) = chart_app(cx, arrow_array::RecordBatch::try_from_iter(vec![("k", k), ("v", v)]).unwrap());
    show_chart(&app, visual);
    set_spec(&app, visual, |spec| {
        spec.kind = ChartKind::Pie;
        spec.aggregation = Aggregation::None;
    });
    let plot = visual.debug_bounds("chart-plot").expect("the plot");
    let (center, radius) = (plot.center(), plot.size.height.as_f32() * 0.4);
    let at = |degrees: f32, fraction: f32| {
        // Clockwise from twelve o'clock.
        let angle = degrees.to_radians();
        gpui::point(center.x + gpui::px(radius * fraction * angle.sin()), center.y - gpui::px(radius * fraction * angle.cos()))
    };
    hover(visual, gpui::point(plot.left() - gpui::px(80.0), plot.top()));
    let rest = painted_quads(visual).len();
    // The tooltip's row swatch (8px) is the hovered wedge's color.
    let swatches = |visual: &mut gpui::VisualTestContext| -> Vec<gpui::Hsla> {
        painted_quads(visual)
            .iter()
            .filter(|q| (q.bounds.size.width.0 / 2.0 - 8.0).abs() < 0.6 && q.background.as_solid().is_some_and(|c| c.a > 0.99))
            .map(|q| q.background.as_solid().unwrap())
            .collect()
    };
    let palette = [theme::chart_1(), theme::chart_2(), theme::chart_3(), theme::chart_4()].map(gpui::Hsla::from);
    // The middle of each wedge (40%, 30%, 20%, 10% of the circle, in order) shows its own tooltip; outside the circle none.
    let mut start = 0.0f32;
    for (k, (name, value)) in names.iter().zip(values).enumerate() {
        let sweep = value as f32 / 100.0 * 360.0;
        hover(visual, at(start + sweep / 2.0, 0.7));
        assert!(painted_quads(visual).len() > rest, "{name}: hovering its wedge ({start}..{} degrees) shows a tooltip", start + sweep);
        let colors = swatches(visual);
        assert!(colors.iter().any(|c| near(*c, palette[k])), "{name}: the tooltip names wedge {k} (its swatch is that wedge's color), got {colors:?}");
        // Both edges of the wedge, a little inside.
        for edge in [start + 3.0, start + sweep - 3.0] {
            hover(visual, at(edge, 0.7));
            assert!(swatches(visual).iter().any(|c| near(*c, palette[k])), "{name}: {edge} degrees is inside wedge {k}");
        }
        start += sweep;
    }
    hover(visual, at(20.0, 1.35));
    assert_eq!(painted_quads(visual).len(), rest, "outside the circle there is no wedge");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn chart_live_postgres_labels_timestamptz_as_the_grid_shows_it(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let tab = rig.tab;
    // A +07 session (set on the session the queries run on; nothing persistent).
    rig.run_on(tab, "SET TIME ZONE 'Asia/Ho_Chi_Minh'");
    let values = "('2024-01-03 10:00:00+07'::timestamptz, 3), ('2024-01-01 09:00:00+07'::timestamptz, 1), ('2024-01-02 23:00:00+07'::timestamptz, 2)";
    let mut first = true;
    for (sql, grid_first, labels) in [
        (
            format!("SELECT date_trunc('day', ts) AS d, v FROM (VALUES {values}) t(ts, v) ORDER BY v DESC"),
            "2024-01-03 00:00:00+07",
            ["2024-01-01", "2024-01-02", "2024-01-03"],
        ),
        (
            format!("SELECT ts, v FROM (VALUES {values}) t(ts, v) ORDER BY v DESC"),
            "2024-01-03 10:00:00+07",
            ["2024-01-01 09:00:00", "2024-01-02 23:00:00", "2024-01-03 10:00:00"],
        ),
    ] {
        rig.run_on(tab, &sql);
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{sql}: {:?}", rig.status());
        // What the grid shows, in the session's zone.
        let shown = rig.app.read_with(rig.visual, |this, _| this.active_grid().cell_text(CellPosition { row: 0, column: 0 }).map(|text| text.to_string()));
        assert_eq!(shown.as_deref(), Some(grid_first), "the grid shows the session's wall clock");
        if first {
            rig.app.update(rig.visual, |this, cx| this.toggle_chart_presentation(tab, cx));
            first = false;
        }
        rig.app.update(rig.visual, |this, cx| {
            this.view_mut(tab).chart.edit_spec_for_test(|spec| {
                spec.kind = ChartKind::Line;
                spec.x = 0;
                spec.values = vec![1];
                spec.aggregation = Aggregation::None;
            });
            this.notify_tab(tab, cx);
        });
        let scene = live_chart_ready(&mut rig, "the chart");
        let ChartData::Categories(data) = &scene.model.data else { panic!("categories") };
        assert_eq!(data.labels.iter().map(ToString::to_string).collect::<Vec<_>>(), labels, "{sql}: the labels are the grid's wall clock");
        assert_eq!(data.series[0].values, vec![Some(1.0), Some(2.0), Some(3.0)]);
    }
    rig.run_on(tab, "RESET TIME ZONE");
}
