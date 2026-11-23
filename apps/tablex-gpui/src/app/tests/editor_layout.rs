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

/// Ctrl+Enter, the chord other database clients use, runs the query too (on macOS, where
/// Cmd+Enter is the editor's own; elsewhere Ctrl+Enter is that same editor binding).
#[gpui::test]
fn ctrl_enter_runs_the_query_like_cmd_enter(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    open_mongo_script_tab_in_default_scenario(&app, visual);
    let tab = active_tab(&app, visual);
    type_in_editor(visual, "db.fixture_collection_00.find({})");
    visual.simulate_keystrokes("ctrl-enter");
    settle(visual);
    assert_results_revealed(visual, "after Ctrl+Enter");
    app.read_with(visual, |this, _| {
        assert!(matches!(
            this.workspace.runtime(tab).unwrap().query.status(),
            crate::query::QueryStatus::Failed(error) if error.contains("unavailable in a UI scenario")
        ), "the run was started");
    });
    assert_eq!(editor_text(&app, visual, tab).trim_end(), "db.fixture_collection_00.find({})");
}

/// Option+X (Run Script) and Option+P (Run From Cursor) run a Mongo script tab, and type nothing.
#[gpui::test]
fn option_x_and_option_p_run_a_mongo_script_tab(cx: &mut TestAppContext) {
    for chord in ["alt-x", "alt-p"] {
        let (app, visual) = layout_app(cx);
        visual.update(|_, cx| actions::bind_keys(cx));
        open_mongo_script_tab_in_default_scenario(&app, visual);
        let tab = active_tab(&app, visual);
        type_in_editor(visual, "db.fixture_collection_00.find({})");
        visual.simulate_keystrokes(chord);
        settle(visual);
        assert_results_revealed(visual, chord);
        app.read_with(visual, |this, _| {
            assert!(matches!(
                this.workspace.runtime(tab).unwrap().query.status(),
                crate::query::QueryStatus::Failed(error) if error.contains("unavailable in a UI scenario")
            ), "{chord} started the run");
        });
        assert_eq!(editor_text(&app, visual, tab).trim_end(), "db.fixture_collection_00.find({})");
    }
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

#[gpui::test]
fn the_splitter_between_the_editor_and_the_results_resizes_both(cx: &mut TestAppContext) {
    let (_app, visual) = layout_app(cx);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    type_in_editor(visual, "select 1 as one");
    click_debug_button(visual, "run-query");
    settle(visual);
    assert_results_revealed(visual, "after Run");
    let drag = |visual: &mut gpui::VisualTestContext, dy: f32| {
        let area = visual.debug_bounds("query-editor-area").expect("editor area");
        let handle = gpui::point(area.center().x, area.bottom());
        let left = Some(gpui::MouseButton::Left);
        let to = handle + gpui::point(gpui::px(0.0), gpui::px(dy));
        visual.simulate_mouse_down(handle, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_move(handle + gpui::point(gpui::px(0.0), gpui::px(dy / 3.0)), left, Default::default());
        visual.simulate_mouse_move(to, left, Default::default());
        visual.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
        settle(visual);
    };
    // Down: a short result gives its room to the editor.
    let before = visual.debug_bounds("query-editor-area").unwrap();
    drag(visual, 300.0);
    let taller = visual.debug_bounds("query-editor-area").unwrap();
    assert!(taller.size.height > before.size.height + gpui::px(250.0), "{before:?} -> {taller:?}");
    let header = visual.debug_bounds("results-header").expect("the Results stay mounted");
    assert!(header.top() >= taller.bottom() - gpui::px(1.0), "below the editor");
    assert!(visual.debug_bounds("result-status-row").is_some(), "with their footer in view");
    // Up: the editor shrinks, never below its header and a few lines.
    drag(visual, -2000.0);
    let shortest = visual.debug_bounds("query-editor-area").unwrap();
    assert!(shortest.size.height < before.size.height, "{shortest:?}");
    assert!(shortest.size.height >= gpui::px(theme::HEADER_HEIGHT + 59.0), "{shortest:?}");
}

// ---- where the results show (`results::placement`) ---------------------------------------------

/// A new SQL tab that ran `sql`; its id.
fn ran_sql_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, sql: &str) -> u64 {
    click_debug_button(visual, "global-new-query");
    settle(visual);
    type_in_editor(visual, sql);
    click_debug_button(visual, "run-query");
    settle(visual);
    assert_results_revealed(visual, "after Run");
    active_tab(app, visual)
}

/// Picks `place` ("below", "right", "tab", "window") in the Results header's menu.
fn pick_results_place(visual: &mut gpui::VisualTestContext, place: &str) {
    click_debug_button(visual, "results-place");
    settle(visual);
    let item = Box::leak(format!("results-place-{place}").into_boxed_str());
    click_debug_button(visual, item);
    settle(visual);
}

#[gpui::test]
fn the_results_menu_puts_the_results_right_of_the_editor_and_back_below(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = ran_sql_tab(&app, visual, "select 1 as one");
    pick_results_place(visual, "right");
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let header = visual.debug_bounds("results-header").expect("Results header");
    assert!(header.left() >= area.right() - gpui::px(1.0), "side by side: {area:?} {header:?}");
    assert!(header.top() < area.center().y, "level with the editor");
    assert!(area.size.height > gpui::px(500.0), "the editor has the full height: {area:?}");
    app.read_with(visual, |this, _| {
        assert_eq!(this.results_place(tab), crate::app::results::placement::ResultsPlace::Right)
    });
    pick_results_place(visual, "below");
    assert_results_revealed(visual, "back below");
}

#[gpui::test]
fn the_setting_puts_the_results_of_every_tab_right_of_the_editor(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    app.update(visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.editor.results_position = crate::settings::ResultsPosition::Right;
        this.settings = std::sync::Arc::new(settings);
        cx.notify();
    });
    click_debug_button(visual, "global-new-query");
    settle(visual);
    type_in_editor(visual, "select 1 as one");
    click_debug_button(visual, "run-query");
    settle(visual);
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let header = visual.debug_bounds("results-header").expect("Results header");
    assert!(header.left() >= area.right() - gpui::px(1.0), "{area:?} {header:?}");
}

#[gpui::test]
fn results_in_a_tab_of_their_own_follow_their_query_tab_and_come_back(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    pick_results_place(visual, "tab");
    // The Results tab is open and active, and shows the query tab's results.
    let host = active_tab(&app, visual);
    assert_ne!(host, query);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tab(host).unwrap().kind, TabKind::Results);
        assert_eq!(this.results_source(host), Some(query));
    });
    assert!(visual.debug_bounds("results-header").is_some(), "the results are in the Results tab");
    assert!(visual.debug_bounds("result-status-row").is_some(), "with their footer");
    assert!(visual.debug_bounds("query-editor-area").is_none(), "and no editor");

    // The query tab: the whole height for the editor, a note where the results were.
    activate_tab_by_click(visual, query);
    assert!(visual.debug_bounds("results-header").is_none(), "shown in one place only");
    let note = visual.debug_bounds("results-moved-note").expect("the note");
    let area = visual.debug_bounds("query-editor-area").unwrap();
    assert!(note.top() >= area.bottom() - gpui::px(1.0) && area.size.height > gpui::px(450.0));

    // A run of the query tab shows in its Results tab.
    let before = app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation());
    type_in_editor(visual, "select 2 as two");
    click_debug_button(visual, "run-query");
    settle(visual);
    let after = app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation());
    assert!(after > before, "the query tab ran");
    click_debug_button(visual, "results-moved-show");
    assert_eq!(active_tab(&app, visual), host, "Show goes to the Results tab");
    assert!(visual.debug_bounds("results-header").is_some());
    // Run from the Results tab runs its query tab.
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation()) > after, "Run in the Results tab runs its query");

    // Bring back: the Results tab closes, the query tab shows its results again.
    activate_tab_by_click(visual, query);
    click_debug_button(visual, "results-moved-back");
    settle(visual);
    app.read_with(visual, |this, _| assert!(this.workspace.tab(host).is_none(), "the Results tab closed"));
    assert_results_revealed(visual, "brought back");

    // Closing the Results tab brings them back too.
    pick_results_place(visual, "tab");
    let host = active_tab(&app, visual);
    let closed_before = app.read_with(visual, |this, _| this.shell.closed_tabs.len());
    app.update(visual, |this, cx| this.close_tab(host, cx));
    settle(visual);
    activate_tab_by_click(visual, query);
    assert_results_revealed(visual, "after the Results tab closed");
    app.read_with(visual, |this, _| {
        assert_eq!(this.results_place(query), crate::app::results::placement::ResultsPlace::Below);
        assert_eq!(this.shell.closed_tabs.len(), closed_before, "Reopen Closed Tab does not bring a Results tab back");
    });

    // Closing the query tab takes its Results tab along.
    pick_results_place(visual, "tab");
    let host = active_tab(&app, visual);
    app.update(visual, |this, cx| this.close_tab(query, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab(host).is_none(), "the Results tab went with its query tab");
        assert!(this.results_source(host).is_none());
    });
}

#[gpui::test]
fn results_in_a_window_of_their_own_render_there_and_come_back_when_it_closes(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    let windows = |visual: &mut gpui::VisualTestContext| visual.cx.update(|cx| cx.windows().len());
    assert_eq!(windows(visual), 1);
    pick_results_place(visual, "window");
    assert_eq!(windows(visual), 2, "a window of their own");
    assert!(visual.debug_bounds("results-header").is_none(), "not in the query tab any more");
    assert!(visual.debug_bounds("results-moved-note").is_some(), "a note in their place");
    let handle = app.read_with(visual, |this, _| this.results_window(query)).expect("the window is kept");

    // The window draws the query tab's results.
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    other.update(|window, _| window.refresh());
    other.run_until_parked();
    assert!(other.debug_bounds("results-window").is_some());
    assert!(other.debug_bounds("results-header").is_some(), "the results render in the window");
    assert!(other.debug_bounds("result-status-row").is_some());

    // A run of the query tab shows there.
    let before = app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation());
    type_in_editor(visual, "select 2 as two");
    click_debug_button(visual, "run-query");
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation()) > before, "the query tab ran");
    other.update(|window, _| window.refresh());
    other.run_until_parked();
    assert!(other.debug_bounds("results-header").is_some());

    // Its buttons work there: Refresh in the window re-runs the query tab's result.
    let before = app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation());
    let refresh = other.debug_bounds("result-refresh").expect("Refresh in the window");
    other.simulate_click(refresh.center(), Default::default());
    other.run_until_parked();
    settle(visual);
    assert!(
        app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation()) > before,
        "Refresh clicked in the results window re-ran the query"
    );

    // The close box: the results come back to the tab.
    assert!(other.simulate_close(), "the window may close");
    other.update(|window, _| window.remove_window());
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.results_window(query).is_none());
        assert_eq!(this.results_place(query), crate::app::results::placement::ResultsPlace::Below);
    });
    assert_results_revealed(visual, "after the window closed");

    // Bring back from the note closes the window.
    pick_results_place(visual, "window");
    assert_eq!(windows(visual), 2);
    click_debug_button(visual, "results-moved-back");
    settle(visual);
    assert_eq!(windows(visual), 1, "Bring back closes the window");
    assert_results_revealed(visual, "brought back");

    // Closing the query tab closes its window.
    pick_results_place(visual, "window");
    assert_eq!(windows(visual), 2);
    app.update(visual, |this, cx| this.close_tab(query, cx));
    settle(visual);
    assert_eq!(windows(visual), 1, "the window went with its query tab");
}

/// The window that holds the app, and the window `AppRoot` runs its `subscribe_in` callbacks in.
fn app_roots_current_window(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> (gpui::AnyWindowHandle, Option<gpui::AnyWindowHandle>) {
    let id = app.entity_id();
    visual.cx.update(|cx| {
        let (main, _) = crate::app::menus::app_window(cx).expect("the app's window");
        (main, cx.with_window(id, |window, _| window.window_handle()))
    })
}

#[gpui::test]
fn picking_a_place_in_the_results_window_closes_it_and_brings_the_results_back(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    let windows = |visual: &mut gpui::VisualTestContext| visual.cx.update(|cx| cx.windows().len());
    pick_results_place(visual, "window");
    assert_eq!(windows(visual), 2);
    let handle = app.read_with(visual, |this, _| this.results_window(query)).expect("the window is kept");
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    settle(&mut other);
    assert!(other.debug_bounds("results-header").is_some());

    // AppRoot's callbacks run in the app's window again once the results window has drawn.
    settle(visual);
    let (main, current) = app_roots_current_window(&app, visual);
    assert_eq!(current, Some(main), "the app's window is AppRoot's current window");

    // The menu of the results window: Below. Its window closes (after the menu's own update).
    click_debug_button(&mut other, "results-place");
    settle(&mut other);
    click_debug_button(&mut other, "results-place-below");
    settle(visual);
    assert_eq!(windows(visual), 1, "the results window closed");
    app.read_with(visual, |this, _| {
        assert!(this.results_window(query).is_none());
        assert_eq!(this.results_place(query), crate::app::results::placement::ResultsPlace::Below);
    });
    assert_results_revealed(visual, "back below");
    assert_eq!(app_roots_current_window(&app, visual).1, Some(main));
}

#[gpui::test]
fn the_results_window_handles_run_find_and_close_tab(cx: &mut TestAppContext) {
    use crate::app::{CloseActiveTab, OpenGridFind, RunActiveQuery};
    let (app, visual) = layout_app(cx);
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    let windows = |visual: &mut gpui::VisualTestContext| visual.cx.update(|cx| cx.windows().len());
    pick_results_place(visual, "window");
    let handle = app.read_with(visual, |this, _| this.results_window(query)).expect("the window is kept");
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    settle(&mut other);
    // Another tab is the active one of the app's window.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    assert_ne!(active_tab(&app, visual), query);

    // Run (Cmd+Enter) in the window runs the query tab of its results.
    let generation = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.runtime(query).unwrap().query.generation())
    };
    let before = generation(&app, visual);
    other.dispatch_action(RunActiveQuery);
    other.run_until_parked();
    settle(visual);
    assert!(generation(&app, visual) > before, "Run in the results window ran its query tab");

    // Find (Cmd+F) opens the find bar of its results, in that window. (The test's query has no
    // connection: the results get a page of rows by hand.)
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
        "a",
        arrow_schema::DataType::Utf8,
        true,
    )]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![std::sync::Arc::new(arrow_array::StringArray::from(vec!["x", "y"])) as arrow_array::ArrayRef],
    )
    .unwrap();
    app.update(visual, |this, cx| {
        this.workspace.activate_tab(query);
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(batch)]),
        )));
        cx.notify();
    });
    click_debug_button(visual, "global-new-query");
    settle(visual);
    settle(&mut other);
    assert!(other.debug_bounds("grid-find-input").is_none());
    other.dispatch_action(OpenGridFind);
    other.run_until_parked();
    settle(&mut other);
    assert!(other.debug_bounds("grid-find-input").is_some(), "the find bar opened in the results window");

    // Close Tab (Cmd+W) brings the results back and closes the window.
    other.dispatch_action(CloseActiveTab);
    other.run_until_parked();
    settle(visual);
    assert_eq!(windows(visual), 1, "the results window closed");
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab(query).is_some(), "the query tab stays");
        assert_eq!(this.results_place(query), crate::app::results::placement::ResultsPlace::Below);
    });
}

#[gpui::test]
fn a_results_tab_opens_after_its_query_tab_and_keeps_it_on_close_others(cx: &mut TestAppContext) {
    use crate::workspace::TabCloseScope;
    let (app, visual) = layout_app(cx);
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    // A tab after the query tab: the Results tab still opens right after the query tab.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let other_tab = active_tab(&app, visual);
    activate_tab_by_click(visual, query);
    pick_results_place(visual, "tab");
    let host = active_tab(&app, visual);
    app.read_with(visual, |this, _| {
        let order: Vec<u64> = this.workspace.tabs().iter().map(|tab| tab.id).collect();
        let at = order.iter().position(|id| *id == query).unwrap();
        assert_eq!(order[at + 1], host, "next to its query tab: {order:?}");
        // Close Others / Left of the Results tab keep its query tab (closing it closes the anchor).
        let others = this.tab_close_targets(host, TabCloseScope::Others);
        assert!(others.contains(&other_tab) && !others.contains(&query), "{others:?}");
        assert!(!this.tab_close_targets(host, TabCloseScope::Left).contains(&query));
        assert!(this.tab_close_targets(query, TabCloseScope::Others).contains(&other_tab));
    });
}

#[gpui::test]
fn the_moved_note_shows_before_the_first_run_too(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let query = active_tab(&app, visual);
    // A tab that has not run (its results were reset): they are still in one place only.
    app.update(visual, |this, cx| {
        this.set_results_place(query, crate::app::results::placement::ResultsPlace::Tab, cx)
    });
    settle(visual);
    activate_tab_by_click(visual, query);
    assert!(visual.debug_bounds("results-moved-note").is_some(), "the note shows");
}

/// The empty result area of an editor tab that has no result (its run was withdrawn, so it counts
/// as run but is idle) offers Run; a new tab shows only its editor, so it never shows the area.
#[gpui::test]
fn the_empty_result_area_runs_the_query_with_its_run_button(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    assert!(visual.debug_bounds("empty-run-query").is_none(), "no result area before the first run");
    let tab = active_tab(&app, visual);
    type_in_editor(visual, "select 1 as one");
    app.update(visual, |this, cx| {
        this.workspace.runtime_mut(tab).unwrap().last_executed_query = Some(String::new());
        cx.notify();
    });
    settle(visual);
    assert!(visual.debug_bounds("result-empty-state").is_some(), "the empty state shows");
    let generation = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().query.generation())
    };
    let before = generation(&app, visual);
    click_debug_button(visual, "empty-run-query");
    settle(visual);
    assert!(generation(&app, visual) > before, "the button ran the tab's query");
    assert!(visual.debug_bounds("empty-run-query").is_none(), "the result replaces the empty state");
}

/// A table-data, Mongo find or Redis key tab that sits idle (restored without running, or its result
/// withdrawn) offers the same run, worded as a load.
#[gpui::test]
fn the_empty_result_area_of_a_browse_tab_loads_the_data_with_its_button(cx: &mut TestAppContext) {
    for mode in [QueryMode::SqlTable, QueryMode::MongoFind, QueryMode::RedisKey] {
        let (app, visual) = layout_app(cx);
        click_debug_button(visual, "global-new-query");
        settle(visual);
        let tab = active_tab(&app, visual);
        app.update(visual, |this, cx| {
            this.workspace.tab_mut(tab).unwrap().query_mode = Some(mode.clone());
            this.workspace.tab_mut(tab).unwrap().collection = "items".into();
            this.workspace.runtime_mut(tab).unwrap().last_executed_query = Some(String::new());
            cx.notify();
        });
        settle(visual);
        assert!(visual.debug_bounds("empty-run-query").is_some(), "{mode:?}: the idle tab has a button");
        let generation = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
            app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().query.generation())
        };
        let before = generation(&app, visual);
        click_debug_button(visual, "empty-run-query");
        settle(visual);
        assert!(generation(&app, visual) > before, "{mode:?}: the button started a run");
    }
}

// ---- hide / show the results (`results.toggle`) -------------------------------------------------

#[gpui::test]
fn the_toggle_results_key_hides_the_results_and_shows_them_again_where_they_were(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    // Below: hidden, the editor has the whole panel; shown again, it is below the editor.
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    // The editor has the panel down to a strip that says the results are hidden (and shows them again).
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let strip = visual.debug_bounds("results-hidden-note").expect("the strip of hidden results");
    let status = visual.debug_bounds("status-bar").expect("status bar");
    assert!(strip.top() >= area.bottom() - gpui::px(1.0) && (status.top() - strip.bottom()).abs() < gpui::px(2.0), "hidden below: editor, strip, status bar: {area:?} {strip:?} {status:?}");
    assert!(area.size.height > gpui::px(500.0), "hidden below: the editor has the rest: {area:?}");
    assert!(visual.debug_bounds("results-header").is_none(), "hidden below: no Results header");
    app.read_with(visual, |this, _| assert!(this.results_hidden(query)));
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    assert_results_revealed(visual, "shown again below");

    // Right: the same, and the results come back to the right.
    pick_results_place(visual, "right");
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    assert!(visual.debug_bounds("results-header").is_none(), "hidden on the right");
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    assert!(area.size.width > gpui::px(800.0), "the editor has the whole width: {area:?}");
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let header = visual.debug_bounds("results-header").expect("Results header");
    assert!(header.left() >= area.right() - gpui::px(1.0), "back on the right: {area:?} {header:?}");

    // The menu entry hides them too; the next run shows them again.
    click_debug_button(visual, "results-place");
    settle(visual);
    click_debug_button(visual, "results-hide");
    settle(visual);
    assert!(visual.debug_bounds("results-header").is_none(), "hidden from the menu");
    click_debug_button(visual, "run-query");
    settle(visual);
    assert!(visual.debug_bounds("results-header").is_some(), "a run shows the results again");
    app.read_with(visual, |this, _| assert!(!this.results_hidden(query)));

    // Hidden, then a place picked in code: shown there.
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    app.update(visual, |this, cx| {
        this.set_results_place(query, crate::app::results::placement::ResultsPlace::Below, cx)
    });
    settle(visual);
    assert_results_revealed(visual, "a place shows them");
}

#[gpui::test]
fn the_toggle_results_key_brings_a_results_tab_or_window_to_the_front(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    pick_results_place(visual, "tab");
    let host = active_tab(&app, visual);
    // From the Results tab: the query tab.
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    assert_eq!(active_tab(&app, visual), query);
    // From the query tab: the Results tab.
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    assert_eq!(active_tab(&app, visual), host);
    app.read_with(visual, |this, _| assert!(!this.results_hidden(query), "a Results tab is never hidden"));

    // A window: the key in the app's window activates it; nothing gets hidden.
    activate_tab_by_click(visual, query);
    app.update(visual, |this, cx| {
        this.set_results_place(query, crate::app::results::placement::ResultsPlace::Window, cx)
    });
    settle(visual);
    let handle = app.read_with(visual, |this, _| this.results_window(query)).expect("the window");
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    app.read_with(visual, |this, _| assert!(!this.results_hidden(query)));
    assert!(visual.debug_bounds("results-moved-note").is_some(), "the note stays");
    // The key in the results window goes back to the app's window without hiding anything.
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    settle(&mut other);
    other.simulate_keystrokes("secondary-6");
    other.run_until_parked();
    assert!(other.debug_bounds("results-header").is_some(), "the results stay in their window");
}

/// The app-level commands (Settings, Assistant, Cmd-S, Format, Open SQL file) find the app's window when a
/// results window is the active one: that window holds no `AppRoot` of its own.
#[gpui::test]
fn settings_opens_from_the_results_window(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    app.update(visual, |this, cx| {
        this.set_results_place(query, crate::app::results::placement::ResultsPlace::Window, cx)
    });
    settle(visual);
    let handle = app.read_with(visual, |this, _| this.results_window(query)).expect("the window");
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    other.update(|window, _| window.activate_window());
    settle(&mut other);
    other.dispatch_action(actions::OpenSettings);
    other.run_until_parked();
    settle(visual);
    let active = app.read_with(visual, |this, _| this.workspace.active_tab().map(|tab| tab.kind));
    assert_eq!(active, Some(crate::workspace::TabKind::Settings), "Settings opened in the app's window");
}

#[gpui::test]
fn format_from_the_results_window_leaves_the_app_windows_tab_alone(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    app.update(visual, |this, cx| {
        this.set_results_place(query, crate::app::results::placement::ResultsPlace::Window, cx)
    });
    settle(visual);
    // Another query tab is the app window's active tab.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    type_in_editor(visual, "select   2   as   two");
    let other_tab = active_tab(&app, visual);
    assert_ne!(other_tab, query);
    let before = editor_text(&app, visual, other_tab);
    let handle = app.read_with(visual, |this, _| this.results_window(query)).expect("the window");
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    other.update(|window, _| window.activate_window());
    settle(&mut other);
    other.dispatch_action(actions::FormatSql);
    other.run_until_parked();
    settle(visual);
    assert_eq!(editor_text(&app, visual, other_tab), before, "Format in the results window is not applied to another tab");
}

#[gpui::test]
fn the_toggle_results_shortcut_is_listed_and_rebindable(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_shortcuts_page(&app, visual);
    assert!(rendered(visual, "shortcut-results.toggle".into()), "listed");
    // Cmd+6 on macOS, Ctrl+6 elsewhere (`secondary-`).
    assert_eq!(
        crate::app::shortcuts::find("results.toggle").unwrap().default_chords(),
        vec!["secondary-6".to_owned()]
    );
    // A free chord rebinds it like any other entry.
    record_chord(visual, "shortcut-chip-results.toggle-0".into(), "alt-shift-6");
    assert_eq!(
        draft_shortcuts(&app, visual).get("results.toggle"),
        Some(&vec!["alt-shift-6".to_owned()])
    );
    click(visual, "settings-apply".into());
    let bound = visual.update(|_, cx| {
        app_bindings(cx).contains(&format!(
            "{:?}",
            shortcuts::key_binding(cx, "alt-shift-6", actions::ToggleResults, None)
        ))
    });
    assert!(bound, "the new chord is live");
}
