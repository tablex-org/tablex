//! A connection that died while nobody was looking (the computer slept, the server restarted, a
//! NAT dropped the idle socket) comes back by itself.
//!
//! * **Wake.** The system's wake notification (GPUI's `on_system_wake`, NSWorkspace's
//!   `DidWakeNotification` on macOS) and, as a fallback, a wall-clock gap between two ticks of a
//!   5 s timer (DBeaver's monitor does the same: a gap over 20 s is a suspend) mark every live
//!   session *stale*. Nothing is sent, nothing closes: the next use (a run, a navigator load)
//!   reopens a stale session first. Sessions of tabs in manual commit mode are never touched.
//! * **Read, reconnect, retry once.** A read-only request that fails with `*.not_connected`
//!   reconnects the connection through the usual connect path and runs again, once. A second
//!   failure shows the error and the Reconnect banner, as before. Anything that may write is never
//!   sent twice: it shows the error and the banner.
//! * **Idle ping.** An open connection that has had no operation for the keep-alive interval
//!   (`query.keep_alive_secs`, 0 = off) gets a trivial request off the UI thread, bounded by
//!   [`PING_TIMEOUT`]. A connection that does not answer is marked lost: the sidebar shows it and
//!   the next use reconnects. A busy connection is never pinged, nor is one pinged twice at once.

use super::*;

use super::transaction::TabRoute;
use std::time::SystemTime;

/// How often the recovery timer ticks.
#[cfg_attr(test, allow(dead_code))]
pub(super) const TICK: Duration = Duration::from_secs(5);
/// A gap between two ticks longer than this is a sleep.
pub(super) const SLEEP_GAP: Duration = Duration::from_secs(20);
/// How long a liveness check may take before the connection counts as dead.
pub(super) const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// What a stale connection says when it is reopened for its next use.
const STALE_REASON: &str = "The computer slept; reconnecting";

#[derive(Default)]
pub(super) struct Recovery {
    /// The wall clock at the last tick (or wake).
    last_tick: Option<SystemTime>,
    /// The run of each tab that already had its automatic reconnect and retry.
    pub(super) retried_runs: HashMap<u64, crate::query::QueryRunId>,
    /// Navigator nodes whose load already had it.
    pub(super) retried_loads: HashSet<NavigatorNodeId>,
    /// Tabs (Mongo find) whose query already had it.
    retried_tabs: HashSet<u64>,
    /// The tab whose run is queued behind the reconnect being started: that reconnect must not
    /// cancel the very run it is for.
    pub(super) queueing: Option<u64>,
    /// Connections with a liveness check under way.
    pinging: HashSet<String>,
    /// Liveness checks started, by connection (tests read it).
    #[cfg(test)]
    pub(super) pings_started: Vec<String>,
    _wake: Option<gpui::Subscription>,
    _ticker: Option<gpui::Task<()>>,
}

/// What a liveness check found.
#[derive(Debug)]
pub(super) enum PingOutcome {
    /// The server answered (an error it answered with is still an answer).
    Alive,
    /// The session is gone, the I/O failed or nothing came back in time.
    Dead(String),
}

/// What a liveness check sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PingKind {
    /// `SELECT 1` through the query path (every SQL driver).
    Sql,
    /// `PING` through the command path (Redis).
    Redis,
    /// The navigator's top level (MongoDB: no SQL, and listing databases is a round trip).
    Navigator,
}

impl PingKind {
    pub(super) fn of(capabilities: &engine_driver_api::DriverCapabilities) -> Self {
        if capabilities.has(Capability::QuerySql) {
            Self::Sql
        } else if capabilities.has(Capability::QueryRedisCommand) {
            Self::Redis
        } else {
            Self::Navigator
        }
    }
}

/// The liveness check itself, on a blocking thread. Never takes the session lock.
fn ping_session(
    session: &engine_driver_host::SharedDriverSession,
    runner: Option<engine_driver_host::QueryRunner>,
    kind: PingKind,
) -> PingOutcome {
    let result = match (kind, runner) {
        (PingKind::Navigator, _) | (_, None) => session
            .list_nav_nodes(
                &serde_json::json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 1})
                    .to_string(),
            )
            .map(|_| ()),
        (kind, Some(runner)) => {
            let (query_kind, payload) = match kind {
                PingKind::Redis => (crate::redis::KIND_COMMAND, serde_json::json!({"command": "PING"})),
                _ => ("sql", serde_json::json!({"sql": "SELECT 1"})),
            };
            let request = serde_json::to_string(&QueryRequest {
                kind: query_kind.into(),
                operation_id: runner.operation_id(),
                database: None,
                object: None,
                offset: 0,
                limit: 0,
                payload,
            })
            .expect("the ping request serializes");
            runner.open_query(&request).map(|page| runner.release_page(page.page_id))
        }
    };
    match result {
        Ok(()) => PingOutcome::Alive,
        // The server answered with a refusal: the connection is up.
        Err(engine_driver_host::LoaderError::DriverRejected { code, .. }) if !crate::sessions::is_connection_lost_code(&code) => {
            PingOutcome::Alive
        }
        Err(error) => PingOutcome::Dead(error.to_string()),
    }
}

impl AppRoot {
    /// Starts watching for a wake and the clock gap. Not under test: tests call
    /// [`Self::on_system_wake`] and [`Self::recovery_tick`] themselves.
    #[cfg_attr(test, allow(dead_code))]
    pub(super) fn start_recovery(&mut self, cx: &mut Context<Self>) {
        self.recovery.last_tick = Some(SystemTime::now());
        let this = cx.entity().downgrade();
        self.recovery._wake = Some(cx.on_system_wake(move |cx| {
            let _ = this.update(cx, |this, cx| this.on_system_wake(cx));
        }));
        self.recovery._ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            let ticked = this.update(cx, |this, cx| this.recovery_tick(SystemTime::now(), Instant::now(), cx));
            if ticked.is_err() {
                break;
            }
        }));
    }

    /// One tick of the timer, at wall-clock time `now` (and monotonic time `at`): a gap since the
    /// last tick means the machine slept (the timer does not run asleep), then idle connections
    /// are checked.
    pub(super) fn recovery_tick(&mut self, now: SystemTime, at: Instant, cx: &mut Context<Self>) {
        let gap = self
            .recovery
            .last_tick
            .and_then(|last| now.duration_since(last).ok())
            .unwrap_or_default();
        self.recovery.last_tick = Some(now);
        if gap > SLEEP_GAP {
            self.mark_sessions_stale(cx);
        }
        self.ping_idle_connections(at, cx);
    }

    /// The system woke. Marks every live session stale; no network, nothing closes.
    pub(super) fn on_system_wake(&mut self, cx: &mut Context<Self>) {
        // The tick that follows must not read the sleep as a second wake.
        self.recovery.last_tick = Some(SystemTime::now());
        self.mark_sessions_stale(cx);
    }

    fn mark_sessions_stale(&mut self, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        if self.sessions.mark_all_stale() > 0 {
            cx.notify();
        }
    }

    /// A stale session is replaced before its next use: it becomes the lost session the connect path
    /// already reopens. `true` when it was stale. The session of a tab in manual commit mode is its
    /// own and stays.
    pub(super) fn reopen_if_stale(&mut self, connection: &str) -> bool {
        if !self.sessions.is_stale(connection) {
            return false;
        }
        let flipped = self.sessions.mark_lost_because(connection, STALE_REASON.to_string());
        if flipped {
            self.sync_connected_ids();
        }
        flipped
    }

    // ---- read, reconnect, retry once ------------------------------------------------------------

    /// `connection` lost its session under a request that may be sent again: it reconnects. `false`
    /// when nobody should (the user disconnected it, or there is no such connection).
    fn reconnect_for_retry(&mut self, connection: &str) -> bool {
        match self.sessions.status(connection) {
            ConnectionStatus::Connected => {
                let flipped = self.mark_connection_lost(connection);
                if flipped {
                    self.sync_connected_ids();
                }
                flipped
            }
            // Another request already noticed: this one waits for the same reconnect.
            ConnectionStatus::Connecting | ConnectionStatus::Failed(_) => true,
            ConnectionStatus::Disconnected => false,
        }
    }

    /// A SQL run (or Redis request) of the connection's shared session failed with `error`: when it
    /// only reads and has not been retried, reconnects and runs it again. `true` when the run is
    /// under way again (the caller reports nothing).
    pub(super) fn retry_run_after_loss(
        &mut self,
        run: crate::query::QueryRunId,
        request: &SqlRun,
        error: &DriverQueryError,
        route: TabRoute,
        cx: &mut Context<Self>,
    ) -> bool {
        let tab_id = request.tab_id;
        // A request on a tab's own session (manual commit) may sit in a transaction that is gone with
        // the session: that is shown, never papered over.
        if !matches!(route, TabRoute::Shared | TabRoute::Hold)
            || self.scenario_ui.service.is_some()
            || !error.is_connection_lost()
            || self.recovery.retried_runs.get(&tab_id) == Some(&run)
            || !self.request_only_reads(request)
            || !self.reconnect_for_retry(&request.connection)
        {
            return false;
        }
        self.recovery.retried_runs.insert(tab_id, run);
        // The failed attempt's task and operation go; the run itself stays the tab's live run.
        self.finish_query_task(tab_id, run);
        self.connect_then_run_sql(run, request.clone(), cx);
        true
    }

    /// Whether sending `request` twice can change nothing: a single pure-read statement (the
    /// classifier the drivers decide `read-only` and `pageable` with), an Explain that does not
    /// run its statement, a Redis key read, a Redis command that only reads.
    fn request_only_reads(&self, request: &SqlRun) -> bool {
        match request.kind {
            RunKind::Sql => engine_driver_kit::sql::classify(&request.sql, &self.sql_lex(request.tab_id)).pageable,
            RunKind::Explain(explain) => explain.mode == engine_plan::ExplainMode::Estimate,
            RunKind::RedisKey => true,
            RunKind::RedisCommand => crate::redis::is_read_only(&request.sql),
        }
    }

    /// A navigator load failed with `error`: reconnects and loads the node again, once. `true` when
    /// the load is under way again (the node keeps showing it loads).
    pub(super) fn retry_load_after_loss(&mut self, request: &LoadRequest, error: &str, cx: &mut Context<Self>) -> bool {
        let connection = request.connection_id();
        if self.scenario_ui.service.is_some()
            || !crate::sessions::is_connection_lost_error(error)
            || !self.driver_runtime.sessions.contains_key(&connection)
        {
            return false;
        }
        let parent = request.parent_id();
        // The second failure of the same node shows.
        if !self.recovery.retried_loads.insert(parent.clone()) {
            self.recovery.retried_loads.remove(&parent);
            return false;
        }
        if !self.reconnect_for_retry(&connection) {
            self.recovery.retried_loads.remove(&parent);
            return false;
        }
        // Reopening the session resets the connection's tree (so the node has no load to register
        // for); the node is expanded again, and loaded, once the new session is up.
        self.ensure_connected(connection.clone(), None, cx);
        self.navigator_ui.pending_loads.entry(connection).or_default().push((request.clone(), 0));
        true
    }

    /// A node loaded: its retry is spent.
    pub(super) fn load_succeeded(&mut self, request: &LoadRequest) {
        self.recovery.retried_loads.remove(&request.parent_id());
    }

    /// A Mongo find tab failed with a lost session: reconnects; the tab runs again with the other
    /// tabs a reconnect re-runs. Once per failure chain.
    pub(super) fn retry_tab_after_loss(&mut self, tab_id: u64, cx: &mut Context<Self>) -> bool {
        let Some(connection) = self
            .workspace
            .tab(tab_id)
            .filter(|tab| tab.query_mode.is_mongo_find())
            .map(|tab| tab.connection_id.clone())
        else {
            return false;
        };
        if self.scenario_ui.service.is_some() {
            return false;
        }
        if !self.recovery.retried_tabs.insert(tab_id) {
            self.recovery.retried_tabs.remove(&tab_id);
            return false;
        }
        if !self.reconnect_for_retry(&connection) {
            self.recovery.retried_tabs.remove(&tab_id);
            return false;
        }
        self.ensure_connected(connection, None, cx);
        true
    }

    /// The tab's query returned: its retry is spent.
    pub(super) fn tab_query_succeeded(&mut self, tab_id: u64) {
        self.recovery.retried_tabs.remove(&tab_id);
    }

    // ---- idle ping --------------------------------------------------------------------------------

    /// Checks every open connection that has been idle for its keep-alive interval.
    pub(super) fn ping_idle_connections(&mut self, at: Instant, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let connected: Vec<String> = self.sessions.connected().map(str::to_string).collect();
        for connection in connected {
            if self.sessions.is_stale(&connection) || self.recovery.pinging.contains(&connection) {
                continue;
            }
            let interval = Duration::from_secs(u64::from(self.effective_for(&connection).keep_alive_secs));
            if interval.is_zero() {
                continue;
            }
            let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else {
                continue;
            };
            let (running, last) = session.activity();
            if running > 0 || at.saturating_duration_since(last) < interval {
                continue;
            }
            self.ping_connection(connection, session, cx);
        }
    }

    fn ping_connection(
        &mut self,
        connection: String,
        session: Arc<engine_driver_host::SharedDriverSession>,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = self.driver_runtime.session_handles.get(&connection).cloned() else {
            return;
        };
        let Some(connection_id) = self.driver_runtime.connection_ids.get(&connection).copied() else {
            return;
        };
        let kind = self
            .driver_runtime
            .capabilities
            .get(&connection)
            .map_or(PingKind::Navigator, |capabilities| PingKind::of(capabilities));
        self.recovery.pinging.insert(connection.clone());
        #[cfg(test)]
        self.recovery.pings_started.push(connection.clone());
        // Registered here, lock-free: the id is what a timed-out check cancels.
        let runner = match kind {
            PingKind::Navigator => None,
            _ => match session.query_runner() {
                Ok(runner) => Some(runner),
                // The session is already gone: the answer is the same as a failed check.
                Err(_) => {
                    self.finish_ping(&connection, &session, PingOutcome::Dead("the session is closed".into()), cx);
                    return;
                }
            },
        };
        let cancel = runner.as_ref().map(|runner| DriverQueryOperation {
            handle,
            connection_id,
            operation_id: runner.operation_id(),
            _owner: Arc::clone(runner.operation()),
        });
        let work_session = Arc::clone(&session);
        let work = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || ping_session(&work_session, runner, kind)).await });
        let timeout = cx.background_executor().timer(PING_TIMEOUT);
        cx.spawn(async move |this, cx| {
            let outcome = smol::future::or(work, async {
                timeout.await;
                PingOutcome::Dead(format!("no answer in {} s", PING_TIMEOUT.as_secs()))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                // Whatever is still stuck is told to stop.
                if matches!(outcome, PingOutcome::Dead(_)) {
                    if let Some(cancel) = cancel {
                        cancel_driver_operation(cancel, cx);
                    }
                }
                this.finish_ping(&connection, &session, outcome, cx);
            });
        })
        .detach();
    }

    /// A liveness check of `session` ended. A dead one marks the connection lost (when it is still
    /// that session's connection): the sidebar shows it, and its next use reconnects.
    pub(super) fn finish_ping(
        &mut self,
        connection: &str,
        session: &Arc<engine_driver_host::SharedDriverSession>,
        outcome: PingOutcome,
        cx: &mut Context<Self>,
    ) {
        self.recovery.pinging.remove(connection);
        let PingOutcome::Dead(why) = outcome else {
            return;
        };
        let current = self
            .driver_runtime
            .sessions
            .get(connection)
            .is_some_and(|live| Arc::ptr_eq(live, session));
        if !current || !self.sessions.is_connected(connection) {
            return;
        }
        if self.mark_connection_lost(connection) {
            self.sync_connected_ids();
            self.report_warning(
                Area::Connect,
                Some(connection),
                format!("The idle connection stopped answering ({why}); it reconnects on its next use"),
            );
            cx.notify();
        }
    }
}
