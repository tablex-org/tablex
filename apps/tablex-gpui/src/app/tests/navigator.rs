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
    let at_depth = |app: &gpui::Entity<AppRoot>, depth: usize, cx: &mut TestAppContext| {
        app.update(cx, |this, _| {
            this.navigator
                .visible_rows()
                .into_iter()
                .filter(|row| row.depth == depth)
                .count()
        })
    };
    // A table lists its five folders at once; its columns load when the Columns folder opens.
    assert_eq!(at_depth(&app, 5, cx), 5);
    assert_eq!(at_depth(&app, 6, cx), 0);
    let folder = find_row(&app, "Columns", cx);
    app.update(cx, |this, cx| this.handle_navigator_disclosure(&folder, cx));
    cx.run_until_parked();
    assert_eq!(at_depth(&app, 6, cx), 8);

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

// The Postgres fixture's `orders` table: five folders at once, each filled from the table's
// description when it is opened and not before; a leaf opens the Properties at its sub-tab.
#[gpui::test]
fn a_fixture_table_shows_its_five_folders_each_loaded_when_opened(cx: &mut TestAppContext) {
    use crate::columns_panel::PropertiesTab;
    use crate::navigator::TableFolder;
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
        let app = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = cx.cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    let row_of = |app: &gpui::Entity<AppRoot>, id: &NavigatorNodeId, cx: &mut TestAppContext| {
        app.update(cx, |this, _| {
            this.navigator.visible_rows().into_iter().find(|row| &row.id == id).expect("a visible row")
        })
    };
    let open = |app: &gpui::Entity<AppRoot>, id: &NavigatorNodeId, cx: &mut TestAppContext| {
        let row = row_of(app, id, cx);
        app.update(cx, |this, cx| this.handle_navigator_disclosure(&row, cx));
        cx.run_until_parked();
    };
    let key = crate::ui_fixture::POSTGRES_ID;
    let table_id = NavigatorNodeId::Object {
        connection: key.into(),
        database: "warehouse".into(),
        schema: "public".into(),
        object: "orders".into(),
    };
    for id in [
        NavigatorNodeId::Connection { connection: key.into() },
        NavigatorNodeId::Database { connection: key.into(), database: "warehouse".into() },
        NavigatorNodeId::Schema { connection: key.into(), database: "warehouse".into(), schema: "public".into() },
        crate::navigator::object_group_id(key, "warehouse", "public", crate::navigator::ObjectGroup::Tables),
        table_id.clone(),
    ] {
        open(&app, &id, cx);
    }

    // Opening the table listed its folders and asked for nothing.
    let folders = app.update(cx, |this, _| this.navigator.loaded_children(&table_id).unwrap().to_vec());
    let titles: Vec<String> = app.update(cx, |this, _| {
        folders
            .iter()
            .map(|id| this.navigator.visible_rows().into_iter().find(|row| &row.id == id).unwrap().label)
            .collect()
    });
    assert_eq!(titles, ["Columns", "Indexes", "Constraints", "Foreign Keys", "Triggers"]);
    app.update(cx, |this, _| {
        for folder in &folders {
            assert!(this.navigator.load_state(folder).is_none(), "{folder:?} is not loaded yet");
        }
    });

    let expected: [(TableFolder, &[&str]); 4] = [
        (TableFolder::Indexes, &["orders_pkey", "orders_name_key", "orders_payload_idx"]),
        (TableFolder::Constraints, &["orders_pkey", "orders_name_key", "orders_score_check"]),
        (TableFolder::ForeignKeys, &["orders_customer_fk", "orders_region_fk"]),
        (TableFolder::Triggers, &["orders_audit", "orders_touch"]),
    ];
    for (folder, names) in expected {
        let id = table_id.table_folder(folder).unwrap();
        open(&app, &id, cx);
        let (labels, depths) = app.update(cx, |this, _| {
            let rows = this.navigator.visible_rows();
            let at = rows.iter().position(|row| row.id == id).unwrap();
            let leaves: Vec<_> = rows[at + 1..].iter().take_while(|row| row.depth > rows[at].depth).collect();
            (
                leaves.iter().map(|row| row.label.clone()).collect::<Vec<_>>(),
                leaves.iter().map(|row| row.depth - rows[at].depth).collect::<Vec<_>>(),
            )
        });
        assert_eq!(labels, names, "{folder:?}");
        assert!(depths.iter().all(|depth| *depth == 1));
    }
    app.update(cx, |this, _| {
        let detail_of = |folder: TableFolder, index: usize| {
            let id = table_id.table_folder(folder).unwrap();
            let child = this.navigator.loaded_children(&id).unwrap()[index].clone();
            this.navigator.table_item(&child).unwrap().detail.clone()
        };
        assert_eq!(detail_of(TableFolder::Indexes, 1), "(name) \u{00B7} unique");
        assert_eq!(detail_of(TableFolder::Constraints, 2), "Check (score)");
        assert_eq!(detail_of(TableFolder::ForeignKeys, 1), "\u{2192} public.regions(id, country)");
        assert_eq!(detail_of(TableFolder::Triggers, 1), "BEFORE UPDATE \u{00B7} disabled");
    });

    // A leaf: its bare name to copy, its definition, and Open shows the matching Properties sub-tab.
    let fk = app.update(cx, |this, _| {
        this.navigator.loaded_children(&table_id.table_folder(TableFolder::ForeignKeys).unwrap()).unwrap()[0].clone()
    });
    app.update(cx, |this, _| {
        assert_eq!(this.navigator_name_text(&fk, crate::app::sidebar_names::NameStyle::Plain), Ok(Some("orders_customer_fk".into())));
        assert!(this.navigator.supports_action(&fk, true, crate::navigator::NavigatorAction::Open));
    });
    let row = row_of(&app, &fk, cx);
    app.update(cx, |this, cx| this.handle_navigator_open(&row, cx));
    cx.run_until_parked();
    app.update(cx, |this, _| {
        let tab = this.workspace.active_tab().expect("the table opened").id;
        let panel = &this.workspace.runtime(tab).unwrap().columns_panel;
        assert_eq!(panel.properties_tab, PropertiesTab::ForeignKeys);
        assert_eq!(panel.presentation, crate::columns_panel::Presentation::Columns);
    });
    let trigger = app.update(cx, |this, _| {
        this.navigator.loaded_children(&table_id.table_folder(TableFolder::Triggers).unwrap()).unwrap()[0].clone()
    });
    app.update(cx, |this, _| {
        let item = this.navigator.table_item(&trigger).unwrap();
        assert!(item.definition.as_deref().is_some_and(|text| text.starts_with("CREATE TRIGGER orders_audit")));
    });
}

// An index leaf's name has priority over its right-edge detail: in a narrow sidebar the detail
// narrows (and may vanish) while the name keeps its width; in a wide one both are shown whole.
// Returns the widths of the name, the detail and the row of `orders_payload_idx` in a sidebar
// that is `sidebar_width` wide from its first frame (the resizable panel keeps its size after).
fn index_row_widths(cx: &mut TestAppContext, sidebar_width: f32) -> (f32, f32, f32) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionTree,
        fixture: FixtureKind::Sample,
        delay: Duration::ZERO,
        timeout_delay: Duration::ZERO,
    };
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| {
            let mut app = AppRoot::new_for_scenario(scenario, cx);
            app.shell.sidebar_width = sidebar_width;
            app
        });
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    for label in ["Postgres fixture", "warehouse", "public", "Tables", "orders", "Indexes"] {
        let row = app.update(visual, |this, _| {
            this.navigator.visible_rows().into_iter().find(|row| row.label == label).unwrap()
        });
        app.update(visual, |this, cx| this.handle_navigator_disclosure(&row, cx));
        visual.run_until_parked();
    }
    settle(visual);
    let index = app.read_with(visual, |this, _| {
        this.navigator.visible_rows().iter().position(|row| row.label == "orders_payload_idx").unwrap()
    });
    let width_of = |visual: &mut gpui::VisualTestContext, kind: &str| {
        let selector: &'static str = Box::leak(format!("sidebar-{kind}-{index}").into_boxed_str());
        f32::from(visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn")).size.width)
    };
    (width_of(visual, "label"), width_of(visual, "detail"), width_of(visual, "row"))
}

#[gpui::test]
fn an_index_row_keeps_its_name_when_the_sidebar_is_narrow(cx: &mut TestAppContext) {
    // Too narrow for the name beside its detail: the detail narrows, the name keeps its width.
    let (label, detail, _row) = index_row_widths(cx, 430.0);
    assert!(label >= 130.0, "the name (18 characters) keeps its full width: {label}");
    assert!(label > detail, "the name outweighs the detail: {label} vs {detail}");
    assert!(detail > 0.0 && detail < 70.0, "the detail gave way but stays: {detail}");
}

#[gpui::test]
fn an_index_row_gives_its_whole_width_to_the_name_in_the_default_sidebar(cx: &mut TestAppContext) {
    // Not even the name fits: it takes everything, the detail is gone.
    let (label, detail, _row) = index_row_widths(cx, 250.0);
    assert!(label >= 40.0, "the name has the room: {label}");
    assert!(detail < 1.0, "the detail yields everything: {detail}");
}

#[gpui::test]
fn an_index_row_shows_its_detail_when_the_sidebar_is_wide(cx: &mut TestAppContext) {
    let (label, detail, _row) = index_row_widths(cx, 700.0);
    assert!(label >= 130.0, "the name is whole: {label}");
    assert!(detail > 40.0, "the detail is shown: {detail}");
}
