// Real-interaction proofs for the Aggregate tab. Everything runs through the
// production sidebar menu, editors, buttons, dialogs and the shared pipeline
// module against the scenario fixture, whose Mongo "driver" shapes requests
// with the same code as the real driver (preview `$limit`, write stripping,
// paging, write confirmation) and logs every request it serves.

fn agg_calls(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> Vec<crate::ui_fixture::FixtureAggregateCall> {
    app.read_with(visual, |this, _| {
        this.scenario_ui
            .service
            .as_ref()
            .unwrap()
            .fixture()
            .aggregate_calls()
            .to_vec()
    })
}

fn agg_writes(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> Vec<(String, String)> {
    app.read_with(visual, |this, _| {
        this.scenario_ui
            .service
            .as_ref()
            .unwrap()
            .fixture()
            .aggregate_writes()
            .to_vec()
    })
}

/// Lets the 400 ms preview debounce fire and the result render.
fn agg_flush(visual: &mut gpui::VisualTestContext) {
    visual
        .cx
        .executor()
        .advance_clock(Duration::from_millis(crate::aggregate::PREVIEW_DEBOUNCE_MS + 50));
    settle(visual);
}

fn agg_open_tab(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-aggregation");
    settle(visual);
    (app, visual)
}

fn agg_draft(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> crate::aggregate::AggregateDraft {
    app.read_with(visual, |this, _| {
        this.workspace
            .active_tab()
            .and_then(|tab| tab.aggregate.clone())
            .expect("active tab is an aggregate tab")
    })
}

/// Replaces the whole text of the Text editor through real keystrokes.
fn agg_type_pipeline(visual: &mut gpui::VisualTestContext, text: &str) {
    let editor = visual.debug_bounds("agg-text-editor").expect("text editor");
    visual.simulate_click(editor.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

#[gpui::test]
fn new_aggregation_menu_item_opens_aggregate_tabs_after_new_query(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    right_click_label(&app, visual, "fixture_collection_00");
    let query = visual.debug_bounds("mongo-menu-new-query").unwrap().origin.y;
    let aggregation = visual
        .debug_bounds("mongo-menu-new-aggregation")
        .expect("New Aggregation entry")
        .origin
        .y;
    let indexes = visual.debug_bounds("mongo-menu-manage-indexes").unwrap().origin.y;
    assert!(query < aggregation && aggregation < indexes, "after New Query, before Manage Indexes");
    click_debug_button(visual, "mongo-menu-new-aggregation");
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::MongoAggregate));
        assert_eq!(tab.database, "fixture_db_0");
        assert_eq!(tab.collection, "fixture_collection_00");
        assert_eq!(tab.connection_id, crate::ui_fixture::MONGO_ID);
        assert_eq!(tab.title, "fixture_collection_00 · Untitled");
        assert!(tab.aggregate.is_some());
    });
    for selector in ["agg-tab", "agg-header", "agg-save", "agg-create-new", "agg-export-data", "agg-export-code", "agg-preview-toggle", "agg-mode", "agg-explain", "agg-options", "agg-run", "agg-text-editor", "agg-preview-pane"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} renders");
    }
    // Several aggregate tabs per collection are allowed.
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-aggregation");
    settle(visual);
    app.read_with(visual, |this, _| {
        let count = this
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.query_mode.is_mongo_aggregate())
            .count();
        assert_eq!(count, 2);
    });
}

#[gpui::test]
fn menu_entry_is_disabled_when_the_driver_cannot_aggregate(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    app.update(visual, |this, _| {
        this.sessions.disconnect(crate::ui_fixture::MONGO_ID);
    });
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-aggregation");
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this
            .workspace
            .tabs()
            .iter()
            .all(|tab| !tab.query_mode.is_mongo_aggregate()));
    });
}

#[gpui::test]
fn first_preview_samples_the_collection_and_edits_are_debounced_into_one_request(
    cx: &mut TestAppContext,
) {
    let (app, visual) = agg_open_tab(cx);
    assert!(agg_calls(&app, visual).is_empty(), "nothing is sent before the debounce");
    agg_flush(visual);
    let calls = agg_calls(&app, visual);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].kind, "mongo_aggregate");
    assert_eq!(calls[0].payload["mode"], "preview");
    assert_eq!(
        calls[0].pipeline,
        vec![bson::doc! { "$limit": 20_i64 }],
        "an empty pipeline previews the first sample_size documents"
    );
    assert!(visual.debug_bounds("agg-preview-subtitle").is_some());
    let subtitle = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        let ui = this.aggregate_ui.tab(tab).unwrap();
        (ui.slot(super::aggregate::SlotKey::Main).unwrap().docs.len(), ui.slot(super::aggregate::SlotKey::Main).unwrap().status.clone())
    });
    assert_eq!(subtitle.0, 20);
    assert_eq!(subtitle.1, super::aggregate::PreviewStatus::Ready);
    assert!(visual.debug_bounds("agg-tree-row").is_some(), "tree rows render");

    // Typing: several keystrokes inside the debounce window -> one request.
    let editor = visual.debug_bounds("agg-text-editor").unwrap();
    visual.simulate_click(editor.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("[{ $match: { active: true } }, { $sort: { count: -1 } }]");
    visual
        .cx
        .executor()
        .advance_clock(Duration::from_millis(crate::aggregate::PREVIEW_DEBOUNCE_MS / 2));
    settle(visual);
    assert_eq!(agg_calls(&app, visual).len(), 1, "still inside the debounce window");
    visual.simulate_input(" ");
    visual
        .cx
        .executor()
        .advance_clock(Duration::from_millis(crate::aggregate::PREVIEW_DEBOUNCE_MS / 2 + 20));
    settle(visual);
    assert_eq!(agg_calls(&app, visual).len(), 1, "typing restarted the timer");
    agg_flush(visual);
    let calls = agg_calls(&app, visual);
    assert_eq!(calls.len(), 2, "exactly one request for the whole burst: {calls:?}");
    assert_eq!(
        calls[1].pipeline,
        vec![
            bson::doc! { "$match": { "active": true } },
            bson::doc! { "$sort": { "count": -1 } },
            bson::doc! { "$limit": 20_i64 },
        ]
    );
    let draft = agg_draft(&app, visual);
    assert!(draft.dirty);
    assert_eq!(draft.stages.len(), 2, "stages followed the valid text");
}

#[gpui::test]
fn invalid_text_shows_line_and_column_and_sends_nothing(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    let before = agg_calls(&app, visual).len();
    agg_type_pipeline(visual, "[\n  { $match: { a: 1 } },\n  { $project: { b: } }\n]");
    agg_flush(visual);
    assert_eq!(agg_calls(&app, visual).len(), before, "no request for an invalid pipeline");
    assert!(visual.debug_bounds("agg-text-error").is_some(), "inline error is visible");
    let message = app.read_with(visual, |this, _| {
        this.aggregate_draft(this.workspace.active_tab().unwrap().id)
            .map(|draft| draft.check().text_error)
            .unwrap()
    });
    let error = message.expect("parse error");
    assert_eq!((error.stage, error.line), (Some(2), 3));
    assert!(error.col > 1);
    assert!(visual.debug_bounds("agg-preview-invalid").is_some() || visual.debug_bounds("agg-tree-row").is_some());
    // Fixing the text clears the error and previews again.
    agg_type_pipeline(visual, "[{ $match: { active: true } }]");
    agg_flush(visual);
    assert!(visual.debug_bounds("agg-text-error").is_none());
    assert_eq!(agg_calls(&app, visual).len(), before + 1);
}

fn agg_stage_ids(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> Vec<u64> {
    agg_draft(app, visual).stages.iter().map(|stage| stage.id).collect()
}

/// `debug_bounds` for a selector built at run time (leaked: tests only).
fn agg_bounds(
    visual: &mut gpui::VisualTestContext,
    selector: impl Into<String>,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    let selector: &'static str = Box::leak(selector.into().into_boxed_str());
    visual.debug_bounds(selector)
}

fn agg_click(visual: &mut gpui::VisualTestContext, selector: &str) {
    let bounds = agg_bounds(visual, selector.to_string())
        .unwrap_or_else(|| panic!("{selector} should render"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

const THREE_STAGES: &str = "[{ $match: { active: true } }, { $sort: { count: -1 } }, { $limit: 5 }]";

/// Opens a tab, loads `THREE_STAGES` through the Text editor and switches to
/// Stages through the real toggle.
fn agg_stages_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    agg_type_pipeline(visual, THREE_STAGES);
    agg_click(visual, "agg-mode-stages");
    agg_flush(visual);
    (app, visual)
}

#[gpui::test]
fn stages_mode_previews_each_expanded_stage_truncated_at_that_stage(cx: &mut TestAppContext) {
    let (app, visual) = agg_stages_app(cx);
    let ids = agg_stage_ids(&app, visual);
    assert_eq!(ids.len(), 3);
    for id in &ids {
        assert!(agg_bounds(visual, format!("agg-stage-{id}")).is_some(), "card {id}");
        assert!(agg_bounds(visual, format!("agg-stage-body-{id}")).is_some());
        assert!(agg_bounds(visual, format!("agg-stage-operator-{id}")).is_some());
        assert!(agg_bounds(visual, format!("agg-stage-handle-{id}")).is_some());
    }
    assert_eq!(agg_draft(&app, visual).mode, crate::aggregate::EditorMode::Stages);
    // The Stages switch (after the Text-mode request) sent one request per
    // expanded stage, each cut at its stage and capped by $limit.
    let calls = agg_calls(&app, visual);
    let stage_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.payload.get("stage_index").is_some())
        .collect();
    assert_eq!(stage_calls.len(), 3, "{calls:?}");
    let mut by_index: Vec<(u64, usize)> = stage_calls
        .iter()
        .map(|call| (call.payload["stage_index"].as_u64().unwrap(), call.pipeline.len()))
        .collect();
    by_index.sort();
    assert_eq!(by_index, vec![(0, 2), (1, 3), (2, 4)], "stage i keeps i+1 stages plus the cap");
    // Per-stage counts: stage 3 is `$limit: 5` -> 5 documents.
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        let ui = this.aggregate_ui.tab(tab).unwrap();
        let count = |id: u64| ui.slot(super::aggregate::SlotKey::Stage(id)).unwrap().docs.len();
        assert_eq!((count(ids[0]), count(ids[1]), count(ids[2])), (20, 20, 5));
    });
    assert!(agg_bounds(visual, format!("agg-stage-preview-{}", ids[2])).is_some());
}

#[gpui::test]
fn stages_add_disable_delete_and_collapse_update_text_and_previews(cx: &mut TestAppContext) {
    let (app, visual) = agg_stages_app(cx);
    let ids = agg_stage_ids(&app, visual);
    let sent = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| agg_calls(app, visual).len();

    // Disable the $sort stage: the text loses it and only 2 stages preview.
    let before = sent(&app, visual);
    agg_click(visual, &format!("agg-stage-toggle-{}", ids[1]));
    assert!(!agg_draft(&app, visual).text.contains("$sort"));
    agg_flush(visual);
    let calls = agg_calls(&app, visual);
    assert_eq!(calls.len() - before, 2, "one request per enabled stage: {calls:?}");
    assert!(calls[before..]
        .iter()
        .all(|call| call.pipeline.iter().all(|stage| stage.keys().next().unwrap() != "$sort")));
    assert!(agg_bounds(visual, format!("agg-stage-preview-{}", ids[1])).is_none(), "disabled stage has no preview");
    assert_eq!(agg_draft(&app, visual).disabled_count(), 1);

    // Add a stage after the first one.
    let before = sent(&app, visual);
    agg_click(visual, &format!("agg-stage-add-{}", ids[0]));
    let draft = agg_draft(&app, visual);
    assert_eq!(draft.stages.len(), 4);
    assert_eq!(draft.stages[1].operator, "$match", "new stage follows the first");
    agg_flush(visual);
    assert_eq!(sent(&app, visual) - before, 3, "previews follow the new enabled stage list");

    // Collapse the preview of the last stage: it is no longer requested.
    let last = *agg_stage_ids(&app, visual).last().unwrap();
    agg_click(visual, &format!("agg-stage-preview-toggle-{last}"));
    agg_flush(visual);
    let before = sent(&app, visual);
    agg_click(visual, &format!("agg-stage-delete-{}", agg_stage_ids(&app, visual)[1]));
    agg_flush(visual);
    let calls = agg_calls(&app, visual);
    assert_eq!(calls.len() - before, 1, "only the first stage's preview remains requested: {calls:?}");
    assert_eq!(agg_draft(&app, visual).stages.len(), 3);
    // Expanding again requests it.
    agg_click(visual, &format!("agg-stage-preview-toggle-{last}"));
    agg_flush(visual);
    assert!(agg_bounds(visual, format!("agg-stage-preview-{last}")).is_some());
}

#[gpui::test]
fn dragging_a_stage_handle_reorders_stages_and_the_pipeline(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = agg_stages_app(cx);
    let ids = agg_stage_ids(&app, visual);
    let handle = agg_bounds(visual, format!("agg-stage-handle-{}", ids[2]))
        .expect("grip handle");
    let target = agg_bounds(visual, format!("agg-stage-{}", ids[0]))
        .expect("first stage card");
    let drop_at = gpui::point(target.center().x, target.top() + gpui::px(10.0));
    visual.simulate_mouse_down(handle.center(), gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(
        gpui::point(handle.center().x, handle.center().y - gpui::px(12.0)),
        gpui::MouseButton::Left,
        Default::default(),
    );
    visual.simulate_mouse_move(drop_at, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_up(drop_at, gpui::MouseButton::Left, Default::default());
    settle(visual);
    let order: Vec<u64> = agg_stage_ids(&app, visual);
    assert_eq!(order, vec![ids[2], ids[0], ids[1]], "the grabbed stage moved to the target position");
    assert!(agg_draft(&app, visual).text.starts_with("[\n  { $limit: 5 }"));
    let before = agg_calls(&app, visual).len();
    agg_flush(visual);
    let calls = agg_calls(&app, visual);
    assert_eq!(calls.len() - before, 3);
    let pipelines: Vec<String> = calls[before..]
        .iter()
        .map(|call| call.payload["pipeline"].as_str().unwrap().to_string())
        .collect();
    assert!(pipelines.iter().all(|text| text.starts_with("[\n  { $limit: 5 }")), "{pipelines:?}");
}

#[gpui::test]
fn operator_select_changes_the_stage_through_a_real_pick(cx: &mut TestAppContext) {
    let (app, visual) = agg_stages_app(cx);
    let ids = agg_stage_ids(&app, visual);
    let before = agg_draft(&app, visual).stages[0].operator.clone();
    assert_eq!(before, "$match");
    // Open the Select of the first stage and pick the entry after $match with
    // real keys (the list starts with $match, $group, ...).
    agg_click(visual, &format!("agg-stage-operator-{}", ids[0]));
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let draft = agg_draft(&app, visual);
    assert_eq!(draft.stages[0].operator, "$group");
    assert_eq!(draft.stages[0].body, crate::aggregate::stage_template("$group"));
    assert!(draft.text.contains("$group"));
    // The Select shows the new choice when reopened.
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        let select = &this.aggregate_ui.tab(tab).unwrap().stages[&ids[0]].operator;
        assert_eq!(select.read(cx).selected_value().map(String::as_str), Some("$group"));
    });
}

#[gpui::test]
fn text_and_stages_stay_in_sync_in_both_directions(cx: &mut TestAppContext) {
    let (app, visual) = agg_stages_app(cx);
    let ids = agg_stage_ids(&app, visual);
    // Disable $sort in Stages, then switch to Text: the text omits it and says so.
    agg_click(visual, &format!("agg-stage-toggle-{}", ids[1]));
    agg_click(visual, "agg-mode-text");
    let draft = agg_draft(&app, visual);
    assert_eq!(draft.mode, crate::aggregate::EditorMode::Text);
    assert!(!draft.text.contains("$sort"));
    assert!(visual.debug_bounds("agg-disabled-notice").is_some(), "disabled stages are announced in Text");
    let editor_text = app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.aggregate_ui.tab(tab).unwrap().text_editor.as_ref().unwrap().read(cx).value().to_string()
    });
    assert_eq!(editor_text, draft.text, "the Text editor shows the regenerated pipeline");
    // Edit in Text, back to Stages: the disabled stage is still there.
    agg_type_pipeline(visual, "[{ $match: { active: false } }, { $limit: 9 }]");
    agg_click(visual, "agg-mode-stages");
    let draft = agg_draft(&app, visual);
    let ops: Vec<_> = draft.stages.iter().map(|stage| (stage.operator.as_str(), stage.enabled)).collect();
    assert_eq!(ops, [("$match", true), ("$sort", false), ("$limit", true)]);
    assert_eq!(draft.stages[0].body, "{ active: false }");
    // A broken text refuses the switch and says why.
    agg_click(visual, "agg-mode-text");
    agg_type_pipeline(visual, "[{ $match: ");
    agg_click(visual, "agg-mode-stages");
    assert_eq!(agg_draft(&app, visual).mode, crate::aggregate::EditorMode::Text);
    assert!(visual.debug_bounds("agg-notice").is_some());
}

#[gpui::test]
fn a_new_edit_cancels_the_in_flight_preview_and_ignores_its_late_answer(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    app.update(visual, |this, _| {
        this.scenario_ui
            .service
            .as_mut()
            .unwrap()
            .fixture_mut()
            .set_aggregate_latency(Duration::from_secs(2));
    });
    agg_flush(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        assert_eq!(this.aggregate_ui.running_count(tab), 1, "request in flight");
    });
    assert_eq!(agg_calls(&app, visual).len(), 1);
    // Edit while it is running: the old request is cancelled.
    agg_type_pipeline(visual, "[{ $match: { active: true } }, { $limit: 3 }]");
    let cancels = app.read_with(visual, |this, _| {
        this.scenario_ui.service.as_ref().unwrap().fixture().aggregate_cancels()
    });
    assert_eq!(cancels, 1, "the stale preview was cancelled");
    agg_flush(visual);
    // Let every answer arrive: only the newest request's result is applied.
    visual.cx.executor().advance_clock(Duration::from_secs(3));
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        let slot = this.aggregate_ui.tab(tab).unwrap().slot(super::aggregate::SlotKey::Main).unwrap();
        assert_eq!(slot.docs.len(), 3, "the late answer of the old request did not overwrite the new one");
        assert_eq!(this.aggregate_ui.running_count(tab), 0);
    });
}

#[gpui::test]
fn stage_previews_run_at_most_two_at_a_time(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    agg_type_pipeline(visual, "[{ $match: {} }, { $sort: { count: 1 } }, { $skip: 1 }, { $limit: 4 }]");
    agg_flush(visual);
    app.update(visual, |this, _| {
        this.scenario_ui
            .service
            .as_mut()
            .unwrap()
            .fixture_mut()
            .set_aggregate_latency(Duration::from_secs(2));
    });
    agg_click(visual, "agg-mode-stages");
    agg_flush(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    app.read_with(visual, |this, _| {
        assert_eq!(this.aggregate_ui.running_count(tab), 2, "two of the four stage previews run");
    });
    let in_first_wave = agg_calls(&app, visual).iter().filter(|call| call.payload.get("stage_index").is_some()).count();
    assert_eq!(in_first_wave, 2);
    visual.cx.executor().advance_clock(Duration::from_secs(2) + Duration::from_millis(50));
    settle(visual);
    let in_second_wave = agg_calls(&app, visual).iter().filter(|call| call.payload.get("stage_index").is_some()).count();
    assert_eq!(in_second_wave, 4, "the queued previews start as slots free up");
    visual.cx.executor().advance_clock(Duration::from_secs(3));
    settle(visual);
    app.read_with(visual, |this, _| assert_eq!(this.aggregate_ui.running_count(tab), 0));
}

fn agg_grid_rows(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| this.active_grid().row_count())
}

fn full_calls(calls: &[crate::ui_fixture::FixtureAggregateCall]) -> Vec<&crate::ui_fixture::FixtureAggregateCall> {
    calls
        .iter()
        .filter(|call| call.payload["mode"] == "full")
        .collect()
}

#[gpui::test]
fn run_shows_the_full_result_in_the_grid_and_pages_with_skip_and_limit(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    agg_type_pipeline(visual, "[{ $match: { active: true } }]");
    agg_flush(visual);
    assert!(visual.debug_bounds("agg-results").is_none(), "no result area before the first run");
    agg_click(visual, "agg-run");
    assert!(visual.debug_bounds("agg-results").is_some(), "Run opens the result area in the tab");
    let calls = agg_calls(&app, visual);
    let runs = full_calls(&calls);
    assert_eq!(runs.len(), 1);
    // Compass-style window: user stages + $limit page+1 (no $skip on page 1).
    assert_eq!(
        runs[0].pipeline,
        vec![bson::doc! { "$match": { "active": true } }, bson::doc! { "$limit": 201_i64 }]
    );
    // 120 matching documents: one page of 120, no next page.
    assert_eq!(agg_grid_rows(&app, visual), 120);
    assert!(visual.debug_bounds("grid-body-cell-0-0").is_some(), "the normal Mongo grid renders the rows");
    assert!(visual.debug_bounds("page-next").is_some());

    // Pagination: a smaller page size makes a second page; Next runs $skip + $limit.
    app.update(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.set_output_limit(tab, 50, cx);
    });
    settle(visual);
    assert_eq!(agg_grid_rows(&app, visual), 50);
    agg_click(visual, "page-next");
    let calls = agg_calls(&app, visual);
    let last = full_calls(&calls).last().copied().unwrap().clone();
    assert_eq!(
        last.pipeline[1..],
        [bson::doc! { "$skip": 50_i64 }, bson::doc! { "$limit": 51_i64 }]
    );
    assert_eq!(agg_grid_rows(&app, visual), 50);
    agg_click(visual, "page-next");
    assert_eq!(agg_grid_rows(&app, visual), 20, "last page");
    // The result is read-only unless the pipeline is $match/$sort/$skip/$limit with _id.
    assert!(visual.debug_bounds("agg-editability").is_some());
}

#[gpui::test]
fn run_of_an_invalid_pipeline_is_blocked_with_a_message_and_sends_nothing(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { a: } }]");
    agg_click(visual, "agg-run");
    assert!(full_calls(&agg_calls(&app, visual)).is_empty());
    assert!(visual.debug_bounds("agg-notice").is_some(), "the blocker is explained");
    assert!(visual.debug_bounds("agg-results").is_none());
}

#[gpui::test]
fn out_pipeline_needs_confirmation_and_previews_never_write(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }, { $out: 'copy' }]");
    agg_flush(visual);
    // The preview ran without the $out stage.
    let calls = agg_calls(&app, visual);
    let preview = calls.iter().rev().find(|call| call.payload["mode"] == "preview").unwrap();
    assert!(preview.pipeline.iter().all(|stage| stage.keys().next().unwrap() != "$out"), "{preview:?}");
    assert_eq!(preview.pipeline.last().unwrap(), &bson::doc! { "$limit": 20_i64 });

    // Run asks first; Cancel runs nothing.
    agg_click(visual, "agg-run");
    assert!(visual.debug_bounds("agg-write-warning").is_some(), "confirmation dialog opens");
    assert!(full_calls(&agg_calls(&app, visual)).is_empty(), "nothing ran yet");
    agg_click(visual, "agg-write-cancel");
    assert!(visual.debug_bounds("agg-write-warning").is_none());
    assert!(full_calls(&agg_calls(&app, visual)).is_empty());
    assert!(agg_writes(&app, visual).is_empty());

    // Run again and confirm: the full request carries confirm_write and no
    // window stages after the $out.
    agg_click(visual, "agg-run");
    agg_click(visual, "agg-write-confirm");
    let calls = agg_calls(&app, visual);
    let runs = full_calls(&calls);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].payload["confirm_write"], true);
    assert_eq!(runs[0].pipeline.len(), 2, "no $skip/$limit appended after $out: {:?}", runs[0].pipeline);
    assert_eq!(agg_writes(&app, visual), vec![("$out".to_string(), "copy".to_string())]);
    // Refresh / paging never re-run a writing pipeline implicitly.
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    settle(visual);
    assert_eq!(agg_writes(&app, visual).len(), 1);
    assert!(visual.debug_bounds("agg-notice").is_some());
}

#[gpui::test]
fn explain_opens_a_dialog_with_the_plan_json_and_strips_writes_for_execution_stats(
    cx: &mut TestAppContext,
) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }, { $out: 'copy' }]");
    agg_click(visual, "agg-explain");
    assert!(visual.debug_bounds("agg-explain-json").is_some(), "plan is shown as JSON");
    let calls = agg_calls(&app, visual);
    let explain = calls.iter().rev().find(|call| call.kind == "mongo_aggregate_explain").unwrap();
    assert_eq!(explain.payload["verbosity"], "queryPlanner");
    assert_eq!(explain.pipeline.len(), 2, "queryPlanner never runs $out, so it stays in the plan");
    agg_click(visual, "agg-explain-close");
    assert!(visual.debug_bounds("agg-explain-json").is_none());
    // executionStats through the dropdown menu: the write stage is stripped.
    agg_open_dropdown(visual, "agg-explain-dropdown");
    agg_click(visual, "agg-explain-stats");
    let calls = agg_calls(&app, visual);
    let stats = calls.iter().rev().find(|call| call.kind == "mongo_aggregate_explain").unwrap();
    assert_eq!(stats.payload["verbosity"], "executionStats");
    assert_eq!(stats.pipeline, vec![bson::doc! { "$match": { "active": true } }], "executionStats would run $out, so it is stripped");
    assert!(visual.debug_bounds("agg-explain-json").is_some());
    agg_click(visual, "agg-explain-close");
    assert!(agg_writes(&app, visual).is_empty(), "explain never writes");
}

fn agg_open_dropdown(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let trigger = visual.debug_bounds(selector).expect("split button");
    visual.simulate_click(
        gpui::point(trigger.right() - gpui::px(3.0), trigger.center().y),
        Default::default(),
    );
    settle(visual);
}

#[gpui::test]
fn export_code_copies_the_pipeline_as_mongosh(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { _id: ObjectId('507f191e810c19729de860ea') } }, { $sort: { zeta: 1, alpha: -1 } }]");
    agg_click(visual, "agg-export-code");
    let copied = visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(
        copied.as_deref(),
        Some("db.fixture_collection_00.aggregate([\n  { $match: { _id: ObjectId(\"507f191e810c19729de860ea\") } },\n  { $sort: { zeta: 1, alpha: -1 } }\n])"),
        "shell syntax, key order preserved"
    );
    assert!(visual.debug_bounds("agg-notice").is_some());
    // An invalid pipeline copies nothing new and says why.
    agg_type_pipeline(visual, "[{ $match: ");
    visual.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
    agg_click(visual, "agg-export-code");
    let copied = visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some("sentinel"));
    let _ = app;
}

#[gpui::test]
fn export_data_writes_the_whole_result_and_surfaces_errors(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }, { $project: { name: 1, count: 1 } }]");
    agg_click(visual, "agg-export-data");
    agg_click(visual, "agg-export-csv");
    assert!(visual.debug_bounds("agg-export-done").is_some(), "completion is reported");
    let exports = app.read_with(visual, |this, _| {
        this.scenario_ui.service.as_ref().unwrap().fixture().aggregate_exports().to_vec()
    });
    assert_eq!(exports.len(), 1);
    let text = std::fs::read_to_string(&exports[0].1).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next().unwrap(), "_id,name,count");
    assert_eq!(text.lines().count(), 121, "all 120 matching documents, not one page: {}", text.lines().count());
    std::fs::remove_file(&exports[0].1).unwrap();
    // The window stages of a Run are not part of an export: the pipeline is the user's.
    assert!(!exports[0].0.contains("$limit"));
    visual.simulate_keystrokes("escape");
    settle(visual);

    // A failing export is shown, not swallowed.
    app.update(visual, |this, _| {
        this.scenario_ui.service.as_mut().unwrap().fixture_mut().fail_next_aggregate_export("disk full");
    });
    agg_click(visual, "agg-export-data");
    agg_click(visual, "agg-export-json");
    assert!(visual.debug_bounds("agg-export-error").is_some());
    visual.simulate_keystrokes("escape");
    settle(visual);

    // A pipeline that writes cannot be exported.
    agg_type_pipeline(visual, "[{ $match: {} }, { $out: 'copy' }]");
    agg_click(visual, "agg-export-data");
    agg_click(visual, "agg-export-csv");
    assert!(visual.debug_bounds("agg-notice").is_some());
    assert_eq!(app.read_with(visual, |this, _| this.scenario_ui.service.as_ref().unwrap().fixture().aggregate_exports().len()), 1);
    assert!(agg_writes(&app, visual).is_empty());
}

fn agg_set_input(visual: &mut gpui::VisualTestContext, selector: &'static str, text: &str) {
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} renders"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

#[gpui::test]
fn options_popover_edits_sample_size_max_time_collation_and_disk_use(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    agg_click(visual, "agg-options");
    assert!(visual.debug_bounds("agg-options-panel").is_some(), "popover opens");
    let panel = visual.debug_bounds("agg-options-panel").unwrap();
    let button = visual.debug_bounds("agg-options").unwrap();
    assert!(panel.top() >= button.bottom(), "anchored below the trigger: {panel:?} {button:?}");
    let viewport = visual.update(|window, _| window.viewport_size());
    assert!(
        panel.right() <= viewport.width && panel.bottom() <= viewport.height,
        "inside the window {viewport:?}: {panel:?}"
    );

    agg_set_input(visual, "agg-opt-sample", "7");
    agg_set_input(visual, "agg-opt-maxtime", "12345");
    agg_set_input(visual, "agg-opt-collation", "{ locale: 'en', strength: 2 }");
    agg_click(visual, "agg-opt-disk");
    let options = agg_draft(&app, visual).options;
    assert_eq!(options.sample_size, 7);
    assert_eq!(options.max_time_ms, 12345);
    assert!(options.allow_disk_use);
    assert_eq!(options.collation, "{ locale: 'en', strength: 2 }");
    agg_flush(visual);
    let last = agg_calls(&app, visual).pop().unwrap();
    assert_eq!(last.payload["sample_size"], 7);
    assert_eq!(last.payload["max_time_ms"], 12345);
    assert_eq!(last.payload["allow_disk_use"], true);
    assert_eq!(last.payload["collation"], "{ locale: 'en', strength: 2 }");
    assert_eq!(last.pipeline.last().unwrap(), &bson::doc! { "$limit": 7_i64 }, "the preview cap follows the sample size");

    // Invalid values are explained and block requests.
    let before = agg_calls(&app, visual).len();
    agg_set_input(visual, "agg-opt-collation", "{ locale");
    assert!(visual.debug_bounds("agg-opt-error").is_some());
    agg_flush(visual);
    assert_eq!(agg_calls(&app, visual).len(), before, "no request with an invalid collation");
    agg_set_input(visual, "agg-opt-sample", "abc");
    assert!(visual.debug_bounds("agg-opt-error").is_some());
    assert_eq!(agg_draft(&app, visual).options.sample_size, 7, "the bad text did not replace the number");
    // Escape closes the popover and the values are still there when reopened.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("agg-options-panel").is_none());
}

#[gpui::test]
fn preview_toggle_turns_requests_off_and_on(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    assert_eq!(agg_calls(&app, visual).len(), 1);
    let toggle = visual.debug_bounds("agg-preview-toggle").unwrap();
    // The switch sits at the right edge of the labelled group.
    visual.simulate_click(gpui::point(toggle.right() - gpui::px(10.0), toggle.center().y), Default::default());
    settle(visual);
    assert!(!agg_draft(&app, visual).preview);
    assert!(visual.debug_bounds("agg-preview-off").is_some());
    agg_type_pipeline(visual, "[{ $match: { active: true } }]");
    agg_flush(visual);
    assert_eq!(agg_calls(&app, visual).len(), 1, "no preview request while Preview is off");
    visual.simulate_click(gpui::point(toggle.right() - gpui::px(10.0), toggle.center().y), Default::default());
    agg_flush(visual);
    assert!(agg_draft(&app, visual).preview);
    assert_eq!(agg_calls(&app, visual).len(), 2, "turning it on previews the current pipeline");
}

fn saved_rows(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> Vec<engine_storage::aggregate::SavedAggregate> {
    app.read_with(visual, |this, _| {
        this.aggregate_store
            .get()
            .and_then(|store| store.as_ref().ok())
            .map(|store| {
                store
                    .list(crate::ui_fixture::MONGO_ID, "fixture_db_0")
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    })
}

/// Save (first time) through the real Save button and dialog.
fn agg_save_as(visual: &mut gpui::VisualTestContext, name: &str) {
    agg_click(visual, "agg-save");
    assert!(visual.debug_bounds("agg-save-name").is_some(), "name dialog opens");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(name);
    visual.simulate_keystrokes("enter");
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
}

#[gpui::test]
fn save_writes_the_pipeline_options_and_database_and_retitles_the_tab(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }, { $sort: { count: -1 } }, { $limit: 5 }]");
    agg_click(visual, "agg-mode-stages");
    let ids = agg_stage_ids(&app, visual);
    agg_click(visual, &format!("agg-stage-toggle-{}", ids[1]));
    agg_click(visual, "agg-options");
    agg_set_input(visual, "agg-opt-sample", "11");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(agg_draft(&app, visual).dirty);

    agg_save_as(visual, "Top active");
    let rows = saved_rows(&app, visual);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!((row.name.as_str(), row.database.as_str(), row.collection.as_str()), ("Top active", "fixture_db_0", "fixture_collection_00"));
    assert_eq!(row.kind, "aggregate");
    assert!(row.pipeline_json.contains("$limit") && !row.pipeline_json.contains("$sort"), "effective pipeline only: {}", row.pipeline_json);
    let options = crate::aggregate::AggregateOptions::from_json(row.options_json.as_deref());
    assert_eq!(options.sample_size, 11);
    assert_eq!(options.disabled.len(), 1);
    assert_eq!(options.disabled[0].operator, "$sort");
    let draft = agg_draft(&app, visual);
    assert_eq!(draft.name.as_deref(), Some("Top active"));
    assert_eq!(draft.saved_id.as_deref(), Some(row.id.as_str()));
    assert!(!draft.dirty);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().title, "fixture_collection_00 · Top active");
    });
    assert!(visual.debug_bounds("agg-dirty").is_none());

    // Save again updates the same row (no dialog, no duplicate).
    agg_click(visual, "agg-mode-text");
    agg_type_pipeline(visual, "[{ $limit: 3 }]");
    assert!(visual.debug_bounds("agg-dirty").is_some() || agg_draft(&app, visual).dirty);
    agg_click(visual, "agg-save");
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert!(visual.debug_bounds("agg-save-name").is_none());
    let rows = saved_rows(&app, visual);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, row.id);
    assert_eq!(rows[0].pipeline_json, r#"[{"$limit":3}]"#, "strict JSON for other readers");
    assert!(
        crate::aggregate::AggregateOptions::from_json(rows[0].options_json.as_deref())
            .shell_text
            .is_some_and(|text| text.contains("$limit: 3")),
        "the user's own text is kept alongside"
    );
}

#[gpui::test]
fn save_as_creates_a_second_entry_and_refuses_a_duplicate_name(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: {} }]");
    agg_save_as(visual, "First");
    // Save As through the dropdown menu.
    agg_open_dropdown(visual, "agg-save-dropdown");
    agg_click(visual, "agg-menu-save-as");
    assert!(visual.debug_bounds("agg-save-name").is_some());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("first");
    visual.simulate_keystrokes("enter");
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert_eq!(saved_rows(&app, visual).len(), 1, "a duplicate name (case-insensitive) is refused");
    assert!(visual.debug_bounds("agg-notice").is_some());
    agg_open_dropdown(visual, "agg-save-dropdown");
    agg_click(visual, "agg-menu-save-as");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Second");
    visual.simulate_keystrokes("enter");
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    let rows = saved_rows(&app, visual);
    assert_eq!(rows.len(), 2);
    let draft = agg_draft(&app, visual);
    assert_eq!(draft.name.as_deref(), Some("Second"), "the tab now edits the copy");
    assert_ne!(rows[0].id, rows[1].id);
    // An empty name is rejected before anything is written.
    agg_open_dropdown(visual, "agg-save-dropdown");
    agg_click(visual, "agg-menu-save-as");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_keystrokes("backspace");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(saved_rows(&app, visual).len(), 2);
}

#[gpui::test]
fn create_new_opens_a_fresh_pipeline_for_the_same_collection(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }]");
    agg_click(visual, "agg-create-new");
    app.read_with(visual, |this, _| {
        let tabs: Vec<_> = this
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.query_mode.is_mongo_aggregate())
            .collect();
        assert_eq!(tabs.len(), 2);
        let active = this.workspace.active_tab().unwrap();
        assert_eq!(active.collection, "fixture_collection_00");
        let draft = active.aggregate.as_ref().unwrap();
        assert_eq!(draft.text, "[]");
        assert!(draft.saved_id.is_none() && draft.name.is_none());
    });
}

#[gpui::test]
fn saved_aggregations_appear_in_the_sidebar_and_open_rename_and_delete(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }, { $limit: 4 }]");
    agg_click(visual, "agg-mode-stages");
    let ids = agg_stage_ids(&app, visual);
    agg_click(visual, &format!("agg-stage-toggle-{}", ids[1]));
    agg_save_as(visual, "Active only");
    // The collection row is now expandable and lists the group, then the item.
    let expandable = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            this.navigator
                .visible_rows()
                .into_iter()
                .find(|row| row.label == "fixture_collection_00")
                .map(|row| row.expandable)
        })
    };
    assert_eq!(expandable(&app, visual), Some(true), "a collection with saved aggregations can expand");
    let other = app.read_with(visual, |this, _| {
        this.navigator.visible_rows().into_iter().find(|row| row.label == "fixture_collection_01").map(|row| row.expandable)
    });
    assert_eq!(other, Some(false), "other collections keep no chevron");
    expand_sidebar_row(&app, visual, "fixture_collection_00");
    assert!(sidebar_labels(&app, visual).iter().any(|label| label == "Aggregations (1)"), "{:?}", sidebar_labels(&app, visual));
    expand_sidebar_row(&app, visual, "Aggregations (1)");
    assert!(sidebar_labels(&app, visual).iter().any(|label| label == "Active only"));

    // Opening activates the existing tab (no duplicate), closing it and
    // opening again restores pipeline, options and the disabled stage.
    let item = app.read_with(visual, |this, _| {
        this.navigator.visible_rows().into_iter().find(|row| row.label == "Active only").unwrap()
    });
    app.update(visual, |this, cx| this.handle_navigator_click(&item, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        let count = this.workspace.tabs().iter().filter(|tab| tab.query_mode.is_mongo_aggregate()).count();
        assert_eq!(count, 1, "already open: activated");
    });
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    app.update(visual, |this, cx| this.close_tab(tab, cx));
    settle(visual);
    app.update(visual, |this, cx| this.handle_navigator_click(&item, cx));
    settle(visual);
    let draft = agg_draft(&app, visual);
    assert_eq!(draft.name.as_deref(), Some("Active only"));
    let ops: Vec<_> = draft.stages.iter().map(|stage| (stage.operator.as_str(), stage.enabled)).collect();
    assert_eq!(ops, [("$match", true), ("$limit", false)]);
    assert!(!draft.dirty);

    // Context menu: Rename.
    right_click_label(&app, visual, "Active only");
    assert!(visual.debug_bounds("agg-menu-open").is_some() && visual.debug_bounds("agg-menu-delete").is_some());
    click_debug_button(visual, "agg-menu-rename");
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Renamed");
    visual.simulate_keystrokes("enter");
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert_eq!(saved_rows(&app, visual)[0].name, "Renamed");
    assert!(sidebar_labels(&app, visual).iter().any(|label| label == "Renamed"), "{:?}", sidebar_labels(&app, visual));
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.title, "fixture_collection_00 · Renamed", "open tabs follow the rename");
    });

    // Delete (with confirmation).
    right_click_label(&app, visual, "Renamed");
    click_debug_button(visual, "agg-menu-delete");
    settle(visual);
    assert!(visual.debug_bounds("agg-delete-confirm").is_some());
    click_debug_button(visual, "agg-delete-confirm");
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert!(saved_rows(&app, visual).is_empty());
    assert_eq!(expandable(&app, visual), Some(false), "no saved aggregations: no chevron");
    let draft = agg_draft(&app, visual);
    assert!(draft.saved_id.is_none() && draft.dirty, "the open tab keeps its pipeline but is no longer saved");
}

#[gpui::test]
fn aggregate_tabs_survive_the_session_snapshot_with_disabled_stages_and_options(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }, { $sort: { count: -1 } }, { $limit: 5 }]");
    agg_click(visual, "agg-mode-stages");
    let ids = agg_stage_ids(&app, visual);
    agg_click(visual, &format!("agg-stage-toggle-{}", ids[1]));
    agg_click(visual, "agg-options");
    agg_set_input(visual, "agg-opt-sample", "9");
    visual.simulate_keystrokes("escape");
    settle(visual);
    let snapshot = app.read_with(visual, |this, _| this.workspace.snapshot());
    let json = crate::gpui_session::encode(&snapshot).expect("encode");
    let decoded: crate::gpui_session::GpuiSession = serde_json::from_slice(&json).expect("decode");
    let mut other = crate::workspace::WorkspaceState::default();
    other.restore_snapshot(&decoded, |_| true);
    let tab = other.tabs().iter().find(|tab| tab.query_mode.is_mongo_aggregate()).expect("restored");
    let draft = tab.aggregate.as_ref().unwrap();
    assert_eq!(draft.mode, crate::aggregate::EditorMode::Stages);
    assert_eq!(draft.options.sample_size, 9);
    let ops: Vec<_> = draft.stages.iter().map(|stage| (stage.operator.as_str(), stage.enabled)).collect();
    assert_eq!(ops, [("$match", true), ("$sort", false), ("$limit", true)]);
    assert_eq!(tab.collection, "fixture_collection_00");
    // An old session file without the field still loads (serde default).
    let legacy = r#"{"version":1,"tabs":[{"kind":"query","title":"x","query_mode":"mongoFind"}]}"#;
    let parsed: crate::gpui_session::GpuiSession = serde_json::from_str(legacy).unwrap();
    assert!(parsed.tabs[0].aggregate.is_none());
}

#[gpui::test]
fn rendering_never_parses_and_an_edit_derives_exactly_once(cx: &mut TestAppContext) {
    // Eight stages: with the old per-card `check()` every frame parsed all
    // stages once per card (O(N^2)).
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    agg_type_pipeline(
        visual,
        "[{ $match: { a: 1 } }, { $sort: { b: 1 } }, { $skip: 1 }, { $limit: 9 }, { $match: { c: 2 } }, { $sort: { d: -1 } }, { $skip: 2 }, { $limit: 8 }]",
    );
    agg_click(visual, "agg-mode-stages");
    agg_flush(visual);
    assert_eq!(agg_stage_ids(&app, visual).len(), 8);
    let parses = crate::mongo_aggregate::parse::parser_runs;
    let derives = crate::aggregate::derive_runs;
    let (p0, d0) = (parses(), derives());
    for _ in 0..12 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    assert_eq!(parses(), p0, "12 renders of an unchanged tab parsed nothing");
    assert_eq!(derives(), d0, "and re-derived nothing");

    // One keystroke into a stage body = one edit = one derive.
    let first = agg_stage_ids(&app, visual)[0];
    let body = agg_bounds(visual, format!("agg-stage-body-{first}")).unwrap();
    visual.simulate_click(body.center(), Default::default());
    settle(visual);
    let d1 = derives();
    visual.simulate_input("x");
    settle(visual);
    assert_eq!(derives() - d1, 1, "one edit, one analysis");
    let p1 = parses();
    for _ in 0..12 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    assert_eq!(parses(), p1, "rendering after the edit parses nothing either");
    assert_eq!(derives() - d1, 1);
    // The Text mode (and the Run output badge) are cached the same way.
    agg_click(visual, "agg-mode-text");
    let p2 = parses();
    for _ in 0..6 {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    assert_eq!(parses(), p2);
}

#[gpui::test]
fn cancelled_previews_keep_their_slot_until_they_finish(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_flush(visual);
    agg_type_pipeline(visual, "[{ $match: {} }, { $sort: { count: 1 } }, { $skip: 1 }, { $limit: 4 }]");
    agg_flush(visual);
    app.update(visual, |this, _| {
        this.scenario_ui.service.as_mut().unwrap().fixture_mut().set_aggregate_latency(Duration::from_secs(4));
    });
    agg_click(visual, "agg-mode-stages");
    agg_flush(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    app.read_with(visual, |this, _| assert_eq!(this.aggregate_ui.running_count(tab), 2));
    let requests = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| agg_calls(app, visual).len();
    let before = requests(&app, visual);
    // Edit a stage: both in-flight previews are cancelled, but the server is
    // still working on them, so no new request may start yet.
    let first = agg_stage_ids(&app, visual)[0];
    let body = agg_bounds(visual, format!("agg-stage-body-{first}")).unwrap();
    visual.simulate_click(body.center(), Default::default());
    settle(visual);
    visual.simulate_input(" ");
    agg_flush(visual);
    let cancels = app.read_with(visual, |this, _| this.scenario_ui.service.as_ref().unwrap().fixture().aggregate_cancels());
    assert_eq!(cancels, 2, "both in-flight previews were cancelled");
    app.read_with(visual, |this, _| assert_eq!(this.aggregate_ui.running_count(tab), 2, "cancelled jobs still hold their slots"));
    assert_eq!(requests(&app, visual), before, "the new generation waits for free slots");
    // When the cancelled requests finish, the queue moves on.
    visual.cx.executor().advance_clock(Duration::from_secs(4) + Duration::from_millis(100));
    settle(visual);
    assert!(requests(&app, visual) > before, "queued previews start once slots free up");
    app.read_with(visual, |this, _| assert!(this.aggregate_ui.running_count(tab) <= 2));
}

#[gpui::test]
fn a_late_answer_of_a_superseded_explain_leaves_the_newer_explain_alone(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }]");
    app.update(visual, |this, _| {
        this.scenario_ui.service.as_mut().unwrap().fixture_mut().set_aggregate_latency(Duration::from_secs(2));
    });
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    agg_click(visual, "agg-explain");
    let first = app.read_with(visual, |this, _| this.aggregate_ui.explain_state(tab)).expect("first explain");
    assert!(first.1, "first explain is in flight");
    visual.cx.executor().advance_clock(Duration::from_secs(1));
    // A second explain while the first is still waiting.
    app.update_in(visual, |this, window, cx| {
        this.explain_aggregate(tab, crate::mongo_aggregate::ExplainVerbosity::ExecutionStats, window, cx);
    });
    settle(visual);
    let second = app.read_with(visual, |this, _| this.aggregate_ui.explain_state(tab)).expect("second explain");
    assert_ne!(second.0, first.0);
    assert!(second.1);
    // The first one's answer arrives now (t = 2.1 s); the second is due at 3 s.
    visual.cx.executor().advance_clock(Duration::from_millis(1100));
    settle(visual);
    assert_eq!(
        app.read_with(visual, |this, _| this.aggregate_ui.explain_state(tab)),
        Some((second.0, true)),
        "the late answer of explain #1 must not mark explain #2 as answered"
    );
    visual.cx.executor().advance_clock(Duration::from_secs(1));
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.aggregate_ui.explain_state(tab)), Some((second.0, false)));
}

#[gpui::test]
fn a_save_that_finishes_after_further_edits_leaves_the_draft_dirty(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $match: { active: true } }]");
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    // Start the save and edit again before its completion runs.
    app.update(visual, |this, cx| {
        this.commit_aggregate_save(tab, Some("Racy".into()), false, cx);
        this.aggregate_draft_mut(tab).unwrap().apply_text("[{ $limit: 1 }]").unwrap();
    });
    settle(visual);
    let draft = agg_draft(&app, visual);
    assert_eq!(draft.name.as_deref(), Some("Racy"), "the save itself succeeded");
    assert!(draft.saved_id.is_some());
    assert!(draft.dirty, "the text typed during the save is not in the saved row");
    let rows = saved_rows(&app, visual);
    assert!(rows[0].pipeline_json.contains("active"), "{}", rows[0].pipeline_json);
    // A save with no edits in between is clean.
    app.update(visual, |this, cx| this.commit_aggregate_save(tab, None, false, cx));
    settle(visual);
    assert!(!agg_draft(&app, visual).dirty);
}

#[gpui::test]
fn a_failed_store_open_is_reported_once_and_retried_only_by_an_explicit_action(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    app.update(visual, |this, cx| {
        this.aggregate_store = Arc::new(std::sync::OnceLock::from(Err("disk is read-only".to_string())));
        this.refresh_saved_aggregates(crate::ui_fixture::MONGO_ID, "fixture_db_0", cx);
    });
    settle(visual);
    let notice = app.read_with(visual, |this, _| this.sidebar.notice.clone());
    assert!(notice.as_deref().is_some_and(|text| text.contains("read-only")), "{notice:?}");
    // Further sidebar loads neither retry the open nor repeat the message.
    app.update(visual, |this, cx| {
        this.sidebar.notice = None;
        this.refresh_saved_aggregates(crate::ui_fixture::MONGO_ID, "fixture_db_0", cx);
        this.refresh_saved_aggregates(crate::ui_fixture::MONGO_ID, "fixture_db_1", cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.sidebar.notice.is_none(), "reported once");
        assert!(this.aggregate_store.get().is_some_and(Result::is_err), "the failure is kept, not retried");
    });
    // Save is an explicit action: it retries (and succeeds here).
    agg_type_pipeline(visual, "[{ $match: {} }]");
    agg_save_as(visual, "After retry");
    app.read_with(visual, |this, _| assert!(this.aggregate_store.get().is_some_and(Result::is_ok)));
    assert_eq!(saved_rows(&app, visual).len(), 1);
}
