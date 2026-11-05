#[gpui::test]
fn mongo_fixture_first_disclosure_connects_and_loads_provider_page(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let mut scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "connection-tree",
        "--ui-fixture",
        "sample",
    ])
    .unwrap()
    .unwrap();
    scenario.delay = std::time::Duration::ZERO;
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
    let disclosure = visual
        .debug_bounds("nav-disclosure-0")
        .expect("Mongo disclosure");
    visual.simulate_click(disclosure.center(), Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        assert!(
            rows[0].expanded,
            "first click must stay expanded after provider activation"
        );
        assert_eq!(
            rows.len(),
            8,
            "four databases plus Load More and three roots"
        );
    });
}

#[gpui::test]
fn scenario_tree_handler_loads_connection_to_columns_sequentially(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionTree,
        fixture: FixtureKind::Sample,
        delay: Duration::ZERO,
        timeout_delay: Duration::ZERO,
    };
    let (root, cx) = cx.add_window_view(|window, cx| {
        let app =
            cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = cx
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    let find_row = |app: &gpui::Entity<AppRoot>, label: &str, cx: &mut TestAppContext| {
        app.update(cx, |this, _| {
            this.navigator
                .visible_rows()
                .into_iter()
                .find(|row| row.label == label)
                .expect("fixture navigator row")
        })
    };
    let connection = find_row(&app, "Postgres fixture", cx);
    app.update(cx, |this, cx| {
        this.handle_navigator_disclosure(&connection, cx)
    });
    cx.run_until_parked();
    let database = find_row(&app, "warehouse", cx);
    app.update(cx, |this, cx| {
        this.handle_navigator_disclosure(&database, cx)
    });
    cx.run_until_parked();
    let schema = find_row(&app, "public", cx);
    app.update(cx, |this, cx| this.handle_navigator_disclosure(&schema, cx));
    let tables = find_row(&app, "Tables", cx);
    app.update(cx, |this, cx| this.handle_navigator_disclosure(&tables, cx));
    cx.run_until_parked();
    let object = find_row(&app, "orders", cx);
    app.update(cx, |this, cx| this.handle_navigator_disclosure(&object, cx));
    cx.run_until_parked();
    let columns = app.update(cx, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .filter(|row| row.depth == 5)
            .count()
    });
    assert_eq!(columns, 8);

    app.update(cx, |this, _| {
        let update = crate::staged_sql::UpdateCellRequest {
            database: "warehouse".into(),
            schema: "public".into(),
            table: "orders".into(),
            key_column: "id".into(),
            key_value: MutationValue::Number("1".into()),
            column: "name".into(),
            value: MutationValue::Text("public orders".into()),
        };
        this.scenario_ui
            .service
            .as_mut()
            .unwrap()
            .apply_sql_updates(
                crate::ui_fixture::POSTGRES_ID,
                "warehouse",
                "public",
                "orders",
                &[update],
            )
            .expect("fixture update");
    });
    app.update(cx, |this, cx| this.handle_navigator_open(&object, cx));
    let public_tab = app.update(cx, |this, _| {
        assert_eq!(this.workspace.tabs().len(), 2);
        this.workspace.active_tab().unwrap().id
    });
    assert_eq!(
        app.update(cx, |this, _| this
            .active_grid()
            .full_cell_text(crate::grid::CellPosition { row: 0, column: 1 })),
        Some("public orders".into())
    );

    let analytics = find_row(&app, "analytics", cx);
    app.update(cx, |this, cx| {
        this.handle_navigator_disclosure(&analytics, cx)
    });
    let analytics_tables = app.update(cx, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .find(|row| {
                row.label == "Tables"
                    && matches!(
                        &row.id,
                        NavigatorNodeId::ObjectGroup {
                            schema,
                            ..
                        } if schema == "analytics"
                    )
            })
            .expect("analytics tables row")
    });
    app.update(cx, |this, cx| {
        this.handle_navigator_disclosure(&analytics_tables, cx)
    });
    cx.run_until_parked();
    let analytics_object = app.update(cx, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .find(|row| {
                matches!(
                    &row.id,
                    NavigatorNodeId::Object {
                        schema,
                        object,
                        ..
                    } if schema == "analytics" && object == "orders"
                )
            })
            .expect("analytics orders row")
    });
    app.update(cx, |this, cx| {
        this.handle_navigator_open(&analytics_object, cx)
    });
    let analytics_tab = app.update(cx, |this, _| {
        assert_eq!(this.workspace.tabs().len(), 3);
        this.workspace.active_tab().unwrap().id
    });
    assert_ne!(public_tab, analytics_tab);
    assert_eq!(
        app.update(cx, |this, _| this
            .active_grid()
            .full_cell_text(crate::grid::CellPosition { row: 0, column: 1 })),
        Some("東京 café".into())
    );

    app.update(cx, |this, cx| this.handle_navigator_open(&object, cx));
    assert_eq!(
        app.update(cx, |this, _| this.workspace.active_tab().unwrap().id),
        public_tab
    );
    assert_eq!(
        app.update(cx, |this, _| this
            .active_grid()
            .full_cell_text(crate::grid::CellPosition { row: 0, column: 1 })),
        Some("public orders".into())
    );
}

fn nav_click_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
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
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    (app, visual)
}

/// Point on a sidebar row's label (right of the chevron slot).
fn nav_label_point(visual: &mut gpui::VisualTestContext, index: usize) -> gpui::Point<gpui::Pixels> {
    let selector: &'static str = Box::leak(format!("nav-disclosure-{index}").into_boxed_str());
    let chevron = visual.debug_bounds(selector).expect("row chevron slot");
    gpui::point(chevron.right() + gpui::px(70.0), chevron.center().y)
}

fn nav_chevron_point(visual: &mut gpui::VisualTestContext, index: usize) -> gpui::Point<gpui::Pixels> {
    let selector: &'static str = Box::leak(format!("nav-disclosure-{index}").into_boxed_str());
    visual.debug_bounds(selector).expect("row chevron slot").center()
}

fn nav_row_state(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, label: &str) -> (usize, bool) {
    app.read_with(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        let index = rows.iter().position(|row| row.label == label).expect("row");
        (index, rows[index].expanded)
    })
}

#[gpui::test]
fn single_click_on_a_connection_label_toggles_it(cx: &mut TestAppContext) {
    let (app, visual) = nav_click_app(cx);
    let has_row = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, label: &str| {
        app.read_with(visual, |this, _| this.navigator.visible_rows().iter().any(|row| row.label == label))
    };
    let (index, expanded) = nav_row_state(&app, visual, "Postgres fixture");
    assert!(!expanded);
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());

    // Single click on the label: selects, connects and expands.
    let point = nav_label_point(visual, index);
    visual.simulate_click(point, Default::default());
    visual.run_until_parked();
    settle(visual);
    assert!(nav_row_state(&app, visual, "Postgres fixture").1, "expanded by a label click");
    assert!(has_row(&app, visual, "warehouse"), "children loaded through the connect path");
    assert!(
        app.read_with(visual, |this, _| matches!(
            this.navigator.selected(),
            Some(NavigatorNodeId::Connection { connection }) if connection == crate::ui_fixture::POSTGRES_ID
        )),
        "the clicked connection is selected"
    );
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), tabs, "no tab opened");

    // Second single click collapses.
    let point = nav_label_point(visual, index);
    visual.simulate_click(point, Default::default());
    settle(visual);
    assert!(!nav_row_state(&app, visual, "Postgres fixture").1);
    assert!(!has_row(&app, visual, "warehouse"));

    // Chevron toggles exactly once (the row handler must not fire as well).
    let point = nav_chevron_point(visual, index);
    visual.simulate_click(point, Default::default());
    visual.run_until_parked();
    settle(visual);
    assert!(nav_row_state(&app, visual, "Postgres fixture").1, "chevron expands once");
    let point = nav_chevron_point(visual, index);
    visual.simulate_click(point, Default::default());
    settle(visual);
    assert!(!nav_row_state(&app, visual, "Postgres fixture").1, "chevron collapses once");

    // Shift / Cmd (Ctrl off macOS) click keep multi-select semantics: no toggle.
    let secondary = visual.update(|_, cx| gpui::Modifiers::secondary_key(cx));
    for modifiers in [gpui::Modifiers { shift: true, ..Default::default() }, secondary] {
        let point = nav_label_point(visual, index);
    visual.simulate_click(point, modifiers);
        settle(visual);
        assert!(!nav_row_state(&app, visual, "Postgres fixture").1, "{modifiers:?} does not toggle");
    }
}

#[gpui::test]
fn double_click_on_a_connection_toggles_once_net(cx: &mut TestAppContext) {
    let (app, visual) = nav_click_app(cx);
    let (index, _) = nav_row_state(&app, visual, "Postgres fixture");
    let point = nav_label_point(visual, index);
    // A real double click: press/release with click_count 1, then 2.
    for count in [1usize, 2] {
        visual.simulate_event(gpui::MouseDownEvent {
            position: point,
            modifiers: Default::default(),
            button: gpui::MouseButton::Left,
            click_count: count,
            first_mouse: false,
        });
        visual.simulate_event(gpui::MouseUpEvent {
            position: point,
            modifiers: Default::default(),
            button: gpui::MouseButton::Left,
            click_count: count,
        });
        visual.run_until_parked();
    }
    settle(visual);
    assert!(nav_row_state(&app, visual, "Postgres fixture").1, "toggled once, not twice");
}

#[gpui::test]
fn single_click_on_a_table_selects_without_opening_or_toggling(cx: &mut TestAppContext) {
    let (app, visual) = nav_click_app(cx);
    for label in ["Postgres fixture", "warehouse", "public", "Tables"] {
        let row = app.update(visual, |this, _| {
            this.navigator.visible_rows().into_iter().find(|row| row.label == label).unwrap()
        });
        app.update(visual, |this, cx| this.handle_navigator_disclosure(&row, cx));
        visual.run_until_parked();
    }
    settle(visual);
    let (index, expanded) = nav_row_state(&app, visual, "orders");
    assert!(!expanded);
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let point = nav_label_point(visual, index);
    visual.simulate_click(point, Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs, "no data tab from a single click");
        assert!(matches!(this.navigator.selected(), Some(NavigatorNodeId::Object { object, .. }) if object == "orders"));
    });
    assert!(!nav_row_state(&app, visual, "orders").1, "single click does not expand the table");
}
