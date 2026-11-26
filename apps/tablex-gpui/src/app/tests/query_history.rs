// Real-interaction proofs for the Query History: the tab and its entry points, search and filters, the
// preview, every action, Clear with its confirmation, MCP calls from the audit log, the retention and the
// recording of runs (failures with their error), and the per-tab strip (click inserts, double click runs).
// What the storage does (migration, search, paging) is covered in `engine-storage`; the merge of the two
// sources and the date ranges in `history/model.rs`.
//
// The history table is shared by every test of the process (one temporary data dir), so each test files its
// rows under a statement tag of its own and narrows the tab to it with the search box.

use engine_storage::history::{self as store_history, Record};

fn history_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.dispatcher.allow_parking();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
        actions::bind_keys(cx);
        cx.set_reduce_motion(true);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(AppRoot::new_isolated);
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(900.0)));
    visual.update(|window, _| window.activate_window());
    settle(visual);
    (app, visual)
}

/// Runs the scheduler until `ready`, moving the test clock on so that the search box's wait and the like end.
fn wait_for(visual: &mut gpui::VisualTestContext, what: &str, mut ready: impl FnMut(&mut gpui::VisualTestContext) -> bool) {
    let start = std::time::Instant::now();
    loop {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        if ready(visual) {
            return;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "timed out waiting for {what}");
        visual.cx.executor().advance_clock(std::time::Duration::from_millis(60));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn tag() -> String {
    format!("tag{}", uuid::Uuid::new_v4().to_simple())
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

/// One run to file: `minutes_ago` before now.
struct Seed {
    connection: String,
    database: &'static str,
    sql: String,
    minutes_ago: f64,
    error: Option<&'static str>,
    duration_ms: Option<i64>,
    rows: i64,
}

fn seed(connection: &str, sql: String, minutes_ago: f64) -> Seed {
    Seed { connection: connection.into(), database: "app", sql, minutes_ago, error: None, duration_ms: Some(12), rows: 3 }
}

fn file(seeds: Vec<Seed>) {
    let conn = engine_storage::app_db::open(&engine_storage::app_db::default_path()).unwrap();
    for seed in seeds {
        store_history::record_in(
            &conn,
            &Record {
                conn_uuid: &seed.connection,
                database: seed.database,
                collection: "",
                filter: &seed.sql,
                project: "",
                sort: "",
                skip: 0,
                limit: 200,
                max_time_ms: 0,
                result_count: seed.rows,
                error: seed.error,
                duration_ms: seed.duration_ms,
                kind: store_history::classify_sql(&seed.sql),
            },
            now_secs() - seed.minutes_ago * 60.0,
        )
        .unwrap();
    }
}

fn stored(tag: &str) -> usize {
    store_history::count(&store_history::HistoryFilter { text: tag.into(), ..Default::default() }).unwrap() as usize
}

fn history_view(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> gpui::Entity<history::HistoryView> {
    app.read_with(visual, |this, _| this.history.view.clone().expect("the History tab is open"))
}

fn open_history(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_query_history(None, window, cx)));
    settle(visual);
}

fn statements(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    let view = history_view(app, visual);
    view.read_with(visual, |view, cx| view.statements(cx))
}

fn wait_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, label: &'static str, ready: impl Fn(&[String]) -> bool) {
    let view = history_view(app, visual);
    // `settled`: no read is on its way (a stale page must not pass for the answer).
    wait_for(visual, label, |visual| view.read_with(visual, |view, cx| view.settled() && ready(&view.statements(cx))));
    settle(visual);
}

/// Clicks the box, replaces what it holds with `text`.
fn type_in_box(visual: &mut gpui::VisualTestContext, selector: &'static str, text: &str) {
    click_debug_button(visual, selector);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
}

/// Types `text` in the search box and waits for the rows that are asked for.
fn search_for(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, text: &str, rows: usize) {
    type_in_box(visual, "history-search", text);
    wait_rows(app, visual, "the search", |found| found.len() == rows);
}

/// Opens the tab on exactly the rows `tag` has.
fn open_on(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tag: &str, rows: usize) {
    open_history(app, visual);
    search_for(app, visual, tag, rows);
}

fn click_row(visual: &mut gpui::VisualTestContext, index: usize) {
    let selector: &'static str = Box::leak(format!("history-row-{index}").into_boxed_str());
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

fn selected_sql(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Option<String> {
    let view = history_view(app, visual);
    view.read_with(visual, |view, _| view.selected_row().map(|row| row.row.statement().to_owned()))
}

fn active_kind_of(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> Option<TabKind> {
    app.read_with(visual, |this, _| this.workspace.active_tab().map(|tab| tab.kind))
}

fn dialog_open(visual: &mut gpui::VisualTestContext) -> bool {
    visual.debug_bounds("dialog-layer").is_some()
}

/// A saved connection the tab can name (never opened).
fn add_connection(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, name: &str) -> String {
    let id = uuid::Uuid::new_v4();
    let name = name.to_owned();
    app.update(visual, |this, cx| {
        this.saved_connections.push(SavedConnection {
            id,
            name,
            uri: "sqlite:///tmp/none.db".into(),
            kind: ConnectionKind::Sqlite,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        });
        cx.notify();
    });
    id.to_string()
}

// ---- the tab -----------------------------------------------------------------------------------------

#[gpui::test]
fn history_the_tab_opens_from_the_palette_the_view_menu_and_the_strip_and_is_one_tab(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    // The palette command.
    visual.update(|window, cx| {
        app.update(cx, |this, cx| command_palette::StaticAction::QueryHistory.run(this, window, cx));
    });
    settle(visual);
    assert_eq!(active_kind_of(&app, visual), Some(TabKind::History));
    assert!(visual.debug_bounds("history-view").is_some(), "the tab is drawn");
    // The View menu's action (the keyboard focus is in the tab, where the shell's handler finds it)
    // shows the same tab, it does not make a second one.
    click_debug_button(visual, "history-search");
    visual.dispatch_action(OpenQueryHistory);
    settle(visual);
    assert_eq!(active_kind_of(&app, visual), Some(TabKind::History));
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::History).count());
    assert_eq!(tabs, 1);
    // Closing the tab lets go of its rows.
    app.update(visual, |this, cx| {
        let id = this.workspace.active_tab().unwrap().id;
        this.close_tab(id, cx);
    });
    assert!(app.read_with(visual, |this, _| this.history.view.is_none()));
}

#[gpui::test]
fn history_the_strip_has_show_all_which_opens_the_manager_on_the_tabs_connection(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    file(vec![
        seed(&connection, format!("SELECT '{tag}' FROM orders"), 5.0),
        seed("another-connection", format!("SELECT '{tag}' FROM other"), 6.0),
    ]);
    let tab = strip_tab(&app, visual, &connection, "orders");
    app.update(visual, |this, cx| this.toggle_history(tab, cx));
    settle(visual);
    click_debug_button(visual, "history-show-all");
    wait_rows(&app, visual, "the manager's rows", |_| true);
    assert_eq!(active_kind_of(&app, visual), Some(TabKind::History));
    let view = history_view(&app, visual);
    let scoped = view.read_with(visual, |view, _| view.query().scopes.iter().map(|scope| scope.conn_uuid.clone()).collect::<Vec<_>>());
    assert_eq!(scoped, [connection], "narrowed to the connection of the tab");
}

// ---- search and filters ------------------------------------------------------------------------------

#[gpui::test]
fn history_search_narrows_the_rows_and_the_kinds_and_sources_filter_them(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    file(vec![
        seed("c1", format!("SELECT '{tag}' FROM users"), 1.0),
        seed("c1", format!("UPDATE users SET a = '{tag}'"), 2.0),
        seed("c1", format!("DROP TABLE x_{tag}"), 3.0),
        Seed { error: Some("no such table: nope"), ..seed("c1", format!("SELECT '{tag}' FROM nope"), 4.0) },
    ]);
    open_on(&app, visual, &tag, 4);
    // Newest first.
    assert!(statements(&app, visual)[0].starts_with("SELECT") && statements(&app, visual)[0].ends_with("FROM users"));
    for (kind, expected) in [("data", 1), ("schema", 1), ("query", 2), ("failed", 1), ("all", 4)] {
        let selector: &'static str = Box::leak(format!("history-kind-{kind}").into_boxed_str());
        click_debug_button(visual, selector);
        wait_rows(&app, visual, "the kind", |found| found.len() == expected);
    }
    // The words of the search all have to be in the row.
    search_for(&app, visual, &format!("{tag} UPDATE"), 1);
    // The failed run's row says why.
    click_debug_button(visual, "history-kind-failed");
}

#[gpui::test]
fn history_a_failed_run_shows_its_error_in_the_row_and_the_preview(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    file(vec![Seed {
        error: Some("no such table: nope\nsecond line"),
        duration_ms: Some(1500),
        ..seed("c1", format!("SELECT '{tag}' FROM nope"), 1.0)
    }]);
    open_on(&app, visual, &tag, 1);
    let view = history_view(&app, visual);
    let row = view.read_with(visual, |view, cx| view.row_views(cx)).remove(0);
    assert!(row.failed);
    assert_eq!(row.result, "no such table: nope", "the short error");
    assert_eq!(row.duration, "1.5 s");
    click_row(visual, 0);
    assert!(visual.debug_bounds("history-preview-error").is_some(), "the error is in the preview");
    // The kind buttons of MCP calls are Failed and Succeeded only.
    click_debug_button(visual, "history-source-mcp");
    assert!(visual.debug_bounds("history-kind-succeeded").is_some() && visual.debug_bounds("history-kind-data").is_none());
}

#[gpui::test]
fn history_load_more_reads_the_next_page_and_the_count_says_there_is_more(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    file((0..230).map(|i| seed("c1", format!("SELECT '{tag}' AS n{i}"), 1.0 + f64::from(i) / 100.0)).collect());
    open_on(&app, visual, &tag, 200);
    assert!(visual.debug_bounds("history-load-more").is_some(), "more than a page: the button is there");
    click_debug_button(visual, "history-load-more");
    wait_rows(&app, visual, "the second page", |found| found.len() == 230);
    assert!(visual.debug_bounds("history-load-more").is_none(), "nothing left to load");
    let all = statements(&app, visual);
    let unique: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(unique.len(), 230, "no row twice");
}

#[gpui::test]
fn history_a_search_with_no_match_says_so_and_an_empty_history_says_it_is_empty(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    open_history(&app, visual);
    let tag = tag();
    search_for(&app, visual, &tag, 0);
    let view = history_view(&app, visual);
    let text = visual.debug_bounds("history-empty").map(|_| true);
    assert_eq!(text, Some(true), "the empty state is drawn");
    assert_eq!(view.read_with(visual, |view, _| view.query().text.clone()), tag);
}

#[gpui::test]
fn history_the_time_range_takes_quick_ranges_and_checked_dates(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    file(vec![
        seed("c1", format!("SELECT '{tag}' AS recent"), 30.0),
        seed("c1", format!("SELECT '{tag}' AS old"), 60.0 * 24.0 * 10.0),
    ]);
    open_on(&app, visual, &tag, 2);
    click_debug_button(visual, "history-range");
    assert!(visual.debug_bounds("history-range-panel").is_some());
    click_debug_button(visual, "history-range-7d");
    wait_rows(&app, visual, "the last 7 days", |found| found.len() == 1);
    assert!(statements(&app, visual)[0].ends_with("recent"));
    // Typed dates: a start after the end is refused, with the reason, and nothing changes.
    click_debug_button(visual, "history-range");
    type_in_box(visual, "history-range-start", "2030-02-02");
    type_in_box(visual, "history-range-end", "2030-01-01");
    click_debug_button(visual, "history-range-apply");
    assert!(visual.debug_bounds("history-range-error").is_some(), "start after end");
    assert_eq!(statements(&app, visual).len(), 1);
    // A date that is not one is refused too.
    type_in_box(visual, "history-range-start", "yesterday");
    type_in_box(visual, "history-range-end", "");
    click_debug_button(visual, "history-range-apply");
    assert!(visual.debug_bounds("history-range-error").is_some());
    // A valid span that holds both rows; the quick ranges are not on any more.
    type_in_box(visual, "history-range-start", "2000-01-01");
    type_in_box(visual, "history-range-end", "2100-01-01");
    click_debug_button(visual, "history-range-apply");
    wait_rows(&app, visual, "the dates", |found| found.len() == 2);
    // Clear drops the range.
    click_debug_button(visual, "history-range");
    click_debug_button(visual, "history-range-clear");
    wait_rows(&app, visual, "all time", |found| found.len() == 2);
}

#[gpui::test]
fn history_the_scope_picker_narrows_by_connection_and_database_and_resets(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    // Names of their own: the shared history holds the connections of the other tests too.
    let (shop_name, other_name) = (format!("Shop-{tag}"), format!("Analytics-{tag}"));
    let (a, b) = (add_connection(&app, visual, &shop_name), add_connection(&app, visual, &other_name));
    file(vec![
        seed(&a, format!("SELECT '{tag}' AS a1"), 1.0),
        Seed { database: "reports", ..seed(&a, format!("SELECT '{tag}' AS a2"), 2.0) },
        seed(&b, format!("SELECT '{tag}' AS b1"), 3.0),
    ]);
    open_on(&app, visual, &tag, 3);
    click_debug_button(visual, "history-scope");
    assert!(visual.debug_bounds("history-scope-panel").is_some());
    // The search box of the panel narrows the choices to these two connections.
    type_in_box(visual, "history-scope-search", &tag);
    // The options are named after the saved connections.
    let view = history_view(&app, visual);
    let names: Vec<String> = view.read_with(visual, |view, _| view.scope_names());
    assert!(names.contains(&shop_name) && names.contains(&other_name), "{names:?}");
    // The choices are listed in the order of the ids, and other tests add connections at the same time:
    // where Shop is in the list is asked again before each click.
    let position = |view: &gpui::Entity<history::HistoryView>, visual: &gpui::VisualTestContext| {
        view.read_with(visual, |view, _| view.scope_names().iter().position(|name| *name == shop_name).unwrap())
    };
    // Tick Shop: its two runs.
    let shop = position(&view, visual);
    let selector: &'static str = Box::leak(format!("history-scope-connection-{shop}").into_boxed_str());
    click_debug_button(visual, selector);
    wait_rows(&app, visual, "Shop", |found| found.len() == 2);
    // Narrow to one of its databases.
    let shop = position(&view, visual);
    let database: &'static str = Box::leak(format!("history-scope-database-{shop}-1").into_boxed_str());
    click_debug_button(visual, database);
    let scopes = view.read_with(visual, |view, _| view.query().scopes.clone());
    assert_eq!(scopes.len(), 1, "{scopes:?}");
    assert_eq!(scopes[0].databases, ["reports"], "{scopes:?}");
    wait_rows(&app, visual, "Shop / one database", |found| found.len() == 1);
    // Reset.
    click_debug_button(visual, "history-scope-reset");
    wait_rows(&app, visual, "everything again", |found| found.len() == 3);
    // Current: the connection of the tab the user came from.
    view.update(visual, |view, cx| {
        view.set_context(Some(b.clone()), view_names(), None);
        cx.notify();
    });
    settle(visual);
    click_debug_button(visual, "history-scope-current");
    wait_rows(&app, visual, "Current", |found| found.len() == 1);
}

// ---- the preview and the actions ---------------------------------------------------------------------

#[gpui::test]
fn history_selecting_a_row_shows_its_whole_statement_highlighted_with_its_fields(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    let long = format!("SELECT '{tag}',\n       name\n  FROM users\n WHERE id > 10");
    file(vec![seed(&connection, long.clone(), 1.0)]);
    open_on(&app, visual, &tag, 1);
    assert!(visual.debug_bounds("history-preview-sql").is_none(), "nothing selected: a hint");
    click_row(visual, 0);
    assert!(visual.debug_bounds("history-preview-sql").is_some());
    let view = history_view(&app, visual);
    assert_eq!(view.read_with(visual, |view, cx| view.preview_text(cx)), long, "the whole statement, not its first line");
    assert!(visual.debug_bounds("history-preview-fields").is_some());
    // The list shows its first line only.
    let line = view.read_with(visual, |view, cx| view.row_views(cx)).remove(0).line;
    assert_eq!(line, format!("SELECT '{tag}',"));
}

#[gpui::test]
fn history_copy_puts_the_statement_on_the_clipboard_and_insert_goes_into_the_editor_the_user_came_from(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    let sql = format!("SELECT '{tag}' AS inserted");
    file(vec![seed(&connection, sql.clone(), 1.0)]);
    // A query tab the user is in; the History tab is opened from it.
    let tab = app.update(visual, |this, cx| {
        let id = this.workspace.open_query_tab_with_metadata("Query", TabMetadata { connection_id: connection.clone(), ..Default::default() });
        this.workspace.tab_mut(id).unwrap().sql = "SELECT 1;\n".into();
        cx.notify();
        id
    });
    settle(visual);
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            let editor = this.editor_for_tab(tab, window, cx);
            editor.update(cx, |editor, cx| editor.set_cursor_position(gpui_component::input::Position::new(1, 0), window, cx));
        })
    });
    open_on(&app, visual, &tag, 1);
    click_row(visual, 0);
    click_debug_button(visual, "history-act-copy");
    assert_eq!(visual.cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(sql.as_str()));
    click_debug_button(visual, "history-act-insert");
    settle(visual);
    let text = app.read_with(visual, |this, _| this.workspace.tab(tab).unwrap().sql.clone());
    assert_eq!(text, format!("SELECT 1;\n{sql}"), "at the cursor of the tab the user came from");
    assert_eq!(active_kind_of(&app, visual), Some(TabKind::Query), "and that tab is shown");
}

#[gpui::test]
fn history_open_in_new_tab_makes_a_query_tab_on_the_runs_connection_and_database(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    let sql = format!("SELECT '{tag}' AS opened");
    file(vec![seed(&connection, sql.clone(), 1.0)]);
    open_on(&app, visual, &tag, 1);
    click_row(visual, 0);
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    click_debug_button(visual, "history-act-open");
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tabs().len() > before, "a tab was made");
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!((tab.kind, tab.sql.as_str(), tab.connection_id.as_str(), tab.database.as_str()), (TabKind::Query, sql.as_str(), connection.as_str(), "app"));
    });
    // A row whose connection is gone says so instead of opening a tab nobody can use.
    let gone = tag_for("gone");
    file(vec![seed("deleted-connection", format!("SELECT '{gone}'"), 1.0)]);
    open_on_again(&app, visual, &gone, 1);
    click_row(visual, 0);
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    click_debug_button(visual, "history-act-open");
    app.read_with(visual, |this, _| {
        let tabs: Vec<_> = this.workspace.tabs().iter().map(|tab| (tab.kind, tab.title.clone(), tab.sql.clone())).collect();
        assert_eq!(this.workspace.tabs().len(), before, "{tabs:?}");
        assert_eq!(this.sidebar.notice.as_deref(), Some("The connection of this entry no longer exists."));
    });
}

fn view_names() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::new()
}

fn tag_for(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().to_simple())
}

/// The tab is open again (shown from another tab): another search.
fn open_on_again(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tag: &str, rows: usize) {
    open_history(app, visual);
    search_for(app, visual, tag, rows);
}

#[gpui::test]
fn history_run_goes_at_once_for_a_read_and_asks_first_for_anything_else(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    file(vec![
        seed(&connection, format!("SELECT '{tag}' AS plain_read"), 1.0),
        seed(&connection, format!("DELETE FROM t WHERE tag = '{tag}'"), 2.0),
    ]);
    open_on(&app, visual, &tag, 2);
    let tabs = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| app.read_with(visual, |this, _| this.workspace.tabs().len());
    // The read: a new tab, and a run asked for in it (it fails at once: the connection is not open).
    click_row(visual, 0);
    let before = tabs(&app, visual);
    click_debug_button(visual, "history-act-run");
    settle(visual);
    assert_eq!(tabs(&app, visual), before + 1, "a tab for the run");
    assert!(visual.debug_bounds("history-run-body").is_none(), "a plain read does not ask");
    let ran = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.sql.clone(), !matches!(this.workspace.runtime(tab.id).unwrap().query.status(), crate::query::QueryStatus::Idle))
    });
    assert_eq!(ran, (format!("SELECT '{tag}' AS plain_read"), true), "the run started");
    // (The connection is not open: whatever the app asks about it is dismissed.)
    visual.simulate_keystrokes("escape");
    settle(visual);
    // Back to the History tab: the write asks, and Cancel runs nothing.
    open_history(&app, visual);
    wait_rows(&app, visual, "the rows again", |found| found.len() == 2);
    click_row(visual, 1);
    assert_eq!(selected_sql(&app, visual), Some(format!("DELETE FROM t WHERE tag = '{tag}'")));
    let before = tabs(&app, visual);
    click_debug_button(visual, "history-act-run");
    settle(visual);
    assert!(visual.debug_bounds("history-run-body").is_some(), "the question is asked");
    assert!(visual.debug_bounds("history-run-sql").is_some(), "with the statement in front of the user");
    assert_eq!(tabs(&app, visual), before, "nothing ran yet");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(tabs(&app, visual), before, "Cancel runs nothing");
    // Asked again, and answered yes.
    click_debug_button(visual, "history-act-run");
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(tabs(&app, visual), before + 1, "the confirmed statement runs in a tab");
}

#[gpui::test]
fn history_delete_asks_and_removes_the_row_from_the_list_and_the_store(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    file(vec![seed("c1", format!("SELECT '{tag}' AS keep"), 1.0), seed("c1", format!("SELECT '{tag}' AS drop_me"), 2.0)]);
    open_on(&app, visual, &tag, 2);
    click_row(visual, 1);
    click_debug_button(visual, "history-act-delete");
    settle(visual);
    assert!(visual.debug_bounds("history-delete-body").is_some(), "asked first");
    assert_eq!(stored(&tag), 2);
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(stored(&tag), 2, "Cancel keeps it");
    click_debug_button(visual, "history-act-delete");
    settle(visual);
    visual.simulate_keystrokes("enter");
    wait_rows(&app, visual, "the row is gone", |found| found.len() == 1);
    assert_eq!(stored(&tag), 1);
    assert!(statements(&app, visual)[0].ends_with("keep"));
    assert_eq!(selected_sql(&app, visual), None, "nothing is selected any more");
}

#[gpui::test]
fn history_the_context_menu_and_the_keys_offer_the_actions(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    let sql = format!("SELECT '{tag}' AS keyed");
    file(vec![seed(&connection, sql.clone(), 1.0)]);
    open_on(&app, visual, &tag, 1);
    // Right click opens the menu of the row: Open, Insert, Copy SQL, ... ; the keys pick the third.
    let row = visual.debug_bounds("history-row-0").expect("row");
    visual.simulate_event(gpui::MouseDownEvent {
        button: gpui::MouseButton::Right,
        position: row.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    settle(visual);
    for _ in 0..3 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(visual.cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(sql.as_str()), "Copy SQL from the menu");
    // The keys, with the list focused: Cmd+C copies, Enter opens in a new tab, Delete asks.
    click_row(visual, 0);
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new()));
    visual.simulate_keystrokes("secondary-c");
    assert_eq!(visual.cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(sql.as_str()));
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), before + 1, "Enter opens it in a new tab");
    open_history(&app, visual);
    click_row(visual, 0);
    visual.simulate_keystrokes("backspace");
    settle(visual);
    assert!(visual.debug_bounds("history-delete-body").is_some(), "Delete asks");
    visual.simulate_keystrokes("escape");
}

#[gpui::test]
fn history_view_details_lists_every_field_and_the_error(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    file(vec![Seed { error: Some("boom"), ..seed(&connection, format!("SELECT '{tag}' FROM nope"), 1.0) }]);
    open_on(&app, visual, &tag, 1);
    click_row(visual, 0);
    click_debug_button(visual, "history-act-details");
    settle(visual);
    assert!(visual.debug_bounds("history-details").is_some());
    assert!(visual.debug_bounds("history-details-request").is_some());
    assert!(visual.debug_bounds("history-details-error").is_some(), "a failed run's error");
    click_debug_button(visual, "history-details-close");
    settle(visual);
    assert!(visual.debug_bounds("history-details").is_none());
}

#[gpui::test]
fn history_analyze_with_ai_is_only_there_when_the_assistant_is_set_up(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let tag = tag();
    file(vec![seed(&connection, format!("SELECT '{tag}' AS ask"), 1.0)]);
    open_on(&app, visual, &tag, 1);
    click_row(visual, 0);
    assert!(visual.debug_bounds("history-act-analyze").is_none(), "no assistant set up: hidden");
    app.update(visual, |this, cx| {
        let mut profile = crate::settings::connection::ConnectionProfile::default();
        profile.overrides.set(engine_connect::profile::AGENT_ACCESS, crate::settings::connection::OverrideValue::Text("read".into()));
        this.connection_policy.set_profile(&connection, profile);
        this.assistant.set_key_state(crate::settings::AiProvider::Anthropic, assistant::KeyState::Set);
        this.assistant_sync(cx);
        cx.notify();
    });
    settle(visual);
    open_history(&app, visual);
    click_row(visual, 0);
    assert!(visual.debug_bounds("history-act-analyze").is_some(), "set up: offered");
    click_debug_button(visual, "history-act-analyze");
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.assistant.dock.open), "the assistant opens with the entry");
}

// ---- Clear ---------------------------------------------------------------------------------------------

#[gpui::test]
fn history_clear_states_how_many_entries_the_filter_shows_and_deletes_only_those(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    file(vec![
        seed("c1", format!("SELECT '{tag}' AS q1"), 1.0),
        seed("c1", format!("SELECT '{tag}' AS q2"), 2.0),
        seed("c1", format!("UPDATE t SET a = '{tag}'"), 3.0),
    ]);
    open_on(&app, visual, &tag, 3);
    // Filter to the queries: Clear counts those two only.
    click_debug_button(visual, "history-kind-query");
    wait_rows(&app, visual, "the queries", |found| found.len() == 2);
    click_debug_button(visual, "history-clear");
    wait_for(visual, "the confirmation", |visual| visual.debug_bounds("history-clear-body").is_some());
    assert!(dialog_open(visual));
    let asked = history_view(&app, visual).read_with(visual, |view, _| view.clear_asked());
    assert_eq!(asked, Some(2), "the question states the count of the filter, not of the whole history");
    // Cancel: nothing goes.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(stored(&tag), 3);
    // Yes: those two go, the update stays.
    click_debug_button(visual, "history-clear");
    wait_for(visual, "the confirmation", |visual| visual.debug_bounds("history-clear-body").is_some());
    visual.simulate_keystrokes("enter");
    wait_for(visual, "the clear", |_| stored(&tag) == 1);
    wait_rows(&app, visual, "the list follows", |found| found.is_empty());
}

#[gpui::test]
fn history_clear_is_not_offered_for_mcp_calls_which_are_read_only(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    open_history(&app, visual);
    click_debug_button(visual, "history-source-mcp");
    settle(visual);
    click_debug_button(visual, "history-clear");
    settle(visual);
    assert!(!dialog_open(visual), "a disabled Clear opens nothing");
}

// ---- MCP calls -----------------------------------------------------------------------------------------

fn audit_line(time: &str, front_end: &str, tool: &str, connection: &str, statement: &str, outcome: &str) -> String {
    serde_json::json!({
        "time": time,
        "front_end": front_end,
        "connection_id": connection,
        "tool": tool,
        "statement": statement,
        "outcome": outcome,
        "rows": 2,
        "duration_ms": 7,
    })
    .to_string()
}

#[gpui::test]
fn history_mcp_calls_come_from_the_audit_log_filter_by_tool_and_are_never_written(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let dir = std::env::temp_dir().join(format!("tablex-history-audit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let audit = dir.join("audit.jsonl");
    let now = chrono::Utc::now();
    let stamp = |minutes: i64| (now - chrono::Duration::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let tag = tag();
    let content = [
        audit_line(&stamp(3), "mcp", "execute_query", "c1", &format!("SELECT '{tag}' FROM a"), "ok"),
        audit_line(&stamp(2), "mcp", "list_tables", "c1", &format!("-- {tag} tables"), "ok"),
        audit_line(&stamp(1), "mcp", "execute_query", "c1", &format!("DELETE FROM b -- {tag}"), "agent.write_refused"),
        // The app's own assistant writes to the same file: not an MCP call.
        audit_line(&stamp(1), "assistant", "execute_query", "c1", &format!("SELECT '{tag}' FROM assistant"), "ok"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(&audit, &content).unwrap();
    app.update(visual, |this, _| this.history.audit_path = Some(audit.clone()));
    file(vec![seed("c1", format!("SELECT '{tag}' FROM sql_side"), 1.0)]);
    open_history(&app, visual);
    // Only MCP calls.
    click_debug_button(visual, "history-source-mcp");
    search_for(&app, visual, &tag, 3);
    let tools = history_view(&app, visual).read_with(visual, |view, cx| view.row_views(cx).iter().map(|row| row.line.to_string()).collect::<Vec<_>>());
    assert!(tools.iter().any(|line| line.starts_with("list_tables")) && tools.iter().filter(|line| line.starts_with("execute_query")).count() == 2, "{tools:?}");
    // The failed one says how it ended; Succeeded and Failed filter.
    click_debug_button(visual, "history-kind-failed");
    wait_rows(&app, visual, "the failed call", |found| found.len() == 1);
    let result = history_view(&app, visual).read_with(visual, |view, cx| view.row_views(cx)[0].result.to_string());
    assert_eq!(result, "agent.write_refused");
    click_debug_button(visual, "history-kind-all");
    wait_rows(&app, visual, "all three", |found| found.len() == 3);
    // The tool select offers the tools of the log.
    click_debug_button(visual, "history-tool");
    visual.simulate_keystrokes("down enter");
    wait_rows(&app, visual, "one tool", |found| found.len() == 2);
    let tool = history_view(&app, visual).read_with(visual, |view, _| view.query().tool.clone());
    assert_eq!(tool.as_deref(), Some("execute_query"), "the first tool of the log, in order");
    // The detail of a call: its request and the summary of its response.
    click_row(visual, 0);
    click_debug_button(visual, "history-act-details");
    settle(visual);
    assert!(visual.debug_bounds("history-details-request").is_some());
    assert!(visual.debug_bounds("history-details-legacy").is_some(), "the response is a summary only");
    click_debug_button(visual, "history-details-close");
    // All: both sources, merged.
    click_debug_button(visual, "history-source-all");
    wait_rows(&app, visual, "all sources", |found| !found.is_empty());
    assert_eq!(std::fs::read_to_string(&audit).unwrap(), content, "the audit log was not touched");
    std::fs::remove_dir_all(dir).unwrap();
}

// ---- recording -----------------------------------------------------------------------------------------

#[gpui::test]
fn history_a_run_is_recorded_unless_settings_say_not_and_a_failure_keeps_its_error_clipped(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let run = |sql: &str, error: Option<String>| history::Recorded {
        connection: "c1".into(),
        database: "app".into(),
        collection: String::new(),
        statement: sql.into(),
        limit: 200,
        rows: 0,
        duration_ms: Some(9),
        error,
        kind: store_history::classify_sql(sql),
    };
    let long = "x".repeat(900);
    app.update(visual, |this, cx| {
        this.record_history(run("SELECT 1", None), cx);
        this.record_history(run("SELECT * FROM nope", Some(crate::app::history::clip_error(&long))), cx);
        assert_eq!(this.recorded_history.len(), 2);
        let failed = &this.recorded_history[1];
        assert_eq!(failed.error.as_deref().map(|e| e.chars().count()), Some(500), "the error is clipped to 500 characters");
        assert_eq!(failed.duration_ms, Some(9));
        // Off: nothing new.
        let mut next = (**this.settings()).clone();
        next.query.history_record = false;
        this.apply_settings(next, None, cx);
        this.record_history(run("SELECT 2", None), cx);
        assert_eq!(this.recorded_history.len(), 2, "recording is off");
    });
}

#[gpui::test]
fn history_the_real_writer_files_a_run_with_its_error_and_duration(cx: &mut TestAppContext) {
    let (_app, _visual) = history_app(cx);
    let tag = tag();
    let run = history::Recorded {
        connection: "c1".into(),
        database: "app".into(),
        collection: String::new(),
        statement: format!("UPDATE t SET a = '{tag}'"),
        limit: 200,
        rows: 0,
        duration_ms: Some(31),
        error: Some("deadlock detected".into()),
        kind: store_history::classify_sql("UPDATE t"),
    };
    crate::app::history::write_history(&run).unwrap();
    let found = store_history::search(&store_history::HistoryFilter { text: tag, ..Default::default() }, None, 10).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].failed());
    assert_eq!((found[0].error.as_deref(), found[0].duration_ms, found[0].kind.as_deref()), (Some("deadlock detected"), Some(31), Some("data_change")));
}

#[gpui::test]
fn history_the_retention_deletes_runs_older_than_the_days_and_keeps_the_rest(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let tag = tag();
    file(vec![
        seed("c1", format!("SELECT '{tag}' AS fresh"), 60.0),
        seed("c1", format!("SELECT '{tag}' AS month_old"), 60.0 * 24.0 * 40.0),
        seed("c1", format!("SELECT '{tag}' AS year_old"), 60.0 * 24.0 * 400.0),
    ]);
    assert_eq!(stored(&tag), 3);
    // 0 = for ever.
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        next.query.history_keep_days = 0;
        this.apply_settings(next, None, cx);
        this.prune_history_in_background(cx);
    });
    settle(visual);
    std::thread::sleep(std::time::Duration::from_millis(200));
    visual.run_until_parked();
    assert_eq!(stored(&tag), 3, "0 keeps everything");
    // 90 days: the year-old one goes.
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        next.query.history_keep_days = 90;
        this.apply_settings(next, None, cx);
        this.prune_history_in_background(cx);
    });
    wait_for(visual, "the prune", |_| stored(&tag) == 2);
    // 30 days: the month-old one goes too.
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        next.query.history_keep_days = 30;
        this.apply_settings(next, None, cx);
        this.prune_history_in_background(cx);
    });
    wait_for(visual, "the prune", |_| stored(&tag) == 1);
}

#[gpui::test]
fn history_the_history_settings_are_on_the_settings_page_with_their_defaults(cx: &mut TestAppContext) {
    let defaults = Settings::default();
    assert!(defaults.query.history_record);
    assert_eq!(defaults.query.history_keep_days, 90);
    for id in ["query.history_record", "query.history_keep_days"] {
        let descriptor = crate::settings::registry::find(id).unwrap_or_else(|| panic!("{id}"));
        assert_eq!(descriptor.section, crate::settings::registry::Section::Query);
    }
    let _ = cx;
}

// ---- the strip under a table's results ------------------------------------------------------------------

/// A table tab on `connection` with a strip over the runs of `collection`.
fn strip_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str, collection: &str) -> u64 {
    let (connection, collection) = (connection.to_owned(), collection.to_owned());
    let tab = app.update(visual, |this, cx| {
        let id = this.workspace.open_query_tab_with_metadata(
            "Query",
            TabMetadata { connection_id: connection, database: "app".into(), collection, ..Default::default() },
        );
        cx.notify();
        id
    });
    settle(visual);
    tab
}

fn file_for_table(connection: &str, collection: &str, sql: &str, minutes_ago: f64) {
    let conn = engine_storage::app_db::open(&engine_storage::app_db::default_path()).unwrap();
    store_history::record_in(
        &conn,
        &Record {
            conn_uuid: connection,
            database: "app",
            collection,
            filter: sql,
            project: "",
            sort: "",
            skip: 0,
            limit: 200,
            max_time_ms: 0,
            result_count: 1,
            error: None,
            duration_ms: Some(3),
            kind: store_history::classify_sql(sql),
        },
        now_secs() - minutes_ago * 60.0,
    )
    .unwrap();
}

#[gpui::test]
fn history_a_click_on_a_strip_entry_inserts_it_and_a_double_click_or_enter_runs_it(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let collection = format!("t_{}", uuid::Uuid::new_v4().to_simple());
    let (first, second) = (format!("SELECT 'one' FROM {collection}"), format!("SELECT 'two' FROM {collection}"));
    file_for_table(&connection, &collection, &first, 2.0);
    file_for_table(&connection, &collection, &second, 1.0);
    let tab = strip_tab(&app, visual, &connection, &collection);
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            let editor = this.editor_for_tab(tab, window, cx);
            editor.update(cx, |editor, cx| editor.set_value("-- mine\n", window, cx));
        })
    });
    app.update(visual, |this, cx| this.toggle_history(tab, cx));
    settle(visual);
    let ids: Vec<i64> = app.read_with(visual, |this, _| this.view(tab).history.entries.iter().map(|entry| entry.id).collect());
    assert_eq!(ids.len(), 2);
    let status = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| !matches!(this.workspace.runtime(tab).unwrap().query.status(), crate::query::QueryStatus::Idle))
    };
    // One click: the statement is in the editor, below what was there, and nothing ran.
    let selector: &'static str = Box::leak(format!("history-entry-{}", ids[1]).into_boxed_str());
    click_debug_button(visual, selector);
    settle(visual);
    let text = app.read_with(visual, |this, _| this.workspace.tab(tab).unwrap().sql.clone());
    assert!(text.contains("-- mine") && text.contains(&first), "inserted into the editor, what was there is kept: {text:?}");
    assert!(!status(&app, visual), "a click does not run it");
    // Double click: it runs.
    let bounds = visual.debug_bounds(Box::leak(format!("history-entry-{}", ids[0]).into_boxed_str())).unwrap();
    for click_count in [1, 2] {
        visual.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: bounds.center(),
            modifiers: Default::default(),
            click_count,
            first_mouse: false,
        });
        visual.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: bounds.center(),
            modifiers: Default::default(),
            click_count,
        });
    }
    settle(visual);
    assert!(status(&app, visual), "a double click runs it");
}

#[gpui::test]
fn history_enter_on_the_selected_strip_entry_runs_it_and_the_arrows_move_the_selection(cx: &mut TestAppContext) {
    let (app, visual) = history_app(cx);
    let connection = add_connection(&app, visual, "Shop");
    let collection = format!("t_{}", uuid::Uuid::new_v4().to_simple());
    file_for_table(&connection, &collection, &format!("SELECT 'one' FROM {collection}"), 2.0);
    file_for_table(&connection, &collection, &format!("SELECT 'two' FROM {collection}"), 1.0);
    let tab = strip_tab(&app, visual, &connection, &collection);
    app.update(visual, |this, cx| this.toggle_history(tab, cx));
    settle(visual);
    let ids: Vec<i64> = app.read_with(visual, |this, _| this.view(tab).history.entries.iter().map(|entry| entry.id).collect());
    // A click selects (and inserts); the strip has the keyboard now.
    click_debug_button(visual, Box::leak(format!("history-entry-{}", ids[0]).into_boxed_str()));
    visual.simulate_keystrokes("down");
    assert_eq!(app.read_with(visual, |this, _| this.view(tab).history.selected), Some(ids[1]));
    visual.simulate_keystrokes("up");
    assert_eq!(app.read_with(visual, |this, _| this.view(tab).history.selected), Some(ids[0]));
    let idle = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| matches!(this.workspace.runtime(tab).unwrap().query.status(), crate::query::QueryStatus::Idle))
    };
    assert!(idle(&app, visual));
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(!idle(&app, visual), "Enter runs the selected entry");
}

#[test]
fn history_a_mongo_find_is_filed_as_the_shell_statement_it_stands_for() {
    use crate::app::results::mongo::query::mongo_find_statement as statement;
    assert_eq!(statement("users", r#"{"age": {"$gt": 21}}"#, r#"{"_id": 1}"#, ""), r#"db.users.find({"age": {"$gt": 21}}).sort({"_id": 1})"#);
    assert_eq!(statement("users", "", "", r#"{"name": 1}"#), r#"db.users.find({}, {"name": 1})"#);
    assert_eq!(statement("order items", "{}", "{}", ""), r#"db.getCollection("order items").find({})"#, "a name that is no identifier");
    // A find reads: it is a query, and running it again does not ask.
    for text in [statement("users", "{}", "", ""), statement("1st", "{}", "", "")] {
        assert_eq!(store_history::classify_sql(&text), "query");
        assert!(!crate::app::history::needs_confirmation_for_test(&text));
    }
}
