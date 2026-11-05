// Real-interaction proofs for the New Query layout: SQL editor and Mongo
// script tabs show only a full-height editor until their first run; the
// Results area appears with the run (success or error) and each tab keeps
// its own state across tab switches.

fn layout_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    (app, visual)
}

fn active_tab(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> u64 {
    app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id)
}

/// The editor fills the whole main panel: its area ends at the status bar and
/// no Results header / footer is mounted.
fn assert_editor_only(visual: &mut gpui::VisualTestContext, what: &str) {
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let status = visual.debug_bounds("status-bar").expect("status bar");
    assert!(
        (status.top() - area.bottom()).abs() < gpui::px(2.0),
        "{what}: the editor must reach the status bar: {area:?} vs {status:?}"
    );
    assert!(area.size.height > gpui::px(500.0), "{what}: full height, got {area:?}");
    assert!(visual.debug_bounds("results-header").is_none(), "{what}: no Results header");
    assert!(visual.debug_bounds("result-status-row").is_none(), "{what}: no result footer");
}

/// The Results area is mounted below an editor that kept its 160px body.
fn assert_results_revealed(visual: &mut gpui::VisualTestContext, what: &str) {
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let header = visual.debug_bounds("results-header").unwrap_or_else(|| panic!("{what}: Results header"));
    assert!(visual.debug_bounds("result-status-row").is_some(), "{what}: result footer");
    assert!(header.top() >= area.bottom() - gpui::px(1.0), "{what}: Results sits below the editor");
    assert!(area.size.height < gpui::px(260.0), "{what}: editor back to its fixed height, got {area:?}");
}

fn type_in_editor(visual: &mut gpui::VisualTestContext, text: &str) {
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    // The editor body is the lower part of the area, under any header / toolbar.
    visual.simulate_click(
        gpui::point(area.center().x, area.bottom() - gpui::px(40.0)),
        Default::default(),
    );
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

fn editor_text(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> String {
    app.read_with(visual, |this, cx| this.query_editors.by_tab[&tab].read(cx).value().to_string())
}

fn editor_entity(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
    tab: u64,
) -> gpui::EntityId {
    app.read_with(visual, |this, _| this.query_editors.by_tab[&tab].entity_id())
}

fn activate_tab_by_click(visual: &mut gpui::VisualTestContext, tab: u64) {
    let bounds = visual
        .debug_bounds(Box::leak(format!("workspace-tab-{tab}").into_boxed_str()))
        .unwrap_or_else(|| panic!("tab {tab} is in the tab bar"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

#[gpui::test]
fn new_sql_tab_shows_only_the_full_height_editor_until_run_then_keeps_its_state(
    cx: &mut TestAppContext,
) {
    let (app, visual) = layout_app(cx);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let first = active_tab(&app, visual);
    assert_editor_only(visual, "fresh SQL tab");
    // Run and History live in the editor's own header, left of Save.
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let run = visual.debug_bounds("run-query").expect("Run in the editor header");
    let history = visual.debug_bounds("query-history").expect("History in the editor header");
    let save = visual.debug_bounds("save-sql-script").expect("Save");
    assert!(area.contains(&run.center()) && area.contains(&history.center()), "inside the editor area");
    assert!(run.right() <= history.left() && history.right() <= save.left(), "Run, History, Save");
    assert_eq!((run.center().y, history.center().y), (save.center().y, save.center().y), "one header row");

    type_in_editor(visual, "select 1 as one");
    assert_eq!(editor_text(&app, visual, first), "select 1 as one");
    assert_editor_only(visual, "typing alone does not reveal results");
    let editor_id = editor_entity(&app, visual, first);

    click_debug_button(visual, "run-query");
    settle(visual);
    assert_results_revealed(visual, "after Run");
    assert_eq!(editor_text(&app, visual, first), "select 1 as one", "text survives the transition");
    assert_eq!(editor_entity(&app, visual, first), editor_id, "the editor entity is reused");

    // A second New Query tab starts editor-only; the first keeps its results.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let second = active_tab(&app, visual);
    assert_ne!(first, second);
    assert_editor_only(visual, "second fresh tab");
    type_in_editor(visual, "select 2");

    activate_tab_by_click(visual, first);
    assert_eq!(active_tab(&app, visual), first);
    assert_results_revealed(visual, "first tab after switching back");
    assert_eq!(editor_text(&app, visual, first), "select 1 as one");

    activate_tab_by_click(visual, second);
    assert_editor_only(visual, "second tab after switching back");
    assert_eq!(editor_text(&app, visual, second), "select 2", "unrun text is kept");
    assert_eq!(editor_entity(&app, visual, first), editor_id);
}

#[gpui::test]
fn restored_sql_tab_with_saved_text_but_no_result_shows_the_full_editor(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    app.update(visual, |this, cx| {
        let id = this.workspace.open_query_tab("Restored");
        this.workspace.tab_mut(id).unwrap().sql = "select * from saved".into();
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);
    assert_editor_only(visual, "restored tab");
    let tab = active_tab(&app, visual);
    assert_eq!(editor_text(&app, visual, tab), "select * from saved");
}

#[gpui::test]
fn keyboard_run_and_a_failed_run_both_reveal_the_results_area(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    // Mongo script tab; the default scenario refuses script runs, so the run
    // fails and its error must still be shown.
    open_mongo_script_tab_in_default_scenario(&app, visual);
    let tab = active_tab(&app, visual);
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab).unwrap().query_mode.is_mongo_script()));
    assert_editor_only(visual, "fresh Mongo script tab");
    assert!(visual.debug_bounds("mongo-script-toolbar").is_some(), "toolbar stays above the editor");

    type_in_editor(visual, "db.fixture_collection_00.find({})");
    let editor_id = editor_entity(&app, visual, tab);
    visual.simulate_keystrokes("secondary-enter");
    settle(visual);
    assert_results_revealed(visual, "after the keyboard shortcut");
    assert!(
        visual.debug_bounds("result-empty-state").is_some(),
        "the failure message is rendered in the revealed result body"
    );
    app.read_with(visual, |this, _| {
        assert!(matches!(
            this.workspace.runtime(tab).unwrap().query.status(),
            crate::query::QueryStatus::Failed(error) if error.contains("unavailable in a UI scenario")
        ));
    });
    assert_eq!(editor_entity(&app, visual, tab), editor_id);
    // Cmd+Enter is the editor's own secondary-enter (it may append a newline).
    assert_eq!(editor_text(&app, visual, tab).trim_end(), "db.fixture_collection_00.find({})");
}

fn open_mongo_script_tab_in_default_scenario(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) {
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        let id = crate::ui_fixture::MONGO_ID;
        let epoch = this.sessions.begin_connect(id).unwrap();
        this.sessions.finish_connect(id, epoch, Ok(()));
        cx.notify();
    });
    settle(visual);
    right_click_label(app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-query");
    settle(visual);
}

#[gpui::test]
fn mongo_script_tab_reveals_results_on_run_and_each_tab_keeps_its_own_state(
    cx: &mut TestAppContext,
) {
    let (app, visual) = script_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    let script = active_tab(&app, visual);
    assert_editor_only(visual, "fresh script tab");

    click_debug_button(visual, "run-query");
    settle(visual);
    assert_results_revealed(visual, "script after Run");
    assert_eq!(script_rows(&app, visual), 200);

    // Run Selected on a second, fresh script tab reveals its results too.
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-query");
    settle(visual);
    let second = active_tab(&app, visual);
    assert_ne!(script, second);
    assert_editor_only(visual, "second fresh script tab");
    click_debug_button(visual, "script-run-selected");
    settle(visual);
    assert_results_revealed(visual, "second script after Run Selected");

    // Switching back and forth keeps every tab's own state.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    assert_editor_only(visual, "SQL tab opened last");
    activate_tab_by_click(visual, script);
    assert_results_revealed(visual, "first script after switching back");
    assert_eq!(script_rows(&app, visual), 200);
}

#[gpui::test]
fn table_data_tabs_keep_their_results_area_without_a_run(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.open_query_tab_with_metadata(
            "orders",
            TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                query_mode: Some(QueryMode::SqlTable),
                ..Default::default()
            },
        );
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);
    assert!(visual.debug_bounds("query-editor-area").is_none(), "no editor on a table tab");
    assert!(visual.debug_bounds("result-status-row").is_some(), "table tabs always show results");
}
