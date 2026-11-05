// Per-tab result views: each tab owns its scroll / selection / find state, and
// renders its result area through its own cached view.

/// Two tabs with results; tab B is active.
fn two_result_tabs(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64, u64) {
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
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    let (a, b) = app.update(visual, |this, cx| {
        let a = this.workspace.open_ran_query_tab("A");
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(wide_grid_batch(
                60, 20,
            ))]),
        )));
        let b = this.workspace.open_ran_query_tab("B");
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(wide_grid_batch(
                60, 20,
            ))]),
        )));
        cx.notify();
        (a, b)
    });
    visual.update(|window, _| window.activate_window());
    settle(visual);
    (app, visual, a, b)
}

/// Everything a tab's result view owns that a user can see change.
#[derive(Debug, PartialEq)]
struct TabViewSnapshot {
    selection: Option<crate::grid::CellRange>,
    find_query: String,
    find_matches: usize,
    find_current: Option<usize>,
    find_open: bool,
    horizontal_scroll: f32,
    vertical_scroll: f32,
    widths: Vec<f32>,
}

fn tab_view_snapshot(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab_id: u64) -> TabViewSnapshot {
    app.read_with(visual, |this, _| {
        let view = this.view(tab_id);
        TabViewSnapshot {
            selection: view.grid_ui.selection.borrow().range,
            find_query: view.grid_ui.find.query.clone(),
            find_matches: view.grid_ui.find.matches.total(),
            find_current: view.grid_ui.find.current,
            find_open: view.grid_ui.find.open,
            horizontal_scroll: f32::from(view.grid_horizontal_scroll.offset().x),
            vertical_scroll: f32::from(view.grid_scroll.0.borrow().base_handle.offset().y),
            widths: view.grid_ui.layout.widths.clone(),
        }
    })
}

fn activate(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) {
    app.update(visual, |this, cx| {
        this.workspace.activate_tab(tab_id);
        cx.notify();
    });
    settle(visual);
}

#[gpui::test]
fn each_tab_keeps_its_own_selection_scroll_find_and_layout_while_another_changes(
    cx: &mut TestAppContext,
) {
    let (app, visual, a, b) = two_result_tabs(cx);
    assert_ne!(a, b);

    // Tab A: select, scroll, resize a column and search.
    activate(&app, visual, a);
    app.update(visual, |this, cx| {
        this.select_grid_cell(a, CellPosition { row: 3, column: 2 });
        let view = this.view_mut(a);
        view.grid_horizontal_scroll
            .set_offset(gpui::point(gpui::px(-120.0), gpui::px(0.0)));
        let base = view.grid_scroll.0.borrow().base_handle.clone();
        base.set_offset(gpui::point(base.offset().x, gpui::px(-90.0)));
        view.grid_ui.layout.set_width(1, 222.0);
        cx.notify();
    });
    settle(visual);
    find_open(visual);
    find_type(visual, "r5c19");
    let a_before = tab_view_snapshot(&app, visual, a);
    assert_eq!(a_before.find_query, "r5c19");
    assert_eq!(a_before.find_matches, 1);
    assert!(a_before.find_open);
    assert_eq!(a_before.widths[1], 222.0);

    // Tab B changes every one of those things; A is untouched.
    let b_untouched = tab_view_snapshot(&app, visual, b);
    assert_eq!(b_untouched.selection, None, "B never saw A's selection");
    assert_eq!(b_untouched.find_query, "");
    assert!(!b_untouched.find_open, "B never saw A's find bar");
    assert_ne!(b_untouched.widths[1], 222.0, "B never saw A's column width");
    activate(&app, visual, b);
    app.update(visual, |this, cx| {
        this.select_grid_cell(b, CellPosition { row: 7, column: 4 });
        let view = this.view_mut(b);
        view.grid_horizontal_scroll
            .set_offset(gpui::point(gpui::px(-40.0), gpui::px(0.0)));
        let base = view.grid_scroll.0.borrow().base_handle.clone();
        base.set_offset(gpui::point(base.offset().x, gpui::px(-30.0)));
        cx.notify();
    });
    settle(visual);
    find_open(visual);
    find_type(visual, "r9c9");
    let b_after = tab_view_snapshot(&app, visual, b);
    assert_eq!(b_after.find_query, "r9c9");
    assert_eq!(b_after.find_matches, 1);
    assert_ne!(b_after.selection, a_before.selection);
    assert_eq!(tab_view_snapshot(&app, visual, a), a_before, "A is untouched by B");

    // Back on A: its search, selection, scroll and widths are exactly as left.
    activate(&app, visual, a);
    assert_eq!(tab_view_snapshot(&app, visual, a), a_before);
    assert_eq!(
        app.read_with(visual, |this, _| this.view(a).grid_ui.find.counter_text()),
        "1 / 1"
    );
    assert!(visual.debug_bounds("grid-find-bar").is_some(), "A's find bar is shown again");
    // And B is as left too.
    assert_eq!(tab_view_snapshot(&app, visual, b), b_after);
}

#[gpui::test]
fn closing_a_tab_drops_only_its_result_view(cx: &mut TestAppContext) {
    let (app, visual, a, b) = two_result_tabs(cx);
    app.update(visual, |this, cx| {
        this.select_grid_cell(a, CellPosition { row: 1, column: 1 });
        this.select_grid_cell(b, CellPosition { row: 2, column: 2 });
        this.close_tab(b, cx);
        cx.notify();
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(!this.tab_views.contains_key(&b), "closed tab's view is dropped");
        assert_eq!(
            this.view(a).grid_ui.selection.borrow().range.map(|range| range.start),
            Some(CellPosition { row: 1, column: 1 })
        );
    });
}

/// Two tabs' cached result views side by side, as split panes will embed them.
struct TwoResultPanes {
    app: gpui::Entity<AppRoot>,
    left: u64,
    right: u64,
}

impl Render for TwoResultPanes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (left, right) = (self.left, self.right);
        let (left, right) = self.app.update(cx, |app, app_cx| {
            (app.embed_tab_results(left, app_cx), app.embed_tab_results(right, app_cx))
        });
        div()
            .size_full()
            .flex()
            .child(div().w_1_2().h_full().flex().child(left))
            .child(div().w_1_2().h_full().flex().child(right))
    }
}

fn results_renders(tab_id: u64) -> u64 {
    results::surface::RENDERS.with(|renders| renders.borrow().get(&tab_id).copied().unwrap_or(0))
}

#[gpui::test]
fn notifying_one_tabs_results_does_not_rerender_the_other_tabs_grid(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let app_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = app_slot.clone();
    let (_root, visual) = cx.add_window_view(move |window, cx| {
        let app = cx.new(AppRoot::new_for_test);
        let (left, right) = app.update(cx, |this, _| {
            let left = this.workspace.open_ran_query_tab("left");
            this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
                crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(
                    wide_grid_batch(60, 20),
                )]),
            )));
            let right = this.workspace.open_ran_query_tab("right");
            this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
                crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(
                    wide_grid_batch(60, 20),
                )]),
            )));
            (left, right)
        });
        *slot.borrow_mut() = Some((app.clone(), left, right));
        let host = cx.new(|_| TwoResultPanes { app, left, right });
        gpui_component::Root::new(host, window, cx)
    });
    let (app, left, right) = app_slot.borrow_mut().take().unwrap();
    // `AppRoot::render` does this when it sits in the window's `Root`.
    visual.update(|window, cx| {
        let root = window.root::<gpui_component::Root>().flatten().map(|root| root.downgrade());
        app.update(cx, |this, _| this.window_root = root);
    });
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    settle(visual);
    // Both panes really draw their own grid.
    assert!(
        results::surface::RENDERS.with(|r| r.borrow().len()) >= 2,
        "both result views rendered"
    );
    // Quiesce: a few plain frames until neither view re-renders by itself.
    visual.run_until_parked();
    let (left_start, right_start) = (results_renders(left), results_renders(right));
    visual.run_until_parked();
    assert_eq!(
        (results_renders(left), results_renders(right)),
        (left_start, right_start),
        "an idle frame replays both cached views"
    );

    // Notifying the right tab re-renders only the right tab.
    app.update(visual, |this, cx| this.notify_tab(right, cx));
    visual.run_until_parked();
    assert_eq!(results_renders(left), left_start, "left grid not rebuilt");
    assert_eq!(results_renders(right), right_start + 1, "right grid rebuilt once");

    // And the other way round.
    app.update(visual, |this, cx| this.notify_tab(left, cx));
    visual.run_until_parked();
    assert_eq!(results_renders(left), left_start + 1);
    assert_eq!(results_renders(right), right_start + 1);

    // A chrome-only root change (fps counter, sidebar) rebuilds neither grid.
    app.update(visual, |this, cx| this.notify_chrome(cx));
    visual.run_until_parked();
    assert_eq!(results_renders(left), left_start + 1, "chrome change leaves left cached");
    assert_eq!(results_renders(right), right_start + 1, "chrome change leaves right cached");

    // Any other root notification stays safe: every results view refreshes.
    app.update(visual, |_, cx| cx.notify());
    visual.run_until_parked();
    assert_eq!(results_renders(left), left_start + 2);
    assert_eq!(results_renders(right), right_start + 2);
}

#[gpui::test]
fn the_shell_replays_the_active_tabs_cached_results_for_chrome_only_changes(
    cx: &mut TestAppContext,
) {
    let (app, visual, _a, b) = two_result_tabs(cx);
    settle(visual);
    visual.run_until_parked();
    let start = results_renders(b);
    assert!(start > 0, "the active tab's results were rendered");

    // fps counter / sidebar-only change: the shell redraws, the grid is replayed.
    app.update(visual, |this, cx| {
        this.shell.fps_display += 1;
        this.notify_chrome(cx);
    });
    visual.run_until_parked();
    assert_eq!(results_renders(b), start, "chrome change must not rebuild the grid");
    assert!(
        visual.debug_bounds("grid-scroll-area").is_some(),
        "the replayed grid is still painted"
    );

    // Tab-scoped and unscoped changes still refresh it.
    app.update(visual, |this, cx| this.notify_tab(b, cx));
    visual.run_until_parked();
    assert_eq!(results_renders(b), start + 1);
    app.update(visual, |_, cx| cx.notify());
    visual.run_until_parked();
    assert_eq!(results_renders(b), start + 2);
}
