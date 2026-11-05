use super::results::context_menu::GenerateKind;
use super::results::copy::{CopyFormat, CopyScope};
use super::results::mutations::CloneSqlRow;

#[gpui::test]
fn resized_column_survives_a_real_refresh_click(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "mongo-grid",
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
    let header = visual.debug_bounds("grid-header-cell-1").unwrap();
    let start = gpui::point(header.right() - px(2.0), header.center().y);
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: start,
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    for delta in [40.0, 80.0] {
        visual.simulate_event(gpui::MouseMoveEvent {
            position: start + gpui::point(px(delta), px(0.0)),
            pressed_button: Some(MouseButton::Left),
            modifiers: Default::default(),
        });
        visual.run_until_parked();
    }
    visual.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: start + gpui::point(px(80.0), px(0.0)),
        modifiers: Default::default(),
        click_count: 1,
    });
    visual.run_until_parked();
    let width = app.update(visual, |this, _| this.av().grid_ui.layout.widths[1]);
    assert!(
        width > f32::from(header.size.width),
        "drag must change the width first"
    );
    let refresh = visual.debug_bounds("result-refresh").unwrap();
    visual.simulate_click(refresh.center(), Default::default());
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.layout.widths[1], width,
            "same-schema refresh must retain user width"
        )
    });
}

#[gpui::test]
fn scenario_empty_loading_error_keep_result_area_and_footer(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    for fixture in ["empty", "loading", "error"] {
        let scenario = UiScenario::from_args([
            "tablex-gpui",
            "--ui-scenario",
            "result-grid",
            "--ui-fixture",
            fixture,
        ])
        .unwrap()
        .unwrap();
        let (root, visual) = cx.add_window_view(|window, cx| {
            let app =
                cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
            gpui_component::Root::new(app, window, cx)
        });
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        let area = visual
            .debug_bounds("result-empty-state")
            .expect("state-specific result area");
        let footer = visual.debug_bounds("result-action-strip").expect("footer");
        assert!(
            area.size.height > px(200.0),
            "{fixture}: empty area must fill available height"
        );
        assert!(
            footer.origin.y >= area.bottom(),
            "{fixture}: footer belongs below result area"
        );
        if fixture == "empty" {
            assert!(
                visual.debug_bounds("grid-header-cell-0").is_some(),
                "zero rows must retain column headers"
            );
        }
        if fixture == "loading" {
            let app = visual
                .cx
                .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
            let cancel = visual
                .debug_bounds("run-query")
                .expect("Cancel query action");
            visual.simulate_click(cancel.center(), Default::default());
            visual.run_until_parked();
            app.update(visual, |this, _| {
                let tab = this.workspace.active_tab().unwrap();
                assert_eq!(
                    this.workspace.runtime(tab.id).unwrap().query.status_name(),
                    "Cancelled"
                );
            });
        }
        let _ = root;
    }
}

#[test]
fn failed_query_does_not_release_the_displayed_page() {
    assert_eq!(superseded_driver_page(Some(7), None), None);
    assert_eq!(superseded_driver_page(Some(7), Some(8)), Some(7));
    assert_eq!(superseded_driver_page(Some(7), Some(7)), None);
    assert_eq!(superseded_driver_page(None, Some(8)), None);
}

#[gpui::test]
fn scenario_refresh_increments_query_generation_without_changing_page(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ResultGrid,
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
    let before = app.update(cx, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let runtime = this.workspace.runtime(tab_id).unwrap();
        (
            runtime.pagination.offset(),
            runtime.query.generation(),
            this.active_grid()
                .full_cell_text(crate::grid::CellPosition { row: 0, column: 0 }),
        )
    });
    app.update(cx, |this, cx| this.run_scenario_query(this.active_id(), cx));
    let after = app.update(cx, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let runtime = this.workspace.runtime(tab_id).unwrap();
        (
            runtime.pagination.offset(),
            runtime.query.generation(),
            this.active_grid()
                .full_cell_text(crate::grid::CellPosition { row: 0, column: 0 }),
        )
    });
    assert_eq!(after.0, before.0);
    assert_eq!(after.2, before.2);
    assert!(after.1 > before.1);
}

#[test]
fn row_json_keeps_native_scalar_types_and_nulls() {
    assert_eq!(
        row_value_to_json("12", "int64", false),
        serde_json::json!(12)
    );
    assert_eq!(
        row_value_to_json("true", "boolean", false),
        serde_json::json!(true)
    );
    assert_eq!(
        row_value_to_json("NULL", "text", true),
        serde_json::Value::Null
    );
    assert_eq!(
        row_value_to_json(r#"{"name":"text"}"#, "text", false),
        serde_json::json!(r#"{"name":"text"}"#)
    );
    assert_eq!(
        row_value_to_json(r#"{"name":"json"}"#, "jsonb", false),
        serde_json::json!({"name": "json"})
    );
}

#[gpui::test]
fn grid_cells_share_header_widths_with_long_values(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });
    let long_value = "x".repeat(512);
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("_id", arrow_schema::DataType::Utf8, false),
        arrow_schema::Field::new("transId", arrow_schema::DataType::Utf8, false),
        arrow_schema::Field::new("sender", arrow_schema::DataType::Utf8, false),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::StringArray::from(vec![Some(
                long_value.as_str(),
            )])) as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::StringArray::from(vec![Some("short")]))
                as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::StringArray::from(vec![Some(
                long_value.as_str(),
            )])) as arrow_array::ArrayRef,
        ],
    )
    .expect("valid batch");

    app.update(cx, |this, cx| {
        this.workspace.open_ran_query_tab("test");
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
    });
    cx.simulate_resize(gpui::size(gpui::px(1000.0), gpui::px(600.0)));
    cx.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    cx.run_until_parked();

    for column in 0..3 {
        let header_selector = match column {
            0 => "grid-header-cell-0",
            1 => "grid-header-cell-1",
            2 => "grid-header-cell-2",
            _ => unreachable!(),
        };
        let body_selector = match column {
            0 => "grid-body-cell-0-0",
            1 => "grid-body-cell-0-1",
            2 => "grid-body-cell-0-2",
            _ => unreachable!(),
        };
        let header = cx
            .debug_bounds(header_selector)
            .expect("header cell should be painted");
        let body = cx
            .debug_bounds(body_selector)
            .expect("body cell should be painted");
        assert_eq!(
            header.origin.x, body.origin.x,
            "body column {column} should start at the header column"
        );
        assert_eq!(
            header.size.width, body.size.width,
            "body column {column} should match the header width"
        );
    }
}

#[gpui::test]
fn horizontal_wheel_scrolls_the_grid(cx: &mut TestAppContext) {
    use gpui_component::scroll::ScrollbarHandle as _;
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = cx.cx.read(|app| {
        root.read(app)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });

    // 12 columns × 160px + 48px gutter = 1968px > 800px viewport → horizontal overflow.
    // 50 rows × ~26px > 248px grid area → vertical overflow.
    let row_count = 50;
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(
        (0..12)
            .map(|i| {
                arrow_schema::Field::new(format!("col{i}"), arrow_schema::DataType::Utf8, true)
            })
            .collect::<Vec<_>>(),
    ));
    let arrays: Vec<arrow_array::ArrayRef> = (0..12)
        .map(|_| {
            std::sync::Arc::new(arrow_array::StringArray::from_iter(
                std::iter::repeat_n(Some("x"), row_count),
            )) as arrow_array::ArrayRef
        })
        .collect();
    let batch = arrow_array::RecordBatch::try_new(schema, arrays).expect("valid batch");
    cx.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(600.0)));
    app.update(cx, |this, cx| {
        this.workspace.open_ran_query_tab("test");
        this.reset_active_view();
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let grid_bounds = cx
        .debug_bounds("grid-scroll-area")
        .expect("grid-scroll-area should be painted");
    let max_offset = app.update(cx, |this, _cx| this.av().grid_horizontal_scroll.max_offset());
    assert!(
        f32::from(max_offset.x) > 0.0,
        "grid should have horizontal overflow (content wider than viewport)"
    );

    let center = grid_bounds.center();

    // --- Horizontal wheel: scroll ngang, KHÔNG scroll dọc ---
    cx.simulate_event(ScrollEvent {
        position: center,
        delta: ScrollDelta::Pixels(Point::new(gpui::px(-100.0), gpui::px(0.0))),
        touch_phase: gpui::TouchPhase::Moved,
        modifiers: Default::default(),
        source: gpui::ScrollSource::Pointer,
    });
    cx.run_until_parked();

    let (h_offset_x, rows_y) = app.update(cx, |this, _cx| {
        (
            this.av().grid_horizontal_scroll.offset().x,
            this.av().grid_scroll.offset().y,
        )
    });
    assert!(
        f32::from(h_offset_x) < 0.0,
        "horizontal wheel should move the grid left"
    );
    assert!(
        f32::from(rows_y) == 0.0,
        "horizontal wheel should NOT scroll rows vertically"
    );

    // --- Vertical wheel: scroll dọc, KHÔNG scroll ngang ---
    cx.simulate_event(ScrollEvent {
        position: center,
        delta: ScrollDelta::Pixels(Point::new(gpui::px(0.0), gpui::px(-100.0))),
        touch_phase: gpui::TouchPhase::Moved,
        modifiers: Default::default(),
        source: gpui::ScrollSource::Pointer,
    });
    cx.run_until_parked();

    let (v_offset_x, rows_y2) = app.update(cx, |this, _cx| {
        (
            this.av().grid_horizontal_scroll.offset().x,
            this.av().grid_scroll.offset().y,
        )
    });
    assert_eq!(
        f32::from(v_offset_x),
        f32::from(h_offset_x),
        "vertical wheel should NOT scroll horizontally"
    );
    assert!(
        f32::from(rows_y2) < 0.0,
        "vertical wheel should scroll rows down"
    );
}

#[gpui::test]
fn grid_arrow_keys_move_the_selected_cell_after_a_click(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "result-grid",
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
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(
        (0..12)
            .map(|column| {
                arrow_schema::Field::new(format!("col{column}"), arrow_schema::DataType::Utf8, true)
            })
            .collect::<Vec<_>>(),
    ));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        (0..12)
            .map(|_| {
                std::sync::Arc::new(arrow_array::StringArray::from(vec![Some("x"); 50]))
                    as arrow_array::ArrayRef
            })
            .collect(),
    )
    .expect("valid result batch");
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        this.av_mut().grid_ui.visible_columns.clear();
        this.av_mut().grid_ui.layout = crate::grid::ColumnLayout::new(this.active_grid().column_count());
        cx.notify();
    });
    visual.simulate_resize(gpui::size(px(800.0), px(600.0)));
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    visual.run_until_parked();

    let first_cell = visual.debug_bounds("grid-body-cell-0-0").unwrap();
    visual.simulate_click(first_cell.center(), Default::default());
    for _ in 0..11 {
        visual.simulate_keystrokes("right");
    }
    for _ in 0..40 {
        visual.simulate_keystrokes("down");
    }
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.selection.borrow().range.unwrap().end,
            crate::grid::CellPosition {
                row: 40,
                column: 11
            }
        );
        assert!(
            f32::from(this.av().grid_scroll.0.borrow().base_handle.offset().y) < -500.0,
            "moving to row 40 must scroll the virtualized rows"
        );
        assert!(
            f32::from(this.av().grid_horizontal_scroll.offset().x) < -800.0,
            "moving to the last column must scroll it into the horizontal viewport"
        );
        assert!(this.av().grid_ui.inline_edit.is_none());
    });

    for _ in 0..9 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("right");
    visual.simulate_keystrokes("shift-up");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let selection = this.av().grid_ui.selection.borrow();
        assert_eq!(
            selection.anchor,
            Some(crate::grid::CellPosition {
                row: 49,
                column: 11
            })
        );
        assert_eq!(
            selection.range.unwrap().end,
            crate::grid::CellPosition {
                row: 48,
                column: 11
            }
        );
        assert!(this.av().grid_ui.inline_edit.is_none());
    });
}

#[gpui::test]
fn grid_hover_leaves_selection_and_layout_untouched(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "result-grid",
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
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(
        (0..12)
            .map(|column| {
                arrow_schema::Field::new(format!("col{column}"), arrow_schema::DataType::Utf8, true)
            })
            .collect::<Vec<_>>(),
    ));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        (0..12)
            .map(|_| {
                std::sync::Arc::new(arrow_array::StringArray::from(vec![Some("x"); 50]))
                    as arrow_array::ArrayRef
            })
            .collect(),
    )
    .expect("valid result batch");
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        this.av_mut().grid_ui.visible_columns.clear();
        this.av_mut().grid_ui.layout = crate::grid::ColumnLayout::new(this.active_grid().column_count());
        cx.notify();
    });
    visual.simulate_resize(gpui::size(px(800.0), px(600.0)));
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    visual.run_until_parked();

    // Hover is style-only: moving over an unselected cell must not select it,
    // open an editor, or shift layout; moving away must leave nothing behind.
    // (Row 0 / column 1 paint in the 800px test viewport next to the sidebar.)
    let hovered = visual.debug_bounds("grid-body-cell-0-1").unwrap();
    visual.simulate_event(gpui::MouseMoveEvent {
        position: hovered.center(),
        pressed_button: None,
        modifiers: Default::default(),
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            this.av().grid_ui.selection.borrow().range.is_none(),
            "hover must not select"
        );
        assert!(this.av().grid_ui.inline_edit.is_none());
    });
    assert_eq!(
        visual.debug_bounds("grid-body-cell-0-1").unwrap(),
        hovered,
        "hover must not shift layout"
    );

    let header = visual.debug_bounds("grid-header-cell-0").unwrap();
    visual.simulate_event(gpui::MouseMoveEvent {
        position: header.center(),
        pressed_button: None,
        modifiers: Default::default(),
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            this.av().grid_ui.selection.borrow().range.is_none(),
            "mouse-leave must restore the unselected state"
        );
        assert!(this.av().grid_ui.inline_edit.is_none());
    });
    assert_eq!(
        visual.debug_bounds("grid-body-cell-0-1").unwrap(),
        hovered,
        "mouse-leave must not shift layout"
    );
}

fn context_test_app<'a>(
    cx: &'a mut TestAppContext,
    kind: &str,
    fixture: &str,
) -> (gpui::Entity<AppRoot>, &'a mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        kind,
        "--ui-fixture",
        fixture,
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
    (app, visual)
}

fn context_runtime(this: &AppRoot) -> &crate::workspace::TabRuntime {
    let tab_id = this.workspace.active_tab().unwrap().id;
    this.workspace.runtime(tab_id).unwrap()
}

#[gpui::test]
fn right_click_targets_its_column_without_discarding_a_selected_range(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    visual.run_until_parked();
    let first = visual.debug_bounds("grid-body-cell-0-0").unwrap();
    let other = visual.debug_bounds("grid-body-cell-0-1").unwrap();
    app.update(visual, |this, _| {
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 0, column: 1 });
    });
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: other.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.context_cell,
            Some(CellPosition { row: 0, column: 1 })
        );
        assert_eq!(
            this.av().grid_ui
                .selection
                .borrow()
                .range
                .unwrap()
                .normalized()
                .start,
            CellPosition { row: 0, column: 0 }
        );
    });
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: first.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.context_cell,
            Some(CellPosition { row: 0, column: 0 })
        )
    });
}

#[gpui::test]
fn sorted_page_edit_uses_original_primary_key_and_clear_restores_source_order(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
            this.sort_context_page(this.active_id(), true, window, cx);
            let tab = this.workspace.active_tab().unwrap().clone();
            let target = CellPosition { row: 0, column: 1 };
            let detail = this.build_cell_detail(&tab, target).unwrap();
            let original_id = this
                .active_grid()
                .full_cell_text(CellPosition { row: 0, column: 0 })
                .unwrap();
            let payload = this.cell_detail_payload(&tab, &detail).unwrap();
            let InlineEditPayload::Sql { key_value, .. } = &payload else {
                panic!("SQL payload required")
            };
            assert_eq!(key_value, &MutationValue::Number(original_id.clone()));
            assert!(stage_inline_sql_edit(
                this.workspace.runtime_mut(tab.id).unwrap(),
                0,
                1,
                &payload,
                &detail.value,
                "phase-one edit"
            ));
            assert_eq!(
                this.workspace.runtime(tab.id).unwrap().pending_edits[0]
                    .request
                    .key_value,
                MutationValue::Number(original_id)
            );
            let mapped = this.active_grid().source_row(0);
            assert_ne!(mapped, 0, "fixture must exercise an actual reorder");
            this.clear_context_page_sort(this.active_id(), window, cx);
            assert_eq!(
                this.active_grid().source_row(0),
                mapped,
                "pending edit must block clearing the row mapping"
            );
        });
    });
    visual.run_until_parked();
}

#[gpui::test]
fn selection_null_rejects_non_nullable_cells_atomically(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 1 });
        selection.extend_to(CellPosition { row: 0, column: 2 });
        drop(selection);
        assert!(this.prepare_context_edits(this.active_id(), None).is_err());
        assert!(context_runtime(this).pending_edits.is_empty());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        let edits = this.prepare_context_edits(this.active_id(), None).unwrap();
        this.stage_context_edits(this.active_id(), edits, None, cx).unwrap();
        let pending = &this.workspace.runtime(tab_id).unwrap().pending_edits;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request.value, MutationValue::Null);
        assert_eq!(pending[0].request.column, "status");
    });
}

#[gpui::test]
fn existing_detail_set_null_cannot_bypass_non_nullable_constraint(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 0, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
        this.set_cell_detail_null(this.active_id(), window, cx);
        assert!(context_runtime(this).pending_edits.is_empty());
    });
}

#[gpui::test]
fn bulk_edit_rejects_invalid_types_and_readonly_columns_without_partial_staging(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        assert!(
            this.prepare_context_edits(this.active_id(), Some("12")).is_err(),
            "primary key must be protected"
        );
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.av().grid_ui
            .selection
            .borrow_mut()
            .extend_to(CellPosition { row: 0, column: 3 });
        assert!(this.prepare_context_edits(this.active_id(), Some("not-boolean")).is_err());
        assert!(context_runtime(this).pending_edits.is_empty());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        let edits = this.prepare_context_edits(this.active_id(), Some("")).unwrap();
        this.stage_context_edits(this.active_id(), edits, Some(""), cx).unwrap();
        let pending = &this.workspace.runtime(tab_id).unwrap().pending_edits;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request.value, MutationValue::Text(String::new()));
    });
}

#[gpui::test]
fn right_click_menu_sort_dispatches_for_the_clicked_column(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    visual.run_until_parked();
    let cell = visual.debug_bounds("grid-body-cell-0-0").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: cell.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.run_until_parked();
    for _ in 0..4 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_ne!(
            this.active_grid().source_row(0),
            0,
            "menu must apply descending sort to page"
        );
        assert_eq!(
            this.active_grid().source_row(0),
            context_runtime(this).grid.source_row(0)
        );
    });
}

#[gpui::test]
fn mongo_sorted_row_inspection_and_null_mutation_retain_source_identity(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab = this.workspace.active_tab().unwrap().clone();
        let column = (0..this.active_grid().column_count())
            .find(|column| this.active_grid().column_name(*column) == Some("name"))
            .unwrap();
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column });
        this.sort_context_page(this.active_id(), true, window, cx);
        let source_row = this.active_grid().source_row(0);
        assert_ne!(source_row, 0);
        let page_id = context_runtime(this).driver_page_id.unwrap();
        let expected = this
            .scenario_ui
            .service
            .as_ref()
            .unwrap()
            .fixture()
            .inspect_mongo_row(page_id, source_row as u64)
            .unwrap();
        assert_eq!(
            this.mongo_row_fields(tab.id, 0).unwrap().mutation_identity,
            expected.mutation_identity
        );
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column });
        let edits = this.prepare_context_edits(this.active_id(), None).unwrap();
        this.stage_context_edits(this.active_id(), edits, None, cx).unwrap();
        let mutation = context_runtime(this)
            .mongo
            .mutations
            .active()
            .last()
            .unwrap()
            .request();
        assert_eq!(mutation.identity, expected.mutation_identity);
    });
}

#[gpui::test]
fn mongo_database_sort_uses_context_column_not_selection_anchor(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let column = (0..this.active_grid().column_count())
            .find(|column| this.active_grid().column_name(*column) == Some("name"))
            .unwrap();
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column });
        this.sort_context_database(this.active_id(), true, window, cx);
        assert_eq!(
            this.workspace.runtime(tab_id).unwrap().mongo.sort,
            r#"{"name":-1}"#
        );
        assert_eq!(context_runtime(this).pagination.offset(), 0);
    });
}

#[gpui::test]
fn mongo_clear_sort_resets_database_sort_and_local_order(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let column = (0..this.active_grid().column_count())
            .find(|column| this.active_grid().column_name(*column) == Some("name"))
            .unwrap();
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column });
        this.sort_context_database(this.active_id(), true, window, cx);
        this.sort_context_page(this.active_id(), true, window, cx);
        this.clear_context_sort(this.active_id(), window, cx);
        assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.sort, "");
        assert_eq!(this.active_grid().source_row(0), 0);
    });
}

#[gpui::test]
fn sql_database_sort_uses_safe_override_and_clear_preserves_editor_query(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab = this.workspace.active_tab().unwrap().clone();
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 1 });
        this.sort_context_database(this.active_id(), true, window, cx);
        let runtime = this.workspace.runtime(tab.id).unwrap();
        let (base, sorted) = runtime
            .context_sql_sort
            .as_ref()
            .expect("SQL sort override");
        assert_eq!(base, &tab.sql);
        assert!(sorted.contains("ORDER BY \"name\" DESC"));
        assert_eq!(
            this.workspace.active_tab().unwrap().sql,
            tab.sql,
            "sort must not edit user SQL"
        );
        this.sort_context_page(this.active_id(), true, window, cx);
        let generation = this.workspace.runtime(tab.id).unwrap().query.generation();
        this.clear_context_sort(this.active_id(), window, cx);
        assert!(this
            .workspace
            .runtime(tab.id)
            .unwrap()
            .context_sql_sort
            .is_none());
        assert_eq!(this.workspace.active_tab().unwrap().sql, tab.sql);
        assert!(this.workspace.runtime(tab.id).unwrap().query.generation() > generation);
        assert_eq!(this.active_grid().source_row(0), 0);
    });
}

#[gpui::test]
fn clear_only_local_sql_sort_does_not_rerun_database_query(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
        this.sort_context_page(this.active_id(), true, window, cx);
        let generation = this.workspace.runtime(tab_id).unwrap().query.generation();
        assert_ne!(this.active_grid().source_row(0), 0);
        this.clear_context_sort(this.active_id(), window, cx);
        assert_eq!(this.active_grid().source_row(0), 0);
        assert_eq!(
            this.workspace.runtime(tab_id).unwrap().query.generation(),
            generation
        );
    });
}

#[gpui::test]
fn sorted_sql_insert_and_delete_stage_original_record_without_writing(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
        this.sort_context_page(this.active_id(), true, window, cx);
        let source_id = this
            .active_grid()
            .full_cell_text(CellPosition { row: 0, column: 0 })
            .unwrap();
        let tab = this.workspace.active_tab().unwrap().clone();
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns[7]
            .is_generated = None;
        assert!(!this.sql_clone_available(this.active_id()));
        assert!(this.prepare_sql_clone(this.active_id(), 0).unwrap_err().contains("generated"));
        let original_name = this
            .active_grid()
            .full_cell_text(CellPosition { row: 0, column: 1 })
            .unwrap();
        let insert = crate::staged_sql::InsertRowRequest {
            database: tab.database.clone(),
            schema: tab.schema.clone().unwrap_or_else(|| "public".into()),
            table: tab.collection.clone(),
            values: vec![("name".into(), MutationValue::Text(original_name))],
        };
        let detail = this
            .build_cell_detail(&tab, CellPosition { row: 0, column: 1 })
            .unwrap();
        let payload = this.cell_detail_payload(&tab, &detail).unwrap();
        assert!(stage_inline_sql_edit(
            this.workspace.runtime_mut(tab_id).unwrap(),
            0,
            1,
            &payload,
            &detail.value,
            "staged clone text"
        ));
        let delete = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        assert_eq!(delete.request.key_value, MutationValue::Number(source_id));
        this.stage_sql_insert(insert, tab_id, cx);
        this.stage_sql_delete(delete, 0, tab_id, cx);
        assert_eq!(context_runtime(this).pending_sql_rows.len(), 2);
        assert!(context_runtime(this).pending_deleted_rows.contains(&0));
    });
}

#[gpui::test]
fn sql_clone_uses_full_sorted_values_and_omits_known_generated_identity_columns(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
        this.sort_context_page(this.active_id(), true, window, cx);
        for column in &mut this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns
        {
            column.is_generated = Some(false);
            column.auto_increment = Some(false);
            if column.name == "id" {
                column.auto_increment = Some(true);
            }
            if column.name == "payload" {
                column.is_generated = Some(true);
            }
        }
        let tab = this.workspace.active_tab().unwrap().clone();
        assert!(this.sql_clone_available(this.active_id()));
        let detail = this
            .build_cell_detail(&tab, CellPosition { row: 0, column: 1 })
            .unwrap();
        let payload = this.cell_detail_payload(&tab, &detail).unwrap();
        assert!(stage_inline_sql_edit(
            this.workspace.runtime_mut(tab_id).unwrap(),
            0,
            1,
            &payload,
            &detail.value,
            "edited before clone"
        ));
        let clone = this.prepare_sql_clone(this.active_id(), 0).unwrap();
        let CloneSqlRow::Ready(request) = clone else {
            panic!("automatic identity should produce a ready request")
        };
        assert!(!request
            .values
            .iter()
            .any(|(name, _)| name == "id" || name == "payload"));
        assert_eq!(
            request
                .values
                .iter()
                .find(|(name, _)| name == "name")
                .unwrap()
                .1,
            MutationValue::Text("edited before clone".into())
        );
        assert_eq!(
            request
                .values
                .iter()
                .find(|(name, _)| name == "big_number")
                .unwrap()
                .1,
            MutationValue::Number("9000000000000000049".into())
        );
    });
}

#[gpui::test]
fn sample_fixture_clone_is_an_editable_pending_insert_not_a_database_write(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        assert!(this.sql_clone_available(this.active_id()));
        this.clone_context_sql_row(this.active_id(), 0, window, cx);
        let staged = &context_runtime(this).pending_sql_rows;
        assert_eq!(staged.len(), 1);
        let crate::workspace::PendingSqlRow::Insert(request) = &staged[0] else {
            panic!("clone stages insert")
        };
        assert!(!request.values.iter().any(|(name, _)| name == "id"));
        assert_eq!(tab_id, this.workspace.active_tab().unwrap().id);
    });
}

#[gpui::test]
fn sql_clone_with_required_manual_primary_key_returns_draft_not_staged_insert(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        for column in &mut this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns
        {
            column.is_generated = Some(false);
            column.auto_increment = Some(false);
            if column.primary_key {
                column.default_expression = Some("'fixed'".into());
            }
        }
        let CloneSqlRow::NeedsValues { request, missing } = this.prepare_sql_clone(this.active_id(), 0).unwrap()
        else {
            panic!("manual PK needs draft")
        };
        assert_eq!(missing, vec!["id"]);
        assert!(!request.values.iter().any(|(name, _)| name == "id"));
        assert!(this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .pending_sql_rows
            .is_empty());
        let columns = &this.workspace.runtime(tab_id).unwrap().structure_columns;
        assert!(super::results::mutations::finish_sql_clone_draft(
            request.clone(),
            &missing,
            "{}",
            columns
        )
        .is_err());
        let mut fields = request
            .values
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    MutationValue::Text(value) | MutationValue::Number(value) => {
                        serde_json::Value::String(value.clone())
                    }
                    MutationValue::Boolean(value) => serde_json::Value::Bool(*value),
                    MutationValue::Null => serde_json::Value::Null,
                };
                (name.clone(), value)
            })
            .collect::<serde_json::Map<_, _>>();
        fields.insert("id".into(), serde_json::Value::String("5001".into()));
        let json = serde_json::to_string(&fields).unwrap();
        let completed =
            super::results::mutations::finish_sql_clone_draft(request, &missing, &json, columns)
                .unwrap();
        assert_eq!(
            completed
                .values
                .iter()
                .find(|(name, _)| name == "id")
                .unwrap()
                .1,
            MutationValue::Number("5001".into())
        );
    });
}

#[gpui::test]
fn sql_clone_draft_dialog_refuses_missing_primary_key_before_staging(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        for column in &mut this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns
        {
            column.is_generated = Some(false);
            column.auto_increment = Some(false);
        }
        this.clone_context_sql_row(this.active_id(), 0, window, cx);
    });
    visual.run_until_parked();
    assert!(visual.debug_bounds("document-editor").is_some());
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    assert!(visual.debug_bounds("document-editor").is_some());
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_sql_rows.is_empty())
    });
}

#[gpui::test]
fn sql_row_actions_refuse_new_staging_during_apply(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .sql_apply_running = true;
        assert!(this.prepare_sql_clone(this.active_id(), 0).is_err());
        assert!(this.prepare_sql_delete(this.active_id(), 0).is_err());
        assert!(context_runtime(this).pending_sql_rows.is_empty());
    });
}

#[gpui::test]
fn composite_primary_key_keeps_cells_editable_but_blocks_single_key_sql_copies(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().clone();
        this.workspace
            .runtime_mut(tab.id)
            .unwrap()
            .structure_columns[1]
            .primary_key = true;
        // The driver gets the whole key with every change: a non-key cell of a
        // composite-key table is editable, key cells are not.
        let position = CellPosition { row: 0, column: 2 };
        assert!(this.build_cell_detail(&tab, position).unwrap().editable);
        this.av().grid_ui.selection.borrow_mut().select(position);
        let edits = this.prepare_context_edits(this.active_id(), Some("changed")).unwrap();
        assert_eq!(this.stage_context_edits(this.active_id(), edits, Some("changed"), cx), Ok(1));
        let staged = &context_runtime(this).pending_edits;
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].key.len(), 2, "the whole key travels with the change: {:?}", staged[0].key);
        let key_cell = CellPosition { row: 0, column: 1 };
        this.av().grid_ui.selection.borrow_mut().select(key_cell);
        assert!(this.prepare_context_edits(this.active_id(), Some("changed")).is_err());
        // UPDATE / INSERT-without-key statements are built from one key column.
        assert!(this.copy_context_selection(this.active_id(), CopyFormat::SqlUpdate).is_err());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        this.av().grid_ui
            .selection
            .borrow_mut()
            .extend_to(CellPosition { row: 0, column: 2 });
        assert!(this
            .copy_context_selection(this.active_id(), CopyFormat::SqlInsertNoPk)
            .is_err());
    });
}

#[gpui::test]
fn two_rows_sharing_first_composite_key_component_stage_updates_on_their_own_whole_key(cx: &mut TestAppContext) {
    use arrow_array::{Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("tenant", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(Int64Array::from(vec![1, 1])),
                std::sync::Arc::new(StringArray::from(vec!["a", "b"])),
                std::sync::Arc::new(StringArray::from(vec!["old", "old"])),
            ],
        )
        .unwrap();
        this.set_active_grid(crate::grid::GridState::new(crate::grid_source::GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        runtime.structure_columns = vec![
            crate::catalog::CatalogColumn {
                name: "id".into(),
                data_type: "int64".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
            crate::catalog::CatalogColumn {
                name: "tenant".into(),
                data_type: "text".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
            crate::catalog::CatalogColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: false,
                primary_key: false,
                ..Default::default()
            },
        ];
        assert_eq!(
            this.active_grid().full_cell_text(CellPosition { row: 0, column: 0 }),
            this.active_grid().full_cell_text(CellPosition { row: 1, column: 0 })
        );
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        // The first component alone is ambiguous; the whole key is not.
        let edits = this.prepare_context_edits(this.active_id(), Some("new")).unwrap();
        assert_eq!(this.stage_context_edits(this.active_id(), edits, Some("new"), cx), Ok(1));
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 1, column: 2 });
        let edits = this.prepare_context_edits(this.active_id(), Some("new")).unwrap();
        assert_eq!(this.stage_context_edits(this.active_id(), edits, Some("new"), cx), Ok(1));
        let keys: Vec<_> = this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .pending_edits
            .iter()
            .map(|edit| serde_json::Value::Object(edit.key.clone()).to_string())
            .collect();
        assert_eq!(keys, [r#"{"id":1,"tenant":"a"}"#, r#"{"id":1,"tenant":"b"}"#]);
    });
}

#[gpui::test]
fn discard_during_in_flight_sql_apply_preserves_submitted_queue(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let request = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(request, 0, tab_id, cx);
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .sql_apply_running = true;
        this.discard_pending_edit(this.active_id(), cx);
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.pending_sql_rows.len(), 1);
        assert!(runtime.pending_deleted_rows.contains(&0));
        assert!(runtime.sql_apply_running);
        let submitted = runtime.pending_sql_rows.clone();
        super::results::editing::reconcile_submitted_sql_batch(
            this.workspace.runtime_mut(tab_id).unwrap(),
            &[],
            &submitted,
        );
        assert!(context_runtime(this).pending_sql_rows.is_empty());
    });
}

#[gpui::test]
fn deleted_sql_row_blocks_bulk_null_and_generation_before_any_cell_is_staged(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let request = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(request, 0, tab_id, cx);
        let mut selected = this.av().grid_ui.selection.borrow_mut();
        selected.select(CellPosition { row: 0, column: 2 });
        selected.extend_to(CellPosition { row: 1, column: 2 });
        drop(selected);
        assert!(this.prepare_context_edits(this.active_id(), None).is_err());
        assert!(this.prepare_context_edits(this.active_id(), Some("bulk")).is_err());
        assert!(this
            .generate_context_selection(this.active_id(), GenerateKind::Null, cx)
            .is_err());
        assert!(context_runtime(this).pending_edits.is_empty());
    });
}

#[gpui::test]
fn context_staging_reports_stale_deleted_row_instead_of_counting_ignored_sql_edit(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        let edits = this.prepare_context_edits(this.active_id(), Some("changed")).unwrap();
        let delete = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(delete, 0, tab_id, cx);
        assert!(this
            .stage_context_edits(this.active_id(), edits, Some("changed"), cx)
            .is_err());
        assert!(context_runtime(this).pending_edits.is_empty());
    });
}

#[gpui::test]
fn sorted_selection_copy_uses_full_source_values_and_one_off_does_not_change_default(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
        this.sort_context_page(this.active_id(), true, window, cx);
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        let expected = this
            .active_grid()
            .full_cell_text(CellPosition { row: 0, column: 0 })
            .unwrap();
        let text = this.copy_context_selection(this.active_id(), CopyFormat::Csv).unwrap();
        assert_eq!(text, expected);
        assert_eq!(
            this.snapshot_selected_bounds(this.active_id()).unwrap().height,
            crate::theme::ROW_HEIGHT as u32
        );
        assert_eq!(
            this.copy_context_scope(this.active_id(), CopyScope::Cell, CopyFormat::Raw)
                .unwrap(),
            expected
        );
        let names = this
            .copy_context_scope(this.active_id(), CopyScope::ColumnNames, CopyFormat::Smart)
            .unwrap();
        assert_eq!(names, "id");
        let row_json = this
            .copy_context_scope(this.active_id(), CopyScope::Rows, CopyFormat::Smart)
            .unwrap();
        let row: serde_json::Value = serde_json::from_str(&row_json).unwrap();
        assert_eq!(
            row["id"],
            serde_json::json!(expected.parse::<i64>().unwrap())
        );
        this.av().grid_ui
            .selection
            .borrow_mut()
            .extend_to(CellPosition { row: 1, column: 0 });
        let rows_json = this
            .copy_context_scope(this.active_id(), CopyScope::Rows, CopyFormat::Smart)
            .unwrap();
        let rows: serde_json::Value = serde_json::from_str(&rows_json).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert_eq!(rows[0]["id"], row["id"]);
        let all_visible = this
            .copy_context_scope(this.active_id(), CopyScope::Page, CopyFormat::Smart)
            .unwrap();
        assert!(all_visible.starts_with("id\tname\tstatus\t"));
        assert_eq!(all_visible.lines().count(), this.active_grid().row_count() + 1);
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        assert_eq!(this.grid_prefs.copy_default, CopyFormat::Smart);
        this.grid_prefs.copy_default = CopyFormat::CsvWithHeaders;
        assert_eq!(
            this.copy_context_selection(this.active_id(), CopyFormat::Smart).unwrap(),
            format!("id\n{expected}")
        );
    });
}

#[gpui::test]
fn snapshot_save_cancel_does_not_create_or_modify_a_file(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 1 });
        this.save_selected_snapshot(this.active_id(), cx);
    });
    visual.run_until_parked();
    assert!(visual.did_prompt_for_new_path());
    visual.simulate_new_path_selection(|_| None);
    visual.run_until_parked();
    app.update(visual, |this, _| assert!(this.av().detail_ui.error.is_none()));
}

#[gpui::test]
fn selected_cells_save_png_through_platform_prompt_to_owned_temp_path(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let path = std::env::temp_dir().join(format!(
        "tablex-selected-cells-{}.png",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        let mut selected = this.av().grid_ui.selection.borrow_mut();
        selected.select(CellPosition { row: 0, column: 1 });
        selected.extend_to(CellPosition { row: 1, column: 2 });
        drop(selected);
        this.save_selected_snapshot(this.active_id(), cx);
    });
    visual.simulate_new_path_selection(|_| Some(path.clone()));
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(this.av().detail_ui.error.is_none(), "{:?}", this.av().detail_ui.error);
    });
    let png = std::fs::read(&path).unwrap();
    let decoded = image::load_from_memory(&png).unwrap();
    assert!(decoded.width() > 0 && decoded.height() >= 3 * crate::theme::ROW_HEIGHT as u32);
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn selected_snapshot_preview_uses_png_image_and_cancel_opens_no_save_prompt(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 1 });
        this.open_selected_snapshot_preview(this.active_id(), window, cx);
    });
    visual.run_until_parked();
    assert!(visual.debug_bounds("snapshot-preview").is_some());
    assert!(!visual.did_prompt_for_new_path());
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    assert!(!visual.did_prompt_for_new_path());
}

#[gpui::test]
fn selected_snapshot_preview_save_writes_png_to_owned_temp_path(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let path = std::env::temp_dir().join(format!("tablex-preview-{}.png", uuid::Uuid::new_v4()));
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 1 });
        this.open_selected_snapshot_preview(this.active_id(), window, cx);
    });
    visual.run_until_parked();
    assert!(visual.debug_bounds("snapshot-preview").is_some());
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    assert!(visual.did_prompt_for_new_path());
    visual.simulate_new_path_selection(|_| Some(path.clone()));
    visual.run_until_parked();
    assert!(
        image::load_from_memory(&std::fs::read(&path).unwrap())
            .unwrap()
            .width()
            >= 160
    );
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn generator_skips_primary_key_and_reports_eligible_counts_without_writing(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        this.av().grid_ui
            .selection
            .borrow_mut()
            .extend_to(CellPosition { row: 0, column: 1 });
        let (eligible, skipped) = this
            .generate_context_selection(this.active_id(), GenerateKind::EmptyString, cx)
            .unwrap();
        assert_eq!((eligible, skipped), (1, 1));
        let pending = &this.workspace.runtime(tab_id).unwrap().pending_edits;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request.column, "name");
        assert_eq!(pending[0].request.value, MutationValue::Text(String::new()));
    });
}

#[gpui::test]
fn configured_snowflake_stages_only_selected_values_after_durable_reservation(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let dir = std::env::temp_dir().join(format!("tablex-snowflake-ui-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    app.update(visual, |this, cx| {
        this.grid_prefs.snowflake_dir = Some(dir.clone());
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 2 });
        selection.extend_to(CellPosition { row: 1, column: 2 });
        drop(selection);
        this.start_snowflake_generation(this.active_id(), 17, cx).unwrap();
        assert!(context_runtime(this).pending_edits.is_empty());
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let pending = &context_runtime(this).pending_edits;
        assert_eq!(pending.len(), 2);
        let ids = pending
            .iter()
            .map(|edit| match &edit.request.value {
                MutationValue::Text(value) => value.parse::<i64>().unwrap(),
                _ => panic!("text column retains decimal Snowflake ID"),
            })
            .collect::<Vec<_>>();
        assert!(ids[0] > 0 && ids[0] < ids[1]);
    });
    assert_eq!(
        super::results::snowflake::SnowflakeStore::read_worker(&dir).unwrap(),
        Some(17)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn native_scenario_uses_one_isolated_root_for_snowflake_copy_and_saved_sql(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let root = app.update(visual, |this, cx| {
        let root = this.saved_sql_ui.source().unwrap().0;
        assert!(root.starts_with(std::env::temp_dir()));
        assert_eq!(
            this.grid_prefs.snowflake_dir.as_deref(),
            Some(root.join("snowflake").as_path())
        );
        assert_eq!(
            this.grid_prefs.copy_default_path.as_deref(),
            Some(root.join("grid-copy-default.json").as_path())
        );
        assert_eq!(this.grid_prefs.copy_default, CopyFormat::Smart);
        this.set_copy_default(this.active_id(), CopyFormat::CsvWithHeaders, cx);
        assert!(root.join("grid-copy-default.json").exists());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.start_snowflake_generation(this.active_id(), 17, cx).unwrap();
        root
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(context_runtime(this).pending_edits.len(), 1);
        assert!(this.grid_prefs.snowflake_dir
            .as_ref()
            .unwrap()
            .join("worker-17.state")
            .exists());
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[gpui::test]
fn scenario_env_root_child_uses_isolated_storage(cx: &mut TestAppContext) {
    let Ok(root) = std::env::var("TABLEX_SCENARIO_EXPECTED_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        assert_eq!(this.saved_sql_ui.source().unwrap().0, root);
        assert_eq!(
            this.grid_prefs.snowflake_dir.as_deref(),
            Some(root.join("snowflake").as_path())
        );
        assert_eq!(
            this.grid_prefs.copy_default_path.as_deref(),
            Some(root.join("grid-copy-default.json").as_path())
        );
        this.set_copy_default(this.active_id(), CopyFormat::CsvWithHeaders, cx);
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.start_snowflake_generation(this.active_id(), 17, cx).unwrap();
    });
    visual.run_until_parked();
    assert!(root.join("grid-copy-default.json").exists());
    assert!(root.join("snowflake/worker-17.state").exists());
}

#[test]
fn scenario_public_sidebar_path_binds_all_writable_roots_in_child_process() {
    let root = std::env::temp_dir().join(format!("tablex-scenario-owned-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app::tests::scenario_env_root_child_uses_isolated_storage",
        ])
        .env("TABLEX_SIDEBAR_PREFS_PATH", root.join("sidebar.json"))
        .env("TABLEX_SCENARIO_EXPECTED_ROOT", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[gpui::test]
fn scenario_temp_alias_child_uses_private_root(cx: &mut TestAppContext) {
    if std::env::var_os("TABLEX_SCENARIO_TEMP_ALIAS_TEST").is_none() {
        return;
    }
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, _| {
        let root = this.saved_sql_ui.source().unwrap().0;
        assert!(root.starts_with(std::env::temp_dir()));
        let alias =
            std::path::PathBuf::from(std::env::var_os("TABLEX_SIDEBAR_PREFS_PATH").unwrap());
        assert_ne!(root, alias.parent().unwrap());
        assert_ne!(root, std::env::temp_dir());
        assert_eq!(
            this.grid_prefs.snowflake_dir.as_deref(),
            Some(root.join("snowflake").as_path())
        );
    });
}

#[cfg(unix)]
#[test]
fn scenario_alias_to_shared_temp_does_not_use_temp_as_storage_root() {
    let alias = std::env::temp_dir().join(format!("tablex-root-alias-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(std::env::temp_dir(), &alias).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app::tests::scenario_temp_alias_child_uses_private_root",
        ])
        .env("TABLEX_SIDEBAR_PREFS_PATH", alias.join("sidebar.json"))
        .env("TABLEX_SCENARIO_TEMP_ALIAS_TEST", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    std::fs::remove_file(alias).unwrap();
}

#[gpui::test]
fn generated_values_reject_mixed_types_and_overflow_without_partial_staging(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let dir = std::env::temp_dir().join(format!("tablex-snowflake-types-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    app.update(visual, |this, cx| {
        this.grid_prefs.snowflake_dir = Some(dir.clone());
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 2 });
        selection.extend_to(CellPosition { row: 0, column: 3 });
        drop(selection);
        assert!(this.start_snowflake_generation(this.active_id(), 17, cx).is_err());
        assert!(context_runtime(this).pending_edits.is_empty());
        assert!(!dir.join("worker.json").exists());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.av().grid_ui
            .selection
            .borrow_mut()
            .extend_to(CellPosition { row: 1, column: 2 });
        assert!(this
            .generate_context_selection(this.active_id(), GenerateKind::Increment(i64::MAX), cx)
            .is_err());
        assert!(context_runtime(this).pending_edits.is_empty());
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn snowflake_worker_dialog_rejects_invalid_id_then_stages_selected_cell(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let dir =
        std::env::temp_dir().join(format!("tablex-snowflake-dialog-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    app.update_in(visual, |this, window, cx| {
        this.grid_prefs.snowflake_dir = Some(dir.clone());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.open_context_snowflake_dialog(this.active_id(), window, cx);
    });
    visual.run_until_parked();
    let input = visual.debug_bounds("snowflake-worker-input").unwrap();
    visual.simulate_click(input.center(), Default::default());
    visual.simulate_input("1024");
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_edits.is_empty())
    });
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("17");
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(context_runtime(this).pending_edits.len(), 1)
    });
    assert_eq!(
        super::results::snowflake::SnowflakeStore::read_worker(&dir).unwrap(),
        Some(17)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn snowflake_delayed_configuration_rejects_changed_selection_before_dialog(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let (release, waiting) = std::sync::mpsc::channel::<()>();
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.open_context_snowflake_dialog_with_read(this.active_id(), 
            move |_| {
                waiting.recv().unwrap();
                Ok(Some(17))
            },
            window,
            cx,
        );
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 1, column: 2 });
    });
    release.send(()).unwrap();
    visual.run_until_parked();
    assert!(visual.debug_bounds("snowflake-worker-input").is_none());
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_edits.is_empty())
    });
}

#[gpui::test]
fn snowflake_delayed_configuration_rejects_reordered_page_before_dialog(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let (release, waiting) = std::sync::mpsc::channel::<()>();
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.open_context_snowflake_dialog_with_read(this.active_id(), 
            move |_| {
                waiting.recv().unwrap();
                Ok(Some(17))
            },
            window,
            cx,
        );
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
        this.sort_context_page(this.active_id(), true, window, cx);
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
    });
    release.send(()).unwrap();
    visual.run_until_parked();
    assert!(visual.debug_bounds("snowflake-worker-input").is_none());
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_edits.is_empty())
    });
}

#[gpui::test]
fn snowflake_delayed_configuration_rejects_changed_tab_before_dialog(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let (release, waiting) = std::sync::mpsc::channel::<()>();
    let tab_a = app.read_with(visual, |this, _| this.active_id());
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.open_context_snowflake_dialog_with_read(this.active_id(), 
            move |_| {
                waiting.recv().unwrap();
                Ok(Some(17))
            },
            window,
            cx,
        );
        this.workspace.open_query_tab("another tab");
    });
    release.send(()).unwrap();
    visual.run_until_parked();
    assert!(visual.debug_bounds("snowflake-worker-input").is_none());
    app.update(visual, |this, _| {
        assert!(this
            .view(tab_a)
            .detail_ui
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("changed"))
    });
}

#[gpui::test]
fn snowflake_dialog_rejects_changed_target_at_confirmation(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.open_context_snowflake_dialog_with_read(this.active_id(), |_| Ok(Some(17)), window, cx);
    });
    visual.run_until_parked();
    assert!(visual.debug_bounds("snowflake-worker-input").is_some());
    app.update(visual, |this, _| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 1, column: 2 });
    });
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_edits.is_empty())
    });
    assert!(visual.debug_bounds("snowflake-worker-input").is_some());
}

#[gpui::test]
fn snowflake_reservation_does_not_stage_into_changed_selection(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let dir = std::env::temp_dir().join(format!("tablex-snowflake-stale-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    app.update(visual, |this, cx| {
        this.grid_prefs.snowflake_dir = Some(dir.clone());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.start_snowflake_generation(this.active_id(), 7, cx).unwrap();
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 1, column: 2 });
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_edits.is_empty());
        assert!(this
            .av().detail_ui
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("changed"));
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn snowflake_reservation_does_not_stage_after_page_reorder_and_reselection(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let dir =
        std::env::temp_dir().join(format!("tablex-snowflake-reorder-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    app.update_in(visual, |this, window, cx| {
        this.grid_prefs.snowflake_dir = Some(dir.clone());
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        this.start_snowflake_generation(this.active_id(), 7, cx).unwrap();
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
        this.sort_context_page(this.active_id(), true, window, cx);
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_edits.is_empty())
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn context_row_and_column_details_target_clicked_cell_not_selection_anchor(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 1, column: 1 });
        this.open_context_row_details(this.active_id(), cx);
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert_eq!(state.target, Some(CellPosition { row: 1, column: 1 }));
        assert_eq!(state.tab, CellDetailTab::Row);
        assert_eq!(
            state.row.as_ref().unwrap().fields.len(),
            this.active_grid().column_count()
        );
        this.open_context_column_details(this.active_id(), cx);
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert_eq!(state.tab, CellDetailTab::Column);
        let column = state.column.as_ref().unwrap();
        assert_eq!(column.name, "name");
        assert_eq!(column.primary_key, Some(false));
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(visual.debug_bounds("cell-detail-column-panel").is_some());
}

#[gpui::test]
fn staged_sql_delete_apply_requires_confirmation_and_cancel_keeps_it(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let request = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(request, 0, tab_id, cx);
        this.confirm_apply_sql_changes(this.active_id(), window, cx);
        assert_eq!(context_runtime(this).pending_sql_rows.len(), 1);
    });
    visual.run_until_parked();
    assert!(visual.debug_bounds("dialog-layer").is_some());
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(context_runtime(this).pending_sql_rows.len(), 1);
    });
}

#[gpui::test]
fn failed_sql_apply_keeps_pending_rows_and_shows_error_in_result_footer(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let request = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(request, 0, tab_id, cx);
        this.confirm_apply_sql_changes(this.active_id(), window, cx);
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    assert!(visual.debug_bounds("result-status-error").is_some());
    app.update(visual, |this, _| {
        assert_eq!(context_runtime(this).pending_sql_rows.len(), 1)
    });
}

#[gpui::test]
fn refresh_with_pending_sql_delete_cancel_preserves_every_pending_change(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let request = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(request, 0, tab_id, cx);
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let refresh = visual.debug_bounds("result-refresh").unwrap();
    visual.simulate_click(refresh.center(), Default::default());
    visual.run_until_parked();
    assert!(visual.debug_bounds("dialog-layer").is_some());
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(context_runtime(this).pending_sql_rows.len(), 1);
        assert!(context_runtime(this).pending_deleted_rows.contains(&0));
    });
}

#[gpui::test]
fn refresh_confirm_discards_sql_staging_only_after_acceptance(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let generation = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let request = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(request, 0, tab_id, cx);
        this.workspace.runtime(tab_id).unwrap().query.generation()
    });
    visual.update(|window, cx| app.update(cx, |this, cx| this.refresh_tab_results(this.active_id(), window, cx)));
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(context_runtime(this).pending_sql_rows.len(), 1)
    });
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        assert!(context_runtime(this).pending_sql_rows.is_empty());
        assert!(this.workspace.runtime(tab_id).unwrap().query.generation() > generation);
    });
}

#[gpui::test]
fn sql_query_does_not_replace_page_while_row_mutations_are_staged(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let request = this.prepare_sql_delete(this.active_id(), 0).unwrap();
        this.stage_sql_delete(request, 0, tab_id, cx);
        let generation = this.workspace.runtime(tab_id).unwrap().query.generation();
        this.run_active_tab_query(cx);
        assert_eq!(
            this.workspace.runtime(tab_id).unwrap().query.generation(),
            generation
        );
        assert!(context_runtime(this).pending_deleted_rows.contains(&0));
    });
}

#[gpui::test]
fn refresh_with_pending_mongo_mutation_cancel_preserves_staged_document(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "pending-edits");
    let before = app.update(visual, |this, _| {
        context_runtime(this).mongo.mutations.active().len()
    });
    assert!(before > 0);
    visual.update(|window, cx| app.update(cx, |this, cx| this.refresh_tab_results(this.active_id(), window, cx)));
    visual.run_until_parked();
    assert!(visual.debug_bounds("dialog-layer").is_some());
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(context_runtime(this).mongo.mutations.active().len(), before);
    });
}

#[gpui::test]
fn refresh_confirm_discards_mongo_staging_and_reruns_current_tab(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "pending-edits");
    let generation = app.update(visual, |this, _| context_runtime(this).query.generation());
    app.update_in(visual, |this, window, cx| {
        this.refresh_tab_results(this.active_id(), window, cx)
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(context_runtime(this).mongo.mutations.active().is_empty());
        assert!(context_runtime(this).query.generation() > generation);
    });
}

#[gpui::test]
fn grid_hover_preserves_pending_delete_row_state(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let scenario = UiScenario::from_args([
        "tablex-gpui",
        "--ui-scenario",
        "result-grid",
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
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(
        (0..12)
            .map(|column| {
                arrow_schema::Field::new(format!("col{column}"), arrow_schema::DataType::Utf8, true)
            })
            .collect::<Vec<_>>(),
    ));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        (0..12)
            .map(|_| {
                std::sync::Arc::new(arrow_array::StringArray::from(vec![Some("x"); 50]))
                    as arrow_array::ArrayRef
            })
            .collect(),
    )
    .expect("valid result batch");
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        this.av_mut().grid_ui.visible_columns.clear();
        this.av_mut().grid_ui.layout = crate::grid::ColumnLayout::new(this.active_grid().column_count());
        let tab_id = this.workspace.active_tab().expect("scenario tab").id;
        this.workspace
            .runtime_mut(tab_id)
            .expect("query runtime")
            .pending_deleted_rows
            .insert(0);
        cx.notify();
    });
    visual.simulate_resize(gpui::size(px(800.0), px(600.0)));
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    visual.run_until_parked();

    // The pending-delete red tint is gated off from hover: hovering the marked
    // row must neither clear the marker, select the cell, nor shift layout.
    let marked = visual.debug_bounds("grid-body-cell-0-1").unwrap();
    visual.simulate_event(gpui::MouseMoveEvent {
        position: marked.center(),
        pressed_button: None,
        modifiers: Default::default(),
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().expect("scenario tab").id;
        assert!(
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .pending_deleted_rows
                .contains(&0),
            "hover must not clear the pending-delete marker"
        );
        assert!(
            this.av().grid_ui.selection.borrow().range.is_none(),
            "hover must not select a pending-delete row"
        );
        assert!(this.av().grid_ui.inline_edit.is_none());
    });
    assert_eq!(
        visual.debug_bounds("grid-body-cell-0-1").unwrap(),
        marked,
        "hover must not shift layout"
    );
}

#[gpui::test]
fn grid_enter_opens_editor_which_owns_arrow_keys(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    let long_text = "a long editable value that needs to be shown outside its cell ".repeat(4);
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("count", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("memo", arrow_schema::DataType::Utf8, false),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::Int64Array::from(vec![1, 2])) as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::Int64Array::from(vec![7, 8])) as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::StringArray::from(vec![
                long_text.as_str(),
                long_text.as_str(),
            ])) as arrow_array::ArrayRef,
        ],
    )
    .expect("valid test batch");
    visual.simulate_resize(gpui::size(px(800.0), px(600.0)));
    visual.update(|window, _| window.activate_window());
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.open_ran_query_tab("test");
        this.workspace
            .runtime_mut(tab_id)
            .expect("query runtime")
            .structure_columns = vec![
            crate::catalog::CatalogColumn {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
            crate::catalog::CatalogColumn {
                name: "count".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: false,
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
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        let existing_payload = InlineEditPayload::Sql {
            primary_key: crate::catalog::CatalogColumn {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
            key_value: MutationValue::Number("2".into()),
            key: Default::default(),
            column_name: "count".into(),
            data_type: "integer".into(),
        };
        assert!(stage_inline_sql_edit(
            runtime,
            1,
            1,
            &existing_payload,
            "8",
            "12"
        ));
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();

    // Click and navigate into the editable value cell before opening it.
    let target = visual.debug_bounds("grid-body-cell-0-0").unwrap();
    visual.simulate_click(target.center(), Default::default());
    visual.simulate_keystrokes("right");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.selection.borrow().range.unwrap().end,
            crate::grid::CellPosition { row: 0, column: 1 }
        );
    });
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let edit = this
            .av().grid_ui
            .inline_edit
            .as_ref()
            .expect("enter must open the inline editor");
        assert_eq!(
            edit.position,
            crate::grid::CellPosition { row: 0, column: 1 }
        );
    });
    visual.simulate_keystrokes("left");
    visual.simulate_keystrokes("right");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            this.av().grid_ui.inline_edit.is_some(),
            "editor must stay open through arrow keys"
        );
        assert_eq!(
            this.av().grid_ui.selection.borrow().range.unwrap().end,
            crate::grid::CellPosition { row: 0, column: 1 },
            "arrows with an active editor must not move grid selection"
        );
    });
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("9");
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            this.av().grid_ui.inline_edit.is_none(),
            "Escape must cancel editing"
        );
        let tab_id = this.workspace.active_tab().expect("query tab").id;
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        assert_eq!(
            runtime.pending_edits.len(),
            1,
            "Escape must retain other pending edits"
        );
        assert_eq!(
            runtime
                .pending_cell_text(crate::grid::CellPosition { row: 1, column: 1 })
                .map(|text| text.as_ref()),
            Some("12")
        );
        assert_eq!(
            this.active_grid()
                .full_cell_text(crate::grid::CellPosition { row: 0, column: 1 })
                .as_deref(),
            Some("7"),
            "Escape must preserve the original cell value"
        );
    });
    visual.simulate_keystrokes("left");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.selection.borrow().range.unwrap().end,
            crate::grid::CellPosition { row: 0, column: 0 },
            "the grid must receive arrows again after Escape"
        );
    });

    visual.simulate_keystrokes("right");
    visual.simulate_keystrokes("enter");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("9");
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let tab_id = this.workspace.active_tab().expect("query tab").id;
        assert_eq!(
            this.workspace.runtime(tab_id).unwrap().pending_edits.len(),
            2
        );
        assert!(
            this.av().grid_ui.inline_edit.is_none(),
            "Enter must submit the edit"
        );
    });
    visual.simulate_keystrokes("left");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.selection.borrow().range.unwrap().end,
            crate::grid::CellPosition { row: 0, column: 0 },
            "the grid must receive arrows after Enter submission"
        );
    });

    visual.simulate_keystrokes("right");
    visual.simulate_keystrokes("right");
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        let edit = this.av().grid_ui.inline_edit.as_ref().expect("textarea editor");
        assert!(
            edit.floating,
            "long values use the floating textarea editor"
        );
        assert_eq!(edit.editor.value(cx).as_ref(), long_text);
    });
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("changed");
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            this.av().grid_ui.inline_edit.is_none(),
            "Escape must close the textarea"
        );
        let tab_id = this.workspace.active_tab().expect("query tab").id;
        assert_eq!(
            this.workspace.runtime(tab_id).unwrap().pending_edits.len(),
            2,
            "textarea cancellation must retain edits submitted in other cells"
        );
        assert_eq!(
            this.active_grid()
                .full_cell_text(crate::grid::CellPosition { row: 0, column: 2 })
                .as_deref(),
            Some(long_text.as_str()),
            "Escape must preserve the textarea's original value"
        );
    });
    visual.simulate_keystrokes("left");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.selection.borrow().range.unwrap().end,
            crate::grid::CellPosition { row: 0, column: 1 },
            "the grid must receive arrows after textarea cancellation"
        );
    });
}

#[gpui::test]
fn context_menu_page_sort_changes_view_mapping_without_touching_query(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab = this.workspace.active_tab().unwrap().clone();
        let sql = tab.sql.clone();
        let offset = this.workspace.runtime(tab.id).unwrap().pagination.offset();
        let generation = this.workspace.runtime(tab.id).unwrap().query.generation();
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 0 });
        this.sort_context_page(this.active_id(), true, window, cx);
        assert_ne!(
            this.active_grid().source_row(0),
            0,
            "page sort must reorder the displayed view"
        );
        assert_eq!(this.workspace.active_tab().unwrap().sql, sql);
        assert_eq!(
            this.workspace.runtime(tab.id).unwrap().pagination.offset(),
            offset
        );
        assert_eq!(
            this.workspace.runtime(tab.id).unwrap().query.generation(),
            generation,
            "page sort must not rerun the query"
        );
        this.clear_context_page_sort(this.active_id(), window, cx);
        assert_eq!(
            this.active_grid().source_row(0),
            0,
            "clear must restore source order"
        );
    });
}

#[gpui::test]
fn context_menu_copy_selected_matches_grid_text(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let first = this
            .active_grid()
            .full_cell_text(CellPosition { row: 0, column: 0 })
            .unwrap();
        let second = this
            .active_grid()
            .full_cell_text(CellPosition { row: 0, column: 1 })
            .unwrap();
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 0, column: 1 });
        drop(selection);
        assert_eq!(
            this.copy_context_selection(this.active_id(), CopyFormat::Smart).unwrap(),
            format!("{first}\t{second}")
        );
        let before = this.grid_prefs.copy_default;
        this.copy_format_to_clipboard(this.active_id(), CopyFormat::CsvWithHeaders, cx);
        assert!(this.av().detail_ui.error.is_none());
        assert_eq!(this.grid_prefs.copy_default, before);
    });
}

#[gpui::test]
fn context_menu_details_actions_open_real_tabs(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 1 });
        this.open_context_cell_details(this.active_id(), cx);
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert!(state.open);
        assert_eq!(state.tab, CellDetailTab::Details);
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 1 });
        this.open_context_column_details(this.active_id(), cx);
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert!(state.open);
        assert_eq!(state.tab, CellDetailTab::Column);
        assert_eq!(state.column.as_ref().unwrap().name, "name");
        this.av_mut().grid_ui.context_cell = Some(CellPosition { row: 0, column: 1 });
        this.open_context_row_details(this.active_id(), cx);
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert!(state.open);
        assert_eq!(state.tab, CellDetailTab::Row);
    });
}

#[gpui::test]
fn context_menu_screenshot_opens_preview_matching_selection(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 1, column: 2 });
    });
    let target = visual.debug_bounds("grid-body-cell-0-1").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: target.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        // Right-click inside the range preserves the 2x3 selection.
        let range = this.av().grid_ui.selection.borrow().range.unwrap().normalized();
        assert_eq!(range.start, CellPosition { row: 0, column: 0 });
        assert_eq!(range.end, CellPosition { row: 1, column: 2 });
    });
    // First down selects item 0; Screenshot is clickable 15
    // (Pin Column sits between the details items and Copy).
    for _ in 0..16 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    assert!(visual.debug_bounds("snapshot-preview").is_some());
    assert!(!visual.did_prompt_for_new_path());
    app.update(visual, |this, _| {
        let table = this.prepare_selected_snapshot(this.active_id()).unwrap();
        assert!(table.bounds.width > 0 && table.bounds.height > 0);
        let data = this
            .collect_copy_context_range(this.active_id(), this.av().grid_ui.selection.borrow().range.unwrap().normalized())
            .unwrap();
        assert_eq!(data.columns, vec!["id", "name", "status"]);
        assert_eq!(data.rows.len(), 2, "preview must cover the selection only");
        assert!(
            this.active_grid().row_count() > 2,
            "fixture must be larger than the selection"
        );
        assert!(
            data.rows[0][1].display().contains('é'),
            "unicode selection content must reach the snapshot"
        );
    });
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    assert!(visual.debug_bounds("snapshot-preview").is_none());
    assert!(!visual.did_prompt_for_new_path());
}

#[gpui::test]
fn sql_clone_draft_dialog_cancel_stages_nothing(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        for column in &mut this
            .workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns
        {
            column.is_generated = Some(false);
            column.auto_increment = Some(false);
        }
        this.clone_context_sql_row(this.active_id(), 0, window, cx);
    });
    visual.run_until_parked();
    assert!(visual.debug_bounds("document-editor").is_some());
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    assert!(visual.debug_bounds("document-editor").is_none());
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_sql_rows.is_empty());
    });
}

#[gpui::test]
fn context_menu_bulk_and_increment_dialogs_never_auto_apply(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, _| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
    });
    app.update_in(visual, |this, window, cx| {
        this.open_context_bulk_edit(this.active_id(), window, cx);
        assert!(context_runtime(this).pending_edits.is_empty());
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update_in(visual, |this, window, cx| {
        this.open_context_increment_dialog(this.active_id(), window, cx);
        assert!(context_runtime(this).pending_edits.is_empty());
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            context_runtime(this).pending_edits.is_empty(),
            "cancelled dialogs must not stage anything"
        );
    });
}

#[gpui::test]
fn context_menu_delete_confirmation_stages_nothing_until_confirmed(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 0 });
        this.confirm_delete_selected_row(this.active_id(), window, cx);
        // The sample fixture is isolated: no live connection applies
        // anything, and confirmation is the only staging path.
        assert!(!context_runtime(this).sql_apply_running);
        assert!(context_runtime(this).pending_sql_rows.is_empty());
        assert!(context_runtime(this).pending_deleted_rows.is_empty());
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(context_runtime(this).pending_sql_rows.is_empty());
        assert!(context_runtime(this).pending_deleted_rows.is_empty());
    });
}

#[gpui::test]
fn context_menu_refresh_cancel_preserves_staging_and_raw_data(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update_in(visual, |this, window, cx| {
        this.av().grid_ui
            .selection
            .borrow_mut()
            .select(CellPosition { row: 0, column: 2 });
        let edits = this.prepare_context_edits(this.active_id(), Some("")).unwrap();
        this.stage_context_edits(this.active_id(), edits, Some(""), cx).unwrap();
        let raw = this
            .active_grid()
            .full_cell_text(CellPosition { row: 0, column: 2 })
            .unwrap();
        this.refresh_tab_results(this.active_id(), window, cx);
        // Cancel/Escape retains UI state and raw data; only Proceed
        // clears the queue and reloads asynchronously.
        assert_eq!(context_runtime(this).pending_edits.len(), 1);
        assert_eq!(
            this.active_grid()
                .full_cell_text(CellPosition { row: 0, column: 2 })
                .unwrap(),
            raw
        );
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            context_runtime(this).pending_edits.len(),
            1,
            "refresh cancel must retain staged edits"
        );
    });
}

#[gpui::test]
fn context_menu_clear_selection_via_real_menu_keyboard(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    visual.run_until_parked();
    let cell = visual.debug_bounds("grid-body-cell-0-1").unwrap();
    app.update(visual, |this, _| {
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 0, column: 1 });
    });
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: cell.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.run_until_parked();
    // First down selects item 0; separators, labels and
    // disabled items are skipped): 2 database, page asc, page desc,
    // clear, 3 details, Copy submenu, Set NULL, Bulk, Generate
    // submenu, Transpose, then Clear Selection; Pin Column (SQL results)
    // comes right after the details items.
    for _ in 0..15 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(
            this.av().grid_ui.selection.borrow().range.is_none(),
            "menu Clear Selection must act on the grid"
        );
    });
}

#[gpui::test]
fn context_menu_dsv_dialog_validates_then_copies(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    app.update(visual, |this, _| {
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 0, column: 1 });
    });
    app.update_in(visual, |this, window, cx| {
        this.open_copy_dsv_dialog(this.active_id(), window, cx);
    });
    visual.run_until_parked();
    let column = visual
        .debug_bounds("copy-dsv-column")
        .expect("column input");
    visual.simulate_click(column.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_keystrokes("backspace");
    visual.run_until_parked();
    let apply = visual.debug_bounds("copy-dsv-apply").expect("apply button");
    visual.simulate_click(apply.center(), Default::default());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("copy-dsv-error").is_some(),
        "invalid DSV settings must keep the dialog open with an error"
    );
    visual.simulate_input(",");
    visual.run_until_parked();
    let apply = visual.debug_bounds("copy-dsv-apply").expect("apply button");
    visual.simulate_click(apply.center(), Default::default());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("copy-dsv-apply").is_none(),
        "valid DSV settings must copy and close"
    );
    app.update(visual, |this, _| {
        assert_eq!(
            this.grid_prefs.copy_default,
            super::results::copy::read_copy_default(
                this.grid_prefs.copy_default_path.as_ref().unwrap(),
            ),
            "one-off DSV copy must not change the persisted default"
        );
    });
}
