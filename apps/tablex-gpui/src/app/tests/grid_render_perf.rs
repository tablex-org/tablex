// Render-level allocation measurement for the results grid. Run on demand:
//
//   cargo test --release -p tablex-gpui grid_render_allocations -- --ignored --nocapture --test-threads=1
//
// Allocations are counted by the crate's test-only global allocator
// (`grid_bench`). The table is informational; no perf asserts.

fn wide_grid_batch(rows: usize, columns: usize) -> arrow_array::RecordBatch {
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(
        (0..columns)
            .map(|i| {
                arrow_schema::Field::new(format!("col{i}"), arrow_schema::DataType::Utf8, true)
            })
            .collect::<Vec<_>>(),
    ));
    let arrays: Vec<arrow_array::ArrayRef> = (0..columns)
        .map(|c| {
            std::sync::Arc::new(arrow_array::StringArray::from_iter(
                (0..rows).map(|r| Some(format!("r{r}c{c}"))),
            )) as arrow_array::ArrayRef
        })
        .collect();
    arrow_array::RecordBatch::try_new(schema, arrays).expect("valid batch")
}

fn wide_grid_app(
    cx: &mut TestAppContext,
    rows: usize,
    columns: usize,
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
        root.read(cx)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    let batch = wide_grid_batch(rows, columns);
    app.update(visual, |this, cx| {
        this.workspace.open_ran_query_tab("test");
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

fn measure_grid_redraws(cx: &mut TestAppContext, rows: usize, columns: usize) -> (u64, u64) {
    const REDRAWS: u64 = 10;
    let (app, visual) = wide_grid_app(cx, rows, columns);
    // Warm-up: text shaping caches, element state, deferred menus.
    for _ in 0..3 {
        app.update(visual, |_, cx| cx.notify());
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let allocs_before = crate::grid_bench::allocation_count();
    let bytes_before = crate::grid_bench::allocation_bytes();
    for _ in 0..REDRAWS {
        app.update(visual, |_, cx| cx.notify());
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let allocs = crate::grid_bench::allocation_count() - allocs_before;
    let bytes = crate::grid_bench::allocation_bytes() - bytes_before;
    (allocs / REDRAWS, bytes / REDRAWS)
}

#[gpui::test]
#[ignore = "on-demand allocation measurement; run with --release --ignored --nocapture"]
fn grid_render_allocations(cx: &mut TestAppContext) {
    let narrow = measure_grid_redraws(cx, 60, 20);
    let wide = measure_grid_redraws(cx, 60, 100);
    eprintln!("grid render allocations per redraw (mean of 10, after 3 warm-up redraws)");
    eprintln!(
        "{:>6} {:>6} {:>12} {:>14}",
        "rows", "cols", "allocs", "bytes"
    );
    eprintln!("{:>6} {:>6} {:>12} {:>14}", 60, 20, narrow.0, narrow.1);
    eprintln!("{:>6} {:>6} {:>12} {:>14}", 60, 100, wide.0, wide.1);
    assert!(narrow.0 > 0 && wide.0 > 0, "redraws should allocate");
}

fn scroll_wide_grid(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    x: Option<f32>,
    y: Option<f32>,
) {
    app.update(visual, |this, cx| {
        if let Some(x) = x {
            let offset = this.av().grid_horizontal_scroll.offset();
            this.av().grid_horizontal_scroll
                .set_offset(gpui::point(gpui::px(-x), offset.y));
        }
        if let Some(y) = y {
            let base = this.av().grid_scroll.0.borrow().base_handle.clone();
            base.set_offset(gpui::point(base.offset().x, gpui::px(-y)));
        }
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Scrolling re-renders with the new offset; a second pass settles the
    // painted bounds the hit test reads.
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
}

#[gpui::test]
fn horizontally_culled_grid_hit_tests_cells_under_both_scroll_offsets(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 60, 100);
    visual.update(|window, _| window.activate_window());

    // Left edge: only the viewport columns (+ overscan) are materialised.
    assert!(visual.debug_bounds("grid-body-cell-0-0").is_some());
    assert!(visual.debug_bounds("grid-header-cell-0").is_some());
    assert!(visual.debug_bounds("grid-body-cell-0-50").is_none());
    assert!(visual.debug_bounds("grid-header-cell-50").is_none());

    // Gutter and the empty area are not cell targets: no selection, no menu.
    let gutter = visual.debug_bounds("pending-delete-row-1").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: gutter.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: gutter.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(this.av().grid_ui.selection.borrow().range.is_none());
        assert_eq!(this.av().grid_ui.context_cell, None);
    });

    // Far right and scrolled down: the far-left column is culled, the last
    // one is painted, and clicks resolve against both offsets.
    let max_x = app.update(visual, |this, _| {
        f32::from(this.av().grid_horizontal_scroll.max_offset().x)
    });
    assert!(max_x > 10_000.0, "100 columns must overflow, got {max_x}");
    scroll_wide_grid(&app, visual, Some(max_x), Some(10.0 * theme::ROW_HEIGHT));
    assert!(visual.debug_bounds("grid-body-cell-12-0").is_none());
    assert!(visual.debug_bounds("grid-header-cell-0").is_none());
    assert!(visual.debug_bounds("grid-body-cell-12-99").is_some());
    assert!(visual.debug_bounds("grid-header-cell-99").is_some());
    assert!(
        visual.debug_bounds("grid-body-cell-0-99").is_none(),
        "row 0 is scrolled out"
    );

    let target = visual.debug_bounds("grid-body-cell-12-99").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: target.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui
                .selection
                .borrow()
                .range
                .unwrap()
                .normalized()
                .start,
            CellPosition {
                row: 12,
                column: 99
            }
        );
    });
    let target = visual.debug_bounds("grid-body-cell-14-97").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: target.center(),
        modifiers: gpui::Modifiers::shift(),
        click_count: 1,
        first_mouse: false,
    });
    app.update(visual, |this, _| {
        let range = this.av().grid_ui.selection.borrow().range.unwrap().normalized();
        assert_eq!(
            range.start,
            CellPosition {
                row: 12,
                column: 97
            }
        );
        assert_eq!(
            range.end,
            CellPosition {
                row: 14,
                column: 99
            }
        );
    });
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: target.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.context_cell,
            Some(CellPosition {
                row: 14,
                column: 97
            })
        );
    });

    // The right-click opened the cell menu; dismiss it before clicking on.
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();

    // Double-click opens the editor; its column stays materialised after
    // scrolling away so the editor keeps its element state.
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.workspace.runtime_mut(tab).unwrap().structure_columns =
            vec![crate::catalog::CatalogColumn {
                name: "col0".into(),
                data_type: "text".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            }];
    });
    let target = visual.debug_bounds("grid-body-cell-13-98").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: target.center(),
        modifiers: Default::default(),
        click_count: 2,
        first_mouse: false,
    });
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(
            this.av().grid_ui.inline_edit.as_ref().map(|edit| edit.position),
            Some(CellPosition {
                row: 13,
                column: 98
            })
        );
    });
    scroll_wide_grid(&app, visual, Some(0.0), None);
    assert!(visual.debug_bounds("grid-body-cell-13-0").is_some());
    assert!(visual.debug_bounds("grid-body-cell-13-50").is_none());
    assert!(
        visual.debug_bounds("grid-body-cell-13-98").is_some(),
        "the inline-edit column is pinned while scrolled away"
    );
}
