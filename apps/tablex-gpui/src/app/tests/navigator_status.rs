// Navigator rows: the status dot on the brand icon, and the failure triangle with its tooltip and
// popover. Real clicks, hovers and keys on the production theme.

const NS_ERROR: &str = "connection to server at \"db.internal\" (10.0.0.5), port 5432 failed: \
Connection refused. Is the server running on that host and accepting TCP/IP connections? \
Check that the firewall lets the client through and that the listen address is right";

/// The sidebar scenario with a long connect delay, so a started connect stays "Connecting".
fn ns_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionTree,
        fixture: FixtureKind::Sample,
        delay: Duration::from_secs(60),
        timeout_delay: Duration::from_secs(60),
    };
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    settle(visual);
    (app, visual)
}

fn ns_index(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, label: &str) -> usize {
    app.update(visual, |this, _| {
        this.navigator
            .visible_rows()
            .iter()
            .position(|row| row.label == label)
            .expect("the fixture row")
    })
}

const NS_POSTGRES: &str = "Postgres fixture";

fn ns_status(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> ConnectionStatus {
    app.update(visual, |this, _| this.sessions.status(crate::ui_fixture::POSTGRES_ID))
}

fn ns_set(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, status: ConnectionStatus) {
    app.update(visual, |this, cx| {
        let id = crate::ui_fixture::POSTGRES_ID;
        let attempt = this.sessions.begin_connect(id).expect("a fresh connection starts");
        match status {
            ConnectionStatus::Connecting => {}
            ConnectionStatus::Connected => assert!(this.sessions.finish_connect(id, attempt, Ok(()))),
            ConnectionStatus::Failed(error) => assert!(this.sessions.finish_connect(id, attempt, Err(error))),
            ConnectionStatus::Disconnected => unreachable!("nothing to start"),
        }
        this.sync_sidebar_connections(cx);
    });
    settle(visual);
}

fn ns_has(visual: &mut gpui::VisualTestContext, selector: String) -> bool {
    visual.debug_bounds(d3_sel(selector)).is_some()
}

fn ns_click(visual: &mut gpui::VisualTestContext, selector: String) {
    let bounds = visual.debug_bounds(d3_sel(selector.clone())).unwrap_or_else(|| panic!("{selector} is drawn"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

/// The selection and expansion a stray row click would change.
fn ns_tree(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> (Vec<NavigatorNodeId>, Vec<(String, bool)>) {
    app.update(visual, |this, _| {
        (
            this.navigator.selected_ids().iter().cloned().collect(),
            this.navigator.visible_rows().iter().map(|row| (row.label.clone(), row.expanded)).collect(),
        )
    })
}

#[gpui::test]
fn a_connected_row_has_the_dot_on_its_icon_and_none_at_the_right_edge(cx: &mut TestAppContext) {
    let (app, visual) = ns_app(cx);
    let index = ns_index(&app, visual, NS_POSTGRES);
    assert!(!ns_has(visual, format!("sidebar-status-dot-connected-{index}")), "not connected yet");
    ns_set(&app, visual, ConnectionStatus::Connected);

    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{index}"))).expect("row");
    let dot = visual.debug_bounds(d3_sel(format!("sidebar-status-dot-connected-{index}")))
        .expect("the connected dot is drawn over the icon");
    assert!(!ns_has(visual, format!("sidebar-status-dot-connecting-{index}")));
    assert!(!ns_has(visual, format!("sidebar-problem-{index}")), "connected is no failure");
    // The icon starts after the 8px inset and the 16px disclosure: the dot is on it, nowhere near the right edge.
    assert!(dot.left() > row.left() + gpui::px(8.0 + 16.0) && dot.right() < row.left() + gpui::px(8.0 + 16.0 + 24.0), "{dot:?} in {row:?}");
    assert!(dot.size.width >= gpui::px(6.0) && dot.size.width <= gpui::px(10.0), "{dot:?}");
    assert!(row.right() - dot.right() > gpui::px(100.0), "no dot at the right edge: {dot:?} in {row:?}");
}

#[gpui::test]
fn a_disconnected_row_has_no_dot_and_a_connecting_row_has_the_warning_one(cx: &mut TestAppContext) {
    let (app, visual) = ns_app(cx);
    let index = ns_index(&app, visual, NS_POSTGRES);
    for kind in ["connected", "connecting"] {
        assert!(!ns_has(visual, format!("sidebar-status-dot-{kind}-{index}")), "disconnected shows no dot");
    }
    assert!(!ns_has(visual, format!("sidebar-problem-{index}")));

    ns_set(&app, visual, ConnectionStatus::Connecting);
    assert!(ns_has(visual, format!("sidebar-status-dot-connecting-{index}")), "connecting");
    assert!(!ns_has(visual, format!("sidebar-status-dot-connected-{index}")));
    assert!(!ns_has(visual, format!("sidebar-problem-{index}")), "connecting is no failure");

    // Gone again once the connection is released.
    app.update(visual, |this, cx| {
        this.sessions.disconnect(crate::ui_fixture::POSTGRES_ID);
        cx.notify();
    });
    settle(visual);
    assert!(!ns_has(visual, format!("sidebar-status-dot-connecting-{index}")));
}

#[gpui::test]
fn a_failed_connection_shows_the_triangle_with_the_first_line_in_its_tooltip(cx: &mut TestAppContext) {
    let (app, visual) = ns_app(cx);
    let index = ns_index(&app, visual, NS_POSTGRES);
    ns_set(&app, visual, ConnectionStatus::Failed(format!("{NS_ERROR}\nsecond line")));
    assert!(ns_has(visual, format!("sidebar-problem-{index}")), "the triangle");
    for kind in ["connected", "connecting"] {
        assert!(!ns_has(visual, format!("sidebar-status-dot-{kind}-{index}")), "a failure has no dot");
    }
    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{index}"))).unwrap();
    let triangle = visual.debug_bounds(d3_sel(format!("sidebar-problem-{index}"))).unwrap();
    assert!(row.right() - triangle.right() < gpui::px(16.0), "at the right edge: {triangle:?} in {row:?}");

    let summary = super::navigator_status::problem_summary(NS_ERROR);
    assert!(summary.ends_with('…') && !summary.contains('\n'), "{summary}");
    let tooltip = format!("sidebar-problem-tooltip {summary}");
    assert!(!ns_has(visual, tooltip.clone()));
    visual.simulate_mouse_move(gpui::point(gpui::px(2.0), gpui::px(2.0)), None, Default::default());
    settle(visual);
    visual.simulate_mouse_move(triangle.center(), None, Default::default());
    visual.executor().advance_clock(Duration::from_millis(700));
    settle(visual);
    assert!(ns_has(visual, tooltip.clone()), "hovering shows the first line, cut");
    visual.simulate_mouse_move(gpui::point(gpui::px(2.0), gpui::px(2.0)), None, Default::default());
    settle(visual);
    assert!(!ns_has(visual, tooltip), "and it goes with the pointer");
}

#[gpui::test]
fn the_popover_holds_the_whole_error_and_closes_on_escape_and_outside_clicks(cx: &mut TestAppContext) {
    let (app, visual) = ns_app(cx);
    let index = ns_index(&app, visual, NS_POSTGRES);
    let message = format!("{NS_ERROR}\nsecond line");
    ns_set(&app, visual, ConnectionStatus::Failed(message.clone()));
    let before = ns_tree(&app, visual);

    ns_click(visual, format!("sidebar-problem-{index}"));
    let panel = visual.debug_bounds(d3_sel(format!("sidebar-problem-popover-{index}"))).expect("the click opens the popover");
    let text = visual.debug_bounds(d3_sel(format!("sidebar-problem-text-{index}"))).expect("the error text");
    assert!(text.size.height > gpui::px(36.0) && text.size.height <= gpui::px(140.0), "wrapped, capped: {text:?}");
    assert!(panel.size.width >= gpui::px(300.0), "{panel:?}");
    assert_eq!(ns_tree(&app, visual), before, "the click neither selected nor toggled the row");
    assert_eq!(ns_status(&app, visual), ConnectionStatus::Failed(message.clone()));

    // Copy puts the whole text on the clipboard and keeps the popover.
    ns_click(visual, format!("sidebar-problem-copy-{index}"));
    let copied = visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some(message.as_str()));
    assert!(ns_has(visual, format!("sidebar-problem-popover-{index}")));
    assert_eq!(ns_tree(&app, visual), before, "Copy did not reach the row either");

    // Escape closes it.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!ns_has(visual, format!("sidebar-problem-popover-{index}")), "Escape");
    assert!(app.update(visual, |this, _| this.navigator_ui.problem_open.is_none()));

    // So does a click outside, which is then the click's only effect on the popover.
    ns_click(visual, format!("sidebar-problem-{index}"));
    assert!(ns_has(visual, format!("sidebar-problem-popover-{index}")), "reopened");
    visual.simulate_click(gpui::point(gpui::px(900.0), gpui::px(500.0)), Default::default());
    settle(visual);
    assert!(!ns_has(visual, format!("sidebar-problem-popover-{index}")), "outside click");
    // And the triangle opens it again.
    ns_click(visual, format!("sidebar-problem-{index}"));
    assert!(ns_has(visual, format!("sidebar-problem-popover-{index}")), "reopened after dismissal");

    // The probe is live: a click on the label itself does select the row.
    visual.simulate_keystrokes("escape");
    settle(visual);
    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{index}"))).unwrap();
    visual.simulate_click(gpui::point(row.left() + gpui::px(80.0), row.center().y), Default::default());
    settle(visual);
    assert_ne!(ns_tree(&app, visual).0, before.0, "a row click selects");
}

#[gpui::test]
fn retry_reconnects_and_clear_goes_back_to_disconnected(cx: &mut TestAppContext) {
    let (app, visual) = ns_app(cx);
    let index = ns_index(&app, visual, NS_POSTGRES);
    ns_set(&app, visual, ConnectionStatus::Failed("Connection refused".into()));
    let before = ns_tree(&app, visual);

    ns_click(visual, format!("sidebar-problem-{index}"));
    ns_click(visual, format!("sidebar-problem-retry-{index}"));
    assert_eq!(ns_status(&app, visual), ConnectionStatus::Connecting, "Retry starts a connect attempt");
    assert!(!ns_has(visual, format!("sidebar-problem-{index}")), "the triangle goes while it connects");
    assert!(!ns_has(visual, format!("sidebar-problem-popover-{index}")), "and the popover");
    assert!(ns_has(visual, format!("sidebar-status-dot-connecting-{index}")), "the warning dot takes over");
    assert_eq!(ns_tree(&app, visual).0, before.0, "Retry did not change the selection");

    // Clear: a lost connection forgets its reason.
    app.update(visual, |this, cx| {
        let id = crate::ui_fixture::POSTGRES_ID;
        this.sessions.disconnect(id);
        let attempt = this.sessions.begin_connect(id).unwrap();
        this.sessions.finish_connect(id, attempt, Err("lost".into()));
        cx.notify();
    });
    settle(visual);
    ns_click(visual, format!("sidebar-problem-{index}"));
    ns_click(visual, format!("sidebar-problem-clear-{index}"));
    assert_eq!(ns_status(&app, visual), ConnectionStatus::Disconnected);
    assert!(!ns_has(visual, format!("sidebar-problem-{index}")), "the triangle is gone");
    assert!(!ns_has(visual, format!("sidebar-problem-popover-{index}")));
    assert!(app.update(visual, |this, _| this.navigator_ui.problem_open.is_none()));
    assert_eq!(ns_tree(&app, visual).0, before.0, "Clear did not change the selection");
}

#[gpui::test]
fn a_node_whose_load_failed_has_the_triangle_and_retry_reloads_it(cx: &mut TestAppContext) {
    let (app, visual) = ns_app(cx);
    let warehouse = NavigatorNodeId::Database {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
    };
    let fail = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.update(visual, |this, cx| {
            this.navigator.reset_node(&warehouse);
            let request = this.navigator.toggle_expansion(&warehouse).request.expect("the load");
            let generation = this.navigator.load_generation(&request).unwrap();
            this.navigator.apply_load(&request, generation, Err("permission denied for database warehouse".into()));
            this.navigator_ui.generation = this.navigator.generation();
            cx.notify();
        });
        settle(visual);
    };
    app.update(visual, |this, _| this.seed_connection_tree_scenario());
    fail(&app, visual);
    let index = ns_index(&app, visual, "warehouse");
    assert!(ns_has(visual, format!("sidebar-problem-{index}")), "the same triangle on a child node");
    for other in ["Mongo fixture", "SQLite fixture"] {
        let other = ns_index(&app, visual, other);
        assert!(!ns_has(visual, format!("sidebar-problem-{other}")), "only the failed node");
    }
    let selected = ns_tree(&app, visual).0;

    ns_click(visual, format!("sidebar-problem-{index}"));
    assert!(ns_has(visual, format!("sidebar-problem-popover-{index}")));
    ns_click(visual, format!("sidebar-problem-retry-{index}"));
    let state = app.update(visual, |this, _| this.navigator.load_state(&warehouse).cloned());
    assert_eq!(state, Some(crate::navigator::NodeLoadState::Loading), "Retry reloads the node");
    assert!(!ns_has(visual, format!("sidebar-problem-{index}")), "the triangle goes while it loads");
    assert_eq!(ns_tree(&app, visual).0, selected, "and the selection did not move");

    // Clear: back to unloaded and collapsed.
    fail(&app, visual);
    let index = ns_index(&app, visual, "warehouse");
    ns_click(visual, format!("sidebar-problem-{index}"));
    ns_click(visual, format!("sidebar-problem-clear-{index}"));
    let (state, expanded) = app.update(visual, |this, _| {
        (
            this.navigator.load_state(&warehouse).cloned(),
            this.navigator.visible_rows().iter().any(|row| row.id == warehouse && row.expanded),
        )
    });
    assert_eq!(state, None);
    assert!(!expanded);
    assert!(!ns_has(visual, format!("sidebar-problem-{index}")));
}

#[test]
fn a_dot_ring_is_the_row_fill_over_the_sidebar() {
    use super::navigator_status::over_sidebar;
    let sidebar = theme::sidebar();
    assert_eq!(over_sidebar(gpui::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }), sidebar, "a plain row");
    let solid = theme::selected();
    assert_eq!(over_sidebar(solid), gpui::Rgba { a: 1.0, ..solid }, "a selected row");
    let tinted = over_sidebar(super::navigator_ui::tinted(gpui::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }, 0.5));
    assert_eq!(tinted.a, 1.0);
    assert!((tinted.r - (sidebar.r + 1.0) / 2.0).abs() < 1e-6 && (tinted.g - sidebar.g / 2.0).abs() < 1e-6);
}
