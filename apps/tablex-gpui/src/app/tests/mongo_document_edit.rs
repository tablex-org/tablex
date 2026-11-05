// Editing the selected document of the Mongo Document view: Edit / Save /
// Cancel through the real header buttons and keys, on the Mongo fixture.

/// The fixture Mongo tab in the Document view with document 1 selected.
fn document_edit_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.update(|window, _| window.activate_window());
    let tab_id = app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.runtime_mut(tab_id).unwrap().mongo.view =
            crate::mongo_result::MongoResultView::Document;
        this.select_mongo_document(tab_id, 1, cx);
        tab_id
    });
    settle(visual);
    (app, visual, tab_id)
}

fn open_document_editor(visual: &mut gpui::VisualTestContext) {
    click_selector(visual, "mongo-document-edit");
}

fn document_editor_state(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
) -> gpui::Entity<EditorState> {
    app.read_with(visual, |this, _| {
        this.view(tab_id)
            .mongo
            .document_edit
            .as_ref()
            .expect("the document editor is open")
            .editor
            .clone()
    })
}

fn document_editor_text(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
) -> String {
    let editor = document_editor_state(app, visual, tab_id);
    editor.read_with(visual, |state, _| state.value().to_string())
}

fn set_document_editor_text(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
    text: String,
) {
    let editor = document_editor_state(app, visual, tab_id);
    editor.update_in(visual, |state, window, cx| state.set_value(text, window, cx));
    settle(visual);
}

fn document_edit_open(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
) -> bool {
    app.read_with(visual, |this, _| this.view(tab_id).mongo.document_edit.is_some())
}

fn staged_mongo(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
) -> Vec<crate::mongo_result::PendingMongoMutation> {
    app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().mongo.mutations.active().to_vec()
    })
}

fn document_edit_error(visual: &mut gpui::VisualTestContext) -> bool {
    visual.debug_bounds("mongo-document-edit-error").is_some()
}

/// The JSON viewer's text (view mode, staged edits applied).
fn document_viewer_text(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
) -> String {
    app.read_with(visual, |this, cx| {
        this.view(tab_id).json_views.mongo_doc.as_ref().unwrap().state.read(cx).value().to_string()
    })
}

/// Tree rows laid out (the fixture document is short enough to fit).
fn tree_rows(visual: &mut gpui::VisualTestContext) -> usize {
    (0..64)
        .filter(|index| {
            let selector: &'static str = Box::leak(format!("mongo-tree-row-{index}").into_boxed_str());
            visual.debug_bounds(selector).is_some()
        })
        .count()
}

/// The seeded text with `name` renamed, `extra` added and `nullable`/`active` removed.
fn edited_document_text(seed: &str) -> String {
    seed.replace("\"mongo-customer-001\"", "\"renamed\"")
        .lines()
        .filter(|line| !line.contains("\"nullable\":") && !line.contains("\"active\":"))
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("{\n", "{\n  \"extra\": 7,\n", 1)
}

#[gpui::test]
fn edit_opens_the_document_in_a_code_editor_and_save_stages_one_change_per_field(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    assert!(visual.debug_bounds("mongo-document-read-only").is_none(), "an editable document says so");
    assert!(visual.debug_bounds("mongo-document-save").is_none());
    app.update(visual, |this, cx| this.toggle_mongo_document_view(tab_id, cx));
    settle(visual);
    let rows_before = tree_rows(visual);
    assert!(rows_before > 5, "the Tree view lists the document's fields");
    // Edit starts from the Tree view too.
    open_document_editor(visual);
    assert!(document_edit_open(&app, visual, tab_id));
    click_selector(visual, "mongo-document-cancel");
    app.update(visual, |this, cx| this.toggle_mongo_document_view(tab_id, cx));
    settle(visual);

    open_document_editor(visual);

    // The editor holds the document; the header swaps Edit for Save / Cancel.
    assert!(visual.debug_bounds("mongo-document-editor").is_some());
    assert!(visual.debug_bounds("mongo-document-edit").is_none());
    assert!(visual.debug_bounds("mongo-document-save").is_some());
    assert!(visual.debug_bounds("mongo-document-cancel").is_some());
    assert!(visual.debug_bounds("mongo-json-editor").is_none(), "the read-only viewer gave way");
    let seed = document_editor_text(&app, visual, tab_id);
    assert!(seed.contains("\"name\": \"mongo-customer-001\""), "{seed}");
    assert!(seed.contains("ObjectId('"), "the default Shell mode: {seed}");
    app.read_with(visual, |this, cx| {
        let editor = this.view(tab_id).mongo.document_edit.as_ref().unwrap().editor.read(cx);
        assert_eq!(editor.language_name().as_ref(), "json");
        assert!(editor.is_editable());
    });

    set_document_editor_text(&app, visual, tab_id, edited_document_text(&seed));
    click_selector(visual, "mongo-document-save");

    assert!(!document_edit_open(&app, visual, tab_id), "Save closes the editor");
    assert!(visual.debug_bounds("mongo-document-edit").is_some());
    let staged = staged_mongo(&app, visual, tab_id);
    let identity = serde_json::json!({"_id": {"$oid": "000000000000000000000002"}});
    let mut seen = Vec::new();
    for mutation in &staged {
        let request = mutation.request();
        assert_eq!(request.kind, "update");
        assert_eq!(request.identity, identity, "filtered on the _id alone");
        assert_eq!(request.operation_id, crate::mongo_result::STAGED_OPERATION_ID);
        seen.push(request.payload.to_string());
    }
    seen.sort();
    assert_eq!(
        seen,
        [
            r#"{"$set":{"extra":{"$numberInt":"7"}}}"#,
            r#"{"$set":{"name":"renamed"}}"#,
            r#"{"$unset":{"active":""}}"#,
            r#"{"$unset":{"nullable":""}}"#,
        ],
        "one $set per changed or added field, one $unset per removed one, nothing else"
    );

    // The grid's pending cells show them (`extra` is not a column of the page).
    app.read_with(visual, |this, _| {
        let grid = &this.workspace.runtime(tab_id).unwrap().grid;
        let cell = |name: &str| {
            let column = (0..grid.column_count()).find(|c| grid.column_name(*c) == Some(name))?;
            this.pending_cell_text(tab_id, crate::grid::CellPosition { row: 1, column })
                .map(|text| text.to_string())
        };
        assert_eq!(cell("name").as_deref(), Some("renamed"));
        assert_eq!(cell("nullable").as_deref(), Some("(unset)"));
        assert_eq!(cell("active").as_deref(), Some("(unset)"));
        assert_eq!(cell("count"), None, "an untouched field stays untouched");
    });

    // The JSON view shows the edited document ...
    let json = document_viewer_text(&app, visual, tab_id);
    assert!(json.contains("\"name\": \"renamed\"") && json.contains("\"extra\": 7"), "{json}");
    assert!(!json.contains("\"nullable\"") && !json.contains("\"active\""), "{json}");
    // ... and so does the Tree view: `extra` added, two fields gone.
    app.update(visual, |this, cx| this.toggle_mongo_document_view(tab_id, cx));
    settle(visual);
    assert_eq!(tree_rows(visual), rows_before - 1);
    app.update(visual, |this, cx| this.toggle_mongo_document_view(tab_id, cx));
    settle(visual);

    // Re-opening seeds the document as it now stands, staged edits included.
    open_document_editor(visual);
    let reopened = document_editor_text(&app, visual, tab_id);
    assert!(reopened.contains("\"name\": \"renamed\"") && !reopened.contains("\"nullable\""), "{reopened}");
    click_selector(visual, "mongo-document-save");
    assert_eq!(staged_mongo(&app, visual, tab_id).len(), 4, "an unchanged save stages nothing more");

    // One Save is one undo step: one Undo reverts the four staged changes, one Redo brings them all back.
    click_selector(visual, "mongo-undo");
    assert!(staged_mongo(&app, visual, tab_id).is_empty(), "one Undo takes the whole Save back");
    click_selector(visual, "mongo-redo");
    assert_eq!(staged_mongo(&app, visual, tab_id).len(), 4, "one Redo brings the whole Save back");
    click_selector(visual, "mongo-undo");
    assert!(staged_mongo(&app, visual, tab_id).is_empty());
    let json = document_viewer_text(&app, visual, tab_id);
    assert!(json.contains("\"name\": \"mongo-customer-001\"") && json.contains("\"nullable\""), "{json}");
    app.read_with(visual, |this, _| {
        let column = {
            let grid = &this.workspace.runtime(tab_id).unwrap().grid;
            (0..grid.column_count()).find(|c| grid.column_name(*c) == Some("name")).unwrap()
        };
        assert_eq!(this.pending_cell_text(tab_id, crate::grid::CellPosition { row: 1, column }), None);
    });
}

#[gpui::test]
fn a_refused_save_keeps_the_editor_open_and_says_why(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);

    // Typed by hand: invalid JSON.
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("{ \"name\": ");
    settle(visual);
    click_selector(visual, "mongo-document-save");
    assert!(document_edit_open(&app, visual, tab_id));
    assert!(document_edit_error(visual), "the parse error is shown in the panel");
    app.read_with(visual, |this, _| {
        let error = this.view(tab_id).mongo.document_edit.as_ref().unwrap().error.clone().unwrap();
        assert!(error.contains("Not a valid document"), "{error}");
    });
    assert!(staged_mongo(&app, visual, tab_id).is_empty());

    // Not an object, and a duplicate key.
    for (text, expected) in [
        ("[1, 2]".to_string(), "Not a valid document"),
        (seed.replacen("\"name\":", "\"count\": 1,\n  \"count\": 2,\n  \"name\":", 1), "duplicate field \"count\""),
    ] {
        set_document_editor_text(&app, visual, tab_id, text);
        click_selector(visual, "mongo-document-save");
        let error = app.read_with(visual, |this, _| {
            this.view(tab_id).mongo.document_edit.as_ref().unwrap().error.clone().unwrap()
        });
        assert!(error.contains(expected), "{expected}: {error}");
    }

    // _id changed, and _id removed: Mongo cannot do either.
    let id_line = "ObjectId('000000000000000000000002')";
    assert!(seed.contains(id_line), "{seed}");
    for text in [
        seed.replace(id_line, "ObjectId('0000000000000000000000ff')"),
        seed.lines().filter(|line| !line.contains("\"_id\":")).collect::<Vec<_>>().join("\n"),
    ] {
        set_document_editor_text(&app, visual, tab_id, text);
        click_selector(visual, "mongo-document-save");
        assert!(document_edit_open(&app, visual, tab_id));
        let error = app.read_with(visual, |this, _| {
            this.view(tab_id).mongo.document_edit.as_ref().unwrap().error.clone().unwrap()
        });
        assert!(error.contains("_id cannot be changed"), "{error}");
        assert!(document_edit_error(visual));
        assert!(staged_mongo(&app, visual, tab_id).is_empty(), "nothing is staged by a refused save");
    }

    // Fixing the text clears the refusal: the next save goes through.
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"fixed\""));
    click_selector(visual, "mongo-document-save");
    assert!(!document_edit_open(&app, visual, tab_id));
    assert_eq!(staged_mongo(&app, visual, tab_id).len(), 1);
}

#[gpui::test]
fn a_save_without_changes_stages_nothing_and_cancel_and_escape_discard(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);

    // Untouched text: Save just closes.
    open_document_editor(visual);
    click_selector(visual, "mongo-document-save");
    assert!(!document_edit_open(&app, visual, tab_id));
    assert!(staged_mongo(&app, visual, tab_id).is_empty());

    // Different formatting of the same document is no change either.
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    set_document_editor_text(&app, visual, tab_id, seed.lines().map(str::trim).collect::<Vec<_>>().join(" "));
    click_selector(visual, "mongo-document-save");
    assert!(!document_edit_open(&app, visual, tab_id));
    assert!(staged_mongo(&app, visual, tab_id).is_empty());

    // Cancel drops the text ...
    open_document_editor(visual);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"cancelled\""));
    click_selector(visual, "mongo-document-cancel");
    assert!(!document_edit_open(&app, visual, tab_id));
    assert!(staged_mongo(&app, visual, tab_id).is_empty());
    assert!(!document_viewer_text(&app, visual, tab_id).contains("cancelled"));

    // ... and so does Escape, with the editor focused.
    open_document_editor(visual);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"escaped\""));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!document_edit_open(&app, visual, tab_id), "Escape cancels");
    assert!(staged_mongo(&app, visual, tab_id).is_empty());
    assert!(visual.debug_bounds("mongo-document-edit").is_some());
    assert!(visual.debug_bounds("mongo-json-editor").is_some(), "the viewer is back");
    open_document_editor(visual);
    assert_eq!(document_editor_text(&app, visual, tab_id), seed, "a new editor starts from the document, not the discarded text");
}

#[gpui::test]
fn the_save_chord_saves_the_document_from_the_focused_editor(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"chorded\""));
    visual.simulate_keystrokes("secondary-s");
    settle(visual);
    assert!(!document_edit_open(&app, visual, tab_id), "Cmd+S saves");
    let staged = staged_mongo(&app, visual, tab_id);
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].request().payload, serde_json::json!({"$set": {"name": "chorded"}}));
}

#[gpui::test]
fn the_editor_starts_from_the_canonical_text_in_canonical_mode_and_reads_it_back(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    app.update(visual, |this, _| {
        this.view_mut(tab_id).mongo.document_json_mode = crate::cell_value::EjsonMode::Canonical;
    });
    settle(visual);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    assert!(seed.contains("\"$oid\": \"000000000000000000000002\"") && seed.contains("$numberLong"), "{seed}");
    // The mode cannot change under the text.
    assert!(visual.debug_bounds("mongo-json-mode").is_some());
    click_selector(visual, "mongo-document-save");
    assert!(staged_mongo(&app, visual, tab_id).is_empty(), "canonical text reads back unchanged");
    open_document_editor(visual);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"name\": \"mongo-customer-001\"", "\"name\": {\"$numberInt\": \"5\"}"));
    click_selector(visual, "mongo-document-save");
    let staged = staged_mongo(&app, visual, tab_id);
    assert_eq!(staged[0].request().payload, serde_json::json!({"$set": {"name": {"$numberInt": "5"}}}));
}

#[gpui::test]
fn while_the_editor_is_open_the_list_the_views_and_the_table_switch_are_locked(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"unsaved\""));

    click_debug(visual, "mongo-document-row-3");
    click_selector(visual, "mongo-view-table");
    click_selector(visual, "mongo-document-view");
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.mongo.view, crate::mongo_result::MongoResultView::Document, "no Table switch");
        assert_eq!(runtime.mongo.document_view, crate::mongo_result::MongoDocumentView::Json, "no Tree switch");
        assert_eq!(runtime.mongo.inspector.selected.unwrap().row_index, 1, "no other document");
        let message = this.view(tab_id).detail_ui.error.clone().unwrap();
        assert!(message.contains("Save or cancel"), "{message}");
    });
    assert!(document_edit_open(&app, visual, tab_id));
    assert!(document_editor_text(&app, visual, tab_id).contains("unsaved"), "the text is still there");

    // Another page of results (the document is gone) leaves the editor behind.
    app.update(visual, |this, _| {
        this.workspace.runtime_mut(tab_id).unwrap().mongo.inspector.reset();
    });
    settle(visual);
    assert!(!document_edit_open(&app, visual, tab_id));
}

#[gpui::test]
fn a_read_only_connection_or_a_document_without_a_write_identity_shows_read_only(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    assert!(visual.debug_bounds("mongo-document-edit").is_some());

    // No _id the driver can address (a projection left it out).
    app.update(visual, |this, _| {
        let inspector = &mut this.workspace.runtime_mut(tab_id).unwrap().mongo.inspector;
        std::sync::Arc::make_mut(inspector.snapshot.as_mut().unwrap()).mutation_identity = None;
    });
    settle(visual);
    assert!(visual.debug_bounds("mongo-document-read-only").is_some());
    assert!(visual.debug_bounds("mongo-document-edit").is_none(), "no Edit button");
    app.read_with(visual, |this, _| {
        let super::results::mongo::document_edit::DocumentEditability::ReadOnly(Some(reason)) =
            this.mongo_document_editability(tab_id)
        else {
            panic!("read only with a reason");
        };
        assert!(reason.contains("no _id"), "the tooltip says why: {reason}");
    });
    app.update_in(visual, |this, window, cx| this.start_mongo_document_edit(tab_id, window, cx));
    assert!(!document_edit_open(&app, visual, tab_id), "and no editor by any other route");

    // A read-only connection.
    let (app, visual, tab_id) = {
        let (app, visual, tab_id) = (app, visual, tab_id);
        select_document_again(&app, visual, tab_id);
        (app, visual, tab_id)
    };
    publish_types(&app, visual, false);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    give_profile(&app, visual, &connection, typed("replica"));
    settle(visual);
    assert!(visual.debug_bounds("mongo-document-read-only").is_some());
    assert!(visual.debug_bounds("mongo-document-edit").is_none(), "no Edit button on a read-only connection");
    app.read_with(visual, |this, _| {
        let super::results::mongo::document_edit::DocumentEditability::ReadOnly(Some(reason)) =
            this.mongo_document_editability(tab_id)
        else {
            panic!("read only with a reason");
        };
        assert!(reason.contains("read-only") && reason.contains("Replica"), "{reason}");
    });
    app.update_in(visual, |this, window, cx| this.start_mongo_document_edit(tab_id, window, cx));
    assert!(!document_edit_open(&app, visual, tab_id));
}

fn select_document_again(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) {
    app.update(visual, |this, cx| this.select_mongo_document(tab_id, 1, cx));
    settle(visual);
}

#[gpui::test]
fn a_document_staged_for_deletion_is_read_only(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    app.update(visual, |this, cx| {
        let identity = serde_json::json!({"_id": {"$oid": "000000000000000000000002"}});
        let request = engine_driver_api::MutationRequest {
            operation_id: crate::mongo_result::STAGED_OPERATION_ID,
            kind: "delete".into(),
            database: Some("shop".into()),
            object: Some("invoices".into()),
            identity,
            payload: serde_json::Value::Null,
        };
        this.stage_mongo_mutation(tab_id, PendingMongoMutation::Delete { request, row: 1 }, cx);
    });
    settle(visual);
    assert!(visual.debug_bounds("mongo-document-read-only").is_some());
    assert!(visual.debug_bounds("mongo-document-edit").is_none());
}

#[gpui::test]
fn staged_document_edits_commit_as_set_and_unset_and_the_document_is_rewritten(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    set_document_editor_text(&app, visual, tab_id, edited_document_text(&seed));
    click_selector(visual, "mongo-document-save");
    assert_eq!(staged_mongo(&app, visual, tab_id).len(), 4);
    click_selector(visual, "mongo-commit");
    settle(visual);
    assert!(staged_mongo(&app, visual, tab_id).is_empty(), "the commit took all four");
    app.update(visual, |this, cx| this.select_mongo_document(tab_id, 1, cx));
    settle(visual);
    let document = app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().mongo.inspector.snapshot.as_ref().unwrap().document.clone()
    });
    assert_eq!(document["name"], "renamed");
    assert_eq!(document["extra"], serde_json::json!({"$numberInt": "7"}));
    assert!(document.get("nullable").is_none() && document.get("active").is_none(), "{document}");
    assert_eq!(document["count"], serde_json::json!({"$numberLong": "1"}), "untouched fields keep their type");
}

#[gpui::test]
fn the_edit_buttons_stay_inside_the_header_of_a_narrow_pane(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    for editing in [false, true] {
        if editing {
            open_document_editor(visual);
        }
        let controls: &[&str] = if editing {
            &["mongo-document-save", "mongo-document-cancel", "mongo-json-mode", "mongo-document-view"]
        } else {
            &["mongo-document-edit", "mongo-json-mode", "mongo-document-view"]
        };
        for width in [1100.0, 900.0, 820.0, 760.0] {
            visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
            settle(visual);
            let panel = visual.debug_bounds("mongo-document-panel").expect("document panel");
            let header = visual.debug_bounds("mongo-document-header").expect("header");
            for control in controls {
                let bounds = visual.debug_bounds(control).unwrap_or_else(|| panic!("{width}: {control}"));
                assert!(bounds.right() <= panel.right(), "{width}: {control} inside the panel");
                assert!(bounds.right() <= header.right(), "{width}: {control} inside the header");
                assert!(bounds.size.width > gpui::px(0.0), "{width}: {control} has width");
            }
        }
    }
    assert!(document_edit_open(&app, visual, tab_id));
}

/// Stages one single-field update on `row` of the fixture tab, like a grid cell edit does.
fn stage_field_update(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
    row: usize,
    field: &str,
    value: serde_json::Value,
) {
    app.update(visual, |this, cx| {
        let request = engine_driver_api::MutationRequest {
            operation_id: crate::mongo_result::STAGED_OPERATION_ID,
            kind: "update".into(),
            database: Some("fixture_db_0".into()),
            object: Some("fixture_collection_00".into()),
            identity: serde_json::json!({"_id": {"$oid": format!("{:024x}", row + 1)}}),
            payload: serde_json::json!({"$set": {field: value.clone()}}),
        };
        let display_text = if value.is_null() { "NULL".to_string() } else { value.to_string() };
        let mutation = PendingMongoMutation::Update {
            request,
            row,
            field: field.into(),
            value,
            display_text,
        };
        this.stage_mongo_mutation(tab_id, mutation, cx);
    });
    settle(visual);
}

fn document_status_error(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Option<String> {
    app.read_with(visual, |this, _| this.view(tab_id).detail_ui.error.clone())
}

#[gpui::test]
fn while_the_editor_is_open_commit_rollback_undo_redo_run_refresh_and_paging_are_refused(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    // A change staged from the grid (and one undone, so Redo has something to do).
    stage_field_update(&app, visual, tab_id, 1, "name", serde_json::json!("Grace"));
    stage_field_update(&app, visual, tab_id, 1, "nullable", serde_json::json!("x"));
    app.update(visual, |this, cx| this.undo_mongo_mutation(tab_id, cx));
    settle(visual);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    assert!(seed.contains("\"name\": \"Grace\""), "seeded with the staged edit: {seed}");
    let typed = seed.replace("\"Grace\"", "\"Typed\"");
    set_document_editor_text(&app, visual, tab_id, typed.clone());
    let generation = query_generation(&app, visual, tab_id);

    // The footer buttons that would act are disabled: real clicks do nothing.
    for selector in ["mongo-rollback", "mongo-commit", "mongo-undo", "mongo-redo", "page-next", "result-refresh"] {
        click_debug(visual, selector);
    }
    let unchanged = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        assert!(document_edit_open(app, visual, tab_id), "the editor stays open");
        assert_eq!(document_editor_text(app, visual, tab_id), typed, "its text is kept");
        let history = app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().mongo.mutations.clone());
        assert_eq!(history.len(), 1, "nothing was committed, rolled back or undone");
        assert!(history.can_redo(), "nothing was redone");
        assert_eq!(query_generation(app, visual, tab_id), generation, "nothing re-ran");
    };
    unchanged(&app, visual);

    // Whatever else reaches the same actions is refused with the reason.
    app.update(visual, |this, _| this.view_mut(tab_id).detail_ui.error = None);
    type Action<'a> = Box<dyn Fn(&mut AppRoot, &mut Window, &mut Context<AppRoot>) + 'a>;
    let actions: Vec<(&str, Action)> = vec![
        ("rollback", Box::new(|this, _, cx| this.rollback_mongo_mutations(tab_id, cx))),
        ("commit", Box::new(|this, _, cx| this.commit_mongo_mutations(tab_id, cx))),
        ("request commit", Box::new(|this, window, cx| this.request_commit_mongo(tab_id, window, cx))),
        ("undo", Box::new(|this, _, cx| this.undo_mongo_mutation(tab_id, cx))),
        ("redo", Box::new(|this, _, cx| this.redo_mongo_mutation(tab_id, cx))),
        ("run", Box::new(|this, _, cx| this.run_tab_query_explicit(tab_id, cx))),
        ("re-run", Box::new(|this, _, cx| this.run_tab_query(tab_id, cx))),
        ("refresh", Box::new(|this, window, cx| this.refresh_tab_results(tab_id, window, cx))),
        ("next page", Box::new(|this, _, cx| this.page_tab_query(tab_id, true, cx))),
        ("limit", Box::new(|this, _, cx| this.set_output_limit(tab_id, 100, cx))),
    ];
    for (name, action) in &actions {
        app.update(visual, |this, _| this.view_mut(tab_id).detail_ui.error = None);
        app.update_in(visual, |this, window, cx| action(this, window, cx));
        settle(visual);
        assert_eq!(
            document_status_error(&app, visual, tab_id).as_deref(),
            Some(super::results::mongo::document_edit::EDITING_MESSAGE),
            "{name} says why"
        );
        unchanged(&app, visual);
    }

    // Cancel clears the reminder; the actions work again.
    click_selector(visual, "mongo-document-cancel");
    assert_eq!(document_status_error(&app, visual, tab_id), None, "Cancel clears the reminder");
    click_selector(visual, "mongo-undo");
    assert!(staged_mongo(&app, visual, tab_id).is_empty(), "Undo works again");
}

#[gpui::test]
fn an_open_document_editor_counts_as_unsaved_when_the_tab_or_the_window_closes(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    let tabs = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.tabs().len())
    };
    let before = tabs(&app, visual);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"kept\""));

    // Closing the tab asks first, as for staged changes; Cancel keeps tab and text.
    app.update_in(visual, |this, window, cx| this.confirm_close_tab(tab_id, window, cx));
    settle(visual);
    assert!(visual.debug_bounds("discard-pending-confirm").is_some(), "the existing discard question");
    assert_eq!(tabs(&app, visual), before);
    click_debug(visual, "discard-pending-cancel");
    assert!(visual.debug_bounds("discard-pending-confirm").is_none());
    assert_eq!(tabs(&app, visual), before);
    assert!(document_editor_text(&app, visual, tab_id).contains("kept"));

    // So do closing the window and quitting.
    let may_close = app.update_in(visual, |this, window, cx| this.window_may_close(window, cx));
    settle(visual);
    assert!(!may_close, "the window does not close under an open editor without asking");
    assert!(visual.debug_bounds("discard-pending-confirm").is_some());
    click_debug(visual, "discard-pending-cancel");
    let quitting = app.update_in(visual, |this, window, cx| this.request_quit(window, cx));
    settle(visual);
    assert!(quitting, "quit asks");
    assert!(visual.debug_bounds("discard-pending-confirm").is_some());
    click_debug(visual, "discard-pending-cancel");
    assert!(document_edit_open(&app, visual, tab_id));

    // Without the editor there is nothing to ask.
    click_selector(visual, "mongo-document-cancel");
    assert!(app.update_in(visual, |this, window, cx| this.window_may_close(window, cx)));
    assert!(!app.update_in(visual, |this, window, cx| this.request_quit(window, cx)));
    app.update_in(visual, |this, window, cx| this.confirm_close_tab(tab_id, window, cx));
    settle(visual);
    assert!(visual.debug_bounds("discard-pending-confirm").is_none());
    assert_eq!(tabs(&app, visual), before - 1, "the tab closes at once");
}

#[gpui::test]
fn a_save_after_the_document_became_unwritable_is_refused_with_the_reason(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"late\""));
    // A delete of this very document is staged meanwhile.
    app.update(visual, |this, cx| {
        let request = engine_driver_api::MutationRequest {
            operation_id: crate::mongo_result::STAGED_OPERATION_ID,
            kind: "delete".into(),
            database: Some("fixture_db_0".into()),
            object: Some("fixture_collection_00".into()),
            identity: serde_json::json!({"_id": {"$oid": "000000000000000000000002"}}),
            payload: serde_json::Value::Null,
        };
        this.stage_mongo_mutation(tab_id, PendingMongoMutation::Delete { request, row: 1 }, cx);
    });
    click_selector(visual, "mongo-document-save");
    assert!(document_edit_open(&app, visual, tab_id), "the text is kept");
    assert!(document_edit_error(visual));
    let error = app.read_with(visual, |this, _| {
        this.view(tab_id).mongo.document_edit.as_ref().unwrap().error.clone().unwrap()
    });
    assert!(error.contains("staged for deletion"), "{error}");
    assert_eq!(staged_mongo(&app, visual, tab_id).len(), 1, "only the delete is staged");
}

#[gpui::test]
fn save_and_cancel_clear_the_reminder_and_return_focus_to_the_documents_view(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    let focused = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.update_in(visual, |this, window, cx| this.view_mut(tab_id).grid_focus(cx).is_focused(window))
    };
    for save in [false, true] {
        open_document_editor(visual);
        let seed = document_editor_text(&app, visual, tab_id);
        assert!(!focused(&app, visual), "the editor has the focus");
        if save {
            set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"x\""));
        }
        app.update(visual, |this, cx| this.undo_mongo_mutation(tab_id, cx));
        assert!(document_status_error(&app, visual, tab_id).is_some(), "a refused action leaves the reminder");
        click_selector(visual, if save { "mongo-document-save" } else { "mongo-document-cancel" });
        assert!(!document_edit_open(&app, visual, tab_id));
        assert_eq!(document_status_error(&app, visual, tab_id), None, "the reminder is gone (save: {save})");
        assert!(focused(&app, visual), "focus is back on the documents view (save: {save})");
    }
}

#[gpui::test]
fn a_staged_unset_cell_is_a_missing_field_for_the_editors_and_for_copy(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    let without = |field: &str| {
        seed.lines().filter(|line| !line.contains(&format!("\"{field}\":"))).collect::<Vec<_>>().join("\n")
    };
    set_document_editor_text(&app, visual, tab_id, without("nullable"));
    click_selector(visual, "mongo-document-save");
    let column = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, name: &str| {
        app.read_with(visual, |this, _| {
            let grid = this.grid(tab_id);
            (0..grid.column_count()).find(|c| grid.column_name(*c) == Some(name)).unwrap()
        })
    };
    let (nullable, name) = (column(&app, visual, "nullable"), column(&app, visual, "name"));
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.pending_cell_text(tab_id, CellPosition { row: 1, column: nullable }).as_deref(),
            Some("(unset)"),
            "the overlay still shows it"
        );
    });

    // The detail panel: missing, not "(unset)", not NULL.
    app.read_with(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap();
        let detail = this.build_cell_detail(tab, CellPosition { row: 1, column: nullable }).unwrap();
        assert_eq!(detail.value.as_ref(), "");
        assert_eq!(detail.type_name.as_str(), "missing");
        assert!(!detail.is_null);
        assert!(detail.editable);
    });

    // The grid's inline editor opens empty.
    app.update_in(visual, |this, window, cx| this.begin_inline_edit(tab_id, 1, nullable, window, cx));
    settle(visual);
    app.read_with(visual, |this, cx| {
        let edit = this.view(tab_id).grid_ui.inline_edit.as_ref().expect("the inline editor opened");
        assert_eq!(edit.editor.value(cx).as_str(), "");
        assert!(matches!(&edit.payload, InlineEditPayload::Mongo { native_type, .. } if native_type == "missing"));
    });
    app.update_in(visual, |this, window, cx| this.cancel_inline_edit(tab_id, window, cx));

    // The detail editor opens empty too, as for a missing field.
    app.update(visual, |this, cx| {
        this.select_grid_cell(tab_id, CellPosition { row: 1, column: nullable });
        this.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
    });
    settle(visual);
    app.update_in(visual, |this, window, cx| this.open_cell_detail_editor(tab_id, window, cx));
    app.read_with(visual, |this, cx| {
        let edit = this.view(tab_id).detail_ui.edit.as_ref().expect("the detail editor opened");
        assert_eq!(edit.editor.value(cx).as_str(), "");
        assert!(matches!(&edit.payload, InlineEditPayload::Mongo { native_type, .. } if native_type == "missing"));
    });
    app.update(visual, |this, cx| this.cancel_cell_detail_editor(tab_id, cx));

    // Copy: an unset field is a blank cell, copied as an empty string and never "(unset)".
    let (text, is_null) = copied_cell(&app, visual, tab_id, 1, nullable);
    assert_eq!((text.as_str(), is_null), ("", false));
    assert_eq!(copied_cell(&app, visual, tab_id, 1, name).0, "mongo-customer-001", "other cells copy as before");
    assert_eq!(clipboard_after_copy(&app, visual, tab_id, 1, nullable), "");
    // In a row copy the unset field is an empty column, in TSV and in JSON.
    let (tsv, json) = app.read_with(visual, |this, _| {
        let grid = this.grid(tab_id);
        let range = crate::grid::CellRange {
            start: CellPosition { row: 1, column: 0 },
            end: CellPosition { row: 1, column: grid.column_count() - 1 },
        };
        let data = this.collect_copy_context_range(tab_id, range).expect("a row with an unset field copies");
        (data.format(super::results::copy::CopyFormat::Tsv).unwrap(), data.format(super::results::copy::CopyFormat::Json).unwrap())
    });
    assert!(!tsv.contains("(unset)") && !json.contains("(unset)"), "{tsv} / {json}");
    let cells = tsv.trim_end_matches('\n').split('\t').collect::<Vec<_>>();
    assert_eq!(cells[nullable], "", "{tsv}");
    assert_eq!(cells[name], "mongo-customer-001");
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    // A JSON row leaves the missing field out (BSON null would be `null`).
    let row = json[0].as_object().unwrap();
    assert!(!row.contains_key("nullable") && row.contains_key("name"), "{row:?}");
}

/// The copied cell (`row`, `column`) of the tab's result: its plain text and whether it is NULL.
fn copied_cell(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
    row: usize,
    column: usize,
) -> (String, bool) {
    app.read_with(visual, |this, _| {
        let cell = CellPosition { row, column };
        let data = this
            .collect_copy_context_range(tab_id, crate::grid::CellRange { start: cell, end: cell })
            .expect("the cell copies");
        let cell = &data.rows[0][0];
        (cell.plain_text().to_string(), cell.is_null())
    })
}

/// What the clipboard holds after copying the single cell (`row`, `column`) over a "before" marker.
fn clipboard_after_copy(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab_id: u64,
    row: usize,
    column: usize,
) -> String {
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
    app.update(visual, |this, cx| {
        this.select_grid_cell(tab_id, CellPosition { row, column });
        this.copy_grid_selection(tab_id, cx);
    });
    visual.cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default()
}

/// Lets `ticks` seconds of the auto-refresh timer go by.
fn let_the_refresh_timer_run(visual: &mut gpui::VisualTestContext, ticks: u32) {
    for _ in 0..ticks {
        visual.executor().advance_clock(std::time::Duration::from_secs(1));
        visual.run_until_parked();
    }
    settle(visual);
}

#[gpui::test]
fn auto_refresh_leaves_an_open_document_editor_alone_and_goes_on_once_it_is_closed(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    app.update(visual, |this, cx| {
        // As after a real run: the tab's connection is up and the query on screen is the one in the bar.
        let connection = this.workspace.tab(tab_id).unwrap().connection_id.clone();
        let epoch = this.sessions.begin_connect(&connection).unwrap();
        this.sessions.finish_connect(&connection, epoch, Ok(()));
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        runtime.last_executed_query = Some(mongo_query_signature(runtime));
        this.set_auto_refresh_interval(tab_id, Some(1), cx);
    });
    visual.run_until_parked();

    // Closed editor: the timer refreshes the result (this is what the rest is measured against).
    let before = query_generation(&app, visual, tab_id);
    let_the_refresh_timer_run(visual, 2);
    let refreshed = query_generation(&app, visual, tab_id);
    assert!(refreshed > before, "the timer runs the query while nothing is being edited ({before} -> {refreshed})");

    // The editor is open with a typed change: five ticks go by, and nothing is run, replaced or lost. (A refresh replaces the
    // page, so the document is picked again each time.)
    select_document_again(&app, visual, tab_id);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    let typed = seed.replace("\"mongo-customer-001\"", "\"typed while the timer ticks\"");
    set_document_editor_text(&app, visual, tab_id, typed.clone());
    let rows_before = app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().grid.row_count());
    let_the_refresh_timer_run(visual, 5);
    assert_eq!(query_generation(&app, visual, tab_id), refreshed, "no refresh while the document is being edited");
    assert!(document_edit_open(&app, visual, tab_id), "the editor stays open");
    assert_eq!(document_editor_text(&app, visual, tab_id), typed, "and keeps the text typed in it");
    assert_eq!(app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().grid.row_count()), rows_before);
    assert!(staged_mongo(&app, visual, tab_id).is_empty());
    assert_eq!(document_status_error(&app, visual, tab_id), None, "the timer does not even ask: no \"finish editing\" message on every tick");

    // Cancel: the timer goes on.
    click_selector(visual, "mongo-document-cancel");
    assert!(!document_edit_open(&app, visual, tab_id));
    let_the_refresh_timer_run(visual, 2);
    let after_cancel = query_generation(&app, visual, tab_id);
    assert!(after_cancel > refreshed, "auto-refresh resumes after Cancel ({refreshed} -> {after_cancel})");

    // Save with nothing changed stages nothing: the timer goes on after it too.
    select_document_again(&app, visual, tab_id);
    open_document_editor(visual);
    let_the_refresh_timer_run(visual, 3);
    assert_eq!(query_generation(&app, visual, tab_id), after_cancel, "held again while the editor is open");
    click_selector(visual, "mongo-document-save");
    assert!(!document_edit_open(&app, visual, tab_id));
    let_the_refresh_timer_run(visual, 2);
    let after_save = query_generation(&app, visual, tab_id);
    assert!(after_save > after_cancel, "auto-refresh resumes after a Save that staged nothing ({after_cancel} -> {after_save})");

    // Save with a change: the staged changes hold the timer (as for any staged edit) until they are rolled back; then it goes on.
    select_document_again(&app, visual, tab_id);
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    set_document_editor_text(&app, visual, tab_id, seed.replace("\"mongo-customer-001\"", "\"saved\""));
    click_selector(visual, "mongo-document-save");
    assert_eq!(staged_mongo(&app, visual, tab_id).len(), 1);
    let_the_refresh_timer_run(visual, 3);
    assert_eq!(query_generation(&app, visual, tab_id), after_save, "staged changes are never refreshed away");
    assert_eq!(staged_mongo(&app, visual, tab_id).len(), 1);
    app.update(visual, |this, cx| this.rollback_mongo_mutations(tab_id, cx));
    let_the_refresh_timer_run(visual, 2);
    assert!(query_generation(&app, visual, tab_id) > after_save, "and the timer goes on once they are rolled back");
}

#[gpui::test]
fn a_cell_of_a_field_the_document_lacks_copies_as_an_empty_string_and_a_null_stays_null(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = document_edit_app(cx);
    let nullable = app.read_with(visual, |this, _| {
        let grid = this.grid(tab_id);
        (0..grid.column_count()).find(|c| grid.column_name(*c) == Some("nullable")).unwrap()
    });
    // Fixture document 3 has no `nullable` field, document 5 holds a BSON null, document 1 a string.
    let missing = crate::ui_fixture::MONGO_DOCUMENT_WITHOUT_NULLABLE;
    assert_eq!(copied_cell(&app, visual, tab_id, missing, nullable), (String::new(), false), "missing: blank, not NULL");
    assert_eq!(clipboard_after_copy(&app, visual, tab_id, missing, nullable), "", "the clipboard gets an empty string");
    assert_eq!(copied_cell(&app, visual, tab_id, 5, nullable), (String::new(), true), "a BSON null is still NULL");
    assert_eq!(copied_cell(&app, visual, tab_id, 1, nullable), ("present".to_string(), false));
    // A row copy: the missing field is an empty TSV column and absent from JSON; the null is `null`.
    let rows = app.read_with(visual, |this, _| {
        let range = |row| crate::grid::CellRange {
            start: CellPosition { row, column: 0 },
            end: CellPosition { row, column: nullable },
        };
        let copy = |row| this.collect_copy_context_range(tab_id, range(row)).unwrap();
        let (gap, null) = (copy(missing), copy(5));
        let tsv = gap.format(super::results::copy::CopyFormat::Tsv).unwrap();
        let json = |data: &super::results::copy::CopyData| {
            serde_json::from_str::<serde_json::Value>(&data.format(super::results::copy::CopyFormat::Json).unwrap()).unwrap()
        };
        (tsv, json(&gap), json(&null))
    });
    assert!(rows.0.ends_with('\t'), "an empty last column, not \"\" or NULL: {:?}", rows.0);
    assert!(rows.1[0].get("nullable").is_none(), "{}", rows.1);
    assert!(rows.2[0]["nullable"].is_null(), "{}", rows.2);
}

#[gpui::test]
fn the_latest_staged_change_wins_for_detail_and_copy(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = document_edit_app(cx);
    // `$set` NULL, then a document edit removes the field: missing, not NULL.
    stage_field_update(&app, visual, tab_id, 1, "name", serde_json::Value::Null);
    let name = app.read_with(visual, |this, _| {
        let grid = this.grid(tab_id);
        (0..grid.column_count()).find(|c| grid.column_name(*c) == Some("name")).unwrap()
    });
    let detail = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            let tab = this.workspace.tab(tab_id).unwrap();
            let detail = this.build_cell_detail(tab, CellPosition { row: 1, column: name }).unwrap();
            (detail.is_null, detail.type_name.to_string(), detail.value.to_string())
        })
    };
    assert!(detail(&app, visual).0, "a staged NULL is NULL");
    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    assert!(seed.contains("\"name\": null"), "{seed}");
    set_document_editor_text(
        &app,
        visual,
        tab_id,
        seed.lines().filter(|line| !line.contains("\"name\":")).collect::<Vec<_>>().join("\n"),
    );
    click_selector(visual, "mongo-document-save");
    assert_eq!(detail(&app, visual), (false, "missing".to_string(), String::new()), "removed, not NULL");
    assert_eq!(copied_cell(&app, visual, tab_id, 1, name), (String::new(), false), "copy sees a missing field, not a NULL");
    assert_eq!(clipboard_after_copy(&app, visual, tab_id, 1, name), "");

    // The other way round: removed, then `$set` again: the later change wins.
    stage_field_update(&app, visual, tab_id, 1, "name", serde_json::json!("back"));
    let (is_null, type_name, _) = detail(&app, visual);
    assert!(!is_null && type_name != "missing", "{type_name}");
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
    app.update(visual, |this, cx| {
        this.select_grid_cell(tab_id, CellPosition { row: 1, column: name });
        this.copy_grid_selection(tab_id, cx);
    });
    let clipboard = visual.cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
    assert!(clipboard.contains("back"), "{clipboard:?}");
}

/// Replaces the pretty-printed line of top-level field `key` in `text`.
fn replace_field_line(text: &str, key: &str, line: &str) -> String {
    let prefix = format!("  \"{key}\":");
    let mut replaced = false;
    let lines = text
        .lines()
        .map(|current| {
            if current.starts_with(&prefix) {
                replaced = true;
                line.to_string()
            } else {
                current.to_string()
            }
        })
        .collect::<Vec<_>>();
    assert!(replaced, "no `{key}` line in {text}");
    lines.join("\n")
}

/// Live: a document edited through the Edit button, committed with the regular
/// Commit and read back from the server keeps its BSON types, and a removed
/// field is gone. Needs the Mongo driver built with `--features abi-v2` and a
/// server: `TABLEX_TEST_MONGO_DYLIB`, `TABLEX_TEST_MONGO_URI`.
#[gpui::test]
#[ignore = "live: needs TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB"]
fn live_mongo_document_edit_commits_set_and_unset_and_keeps_bson_types(cx: &mut TestAppContext) {
    let Some((app, visual, tab_id)) = live_mongo_tab(cx) else {
        eprintln!("live mongo test skipped: TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB not set");
        return;
    };
    let database = format!("tablex_gpui_docedit_{}", std::process::id());
    let session = app.read_with(visual, |this, _| {
        let connection = &this.workspace.tab(tab_id).unwrap().connection_id;
        this.driver_runtime.sessions[connection].clone()
    });
    // The test's own database is dropped when the test ends, also when an assertion fails.
    let _cleanup = DropDatabase { session: session.clone(), database: database.clone() };
    let mutate = |kind: &str, identity: serde_json::Value, payload: serde_json::Value| {
        let operation = session.begin_operation().expect("operation");
        session
            .mutate(
                &operation,
                engine_driver_api::MutationRequest {
                    operation_id: crate::mongo_result::STAGED_OPERATION_ID,
                    kind: kind.into(),
                    database: Some(database.clone()),
                    object: Some("docs".into()),
                    identity,
                    payload,
                },
            )
            .expect("seed the document");
    };
    mutate(
        "insert",
        serde_json::Value::Null,
        serde_json::json!({
            "_id": {"$oid": "66d4f84d71c18f22d433b003"},
            "name": "Ada",
            "age": {"$numberInt": "36"},
            "score": {"$numberDouble": "1.5"},
            "big": {"$numberLong": "5"},
            "born": {"$date": {"$numberLong": "1725148800123"}},
            "legacy": "drop me",
            "nested": {"a": {"$numberInt": "1"}, "b": [{"$numberInt": "1"}, "x"]},
        }),
    );
    app.update(visual, |this, _| this.workspace.tab_mut(tab_id).unwrap().database = database.clone());
    live_run(&app, visual, tab_id, "docs", "{}", "");
    app.update(visual, |this, cx| {
        this.workspace.runtime_mut(tab_id).unwrap().mongo.view = crate::mongo_result::MongoResultView::Document;
        this.select_mongo_document(tab_id, 0, cx);
    });
    let wait_for = |visual: &mut gpui::VisualTestContext, what: &str, done: &dyn Fn(&AppRoot) -> bool| {
        for _ in 0..200 {
            settle(visual);
            if app.read_with(visual, |this, _| done(this)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("timed out waiting for {what}");
    };
    wait_for(visual, "the document", &|this| {
        this.workspace.runtime(tab_id).unwrap().mongo.inspector.snapshot.is_some()
    });

    open_document_editor(visual);
    let seed = document_editor_text(&app, visual, tab_id);
    assert!(seed.contains("\"age\": 36") && seed.contains("ISODate('2024-09-01T00:00:00.123Z')"), "{seed}");
    let mut text = seed.replace("\"Ada\"", "\"Grace\"");
    text = replace_field_line(&text, "age", "  \"age\": 37,");
    text = replace_field_line(&text, "born", "  \"born\": ISODate('2025-01-02T03:04:05.678Z'),");
    // `legacy` goes, `added` comes.
    text = replace_field_line(&text, "legacy", "  \"added\": Long('9'),");
    set_document_editor_text(&app, visual, tab_id, text);
    click_selector(visual, "mongo-document-save");
    let staged = staged_mongo(&app, visual, tab_id);
    let mut payloads = staged.iter().map(|m| m.request().payload.to_string()).collect::<Vec<_>>();
    payloads.sort();
    assert_eq!(
        payloads,
        [
            r#"{"$set":{"added":{"$numberLong":"9"}}}"#,
            r#"{"$set":{"age":{"$numberInt":"37"}}}"#,
            r#"{"$set":{"born":{"$date":{"$numberLong":"1735787045678"}}}}"#,
            r#"{"$set":{"name":"Grace"}}"#,
            r#"{"$unset":{"legacy":""}}"#,
        ]
    );

    click_selector(visual, "mongo-commit");
    wait_for(visual, "the commit", &|this| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        runtime.mongo.mutations.is_empty()
            && runtime.mongo.inspector.snapshot.as_ref().is_some_and(|s| s.document["name"] == "Grace")
    });
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert_eq!(runtime.mongo.commit_status, crate::mongo_result::MongoCommitStatus::Idle);
        // The document was re-read from the server after the commit.
        let document = &runtime.mongo.inspector.snapshot.as_ref().unwrap().document;
        assert_eq!(document["name"], "Grace");
        assert_eq!(document["age"], serde_json::json!({"$numberInt": "37"}), "Int32 stays Int32");
        assert_eq!(document["big"], serde_json::json!({"$numberLong": "5"}), "untouched Int64 stays Int64");
        assert_eq!(document["score"], serde_json::json!({"$numberDouble": "1.5"}), "untouched Double stays Double");
        assert_eq!(
            document["born"],
            serde_json::json!({"$date": {"$numberLong": "1735787045678"}}),
            "a date stays a date"
        );
        assert_eq!(document["added"], serde_json::json!({"$numberLong": "9"}));
        assert_eq!(document["nested"], serde_json::json!({"a": {"$numberInt": "1"}, "b": [{"$numberInt": "1"}, "x"]}));
        assert!(document.get("legacy").is_none(), "the unset field is gone: {document}");
        assert_eq!(document["_id"], serde_json::json!({"$oid": "66d4f84d71c18f22d433b003"}));
    });
}
