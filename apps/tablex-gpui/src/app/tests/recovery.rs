// Connection recovery (`app::recovery`): wake detection, stale sessions, reconnect and retry of
// reads, no retry of writes or of a transaction, the idle liveness check and its setting. The
// rigs load the REAL SQLite driver through the host loader and reconnect through the real
// connector (a driver store of their own), so "the session died" is the driver's own
// `session.not_connected`. The live PostgreSQL proofs at the end are `#[ignore]`d and need the
// throwaway container named in `TABLEX_RECOVER_PG_NAME` (see `live_pg`).

use std::time::{Duration as StdDuration, Instant as StdInstant, SystemTime as StdSystemTime};

/// A SQLite rig that can also reconnect: its driver is installed in a store of the rig's own.
fn recovery_rig<'a>(cx: &'a mut TestAppContext, db: &SqliteDb) -> SqlRig<'a> {
    recovery_rig_of(cx, ConnectionKind::Sqlite, ("dbengine-sqlite", ""), &db.uri(), &["sqlite"])
}

fn recovery_rig_of<'a>(
    cx: &'a mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    uri: &str,
    schemes: &[&str],
) -> SqlRig<'a> {
    install_store(SqlRig::new(cx, kind, driver, uri), kind, schemes)
}

/// Installs the rig's driver in a driver store of its own, so a reconnect finds it.
fn install_store<'a>(rig: SqlRig<'a>, kind: ConnectionKind, schemes: &[&str]) -> SqlRig<'a> {
    let dylib = rig._driver_copy.0.clone();
    let store_dir = std::env::temp_dir().join(format!("tablex-recovery-store-{}", uuid::Uuid::new_v4()));
    let store = engine_driver_host::InstalledDriverStore::new(&store_dir);
    store
        .save(engine_driver_host::InstalledDriverRecord {
            id: kind.driver_id().into(),
            version: "0.0.0-test".into(),
            sha256: driver_sha256(&dylib),
            path: dylib,
            team_id: "TEST".into(),
            installed_at: "2026-10-11T00:00:00Z".into(),
            app_abi_version: 2,
            uri_schemes: schemes.iter().map(|scheme| scheme.to_string()).collect(),
            license: "MIT".into(),
            source_repo: String::new(),
            disabled: false,
            ..Default::default()
        })
        .unwrap();
    rig.app.update(rig.visual, |this, _| {
        this.driver_ui.store = Arc::new(store.clone());
        this.driver_ui.installed = drivers::DriverManagerUi::installed_ids(&store);
    });
    rig
}

impl SqlRig<'_> {
    /// The app's current driver session of the rig's connection.
    fn live_session(&self) -> Option<Arc<engine_driver_host::SharedDriverSession>> {
        self.app
            .read_with(self.visual, |this, _| this.driver_runtime.sessions.get(&self.key).cloned())
    }

    /// The server went away: the session the app holds answers `session.not_connected` from now on.
    fn kill_session(&self) {
        self.live_session()
            .expect("a live session")
            .lock()
            .unwrap()
            .disconnect()
            .expect("disconnect");
    }

    fn connection_status(&self) -> ConnectionStatus {
        self.app.read_with(self.visual, |this, _| this.sessions.status(&self.key))
    }

    fn wait_connection(&mut self, label: &'static str, expected: impl Fn(&ConnectionStatus) -> bool) {
        let (app, key) = (self.app.clone(), self.key.clone());
        wait_until(self.visual, label, |visual| app.read_with(visual, |this, _| expected(&this.sessions.status(&key))));
        settle(self.visual);
    }

    fn set_keep_alive(&mut self, seconds: u32) {
        self.app.update(self.visual, |this, cx| {
            let mut next = (*this.settings).clone();
            next.query.keep_alive_secs = seconds;
            this.apply_settings(next, None, cx);
        });
    }

    /// One recovery tick `after` the rig's start (wall clock and monotonic clock together).
    fn tick(&mut self, wall: StdSystemTime, at: StdInstant) {
        self.app.update(self.visual, |this, cx| this.recovery_tick(wall, at, cx));
        settle(self.visual);
    }

    fn pings(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.recovery.pings_started.len())
    }

    fn retried_runs(&self) -> usize {
        self.app.read_with(self.visual, |this, _| this.recovery.retried_runs.len())
    }
}

fn ready(status: &crate::query::QueryStatus) -> bool {
    matches!(status, crate::query::QueryStatus::Ready { .. })
}

// ---- wake -------------------------------------------------------------------------------------

#[gpui::test]
fn a_wake_marks_live_sessions_stale_and_changes_nothing_on_screen(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = recovery_rig(cx, &db);
    let before = rig.live_session().unwrap();
    rig.app.update(rig.visual, |this, cx| this.on_system_wake(cx));
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.sessions.is_stale(&rig.key), "stale");
        assert_eq!(this.sessions.status(&rig.key), ConnectionStatus::Connected, "still shown as connected");
    });
    let after = rig.live_session().unwrap();
    assert!(Arc::ptr_eq(&before, &after), "nothing was closed or replaced");
    assert!(rig.session.try_lock().unwrap().query_runner().is_ok(), "the session itself is alive");
    assert_eq!(rig.pings(), 0, "no network on the wake");
}

#[gpui::test]
fn a_wall_clock_gap_between_ticks_is_a_sleep(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.set_keep_alive(0);
    let (wall, at) = (StdSystemTime::now(), StdInstant::now());
    rig.tick(wall, at);
    // The timer's normal cadence: nothing.
    rig.tick(wall + StdDuration::from_secs(5), at + StdDuration::from_secs(5));
    rig.tick(wall + StdDuration::from_secs(10), at + StdDuration::from_secs(10));
    rig.app.read_with(rig.visual, |this, _| assert!(!this.sessions.is_stale(&rig.key)));
    // The clock jumped an hour between two ticks (the monotonic clock did not: it stops asleep).
    rig.tick(wall + StdDuration::from_secs(3600), at + StdDuration::from_secs(15));
    rig.app.read_with(rig.visual, |this, _| assert!(this.sessions.is_stale(&rig.key), "a gap over 20 s"));
}

#[gpui::test]
fn a_stale_session_reconnects_first_and_the_read_then_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    let old = rig.live_session().unwrap();
    rig.app.update(rig.visual, |this, cx| this.on_system_wake(cx));
    rig.run("SELECT count(*) FROM items");
    rig.wait_status("the read after the wake", ready);
    let new = rig.live_session().unwrap();
    assert!(!Arc::ptr_eq(&old, &new), "the session was replaced before the read");
    assert_eq!(rig.rows(), 1);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(!this.sessions.is_stale(&rig.key), "a fresh session is not stale");
        assert_eq!(this.sessions.status(&rig.key), ConnectionStatus::Connected);
    });
    assert!(rig.log().is_empty(), "nothing failed: {:?}", rig.log());
}

// ---- read: reconnect and retry once -----------------------------------------------------------

#[gpui::test]
fn a_read_on_a_dead_session_reconnects_and_runs_again_once(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    let old = rig.live_session().unwrap();
    rig.kill_session();
    rig.run("SELECT count(*) FROM items");
    rig.wait_status("the retried read", ready);
    assert_eq!(rig.rows(), 1);
    let new = rig.live_session().unwrap();
    assert!(!Arc::ptr_eq(&old, &new), "a new session ran it");
    assert_eq!(rig.connection_status(), ConnectionStatus::Connected);
    assert_eq!(rig.retried_runs(), 1, "it was the one retry");
    assert!(rig.log().is_empty(), "no error was shown or logged: {:?}", rig.log());
    assert!(rig.visual.debug_bounds("connection-lost-reconnect").is_none(), "no Reconnect banner");
}

#[gpui::test]
fn a_read_whose_reconnect_fails_shows_the_error_and_the_reconnect_banner(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.kill_session();
    // The database went away with the connection: the reconnect cannot succeed.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        let saved = this.saved_connections.iter_mut().find(|saved| saved.matches_id(&key)).unwrap();
        saved.uri = "sqlite:///nonexistent-dir-tablex-recovery/none.db".into();
    });
    rig.run("SELECT count(*) FROM items");
    rig.wait_not_running("the read whose reconnect failed");
    settle(rig.visual);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    assert!(matches!(rig.connection_status(), ConnectionStatus::Failed(_)));
    assert!(rig.visual.debug_bounds("connection-lost-reconnect").is_some(), "the Reconnect action is offered");
}

#[gpui::test]
fn a_second_loss_of_the_same_run_is_shown_not_retried(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = recovery_rig(cx, &db);
    let lost = DriverQueryError::Driver(engine_driver_host::LoaderError::DriverRejected {
        code: "query.not_connected".into(),
        message: "gone".into(),
        retryable: false,
    });
    let (tab, key) = (rig.tab, rig.key.clone());
    let (first, second, other_run) = rig.app.update(rig.visual, |this, cx| {
        let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        let request = SqlRun {
            tab_id: tab,
            kind: RunKind::Sql,
            connection: key.clone(),
            sql: "SELECT 1".into(),
            database: String::new(),
            collection: String::new(),
            limit: 200,
            offset: 0,
            confirmed: false,
            read_only: false,
        };
        let first = this.retry_run_after_loss(run, &request, &lost, super::transaction::TabRoute::Shared, cx);
        // The retried run fails again with the same loss.
        let second = this.retry_run_after_loss(run, &request, &lost, super::transaction::TabRoute::Shared, cx);
        // A new run of the tab has its own retry.
        this.workspace.runtime_mut(tab).unwrap().query.cancel();
        let next = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        let other_run = this.retry_run_after_loss(next, &request, &lost, super::transaction::TabRoute::Shared, cx);
        (first, second, other_run)
    });
    assert!(first, "the first loss is retried");
    assert!(!second, "the second loss of the same run is not");
    assert!(other_run, "another run starts over");
}

#[gpui::test]
fn a_write_on_a_dead_session_is_not_retried(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.kill_session();
    rig.run("INSERT INTO items(name) VALUES ('z')");
    rig.wait_not_running("the write on a dead session");
    settle(rig.visual);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    assert_eq!(rig.retried_runs(), 0, "nothing was retried");
    assert_eq!(
        rig.connection_status(),
        ConnectionStatus::Failed(crate::sessions::CONNECTION_LOST_REASON.into()),
        "the loss is shown"
    );
    assert!(rig.visual.debug_bounds("connection-lost-reconnect").is_some(), "with its Reconnect action");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "and the row was never written");
    // Not even a multi-statement text that starts like a read.
    let rig2_run = |rig: &mut SqlRig<'_>, sql: &str| {
        rig.click("connection-lost-reconnect");
        rig.wait_connection("the manual reconnect", |status| *status == ConnectionStatus::Connected);
        rig.kill_session();
        rig.run(sql);
        rig.wait_not_running("the run on a dead session");
    };
    rig2_run(&mut rig, "SELECT 1; DELETE FROM items");
    assert_eq!(rig.retried_runs(), 0, "a script with a write is not retried");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn a_loss_inside_a_transaction_is_shown_never_replaced(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    let tab = rig.tab;
    // Manual commit: the tab gets a session of its own, which holds its transaction.
    rig.click("tx-mode-manual");
    rig.run_on(tab, "INSERT INTO items(name) VALUES ('in-transaction')");
    assert_eq!(rig.pending(tab), 1, "a transaction is open");
    let own = rig.app.read_with(rig.visual, |this, _| this.driver_runtime.tab_sessions.get(&tab).map(|own| Arc::clone(&own.session)));
    let own = own.expect("the tab opened its own session");
    own.lock().unwrap().disconnect().expect("the own session dies");
    let shared_before = rig.live_session().unwrap();

    // Even a read of the transaction is not sent again: what it saw died with it.
    rig.run("SELECT count(*) FROM items");
    rig.wait_not_running("the statement on the lost own session");
    settle(rig.visual);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    assert_eq!(rig.retried_runs(), 0, "a statement of a transaction is never sent again");
    let shared_after = rig.live_session().unwrap();
    assert!(Arc::ptr_eq(&shared_before, &shared_after), "the shared session was not swapped for it");
    assert_eq!(rig.connection_status(), ConnectionStatus::Connected, "the connection itself is fine");
    assert!(rig.tx(tab).unwrap().lost.is_some(), "the transaction is shown as lost");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "and nothing of it was committed");
}

#[gpui::test]
fn a_mongo_find_that_lost_its_session_reconnects_once_per_failure_chain(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    let (key, sql_tab) = (rig.key.clone(), rig.tab);
    let find = rig.app.update(rig.visual, |this, _| {
        this.workspace.open_query_tab_with_metadata(
            "orders",
            TabMetadata {
                connection_id: key.clone(),
                collection: "orders".into(),
                query_mode: Some(QueryMode::MongoFind),
                ..Default::default()
            },
        )
    });
    let (first, sql_tab_retried, second) = rig.app.update(rig.visual, |this, cx| {
        let first = this.retry_tab_after_loss(find, cx);
        // A SQL editor tab is not a find: it is never retried this way.
        let sql_tab_retried = this.retry_tab_after_loss(sql_tab, cx);
        // The retried find lost its session again: that one shows.
        let second = this.retry_tab_after_loss(find, cx);
        (first, sql_tab_retried, second)
    });
    assert!(first, "the first loss reconnects");
    assert!(!sql_tab_retried);
    assert!(!second, "the second is shown");
    rig.wait_connection("the reconnect", |status| *status == ConnectionStatus::Connected);
    // A find that came back whole starts over next time.
    let again = rig.app.update(rig.visual, |this, cx| {
        this.tab_query_succeeded(find);
        this.retry_tab_after_loss(find, cx)
    });
    assert!(again, "a success resets the chain");
}

// ---- idle ping --------------------------------------------------------------------------------

#[gpui::test]
fn an_idle_connection_that_stops_answering_is_marked_lost(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.set_keep_alive(60);
    rig.kill_session();
    let (wall, at) = (StdSystemTime::now(), StdInstant::now());
    rig.tick(wall, at + StdDuration::from_secs(61));
    assert_eq!(rig.pings(), 1, "it was idle for the interval");
    rig.wait_connection("the ping to give up", |status| matches!(status, ConnectionStatus::Failed(_)));
    assert_eq!(rig.connection_status(), ConnectionStatus::Failed(crate::sessions::CONNECTION_LOST_REASON.into()));
    let warned = rig.log().iter().any(|(_, severity, _)| *severity == crate::error_log::Severity::Warning);
    assert!(warned, "the Error Log says it: {:?}", rig.log());
    // The next use reconnects.
    rig.run("SELECT count(*) FROM items");
    rig.wait_status("the read after the lost connection", ready);
    assert_eq!(rig.connection_status(), ConnectionStatus::Connected);
}

#[gpui::test]
fn an_idle_connection_that_answers_is_left_alone(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.set_keep_alive(60);
    let before = rig.live_session().unwrap();
    let (wall, at) = (StdSystemTime::now(), StdInstant::now());
    rig.tick(wall, at + StdDuration::from_secs(61));
    assert_eq!(rig.pings(), 1);
    rig.wait_driver_idle();
    settle(rig.visual);
    assert_eq!(rig.connection_status(), ConnectionStatus::Connected);
    assert!(Arc::ptr_eq(&before, &rig.live_session().unwrap()));
    // The ping itself was activity: another one waits for another interval.
    rig.tick(wall + StdDuration::from_secs(5), Instant::now() + StdDuration::from_secs(10));
    assert_eq!(rig.pings(), 1, "never twice inside the interval");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

#[gpui::test]
fn the_idle_ping_respects_the_interval_and_zero_means_off(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    let (wall, at) = (StdSystemTime::now(), StdInstant::now());
    rig.set_keep_alive(60);
    rig.tick(wall, at + StdDuration::from_secs(30));
    assert_eq!(rig.pings(), 0, "30 s idle is inside the 60 s interval");
    rig.set_keep_alive(0);
    rig.tick(wall + StdDuration::from_secs(5), at + StdDuration::from_secs(100_000));
    assert_eq!(rig.pings(), 0, "0 is off, however long it was idle");
    rig.set_keep_alive(10);
    rig.tick(wall + StdDuration::from_secs(10), at + StdDuration::from_secs(100_000));
    assert_eq!(rig.pings(), 1, "a shorter interval is honoured");
    rig.wait_driver_idle();
}

#[gpui::test]
fn a_per_connection_override_beats_the_global_interval(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.set_keep_alive(0);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        let mut profile = engine_connect::profile::ConnectionProfile::default();
        profile.overrides.set(engine_connect::profile::KEEP_ALIVE_SECS, engine_connect::profile::OverrideValue::Number(30.0));
        this.connection_policy.set_profile(&key, profile);
    });
    let (wall, at) = (StdSystemTime::now(), StdInstant::now());
    rig.tick(wall, at + StdDuration::from_secs(31));
    assert_eq!(rig.pings(), 1, "this connection keeps itself alive though the global switch is off");
    rig.wait_driver_idle();
}

#[gpui::test]
fn a_busy_connection_is_never_pinged(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.set_keep_alive(10);
    let (wall, at) = (StdSystemTime::now(), StdInstant::now());
    let session = rig.live_session().unwrap();
    // An operation is running on the session (a long query would be).
    let busy = session.begin_operation().expect("an operation");
    rig.tick(wall, at + StdDuration::from_secs(500));
    assert_eq!(rig.pings(), 0, "busy: skipped");
    drop(busy);
    rig.tick(wall + StdDuration::from_secs(5), Instant::now() + StdDuration::from_secs(500));
    assert_eq!(rig.pings(), 1, "idle again: checked");
    // One at a time: while that check is still out, the next tick starts no other.
    rig.app.update(rig.visual, |this, cx| {
        this.recovery_tick(wall + StdDuration::from_secs(10), Instant::now() + StdDuration::from_secs(1000), cx);
    });
    assert!(rig.pings() <= 2, "at most one more once the first ended");
    rig.wait_driver_idle();
}

#[gpui::test]
fn a_stale_connection_is_not_pinged(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = recovery_rig(cx, &db);
    rig.set_keep_alive(10);
    rig.app.update(rig.visual, |this, cx| this.on_system_wake(cx));
    rig.tick(StdSystemTime::now(), Instant::now() + StdDuration::from_secs(500));
    assert_eq!(rig.pings(), 0, "it reconnects on its next use instead");
}

// ---- navigator --------------------------------------------------------------------------------

#[gpui::test]
fn a_listing_on_a_dead_session_reconnects_and_loads_once(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = install_store(nav_rig_sqlite(cx, &db), ConnectionKind::Sqlite, &["sqlite"]);
    let key = rig.key.clone();
    rig.kill_session();
    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_connection("the reconnect a listing started", |status| *status == ConnectionStatus::Connected);
    let app = rig.app.clone();
    let node = connection_node(&key);
    wait_until(rig.visual, "the retried listing", |visual| {
        app.read_with(visual, |this, _| {
            this.navigator
                .visible_index(&node)
                .and_then(|index| this.navigator.visible_row_at(index))
                .is_some_and(|row| row.expanded && !matches!(row.load, Some(crate::navigator::NodeLoadState::Failed(_)) | Some(crate::navigator::NodeLoadState::Loading)))
        })
    });
    assert!(rig.navigator_errors().is_empty(), "no error was shown: {:?}", rig.navigator_errors());
}

// ---- the setting ------------------------------------------------------------------------------

#[gpui::test]
fn the_keep_alive_setting_renders_validates_and_persists(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "query");
    assert!(rendered(visual, "setting-query.keep_alive_secs".into()), "the row is on the Query page");
    assert_eq!(applied(&app, visual).query.keep_alive_secs, 60, "on by default, every 60 s");
    let input = visual.debug_bounds("setting-control-query.keep_alive_secs").expect("the number input");
    visual.simulate_click(input.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("30");
    settle(visual);
    assert_eq!(draft_of(&app, visual).query.keep_alive_secs, 30);
    assert!(rendered(visual, "setting-reset-query.keep_alive_secs".into()), "a changed value can be reset");
    click(visual, "settings-apply".into());
    assert_eq!(applied(&app, visual).query.keep_alive_secs, 30);
    visual.executor().advance_clock(StdDuration::from_millis(400));
    wait_until(visual, "settings.json", |_| path.exists());
    let on_disk = crate::settings::store::load(&path, &crate::settings::store::Legacy::default());
    assert_eq!(on_disk.settings.query.keep_alive_secs, 30, "persisted");
    // 0 is a value of its own: off.
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("0");
    settle(visual);
    assert_eq!(draft_of(&app, visual).query.keep_alive_secs, 0);
    assert!(!rendered(visual, "setting-error-query.keep_alive_secs".into()), "0 is valid");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn the_keep_alive_setting_is_overridable_per_connection_and_off_at_zero() {
    let descriptor = crate::settings::registry::find("query.keep_alive_secs").expect("registered");
    assert!(descriptor.overridable, "like page size, a connection may override it");
    let mut settings = Settings::default();
    assert_eq!(settings.for_connection(&Default::default()).keep_alive_secs, 60);
    settings.query.keep_alive_secs = 0;
    assert_eq!(settings.for_connection(&Default::default()).keep_alive_secs, 0);
    let mut profile = engine_connect::profile::ConnectionProfile::default();
    profile.overrides.set("query.keep_alive_secs", engine_connect::profile::OverrideValue::Number(15.0));
    assert_eq!(settings.for_connection(&profile).keep_alive_secs, 15);
}

// ---- live (need a throwaway PostgreSQL container: see `live_pg`) --------------------------------

/// `(container name, uri)` of the throwaway PostgreSQL the live proofs use.
fn live_pg() -> Option<(String, String)> {
    let name = std::env::var("TABLEX_RECOVER_PG_NAME").ok()?;
    let uri = std::env::var("TABLEX_RECOVER_PG_URI").ok()?;
    assert!(name.starts_with("tablex-recover-"), "only the container of this proof is ever touched");
    Some((name, uri))
}

fn docker(name: &str, verb: &str) {
    assert!(name.starts_with("tablex-recover-"));
    let output = std::process::Command::new("docker").args([verb, name]).output().expect("docker");
    assert!(output.status.success(), "docker {verb} {name}: {}", String::from_utf8_lossy(&output.stderr));
}

/// Waits (real time) until the server answers a new connection again.
fn wait_pg_ready(uri: &str) {
    let start = StdInstant::now();
    loop {
        let reachable = url::Url::parse(uri)
            .ok()
            .and_then(|url| Some((url.host_str()?.to_string(), url.port()?)))
            .is_some_and(|(host, port)| std::net::TcpStream::connect_timeout(
                &std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port)).unwrap().next().unwrap(),
                StdDuration::from_secs(1),
            ).is_ok());
        if reachable {
            std::thread::sleep(StdDuration::from_secs(2));
            return;
        }
        assert!(start.elapsed() < StdDuration::from_secs(60), "the container did not come back");
        std::thread::sleep(StdDuration::from_millis(500));
    }
}

fn pg_rig<'a>(cx: &'a mut TestAppContext, uri: &str) -> SqlRig<'a> {
    recovery_rig_of(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), uri, &["postgres", "postgresql"])
}

#[gpui::test]
#[ignore = "needs TABLEX_RECOVER_PG_NAME / TABLEX_RECOVER_PG_URI (container tablex-recover-pg)"]
fn live_a_paused_server_is_noticed_by_the_idle_ping(cx: &mut TestAppContext) {
    let Some((name, uri)) = live_pg() else { return };
    let mut rig = pg_rig(cx, &uri);
    rig.set_keep_alive(10);
    docker(&name, "pause");
    let (wall, at) = (StdSystemTime::now(), StdInstant::now());
    rig.tick(wall, at + StdDuration::from_secs(11));
    assert_eq!(rig.pings(), 1);
    // The ping has 10 s; the test clock only moves when told to.
    std::thread::sleep(StdDuration::from_millis(300));
    rig.visual.executor().advance_clock(StdDuration::from_secs(11));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.wait_connection("the paused server to be given up on", |status| matches!(status, ConnectionStatus::Failed(_)));
    }));
    docker(&name, "unpause");
    result.expect("a connection that stops answering is marked lost");
    assert!(rig.log().iter().any(|(_, severity, _)| *severity == crate::error_log::Severity::Warning), "{:?}", rig.log());
}

#[gpui::test]
#[ignore = "needs TABLEX_RECOVER_PG_NAME / TABLEX_RECOVER_PG_URI (container tablex-recover-pg)"]
fn live_a_read_after_a_server_restart_reconnects_without_the_user_and_a_write_does_not(cx: &mut TestAppContext) {
    let Some((name, uri)) = live_pg() else { return };
    let mut rig = pg_rig(cx, &uri);
    rig.run("SELECT 1");
    rig.wait_status("the first read", ready);
    docker(&name, "restart");
    wait_pg_ready(&uri);
    let old = rig.live_session().unwrap();

    // A read: reconnected and run again, nobody clicked anything.
    rig.run("SELECT 2");
    rig.wait_status("the read after the restart", ready);
    assert!(!Arc::ptr_eq(&old, &rig.live_session().unwrap()), "a new session");
    assert_eq!(rig.retried_runs(), 1);
    assert_eq!(rig.connection_status(), ConnectionStatus::Connected);
    assert!(rig.log().is_empty(), "{:?}", rig.log());

    // A write after the next restart: shown, not sent again.
    rig.run("CREATE TABLE IF NOT EXISTS tablex_recover_probe(n int)");
    rig.wait_status("the setup write", ready);
    docker(&name, "restart");
    wait_pg_ready(&uri);
    let retried = rig.retried_runs();
    rig.run("INSERT INTO tablex_recover_probe VALUES (1)");
    rig.wait_not_running("the write after the restart");
    settle(rig.visual);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    assert_eq!(rig.retried_runs(), retried, "the write was not retried");
    assert!(matches!(rig.connection_status(), ConnectionStatus::Failed(_)));
    assert!(rig.visual.debug_bounds("connection-lost-reconnect").is_some());
    // The user reconnects and finds the table empty: the write never reached the server.
    rig.click("connection-lost-reconnect");
    rig.wait_connection("the manual reconnect", |status| *status == ConnectionStatus::Connected);
    rig.run("SELECT * FROM tablex_recover_probe");
    rig.wait_status("the probe table", ready);
    assert_eq!(rig.rows(), 0);
    rig.run("DROP TABLE tablex_recover_probe");
    rig.wait_status("the cleanup", ready);
}
