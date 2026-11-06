// Process List: the real tab, opened from the real connection menu, against a scripted server (a fake session that answers the list query
// from a state the tests change and logs every statement it is given), and, at the end, `#[ignore]`d live tests against the dedicated
// `tablex-test-*` containers (a long query on a second connection, found in the list, cancelled or killed from the dialog).

use crate::app::command_palette::StaticAction;
use crate::app::process_list::{InjectedSession, ProcessListView, ProcessSession, Snapshot};
use crate::settings::connection::PRODUCTION;
use crate::sql_account::catalog::{ReadError, TextRows};

#[derive(Default)]
struct PlServer {
    rows: TextRows,
    list_error: Option<String>,
    run_error: Option<String>,
    /// Reads of the list.
    lists: usize,
    /// Sessions the tab opened and sessions it let go of.
    opened: usize,
    closed: usize,
    /// Statements run as actions, in order.
    ran: Vec<String>,
    /// The list query that was sent.
    list_sql: Vec<String>,
}

type PlState = std::sync::Arc<std::sync::Mutex<PlServer>>;

struct PlSession(PlState);

impl ProcessSession for PlSession {
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError> {
        let mut state = self.0.lock().unwrap();
        state.lists += 1;
        state.list_sql.push(sql.to_owned());
        match &state.list_error {
            Some(message) => Err(ReadError::from_server(message.clone())),
            None => Ok(state.rows.clone()),
        }
    }

    fn run(&mut self, sql: &str) -> Result<(), ReadError> {
        let mut state = self.0.lock().unwrap();
        if let Some(message) = &state.run_error {
            return Err(ReadError::from_server(message.clone()));
        }
        state.ran.push(sql.to_owned());
        Ok(())
    }
}

impl Drop for PlSession {
    fn drop(&mut self) {
        self.0.lock().unwrap().closed += 1;
    }
}

fn pl_row(cells: &[&str]) -> Vec<Option<String>> {
    cells.iter().map(|cell| Some((*cell).to_owned())).collect()
}

/// A `pg_stat_activity` row, in the order the list query selects.
fn pl_pg(pid: &str, user: &str, database: &str, state: &str, seconds: &str, own: bool, query: &str) -> Vec<Option<String>> {
    pl_row(&[pid, user, database, "psql", "10.0.0.5", state, "", "", "2026-11-06 09:00:00", "", "2026-11-06 09:01:00", seconds, if own { "true" } else { "false" }, query])
}

fn pl_pg_server() -> PlState {
    std::sync::Arc::new(std::sync::Mutex::new(PlServer {
        rows: vec![
            pl_pg("101", "app", "shop", "active", "12.5", false, "SELECT pg_sleep(30)"),
            pl_pg("102", "reporting", "shop", "idle", "300", false, "COMMIT"),
            pl_pg("103", "tablex", "warehouse", "active", "0.001", true, "SELECT pid FROM pg_stat_activity"),
            pl_pg("104", "etl", "warehouse", "active", "3.0", false, "UPDATE orders SET paid = true"),
        ],
        ..PlServer::default()
    }))
}

struct PlRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    server: PlState,
}

fn pl_wire(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, server: &PlState) {
    let server = server.clone();
    app.update(visual, |this, _| {
        this.process_list_injected = Some(std::sync::Arc::new(move || -> Box<dyn ProcessSession> {
            server.lock().unwrap().opened += 1;
            Box::new(PlSession(server.clone()))
        }) as std::sync::Arc<InjectedSession>);
    });
}

fn pl_pg_rig(cx: &mut TestAppContext) -> PlRig<'_> {
    let (app, visual, _, _) = oa_pg(cx);
    let server = pl_pg_server();
    pl_wire(&app, visual, &server);
    PlRig { app, visual, server }
}

/// A MySQL / MariaDB / ClickHouse fixture connection on top of the PostgreSQL fixture app.
fn pl_other_rig(cx: &mut TestAppContext, kind: ConnectionKind, server: PlState) -> (PlRig<'_>, String) {
    let (app, visual, _, _) = oa_pg(cx);
    pl_wire(&app, visual, &server);
    let id = uuid::Uuid::new_v4();
    app.update(visual, |this, cx| {
        this.saved_connections.push(SavedConnection {
            id,
            name: "Other fixture".into(),
            uri: "mysql://root@127.0.0.1:3306/shop".into(),
            kind,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        });
        cx.notify();
    });
    settle(visual);
    (PlRig { app, visual, server }, id.to_string())
}

impl PlRig<'_> {
    fn view(&mut self) -> gpui::Entity<ProcessListView> {
        self.app.read_with(self.visual, |this, _| this.process_lists.values().next().cloned().expect("a Process List tab is open"))
    }

    fn snapshot(&mut self) -> Snapshot {
        let view = self.view();
        view.read_with(self.visual, |view, cx| view.snapshot(cx))
    }

    fn wait_rows(&mut self) {
        let view = self.view();
        wait_until(self.visual, "the sessions to be listed", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).total > 0 && !view.snapshot(cx).loading));
        settle(self.visual);
    }

    fn wait_idle(&mut self) {
        let view = self.view();
        wait_until(self.visual, "the read to end", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
        settle(self.visual);
    }

    /// Opens the tab from the connection's menu (the PostgreSQL fixture).
    fn open_pg(&mut self) {
        oa_menu_on(&self.app, self.visual, &NavigatorNodeId::Connection { connection: OA_PG.into() }, "connection-menu-process-list");
        self.wait_rows();
    }

    fn open(&mut self, connection: &str) {
        let connection = connection.to_owned();
        self.visual.update(|window, cx| self.app.update(cx, |this, cx| this.open_process_list(&connection, window, cx)));
        settle(self.visual);
        self.wait_rows();
    }

    fn click(&mut self, selector: &'static str) {
        oa_click(self.visual, selector);
    }

    /// Clicks the cell of the shown row `index`.
    fn select(&mut self, index: usize) {
        let selector: &'static str = Box::leak(format!("process-cell-{index}-1").into_boxed_str());
        self.click(selector);
    }

    /// Selects the row of the session `key` among the shown ones.
    fn select_key(&mut self, key: &str) {
        let at = self.snapshot().keys.iter().position(|shown| shown == key).unwrap_or_else(|| panic!("{key} is shown"));
        self.select(at);
    }

    fn ran(&self) -> Vec<String> {
        self.server.lock().unwrap().ran.clone()
    }

    fn lists(&self) -> usize {
        self.server.lock().unwrap().lists
    }

    fn dialog_open(&mut self) -> bool {
        self.visual.debug_bounds("process-confirm-sql").is_some()
    }

    /// The statement the open dialog shows (its Copy SQL button).
    fn dialog_sql(&mut self) -> String {
        self.click("process-confirm-copy");
        self.visual.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default()
    }

    fn wait_ran(&mut self, count: usize) {
        let server = self.server.clone();
        wait_until(self.visual, "the action to run", |_| server.lock().unwrap().ran.len() >= count);
        self.wait_idle();
    }
}

// ---- offering it -----------------------------------------------------------------------------------------------------------

#[gpui::test]
fn the_connection_menu_opens_one_process_list_tab_and_servers_without_sessions_have_no_entry(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    let (kind, title, connection) = rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.kind, tab.title.clone(), tab.connection_id.clone())
    });
    assert_eq!((kind, title.as_str(), connection.as_str()), (TabKind::ProcessList, "Process List", OA_PG));
    for selector in [
        "process-list-view",
        "process-list-title",
        "process-list-refresh",
        "process-list-interval",
        "process-list-hide-idle",
        "process-list-filter",
        "process-list-count",
        "process-list-cancel",
        "process-list-kill",
        "process-list-table",
    ] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // The list is read with the PostgreSQL query: pg_stat_activity, the app's own backend marked by pg_backend_pid().
    let sql = rig.server.lock().unwrap().list_sql[0].clone();
    assert!(sql.contains("pg_stat_activity") && sql.contains("pg_backend_pid()"), "{sql}");
    // Asking again activates the same tab.
    oa_menu_on(&rig.app, rig.visual, &NavigatorNodeId::Connection { connection: OA_PG.into() }, "connection-menu-process-list");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::ProcessList).count()), 1);

    // SQLite and MongoDB (and Redis) have no sessions to list: no entry, and the palette's way in says why.
    for connection in [crate::ui_fixture::SQLITE_ID, crate::ui_fixture::MONGO_ID] {
        assert!(!rig.app.read_with(rig.visual, |this, _| this.process_list_supported(connection)), "{connection}");
        rig.app.update(rig.visual, |this, cx| {
            this.sidebar.notice = None;
            cx.notify();
        });
        rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_process_list(connection, window, cx)));
        settle(rig.visual);
        let notice = rig.app.read_with(rig.visual, |this, _| this.sidebar.notice.clone());
        assert!(notice.as_deref().is_some_and(|text| text.contains("covers PostgreSQL, MySQL, MariaDB and ClickHouse")), "{connection}: {notice:?}");
    }
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::ProcessList).count()), 1);
    assert!(rig.app.read_with(rig.visual, |this, _| this.process_list_supported(OA_PG)));
}

#[gpui::test]
fn the_palette_command_opens_the_tab_of_the_connection_selected_in_the_navigator(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.app.update(rig.visual, |this, _| this.navigator.select(NavigatorNodeId::Connection { connection: OA_PG.into() }));
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| StaticAction::ProcessList.run(this, window, cx)));
    settle(rig.visual);
    rig.wait_rows();
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().kind), TabKind::ProcessList);
}

// ---- the list --------------------------------------------------------------------------------------------------------------

#[gpui::test]
fn the_sessions_are_listed_longest_first_with_the_idle_ones_hidden_and_the_apps_own_marked(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    let snapshot = rig.snapshot();
    // Hide idle is on by default: the idle COMMIT of 102 is not shown. Running sessions, the longest at it first.
    assert!(snapshot.hide_idle);
    assert_eq!((snapshot.keys.as_slice(), snapshot.total), (["101".to_owned(), "104".to_owned(), "103".to_owned()].as_slice(), 4));
    assert_eq!(snapshot.cells[0][..3], ["101".to_owned(), "app".to_owned(), "shop".to_owned()]);
    assert_eq!(snapshot.cells[0][4], "12.50 s");
    assert_eq!(snapshot.cells[0][5], "SELECT pg_sleep(30)");
    assert_eq!(snapshot.cells[2][0], "103 (this app)", "the app's own backend is marked");
    // The table draws them: a cell of each shown row, and none for the row that is not shown.
    for row in 0..3 {
        let selector: &'static str = Box::leak(format!("process-cell-{row}-1").into_boxed_str());
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("process-cell-3-0").is_none());
    assert!(rig.visual.debug_bounds("process-list-error").is_none());
}

#[gpui::test]
fn only_the_rows_in_view_are_drawn_and_the_session_is_kept_between_reads(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.server.lock().unwrap().rows = (0..5000).map(|n| pl_pg(&(1000 + n).to_string(), "load", "shop", "active", "1.0", false, "SELECT 1")).collect();
    rig.open_pg();
    assert_eq!(rig.snapshot().keys.len(), 5000);
    assert!(rig.visual.debug_bounds("process-cell-0-1").is_some());
    assert!(rig.visual.debug_bounds("process-cell-4000-1").is_none(), "a row far out of view is not built");
    // Refresh twice more: still the one session (a refresh is a query, not a login).
    for _ in 0..2 {
        rig.click("process-list-refresh");
        rig.wait_idle();
    }
    let state = rig.server.lock().unwrap();
    assert_eq!((state.lists, state.opened), (3, 1));
}

#[gpui::test]
fn hide_idle_and_the_filter_narrow_the_list_by_user_database_and_query(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    // Hide idle off: the idle session joins the list (after the running ones).
    rig.click("process-list-hide-idle");
    assert_eq!(rig.snapshot().keys, ["101", "104", "103", "102"]);
    assert!(!rig.snapshot().hide_idle);
    // The filter, typed: query, user, database; any case.
    oa_type(rig.visual, "process-list-filter", "PG_SLEEP");
    assert_eq!(rig.snapshot().keys, ["101"]);
    oa_type(rig.visual, "process-list-filter", "etl");
    assert_eq!(rig.snapshot().keys, ["104"]);
    oa_type(rig.visual, "process-list-filter", "warehouse");
    assert_eq!(rig.snapshot().keys, ["104", "103"]);
    oa_type(rig.visual, "process-list-filter", "nobody");
    assert!(rig.snapshot().keys.is_empty());
    assert!(rig.visual.debug_bounds("process-cell-0-0").is_none());
    // Hide idle on again: the idle one goes; the filter on "reporting" (its user) then shows nothing.
    oa_type(rig.visual, "process-list-filter", "reporting");
    assert_eq!(rig.snapshot().keys, ["102"]);
    rig.click("process-list-hide-idle");
    assert!(rig.snapshot().keys.is_empty());
    // The count says how many of the sessions are shown.
    assert_eq!(rig.snapshot().total, 4);
    // Nothing of this asked the server again: filtering is local.
    assert_eq!(rig.lists(), 1);
}

#[gpui::test]
fn the_selection_follows_its_session_across_a_refresh_and_goes_when_the_session_does(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.select_key("104");
    assert_eq!(rig.snapshot().selected.as_deref(), Some("104"));
    // A longer query appears above it: its position changes, the selection stays on the session.
    rig.server.lock().unwrap().rows.push(pl_pg("105", "app", "shop", "active", "90", false, "SELECT slow()"));
    rig.click("process-list-refresh");
    rig.wait_idle();
    assert_eq!(rig.snapshot().keys, ["105", "101", "104", "103"]);
    assert_eq!(rig.snapshot().selected.as_deref(), Some("104"));
    // The session ends.
    rig.server.lock().unwrap().rows.retain(|row| row[0].as_deref() != Some("104"));
    rig.click("process-list-refresh");
    rig.wait_idle();
    assert_eq!(rig.snapshot().selected, None);
}

// ---- errors ----------------------------------------------------------------------------------------------------------------

#[gpui::test]
fn a_failed_read_shows_inline_keeps_the_rows_and_the_next_read_connects_again(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.server.lock().unwrap().list_error = Some("permission denied for view pg_stat_activity".into());
    rig.click("process-list-refresh");
    rig.wait_idle();
    let snapshot = rig.snapshot();
    assert!(snapshot.error.as_deref().is_some_and(|error| error.contains("permission denied")), "{snapshot:?}");
    assert!(rig.visual.debug_bounds("process-list-error").is_some(), "the error is in the tab");
    assert_eq!(snapshot.total, 4, "the rows read before stay");
    // The session that failed is let go; the next read opens another.
    rig.server.lock().unwrap().list_error = None;
    rig.click("process-list-refresh");
    rig.wait_idle();
    assert!(rig.snapshot().error.is_none());
    assert!(rig.visual.debug_bounds("process-list-error").is_none());
    let state = rig.server.lock().unwrap();
    assert_eq!(state.opened, 2);
    assert_eq!(state.closed, 1);
}

#[gpui::test]
fn a_first_read_that_fails_says_so_in_the_tab(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.server.lock().unwrap().list_error = Some("connection refused".into());
    oa_menu_on(&rig.app, rig.visual, &NavigatorNodeId::Connection { connection: OA_PG.into() }, "connection-menu-process-list");
    let view = rig.view();
    wait_until(rig.visual, "the failure", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).error.is_some()));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("process-list-error").is_some());
    assert_eq!(rig.snapshot().total, 0);
}

// ---- cancel and kill -------------------------------------------------------------------------------------------------------

#[gpui::test]
fn cancel_and_kill_go_through_the_confirmation_with_the_exact_sql_shown_and_sent(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.select_key("101");

    // Cancel Query: the dialog shows the statement; Cancel in it sends nothing.
    rig.click("process-list-cancel");
    assert!(rig.dialog_open(), "the button asks first");
    assert!(rig.visual.debug_bounds("process-confirm-summary").is_some() && rig.visual.debug_bounds("process-confirm-query").is_some());
    assert_eq!(rig.dialog_sql(), "SELECT pg_cancel_backend(101)");
    assert!(rig.ran().is_empty(), "nothing is sent before the answer");
    assert!(rig.visual.debug_bounds("process-confirm-ack").is_none(), "an ordinary connection asks once");
    rig.click("process-confirm-cancel");
    assert!(!rig.dialog_open());
    assert!(rig.ran().is_empty());

    // Again, and Run: the statement that was shown is the one that is sent, and the list is read again.
    let lists = rig.lists();
    rig.click("process-list-cancel");
    rig.click("process-confirm-run");
    rig.wait_ran(1);
    assert_eq!(rig.ran(), ["SELECT pg_cancel_backend(101)"]);
    assert!(!rig.dialog_open());
    assert!(rig.lists() > lists, "the list is read again after the action");
    assert!(rig.snapshot().notice.is_some_and(|(text, failed)| !failed && text.contains("Cancel Query")));

    // Kill Session.
    rig.select_key("104");
    rig.click("process-list-kill");
    assert_eq!(rig.dialog_sql(), "SELECT pg_terminate_backend(104)");
    // Escape closes without sending.
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(!rig.dialog_open());
    assert_eq!(rig.ran().len(), 1);
    rig.click("process-list-kill");
    rig.click("process-confirm-run");
    rig.wait_ran(2);
    assert_eq!(rig.ran(), ["SELECT pg_cancel_backend(101)", "SELECT pg_terminate_backend(104)"]);
}

#[gpui::test]
fn a_server_that_refuses_the_action_is_shown_in_the_tab(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.server.lock().unwrap().run_error = Some("permission denied to terminate process".into());
    rig.select_key("101");
    rig.click("process-list-kill");
    rig.click("process-confirm-run");
    let view = rig.view();
    wait_until(rig.visual, "the refusal", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).notice.is_some()));
    settle(rig.visual);
    let (text, failed) = rig.snapshot().notice.unwrap();
    assert!(failed && text.contains("permission denied"), "{text}");
    assert!(rig.visual.debug_bounds("process-list-error").is_some());
    assert!(rig.ran().is_empty());
}

#[gpui::test]
fn the_apps_own_session_cannot_be_cancelled_or_killed(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.select_key("103");
    assert_eq!(rig.snapshot().selected.as_deref(), Some("103"));
    assert!(!rig.snapshot().cancel_enabled && !rig.snapshot().kill_enabled, "both buttons are disabled for it");
    for button in ["process-list-cancel", "process-list-kill"] {
        rig.click(button);
        assert!(!rig.dialog_open(), "{button}: no dialog for the app's own session");
    }
    assert!(rig.ran().is_empty());
    // Any other session can be.
    rig.select_key("101");
    assert!(rig.snapshot().cancel_enabled && rig.snapshot().kill_enabled);
    rig.click("process-list-cancel");
    assert!(rig.dialog_open());
    rig.click("process-confirm-cancel");
}

#[gpui::test]
fn a_read_only_connection_cannot_act_even_with_the_dialog_open(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.select_key("101");
    // Read-only from the start: the buttons do nothing.
    publish_types(&rig.app, rig.visual, true);
    give_profile(&rig.app, rig.visual, OA_PG, typed(PRODUCTION));
    for button in ["process-list-cancel", "process-list-kill"] {
        rig.click(button);
        assert!(!rig.dialog_open(), "{button}: a read-only connection does not ask");
    }
    let snapshot = rig.snapshot();
    assert!(!snapshot.cancel_enabled && !snapshot.kill_enabled, "both buttons are disabled");
    assert!(rig.ran().is_empty());

    // Writable and ordinary: the dialog opens; the connection turns read-only behind it; Run is refused when pressed.
    give_profile(&rig.app, rig.visual, OA_PG, ConnectionProfile::default());
    assert!(rig.snapshot().cancel_enabled && rig.snapshot().kill_enabled);
    rig.click("process-list-cancel");
    assert!(rig.dialog_open());
    give_profile(&rig.app, rig.visual, OA_PG, typed(PRODUCTION));
    rig.click("process-confirm-run");
    assert!(!rig.dialog_open(), "the dialog closed");
    assert!(rig.ran().is_empty(), "the gate is read again when Run is pressed");
    assert!(rig.snapshot().notice.is_some_and(|(text, failed)| failed && text.contains("read-only")));
}

#[gpui::test]
fn a_production_connection_wants_its_confirmation_ticked_before_the_statement_is_sent(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    publish_types(&rig.app, rig.visual, false);
    give_profile(&rig.app, rig.visual, OA_PG, typed(PRODUCTION));
    rig.select_key("101");
    rig.click("process-list-kill");
    assert!(rig.visual.debug_bounds("process-confirm-ack").is_some(), "Production asks for a named confirmation");
    // Run is disabled until it is ticked: clicking it sends nothing and the dialog stays.
    rig.click("process-confirm-run");
    assert!(rig.dialog_open());
    assert!(rig.ran().is_empty());
    rig.click("process-confirm-ack");
    rig.click("process-confirm-run");
    rig.wait_ran(1);
    assert_eq!(rig.ran(), ["SELECT pg_terminate_backend(101)"]);
}

// ---- the other servers -----------------------------------------------------------------------------------------------------

fn pl_mysql_server() -> PlState {
    std::sync::Arc::new(std::sync::Mutex::new(PlServer {
        rows: vec![
            pl_row(&["5", "root", "localhost:3306", "shop", "Query", "42", "executing", "SELECT SLEEP(30)", "0"]),
            pl_row(&["6", "app", "10.0.0.2:1", "", "Sleep", "120", "", "", "0"]),
            pl_row(&["18446744073709551615", "tablex", "10.0.0.3:1", "shop", "Query", "0", "executing", "SELECT ID FROM information_schema.PROCESSLIST", "1"]),
        ],
        ..PlServer::default()
    }))
}

#[gpui::test]
fn mysql_and_mariadb_cancel_with_kill_query_and_end_a_connection_with_kill(cx: &mut TestAppContext) {
    for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb] {
        let (mut rig, id) = pl_other_rig(cx, kind, pl_mysql_server());
        rig.open(&id);
        assert!(rig.server.lock().unwrap().list_sql[0].contains("information_schema.PROCESSLIST"));
        assert!(rig.server.lock().unwrap().list_sql[0].contains("CONNECTION_ID()"));
        // Hide idle: the Sleep connection is not shown; the app's own is marked and cannot be acted on.
        assert_eq!(rig.snapshot().keys, ["5", "18446744073709551615"]);
        rig.select_key("5");
        rig.click("process-list-cancel");
        assert_eq!(rig.dialog_sql(), "KILL QUERY 5");
        rig.click("process-confirm-run");
        rig.wait_ran(1);
        rig.select_key("5");
        rig.click("process-list-kill");
        assert_eq!(rig.dialog_sql(), "KILL 5");
        rig.click("process-confirm-run");
        rig.wait_ran(2);
        assert_eq!(rig.ran(), ["KILL QUERY 5", "KILL 5"]);
        rig.select_key("18446744073709551615");
        rig.click("process-list-kill");
        assert!(!rig.dialog_open(), "the app's own connection id is not killed");
        assert_eq!(rig.ran().len(), 2);
    }
}

#[gpui::test]
fn clickhouse_cancels_by_a_quoted_query_id_and_has_no_session_to_kill(cx: &mut TestAppContext) {
    // A query id is text the server (or a client) chose: it is a string literal in the statement and nothing else.
    let hostile = "id' OR 1=1; DROP TABLE t; --";
    let server = std::sync::Arc::new(std::sync::Mutex::new(PlServer {
        rows: vec![
            pl_row(&[hostile, "tablex", "::ffff:172.17.0.1", "12.5", "100000", "1048576", "SELECT sleepEachRow(1) FROM numbers(30)", "false"]),
            pl_row(&["own-1", "tablex", "::ffff:172.17.0.1", "0.001", "0", "4096", "SELECT query_id FROM system.processes", "true"]),
        ],
        ..PlServer::default()
    }));
    let (mut rig, id) = pl_other_rig(cx, ConnectionKind::Clickhouse, server);
    rig.open(&id);
    assert!(rig.server.lock().unwrap().list_sql[0].contains("system.processes"));
    assert_eq!(rig.snapshot().keys, [hostile, "own-1"]);
    rig.select_key(hostile);
    assert!(rig.snapshot().cancel_enabled && !rig.snapshot().kill_enabled, "Cancel Query only");
    rig.click("process-list-kill");
    assert!(!rig.dialog_open(), "ClickHouse has no session kill");
    rig.click("process-list-cancel");
    let shown = rig.dialog_sql();
    assert_eq!(shown, "KILL QUERY WHERE query_id = 'id'' OR 1=1; DROP TABLE t; --'");
    rig.click("process-confirm-run");
    rig.wait_ran(1);
    assert_eq!(rig.ran(), [shown]);
    rig.select_key("own-1");
    rig.click("process-list-cancel");
    assert!(!rig.dialog_open());
}

// ---- the clock -------------------------------------------------------------------------------------------------------------

#[gpui::test]
fn the_refresh_interval_is_a_select_that_starts_one_clock_and_the_clock_stops_when_the_tab_is_hidden_or_closed(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    assert_eq!((rig.snapshot().interval, rig.snapshot().timer_running), (None, false), "off until asked");
    // Open the select and dismiss it with Escape: nothing changes.
    rig.click("process-list-interval");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert_eq!(rig.snapshot().interval, None);
    // Open it, one down (2 s), enter.
    rig.click("process-list-interval");
    rig.visual.simulate_keystrokes("down enter");
    settle(rig.visual);
    rig.wait_idle();
    assert_eq!((rig.snapshot().interval, rig.snapshot().timer_running), (Some(2), true));
    // Choosing it read the list once; each 2 s after is another read.
    let base = rig.lists();
    assert_eq!(base, 2);
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_millis(2100));
    rig.wait_idle();
    assert_eq!(rig.lists(), base + 1);
    // Reopened, it still holds 2 s: the next one down is 5 s.
    rig.click("process-list-interval");
    rig.visual.simulate_keystrokes("down enter");
    settle(rig.visual);
    rig.wait_idle();
    assert_eq!(rig.snapshot().interval, Some(5));
    // Hidden (another tab shown): the clock's next turn stops it and reads nothing.
    let tab = rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().id);
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.open_query_tab("scratch");
        cx.notify();
    });
    settle(rig.visual);
    let before = rig.lists();
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_secs(6));
    settle(rig.visual);
    assert_eq!(rig.lists(), before, "a hidden tab reads nothing");
    assert!(!rig.snapshot().timer_running, "the clock stopped");
    // Shown again: the clock starts again, with a read.
    rig.app.update(rig.visual, |this, cx| this.activate_tab_ui(tab, cx));
    settle(rig.visual);
    rig.wait_idle();
    assert!(rig.snapshot().timer_running);
    assert!(rig.lists() > before);
    // Closed: the entity, its clock and its session go, and nothing reads any more.
    let after_resume = rig.lists();
    let weak = rig.view().downgrade();
    rig.app.update(rig.visual, |this, cx| this.close_tab(tab, cx));
    settle(rig.visual);
    assert!(rig.app.read_with(rig.visual, |this, _| this.process_lists.is_empty()));
    assert!(weak.upgrade().is_none(), "the tab's entity is gone");
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_secs(30));
    settle(rig.visual);
    assert_eq!(rig.lists(), after_resume, "a closed tab reads nothing");
    let server = rig.server.clone();
    wait_until(rig.visual, "the session to be let go", |_| server.lock().unwrap().closed >= 1);
    assert_eq!(server.lock().unwrap().opened, 1);
}

// ---- the connection closing, the confirmation, the render ------------------------------------------------------------------

fn pl_set_interval(rig: &mut PlRig<'_>, seconds: u64) {
    let view = rig.view();
    view.update(rig.visual, |view, cx| view.set_interval(Some(seconds), cx));
    rig.wait_idle();
}

#[gpui::test]
fn disconnecting_releases_the_tabs_session_and_clock_and_nothing_reads_or_acts_until_the_connection_is_open_again(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    pl_set_interval(&mut rig, 2);
    rig.select_key("101");
    assert!(rig.snapshot().timer_running && rig.snapshot().cancel_enabled);
    {
        let state = rig.server.lock().unwrap();
        assert_eq!((state.opened, state.closed), (1, 0), "the tab holds a session of its own");
    }

    // Disconnect, as the connection menu does.
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(OA_PG, cx));
    settle(rig.visual);
    let server = rig.server.clone();
    wait_until(rig.visual, "the session to be let go", |_| server.lock().unwrap().closed >= 1);
    let snapshot = rig.snapshot();
    assert!(!snapshot.timer_running, "the clock is gone");
    assert!(snapshot.error.as_deref().is_some_and(|text| text.contains("closed")), "the tab says it is disconnected: {:?}", snapshot.error);
    assert!(rig.visual.debug_bounds("process-list-error").is_some());
    assert!(!snapshot.cancel_enabled && !snapshot.kill_enabled, "nothing can be cancelled or killed");
    assert_eq!(snapshot.total, 4, "what was read stays on screen");

    // No read, whatever the clock would have done, and the buttons ask nothing.
    let lists = rig.lists();
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_secs(30));
    settle(rig.visual);
    assert_eq!(rig.lists(), lists, "nothing is read while the connection is closed");
    for button in ["process-list-cancel", "process-list-kill"] {
        rig.click(button);
        assert!(!rig.dialog_open(), "{button}");
    }
    assert!(rig.ran().is_empty());
    assert_eq!(rig.server.lock().unwrap().opened, 1, "and no session was opened");

    // The connection opens again: the tab reads again and the clock it had comes back.
    rig.app.update(rig.visual, |this, cx| this.rerun_tabs_after_reconnect(OA_PG, cx));
    settle(rig.visual);
    rig.wait_idle();
    let snapshot = rig.snapshot();
    assert!(snapshot.timer_running && snapshot.error.is_none(), "{snapshot:?}");
    assert!(rig.lists() > lists);
    assert_eq!(rig.server.lock().unwrap().opened, 2, "on a session of its own again");
}

#[gpui::test]
fn a_refresh_of_a_tab_whose_connection_is_closed_gets_no_session_to_read_through(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(OA_PG, cx));
    settle(rig.visual);
    let server = rig.server.clone();
    wait_until(rig.visual, "the session to be let go", |_| server.lock().unwrap().closed >= 1);
    // A real window: the fixture connection is not exempt from the open-connection rule.
    rig.app.update(rig.visual, |this, _| {
        this.scenario_ui.service = None;
        assert!(this.process_list_source(OA_PG).is_err_and(|message| message.contains("closed")));
    });
    let lists = rig.lists();
    rig.click("process-list-refresh");
    rig.wait_idle();
    assert_eq!(rig.lists(), lists, "a refresh reads nothing");
    assert_eq!(rig.server.lock().unwrap().opened, 1, "and opens no session");
    assert!(rig.snapshot().error.is_some_and(|text| text.contains("closed")));
}

#[gpui::test]
fn the_clock_does_not_read_while_a_cancel_or_kill_confirmation_waits(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.select_key("101");
    pl_set_interval(&mut rig, 2);
    rig.click("process-list-cancel");
    assert!(rig.dialog_open());
    let lists = rig.lists();
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_secs(7));
    settle(rig.visual);
    rig.wait_idle();
    assert_eq!(rig.lists(), lists, "the rows stay put under the dialog");
    assert_eq!(rig.dialog_sql(), "SELECT pg_cancel_backend(101)", "and the dialog still asks about the statement it was built with");
    assert!(rig.snapshot().timer_running, "the clock itself keeps its place");

    // Answered: the clock reads again.
    rig.click("process-confirm-cancel");
    assert!(!rig.dialog_open());
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_millis(2100));
    settle(rig.visual);
    rig.wait_idle();
    assert!(rig.lists() > lists, "after the dialog the clock reads again");

    // Escape answers it too.
    rig.click("process-list-kill");
    assert!(rig.dialog_open());
    let lists = rig.lists();
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_secs(5));
    settle(rig.visual);
    rig.wait_idle();
    assert_eq!(rig.lists(), lists, "paused again under the second dialog");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(!rig.dialog_open());
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_millis(2100));
    settle(rig.visual);
    rig.wait_idle();
    assert!(rig.lists() > lists, "and reading again after Escape");

    // Run answers it: the action is sent and the list is read.
    rig.click("process-list-kill");
    rig.click("process-confirm-run");
    rig.wait_ran(1);
    let lists = rig.lists();
    rig.visual.cx.executor().advance_clock(std::time::Duration::from_millis(2100));
    settle(rig.visual);
    rig.wait_idle();
    assert!(rig.lists() > lists, "and after Run");
}

#[gpui::test]
fn the_gate_is_read_when_settings_change_and_not_on_every_frame(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.open_pg();
    rig.select_key("101");
    let reads = |rig: &mut PlRig<'_>| rig.app.read_with(rig.visual, |this, _| this.process_list_gate_reads.get());
    let first = reads(&mut rig);
    assert!(first >= 1, "the first frame read it");
    // Frames of the tab, and of the whole window, with nothing changed.
    for _ in 0..5 {
        let view = rig.view();
        view.update(rig.visual, |_, cx| cx.notify());
        rig.app.update(rig.visual, |_, cx| cx.notify());
        settle(rig.visual);
    }
    rig.click("process-list-refresh");
    rig.wait_idle();
    assert_eq!(reads(&mut rig), first, "no frame read the gate again");
    assert!(rig.snapshot().cancel_enabled);

    // The connection becomes read-only: the next frame has the new gate, and only that one read it.
    publish_types(&rig.app, rig.visual, true);
    give_profile(&rig.app, rig.visual, OA_PG, typed(PRODUCTION));
    let view = rig.view();
    view.update(rig.visual, |_, cx| cx.notify());
    settle(rig.visual);
    assert!(reads(&mut rig) > first, "a changed profile is read again");
    let after = reads(&mut rig);
    for _ in 0..3 {
        let view = rig.view();
        view.update(rig.visual, |_, cx| cx.notify());
        settle(rig.visual);
    }
    assert_eq!(reads(&mut rig), after);
    assert!(!rig.snapshot().cancel_enabled, "and the buttons follow it");

    // A rename shows in the subtitle without waiting for anything else.
    rig.app.update(rig.visual, |this, cx| {
        this.saved_connections.iter_mut().find(|saved| saved.matches_id(OA_PG)).unwrap().name = "Renamed fixture".into();
        cx.notify();
    });
    settle(rig.visual);
    let view = rig.view();
    view.update(rig.visual, |_, cx| cx.notify());
    settle(rig.visual);
    assert!(reads(&mut rig) > after);
}

#[gpui::test]
fn at_1100_px_the_session_the_duration_and_the_query_are_on_screen(cx: &mut TestAppContext) {
    let mut rig = pl_pg_rig(cx);
    rig.visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    rig.open_pg();
    settle(rig.visual);
    let window = gpui::px(1100.0);
    // Columns in order: PID, User, Database, State, Duration, Query.
    for (column, name) in [(0, "PID"), (1, "User"), (3, "State"), (4, "Duration"), (5, "Query")] {
        let selector: &'static str = Box::leak(format!("process-cell-0-{column}").into_boxed_str());
        let bounds = rig.visual.debug_bounds(selector).unwrap_or_else(|| panic!("{name} is drawn"));
        assert!(bounds.left() < window, "{name} starts inside the window");
    }
    let query = rig.visual.debug_bounds("process-cell-0-5").unwrap();
    assert!(query.left() + gpui::px(100.0) < window, "at least 100 px of the query show: {:?}", query);
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) --------------------------------------------------
//
// Run with the URIs of the containers (see sql_ddl/live.rs) and `-- --ignored --test-threads=1`. A test without its URI says so and ends.
// Each starts a long statement on a second connection of the same user, finds it in the tab by a tag in its text (typed in the filter),
// cancels it (or kills its session) from the dialog, and checks that the statement really ended long before it would have.

fn pl_live(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str, sleep: &str, kills_session: bool) {
    use crate::app::accounts::DriverCatalog;
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let rig = nav_rig(cx, kind, driver, &uri);
    let SqlRig { app, visual, key, handle, .. } = rig;
    let mut rig = PlRig { app, visual, server: Default::default() };
    rig.open(&key);
    assert!(rig.snapshot().error.is_none(), "{:?}", rig.snapshot());

    for (action, button, expect_row_gone) in [("cancel", "process-list-cancel", false), ("kill", "process-list-kill", true)] {
        if action == "kill" && !kills_session {
            continue;
        }
        let tag = format!("tagpl{}", &uuid::Uuid::new_v4().to_simple().to_string()[..10]);
        let statement = sleep.replace("TAG", &tag);
        // The long statement, on a second connection.
        let mut worker = handle
            .connect(&serde_json::json!({ "uri": uri }).to_string())
            .map(DriverCatalog::of_session)
            .unwrap_or_else(|error| panic!("a second connection: {error}"));
        let (sender, receiver) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            let result = worker.run(&statement).map_err(|error| error.to_string());
            let _ = sender.send((result, started.elapsed()));
        });
        // It shows up in the list (found by the tag, typed in the filter; the tab reads the server again until it is there).
        oa_type(rig.visual, "process-list-filter", &tag);
        let mut found = false;
        for _ in 0..40 {
            rig.click("process-list-refresh");
            rig.wait_idle();
            if rig.snapshot().keys.len() == 1 {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        assert!(found, "the long statement is in the list: {:?}", rig.snapshot());
        rig.select(0);
        let key_of_row = rig.snapshot().selected.expect("a selected row");
        rig.click(button);
        assert!(rig.dialog_open(), "{action}: the dialog asks");
        let sql = rig.dialog_sql();
        eprintln!("{action}: {sql}");
        assert!(sql.contains(&key_of_row.replace('\'', "''")), "{sql} names the session {key_of_row}");
        rig.click("process-confirm-run");
        // The statement ends on its own, long before its 30 seconds.
        let (result, took) = loop {
            rig.visual.run_until_parked();
            match receiver.try_recv() {
                Ok(done) => break done,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    assert!(started.elapsed() < std::time::Duration::from_secs(20), "{action}: the statement did not end");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => panic!("{error}"),
            }
        };
        assert!(result.is_err(), "{action}: the statement was interrupted, not finished: {result:?}");
        assert!(took < std::time::Duration::from_secs(20), "{action}: it ended after {took:?}");
        eprintln!("{action}: ended after {took:?}: {}", result.unwrap_err());
        rig.wait_idle();
        assert!(rig.snapshot().error.is_none(), "{:?}", rig.snapshot());
        // The list no longer has it running. After a kill the session is gone altogether (Hide idle off to see idle ones too).
        rig.click("process-list-hide-idle");
        let mut gone = false;
        for _ in 0..40 {
            rig.click("process-list-refresh");
            rig.wait_idle();
            let snapshot = rig.snapshot();
            let active = snapshot.cells.iter().any(|cells| cells.iter().any(|cell| cell.contains(&tag)) && !cells.iter().any(|cell| cell == "idle" || cell == "Sleep"));
            let listed = snapshot.keys.contains(&key_of_row);
            if !active && (!expect_row_gone || !listed) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        assert!(gone, "{action}: the statement is no longer running: {:?}", rig.snapshot());
        rig.click("process-list-hide-idle");
        let filter = rig.visual.debug_bounds("process-list-filter").expect("the filter");
        rig.visual.simulate_click(filter.center(), Default::default());
        rig.visual.simulate_keystrokes("secondary-a backspace");
        settle(rig.visual);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgres_a_long_query_is_found_cancelled_and_its_session_killed(cx: &mut TestAppContext) {
    pl_live(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI", "SELECT pg_sleep(30) /* TAG */", true);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_a_long_query_is_found_cancelled_and_its_connection_killed(cx: &mut TestAppContext) {
    pl_live(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI", "SELECT SLEEP(30) /* TAG */", true);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_long_query_is_found_cancelled_and_its_connection_killed(cx: &mut TestAppContext) {
    pl_live(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI", "SELECT SLEEP(30) /* TAG */", true);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (container tablex-test-clickhouse)"]
fn live_clickhouse_a_long_query_is_found_and_cancelled(cx: &mut TestAppContext) {
    pl_live(
        cx,
        ConnectionKind::Clickhouse,
        CLICKHOUSE_DRIVER,
        "TABLEX_TEST_CLICKHOUSE_URI",
        "SELECT sleepEachRow(0.5) FROM numbers(30) SETTINGS max_block_size = 1 /* TAG */",
        false,
    );
}
