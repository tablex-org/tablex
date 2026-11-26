//! DuckDB on the shared driver kit: the kit owns the session, page registry, Arrow leases, cancel
//! state and the `dbengine_*` exports; this crate implements the SQL conformance rules of
//! `engine_driver_api::sql`.
//!
//! * URI: `duckdb://<file>` or `duckdb://:memory:`; `?mode=ro` opens the file read-only (the file
//!   must exist). Several connections to one file in this process share one DuckDB instance (the
//!   library refuses a second one on the same file): each gets its own DuckDB connection to it, so
//!   they run side by side and see each other's committed data.
//! * `?external_access=off` (what an agent's session asks for) opens the instance with
//!   `enable_external_access = false` and a locked configuration: SQL can then read and write no
//!   file or URL besides the database itself, `ATTACH`, `COPY`, `INSTALL` and the file readers fail
//!   in the engine, and a `SET` cannot undo it. DuckDB keeps that setting for the whole instance,
//!   so the instances of one file are told apart by it too: a connection that asks for a locked
//!   instance of a file already open with file access (or the reverse) is refused instead of
//!   widening or narrowing the other's access.
//! * Results are Arrow batches from DuckDB's own Arrow export (the duckdb crate links the same
//!   arrow version as this workspace), read from a streaming result: a page reads only the chunks
//!   it needs (`offset + limit + 1` rows, one lookahead row makes `truncated` exact), and slices
//!   them without copying; only a page that spans several chunks is joined. `total_count` is
//!   `None`. Only a single pure read (`SELECT`/`WITH ... SELECT`/`VALUES`/`TABLE`/`SHOW`/`EXPLAIN`,
//!   by `engine_driver_kit::sql::classify`) is pageable; any other statement or script runs once
//!   (to its end) and `offset > 0` is refused with `query.not_pageable`. A script returns the rows
//!   of its last statement that produces a result set (`SELECT 1 AS a; INSERT ...` returns `a`); a
//!   script without one is a zero-column page whose `total_count` is the rows DML changed.
//! * DuckDB types arrive as the matching Arrow ones: `HUGEINT`/`DECIMAL` `Decimal128`, `INTERVAL`
//!   `Interval(MonthDayNano)`, `LIST`/`STRUCT`/`MAP` nested arrays, `UUID` `Utf8`, `ENUM` a
//!   dictionary, `BLOB` `Binary`, `TIMESTAMPTZ` a timestamp in `UTC`. A `UNION` becomes `Utf8` (its
//!   text), so the grid never reads it as another driver's mixed column.
//! * `payload.read_only` runs the text in `BEGIN TRANSACTION READ ONLY` (DuckDB has no per-
//!   statement setting), after the lexer refused anything that is not a plain read. Inside a
//!   transaction the user opened, the lexer check alone applies: a transaction cannot be nested.
//! * Transactions: DuckDB has no savepoints. A mutation outside a user transaction runs in its own;
//!   inside one it joins it, and a change that fails before anything was written (no row, several
//!   rows, bad value) leaves the transaction as it was. A batch that fails after some changes were
//!   written inside a user transaction reports `mutation.partially_applied`: those changes stay in
//!   the user's transaction. The driver follows the user's `BEGIN`/`COMMIT`/`ROLLBACK`/`START`
//!   statements, and after any of them (and after an `EXECUTE`, which can run a prepared `COMMIT`)
//!   asks DuckDB itself, since the C API has no call for it: `txid_current()` is the same in every
//!   statement of an open transaction. A `COMMIT` that does not parse leaves the flag as it was.
//!   A `BEGIN` while one is open is not
//!   run: DuckDB refuses it and the refusal aborts the open transaction, where PostgreSQL only
//!   warns, and the app sends a `BEGIN` before every statement of a manual-commit tab.
//! * Text is split by the SQL lexer (`SqlLexOptions::duckdb()`, mirroring DuckDB's own tokenizer), and
//!   before anything runs the driver asks DuckDB's `duckdb_extract_statements` (on a private
//!   parse-only instance, `split.rs`) and refuses a statement DuckDB does not read as exactly one: the
//!   duckdb crate's `prepare` RUNS every statement it splits off by itself before the last.
//! * Navigator and `describe_object`: see [`catalog`]. They run on a second DuckDB connection to
//!   the same instance, so they do not wait behind a running user query, and they see committed
//!   data only.
//! * CSV is core; the `parquet` and `json` extensions are linked in statically (cargo features), so
//!   `read_parquet`, `read_json_auto` and `COPY ... (FORMAT parquet | json)` work with no `INSTALL`,
//!   no `LOAD` and no network. No other extension is bundled.
//! * `create_database` does not apply (a database is a file) and is not advertised.

mod catalog;
mod mutation;
mod split;

use arrow_array::builder::StringBuilder;
use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use duckdb::{AccessMode, Config, Connection, InterruptHandle};
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::{
    Capability, DriverCapabilities, MutationRequest, NavigatorPage, NavigatorRequest, OperationId,
    PageFlags, QueryRequest, SqlDialectInfo,
};
use engine_driver_kit::sql::{
    classify, normalize_duckdb_spaces, Classification, Keyword, SqlLexOptions, Statement,
};
use engine_driver_kit::{
    page_columns, ActiveOperation, BackendError, ConnectRequest, DriverBackend, MutationOutcome,
    OperationGuard, QueryOutput,
};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

const DRIVER: &str = "duckdb";
/// Rows of a page when the request's `limit` is `0`.
const DEFAULT_PAGE_ROWS: u64 = 200;
/// Hard cap on the rows of one page.
const MAX_PAGE_ROWS: u64 = 100_000;

/// The message of a cancel seen before a statement started (DuckDB's own word for the same thing).
const CANCELLED_BEFORE_START: &str = "Interrupted!";

/// The longest an operation waits for an interrupt that is being sent as it ends.
const INTERRUPT_WAIT: Duration = Duration::from_secs(1);

/// Pauses between the interrupts a cancel repeats while its statement still runs.
const REINTERRUPT_AFTER: [Duration; 4] = [
    Duration::from_millis(10),
    Duration::from_millis(40),
    Duration::from_millis(150),
    Duration::from_millis(500),
];

/// Which operation runs on the user connection and which one a cancel was asked for.
///
/// `duckdb_interrupt` only stops what runs at that instant, and DuckDB forgets an interrupt when
/// the next statement starts: a cancel that lands before the statement starts would be lost. So
/// the request is recorded and looked at before every statement of the operation
/// ([`CancelState::stop_requested`]), and a cancel repeats its interrupt while its operation still
/// runs ([`CancelState::cancel`]). A cancel only ever matches the operation it named, and only
/// while that operation's cancellable phase lasts. Interrupts being sent are counted, and an
/// operation does not end until none is in flight ([`RunningOperation`]'s drop), so a canceller
/// that was preempted can never interrupt the next operation's statement.
struct CancelState {
    running: AtomicU64,
    requested: AtomicU64,
    /// `duckdb_interrupt` calls between "this operation is running" and "returned".
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

    /// Whether the running operation was asked to stop.
    fn stop_requested(&self) -> bool {
        let running = self.running.load(Ordering::Acquire);
        running != Self::NONE && running == self.requested.load(Ordering::Acquire)
    }

    /// Records the cancel of `operation_id`, unless that would erase the cancel of the running
    /// operation: a late cancel for an operation that already ended, or for one still waiting,
    /// must not let a script that was cancelled between two statements run on. (The kit
    /// remembers the cancel of a waiting operation and reports it when it activates.)
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

    fn is_running(&self, operation_id: OperationId) -> bool {
        self.running.load(Ordering::SeqCst) == operation_id.0
    }

    /// Calls `interrupt` if `operation_id` is in its cancellable phase, and counts it as in
    /// flight from before the check until it returned (the operation clears the running mark,
    /// then reads the count, both `SeqCst`: one side always sees the other).
    fn interrupt_if_running(&self, operation_id: OperationId, interrupt: impl FnOnce()) -> bool {
        struct InFlight<'a>(&'a AtomicUsize);
        impl Drop for InFlight<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.interrupts.fetch_add(1, Ordering::SeqCst);
        let _in_flight = InFlight(&self.interrupts);
        let running = self.is_running(operation_id);
        if running {
            interrupt();
        }
        running
    }

    /// The cancel: record it, interrupt the running statement, and repeat while the operation
    /// still runs (an interrupt that lands in the instant before a statement starts is forgotten
    /// by DuckDB, and the statement then runs on).
    fn cancel(&self, operation_id: OperationId, interrupt: &InterruptHandle) {
        self.request(operation_id);
        if !self.interrupt_if_running(operation_id, || interrupt.interrupt()) {
            return;
        }
        for pause in REINTERRUPT_AFTER {
            std::thread::sleep(pause);
            if !self.interrupt_if_running(operation_id, || interrupt.interrupt()) {
                return;
            }
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
struct DuckCanceller {
    interrupt: Arc<InterruptHandle>,
    state: Arc<CancelState>,
}

/// An open DuckDB instance. Its connection is only used to make the connections that do the
/// work; it must outlive them (a clone does not own the database).
struct Shared {
    root: Mutex<Connection>,
}

/// The file instances open in this process: path, access, instance.
type OpenInstances = Vec<(PathBuf, Access, Weak<Shared>)>;

/// How an instance was opened: DuckDB fixes both for the whole instance.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Access {
    read_only: bool,
    /// `enable_external_access = false` and a locked configuration.
    no_external_access: bool,
}

impl Access {
    fn describe(self) -> String {
        format!(
            "{}{}",
            if self.read_only { "read-only" } else { "for writing" },
            if self.no_external_access { " without file access" } else { "" }
        )
    }
}
static OPEN: Mutex<OpenInstances> = Mutex::new(Vec::new());

struct DuckConnection {
    connection: Mutex<Connection>,
    /// For catalog calls: another connection to the same instance, so they never wait behind a
    /// running user query.
    metadata: Mutex<Connection>,
    cancel: Arc<CancelState>,
    interrupt: Arc<InterruptHandle>,
    /// The user ran `BEGIN` and has not ended it yet.
    in_transaction: AtomicBool,
    /// A statement that may have changed the transaction ran and DuckDB could not be asked what
    /// it did (an interrupt): `in_transaction` is then `false` (a read-only scope that finds a
    /// transaction open fails, it never runs unscoped) and the question is asked again before the
    /// next statement ([`DuckConnection::settle_transaction`]).
    transaction_unknown: AtomicBool,
    /// The instance has file access off and a locked configuration, as DuckDB itself says (not
    /// as the URI asked): sent in the connect reply for hosts that must not let SQL reach files.
    file_access_locked: bool,
    /// A `ROLLBACK` failed: the transaction state is unknown.
    broken: AtomicBool,
    /// Last: dropped after the connections made from its database.
    _shared: Arc<Shared>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl DuckConnection {
    fn lock(&self) -> MutexGuard<'_, Connection> {
        lock(&self.connection)
    }

    fn check_usable(&self, area: &str) -> Result<(), BackendError> {
        if self.broken.load(Ordering::Acquire) {
            Err(BackendError::with_code(
                format!("{area}.not_connected"),
                "the connection is in an unknown state after a failed rollback; reconnect",
            ))
        } else {
            Ok(())
        }
    }

    /// Asks DuckDB where the transaction stands when the last answer failed, so a `BEGIN` of a
    /// manual-commit tab is skipped (or run) by what is true now.
    fn settle_transaction(&self, connection: &Connection) {
        if self.transaction_unknown.load(Ordering::Acquire) {
            if let Some(open) = transaction_open(connection) {
                self.in_transaction.store(open, Ordering::Release);
                self.transaction_unknown.store(false, Ordering::Release);
            }
        }
    }

    /// Runs a catalog read on the metadata connection.
    fn with_metadata<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        read(&lock(&self.metadata))
    }
}

/// DuckDB backend.
struct DuckBackend;

impl DriverBackend for DuckBackend {
    type Connection = DuckConnection;
    type PageContext = ();
    type Canceller = DuckCanceller;

    fn capabilities() -> DriverCapabilities {
        let mut capabilities = DriverCapabilities::new(
            DRIVER,
            "DuckDB",
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
                Capability::MetadataSchemaColumns,
                Capability::QueryReadOnly,
                Capability::QueryRunToEnd,
            ]
            .map(|capability| capability.as_str().to_owned())
            .into(),
        );
        capabilities.uri_schemes = vec![DRIVER.into()];
        capabilities.editors = vec!["sql".into()];
        capabilities.navigator_kinds = ["database", "schema", "table", "view", "column"]
            .map(String::from)
            .into();
        capabilities.sql_dialect = Some(SqlDialectInfo {
            identifier_quote: "\"".into(),
            string_literal_quote: "'".into(),
            supports_schemas: true,
            database_qualified_names: false,
            // `LIKE` on a number is a binder error in DuckDB: cast the column first.
            like_requires_text_cast: true,
            string_backslash_escapes: false,
            double_quote_identifiers: true,
            bracket_identifiers: false,
        });
        capabilities
    }

    fn connect(request: &ConnectRequest) -> Result<DuckConnection, BackendError> {
        let target = parse_uri(&request.uri)
            .map_err(|error| BackendError::with_code("connect.bad_request", error))?;
        let shared = match &target.path {
            None => Arc::new(Shared {
                root: Mutex::new(open_instance(None, target.access)?),
            }),
            Some(path) => shared_instance(path, target.access)?,
        };
        let (connection, metadata) = {
            let root = lock(&shared.root);
            (clone_connection(&root)?, clone_connection(&root)?)
        };
        let interrupt = connection.interrupt_handle();
        let file_access_locked = target.access.no_external_access && engine_locks_file_access(&connection);
        Ok(DuckConnection {
            connection: Mutex::new(connection),
            metadata: Mutex::new(metadata),
            cancel: Arc::new(CancelState::new()),
            interrupt,
            in_transaction: AtomicBool::new(false),
            transaction_unknown: AtomicBool::new(false),
            file_access_locked,
            broken: AtomicBool::new(false),
            _shared: shared,
        })
    }

    fn connection_file_access_locked(connection: &DuckConnection) -> bool {
        connection.file_access_locked
    }

    fn list_nav_nodes(
        connection: &DuckConnection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        connection.check_usable("navigator")?;
        connection.with_metadata(|conn| catalog::list(conn, request))
    }

    fn describe_object(
        connection: &DuckConnection,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        connection.check_usable("describe")?;
        connection.with_metadata(|conn| catalog::describe(conn, request))
    }

    fn open_query(
        connection: &DuckConnection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput, BackendError> {
        if request.kind != "sql" && request.kind != "sql_query" {
            return Err(BackendError::with_code(
                "query.unsupported",
                "DuckDB accepts kind=sql",
            ));
        }
        let flag = |name: &str| {
            request
                .payload
                .get(name)
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        };
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
        // DuckDB rewrites some Unicode spaces to ASCII ones before it tokenizes, and the lexer reads
        // them as word bytes: everything below (the split, `check_split`, the statements `prepare`
        // gets) works on the text as DuckDB reads it.
        let normalized = normalize_duckdb_spaces(sql);
        let sql = normalized.as_deref().unwrap_or(sql);
        let classification = classify(sql, &LEX);
        if classification.is_empty() {
            return Err(BackendError::with_code(
                "query.bad_request",
                "the SQL contains no statement",
            ));
        }
        check_split(sql, &classification)?;
        let read_only = flag("read_only");
        // The READ ONLY scope is a transaction, and the text could end it (`COMMIT`) or run
        // what it does not stop: refuse any statement that is not a plain read.
        if let (true, Some(refusal)) = (read_only, classification.read_only_refusal()) {
            return Err(refusal.into());
        }
        if let (Some(danger), false) = (&classification.danger, flag("confirmed")) {
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
        let page = Page {
            offset: request.offset,
            limit,
            run_to_end: flag("run_to_end"),
        };
        let (batch, truncated) = match query(
            connection,
            operation,
            sql,
            &classification,
            page,
            read_only,
        )? {
            QueryResult::Rows { batch, truncated } => (batch, truncated),
            QueryResult::Command { affected_rows } => {
                return Ok(QueryOutput::command_complete(affected_rows, ()));
            }
        };
        let columns = page_columns(&batch, data_type_label);
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
        connection: &DuckConnection,
        request: &MutationRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        let plan = mutation::Plan::from_request(request)?;
        connection.check_usable("mutation")?;
        let guard = connection.lock();
        let active = activate(operation, guard, "mutation")?;
        let _running = connection.cancel.run(active.operation_id());
        if connection.cancel.stop_requested() {
            return Err(BackendError::with_code(
                codes::MUTATION_CANCELLED,
                CANCELLED_BEFORE_START,
            ));
        }
        connection.settle_transaction(&active);
        let joins_user_transaction = connection.in_transaction.load(Ordering::Acquire);
        let outcome = mutation::apply(&active, &plan, joins_user_transaction, &|| {
            connection.cancel.stop_requested()
        });
        if outcome.broken {
            connection.broken.store(true, Ordering::Release);
        }
        match outcome.result {
            Ok(affected) => Ok(MutationOutcome::new(affected)),
            Err(failure) => Err(failure.into_error(active.cancel_requested())),
        }
    }

    fn canceller(connection: &DuckConnection) -> Result<DuckCanceller, BackendError> {
        Ok(DuckCanceller {
            interrupt: Arc::clone(&connection.interrupt),
            state: Arc::clone(&connection.cancel),
        })
    }

    /// Takes no connection lock, as `cancel` requires: it records the request and interrupts the
    /// statement through DuckDB's interrupt handle, only while this operation is the one running.
    fn cancel(canceller: &DuckCanceller, operation_id: OperationId) -> Result<(), BackendError> {
        canceller.state.cancel(operation_id, &canceller.interrupt);
        Ok(())
    }
}

engine_driver_kit::export_driver!(DuckBackend; cancel_operation, mutate, describe_object);

/// How DuckDB's PEG parser tokenizes text (`src/parser/peg/tokenizer` of DuckDB): dollar quotes,
/// `E'..'` strings, nested block comments, `"` and `` ` `` names, no brackets. This build has no
/// PostgreSQL parser. Where this and DuckDB could still differ, [`check_split`] decides.
const LEX: SqlLexOptions = SqlLexOptions::duckdb();

/// Refuses text in which DuckDB does not read each statement the lexer found as exactly one (text
/// DuckDB cannot parse is let through: running it fails before anything executes).
/// The duckdb crate's `prepare` runs every statement it splits off by itself before the last, so
/// it must never be handed one that DuckDB would split: those statements would run past the
/// classification (the read-only scope, the agent's classifier). Runs before anything executes.
fn check_split(sql: &str, classification: &Classification) -> Result<(), BackendError> {
    for statement in &classification.statements {
        let text = &sql[statement.range.clone()];
        // DuckDB runs its Unicode-space rewrite again on each slice it is handed, starting in the
        // plain state: after a `/* ' */` (which its scan takes for a quote) the whole text may
        // need no rewrite and a later slice does. The slice would then be a different statement
        // from the one that was classified. The text is refused, so what is classified and what
        // runs stay the same bytes.
        if normalize_duckdb_spaces(text).is_some() {
            return Err(BackendError::with_code(
                "query.bad_request",
                "a Unicode space character (U+00A0, U+2000 to U+200B, U+202F, U+205F, U+2060, \
                 U+3000, U+FEFF) outside quotes makes DuckDB read this statement differently from \
                 how the text splits: write an ASCII space in its place",
            ));
        }
        match split::statement_count(text) {
            Ok(split::Count::Statements(1) | split::Count::ParseError(_)) => {}
            Ok(split::Count::Statements(count)) => {
                return Err(BackendError::with_code(
                    "query.bad_request",
                    format!(
                        "DuckDB finds {count} statements where the SQL lexer finds one, so \
                         this text is not run: separate statements with `;` and quote names \
                         and strings in the usual way"
                    ),
                ))
            }
            Err(message) => return Err(BackendError::new(message)),
        }
    }
    Ok(())
}

/// `OperationGuard::activate`, with a cancel that arrived while the operation waited for the
/// connection reported as `<area>.cancelled`.
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

// ---------------------------------------------------------------------------------- connecting

struct Target {
    /// `None`: in memory.
    path: Option<PathBuf>,
    access: Access,
}

fn parse_uri(uri: &str) -> Result<Target, String> {
    let prefix = "duckdb://";
    let Some(uri_prefix) = uri.get(..prefix.len()) else {
        return Err("DuckDB URI must start with duckdb://".to_string());
    };
    if !uri_prefix.eq_ignore_ascii_case(prefix) {
        return Err("DuckDB URI must start with duckdb://".to_string());
    }
    let rest = &uri[prefix.len()..];
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if path.is_empty() || path == "/" {
        return Err("DuckDB URI path is empty".into());
    }
    let option = |name: &str, wanted: &str| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .any(|(key, value)| key == name && value.eq_ignore_ascii_case(wanted))
    };
    let no_external_access = option("external_access", "off");
    if path == ":memory:" {
        return Ok(Target {
            path: None,
            access: Access {
                read_only: false,
                no_external_access,
            },
        });
    }
    Ok(Target {
        path: Some(PathBuf::from(percent_decode(path)?)),
        access: Access {
            read_only: option("mode", "ro"),
            no_external_access,
        },
    })
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bad = || "invalid percent escape in DuckDB URI".to_string();
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let encoded = bytes.get(index + 1..index + 3).ok_or_else(bad)?;
            let text = std::str::from_utf8(encoded).map_err(|_| bad())?;
            out.push(u8::from_str_radix(text, 16).map_err(|_| bad())?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

fn duck_failure(error: &duckdb::Error) -> String {
    match error {
        duckdb::Error::DuckDBFailure(_, Some(message)) => message.clone(),
        other => other.to_string(),
    }
}

fn config(access: Access) -> Result<Config, BackendError> {
    let fail = |error: duckdb::Error| BackendError::new(duck_failure(&error));
    // A desktop client does not fetch code from the network on its own: no extension is installed
    // behind the user's back (an installed one still loads when a query needs it).
    let mut config = Config::default()
        .with("autoinstall_known_extensions", "false")
        .map_err(fail)?;
    if access.read_only {
        config = config.access_mode(AccessMode::ReadOnly).map_err(fail)?;
    }
    if access.no_external_access {
        config = config.with("enable_external_access", "false").map_err(fail)?;
    }
    Ok(config)
}

fn open_instance(path: Option<&Path>, access: Access) -> Result<Connection, BackendError> {
    let config = config(access)?;
    let connection = match path {
        None => Connection::open_in_memory_with_flags(config),
        Some(path) => Connection::open_with_flags(path, config),
    }
    .map_err(|error| BackendError::new(duck_failure(&error)))?;
    if access.no_external_access {
        // After the open, so no option of the config is left unapplied behind the lock.
        connection
            .execute_batch("SET lock_configuration = true")
            .map_err(|error| BackendError::new(duck_failure(&error)))?;
    }
    Ok(connection)
}

fn clone_connection(root: &Connection) -> Result<Connection, BackendError> {
    root.try_clone()
        .map_err(|error| BackendError::new(duck_failure(&error)))
}

/// The instance of `path`: the one already open in this process when there is one.
fn shared_instance(path: &Path, access: Access) -> Result<Arc<Shared>, BackendError> {
    let key = instance_key(path);
    let mut open = lock(&OPEN);
    open.retain(|(_, _, instance)| instance.strong_count() > 0);
    if let Some((_, was, instance)) = open.iter().find(|(open, _, _)| *open == key) {
        if let Some(shared) = instance.upgrade() {
            if *was != access {
                return Err(BackendError::with_code(
                    "connect.failed",
                    format!(
                        "this file is already open {} in this session; close that connection first",
                        was.describe()
                    ),
                ));
            }
            return Ok(shared);
        }
    }
    let shared = Arc::new(Shared {
        root: Mutex::new(open_instance(Some(path), access)?),
    });
    // The file exists now (DuckDB creates it when it opens): its real path is the key, so a second
    // connection finds this instance whichever way it spells the path.
    let key = std::fs::canonicalize(path).unwrap_or(key);
    open.push((key, access, Arc::downgrade(&shared)));
    Ok(shared)
}

/// The key of the instance of `path`: its real path. A file that is not there yet (DuckDB creates it
/// on the first open) gets the real path of its folder and its own name, which is what its real path
/// will be: a temp folder on macOS is a link (`/var` is `/private/var`), and a key that changed once
/// the file existed would keep two connections to one file apart.
fn instance_key(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let folder = match path.parent() {
        Some(folder) if !folder.as_os_str().is_empty() => folder,
        _ => Path::new("."),
    };
    match (std::fs::canonicalize(folder), path.file_name()) {
        (Ok(folder), Some(name)) => folder.join(name),
        _ => path.to_path_buf(),
    }
}

// ------------------------------------------------------------------------------------- errors

/// Maps a DuckDB error. An interrupt the user asked for (`cancel`) is `<area>.cancelled`; one
/// nobody requested is retryable `<area>.interrupted`; a write refused by a read-only scope or
/// file is `query.read_only_violation`; anything else is a plain failure.
fn duck_error<G>(
    active: &ActiveOperation<'_, G>,
    error: &duckdb::Error,
    area: &str,
) -> BackendError {
    let message = duck_failure(error);
    // An interrupt that lands between two statements of the operation (the READ ONLY `BEGIN` and
    // the statement) aborts the transaction instead of the statement: the cancel is what failed it.
    if message.starts_with("INTERRUPT Error")
        || (active.cancel_requested() && message.contains("transaction is aborted"))
    {
        return if active.cancel_requested() {
            BackendError::with_code(format!("{area}.cancelled"), message)
        } else {
            BackendError::with_code(format!("{area}.interrupted"), message).retryable()
        };
    }
    if area == "query" && message.contains("read-only mode") {
        return BackendError::with_code(codes::QUERY_READ_ONLY_VIOLATION, message);
    }
    BackendError::new(message)
}

// -------------------------------------------------------------------------------------- query

enum QueryResult {
    /// `truncated`: more rows exist beyond this page (exact, by lookahead).
    Rows { batch: RecordBatch, truncated: bool },
    /// Statements without a result set (DDL, DML).
    Command { affected_rows: u64 },
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
    connection: &DuckConnection,
    operation: &mut OperationGuard<'_>,
    sql: &str,
    classification: &Classification,
    page: Page,
    read_only: bool,
) -> Result<QueryResult, BackendError> {
    let guard = connection.lock();
    // `active` owns the connection guard: the operation stops being "active" before the
    // connection is released.
    let active = activate(operation, guard, "query")?;
    let running = connection.cancel.run(active.operation_id());
    connection.settle_transaction(&active);
    // A transaction cannot be nested: inside the user's own, the lexer's refusal is the scope.
    let scope = read_only && !connection.in_transaction.load(Ordering::Acquire);
    let mut began = false;
    let outcome = if scope {
        active
            .execute_batch("BEGIN TRANSACTION READ ONLY")
            .map_err(|error| duck_error(&active, &error, "query"))
            .map(|()| began = true)
    } else {
        Ok(())
    }
    .and_then(|()| query_script(&active, connection, sql, classification, page, !scope));
    // The rollback below must run even when the statement was cancelled.
    drop(running);
    if scope {
        // A `BEGIN` that an interrupt hit has begun the transaction all the same (and aborted it):
        // it is rolled back whether or not it said it began, and "no transaction" is no failure then.
        let rolled_back = active.execute_batch("ROLLBACK");
        if began && rolled_back.is_err() {
            connection.broken.store(true, Ordering::Release);
        }
    }
    outcome
}

fn query_script<G>(
    active: &ActiveOperation<'_, G>,
    connection: &DuckConnection,
    sql: &str,
    classification: &Classification,
    page: Page,
    track_transaction: bool,
) -> Result<QueryResult, BackendError>
where
    G: std::ops::Deref<Target = Connection>,
{
    // Every statement runs exactly once, in order. The page is the rows of the LAST statement
    // that produces a result set; a script without one is a zero-column page. A pageable read is
    // a single statement: it skips `offset` rows and reads one lookahead row; anything else ran
    // once at offset 0 and is never "truncated".
    let pageable = classification.pageable && !page.run_to_end;
    let mut result: Option<(RecordBatch, bool)> = None;
    let mut affected_rows = 0_u64;
    for statement in &classification.statements {
        // A cancel between two statements (or before the first) stops no running statement:
        // look before each one starts.
        if connection.cancel.stop_requested() {
            return Err(BackendError::with_code(
                "query.cancelled",
                CANCELLED_BEFORE_START,
            ));
        }
        if track_transaction {
            connection.settle_transaction(active);
        }
        // DuckDB answers a `BEGIN` inside a transaction with an error that aborts that transaction
        // (PostgreSQL only warns): it is not run, like the warning it would be elsewhere. A bare
        // `START [WORK]` begins one too, though the kit counts only `START TRANSACTION`.
        if track_transaction
            && (statement.opens_transaction || statement.keyword == Some(Keyword::Start))
            && connection.in_transaction.load(Ordering::Acquire)
        {
            continue;
        }
        let executed = active
            .prepare(&sql[statement.range.clone()])
            .map_err(|error| duck_error(active, &error, "query"))
            .and_then(|mut prepared| {
                run_statement(
                    active,
                    &mut prepared,
                    page,
                    pageable,
                    statement.read_only,
                    // A statement that is not a plain read runs to its end whatever the page keeps.
                    statement.read_only && !page.run_to_end,
                )
            });
        // What the text says is not what happened: `COMMIT x` is a parse error that ends nothing,
        // `START WORK` and an `EXECUTE` of a prepared `COMMIT` are not in the kit's flags. DuckDB is
        // asked; its answer is the flag.
        if track_transaction && changes_transaction(sql, statement) {
            let known = if let Some(open) = transaction_open(active) {
                connection.in_transaction.store(open, Ordering::Release);
                true
            } else if statement.ends_transaction {
                // Not answered (an interrupt): a `COMMIT` that fails has rolled the transaction
                // back and a `ROLLBACK` that fails found none.
                connection.in_transaction.store(false, Ordering::Release);
                true
            } else if (statement.opens_transaction || statement.keyword == Some(Keyword::Start))
                && executed.is_ok()
            {
                connection.in_transaction.store(true, Ordering::Release);
                true
            } else {
                // The statement (an `EXECUTE`, a `SAVEPOINT`, a `BEGIN` that failed) may have
                // changed the transaction and nobody can say: keeping the old answer could skip
                // a `BEGIN` or the read-only scope wrongly. `false` is the safe side, and DuckDB
                // is asked again before the next statement.
                connection.in_transaction.store(false, Ordering::Release);
                false
            };
            connection.transaction_unknown.store(!known, Ordering::Release);
        }
        match executed? {
            Executed::Command(rows) => affected_rows += rows,
            Executed::Rows(rows, truncated) => result = Some((rows, truncated)),
        }
    }
    match result {
        Some((batch, truncated)) => Ok(QueryResult::Rows {
            batch: normalize(batch)?,
            truncated,
        }),
        None => Ok(QueryResult::Command { affected_rows }),
    }
}

/// The statement may have begun or ended the transaction: it says so (`BEGIN`, `COMMIT`, `START`,
/// `SAVEPOINT`, ...), or it is an `EXECUTE`, which runs a statement prepared earlier, and that
/// one can be a `COMMIT`.
fn changes_transaction(sql: &str, statement: &Statement) -> bool {
    statement.controls_transaction
        || statement.keyword == Some(Keyword::Start)
        || statement.keyword_range.as_ref().is_some_and(|range| {
            sql[range.clone()].eq_ignore_ascii_case("EXECUTE")
        })
}

/// Whether the connection is inside a transaction, asked of DuckDB (its C API has no call for
/// it): `system.main.txid_current()` (qualified: a macro of the database file may be called
/// `txid_current` and would be found first by a bare call) is the transaction's id, the same in every statement of an open
/// transaction and new in each autocommit statement; an aborted transaction refuses the question.
/// `None`: the answer failed for another reason.
fn transaction_open(connection: &Connection) -> Option<bool> {
    let id = || connection.query_row("SELECT system.main.txid_current()", [], |row| row.get::<_, u64>(0));
    match (id(), id()) {
        (Ok(first), Ok(second)) => Some(first == second),
        (Err(error), _) | (_, Err(error)) => duck_failure(&error)
            .contains("transaction is aborted")
            .then_some(true),
    }
}

/// Whether DuckDB says file access is off and the configuration locked on this instance. The names
/// are qualified (`system.main`): a database file can define a macro called `current_setting`
/// that a bare call would find first, and answer whatever it likes. Any failure answers `false`.
fn engine_locks_file_access(connection: &Connection) -> bool {
    connection
        .query_row(
            "SELECT system.main.current_setting('enable_external_access'), \
                    system.main.current_setting('lock_configuration')",
            [],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
        )
        .is_ok_and(|(external_access, locked)| !external_access && locked)
}

enum Executed {
    /// DDL, DML or a session statement; the rows it changed.
    Command(u64),
    Rows(RecordBatch, bool),
}

/// DuckDB answers a statement without a result set with one `Count` column (DML, and `CREATE
/// TABLE`) or one `Success` column (the rest). A plain read never counts as one: `SELECT 1 AS
/// "Count"` is data.
fn command_shape(schema: &Schema) -> bool {
    match schema.fields().as_ref() {
        [field] => {
            (field.name() == "Count" && field.data_type() == &DataType::Int64)
                || (field.name() == "Success" && field.data_type() == &DataType::Boolean)
        }
        _ => false,
    }
}

fn run_statement<G>(
    active: &ActiveOperation<'_, G>,
    prepared: &mut duckdb::Statement<'_>,
    page: Page,
    pageable: bool,
    pure_read: bool,
    stop_early: bool,
) -> Result<Executed, BackendError> {
    read_page(prepared, page, pageable, pure_read, stop_early).map_err(|error| match error {
        PageError::Duck(error) => duck_error(active, &error, "query"),
        PageError::Arrow(error) => BackendError::new(error.to_string()),
    })
}

enum PageError {
    Duck(duckdb::Error),
    Arrow(arrow_schema::ArrowError),
}

impl From<duckdb::Error> for PageError {
    fn from(error: duckdb::Error) -> Self {
        Self::Duck(error)
    }
}

/// Runs the statement and keeps the rows of the page: skips `page.offset` rows, keeps up to
/// `page.limit` (one more when `pageable`, the lookahead that makes `truncated` exact). Chunks
/// are sliced, not copied; a read that `stop_early` is dropped once it has enough, any other
/// statement is stepped to its end. `pure_read`: the statement is a plain read, whose answer is
/// data whatever its shape.
fn read_page(
    prepared: &mut duckdb::Statement<'_>,
    page: Page,
    pageable: bool,
    pure_read: bool,
    stop_early: bool,
) -> Result<Executed, PageError> {
    let stream = prepared.stream_arrow([])?;
    let schema: SchemaRef = stream.get_schema();
    let wanted = if pageable { page.limit + 1 } else { page.limit } as usize;
    let command = !pure_read && command_shape(&schema);
    let mut to_skip = page.offset as usize;
    let mut kept: Vec<RecordBatch> = Vec::new();
    let mut have = 0_usize;
    let mut changed = 0_u64;
    for batch in stream {
        let batch = batch?;
        if command {
            if let Some(counts) = batch.column(0).as_primitive_opt::<Int64Type>() {
                changed += counts.values().iter().map(|&n| n.max(0) as u64).sum::<u64>();
            }
            continue;
        }
        if have >= wanted {
            if stop_early {
                break;
            }
            continue;
        }
        let mut rows = batch.num_rows();
        if to_skip >= rows {
            to_skip -= rows;
            continue;
        }
        let mut slice = batch;
        if to_skip > 0 {
            slice = slice.slice(to_skip, rows - to_skip);
            rows -= to_skip;
            to_skip = 0;
        }
        if have + rows > wanted {
            slice = slice.slice(0, wanted - have);
            rows = wanted - have;
        }
        have += rows;
        kept.push(slice);
    }
    if command {
        return Ok(Executed::Command(changed));
    }
    let truncated = pageable && have > page.limit as usize;
    if truncated {
        // The lookahead row only says there is more: it is not part of the page.
        let last = kept.len() - 1;
        kept[last] = kept[last].slice(0, kept[last].num_rows() - 1);
    }
    let batch = match kept.len() {
        0 => RecordBatch::new_empty(schema),
        _ => arrow_select::concat::concat_batches(&schema, &kept).map_err(PageError::Arrow)?,
    };
    Ok(Executed::Rows(batch, truncated))
}

/// What the Arrow export leaves for the host to read: a `TIMESTAMPTZ` is a timestamp in `UTC`,
/// which the formatters without a time zone database cannot read (the same instants labelled
/// `+00:00` are), and a `UNION` has no layout the grid knows, so it is its text.
fn normalize(batch: RecordBatch) -> Result<RecordBatch, BackendError> {
    let needs = |data_type: &DataType| match data_type {
        DataType::Timestamp(_, Some(zone)) => zone.as_ref() == "UTC",
        DataType::Union(_, _) => true,
        _ => false,
    };
    if !batch.schema().fields().iter().any(|field| needs(field.data_type())) {
        return Ok(batch);
    }
    let fail = |error: arrow_schema::ArrowError| BackendError::new(error.to_string());
    let mut fields = Vec::with_capacity(batch.num_columns());
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        let (data_type, column) = match field.data_type() {
            DataType::Timestamp(unit, Some(zone)) if zone.as_ref() == "UTC" => {
                let data_type = DataType::Timestamp(*unit, Some("+00:00".into()));
                let column = arrow_cast::cast(column, &data_type).map_err(fail)?;
                (data_type, column)
            }
            DataType::Union(_, _) => (DataType::Utf8, union_text(column).map_err(fail)?),
            other => (other.clone(), Arc::clone(column)),
        };
        fields.push(Field::new(field.name(), data_type, field.is_nullable()));
        columns.push(column);
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).map_err(fail)
}

fn union_text(column: &ArrayRef) -> Result<ArrayRef, arrow_schema::ArrowError> {
    let formatter = ArrayFormatter::try_new(column.as_ref(), &FormatOptions::new())?;
    let nulls = column.logical_nulls();
    let mut builder = StringBuilder::with_capacity(column.len(), column.len() * 8);
    let mut text = String::new();
    for row in 0..column.len() {
        if nulls.as_ref().is_some_and(|nulls| nulls.is_null(row)) {
            builder.append_null();
        } else {
            text.clear();
            write!(text, "{}", formatter.value(row)).expect("writing to a String cannot fail");
            builder.append_value(&text);
        }
    }
    Ok(Arc::new(builder.finish()))
}

/// The column's type as DuckDB names it (`BIGINT`, `DECIMAL(18,3)`, `INTEGER[]`).
fn data_type_label(data_type: &DataType) -> String {
    match data_type {
        DataType::Null => "NULL".into(),
        DataType::Boolean => "BOOLEAN".into(),
        DataType::Int8 => "TINYINT".into(),
        DataType::Int16 => "SMALLINT".into(),
        DataType::Int32 => "INTEGER".into(),
        DataType::Int64 => "BIGINT".into(),
        DataType::UInt8 => "UTINYINT".into(),
        DataType::UInt16 => "USMALLINT".into(),
        DataType::UInt32 => "UINTEGER".into(),
        DataType::UInt64 => "UBIGINT".into(),
        DataType::Float32 => "FLOAT".into(),
        DataType::Float64 => "DOUBLE".into(),
        DataType::Decimal128(precision, scale) => format!("DECIMAL({precision},{scale})"),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "VARCHAR".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "BLOB".into(),
        DataType::Date32 | DataType::Date64 => "DATE".into(),
        DataType::Time64(_) | DataType::Time32(_) => "TIME".into(),
        DataType::Timestamp(_, Some(_)) => "TIMESTAMP WITH TIME ZONE".into(),
        DataType::Timestamp(_, None) => "TIMESTAMP".into(),
        DataType::Interval(_) | DataType::Duration(_) => "INTERVAL".into(),
        DataType::Dictionary(_, _) => "ENUM".into(),
        DataType::List(field) | DataType::LargeList(field) => {
            format!("{}[]", data_type_label(field.data_type()))
        }
        DataType::FixedSizeList(field, size) => {
            format!("{}[{size}]", data_type_label(field.data_type()))
        }
        DataType::Struct(_) => "STRUCT".into(),
        DataType::Map(_, _) => "MAP".into(),
        _ => "VARCHAR".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        data_type_label, normalize, parse_uri, percent_decode, read_page, DuckBackend, Executed,
        Page,
    };
    use arrow_array::cast::AsArray;
    use arrow_array::types::Int32Type;
    use arrow_array::{Array, RecordBatch};
    use arrow_schema::{DataType, TimeUnit};
    use duckdb::Connection;
    use engine_driver_kit::{default_cell_text, DriverBackend, ExportedSymbols};

    fn page(conn: &Connection, sql: &str, offset: u64, limit: u64, pageable: bool) -> Executed {
        let mut statement = conn.prepare(sql).unwrap();
        read_page(
            &mut statement,
            Page {
                offset,
                limit,
                run_to_end: false,
            },
            pageable,
            pageable,
            pageable,
        )
        .ok()
        .expect("the page reads")
    }

    fn rows(executed: Executed) -> (RecordBatch, bool) {
        match executed {
            Executed::Rows(batch, truncated) => (batch, truncated),
            Executed::Command(_) => panic!("a command, not rows"),
        }
    }

    fn numbers(batch: &RecordBatch) -> Vec<i32> {
        batch.column(0).as_primitive::<Int32Type>().values().to_vec()
    }

    #[test]
    fn a_page_is_a_slice_across_chunk_boundaries_with_an_exact_lookahead() {
        let conn = Connection::open_in_memory().unwrap();
        let sql = "SELECT i::INTEGER AS n FROM range(1, 5001) t(i) ORDER BY n";
        // DuckDB hands out 2048-row chunks: the page starts in one and ends in the next.
        let (batch, truncated) = rows(page(&conn, sql, 2040, 20, true));
        assert_eq!(numbers(&batch), (2041..=2060).collect::<Vec<_>>());
        assert!(truncated);
        // The last rows: no lookahead row, not truncated.
        let (batch, truncated) = rows(page(&conn, sql, 4990, 20, true));
        assert_eq!(numbers(&batch), (4991..=5000).collect::<Vec<_>>());
        assert!(!truncated);
        // Exactly the remaining rows is not truncated either; past the end is empty.
        assert!(!rows(page(&conn, sql, 4980, 20, true)).1);
        let (batch, truncated) = rows(page(&conn, sql, 5000, 20, true));
        assert_eq!((batch.num_rows(), truncated), (0, false));
        assert_eq!(batch.num_columns(), 1);
        // A statement that is not pageable keeps `limit` rows and is never truncated.
        let (batch, truncated) = rows(page(&conn, sql, 0, 7, false));
        assert_eq!((batch.num_rows(), truncated), (7, false));
    }

    #[test]
    fn commands_are_told_from_reads_by_their_result_shape() {
        let conn = Connection::open_in_memory().unwrap();
        let run = |sql: &str, pure_read: bool| {
            let mut statement = conn.prepare(sql).unwrap();
            read_page(
                &mut statement,
                Page {
                    offset: 0,
                    limit: 10,
                    run_to_end: false,
                },
                false,
                pure_read,
                pure_read,
            )
            .ok()
            .expect("runs")
        };
        assert!(matches!(run("CREATE TABLE t (a INT)", false), Executed::Command(0)));
        assert!(matches!(run("INSERT INTO t VALUES (1), (2), (3)", false), Executed::Command(3)));
        assert!(matches!(run("UPDATE t SET a = a + 1 WHERE a > 1", false), Executed::Command(2)));
        assert!(matches!(run("SET threads = 2", false), Executed::Command(0)));
        assert!(matches!(
            run("INSERT INTO t VALUES (9) RETURNING a", false),
            Executed::Rows(..)
        ));
        // A plain read is data even when it looks like a command's answer.
        let (batch, _) = rows(run("SELECT 5::BIGINT AS \"Count\"", true));
        assert_eq!(batch.num_rows(), 1);
    }

    #[test]
    fn duckdb_types_arrive_as_arrow_types_the_host_reads() {
        let conn = Connection::open_in_memory().unwrap();
        let sql = "SELECT 170141183460469231731687303715884105727::HUGEINT AS h, \
            12.345::DECIMAL(10,3) AS d, INTERVAL '1 year 2 days' AS iv, [1, 2] AS l, \
            {'a': 1, 'b': 'x'} AS s, MAP {'k': 1} AS m, \
            '8c1b8a9e-0000-4000-8000-000000000001'::UUID AS u, \
            TIMESTAMPTZ '2024-05-06 07:08:09+00' AS tz, 'b'::ENUM('a', 'b') AS e, \
            '\\xDE\\xAD'::BLOB AS blob, union_value(num := 7) AS un";
        let (batch, _) = rows(page(&conn, sql, 0, 10, true));
        let batch = normalize(batch).expect("normalizes");
        let types: Vec<_> = batch.schema().fields().iter().map(|f| f.data_type().clone()).collect();
        assert_eq!(types[0], DataType::Decimal128(38, 0));
        assert_eq!(types[1], DataType::Decimal128(10, 3));
        assert!(matches!(types[2], DataType::Interval(_)));
        assert!(matches!(types[3], DataType::List(_)));
        assert!(matches!(types[4], DataType::Struct(_)));
        assert!(matches!(types[5], DataType::Map(..)));
        assert_eq!(types[6], DataType::Utf8);
        // Relabelled, not converted: the host's formatters read `+00:00`, not `UTC`.
        assert_eq!(types[7], DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())));
        assert!(matches!(types[8], DataType::Dictionary(..)));
        assert_eq!(types[9], DataType::Binary);
        assert_eq!(types[10], DataType::Utf8);
        let text = |column: usize| default_cell_text(batch.column(column), 0);
        assert_eq!(text(0), "170141183460469231731687303715884105727");
        assert_eq!(text(1), "12.345");
        assert_eq!(text(6), "8c1b8a9e-0000-4000-8000-000000000001");
        assert_eq!(text(7), "2024-05-06T07:08:09Z");
        assert_eq!(text(8), "b");
        assert_eq!(text(10), "{num=7}");
        assert!(!batch.column(10).is_null(0));
        assert!(text(3).contains('1') && text(3).contains('2'), "{}", text(3));
        assert!(text(4).contains('x'), "{}", text(4));
    }

    #[test]
    fn a_page_of_a_wide_result_reads_the_chunks_it_needs_only() {
        let conn = Connection::open_in_memory().unwrap();
        // 10^12 rows: only a streaming read can return the first 200 of it.
        let started = std::time::Instant::now();
        let (batch, truncated) = rows(page(&conn, "SELECT i FROM range(1000000000000) t(i)", 0, 200, true));
        assert_eq!((batch.num_rows(), truncated), (200, true));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn exported_symbols_match_capabilities() {
        ExportedSymbols::CANCEL_OPERATION
            .union(ExportedSymbols::MUTATE)
            .union(ExportedSymbols::DESCRIBE_OBJECT)
            .validate(&DuckBackend::capabilities())
            .expect("DuckDB exports exactly the optional symbols it advertises");
    }

    #[test]
    fn uris_name_a_file_memory_and_a_mode() {
        let file = parse_uri("duckdb:///tmp/a%20b.duckdb?mode=ro").unwrap();
        assert_eq!(file.path.unwrap().to_str(), Some("/tmp/a b.duckdb"));
        assert!(file.access.read_only && !file.access.no_external_access);
        let plain = parse_uri("DUCKDB:///tmp/x.ddb").unwrap();
        assert!(!plain.access.read_only && !plain.access.no_external_access);
        let locked = parse_uri("duckdb:///tmp/x.ddb?mode=ro&external_access=OFF").unwrap();
        assert!(locked.access.read_only && locked.access.no_external_access);
        assert!(parse_uri("duckdb://:memory:?external_access=off").unwrap().access.no_external_access);
        // Only `off` locks; anything else is the default.
        assert!(!parse_uri("duckdb:///tmp/x.ddb?external_access=on").unwrap().access.no_external_access);
        let memory = parse_uri("duckdb://:memory:").unwrap();
        assert!(memory.path.is_none());
        for bad in ["duckdb://", "duckdb:///", "sqlite:///x.db", "x", ""] {
            assert!(parse_uri(bad).is_err(), "{bad:?}");
        }
        assert!(percent_decode("%zz").is_err());
        assert!(percent_decode("%4").is_err());
        assert_eq!(percent_decode("%C3%A9").unwrap(), "é");
    }

    #[test]
    fn type_labels_follow_duckdb_names() {
        assert_eq!(data_type_label(&DataType::Decimal128(18, 3)), "DECIMAL(18,3)");
        assert_eq!(data_type_label(&DataType::UInt8), "UTINYINT");
    }
}
