//! Running a structure change-set on its connection (AppRoot side of the review
//! dialog's Apply) and refreshing everything it changed.
//!
//! **Isolation.** Every apply runs on a *dedicated driver session*: a second
//! session of the connection, opened for the batch with the connection's own
//! profile and credentials, and closed afterwards. It is never the user's
//! interactive session, so a transaction the user has open in a SQL tab is
//! neither committed nor rolled back by the apply (MySQL's implicit commit of
//! DDL included), and nothing the user runs interleaves with the batch. At most
//! one apply runs per connection, whoever asked ([`ApplyTarget`]: a diagram or a Properties
//! tab; [`Applies`] registry); a second is refused.
//!
//! On that session, before anything is changed:
//! 1. `sql_mode` is read (MySQL, MariaDB) and the SQL is generated again for that
//!    mode; it must equal the SQL that was reviewed (`NO_BACKSLASH_ESCAPES`
//!    changes how typed text is lexed and quoted).
//! 2. Every table the change-set touches is described again; a change whose
//!    starting structure no longer matches is *stale* and nothing runs
//!    (`sql_ddl::schema::stale`).
//! 3. Lock waits are bounded (`SET LOCAL lock_timeout`, `lock_wait_timeout`,
//!    `busy_timeout`; ClickHouse has no equivalent) and a statement timeout is
//!    off, so an apply neither waits for ever nor is cut short.
//!
//! The statements then go as ordinary SQL queries (`confirmed`: the review
//! dialog is the confirmation), in `BEGIN` / `COMMIT` for dialects with
//! transactional DDL. Cancel stops the running statement through the driver's
//! operation id and rolls back where it can.

use engine_connect::records::RuntimeUri;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use engine_driver_host::{DriverHandle, DriverSession};
use gpui::Context;

use super::live::live_table;
use super::{ApplyRequest, EditScope, Wording};
use crate::app::results::diagram::view::DiagramKind;
use crate::app::AppRoot;
use crate::connections::ConnectionKind;
use crate::error_log::Area;
use crate::sql_ddl::apply::{apply_checked, Outcome, StatementRunner};
use crate::sql_ddl::schema::{stale, LiveSchema, LiveTable};
use crate::sql_ddl::{dialect_for, dialect_in_mode, plan_with_lock_timeout, Change, DdlDialect, Planned, TableId, DEFAULT_LOCK_TIMEOUT_SECS};

/// Cancels the statement that is running (the driver call): shared so the Cancel
/// button can use it while the apply thread waits for the statement.
type Canceller = Arc<dyn Fn() + Send + Sync>;

/// Shared between an apply and the Cancel button.
#[derive(Default)]
pub(in crate::app) struct ApplyControl {
    cancelled: AtomicBool,
    running: Mutex<Option<Canceller>>,
}

impl ApplyControl {
    pub(in crate::app) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Asks the apply to stop: no further statement starts, and the one that
    /// runs is cancelled through the driver. Blocks on the driver call: run it
    /// off the UI thread.
    pub(in crate::app) fn cancel(&self) {
        // The flag first, then the registered statement: a statement registers first and then looks at the
        // flag (`register`), so one of the two always sees the other and a cancel cannot fall between them.
        self.cancelled.store(true, Ordering::SeqCst);
        let running = self.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if let Some(cancel) = running {
            cancel();
        }
    }

    /// Makes `canceller` the running statement's. `Err` when the user has cancelled already (the
    /// statement must not start).
    pub(in crate::app) fn register(&self, canceller: Canceller) -> Result<(), Cancelled> {
        *self.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(canceller);
        if self.is_cancelled() {
            self.clear_running();
            return Err(Cancelled);
        }
        Ok(())
    }

    pub(in crate::app) fn clear_running(&self) {
        *self.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// The user cancelled before the statement started.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::app) struct Cancelled;

const CANCELLED: &str = "cancelled by the user";

/// Who asked for an apply and gets its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ApplyTarget {
    /// The review dialog of the diagram on this tab.
    Diagram { tab_id: u64, kind: DiagramKind },
    /// The review dialog of the table Properties editor `edit` (its entity id) on this tab. The
    /// answer goes to that editor, whatever the tab shows by then.
    Properties { tab_id: u64, edit: u64 },
    /// The review dialog of the navigator's object action `edit` (its entity id): drop, rename,
    /// truncate, new table, new schema / database. It belongs to no tab.
    Object { edit: u64 },
}

/// The table an apply was asked for, captured when it starts: the tab may show another table (or
/// be gone) when the answer comes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct TableOrigin {
    pub(in crate::app) connection: String,
    pub(in crate::app) database: String,
    /// `""` for an engine without schemas.
    pub(in crate::app) schema: String,
    pub(in crate::app) object: String,
}

impl ApplyTarget {
    /// Where the error log files its entries. A Properties apply stays in `Area::App`: the log has
    /// no area for the results grid's Properties presentation.
    pub(in crate::app) fn area(self) -> Area {
        match self {
            Self::Diagram { .. } => Area::Diagram,
            Self::Properties { .. } => Area::App,
            Self::Object { .. } => Area::Admin,
        }
    }

    /// How the messages of this apply speak of the place it was asked from.
    pub(in crate::app) fn wording(self) -> Wording {
        match self {
            Self::Diagram { .. } => Wording::DIAGRAM,
            Self::Properties { .. } => Wording::PROPERTIES,
            Self::Object { .. } => Wording::OBJECT,
        }
    }
}

/// Who runs an apply and the handle that cancels it.
type Running = (ApplyTarget, Arc<ApplyControl>);

/// The applies running now, one per connection. Registering returns a guard that
/// frees the connection when it drops (a finished job, a refusal, a panic).
#[derive(Clone, Default)]
pub(in crate::app) struct Applies(Arc<Mutex<HashMap<String, Running>>>);

impl Applies {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Running>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Who runs the apply on `connection`.
    pub(in crate::app) fn running(&self, connection: &str) -> Option<ApplyTarget> {
        self.map().get(connection).map(|(target, _)| *target)
    }

    pub(in crate::app) fn control(&self, connection: &str) -> Option<Arc<ApplyControl>> {
        self.map().get(connection).map(|(_, control)| Arc::clone(control))
    }

    #[cfg(test)]
    pub(in crate::app) fn len(&self) -> usize {
        self.map().len()
    }

    /// Takes the connection for `target`; `Err` with the one that holds it.
    pub(in crate::app) fn begin(&self, connection: &str, target: ApplyTarget, control: &Arc<ApplyControl>) -> Result<ApplyGuard, ApplyTarget> {
        let mut map = self.map();
        if let Some((running, _)) = map.get(connection) {
            return Err(*running);
        }
        map.insert(connection.to_owned(), (target, Arc::clone(control)));
        Ok(ApplyGuard { applies: self.clone(), connection: connection.to_owned() })
    }
}

/// Frees a connection's apply slot on drop.
pub(in crate::app) struct ApplyGuard {
    applies: Applies,
    connection: String,
}

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        self.applies.map().remove(&self.connection);
    }
}

/// Everything the apply needs besides the session.
pub(in crate::app) struct ApplyJob {
    pub(in crate::app) kind: ConnectionKind,
    pub(in crate::app) scope: EditScope,
    pub(in crate::app) changes: Vec<Change>,
    /// The script the user reviewed: what is applied must be the same text.
    pub(in crate::app) reviewed_script: String,
    pub(in crate::app) lock_timeout_secs: u32,
}

/// The session an apply runs on: statements, `sql_mode`, and catalog reads.
pub(in crate::app) trait ApplySession: StatementRunner {
    /// `@@sql_mode` of a MySQL / MariaDB server; `None` for other servers.
    fn sql_mode(&mut self) -> Result<Option<String>, String>;
    /// Whether this session can describe tables (a test double may not).
    fn describes(&self) -> bool {
        true
    }
    /// The described table; `None` when it does not exist.
    fn describe(&mut self, table: &TableId, scope: &EditScope) -> Result<Option<LiveTable>, String>;
    /// What a message tells the user to do when the table changed under the change-set.
    fn refresh_hint(&self) -> &'static str {
        Wording::DIAGRAM.refresh
    }
}

/// An apply that ran (it may have failed): what was planned and what happened.
pub(in crate::app) struct ApplyRun {
    pub(in crate::app) planned: Planned,
    pub(in crate::app) outcome: Outcome,
    /// What the server's `sql_mode` was, when it was read.
    pub(in crate::app) sql_mode: Option<String>,
}

/// An apply that did not start, and why.
pub(in crate::app) struct Refusal {
    pub(in crate::app) message: String,
    pub(in crate::app) sql_mode: Option<String>,
}

/// The tables whose current structure a change-set depends on: every table it
/// touches under the name the database has (a table the change-set creates is not there yet).
fn touched_tables(changes: &[Change]) -> Vec<TableId> {
    let mut tables: Vec<TableId> = Vec::new();
    for change in changes {
        let table = match change {
            Change::CreateTable { .. } => continue,
            // A namespace is no table; a view is left to the server (no catalog check describes views).
            Change::CreateNamespace { .. } | Change::DropView { .. } | Change::RenameView { .. } => continue,
            Change::RenameTable { table, .. } => table.clone(),
            other => match other.table() {
                Some(table) => crate::sql_ddl::schema::original_table(changes, table),
                None => continue,
            },
        };
        let created = changes.iter().any(|c| matches!(c, Change::CreateTable { table: t, .. } if Some(t) == change.table()));
        if !created && !tables.contains(&table) {
            tables.push(table);
        }
    }
    tables
}

/// The tables the check reads: the ones the change-set changes, plus the names it
/// creates or renames to (they must still be free).
fn checked_tables(changes: &[Change]) -> Vec<TableId> {
    let mut tables = touched_tables(changes);
    for change in changes {
        let target = match change {
            Change::CreateTable { table, .. } => table.clone(),
            Change::RenameTable { table, to } => TableId { schema: table.schema.clone(), name: to.clone() },
            _ => continue,
        };
        if !tables.contains(&target) {
            tables.push(target);
        }
    }
    tables
}

/// The checks and the run, on `session`. Nothing is changed unless every check passes.
///
/// The catalog is re-read *inside* the apply's transaction (where the dialect has
/// one), after the session's lock wait is bounded and, on PostgreSQL, after the
/// touched tables are locked against other DDL: what the check saw is what the
/// statements then change.
pub(in crate::app) fn run_apply<S: ApplySession>(job: &ApplyJob, session: &mut S) -> Result<ApplyRun, Refusal> {
    let refuse = |message: String, sql_mode: Option<String>| Refusal { message, sql_mode };
    let sql_mode = match session.sql_mode() {
        Ok(mode) => mode,
        Err(_) if session.cancelled() => return Err(refuse("Cancelled before anything was changed.".into(), None)),
        Err(error) => return Err(refuse(format!("Could not read the server's sql_mode: {error}"), None)),
    };
    let dialect: Box<dyn DdlDialect> = dialect_in_mode(job.kind, sql_mode.as_deref().unwrap_or(""))
        .ok_or_else(|| refuse("This connection has no SQL structure to change.".into(), sql_mode.clone()))?;
    let planned = plan_with_lock_timeout(&*dialect, &job.changes, job.lock_timeout_secs);
    if let Some((index, error)) = planned.first_error() {
        return Err(refuse(format!("{}: {error}", job.changes[index].title()), sql_mode));
    }
    if planned.script() != job.reviewed_script {
        return Err(refuse(
            "The SQL for this server differs from the SQL that was reviewed (the server's sql_mode changes how text is quoted). Open Changes and review it again."
                .into(),
            sql_mode,
        ));
    }
    let check = |session: &mut S| -> Result<(), Refusal> {
        if !session.describes() {
            return Ok(());
        }
        let nothing_changed = |message: String| refuse(message, sql_mode.clone());
        let locks = dialect.check_locks(&touched_tables(&job.changes)).map_err(|error| nothing_changed(error.to_string()))?;
        for sql in &locks {
            session.run(sql).map_err(|error| {
                nothing_changed(if session.cancelled() {
                    "Cancelled before anything was changed.".to_owned()
                } else {
                    format!(
                        "Could not lock the tables to check them ({error}). Nothing was changed; the table may have changed: {}.",
                        session.refresh_hint()
                    )
                })
            })?;
        }
        let mut live = LiveSchema::new(false);
        for table in checked_tables(&job.changes) {
            if session.cancelled() {
                return Err(nothing_changed("Cancelled before anything was changed.".into()));
            }
            match session.describe(&table, &job.scope).map_err(|error| nothing_changed(format!("Could not describe {table}: {error}")))? {
                Some(described) => live.present(table, described),
                None => live.absent(table),
            }
        }
        let stale = stale(&job.changes, &live);
        if stale.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = stale.iter().map(|(index, reason)| format!("{}: {reason}", job.changes[*index].title())).collect();
        Err(nothing_changed(list.join("\n")))
    };
    // The statements that carry a password are made with its real value only here, on the apply's thread: `planned` (what
    // the report and the log show) keeps the placeholder.
    let exec = crate::sql_account::executable(&*dialect, &job.changes, &planned).map_err(|error| refuse(format!("{}: {error}", error_title(&job.changes)), sql_mode.clone()))?;
    let mut outcome = apply_checked(exec.planned(), session, check)?;
    // A server repeats the text around a mistake in its message: the password is not in what is kept.
    let failed_change = outcome.failed.as_ref().and_then(|failure| exec.planned().change_of_step(failure.step));
    if let Some(failure) = &mut outcome.failed {
        failure.message = exec.clean_failure(failed_change, &failure.message);
    }
    if let Some(error) = &mut outcome.rollback_error {
        *error = exec.scrub(error);
    }
    Ok(ApplyRun { planned, outcome, sql_mode })
}

/// What a refusal to make the executable text names (no password in it).
fn error_title(changes: &[Change]) -> String {
    changes.iter().find_map(|change| matches!(change, Change::Account(_)).then(|| change.title())).unwrap_or_else(|| "The change".to_owned())
}

/// The request as the JSON the driver is given. The request holds the statement twice (the value and its JSON text); the text is a
/// [`engine_net::Secret`] (wiped when it drops) and the copy in the request is wiped here, so a statement with a password in it is
/// not left behind in either. (What the driver's own call copies, a C string made of the text, is out of this code's reach.)
pub(in crate::app) fn request_json(request: &mut engine_driver_api::QueryRequest) -> Result<engine_net::Secret, serde_json::Error> {
    let json = engine_net::Secret::new(serde_json::to_string(&*request)?);
    if let Some(serde_json::Value::String(text)) = request.payload.get_mut("sql") {
        crate::sql_account::password::wipe_string(text);
    }
    Ok(json)
}

/// A driver session opened for one apply.
pub(in crate::app) struct DriverApplySession {
    session: DriverSession,
    handle: Arc<DriverHandle>,
    control: Arc<ApplyControl>,
    database: Option<String>,
    kind: ConnectionKind,
    refresh: &'static str,
    /// This session's own SSH tunnel (one tunnel per database session). Declared last: it goes
    /// after the session has disconnected through it.
    _tunnel: Option<engine_connect::connect::LiveTunnel>,
}

impl DriverApplySession {
    /// Opens a second session on `handle` (the driver the connection uses) with `uri`, through
    /// a tunnel of its own when the connection has one (`expects_tunnel`: a connection configured
    /// for a tunnel never gets a direct session).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::app) fn connect(
        connector: &engine_connect::connect::Connector,
        prompts: &Arc<dyn engine_connect::connect::TunnelPrompts>,
        handle: Arc<DriverHandle>,
        saved: &crate::connections::SavedConnection,
        uri: &RuntimeUri,
        connect_timeout_secs: u32,
        database: Option<String>,
        control: Arc<ApplyControl>,
        tunnel: Option<crate::ssh_tunnel::TunnelAccess>,
    ) -> Result<Self, String> {
        let kind = saved.kind;
        let (session, tunnel) = connector
            .open_session(&handle, saved, uri, connect_timeout_secs, tunnel, prompts)
            .map_err(|failure| crate::sessions::redact_secrets(&failure.message()))?;
        Ok(Self { session, handle, control, database, kind, refresh: Wording::DIAGRAM.refresh, _tunnel: tunnel })
    }

    /// The session speaks of `wording` in its refusals (the default is the diagram's).
    pub(in crate::app) fn worded(mut self, wording: Wording) -> Self {
        self.refresh = wording.refresh;
        self
    }

    /// [`Self::connect`] for tests: opening a second session needs no driver store, and nothing
    /// is asked (a tunnel is expected exactly when an access is given).
    #[cfg(test)]
    pub(in crate::app) fn connect_for_test(
        handle: Arc<DriverHandle>,
        uri: &str,
        database: Option<String>,
        kind: ConnectionKind,
        control: Arc<ApplyControl>,
        tunnel: Option<crate::ssh_tunnel::TunnelAccess>,
    ) -> Result<Self, String> {
        let connector = engine_connect::connect::Connector::new(Arc::new(engine_driver_host::InstalledDriverStore::new(
            std::env::temp_dir(),
        )));
        let prompts: Arc<dyn engine_connect::connect::TunnelPrompts> = Arc::new(engine_connect::connect::RefusePrompts);
        let saved = crate::connections::SavedConnection {
            id: uuid::Uuid::nil(),
            name: String::new(),
            uri: String::new(),
            kind,
            save_password: false,
            is_pinned: false,
            disabled: false,
            // A tunnel is required exactly when the test hands one over.
            ssh_tunnel: tunnel.is_some(),
        };
        let uri = RuntimeUri::new(uri);
        Self::connect(&connector, &prompts, handle, &saved, &uri, 0, database, control, tunnel)
    }

    /// Sends `sql` as a registered operation. `cancellable`: the Cancel button may stop it (not
    /// `COMMIT` / `ROLLBACK`); a cancel that came before the registration stops it from starting.
    fn send(&mut self, sql: &str, cancellable: bool) -> Result<engine_driver_api::QueryPage, String> {
        let operation = self.session.begin_operation().map_err(|error| error.to_string())?;
        if cancellable {
            let (handle, connection_id, operation_id) = (Arc::clone(&self.handle), self.session.connection_id(), operation.id());
            let canceller: Canceller = Arc::new(move || {
                let _ = handle.cancel_operation(connection_id, &engine_driver_api::CancelRequest { operation_id });
            });
            self.control.register(canceller).map_err(|Cancelled| CANCELLED.to_owned())?;
        }
        let mut request = engine_driver_api::QueryRequest {
            kind: "sql".into(),
            operation_id: operation.id(),
            database: self.database.clone(),
            object: None,
            offset: 0,
            limit: 10,
            payload: serde_json::json!({"sql": sql, "confirmed": true}),
        };
        let json = request_json(&mut request).map_err(|error| error.to_string())?;
        let page = self.session.open_query(json.expose());
        self.control.clear_running();
        page.map_err(|error| crate::sessions::redact_secrets(&error.to_string()))
    }
}

impl StatementRunner for DriverApplySession {
    fn run(&mut self, sql: &str) -> Result<(), String> {
        if self.control.is_cancelled() {
            return Err(CANCELLED.into());
        }
        let page = self.send(sql, true)?;
        let _ = self.session.release_page(page.page_id);
        Ok(())
    }

    fn finish(&mut self, sql: &str) -> Result<(), String> {
        let page = self.send(sql, false)?;
        let _ = self.session.release_page(page.page_id);
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.control.is_cancelled()
    }
}

impl ApplySession for DriverApplySession {
    fn sql_mode(&mut self) -> Result<Option<String>, String> {
        if !matches!(self.kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) {
            return Ok(None);
        }
        let page = self.send("SELECT @@sql_mode", true)?;
        let columns = page.columns.len() as u64;
        let mut mode = String::new();
        if page.row_count > 0 && columns > 0 {
            let batch = self
                .session
                .render_cell_batch(page.page_id, 0, 1, 0, 1)
                .map_err(|error| crate::sessions::redact_secrets(&error.to_string()))?;
            mode = batch.cell(0).map(|cell| cell.text).unwrap_or_default();
        }
        let _ = self.session.release_page(page.page_id);
        Ok(Some(mode))
    }

    fn describe(&mut self, table: &TableId, scope: &EditScope) -> Result<Option<LiveTable>, String> {
        let dialect = dialect_for(scope.kind).ok_or("no SQL dialect")?;
        let (database, schema) = if dialect.schema_is_database() {
            (table.schema.clone().or_else(|| Some(scope.database.clone())), None)
        } else {
            (Some(scope.database.clone()), table.schema.clone())
        };
        let request = serde_json::json!({
            "database": database.filter(|d| !d.is_empty()),
            "schema": schema.filter(|s| !s.is_empty()),
            "object": table.name,
        });
        let described = match self.session.describe_object(&request) {
            Ok(described) => described,
            Err(engine_driver_host::LoaderError::DriverRejected { code, .. }) if code == engine_driver_api::sql::codes::DESCRIBE_NOT_FOUND => {
                return Ok(None)
            }
            Err(error) => return Err(crate::sessions::redact_secrets(&error.to_string())),
        };
        let properties: crate::catalog::TableProperties = serde_json::from_value(described).map_err(|error| error.to_string())?;
        Ok(Some(live_table(&properties)))
    }

    fn refresh_hint(&self) -> &'static str {
        self.refresh
    }
}

impl Drop for DriverApplySession {
    fn drop(&mut self) {
        let _ = self.session.disconnect();
    }
}

#[cfg(test)]
pub(in crate::app) type InjectedExec = dyn Fn(&str, &ApplyControl) -> Result<(), String> + Send + Sync;

/// Test double of the apply session.
#[cfg(test)]
pub(in crate::app) struct InjectedSession {
    pub(in crate::app) exec: Arc<InjectedExec>,
    pub(in crate::app) control: Arc<ApplyControl>,
    pub(in crate::app) refresh: &'static str,
    /// What `SELECT @@sql_mode` answers; `None`: nothing.
    pub(in crate::app) sql_mode: Option<String>,
}

#[cfg(test)]
impl StatementRunner for InjectedSession {
    fn run(&mut self, sql: &str) -> Result<(), String> {
        if self.control.is_cancelled() {
            return Err(CANCELLED.into());
        }
        (self.exec)(sql, &self.control)
    }

    fn finish(&mut self, sql: &str) -> Result<(), String> {
        (self.exec)(sql, &self.control)
    }

    fn cancelled(&self) -> bool {
        self.control.is_cancelled()
    }
}

#[cfg(test)]
impl ApplySession for InjectedSession {
    fn sql_mode(&mut self) -> Result<Option<String>, String> {
        Ok(self.sql_mode.clone())
    }

    fn describes(&self) -> bool {
        false
    }

    fn describe(&mut self, _: &TableId, _: &EditScope) -> Result<Option<LiveTable>, String> {
        Err("injected sessions describe nothing".into())
    }

    fn refresh_hint(&self) -> &'static str {
        self.refresh
    }
}

/// An in-memory SQLite database (`:memory:`, `mode=memory`) exists for one connection only: a second
/// session would open a different, empty one.
pub(in crate::app) fn sqlite_is_in_memory(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.contains(":memory:") || lower.contains("mode=memory")
}

/// What opening one more session of a connection needs (the structure apply's, a SQL tab's in
/// manual commit mode): everything is resolved on the UI thread, the connect itself runs on a
/// worker thread ([`SessionSpec::open`]).
pub(in crate::app) struct SessionSpec {
    pub(in crate::app) handle: Arc<DriverHandle>,
    /// The live session's own URI (a password typed and never saved is only there), else the
    /// saved connection's, read from the credential store when the session opens (a keychain read
    /// is blocking: never on the UI thread).
    pub(in crate::app) uri: Option<RuntimeUri>,
    pub(in crate::app) saved: crate::connections::SavedConnection,
    pub(in crate::app) connect_timeout: u32,
    pub(in crate::app) tunnel: Option<crate::ssh_tunnel::TunnelAccess>,
    pub(in crate::app) connector: engine_connect::connect::Connector,
    pub(in crate::app) prompts: Arc<dyn engine_connect::connect::TunnelPrompts>,
}

impl SessionSpec {
    /// Blocking: the URI to connect with.
    pub(in crate::app) fn resolve_uri(&self) -> Result<RuntimeUri, String> {
        match &self.uri {
            Some(uri) => Ok(uri.clone()),
            None => crate::sessions::engine_uri(&self.saved).map_err(|error| crate::sessions::redact_secrets(&error)),
        }
    }

    /// Blocking: connects (and opens the session's own tunnel, when the connection has one).
    pub(in crate::app) fn open(self) -> Result<(DriverSession, Option<engine_connect::connect::LiveTunnel>), String> {
        let uri = self.resolve_uri()?;
        self.connector
            .open_session(&self.handle, &self.saved, &uri, self.connect_timeout, self.tunnel, &self.prompts)
            .map_err(|failure| crate::sessions::redact_secrets(&failure.message()))
    }
}

/// Where the statements of one apply go.
pub(in crate::app) enum Executor {
    Driver(Box<SessionSpec>),
    #[cfg(test)]
    Injected(Arc<InjectedExec>, Option<String>),
}

/// Runs `job` where `executor` says, blocking: on a session of its own it opens (never the
/// connection's, never a SQL tab's), or on the test double.
pub(in crate::app) fn execute(
    executor: Executor,
    job: &ApplyJob,
    database: Option<String>,
    control: Arc<ApplyControl>,
    wording: Wording,
) -> Result<ApplyRun, Refusal> {
    match executor {
        Executor::Driver(spec) => {
            let uri = match spec.resolve_uri() {
                Ok(uri) => uri,
                Err(error) => return Err(Refusal { message: format!("Could not open a session for the change: {error}"), sql_mode: None }),
            };
            let SessionSpec { handle, saved, connect_timeout, tunnel, connector, prompts, .. } = *spec;
            match DriverApplySession::connect(&connector, &prompts, handle, &saved, &uri, connect_timeout, database, control, tunnel) {
                Ok(session) => run_apply(job, &mut session.worded(wording)),
                Err(error) => Err(Refusal {
                    message: format!("Could not open a session for the change: {error}"),
                    sql_mode: None,
                }),
            }
        }
        #[cfg(test)]
        Executor::Injected(exec, sql_mode) => run_apply(job, &mut InjectedSession { exec, control, refresh: wording.refresh, sql_mode }),
    }
}

impl AppRoot {
    /// Apply in the review dialog of `target`.
    pub(in crate::app) fn apply_structure_changes(&mut self, target: ApplyTarget, request: ApplyRequest, cx: &mut Context<Self>) {
        let Some(origin) = self.apply_origin(target, cx) else { return };
        let (connection, database, schema) = (origin.connection.clone(), origin.database.clone(), Some(origin.schema.clone()).filter(|s| !s.is_empty()));
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(&connection)).cloned() else {
            self.structure_apply_refused(target, Some(&connection), "This connection no longer exists.", cx);
            return;
        };
        // The dialect the editor reviewed (a capture may stand in another one).
        let Some(edit) = self.structure_edit_context(&connection, &database, schema.as_deref()) else {
            self.structure_apply_refused(target, Some(&connection), "This connection has no SQL structure to change.", cx);
            return;
        };
        // Checked again here: the connection may have become read-only since the dialog opened.
        if let Some(message) = edit.disabled.clone() {
            self.structure_apply_refused(target, Some(&connection), &message, cx);
            return;
        }
        // The connection may have become one that asks for a confirmation since the dialog opened.
        if edit.confirm_execute && !request.confirmed {
            let message = format!(
                "{} now asks for a confirmation before it runs anything (its connection type changed). Review the changes again and confirm them.",
                edit.subject
            );
            self.structure_apply_refused(target, Some(&connection), &message, cx);
            if matches!(target, ApplyTarget::Object { .. }) {
                self.object_action_context_changed(cx);
            }
            return;
        }
        const BUSY: &str =
            "Another structure change is already running on this connection. Wait for it to finish, or cancel it, then try again.";
        if let Some(running) = self.diagram.applies.running(&connection) {
            // The same dialog asked again (a diagram of this tab, or this very Properties editor); a
            // new editor on the tab after its table changed is refused like any other requester.
            let own = match (&running, &target) {
                (ApplyTarget::Diagram { tab_id: running, .. }, ApplyTarget::Diagram { tab_id: asked, .. }) => running == asked,
                (ApplyTarget::Properties { edit: running, .. }, ApplyTarget::Properties { edit: asked, .. }) => running == asked,
                (ApplyTarget::Object { edit: running }, ApplyTarget::Object { edit: asked }) => running == asked,
                _ => false,
            };
            if own {
                // The running apply's own dialog keeps showing it: only the log hears about the refusal.
                self.report_error(target.area(), Some(&connection), BUSY);
            } else {
                self.structure_apply_refused(target, Some(&connection), BUSY, cx);
            }
            return;
        }
        let executor = match self.structure_executor(&saved, &connection) {
            Ok(executor) => executor,
            Err(message) => {
                self.structure_apply_refused(target, Some(&connection), &message, cx);
                return;
            }
        };
        let control = Arc::new(ApplyControl::default());
        let Ok(slot) = self.diagram.applies.begin(&connection, target, &control) else {
            self.report_error(target.area(), Some(&connection), BUSY);
            return;
        };
        let job = ApplyJob {
            kind: edit.scope.kind,
            scope: edit.scope.clone(),
            changes: request.changes,
            reviewed_script: request.script,
            lock_timeout_secs: DEFAULT_LOCK_TIMEOUT_SECS,
        };
        let database = Some(database).filter(|d| !d.is_empty());
        let wording = target.wording();
        cx.spawn(async move |this, cx| {
            let work = cx.background_executor().spawn(async move {
                smol::unblock(move || {
                    // The connection is free again however this ends: a refusal, a failure, a panic.
                    let _slot = slot;
                    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(executor, &job, database, Arc::clone(&control), wording)));
                    let result = run.unwrap_or_else(|_| {
                        Err(Refusal {
                            message: format!(
                                "The structure change stopped unexpectedly. Statements may have run: {} and check the database before trying again.",
                                wording.refresh
                            ),
                            sql_mode: None,
                        })
                    });
                    (result, job)
                })
                .await
            });
            let (result, job) = work.await;
            let _ = this.update(cx, |this, cx| this.finish_structure_apply(target, &connection, &origin, &job, result, cx));
        })
        .detach();
        cx.notify();
    }

    pub(in crate::app) fn structure_executor(&self, saved: &crate::connections::SavedConnection, connection: &str) -> Result<Executor, String> {
        #[cfg(test)]
        if let Some(exec) = &self.diagram.injected_exec {
            return Ok(Executor::Injected(Arc::clone(exec), self.diagram.injected_sql_mode.clone()));
        }
        self.secondary_session_spec(saved, connection).map(|spec| Executor::Driver(Box::new(spec)))
    }

    /// What a second session of `connection` needs. Never the connection's own session: it has a
    /// tunnel of its own (shared only with a bastion that asks questions, so a one-time code is not
    /// asked twice), and a connection configured for a tunnel never gets a direct session.
    pub(in crate::app) fn secondary_session_spec(&self, saved: &crate::connections::SavedConnection, connection: &str) -> Result<SessionSpec, String> {
        if self.scenario_ui.service.is_some() {
            return Err("Fixture connections do not open a second session.".into());
        }
        let tunnel = self.secondary_tunnel_access(connection);
        if saved.ssh_tunnel && tunnel.is_none() {
            return Err("Refusing to open a direct session: this connection is configured to go through an SSH tunnel, which is not open.".into());
        }
        let handle = self
            .driver_runtime
            .session_handles
            .get(connection)
            .cloned()
            .ok_or_else(|| "The connection is not open.".to_owned())?;
        let supported = self
            .driver_capabilities(connection, saved.kind.driver_id())
            .is_some_and(|capabilities| capabilities.has(engine_driver_api::Capability::QuerySql));
        if !supported {
            return Err("This driver cannot run SQL statements.".into());
        }
        let uri = self.driver_runtime.session_uris.get(connection).cloned();
        let connect_timeout = self.effective_for(connection).connect_timeout_secs;
        Ok(SessionSpec {
            handle,
            uri,
            saved: saved.clone(),
            connect_timeout,
            tunnel,
            connector: self.connector(),
            prompts: self.tunnel_prompts(),
        })
    }

    /// The table `target` asks about: the Properties editor's own table, the tab's for a diagram.
    /// `None`: the editor is gone.
    fn apply_origin(&self, target: ApplyTarget, cx: &gpui::App) -> Option<TableOrigin> {
        match target {
            ApplyTarget::Properties { edit, .. } => self.properties_edits.values().find(|entity| entity.entity_id().as_u64() == edit).map(|entity| entity.read(cx).origin()),
            ApplyTarget::Object { edit } => self.object_action.as_ref().filter(|entity| entity.entity_id().as_u64() == edit).map(|entity| entity.read(cx).origin()),
            ApplyTarget::Diagram { tab_id, .. } => {
                let tab = self.workspace.tab(tab_id)?;
                Some(TableOrigin {
                    connection: tab.connection_id.clone(),
                    database: tab.database.clone(),
                    schema: tab.schema.clone().unwrap_or_default(),
                    object: tab.collection.clone(),
                })
            }
        }
    }

    /// The Cancel button of the progress UI: stops the apply running on `target`'s connection.
    pub(in crate::app) fn cancel_structure_apply(&mut self, target: ApplyTarget, cx: &mut Context<Self>) {
        let Some(connection) = self.apply_origin(target, cx).map(|origin| origin.connection) else { return };
        let Some(control) = self.diagram.applies.control(&connection) else { return };
        cx.background_executor().spawn(async move { smol::unblock(move || control.cancel()).await }).detach();
    }

    /// Nothing ran: tell the dialog and the error log.
    fn structure_apply_refused(&mut self, target: ApplyTarget, connection: Option<&str>, message: &str, cx: &mut Context<Self>) {
        self.report_error(target.area(), connection, message);
        self.answer_structure_apply(target, &[], &[], message.to_owned(), true, cx);
    }

    fn finish_structure_apply(
        &mut self,
        target: ApplyTarget,
        connection: &str,
        origin: &TableOrigin,
        job: &ApplyJob,
        result: Result<ApplyRun, Refusal>,
        cx: &mut Context<Self>,
    ) {
        let run = match result {
            Ok(run) => run,
            Err(refusal) => {
                // A mode learned on the way makes the next review use the right quoting.
                if let Some(mode) = refusal.sql_mode {
                    self.diagram.sql_modes.insert(connection.to_owned(), mode);
                    if matches!(target, ApplyTarget::Object { .. }) {
                        self.object_action_context_changed(cx);
                    }
                }
                self.structure_apply_refused(target, Some(connection), &refusal.message, cx);
                cx.notify();
                return;
            }
        };
        if let Some(mode) = &run.sql_mode {
            self.diagram.sql_modes.insert(connection.to_owned(), mode.clone());
        }
        let (planned, outcome) = (&run.planned, &run.outcome);
        let steps = planned.steps();
        let report = outcome.report(&steps);
        let failed = !outcome.succeeded();
        let done = outcome.completed_changes(planned);
        let partial = outcome.partial_changes(planned);
        if failed {
            self.report_error(target.area(), Some(connection), &report);
        }
        self.answer_structure_apply(target, &done, &partial, report, failed, cx);
        // Whatever ran changed the catalog: the sidebar's lists, open Properties
        // views and the diagram reload (a statement of a failed run may have run).
        if !outcome.applied.is_empty() {
            self.refresh_after_structure_change(target, origin, &job.changes, cx);
        }
        cx.notify();
    }

    /// Tells the editor that asked how its apply ended (`done` / `partial`: indices into the change-set).
    fn answer_structure_apply(
        &mut self,
        target: ApplyTarget,
        done: &[usize],
        partial: &[usize],
        report: String,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        match target {
            ApplyTarget::Diagram { tab_id, .. } => self.diagram_apply_answer(tab_id, done, partial, report, failed, cx),
            ApplyTarget::Properties { edit, .. } => self.properties_apply_answer(edit, done, partial, report, failed, cx),
            ApplyTarget::Object { edit } => self.object_action_answer(edit, done, partial, report, failed, cx),
        }
    }

    /// The answer of an apply asked from a table's Properties editor: that editor records the outcome
    /// in its `EditSession`. An editor that is gone (the tab shows another table, it was closed) hears
    /// nothing: the answer is not for whatever editor the tab has now.
    fn properties_apply_answer(&mut self, editor: u64, done: &[usize], partial: &[usize], report: String, failed: bool, cx: &mut Context<Self>) {
        let found = self.properties_edits.values().find(|entity| entity.entity_id().as_u64() == editor).cloned();
        if let Some(edit) = found {
            edit.update(cx, |edit, cx| edit.apply_finished(done, partial, report, failed, cx));
        }
    }

    fn refresh_after_structure_change(&mut self, target: ApplyTarget, origin: &TableOrigin, changes: &[Change], cx: &mut Context<Self>) {
        use crate::navigator::{object_group_id, NavigatorNodeId, ObjectGroup};
        // The navigator's object actions reload only the group they changed, in place.
        if matches!(target, ApplyTarget::Object { .. }) {
            return self.refresh_after_object_action(origin, changes, cx);
        }
        let (connection, database, schema) = (origin.connection.clone(), origin.database.clone(), origin.schema.clone());
        let group = object_group_id(&connection, &database, &schema, ObjectGroup::Tables);
        self.navigator.reset_node(&group);
        self.invalidate_table_properties(&NavigatorNodeId::Schema { connection, database, schema });
        if self.scenario_ui.service.is_none() {
            if let Some(request) = self.navigator.begin_background_load(&group) {
                self.ensure_connected_then_load(request, cx);
            }
        }
        match target {
            ApplyTarget::Diagram { tab_id, kind } => self.refresh_diagram_after_apply(tab_id, kind, cx),
            // The table's properties were invalidated above (the editor takes the new ones when they
            // arrive); the tabs showing the table reload their columns.
            ApplyTarget::Properties { .. } => self.refresh_table_tabs_after_apply(origin, cx),
            ApplyTarget::Object { .. } => {}
        }
    }

    /// The URI the dedicated apply session connects with: the one the live session was opened with
    /// (so a password the user typed, never saved, is used again from memory), else the saved
    /// connection's own (its password from the credential store).
    #[cfg(test)]
    pub(in crate::app) fn apply_session_uri(&self, saved: &crate::connections::SavedConnection, connection: &str) -> Result<RuntimeUri, String> {
        match self.driver_runtime.session_uris.get(connection) {
            Some(uri) => Ok(uri.clone()),
            None => crate::sessions::engine_uri(saved).map_err(|error| crate::sessions::redact_secrets(&error)),
        }
    }
}
