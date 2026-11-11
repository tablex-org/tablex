use super::*;

pub(super) fn superseded_driver_page(old: Option<u64>, incoming: Option<u64>) -> Option<u64> {
    incoming.and_then(|incoming| old.filter(|page_id| *page_id != incoming))
}

/// The driver no longer holds the page a tab still shows: its commit
/// invalidated the connection's pages, or a reconnect cleared them. The rows
/// on screen live in the grid's own lease, so only reads by page id fail.
pub(super) fn is_stale_driver_page(error: &engine_driver_host::LoaderError) -> bool {
    matches!(
        error,
        engine_driver_host::LoaderError::DriverRejected { code, .. }
            if matches!(code.as_str(), "inspect.row_not_found" | "inspect.page_not_found")
    )
}

#[derive(Default)]
pub(super) struct DriverRuntime {
    pub(super) sessions: HashMap<String, Arc<engine_driver_host::SharedDriverSession>>,
    pub(super) connection_ids: HashMap<String, u64>,
    /// Capabilities captured when the session connected (immutable per
    /// session), so UI gating never has to lock a busy session.
    pub(super) capabilities: HashMap<String, Arc<engine_driver_api::DriverCapabilities>>,
    /// The driver generation each session connected through. After an update
    /// an old session keeps its own generation while `DriverManagerUi::handles`
    /// follows the newest one; operations (cancel) must use the session's.
    pub(super) session_handles: HashMap<String, Arc<engine_driver_host::DriverHandle>>,
    /// The URI each live session was opened with, credentials included (the password a user
    /// typed for a connection that does not save one lives only here). Memory only: never
    /// written, never logged ([`RuntimeUri`]'s `Debug` hides it). A structure apply opens its
    /// own session with the same credentials.
    pub(super) session_uris: HashMap<String, engine_connect::records::RuntimeUri>,
    /// The server version each live session reported (MySQL numeric form), for the SQL lexer:
    /// versioned `/*!N ... */` comments are code or comment depending on it.
    pub(super) server_versions: HashMap<String, u32>,
    pub(super) query_operations: HashMap<u64, DriverQueryOperation>,
    /// The sessions of SQL tabs in manual commit mode, by tab id (see `app::transaction`). A tab
    /// without one uses its connection's session in `sessions`.
    pub(super) tab_sessions: HashMap<u64, super::transaction::TabSession>,
    /// The window hosting this root, recorded by its first render, so a
    /// background completion can open a dialog without a `Window` in hand.
    pub(super) window: Option<gpui::AnyWindowHandle>,
    /// Disconnects every session when the application quits (see
    /// [`AppRoot::disconnect_sessions_for_quit`]).
    pub(super) _quit: Option<gpui::Subscription>,
    /// Pages handed back, as `(connection, page id)` (tests assert that a
    /// page is released through the connection that owns it).
    #[cfg(test)]
    pub(super) released_pages: Vec<(String, u64)>,
}

pub(super) struct DriverQueryOperation {
    pub(super) handle: Arc<engine_driver_host::DriverHandle>,
    pub(super) connection_id: u64,
    pub(super) operation_id: OperationId,
    /// Keeps the host-allocated operation registered (pins the driver, keeps
    /// the id cancellable) for as long as the request is in flight.
    pub(super) _owner: Arc<engine_driver_host::DriverOperation>,
}

/// A started driver request: the session, a detached runner (so the query
/// does not hold the session lock) and the cancellable operation.
pub(super) struct PreparedDriverQuery {
    pub(super) session: Arc<engine_driver_host::SharedDriverSession>,
    pub(super) runner: engine_driver_host::QueryRunner,
    pub(super) operation: DriverQueryOperation,
}

/// Cancels a driver operation (`operation.cancel`) off the UI thread. A
/// driver without the capability, or an operation that already finished,
/// is not an error here.
pub(super) fn cancel_driver_operation(operation: DriverQueryOperation, cx: &mut gpui::App) {
    let task = cx.background_executor().spawn(async move {
        smol::unblock(move || {
            let _ = operation.handle.cancel_operation(
                operation.connection_id,
                &engine_driver_api::CancelRequest {
                    operation_id: operation.operation_id,
                },
            );
        })
        .await;
    });
    task.detach();
}

impl SqlRun {
    /// `QueryRequest.payload` of this run.
    pub(super) fn payload(&self) -> serde_json::Value {
        match self.kind {
            // An Explain runs read-only whatever the statement is (capability `query.read_only`):
            // the server, not a lexer, refuses a write hidden in an Analyze (`SELECT ... INTO`, a
            // function with side effects) with `query.read_only_violation`.
            RunKind::Explain(explain) => {
                let mut payload = serde_json::json!({"sql": self.sql});
                if self.confirmed {
                    payload["confirmed"] = true.into();
                }
                if explain.read_only || self.read_only {
                    payload["read_only"] = true.into();
                }
                payload
            }
            RunKind::Sql => {
                let mut payload = serde_json::json!({"sql": self.sql});
                if self.confirmed {
                    payload["confirmed"] = true.into();
                }
                if self.read_only {
                    payload["read_only"] = true.into();
                }
                payload
            }
            RunKind::RedisCommand => crate::redis::command_payload(&self.sql, self.confirmed),
            RunKind::RedisKey => crate::redis::key_payload(&self.sql),
        }
    }
}

impl AppRoot {
    /// Capabilities of the driver behind `connection`, without touching the
    /// session lock: the copy cached at connect time, else the loaded handle
    /// of `driver_id` (the same immutable value). `None` when neither exists.
    /// Tells the navigator which object kinds the driver of `connection` lists, so a schema shows
    /// only the groups the driver can answer.
    pub(super) fn sync_navigator_kinds(&mut self, connection: &str) {
        let kinds = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .and_then(|saved| self.driver_capabilities(connection, saved.kind.driver_id()))
            .map(|capabilities| capabilities.navigator_kinds.clone());
        self.navigator.set_driver_kinds(connection, kinds);
    }

    pub(super) fn driver_capabilities(
        &self,
        connection: &str,
        driver_id: &str,
    ) -> Option<&engine_driver_api::DriverCapabilities> {
        self.driver_runtime
            .capabilities
            .get(connection)
            .map(|capabilities| capabilities.as_ref())
            .or_else(|| {
                self.driver_ui
                    .handles
                    .get(driver_id)
                    .map(|handle| handle.capabilities())
            })
    }

    /// What to tell the user when the installed driver `driver_id` lacks a
    /// capability the app needs: which version they have (when the Driver
    /// Manager has read the store) and where to update it.
    pub(super) fn outdated_driver_message(&self, driver_id: &str, name: &str, cannot: &str) -> String {
        let installed = self
            .driver_ui
            .records
            .get(driver_id)
            .map(|record| format!(" ({})", record.version))
            .unwrap_or_default();
        format!("The installed {name} driver{installed} {cannot}; update it in the Driver Manager")
    }

    /// Resolves the live driver session of `connection` and registers a
    /// driver operation for the next request.
    pub(super) fn begin_driver_query(
        &self,
        connection: &str,
    ) -> Result<PreparedDriverQuery, DriverQueryError> {
        self.begin_tab_driver_query(None, connection)
    }

    /// [`Self::begin_driver_query`] for a request of `tab`'s own session (manual commit mode): that
    /// session is the one the request goes to, and there is no falling back to the connection's.
    pub(super) fn begin_tab_driver_query(
        &self,
        tab: Option<u64>,
        connection: &str,
    ) -> Result<PreparedDriverQuery, DriverQueryError> {
        let own = match tab {
            Some(tab) => Some(
                self.own_session(tab, connection)
                    .ok_or_else(|| DriverQueryError::Session("this tab's session is no longer open".into()))?,
            ),
            None => None,
        };
        let (session, handle, connection_id) = match own {
            Some(own) => (Arc::clone(&own.session), Arc::clone(&own.handle), own.connection_id),
            None => {
                let session = self
                    .driver_runtime
                    .sessions
                    .get(connection)
                    .cloned()
                    .ok_or(DriverQueryError::NotConnected)?;
                let driver_id = self
                    .saved_connections
                    .iter()
                    .find(|saved| saved.matches_id(connection))
                    .map(|saved| saved.kind.driver_id())
                    .ok_or(DriverQueryError::ConnectionMissing)?;
                let handle = self
                    .driver_runtime
                    .session_handles
                    .get(connection)
                    .or_else(|| self.driver_ui.handles.get(driver_id))
                    .cloned()
                    .ok_or(DriverQueryError::DriverNotLoaded)?;
                let connection_id = self
                    .driver_runtime
                    .connection_ids
                    .get(connection)
                    .copied()
                    .ok_or(DriverQueryError::ConnectionIdMissing)?;
                (session, handle, connection_id)
            }
        };
        // Lock-free: this runs on the UI thread, and the session lock can be
        // held for a whole navigator round trip. The runner registers the
        // host-allocated operation whose id the request carries.
        let runner = session.query_runner()?;
        Ok(PreparedDriverQuery {
            session,
            operation: DriverQueryOperation {
                handle,
                connection_id,
                operation_id: runner.operation_id(),
                _owner: Arc::clone(runner.operation()),
            },
            runner,
        })
    }
}

/// Why a driver request did not produce a page. `Display` keeps the driver's
/// `driver rejected the request: <code>: <message>` text, which the Error Log
/// and the lost-connection check parse.
#[derive(Debug)]
pub(super) enum DriverQueryError {
    /// The connection has no live driver session.
    NotConnected,
    ConnectionMissing,
    DriverNotLoaded,
    ConnectionIdMissing,
    SessionPoisoned,
    /// A tab's own session could not be opened.
    Session(String),
    /// The host stopped a multi-request batch after the user's Cancel.
    Cancelled,
    /// The loaded driver lacks a capability the request needs.
    Unsupported(&'static str),
    Driver(engine_driver_host::LoaderError),
    Page(engine_driver_host::ArrowPageError),
}

impl std::fmt::Display for DriverQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("Driver session is not connected"),
            Self::ConnectionMissing => f.write_str("Driver connection metadata is missing"),
            Self::DriverNotLoaded => f.write_str("Driver handle is not loaded"),
            Self::ConnectionIdMissing => f.write_str("Driver connection id is missing"),
            Self::SessionPoisoned => f.write_str("Driver session lock poisoned"),
            Self::Session(message) => write!(f, "Could not open a session for this tab: {message}"),
            Self::Cancelled => f.write_str("Operation cancelled"),
            Self::Unsupported(what) => write!(f, "Loaded driver does not support {what}"),
            Self::Driver(error) => error.fmt(f),
            Self::Page(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for DriverQueryError {}

impl From<engine_driver_host::LoaderError> for DriverQueryError {
    fn from(error: engine_driver_host::LoaderError) -> Self {
        Self::Driver(error)
    }
}

impl From<engine_driver_host::ArrowPageError> for DriverQueryError {
    fn from(error: engine_driver_host::ArrowPageError) -> Self {
        Self::Page(error)
    }
}

impl DriverQueryError {
    /// `(code, message, retryable)` when the driver answered `ok: false`.
    fn rejection(&self) -> Option<(&str, &str, bool)> {
        let (Self::Driver(error) | Self::Page(engine_driver_host::ArrowPageError::Driver(error))) =
            self
        else {
            return None;
        };
        match error {
            engine_driver_host::LoaderError::DriverRejected {
                code,
                message,
                retryable,
            } => Some((code, message, *retryable)),
            _ => None,
        }
    }

    pub(super) fn has_code(&self, wanted: &str) -> bool {
        self.rejection().is_some_and(|(code, ..)| code == wanted)
    }

    /// The reason a dangerous statement needs the user's confirmation.
    pub(super) fn confirmation_reason(&self) -> Option<&str> {
        self.has_code(engine_driver_api::sql::codes::QUERY_CONFIRMATION_REQUIRED)
            .then(|| self.rejection().map(|(_, message, _)| message))
            .flatten()
    }

    /// A statement that wrote inside a read-only run (`query.read_only_violation`).
    pub(super) fn is_read_only_violation(&self) -> bool {
        self.has_code(QUERY_READ_ONLY_VIOLATION)
    }

    /// The driver cannot run this read-only now: a transaction of the user's is open on the
    /// session (`query.read_only_unavailable`; MySQL / MariaDB).
    pub(super) fn is_read_only_unavailable(&self) -> bool {
        self.has_code(QUERY_READ_ONLY_UNAVAILABLE)
    }

    pub(super) fn is_not_pageable(&self) -> bool {
        self.has_code(engine_driver_api::sql::codes::QUERY_NOT_PAGEABLE)
    }

    /// The user's own cancel (`<area>.cancelled`), not a failure.
    pub(super) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || self
                .rejection()
                .is_some_and(|(code, ..)| code.ends_with(".cancelled"))
    }

    /// The driver said the session is gone (`<area>.not_connected`). Only call
    /// sites that hold just the stringified error use
    /// `sessions::is_connection_lost_error`.
    pub(super) fn is_connection_lost(&self) -> bool {
        self.rejection()
            .is_some_and(|(code, ..)| crate::sessions::is_connection_lost_code(code))
    }

    /// Text for the tab's status line: an interrupt nobody asked for
    /// (`query.interrupted`, retryable) says that running again may succeed.
    pub(super) fn status_text(&self) -> String {
        match self.rejection() {
            Some((_, _, true)) => {
                format!("{self} (the query was interrupted; run it again)")
            }
            _ => self.to_string(),
        }
    }
}

/// Rows an Explain run asks the driver for: a plan is one JSON / text cell, or a few dozen
/// SQLite rows, and is never paged (the driver's cap per page).
pub(super) const EXPLAIN_ROW_LIMIT: i64 = 100_000;

/// What the result of an EXPLAIN statement is turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExplainRun {
    pub(super) dialect: engine_plan::ExplainDialect,
    pub(super) mode: engine_plan::ExplainMode,
    /// Sent with `payload.read_only`. Always, except the one retry of an Estimate that a driver
    /// refused with `query.read_only_unavailable` (a transaction of the user's is open).
    pub(super) read_only: bool,
}

impl ExplainRun {
    #[cfg(test)]
    pub(super) fn new(dialect: engine_plan::ExplainDialect, mode: engine_plan::ExplainMode) -> Self {
        Self {
            dialect,
            mode,
            read_only: true,
        }
    }

    /// The same Explain without the read-only request. Only an Estimate may drop it (it executes
    /// nothing); Analyze never does.
    pub(super) fn without_read_only(self) -> Option<Self> {
        (self.read_only && self.mode == engine_plan::ExplainMode::Estimate).then_some(Self {
            read_only: false,
            ..self
        })
    }
}

/// What a driver run sends: a SQL statement, a Redis console command, or the
/// typed value page of a Redis key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunKind {
    Sql,
    /// An EXPLAIN statement (a SQL run whose result becomes the tab's plan, not its rows).
    Explain(ExplainRun),
    RedisCommand,
    /// `SqlRun::sql` holds the base64 key.
    RedisKey,
}

impl RunKind {
    pub(super) fn of_tab(tab: &Tab) -> Self {
        if tab.query_mode.is_redis_key() {
            Self::RedisKey
        } else if tab.query_mode.is_redis_command() {
            Self::RedisCommand
        } else {
            Self::Sql
        }
    }

    /// The query kind and the capability the driver must advertise for it.
    pub(super) fn query_kind(self) -> (&'static str, Capability) {
        match self {
            Self::Sql | Self::Explain(_) => ("sql", Capability::QuerySql),
            Self::RedisCommand => (crate::redis::KIND_COMMAND, Capability::QueryRedisCommand),
            Self::RedisKey => (crate::redis::KIND_KEY, Capability::QueryRedisKey),
        }
    }
}

/// One statement (or Redis command) run on the driver route. A confirmation
/// dialog keeps it and resends it with `confirmed` set.
#[derive(Debug, Clone)]
pub(super) struct SqlRun {
    pub(super) tab_id: u64,
    pub(super) kind: RunKind,
    pub(super) connection: String,
    pub(super) sql: String,
    pub(super) database: String,
    /// Query-history context (the tab's collection key).
    pub(super) collection: String,
    pub(super) limit: i64,
    pub(super) offset: i64,
    pub(super) confirmed: bool,
    /// The connection is read-only: the driver runs the statement read-only too
    /// (`payload.read_only`, capability `query.read_only`) and refuses a write with
    /// `query.read_only_violation`, whatever the app's own check let through.
    pub(super) read_only: bool,
}

pub(super) enum DriverMongoResult {
    PageLease(engine_driver_host::PageLease),
    CellBatch(arrow_array::RecordBatch),
}

pub(super) struct DriverPageGuard {
    session: Arc<engine_driver_host::SharedDriverSession>,
    page_id: Option<u64>,
}

/// How long quitting waits for the sessions to say goodbye: a driver whose server stopped
/// answering must not hold the quit (the process ends right after).
const QUIT_DISCONNECT_WAIT: Duration = Duration::from_millis(1500);

impl AppRoot {
    /// What the platform runs when the application quits. The last reference to a session
    /// disconnects it (a network goodbye, which can wait on a dead server), and the quit
    /// would drop those references on the main thread: they are taken out here and dropped
    /// on one worker, and the quit waits for it at most [`QUIT_DISCONNECT_WAIT`] in total.
    pub(super) fn disconnect_sessions_for_quit(
        &mut self,
    ) -> impl std::future::Future<Output = ()> + 'static {
        let sessions: Vec<_> = self.driver_runtime.sessions.drain().map(|(_, session)| session).collect();
        let tab_sessions: Vec<_> = std::mem::take(&mut self.driver_runtime.tab_sessions).into_values().collect();
        async move {
            if sessions.is_empty() && tab_sessions.is_empty() {
                return;
            }
            let (done, finished) = smol::channel::bounded::<()>(1);
            std::thread::spawn(move || {
                drop(sessions);
                drop(tab_sessions);
                let _ = done.send_blocking(());
            });
            smol::future::or(
                async {
                    let _ = finished.recv().await;
                },
                async {
                    smol::Timer::after(QUIT_DISCONNECT_WAIT).await;
                },
            )
            .await;
        }
    }
}

pub(super) fn release_driver_page_async(
    session: Arc<engine_driver_host::SharedDriverSession>,
    page_id: u64,
) {
    session.queue_page_release(page_id);
    smol::spawn(async move {
        smol::unblock(move || {
            drop(session.lock());
        })
        .await;
    })
    .detach();
}

impl DriverPageGuard {
    pub(super) fn new(session: Arc<engine_driver_host::SharedDriverSession>, page_id: u64) -> Self {
        Self {
            session,
            page_id: Some(page_id),
        }
    }

    pub(super) fn disarm(&mut self) {
        self.page_id = None;
    }
}

impl Drop for DriverPageGuard {
    fn drop(&mut self) {
        let Some(page_id) = self.page_id.take() else {
            return;
        };
        release_driver_page_async(Arc::clone(&self.session), page_id);
    }
}

/// Why an Explain run produced no plan.
#[derive(Debug)]
pub(super) enum ExplainRunError {
    /// The query itself failed (refused, cancelled, session lost, ...).
    Query(DriverQueryError),
    /// The server answered, but the answer is not a plan this build reads.
    Plan(engine_plan::ExplainError),
    /// A cell of the result could not be read as text.
    Cell(arrow_schema::ArrowError),
}

impl std::fmt::Display for ExplainRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(error) => error.fmt(f),
            Self::Plan(error) => write!(f, "Could not read the plan: {error}"),
            Self::Cell(error) => write!(f, "Could not read the plan: {error}"),
        }
    }
}

impl std::error::Error for ExplainRunError {}

/// `query.read_only_violation`: a statement wrote inside a read-only run.
const QUERY_READ_ONLY_VIOLATION: &str = "query.read_only_violation";
/// `query.read_only_unavailable`: the session cannot run read-only now (an open user transaction).
const QUERY_READ_ONLY_UNAVAILABLE: &str = "query.read_only_unavailable";

/// The text of every cell of a result batch, as `Plan::parse` takes it. NULL stays `None`. The
/// output cap (`MAX_RAW_BYTES`) is checked on the Arrow buffers first, so a huge answer is
/// refused before a second copy of it is made as text.
pub(super) fn plan_rows(batch: &arrow_array::RecordBatch) -> Result<engine_plan::PlanRows, ExplainRunError> {
    use arrow_array::Array as _;
    use arrow_cast::display::{ArrayFormatter, FormatOptions};
    let bytes: usize = batch.columns().iter().map(|column| column.get_buffer_memory_size()).sum();
    if bytes > engine_plan::MAX_RAW_BYTES {
        return Err(ExplainRunError::Plan(engine_plan::ExplainError::OutputTooLarge {
            bytes,
            limit: engine_plan::MAX_RAW_BYTES,
        }));
    }
    let options = FormatOptions::default();
    let formatters = batch
        .columns()
        .iter()
        .map(|column| ArrayFormatter::try_new(column.as_ref(), &options))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ExplainRunError::Cell)?;
    let rows = (0..batch.num_rows())
        .map(|row| {
            batch
                .columns()
                .iter()
                .zip(&formatters)
                .map(|(column, formatter)| {
                    if column.is_null(row) {
                        Ok(None)
                    } else {
                        Ok(Some(formatter.value(row).try_to_string()?))
                    }
                })
                .collect::<Result<Vec<_>, arrow_schema::ArrowError>>()
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(ExplainRunError::Cell)?;
    Ok(engine_plan::PlanRows {
        columns: batch.schema().fields().iter().map(|field| field.name().clone()).collect(),
        rows,
    })
}

/// Reads the plan out of a finished Explain run. Runs on a worker thread. The driver's page is
/// handed back as soon as its cells are copied out, before the (possibly long) parse.
fn explain_plan(
    explain: ExplainRun,
    result: Result<
        (
            engine_driver_api::QueryPage,
            engine_driver_host::ImportedArrowBatch,
            DriverPageGuard,
        ),
        DriverQueryError,
    >,
) -> Result<engine_plan::Plan, ExplainRunError> {
    let (_, imported, page_guard) = result.map_err(ExplainRunError::Query)?;
    let rows = plan_rows(&imported.batch);
    drop(imported);
    drop(page_guard);
    engine_plan::Plan::parse(explain.dialect, explain.mode, &rows?).map_err(ExplainRunError::Plan)
}

pub(super) use super::driver_manager::DriverManagerUi;

impl AppRoot {
    pub(super) fn release_driver_page(&mut self, tab_id: u64) {
        let Some((connection, page_id)) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| {
                self.workspace
                    .runtime(tab_id)
                    .and_then(|runtime| runtime.driver_page_id)
                    .map(|page_id| (tab.connection_id.clone(), page_id))
            })
        else {
            return;
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.driver_page_id = None;
        }
        #[cfg(test)]
        self.driver_runtime
            .released_pages
            .push((connection.clone(), page_id));
        if let Some(session) = self.tab_session_for(tab_id, &connection) {
            release_driver_page_async(session, page_id);
        }
    }

    /// Runs `request` on the tab's driver session. The query goes through a
    /// detached runner, never under the session lock, so a long statement
    /// cannot queue the navigator (or any other tab) behind it; the lock is
    /// taken only to register and read the finished page.
    pub(super) fn run_driver_sql_query(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        route: super::transaction::TabRoute,
        cx: &mut Context<Self>,
    ) {
        let tab_id = request.tab_id;
        let own_tab = (route == super::transaction::TabRoute::Own).then_some(tab_id);
        // Which session this request runs on: what its failure, if the connection is lost, loses.
        let reporting = super::transaction::Reporting {
            tab_id,
            route,
            own_id: own_tab
                .and_then(|tab| self.own_session(tab, &request.connection))
                .map(|own| own.connection_id),
        };
        let prepared = match self.begin_sql_query(own_tab, &request.connection, request.kind) {
            Ok(prepared) => prepared,
            Err(error) => {
                // Nothing was sent: a tab smart commit switched for it goes back.
                self.smart_commit_undo(tab_id);
                if self.retry_run_after_loss(run, &request, &error, route, cx) {
                    return;
                }
                let connection = request.connection;
                self.transactions_report(reporting, |this| this.apply_driver_arrow_query_result(tab_id, run, connection, Err(error)));
                return;
            }
        };
        // A tab in manual commit mode (PostgreSQL, SQLite) sends a `BEGIN` before its statement. The
        // worker holds the tab's "in flight" mark until it is done, cancelled or not.
        let tx_run = self.tx_sending(&request, route, cx);

        let begin = tx_run.as_ref().and_then(super::transaction::TxRun::begin);
        let in_flight = tx_run.as_ref().map(super::transaction::TxRun::enter);
        let PreparedDriverQuery {
            session,
            runner,
            operation,
        } = prepared;
        let operation_id = operation.operation_id;
        self.driver_runtime.query_operations.insert(tab_id, operation);
        let database = (!request.database.is_empty()).then(|| request.database.clone());
        let request_json = serde_json::to_string(&engine_driver_api::QueryRequest {
            kind: request.kind.query_kind().0.into(),
            operation_id,
            database: database.clone(),
            object: None,
            offset: request.offset.max(0) as u64,
            limit: request.limit.max(0) as u64,
            payload: request.payload(),
        })
        .expect("SQL driver query request serializes");
        // `smol::unblock`: a statement may run for minutes, which must not pin
        // a worker of the shared background pool; the elastic blocking pool
        // exists for exactly this.
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let _in_flight = in_flight;
                let session_for_guard = Arc::clone(&session);
                if let Some((begin, kind)) = begin {
                    // "Already in a transaction" is the usual answer (a warning on PostgreSQL, an
                    // error on SQLite), and an aborted PostgreSQL transaction refuses it too: the
                    // statement itself reports that. Anything else is the statement's failure.
                    if let Err(error) = super::transaction::exec_control(&session, begin, database.as_deref().unwrap_or("")) {
                        if super::transaction::begin_error_matters(kind, &error) {
                            return Err(error);
                        }
                    }
                }
                let page = runner.open_query(&request_json)?;
                // A poisoned lock after `open_query` must not leak the page.
                let mut session = session.lock().map_err(|_| {
                    runner.release_page(page.page_id);
                    DriverQueryError::SessionPoisoned
                })?;
                // Releases the page itself when the session is gone.
                session.adopt_page(&runner, page.page_id)?;
                let page_guard = DriverPageGuard::new(session_for_guard, page.page_id);
                let lease = session.acquire_arrow_page(page.page_id, 0, page.row_count)?;
                // SAFETY: the lease holds the Arrow C Data pair the driver
                // exported for this page; import consumes it once.
                let imported = unsafe { lease.into_record_batch() }?;
                Ok::<_, DriverQueryError>((page, imported, page_guard))
            })
            .await
        });
        let run_request = request;
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            // An Explain result is turned into a plan off the UI thread (a plan can be megabytes
            // of JSON); the page goes back to the driver when that is done.
            if let RunKind::Explain(explain) = run_request.kind {
                let outcome = cx
                    .background_executor()
                    .spawn(async move { explain_plan(explain, result) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.transactions_report(reporting, |this| this.finish_explain_run(run, run_request, explain, outcome, cx));
                    // An Explain ends no run of the transaction code's: the session it read on may be unwanted now.
                    this.release_own_session_if_unwanted(tab_id, cx);
                    cx.notify();
                });
                return;
            }
            let _ = this.update(cx, |this, cx| {
                // Rows it returned, or (a command) affected when the driver knows.
                let rows = result.as_ref().ok().and_then(|(page, ..)| {
                    if page.columns.is_empty() { page.total_count.filter(|_| page.total_is_exact) } else { Some(page.row_count) }
                });
                this.tx_statement_finished(tab_id, tx_run, rows, result.as_ref().err(), cx);
                this.transactions_report(reporting, |this| this.finish_driver_sql_run(run, run_request, result, cx));
                // A script ends in `land_script`; a single statement ends here.
                if !this.is_script_run(tab_id, run) {
                    this.auto_run_ended(tab_id, cx);
                }
                cx.notify();
            });
        });
        self.query_tasks.tasks.insert(tab_id, task);
        cx.notify();
    }

    /// `begin_tab_driver_query`, plus the capabilities `kind` needs.
    fn begin_sql_query(
        &self,
        own_tab: Option<u64>,
        connection: &str,
        kind: RunKind,
    ) -> Result<PreparedDriverQuery, DriverQueryError> {
        let prepared = self.begin_tab_driver_query(own_tab, connection)?;
        let wanted = kind.query_kind().1;
        let supported = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .and_then(|saved| self.driver_capabilities(connection, saved.kind.driver_id()))
            .is_some_and(|capabilities| {
                capabilities.has(wanted)
                    && capabilities.has(Capability::QueryArrowCData)
                    // Explain Analyze executes the statement: only on a driver that can run it
                    // read-only (unknown capabilities fail the check as well).
                    && (!matches!(kind, RunKind::Explain(explain) if explain.mode == engine_plan::ExplainMode::Analyze)
                        || capabilities.has(Capability::QueryReadOnly))
            });
        if supported {
            Ok(prepared)
        } else {
            Err(DriverQueryError::Unsupported(match kind {
                RunKind::Explain(explain) if explain.mode == engine_plan::ExplainMode::Analyze => {
                    "read-only execution (needed by Explain Analyze)"
                }
                RunKind::Sql | RunKind::Explain(_) => "SQL queries",
                RunKind::RedisCommand => "Redis commands",
                RunKind::RedisKey => "Redis key values",
            }))
        }
    }

    /// A finished SQL run: a confirmation request opens the dialog, anything
    /// else is applied to the tab (and recorded in the query history).
    pub(super) fn finish_driver_sql_run(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        result: Result<
            (
                engine_driver_api::QueryPage,
                engine_driver_host::ImportedArrowBatch,
                DriverPageGuard,
            ),
            DriverQueryError,
        >,
        cx: &mut Context<Self>,
    ) {
        // A statement of a script: its outcome is recorded and the next one sent.
        if self.is_script_run(request.tab_id, run) {
            self.finish_script_statement(run, request, result, cx);
            return;
        }
        // A read that lost its session reconnects and runs once more; nothing else is sent twice.
        if let (Some(error), Some(route)) = (
            result.as_ref().err(),
            self.transactions
                .reporting
                .filter(|reporting| reporting.tab_id == request.tab_id)
                .map(|reporting| reporting.route),
        ) {
            if self.retry_run_after_loss(run, &request, error, route, cx) {
                return;
            }
        }
        if let Some(reason) = result
            .as_ref()
            .err()
            .and_then(DriverQueryError::confirmation_reason)
        {
            let reason = reason.to_string();
            self.await_sql_confirmation(run, request, reason, cx);
            return;
        }
        let rows = result.as_ref().ok().map(|(page, ..)| page.row_count as i64);
        let applied =
            self.apply_driver_arrow_query_result(request.tab_id, run, request.connection.clone(), result);
        if applied {
            self.ensure_table_structure(request.tab_id, cx);
            if request.kind == RunKind::RedisKey {
                self.after_redis_key_page(request.tab_id, cx);
            }
        }
        if let (true, Some(rows), true) = (applied, rows, request.kind != RunKind::RedisKey) {
            self.record_query_history(&request, rows, cx);
        }
    }

    /// The driver refused a dangerous statement until the user confirms it.
    /// The run never started, so the tab goes back to what it showed.
    pub(super) fn await_sql_confirmation(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        let tab_id = request.tab_id;
        self.finish_query_timing(tab_id, run);
        let withdrawn = self
            .workspace
            .runtime_mut(tab_id)
            .is_some_and(|runtime| runtime.query.withdraw(run));
        self.finish_query_task(tab_id, run);
        if !withdrawn {
            return;
        }
        // "Confirm dangerous statements" off: the driver's refusal is answered
        // with the confirmation the user pre-approved in Settings.
        if !self.effective_for(&request.connection).confirm_dangerous {
            self.run_confirmed_sql(request, cx);
            return;
        }
        let Some(window) = self.driver_runtime.window else {
            return;
        };
        let root = cx.entity().downgrade();
        // The window cannot be updated while this entity is: open the dialog
        // once the current update has finished.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| {
                    this.open_sql_confirmation(request, reason, window, cx);
                });
            });
        });
    }

    pub(super) fn open_sql_confirmation(
        &mut self,
        request: SqlRun,
        reason: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let request = request.clone();
            dialog
                .width(px(520.0))
                .title(if request.kind == RunKind::RedisCommand {
                    "Run this command?"
                } else {
                    "Run this statement?"
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().debug_selector(|| "sql-confirm-reason".into()).child(reason.clone()))
                        .child(
                            div()
                                .debug_selector(|| "sql-confirm-statement".into())
                                .max_h(px(160.0))
                                .overflow_hidden()
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .child(request.sql.clone()),
                        ),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Run anyway"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div().debug_selector(|| "sql-confirm-cancel".into()).child(
                                gpui_component::dialog::DialogClose::new()
                                    .trigger(|button| button.label("Cancel")),
                            ),
                        )
                        .child(
                            div().debug_selector(|| "sql-confirm-run".into()).child(
                                gpui_component::dialog::DialogAction::new().child(
                                    Button::new("sql-confirm-run").label("Run anyway").danger(),
                                ),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| {
                        this.run_confirmed_sql(request.clone(), cx);
                    });
                    true
                })
        });
        cx.notify();
    }

    /// Resends a confirmed statement as a new run of its tab.
    fn run_confirmed_sql(&mut self, mut request: SqlRun, cx: &mut Context<Self>) {
        let Some(run) = self
            .workspace
            .runtime_mut(request.tab_id)
            .and_then(|runtime| runtime.query.begin())
        else {
            return;
        };
        request.confirmed = true;
        // "Fix" sends this with the failure, as for a run that needed no confirmation.
        let schema = self
            .workspace
            .tab(request.tab_id)
            .and_then(|tab| tab.schema.clone());
        if let Some(runtime) = self.workspace.runtime_mut(request.tab_id) {
            runtime.query.describe(
                run,
                crate::query::RunContext {
                    sql: request.sql.clone(),
                    connection: request.connection.clone(),
                    database: request.database.clone(),
                    schema,
                },
            );
        }
        if self.scenario_ui.service.is_some() {
            self.run_scenario_redis_console(run, request, cx);
            return;
        }
        self.start_driver_sql_run(run, request, cx);
    }

    /// Registers `run` as its tab's task and sends the statement.
    pub(super) fn start_driver_sql_run(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        cx: &mut Context<Self>,
    ) {
        let tab_id = request.tab_id;
        // The tab's session: its own in manual commit mode (opened below when it is not yet), or
        // the connection's. A statement the mode refuses fails the run before anything is sent.
        // Smart commit: a statement that modifies starts a transaction on the tab's own session first.
        let smart = self.smart_commit_switch(&request, cx);
        let route = match self.route_tab_statement(&request) {
            Ok(route) => route,
            Err(reason) => {
                if smart {
                    self.smart_commit_undo(tab_id);
                }
                self.fail_pending_sql(tab_id, run, &reason, cx);
                return;
            }
        };
        if !self.query_tasks.registry.register(tab_id, run) {
            if smart {
                self.smart_commit_undo(tab_id);
            }
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                let _ = runtime.query.fail(run, "Query task already exists");
            }
            self.report_query_failure(tab_id, "Query task already exists");
            return;
        }
        self.query_tasks
            .started_at
            .insert(tab_id, (run, Instant::now()));
        self.arm_query_timeout(tab_id, run, cx);
        self.script_statement_sent(tab_id, run);
        match route {
            super::transaction::TabRoute::Open => self.open_tab_session_then_run(run, request, cx),
            super::transaction::TabRoute::Hold => self.hold_then_run(run, request, cx),
            route => self.run_driver_sql_query(run, request, route, cx),
        }
    }

    /// The global query timeout: one timer per run, only when a limit is set.
    /// It cancels the driver operation through the normal cancel path, so no
    /// driver contract is involved.
    fn arm_query_timeout(&mut self, tab_id: u64, run: crate::query::QueryRunId, cx: &mut Context<Self>) {
        let Some(timeout) = self.effective_for_tab(tab_id).query_timeout else {
            return;
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(timeout).await;
            let _ = this.update(cx, |this, cx| this.expire_sql_run(tab_id, run, timeout, cx));
        })
        .detach();
    }

    /// The run is still the tab's live run after its time limit: stop it.
    pub(super) fn expire_sql_run(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        timeout: Duration,
        cx: &mut Context<Self>,
    ) {
        if !self.query_tasks.registry.owns(tab_id, run) {
            return;
        }
        let operation = self.driver_runtime.query_operations.remove(&tab_id);
        self.query_tasks.tasks.remove(&tab_id);
        self.query_tasks.registry.clear(tab_id, run);
        self.tx_run_cancelled(tab_id, cx);
        let message = format!(
            "Query timed out after {} s (change the limit in Settings \u{203A} Query & Connections)",
            timeout.as_secs()
        );
        // A script's statement fails; the script goes on or stops by its setting.
        if self.is_script_run(tab_id, run) {
            if let Some(operation) = operation {
                cancel_driver_operation(operation, cx);
            }
            self.script_statement_timed_out(tab_id, run, &message, cx);
            cx.notify();
            return;
        }
        self.query_tasks.started_at.remove(&tab_id);
        let failed = self.workspace.runtime_mut(tab_id).is_some_and(|runtime| {
            let failed = runtime.query.fail(run, message.clone());
            if failed {
                runtime.abandon_sql_run();
            }
            failed
        });
        if failed {
            self.report_query_failure(tab_id, &message);
        }
        if let Some(operation) = operation {
            cancel_driver_operation(operation, cx);
        }
        cx.notify();
    }

    /// Opens the statement's connection and sends the statement once it is up
    /// (or fails the run if it cannot be opened). A cancel, close or newer run
    /// of the tab supersedes the queued statement.
    pub(super) fn connect_then_run_sql(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        cx: &mut Context<Self>,
    ) {
        let tab_id = request.tab_id;
        // Reopening a dead session cancels the runs of the connection's tabs: not this one's.
        self.recovery.queueing = Some(tab_id);
        self.ensure_connected(request.connection.clone(), None, cx);
        self.recovery.queueing = None;
        let epoch = matches!(
            self.sessions.status(&request.connection),
            ConnectionStatus::Connecting
        )
        .then(|| self.sessions.generation(&request.connection))
        .flatten();
        let Some(epoch) = epoch else {
            self.fail_pending_sql(
                tab_id,
                run,
                "Could not connect the driver; check connection status",
                cx,
            );
            return;
        };
        self.query_tasks.pending_runs.insert(
            tab_id,
            super::workspace::PendingRun {
                connection: request.connection.clone(),
                epoch,
                run,
                kind: super::workspace::PendingRunKind::Sql(request),
            },
        );
        cx.notify();
    }

    /// The queued statement's connection opened.
    pub(super) fn resume_pending_sql(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        cx: &mut Context<Self>,
    ) {
        let live = self.workspace.runtime(request.tab_id).is_some_and(|runtime| {
            matches!(
                runtime.query.status(),
                crate::query::QueryStatus::Running { run: current } if *current == run
            )
        });
        if live {
            self.start_driver_sql_run(run, request, cx);
        }
    }

    /// A queued statement that cannot run: the tab shows the error and the
    /// Error Log records it.
    pub(super) fn fail_pending_sql(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        let failed = self.workspace.runtime_mut(tab_id).is_some_and(|runtime| {
            let failed = runtime.query.fail(run, crate::sessions::redact_secrets(reason));
            if failed {
                runtime.script_run = None;
            }
            failed
        });
        if failed {
            self.report_query_failure(tab_id, reason);
        }
        self.query_tasks.started_at.remove(&tab_id);
        cx.notify();
    }
}
