//! Plumbing shared by the scenarios: the loaded driver, connections, scratch
//! namespaces and small query helpers. Everything goes through the real host
//! (`engine_driver_host`), exactly as the application does.

use crate::fixture::{Scratch, SqlFixture};
use engine_driver_api::{
    CancelRequest, MutationRequest, MutationResult, NavigatorPage, OperationId, QueryPage,
};
use engine_driver_host::{
    DriverHandle, DriverLoader, DriverOperation, DriverSession, LoaderError, QueryRunner,
};
use serde_json::{json, Value};
use std::fmt::{Debug, Display};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Why a scenario did not pass.
#[derive(Debug)]
pub enum Failure {
    /// A rule was violated (or the driver errored unexpectedly).
    Fail(String),
    /// The scenario does not apply to this driver/fixture.
    Skip(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Fail(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::Fail(message.to_owned())
    }
}

impl From<LoaderError> for Failure {
    fn from(error: LoaderError) -> Self {
        Self::Fail(describe(&error))
    }
}

pub type Check = Result<(), Failure>;

/// `code (retryable): message` for a rejection, the plain message otherwise.
pub fn describe(error: &LoaderError) -> String {
    match error {
        LoaderError::DriverRejected {
            code,
            message,
            retryable,
        } => format!("{code} (retryable={retryable}): {message}"),
        other => other.to_string(),
    }
}

/// Collects every violation of a scenario so one run reports them all.
#[derive(Default)]
pub struct Problems(Vec<String>);

impl Problems {
    pub fn push(&mut self, message: impl Into<String>) {
        self.0.push(message.into());
    }

    /// Records `message()` unless `condition`.
    pub fn check(&mut self, condition: bool, message: impl FnOnce() -> String) {
        if !condition {
            self.0.push(message());
        }
    }

    /// Passes when nothing was recorded; otherwise one failure listing all.
    pub fn finish(self) -> Check {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Failure::Fail(self.0.join("\n        ")))
        }
    }
}

/// Fails the scenario unless `$cond`.
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err($crate::harness::Failure::Fail(format!($($arg)+)));
        }
    };
}

/// A loaded driver plus the connection URI and fixture. One per run; scenarios
/// open their own connections and scratch namespaces from it.
pub struct Ctx<'a> {
    pub handle: Arc<DriverHandle>,
    pub uri: String,
    pub fixture: &'a dyn SqlFixture,
    _staged: TempDir,
    counter: AtomicU64,
}

impl<'a> Ctx<'a> {
    /// Loads a staged copy of `dylib` (the original is never `dlopen`ed twice).
    pub fn load(dylib: &Path, uri: &str, fixture: &'a dyn SqlFixture) -> Result<Self, String> {
        let staged = TempDir::new().map_err(|error| error.to_string())?;
        let copy = staged.path().join(
            dylib
                .file_name()
                .ok_or_else(|| format!("{} has no file name", dylib.display()))?,
        );
        std::fs::copy(dylib, &copy)
            .map_err(|error| format!("cannot stage {}: {error}", dylib.display()))?;
        let handle = DriverLoader
            .load(&copy)
            .map_err(|error| format!("driver does not load: {}", describe(&error)))?;
        if let Some(admin_uri) = fixture.admin_uri(uri) {
            let setup = fixture.admin_setup(uri);
            if !setup.is_empty() {
                let mut admin = handle
                    .connect(&json!({ "uri": admin_uri }).to_string())
                    .map_err(|error| format!("admin connect: {}", describe(&error)))?;
                for sql in setup {
                    exec_on(&mut admin, &sql)
                        .map_err(|failure| format!("admin setup `{sql}`: {failure:?}"))?;
                }
                let _ = admin.disconnect();
            }
        }
        Ok(Self {
            handle,
            uri: uri.to_owned(),
            fixture,
            _staged: staged,
            counter: AtomicU64::new(0),
        })
    }

    /// A process-unique suffix for scratch names.
    pub fn unique(&self) -> String {
        format!(
            "{}_{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub fn connect(&self) -> Result<Conn<'_>, Failure> {
        self.connect_to(&self.uri)
    }

    /// A connection over the fixture's privileged URI (`None` without one).
    pub fn admin_connect(&self) -> Result<Option<Conn<'_>>, Failure> {
        match self.fixture.admin_uri(&self.uri) {
            Some(uri) => self.connect_to(&uri).map(Some),
            None => Ok(None),
        }
    }

    fn connect_to(&self, uri: &str) -> Result<Conn<'_>, Failure> {
        let session = self
            .handle
            .connect(&json!({ "uri": uri }).to_string())
            .map_err(|error| Failure::Fail(format!("connect: {}", describe(&error))))?;
        Ok(Conn { ctx: self, session })
    }

    /// Creates a scratch namespace (and optionally the standard tables); it is
    /// dropped when the guard goes out of scope, over a fresh connection.
    pub fn scratch(&self, with_tables: bool) -> Result<ScratchGuard<'_>, Failure> {
        let mut conn = self.connect()?;
        let database = conn.scalar(&self.fixture.current_database_query())?;
        let scratch = Scratch {
            name: self.fixture.scratch_name(&self.unique()),
            database,
        };
        let guard = ScratchGuard { ctx: self, scratch };
        for sql in self.fixture.create_scratch(&guard.scratch) {
            conn.exec(&sql)?;
        }
        if with_tables {
            for sql in self.fixture.create_tables(&guard.scratch) {
                conn.exec(&sql)?;
            }
        }
        Ok(guard)
    }

    /// Runs the given statements over a fresh connection, ignoring failures
    /// (cleanup must never mask the scenario's own result).
    pub fn cleanup(&self, statements: &[String]) {
        match self.connect() {
            Ok(mut conn) => {
                for sql in statements {
                    if let Err(error) = conn.exec(sql) {
                        eprintln!("cleanup `{sql}` failed: {error:?}");
                    }
                }
            }
            Err(error) => eprintln!("cleanup could not connect: {error:?}"),
        }
    }
}

/// Owns a scratch namespace for the duration of a scenario.
pub struct ScratchGuard<'a> {
    ctx: &'a Ctx<'a>,
    pub scratch: Scratch,
}

impl ScratchGuard<'_> {
    pub fn table(&self, name: &str) -> String {
        self.ctx.fixture.table(&self.scratch, name)
    }
}

impl Drop for ScratchGuard<'_> {
    fn drop(&mut self) {
        let statements = self.ctx.fixture.drop_scratch(&self.scratch);
        self.ctx.cleanup(&statements);
    }
}

/// One driver connection with query helpers.
pub struct Conn<'a> {
    pub ctx: &'a Ctx<'a>,
    pub session: DriverSession,
}

/// A query running on another thread, cancellable by its operation id.
pub struct Running {
    pub operation: DriverOperation,
    pub thread: std::thread::JoinHandle<(QueryRunner, Result<QueryPage, LoaderError>)>,
}

impl Conn<'_> {
    /// Opens a query page (nothing is released: the session frees its pages on
    /// disconnect).
    pub fn open_page(
        &mut self,
        sql: &str,
        offset: u64,
        limit: u64,
        confirmed: bool,
    ) -> Result<QueryPage, LoaderError> {
        let operation = self.session.begin_operation()?;
        self.session
            .open_query(&query_request(operation.id(), sql, offset, limit, confirmed).to_string())
    }

    /// Runs `sql` as setup: confirmed, page discarded.
    pub fn exec(&mut self, sql: &str) -> Result<(), Failure> {
        exec_on(&mut self.session, sql)
    }

    /// Runs `sql` exactly as a user would (not confirmed) and discards the page.
    pub fn run(&mut self, sql: &str) -> Result<QueryPage, LoaderError> {
        let page = self.open_page(sql, 0, 100, false)?;
        let _ = self.session.release_page(page.page_id);
        Ok(page)
    }

    /// All cells of a query as text, row-major.
    pub fn rows(&mut self, sql: &str) -> Result<Vec<Vec<String>>, Failure> {
        let page = self
            .open_page(sql, 0, 1000, false)
            .map_err(|error| Failure::Fail(format!("`{sql}`: {}", describe(&error))))?;
        let columns = page.columns.len() as u64;
        let mut rows = vec![vec![String::new(); page.columns.len()]; page.row_count as usize];
        if page.row_count > 0 && columns > 0 {
            let batch = self
                .session
                .render_cell_batch(page.page_id, 0, page.row_count, 0, columns)
                .map_err(|error| Failure::Fail(format!("render `{sql}`: {}", describe(&error))))?;
            for index in 0..batch.cell_count() {
                if let Some(cell) = batch.cell(index) {
                    rows[cell.row as usize][cell.column as usize] = cell.text;
                }
            }
        }
        let _ = self.session.release_page(page.page_id);
        Ok(rows)
    }

    /// First cell of the first row.
    pub fn scalar(&mut self, sql: &str) -> Result<String, Failure> {
        self.rows(sql)?
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next())
            .ok_or_else(|| Failure::Fail(format!("`{sql}` returned no rows")))
    }

    pub fn count(&mut self, from_where: &str) -> Result<i64, Failure> {
        let text = self.scalar(&format!("SELECT COUNT(*) FROM {from_where}"))?;
        text.parse()
            .map_err(|_| Failure::Fail(format!("COUNT(*) FROM {from_where} gave {text:?}")))
    }

    pub fn nav(&mut self, request: &Value) -> Result<NavigatorPage, LoaderError> {
        self.session.list_nav_nodes(&request.to_string())
    }

    pub fn describe(&mut self, object: &Value) -> Result<Value, LoaderError> {
        self.session.describe_object(object)
    }

    /// A mutation on `table` of `scratch` (`kind` + `payload` as the contract says).
    pub fn mutate(
        &mut self,
        scratch: &Scratch,
        table: &str,
        kind: &str,
        payload: Value,
    ) -> Result<MutationResult, LoaderError> {
        let target = self.ctx.fixture.mutation_target(scratch, table);
        let operation = self.session.begin_operation()?;
        self.session.mutate(&MutationRequest {
            operation_id: operation.id(),
            kind: kind.into(),
            database: target.database,
            object: Some(target.object),
            identity: target.identity,
            payload,
        })
    }

    /// Starts `sql` on its own thread (the session stays usable) and returns a
    /// handle to cancel/join it.
    pub fn spawn_query(&mut self, sql: &str) -> Result<Running, Failure> {
        let operation = self.session.begin_operation()?;
        let request = query_request(operation.id(), sql, 0, 100, true);
        let runner = self.session.query_runner()?;
        let thread = std::thread::spawn(move || {
            let result = runner.open_query(&request.to_string());
            (runner, result)
        });
        Ok(Running { operation, thread })
    }

    /// Cancels operation `id` of this connection through the host.
    pub fn cancel(&self, id: OperationId) -> Result<(), LoaderError> {
        self.ctx.handle.cancel_operation(
            self.session.connection_id(),
            &CancelRequest { operation_id: id },
        )
    }

    /// Waits (bounded) for a spawned query and returns its result.
    pub fn join(&mut self, running: Running, wait: Duration) -> Result<QueryPage, LoaderError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = running.thread;
        std::thread::spawn(move || {
            let _ = sender.send(thread.join());
        });
        match receiver.recv_timeout(wait) {
            Ok(Ok((runner, result))) => {
                if let Ok(page) = &result {
                    runner.release_page(page.page_id);
                }
                drop(running.operation);
                result
            }
            Ok(Err(_)) => Err(LoaderError::MalformedResponse(
                "query thread panicked".into(),
            )),
            Err(_) => Err(LoaderError::MalformedResponse(format!(
                "query still running after {wait:?} (not cancelled/interrupted)"
            ))),
        }
    }
}

impl Drop for Conn<'_> {
    fn drop(&mut self) {
        let _ = self.session.disconnect();
    }
}

/// Runs `sql` confirmed on `session` and discards the page.
fn exec_on(session: &mut DriverSession, sql: &str) -> Result<(), Failure> {
    let operation = session.begin_operation()?;
    let page = session
        .open_query(&query_request(operation.id(), sql, 0, 100, true).to_string())
        .map_err(|error| Failure::Fail(format!("`{sql}`: {}", describe(&error))))?;
    let _ = session.release_page(page.page_id);
    Ok(())
}

fn query_request(
    operation_id: OperationId,
    sql: &str,
    offset: u64,
    limit: u64,
    confirmed: bool,
) -> Value {
    let mut payload = json!({ "sql": sql });
    if confirmed {
        payload["confirmed"] = json!(true);
    }
    json!({
        "kind": "sql", "operation_id": operation_id, "database": null, "object": null,
        "offset": offset, "limit": limit, "payload": payload
    })
}

/// The rejection code (and retryable flag + message) of an expected failure.
pub fn rejection<T: Debug>(
    result: Result<T, LoaderError>,
    what: &str,
) -> Result<(String, bool, String), Failure> {
    match result {
        Err(LoaderError::DriverRejected {
            code,
            message,
            retryable,
        }) => Ok((code, retryable, message)),
        Err(other) => Err(Failure::Fail(format!(
            "{what}: expected a driver rejection, got {other}"
        ))),
        Ok(value) => {
            let mut shown = format!("{value:?}");
            shown.truncate(160);
            Err(Failure::Fail(format!(
                "{what}: expected a rejection, but it succeeded ({shown})"
            )))
        }
    }
}

/// Asserts the call was rejected with `code`.
pub fn expect_rejection<T: Debug>(
    result: Result<T, LoaderError>,
    code: &str,
    what: &str,
) -> Result<(bool, String), Failure> {
    let (actual, retryable, message) = rejection(result, what)?;
    if actual != code {
        return Err(Failure::Fail(format!(
            "{what}: expected {code}, got {actual}: {message}"
        )));
    }
    Ok((retryable, message))
}

pub fn expect_ok<T, E: Display>(result: Result<T, E>, what: &str) -> Result<T, Failure> {
    result.map_err(|error| Failure::Fail(format!("{what}: {error}")))
}
