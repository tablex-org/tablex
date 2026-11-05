#[gpui::test]
fn cell_details_panel_shows_metadata_layout(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        crate::theme::init(app);
    });
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
    let tab_id = app.update(cx, |this, _| {
        let tab_id = this.workspace.open_ran_query_tab("Query");
        let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
            arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
            arrow_schema::Field::new("name", arrow_schema::DataType::Utf8, true),
        ]));
        let batch = arrow_array::RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![12]))
                    as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::StringArray::from(vec![Some("Btest")]))
                    as arrow_array::ArrayRef,
            ],
        )
        .expect("valid test batch");
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        runtime.structure_columns = vec![
            crate::catalog::CatalogColumn {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
            crate::catalog::CatalogColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
                ..Default::default()
            },
        ];
        this.reset_active_view();
        this.select_grid_cell(this.active_id(), CellPosition { row: 0, column: 1 });
        tab_id
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert!(cx.debug_bounds("cell-details-toggle").is_none());
    let grid_bounds = cx
        .debug_bounds("grid-scroll-area")
        .expect("grid should render");
    let rail_bounds = cx
        .debug_bounds("cell-detail-rail")
        .expect("cell detail rail should render");
    assert!(rail_bounds.left() >= grid_bounds.right());
    let details_trigger = cx
        .debug_bounds("cell-detail-rail-details")
        .expect("cell detail rail should render");
    cx.simulate_click(details_trigger.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert!(cx.debug_bounds("cell-detail-tab-strip").is_some());
    assert!(cx.debug_bounds("cell-detail-details-tab").is_some());
    assert!(cx.debug_bounds("cell-detail-details-tab-close").is_some());
    assert!(cx.debug_bounds("cell-detail-row-tab").is_none());
    let panel_bounds = cx
        .debug_bounds("cell-detail-panel")
        .expect("cell detail panel should render");
    let rail_bounds = cx
        .debug_bounds("cell-detail-rail")
        .expect("cell detail rail should render");
    // Layout order: grid | panel | rail.
    let grid_bounds = cx
        .debug_bounds("grid-scroll-area")
        .expect("grid should render");
    assert!(panel_bounds.left() >= grid_bounds.right() - gpui::px(1.0));
    assert!(rail_bounds.left() >= panel_bounds.right() - gpui::px(1.0));
    let rail_header = cx
        .debug_bounds("cell-detail-rail-header")
        .expect("rail header should render");
    assert_eq!(
        rail_header.size.height,
        gpui::px(crate::theme::RESULT_GRID_HEADER_HEIGHT)
    );
    app.read_with(cx, |this, _| {
        let detail = this.selected_cell_detail(this.active_id()).expect("selected cell detail");
        assert_eq!(detail.column_name, "name");
        assert_eq!(detail.row_number, 1);
        assert_eq!(detail.type_name, "text");
        assert_eq!(detail.value.as_ref(), "Btest");
        assert!(!detail.is_null);
        assert_eq!(detail.length, 5);
    });

    for selector in [
        "cell-detail-panel",
        "cell-detail-column",
        "cell-detail-row",
        "cell-detail-null",
        "cell-detail-length",
        "cell-detail-value",
        "cell-detail-body-content",
        "cell-detail-close",
        "cell-detail-copy-column",
        "cell-detail-copy-sql-condition",
        "cell-detail-actions",
        "cell-detail-value-header",
        "cell-detail-edit",
        "cell-detail-copy-value",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} should render"
        );
    }
    assert!(
        cx.debug_bounds("cell-detail-value")
            .expect("cell value should render")
            .size
            .height
            >= gpui::px(64.0)
    );
    assert_eq!(
        cx.debug_bounds("cell-detail-header")
            .expect("cell detail header should render")
            .size
            .height,
        gpui::px(crate::theme::RESULT_GRID_HEADER_HEIGHT)
    );
    // Plain text: no JSON mode toggle, no applicable conversion.
    assert!(cx.debug_bounds("cell-detail-value-mode").is_none());
    assert!(cx.debug_bounds("cell-detail-convert").is_none());
    assert!(cx.debug_bounds("cell-detail-json-value").is_none());
    assert!(cx.debug_bounds("cell-detail-footer").is_none());
    assert!(cx.debug_bounds("cell-detail-comment").is_none());
    assert!(cx.debug_bounds("cell-detail-value-label").is_some());
    assert!(
        cx.debug_bounds("cell-detail-copy-value")
            .expect("copy value control should render")
            .size
            .height
            <= gpui::px(28.0)
    );
    assert!(
        cx.debug_bounds("cell-detail-row-value")
            .expect("row value should render")
            .size
            .width
            > gpui::px(20.0)
    );
    assert!(app.read_with(cx, |this, _| {
        this.selected_cell_detail(this.active_id())
            .expect("selected cell detail")
            .editable
    }));

    let initial_edit = cx
        .debug_bounds("cell-detail-edit")
        .expect("edit control should render");
    cx.simulate_click(initial_edit.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(app.read_with(cx, |this, _| this.av().detail_ui.edit.is_some()));
    app.update(cx, |this, cx| this.cancel_cell_detail_editor(this.active_id(), cx));
    assert!(cx.debug_bounds("cell-detail-edit").is_some());
    assert!(cx.debug_bounds("cell-detail-value-tab").is_none());
    assert!(cx.debug_bounds("cell-detail-rail-row").is_some());
    assert!(cx.debug_bounds("cell-detail-rail-json").is_none());
    let row_trigger = cx
        .debug_bounds("cell-detail-rail-row")
        .expect("row detail rail button should render");
    cx.simulate_click(row_trigger.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("cell-detail-row-panel").is_some());
    assert!(cx.debug_bounds("cell-detail-row-tab").is_some());
    assert!(cx.debug_bounds("cell-detail-details-tab").is_some());
    let tab_state = |cx: &mut gpui::VisualTestContext| {
        app.read_with(cx, |this, _| {
            let state = &this.workspace.runtime(tab_id).expect("query runtime").cell_detail;
            (state.open, state.tab, state.opened_tabs.clone())
        })
    };
    assert_eq!(
        tab_state(cx),
        (
            true,
            CellDetailTab::Row,
            vec![CellDetailTab::Details, CellDetailTab::Row]
        )
    );
    app.read_with(cx, |this, _| {
        let row = this
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.cell_detail.row.as_ref())
            .expect("row detail should be loaded");
        assert_eq!(row.row_number, 1);
        assert_eq!(row.fields[0].value, "12");
        assert!(row.json.contains("\"id\": 12"));
        assert!(row.json.contains("\"name\": \"Btest\""));
    });
    assert!(cx.debug_bounds("cell-detail-tree-row").is_some());
    let column_trigger = cx
        .debug_bounds("cell-detail-rail-column")
        .expect("column rail button should render");
    cx.simulate_click(column_trigger.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("cell-detail-column-panel").is_some());
    assert!(cx.debug_bounds("cell-detail-column-tab").is_some());
    // Three tabs at the default width keep their labels (no icon-only tabs).
    for selector in [
        "cell-detail-details-tab-body",
        "cell-detail-row-tab-body",
        "cell-detail-column-tab-body",
    ] {
        assert!(
            cx.debug_bounds(selector).expect("tab body").size.width > gpui::px(48.0),
            "{selector} should be labeled"
        );
    }
    // Clicking the active rail button closes only that tab and activates its neighbor.
    cx.simulate_click(column_trigger.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(
        tab_state(cx),
        (
            true,
            CellDetailTab::Row,
            vec![CellDetailTab::Details, CellDetailTab::Row]
        )
    );
    assert!(cx.debug_bounds("cell-detail-column-tab").is_none());
    assert!(cx.debug_bounds("cell-detail-panel").is_some());
    // Closing a tab through its x activates the neighbor.
    let row_close = cx
        .debug_bounds("cell-detail-row-tab-close")
        .expect("active tab close button should render");
    cx.simulate_click(row_close.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(
        tab_state(cx),
        (true, CellDetailTab::Details, vec![CellDetailTab::Details])
    );
    // Closing the last tab closes the panel.
    let details_close = cx
        .debug_bounds("cell-detail-details-tab-close")
        .expect("active tab close button should render");
    cx.simulate_click(details_close.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(!tab_state(cx).0);
    assert!(cx.debug_bounds("cell-detail-panel").is_none());
    let details_trigger = cx
        .debug_bounds("cell-detail-rail-details")
        .expect("details rail button should render");
    cx.simulate_click(details_trigger.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        assert_eq!(
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .cell_detail
                .tab,
            CellDetailTab::Details
        );
        assert!(
            this.selected_cell_detail(this.active_id())
                .expect("selected detail")
                .editable
        );
    });
    let edit_button = cx
        .debug_bounds("cell-detail-edit")
        .expect("edit control should render");
    cx.simulate_click(edit_button.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let editor = cx.cx.read(|app_context| {
        app.read(app_context)
            .av().detail_ui
            .edit
            .as_ref()
            .expect("cell detail editor should open")
            .editor
            .clone()
    });
    app.update_in(cx, |_, window, cx| {
        editor.set_value("Grace", window, cx);
    });
    app.update(cx, |this, cx| this.commit_cell_detail_editor(this.active_id(), cx));
    let pending = cx.cx.read(|app_context| {
        let this = app.read(app_context);
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        (
            runtime
                .pending_cell_text(CellPosition { row: 0, column: 1 })
                .map(|text| text.to_string()),
            runtime.pending_edits.len(),
            runtime.cell_detail.tab,
        )
    });
    assert_eq!(pending.0.as_deref(), Some("Grace"));
    assert_eq!(pending.1, 1);
    assert_eq!(pending.2, CellDetailTab::Details);

    app.update_in(cx, |this, window, cx| {
        this.set_cell_detail_null(this.active_id(), window, cx);
    });
    app.read_with(cx, |this, _| {
        let runtime = this.workspace.runtime(tab_id).expect("query runtime");
        assert_eq!(
            runtime
                .pending_cell_text(CellPosition { row: 0, column: 1 })
                .map(|text| text.as_ref()),
            Some("NULL")
        );
        assert!(matches!(
            runtime.pending_edits[0].request.value,
            MutationValue::Null
        ));
    });

    assert!(
        cx.debug_bounds("cell-detail-placement").is_some(),
        "placement dropdown should render"
    );
    app.update(cx, |this, cx| {
        this.set_cell_detail_placement(this.active_id(), CellDetailPlacement::Bottom, cx)
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        assert_eq!(
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .cell_detail
                .placement,
            CellDetailPlacement::Bottom
        );
    });

    let close = cx
        .debug_bounds("cell-detail-close")
        .expect("close control should render");
    cx.simulate_click(close.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        assert!(
            !this
                .workspace
                .runtime(tab_id)
                .expect("query runtime")
                .cell_detail
                .open
        );
    });
    assert!(cx.debug_bounds("cell-detail-panel").is_none());

}

#[gpui::test]
fn row_tab_tree_expands_json_columns_on_click_and_resets_on_row_change(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        crate::theme::init(app);
    });
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
    let tab_id = app.update(cx, |this, _| {
        let tab_id = this.workspace.open_ran_query_tab("Query");
        let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
            arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
            arrow_schema::Field::new("payload", arrow_schema::DataType::Utf8, true),
        ]));
        let batch = arrow_array::RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![1, 2]))
                    as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::StringArray::from(vec![
                    Some(r#"{"owner":{"name":"Ada"},"tags":["a","b"]}"#),
                    Some(r#"{"other":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#),
                ])) as arrow_array::ArrayRef,
            ],
        )
        .expect("valid test batch");
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        runtime.structure_columns = vec![
            crate::catalog::CatalogColumn {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
            crate::catalog::CatalogColumn {
                name: "payload".into(),
                data_type: "jsonb".into(),
                nullable: true,
                primary_key: false,
                ..Default::default()
            },
        ];
        this.reset_active_view();
        this.select_grid_cell(this.active_id(), CellPosition { row: 0, column: 1 });
        tab_id
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.update(cx, |this, cx| {
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Row, cx);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let tree = |cx: &mut gpui::VisualTestContext| {
        app.read_with(cx, |this, _| {
            let state = &this.workspace.runtime(tab_id).expect("query runtime").cell_detail;
            state
                .row_tree
                .iter()
                .map(|row| (row.key.clone(), row.depth, row.value.clone()))
                .collect::<Vec<_>>()
        })
    };
    // Containers start collapsed with a summary; SQL type names are kept at the top level.
    assert_eq!(
        tree(cx),
        vec![
            ("id".to_string(), 0, "1".to_string()),
            ("payload".to_string(), 0, "{2 fields}".to_string()),
        ]
    );
    app.read_with(cx, |this, _| {
        let state = &this.workspace.runtime(tab_id).expect("query runtime").cell_detail;
        assert_eq!(state.row_tree[0].type_name, "integer");
        assert_eq!(state.row_tree[1].type_name, "jsonb");
    });

    // Clicking a container row expands it through the real listener.
    // Rows share one debug selector, so the bounds are those of the last
    // rendered row, which is the `payload` container.
    let payload_row = cx.debug_bounds("cell-detail-tree-row").expect("tree row should render");
    cx.simulate_click(payload_row.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(
        tree(cx)
            .iter()
            .map(|(key, depth, _)| (key.as_str(), *depth))
            .collect::<Vec<_>>(),
        vec![("id", 0), ("payload", 0), ("owner", 1), ("tags", 1)]
    );

    // Selecting another row resets the expansion state.
    app.update(cx, |this, _| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        let state = &this.workspace.runtime(tab_id).expect("query runtime").cell_detail;
        assert!(state.row_expanded.is_empty());
        assert_eq!(state.row_tree.len(), 2);
        assert_eq!(state.row_tree[1].value, "{1 fields}");
    });

    // The Row tab's JSON view is the official read-only, tree-sitter highlighted code editor.
    assert!(cx.debug_bounds("cell-detail-row-view-tree").is_some());
    let json_toggle = cx
        .debug_bounds("cell-detail-row-view-json")
        .expect("JSON view toggle");
    cx.simulate_click(json_toggle.center(), Default::default());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("cell-detail-json-panel").is_some());
    assert!(cx.debug_bounds("cell-detail-tree-row").is_none());
    app.read_with(cx, |this, cx| {
        let state = this
            .av().json_views
            .cell_json
            .as_ref()
            .expect("json tab should own a code editor")
            .state
            .read(cx);
        let row = this
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.cell_detail.row.as_ref())
            .expect("row detail");
        assert_eq!(state.language_name().as_ref(), "json");
        assert!(!state.is_editable());
        assert_eq!(state.value().as_ref(), row.json.as_ref());
    });
}

#[gpui::test]
fn value_section_highlights_json_offers_conversions_and_copies_conditions(
    cx: &mut TestAppContext,
) {
    cx.update(|app| {
        gpui_component::init(app);
        crate::theme::init(app);
    });
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
    let tab_id = app.update(cx, |this, _| {
        let tab_id = this.workspace.open_ran_query_tab("Query");
        let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
            arrow_schema::Field::new("payload", arrow_schema::DataType::Utf8, true),
            arrow_schema::Field::new("ts", arrow_schema::DataType::Utf8, true),
            arrow_schema::Field::new("qty", arrow_schema::DataType::Int64, true),
        ]));
        let batch = arrow_array::RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(arrow_array::StringArray::from(vec![
                    Some(r#"{"a":{"b":[1,2]},"ok":true}"#),
                    Some("plain"),
                    Some("x"),
                ])) as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::StringArray::from(vec![
                    Some("1752033379"),
                    Some("1752033379"),
                    Some("1752033379"),
                ])) as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![10, 20, 5]))
                    as arrow_array::ArrayRef,
            ],
        )
        .expect("valid test batch");
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        runtime.structure_columns = ["payload", "ts", "qty"]
            .iter()
            .zip(["jsonb", "text", "integer"])
            .map(|(name, data_type)| crate::catalog::CatalogColumn {
                name: (*name).into(),
                data_type: data_type.into(),
                nullable: true,
                ..Default::default()
            })
            .collect();
        this.reset_active_view();
        this.select_grid_cell(this.active_id(), CellPosition { row: 0, column: 0 });
        tab_id
    });
    cx.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(800.0)));
    app.update(cx, |this, cx| this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    // JSON-like value: highlighted read-only code editor plus the mode toggle.
    assert!(cx.debug_bounds("cell-detail-json-value").is_some());
    assert!(cx.debug_bounds("cell-detail-value-mode").is_some());
    let viewer_text = |cx: &mut gpui::VisualTestContext| {
        app.read_with(cx, |this, cx| {
            let state = this
                .av().json_views
                .cell_value
                .as_ref()
                .expect("value viewer")
                .state
                .read(cx);
            assert_eq!(state.language_name().as_ref(), "json");
            assert!(!state.is_editable());
            state.value().to_string()
        })
    };
    assert_eq!(
        viewer_text(cx),
        "{\n  \"a\": {\n    \"b\": [\n      1,\n      2\n    ]\n  },\n  \"ok\": true\n}"
    );
    app.update(cx, |this, cx| {
        this.av_mut().detail_ui.value_mode = crate::cell_value::ValueMode::Raw;
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(viewer_text(cx), r#"{"a":{"b":[1,2]},"ok":true}"#);

    // The SQL condition action copies a quoted condition for the selected cell.
    let condition = cx
        .debug_bounds("cell-detail-copy-sql-condition")
        .expect("copy SQL condition action");
    cx.simulate_click(condition.center(), Default::default());
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()).as_deref(),
        Some(r#""payload" = '{"a":{"b":[1,2]},"ok":true}'"#)
    );
    let column = cx
        .debug_bounds("cell-detail-copy-column")
        .expect("copy column action");
    cx.simulate_click(column.center(), Default::default());
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()).as_deref(),
        Some("payload")
    );

    // A unix timestamp offers conversions; JSON controls disappear for plain text.
    app.update(cx, |this, _| this.select_grid_cell(this.active_id(), CellPosition { row: 0, column: 1 }));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("cell-detail-json-value").is_none());
    assert!(cx.debug_bounds("cell-detail-value-mode").is_none());
    assert!(cx.debug_bounds("cell-detail-convert").is_some());

    // Footer stats: numeric cells of a multi-cell selection, computed once per selection.
    app.update(cx, |this, _| {
        let mut selection = this.av().grid_ui.selection.borrow_mut();
        selection.select(CellPosition { row: 0, column: 2 });
        selection.extend_to(CellPosition { row: 2, column: 2 });
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("result-status-selection-stats").is_some());
    app.read_with(cx, |this, _| {
        let (_, stats) = this.av().grid_ui.selection_stats.expect("stats are cached");
        assert_eq!(stats.label(), "SUM: 35  AVG: ≈11.666667  MIN: 5  MAX: 20  3 cells 3 rows");
    });
    let cached = app.read_with(cx, |this, _| this.av().grid_ui.selection_stats.map(|(key, _)| key));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(
        app.read_with(cx, |this, _| this.av().grid_ui.selection_stats.map(|(key, _)| key)),
        cached
    );
    let _ = tab_id;
}

#[gpui::test]
fn mongo_document_json_view_uses_the_highlighted_code_editor(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.runtime_mut(tab_id).unwrap().mongo.view =
            crate::mongo_result::MongoResultView::Document;
        this.select_mongo_document(tab_id, 1, cx);
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();

    assert!(visual.debug_bounds("mongo-json-editor").is_some());
    app.read_with(visual, |this, cx| {
        let state = this
            .av().json_views
            .mongo_doc
            .as_ref()
            .expect("document JSON view owns a code editor")
            .state
            .read(cx);
        assert_eq!(state.language_name().as_ref(), "json");
        assert!(!state.is_editable());
        assert!(state.value().contains("\"name\": \"mongo-customer-001\""));
    });
}

#[gpui::test]
fn editing_a_json_value_uses_the_code_editor_and_skips_unchanged_commits(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 7 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
    });
    app.update_in(visual, |this, window, cx| this.open_cell_detail_editor(this.active_id(), window, cx));
    app.read_with(visual, |this, cx| {
        let edit = this.av().detail_ui.edit.as_ref().expect("nested documents are editable");
        assert!(matches!(edit.editor, super::results::detail::CellDetailEditor::Code(_)));
        // Seeded pretty-printed; the stored value stays the base for staged edits.
        assert!(edit.seed_text.contains("\n  \""));
        assert_eq!(
            edit.original_text.as_ref(),
            this.selected_cell_detail(this.active_id()).unwrap().value.as_ref()
        );
        assert_eq!(edit.editor.value(cx), edit.seed_text);
    });
    // Committing the untouched pretty text is a no-op (nothing staged).
    app.update(visual, |this, cx| this.commit_cell_detail_editor(this.active_id(), cx));
    app.read_with(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        assert!(this.av().detail_ui.edit.is_none());
        assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.mutations.len(), 0);
    });
}

#[gpui::test]
fn mongo_json_viewer_rebuilds_text_only_when_its_inputs_change(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let tab_id = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.runtime_mut(tab_id).unwrap().mongo.view =
            crate::mongo_result::MongoResultView::Document;
        this.select_mongo_document(tab_id, 1, cx);
        tab_id
    });
    let render = |visual: &mut gpui::VisualTestContext| {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    };
    let builds = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.av().json_views.text_builds)
    };
    let text = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| {
            this.av().json_views.mongo_doc.as_ref().unwrap().state.read(cx).value().to_string()
        })
    };
    render(visual);
    render(visual);
    let initial = builds(visual);
    assert!(initial >= 1);
    assert!(text(visual).contains("\"name\": \"mongo-customer-001\""));

    // Unchanged state: any number of frames never rebuilds the text.
    for _ in 0..5 {
        render(visual);
        app.update(visual, |_, cx| cx.notify());
        render(visual);
    }
    assert_eq!(builds(visual), initial);

    // Staging a mutation rebuilds exactly once, with the update applied.
    app.update(visual, |this, _| {
        let history = &mut this.workspace.runtime_mut(tab_id).unwrap().mongo.mutations;
        history.stage(crate::mongo_result::PendingMongoMutation::Update {
            request: engine_driver_api::MutationRequest {
                operation_id: engine_driver_api::OperationId(1),
                kind: "update".into(),
                database: Some("app".into()),
                object: Some("users".into()),
                identity: serde_json::json!({}),
                payload: serde_json::json!({"$set": {"name": "Edited"}}),
            },
            row: 1,
            field: "name".into(),
            value: serde_json::json!("Edited"),
            display_text: "Edited".into(),
        });
    });
    render(visual);
    render(visual);
    assert_eq!(builds(visual), initial + 1);
    assert!(text(visual).contains("\"name\": \"Edited\""));

    // Undo and redo each rebuild once and are reflected in the text.
    app.update(visual, |this, _| {
        this.workspace.runtime_mut(tab_id).unwrap().mongo.mutations.undo();
    });
    render(visual);
    render(visual);
    assert_eq!(builds(visual), initial + 2);
    assert!(text(visual).contains("\"name\": \"mongo-customer-001\""));
    app.update(visual, |this, _| {
        this.workspace.runtime_mut(tab_id).unwrap().mongo.mutations.redo();
    });
    render(visual);
    assert_eq!(builds(visual), initial + 3);
    assert!(text(visual).contains("\"name\": \"Edited\""));
}

#[gpui::test]
fn footer_stats_follow_the_selected_display_cells(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let select = |visual: &mut gpui::VisualTestContext,
                  start: (usize, usize),
                  end: (usize, usize)| {
        app.update(visual, |this, _| {
            let mut selection = this.av().grid_ui.selection.borrow_mut();
            selection.select(CellPosition { row: start.0, column: start.1 });
            selection.extend_to(CellPosition { row: end.0, column: end.1 });
        });
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        app.read_with(visual, |this, _| {
            let range = this.av().grid_ui.selection.borrow().range.unwrap().normalized();
            let (_, stats) = this.av().grid_ui.selection_stats.expect("stats");
            (
                ((range.start.row, range.start.column), (range.end.row, range.end.column)),
                stats.label(),
            )
        })
    };

    // Column 0 is `id` (1-based ids), rows 2..=7 are display rows 1..=6.
    let (range, label) = select(visual, (1, 0), (6, 0));
    assert_eq!(range, ((1, 0), (6, 0)));
    assert_eq!(label, "SUM: 27  AVG: 4.5  MIN: 2  MAX: 7  6 cells 6 rows");

    // id, name, status: 18 cells, only the id cells are numeric.
    let (range, label) = select(visual, (1, 0), (6, 2));
    assert_eq!(range, ((1, 0), (6, 2)));
    assert_eq!(label, "SUM: 27  AVG: 4.5  MIN: 2  MAX: 7  18 cells 6 rows");

    // Text-only selections hide SUM/AVG and keep the counts.
    let (_, label) = select(visual, (1, 1), (2, 1));
    assert_eq!(label, "2 cells 2 rows");

    // A fresh selection starts a new range from its own anchor.
    let (range, label) = select(visual, (0, 5), (6, 5));
    assert_eq!(range, ((0, 5), (6, 5)));
    assert_eq!(label, "SUM: ≈2.1  AVG: ≈0.3  MIN: ≈0  MAX: ≈0.6  7 cells 7 rows");
}

#[test]
fn document_list_labels_use_plain_scalars() {
    use super::results::mongo::mongo_document_preview;
    let hex = "000000000000000000000001";
    assert_eq!(mongo_document_preview(&format!("ObjectId({hex})")), hex);
    assert_eq!(mongo_document_preview(hex), hex);
    assert_eq!(mongo_document_preview(&format!(r#"{{"$oid":"{hex}"}}"#)), hex);
    assert_eq!(mongo_document_preview(r#"{"$numberLong":"42"}"#), "42");
    assert_eq!(mongo_document_preview("customer-7"), "customer-7");
    // Not a scalar wrapper: kept verbatim (and capped to one short line).
    assert_eq!(mongo_document_preview(r#"{"a":1}"#), r#"{"a":1}"#);
    assert!(mongo_document_preview(&"x".repeat(80)).ends_with("..."));
}

#[gpui::test]
fn mongo_json_views_default_to_shell_syntax_and_rebuild_once_per_mode_change(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let tab_id = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 7 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Row, cx);
        this.set_cell_detail_row_view(this.active_id(), crate::cell_detail::RowView::Json, cx);
        this.workspace.runtime_mut(tab_id).unwrap().mongo.view =
            crate::mongo_result::MongoResultView::Document;
        this.select_mongo_document(tab_id, 1, cx);
        tab_id
    });
    let render = |visual: &mut gpui::VisualTestContext| {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    };
    render(visual);
    render(visual);
    let texts = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| {
            (
                this.av().json_views.cell_json.as_ref().map(|v| v.state.read(cx).value().to_string()),
                this.av().json_views.mongo_doc.as_ref().map(|v| v.state.read(cx).value().to_string()),
                this.av().json_views.text_builds,
            )
        })
    };
    let (tab_json, doc_json, builds) = texts(visual);
    for text in [tab_json.unwrap(), doc_json.unwrap()] {
        // Every type is visible in the text: an Int64 is `Long('1')`, an ObjectId
        // `ObjectId('…')`, a Date `ISODate('…')`, never a bare number or string.
        assert!(text.contains("\"count\": Long('1'),"), "{text}");
        assert!(text.contains("\"_id\": ObjectId('000000000000000000000002')"), "{text}");
        assert!(text.contains("\"createdAt\": ISODate('2024-09-02T"), "{text}");
        assert!(!text.contains("$numberLong") && !text.contains("$date"), "{text}");
    }
    assert!(visual.debug_bounds("mongo-json-mode").is_some());

    // Switching either view to canonical rebuilds exactly that view once.
    app.update(visual, |this, _| this.av_mut().mongo.document_json_mode = crate::cell_value::EjsonMode::Canonical);
    render(visual);
    render(visual);
    let (_, doc_json, after) = texts(visual);
    assert_eq!(after, builds + 1);
    assert!(doc_json.unwrap().contains("$numberLong"));
    app.update(visual, |this, _| {
        this.workspace.runtime_mut(tab_id).unwrap().cell_detail.row_json_mode =
            crate::cell_value::EjsonMode::Canonical;
    });
    render(visual);
    render(visual);
    let (tab_json, _, after2) = texts(visual);
    assert_eq!(after2, after + 1);
    assert!(tab_json.unwrap().contains("$numberLong"));
}

#[gpui::test]
fn sql_results_have_no_legacy_blocks_and_insert_lives_in_the_footer(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    app.update(visual, |this, _| this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 }));
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Cell-detail panel closed: the old "Row Detail" / "Structure" blocks are gone.
    assert!(visual.debug_bounds("cell-detail-panel").is_none());
    assert!(visual.debug_bounds("insert-row").is_none());
    let add = visual.debug_bounds("sql-add-row").expect("footer insert button");
    visual.simulate_click(add.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // The same staged-insert dialog opens (a JSON editor dialog).
    assert!(visual.update(|window, cx| window.has_active_dialog(cx)));
}

#[gpui::test]
fn compact_tabs_activate_on_icon_click_and_never_close_by_accident(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let tab_id = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
        for tab in [CellDetailTab::Details, CellDetailTab::Row, CellDetailTab::Column] {
            this.open_cell_detail_tab(this.active_id(), tab, cx);
        }
        // Narrow panel: tabs collapse to icon-only.
        this.workspace.runtime_mut(tab_id).unwrap().cell_detail.right_width = 260.0;
        tab_id
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();

    let details = visual
        .debug_bounds("cell-detail-details-tab-body")
        .expect("compact tab body");
    assert!(details.size.width >= gpui::px(28.0), "icon click target is at least 28px");
    assert!(details.size.width < gpui::px(48.0), "narrow panel renders icon-only tabs");
    visual.simulate_click(details.center(), Default::default());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    app.read_with(visual, |this, _| {
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert_eq!(state.tab, CellDetailTab::Details);
        assert_eq!(state.opened_tabs.len(), 3, "clicking a compact tab must not close it");
    });

    // Close Others keeps the chosen tab only.
    app.update(visual, |this, cx| this.close_other_cell_detail_tabs(this.active_id(), CellDetailTab::Row, cx));
    app.read_with(visual, |this, _| {
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert_eq!(state.opened_tabs, vec![CellDetailTab::Row]);
        assert_eq!(state.tab, CellDetailTab::Row);
    });
}

#[gpui::test]
fn closing_the_detail_panel_on_another_tab_keeps_an_edit_in_progress(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let tab_a = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
        tab_id
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Real click on the panel's edit control starts an edit on tab A.
    let edit = visual.debug_bounds("cell-detail-edit").expect("edit control");
    visual.simulate_click(edit.center(), Default::default());
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    app.read_with(visual, |this, _| {
        assert!(this.view(tab_a).detail_ui.edit.is_some());
    });

    // Another tab with its own open panel.
    let tab_b = app.update(visual, |this, _| {
        let id = this
            .workspace
            .open_query_tab_with_metadata("B".to_string(), TabMetadata::default());
        let state = &mut this.workspace.runtime_mut(id).unwrap().cell_detail;
        state.open_tab(CellDetailTab::Details);
        state.open_tab(CellDetailTab::Row);
        this.view_mut(tab_a).detail_ui.error = Some("edit on A failed".into());
        id
    });
    assert_ne!(tab_a, tab_b);

    // Close Others / close tab / close panel on B: A's edit and its error live on.
    app.update(visual, |this, cx| this.close_other_cell_detail_tabs(tab_b, CellDetailTab::Row, cx));
    app.read_with(visual, |this, _| {
        assert!(this.view(tab_a).detail_ui.edit.is_some());
        assert!(this.view(tab_a).detail_ui.error.is_some(), "error belongs to the surviving edit");
    });
    app.update(visual, |this, cx| this.close_cell_detail_tab(tab_b, CellDetailTab::Details, cx));
    app.read_with(visual, |this, _| {
        assert!(this.view(tab_a).detail_ui.edit.is_some());
    });
    app.update(visual, |this, cx| this.close_cell_detail_panel(tab_b, cx));
    app.read_with(visual, |this, _| {
        assert!(
            this.view(tab_a).detail_ui.edit.is_some(),
            "closing B's panel must not discard A's unsaved edit"
        );
    });

    // Closing A's own panel does discard it (and the error with it).
    app.update(visual, |this, cx| this.close_cell_detail_panel(tab_a, cx));
    app.read_with(visual, |this, _| {
        assert!(this.view(tab_a).detail_ui.edit.is_none());
        assert!(this.view(tab_a).detail_ui.error.is_none());
    });
}

#[test]
fn convert_preview_is_scoped_to_its_tab_cell_and_offered_kinds() {
    use crate::cell_value::ConvertKind::{HexToText, UnixSecondsToIso};
    let position = CellPosition { row: 1, column: 1 };
    let chosen = Some((7, position, UnixSecondsToIso));
    let offered = [UnixSecondsToIso, HexToText];
    let scoped = super::results::detail_view::scoped_convert;
    assert_eq!(scoped(chosen, 7, position, &offered), Some(UnixSecondsToIso));
    assert_eq!(scoped(chosen, 8, position, &offered), None, "same cell on another tab");
    assert_eq!(scoped(chosen, 7, CellPosition { row: 2, column: 1 }, &offered), None);
    assert_eq!(scoped(chosen, 7, position, &[HexToText]), None, "kind no longer offered");
    assert_eq!(scoped(None, 7, position, &offered), None);
}

#[gpui::test]
fn popup_placement_floats_over_a_full_size_grid_and_follows_the_selection(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let tab_id = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
        tab_id
    });
    let render = |visual: &mut gpui::VisualTestContext| {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    };
    render(visual);
    let docked_grid = visual.debug_bounds("grid-scroll-area").expect("grid").size.width;
    assert!(visual.debug_bounds("cell-detail-popup").is_none());

    // Right -> Bottom -> Popup through the real placement API.
    app.update(visual, |this, cx| {
        this.set_cell_detail_placement(this.active_id(), CellDetailPlacement::Popup, cx)
    });
    render(visual);
    let popup = visual.debug_bounds("cell-detail-popup").expect("popup should render");
    let grid = visual.debug_bounds("grid-scroll-area").expect("grid");
    assert!(grid.size.width > docked_grid, "grid takes the full width in popup mode");
    assert_eq!(popup.size.width, gpui::px(520.0));
    assert_eq!(popup.size.height, gpui::px(420.0));
    assert!(popup.right() <= gpui::px(1100.0) && popup.bottom() <= gpui::px(850.0));
    assert!(visual.debug_bounds("cell-detail-tab-strip").is_some());
    assert!(visual.debug_bounds("cell-detail-drag-handle").is_some());

    // Non-modal: the grid stays interactive and the popup follows the selection.
    let value_of = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            this.selected_cell_detail(this.active_id()).map(|detail| detail.row_number)
        })
    };
    assert_eq!(value_of(visual), Some(2));
    app.update(visual, |this, _| this.select_grid_cell(this.active_id(), CellPosition { row: 3, column: 1 }));
    render(visual);
    assert_eq!(value_of(visual), Some(4));
    assert!(visual.debug_bounds("cell-detail-popup").is_some());
    assert!(visual.debug_bounds("cell-detail-row-value").is_some());

    // Dragging the title bar moves the popup (through the drag overlay).
    let handle = visual.debug_bounds("cell-detail-drag-handle").unwrap();
    let before = visual.debug_bounds("cell-detail-popup").unwrap();
    visual.simulate_mouse_down(handle.center(), gpui::MouseButton::Left, Default::default());
    render(visual);
    assert!(visual.debug_bounds("cell-detail-popup-drag-overlay").is_some());
    let target = gpui::point(handle.center().x - gpui::px(120.0), handle.center().y - gpui::px(8.0));
    visual.simulate_mouse_move(target, None, Default::default());
    visual.simulate_mouse_up(target, gpui::MouseButton::Left, Default::default());
    render(visual);
    let after = visual.debug_bounds("cell-detail-popup").unwrap();
    let results = visual.debug_bounds("results-with-columns-panel").unwrap();
    assert_eq!(after.origin.x, before.origin.x - gpui::px(120.0), "{before:?} {after:?} {results:?}");
    assert_eq!(after.origin.y, before.origin.y - gpui::px(8.0));
    assert!(visual.debug_bounds("cell-detail-popup-drag-overlay").is_none());

    // Placement and geometry persist per result tab; closing the popup closes the panel.
    app.read_with(visual, |this, _| {
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert_eq!(state.placement, CellDetailPlacement::Popup);
        assert!(state.popup.position.is_some());
    });
    let close = visual.debug_bounds("cell-detail-close").expect("close");
    visual.simulate_click(close.center(), Default::default());
    render(visual);
    assert!(visual.debug_bounds("cell-detail-popup").is_none());
    app.read_with(visual, |this, _| {
        let state = &this.workspace.runtime(tab_id).unwrap().cell_detail;
        assert!(!state.open);
        assert_eq!(state.placement, CellDetailPlacement::Popup);
    });
}

#[gpui::test]
fn popup_stays_inside_the_results_area_even_when_it_must_shrink(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(620.0)));
    let render = |visual: &mut gpui::VisualTestContext| {
        for _ in 0..3 {
            visual.update(|window, _| window.refresh());
            visual.run_until_parked();
        }
    };
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
        this.set_cell_detail_placement(this.active_id(), CellDetailPlacement::Popup, cx);
    });
    render(visual);

    let results = visual
        .debug_bounds("results-with-columns-panel")
        .expect("results area");
    let popup = visual.debug_bounds("cell-detail-popup").expect("popup");
    let slack = gpui::px(1.0);
    let footer = visual.debug_bounds("result-action-footer").expect("footer");
    let right_rail = visual.debug_bounds("cell-detail-rail").expect("right rail");
    let left_rail = visual.debug_bounds("columns-rail").expect("left rail");
    // Grid area only: below the results top (SQL editor stays visible), above
    // the footer's top edge and between the two rails, which stay uncovered.
    assert!(popup.top() >= results.top() - slack, "{popup:?} {results:?}");
    assert!(popup.bottom() <= footer.top() + slack, "{popup:?} footer {footer:?}");
    assert!(popup.left() >= left_rail.right() - slack, "{popup:?}");
    assert!(popup.right() <= right_rail.left() + slack, "{popup:?}");

    // Dragging far up/left pins the popup to the results area edges, not the window's.
    let handle = visual.debug_bounds("cell-detail-drag-handle").unwrap();
    visual.simulate_mouse_down(handle.center(), gpui::MouseButton::Left, Default::default());
    render(visual);
    let target = gpui::point(gpui::px(2.0), gpui::px(2.0));
    visual.simulate_mouse_move(target, None, Default::default());
    visual.simulate_mouse_up(target, gpui::MouseButton::Left, Default::default());
    render(visual);
    let popup = visual.debug_bounds("cell-detail-popup").unwrap();
    assert!((popup.top() - results.top()).abs() <= slack, "{popup:?} {results:?}");
    assert!((popup.left() - left_rail.right()).abs() <= slack, "{popup:?} {left_rail:?}");
}


fn columns_state(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> (crate::columns_panel::Presentation, Option<usize>, usize, usize) {
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let panel = &this.workspace.runtime(tab.id).unwrap().columns_panel;
        (panel.presentation, panel.selected, panel.items.len(), panel.filtered.len())
    })
}

fn settle(visual: &mut gpui::VisualTestContext) {
    for _ in 0..3 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
}

#[gpui::test]
fn left_rail_presentations_are_exclusive_and_columns_replaces_the_grid(cx: &mut TestAppContext) {
    use crate::columns_panel::Presentation;
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);

    // Grid presentation: rail with two stacked 28px buttons, grid shown, no columns view.
    let rail = visual.debug_bounds("columns-rail").expect("rail");
    assert_eq!(rail.size.width, gpui::px(36.0));
    let grid_button = visual.debug_bounds("columns-rail-grid-button").expect("grid button");
    let columns_button = visual.debug_bounds("properties-rail-button").expect("columns button");
    assert!(grid_button.size.width >= gpui::px(28.0) && columns_button.size.height >= gpui::px(28.0));
    assert!(columns_button.top() > grid_button.bottom());
    let grid_area = visual.debug_bounds("grid-scroll-area").expect("grid");
    assert!(visual.debug_bounds("columns-view").is_none());

    // Columns replaces the grid in the same area (no side panel).
    visual.simulate_click(columns_button.center(), Default::default());
    settle(visual);
    assert_eq!(columns_state(&app, visual).0, Presentation::Columns);
    assert!(visual.debug_bounds("grid-scroll-area").is_none(), "grid is not rendered");
    let view = visual.debug_bounds("columns-view").expect("columns view");
    assert!(view.left() >= rail.right() - gpui::px(1.0));
    assert!(view.size.width >= grid_area.size.width - gpui::px(1.0), "fills the grid area");
    // Tab strip sits in the grid-header-height row; the title + filter
    // toolbar is the Columns tab's own row below it.
    let strip = visual.debug_bounds("properties-tab-strip").expect("strip");
    assert_eq!(strip.size.height, gpui::px(crate::theme::RESULT_GRID_HEADER_HEIGHT));
    let header = visual.debug_bounds("columns-view-header").expect("header");
    assert!(header.top() >= strip.bottom() - gpui::px(1.0));
    assert!(visual.debug_bounds("columns-view-filter").is_some());

    // Grid switches back.
    let grid_button = visual.debug_bounds("columns-rail-grid-button").unwrap();
    visual.simulate_click(grid_button.center(), Default::default());
    settle(visual);
    assert_eq!(columns_state(&app, visual).0, Presentation::Grid);
    assert!(visual.debug_bounds("grid-scroll-area").is_some());
    assert!(visual.debug_bounds("columns-view").is_none());
}

#[gpui::test]
fn columns_view_filters_selects_and_syncs_the_right_panel_without_leaving_properties(
    cx: &mut TestAppContext,
) {
    use crate::columns_panel::Presentation;
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 2, column: 0 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
    });
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.show_columns_presentation(tab_id, cx)
    });
    settle(visual);
    let (_, selected, total, shown) = columns_state(&app, visual);
    assert_eq!(selected, Some(0), "starts at the grid's selected column");
    assert_eq!((total, shown), (8, 8));

    // SQL metadata: PK, Nullable and Default columns appear when known.
    assert!(visual.debug_bounds("columns-view-row").is_some());
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let panel = &this.workspace.runtime(tab.id).unwrap().columns_panel;
        assert!(panel.items[0].primary_key);
        assert!(panel.has_nullable);
    });

    // Filter through the real input.
    let input = app.read_with(visual, |this, _| {
        this.columns_filter_inputs.values().next().cloned().expect("filter input")
    });
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.set_value("STAT", window, cx));
    });
    settle(visual);
    assert_eq!(columns_state(&app, visual).3, 1, "only `status` matches");
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.set_value("", window, cx));
    });
    settle(visual);
    assert_eq!(columns_state(&app, visual).3, 8);

    // Selecting a row points the right panel's Column tab at it, stays in
    // Properties and leaves the grid selection exactly where it was.
    let grid_selection = app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range);
    app.update(visual, |this, cx| this.select_column_row(this.active_id(), 2, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        let state = this.tab_cell_detail(this.active_id()).unwrap();
        assert_eq!(state.tab, CellDetailTab::Column);
        assert_eq!(state.column.as_ref().unwrap().name, "status");
    });
    assert_eq!(columns_state(&app, visual).0, Presentation::Columns);
    assert_eq!(columns_state(&app, visual).1, Some(2));
    assert!(visual.debug_bounds("cell-detail-panel").is_some(), "right panel stays open");
    let unchanged = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range) == grid_selection
    };
    assert!(unchanged(visual), "grid selection untouched");

    // Real single click, keyboard, repeated quick click and a true double
    // click (click_count 2) all just select: no jump to the grid.
    let row = visual.debug_bounds("columns-view-row").unwrap();
    visual.simulate_click(row.center(), Default::default());
    settle(visual);
    let before = columns_state(&app, visual).1.unwrap();
    visual.simulate_keystrokes("down");
    settle(visual);
    assert_eq!(columns_state(&app, visual).1, Some(before + 1).filter(|c| *c < 8).or(Some(before)));
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(columns_state(&app, visual).0, Presentation::Columns, "Enter only selects");
    let row = visual.debug_bounds("columns-view-row").unwrap();
    let position = row.center();
    visual.simulate_click(position, Default::default());
    visual.simulate_click(position, Default::default());
    settle(visual);
    assert_eq!(columns_state(&app, visual).0, Presentation::Columns, "quick repeat click");
    visual.simulate_event(gpui::MouseDownEvent {
        position,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    visual.simulate_event(gpui::MouseUpEvent {
        position,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
    });
    settle(visual);
    assert_eq!(columns_state(&app, visual).0, Presentation::Columns, "double click");
    assert!(columns_state(&app, visual).1.is_some(), "the clicked row is selected");
    assert!(visual.debug_bounds("grid-scroll-area").is_none(), "grid is never shown");
    assert!(unchanged(visual), "grid selection still untouched");
}

#[gpui::test]
fn mongo_document_view_keeps_the_rail_and_can_show_columns(cx: &mut TestAppContext) {
    use crate::columns_panel::Presentation;
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let view = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            let tab = this.workspace.active_tab().unwrap();
            this.workspace.runtime(tab.id).unwrap().mongo.view
        })
    };
    settle(visual);
    app.update(visual, |this, cx| {
        this.set_mongo_view(this.active_id(), crate::mongo_result::MongoResultView::Document, cx)
    });
    settle(visual);
    assert!(visual.debug_bounds("columns-rail").is_some(), "rail renders in document view");
    assert!(visual.debug_bounds("grid-scroll-area").is_none());

    // Columns from the document view: Mongo shows Name | Type only.
    let columns_button = visual.debug_bounds("properties-rail-button").unwrap();
    visual.simulate_click(columns_button.center(), Default::default());
    settle(visual);
    assert_eq!(columns_state(&app, visual).0, Presentation::Columns);
    assert!(visual.debug_bounds("columns-view").is_some());
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let panel = &this.workspace.runtime(tab.id).unwrap().columns_panel;
        assert!(!panel.has_nullable && !panel.has_default && !panel.has_comment, "{:?}", &panel.items[..3]);
        assert_eq!(panel.items[0].name, "_id");
    });

    // Grid returns to the table grid (and leaves the document view).
    let grid_button = visual.debug_bounds("columns-rail-grid-button").unwrap();
    visual.simulate_click(grid_button.center(), Default::default());
    settle(visual);
    assert_eq!(view(visual), crate::mongo_result::MongoResultView::Table);
    assert_eq!(columns_state(&app, visual).0, Presentation::Grid);
    assert!(visual.debug_bounds("grid-scroll-area").is_some());
}

#[gpui::test]
fn narrow_right_panel_uses_its_rendered_width_for_compact_tabs(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
        for tab in [CellDetailTab::Details, CellDetailTab::Column, CellDetailTab::Row] {
            this.open_cell_detail_tab(this.active_id(), tab, cx);
        }
    });
    settle(visual);
    let labeled = visual.debug_bounds("cell-detail-details-tab-body").unwrap();
    assert!(labeled.size.width > gpui::px(48.0));

    // A narrow window squeezes the panel below the threshold although its
    // stored width (320) is unchanged.
    visual.simulate_resize(gpui::size(gpui::px(640.0), gpui::px(850.0)));
    settle(visual);
    let panel = visual.debug_bounds("cell-detail-panel").unwrap();
    app.read_with(visual, |this, _| {
        assert_eq!(this.tab_cell_detail(this.active_id()).unwrap().right_width, 320.0);
    });
    assert!(panel.size.width < gpui::px(300.0), "panel is squeezed: {:?}", panel.size.width);
    let compact = visual.debug_bounds("cell-detail-details-tab-body").unwrap();
    assert!(compact.size.width < gpui::px(48.0), "icon-only below the threshold");

    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    let restored = visual.debug_bounds("cell-detail-details-tab-body").unwrap();
    assert!(restored.size.width > gpui::px(48.0));
}

#[gpui::test]
fn properties_columns_table_is_content_sized_and_left_aligned(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.show_columns_presentation(tab_id, cx)
    });
    settle(visual);

    // Horizontal sub-tab strip in the grid-header-height row, Columns active,
    // with the content (toolbar + table) directly below it.
    let strip = visual.debug_bounds("properties-tab-strip").expect("tab strip");
    assert_eq!(strip.size.height, gpui::px(crate::theme::RESULT_GRID_HEADER_HEIGHT));
    let list = visual.debug_bounds("properties-tab-list").expect("sub-tab list");
    let tab = visual.debug_bounds("properties-tab-columns").expect("Columns sub-tab");
    assert!(list.size.width < strip.size.width, "content-sized strip, not a 160px column");
    assert_eq!(list.size.height, gpui::px(30.0), "secondary-tab strip is 30px");
    assert_eq!(list.bottom(), strip.bottom() - gpui::px(1.0), "sits on the strip's bottom line");
    assert_eq!(tab.size.height, gpui::px(30.0));
    // Underline: 2px accent bar flush with the bottom line, tab width minus padding.
    let underline = visual.debug_bounds("properties-tab-underline").expect("active underline");
    assert_eq!(underline.size.height, gpui::px(2.0));
    assert_eq!(underline.bottom(), list.bottom());
    assert_eq!(underline.size.width, tab.size.width - gpui::px(20.0));
    assert_eq!(underline.left(), tab.left() + gpui::px(10.0));
    let content = visual.debug_bounds("columns-content").expect("content");
    assert!(content.top() >= strip.bottom() - gpui::px(1.0), "content below the strip");
    assert_eq!(content.left(), strip.left(), "content spans the full width");

    // Toolbar: title then a compact fixed filter right after it (not far right).
    let filter = visual.debug_bounds("columns-view-filter").unwrap();
    assert_eq!(filter.size.width, gpui::px(220.0));
    assert!(
        filter.right() < content.left() + gpui::px(400.0),
        "filter sits next to the title, not at the far right: {filter:?} {content:?}"
    );

    // Rows are exactly as wide as the sum of the measured column widths and
    // narrower than the container; the rest of the area stays empty.
    let (total, columns) = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let panel = &this.workspace.runtime(tab.id).unwrap().columns_panel;
        (panel.widths.total(), panel.items.len())
    });
    assert_eq!(columns, 8);
    let row = visual.debug_bounds("columns-view-row").expect("row");
    let table_header = visual.debug_bounds("columns-table-header").expect("table header");
    assert!((f32::from(row.size.width) - total).abs() < 0.5);
    assert!((f32::from(table_header.size.width) - total).abs() < 0.5);
    assert!(row.size.width < content.size.width - gpui::px(100.0), "{row:?} in {content:?}");
    assert_eq!(row.left(), content.left(), "left-aligned");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let widths = this.workspace.runtime(tab.id).unwrap().columns_panel.widths;
        // SQL fixture: Name | Type | Nullable | Key shown, no Default/Comment.
        assert!(widths.nullable > 0.0 && widths.key > 0.0);
        assert_eq!((widths.default, widths.comment), (0.0, 0.0));
    });
}

/// Properties presentation with no grid selection: the right rail must still
/// open the panel (empty states), and selecting a column row fills it in.
fn right_panel_opens_without_selection(cx: &mut TestAppContext, scenario: &str, column: usize, name: &str) {
    use crate::columns_panel::Presentation;
    let (app, visual) = context_test_app(cx, scenario, "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    app.update(visual, |this, cx| {
        this.av().grid_ui.selection.borrow_mut().clear();
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.show_columns_presentation(tab_id, cx);
    });
    settle(visual);
    let no_selection = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range.is_none())
    };
    assert!(no_selection(&app, visual));
    assert!(visual.debug_bounds("cell-detail-panel").is_none());

    // Column rail button is enabled and opens the panel with an empty state.
    let column_button = visual.debug_bounds("cell-detail-rail-column").expect("column rail button");
    visual.simulate_click(column_button.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("cell-detail-panel").is_some(), "panel renders without a selection");
    assert!(visual.debug_bounds("cell-detail-column-panel").is_some(), "Column empty state");
    app.read_with(visual, |this, _| {
        let state = this.tab_cell_detail(this.active_id()).unwrap();
        assert!(state.open && state.tab == CellDetailTab::Column && state.column.is_none());
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(
            this.workspace.runtime(tab.id).unwrap().columns_panel.presentation,
            Presentation::Columns,
            "presentation unchanged"
        );
    });
    assert!(no_selection(&app, visual), "opening the panel does not select a cell");

    // Value and Row tabs show the cell empty state.
    let details = visual.debug_bounds("cell-detail-rail-details").expect("details rail button");
    visual.simulate_click(details.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("cell-detail-empty").is_some(), "Value empty state");
    let row_button = visual.debug_bounds("cell-detail-rail-row").expect("row rail button");
    visual.simulate_click(row_button.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("cell-detail-empty").is_some(), "Row empty state");
    assert!(no_selection(&app, visual));

    // Selecting a column row while the panel is open fills the Column tab in.
    app.update(visual, |this, cx| this.select_column_row(this.active_id(), column, cx));
    settle(visual);
    app.update(visual, |this, cx| this.open_cell_detail_tab(this.active_id(), CellDetailTab::Column, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        let state = this.tab_cell_detail(this.active_id()).unwrap();
        assert_eq!(state.tab, CellDetailTab::Column);
        assert_eq!(state.column.as_ref().expect("column shown").name, name);
    });
    assert!(no_selection(&app, visual), "grid selection still untouched");
    assert_eq!(columns_state(&app, visual).0, Presentation::Columns);

    // Panel closed + a column row selected: the Column rail opens on that column.
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.close_cell_detail_panel(tab_id, cx);
    });
    settle(visual);
    assert!(visual.debug_bounds("cell-detail-panel").is_none());
    let column_button = visual.debug_bounds("cell-detail-rail-column").unwrap();
    visual.simulate_click(column_button.center(), Default::default());
    settle(visual);
    app.read_with(visual, |this, _| {
        let column = this.tab_cell_detail(this.active_id()).unwrap().column.as_ref().expect("selected column row");
        assert_eq!(column.name, name);
    });
    assert!(no_selection(&app, visual));

    // Popup placement shows the same empty state.
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.close_cell_detail_panel(tab_id, cx);
        this.set_cell_detail_placement(this.active_id(), crate::cell_detail::CellDetailPlacement::Popup, cx);
    });
    settle(visual);
    let details = visual.debug_bounds("cell-detail-rail-details").unwrap();
    visual.simulate_click(details.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("cell-detail-panel").is_some(), "popup renders");
    assert!(visual.debug_bounds("cell-detail-empty").is_some(), "popup empty state");
}

#[gpui::test]
fn sql_right_panel_opens_from_the_rail_without_a_selection(cx: &mut TestAppContext) {
    right_panel_opens_without_selection(cx, "result-grid", 2, "status");
}

#[gpui::test]
fn mongo_right_panel_opens_from_the_rail_without_a_selection(cx: &mut TestAppContext) {
    right_panel_opens_without_selection(cx, "mongo-grid", 1, "name");
}

#[gpui::test]
fn scrolling_inside_the_popup_does_not_scroll_the_grid_behind_it(cx: &mut TestAppContext) {
    use gpui_component::scroll::ScrollbarHandle as _;
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    let render = |visual: &mut gpui::VisualTestContext| {
        for _ in 0..3 {
            visual.update(|window, _| window.refresh());
            visual.run_until_parked();
        }
    };
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Row, cx);
        this.set_cell_detail_placement(this.active_id(), CellDetailPlacement::Popup, cx);
    });
    render(visual);

    let popup = visual.debug_bounds("cell-detail-popup").expect("popup");
    let grid = visual.debug_bounds("grid-scroll-area").expect("grid");
    let probe = popup.center();
    assert!(
        grid.contains(&probe),
        "the probe point must lie over the grid so the event could leak: {popup:?} {grid:?}"
    );
    let before = app.update(visual, |this, _| {
        (this.av().grid_scroll.offset().y, this.av().grid_horizontal_scroll.offset().x)
    });
    for delta in [
        gpui::point(gpui::px(0.0), gpui::px(-120.0)),
        gpui::point(gpui::px(-120.0), gpui::px(0.0)),
    ] {
        visual.simulate_event(gpui::ScrollEvent {
            position: probe,
            delta: gpui::ScrollDelta::Pixels(delta),
            touch_phase: gpui::TouchPhase::Moved,
            modifiers: Default::default(),
            source: gpui::ScrollSource::Pointer,
        });
        visual.run_until_parked();
    }
    let after = app.update(visual, |this, _| {
        (this.av().grid_scroll.offset().y, this.av().grid_horizontal_scroll.offset().x)
    });
    assert_eq!(before, after, "wheel over the popup must not scroll the grid");

    // Outside the popup the grid still scrolls normally.
    let outside = gpui::point(grid.left() + gpui::px(40.0), grid.bottom() - gpui::px(20.0));
    assert!(!popup.contains(&outside), "{outside:?} must be outside {popup:?}");
    visual.simulate_event(gpui::ScrollEvent {
        position: outside,
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-120.0))),
        touch_phase: gpui::TouchPhase::Moved,
        modifiers: Default::default(),
        source: gpui::ScrollSource::Pointer,
    });
    visual.run_until_parked();
    let rows_y = app.update(visual, |this, _| this.av().grid_scroll.offset().y);
    assert!(f32::from(rows_y) < f32::from(before.0), "grid outside the popup must still scroll");
}

/// Batch whose fields carry the engine's native type metadata when given.
fn native_typed_batch(columns: &[(&str, arrow_schema::DataType, Option<&str>)]) -> arrow_array::RecordBatch {
    let fields: Vec<arrow_schema::Field> = columns
        .iter()
        .map(|(name, data_type, native)| {
            let field = arrow_schema::Field::new(*name, data_type.clone(), true);
            match native {
                Some(native) => field.with_metadata(std::collections::HashMap::from([(
                    crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_string(),
                    native.to_string(),
                )])),
                None => field,
            }
        })
        .collect();
    let arrays: Vec<arrow_array::ArrayRef> = columns
        .iter()
        .map(|(_, data_type, _)| match data_type {
            arrow_schema::DataType::Int64 => std::sync::Arc::new(arrow_array::Int64Array::from(
                vec![Some(7)],
            )) as arrow_array::ArrayRef,
            _ => std::sync::Arc::new(arrow_array::StringArray::from(vec![Some("x")]))
                as arrow_array::ArrayRef,
        })
        .collect();
    arrow_array::RecordBatch::try_new(std::sync::Arc::new(arrow_schema::Schema::new(fields)), arrays)
        .expect("valid typed batch")
}

fn catalog_column(name: &str, data_type: &str) -> crate::catalog::CatalogColumn {
    crate::catalog::CatalogColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable: true,
        ..Default::default()
    }
}

fn column_type_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        crate::theme::init(app);
    });
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
    (app, cx)
}

#[gpui::test]
fn table_backed_result_shows_one_type_in_header_properties_and_cell_detail(
    cx: &mut TestAppContext,
) {
    use arrow_schema::DataType::{Int64, Utf8};
    let (app, cx) = column_type_app(cx);
    // Postgres-style table: catalog types carry typmods; the engine metadata
    // carries base types; `extra` is not in the catalog; `plain` has neither.
    let expected = [
        ("id", "bigint"),
        ("name", "character varying(255)"),
        ("at", "timestamp(3) with time zone"),
        ("Created", "date"),
        ("extra", "integer"),
        ("plain", "int64"),
    ];
    app.update(cx, |this, _| {
        let tab_id = this.workspace.open_ran_query_tab("Query");
        let batch = native_typed_batch(&[
            ("id", Int64, Some("bigint")),
            ("name", Utf8, Some("character varying")),
            ("at", Utf8, Some("timestamp with time zone")),
            ("Created", Utf8, Some("text")),
            ("extra", Int64, Some("integer")),
            ("plain", Int64, None),
        ]);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        runtime.structure_columns = vec![
            catalog_column("id", "bigint"),
            catalog_column("name", "character varying(255)"),
            catalog_column("at", "timestamp(3) with time zone"),
            // Catalog name differs in case: matched case-insensitively.
            catalog_column("created", "date"),
        ];
        this.reset_active_view();
    });
    cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    app.read_with(cx, |this, _| {
        let tab = this.workspace.active_tab().cloned().expect("tab");
        let items = this.build_column_items(this.active_id());
        assert_eq!(items.len(), expected.len());
        for (index, (name, want)) in expected.iter().enumerate() {
            let position = CellPosition {
                row: 0,
                column: index,
            };
            let header = this.av().header_column_type(index);
            let detail_panel = this.build_cell_detail(&tab, position).expect("cell detail");
            let column_detail = this.build_column_detail(&tab, position).expect("column detail");
            let row_detail = this.build_cell_detail_row(&tab, 0).expect("row detail");
            assert_eq!(items[index].name, *name);
            assert_eq!(header.as_ref(), *want, "header for {name}");
            assert_eq!(items[index].type_name, *want, "properties for {name}");
            assert_eq!(detail_panel.type_name, *want, "cell detail for {name}");
            assert_eq!(column_detail.type_name, *want, "column detail for {name}");
            assert_eq!(row_detail.fields[index].type_name, *want, "row detail for {name}");
        }
    });
}

#[gpui::test]
fn ad_hoc_result_header_uses_engine_native_type_then_arrow_fallback(cx: &mut TestAppContext) {
    use arrow_schema::DataType::{Int64, Utf8};
    let (app, cx) = column_type_app(cx);
    app.update(cx, |this, _| {
        let tab_id = this.workspace.open_query_tab("Query");
        let batch = native_typed_batch(&[
            ("id", Int64, Some("bigint")),
            ("note", Utf8, Some("text")),
            ("computed", Int64, None),
        ]);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        this.reset_active_view();
        this.sync_column_type_labels(this.active_id());
        let labels: Vec<_> = (0..3).map(|c| this.av().header_column_type(c).to_string()).collect();
        assert_eq!(labels, ["bigint", "text", "int64"]);
        // Grid right-alignment keys on the same labels: native `bigint` counts
        // as numeric, not only Arrow/BSON names.
        let numeric: Vec<_> = (0..3).map(|c| this.av().column_is_numeric(c)).collect();
        assert_eq!(numeric, [true, false, true]);
        let items = this.build_column_items(this.active_id());
        let item_types: Vec<_> = items.iter().map(|item| item.type_name.as_str()).collect();
        assert_eq!(item_types, ["bigint", "text", "int64"]);

        // A catalog arriving later invalidates the cached labels.
        let tab_id = this.workspace.active_tab().map(|tab| tab.id).unwrap();
        this.workspace.runtime_mut(tab_id).unwrap().structure_columns =
            vec![catalog_column("id", "integer")];
        this.sync_column_type_labels(this.active_id());
        assert_eq!(this.av().header_column_type(0).as_ref(), "integer");
        assert_eq!(this.av().header_column_type(1).as_ref(), "text");
        assert!(this.av().column_is_numeric(0), "catalog `integer` stays numeric");
        assert!(!this.av().column_is_numeric(1));
        assert!(!this.av().column_is_numeric(9), "unknown column is not numeric");
    });
}

#[gpui::test]
fn mongo_result_header_keeps_bson_types_and_ignores_catalog(cx: &mut TestAppContext) {
    let (app, cx) = column_type_app(cx);
    app.update(cx, |this, _| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace.active_tab_mut().expect("active tab").query_mode =
            Some(QueryMode::MongoFind);
        let batch = native_typed_batch(&[
            ("_id", arrow_schema::DataType::Utf8, None),
            ("name", arrow_schema::DataType::Utf8, None),
        ]);
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(
            ArrowGridSource::with_column_types(
                vec![std::sync::Arc::new(batch)],
                ["objectId".to_string(), "string".to_string()],
            ),
        ));
        runtime.structure_columns = vec![catalog_column("name", "text")];
        this.reset_active_view();
        this.sync_column_type_labels(this.active_id());
        assert_eq!(this.av().header_column_type(0).as_ref(), "objectId");
        assert_eq!(this.av().header_column_type(1).as_ref(), "string");
        let items = this.build_column_items(this.active_id());
        assert_eq!(items[0].type_name, "objectId");
        assert_eq!(items[1].type_name, "string");
    });
}

#[test]
fn column_type_resolver_prefers_catalog_then_native_then_logical() {
    use super::results::column_type::resolve_column_type;
    let catalog = vec![catalog_column("Id", "bigint"), catalog_column("id", "integer")];
    // Exact match wins over a case-insensitive one.
    assert_eq!(resolve_column_type(&catalog, "id", Some("text"), Some("string")), Some("integer"));
    assert_eq!(resolve_column_type(&catalog, "ID", Some("text"), None), Some("bigint"));
    assert_eq!(resolve_column_type(&[], "id", Some("text"), Some("string")), Some("text"));
    assert_eq!(resolve_column_type(&[], "id", None, Some("string")), Some("string"));
    assert_eq!(resolve_column_type(&[], "id", Some(""), None), None);
    let empty_catalog = vec![catalog_column("id", "")];
    assert_eq!(resolve_column_type(&empty_catalog, "id", Some("uuid"), None), Some("uuid"));
}

#[test]
fn column_type_categories_cover_native_and_arrow_names() {
    use super::results::column_type::{column_type_category, ColumnTypeCategory::*};
    for name in [
        "bigint", "integer", "smallint", "numeric(10,2)", "real", "double precision", "money",
        "serial", "INTEGER", "int64", "int32", "float", "number", "decimal128", "DOUBLE",
    ] {
        assert_eq!(column_type_category(name), Number, "{name}");
    }
    for name in ["boolean", "bool", "BOOLEAN"] {
        assert_eq!(column_type_category(name), Boolean, "{name}");
    }
    for name in [
        "timestamp(3) with time zone", "timestamp without time zone", "date", "datetime",
        "time without time zone", "interval", "DATETIME",
    ] {
        assert_eq!(column_type_category(name), Temporal, "{name}");
    }
    for name in ["json", "jsonb", "text[]", "integer[]", "array", "object"] {
        assert_eq!(column_type_category(name), Object, "{name}");
    }
    for name in ["bytea", "binary", "BLOB"] {
        assert_eq!(column_type_category(name), Binary, "{name}");
    }
    assert_eq!(column_type_category("uuid"), Uuid);
    for name in ["text", "character varying(255)", "varchar", "string", "character", "point", "name"] {
        assert_eq!(column_type_category(name), Text, "{name}");
    }
}

#[gpui::test]
fn caret_dropdown_buttons_are_wide_enough_for_icon_and_caret(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: 1 });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
    });
    for _ in 0..3 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let placement = visual.debug_bounds("cell-detail-placement").expect("placement button");
    // 14px icon + caret + padding: an icon-only square (24px) would clip the caret.
    assert!(
        placement.size.width > placement.size.height,
        "placement dropdown must be wider than tall to show the caret: {placement:?}"
    );
}

/// A narrow pane must keep the document header's view controls (Relaxed /
/// Canonical and the Tree/JSON switch) inside the document panel: the
/// identity truncates and the panel minimums give way instead.
#[gpui::test]
fn mongo_document_header_controls_stay_visible_in_a_narrow_pane(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.runtime_mut(tab_id).unwrap().mongo.view =
            crate::mongo_result::MongoResultView::Document;
        this.select_mongo_document(tab_id, 1, cx);
    });
    for width in [1100.0, 900.0, 820.0, 760.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        let panel = visual.debug_bounds("mongo-document-panel").expect("document panel");
        let header = visual.debug_bounds("mongo-document-header").expect("header");
        for control in ["mongo-json-mode", "mongo-document-view"] {
            let bounds = visual.debug_bounds(control).unwrap_or_else(|| panic!("{width}: {control}"));
            assert!(bounds.right() <= panel.right(), "{width}: {control} inside the panel");
            assert!(bounds.right() <= header.right(), "{width}: {control} inside the header");
            assert!(bounds.right() <= gpui::px(width), "{width}: {control} on screen");
        }
    }
}
