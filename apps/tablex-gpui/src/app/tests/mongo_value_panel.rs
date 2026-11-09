// The Value panel of a Mongo find result. The row fields behind a selected cell
// are read from the driver on a background executor AFTER the selection (the
// frame notices it), so the panel built at selection time must be rebuilt when
// the read lands: it must never keep the grid's `<object>` placeholder as "the
// value". Driven by real clicks on a real host session (the mock driver for the
// default tests, the real MongoDB driver for the `#[ignore]`d live one).
//
// Live run (container `tablex-test-mongo`, a scratch database that is dropped):
//
//   (cd rust-engine && cargo build -p dbengine-mongo --features abi-v2 -j 4)
//   TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:57017 \
//   TABLEX_TEST_MONGO_DYLIB=$PWD/rust-engine/target/debug/libdbengine_mongo.dylib \
//     cargo test -p tablex-gpui -j 4 live_mongo_value_panel -- --ignored --test-threads=1

/// The result grid of find tab `tab` with the Details panel open and the cell
/// `(row, column)` clicked, the way a user does it.
fn open_details_and_click_cell(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab: u64,
    row: usize,
    column: usize,
) {
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(900.0)));
    settle(visual);
    click_debug_button(visual, "cell-detail-rail-details");
    settle(visual);
    let selector: &'static str = Box::leak(format!("grid-body-cell-{row}-{column}").into_boxed_str());
    click_debug_button(visual, selector);
    app.read_with(visual, |this, _| {
        let selection = this.view(tab).grid_ui.selection.borrow().range.map(|range| range.start);
        assert_eq!(selection, Some(CellPosition { row, column }), "the click selected the cell");
    });
}

fn panel_detail(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> CellDetail {
    app.read_with(visual, |this, _| this.selected_cell_detail(tab).cloned().expect("the panel has a detail"))
}

#[gpui::test]
fn the_value_panel_is_rebuilt_from_the_row_fields_when_the_read_lands(cx: &mut TestAppContext) {
    let (app, visual, tab, _session) = find_tab_on_real_session(cx);
    app.update(visual, |this, _| this.av().grid_ui.selection.borrow_mut().clear());
    // Row 0, `_id`: the panel is built at the click, before the frame has
    // started the background read of the row.
    open_details_and_click_cell(&app, visual, tab, 0, 0);
    wait_until(visual, "the selection's row fields", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.mongo_row_fields(tab, 0).is_ok())
    });
    settle(visual);
    let (detail, field) = app.read_with(visual, |this, _| {
        let fields = this.mongo_row_fields(tab, 0).unwrap();
        (this.selected_cell_detail(tab).cloned().unwrap(), fields.fields[0].clone())
    });
    assert_eq!(detail.type_name, "object_id", "the driver's native type, not the grid's guess");
    assert_eq!(&*detail.value, mongo_detail_value(&field).as_str());
}

#[gpui::test]
fn an_inspect_row_failure_is_logged_not_swallowed_and_does_not_repeat_per_frame(cx: &mut TestAppContext) {
    let (app, visual, tab, _session) = find_tab_on_real_session(cx);
    // The mock cannot inspect the pages of the object `inspect_fails`.
    let page = app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab).unwrap().collection = "inspect_fails".into();
        let page = this.workspace.runtime(tab).unwrap().driver_page_id;
        this.run_tab_query(tab, cx);
        page
    });
    wait_until(visual, "the re-run to finish", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().driver_page_id != page)
    });
    let logged = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .filter(|entry| entry.area == crate::error_log::Area::Query && entry.message.contains("mock inspect failure"))
                .map(|entry| (entry.message.to_string(), entry.repeat))
                .collect::<Vec<_>>()
        })
    };
    wait_until(visual, "the failed row read to be logged", |visual| {
        settle(visual);
        !logged(&app, visual).is_empty()
    });
    for _ in 0..20 {
        settle(visual);
    }
    let entries = logged(&app, visual);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(entries[0].0.contains("Could not read row 1"), "{entries:?}");
    // A selection is read, and retried once; frames do not read again.
    assert!(entries[0].1 <= 2, "{entries:?}");
}

/// A document of the shape of a logged request: nested documents, an array of
/// documents, nulls, an empty document.
fn log_document(n: i32) -> serde_json::Value {
    serde_json::json!({
        "retries": { "$numberInt": (n * 3).to_string() },
        "data": {
            "body": {
                "type": format!("t{n}"), "amount": { "$numberInt": (n * 100).to_string() },
                "item": [{ "itemId": format!("i{n}"), "itemPrice": { "$numberInt": "5" } }],
                "customer": null, "note": null, "state": "ok", "channel": "web",
                "summary": format!("order {n}"), "refunded": { "$numberInt": "0" },
                "parties": { "buyer": { "code": "p" }, "seller": { "id": "x" }, "carrier": {} },
            },
            "meta": { "tag": format!("s{n}"), "at": "now" },
            "message": format!("m{n}"),
        },
        "extra": { "a": { "$numberInt": n.to_string() }, "b": [{ "k": 1 }, { "k": "two" }] },
        "logs": [format!("log {n}"), "second"],
    })
}

/// Opens a find tab with SORT {"_id":1} on a scratch collection of 20 documents
/// of the REAL MongoDB driver (`TABLEX_TEST_MONGO_URI` / `_DYLIB`), runs it and
/// returns the tab. The collection is dropped when `_guard` goes out of scope.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_MONGO_URI (127.0.0.1:57017) and TABLEX_TEST_MONGO_DYLIB"]
fn live_mongo_value_panel_shows_the_document_of_a_sorted_find(cx: &mut TestAppContext) {
    let (Ok(uri), Ok(dylib)) = (std::env::var("TABLEX_TEST_MONGO_URI"), std::env::var("TABLEX_TEST_MONGO_DYLIB")) else {
        panic!("TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB are not set");
    };
    assert!(uri.contains("127.0.0.1:57017"), "refusing to touch {uri}");
    let database = format!("tablex_value_panel_{}", uuid::Uuid::new_v4().to_simple());
    let (app, visual) = script_app(cx);
    visual.dispatcher.allow_parking();
    let connection = crate::ui_fixture::MONGO_ID;
    let session = app.update(visual, |this, _| {
        this.scenario_ui.service = None;
        let handle = engine_driver_host::DriverLoader.load(std::path::Path::new(&dylib)).expect("mongo driver loads");
        let session = handle
            .connect(&serde_json::json!({ "uri": uri, "name": "live" }).to_string())
            .expect("connect to the local MongoDB");
        let engine_id = session.connection_id();
        this.driver_runtime.capabilities.insert(connection.into(), std::sync::Arc::new(handle.capabilities().clone()));
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        this.driver_runtime.sessions.insert(connection.into(), session.clone());
        this.driver_runtime.connection_ids.insert(connection.into(), engine_id);
        this.driver_ui.handles.insert("mongodb".into(), handle);
        session
    });
    let write = |kind: &str, object: &str, payload: serde_json::Value| {
        let operation = session.begin_operation().expect("operation");
        let request = engine_driver_api::MutationRequest {
            operation_id: operation.id(),
            kind: kind.into(),
            database: Some(database.clone()),
            object: Some(object.into()),
            identity: serde_json::Value::Null,
            payload,
        };
        session.mutate(&operation, request).map(|_| ())
    };
    // `_id`s of one type in a known order: the sort {"_id":1} is the insert order.
    for n in 0..20 {
        let mut document = log_document(n);
        document["_id"] = serde_json::json!({ "$numberInt": n.to_string() });
        write("insert", "logs", document).expect("insert");
    }
    struct Drop_<'a>(&'a dyn Fn() -> Result<(), String>);
    impl Drop for Drop_<'_> {
        fn drop(&mut self) {
            let _ = (self.0)();
        }
    }
    let cleanup = || write("drop_collection", "logs", serde_json::Value::Null).map_err(|error| error.to_string());
    let _guard = Drop_(&cleanup);

    let tab = app.update(visual, |this, _| {
        let id = this.workspace.open_query_tab_with_metadata(
            "logs",
            crate::workspace::TabMetadata {
                source_id: None,
                connection_id: connection.into(),
                database: database.clone(),
                schema: None,
                collection: "logs".into(),
                query_mode: Some(QueryMode::MongoFind),
                is_pinned: false,
            },
        );
        this.workspace.runtime_mut(id).unwrap().mongo.sort = r#"{"_id":1}"#.into();
        id
    });
    app.update(visual, |this, cx| this.run_tab_query(tab, cx));
    wait_until(visual, "the find to finish", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().driver_page_id.is_some())
    });
    let (data, extra) = app.read_with(visual, |this, _| {
        let column = |name: &str| (0..this.grid(tab).column_count()).find(|c| this.grid(tab).column_name(*c) == Some(name)).unwrap();
        (column("data"), column("extra"))
    });
    open_details_and_click_cell(&app, visual, tab, 8, data);
    wait_until(visual, "the row's fields", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.mongo_row_fields(tab, 8).is_ok())
    });
    settle(visual);
    let detail = panel_detail(&app, visual, tab);
    assert_eq!(detail.type_name, "document");
    assert_ne!(&*detail.value, "<object>", "the grid placeholder is not the value");
    assert_eq!(detail.length, detail.value.chars().count());
    let parsed = engine_mongo_script::shell_value::parse_document(&detail.value, engine_mongo_script::shell_value::NumberTyping::Mongosh)
        .expect("the panel text is the document in shell syntax");
    assert_eq!(parsed.get_document("body").unwrap().get_str("type"), Ok("t8"));
    assert!(detail.formatted_json.as_deref().is_some_and(|json| json.contains("\"message\"") && json.contains("m8")));

    // Moving to another cell while the panel is open shows that cell's document.
    let selector: &'static str = Box::leak(format!("grid-body-cell-9-{extra}").into_boxed_str());
    click_debug_button(visual, selector);
    wait_until(visual, "the next row's fields", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.mongo_row_fields(tab, 9).is_ok())
    });
    settle(visual);
    let detail = panel_detail(&app, visual, tab);
    assert_eq!(detail.column_name, "extra");
    assert!(!detail.value.contains("<object>") && !detail.value.contains("<array"), "{}", detail.value);
}
