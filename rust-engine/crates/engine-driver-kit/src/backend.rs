//! The trait a driver implements, and the plain Rust types it exchanges with
//! the kit. Nothing here exposes a raw pointer: all FFI lives in the kit.

use crate::cell_text::CellFormatter;
use crate::operation::OperationGuard;
use arrow_array::{RecordBatch, RecordBatchOptions};
use engine_driver_api::{
    DriverCapabilities, DriverError, ExportRequest, ExportResult, MutationRequest, NavigatorPage,
    NavigatorRequest, OperationId, PageColumn, PageFlags, QueryRequest, RowField, RowFields,
    SqlDialectInfo,
};
use serde::Deserialize;
use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

/// Failure reported by a [`DriverBackend`] method.
///
/// `code` is optional: when absent the kit uses the entry point's default code
/// (`connect.failed`, `query.failed`, `navigator.failed`, `mutation.failed`,
/// `export.failed`, `describe.failed`, `inspect.failed`, `operation.failed`).
/// Set it to keep a more specific wire code such as `query.unsupported`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub code: Option<Cow<'static, str>>,
    pub message: String,
    pub retryable: bool,
}

impl BackendError {
    /// An error with the entry point's default code.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
            retryable: false,
        }
    }

    /// An error with an explicit wire code.
    pub fn with_code(code: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.into()),
            message: message.into(),
            retryable: false,
        }
    }

    /// Marks the error as safe for the host to retry.
    #[must_use]
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub(crate) fn into_failure(self, default_code: &'static str) -> DriverError {
        DriverError {
            code: self
                .code
                .map_or_else(|| default_code.to_owned(), Cow::into_owned),
            message: self.message,
            retryable: self.retryable,
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}

impl From<crate::sql::ReadOnlyRefusal> for BackendError {
    fn from(refusal: crate::sql::ReadOnlyRefusal) -> Self {
        Self::with_code(
            engine_driver_api::sql::codes::QUERY_READ_ONLY_VIOLATION,
            refusal.to_string(),
        )
    }
}

impl From<String> for BackendError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for BackendError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Parsed `dbengine_connect` request. `uri` is required; every other field the
/// host sends is kept in `extra` (cold path, one allocation per connect).
///
/// Deliberately not `Debug`: the URI may carry credentials.
#[derive(Deserialize)]
pub struct ConnectRequest {
    pub uri: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// What a backend returns from [`DriverBackend::open_query`]. The kit allocates
/// the page id, registers the batch, and answers the host with a `QueryPage`.
pub struct QueryOutput<C = ()> {
    /// The full result. Leases slice it zero-copy.
    pub batch: RecordBatch,
    /// One descriptor per batch column (checked by the kit).
    pub columns: Vec<PageColumn>,
    pub total_count: Option<u64>,
    pub total_is_exact: bool,
    pub flags: PageFlags,
    /// Backend data kept with the page (e.g. the source table), handed back to
    /// [`DriverBackend::inspect_row`] and [`DriverBackend::export`].
    pub context: C,
}

impl<C> QueryOutput<C> {
    /// Result of a statement that returns no rows (DDL, DML): an empty
    /// zero-column page whose `total_count` is `affected_rows`. Use this when the
    /// backend's statement has no result columns instead of building a batch.
    pub fn command_complete(affected_rows: u64, context: C) -> Self {
        Self {
            batch: empty_batch(),
            columns: Vec::new(),
            total_count: Some(affected_rows),
            total_is_exact: true,
            flags: PageFlags {
                truncated: false,
                cancelled: false,
                read_only: true,
            },
            context,
        }
    }
}

/// A batch with no columns and no rows (a plain `RecordBatch::try_new` rejects
/// zero columns unless the row count is given explicitly).
pub fn empty_batch() -> RecordBatch {
    batch_with_row_count(Arc::new(arrow_schema::Schema::empty()), Vec::new(), 0)
        .expect("an empty batch is always valid")
}

/// `RecordBatch::try_new` that also accepts zero columns, using `row_count`.
pub fn batch_with_row_count(
    schema: arrow_schema::SchemaRef,
    columns: Vec<arrow_array::ArrayRef>,
    row_count: usize,
) -> Result<RecordBatch, BackendError> {
    RecordBatch::try_new_with_options(
        schema,
        columns,
        &RecordBatchOptions::new().with_row_count(Some(row_count)),
    )
    .map_err(|error| BackendError::new(error.to_string()))
}

/// A registered result page, as seen by backend hooks.
pub struct Page<C = ()> {
    pub page_id: u64,
    pub connection_id: u64,
    pub batch: RecordBatch,
    pub columns: Vec<PageColumn>,
    pub context: C,
}

/// Result of [`DriverBackend::mutate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationOutcome {
    pub affected_rows: u64,
    /// Drop every open page of the connection and report their ids to the host
    /// (their rows may be stale after a write). The safe default: a kit page
    /// is an arbitrary result set that does not record the tables it read, so
    /// the kit cannot narrow this to the mutated object. A backend whose
    /// mutation cannot affect any page opts out with [`Self::keep_pages`].
    pub invalidate_pages: bool,
}

impl MutationOutcome {
    /// `affected_rows` written; the connection's pages are invalidated.
    pub fn new(affected_rows: u64) -> Self {
        Self {
            affected_rows,
            invalidate_pages: true,
        }
    }

    /// Keeps the connection's open pages (the mutation cannot affect them).
    #[must_use]
    pub fn keep_pages(mut self) -> Self {
        self.invalidate_pages = false;
        self
    }
}

/// A database driver, expressed in plain Rust types.
///
/// Implement it on a zero-sized marker type and invoke
/// [`export_driver!`](crate::export_driver) once per cdylib to emit the
/// `dbengine_*` C symbols. All methods are associated functions: per-driver
/// global state belongs in the driver's own `static`s, per-connection state in
/// [`Self::Connection`].
///
/// Threading: the host may call any method concurrently from several threads,
/// and a connection may be used by several operations at once, so
/// `Connection` must be `Send + Sync` and serialize its own I/O (SQLite wraps
/// its handle in a `Mutex`). The kit never holds a registry or operation-state
/// lock while calling a backend method.
///
/// Panics are caught at the FFI boundary and become error envelopes, but they
/// still poison any `Mutex` the backend held; backends should not panic.
///
/// Lifecycle: `init` may be called again after `shutdown` in the same loaded
/// image (the host's `dlclose` usually does not unload the library on macOS), so
/// `shutdown` must not permanently tear down process-wide statics (runtimes,
/// pools, `OnceLock`s that cannot be rebuilt); release per-session resources only.
pub trait DriverBackend: Sized + 'static {
    /// One open connection. Shared (`Arc`) by every in-flight call.
    type Connection: Send + Sync + 'static;
    /// Per-page backend data; use `()` when unused.
    type PageContext: Send + Sync + 'static;
    /// Cross-thread cancel handle for one connection (see [`canceller`](Self::canceller)).
    type Canceller: Send + Sync + 'static;

    /// Advertised capabilities. The macro's optional symbol list must agree with
    /// them (checked at `dbengine_init`, see [`ExportedSymbols::validate`](crate::ExportedSymbols::validate)).
    fn capabilities() -> DriverCapabilities;

    /// Called by `dbengine_init`.
    fn init() -> Result<(), BackendError> {
        Ok(())
    }

    /// Called by `dbengine_shutdown` after every connection was disconnected.
    fn shutdown() {}

    fn connect(request: &ConnectRequest) -> Result<Self::Connection, BackendError>;

    /// The SQL dialect of this connection when a session setting makes it differ
    /// from the static [`capabilities`](Self::capabilities) (MySQL/MariaDB
    /// `NO_BACKSLASH_ESCAPES`). Sent as `sql_dialect` in the `dbengine_connect`
    /// reply, read once at connect; `None` (the default) means "as advertised".
    fn connection_dialect(_connection: &Self::Connection) -> Option<SqlDialectInfo> {
        None
    }

    /// The server's version as a number, sent as `server_version` in the
    /// `dbengine_connect` reply so the host can lex SQL as this server does (MySQL
    /// numeric form: MySQL 8.4.6 = `80406`, MariaDB 11.8.9 = `110809`). `None` (the
    /// default): not reported.
    fn connection_server_version(_connection: &Self::Connection) -> Option<u32> {
        None
    }

    /// Called when the host disconnects (or at shutdown), before the kit drops
    /// its reference. In-flight calls may still hold the connection, so the
    /// real close should also happen in `Drop`.
    fn disconnect(_connection: &Self::Connection) {}

    fn list_nav_nodes(
        connection: &Self::Connection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError>;

    /// Runs a query and returns the whole result as one batch. A statement without
    /// result columns returns [`QueryOutput::command_complete`].
    ///
    /// The kit already registered `operation` (so a cancel arriving early is
    /// remembered). Take the connection lock, then immediately pass the guard to
    /// [`OperationGuard::activate`] and work through the returned
    /// [`ActiveOperation`](crate::ActiveOperation), which owns the guard and
    /// derefs to it: it ends the active phase *before* the lock is released, by
    /// construction. From activation until it drops, a cancel for this operation
    /// reaches [`cancel`](Self::cancel). If the server reports an interrupt
    /// (Postgres SQLSTATE 57014, MySQL 1317, SQLite `SQLITE_INTERRUPT`) while
    /// [`ActiveOperation::cancel_requested`](crate::ActiveOperation::cancel_requested)
    /// is `false`, nobody asked for it: return `BackendError::with_code(
    /// "query.interrupted", ..).retryable()`.
    fn open_query(
        connection: &Self::Connection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput<Self::PageContext>, BackendError>;

    /// Builds the handle `cancel` uses to stop a running statement of this
    /// connection from another thread (SQLite: `InterruptHandle`; Postgres:
    /// `CancelToken`; MySQL: the connection's thread id). Called once, right
    /// after `connect`; it may take the connection's lock. Non-cancellable
    /// backends return `()`.
    fn canceller(connection: &Self::Connection) -> Result<Self::Canceller, BackendError>;

    /// Stops the statement of the *active* operation `operation_id`. Only
    /// reached when the macro exports `cancel_operation` (and on disconnect).
    ///
    /// The kit decides "this operation is active" under its operation-state
    /// lock, releases the lock, and only then calls `cancel`, so a slow cancel
    /// (a Postgres cancel request, MySQL `KILL QUERY`) never blocks other
    /// operations. `cancel` **must not take the connection lock**: the running
    /// statement holds it, so `cancel` would wait until the statement ended on
    /// its own, defeating the cancel. Use a side channel instead (SQLite
    /// `InterruptHandle`, a Postgres `CancelToken`, a second MySQL connection).
    /// In a tiny window the operation may finish and the next one start before
    /// the cancel lands; a canceller therefore targets the connection's current
    /// statement.
    fn cancel(
        _canceller: &Self::Canceller,
        _operation_id: OperationId,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// Chooses how column `column` of `page` is rendered as text. Called once per
    /// rendered column per `render_cell_batch`/`inspect_row` call; the returned
    /// formatter then writes each cell into a reused buffer. The default is
    /// [`CellFormatter::new`]; override to use `page.context` or column metadata
    /// (`CellFormatter::custom`).
    fn cell_formatter(page: &Page<Self::PageContext>, column: usize) -> CellFormatter<'_> {
        CellFormatter::new(page.batch.column(column))
    }

    /// Row details for `dbengine_inspect_row`; `row` is in range. The default
    /// renders every column read-only through [`cell_formatter`](Self::cell_formatter).
    fn inspect_row(page: &Page<Self::PageContext>, row: usize) -> Result<RowFields, BackendError> {
        Ok(default_inspect_row::<Self>(page, row))
    }

    /// `dbengine_mutate` (capability `mutation.*` / `database.create`).
    fn mutate(
        _connection: &Self::Connection,
        _request: &MutationRequest,
        _operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        Err(BackendError::with_code(
            "mutation.unsupported",
            "driver does not support mutations",
        ))
    }

    /// `dbengine_export` (capability `export.*`). `page` is the open page named
    /// by `request.page_id`, already checked to belong to this connection.
    fn export(
        _connection: &Self::Connection,
        _request: &ExportRequest,
        _page: Option<&Page<Self::PageContext>>,
        _operation: &mut OperationGuard<'_>,
    ) -> Result<ExportResult, BackendError> {
        Err(BackendError::with_code(
            "export.unsupported",
            "driver does not support export",
        ))
    }

    /// `dbengine_describe_object` (capability `metadata.table_properties`).
    /// `request` and the returned value are caller-defined JSON; the backend
    /// deserializes the former and serializes its typed result into the latter.
    fn describe_object(
        _connection: &Self::Connection,
        _request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        Err(BackendError::with_code(
            "describe.unsupported",
            "driver does not support object description",
        ))
    }
}

/// The default [`DriverBackend::inspect_row`]: one read-only field per column.
pub fn default_inspect_row<B: DriverBackend>(page: &Page<B::PageContext>, row: usize) -> RowFields {
    let mut fields = Vec::with_capacity(page.columns.len());
    for (column, metadata) in page.columns.iter().enumerate() {
        let mut text = String::new();
        let formatter = B::cell_formatter(page, column);
        formatter.write(row, &mut text);
        // SQL NULL is `value: null`; the text `NULL` stays a string.
        let value = if formatter.is_null(row) {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(text.clone())
        };
        fields.push(RowField {
            name: metadata.name.clone(),
            native_type: metadata.data_type.clone(),
            display_text: text,
            value,
            editable: false,
        });
    }
    RowFields {
        page_id: page.page_id,
        row_index: row as u64,
        fields,
        mutation_identity: serde_json::json!({}),
    }
}

/// Builds one [`PageColumn`] per schema field (`id` is the column index,
/// `nullable` comes from the field). `type_label` names each Arrow type for
/// the host.
pub fn page_columns(
    batch: &RecordBatch,
    type_label: impl Fn(&arrow_schema::DataType) -> String,
) -> Vec<PageColumn> {
    batch
        .schema()
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| PageColumn {
            id: index.to_string(),
            name: field.name().clone(),
            data_type: type_label(field.data_type()),
            nullable: field.is_nullable(),
        })
        .collect()
}
