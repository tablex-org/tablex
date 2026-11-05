#[test]
fn mutation_input_uses_column_types_without_treating_text_null_as_sql_null() {
    assert_eq!(
        mutation_value_from_input("42", "integer").expect("integer"),
        MutationValue::Number("42".into())
    );
    assert_eq!(
        mutation_value_from_input("NULL", "text").expect("text"),
        MutationValue::Text("NULL".into())
    );
    // NULL is never a spelling: the text is a wrong number; only an empty
    // editor in a numeric column is NULL.
    assert!(mutation_value_from_input("NULL", "integer").is_err());
    assert_eq!(
        mutation_value_from_input("", "integer").expect("empty"),
        MutationValue::Null
    );
    assert!(mutation_value_from_input("maybe", "boolean").is_err());
    assert_eq!(
        mutation_value_from_input("18446744073709551600", "uint64").expect("uint64"),
        MutationValue::Number("18446744073709551600".into())
    );
}

#[test]
fn mongo_query_payload_includes_projection_text() {
    let payload = mongo_query_payload("{}", r#"{"_id":1}"#, r#"{"name":1}"#);

    assert_eq!(payload["filter"], "{}");
    assert_eq!(payload["sort"], r#"{"_id":1}"#);
    assert_eq!(payload["project"], r#"{"name":1}"#);
}

#[test]
fn mongo_script_request_is_never_an_sql_or_find_filter_request() {
    let request = super::results::mongo::query::mongo_request(
        engine_driver_api::OperationId(7),
        "warehouse",
        "orders",
        20,
        10,
        "",
        "",
        "",
        Some("db.orders.find({ active: true })"),
    );
    assert_eq!(request.kind, "mongo_script");
    assert_eq!(request.database.as_deref(), Some("warehouse"));
    assert_eq!(
        request.payload["script"],
        "db.orders.find({ active: true })"
    );
    assert!(request.payload.get("filter").is_none());
}

#[test]
fn insert_input_accepts_known_columns_and_rejects_unknown_columns() {
    let columns = vec![
        crate::catalog::CatalogColumn {
            name: "name".into(),
            data_type: "text".into(),
            nullable: false,
            primary_key: false,
            ..Default::default()
        },
        crate::catalog::CatalogColumn {
            name: "age".into(),
            data_type: "integer".into(),
            nullable: true,
            primary_key: false,
            ..Default::default()
        },
    ];
    let values =
        insert_values_from_json(r#"{"name":"Ada","age":37}"#, &columns).expect("known columns");
    assert_eq!(values.len(), 2);
    assert!(insert_values_from_json(r#"{"unknown":true}"#, &columns).is_err());
    assert!(insert_values_from_json(r#"{"name":null}"#, &columns).is_err());
}

#[test]
fn mongo_input_preserves_native_bson_shapes() {
    assert_eq!(
        mongo_value_from_input("ObjectId(66d4f84d0000000000000003)", "object_id")
            .expect("object id"),
        serde_json::json!({"$oid": "66d4f84d0000000000000003"})
    );
    assert_eq!(
        mongo_value_from_input("42", "int64").expect("integer"),
        serde_json::json!({"$numberLong": "42"})
    );
    assert_eq!(
        mongo_value_from_input(r#"{"enabled":true}"#, "document").expect("document"),
        serde_json::json!({"enabled": true})
    );
    assert_eq!(
        mongo_value_from_input("2024-09-01T00:00:00.000Z", "date_time").expect("date time"),
        serde_json::json!({"$date": {"$numberLong": "1725148800000"}})
    );
    assert!(mongo_value_from_input("not-an-id", "object_id").is_err());
}

#[test]
fn mongo_editor_preserves_decimal_precision_and_integer_width() {
    let decimal = "12345678901234567890.25";
    let value = mongo_value_from_input(decimal, "decimal128").unwrap();
    assert_eq!(value, serde_json::json!({"$numberDecimal": decimal}));
    assert!(
        matches!(bson::Bson::try_from(value).unwrap(), bson::Bson::Decimal128(value) if value.to_string() == decimal)
    );
    assert!(matches!(
        bson::Bson::try_from(mongo_value_from_input("42", "int64").unwrap()).unwrap(),
        bson::Bson::Int64(42)
    ));
    assert!(matches!(
        bson::Bson::try_from(mongo_value_from_input("42", "int32").unwrap()).unwrap(),
        bson::Bson::Int32(42)
    ));
    assert!(mongo_value_from_input("2147483648", "int32").is_err());
}

#[test]
fn mongo_null_editor_requires_an_explicit_json_value() {
    assert!(mongo_value_from_input("present", "null").is_err());
    assert_eq!(
        mongo_value_from_input("\"present\"", "null").unwrap(),
        serde_json::json!("present")
    );
    assert_eq!(
        mongo_value_from_input("NULL", "null").unwrap(),
        serde_json::Value::Null
    );
}

#[test]
fn mongo_detail_values_round_trip_through_the_typed_editor() {
    for (native_type, value) in [
        (
            "date_time",
            serde_json::json!({"$date": {"$numberLong": "1725148800000"}}),
        ),
        (
            "decimal128",
            serde_json::json!({"$numberDecimal": "12345678901234567890.25"}),
        ),
        ("int64", serde_json::json!({"$numberLong": "42"})),
    ] {
        let field = engine_driver_api::RowField {
            name: "value".into(),
            native_type: native_type.into(),
            display_text: String::new(),
            value: value.clone(),
            editable: true,
        };
        let parsed = mongo_value_from_input(&mongo_detail_value(&field), native_type)
            .expect("detail editor accepts its own displayed format");
        assert_eq!(
            bson::Bson::try_from(parsed).unwrap(),
            bson::Bson::try_from(value).unwrap()
        );
    }
    assert!(mongo_value_from_input("not-a-date", "date_time").is_err());
    assert!(mongo_value_from_input("{}", "array").is_err());
    assert!(mongo_value_from_input("[]", "document").is_err());
}

#[test]
fn mongo_typed_editor_requests_commit_without_losing_values() {
    let mut fixture = crate::ui_fixture::UiFixture::new(FixtureKind::Sample);
    let connection = crate::ui_fixture::MONGO_ID;
    let page = fixture
        .query_mongo_page(
            connection,
            "fixture_db_0",
            "fixture_collection_00",
            0,
            50,
            None,
            None,
        )
        .unwrap();
    let fields = fixture.inspect_mongo_row(page.page.page_id, 0).unwrap();
    for field in &fields.fields {
        let parsed =
            mongo_value_from_input(&mongo_detail_value(field), &field.native_type).unwrap();
        assert_eq!(
            bson::Bson::try_from(parsed).unwrap(),
            bson::Bson::try_from(field.value.clone()).unwrap(),
            "unchanged editor value must preserve {}",
            field.name
        );
    }
    let mut requests = Vec::new();
    for (field, native_type, input) in [
        ("nullable", "null", "\"present\""),
        ("balance", "decimal128", "12345678901234567890.25"),
        ("createdAt", "date_time", r#"{"$date":{"$numberLong":"0"}}"#),
    ] {
        let payload = InlineEditPayload::Mongo {
            native_type: native_type.into(),
            identity: fields.mutation_identity.clone(),
            database: "fixture_db_0".into(),
            collection: "fixture_collection_00".into(),
            field: field.into(),
        };
        let PendingMongoMutation::Update { request, value, .. } =
            build_inline_mongo_update(&payload, 0, input).unwrap()
        else {
            panic!("expected update")
        };
        let decoded: bson::Document = serde_json::from_value(request.payload.clone()).unwrap();
        assert_eq!(
            decoded.get_document("$set").unwrap().get(field),
            Some(&bson::Bson::try_from(value).unwrap())
        );
        requests.push(request);
    }
    assert_eq!(
        fixture
            .apply_mongo_mutations(
                connection,
                "fixture_db_0",
                "fixture_collection_00",
                &requests
            )
            .unwrap(),
        3
    );
    let refreshed = fixture
        .query_mongo_page(
            connection,
            "fixture_db_0",
            "fixture_collection_00",
            0,
            50,
            None,
            None,
        )
        .unwrap();
    let updated = fixture
        .inspect_mongo_row(refreshed.page.page_id, 0)
        .unwrap();
    let field = |name: &str| {
        updated
            .fields
            .iter()
            .find(|field| field.name == name)
            .unwrap()
    };
    assert_eq!(field("nullable").value, serde_json::json!("present"));
    assert_eq!(field("nullable").native_type, "string");
    assert_eq!(
        field("balance").value,
        serde_json::json!({"$numberDecimal": "12345678901234567890.25"})
    );
    assert_eq!(field("balance").native_type, "decimal128");
    assert_eq!(
        field("createdAt").value,
        serde_json::json!({"$date": {"$numberLong": "0"}})
    );
    assert_eq!(
        field("count").value,
        serde_json::json!({"$numberLong": "0"})
    );
}

#[test]
fn inline_sql_edit_replaces_same_cell_and_rejects_invalid_input() {
    let mut runtime = crate::workspace::TabRuntime::new();
    let payload = InlineEditPayload::Sql {
        primary_key: crate::catalog::CatalogColumn {
            name: "id".into(),
            data_type: "integer".into(),
            nullable: false,
            primary_key: true,
            ..Default::default()
        },
        key_value: MutationValue::Number("1".into()),
        key: Default::default(),
        column_name: "name".into(),
        data_type: "text".into(),
    };
    if let InlineEditPayload::Sql { primary_key, .. } = &payload {
        runtime.structure_columns.push(primary_key.clone());
    }
    assert!(stage_inline_sql_edit(
        &mut runtime,
        3,
        1,
        &payload,
        "old",
        "Ada"
    ));
    assert_eq!(runtime.pending_edits.len(), 1);
    assert_eq!(
        runtime
            .pending_cell_text(CellPosition { row: 3, column: 1 })
            .map(|text| text.as_ref()),
        Some("Ada")
    );
    assert!(stage_inline_sql_edit(
        &mut runtime,
        3,
        1,
        &payload,
        "old",
        "Grace"
    ));
    assert_eq!(
        runtime.pending_edits.len(),
        1,
        "editing the same cell should replace, not append"
    );
    assert_eq!(
        runtime
            .pending_cell_text(CellPosition { row: 3, column: 1 })
            .map(|text| text.as_ref()),
        Some("Grace")
    );
    let bool_payload = InlineEditPayload::Sql {
        primary_key: crate::catalog::CatalogColumn {
            name: "id".into(),
            data_type: "integer".into(),
            nullable: false,
            primary_key: true,
            ..Default::default()
        },
        key_value: MutationValue::Number("1".into()),
        key: Default::default(),
        column_name: "active".into(),
        data_type: "boolean".into(),
    };
    assert!(!stage_inline_sql_edit(
        &mut runtime,
        4,
        1,
        &bool_payload,
        "false",
        "maybe"
    ));
    assert_eq!(
        runtime.pending_edits.len(),
        1,
        "invalid input should not stage a new edit"
    );
    assert!(stage_inline_sql_edit(
        &mut runtime,
        3,
        1,
        &payload,
        "old",
        "old"
    ));
    assert!(runtime.pending_edits.is_empty());
    assert!(runtime
        .pending_cell_text(CellPosition { row: 3, column: 1 })
        .is_none());
}

#[test]
fn inline_sql_edit_rejects_nonnullable_null_and_primary_key_mutations() {
    let mut runtime = crate::workspace::TabRuntime::new();
    runtime.structure_columns = vec![
        crate::catalog::CatalogColumn {
            name: "id".into(),
            data_type: "int64".into(),
            nullable: false,
            primary_key: true,
            ..Default::default()
        },
        crate::catalog::CatalogColumn {
            name: "count".into(),
            data_type: "int64".into(),
            nullable: false,
            primary_key: false,
            ..Default::default()
        },
    ];
    let primary_key = runtime.structure_columns[0].clone();
    let payload = InlineEditPayload::Sql {
        primary_key: primary_key.clone(),
        key_value: MutationValue::Number("1".into()),
        key: Default::default(),
        column_name: "count".into(),
        data_type: "int64".into(),
    };
    assert!(!stage_inline_sql_edit(
        &mut runtime,
        0,
        1,
        &payload,
        "5",
        "NULL"
    ));
    assert!(!stage_inline_sql_edit(
        &mut runtime,
        0,
        0,
        &InlineEditPayload::Sql {
            primary_key,
            key_value: MutationValue::Number("1".into()),
            key: Default::default(),
            column_name: "id".into(),
            data_type: "int64".into(),
        },
        "1",
        "2"
    ));
    assert!(runtime.pending_edits.is_empty());
    // A composite key is edited through its whole key (the driver route), but
    // never through one of its own columns.
    runtime.structure_columns[1].primary_key = true;
    let primary_key = runtime.structure_columns[0].clone();
    let payload = |column_name: &str, data_type: &str| InlineEditPayload::Sql {
        primary_key: primary_key.clone(),
        key_value: MutationValue::Number("1".into()),
        key: Default::default(),
        column_name: column_name.into(),
        data_type: data_type.into(),
    };
    assert!(!stage_inline_sql_edit(&mut runtime, 0, 1, &payload("count", "int64"), "5", "6"));
    assert!(runtime.pending_edits.is_empty());
    assert!(stage_inline_sql_edit(&mut runtime, 0, 2, &payload("name", "text"), "Ada", "Changed"));
    assert_eq!(runtime.pending_edits.len(), 1);
}

#[test]
fn submitted_sql_batch_reconciliation_preserves_later_queue_entries() {
    let mut runtime = crate::workspace::TabRuntime::new();
    let first = crate::workspace::PendingSqlRow::Insert(crate::staged_sql::InsertRowRequest {
        database: "db".into(),
        schema: "main".into(),
        table: "orders".into(),
        values: vec![("name".into(), MutationValue::Text("first".into()))],
    });
    let later = crate::workspace::PendingSqlRow::Insert(crate::staged_sql::InsertRowRequest {
        database: "db".into(),
        schema: "main".into(),
        table: "orders".into(),
        values: vec![("name".into(), MutationValue::Text("later".into()))],
    });
    runtime.pending_sql_rows.push(first.clone());
    runtime.pending_sql_rows.push(later.clone());
    super::results::editing::reconcile_submitted_sql_batch(&mut runtime, &[], &[first]);
    assert_eq!(runtime.pending_sql_rows, vec![later]);
}

#[test]
fn inline_mongo_update_builds_typed_set_payload() {
    let payload = InlineEditPayload::Mongo {
        native_type: "object_id".into(),
        identity: serde_json::json!({"_id": {"$oid": "66d4f84d0000000000000003"}}),
        database: "app".into(),
        collection: "users".into(),
        field: "owner".into(),
    };
    let mutation = build_inline_mongo_update(&payload, 7, "ObjectId(66d4f84d0000000000000003)")
        .expect("valid object id");
    let crate::mongo_result::PendingMongoMutation::Update { request, field, .. } = mutation else {
        panic!("expected an update mutation");
    };
    assert_eq!(field, "owner");
    assert_eq!(request.kind, "update");
    assert_eq!(
        request.payload,
        serde_json::json!({"$set": {"owner": {"$oid": "66d4f84d0000000000000003"}}})
    );
    assert!(build_inline_mongo_update(&payload, 7, "not-an-id").is_err());
}

#[test]
fn mongo_pending_overlay_follows_active_update_history() {
    let mut runtime = crate::workspace::TabRuntime::new();
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
        "name",
        arrow_schema::DataType::Utf8,
        false,
    )]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::StringArray::from(vec!["old"]))
                as arrow_array::ArrayRef,
        ],
    )
    .expect("valid test batch");
    runtime.grid = crate::grid::GridState::new(GridContentSource::Arrow(
        crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
    ));
    let payload = InlineEditPayload::Mongo {
        native_type: "string".into(),
        identity: serde_json::json!({"_id": 1}),
        database: "app".into(),
        collection: "users".into(),
        field: "name".into(),
    };
    runtime
        .mongo
        .mutations
        .stage(build_inline_mongo_update(&payload, 0, "Grace").expect("valid update"));
    rebuild_mongo_pending_overlay(&mut runtime);
    assert_eq!(
        runtime
            .pending_cell_text(CellPosition { row: 0, column: 0 })
            .map(|text| text.as_ref()),
        Some("Grace")
    );

    assert!(runtime.mongo.mutations.undo());
    rebuild_mongo_pending_overlay(&mut runtime);
    assert!(runtime
        .pending_cell_text(CellPosition { row: 0, column: 0 })
        .is_none());

    assert!(runtime.mongo.mutations.redo());
    rebuild_mongo_pending_overlay(&mut runtime);
    assert!(runtime
        .pending_cell_text(CellPosition { row: 0, column: 0 })
        .is_some());

    runtime.mongo.mutations.rollback();
    rebuild_mongo_pending_overlay(&mut runtime);
    assert!(runtime
        .pending_cell_text(CellPosition { row: 0, column: 0 })
        .is_none());
}

#[gpui::test]
fn hidden_mongo_column_edit_reads_the_displayed_source_cell(cx: &mut TestAppContext) {
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
    let tab_id = app.update(cx, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace
            .active_tab_mut()
            .expect("active tab")
            .query_mode = Some(QueryMode::MongoFind);
        let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
            arrow_schema::Field::new("_id", arrow_schema::DataType::Int64, false),
            arrow_schema::Field::new("transId", arrow_schema::DataType::Utf8, false),
            arrow_schema::Field::new("value", arrow_schema::DataType::Int64, false),
            arrow_schema::Field::new("transType", arrow_schema::DataType::Utf8, false),
        ]));
        let batch = arrow_array::RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![1]))
                    as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::StringArray::from(vec!["TX-001"]))
                    as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![100000]))
                    as arrow_array::ArrayRef,
                std::sync::Arc::new(arrow_array::StringArray::from(vec!["PAY"]))
                    as arrow_array::ArrayRef,
            ],
        )
        .expect("valid test batch");
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        runtime.mongo.known_columns = vec![
            "_id".into(),
            "transId".into(),
            "value".into(),
            "transType".into(),
        ];
        runtime.mongo.visible_columns = HashSet::from([
            "_id".into(),
            "transId".into(),
            "value".into(),
            "transType".into(),
        ]);
        this.toggle_mongo_column(tab_id, "transId".into(), cx);
        cx.notify();
        tab_id
    });

    let text = app.read_with(cx, |this, _| {
        this.cell_edit_text(tab_id, CellPosition { row: 0, column: 2 })
    });
    assert_eq!(text.map(|text| text.to_string()), Some("PAY".into()));

    app.update(cx, |this, cx| {
        let runtime = this.workspace.runtime_mut(tab_id).expect("query runtime");
        let payload = InlineEditPayload::Mongo {
            native_type: "string".into(),
            identity: serde_json::json!({"_id": 1}),
            database: "app".into(),
            collection: "payments".into(),
            field: "transType".into(),
        };
        runtime
            .mongo
            .mutations
            .stage(build_inline_mongo_update(&payload, 0, "TRANSFER").expect("valid update"));
        rebuild_mongo_pending_overlay(runtime);
        cx.notify();
    });
    let pending_text = app.read_with(cx, |this, _| {
        this.cell_edit_text(tab_id, CellPosition { row: 0, column: 2 })
    });
    assert_eq!(
        pending_text.map(|text| text.to_string()),
        Some("TRANSFER".into())
    );

    app.update(cx, |this, cx| {
        this.toggle_mongo_column(tab_id, "transId".into(), cx);
    });
    let restored_text = app.read_with(cx, |this, _| {
        this.cell_edit_text(tab_id, CellPosition { row: 0, column: 3 })
    });
    assert_eq!(
        restored_text.map(|text| text.to_string()),
        Some("TRANSFER".into())
    );
}

#[gpui::test]
fn long_cell_starts_a_floating_editor_with_the_full_value(cx: &mut TestAppContext) {
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
    let long_value = "6697d973eed2a4d1".repeat(8);
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("value", arrow_schema::DataType::Utf8, true),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::Int64Array::from(vec![1])) as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::StringArray::from(vec![Some(
                long_value.as_str(),
            )])) as arrow_array::ArrayRef,
        ],
    )
    .expect("valid batch");
    let _tab_id = app.update(cx, |this, cx| {
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
                name: "value".into(),
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
                ..Default::default()
            },
        ];
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        this.av_mut().grid_ui.layout.set_width(1, 60.0);
        cx.notify();
        tab_id
    });
    cx.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(600.0)));
    cx.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    cx.run_until_parked();
    let grid_bounds = cx
        .debug_bounds("grid-scroll-area")
        .expect("grid-scroll-area should be painted");
    let edit_cell = gpui::Point::new(
        grid_bounds.origin.x + gpui::px(48.0 + 160.0 + 24.0),
        grid_bounds.origin.y + gpui::px(theme::RESULT_GRID_HEADER_HEIGHT + theme::ROW_HEIGHT / 2.0),
    );
    cx.simulate_event(gpui::MouseDownEvent {
        position: edit_cell,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        position: edit_cell,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let floating_editor = cx
        .debug_bounds("inline-cell-editor-floating")
        .expect("overflowing cell should render a floating editor");
    assert!(
        floating_editor.size.width >= gpui::px(128.0),
        "floating editor should be wider than a narrow source cell"
    );

    app.read_with(cx, |this, cx| {
        let edit = this
            .av().grid_ui
            .inline_edit
            .as_ref()
            .expect("editor should be active");
        assert!(
            edit.floating,
            "an overflowing cell should use a floating editor"
        );
        assert_eq!(edit.editor.value(cx).as_ref(), long_value);
    });
}

#[gpui::test]
fn inline_edit_blur_closes_editor_without_staging_invalid_or_unchanged_value(
    cx: &mut TestAppContext,
) {
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

    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("count", arrow_schema::DataType::Int64, false),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::Int64Array::from(vec![1])) as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::Int64Array::from(vec![7])) as arrow_array::ArrayRef,
        ],
    )
    .expect("valid test batch");

    cx.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(600.0)));
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert!(cx.update(|window, _| window.is_window_active()));
    let tab_id = app.update(cx, |this, cx| {
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
        ];
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
        tab_id
    });

    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let grid_bounds = cx
        .debug_bounds("grid-scroll-area")
        .expect("grid-scroll-area should be painted");
    let edit_cell = gpui::Point::new(
        grid_bounds.origin.x + gpui::px(48.0 + 160.0 + 24.0),
        grid_bounds.origin.y + gpui::px(theme::RESULT_GRID_HEADER_HEIGHT + theme::ROW_HEIGHT / 2.0),
    );
    cx.simulate_event(gpui::MouseDownEvent {
        position: edit_cell,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        position: edit_cell,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    let editor = cx.cx.read(|app_context| {
        app.read(app_context)
            .av().grid_ui
            .inline_edit
            .as_ref()
            .expect("editor should be active")
            .editor
            .clone()
    });
    assert!(
        cx.update(|window, app_context| { editor.focus_handle(app_context).is_focused(window) }),
        "editor should retain focus after double-click"
    );
    cx.simulate_input("9");
    let immediate_value = cx.cx.read(|app_context| editor.value(app_context));
    assert_eq!(
        immediate_value.as_ref(),
        "9",
        "typing immediately after opening should replace the cell value"
    );

    let first_cell = gpui::Point::new(
        grid_bounds.origin.x + gpui::px(24.0),
        grid_bounds.origin.y + gpui::px(theme::RESULT_GRID_HEADER_HEIGHT + theme::ROW_HEIGHT / 2.0),
    );
    cx.simulate_click(first_cell, Default::default());
    assert!(
        cx.update(|window, app_context| { app.read(app_context).av().grid_is_focused(window) })
    );
    let (editing, pending, display) = cx.cx.read(|app_context| {
        let this = app.read(app_context);
        (
            this.av().grid_ui.inline_edit.is_some(),
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .pending_edits
                .len(),
            this.workspace.runtime(tab_id).and_then(|runtime| {
                runtime
                    .pending_cell_text(CellPosition { row: 0, column: 1 })
                    .map(|text| text.to_string())
            }),
        )
    });
    assert!(!editing, "valid focus loss should close the editor");
    assert_eq!(pending, 1, "valid focus loss should stage an edit");
    assert_eq!(display.as_deref(), Some("9"));
    let copied = app.update(cx, |this, _| {
        let mut selection = crate::grid::GridSelection::default();
        selection.select(CellPosition { row: 0, column: 1 });
        this.selection_as_tsv(this.active_id(), &selection)
    });
    assert_eq!(copied, "9", "copy should use the staged display value");

    app.update_in(cx, |this, window, cx| {
        let tab = this
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .cloned()
            .expect("active query tab");
        this.begin_inline_edit(tab.id, 0, 1, window, cx);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let editor = cx.cx.read(|app_context| {
        app.read(app_context)
            .av().grid_ui
            .inline_edit
            .as_ref()
            .expect("pending editor should reopen")
            .editor
            .clone()
    });
    let reopened_value = cx.cx.read(|app_context| editor.value(app_context));
    assert_eq!(
        reopened_value.as_ref(),
        "9",
        "reopened editor should use the staged value"
    );
    editor
        .as_input()
        .expect("short inline edit uses a single-line input")
        .update_in(cx, |state, window, cx| {
            state.set_value("not-an-integer", window, cx);
        });
    app.update_in(cx, |this, window, cx| {
        this.commit_inline_edit(this.active_id(), InlineEditExit::Submit, Some(window), cx);
    });
    assert!(
        cx.cx
            .read(|app_context| app.read(app_context).av().grid_ui.inline_edit.is_some()),
        "invalid submit should keep the editor open"
    );

    cx.simulate_click(first_cell, Default::default());
    let (editing, pending, display) = cx.cx.read(|app_context| {
        let this = app.read(app_context);
        (
            this.av().grid_ui.inline_edit.is_some(),
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .pending_edits
                .len(),
            this.workspace.runtime(tab_id).and_then(|runtime| {
                runtime
                    .pending_cell_text(CellPosition { row: 0, column: 1 })
                    .map(|text| text.to_string())
            }),
        )
    });
    assert!(!editing, "focus loss should close an invalid editor");
    assert_eq!(
        pending, 1,
        "invalid focus loss must preserve existing pending edit"
    );
    assert_eq!(display.as_deref(), Some("9"));

    app.update(cx, |this, cx| this.discard_pending_edit(this.active_id(), cx));
    app.update_in(cx, |this, window, cx| {
        let tab = this
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .cloned()
            .expect("active query tab");
        this.begin_inline_edit(tab.id, 0, 1, window, cx);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.simulate_click(first_cell, Default::default());

    let (editing, pending, display) = cx.cx.read(|app_context| {
        let this = app.read(app_context);
        (
            this.av().grid_ui.inline_edit.is_some(),
            this.workspace
                .runtime(tab_id)
                .expect("query runtime")
                .pending_edits
                .len(),
            this.workspace.runtime(tab_id).and_then(|runtime| {
                runtime
                    .pending_cell_text(CellPosition { row: 0, column: 1 })
                    .map(|text| text.to_string())
            }),
        )
    });
    assert!(!editing, "unchanged focus loss should close the editor");
    assert_eq!(pending, 0, "unchanged focus loss must not stage an edit");
    assert!(
        display.is_none(),
        "discard should clear the pending overlay"
    );
}
