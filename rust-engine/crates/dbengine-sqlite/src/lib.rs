//! SQLite on the shared driver kit: the kit owns the session, page registry,
//! Arrow leases, cancel state and the `dbengine_*` exports; this crate implements
//! the SQL conformance rules of `engine_driver_api::sql`.
//!
//! * Queries (`kind = "sql"`): only a single pure read (`SELECT`/`WITH ... SELECT`/
//!   `VALUES`/`EXPLAIN`, by `engine_driver_kit::sql::classify`) is pageable. Any
//!   other statement or script runs once: `offset > 0` is refused with
//!   `query.not_pageable` and the page reports `truncated = false`. `limit == 0`
//!   means 200 rows, pages are capped at 100 000 and read with one lookahead row,
//!   so `truncated` is exact; `total_count` is `None`. Dangerous statements
//!   (`DROP`, `ALTER`, `DELETE`/`UPDATE` without `WHERE`, `GRANT`/`REVOKE`, ...) fail
//!   with `query.confirmation_required` unless `payload.confirmed`. A requested
//!   cancel fails with `query.cancelled`; an interrupt nobody asked for with
//!   retryable `query.interrupted`. A script of several statements runs each
//!   once, in order, and returns the rows of the last statement that produces a
//!   result set (`SELECT 1 AS a; INSERT ...` returns `a`; a script without any
//!   result set returns a zero-column page). `CREATE TRIGGER ... BEGIN ...; END`
//!   bodies are kept in one piece.
//! * Mutations (`insert`/`update`/`delete`/`batch`): see [`mutation`]. Each runs in a
//!   `SAVEPOINT`, so inside a user `BEGIN` it joins that transaction and never
//!   ends it; a failed `ROLLBACK TO` marks the connection broken
//!   (`<area>.not_connected` afterwards).
//! * Navigator and `describe_object`: see [`catalog`]. They run on a second,
//!   read-only connection to the same file, so they do not wait behind a running
//!   user query, and they see only committed data (a table created inside a
//!   transaction that is still open appears after its `COMMIT`). An in-memory
//!   database has no second connection: its catalog calls share the user
//!   connection and wait behind a running query. In WAL and rollback-journal mode
//!   alike the reader waits at most 2 s (`busy_timeout`) for a writer's commit and
//!   then falls back to the user connection.
//! * `create_database` does not apply to SQLite (a database is a file) and is not
//!   advertised.

mod catalog;
mod mutation;

use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, Float64Builder, Int64Builder, StringBuilder,
};
use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray, UnionArray};
use arrow_buffer::ScalarBuffer;
use arrow_schema::{DataType, Field, Schema, UnionFields};
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::{
    Capability, DriverCapabilities, MutationRequest, NavigatorPage, NavigatorRequest, OperationId,
    PageFlags, QueryRequest, SqlDialectInfo,
};
use engine_driver_kit::sql::{classify, Classification, Keyword, SqlLexOptions};
use engine_driver_kit::{
    page_columns, ActiveOperation, BackendError, ConnectRequest, DriverBackend, MutationOutcome,
    OperationGuard, QueryOutput,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const DRIVER: &str = "sqlite";
/// Rows of a page when the request's `limit` is `0`.
const DEFAULT_PAGE_ROWS: u64 = 200;
/// Hard cap on the rows of one page.
const MAX_PAGE_ROWS: u64 = 100_000;
/// How long a catalog call waits for a writer on the metadata connection.
const METADATA_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// The message of a cancel seen before a statement started (SQLite's own word for the same
/// thing, so both read alike).
const CANCELLED_BEFORE_START: &str = "interrupted";

/// VM instructions between two looks at the cancel state (`sqlite3_progress_handler`). A look is
/// two atomic loads, so this costs nothing measurable (`progress_handler_overhead` below), and a
/// statement that is cancelled stops within this many instructions.
const CANCEL_CHECK_OPS: std::os::raw::c_int = 1000;

/// The longest an operation waits for an interrupt that is being sent as it ends.
const INTERRUPT_WAIT: Duration = Duration::from_secs(1);

/// Which operation runs on the user connection and which one a cancel was asked for.
///
/// `sqlite3_interrupt` only stops a statement that is running at that instant: a cancel that
/// lands after the operation became active but before its statement starts (between the
/// `read_only` pragmas, between the statements of a script) would be lost. So the request is
/// recorded, and looked at in two places: before every statement of the operation
/// ([`CancelState::stop_requested`] in `query_script` and before a mutation's changes), and by
/// the connection's progress handler every [`CANCEL_CHECK_OPS`] instructions of a running one.
/// A cancel only ever matches the operation it named (the request is forgotten when that
/// operation ends), and only while that operation's cancellable phase lasts.
///
/// Known limit: a cancel that lands in the instant between the look and the start of a statement
/// that is a single long instruction (`SELECT count(*)` on a big table, `VACUUM`) is not seen
/// until that statement ends.
///
/// `sqlite3_interrupt` is not atomic with the check that the operation is still running, so the
/// interrupts being sent are counted ([`CancelState::interrupt_if_running`]) and an operation does
/// not end until none is in flight ([`RunningOperation`]'s drop): a canceller that was preempted
/// between the check and the call can never interrupt the next operation's statement.
struct CancelState {
    running: AtomicU64,
    requested: AtomicU64,
    /// `sqlite3_interrupt` calls between "this operation is running" and "returned".
    interrupts: AtomicUsize,
}

impl CancelState {
    /// No operation (host operation ids are a counter that never gets here).
    const NONE: u64 = u64::MAX;

    fn new() -> Self {
        Self {
            running: AtomicU64::new(Self::NONE),
            requested: AtomicU64::new(Self::NONE),
            interrupts: AtomicUsize::new(0),
        }
    }

    /// The progress handler's answer: stop the statement of the running operation.
    fn stop_requested(&self) -> bool {
        let running = self.running.load(Ordering::Acquire);
        running != Self::NONE && running == self.requested.load(Ordering::Acquire)
    }

    /// Records the cancel of `operation_id`, unless that would erase the cancel of the running
    /// operation: a late cancel for an operation that already ended, or for one still waiting,
    /// must not let a script that was cancelled between two statements run on. (The kit
    /// remembers the cancel of a waiting operation and reports it when it activates.) A
    /// compare-and-swap, so a request of the running operation that lands meanwhile is kept too.
    fn request(&self, operation_id: OperationId) {
        let mut current = self.requested.load(Ordering::Acquire);
        loop {
            if current != Self::NONE
                && current != operation_id.0
                && current == self.running.load(Ordering::SeqCst)
            {
                return;
            }
            match self.requested.compare_exchange_weak(
                current,
                operation_id.0,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(seen) => current = seen,
            }
        }
    }

    /// Whether `operation_id` is in its cancellable phase right now.
    fn is_running(&self, operation_id: OperationId) -> bool {
        self.running.load(Ordering::SeqCst) == operation_id.0
    }

    /// Calls `interrupt` if `operation_id` is in its cancellable phase, and counts it as in
    /// flight from before the check until it returned. The count is incremented, then the running
    /// mark read; the operation clears the mark, then reads the count (both `SeqCst`): one side
    /// always sees the other, so either no interrupt is sent or the operation waits for it.
    fn interrupt_if_running(&self, operation_id: OperationId, interrupt: impl FnOnce()) {
        struct InFlight<'a>(&'a AtomicUsize);
        impl Drop for InFlight<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.interrupts.fetch_add(1, Ordering::SeqCst);
        let _in_flight = InFlight(&self.interrupts);
        if self.is_running(operation_id) {
            interrupt();
        }
    }

    /// Marks `operation_id` as the one whose statements run until the guard drops.
    fn run(&self, operation_id: OperationId) -> RunningOperation<'_> {
        self.running.store(operation_id.0, Ordering::SeqCst);
        RunningOperation {
            state: self,
            operation_id,
        }
    }
}

/// Ends the cancellable phase on drop, so statements outside an operation are never stopped,
/// and forgets a cancel that named this operation.
struct RunningOperation<'a> {
    state: &'a CancelState,
    operation_id: OperationId,
}

impl Drop for RunningOperation<'_> {
    fn drop(&mut self) {
        self.state
            .running
            .store(CancelState::NONE, Ordering::SeqCst);
        // An interrupt that passed the check just before is delivered before the next
        // statement can start (bounded: it is a call that returns at once).
        let deadline = std::time::Instant::now() + INTERRUPT_WAIT;
        while self.state.interrupts.load(Ordering::SeqCst) > 0
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        let _ = self.state.requested.compare_exchange(
            self.operation_id.0,
            CancelState::NONE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// What `cancel` needs, usable without the connection lock.
struct SqliteCanceller {
    interrupt: InterruptHandle,
    state: Arc<CancelState>,
}

struct SqliteConnection {
    connection: Mutex<Connection>,
    cancel: Arc<CancelState>,
    /// The database file; `None` for `:memory:` (no second connection possible).
    path: Option<PathBuf>,
    /// Lazily opened read-only connection for catalog calls.
    metadata: Mutex<Option<Connection>>,
    /// A `ROLLBACK TO` failed: the transaction state is unknown.
    broken: AtomicBool,
}

impl SqliteConnection {
    fn lock(&self) -> Result<MutexGuard<'_, Connection>, BackendError> {
        self.connection
            .lock()
            .map_err(|_| BackendError::new("SQLite connection lock poisoned"))
    }

    fn check_usable(&self, area: &str) -> Result<(), BackendError> {
        if self.broken.load(Ordering::Acquire) {
            Err(BackendError::with_code(
                format!("{area}.not_connected"),
                "the connection is in an unknown state after an interrupted statement; reconnect",
            ))
        } else {
            Ok(())
        }
    }

    /// Runs a catalog read on the metadata connection (user connection for
    /// `:memory:`, or when the read-only connection cannot be opened).
    fn with_metadata<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        if let Some(path) = &self.path {
            let mut metadata = self
                .metadata
                .lock()
                .map_err(|_| BackendError::new("SQLite metadata lock poisoned"))?;
            if metadata.is_none() {
                *metadata = Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .and_then(|conn| {
                    conn.busy_timeout(METADATA_BUSY_TIMEOUT)?;
                    Ok(conn)
                })
                .ok();
            }
            if let Some(conn) = metadata.as_ref() {
                return read(conn);
            }
        }
        read(&*self.lock()?)
    }
}

/// SQLite backend.
struct SqliteBackend;

impl DriverBackend for SqliteBackend {
    type Connection = SqliteConnection;
    type PageContext = ();
    type Canceller = SqliteCanceller;

    fn capabilities() -> DriverCapabilities {
        let mut capabilities = DriverCapabilities::new(
            DRIVER,
            "SQLite",
            [
                Capability::Navigator,
                Capability::QuerySql,
                Capability::QueryArrowCData,
                Capability::OperationCancel,
                Capability::MutationInsert,
                Capability::MutationUpdate,
                Capability::MutationDelete,
                Capability::MutationBatch,
                Capability::MetadataTableProperties,
                Capability::QueryReadOnly,
                Capability::QueryRunToEnd,
            ]
            .map(|capability| capability.as_str().to_owned())
            .into(),
        );
        capabilities.uri_schemes = vec![DRIVER.into()];
        capabilities.editors = vec!["sql".into()];
        capabilities.navigator_kinds = ["database", "table", "view", "trigger", "column"]
            .map(String::from)
            .into();
        capabilities.sql_dialect = Some(SqlDialectInfo {
            identifier_quote: "\"".into(),
            string_literal_quote: "'".into(),
            supports_schemas: false,
            database_qualified_names: false,
            like_requires_text_cast: false,
            string_backslash_escapes: false,
            double_quote_identifiers: true,
            bracket_identifiers: true,
        });
        capabilities
    }

    fn connect(request: &ConnectRequest) -> Result<SqliteConnection, BackendError> {
        let path = sqlite_path(&request.uri)
            .map_err(|error| BackendError::with_code("connect.bad_request", error))?;
        let (connection, path) = if path == ":memory:" {
            (Connection::open_in_memory(), None)
        } else {
            (Connection::open(&path), Some(PathBuf::from(path)))
        };
        let connection = connection.map_err(|error| BackendError::new(error.to_string()))?;
        let cancel = Arc::new(CancelState::new());
        let state = Arc::clone(&cancel);
        connection
            .progress_handler(CANCEL_CHECK_OPS, Some(move || state.stop_requested()))
            .map_err(|error| BackendError::new(error.to_string()))?;
        Ok(SqliteConnection {
            connection: Mutex::new(connection),
            cancel,
            path,
            metadata: Mutex::new(None),
            broken: AtomicBool::new(false),
        })
    }

    fn list_nav_nodes(
        connection: &SqliteConnection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        connection.check_usable("navigator")?;
        connection.with_metadata(|conn| catalog::list(conn, request))
    }

    fn describe_object(
        connection: &SqliteConnection,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        connection.check_usable("describe")?;
        connection.with_metadata(|conn| catalog::describe(conn, request))
    }

    fn open_query(
        connection: &SqliteConnection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput, BackendError> {
        if request.kind != "sql" && request.kind != "sql_query" {
            return Err(BackendError::with_code(
                "query.unsupported",
                "SQLite accepts kind=sql",
            ));
        }
        let sql = request
            .payload
            .get("sql")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if sql.trim().is_empty() {
            return Err(BackendError::with_code(
                "query.bad_request",
                "missing SQL payload",
            ));
        }
        connection.check_usable("query")?;
        let classification = classify(sql, &SqlLexOptions::sqlite());
        if classification.is_empty() {
            return Err(BackendError::with_code(
                "query.bad_request",
                "the SQL contains no statement",
            ));
        }
        let read_only = request
            .payload
            .get("read_only")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        // `PRAGMA query_only` is the READ ONLY scope, and the text can switch it off
        // (`PRAGMA query_only = OFF; DELETE ...`) or end the transaction around it:
        // refuse a script with a statement that is not a plain read, and any
        // statement that controls the transaction.
        if let (true, Some(refusal)) = (read_only, classification.read_only_refusal()) {
            return Err(refusal.into());
        }
        let confirmed = request
            .payload
            .get("confirmed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if let (Some(danger), false) = (&classification.danger, confirmed) {
            return Err(BackendError::with_code(
                codes::QUERY_CONFIRMATION_REQUIRED,
                danger.reason,
            ));
        }
        if request.offset > 0 && !classification.pageable {
            return Err(BackendError::with_code(
                codes::QUERY_NOT_PAGEABLE,
                "only a single read (SELECT, WITH ... SELECT, VALUES, EXPLAIN) can be paged; \
                 this statement runs once, at offset 0",
            ));
        }
        let limit = match request.limit {
            0 => DEFAULT_PAGE_ROWS,
            limit => limit.min(MAX_PAGE_ROWS),
        };
        // Capability `query.run_to_end`: every row is stepped, `limit` of them kept.
        let run_to_end = request
            .payload
            .get("run_to_end")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let result = query(
            connection,
            operation,
            sql,
            &classification,
            Page { offset: request.offset, limit, run_to_end },
            read_only,
        )?;
        let (batch, truncated) = match result {
            QueryResult::Rows { batch, truncated } => (batch, truncated),
            QueryResult::Command { affected_rows } => {
                return Ok(QueryOutput::command_complete(affected_rows, ()));
            }
        };
        let columns = page_columns(&batch, |data_type| data_type_label(data_type).into());
        Ok(QueryOutput {
            batch,
            columns,
            total_count: None,
            total_is_exact: false,
            flags: PageFlags {
                truncated,
                cancelled: false,
                read_only: true,
            },
            context: (),
        })
    }

    fn mutate(
        connection: &SqliteConnection,
        request: &MutationRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        let plan = mutation::Plan::from_request(request)?;
        connection.check_usable("mutation")?;
        let guard = connection.lock()?;
        let active = activate(operation, guard, "mutation")?;
        let savepoint = format!("tablex_mutation_{}", active.operation_id().0);
        // Only the changes themselves can be cancelled; the `ROLLBACK TO` / `RELEASE` that
        // follow must run whatever was asked.
        let (result, broken) = mutation::in_savepoint(&active, &savepoint, |conn| {
            let _running = connection.cancel.run(active.operation_id());
            if connection.cancel.stop_requested() {
                return Err(mutation::Failure::Contract {
                    code: "mutation.cancelled",
                    message: CANCELLED_BEFORE_START.into(),
                });
            }
            mutation::apply_all(conn, &plan)
        });
        if broken {
            connection.broken.store(true, Ordering::Release);
        }
        match result {
            Ok(affected) => Ok(MutationOutcome::new(affected)),
            Err(mutation::Failure::Contract { code, message }) => {
                Err(BackendError::with_code(code, message))
            }
            Err(mutation::Failure::Sqlite(error)) => Err(sqlite_error(&active, error, "mutation")),
        }
    }

    fn canceller(connection: &SqliteConnection) -> Result<SqliteCanceller, BackendError> {
        Ok(SqliteCanceller {
            interrupt: connection.lock()?.get_interrupt_handle(),
            state: Arc::clone(&connection.cancel),
        })
    }

    /// Neither step takes the connection lock, as `cancel` requires. The request is recorded
    /// first, so a statement of this operation that has not started yet is stopped before it
    /// runs or by the progress handler; `sqlite3_interrupt` stops the one running now, and is
    /// only sent while this operation is the one running, so a late cancel cannot hit the next
    /// operation's statement or a catalog read.
    fn cancel(canceller: &SqliteCanceller, operation_id: OperationId) -> Result<(), BackendError> {
        canceller.state.request(operation_id);
        canceller
            .state
            .interrupt_if_running(operation_id, || canceller.interrupt.interrupt());
        Ok(())
    }
}

engine_driver_kit::export_driver!(SqliteBackend; cancel_operation, mutate, describe_object);

/// `OperationGuard::activate`, with a cancel that arrived while the operation
/// waited for the connection reported as `<area>.cancelled`.
fn activate<'a, G>(
    operation: &'a mut OperationGuard<'_>,
    guard: G,
    area: &str,
) -> Result<ActiveOperation<'a, G>, BackendError> {
    operation.activate(guard).map_err(|error| {
        if error.code.is_none() && error.message == "operation cancelled" {
            BackendError::with_code(format!("{area}.cancelled"), error.message)
        } else {
            error
        }
    })
}

fn sqlite_path(uri: &str) -> Result<String, String> {
    let prefix = "sqlite://";
    let Some(uri_prefix) = uri.get(..prefix.len()) else {
        return Err("SQLite URI must start with sqlite://".to_string());
    };
    if !uri_prefix.eq_ignore_ascii_case(prefix) {
        return Err("SQLite URI must start with sqlite://".to_string());
    }
    let rest = &uri[prefix.len()..];
    let path = rest.split_once('?').map_or(rest, |(path, _)| path);
    if path.is_empty() || path == "/" {
        return Err("SQLite URI path is empty".into());
    }
    Ok(if path == ":memory:" {
        path.to_string()
    } else {
        percent_decode(path)?
    })
}

fn percent_decode(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let end = index + 3;
            let encoded = bytes
                .get(index + 1..end)
                .ok_or_else(|| "invalid percent escape in SQLite URI".to_string())?;
            let text = std::str::from_utf8(encoded).map_err(|error| error.to_string())?;
            let byte = u8::from_str_radix(text, 16)
                .map_err(|_| "invalid percent escape in SQLite URI".to_string())?;
            out.push(byte as char);
            index = end;
        } else {
            out.push(bytes[index] as char);
            index += 1;
        }
    }
    Ok(out)
}

#[derive(Debug)]
enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    /// A TEXT value whose bytes are not UTF-8: kept exactly as stored.
    TextBytes(Vec<u8>),
}

/// Maps a SQLite error. An interrupt the user asked for (`cancel`) is
/// `<area>.cancelled`; one nobody requested (`sqlite3_interrupt` from outside the
/// kit) is retryable `<area>.interrupted`; anything else is a plain failure.
fn sqlite_error<G>(
    active: &ActiveOperation<'_, G>,
    error: rusqlite::Error,
    area: &str,
) -> BackendError {
    let interrupted = matches!(
        &error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::OperationInterrupted
    );
    let read_only = area == "query"
        && matches!(
            &error,
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == rusqlite::ErrorCode::ReadOnly
        );
    if read_only {
        // `PRAGMA query_only` (a `read_only` request) or a read-only file refused a write.
        BackendError::with_code(codes::QUERY_READ_ONLY_VIOLATION, error.to_string())
    } else if !interrupted {
        BackendError::new(error.to_string())
    } else if active.cancel_requested() {
        BackendError::with_code(format!("{area}.cancelled"), error.to_string())
    } else {
        BackendError::with_code(format!("{area}.interrupted"), error.to_string()).retryable()
    }
}

enum QueryResult {
    /// `truncated`: more rows exist beyond this page (exact, by lookahead).
    Rows { batch: RecordBatch, truncated: bool },
    /// A statement without result columns (DDL, DML).
    Command { affected_rows: u64 },
}

/// What one result-producing statement returned: column names, column-major
/// values, and whether more rows exist beyond them (lookahead, pageable reads
/// only).
struct ResultSet {
    names: Vec<String>,
    values: Vec<Vec<SqlValue>>,
    truncated: bool,
}

/// Which rows of a statement a request keeps.
#[derive(Clone, Copy)]
struct Page {
    offset: u64,
    limit: u64,
    /// Capability `query.run_to_end`: every row is stepped, `limit` of them kept.
    run_to_end: bool,
}

fn query(
    connection: &SqliteConnection,
    operation: &mut OperationGuard<'_>,
    sql: &str,
    classification: &Classification,
    page: Page,
    force_read_only: bool,
) -> Result<QueryResult, BackendError> {
    let guard = connection.lock()?;
    // `active` owns the connection guard: the operation stops being "active"
    // before the connection is released.
    let active = activate(operation, guard, "query")?;
    let running = connection.cancel.run(active.operation_id());
    // `payload.read_only`: `PRAGMA query_only` for THIS statement, restored after
    // (to the value it had), so even a function that writes fails.
    let previous_query_only = if force_read_only {
        let previous: bool = active
            .query_row("PRAGMA query_only", [], |row| row.get(0))
            .map_err(|error| sqlite_error(&active, error, "query"))?;
        Some(previous)
    } else {
        None
    };
    let outcome = match previous_query_only {
        Some(_) => active
            .execute_batch("PRAGMA query_only = ON")
            .map_err(|error| sqlite_error(&active, error, "query")),
        None => Ok(()),
    }
    .and_then(|()| {
        query_script(
            &active,
            &connection.cancel,
            sql,
            classification,
            page.offset,
            page.limit,
            page.run_to_end,
        )
    });
    // The restore below must run even when the statement was cancelled.
    drop(running);
    if let Some(previous) = previous_query_only {
        // A restore that did not take would leave the connection read-only (or writable)
        // behind the user's back, so it is read back. A cancel decided while the statement ran
        // can be delivered an instant later and interrupt the restore itself; SQLite forgets an
        // interrupt when the next statement starts, so trying again is enough.
        let restore = if previous {
            "PRAGMA query_only = ON"
        } else {
            "PRAGMA query_only = OFF"
        };
        let restored = (0..4).any(|_| {
            active
                .execute_batch(restore)
                .and_then(|()| active.query_row("PRAGMA query_only", [], |row| row.get(0)))
                .ok()
                == Some(previous)
        });
        if !restored {
            connection.broken.store(true, Ordering::Release);
        }
    }
    outcome
}

fn query_script<G>(
    active: &ActiveOperation<'_, G>,
    cancel: &CancelState,
    sql: &str,
    classification: &Classification,
    offset: u64,
    limit: u64,
    run_to_end: bool,
) -> Result<QueryResult, BackendError>
where
    G: std::ops::Deref<Target = Connection>,
{
    let before = active.total_changes();
    // Every statement runs exactly once, in order. The page is the rows of the
    // LAST statement that produces a result set (a trailing `INSERT` after
    // `SELECT 1 AS a` leaves `a`); a script without one is a zero-column page.
    // A pageable read is a single statement: it skips `offset` rows and reads
    // one lookahead row; anything else ran once at offset 0 and is never
    // "truncated".
    let pageable = classification.pageable;
    let mut result: Option<ResultSet> = None;
    for range in statement_ranges(sql, classification) {
        // A cancel between two statements (or before the first) stops no running statement:
        // look before each one starts.
        if cancel.stop_requested() {
            return Err(BackendError::with_code(
                "query.cancelled",
                CANCELLED_BEFORE_START,
            ));
        }
        let mut statement = active
            .prepare(&sql[range])
            .map_err(|error| sqlite_error(active, error, "query"))?;
        if statement.column_count() == 0 {
            statement
                .execute([])
                .map_err(|error| sqlite_error(active, error, "query"))?;
            continue;
        }
        result = Some(read_result_set(
            active,
            &mut statement,
            offset,
            limit,
            pageable && !run_to_end,
            run_to_end,
        )?);
    }
    let Some(ResultSet {
        names,
        values,
        truncated,
    }) = result
    else {
        // `total_changes` counts DML only, unlike `changes`, which is stale after DDL.
        return Ok(QueryResult::Command {
            affected_rows: active.total_changes().saturating_sub(before),
        });
    };
    let arrays = values.into_iter().map(build_array).collect::<Vec<_>>();
    let fields = arrays
        .iter()
        .enumerate()
        .map(|(index, array)| Field::new(names[index].clone(), array.data_type().clone(), true))
        .collect::<Vec<_>>();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .map(|batch| QueryResult::Rows { batch, truncated })
        .map_err(|error| BackendError::new(error.to_string()))
}

fn read_result_set<G>(
    active: &ActiveOperation<'_, G>,
    statement: &mut rusqlite::Statement<'_>,
    offset: u64,
    limit: u64,
    pageable: bool,
    run_to_end: bool,
) -> Result<ResultSet, BackendError> {
    let fail = |error| sqlite_error(active, error, "query");
    let column_count = statement.column_count();
    let names = (0..column_count)
        .map(|index| statement.column_name(index).unwrap_or("column").to_string())
        .collect::<Vec<_>>();
    let wanted = if pageable { limit + 1 } else { limit } as usize;
    let mut rows = statement.query([]).map_err(fail)?;
    let mut values = (0..column_count)
        .map(|_| Vec::new())
        .collect::<Vec<Vec<SqlValue>>>();
    let mut skipped = 0;
    let mut read = 0;
    loop {
        if read >= wanted && !run_to_end {
            break;
        }
        let Some(row) = rows.next().map_err(fail)? else {
            break;
        };
        // Past the rows kept, a statement run to its end is stepped and the row dropped.
        if read >= wanted {
            continue;
        }
        if skipped < offset {
            skipped += 1;
            continue;
        }
        for (column, column_values) in values.iter_mut().enumerate() {
            column_values.push(match row.get_ref(column).map_err(fail)? {
                ValueRef::Null => SqlValue::Null,
                ValueRef::Integer(value) => SqlValue::Integer(value),
                ValueRef::Real(value) => SqlValue::Real(value),
                // Never repaired: bytes that are not text are delivered as bytes.
                ValueRef::Text(value) => match std::str::from_utf8(value) {
                    Ok(text) => SqlValue::Text(text.to_owned()),
                    Err(_) => SqlValue::TextBytes(value.to_vec()),
                },
                ValueRef::Blob(value) => SqlValue::Blob(value.to_vec()),
            });
        }
        read += 1;
    }
    let truncated = pageable && read > limit as usize;
    if truncated {
        for column_values in &mut values {
            column_values.pop();
        }
    }
    Ok(ResultSet {
        names,
        values,
        truncated,
    })
}

/// The byte range of every statement of the script, as SQLite sees them. The
/// classifier splits at every top-level `;`, which cuts a `CREATE TRIGGER`
/// body (`BEGIN stmt; stmt; END`) apart; this puts it back together: a
/// trigger runs from its `CREATE` to the `END` statement that closes it. (`END`
/// can only start a statement as that terminator: inside a body statement it is
/// part of `CASE ... END`.)
fn statement_ranges(sql: &str, classification: &Classification) -> Vec<Range<usize>> {
    let statements = &classification.statements;
    let mut ranges = Vec::with_capacity(statements.len());
    let mut index = 0;
    while index < statements.len() {
        let statement = &statements[index];
        let mut end = statement.range.end;
        if statement.keyword == Some(Keyword::Create)
            && creates_trigger(&sql[statement.range.clone()])
        {
            let closing = statements[index + 1..]
                .iter()
                .position(|next| next.keyword == Some(Keyword::End));
            if let Some(offset) = closing {
                index += 1 + offset;
                end = statements[index].range.end;
            } else {
                // No terminator: let SQLite report the syntax error on the rest.
                index = statements.len() - 1;
                end = statements[index].range.end;
            }
        }
        ranges.push(statement.range.start..end);
        index += 1;
    }
    ranges
}

/// `CREATE [TEMP|TEMPORARY] TRIGGER ...`.
fn creates_trigger(statement: &str) -> bool {
    let mut words = statement
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|word| !word.is_empty());
    if !words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("create"))
    {
        return false;
    }
    match words.next() {
        Some(word)
            if word.eq_ignore_ascii_case("temp") || word.eq_ignore_ascii_case("temporary") =>
        {
            words.next()
        }
        other => other,
    }
    .is_some_and(|word| word.eq_ignore_ascii_case("trigger"))
}

/// Type ids of the children of a mixed column (a dense Arrow union), one per
/// SQLite storage class. SQL NULL is a null of the `INTEGER` child.
pub const MIXED_INTEGER: i8 = 0;
pub const MIXED_REAL: i8 = 1;
pub const MIXED_TEXT: i8 = 2;
pub const MIXED_BLOB: i8 = 3;
/// A TEXT value that is not valid UTF-8, as the bytes SQLite holds. Show it as
/// bytes and never write it back as text.
pub const MIXED_TEXT_BYTES: i8 = 4;

/// The union fields of a mixed column.
fn mixed_fields() -> UnionFields {
    UnionFields::new(
        [
            MIXED_INTEGER,
            MIXED_REAL,
            MIXED_TEXT,
            MIXED_BLOB,
            MIXED_TEXT_BYTES,
        ],
        [
            Field::new("integer", DataType::Int64, true),
            Field::new("real", DataType::Float64, true),
            Field::new("text", DataType::Utf8, true),
            Field::new("blob", DataType::Binary, true),
            Field::new("text_bytes", DataType::Binary, true),
        ],
    )
}

/// One Arrow column from the values of a result column.
///
/// A column that holds a single storage class (and NULLs) keeps its native type:
/// `Int64`, `Float64`, `Utf8`, `Binary` (a column of only NULLs is `Utf8`). A
/// column that holds several, or any TEXT that is not UTF-8, is a dense union
/// whose child tells every cell's class (`MIXED_*`): an INTEGER `1` and a TEXT
/// `'1'`, a BLOB and the text `X'AB'` stay different values, and a BLOB is the
/// same `Binary` bytes whether or not the column is mixed.
fn build_array(values: Vec<SqlValue>) -> ArrayRef {
    let classes = values.iter().fold(0_u8, |classes, value| {
        classes
            | match value {
                SqlValue::Blob(_) => 1,
                SqlValue::Text(_) => 2,
                SqlValue::Real(_) => 4,
                SqlValue::Integer(_) => 8,
                SqlValue::TextBytes(_) => 16,
                SqlValue::Null => 0,
            }
    });
    match classes {
        1 => {
            let mut builder = BinaryBuilder::new();
            for value in values {
                match value {
                    SqlValue::Blob(value) => builder.append_value(value),
                    SqlValue::Null => builder.append_null(),
                    _ => unreachable!("non-binary value in homogeneous binary array"),
                }
            }
            Arc::new(builder.finish())
        }
        4 => Arc::new(Float64Array::from(
            values
                .into_iter()
                .map(|value| match value {
                    SqlValue::Real(value) => Some(value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        8 => Arc::new(Int64Array::from(
            values
                .into_iter()
                .map(|value| match value {
                    SqlValue::Integer(value) => Some(value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        0 | 2 => Arc::new(StringArray::from(
            values
                .into_iter()
                .map(|value| match value {
                    SqlValue::Text(value) => Some(value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        _ => build_mixed(values),
    }
}

fn build_mixed(values: Vec<SqlValue>) -> ArrayRef {
    let rows = values.len();
    let mut integers = Int64Builder::new();
    let mut reals = Float64Builder::new();
    let mut texts = StringBuilder::new();
    let mut blobs = BinaryBuilder::new();
    let mut text_bytes = BinaryBuilder::new();
    let mut type_ids = Vec::with_capacity(rows);
    let mut offsets = Vec::with_capacity(rows);
    // Row offset into its child: the child's length before the push.
    for value in values {
        let (id, offset) = match value {
            SqlValue::Null => (MIXED_INTEGER, integers.len()),
            SqlValue::Integer(_) => (MIXED_INTEGER, integers.len()),
            SqlValue::Real(_) => (MIXED_REAL, reals.len()),
            SqlValue::Text(_) => (MIXED_TEXT, texts.len()),
            SqlValue::Blob(_) => (MIXED_BLOB, blobs.len()),
            SqlValue::TextBytes(_) => (MIXED_TEXT_BYTES, text_bytes.len()),
        };
        match value {
            SqlValue::Null => integers.append_null(),
            SqlValue::Integer(value) => integers.append_value(value),
            SqlValue::Real(value) => reals.append_value(value),
            SqlValue::Text(value) => texts.append_value(value),
            SqlValue::Blob(value) => blobs.append_value(value),
            SqlValue::TextBytes(value) => text_bytes.append_value(value),
        }
        type_ids.push(id);
        offsets.push(i32::try_from(offset).unwrap_or(i32::MAX));
    }
    let children: Vec<ArrayRef> = vec![
        Arc::new(integers.finish()),
        Arc::new(reals.finish()),
        Arc::new(texts.finish()),
        Arc::new(blobs.finish()),
        Arc::new(text_bytes.finish()),
    ];
    Arc::new(
        UnionArray::try_new(
            mixed_fields(),
            ScalarBuffer::from(type_ids),
            Some(ScalarBuffer::from(offsets)),
            children,
        )
        .expect("the type ids and offsets built above are consistent with the children"),
    )
}

fn data_type_label(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::Int64 => "int64",
        DataType::Float64 => "float64",
        DataType::Binary => "binary",
        // Several storage classes: every cell carries its own (see `build_array`).
        DataType::Union(_, _) => "mixed",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_array, classify, creates_trigger, data_type_label, percent_decode, sqlite_path,
        statement_ranges, Ordering, SqlLexOptions, SqlValue, SqliteBackend, MIXED_BLOB,
        MIXED_INTEGER, MIXED_REAL, MIXED_TEXT, MIXED_TEXT_BYTES,
    };
    use arrow_array::cast::AsArray;
    use arrow_array::{Array, ArrayRef, UnionArray};
    use arrow_schema::DataType;
    use engine_driver_kit::{default_cell_text, DriverBackend, ExportedSymbols};

    fn array_value_text(array: &ArrayRef, row: usize) -> String {
        default_cell_text(array, row)
    }

    #[test]
    fn capabilities_advertise_typed_features_and_sql_dialect() {
        use engine_driver_kit::api::{Capability, DriverCapabilities};
        let raw = super::dbengine_capabilities();
        // SAFETY: the driver returns a NUL-terminated string it owns until freed.
        let json = unsafe { std::ffi::CStr::from_ptr(raw) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: raw came from dbengine_capabilities and is freed once.
        unsafe { super::dbengine_free_string(raw) };
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["features"],
            serde_json::json!([
                "navigator",
                "query.sql",
                "query.arrow_c_data",
                "operation.cancel",
                "mutation.insert",
                "mutation.update",
                "mutation.delete",
                "mutation.batch",
                "metadata.table_properties",
                "query.read_only",
                "query.run_to_end"
            ])
        );
        assert_eq!(value["sql_dialect"]["identifier_quote"], "\"");
        let capabilities: DriverCapabilities = serde_json::from_str(&json).unwrap();
        assert!(capabilities.has(Capability::QuerySql));
        assert!(capabilities.has(Capability::OperationCancel));
        assert!(capabilities.has(Capability::MutationBatch));
        assert!(capabilities.has(Capability::MetadataTableProperties));
        assert!(!capabilities.has(Capability::DatabaseCreate));
        assert!(!capabilities.has(Capability::QueryPageLease));
        assert_eq!(
            capabilities.navigator_kinds,
            ["database", "table", "view", "trigger", "column"]
        );
        let dialect = capabilities
            .sql_dialect
            .expect("sqlite advertises a dialect");
        assert_eq!(dialect.string_literal_quote, "'");
        assert!(!dialect.supports_schemas);
        assert!(!dialect.string_backslash_escapes);
    }

    #[test]
    fn parses_sqlite_paths_without_network_or_global_state() {
        assert_eq!(sqlite_path("sqlite://:memory:").unwrap(), ":memory:");
        assert_eq!(
            sqlite_path("sqlite:///tmp/a%20b.db").unwrap(),
            "/tmp/a b.db"
        );
        assert_eq!(
            sqlite_path("SQLITE:///tmp/a%20b.db").unwrap(),
            "/tmp/a b.db"
        );
        assert_eq!(percent_decode("a%2Fb").unwrap(), "a/b");
    }

    #[test]
    fn mixed_storage_classes_preserve_each_value() {
        let array = build_array(vec![
            SqlValue::Integer(1),
            SqlValue::Text("two".into()),
            SqlValue::Real(3.5),
            SqlValue::Null,
        ]);

        assert_eq!(array_value_text(&array, 0), "1");
        assert_eq!(array_value_text(&array, 1), "two");
        assert_eq!(array_value_text(&array, 2), "3.5");
        assert_eq!(array_value_text(&array, 3), "NULL");
        assert_eq!(classes(&array), [0, 2, 1, 0]);
        assert!(array
            .as_any()
            .downcast_ref::<UnionArray>()
            .unwrap()
            .child(MIXED_INTEGER)
            .is_null(1));
    }

    /// The class (union type id) of every cell of a mixed column.
    fn classes(array: &ArrayRef) -> Vec<i8> {
        let union = array
            .as_any()
            .downcast_ref::<UnionArray>()
            .expect("a mixed column");
        (0..union.len()).map(|row| union.type_id(row)).collect()
    }

    #[test]
    fn an_integer_and_the_text_of_the_same_digits_are_told_apart() {
        let array = build_array(vec![
            SqlValue::Integer(1),
            SqlValue::Text("1".into()),
            SqlValue::Real(1.0),
            SqlValue::Blob(vec![0xab]),
            SqlValue::Text("X'AB'".into()),
            SqlValue::Null,
            SqlValue::Text("NULL".into()),
        ]);
        let DataType::Union(fields, mode) = array.data_type() else {
            panic!("a mixed column is a union");
        };
        assert_eq!(*mode, arrow_schema::UnionMode::Dense);
        let names: Vec<_> = fields
            .iter()
            .map(|(id, field)| (id, field.name().as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (0, "integer"),
                (1, "real"),
                (2, "text"),
                (3, "blob"),
                (4, "text_bytes")
            ]
        );
        assert_eq!(
            classes(&array),
            [
                MIXED_INTEGER,
                MIXED_TEXT,
                MIXED_REAL,
                MIXED_BLOB,
                MIXED_TEXT,
                MIXED_INTEGER,
                MIXED_TEXT
            ]
        );
        let union = array.as_any().downcast_ref::<UnionArray>().unwrap();
        // The cell values are native: exact i64, a real BLOB, text as text.
        assert_eq!(
            union
                .value(0)
                .as_primitive::<arrow_array::types::Int64Type>()
                .value(0),
            1
        );
        assert_eq!(union.value(1).as_string::<i32>().value(0), "1");
        assert_eq!(union.value(3).as_binary::<i32>().value(0), &[0xab]);
        assert_eq!(union.value(4).as_string::<i32>().value(0), "X\'AB\'");
        // A real SQL NULL and the text `NULL` differ for the null bit.
        let formatter = engine_driver_kit::CellFormatter::new(&array);
        assert!(formatter.is_null(5) && !formatter.is_null(6));
        // The type label of the page column.
        assert_eq!(data_type_label(array.data_type()), "mixed");
    }

    #[test]
    fn a_blob_is_the_same_bytes_in_a_mixed_column_and_in_a_plain_one() {
        let plain = build_array(vec![SqlValue::Blob(vec![0, 0xff]), SqlValue::Null]);
        let mixed = build_array(vec![
            SqlValue::Blob(vec![0, 0xff]),
            SqlValue::Text("t".into()),
        ]);
        assert_eq!(array_value_text(&plain, 0), array_value_text(&mixed, 0));
        assert_eq!(array_value_text(&plain, 0), "X'00FF'");
        let union = mixed.as_any().downcast_ref::<UnionArray>().unwrap();
        assert_eq!(
            union.value(0).as_binary::<i32>().value(0),
            plain.as_binary::<i32>().value(0)
        );
    }

    #[test]
    fn text_that_is_not_utf8_is_its_own_class_even_alone() {
        let bytes = vec![b'c', 0xe9];
        let array = build_array(vec![SqlValue::TextBytes(bytes.clone()), SqlValue::Null]);
        assert_eq!(classes(&array), [MIXED_TEXT_BYTES, MIXED_INTEGER]);
        let union = array.as_any().downcast_ref::<UnionArray>().unwrap();
        assert_eq!(union.value(0).as_binary::<i32>().value(0), &bytes[..]);
        // It is not a BLOB and not text: the class says so.
        assert_ne!(MIXED_TEXT_BYTES, MIXED_BLOB);
        assert_ne!(MIXED_TEXT_BYTES, MIXED_TEXT);
    }

    #[test]
    fn a_mixed_column_survives_the_arrow_c_data_interface_and_slicing() {
        use arrow_array::ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema};
        let array = build_array(vec![
            SqlValue::Integer(1),
            SqlValue::Text("1".into()),
            SqlValue::Blob(vec![9]),
            SqlValue::Real(2.5),
        ]);
        // The kit hands out zero-copy slices of the page.
        let sliced = array.slice(1, 3);
        let data = sliced.to_data();
        let schema = FFI_ArrowSchema::try_from(data.data_type()).unwrap();
        let ffi = FFI_ArrowArray::new(&data);
        // SAFETY: both structs were just exported from `data`.
        let imported = unsafe { from_ffi(ffi, &schema) }.unwrap();
        let imported = arrow_array::make_array(imported);
        assert_eq!(classes(&imported), [MIXED_TEXT, MIXED_BLOB, MIXED_REAL]);
        assert_eq!(array_value_text(&imported, 0), "1");
        assert_eq!(array_value_text(&imported, 1), "X'09'");
        assert_eq!(array_value_text(&imported, 2), "2.5");
    }

    #[test]
    fn mixed_integer_and_real_values_preserve_exact_integer_text() {
        let array = build_array(vec![
            SqlValue::Integer(9_007_199_254_740_993),
            SqlValue::Integer(i64::MAX),
            SqlValue::Real(3.5),
            SqlValue::Null,
        ]);

        assert!(matches!(array.data_type(), DataType::Union(_, _)));
        assert_eq!(array_value_text(&array, 0), "9007199254740993");
        assert_eq!(array_value_text(&array, 1), "9223372036854775807");
        assert_eq!(array_value_text(&array, 2), "3.5");
        assert_eq!(array_value_text(&array, 3), "NULL");
    }

    #[test]
    fn mixed_blob_values_use_distinguishable_sqlite_literals() {
        let array = build_array(vec![
            SqlValue::Blob(vec![0xab, 0xcd]),
            SqlValue::Blob(vec![0xef, 0x01]),
            SqlValue::Text("ABCD".into()),
            SqlValue::Integer(42),
            SqlValue::Null,
        ]);

        assert!(matches!(array.data_type(), DataType::Union(_, _)));
        assert_eq!(array_value_text(&array, 0), "X'ABCD'");
        assert_eq!(array_value_text(&array, 1), "X'EF01'");
        assert_eq!(array_value_text(&array, 2), "ABCD");
        assert_eq!(array_value_text(&array, 3), "42");
        assert_eq!(array_value_text(&array, 4), "NULL");
    }

    #[test]
    fn homogeneous_storage_classes_keep_native_arrow_types() {
        let integers = build_array(vec![SqlValue::Integer(1), SqlValue::Null]);
        let reals = build_array(vec![SqlValue::Real(1.5), SqlValue::Null]);
        let blobs = build_array(vec![SqlValue::Blob(vec![0xab]), SqlValue::Null]);

        assert_eq!(integers.data_type(), &DataType::Int64);
        assert_eq!(reals.data_type(), &DataType::Float64);
        assert_eq!(blobs.data_type(), &DataType::Binary);
        assert_eq!(array_value_text(&blobs, 0), "X'AB'");
    }

    #[test]
    fn a_broken_connection_refuses_later_calls_with_not_connected() {
        let connection = SqliteBackend::connect(
            &serde_json::from_value(serde_json::json!({"uri": "sqlite://:memory:"})).unwrap(),
        )
        .ok()
        .unwrap();
        assert!(connection.check_usable("query").is_ok());
        connection.broken.store(true, Ordering::Release);
        for area in ["query", "mutation", "navigator", "describe"] {
            let error = connection.check_usable(area).unwrap_err();
            assert_eq!(
                error.code.as_deref(),
                Some(format!("{area}.not_connected").as_str())
            );
        }
    }

    fn ranges(sql: &str) -> Vec<&str> {
        let class = classify(sql, &SqlLexOptions::sqlite());
        statement_ranges(sql, &class)
            .into_iter()
            .map(|range| sql[range].trim().trim_end_matches(';').trim())
            .collect()
    }

    #[test]
    fn trigger_bodies_stay_in_one_statement() {
        let trigger =
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1; SELECT CASE WHEN 1 THEN 2 END; END";
        assert_eq!(ranges(trigger), [trigger]);
        let script = format!("CREATE TABLE a(x); {trigger}; SELECT 3 AS n; CREATE TEMP TRIGGER u BEFORE DELETE ON a BEGIN SELECT 9; END");
        assert_eq!(
            ranges(&script),
            [
                "CREATE TABLE a(x)",
                trigger,
                "SELECT 3 AS n",
                "CREATE TEMP TRIGGER u BEFORE DELETE ON a BEGIN SELECT 9; END"
            ]
        );
        // `trigger` elsewhere in a statement does not glue statements together.
        assert_eq!(
            ranges("CREATE TABLE trigger_log(x); SELECT 'trigger'; SELECT 2"),
            [
                "CREATE TABLE trigger_log(x)",
                "SELECT 'trigger'",
                "SELECT 2"
            ]
        );
        // Brackets, quotes and comments with `;` do not split.
        assert_eq!(
            ranges("SELECT [a;b] FROM t; /* ; */ SELECT 'x;y'"),
            ["SELECT [a;b] FROM t", "/* ; */ SELECT 'x;y'"]
        );
        // An unterminated trigger is left whole for SQLite to reject.
        assert_eq!(
            ranges("CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1; SELECT 2").len(),
            1
        );
    }

    #[test]
    fn creating_a_trigger_is_recognised_by_its_head() {
        assert!(creates_trigger("create  trigger t ..."));
        assert!(creates_trigger("CREATE TEMPORARY TRIGGER t"));
        assert!(!creates_trigger("CREATE TABLE trigger(x)"));
        assert!(!creates_trigger("CREATE INDEX i ON trigger(x)"));
        assert!(!creates_trigger("SELECT trigger"));
    }

    #[test]
    fn exported_symbols_match_capabilities() {
        ExportedSymbols::CANCEL_OPERATION
            .union(ExportedSymbols::MUTATE)
            .union(ExportedSymbols::DESCRIBE_OBJECT)
            .validate(&SqliteBackend::capabilities())
            .expect("sqlite exports exactly the optional symbols it advertises");
    }

    #[test]
    fn a_cancel_only_matches_its_own_operation_while_it_is_cancellable() {
        use super::{CancelState, OperationId};

        let state = CancelState::new();
        assert!(!state.stop_requested(), "nothing runs, nothing was asked");
        // A cancel before the operation is cancellable is remembered ...
        state.request(OperationId(7));
        assert!(!state.stop_requested());
        assert!(!state.is_running(OperationId(7)));
        {
            let _running = state.run(OperationId(7));
            // ... and seen by its first look.
            assert!(state.stop_requested());
            assert!(state.is_running(OperationId(7)));
        }
        // Past the cancellable phase nothing is stopped, and the request is forgotten: an
        // operation that reused the id would not start cancelled.
        assert!(!state.stop_requested());
        let _again = state.run(OperationId(7));
        assert!(!state.stop_requested());
        drop(_again);

        // A cancel for another operation never stops the running one, and survives it.
        let running = state.run(OperationId(8));
        state.request(OperationId(9));
        assert!(!state.stop_requested());
        drop(running);
        let _next = state.run(OperationId(9));
        assert!(state.stop_requested());
    }

    /// Operation 8 runs and is cancelled between two statements; the late cancel of the finished
    /// operation 7 and the cancel of the waiting operation 9 arrive afterwards (any order).
    #[test]
    fn a_late_cancel_of_another_operation_does_not_erase_the_running_ones() {
        use super::{CancelState, OperationId};

        for others in [[7, 9], [9, 7]] {
            let state = CancelState::new();
            let running = state.run(OperationId(8));
            state.request(OperationId(8));
            assert!(state.stop_requested());
            for other in others {
                state.request(OperationId(other));
                assert!(state.stop_requested(), "{other} erased the request of 8");
            }
            // Asking again for the running one changes nothing either.
            state.request(OperationId(8));
            assert!(state.stop_requested());
            drop(running);
            // Nothing outlives the operation.
            assert!(!state.stop_requested());
            let _next = state.run(OperationId(9));
            assert!(!state.stop_requested());
        }
    }

    #[test]
    fn an_interrupt_in_flight_delays_the_end_of_the_operation_and_none_starts_after_it() {
        use super::{CancelState, OperationId};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let state = Arc::new(CancelState::new());
        let running = state.run(OperationId(7));
        let sent = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let canceller = {
            let (state, sent) = (Arc::clone(&state), Arc::clone(&sent));
            std::thread::spawn(move || {
                state.interrupt_if_running(OperationId(7), || {
                    entered_tx.send(()).unwrap();
                    // Preempted between the check and the call.
                    std::thread::sleep(Duration::from_millis(250));
                    sent.store(true, Ordering::SeqCst);
                });
            })
        };
        entered_rx.recv().unwrap();
        let started = Instant::now();
        drop(running);
        assert!(
            sent.load(Ordering::SeqCst),
            "the interrupt landed before the operation ended"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "{:?}",
            started.elapsed()
        );
        canceller.join().unwrap();
        // After the operation ended, nothing is sent.
        state.interrupt_if_running(OperationId(7), || panic!("the operation is over"));
        // An operation with nothing in flight ends at once.
        let again = state.run(OperationId(8));
        let started = Instant::now();
        drop(again);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// Benchmark, not a check (`cargo test --release -p dbengine-sqlite -- --ignored
    /// progress_handler_overhead --nocapture`): the same CPU-bound statement with and without
    /// the cancel look every `CANCEL_CHECK_OPS` instructions.
    #[test]
    #[ignore = "benchmark"]
    fn progress_handler_overhead() {
        use super::{CancelState, CANCEL_CHECK_OPS};
        use std::sync::Arc;
        use std::time::Instant;

        const SQL: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c \
                           WHERE x < 20000000) SELECT count(*) FROM c";
        let plain = rusqlite::Connection::open_in_memory().unwrap();
        let watched = rusqlite::Connection::open_in_memory().unwrap();
        let state = Arc::new(CancelState::new());
        let _running = state.run(super::OperationId(7));
        let handler = Arc::clone(&state);
        watched
            .progress_handler(CANCEL_CHECK_OPS, Some(move || handler.stop_requested()))
            .unwrap();
        let time = |connection: &rusqlite::Connection| {
            (0..5)
                .map(|_| {
                    let started = Instant::now();
                    let rows: i64 = connection.query_row(SQL, [], |row| row.get(0)).unwrap();
                    assert_eq!(rows, 20_000_000);
                    started.elapsed()
                })
                .min()
                .unwrap()
        };
        // Interleaved so neither side gets the warmer cache.
        let (mut best_plain, mut best_watched) = (time(&plain), time(&watched));
        best_plain = best_plain.min(time(&plain));
        best_watched = best_watched.min(time(&watched));
        println!(
            "20M-row recursive count: plain {best_plain:?}, with cancel look {best_watched:?} \
             ({:+.2} %)",
            (best_watched.as_secs_f64() / best_plain.as_secs_f64() - 1.0) * 100.0
        );
    }
}
