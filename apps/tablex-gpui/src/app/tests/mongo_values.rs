// Value correctness of the MongoDB cells: what the editors show and read back,
// how a typed value gets its BSON type, rows that cannot be written, a write
// that matches nothing, and the three kinds of "no value" in the grid.

fn mongo_field(native_type: &str, value: serde_json::Value) -> engine_driver_api::RowField {
    engine_driver_api::RowField {
        name: "f".into(),
        native_type: native_type.into(),
        display_text: String::new(),
        value,
        editable: true,
    }
}

#[test]
fn every_bson_type_round_trips_through_the_editor_text_unchanged() {
    use serde_json::json;
    let cases = [
        ("string", json!("NULL")),
        ("string", json!("5")),
        ("string", json!("  {\"a\": 1}\n")),
        ("string", json!("")),
        ("boolean", json!(true)),
        ("int32", json!({"$numberInt": "-5"})),
        ("int64", json!({"$numberLong": "5"})),
        ("int64", json!({"$numberLong": "9007199254740993"})),
        ("double", json!({"$numberDouble": "5.0"})),
        ("double", json!({"$numberDouble": "0.30000000000000004"})),
        // The driver writes a Double's digits in full; the editor shows an exponent.
        ("double", bson::Bson::Double(1.5e300).into_canonical_extjson()),
        ("double", bson::Bson::Double(5e-324).into_canonical_extjson()),
        ("double", json!({"$numberDouble": "NaN"})),
        ("double", json!({"$numberDouble": "Infinity"})),
        ("double", json!({"$numberDouble": "-Infinity"})),
        ("double", json!({"$numberDouble": "-0.0"})),
        ("decimal128", json!({"$numberDecimal": "12345678901234567890.25"})),
        ("object_id", json!({"$oid": "66d4f84d0000000000000003"})),
        ("date_time", json!({"$date": {"$numberLong": "1700000000123"}})),
        ("date_time", json!({"$date": {"$numberLong": "253402300800000"}})),
        ("null", serde_json::Value::Null),
        ("undefined", json!({"$undefined": true})),
        ("min_key", json!({"$minKey": 1})),
        ("max_key", json!({"$maxKey": 1})),
        ("binary", json!({"$binary": {"base64": "AAH/", "subType": "00"}})),
        ("binary", json!({"$binary": {"base64": "CQ==", "subType": "90"}})),
        ("binary", json!({"$binary": {"base64": "AAECAwQFBgcICQoLDA0ODw==", "subType": "04"}})),
        ("binary", json!({"$binary": {"base64": "AAECAwQFBgcICQoLDA0ODw==", "subType": "03"}})),
        ("regex", json!({"$regularExpression": {"pattern": "^a.b$", "options": "i"}})),
        ("regex", json!({"$regularExpression": {"pattern": "a/b", "options": ""}})),
        ("timestamp", json!({"$timestamp": {"t": 1700000000_u32, "i": 7_u32}})),
        ("symbol", json!({"$symbol": "s"})),
        ("javascript", json!({"$code": "x => 1"})),
        ("javascript_scope", json!({"$code": "f", "$scope": {"a": {"$numberLong": "1"}}})),
        (
            "document",
            json!({"z": {"$numberLong": "12345"}, "a": {"n": [{"$numberInt": "1"}, {"$numberDouble": "2.5"}, {"$numberLong": "3"}]}, "d": {"$date": {"$numberLong": "5"}}}),
        ),
        ("array", json!([{"k": "v"}, [], {"$numberLong": "7"}, {"$numberDecimal": "1.10"}])),
    ];
    for (native_type, value) in cases {
        let text = mongo_detail_value(&mongo_field(native_type, value.clone()));
        let read_back = mongo_value_from_input(&text, native_type)
            .unwrap_or_else(|error| panic!("{native_type} {value}: {error}\n{text}"));
        assert_eq!(read_back, value, "{native_type}\n{text}");
    }
}

#[test]
fn a_double_is_shown_with_every_digit_and_never_as_an_integer() {
    let shown = |text: &str| mongo_detail_value(&mongo_field("double", serde_json::json!({"$numberDouble": text})));
    assert_eq!(shown("5.0"), "5.0");
    assert_eq!(shown("1234.5678"), "1234.5678", "no rounding to two decimals");
    assert_eq!(shown("0.1"), "0.1");
    // An Int64 / Int32 / Decimal128 of the same number read differently.
    let long = mongo_detail_value(&mongo_field("int64", serde_json::json!({"$numberLong": "5"})));
    assert_eq!(long, "5");
    assert_ne!(shown("5.0"), long);
}

#[test]
fn a_plain_number_typed_into_a_new_or_null_field_gets_the_type_mongosh_gives_it() {
    use serde_json::json;
    for native_type in ["missing", "null"] {
        let read = |input: &str| mongo_value_from_input(input, native_type).unwrap_or_else(|e| panic!("{input}: {e}"));
        assert_eq!(read("5"), json!({"$numberInt": "5"}));
        assert_eq!(read("-5"), json!({"$numberInt": "-5"}));
        assert_eq!(read("5.0"), json!({"$numberDouble": "5.0"}));
        assert_eq!(read("1.5"), json!({"$numberDouble": "1.5"}));
        assert_eq!(read("2147483648"), json!({"$numberDouble": "2147483648.0"}));
        assert_eq!(read("Long('5')"), json!({"$numberLong": "5"}));
        assert_eq!(read("NumberLong(7)"), json!({"$numberLong": "7"}));
        assert_eq!(read("NumberInt(3)"), json!({"$numberInt": "3"}));
        assert_eq!(read("Decimal128('1.50')"), json!({"$numberDecimal": "1.50"}));
        assert_eq!(read("\"text\""), json!("text"));
        assert_eq!(read("true"), json!(true));
        assert_eq!(
            read("ISODate('2024-09-01T00:00:00Z')"),
            json!({"$date": {"$numberLong": "1725148800000"}})
        );
        assert_eq!(
            read(r#"ObjectId("66d4f84d0000000000000003")"#),
            json!({"$oid": "66d4f84d0000000000000003"})
        );
        assert_eq!(
            read(r#"{ a: 1, "b.c": 2.5, d: [Long('1')] }"#),
            json!({"a": {"$numberInt": "1"}, "b.c": {"$numberDouble": "2.5"}, "d": [{"$numberLong": "1"}]})
        );
    }
    let error = mongo_value_from_input("hello", "missing").unwrap_err().to_string();
    assert!(error.contains("hello") && error.contains("quotes") || error.contains("\"text\""), "{error}");
    // An integer a Double cannot hold exactly is refused, not rounded.
    assert!(mongo_value_from_input("9007199254740993", "missing").is_err());
}

#[test]
fn a_cell_keeps_its_bson_type_when_a_plain_number_is_typed() {
    use serde_json::json;
    assert_eq!(mongo_value_from_input("7", "int64").unwrap(), json!({"$numberLong": "7"}));
    assert_eq!(mongo_value_from_input("-7", "int32").unwrap(), json!({"$numberInt": "-7"}));
    assert_eq!(mongo_value_from_input("5", "double").unwrap(), json!({"$numberDouble": "5.0"}));
    assert_eq!(mongo_value_from_input("NaN", "double").unwrap(), json!({"$numberDouble": "NaN"}));
    assert_eq!(mongo_value_from_input("Long('7')", "int64").unwrap(), json!({"$numberLong": "7"}));
    assert!(mongo_value_from_input("2147483648", "int32").is_err());
    assert!(mongo_value_from_input("5.5", "int64").is_err());
    assert!(mongo_value_from_input("Long('7')", "int32").is_err());
    assert!(mongo_value_from_input("[1]", "document").is_err());
    assert!(mongo_value_from_input("5", "binary").is_err());
}

fn row_with_identity(identity: serde_json::Value) -> engine_driver_api::RowFields {
    engine_driver_api::RowFields {
        page_id: 1,
        row_index: 0,
        fields: Vec::new(),
        mutation_identity: identity,
    }
}

#[test]
fn rows_without_an_id_and_unaddressable_field_names_are_refused_with_the_reason() {
    use results::mongo::MongoEditRefusal;
    let no_id = row_with_identity(serde_json::Value::Null);
    let refusal = mongo_edit_refusal(&no_id, "name").expect("no _id, no write");
    assert_eq!(refusal, MongoEditRefusal::NoIdentity);
    let message = refusal.to_string();
    assert!(message.contains("_id") && message.contains("projection"), "{message}");

    let row = row_with_identity(serde_json::json!({"_id": {"$numberInt": "1"}}));
    assert_eq!(mongo_edit_refusal(&row, "name"), None);
    assert_eq!(mongo_edit_refusal(&row, "_id"), Some(MongoEditRefusal::IdField));
    for name in ["a.b", "$set", "tags.0", ""] {
        let refusal = mongo_edit_refusal(&row, name).expect(name);
        assert_eq!(refusal, MongoEditRefusal::FieldName(name.into()));
        assert!(refusal.to_string().contains("$setField"));
    }
}

#[test]
fn no_update_is_built_for_a_filter_that_is_not_the_documents_id() {
    for identity in [
        serde_json::Value::Null,
        serde_json::json!({"rowIndex": 3}),
        serde_json::json!({"name": "Ada"}),
        serde_json::json!({"_id": 1, "name": "Ada"}),
        serde_json::json!({}),
    ] {
        let payload = InlineEditPayload::Mongo {
            native_type: "string".into(),
            identity: identity.clone(),
            database: "db".into(),
            collection: "c".into(),
            field: "name".into(),
        };
        assert!(build_inline_mongo_update(&payload, 0, "x").is_err(), "{identity}");
    }
    let payload = InlineEditPayload::Mongo {
        native_type: "missing".into(),
        identity: serde_json::json!({"_id": {"$numberInt": "1"}}),
        database: "db".into(),
        collection: "c".into(),
        field: "created".into(),
    };
    let PendingMongoMutation::Update { request, value, .. } =
        build_inline_mongo_update(&payload, 0, "5").unwrap()
    else {
        panic!("update")
    };
    assert_eq!(request.payload, serde_json::json!({"$set": {"created": {"$numberInt": "5"}}}));
    assert_eq!(value, serde_json::json!({"$numberInt": "5"}));
}

#[test]
fn missing_null_and_the_text_null_are_drawn_apart_in_the_grid() {
    let source = crate::grid_bson::test_source(
        vec![
            bson::doc! { "f": bson::Bson::Null },
            bson::doc! { "f": "NULL" },
            bson::doc! { "g": 1 },
        ],
        &[("f", "string")],
    );
    let grid = crate::grid::GridState::new(crate::grid_source::GridContentSource::from(source));
    let display = crate::app::settings_state::GridDisplay::from_settings(&crate::settings::Settings::default());
    let at = |row| CellPosition { row, column: 0 };
    let (null_text, null_muted) = display.cell_text(&grid, at(0));
    let (string_text, string_muted) = display.cell_text(&grid, at(1));
    let (missing_text, missing_muted) = display.cell_text(&grid, at(2));
    assert_eq!(null_text, "NULL");
    assert!(null_muted, "a real null is dimmed");
    assert_eq!(string_text, "NULL");
    assert!(!string_muted, "the string \"NULL\" is ordinary text");
    assert_ne!(missing_text, null_text);
    assert_ne!(missing_text, string_text);
    assert!(
        missing_muted,
        "a missing field is dimmed too, with its own marker"
    );
}

/// The Mongo fixture's grid, window sized for the whole row, with the display
/// column of `nullable` and the row of the document that lacks that field.
fn mongo_missing_field_app(
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<AppRoot>,
    &mut gpui::VisualTestContext,
    usize,
    usize,
) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1600.0), gpui::px(850.0)));
    settle(visual);
    let row = crate::ui_fixture::MONGO_DOCUMENT_WITHOUT_NULLABLE;
    let column = app.read_with(visual, |this, _| {
        let tab_id = this.active_id();
        (0..this.display_column_count(tab_id))
            .find(|column| {
                this.source_column_for_display(tab_id, *column)
                    .and_then(|source| this.active_grid().column_name(source))
                    == Some("nullable")
            })
            .expect("nullable column")
    });
    (app, visual, row, column)
}

#[gpui::test]
fn typing_into_a_missing_mongo_field_stages_a_set_that_creates_exactly_that_field(
    cx: &mut TestAppContext,
) {
    let (app, visual, row, column) = mongo_missing_field_app(cx);
    // The grid draws the missing field with its own marker, apart from a real
    // null (row 0) and the other documents' strings.
    app.read_with(visual, |this, _| {
        let source = this
            .source_column_for_display(this.active_id(), column)
            .unwrap();
        let text = |row| {
            this.active_grid()
                .cell_text(CellPosition {
                    row,
                    column: source,
                })
                .map(|text| text.to_string())
        };
        assert_eq!(text(row).as_deref(), Some(crate::grid_bson::MISSING_TEXT));
        assert_eq!(text(0).as_deref(), Some("NULL"));
        assert_eq!(text(1).as_deref(), Some("present"));
    });
    let center = visual
        .debug_bounds(Box::leak(
            format!("grid-body-cell-{row}-{column}").into_boxed_str(),
        ))
        .expect("the missing cell is painted")
        .center();
    double_click(visual, center);
    app.read_with(visual, |this, cx| {
        let edit = this
            .av()
            .grid_ui
            .inline_edit
            .as_ref()
            .expect("a missing field opens an editor");
        assert_eq!(edit.position, CellPosition { row, column });
        assert_eq!(edit.editor.value(cx).as_ref(), "", "it opens empty");
    });
    let editor = app.read_with(visual, |this, _| {
        this.av()
            .grid_ui
            .inline_edit
            .as_ref()
            .unwrap()
            .editor
            .clone()
    });
    editor
        .as_input()
        .expect("a short value uses the single-line input")
        .update_in(visual, |state, window, cx| state.set_value("7", window, cx));
    app.update_in(visual, |this, window, cx| {
        let tab = this.active_id();
        this.commit_inline_edit(tab, InlineEditExit::Submit, Some(window), cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab_id = this.active_id();
        let mutations = this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .mongo
            .mutations
            .active();
        assert_eq!(
            mutations.len(),
            1,
            "{:?}",
            this.view(tab_id).detail_ui.error
        );
        let PendingMongoMutation::Update {
            request,
            row: staged_row,
            field,
            ..
        } = &mutations[0]
        else {
            panic!("an update")
        };
        assert_eq!((*staged_row, field.as_str()), (row, "nullable"));
        // Exactly one field is created, typed as mongosh types a typed 7.
        assert_eq!(
            request.payload,
            serde_json::json!({"$set": {"nullable": {"$numberInt": "7"}}})
        );
        // The filter is the document's own _id (row 3 of the fixture).
        assert_eq!(
            request.identity,
            serde_json::json!({"_id": {"$oid": format!("{:024x}", row + 1)}})
        );
        assert_eq!(
            this.pending_cell_text(tab_id, CellPosition { row, column })
                .as_deref(),
            Some("7")
        );
    });
}

#[gpui::test]
fn set_null_on_a_missing_mongo_field_stages_the_field_as_null(cx: &mut TestAppContext) {
    let (app, visual, row, column) = mongo_missing_field_app(cx);
    let center = visual
        .debug_bounds(Box::leak(
            format!("grid-body-cell-{row}-{column}").into_boxed_str(),
        ))
        .expect("the missing cell is painted")
        .center();
    visual.simulate_click(center, Default::default());
    app.update_in(visual, |this, window, cx| {
        let tab = this.active_id();
        this.set_context_selection_null(tab, window, cx)
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab_id = this.active_id();
        let mutations = this
            .workspace
            .runtime(tab_id)
            .unwrap()
            .mongo
            .mutations
            .active();
        assert_eq!(
            mutations.len(),
            1,
            "{:?}",
            this.view(tab_id).detail_ui.error
        );
        let PendingMongoMutation::Update { request, value, .. } = &mutations[0] else {
            panic!("an update")
        };
        assert_eq!(
            request.payload,
            serde_json::json!({"$set": {"nullable": null}})
        );
        assert!(value.is_null());
        assert_eq!(
            this.pending_cell_text(tab_id, CellPosition { row, column })
                .as_deref(),
            Some("NULL")
        );
    });
}

#[gpui::test]
fn the_detail_editor_commits_only_what_changed_and_keeps_untouched_types(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let nested_column = app.read_with(visual, |this, _| {
        (0..this.active_grid().column_count())
            .find(|column| this.active_grid().column_name(*column) == Some("nested"))
            .expect("nested column")
    });
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: nested_column });
        this.open_cell_detail_tab(this.active_id(), CellDetailTab::Details, cx);
    });
    app.update_in(visual, |this, window, cx| this.open_cell_detail_editor(this.active_id(), window, cx));
    let seed = app.read_with(visual, |this, cx| {
        let edit = this.av().detail_ui.edit.as_ref().expect("nested documents are editable");
        edit.editor.value(cx).to_string()
    });
    // The seed is shell syntax: the Int64 / Int32 / Date inside are visible and parse back.
    assert!(seed.contains("\"index\": Long('1')"), "{seed}");
    assert!(seed.contains("\"tier\": 2"), "{seed}");
    assert!(seed.contains("ISODate('"), "{seed}");
    // Unchanged: nothing is staged.
    app.update(visual, |this, cx| this.commit_cell_detail_editor(this.active_id(), cx));
    app.read_with(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.mutations.len(), 0);
    });

    // Change one member; the others keep their BSON types in the staged write.
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), CellPosition { row: 1, column: nested_column });
        this.refresh_cell_detail(this.active_id());
        cx.notify();
    });
    app.update_in(visual, |this, window, cx| this.open_cell_detail_editor(this.active_id(), window, cx));
    let edited = seed.replace("\"ok\": true", "\"ok\": false");
    assert_ne!(edited, seed);
    app.update_in(visual, |this, window, cx| {
        this.av()
            .detail_ui
            .edit
            .as_ref()
            .expect("editor open")
            .editor
            .set_value(&edited, window, cx);
        this.commit_cell_detail_editor(this.active_id(), cx);
    });
    app.read_with(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let mutations = this.workspace.runtime(tab_id).unwrap().mongo.mutations.active();
        assert_eq!(mutations.len(), 1, "{:?}", this.av().detail_ui.error);
        let PendingMongoMutation::Update { request, .. } = &mutations[0] else {
            panic!("update")
        };
        let set = &request.payload["$set"]["nested"];
        assert_eq!(set["ok"], false);
        assert_eq!(set["index"], serde_json::json!({"$numberLong": "1"}));
        assert_eq!(set["owner"]["tier"], serde_json::json!({"$numberInt": "2"}));
        assert!(set["updatedAt"]["$date"]["$numberLong"].is_string());
        assert_eq!(
            request.identity["_id"]["$oid"].as_str().map(str::len),
            Some(24),
            "the filter is the document's _id"
        );
    });
}

#[gpui::test]
fn a_write_that_matched_nothing_keeps_the_change_staged_and_says_why(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let tab_id = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let row = this.mongo_row_fields(tab_id, 0).expect("row");
        let payload = InlineEditPayload::Mongo {
            native_type: "string".into(),
            identity: row.mutation_identity.clone(),
            database: this.workspace.active_tab().unwrap().database.clone(),
            collection: this.workspace.active_tab().unwrap().collection.clone(),
            field: "name".into(),
        };
        let mutation = build_inline_mongo_update(&payload, 0, "Grace").unwrap();
        this.stage_mongo_mutation(tab_id, mutation, cx);
        tab_id
    });
    let rejection = engine_driver_host::LoaderError::DriverRejected {
        code: "mutation.no_rows_affected".into(),
        message: "No document matched this _id: it was deleted, or its _id changed, since the page was read. Reload the results; the staged change was not applied.".into(),
        retryable: false,
    };
    app.update(visual, |this, cx| {
        this.finish_mongo_commit(tab_id, "conn", Err((0, DriverQueryError::Driver(rejection))), cx);
    });
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.mongo.mutations.len(), 1, "the change stays staged for a retry");
        match &runtime.mongo.commit_status {
            crate::mongo_result::MongoCommitStatus::Partial { committed, error } => {
                assert_eq!(*committed, 0);
                assert!(error.contains("deleted") && error.contains("changed"), "{error}");
            }
            other => panic!("{other:?}"),
        }
    });
}

#[gpui::test]
fn a_json_looking_string_is_edited_as_stored_and_never_reindented(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let stored = "{\"a\":1,   \"b\" : [1,2]}";
    let name_column = app.read_with(visual, |this, _| {
        (0..this.active_grid().column_count())
            .find(|column| this.active_grid().column_name(*column) == Some("name"))
            .expect("name column")
    });
    app.update(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().clone();
        let row = this.mongo_row_fields(tab.id, 0).expect("row");
        let payload = InlineEditPayload::Mongo {
            native_type: "string".into(),
            identity: row.mutation_identity.clone(),
            database: tab.database.clone(),
            collection: tab.collection.clone(),
            field: "name".into(),
        };
        let mutation = build_inline_mongo_update(&payload, 0, stored).unwrap();
        this.stage_mongo_mutation(tab.id, mutation, cx);
        this.select_grid_cell(tab.id, CellPosition { row: 0, column: name_column });
        this.open_cell_detail_tab(tab.id, CellDetailTab::Details, cx);
    });
    app.update_in(visual, |this, window, cx| this.open_cell_detail_editor(this.active_id(), window, cx));
    app.read_with(visual, |this, cx| {
        let edit = this.av().detail_ui.edit.as_ref().expect("editor");
        assert_eq!(edit.seed_text.as_ref(), stored, "seeded with the stored text");
        assert_eq!(edit.editor.value(cx).as_ref(), stored);
    });
    app.update(visual, |this, cx| this.commit_cell_detail_editor(this.active_id(), cx));
    app.read_with(visual, |this, _| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        let mutations = this.workspace.runtime(tab_id).unwrap().mongo.mutations.active();
        assert_eq!(mutations.len(), 1, "committing the untouched text stages nothing new");
        let PendingMongoMutation::Update { request, .. } = &mutations[0] else {
            panic!("update")
        };
        assert_eq!(request.payload["$set"]["name"], stored);
    });
}

#[test]
fn duplicate_and_insert_keep_every_numeric_type_whatever_its_sign_or_size() {
    use serde_json::json;
    let fields: Vec<_> = [
        ("_id", "object_id", json!({"$oid": "66d4f84d0000000000000003"})),
        ("neg_i32", "int32", json!({"$numberInt": "-5"})),
        ("small_i64", "int64", json!({"$numberLong": "5"})),
        ("neg_i64", "int64", json!({"$numberLong": "-5"})),
        ("big_i32", "int32", json!({"$numberInt": "2147483647"})),
        ("whole_double", "double", json!({"$numberDouble": "5.0"})),
        ("nan", "double", json!({"$numberDouble": "NaN"})),
        ("dec", "decimal128", json!({"$numberDecimal": "5"})),
        ("doc", "document", json!({"a": {"$numberLong": "1"}, "b": [{"$numberInt": "2"}]})),
    ]
    .into_iter()
    .map(|(name, native_type, value)| engine_driver_api::RowField {
        name: name.into(),
        native_type: native_type.into(),
        display_text: String::new(),
        value,
        editable: true,
    })
    .collect();
    let text = mongo_duplicate_text(&fields).expect("duplicate text");
    assert!(!text.contains("\"_id\""), "{text}");
    // The Insert dialog reads it back the way it does the user's typing.
    let parsed = engine_mongo_script::shell_value::parse_document(
        &text,
        engine_mongo_script::shell_value::NumberTyping::Mongosh,
    )
    .unwrap();
    let canonical = bson::Bson::Document(parsed).into_canonical_extjson();
    for field in fields.iter().skip(1) {
        assert_eq!(canonical[&field.name], field.value, "{}\n{text}", field.name);
    }
}
