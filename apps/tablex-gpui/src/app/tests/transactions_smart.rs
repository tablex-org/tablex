// Smart commit, the transaction log and the pending-transactions list, through the real SQLite
// driver (a file database), the real toolbar, dialogs and palette. The live PostgreSQL / MySQL /
// MariaDB runs at the end are `#[ignore]`d like the other live tests.

use crate::transaction::LogResult;

/// The connection type every test here gives the connection.
fn smart_type(recover: bool) -> impl FnOnce(&mut crate::settings::connection::ConnectionType) {
    move |ty| {
        ty.smart_commit = true;
        ty.smart_commit_recover = recover;
    }
}

const SEL_NAME: &str = "SELECT name FROM items WHERE id = 1";

impl SqlRig<'_> {
    fn probe(&mut self, selector: String) -> bool {
        // `debug_bounds` wants a 'static key; a leaked probe string per call is test-only.
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        self.visual.debug_bounds(selector).is_some()
    }

    fn click_probe(&mut self, selector: String) {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        self.click(selector);
    }

    fn wait_probe(&mut self, selector: String) {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        self.wait_rendered(selector);
    }

    fn wait_rendered_gone_dyn(&mut self, selector: String) {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        self.wait_rendered_gone(selector);
    }

    fn log_of(&self, tab: u64) -> Vec<crate::transaction::LogEntry> {
        self.app.read_with(self.visual, |this, _| this.transactions.tx(tab).map(|tx| tx.log.entries().cloned().collect()).unwrap_or_default())
    }
}

// ---- smart commit ---------------------------------------------------------------------------------

#[gpui::test]
fn smart_commit_tab_runs_on_a_session_of_its_own_from_its_first_statement_and_a_write_starts_a_transaction_on_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let handle_rig = {
        let mut rig = SqlRig::sqlite(cx, &db);
        let a = rig.tab;
        let handle = private_driver(&mut rig);
        let base = handle.active_session_count();
        rig.set_connection_type(smart_type(true));

        // The first statement, a read: the tab has its own session, in auto-commit (nothing counted).
        rig.run_on(a, COUNT_ITEMS);
        rig.run_on(a, "PRAGMA foreign_keys = ON");
        let own = rig.own_session(a).expect("a session of its own from the first statement");
        assert_eq!((rig.mode(a), rig.own_sessions(), rig.pending(a)), (CommitMode::Auto, 1, 0));
        assert_eq!(handle.active_session_count(), base + 1, "exactly one more connection");
        assert!(rig.shared_untouched() && !rig.rendered_now("tx-commit"));

        // A modifying statement starts a transaction on that same session.
        rig.run_on(a, "UPDATE items SET name = 'smart' WHERE id = 1");
        assert_eq!(rig.mode(a), CommitMode::Manual);
        assert_eq!((rig.pending(a), rig.modifying(a)), (1, 1));
        assert_eq!(rig.own_session(a), Some(own), "no new session");
        assert!(rig.tx(a).is_some_and(|tx| tx.smart));
        assert_eq!(db.sql(SEL_NAME), "a", "not committed");
        assert!(rig.rendered_now("tx-commit") && rig.rendered_now("tx-rollback"), "the toolbar shows the transaction");

        // Commit lands it and, with recovery on, the tab is in auto-commit again on the same session.
        rig.click("tx-commit");
        rig.wait_tx("the commit", a, |tx| tx.statements == 0);
        assert_eq!(db.sql(SEL_NAME), "smart");
        let app = rig.app.clone();
        wait_until(rig.visual, "the tab to return", |visual| app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto));
        assert_eq!((rig.own_session(a), rig.own_sessions()), (Some(own), 1));
        assert!(!rig.tx(a).is_some_and(|tx| tx.smart) && !rig.rendered_now("tx-commit"));

        // Again; Roll Back returns it too. Still the one session.
        rig.run_on(a, "UPDATE items SET name = 'again' WHERE id = 1");
        assert_eq!(rig.mode(a), CommitMode::Manual);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        assert_eq!(db.sql(SEL_NAME), "smart", "rolled back");
        let app = rig.app.clone();
        wait_until(rig.visual, "the tab to return", |visual| app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto));
        assert_eq!((rig.own_session(a), handle.active_session_count()), (Some(own), base + 1));
        // A statement that changes no data leaves the tab in auto-commit, on the same session.
        rig.run_on(a, "PRAGMA foreign_keys = OFF");
        assert_eq!((rig.mode(a), rig.own_session(a)), (CommitMode::Auto, Some(own)));

        // Closing the tab closes the session.
        rig.app.update(rig.visual, |this, cx| this.close_tab(a, cx));
        wait_driver_sessions(&mut rig, &handle, base, "the tab's session to be disconnected");
        handle
    };
    drop(handle_rig);
}

#[gpui::test]
fn smart_commit_applies_to_editors_that_opened_after_it_was_turned_on_not_to_one_already_on_the_shared_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    // `a` runs on the shared session while smart commit is off.
    rig.run_on(a, "UPDATE items SET name = 'plain' WHERE id = 1");
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 0));
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, "UPDATE items SET name = 'shared' WHERE id = 1");
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 0), "never moved to a session of its own");
    assert_eq!(db.sql(SEL_NAME), "shared", "committed by itself");
    // A new editor gets smart commit.
    let b = rig.second_tab();
    rig.run_on(b, "UPDATE items SET name = 'smart' WHERE id = 2");
    assert_eq!((rig.mode(b), rig.own_sessions(), rig.pending(b)), (CommitMode::Manual, 1, 1));
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "b");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", b, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "b");
}

#[gpui::test]
fn without_recovery_the_tab_stays_manual_and_without_smart_commit_nothing_switches(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, plain) = (rig.tab, rig.second_tab());

    // The default: off. An UPDATE commits by itself, on the shared session.
    rig.run_on(plain, "UPDATE items SET name = 'plain' WHERE id = 2");
    assert_eq!((rig.mode(plain), rig.own_sessions()), (CommitMode::Auto, 0));
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "plain");

    // Smart commit on, recovery off.
    rig.set_connection_type(smart_type(false));
    rig.run_on(a, "UPDATE items SET name = 'kept' WHERE id = 1");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.activate(a);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql(SEL_NAME), "kept");
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Manual, 1), "recovery is its own switch");

    // The user returns it to auto-commit: the same session, autocommit on.
    rig.click("tx-mode-auto");
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 1));
    // Recovery on its own does nothing: it needs smart commit.
    rig.set_connection_type(|ty| ty.smart_commit_recover = true);
    rig.run_on(a, "UPDATE items SET name = 'still' WHERE id = 1");
    assert_eq!(rig.mode(a), CommitMode::Auto);
    assert_eq!(db.sql(SEL_NAME), "still");
}

#[gpui::test]
fn a_begin_typed_on_a_smart_tab_in_auto_commit_is_the_tabs_transaction(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    let before = db.sql(SEL_NAME);
    rig.run_on(a, SEL_NAME);
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 1));

    // A typed BEGIN followed by reads only: the transaction is shown (and asked about on close), and
    // Roll Back ends it, so the session holds no lock other tabs would wait on.
    rig.run_on(a, "BEGIN");
    assert_eq!(rig.mode(a), CommitMode::Manual, "{:?}", rig.status());
    rig.run_on(a, SEL_NAME);
    assert!(rig.pending(a) >= 1, "the BEGIN and the read are the tab's transaction");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(rig.mode(a), CommitMode::Auto);
    // A typed BEGIN, then a write: nothing is committed until the tab says so.
    rig.run_on(a, "BEGIN");
    assert_eq!(rig.mode(a), CommitMode::Manual, "{:?}", rig.status());
    rig.run_on(a, "UPDATE items SET name = 'begun' WHERE id = 1");
    assert_eq!(db.sql(SEL_NAME), before, "nothing committed yet");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql(SEL_NAME), before);
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 1), "back to auto-commit on the same session");

    // A SAVEPOINT outside a transaction opens one on SQLite: the same.
    rig.run_on(a, "SAVEPOINT s1");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.run_on(a, "UPDATE items SET name = 'saved' WHERE id = 1");
    rig.run_on(a, "COMMIT");
    rig.wait_tx("the typed commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql(SEL_NAME), "saved");
    assert_eq!(rig.mode(a), CommitMode::Auto, "a typed COMMIT returns it too");
}

#[gpui::test]
fn only_a_tab_smart_commit_switched_is_switched_back_and_a_failed_first_statement_undoes_the_switch(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));

    // A tab the user put in manual mode stays there after Commit.
    rig.click("tx-mode-manual");
    rig.run_on(a, "UPDATE items SET name = 'mine' WHERE id = 1");
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.click("tx-mode-auto");
    assert_eq!(rig.mode(a), CommitMode::Auto);

    // The statement that started the transaction fails: nothing is open, the tab is in auto-commit again.
    rig.run_on(a, "UPDATE nope SET a = 1 WHERE id = 1");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));
    let app = rig.app.clone();
    wait_until(rig.visual, "the transaction to be undone", |visual| {
        app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto && !this.transactions.is_ending(a))
    });
    assert!(!rig.tx(a).is_some_and(|tx| tx.is_open() || tx.smart || tx.smart_fresh));
    // Nothing is left open on the session: a plain write commits by itself again, and the next one
    // that modifies starts a transaction like the first.
    rig.run_on(a, "UPDATE items SET name = 'one' WHERE id = 1");
    rig.run_on(a, "UPDATE nope SET a = 1 WHERE id = 1");
    assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Manual, 2));
    assert!(rig.tx(a).is_some_and(|tx| !tx.smart_fresh), "it failed after others ran in the transaction: it stays open");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql(SEL_NAME), "mine");
}

#[gpui::test]
fn smart_commit_never_switches_a_read_only_connection_or_a_kind_without_transactions(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(|ty| {
        smart_type(true)(ty);
        ty.read_only = true;
    });
    rig.run_on(a, "UPDATE items SET name = 'no' WHERE id = 1");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "read-only refuses it");
    assert_eq!((rig.mode(a), rig.own_sessions()), (CommitMode::Auto, 0));
    assert_eq!(db.sql(SEL_NAME), "a");
}

#[gpui::test]
fn a_statement_the_driver_holds_back_for_confirmation_does_not_leave_the_tab_switched(cx: &mut TestAppContext) {
    let db = SqliteDb::new(&format!("{ITEMS_SEED} CREATE TABLE victim(a INTEGER);"));
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, "DROP TABLE victim");
    assert!(rig.rendered_now("sql-confirm-run"), "the driver asks first");
    // The question comes before the transaction starts: nothing was begun, nothing has to be undone.
    assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Auto, 0), "nothing ran: no transaction");
    assert!(rig.app.read_with(rig.visual, |this, _| this.transactions.tx(a).is_none_or(|tx| !tx.smart && !tx.is_open())));
    // Run anyway: the statement is sent again, switches the tab again, and waits for the user.
    rig.click("sql-confirm-run");
    rig.wait_idle(a);
    rig.wait_tx("the transaction", a, |tx| tx.is_open());
    assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Manual, 1));
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'victim'"), "1", "not committed yet");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'victim'"), "1", "rolled back: the table is still there");
    assert_eq!(rig.mode(a), CommitMode::Auto);
}

#[gpui::test]
fn smart_commit_switches_the_kinds_with_transactions_and_not_clickhouse(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let update = "UPDATE t SET a = 1 WHERE id = 1";
    for (kind, switches) in [
        (ConnectionKind::Postgres, true),
        (ConnectionKind::Mysql, true),
        (ConnectionKind::Mariadb, true),
        (ConnectionKind::Sqlite, true),
        (ConnectionKind::Clickhouse, false),
    ] {
        let tab = rig.tab(kind, update);
        let key = rig.app.read_with(rig.visual, |this, _| this.workspace.tab(tab).unwrap().connection_id.clone());
        rig.app.update(rig.visual, |this, cx| {
            let mut settings = (*this.settings).clone();
            settings.connection_types.retain(|existing| existing.id != "smart-type");
            let mut ty = crate::settings::connection::ConnectionType { id: "smart-type".into(), name: "smart".into(), ..Default::default() };
            smart_type(true)(&mut ty);
            settings.connection_types.push(ty);
            this.settings = std::sync::Arc::new(settings);
            let profile = crate::settings::connection::ConnectionProfile { type_id: "smart-type".into(), ..Default::default() };
            this.connection_policy.set_profile(&key, profile);
            cx.notify();
        });
        let (switched, mode) = rig.app.update(rig.visual, |this, cx| {
            let request = SqlRun {
                tab_id: tab,
                kind: RunKind::Sql,
                connection: key.clone(),
                sql: update.into(),
                database: String::new(),
                collection: String::new(),
                limit: 10,
                offset: 0,
                confirmed: false,
                read_only: false,
            };
            (this.smart_commit_switch(&request, cx), this.commit_mode(tab))
        });
        assert_eq!(switched, switches, "{kind:?}");
        assert_eq!(mode, if switches { CommitMode::Manual } else { CommitMode::Auto }, "{kind:?}");
    }
}

// ---- the session state of the tab --------------------------------------------------------------

/// A parent table and a child that cascades: the foreign key decides what a DELETE of the parent
/// does, and SQLite enforces it only on a connection that said `PRAGMA foreign_keys = ON`.
const FK_SEED: &str = "CREATE TABLE p(id INTEGER PRIMARY KEY); CREATE TABLE c(pid INTEGER REFERENCES p(id) ON DELETE CASCADE); \
     INSERT INTO p VALUES (1),(2),(3); INSERT INTO c VALUES (1),(1),(2),(3); \
     CREATE TABLE late(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED);";

#[gpui::test]
fn a_script_that_sets_the_session_before_the_write_runs_both_on_the_session_of_the_write(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FK_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(false));
    // The PRAGMA is a statement of its own in the script: it must be on the session the DELETE is.
    rig.run_on(a, "PRAGMA foreign_keys = ON; DELETE FROM p WHERE id = 1");
    assert_eq!(rig.mode(a), CommitMode::Manual, "the script modifies: the tab was switched before its first statement");
    assert_eq!(db.sql("SELECT count(*) FROM p"), "3", "not committed");
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM p"), "2");
    assert_eq!(db.sql("SELECT count(*) FROM c WHERE pid = 1"), "0", "the foreign key was enforced: the children went with the parent");
    assert_eq!(db.sql("SELECT count(*) FROM c"), "2");
}

#[gpui::test]
fn what_a_smart_tab_set_in_one_run_is_still_on_its_session_when_a_later_run_writes(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FK_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, "PRAGMA foreign_keys = ON");
    assert_eq!(rig.mode(a), CommitMode::Auto, "setting a pragma changes no data");
    let own = rig.own_session(a);
    // A run of its own: the transaction starts on the same session, which has the pragma. Nothing is replayed.
    rig.run_on(a, "DELETE FROM p WHERE id = 2");
    assert_eq!((rig.mode(a), rig.own_session(a)), (CommitMode::Manual, own));
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM c WHERE pid = 2"), "0", "the foreign key was enforced");
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to return", |visual| app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto));
    assert_eq!(rig.own_session(a), own, "the session is kept");
    // Still on it, for the next one.
    rig.run_on(a, "DELETE FROM p WHERE id = 3");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM p WHERE id = 3"), "1");
}

#[gpui::test]
fn a_manual_sqlite_tab_rolls_a_pragma_back_with_its_transaction(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FK_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "PRAGMA user_version = 5");
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("PRAGMA user_version"), "0", "rolled back with the transaction");
    assert_eq!(rig.logged_times("failed"), 0, "no error: {:?}", rig.log_messages());
    // A PRAGMA and a write in one request: both in the transaction.
    rig.run_on(a, "PRAGMA user_version = 6; DELETE FROM p WHERE id = 1");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!((db.sql("PRAGMA user_version"), db.sql("SELECT count(*) FROM p")), ("0".into(), "3".into()));
    assert_eq!(rig.logged_times("failed"), 0, "no error: {:?}", rig.log_messages());
}

#[gpui::test]
fn a_continue_on_error_script_keeps_the_later_writes_in_the_transaction_after_its_first_one_failed(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.app.update(rig.visual, |this, _| {
        let mut settings = (*this.settings).clone();
        settings.editor.on_script_error = crate::settings::OnScriptError::Continue;
        this.settings = std::sync::Arc::new(settings);
    });
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, "UPDATE nope SET a = 1 WHERE id = 1; UPDATE items SET name = 'later' WHERE id = 1; UPDATE items SET name = 'last' WHERE id = 2");
    assert_eq!(rig.mode(a), CommitMode::Manual, "the later writes are in the transaction");
    assert_eq!(rig.pending(a), 3);
    assert_eq!(db.sql(SEL_NAME), "a", "not committed");
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "b");
}

#[gpui::test]
fn a_typed_commit_returns_a_smart_tab_and_an_explicit_transaction_in_a_script_does_not_end_smart_commit(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, "UPDATE items SET name = 'typed' WHERE id = 1");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.run_on(a, "COMMIT");
    assert_eq!(db.sql(SEL_NAME), "typed");
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to return", |visual| app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto));
    assert_eq!(rig.own_sessions(), 1, "the same session");

    // BEGIN; ...; COMMIT; UPDATE b: the write after the COMMIT waits in a transaction too.
    rig.run_on(a, "BEGIN; UPDATE items SET name = 'one' WHERE id = 2; COMMIT; UPDATE items SET name = 'two' WHERE id = 3");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "one", "inside the explicit transaction, committed by it");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 3"), "c", "after it: waiting for Commit");
    assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Manual, 1));
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 3"), "c");
}

#[gpui::test]
fn a_run_cancelled_while_its_session_opens_leaves_the_tab_in_auto_commit(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    rig.start_on(a, "UPDATE items SET name = 'never' WHERE id = 1");
    rig.app.update(rig.visual, |this, cx| this.cancel_tab_query(a, cx));
    let app = rig.app.clone();
    wait_until(rig.visual, "the switch to be undone", |visual| app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto));
    for _ in 0..20 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rig.mode(a), CommitMode::Auto, "back in auto-commit");
    assert!(rig.own_sessions() <= 1);
    assert_eq!(db.sql(SEL_NAME), "a");
}

#[gpui::test]
fn a_commit_the_server_refuses_still_ends_the_transaction_and_returns_the_smart_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(FK_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, "PRAGMA foreign_keys = ON");
    // The deferred key is checked at COMMIT.
    rig.run_on(a, "INSERT INTO late VALUES (99)");
    assert_eq!(rig.mode(a), CommitMode::Manual);
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert_eq!(db.sql("SELECT count(*) FROM late"), "0", "refused");
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to return", |visual| app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto));
}

/// A script that ends the transaction smart commit started with its own COMMIT / ROLLBACK, and goes on.
/// The tab recovers at the commit event and judges each statement afresh. What follows a COMMIT is in auto-commit until a statement modifies again, which starts the next
/// transaction; a tab that stays in manual mode for the rest of the script would count a read as an open
/// transaction, and show it after the script ended.
#[gpui::test]
fn a_scripts_own_commit_returns_the_smart_tab_before_its_next_statement_and_a_write_after_it_starts_the_next_transaction(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));

    // BEGIN; write; COMMIT; read: the read follows the commit in auto-commit, nothing is left open.
    rig.run_on(a, "BEGIN; UPDATE items SET name = 'one' WHERE id = 2; COMMIT; SELECT count(*) FROM items");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "one", "committed by the script's COMMIT");
    assert_eq!((rig.mode(a), rig.pending(a), rig.own_sessions()), (CommitMode::Auto, 0, 1), "{:?}", rig.status());
    assert!(!rig.rendered_now("tx-commit"), "no open transaction in the toolbar");

    // The same with ROLLBACK, and a write after it: that one starts the next transaction, and waits.
    rig.run_on(a, "BEGIN; UPDATE items SET name = 'gone' WHERE id = 2; ROLLBACK; UPDATE items SET name = 'two' WHERE id = 3; SELECT 1");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "one", "rolled back by the script");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 3"), "c", "the write after the ROLLBACK waits for Commit");
    assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Manual, 2));
    assert!(rig.tx(a).is_some_and(|tx| tx.smart));
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    let app = rig.app.clone();
    wait_until(rig.visual, "the tab to return", |visual| app.read_with(visual, |this, _| this.commit_mode(a) == CommitMode::Auto));
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 3"), "c");
}

#[gpui::test]
fn without_recovery_a_scripts_own_commit_leaves_the_tab_manual(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(false));
    rig.run_on(a, "BEGIN; UPDATE items SET name = 'one' WHERE id = 2; COMMIT");
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "one");
    assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Manual, 0), "recovery is its own switch");
}

// ---- the session follows its connection type ------------------------------------------------------

impl SqlRig<'_> {
    /// The rig's connection type changed the way the settings page's Apply changes it (everything that
    /// follows a settings change runs).
    fn apply_type(&mut self, change: impl FnOnce(&mut crate::settings::connection::ConnectionType)) {
        self.app.update(self.visual, |this, cx| {
            let mut settings = (*this.settings).clone();
            let ty = settings.connection_types.iter_mut().find(|ty| ty.id == "test-type").expect("set_connection_type made it");
            change(ty);
            this.apply_settings(settings, None, cx);
        });
        settle(self.visual);
    }
}

#[gpui::test]
fn turning_smart_commit_off_closes_idle_own_sessions_and_releases_an_open_transaction_once_it_ended(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (a, b) = (rig.tab, rig.second_tab());
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, SEL_NAME);
    rig.run_on(b, "UPDATE items SET name = 'open' WHERE id = 2");
    assert_eq!((rig.own_sessions(), rig.mode(b), rig.pending(b)), (2, CommitMode::Manual, 1));

    rig.apply_type(|ty| ty.smart_commit = false);
    assert_eq!((rig.own_session(a), rig.mode(a)), (None, CommitMode::Auto), "idle: closed, back on the shared session");
    assert!(rig.own_session(b).is_some() && rig.pending(b) == 1, "an open transaction is left alone");
    assert_eq!(rig.own_sessions(), 1);

    // Its transaction ends: the session goes with it, and the tab is on the shared session.
    rig.activate(b);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", b, |tx| tx.statements == 0);
    let app = rig.app.clone();
    wait_until(rig.visual, "the session to be released", |visual| app.read_with(visual, |this, _| this.driver_runtime.tab_sessions.is_empty()));
    assert_eq!(rig.mode(b), CommitMode::Auto);
    // Both write on the shared session now: committed by themselves.
    rig.run_on(a, "UPDATE items SET name = 'shared' WHERE id = 1");
    assert_eq!((rig.mode(a), rig.own_sessions(), db.sql(SEL_NAME)), (CommitMode::Auto, 0, "shared".into()));
    assert!(rig.shared_untouched());
}

#[gpui::test]
fn a_connection_that_turns_read_only_closes_the_idle_own_session_of_any_tab_and_keeps_a_manual_one_when_only_smart_commit_goes(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (smart, manual) = (rig.tab, rig.second_tab());
    rig.set_connection_type(smart_type(true));
    rig.run_on(smart, SEL_NAME);
    rig.activate(manual);
    rig.click("tx-mode-manual");
    rig.run_on(manual, SEL_NAME);
    assert_eq!((rig.own_sessions(), rig.mode(manual)), (2, CommitMode::Manual));
    // (A read opens the manual tab's transaction: it is idle once that is ended.)
    rig.activate(manual);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", manual, |tx| tx.statements == 0);

    // Smart commit off: the tab smart commit gave a session loses it, the one the user made manual does not.
    rig.apply_type(|ty| ty.smart_commit = false);
    assert_eq!((rig.own_session(smart), rig.own_sessions()), (None, 1));
    assert_eq!(rig.mode(manual), CommitMode::Manual, "theirs");

    // Read-only: no manual commit there, so no session of its own, whoever asked for it. (An open
    // transaction would keep it until it ends: the test above.)
    rig.apply_type(|ty| ty.read_only = true);
    assert_eq!((rig.own_session(manual), rig.own_sessions(), rig.mode(manual)), (None, 0, CommitMode::Auto));
    rig.run_on(manual, SEL_NAME);
    assert_eq!(rig.tab_cell(manual), "a");
    assert_eq!(rig.own_sessions(), 0);
}

#[gpui::test]
fn a_connection_moved_to_a_read_only_type_releases_its_idle_own_sessions_too(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, SEL_NAME);
    assert_eq!(rig.own_sessions(), 1);
    // The connection is edited to another type, as the connection dialog does.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.connection_types.push(crate::settings::connection::ConnectionType { id: "ro-type".into(), name: "ro".into(), read_only: true, ..Default::default() });
        this.settings = std::sync::Arc::new(settings);
        let profile = crate::settings::connection::ConnectionProfile { type_id: "ro-type".into(), ..Default::default() };
        this.connection_policy.set_profile(&key, profile);
        let saved = this.saved_connections.clone();
        this.apply_saved_connections(saved, cx);
    });
    settle(rig.visual);
    assert_eq!((rig.own_sessions(), rig.mode(a)), (0, CommitMode::Auto));
}

/// The visible result of a tab whose session is released keeps its paging: the page buttons re-send the
/// statement on screen through whichever session the tab has then (the shared one), and the page handle of the
/// closed session is dropped, as the Auto-commit / Manual toggle drops it. An Explain, which ends no run of
/// the transaction code, releases the session too once it is done.
#[gpui::test]
fn a_released_sessions_result_still_pages_and_an_explain_ends_the_wait_for_the_release(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_page_size(2);
    rig.set_connection_type(smart_type(true));
    rig.run("SELECT id, name FROM items ORDER BY id");
    rig.wait_not_running("page 1");
    assert_eq!((rig.own_sessions(), rig.first_column()), (1, ["1", "2"].map(String::from).to_vec()));
    rig.apply_type(|ty| ty.smart_commit = false);
    assert_eq!(rig.own_sessions(), 0, "idle: released");
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(a).unwrap().driver_page_id.is_none()), "the closed session's page handle is dropped");
    rig.next_page();
    rig.wait_not_running("page 2");
    assert_eq!(rig.paging(), (1, true));
    assert_eq!(rig.first_column(), ["3", "4"], "page 2 of the statement on screen, from the shared session");
    assert_eq!(rig.own_sessions(), 0);

    // An Explain on a smart tab's session while the type stops asking for it: released when the Explain is done.
    rig.apply_type(|ty| ty.smart_commit = true);
    let b = rig.second_tab();
    rig.run_on(b, SEL_NAME);
    assert_eq!(rig.own_sessions(), 1);
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.tab_mut(b).unwrap().sql = SEL_NAME.into();
        this.explain_bound(b, engine_plan::ExplainMode::Estimate, SEL_NAME.into(), cx);
    });
    rig.apply_type(|ty| ty.smart_commit = false);
    let app = rig.app.clone();
    wait_until(rig.visual, "the session to be released after the Explain", |visual| app.read_with(visual, |this, _| this.driver_runtime.tab_sessions.is_empty()));
}

/// A smart commit session that goes back to auto-commit without a round trip has `SET autocommit = 1` owed to
/// its next request - on MySQL / MariaDB only, and not once the session is gone (a new one starts with autocommit on).
#[gpui::test]
fn the_deferred_autocommit_is_owed_by_the_mysql_family_only_and_not_by_a_lost_session(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.set_connection_type(smart_type(true));
    rig.run_on(a, SEL_NAME);
    let owed = |rig: &mut SqlRig<'_>, restore: bool| {
        rig.app.update(rig.visual, |this, cx| {
            let mut tx = this.transactions.tx(a).cloned().expect("the tab ran");
            tx.autocommit_restore = restore;
            this.transactions.set_for_capture(a, tx);
            let request = SqlRun {
                tab_id: a,
                kind: RunKind::Sql,
                connection: this.workspace.tab(a).unwrap().connection_id.clone(),
                sql: SEL_NAME.into(),
                database: String::new(),
                collection: String::new(),
                limit: 10,
                offset: 0,
                confirmed: false,
                read_only: false,
            };
            this.tx_sending(&request, super::transaction::TabRoute::Own, cx).expect("on the own session").begin().map(|(sql, _)| sql)
        })
    };
    assert_eq!(owed(&mut rig, true), None, "SQLite has no autocommit to turn on");
    rig.app.update(rig.visual, |this, _| {
        let mut tx = this.transactions.tx(a).cloned().unwrap();
        tx.autocommit_restore = true;
        this.transactions.set_for_capture(a, tx);
        this.lose_tab_transaction(a, "gone");
    });
    assert!(!rig.tx(a).unwrap().autocommit_restore, "the session is gone: nothing to restore");
}

// ---- the lexer the session reads with --------------------------------------------------------------

/// The statements of a manual MySQL tab are judged with the options of its session: a `SET` that only spells
/// `autocommit` in a string, a comment or a user variable is not refused, and one that really sets it among
/// other assignments is, wherever the text mentions it first.
#[gpui::test]
fn a_manual_mysql_tab_refuses_set_autocommit_as_the_server_reads_it_not_as_the_text_spells_it(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Mysql, "SELECT 1");
    rig.app.update(rig.visual, |this, _| this.transactions.set_for_capture(tab, crate::transaction::TabTx::new(CommitMode::Manual)));
    let route = |rig: &mut ExplainRig<'_>, sql: &str| {
        rig.app.update(rig.visual, |this, _| {
            let connection = this.workspace.tab(tab).unwrap().connection_id.clone();
            this.route_tab_statement(&SqlRun {
                tab_id: tab,
                kind: RunKind::Sql,
                connection,
                sql: sql.into(),
                database: String::new(),
                collection: String::new(),
                limit: 10,
                offset: 0,
                confirmed: false,
                read_only: false,
            })
        })
    };
    for sql in ["SET @note = 'autocommit = 0'", "SET @my_autocommit = 0", "SET /* autocommit = 1 */ @a = 1", "SET @a = 1 # autocommit = 0"] {
        assert_eq!(route(&mut rig, sql), Ok(super::transaction::TabRoute::Open), "{sql}");
    }
    for sql in ["SET @a = 'autocommit', autocommit = 1", "SET /* autocommit */ autocommit = 1", "SET `autocommit` = 1"] {
        assert!(route(&mut rig, sql).is_err_and(|message| message.contains("SET autocommit")), "{sql}");
    }
}

// ---- the pending list with real sessions --------------------------------------------------------

impl SqlRig<'_> {
    /// A manual tab on a connection of its own to `db`, with an UPDATE of `name` of row `id` sent.
    fn tab_on(&mut self, db: &SqliteDb, name: &str, id: u32) -> u64 {
        let handle = self.handle.clone();
        let uri = db.uri();
        let tab = self.app.update(self.visual, |this, cx| {
            let first = this.saved_connections.iter().find(|saved| saved.matches_id(&self.key)).cloned().unwrap();
            let uuid = uuid::Uuid::new_v4();
            let key = uuid.to_string();
            this.saved_connections.push(SavedConnection { id: uuid, name: format!("db-{name}"), uri: uri.clone(), ..first });
            let session = handle.connect(&serde_json::json!({ "uri": uri }).to_string()).expect("the driver connects");
            let engine_id = session.connection_id();
            this.driver_runtime.sessions.insert(key.clone(), std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session)));
            this.driver_runtime.connection_ids.insert(key.clone(), engine_id);
            this.driver_runtime.session_handles.insert(key.clone(), handle.clone());
            this.driver_runtime.capabilities.insert(key.clone(), std::sync::Arc::new(handle.capabilities().clone()));
            let epoch = this.sessions.begin_connect(&key).unwrap();
            this.sessions.finish_connect(&key, epoch, Ok(()));
            let tab = this.workspace.open_query_tab_with_metadata(name, TabMetadata { connection_id: key, ..Default::default() });
            this.reset_active_view();
            cx.notify();
            tab
        });
        settle(self.visual);
        self.activate(tab);
        self.click("tx-mode-manual");
        self.run_on(tab, &format!("UPDATE items SET name = '{name}' WHERE id = {id}"));
        tab
    }
}

#[gpui::test]
fn the_pending_list_ends_real_transactions_of_several_connections_one_row_or_the_selected(cx: &mut TestAppContext) {
    let (db_a, db_b, db_c) = (SqliteDb::new(ITEMS_SEED), SqliteDb::new(ITEMS_SEED), SqliteDb::new(ITEMS_SEED));
    let mut rig = SqlRig::sqlite(cx, &db_a);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "UPDATE items SET name = 'from-a' WHERE id = 1");
    let b = rig.tab_on(&db_b, "from-b", 2);
    let c = rig.tab_on(&db_c, "from-c", 3);
    for tab in [a, b, c] {
        assert!(rig.own_session(tab).is_some(), "tab {tab} has a session of its own");
    }

    rig.app.update_in(rig.visual, |this, window, cx| command_palette::StaticAction::PendingTransactions.run(this, window, cx));
    rig.wait_rendered("tx-pending-list");
    for tab in [a, b, c] {
        assert!(rig.probe(format!("tx-pending-row-{tab}")), "tab {tab} is listed");
    }
    // Commit one row: that connection's data lands, the others do not.
    rig.click_probe(format!("tx-pending-commit-{a}"));
    rig.wait_tx("the commit of a", a, |tx| tx.statements == 0);
    rig.wait_rendered_gone_dyn(format!("tx-pending-row-{a}"));
    assert_eq!(db_a.sql("SELECT name FROM items WHERE id = 1"), "from-a");
    assert_eq!((db_b.sql("SELECT name FROM items WHERE id = 2"), db_c.sql("SELECT name FROM items WHERE id = 3")), ("b".into(), "c".into()));
    assert_eq!((rig.pending(b), rig.pending(c)), (1, 1));
    // Take c out of the selection: Roll Back selected undoes b only.
    rig.click_probe(format!("tx-pending-check-{c}"));
    rig.click("tx-pending-rollback-selected");
    rig.wait_tx("the rollback of b", b, |tx| tx.statements == 0);
    rig.wait_rendered_gone_dyn(format!("tx-pending-row-{b}"));
    assert_eq!(db_b.sql("SELECT name FROM items WHERE id = 2"), "b", "rolled back");
    assert_eq!(rig.pending(c), 1);
    // Select it again and commit.
    rig.click("tx-pending-all");
    rig.click("tx-pending-commit-selected");
    rig.wait_tx("the commit of c", c, |tx| tx.statements == 0);
    rig.wait_rendered("tx-pending-empty");
    assert_eq!(db_c.sql("SELECT name FROM items WHERE id = 3"), "from-c");
}

#[gpui::test]
fn a_row_whose_tab_is_running_is_not_taken_by_the_list_and_a_closed_tab_leaves_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let b = rig.second_tab();
    rig.app.update(rig.visual, |this, _| {
        this.transactions.set_for_capture(b, open_tx_state());
    });
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, COUNT_ITEMS);
    rig.start_on(a, LONG_QUERY);
    let app = rig.app.clone();
    wait_until(rig.visual, "the statement to run", |visual| app.read_with(visual, |this, _| !this.workspace.runtime(a).unwrap().query.can_run()));

    rig.app.update_in(rig.visual, |this, window, cx| this.open_pending_transactions(window, cx));
    rig.wait_rendered("tx-pending-list");
    assert!(rig.probe(format!("tx-pending-row-{a}")) && rig.probe(format!("tx-pending-row-{b}")));
    // Its row's buttons do nothing, and the bulk action skips it: the statement keeps running.
    rig.click_probe(format!("tx-pending-rollback-{a}"));
    rig.click("tx-pending-rollback-selected");
    for _ in 0..10 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(a).unwrap().query.can_run()), "still running: nothing cancelled it");
    assert!(rig.app.read_with(rig.visual, |this, _| this.pending_transaction_rows().iter().any(|row| row.tab_id == a && row.blocked)), "a's row is marked busy");
    rig.wait_rendered_gone_dyn(format!("tx-pending-row-{b}"));
    assert_eq!(rig.pending(b), 0, "b (idle) was taken by the bulk action ...");
    assert!(rig.probe(format!("tx-pending-row-{a}")), "... and a's row stays");
    assert!(rig.probe("tx-pending-count".into()));

    // A tab closed while the list is open leaves it.
    rig.app.update(rig.visual, |this, cx| this.cancel_tab_query(a, cx));
    rig.wait_idle(a);
    rig.app.update(rig.visual, |this, cx| {
        this.close_tab(a, cx);
        cx.notify();
    });
    rig.wait_rendered_gone_dyn(format!("tx-pending-row-{a}"));
    rig.wait_rendered("tx-pending-empty");
}

// ---- the transaction log --------------------------------------------------------------------------

#[gpui::test]
fn the_log_lists_the_statements_of_the_transaction_with_their_flags_and_a_commit_clears_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, COUNT_ITEMS);
    rig.run_on(a, "UPDATE items SET name = 'logged' WHERE id IN (1, 2)");
    rig.run_on(a, "UPDATE nope SET password = 'hunter2' WHERE id = 1");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));

    let log = rig.log_of(a);
    assert_eq!(log.len(), 3);
    assert_eq!(log.iter().map(|entry| (entry.writes, entry.result)).collect::<Vec<_>>(), [
        (false, LogResult::Ok),
        (true, LogResult::Ok),
        (true, LogResult::Failed),
    ]);
    assert_eq!((log[0].rows, log[1].rows), (Some(1), Some(2)), "rows returned, rows affected");
    assert!(log[2].sql.contains("password = '********'") && !log[2].sql.contains("hunter2"), "{}", log[2].sql);
    assert!(log[2].error.as_deref().is_some_and(|error| !error.contains("hunter2")));

    // The real click on the status text opens it: one row per statement.
    rig.click("tx-status");
    rig.wait_rendered("tx-log-summary");
    for index in 0..3 {
        assert!(rig.probe(format!("tx-log-row-{index}")), "row {index}");
    }
    assert!(!rig.probe("tx-log-row-3".into()));
    for (index, kind) in ["reads", "modifies", "modifies"].into_iter().enumerate() {
        assert!(rig.probe(format!("tx-log-kind-{index}-{kind}")), "row {index} is {kind}");
    }
    assert!(rig.probe("tx-log-close".into()) && !rig.probe("tx-log-empty".into()));
    rig.click("tx-log-close");
    rig.wait_rendered_gone("tx-log-summary");

    // Commit ends the transaction: the log is gone, and an empty one says so.
    rig.click("tx-commit");
    rig.wait_tx("the commit", a, |tx| tx.statements == 0);
    assert!(rig.log_of(a).is_empty());
    rig.click("tx-status");
    rig.wait_rendered("tx-log-empty");
    assert!(!rig.probe("tx-log-row-0".into()));
}

#[gpui::test]
fn the_log_is_capped_and_an_auto_commit_tab_keeps_none(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.run_on(a, COUNT_ITEMS);
    assert!(rig.log_of(a).is_empty(), "auto-commit: no transaction, no log");
    rig.click("tx-mode-manual");
    let cap = crate::transaction::LOG_MAX_ENTRIES;
    // Fed as the run path feeds it, one statement at a time (a real run per entry would take minutes).
    rig.app.update(rig.visual, |this, _| {
        let mut tx = TabTx::new(CommitMode::Manual);
        let lex = engine_driver_kit::sql::SqlLexOptions::sqlite();
        for index in 0..cap + 3 {
            let now = Instant::now();
            tx.sending(now, true);
            tx.log.begin(now, true, false, &format!("UPDATE items SET name = '{index}'"), &lex);
            tx.log.finish(LogResult::Ok, Some(1), None, now);
            tx.finished(crate::transaction::Outcome::Done, true, false, false, now);
        }
        this.transactions.set_for_capture(a, tx);
    });
    assert_eq!(rig.log_of(a).len(), cap);
    rig.click("tx-status");
    rig.wait_rendered("tx-log-older");
    assert!(rig.probe("tx-log-row-0".into()), "the first visible row is drawn");
    assert!(!rig.probe(format!("tx-log-row-{}", cap - 1)), "rows beyond the viewport are not built");
}

// ---- the pending transactions ---------------------------------------------------------------------

#[gpui::test]
fn the_pending_list_commits_one_row_and_rolls_back_the_selected(cx: &mut TestAppContext) {
    // SQLite has one writer at a time and a commit waits for readers: tab a is the real transaction
    // on the database, b and c are open transactions without a session behind them.
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    let b = rig.second_tab();
    let key = rig.key.clone();
    let c = rig.app.update(rig.visual, |this, cx| {
        let tab = this.workspace.open_query_tab_with_metadata("SQL 3", TabMetadata { connection_id: key, ..Default::default() });
        cx.notify();
        tab
    });
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, "UPDATE items SET name = 'from-a' WHERE id = 1");
    rig.app.update(rig.visual, |this, _| {
        for tab in [b, c] {
            this.transactions.set_for_capture(tab, open_tx_state());
        }
    });
    assert_eq!(db.sql(SEL_NAME), "a", "nothing committed yet");

    // Opened from the command palette, like the menu item.
    rig.app.update_in(rig.visual, |this, window, cx| command_palette::StaticAction::PendingTransactions.run(this, window, cx));
    rig.wait_rendered("tx-pending-list");
    for tab in [a, b, c] {
        assert!(rig.probe(format!("tx-pending-row-{tab}")), "tab {tab} is listed");
    }

    // Commit one row: that tab's counter resets and its row goes; the others stay.
    rig.click_probe(format!("tx-pending-commit-{a}"));
    rig.wait_tx("the commit of a", a, |tx| tx.statements == 0);
    rig.wait_rendered_gone_dyn(format!("tx-pending-row-{a}"));
    assert_eq!((rig.pending(a), rig.pending(b), rig.pending(c)), (0, 2, 2));
    assert_eq!(db.sql(SEL_NAME), "from-a");
    assert!(rig.probe(format!("tx-pending-row-{b}")) && rig.probe(format!("tx-pending-row-{c}")));

    // a opens another transaction; c is taken out of the selection: Roll Back selected ends a and b.
    rig.run_on(a, "UPDATE items SET name = 'from-a2' WHERE id = 2");
    rig.wait_probe(format!("tx-pending-row-{a}"));
    rig.click_probe(format!("tx-pending-check-{c}"));
    rig.click("tx-pending-rollback-selected");
    rig.wait_tx("the rollback of a", a, |tx| tx.statements == 0);
    rig.wait_rendered_gone_dyn(format!("tx-pending-row-{a}"));
    rig.wait_rendered_gone_dyn(format!("tx-pending-row-{b}"));
    assert_eq!((rig.pending(a), rig.pending(b), rig.pending(c)), (0, 0, 2));
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 2"), "b", "rolled back");

    // Select all again and commit: c ends, and the list says nothing is open.
    rig.click("tx-pending-all");
    rig.click("tx-pending-commit-selected");
    rig.wait_tx("the commit of c", c, |tx| tx.statements == 0);
    rig.wait_rendered("tx-pending-empty");
}

#[gpui::test]
fn the_close_question_links_to_the_pending_list(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let a = rig.tab;
    rig.click("tx-mode-manual");
    rig.run_on(a, "UPDATE items SET name = 'linked' WHERE id = 1");
    rig.app.update_in(rig.visual, |this, window, cx| this.confirm_close_tab(a, window, cx));
    settle(rig.visual);
    assert!(rig.rendered_now("tx-guard-pending"), "the question offers the list");
    rig.click("tx-guard-pending");
    rig.wait_rendered("tx-pending-list");
    assert!(!rig.rendered_now("tx-guard-commit"), "the question is left");
    assert!(rig.probe(format!("tx-pending-row-{a}")));
    assert!(rig.app.read_with(rig.visual, |this, _| this.workspace.tab(a).is_some()), "the tab is still there");
    rig.click_probe(format!("tx-pending-rollback-{a}"));
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    assert_eq!(db.sql(SEL_NAME), "a");
}

#[gpui::test]
fn the_pending_list_is_in_the_palette_and_the_file_menu(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    visual.update(|_, cx| menus::finish_launch(cx, |_| {}));
    let tab = app.update(visual, |this, cx| {
        let tab = this.workspace.open_query_tab_with_metadata("orders", TabMetadata { connection_id: crate::ui_fixture::POSTGRES_ID.into(), ..Default::default() });
        this.workspace.activate_tab(tab);
        this.transactions.set_for_capture(tab, open_tx_state());
        cx.notify();
        tab
    });
    palette_open(visual);
    palette_type(visual, "pending");
    let found = palette_snapshot(&app, visual);
    assert!(palette_titles(&found, "Actions").contains(&"Pending Transactions".to_string()), "{found:?}");
    visual.update(|window, cx| window.close_dialog(cx));
    palette_settle(visual);

    visual.dispatch_action(menus::PendingTransactions);
    palette_settle(visual);
    assert!(visual.debug_bounds("tx-pending-list").is_some(), "the menu item opens the list");
    let row: &'static str = Box::leak(format!("tx-pending-row-{tab}").into_boxed_str());
    assert!(visual.debug_bounds(row).is_some());
}

// ---- the settings -----------------------------------------------------------------------------------

#[gpui::test]
fn the_smart_commit_switches_are_in_the_connection_type_page_and_persist(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "query");
    click(visual, format!("connection-type-row-{PRODUCTION}"));
    let production = |settings: &Settings| settings.connection_type(PRODUCTION).unwrap().clone();

    // Off by default; recovery waits for smart commit.
    assert!(!production(&draft_of(&app, visual)).smart_commit && !production(&draft_of(&app, visual)).smart_commit_recover);
    assert!(rendered(visual, "connection-type-flag-smart-commit".into()) && rendered(visual, "connection-type-flag-smart-commit-recover".into()));
    click(visual, "connection-type-flag-smart-commit-recover".into());
    assert!(!production(&draft_of(&app, visual)).smart_commit_recover, "disabled until smart commit is on");
    click(visual, "connection-type-flag-smart-commit".into());
    assert!(production(&draft_of(&app, visual)).smart_commit);
    click(visual, "connection-type-flag-smart-commit-recover".into());
    assert!(production(&draft_of(&app, visual)).smart_commit_recover);
    assert!(!production(&applied(&app, visual)).smart_commit, "the app has not applied it yet");

    click(visual, "settings-apply".into());
    let settings = applied(&app, visual);
    assert!(production(&settings).smart_commit && production(&settings).smart_commit_recover);
    visual.executor().advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the settings file", |_| path.exists());
    let reloaded = crate::settings::store::load(&path, &Default::default()).settings;
    assert!(production(&reloaded).smart_commit && production(&reloaded).smart_commit_recover, "survives a relaunch");
}

// ---- live: smart commit on a real server ------------------------------------------------------------

/// Auto-commit tab, smart commit on: an UPDATE switches the tab to manual, the server and another tab
/// still see the old value, Roll Back leaves the row unchanged on the server, and a second UPDATE
/// followed by Commit lands.
fn live_smart_commit(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let a = rig.tab;
    let b = rig.second_tab();
    let table = format!("cf_smart_{}", uuid::Uuid::new_v4().to_simple());
    exec_confirmed(&rig.session, &format!("CREATE TABLE {table}(id int PRIMARY KEY, name varchar(40))"));
    exec_confirmed(&rig.session, &format!("INSERT INTO {table} VALUES (1, 'one')"));
    let name = format!("SELECT name FROM {table} WHERE id = 1");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.set_connection_type(smart_type(true));
        rig.activate(a);
        rig.run_on(a, &format!("UPDATE {table} SET name = 'changed' WHERE id = 1"));
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
        assert_eq!((rig.mode(a), rig.modifying(a)), (CommitMode::Manual, 1), "smart commit switched the tab");
        assert!(rig.own_session(a).is_some() && rig.shared_untouched());
        rig.run_on(a, &name);
        assert_eq!(rig.tab_cell(a), "changed", "the tab sees its own change");
        rig.run_on(b, &name);
        assert_eq!(rig.tab_cell(b), "one", "another tab on the same connection does not");
        assert_eq!(rig.log_of(a).len(), 2);

        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        assert_eq!(rig.mode(a), CommitMode::Auto, "back to auto-commit");
        rig.run_on(b, &name);
        assert_eq!(rig.tab_cell(b), "one", "the row is unchanged on the server");
        rig.run_on(a, &name);
        assert_eq!(rig.tab_cell(a), "one");

        rig.run_on(a, &format!("UPDATE {table} SET name = 'kept' WHERE id = 1"));
        assert_eq!(rig.mode(a), CommitMode::Manual);
        rig.click("tx-commit");
        rig.wait_tx("the commit", a, |tx| tx.statements == 0);
        rig.run_on(b, &name);
        assert_eq!(rig.tab_cell(b), "kept", "committed");
        assert_eq!(rig.mode(a), CommitMode::Auto);
    }));
    rig.activate(a);
    rig.app.update(rig.visual, |this, _| this.tab_transaction_closed(a));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_smart_commit_rolls_back_an_update_it_switched_for(cx: &mut TestAppContext) {
    live_smart_commit(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_smart_commit_rolls_back_an_update_it_switched_for(cx: &mut TestAppContext) {
    live_smart_commit(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_smart_commit_rolls_back_an_update_it_switched_for(cx: &mut TestAppContext) {
    live_smart_commit(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// Session state before a write, on a real server: the row the script changes is the one the session
/// said (`search_path` on PostgreSQL, a user variable on MySQL / MariaDB), also when the `SET` ran in
/// an earlier run of the tab.
fn live_session_state(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let a = rig.tab;
    let id = uuid::Uuid::new_v4().to_simple().to_string();
    let (table, schema) = (format!("cf_state_{id}"), format!("cf_sch_{id}"));
    let postgres = kind == ConnectionKind::Postgres;
    // PostgreSQL: the same table name in two schemas; MySQL: one table, the value from a variable.
    if postgres {
        exec_confirmed(&rig.session, &format!("CREATE TABLE public.{table}(id int PRIMARY KEY, v int)"));
        exec_confirmed(&rig.session, &format!("CREATE SCHEMA {schema}"));
        exec_confirmed(&rig.session, &format!("CREATE TABLE {schema}.{table}(id int PRIMARY KEY, v int)"));
        exec_confirmed(&rig.session, &format!("INSERT INTO public.{table} VALUES (1, 0)"));
        exec_confirmed(&rig.session, &format!("INSERT INTO {schema}.{table} VALUES (1, 0)"));
    } else {
        exec_confirmed(&rig.session, &format!("CREATE TABLE {table}(id int PRIMARY KEY, v int)"));
        exec_confirmed(&rig.session, &format!("INSERT INTO {table} VALUES (1, 0)"));
    }
    let reader = rig.second_tab();
    let read = move |rig: &mut SqlRig<'_>, sql: String| {
        rig.run_on(reader, &sql);
        rig.tab_cell(reader)
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.set_connection_type(smart_type(true));
        rig.activate(a);
        let (set, update) = if postgres {
            (format!("SET search_path TO {schema}"), format!("UPDATE {table} SET v = 9 WHERE id = 1"))
        } else {
            ("SET @p = 9".to_owned(), format!("UPDATE {table} SET v = @p WHERE id = 1"))
        };
        let value = |rig: &mut SqlRig<'_>, schema_name: &str| {
            let qualified = if postgres { format!("{schema_name}.{table}") } else { table.clone() };
            read(rig, format!("SELECT v FROM {qualified} WHERE id = 1"))
        };

        // One script: the SET and the write are on the same (transaction) session.
        rig.run_on(a, &format!("{set}; {update}"));
        assert_eq!(rig.mode(a), CommitMode::Manual, "{:?}", rig.status());
        rig.activate(a);
        rig.click("tx-commit");
        rig.wait_tx("the commit", a, |tx| tx.statements == 0);
        if postgres {
            assert_eq!((value(&mut rig, "public"), value(&mut rig, &schema)), ("0".into(), "9".into()), "the script's schema, not public");
        } else {
            assert_eq!(value(&mut rig, ""), "9", "the variable was set on the session of the UPDATE");
        }

        // Two runs: the SET on the shared session, the write after it.
        rig.activate(a);
        if rig.mode(a) == CommitMode::Manual {
            rig.click("tx-mode-auto");
        }
        let reset = if postgres { format!("UPDATE {schema}.{table} SET v = 0 WHERE id = 1") } else { format!("UPDATE {table} SET v = 0 WHERE id = 1") };
        exec_confirmed(&rig.session, &reset);
        rig.run_on(a, &set);
        assert_eq!(rig.mode(a), CommitMode::Auto);
        let update = if postgres { format!("UPDATE {table} SET v = 7 WHERE id = 1") } else { format!("UPDATE {table} SET v = @p + 0 WHERE id = 1") };
        rig.run_on(a, &update);
        assert_eq!(rig.mode(a), CommitMode::Manual, "{:?}", rig.status());
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        if postgres {
            assert_eq!((value(&mut rig, "public"), value(&mut rig, &schema)), ("0".into(), "0".into()), "rolled back, and not in public");
        } else {
            assert_eq!(value(&mut rig, ""), "0", "rolled back");
        }
        rig.activate(a);
        rig.run_on(a, &update);
        rig.click("tx-commit");
        rig.wait_tx("the commit", a, |tx| tx.statements == 0);
        if postgres {
            assert_eq!((value(&mut rig, "public"), value(&mut rig, &schema)), ("0".into(), "7".into()), "the schema the SET named");
        } else {
            assert_eq!(value(&mut rig, ""), "9", "@p was 9 on the new session too");
        }
    }));
    rig.app.update(rig.visual, |this, _| this.tab_transaction_closed(a));
    if postgres {
        exec_confirmed(&rig.session, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE"));
        exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS public.{table}"));
    } else {
        exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_smart_commit_keeps_the_search_path_a_set_gave_the_write(cx: &mut TestAppContext) {
    live_session_state(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_smart_commit_keeps_the_user_variable_a_set_gave_the_write(cx: &mut TestAppContext) {
    live_session_state(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_smart_commit_keeps_the_user_variable_a_set_gave_the_write(cx: &mut TestAppContext) {
    live_session_state(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// MySQL / MariaDB commit by themselves at DDL: smart commit does not switch the tab for it (the
/// change would commit anyway), and a statement the server refuses inside a transaction never is.
fn live_mysql_ddl(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let a = rig.tab;
    let table = format!("cf_ddl_{}", uuid::Uuid::new_v4().to_simple());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.set_connection_type(smart_type(true));
        rig.run_on(a, &format!("CREATE TABLE {table}(id int PRIMARY KEY)"));
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
        assert_eq!(rig.mode(a), CommitMode::Auto, "DDL commits by itself: no transaction is started");
        rig.run_on(a, &format!("CREATE INDEX {table}_i ON {table}(id)"));
        assert_eq!(rig.mode(a), CommitMode::Auto);
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)) && !rig.rendered_now("sql-confirm-run"), "{:?}", rig.status());
        // DML on it does switch.
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (1)"));
        assert_eq!(rig.mode(a), CommitMode::Manual, "{:?}", rig.status());
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        // A script that writes and then runs DDL: the server commits the INSERT at the DDL, the header
        // shows nothing open (and Roll Back does not claim to undo it), and the log says where it was committed.
        let reader = rig.second_tab();
        rig.activate(a);
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (3); CREATE INDEX {table}_k ON {table}(id)"));
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
        assert_eq!(rig.pending(a), 0, "nothing is open any more");
        assert!(!rig.rendered_now("tx-rollback"), "no Roll Back to claim anything");
        rig.run_on(reader, &format!("SELECT count(*) FROM {table} WHERE id = 3"));
        assert_eq!(rig.tab_cell(reader), "1", "the INSERT is on the server");
        assert_eq!(rig.log_of(a).iter().map(|entry| entry.commits).collect::<Vec<_>>(), [true], "the log starts again at the commit");
        // In a tab the user made manual, the log says which statement the server committed by itself.
        rig.activate(a);
        rig.click("tx-mode-manual");
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (2)"));
        rig.run_on(a, &format!("CREATE INDEX {table}_j ON {table}(id)"));
        let log = rig.log_of(a);
        assert_eq!(log.iter().map(|entry| entry.commits).collect::<Vec<_>>(), [true], "the CREATE INDEX committed what came before it: the log starts at it");
        assert_eq!(rig.pending(a), 0, "and the INSERT is no longer something Roll Back would undo");
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        // A DDL the server refuses at parse commits nothing: the INSERT before it is still open, and
        // Roll Back undoes it (in the manual tab, then in a smart one that returns to auto-commit).
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (4)"));
        rig.run_on(a, &format!("CREATE TABLE {table}_bad (id int,)"));
        assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
        assert!(rig.pending(a) >= 1, "a syntax error commits nothing: the INSERT is still open");
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        rig.run_on(reader, &format!("SELECT count(*) FROM {table} WHERE id = 4"));
        assert_eq!(rig.tab_cell(reader), "0", "rolled back, not committed by the failed DDL");
        rig.activate(a);
        rig.click("tx-mode-auto");
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (5)"));
        assert_eq!(rig.mode(a), CommitMode::Manual);
        rig.run_on(a, &format!("CREATE TABLE {table}_bad2 (id int,)"));
        assert!(!rig.rendered_now("sql-confirm-run"));
        assert!(rig.mode(a) == CommitMode::Manual && rig.pending(a) >= 1, "no return to auto-commit: that would commit the INSERT");
        rig.run_on(reader, &format!("SELECT count(*) FROM {table} WHERE id = 5"));
        assert_eq!(rig.tab_cell(reader), "0");
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        assert_eq!(rig.mode(a), CommitMode::Auto);
        // A transaction typed in the smart tab in auto-commit is the tab's: counted, rolled back, and
        // autocommit is off for every statement of it.
        rig.run_on(a, "START TRANSACTION");
        assert_eq!(rig.mode(a), CommitMode::Manual, "{:?}", rig.status());
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (6)"));
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (7)"));
        rig.run_on(reader, &format!("SELECT count(*) FROM {table} WHERE id IN (6, 7)"));
        assert_eq!(rig.tab_cell(reader), "0", "nothing committed on its own");
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        rig.run_on(reader, &format!("SELECT count(*) FROM {table} WHERE id IN (6, 7)"));
        assert_eq!(rig.tab_cell(reader), "0");
        assert_eq!(rig.mode(a), CommitMode::Auto);
    }));
    rig.app.update(rig.visual, |this, _| this.tab_transaction_closed(a));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_smart_commit_leaves_ddl_in_auto_commit(cx: &mut TestAppContext) {
    live_mysql_ddl(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_smart_commit_leaves_ddl_in_auto_commit(cx: &mut TestAppContext) {
    live_mysql_ddl(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

// ---- on a real server: a script's own COMMIT, a versioned comment, the session following its type ----

/// A script that ends the smart transaction with its own COMMIT: the tab is in auto-commit before the next
/// statement (on MySQL / MariaDB that statement turns autocommit on again on the session), what follows a
/// COMMIT and writes starts the next transaction, and turning smart commit off closes the idle session on the
/// server.
fn live_script_commit(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let a = rig.tab;
    let b = rig.second_tab();
    let handle = rig.handle.clone();
    let table = format!("cf_script_{}", uuid::Uuid::new_v4().to_simple());
    let postgres = kind == ConnectionKind::Postgres;
    exec_confirmed(&rig.session, &format!("CREATE TABLE {table}(id int PRIMARY KEY, name varchar(40))"));
    exec_confirmed(&rig.session, &format!("INSERT INTO {table} VALUES (1, 'one'), (2, 'two')"));
    let name = |id: u32| format!("SELECT name FROM {table} WHERE id = {id}");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.set_connection_type(smart_type(true));
        let base = handle.active_session_count();
        let begin = if postgres { "BEGIN" } else { "START TRANSACTION" };
        // BEGIN; write; COMMIT; a read that tells whether the session commits by itself.
        let probe = if postgres { "SELECT 1" } else { "SELECT @@autocommit" };
        rig.run_on(a, &format!("{begin}; UPDATE {table} SET name = 'x' WHERE id = 1; COMMIT; {probe}"));
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
        assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Auto, 0), "the script's COMMIT returned the tab");
        if !postgres {
            assert_eq!(rig.tab_cell(a), "1", "autocommit is on again for the statement after the COMMIT");
        }
        rig.run_on(b, &name(1));
        assert_eq!(rig.tab_cell(b), "x", "committed by the script's COMMIT");

        // ROLLBACK; then a write: that one starts the next transaction and waits for the user.
        rig.run_on(a, &format!("{begin}; UPDATE {table} SET name = 'gone' WHERE id = 2; ROLLBACK; UPDATE {table} SET name = 'z' WHERE id = 2"));
        assert_eq!((rig.mode(a), rig.pending(a)), (CommitMode::Manual, 1), "{:?}", rig.status());
        rig.run_on(b, &name(2));
        assert_eq!(rig.tab_cell(b), "two", "neither the rolled back write nor the one waiting is on the server");
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        assert_eq!(rig.mode(a), CommitMode::Auto);
        if !postgres {
            rig.run_on(a, probe);
            assert_eq!(rig.tab_cell(a), "1", "the next request on the session turned autocommit on");
        }

        // COMMIT AND CHAIN commits and opens the next transaction: the tab keeps it, tracked, instead of going back.
        rig.run_on(a, &format!("{begin}; UPDATE {table} SET name = 'c1' WHERE id = 1; COMMIT AND CHAIN; {probe}"));
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
        assert_eq!(rig.mode(a), CommitMode::Manual, "the chained transaction is open on the session");
        assert!(rig.pending(a) >= 1);
        rig.run_on(b, &name(1));
        assert_eq!(rig.tab_cell(b), "c1", "the first transaction was committed by the chain");
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        assert_eq!(rig.mode(a), CommitMode::Auto);

        // The type stops asking for smart commit: the idle sessions of both tabs are closed on the server.
        assert_eq!((rig.own_sessions(), handle.active_session_count()), (2, base + 2));
        rig.apply_type(|ty| ty.smart_commit = false);
        wait_driver_sessions(&mut rig, &handle, base, "the idle session to be closed");
        assert_eq!((rig.own_sessions(), rig.mode(a)), (0, CommitMode::Auto));
        rig.run_on(a, &format!("UPDATE {table} SET name = 'shared' WHERE id = 2"));
        rig.run_on(b, &name(2));
        assert_eq!(rig.tab_cell(b), "shared", "on the shared session the write commits by itself");
    }));
    rig.app.update(rig.visual, |this, _| this.tab_transaction_closed(a));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_smart_commit_script_commit_returns_the_tab_and_the_type_releases_the_session(cx: &mut TestAppContext) {
    live_script_commit(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_smart_commit_script_commit_returns_the_tab_and_the_type_releases_the_session(cx: &mut TestAppContext) {
    live_script_commit(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_smart_commit_script_commit_returns_the_tab_and_the_type_releases_the_session(cx: &mut TestAppContext) {
    live_script_commit(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// The undo of a smart transaction cannot send its ROLLBACK and `SET autocommit = 1` when a Roll Back is
/// already on its way: the tab is in auto-commit, the session is not yet, and the next request turns it on.
fn live_restore_when_the_undo_sends_nothing(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    use super::transaction::TxEnd;
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let a = rig.tab;
    let table = format!("cf_restore_{}", uuid::Uuid::new_v4().to_simple());
    exec_confirmed(&rig.session, &format!("CREATE TABLE {table}(id int PRIMARY KEY, name varchar(40))"));
    exec_confirmed(&rig.session, &format!("INSERT INTO {table} VALUES (1, 'one')"));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.set_connection_type(smart_type(true));
        rig.run_on(a, &format!("UPDATE {table} SET name = 'x' WHERE id = 1"));
        assert_eq!(rig.mode(a), CommitMode::Manual);
        rig.app.update(rig.visual, |this, cx| {
            let job = this.spawn_end(a, TxEnd::Rollback, cx).expect("on its way");
            this.smart_leave_transaction(a, true, cx);
            finish_end_when_done(a, TxEnd::Rollback, job, cx);
        });
        let app = rig.app.clone();
        wait_until(rig.visual, "the Roll Back to end", |visual| app.read_with(visual, |this, _| !this.transactions.is_ending(a)));
        assert_eq!(rig.mode(a), CommitMode::Auto);
        rig.run_on(a, "SELECT @@autocommit");
        assert_eq!(rig.tab_cell(a), "1", "the session commits by itself again");
    }));
    rig.app.update(rig.visual, |this, _| this.tab_transaction_closed(a));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_smart_commit_restores_autocommit_when_the_undo_sent_nothing(cx: &mut TestAppContext) {
    live_restore_when_the_undo_sends_nothing(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_smart_commit_restores_autocommit_when_the_undo_sent_nothing(cx: &mut TestAppContext) {
    live_restore_when_the_undo_sends_nothing(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// A versioned comment the server skips (its version is above the server's) is a comment: the statement
/// after it is an INSERT, which commits nothing, so the transaction stays open and Roll Back undoes it. The
/// app used to read the comment as code (no version), take the INSERT for DDL that commits by itself, and
/// show nothing open while the row waited on the server.
fn live_versioned_comment(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let a = rig.tab;
    let b = rig.second_tab();
    let table = format!("cf_vcomment_{}", uuid::Uuid::new_v4().to_simple());
    exec_confirmed(&rig.session, &format!("CREATE TABLE {table}(id int PRIMARY KEY)"));
    let count = format!("SELECT count(*) FROM {table}");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.activate(a);
        rig.click("tx-mode-manual");
        rig.run_on(a, &format!("INSERT INTO {table} VALUES (1)"));
        rig.run_on(a, &format!("/*!99999 DROP TABLE nothing */ INSERT INTO {table} VALUES (2)"));
        assert!(!matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
        assert_eq!(rig.pending(a), 2, "nothing was committed by the server: both statements are still open");
        assert_eq!(rig.log_of(a).iter().map(|entry| entry.commits).collect::<Vec<_>>(), [false, false]);
        rig.run_on(b, &count);
        assert_eq!(rig.tab_cell(b), "0");
        rig.activate(a);
        rig.click("tx-rollback");
        rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
        rig.run_on(b, &count);
        assert_eq!(rig.tab_cell(b), "0", "rolled back");
    }));
    rig.app.update(rig.visual, |this, _| this.tab_transaction_closed(a));
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_a_versioned_comment_above_the_server_is_not_code_for_the_transaction_log(cx: &mut TestAppContext) {
    live_versioned_comment(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_a_versioned_comment_above_the_server_is_not_code_for_the_transaction_log(cx: &mut TestAppContext) {
    live_versioned_comment(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}
