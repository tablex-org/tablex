// Manual commit mode of a SQL tab, through the real SQLite driver (a file database, so a second
// session sees the same data; built on demand, see `sql_driver.rs`) and the real toolbar buttons,
// dialogs and Apply. The live PostgreSQL / MySQL / MariaDB runs at the end are `#[ignore]`d like
// the other live tests: they prove that what a tab does in manual mode is invisible to another tab
// of the same connection until Commit.

use crate::transaction::{CommitMode, TabTx, Typed};

impl SqlRig<'_> {
    /// Another SQL tab on the rig's connection (auto-commit), made the active tab.
    fn second_tab(&mut self) -> u64 {
        let key = self.key.clone();
        self.app.update(self.visual, |this, cx| {
            let tab = this.workspace.open_query_tab_with_metadata(
                "SQL 2",
                TabMetadata { connection_id: key, ..Default::default() },
            );
            this.reset_active_view();
            cx.notify();
            tab
        })
    }

    fn activate(&mut self, tab: u64) {
        self.app.update(self.visual, |this, cx| {
            this.workspace.activate_tab(tab);
            cx.notify();
        });
        settle(self.visual);
    }

    /// Runs `sql` in `tab` through its Run button's handler and waits for the run to end.
    fn run_on(&mut self, tab: u64, sql: &str) {
        self.app.update(self.visual, |this, cx| {
            this.workspace.activate_tab(tab);
            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
            this.run_active_tab_query(cx);
        });
        self.wait_idle(tab);
    }

    fn wait_idle(&mut self, tab: u64) {
        let app = self.app.clone();
        wait_until(self.visual, "the run to end", |visual| {
            app.read_with(visual, |this, _| {
                this.workspace.runtime(tab).unwrap().query.can_run() && !this.query_tasks.tasks.contains_key(&tab)
            })
        });
        settle(self.visual);
    }

    fn tx(&self, tab: u64) -> Option<TabTx> {
        self.app.read_with(self.visual, |this, _| this.transactions.tx(tab).cloned())
    }

    fn mode(&self, tab: u64) -> CommitMode {
        self.app.read_with(self.visual, |this, _| this.commit_mode(tab))
    }

    /// Statements the app counts on the tab's session since its last Commit / Rollback.
    fn pending(&self, tab: u64) -> u32 {
        self.tx(tab).map_or(0, |tx| tx.statements)
    }

    /// How many cleanups of an auto-commit run were started.
    fn cleanups(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.transactions.cleanups)
    }

    fn log_messages(&self) -> Vec<String> {
        self.app.read_with(self.visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .map(|entry| entry.message.to_string())
                .collect()
        })
    }

    /// How many times the Error Log recorded a message containing `needle` (identical ones in a
    /// row are one entry with a repeat count).
    fn logged_times(&self, needle: &str) -> u32 {
        self.app.read_with(self.visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .filter(|entry| entry.message.contains(needle))
                .map(|entry| entry.repeat.max(1))
                .sum()
        })
    }

    fn modifying(&self, tab: u64) -> u32 {
        self.tx(tab).map_or(0, |tx| tx.modifying)
    }

    /// Waits until `sql` on `tab` answers `expected` (the shared session ends an auto-commit tab's
    /// open transaction a moment after its run).
    fn eventually(&mut self, tab: u64, sql: &str, expected: &str) {
        for _ in 0..200 {
            self.run_on(tab, sql);
            if self.tab_cell(tab) == expected {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{sql} never answered {expected}: {}", self.tab_cell(tab));
    }

    /// The effective connection type of the rig's connection, set by `change`.
    fn set_connection_type(&mut self, change: impl FnOnce(&mut crate::settings::connection::ConnectionType)) {
        let key = self.key.clone();
        self.app.update(self.visual, |this, cx| {
            let profile = crate::settings::connection::ConnectionProfile { type_id: "test-type".into(), ..Default::default() };
            let mut settings = (*this.settings).clone();
            let mut ty = crate::settings::connection::ConnectionType { id: "test-type".into(), name: "test".into(), ..Default::default() };
            change(&mut ty);
            settings.connection_types.retain(|existing| existing.id != "test-type");
            settings.connection_types.push(ty);
            this.settings = std::sync::Arc::new(settings);
            this.connection_policy.set_profile(&key, profile);
            cx.notify();
        });
        settle(self.visual);
    }

    fn wait_tx(&mut self, label: &'static str, tab: u64, ready: impl Fn(&TabTx) -> bool) {
        let app = self.app.clone();
        wait_until(self.visual, label, |visual| {
            app.read_with(visual, |this, _| this.transactions.tx(tab).is_some_and(&ready))
        });
        settle(self.visual);
    }

    /// The driver's own id of the session the tab has of its own.
    fn own_session(&self, tab: u64) -> Option<u64> {
        self.app.read_with(self.visual, |this, _| this.driver_runtime.tab_sessions.get(&tab).map(|own| own.connection_id))
    }

    fn own_sessions(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.driver_runtime.tab_sessions.len())
    }

    /// The connection's shared session is still the one the rig connected.
    fn shared_untouched(&self) -> bool {
        let (key, session) = (self.key.clone(), self.session.clone());
        self.app.read_with(self.visual, |this, _| {
            this.driver_runtime.sessions.get(&key).is_some_and(|shared| std::sync::Arc::ptr_eq(shared, &session))
        })
    }

    /// First cell of the first row of `tab`'s result.
    fn tab_cell(&self, tab: u64) -> String {
        self.app.read_with(self.visual, |this, _| {
            this.workspace
                .runtime(tab)
                .unwrap()
                .grid
                .cell_text(CellPosition { row: 0, column: 0 })
                .map(|text| text.to_string())
                .unwrap_or_default()
        })
    }

    fn rendered_now(&mut self, selector: &'static str) -> bool {
        self.visual.debug_bounds(selector).is_some()
    }

    /// A second saved connection to the rig's database, connected through the rig's driver.
    fn add_connection(&mut self) -> String {
        let key = self.key.clone();
        let handle = self.handle.clone();
        self.app.update(self.visual, |this, cx| {
            let first = this.saved_connections.iter().find(|saved| saved.matches_id(&key)).cloned().unwrap();
            let id = uuid::Uuid::new_v4();
            let other = id.to_string();
            this.saved_connections.push(SavedConnection { id, name: "driver-test-2".into(), ..first.clone() });
            let session = handle.connect(&serde_json::json!({ "uri": first.uri }).to_string()).expect("the driver connects");
            let engine_id = session.connection_id();
            this.driver_runtime.sessions.insert(other.clone(), std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session)));
            this.driver_runtime.connection_ids.insert(other.clone(), engine_id);
            this.driver_runtime.session_handles.insert(other.clone(), handle.clone());
            this.driver_runtime.capabilities.insert(other.clone(), std::sync::Arc::new(handle.capabilities().clone()));
            let epoch = this.sessions.begin_connect(&other).unwrap();
            this.sessions.finish_connect(&other, epoch, Ok(()));
            cx.notify();
            other
        })
    }

    /// Starts `sql` in `tab` without waiting for it.
    fn start_on(&mut self, tab: u64, sql: &str) {
        self.app.update(self.visual, |this, cx| {
            this.workspace.activate_tab(tab);
            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
            this.run_active_tab_query(cx);
        });
    }

    fn status_of(&self, tab: u64) -> crate::query::QueryStatus {
        self.app.read_with(self.visual, |this, _| this.workspace.runtime(tab).unwrap().query.status().clone())
    }

    fn end_blocked(&self, tab: u64) -> Option<super::transaction::EndBlocked> {
        self.app.read_with(self.visual, |this, _| this.end_blocked(tab))
    }

    fn wait_rendered_gone(&mut self, selector: &'static str) {
        for _ in 0..200 {
            settle(self.visual);
            if self.visual.debug_bounds(selector).is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("{selector} is still rendered");
    }

    fn set_effective_read_only(&mut self, read_only: bool) {
        let key = self.key.clone();
        self.app.update(self.visual, |this, cx| {
            let profile = crate::settings::connection::ConnectionProfile { type_id: "read-only-test".into(), ..Default::default() };
            let mut settings = (*this.settings).clone();
            let mut ty = crate::settings::connection::ConnectionType { id: "read-only-test".into(), name: "ro".into(), ..Default::default() };
            ty.read_only = read_only;
            settings.connection_types.push(ty);
            this.settings = std::sync::Arc::new(settings);
            this.connection_policy.set_profile(&key, profile);
            cx.notify();
        });
        settle(self.visual);
    }
}

const COUNT_ITEMS: &str = "SELECT count(*) FROM items";

#[gpui::test]
fn manual_mode_runs_on_a_session_of_the_tab_and_hides_its_changes_until_commit(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, shared_id) = (rig.tab, rig.session.lock().unwrap().connection_id());
    let b = rig.second_tab();
    rig.activate(a);

    // Auto-commit: the toggle is there, nothing ends a transaction yet, no session of its own.
    assert_eq!(rig.mode(a), CommitMode::Auto);
    assert!(rig.rendered_now("tx-mode-auto") && rig.rendered_now("tx-mode-manual"));
    assert!(!rig.rendered_now("tx-commit") && !rig.rendered_now("tx-status"));
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.own_sessions(), 0, "an auto-commit tab opens no extra connection");

    // The toggle opens no session by itself: the first statement does.
    rig.click("tx-mode-manual");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    assert_eq!(rig.own_sessions(), 0, "lazily, at the first statement");
    assert!(rig.rendered_now("tx-commit") && rig.rendered_now("tx-rollback"));
    assert_eq!(rig.pending(a), 0);

    rig.run_on(a, "INSERT INTO items(name) VALUES ('x')");
    let own = rig.own_session(a).expect("the first statement opened the tab's session");
    assert_ne!(own, shared_id, "a session of its own, not the connection's");
    assert!(rig.shared_untouched(), "the connection's shared session is untouched");
    assert_eq!(rig.own_sessions(), 1);
    assert_eq!(rig.pending(a), 1);
    assert_eq!(db.sql(COUNT_ITEMS), "5", "nothing is committed yet");

    // The tab sees its own change, another tab and another process do not.
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(a), "6");
    assert_eq!(rig.pending(a), 2);
    rig.run_on(b, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(b), "5", "another tab does not see the uncommitted row");
    assert_eq!(rig.own_session(b), None, "the other tab stays on the shared session");
    assert_eq!(rig.own_session(a), Some(own), "the session is opened once");

    // The toolbar says so.
    rig.activate(a);
    assert!(rig.rendered_now("tx-status"));
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql(COUNT_ITEMS), "6");
    rig.run_on(b, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(b), "6", "committed: the other tab sees it");

    // Back to auto-commit closes the tab's session; the next statement is on the shared one.
    assert_eq!(rig.own_session(a), Some(own), "Commit keeps the session of a manual tab");
    rig.activate(a);
    rig.click("tx-mode-auto");
    assert_eq!(rig.mode(a), CommitMode::Auto);
    assert_eq!(rig.own_sessions(), 0);
    assert!(!rig.rendered_now("tx-commit"));
    rig.run_on(a, "INSERT INTO items(name) VALUES ('y')");
    assert_eq!(db.sql(COUNT_ITEMS), "7", "auto-commit again: committed by itself");
    assert!(rig.shared_untouched());
}

#[gpui::test]
fn rollback_discards_what_the_transaction_did(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "DELETE FROM items WHERE id <= 2");
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(a), "3");
    assert_eq!(rig.pending(a), 2);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql(COUNT_ITEMS), "5");
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(a), "5", "the tab itself sees the rows again");
    // The statement after a rollback opens a new transaction, counted from one.
    assert_eq!(rig.pending(a), 1);
}

#[gpui::test]
fn a_manual_tab_takes_typed_transaction_statements_as_typed_and_stays_manual(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, "BEGIN");
    assert_eq!(rig.pending(a), 1, "the typed BEGIN is counted, and no second BEGIN was added before it");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('t')");
    assert_eq!(rig.pending(a), 2);
    rig.run_on(b, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(b), "5", "the other tab sees no uncommitted row");

    // A typed ROLLBACK resets the counters and undoes the row; a typed COMMIT keeps one.
    rig.run_on(a, "ROLLBACK");
    assert_eq!((rig.pending(a), rig.modifying(a)), (0, 0));
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 't'"), "0");
    rig.run_on(a, "BEGIN");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('kept')");
    rig.run_on(a, "COMMIT");
    assert_eq!(rig.pending(a), 0);
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'kept'"), "1");
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Manual, 1), "the tab stays manual on its session");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('after')");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'after'"), "0", "not committed by itself");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
}

#[gpui::test]
fn a_transaction_an_auto_commit_run_leaves_open_is_rolled_back_when_the_run_ends(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    assert_eq!(rig.mode(a), CommitMode::Auto);

    // A script: what ran before the BEGIN is committed, what follows it is not.
    rig.run_on(a, "INSERT INTO items(name) VALUES ('before'); BEGIN; INSERT INTO items(name) VALUES ('inside'); INSERT INTO items(name) VALUES ('inside2')");
    rig.eventually(b, "SELECT group_concat(name) FROM items WHERE name IN ('before','inside','inside2')", "before");
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 0), "the tab stays on the shared session");
    assert!(rig.shared_untouched());

    let cleanups = rig.cleanups();
    assert_eq!(cleanups, 1);

    // One that ends its transaction keeps it, and cleans nothing up: no ROLLBACK, no notice.
    rig.run_on(a, "BEGIN; INSERT INTO items(name) VALUES ('kept'); COMMIT");
    rig.run_on(b, "SELECT count(*) FROM items WHERE name = 'kept'");
    assert_eq!(rig.tab_cell(b), "1");
    rig.run_on(a, "COMMIT");
    rig.run_on(a, "SELECT 1");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('plain')");
    assert_eq!(rig.cleanups(), cleanups, "nothing was left open: nothing was ended");

    // A BEGIN alone is ended too: the next run of another tab commits by itself again.
    rig.run_on(a, "BEGIN");
    rig.run_on(b, "INSERT INTO items(name) VALUES ('autocommitted')");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'autocommitted'"), "1");
}

// ---- the questions --------------------------------------------------------------------------------

#[gpui::test]
fn closing_a_tab_with_an_open_transaction_asks_commit_rollback_or_cancel(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let _b = rig.second_tab();
    // A: manual, with a row inserted.
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('closing')");
    let own = rig.own_session(a).expect("a session");

    // Cancel keeps the tab, its session and its transaction.
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-commit") && rig.rendered_now("tx-guard-rollback") && rig.rendered_now("tx-guard-cancel"));
    assert!(rig.rendered_now("tx-guard-tabs"));
    rig.click("tx-guard-cancel");
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(a).is_some()));
    assert_eq!(rig.own_session(a), Some(own));
    assert_eq!(rig.pending(a), 1);

    // Roll Back: the transaction is rolled back, then the tab closes with its session.
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    rig.click("tx-guard-rollback");
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to close", |visual| app.read_with(visual, |this, _| this.workspace.tab(a).is_none()));
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'closing'"), "0");
    assert_eq!(rig.own_sessions(), 0, "its session closed with it");
    assert!(rig.shared_untouched());
}

#[gpui::test]
fn closing_a_tab_with_commit_commits_then_closes(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let _b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('committed on close')");
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    rig.click("tx-guard-commit");
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to close", |visual| app.read_with(visual, |this, _| this.workspace.tab(a).is_none()));
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'committed on close'"), "1");
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn a_tab_without_a_transaction_closes_without_a_question_and_drops_its_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let _b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('x')");
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert!(rig.own_session(a).is_some(), "the manual tab keeps its session between transactions");
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    assert!(!rig.rendered_now("tx-guard-commit"), "nothing is open: no question");
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(a).is_none()));
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn switching_to_auto_commit_with_an_open_transaction_asks_first(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('switch')");

    rig.click("tx-mode-auto");
    assert!(rig.rendered_now("tx-guard-commit"), "asked");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.click("tx-guard-cancel");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    assert_eq!(rig.pending(a), 1);

    rig.click("tx-mode-auto");
    rig.click("tx-guard-commit");
    rig.wait_tx("the switch", a, |tx| tx.mode == CommitMode::Auto);
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'switch'"), "1", "Commit committed before switching");
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn quitting_and_closing_the_window_ask_while_a_transaction_is_open(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    // Nothing open: quit is not held back.
    assert!(!rig.app.update_in(rig.visual, |this, window, cx| this.request_quit(window, cx)));
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('quit')");
    assert!(rig.app.update_in(rig.visual, |this, window, cx| this.request_quit(window, cx)), "asks");
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-commit"));
    rig.click("tx-guard-cancel");
    assert_eq!(rig.pending(a), 1, "Cancel leaves the transaction open");
    assert!(rig.app.update_in(rig.visual, |this, window, cx| this.request_quit(window, cx)), "asks again");
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"));
    rig.click("tx-guard-cancel");

    // The window's close button asks the same question.
    let closes = rig.visual.simulate_close();
    assert!(!closes, "the open transaction holds the window back");
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-commit"));
    rig.click("tx-guard-cancel");
}

#[gpui::test]
fn disconnecting_the_connection_asks_and_then_closes_the_tabs_sessions(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let key = rig.key.clone();
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('disc')");

    rig.app.update_in(rig.visual, |this, window, cx| this.request_disconnect(&key, window, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "asked");
    rig.click("tx-guard-cancel");
    assert!(rig.shared_untouched(), "Cancel disconnected nothing");
    assert_eq!(rig.pending(a), 1);

    rig.app.update_in(rig.visual, |this, window, cx| this.request_disconnect(&key, window, cx));
    settle(rig.visual);
    rig.click("tx-guard-rollback");
    let (app, key2) = (rig.app.clone(), key.clone());
    wait_until(rig.visual, "the disconnect", |visual| {
        app.read_with(visual, |this, _| !this.driver_runtime.sessions.contains_key(&key2))
    });
    assert_eq!(rig.own_sessions(), 0, "the tab's session went with the connection");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'disc'"), "0");
}

// ---- broken sessions, read-only, kinds ------------------------------------------------------------

#[gpui::test]
fn a_session_that_dropped_shows_the_transaction_as_lost_and_is_dropped(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('lost')");
    // The session dies under the transaction (the driver is told to disconnect it).
    let session = rig.app.read_with(rig.visual, |this, _| this.driver_runtime.tab_sessions.get(&a).unwrap().session.clone());
    session.lock().unwrap().disconnect().unwrap();

    rig.run_on(a, COUNT_ITEMS);
    let tx = rig.tx(a).unwrap();
    assert!(tx.lost.as_deref().is_some_and(|lost| lost.starts_with("Session lost: uncommitted changes were rolled back")), "{:?}", tx.lost);
    assert_eq!(tx.statements, 0, "the counter is reset");
    assert_eq!(rig.own_sessions(), 0, "the broken session was dropped");
    assert!(rig.shared_untouched(), "the connection's own session is fine");
    let connection_lost = rig.app.read_with(rig.visual, |this, _| {
        matches!(this.sessions.status(&rig.key), ConnectionStatus::Failed(_))
    });
    assert!(!connection_lost, "only the tab's session is lost, not the connection");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'lost'"), "0", "the server rolled it back");
    assert!(rig.rendered_now("tx-status"));
    assert!(rig.log().iter().any(|(_, severity, _)| *severity == crate::error_log::Severity::Warning), "the Error Log says so");

    // The next statement opens a fresh session of its own and clears the message.
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(a), "5");
    assert_eq!(rig.tx(a).unwrap().lost, None);
    assert!(rig.own_session(a).is_some(), "a fresh session of its own");
    assert_eq!(rig.pending(a), 1);
}

#[gpui::test]
fn manual_mode_is_unavailable_on_a_read_only_connection(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_effective_read_only(true);
    let reason = rig.app.read_with(rig.visual, |this, _| this.manual_unavailable_reason(a));
    assert!(reason.is_some_and(|reason| reason.contains("read-only")));
    rig.click("tx-mode-manual");
    assert_eq!(rig.mode(a), CommitMode::Auto, "the toggle does nothing");

    // A typed BEGIN is left to the driver, which refuses it in its read-only run: no session, no state.
    rig.run_on(a, "BEGIN");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));
    assert_eq!(rig.own_sessions(), 0);
    assert_eq!(rig.mode(a), CommitMode::Auto);
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(a), "5", "reads keep working");
}

#[gpui::test]
fn a_connection_type_that_is_not_auto_commit_starts_its_tabs_manual(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        let ty = crate::settings::connection::ConnectionType {
            id: "manual-type".into(),
            name: "manual".into(),
            auto_commit: false,
            ..Default::default()
        };
        settings.connection_types.push(ty);
        this.settings = std::sync::Arc::new(settings);
        let profile = crate::settings::connection::ConnectionProfile { type_id: "manual-type".into(), ..Default::default() };
        this.connection_policy.set_profile(&key, profile);
        cx.notify();
    });
    let fresh = rig.second_tab();
    assert_eq!(rig.mode(fresh), CommitMode::Manual, "the type says: not auto-commit");
    rig.run_on(fresh, "INSERT INTO items(name) VALUES ('typed default')");
    assert!(rig.own_session(fresh).is_some());
    assert_eq!(db.sql(COUNT_ITEMS), "5");
}

#[gpui::test]
fn kinds_without_transactions_offer_the_toggle_disabled_with_the_reason(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    {
        let kind = ConnectionKind::Clickhouse;
        let tab = rig.tab(kind, "SELECT 1");
        let reason = rig.app.read_with(rig.visual, |this, _| this.manual_unavailable_reason(tab));
        assert!(reason.is_some_and(|reason| reason.contains("no transactions")), "{kind:?}");
        assert!(rig.rendered("tx-mode-manual"), "{kind:?}: the toggle is drawn");
        rig.click("tx-mode-manual");
        let mode = rig.app.read_with(rig.visual, |this, _| this.commit_mode(tab));
        assert_eq!(mode, CommitMode::Auto, "{kind:?}: it does nothing");
        assert!(!rig.rendered("tx-commit"), "{kind:?}");
    }
    // The kinds that do have transactions offer it.
    for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql, ConnectionKind::Mariadb, ConnectionKind::Sqlite] {
        let tab = rig.tab(kind, "SELECT 1");
        let reason = rig.app.read_with(rig.visual, |this, _| this.manual_unavailable_reason(tab));
        assert_eq!(reason, None, "{kind:?}");
    }
}

// ---- review fixes ---------------------------------------------------------------------------------

/// A transaction open for a couple of statements, for tests that need no session behind it.
fn open_tx_state() -> TabTx {
    let mut tx = TabTx::new(CommitMode::Manual);
    tx.statements = 2;
    tx.modifying = 1;
    tx.since = Some(Instant::now());
    tx.last_active = tx.since;
    tx
}

#[gpui::test]
fn a_tab_moved_to_another_connection_asks_first_and_drops_its_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    let other = rig.add_connection();
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('moved')");
    let target = other.clone();
    rig.app.update(rig.visual, |this, cx| this.retarget_script_connection(a, &target, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "asked");
    let connection_of = |rig: &SqlRig<'_>| rig.app.read_with(rig.visual, |this, _| this.workspace.tab(a).unwrap().connection_id.clone());
    assert_eq!(connection_of(&rig), key, "not moved yet");
    rig.click("tx-guard-cancel");
    assert_eq!(connection_of(&rig), key);
    assert_eq!(rig.pending(a), 1);

    let target = other.clone();
    rig.app.update(rig.visual, |this, cx| this.retarget_script_connection(a, &target, cx));
    settle(rig.visual);
    rig.click("tx-guard-rollback");
    let app = rig.app.clone();
    let other2 = other.clone();
    wait_until(rig.visual, "the tab to move", |visual| {
        app.read_with(visual, |this, _| this.workspace.tab(a).unwrap().connection_id == other2)
    });
    assert_eq!(rig.own_sessions(), 0, "the old connection's session was dropped");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'moved'"), "0", "rolled back");
}

#[gpui::test]
fn a_session_of_another_connection_is_never_used_for_a_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let other = rig.add_connection();
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('old connection')");
    let old = rig.own_session(a).expect("a session");
    // The tab is moved without anyone closing its session (what a missed path would do).
    let moved = other.clone();
    rig.app.update(rig.visual, |this, _| this.workspace.tab_mut(a).unwrap().connection_id = moved);
    let belongs = rig.app.read_with(rig.visual, |this, _| this.own_session(a, &other).is_some());
    assert!(!belongs, "the old session is not the tab's session on the new connection");
    let shared_is_other = rig.app.read_with(rig.visual, |this, _| {
        let used = this.tab_session_for(a, &other).unwrap();
        std::sync::Arc::ptr_eq(&used, this.driver_runtime.sessions.get(&other).unwrap())
    });
    assert!(shared_is_other, "pages of the tab go to the new connection's session, not the old own one");
    rig.run_on(a, COUNT_ITEMS);
    let new = rig.own_session(a).expect("a session of the new connection");
    assert_ne!(new, old, "a new session, opened for the connection the tab is on");
    assert_eq!(rig.tab_cell(a), "5", "it did not run in the old connection's transaction");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'old connection'"), "0");
}

#[gpui::test]
fn in_the_guard_dialog_enter_rolls_back_and_escape_cancels(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let _b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('enter')");
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-commit"));
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    rig.wait_rendered_gone("tx-guard-commit");
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(a).is_some()), "Escape: the tab is still there");
    assert_eq!(rig.pending(a), 1, "Escape: the transaction is still open");

    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    rig.visual.simulate_keystrokes("enter");
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to close", |visual| app.read_with(visual, |this, _| this.workspace.tab(a).is_none()));
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'enter'"), "0", "Enter: rolled back, not committed");
}

#[gpui::test]
fn a_manual_tab_that_ran_any_statement_is_asked_about_but_one_that_ran_none_is_not(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let _b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    // Nothing run yet: switching back and closing ask nothing.
    rig.click("tx-mode-auto");
    assert!(!rig.rendered_now("tx-guard-commit"));
    rig.click("tx-mode-manual");
    // A read is a statement of the transaction too: asked, though nothing was modified.
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!((rig.pending(a), rig.modifying(a)), (1, 0));
    assert!(rig.app.read_with(rig.visual, |this, _| this.open_transactions_on(std::slice::from_ref(&this.workspace.tab(a).unwrap().connection_id))));
    rig.click("tx-mode-auto");
    assert!(rig.rendered_now("tx-guard-commit"), "reads only, still asked");
    rig.click("tx-guard-cancel");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-commit"));
    rig.click("tx-guard-rollback");
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to close", |visual| app.read_with(visual, |this, _| this.workspace.tab(a).is_none()));
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn a_transaction_with_changes_is_rolled_back_after_the_idle_limit_of_its_connection_type(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    // The type does not ask for it: nothing is rolled back, however long it idles.
    rig.set_connection_type(|ty| ty.idle_transaction_secs = Some(60));
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('idle')");
    rig.app.update(rig.visual, |this, cx| {
        this.transactions.age(a, 3600);
        this.roll_back_idle(cx);
    });
    settle(rig.visual);
    assert_eq!(rig.pending(a), 1);

    // Asked for, with a 60 s limit: the hour it already idled is past it.
    rig.set_connection_type(|ty| {
        ty.auto_close_idle_transactions = true;
        ty.idle_transaction_secs = Some(60);
    });
    rig.app.update(rig.visual, |this, cx| {
        this.transactions.age(a, 1);
        this.roll_back_idle(cx);
    });
    settle(rig.visual);
    assert_eq!(rig.pending(a), 0, "rolled back");
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'idle'"), "0");

    // Only reads: the timer leaves it alone (closing it is silent anyway).
    rig.run_on(a, COUNT_ITEMS);
    rig.app.update(rig.visual, |this, cx| {
        this.transactions.age(a, 3600);
        this.roll_back_idle(cx);
    });
    settle(rig.visual);
    assert_eq!(rig.pending(a), 1);
    let _ = b;
}

#[gpui::test]
fn a_postgres_tab_whose_statement_failed_shows_a_hint_and_keeps_commit_and_rollback_enabled(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "SELECT 1");
    let state = |rig: &mut ExplainRig<'_>, aborted_hint: bool| {
        rig.app.update(rig.visual, |this, cx| {
            let mut tx = open_tx_state();
            tx.aborted_hint = aborted_hint;
            this.transactions.set_for_capture(tab, tx);
            cx.notify();
        });
        settle(rig.visual);
    };
    state(&mut rig, false);
    assert!(rig.rendered("tx-status") && !rig.rendered("tx-hint"));
    state(&mut rig, true);
    assert!(rig.rendered("tx-hint"), "PostgreSQL: the transaction is aborted until you roll back");
    // A hint only: nothing is disabled or refused because of it.
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.end_blocked(tab)), None);
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(tab, window, cx));
    settle(rig.visual);
    assert!(rig.rendered("tx-guard-commit") && !rig.rendered("tx-guard-failed"));
    rig.click("tx-guard-cancel");
}

#[gpui::test]
fn table_data_tabs_have_no_commit_mode_and_never_open_a_transaction(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (tab, key) = (rig.tab, rig.key.clone());
    // Even on a connection type that starts tabs in manual commit.
    rig.app.update(rig.visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.connection_types.push(crate::settings::connection::ConnectionType {
            id: "manual-type".into(),
            name: "manual".into(),
            auto_commit: false,
            ..Default::default()
        });
        this.settings = std::sync::Arc::new(settings);
        let profile = crate::settings::connection::ConnectionProfile { type_id: "manual-type".into(), ..Default::default() };
        this.connection_policy.set_profile(&key, profile);
        cx.notify();
    });
    rig.open_table("items");
    rig.wait_structure();
    assert!(!rig.rendered_now("tx-mode-manual") && !rig.rendered_now("tx-mode-auto"), "no toggle on a table-data tab");
    assert_eq!(rig.mode(tab), CommitMode::Auto);
    assert_eq!(rig.own_sessions(), 0, "browsing opened no session of its own, and no transaction");
    assert!(rig.tx(tab).is_none());
    // The grid Apply is the driver's own transaction: it commits by itself.
    rig.edit_cell(0, 1, "grid");
    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(db.sql(ITEMS_DUMP), "1:grid:1|2:b:2|3:c:3");
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn a_database_other_than_the_open_transactions_is_refused(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let set_database = |rig: &mut SqlRig<'_>, database: &str| {
        let database = database.to_owned();
        rig.app.update(rig.visual, |this, _| this.workspace.tab_mut(a).unwrap().database = database);
    };
    let session_database = |rig: &SqlRig<'_>| {
        rig.app.read_with(rig.visual, |this, _| this.driver_runtime.tab_sessions.get(&a).map(|own| own.database.clone()))
    };
    rig.click("tx-mode-manual");
    // The session opens on the tab's database "" and its first transaction is on it.
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(session_database(&rig).as_deref(), Some(""));
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    // The next transaction opens on "main": that is the database it lives on, whatever the session opened with.
    set_database(&mut rig, "main");
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(session_database(&rig).as_deref(), Some("main"));
    assert_eq!(rig.pending(a), 1);
    // "" is the connection's default database, which SQLite calls "main": the same one.
    set_database(&mut rig, "");
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.pending(a), 2);
    // Another database while it is open: refused, with the reason; nothing is sent.
    set_database(&mut rig, "other");
    rig.run_on(a, COUNT_ITEMS);
    assert!(
        matches!(rig.status(), crate::query::QueryStatus::Failed(message) if message.contains("open on database \"main\"")),
        "{:?}",
        rig.status()
    );
    assert_eq!(rig.pending(a), 2, "the refused statement did not count");
    // After Commit any database is fine again.
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    rig.run_on(a, COUNT_ITEMS);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }));
}

#[gpui::test]
fn a_run_is_refused_while_commit_or_rollback_is_on_its_way(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('ending')");
    rig.app.update(rig.visual, |this, cx| {
        this.transactions.set_ending(a, true);
        cx.notify();
    });
    settle(rig.visual);
    rig.run_on(a, "INSERT INTO items(name) VALUES ('during')");
    assert!(
        matches!(rig.status(), crate::query::QueryStatus::Failed(message) if message.contains("still running")),
        "{:?}",
        rig.status()
    );
    assert_eq!(rig.pending(a), 1, "nothing was sent in the middle of the Commit");
    // The toolbar is busy too: neither button ends it a second time, and the mode cannot change.
    assert!(rig.app.read_with(rig.visual, |this, _| this.end_blocked(a)).is_some());
    rig.click("tx-mode-auto");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.app.update(rig.visual, |this, _| this.transactions.set_ending(a, false));
    rig.run_on(a, "INSERT INTO items(name) VALUES ('after')");
    assert_eq!(rig.pending(a), 2);
}

/// A Commit or Roll Back asked for while the same one is already on its way (a second click or shortcut, the
/// pending list, the close question, the idle timer) is answered by the one on its way: no message, no second
/// round trip. Another one is not, and says which is running.
#[gpui::test]
fn a_commit_or_rollback_asked_for_while_the_same_one_is_on_its_way_is_absorbed_silently(cx: &mut TestAppContext) {
    use super::transaction::TxEnd;
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('once')");
    let notifications = |rig: &mut SqlRig<'_>| rig.visual.update(|window, cx| window.notifications(cx).len());
    let before = notifications(&mut rig);
    let (waited, refused) = (std::rc::Rc::new(std::cell::Cell::new(false)), std::rc::Rc::new(std::cell::Cell::new(false)));
    let (waited_flag, refused_flag) = (waited.clone(), refused.clone());

    rig.app.update_in(rig.visual, |this, window, cx| {
        // A Commit is on its way (a real one, on the tab's session) ...
        let job = this.spawn_end(a, TxEnd::Commit, cx).expect("on its way");
        // ... the same one asked for again through the shortcut / palette path: absorbed ...
        this.end_tab_transaction(a, TxEnd::Commit, window, cx);
        // ... and through the lists and the questions, which wait for it instead of failing ...
        this.end_transactions(vec![a], TxEnd::Commit, Some(Box::new(move |_, _, _| waited_flag.set(true))), cx);
        // ... the other end is not what was asked for: it says which one is running, and does not run.
        this.end_tab_transaction(a, TxEnd::Rollback, window, cx);
        this.end_transactions(vec![a], TxEnd::Rollback, Some(Box::new(move |_, _, _| refused_flag.set(true))), cx);
        finish_end_when_done(a, TxEnd::Commit, job, cx);
    });
    rig.wait_tx("the Commit", a, |tx| tx.statements == 0);
    let flag = waited.clone();
    wait_until(rig.visual, "what waited for the Commit to go on", |_| flag.get());
    for _ in 0..5 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'once'"), "1", "committed once, not rolled back");
    // Its own "Committed" note, and the refusal of the Roll Back click; nothing for the absorbed requests.
    assert_eq!(notifications(&mut rig), before + 2, "{:?}", rig.log_messages());
    assert_eq!(rig.logged_times("was not run"), 1, "only the Roll Back list, which was refused: {:?}", rig.log_messages());
    assert!(!refused.get(), "what waited for the Roll Back did not go on");
    assert!(rig.log_messages().iter().all(|message| !message.contains("Wait for the running statement")), "{:?}", rig.log_messages());
}

/// A real Commit / Roll Back started with `spawn_end` is finished the way the end flow finishes it, once the
/// requests that arrive while it is on its way have been made.
fn finish_end_when_done(
    tab: u64,
    end: super::transaction::TxEnd,
    job: gpui::Task<Result<super::transaction::EndResult, DriverQueryError>>,
    cx: &mut Context<AppRoot>,
) {
    cx.spawn(async move |this, cx| {
        let result = job.await;
        let _ = this.update(cx, |this, cx| this.finish_end(tab, end, result, None, cx));
    })
    .detach();
}

/// What waited for a Commit on its way goes on only when that Commit ended the transaction well. The server
/// refuses this one at COMMIT (a deferred foreign key): the transaction is over, but the continuation of the
/// close question (here the disconnect) must not run as if it were committed - the user reads the error first.
#[gpui::test]
fn what_waited_for_a_commit_that_failed_does_not_go_on(cx: &mut TestAppContext) {
    use super::transaction::TxEnd;
    let db = SqliteDb::new(FK_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, "PRAGMA foreign_keys = ON");
    rig.run_on(a, "INSERT INTO late VALUES (99)");
    assert_eq!(rig.pending(a), 1);

    // The user clicks Commit; while it is on its way they disconnect, and answer the question with Commit.
    let job = std::rc::Rc::new(std::cell::RefCell::new(None));
    let started = job.clone();
    rig.app.update(rig.visual, |this, cx| *started.borrow_mut() = this.spawn_end(a, TxEnd::Commit, cx));
    assert!(job.borrow().is_some());
    rig.app.update_in(rig.visual, |this, window, cx| this.request_disconnect(&key, window, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-commit"), "the open transaction is asked about");
    rig.click("tx-guard-commit");
    for _ in 0..5 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    // The Commit on its way is refused by the server.
    let finishing = job.borrow_mut().take().expect("started");
    rig.app.update(rig.visual, |_, cx| finish_end_when_done(a, TxEnd::Commit, finishing, cx));
    rig.wait_tx("the failed Commit", a, |tx| tx.statements == 0);
    for _ in 0..10 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert_eq!(db.sql("SELECT count(*) FROM late"), "0", "refused");
    assert!(rig.logged_times("Commit failed") >= 1, "the error is shown: {:?}", rig.log_messages());
    assert!(
        rig.app.read_with(rig.visual, |this, _| this.driver_runtime.sessions.contains_key(&key)),
        "the connection was not closed as if the Commit had gone through"
    );
}

#[gpui::test]
fn a_cancelled_statement_keeps_commit_and_rollback_waiting_until_its_worker_is_done(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.start_on(a, LONG_QUERY);
    // The session opens, the app's BEGIN runs, then the long statement.
    let app = rig.app.clone();
    wait_until(rig.visual, "the long statement to be on the session", |visual| {
        app.read_with(visual, |this, _| {
            this.driver_runtime.query_operations.contains_key(&a) && this.driver_runtime.tab_sessions.contains_key(&a)
        })
    });
    rig.app.update(rig.visual, |this, cx| {
        this.cancel_tab_query(a, cx);
        cx.notify();
    });
    // The tab is free again to the app, but the worker has not stopped: Commit / Rollback wait.
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(a).unwrap().query.can_run()));
    let blocked = rig.end_blocked(a);
    assert_eq!(blocked, Some(super::transaction::EndBlocked::Stopping));
    let refused = rig.app.update(rig.visual, |this, _| {
        this.route_tab_statement(&SqlRun {
            tab_id: a,
            kind: RunKind::Sql,
            connection: this.workspace.tab(a).unwrap().connection_id.clone(),
            sql: "SELECT 1".into(),
            database: String::new(),
            collection: String::new(),
            limit: 10,
            offset: 0,
            confirmed: false,
            read_only: false,
        })
    });
    assert!(refused.is_err_and(|message| message.contains("still stopping")), "statements wait too");
    rig.wait_driver_idle();
    let app = rig.app.clone();
    wait_until(rig.visual, "the worker to finish", |visual| {
        app.read_with(visual, |this, _| this.end_blocked(a).is_none())
    });
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
}

#[gpui::test]
fn the_shared_session_being_released_does_not_cancel_a_manual_tabs_statement(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.start_on(a, LONG_QUERY);
    let app = rig.app.clone();
    wait_until(rig.visual, "the long statement to be on the session", |visual| {
        app.read_with(visual, |this, _| this.driver_runtime.query_operations.contains_key(&a))
    });
    // The connection's shared session is released (it dropped; a reconnect does the same).
    let released = key.clone();
    rig.app.update(rig.visual, |this, cx| this.release_dead_session(&released, cx));
    settle(rig.visual);
    assert!(
        matches!(rig.status_of(a), crate::query::QueryStatus::Running { .. }),
        "the tab's own statement runs on: {:?}",
        rig.status_of(a)
    );
    assert!(rig.own_session(a).is_some(), "and its session is kept");
    assert!(rig.app.read_with(rig.visual, |this, _| !this.driver_runtime.sessions.contains_key(&key)), "while the shared one is gone");
    rig.app.update(rig.visual, |this, cx| this.cancel_tab_query(a, cx));
    rig.wait_driver_idle();
}

#[gpui::test]
fn cancelling_a_connect_or_clearing_a_failed_one_keeps_the_tabs_own_sessions(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('kept')");
    // The shared connection fails; the user cancels / clears it.
    let lost = key.clone();
    rig.app.update(rig.visual, |this, _| assert!(this.sessions.mark_lost_because(&lost, "gone".into())));
    let cancelled = key.clone();
    rig.app.update(rig.visual, |this, cx| this.cancel_pending_connect(&cancelled, cx));
    settle(rig.visual);
    assert!(rig.own_session(a).is_some(), "the tab's session is not the connection's");
    assert_eq!(rig.pending(a), 1);
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'kept'"), "1");
}

#[gpui::test]
fn deleting_a_connection_with_an_open_transaction_asks_first(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('delete')");
    let id = key.clone();
    rig.app.update_in(rig.visual, |this, window, cx| this.delete_connection_with_confirmation(id.clone(), window, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "the transaction is asked about before the delete");
    rig.click("tx-guard-cancel");
    assert_eq!(rig.pending(a), 1);
    // Past the question nothing deletes it under an open transaction either.
    let result = rig.app.update(rig.visual, |this, cx| this.delete_connection(&key, cx));
    assert!(result.is_err_and(|message| message.contains("open transaction")));
    assert!(rig.app.read_with(rig.visual, |this, _| this.saved_connections.iter().any(|saved| saved.matches_id(&key))));
    assert!(rig.app.read_with(rig.visual, |this, _| this.open_transactions_on(std::slice::from_ref(&key))));
}

#[gpui::test]
fn reconnecting_stale_sessions_and_removing_a_driver_ask_about_open_transactions(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('driver')");
    // Removing the driver closes every session on it.
    rig.app.update(rig.visual, |this, cx| {
        this.perform_driver_step(super::driver_manager::DriverStep::Remove { id: "sqlite".into(), remove: true, disconnect: true }, Vec::new(), cx)
    });
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "asked before the sessions are closed");
    rig.click("tx-guard-cancel");
    assert!(rig.shared_untouched() && rig.own_session(a).is_some(), "Cancel closed nothing");
    assert_eq!(rig.pending(a), 1);

    // A newer generation of the driver makes the connection's session stale; reconnecting asks too.
    let dylib = built_driver("dbengine-sqlite", "");
    let copy = std::env::temp_dir().join(format!("tablex-driver-copy-{}{}", uuid::Uuid::new_v4(), std::env::consts::DLL_SUFFIX));
    std::fs::copy(&dylib, &copy).unwrap();
    let newer = engine_driver_host::DriverLoader.load_verified(&copy, &driver_sha256(&copy)).expect("a second generation loads");
    rig.app.update(rig.visual, |this, _| {
        this.driver_ui.handles.insert("sqlite".into(), newer);
    });
    let stale = rig.app.read_with(rig.visual, |this, _| this.driver_sessions("sqlite").iter().filter(|session| session.stale).count());
    assert_eq!(stale, 1, "the connection's session runs on the older generation");
    rig.app.update(rig.visual, |this, cx| this.reconnect_stale_sessions("sqlite", cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "asked before reconnecting");
    rig.click("tx-guard-cancel");
    assert!(rig.shared_untouched() && rig.own_session(a).is_some());
    let _ = std::fs::remove_file(&copy);
}

#[gpui::test]
fn closing_the_window_asks_about_transactions_then_about_drafts(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    app.update(visual, |this, _| this.transactions.set_for_capture(tab_id, open_tx_state()));
    assert!(!visual.simulate_close(), "both are open: the window is held");
    settle(visual);
    assert!(visual.debug_bounds("tx-guard-rollback").is_some(), "the transaction is asked about first");
    assert!(!pe_discard_dialog_open(visual));
    // Roll Back ends it; the close check runs again, so the draft is asked about next.
    diagram_click(visual, "tx-guard-rollback");
    settle(visual);
    assert!(pe_discard_dialog_open(visual), "then the structure draft: it was not skipped");
    assert_eq!(visual.cx.update(|cx| cx.windows().len()), 1, "the window is still open");
    diagram_click(visual, "discard-pending-cancel");
    assert_eq!(visual.cx.update(|cx| cx.windows().len()), 1, "Cancel keeps the window");
    assert!(!visual.simulate_close());
    settle(visual);
    let discard = visual.debug_bounds("discard-pending-confirm").expect("the Discard button");
    visual.simulate_click(discard.center(), Default::default());
    visual.cx.run_until_parked();
    assert_eq!(visual.cx.update(|cx| cx.windows().len()), 0, "Discard closes the window");
}

#[gpui::test]
fn a_statement_held_back_for_confirmation_is_not_counted_until_it_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(&format!("{ITEMS_SEED} CREATE TABLE victim(a INTEGER);"));
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "DROP TABLE victim");
    assert!(rig.rendered_now("sql-confirm-run"), "the driver asks first");
    assert_eq!(rig.pending(a), 0, "it has not run: nothing to commit or roll back");
    // Run anyway: the statement runs in the tab's transaction.
    rig.click("sql-confirm-run");
    rig.wait_idle(a);
    rig.wait_tx("the transaction", a, |tx| tx.is_open());
    assert_eq!(rig.pending(a), 1);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'victim'"), "1", "not committed yet");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'victim'"), "1", "rolled back: the table is still there");
}

#[gpui::test]
fn rollback_to_a_savepoint_keeps_the_earlier_work_and_the_transaction(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('before')");
    rig.run_on(a, "SAVEPOINT s1");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('after')");
    rig.run_on(a, "ROLLBACK TO s1");
    assert_eq!(rig.pending(a), 4, "ROLLBACK TO does not end the transaction");
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT group_concat(name) FROM items WHERE name IN ('before','after')"), "before");
}

#[gpui::test]
fn a_second_session_spec_reads_no_credentials_on_the_ui_thread(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let key = rig.key.clone();
    let (without, with) = rig.app.update(rig.visual, |this, _| {
        let saved = this.saved_connections.iter().find(|saved| saved.matches_id(&key)).cloned().unwrap();
        let without = this.secondary_session_spec(&saved, &key).map(|spec| spec.uri.is_none()).map_err(|error| error.to_string());
        this.driver_runtime.session_uris.insert(key.clone(), engine_connect::records::RuntimeUri::new("sqlite:///typed.db"));
        let with = this.secondary_session_spec(&saved, &key).map(|spec| spec.uri.is_some()).map_err(|error| error.to_string());
        (without, with)
    });
    assert_eq!(without, Ok(true), "the saved connection's URI (and its password) is resolved when the session opens");
    assert_eq!(with, Ok(true), "the live session's own URI is used as it is");
}

#[gpui::test]
fn the_toolbar_says_the_assistant_and_mcp_do_not_see_uncommitted_changes(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "SELECT 1");
    rig.app.update(rig.visual, |this, _| this.transactions.set_for_capture(tab, open_tx_state()));
    rig.app.update(rig.visual, |_, cx| cx.notify());
    settle(rig.visual);
    assert!(rig.rendered("tx-status"));
    assert!(super::transaction::transaction_tooltips().iter().all(|text| text.contains("MCP")), "each explanation names MCP clients");
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) --------------------

/// Tab A (manual) inserts; tab B (auto) on the same connection and the server itself see the old
/// value until Commit; Rollback discards; a typed BEGIN does not leak into the other tab.
fn live_isolation(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let a = rig.tab;
    let b = rig.second_tab();
    let table = format!("tablex_tx_{}", uuid::Uuid::new_v4().to_simple());
    rig.run_on(a, &format!("CREATE TABLE {table}(id int PRIMARY KEY, name varchar(40))"));
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "create: {:?}", rig.status());
    rig.run_on(a, &format!("INSERT INTO {table} VALUES (1, 'one')"));
    let count = format!("SELECT count(*) FROM {table}");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(rig.own_sessions(), 0);

        // Manual: the insert is private to A.
        rig.activate(a);
        rig.click("tx-mode-manual");
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (2, 'two')"));
        assert!(rig.own_session(a).is_some());
        assert!(rig.shared_untouched());
        rig.run_on(a, &count);
        assert_eq!(rig.tab_cell(a), "2", "A sees its own row");
        rig.run_on(b, &count);
        assert_eq!(rig.tab_cell(b), "1", "B (auto-commit) does not see the uncommitted row");
        rig.activate(a);
        rig.click("tx-commit");
        rig.wait_tx("the commit", a, |tx| tx.statements == 0);
        rig.run_on(b, &count);
        assert_eq!(rig.tab_cell(b), "2", "after Commit B sees it");

        // Rollback discards.
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (3, 'three')"));
        rig.run_on(a, &format!("UPDATE {table} SET name = 'changed' WHERE id = 1"));
        assert_eq!(rig.pending(a), 2);
        rig.run_on(b, &format!("SELECT name FROM {table} WHERE id = 1"));
        assert_eq!(rig.tab_cell(b), "one");
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        rig.run_on(b, &count);
        assert_eq!(rig.tab_cell(b), "2", "Rollback discarded the third row");
        rig.run_on(b, &format!("SELECT name FROM {table} WHERE id = 1"));
        assert_eq!(rig.tab_cell(b), "one");

        // Back to auto on A, then a typed BEGIN in an auto tab does not leak into B.
        rig.activate(a);
        rig.click("tx-mode-auto");
        assert_eq!(rig.own_sessions(), 0);
        let begin = match kind {
            ConnectionKind::Postgres => "BEGIN",
            _ => "START TRANSACTION",
        };
        // A BEGIN an auto-commit run leaves open is rolled back when the run ends: the DELETE is gone
        // again, and the shared session is back in autocommit for everybody.
        rig.run_on(a, &format!("{begin}; DELETE FROM {table} WHERE id = 2"));
        assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 0));
        rig.eventually(b, &count, "2");
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (9, 'nine')"));
        rig.eventually(b, &count, "3");
        rig.run_on(a, &format!("DELETE FROM {table} WHERE id = 9"));
    }));
    // The table goes away whatever happened.
    rig.activate(a);
    if rig.tx(a).is_some_and(|tx| tx.is_open()) {
        rig.run_on(a, "ROLLBACK");
    }
    rig.app.update(rig.visual, |this, _| this.tab_transaction_closed(a));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_manual_transactions_are_private_to_their_tab(cx: &mut TestAppContext) {
    live_isolation(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_manual_transactions_are_private_to_their_tab(cx: &mut TestAppContext) {
    live_isolation(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_manual_transactions_are_private_to_their_tab(cx: &mut TestAppContext) {
    live_isolation(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// MySQL / MariaDB: the tab's session runs with `autocommit = 0`, so every statement is inside a
/// transaction the server owns. A DDL statement commits by itself and the INSERT after it starts a
/// new transaction that Roll Back really undoes; the app only counts statements, and only a Commit,
/// a Roll Back or a typed COMMIT / ROLLBACK resets the count.
fn live_autocommit_off(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    const TABLE: &str = "tablex_tx_implicit";
    const DDL_TABLE: &str = "tablex_tx_implicit_ddl";
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let (_drop, _drop_ddl) = (DropScratchTable(rig.session.clone(), TABLE), DropScratchTable(rig.session.clone(), DDL_TABLE));
    for table in [TABLE, DDL_TABLE] {
        exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    }
    exec_confirmed(&rig.session, &format!("CREATE TABLE {TABLE}(id int PRIMARY KEY) ENGINE=InnoDB"));
    let a = rig.tab;
    let b = rig.second_tab();
    let count = format!("SELECT count(*) FROM {TABLE}");
    rig.activate(a);
    rig.click("tx-mode-manual");

    // The tab's session does not autocommit; nothing was injected before the statement.
    rig.run_on(a, "SELECT @@autocommit");
    assert_eq!(rig.tab_cell(a), "0", "the session of a manual tab runs with autocommit off");
    assert_eq!(rig.pending(a), 1);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);

    // DDL, then an INSERT, then Roll Back: the DDL committed by itself, the INSERT is undone.
    rig.run_on(a, &format!("CREATE TABLE {DDL_TABLE}(a int)"));
    rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES (1)"));
    assert_eq!(rig.pending(a), 1, "the DDL committed by itself: only the INSERT after it is pending");
    rig.run_on(b, &count);
    assert_eq!(rig.tab_cell(b), "0", "not committed yet");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    rig.run_on(b, &count);
    assert_eq!(rig.tab_cell(b), "0", "Roll Back undid the INSERT");
    rig.run_on(b, &format!("SELECT count(*) FROM information_schema.tables WHERE table_name = '{DDL_TABLE}'"));
    assert_eq!(rig.tab_cell(b), "1", "the DDL had committed by itself");

    // A typed COMMIT commits and resets the count; the tab stays on its session.
    rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES (2)"));
    rig.run_on(a, "COMMIT");
    assert_eq!(rig.pending(a), 0);
    rig.run_on(b, &count);
    assert_eq!(rig.tab_cell(b), "1");
    assert!(rig.own_session(a).is_some());

    // `SET autocommit` is refused in a manual tab (the app relies on the session's autocommit being
    // off); a typed START TRANSACTION commits what was pending, so the counters start again.
    rig.activate(a);
    rig.run_on(a, "SET autocommit = 1");
    assert!(
        matches!(rig.status(), crate::query::QueryStatus::Failed(message) if message.contains("Switch the tab to Auto-commit instead")),
        "{:?}",
        rig.status()
    );
    rig.run_on(a, "SELECT @@autocommit");
    assert_eq!(rig.tab_cell(a), "0", "the session still runs with autocommit off");
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES (5)"));
    assert_eq!((rig.pending(a), rig.modifying(a)), (1, 1));
    rig.run_on(a, "START TRANSACTION");
    assert_eq!((rig.pending(a), rig.modifying(a)), (1, 0), "START TRANSACTION committed the INSERT: nothing is pending but the statement itself");
    rig.run_on(b, &format!("SELECT count(*) FROM {TABLE} WHERE id = 5"));
    assert_eq!(rig.tab_cell(b), "1");
    rig.activate(a);
    // (The error toast of the refused SET sits over the Roll Back button for a few seconds: the
    // command is run as the shortcut would.)
    rig.app.update_in(rig.visual, |this, window, cx| this.end_active_transaction(super::transaction::TxEnd::Rollback, window, cx));
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);

    // An auto-commit tab that turns autocommit off on the shared session: the run ends it, with a
    // ROLLBACK of what it did and autocommit on again.
    rig.run_on(b, &format!("SET autocommit = 0; INSERT INTO {TABLE} VALUES (3)"));
    assert_eq!((rig.mode(b), rig.own_sessions()), (CommitMode::Auto, 1), "b stayed on the shared session (a has the only own one)");
    rig.eventually(b, &format!("SELECT count(*) FROM {TABLE} WHERE id = 3"), "0");
    rig.eventually(b, "SELECT @@autocommit", "1");
    // And a START TRANSACTION left open the same way.
    rig.run_on(b, &format!("START TRANSACTION; INSERT INTO {TABLE} VALUES (4)"));
    rig.eventually(b, &format!("SELECT count(*) FROM {TABLE} WHERE id = 4"), "0");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_manual_session_runs_with_autocommit_off(cx: &mut TestAppContext) {
    live_autocommit_off(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_manual_session_runs_with_autocommit_off(cx: &mut TestAppContext) {
    live_autocommit_off(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// PostgreSQL DDL is transactional: a CREATE TABLE in the transaction is undone by Roll Back and the
/// transaction stays open after it.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_ddl_stays_inside_the_manual_transaction(cx: &mut TestAppContext) {
    const DDL_TABLE: &str = "tablex_tx_pg_ddl";
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _drop = DropScratchTable(rig.session.clone(), DDL_TABLE);
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {DDL_TABLE}"));
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, &format!("CREATE TABLE {DDL_TABLE}(a int)"));
    assert_eq!(rig.pending(a), 1, "still open: PostgreSQL did not commit");
    rig.run_on(b, &format!("SELECT count(*) FROM pg_tables WHERE tablename = '{DDL_TABLE}'"));
    assert_eq!(rig.tab_cell(b), "0", "private to the tab");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    rig.run_on(b, &format!("SELECT count(*) FROM pg_tables WHERE tablename = '{DDL_TABLE}'"));
    assert_eq!(rig.tab_cell(b), "0", "rolled back with the transaction");
}

/// A COMMIT the server refuses (a deferred foreign key) rolled the transaction back: the error is
/// shown, the count is reset, and the tab goes on with a new transaction. Then the failed-statement
/// path: the hint appears, `ROLLBACK TO` a savepoint clears it, and a typed COMMIT commits what was
/// done before the failure.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_a_refused_commit_resets_the_count_and_a_failed_statement_rolls_back_to_a_savepoint(cx: &mut TestAppContext) {
    const PARENT: &str = "tablex_tx_parent";
    const CHILD: &str = "tablex_tx_child";
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    // Locals drop in reverse order: the child (which references the parent) goes first.
    let (_parent, _child) = (DropScratchTable(rig.session.clone(), PARENT), DropScratchTable(rig.session.clone(), CHILD));
    for table in [CHILD, PARENT] {
        exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    }
    exec_confirmed(&rig.session, &format!("CREATE TABLE {PARENT}(id int PRIMARY KEY)"));
    exec_confirmed(
        &rig.session,
        &format!("CREATE TABLE {CHILD}(id int PRIMARY KEY, p int REFERENCES {PARENT}(id) DEFERRABLE INITIALLY DEFERRED)"),
    );
    let a = rig.tab;
    let b = rig.second_tab();
    let count = format!("SELECT count(*) FROM {CHILD}");
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, &format!("INSERT INTO {PARENT} VALUES (1)"));
    rig.run_on(a, &format!("INSERT INTO {CHILD} VALUES (1, 999)"));
    assert_eq!(rig.pending(a), 2, "the deferred constraint does not fail the statement");
    rig.click("tx-commit");
    rig.wait_tx("the refused commit", a, |tx| tx.statements == 0);
    assert!(rig.log().iter().any(|(_, severity, _)| *severity == crate::error_log::Severity::Error), "the error is shown in the Error Log");
    rig.run_on(b, &count);
    assert_eq!(rig.tab_cell(b), "0", "the server rolled it back");

    // The retry goes through (the parent row was rolled back too).
    rig.run_on(a, &format!("INSERT INTO {PARENT} VALUES (1)"));
    rig.run_on(a, &format!("INSERT INTO {CHILD} VALUES (2, 1)"));
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    rig.run_on(b, &count);
    assert_eq!(rig.tab_cell(b), "1", "the retried transaction committed");

    // A failed statement: the hint; ROLLBACK TO a savepoint clears it; COMMIT keeps the earlier work.
    rig.run_on(a, &format!("INSERT INTO {CHILD} VALUES (3, 1)"));
    rig.run_on(a, "SAVEPOINT s1");
    rig.run_on(a, "SELECT 1/0");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));
    assert!(rig.tx(a).unwrap().aborted_hint);
    rig.activate(a);
    assert!(rig.rendered_now("tx-hint"), "PostgreSQL: the transaction is aborted until you roll back");
    assert_eq!(rig.pending(a), 3, "the failed statement is counted");
    rig.run_on(a, "ROLLBACK TO SAVEPOINT s1");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "{:?}", rig.status());
    assert!(!rig.tx(a).unwrap().aborted_hint, "cleared by the statement that ran");
    rig.run_on(a, "COMMIT");
    assert_eq!(rig.pending(a), 0, "a typed COMMIT resets the count");
    rig.run_on(b, &count);
    assert_eq!(rig.tab_cell(b), "2", "the row before the failure was committed");

    // A failure and a typed ROLLBACK: nothing is kept, the hint goes with the transaction.
    rig.run_on(a, &format!("INSERT INTO {CHILD} VALUES (4, 1)"));
    rig.run_on(a, "SELECT 1/0");
    assert!(rig.tx(a).unwrap().aborted_hint);
    rig.run_on(a, "ROLLBACK");
    assert_eq!((rig.pending(a), rig.tx(a).unwrap().aborted_hint), (0, false));
    rig.run_on(b, &count);
    assert_eq!(rig.tab_cell(b), "2");
}

/// A grid Apply on a table-data tab is the driver's own transaction: it commits at once, and does not
/// join (or disturb) the transaction a SQL tab of the same connection has open.
fn live_table_apply_beside_a_manual_tab(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
    schema: Option<&str>,
) {
    const TABLE: &str = "tablex_tx_grid";
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let _drop = DropScratchTable(rig.session.clone(), TABLE);
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {TABLE}"));
    exec_confirmed(&rig.session, &format!("CREATE TABLE {TABLE}(id int PRIMARY KEY, name varchar(40), qty int)"));
    exec_confirmed(&rig.session, &format!("INSERT INTO {TABLE} VALUES (1, 'a', 1), (2, 'b', 2)"));
    let sql_tab = rig.tab;
    let table_tab = rig.second_tab();
    // The SQL tab holds a transaction open on another row.
    rig.activate(sql_tab);
    rig.click("tx-mode-manual");
    rig.run_on(sql_tab, &format!("INSERT INTO {TABLE} VALUES (3, 'c', 3)"));
    // The other tab becomes the table-data tab and applies an edit.
    rig.activate(table_tab);
    rig.tab = table_tab;
    rig.open_table_in("tablex", schema, TABLE);
    rig.wait_structure();
    rig.rerun_table("id <= 2", "id");
    rig.edit_cell(0, 1, "edited");
    rig.apply();
    rig.wait_apply_finished();
    rig.run_on(sql_tab, &format!("SELECT name FROM {TABLE} WHERE id = 1"));
    assert_eq!(rig.tab_cell(sql_tab), "edited", "the Apply committed by itself");
    assert_eq!(rig.pending(sql_tab), 2, "the SQL tab's transaction is its own, still open");
    assert!(rig.tx(table_tab).is_none(), "the table tab has no transaction");
    rig.activate(sql_tab);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", sql_tab, |tx| tx.statements == 0);
    rig.run_on(sql_tab, &format!("SELECT count(*) FROM {TABLE}"));
    assert_eq!(rig.tab_cell(sql_tab), "2", "Roll Back undid the SQL tab's row and not the Apply");
    // The read above opened a transaction again: end it, or the table cannot be dropped.
    rig.activate(sql_tab);
    rig.click("tx-rollback");
    rig.wait_tx("the end", sql_tab, |tx| tx.statements == 0);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_a_table_apply_commits_beside_a_manual_sql_tab(cx: &mut TestAppContext) {
    live_table_apply_beside_a_manual_tab(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI", Some("public"));
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_table_apply_commits_beside_a_manual_sql_tab(cx: &mut TestAppContext) {
    live_table_apply_beside_a_manual_tab(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI", None);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_a_table_apply_commits_beside_a_manual_sql_tab(cx: &mut TestAppContext) {
    live_table_apply_beside_a_manual_tab(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI", None);
}

/// The structure apply, through the app's own executor (`structure_executor` / `execute`): it opens a
/// session of its own, so a transaction a manual SQL tab has open is neither committed nor rolled
/// back by it.
fn live_structure_apply_beside_a_transaction(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
    namespace: &str,
) {
    use crate::app::structure_edit::apply::{execute, ApplyControl, ApplyJob, Executor};
    use crate::sql_ddl::{dialect_for, plan, Change, ColumnSpec, TableId, DEFAULT_LOCK_TIMEOUT_SECS};
    const OPEN: &str = "tablex_tx_ddl_open";
    const MADE: &str = "tablex_tx_ddl_made";
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let (_drop_open, _drop_made) = (DropScratchTable(rig.session.clone(), OPEN), DropScratchTable(rig.session.clone(), MADE));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {OPEN}"));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {MADE}"));
    exec_confirmed(&rig.session, &format!("CREATE TABLE {OPEN}(id int PRIMARY KEY) ENGINE=InnoDB").replace(" ENGINE=InnoDB", if kind == ConnectionKind::Postgres { "" } else { " ENGINE=InnoDB" }));
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, &format!("INSERT INTO {OPEN} VALUES (1)"));
    assert_eq!(rig.pending(a), 1);
    let tab_session = rig.own_session(a).expect("the tab's own session");

    // The apply, as the review dialog runs it: the executor of the connection, a job, its own session.
    let key = rig.key.clone();
    let executor = rig.app.read_with(rig.visual, |this, _| {
        let saved = this.saved_connections.iter().find(|saved| saved.matches_id(&key)).cloned().unwrap();
        this.structure_executor(&saved, &key)
    });
    let executor = executor.unwrap_or_else(|message| panic!("{message}"));
    assert!(matches!(executor, Executor::Driver(_)), "the real driver executor, not a test double");
    let postgres = kind == ConnectionKind::Postgres;
    let scope = super::structure_edit::EditScope {
        kind,
        database: "tablex".into(),
        schema: postgres.then(|| namespace.to_owned()),
    };
    let changes = vec![Change::CreateTable {
        table: TableId { schema: Some(namespace.to_owned()), name: MADE.into() },
        columns: vec![ColumnSpec { name: "id".into(), data_type: "int".into(), nullable: false, ..Default::default() }],
        primary_key: vec!["id".into()],
        comment: None,
    }];
    let job = ApplyJob {
        kind,
        scope,
        reviewed_script: plan(dialect_for(kind).unwrap(), &changes).script(),
        changes,
        lock_timeout_secs: DEFAULT_LOCK_TIMEOUT_SECS,
    };
    let database = (!postgres).then(|| "tablex".to_owned());
    let run = execute(executor, &job, database, std::sync::Arc::new(ApplyControl::default()), super::structure_edit::Wording::DIAGRAM)
        .unwrap_or_else(|refusal| panic!("{}", refusal.message));
    assert!(run.outcome.succeeded(), "{}", run.outcome.report(&run.planned.steps()));

    assert_eq!(rig.pending(a), 1, "the tab's transaction is still open");
    assert_eq!(rig.own_session(a), Some(tab_session), "on the same session");
    rig.run_on(b, &format!("SELECT count(*) FROM {MADE}"));
    assert_eq!(rig.tab_cell(b), "0", "the table the apply made is there");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    rig.run_on(b, &format!("SELECT count(*) FROM {OPEN}"));
    assert_eq!(rig.tab_cell(b), "0", "the rollback still discarded the tab's insert: the apply did not commit it");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_a_structure_apply_leaves_a_manual_transaction_alone(cx: &mut TestAppContext) {
    live_structure_apply_beside_a_transaction(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI", "public");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_structure_apply_leaves_a_manual_transaction_alone(cx: &mut TestAppContext) {
    live_structure_apply_beside_a_transaction(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI", "tablex");
}

/// PostgreSQL: `SET TRANSACTION ...`, a quoted `SET` and `LOCK TABLE` mean something only inside a
/// transaction. They get the BEGIN before them like every statement, so the writes after them are in
/// it too and Roll Back removes them.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_a_setting_in_a_manual_tab_does_not_leave_the_next_write_autocommitted(cx: &mut TestAppContext) {
    const TABLE: &str = "tablex_tx_setting";
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _drop = DropScratchTable(rig.session.clone(), TABLE);
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {TABLE}"));
    exec_confirmed(&rig.session, &format!("CREATE TABLE {TABLE}(id int PRIMARY KEY)"));
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    let lock = format!("LOCK TABLE {TABLE}");
    for (index, setting) in ["SET TRANSACTION ISOLATION LEVEL SERIALIZABLE", "SET \"application_name\" = 'x'", lock.as_str()]
        .into_iter()
        .enumerate()
    {
        rig.run_on(a, setting);
        assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "{setting}: {:?}", rig.status());
        assert!(rig.tx(a).unwrap().is_open(), "{setting}: the BEGIN before it opened the transaction it belongs to");
        rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES ({index})"));
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        rig.run_on(b, &format!("SELECT count(*) FROM {TABLE}"));
        assert_eq!(rig.tab_cell(b), "0", "{setting}: the row after it was in the transaction, and rolled back");
    }
}

// ---- the opening of a tab's session never leaks a connection ---------------------------------------

/// A generation of the SQLite driver of its own for the sessions the rig's tab opens (the loaded
/// driver is shared by every test of the process, so its session count is not this test's alone).
fn private_driver(rig: &mut SqlRig<'_>) -> std::sync::Arc<engine_driver_host::DriverHandle> {
    let dylib = built_driver("dbengine-sqlite", "");
    let copy = std::env::temp_dir().join(format!("tablex-driver-private-{}{}", uuid::Uuid::new_v4(), std::env::consts::DLL_SUFFIX));
    std::fs::copy(&dylib, &copy).unwrap();
    let private = engine_driver_host::DriverLoader.load_verified(&copy, &driver_sha256(&copy)).expect("a private generation loads");
    let (key, handle) = (rig.key.clone(), private.clone());
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.session_handles.insert(key, handle);
    });
    private
}

/// Waits until `handle` has `expected` connected sessions (a closed session is disconnected on a
/// worker thread, a moment after it was dropped).
fn wait_driver_sessions(rig: &mut SqlRig<'_>, handle: &std::sync::Arc<engine_driver_host::DriverHandle>, expected: usize, label: &'static str) {
    let handle = handle.clone();
    wait_until(rig.visual, label, |_| handle.active_session_count() == expected);
}

#[gpui::test]
fn a_cancel_while_the_session_opens_leaves_no_connection_behind(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let handle = private_driver(&mut rig);
    let base = handle.active_session_count();
    rig.click("tx-mode-manual");
    rig.start_on(a, COUNT_ITEMS);
    // The run is cancelled before the session is open: its task is dropped, the opening is not.
    rig.app.update(rig.visual, |this, cx| this.cancel_tab_query(a, cx));
    let app = rig.app.clone();
    wait_until(rig.visual, "the opening to end", |visual| app.read_with(visual, |this, _| this.driver_runtime.tab_sessions.contains_key(&a)));
    assert_eq!(handle.active_session_count(), base + 1, "registered: the tab is still manual and will use it");
    assert!(matches!(rig.status_of(a), crate::query::QueryStatus::Idle | crate::query::QueryStatus::Cancelled), "{:?}", rig.status_of(a));
    // Nothing runs from the cancelled run; closing the tab closes the session.
    rig.app.update(rig.visual, |this, cx| this.close_tab(a, cx));
    wait_driver_sessions(&mut rig, &handle, base, "the session to be disconnected");
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn a_tab_closed_while_its_session_opens_closes_the_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let _b = rig.second_tab();
    rig.activate(a);
    let handle = private_driver(&mut rig);
    let base = handle.active_session_count();
    rig.click("tx-mode-manual");
    rig.start_on(a, COUNT_ITEMS);
    rig.app.update(rig.visual, |this, cx| this.close_tab(a, cx));
    // The opening ends after the tab is gone: its session is disconnected, not registered, not dropped open.
    wait_driver_sessions(&mut rig, &handle, base, "the session of the closed tab to be disconnected");
    settle(rig.visual);
    assert_eq!(rig.own_sessions(), 0);
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(a).is_none()));
}

#[gpui::test]
fn a_disconnect_while_the_session_opens_closes_the_opening_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    let handle = private_driver(&mut rig);
    let base = handle.active_session_count();
    rig.click("tx-mode-manual");
    rig.start_on(a, COUNT_ITEMS);
    // The user disconnects (nothing is open yet, so nothing is asked) before the session is open.
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    wait_driver_sessions(&mut rig, &handle, base, "the opening session to be disconnected");
    settle(rig.visual);
    assert_eq!(rig.own_sessions(), 0, "it was not registered");
    assert!(rig.app.read_with(rig.visual, |this, _| !this.driver_runtime.sessions.contains_key(&key)));
}

#[gpui::test]
fn a_session_whose_setup_failed_is_disconnected(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let handle = private_driver(&mut rig);
    let base = handle.active_session_count();
    rig.click("tx-mode-manual");
    super::transaction::FAIL_SETUP.with(|fail| fail.set(true));
    rig.run_on(a, COUNT_ITEMS);
    super::transaction::FAIL_SETUP.with(|fail| fail.set(false));
    assert!(
        matches!(rig.status(), crate::query::QueryStatus::Failed(message) if message.contains("setup failed")),
        "{:?}",
        rig.status()
    );
    assert_eq!(rig.own_sessions(), 0);
    wait_driver_sessions(&mut rig, &handle, base, "the session whose setup failed to be disconnected");
    // A later run opens a good one.
    rig.run_on(a, COUNT_ITEMS);
    assert!(rig.own_session(a).is_some());
    assert_eq!(rig.tab_cell(a), "5");
}

// ---- smaller review fixes ----------------------------------------------------------------------------

#[gpui::test]
fn an_explain_on_a_tab_whose_commit_is_on_its_way_waits_too(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('x')");
    let explain = |rig: &mut SqlRig<'_>| {
        rig.app.update(rig.visual, |this, _| {
            this.route_tab_statement(&SqlRun {
                tab_id: a,
                kind: RunKind::Explain(ExplainRun::new(engine_plan::ExplainDialect::Sqlite, engine_plan::ExplainMode::Estimate)),
                connection: this.workspace.tab(a).unwrap().connection_id.clone(),
                sql: "EXPLAIN QUERY PLAN SELECT 1".into(),
                database: String::new(),
                collection: String::new(),
                limit: 10,
                offset: 0,
                confirmed: false,
                read_only: true,
            })
        })
    };
    assert_eq!(explain(&mut rig), Ok(super::transaction::TabRoute::Own), "on the tab's session, where its changes are");
    rig.app.update(rig.visual, |this, _| this.transactions.set_ending(a, true));
    assert!(explain(&mut rig).is_err_and(|message| message.contains("still running")));
    rig.app.update(rig.visual, |this, _| this.transactions.set_ending(a, false));
}

#[gpui::test]
fn removing_a_driver_asks_about_tabs_of_connections_whose_shared_session_is_gone(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('driver gone')");
    // The shared session is gone (dropped and released); the tab's own session is not.
    rig.app.update(rig.visual, |this, cx| this.release_dead_session(&key, cx));
    settle(rig.visual);
    assert!(rig.app.read_with(rig.visual, |this, _| !this.driver_runtime.sessions.contains_key(&key)));
    rig.app.update(rig.visual, |this, cx| {
        this.perform_driver_step(super::driver_manager::DriverStep::Remove { id: "sqlite".into(), remove: true, disconnect: true }, Vec::new(), cx)
    });
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "asked, though no shared session of the driver is live");
    rig.click("tx-guard-cancel");
    assert!(rig.own_session(a).is_some());
}

#[gpui::test]
fn a_commit_that_got_no_answer_says_it_is_unknown_whether_it_was_applied(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('unknown')");
    let session = rig.app.read_with(rig.visual, |this, _| this.driver_runtime.tab_sessions.get(&a).unwrap().session.clone());
    session.lock().unwrap().disconnect().unwrap();
    rig.click("tx-commit");
    rig.wait_tx("the failed commit", a, |tx| tx.lost.is_some());
    let tx = rig.tx(a).unwrap();
    let reason = tx.lost.as_deref().unwrap();
    assert!(reason.contains("unknown") && !reason.contains("rolled back"), "{reason}");
    assert_eq!(tx.statements, 0);
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn with_confirmation_off_a_held_back_statement_runs_on_in_its_transaction_without_a_warning(cx: &mut TestAppContext) {
    let db = SqliteDb::new(&format!("{ITEMS_SEED} CREATE TABLE victim(a INTEGER);"));
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_settings(|settings| settings.editor.confirm_dangerous = false);
    rig.click("tx-mode-manual");
    rig.run_on(a, "DROP TABLE victim");
    rig.wait_tx("the statement in its transaction", a, |tx| tx.is_open());
    settle(rig.visual);
    assert_eq!(rig.pending(a), 1, "the statement ran in the transaction the app began");
    assert!(rig.log().iter().all(|(_, severity, _)| *severity != crate::error_log::Severity::Warning), "no 'was not run' warning: {:?}", rig.log());
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'victim'"), "1");
}

#[gpui::test]
fn another_tabs_run_waits_for_the_cleanup_of_an_auto_commit_run_and_never_sees_its_transaction(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    rig.run_on(a, "BEGIN; INSERT INTO items(name) VALUES ('inside')");
    assert_eq!(rig.cleanups(), 1);
    // Straight away, on the same shared session: held until the ROLLBACK landed, so the row is not there.
    rig.run_on(b, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(b), "5", "the run did not see the open transaction");
}

#[gpui::test]
fn a_savepoint_an_auto_commit_run_opens_on_sqlite_is_ended_too_and_a_released_one_is_not_a_warning(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.run_on(a, "SAVEPOINT s; INSERT INTO items(name) VALUES ('inside')");
    assert_eq!(rig.cleanups(), 1);
    rig.eventually(a, COUNT_ITEMS, "5");
    // Released: that committed it; the ROLLBACK that follows finds nothing open, which is no news.
    rig.run_on(a, "SAVEPOINT s; INSERT INTO items(name) VALUES ('released'); RELEASE s");
    rig.eventually(a, COUNT_ITEMS, "6");
    assert!(rig.log().iter().all(|(_, severity, _)| *severity != crate::error_log::Severity::Warning), "{:?}", rig.log_messages());
}

#[gpui::test]
fn a_cancel_that_lands_on_a_begin_or_autocommit_off_is_ended_like_one_that_ran(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        this.transactions.set_auto_sent(a, Typed::Begin);
        this.tx_run_cancelled(a, cx);
    });
    assert_eq!(rig.cleanups(), 1, "the BEGIN may have run");
    rig.app.update(rig.visual, |this, cx| {
        this.transactions.set_auto_sent(a, Typed::Other);
        this.tx_run_cancelled(a, cx);
    });
    assert_eq!(rig.cleanups(), 1, "a statement that is not a transaction statement leaves nothing");
}

#[gpui::test]
fn a_manual_tab_refuses_a_typed_set_autocommit_on_mysql_and_mariadb(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb] {
        let tab = rig.tab(kind, "SELECT 1");
        rig.app.update(rig.visual, |this, _| this.transactions.set_for_capture(tab, TabTx::new(CommitMode::Manual)));
        for sql in ["SET autocommit = 1", "set @@autocommit=0", "SET SESSION autocommit = OFF"] {
            let refused = rig.app.update(rig.visual, |this, _| {
                this.route_tab_statement(&SqlRun {
                    tab_id: tab,
                    kind: RunKind::Sql,
                    connection: this.workspace.tab(tab).unwrap().connection_id.clone(),
                    sql: sql.into(),
                    database: String::new(),
                    collection: String::new(),
                    limit: 10,
                    offset: 0,
                    confirmed: false,
                    read_only: false,
                })
            });
            assert!(refused.is_err_and(|message| message.contains("Switch the tab to Auto-commit instead")), "{kind:?} {sql}");
        }
        let fine = rig.app.update(rig.visual, |this, _| {
            this.route_tab_statement(&SqlRun {
                tab_id: tab,
                kind: RunKind::Sql,
                connection: this.workspace.tab(tab).unwrap().connection_id.clone(),
                sql: "SET names utf8mb4".into(),
                database: String::new(),
                collection: String::new(),
                limit: 10,
                offset: 0,
                confirmed: false,
                read_only: false,
            })
        });
        assert!(fine.is_ok(), "{kind:?}: another SET is not refused");
    }
}

#[gpui::test]
fn a_cancelled_statement_counts_as_modifying_on_a_manual_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.start_on(a, LONG_QUERY);
    let app = rig.app.clone();
    wait_until(rig.visual, "the long statement to be on the session", |visual| {
        app.read_with(visual, |this, _| this.driver_runtime.query_operations.contains_key(&a))
    });
    assert_eq!((rig.pending(a), rig.modifying(a)), (1, 0), "a read, running");
    rig.app.update(rig.visual, |this, cx| this.cancel_tab_query(a, cx));
    assert_eq!((rig.pending(a), rig.modifying(a)), (1, 1), "cancelled: nobody knows how far it got");
    rig.wait_driver_idle();
    // Rolling it back asks nothing (it is the user's own click) and ends it.
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
}

#[gpui::test]
fn what_a_failed_request_loses_follows_the_session_it_ran_on_not_the_tabs_session_now(cx: &mut TestAppContext) {
    use super::transaction::{Reporting, TabRoute};
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('tunnel')");
    let own = rig.own_session(a).unwrap();
    let report = |rig: &mut SqlRig<'_>, route: TabRoute, own_id: Option<u64>, request_lost: bool, tunnel_down: bool| {
        rig.app.update(rig.visual, |this, _| {
            this.transactions.set_reporting(Some(Reporting { tab_id: a, route, own_id }));
            this.note_lost(a, request_lost, tunnel_down);
            this.transactions.set_reporting(None);
        });
    };
    let shared_failed = |rig: &SqlRig<'_>| rig.app.read_with(rig.visual, |this, _| matches!(this.sessions.status(&key), ConnectionStatus::Failed(_)));
    // A request on the shared session that says the connection is lost marks the shared connection,
    // and never the tab's own session, though the tab has one.
    report(&mut rig, TabRoute::Shared, None, true, false);
    assert!(shared_failed(&rig));
    assert_eq!((rig.own_session(a), rig.pending(a)), (Some(own), 1));
    let key2 = key.clone();
    rig.app.update(rig.visual, |this, _| {
        let epoch = this.sessions.begin_connect(&key2).unwrap();
        this.sessions.finish_connect(&key2, epoch, Ok(()));
    });
    assert!(!shared_failed(&rig));
    // A request on the tab's own session that failed on something else while the shared tunnel is
    // down: the shared session is marked, the tab's own session and its transaction are not.
    report(&mut rig, TabRoute::Own, Some(own), false, true);
    assert!(shared_failed(&rig));
    assert_eq!((rig.own_session(a), rig.pending(a)), (Some(own), 1));
    rig.app.update(rig.visual, |this, _| {
        let epoch = this.sessions.begin_connect(&key2).unwrap();
        this.sessions.finish_connect(&key2, epoch, Ok(()));
    });
    // A request on a session of its own that is gone (an old one, or a newer one has replaced it) is
    // old news: nothing is dropped, nothing is marked.
    report(&mut rig, TabRoute::Own, Some(own + 1000), true, false);
    assert!(!shared_failed(&rig));
    assert_eq!((rig.own_session(a), rig.pending(a)), (Some(own), 1));
    // The request on the tab's own session itself says it is lost: that session, with the message.
    report(&mut rig, TabRoute::Own, Some(own), true, false);
    assert!(!shared_failed(&rig), "the shared connection is not known to be lost");
    assert_eq!(rig.own_sessions(), 0);
    assert!(rig.tx(a).unwrap().lost.is_some());
    // Late: the same failure reported again with no session left is not counted anywhere.
    report(&mut rig, TabRoute::Own, Some(own), true, false);
    assert!(!shared_failed(&rig));
}

#[gpui::test]
fn a_held_run_cancelled_before_it_was_sent_is_not_an_open_transaction(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    let gate = rig.app.update(rig.visual, |this, _| this.transactions.hold(&key));
    rig.start_on(a, "BEGIN");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.transactions.auto_open_tabs()), 0, "held: not sent, not noted");
    rig.app.update(rig.visual, |this, cx| this.cancel_tab_query(a, cx));
    drop(gate);
    settle(rig.visual);
    assert_eq!(rig.cleanups(), 0, "nothing was open, nothing is ended");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.transactions.auto_open_tabs()), 0);
}

#[gpui::test]
fn releasing_the_shared_session_ends_the_hold_and_forgets_what_a_run_left(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    let _gate = rig.app.update(rig.visual, |this, _| {
        this.transactions.set_auto_sent(a, Typed::Begin);
        this.transactions.hold(&key)
    });
    assert!(rig.app.read_with(rig.visual, |this, _| this.transactions.is_holding(&key)));
    rig.app.update(rig.visual, |this, cx| this.release_dead_session(&key, cx));
    assert!(!rig.app.read_with(rig.visual, |this, _| this.transactions.is_holding(&key)));
    assert_eq!(rig.cleanups(), 0, "the session goes and the server rolls back with it");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.transactions.auto_open_tabs()), 0);
}

#[gpui::test]
fn a_commit_that_was_never_sent_keeps_the_transaction_open_and_says_so(cx: &mut TestAppContext) {
    use super::transaction::{EndResult, TxEnd};
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('unsent')");
    let ended = rig.app.update(rig.visual, |this, cx| {
        this.finish_end(a, TxEnd::Commit, Ok(EndResult::NotSent(DriverQueryError::Session("the check failed".into()))), None, cx)
    });
    assert!(!ended);
    assert_eq!((rig.pending(a), rig.modifying(a)), (1, 1), "still open: the COMMIT never went out");
    assert!(rig.own_session(a).is_some());
    assert!(rig.log_messages().iter().any(|message| message.contains("Commit was not sent")), "{:?}", rig.log_messages());
    assert!(rig.tx(a).unwrap().lost.is_none(), "not claimed lost");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
}

#[gpui::test]
fn an_edited_connection_asks_about_every_manual_tab_with_statements_then_reopens_on_the_new_settings(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let newer = SqliteDb::new(&format!("{ITEMS_SEED} INSERT INTO items(name) VALUES ('only in the new database');"));
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    // A tab that ran nothing: no question; its session just goes (the tab stays manual).
    rig.run_on(a, COUNT_ITEMS);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    let first = rig.own_session(a).unwrap();
    // The live session's URI is the old one: it must not be what the next session connects with.
    let stale = db.uri();
    let new_uri = newer.uri();
    rig.app.update(rig.visual, |this, cx| {
        this.driver_runtime.session_uris.insert(key.clone(), engine_connect::records::RuntimeUri::new(&stale));
        this.saved_connections.iter_mut().find(|saved| saved.matches_id(&key)).unwrap().uri = new_uri.clone();
        this.connection_edited(&key, cx);
    });
    settle(rig.visual);
    assert!(!rig.rendered_now("tx-guard-rollback"));
    assert_eq!((rig.own_sessions(), rig.mode(a)), (0, CommitMode::Manual));
    // A read is a statement: asked.
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.tab_cell(a), "6", "a session on the new settings: the new database has one more row");
    let second = rig.own_session(a).unwrap();
    assert_ne!(first, second);
    rig.app.update(rig.visual, |this, cx| this.connection_edited(&key, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "asked like a reconnect, reads included");
    rig.click("tx-guard-cancel");
    assert_eq!((rig.own_session(a), rig.pending(a)), (Some(second), 1), "Cancel keeps everything");
    rig.app.update(rig.visual, |this, cx| this.connection_edited(&key, cx));
    settle(rig.visual);
    rig.click("tx-guard-rollback");
    let app = rig.app.clone();
    wait_until(rig.visual, "the reopening", |visual| app.read_with(visual, |this, _| !this.driver_runtime.tab_sessions.contains_key(&a)));
    assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Manual, 0));
}

#[gpui::test]
fn a_session_is_never_closed_under_a_worker_that_still_runs_on_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.run_on(a, COUNT_ITEMS);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    // An Explain is on the session (it is not a statement of the transaction: nothing to ask).
    let guard = rig.app.update(rig.visual, |this, cx| {
        let request = SqlRun {
            tab_id: a,
            kind: RunKind::Explain(ExplainRun::new(engine_plan::ExplainDialect::Sqlite, engine_plan::ExplainMode::Estimate)),
            connection: key.clone(),
            sql: COUNT_ITEMS.into(),
            database: String::new(),
            collection: String::new(),
            limit: 10,
            offset: 0,
            confirmed: false,
            read_only: true,
        };
        this.tx_sending(&request, super::transaction::TabRoute::Own, cx).expect("on the own session").enter()
    });
    rig.app.update(rig.visual, |this, cx| this.connection_edited(&key, cx));
    for _ in 0..10 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(rig.own_session(a).is_some(), "not closed while the worker runs");
    drop(guard);
    let app = rig.app.clone();
    wait_until(rig.visual, "the session to close once the worker is done", |visual| {
        app.read_with(visual, |this, _| !this.driver_runtime.tab_sessions.contains_key(&a))
    });
}

#[gpui::test]
fn an_edited_connection_cancels_a_running_statement_and_waits_for_its_worker_before_closing_the_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.run_on(a, "INSERT INTO items(name) VALUES ('running')");
    rig.start_on(a, LONG_QUERY);
    let app = rig.app.clone();
    wait_until(rig.visual, "the long statement to be on the session", |visual| {
        app.read_with(visual, |this, _| this.driver_runtime.query_operations.contains_key(&a))
    });
    rig.app.update(rig.visual, |this, cx| this.connection_edited(&key, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-rollback"), "a tab whose statement is still running is asked about too");
    rig.click("tx-guard-rollback");
    let app = rig.app.clone();
    wait_until(rig.visual, "the session to close", |visual| app.read_with(visual, |this, _| !this.driver_runtime.tab_sessions.contains_key(&a)));
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(a).unwrap().query.can_run()), "the run was cancelled");
    assert_eq!(rig.pending(a), 0);
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'running'"), "0");
    rig.wait_driver_idle();
}

#[gpui::test]
fn a_page_re_run_is_held_but_neither_counted_nor_given_a_begin_and_an_explain_is_held_too(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, COUNT_ITEMS);
    assert_eq!(rig.pending(a), 1);
    let request = |this: &AppRoot, kind: RunKind, offset: i64| SqlRun {
        tab_id: a,
        kind,
        connection: this.workspace.tab(a).unwrap().connection_id.clone(),
        sql: COUNT_ITEMS.into(),
        database: String::new(),
        collection: String::new(),
        limit: 10,
        offset,
        confirmed: false,
        read_only: false,
    };
    let (begin_of_page, begin_of_run) = rig.app.update(rig.visual, |this, cx| {
        let page = this.tx_sending(&request(this, RunKind::Sql, 10), super::transaction::TabRoute::Own, cx).expect("on the own session");
        let page_begin = page.begin().map(|(sql, _)| sql);
        let run = this.tx_sending(&request(this, RunKind::Sql, 0), super::transaction::TabRoute::Own, cx).expect("on the own session");
        (page_begin, run.begin().map(|(sql, _)| sql))
    });
    assert_eq!(begin_of_page, None, "no BEGIN before a page of a result the tab already has");
    assert_eq!(begin_of_run, Some("BEGIN"), "a statement gets one");
    assert_eq!(rig.pending(a), 2, "only the statement was counted");
    // An Explain is not counted either, but a cancelled one still stops Commit / Roll Back from running under it.
    let guard = rig.app.update(rig.visual, |this, cx| {
        let explain = RunKind::Explain(ExplainRun::new(engine_plan::ExplainDialect::Sqlite, engine_plan::ExplainMode::Estimate));
        let run = this.tx_sending(&request(this, explain, 0), super::transaction::TabRoute::Own, cx).expect("held");
        assert_eq!(run.begin().map(|(sql, _)| sql), None);
        run.enter()
    });
    assert_eq!(rig.pending(a), 2);
    assert_eq!(rig.end_blocked(a), Some(super::transaction::EndBlocked::Stopping), "the Explain is on the session");
    drop(guard);
    assert_eq!(rig.end_blocked(a), None);
}

#[gpui::test]
fn a_statement_goes_where_the_one_decision_says_never_to_a_sessions_it_does_not_name(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, key) = (rig.tab, rig.key.clone());
    rig.click("tx-mode-manual");
    rig.run_on(a, COUNT_ITEMS);
    let run = |rig: &mut SqlRig<'_>| {
        rig.app.update(rig.visual, |this, _| {
            this.route_tab_statement(&SqlRun {
                tab_id: a,
                kind: RunKind::Sql,
                connection: this.workspace.tab(a).unwrap().connection_id.clone(),
                sql: COUNT_ITEMS.into(),
                database: String::new(),
                collection: String::new(),
                limit: 10,
                offset: 0,
                confirmed: false,
                read_only: false,
            })
        })
    };
    assert_eq!(run(&mut rig), Ok(super::transaction::TabRoute::Own));
    // An auto-commit tab never uses a session of its own, even one that is still there.
    rig.app.update(rig.visual, |this, _| this.transactions.set_for_capture(a, TabTx::new(CommitMode::Auto)));
    assert_eq!(run(&mut rig), Ok(super::transaction::TabRoute::Shared));
    let explain_routed = rig.app.update(rig.visual, |this, _| {
        this.route_tab_statement(&SqlRun {
            tab_id: a,
            kind: RunKind::Explain(ExplainRun::new(engine_plan::ExplainDialect::Sqlite, engine_plan::ExplainMode::Estimate)),
            connection: this.workspace.tab(a).unwrap().connection_id.clone(),
            sql: COUNT_ITEMS.into(),
            database: String::new(),
            collection: String::new(),
            limit: 10,
            offset: 0,
            confirmed: false,
            read_only: true,
        })
    });
    assert_eq!(explain_routed, Ok(super::transaction::TabRoute::Shared), "not the own session of a tab that is auto-commit");
    // A statement routed to a session of the tab's own that is gone is refused, not sent on the shared one.
    rig.app.update(rig.visual, |this, _| this.transactions.set_for_capture(a, TabTx::new(CommitMode::Manual)));
    let refused = rig.app.update(rig.visual, |this, _| this.begin_tab_driver_query(Some(999), &key).err().map(|error| error.to_string()));
    assert!(refused.is_some_and(|message| message.contains("no longer open")));
}

#[gpui::test]
fn the_toolbars_tooltips_are_built_once_per_settings_snapshot(cx: &mut TestAppContext) {
    let rig = ExplainRig::new(cx);
    let builds = |rig: &ExplainRig<'_>| rig.app.read_with(rig.visual, |this, _| this.transactions.tooltip_builds.get());
    let (first, second) = rig.app.read_with(rig.visual, |this, _| (this.end_tooltips(), this.end_tooltips()));
    assert_eq!((builds(&rig), first.0.clone()), (1, second.0));
    assert!(first.0.contains("Commit") && first.1.contains("Roll back"));
    rig.app.update(rig.visual, |this, _| this.settings = std::sync::Arc::new((*this.settings).clone()));
    rig.app.read_with(rig.visual, |this, _| this.end_tooltips());
    assert_eq!(builds(&rig), 2, "a new snapshot builds new ones");
}

/// PostgreSQL's COMMIT of an aborted transaction rolls it back and reports success: the tab says
/// so, resets its counters and keeps nothing; the same after a cancelled statement.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_commit_after_an_aborted_transaction_says_it_rolled_back(cx: &mut TestAppContext) {
    const TABLE: &str = "tablex_tx_aborted";
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _drop = DropScratchTable(rig.session.clone(), TABLE);
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {TABLE}"));
    exec_confirmed(&rig.session, &format!("CREATE TABLE {TABLE}(id int PRIMARY KEY)"));
    let a = rig.tab;
    let b = rig.second_tab();
    rig.activate(a);
    rig.click("tx-mode-manual");
    let rolled_back = |rig: &SqlRig<'_>| rig.logged_times("COMMIT rolled it back");

    // A failed statement, then Commit.
    rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES (1)"));
    rig.run_on(a, "SELECT 1/0");
    assert!(rig.tx(a).unwrap().aborted_hint);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(rolled_back(&rig), 1, "{:?}", rig.log_messages());
    assert!(!rig.tx(a).unwrap().aborted_hint);
    rig.run_on(b, &format!("SELECT count(*) FROM {TABLE}"));
    assert_eq!(rig.tab_cell(b), "0", "nothing was committed");

    // A cancelled statement aborts it too.
    rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES (2)"));
    rig.start_on(a, "SELECT pg_sleep(30)");
    let app = rig.app.clone();
    wait_until(rig.visual, "the long statement to be on the session", |visual| {
        app.read_with(visual, |this, _| this.driver_runtime.query_operations.contains_key(&a))
    });
    rig.app.update(rig.visual, |this, cx| this.cancel_tab_query(a, cx));
    assert!(rig.tx(a).unwrap().aborted_hint, "a cancel aborts the PostgreSQL transaction");
    assert_eq!(rig.modifying(a), 2, "the insert and the cancelled statement");
    rig.wait_driver_idle();
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(rolled_back(&rig), 2, "{:?}", rig.log_messages());
    rig.run_on(b, &format!("SELECT count(*) FROM {TABLE}"));
    assert_eq!(rig.tab_cell(b), "0");

    // A transaction that is fine commits, and says nothing of the kind.
    rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES (3)"));
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(rolled_back(&rig), 2);
    rig.run_on(b, &format!("SELECT count(*) FROM {TABLE}"));
    assert_eq!(rig.tab_cell(b), "1");
}

/// `PREPARE TRANSACTION` takes the transaction away from the session: the counters start again.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_prepare_transaction_resets_the_counters(cx: &mut TestAppContext) {
    const TABLE: &str = "tablex_tx_prepared";
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let _drop = DropScratchTable(rig.session.clone(), TABLE);
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {TABLE}"));
    exec_confirmed(&rig.session, &format!("CREATE TABLE {TABLE}(id int PRIMARY KEY)"));
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, &format!("INSERT INTO {TABLE} VALUES (1)"));
    rig.run_on(a, "PREPARE TRANSACTION 'tablex_tx_prepared_gid'");
    if matches!(rig.status(), crate::query::QueryStatus::Failed(message) if message.contains("prepared transactions are disabled")) {
        eprintln!("skipped: the server has max_prepared_transactions = 0");
        return;
    }
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "{:?}", rig.status());
    assert_eq!((rig.pending(a), rig.modifying(a)), (0, 0), "the session has no transaction any more");
    exec_confirmed(&rig.session, "ROLLBACK PREPARED 'tablex_tx_prepared_gid'");
}
