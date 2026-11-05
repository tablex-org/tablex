#[gpui::test]
fn auto_refresh_custom_menu_validates_without_executing_query(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let app = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(app, window, cx)
    });
    let app = cx
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    let tab = app.update(cx, |this, cx| {
        let id = this.workspace.open_query_tab("Refresh test");
        let tab = this.workspace.active_tab_mut().unwrap();
        tab.query_mode = Some(QueryMode::MongoFind);
        tab.connection_id = "disconnected-test".into();
        cx.notify();
        id
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let before = app.update(cx, |this, _| {
        this.workspace.runtime(tab).unwrap().query.clone()
    });
    let trigger = cx
        .debug_bounds("result-refresh-dropdown")
        .expect("refresh split button");
    cx.simulate_click(
        gpui::point(trigger.right() - px(3.0), trigger.center().y),
        Default::default(),
    );
    cx.run_until_parked();
    // "Customize..." is the menu's first item (then "Stop", then the presets).
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let input = cx
        .debug_bounds("auto-refresh-seconds")
        .expect("Customize opens seconds input");
    cx.simulate_click(input.center(), Default::default());
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_keystrokes("0");
    let apply = cx
        .debug_bounds("auto-refresh-custom-apply")
        .expect("Apply button");
    cx.simulate_click(apply.center(), Default::default());
    cx.run_until_parked();
    app.update(cx, |this, _| {
        assert_eq!(this.workspace.auto_refresh_seconds(tab), None);
        assert_eq!(this.workspace.runtime(tab).unwrap().query, before);
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let input = cx
        .debug_bounds("auto-refresh-seconds")
        .expect("custom dialog remains after invalid input");
    cx.simulate_click(input.center(), Default::default());
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_keystrokes("12");
    let apply = cx.debug_bounds("auto-refresh-custom-apply").unwrap();
    cx.simulate_click(apply.center(), Default::default());
    cx.run_until_parked();
    app.update(cx, |this, _| {
        assert_eq!(this.workspace.auto_refresh_seconds(tab), Some(12));
        assert!(this.auto_refresh.tasks.contains_key(&tab));
        assert_eq!(this.workspace.runtime(tab).unwrap().query, before);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("auto-refresh-seconds").is_none());
    let trigger = cx.debug_bounds("result-refresh-dropdown").unwrap();
    cx.simulate_click(
        gpui::point(trigger.right() - px(3.0), trigger.center().y),
        Default::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    app.update(cx, |this, _| {
        assert_eq!(this.workspace.auto_refresh_seconds(tab), None);
        assert!(!this.auto_refresh.tasks.contains_key(&tab));
        assert_eq!(this.workspace.runtime(tab).unwrap().query, before);
    });
}

#[gpui::test]
fn auto_refresh_timer_never_submits_sql_and_is_cancelled_on_close(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let app = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(app, window, cx)
    });
    let app = cx
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    let tab = app.update(cx, |this, cx| {
        let tab = this.workspace.open_query_tab("SQL timer safety");
        this.workspace.active_tab_mut().unwrap().sql =
            "UPDATE counters SET value = value + 1; DROP TABLE secrets".into();
        this.set_auto_refresh_interval(tab, Some(1), cx);
        tab
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();
    app.update(cx, |this, cx| {
        assert_eq!(
            this.workspace.runtime(tab).unwrap().query,
            crate::query::QueryState::default()
        );
        assert!(!this.query_tasks.tasks.contains_key(&tab));
        this.close_tab(tab, cx);
        assert!(!this.auto_refresh.tasks.contains_key(&tab));
    });
}

fn grid_scenario_app<'a>(
    cx: &'a mut TestAppContext,
    scenario: &str,
) -> (gpui::Entity<AppRoot>, &'a mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        scenario,
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
    (app, visual)
}

#[gpui::test]
fn sql_results_footer_keeps_the_row_count_and_has_no_auto_refresh_label(cx: &mut TestAppContext) {
    let (app, visual) = grid_scenario_app(cx, "result-grid");
    assert!(
        visual.debug_bounds("result-status-summary").is_some(),
        "the results footer keeps the row count"
    );
    assert!(
        visual.debug_bounds("result-status-auto").is_none(),
        "auto-refresh is unavailable for SQL tabs: no label at all"
    );
    assert!(visual.debug_bounds("status-bar").is_some());
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        let query = &this.workspace.runtime(tab).unwrap().query;
        assert_eq!(query.status_name(), "Ready", "the scenario holds a loaded result");
        let text = query.status_text();
        assert!(!text.contains("rows"), "app status bar repeats the footer count: {text}");
    });
}

#[gpui::test]
fn mongo_results_footer_shows_a_compact_auto_refresh_control_that_opens_the_interval_menu(
    cx: &mut TestAppContext,
) {
    let (app, visual) = grid_scenario_app(cx, "mongo-grid");
    let control = visual
        .debug_bounds("result-status-auto")
        .expect("auto-refresh control for a Mongo tab");
    let footer = visual.debug_bounds("result-status-row").unwrap();
    assert!(
        control.size.height <= footer.size.height,
        "the control stays compact inside the status row"
    );
    assert!(visual.debug_bounds("result-status-summary").is_some());
    visual.simulate_click(control.center(), Default::default());
    visual.run_until_parked();
    // Customize, Stop (disabled while Off), then the presets.
    visual.simulate_keystrokes("down down enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        assert_eq!(
            this.workspace.auto_refresh_seconds(tab),
            Some(crate::auto_refresh::PRESET_INTERVALS[0]),
            "choosing a preset from the control's menu starts auto-refresh"
        );
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(visual.debug_bounds("result-status-auto").is_some());
}
