//! The load's session: a second session of the connection (never a tab's own, never the shared one), the
//! batches as `batch` mutations of inserts through the driver, and `BEGIN` / `COMMIT` / `ROLLBACK` around
//! them for a one-transaction load.
//!
//! The driver binds every value as a parameter, per dialect, and applies one `batch` in one transaction (inside
//! the load's own transaction it joins through a savepoint and never ends it), so no SQL text is built here.

use std::sync::Arc;

use engine_driver_api::sql::{codes, SqlRowOp, SqlTableIdentity};
use engine_driver_api::{CancelRequest, MutationRequest, QueryRequest};
use engine_driver_host::{DriverHandle, DriverSession, LoaderError};
use serde_json::{Map, Value};

use super::run::{BatchWriter, WriteFailure};
use crate::app::results::editing::PartialApply;
use crate::app::structure_edit::apply::{ApplyControl, SessionSpec};
use crate::connections::ConnectionKind;

/// The table the rows go to, as a mutation names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct TargetTable {
    pub(in crate::app) database: String,
    pub(in crate::app) schema: String,
    pub(in crate::app) object: String,
}

pub(in crate::app) struct DriverWriter {
    session: DriverSession,
    handle: Arc<DriverHandle>,
    control: Arc<ApplyControl>,
    table: TargetTable,
    kind: ConnectionKind,
    /// The bytes of cells one batch may hold, when the server's packet limit says (MySQL and MariaDB): half of it.
    packet_cap: Option<usize>,
    /// This session's own SSH tunnel, when the connection has one. Declared last: it goes after the session
    /// has disconnected through it.
    _tunnel: Option<engine_connect::connect::LiveTunnel>,
}

impl DriverWriter {
    /// Blocking: opens the session.
    pub(in crate::app) fn open(spec: SessionSpec, table: TargetTable, kind: ConnectionKind, control: Arc<ApplyControl>) -> Result<Self, String> {
        let handle = Arc::clone(&spec.handle);
        let (session, tunnel) = spec.open()?;
        Ok(Self { session, handle, control, table, kind, packet_cap: None, _tunnel: tunnel })
    }

    /// A transfer writes timestamps as text that must mean the same instant on every server: the session works in UTC (a MySQL
    /// `timestamp` and a PostgreSQL `timestamptz` read a plain time by the zone of the session). A server that refuses, or that
    /// does not take the setting (ClickHouse before 23.6 ignores it), fails: a transfer that shifts every timestamp is worse than
    /// none.
    pub(in crate::app) fn pin_utc(&mut self) -> Result<(), String> {
        let (sql, check) = match self.kind {
            ConnectionKind::Postgres => ("SET TIME ZONE 'UTC'", None),
            ConnectionKind::Mysql | ConnectionKind::Mariadb => ("SET time_zone = '+00:00'", None),
            ConnectionKind::Clickhouse => ("SET session_timezone = 'UTC'", Some("SELECT timezone()")),
            _ => return Ok(()),
        };
        self.statement(sql, false).map_err(|error| format!("The target session could not be set to UTC: {error}"))?;
        if let Some(check) = check {
            let database = Some(self.table.database.clone()).filter(|database| !database.is_empty());
            if first_cell(&mut self.session, database, check).as_deref() != Some("UTC") {
                return Err("The target server does not take a session time zone (ClickHouse needs 23.6 or later): its timestamps would be shifted.".to_owned());
            }
        }
        Ok(())
    }

    /// MySQL and MariaDB refuse a statement larger than `max_allowed_packet`: a batch holds half of it at most (a cell is a little
    /// longer in the statement than as text), and never more than the usual cap.
    pub(in crate::app) fn learn_packet_limit(&mut self) {
        if !matches!(self.kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) {
            return;
        }
        let database = Some(self.table.database.clone()).filter(|database| !database.is_empty());
        self.packet_cap = first_cell(&mut self.session, database, "SELECT @@max_allowed_packet").and_then(|text| text.trim().parse::<usize>().ok()).map(|limit| (limit / 2).clamp(64 * 1024, super::run::BATCH_BYTES_CAP));
    }

    /// The next rows go to `table` (a transfer reuses its session for the tables it writes one after the other).
    pub(in crate::app) fn aim(&mut self, table: TargetTable) {
        self.table = table;
    }

    /// `rows` is how many rows the failed request carried (a partly applied batch the driver's report cannot be read
    /// for is that many rows of unknown outcome).
    fn failure(&self, error: &LoaderError, rows: usize) -> WriteFailure {
        let text = crate::sessions::redact_secrets(&error.to_string());
        // Before the cancel shortcut: a batch cut off by a cancel may still have written rows, and the report says which.
        if let LoaderError::DriverRejected { code, message, .. } = error {
            if code == codes::MUTATION_PARTIALLY_APPLIED {
                let message = crate::sessions::redact_secrets(message);
                let (applied, unknown) = match PartialApply::parse(&message) {
                    Some(partial) => (partial.applied.len() as u64, partial.unknown.len() as u64),
                    None => (0, rows as u64),
                };
                return WriteFailure::Partial { applied, unknown, message };
            }
        }
        if self.control.is_cancelled() {
            return WriteFailure::Cancelled;
        }
        match error {
            LoaderError::DriverRejected { code, .. } if code.ends_with(".cancelled") => WriteFailure::Cancelled,
            LoaderError::DriverRejected { code, .. } if crate::sessions::is_connection_lost_code(code) => WriteFailure::Lost(text),
            // The server (or the driver, for a value it cannot bind) refused this data.
            // Nobody knows whether a COMMIT that was cut off was applied: that is not a refusal.
            LoaderError::DriverRejected { code, .. } if code == codes::MUTATION_COMMIT_UNKNOWN => WriteFailure::Lost(text),
            LoaderError::DriverRejected { code, .. } if code.starts_with("mutation.") || code == "query.failed" => WriteFailure::Rejected(message_of(&text)),
            _ => WriteFailure::Lost(text),
        }
    }

    /// Runs `call` as an operation the Cancel button can reach. `cancellable: false` for COMMIT and ROLLBACK.
    fn operation<T>(
        &mut self,
        cancellable: bool,
        rows: usize,
        call: impl FnOnce(&mut DriverSession, engine_driver_api::OperationId) -> Result<T, LoaderError>,
    ) -> Result<T, WriteFailure> {
        if cancellable && self.control.is_cancelled() {
            return Err(WriteFailure::Cancelled);
        }
        let operation = self.session.begin_operation().map_err(|error| self.failure(&error, rows))?;
        if cancellable {
            let (handle, connection_id, operation_id) = (Arc::clone(&self.handle), self.session.connection_id(), operation.id());
            let canceller: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _ = handle.cancel_operation(connection_id, &CancelRequest { operation_id });
            });
            self.control.register(canceller).map_err(|_| WriteFailure::Cancelled)?;
        }
        let result = call(&mut self.session, operation.id());
        self.control.clear_running();
        result.map_err(|error| self.failure(&error, rows))
    }

    fn statement(&mut self, sql: &'static str, cancellable: bool) -> Result<(), WriteFailure> {
        let database = Some(self.table.database.clone()).filter(|database| !database.is_empty());
        self.operation(cancellable, 0, |session, operation_id| {
            let request = QueryRequest {
                kind: "sql".into(),
                operation_id,
                database,
                object: None,
                offset: 0,
                limit: 1,
                payload: serde_json::json!({"sql": sql, "confirmed": true}),
            };
            let json = serde_json::to_string(&request).map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
            let page = session.open_query(&json)?;
            let _ = session.release_page(page.page_id);
            Ok(())
        })
    }
}

/// The server's words of a driver rejection (`driver rejected the request: <code>: <message>`).
pub(in crate::app) fn message_of(text: &str) -> String {
    text.strip_prefix("driver rejected the request: ")
        .and_then(|rest| rest.split_once(": "))
        .map_or_else(|| text.to_owned(), |(_, message)| message.to_owned())
}

/// The first cell of the first row of `sql` on `session`, as the grid shows it.
pub(in crate::app) fn first_cell(session: &mut DriverSession, database: Option<String>, sql: &str) -> Option<String> {
    use crate::grid_arrow::{ArrowCellReader, BinaryDisplay};
    let operation = session.begin_operation().ok()?;
    let request = QueryRequest { kind: "sql".into(), operation_id: operation.id(), database, object: None, offset: 0, limit: 1, payload: serde_json::json!({ "sql": sql, "confirmed": true }) };
    let page = session.open_query(&serde_json::to_string(&request).ok()?).ok()?;
    let text = session.acquire_arrow_page(page.page_id, 0, page.row_count).ok().and_then(|lease| {
        // SAFETY: the lease holds the Arrow C Data pair the driver exported for this page; import consumes it once.
        let imported = unsafe { lease.into_record_batch() }.ok()?;
        let batch = &imported.batch;
        if batch.num_rows() == 0 || batch.num_columns() == 0 {
            return None;
        }
        let reader = ArrowCellReader::new(batch.column(0).as_ref(), BinaryDisplay::Hex);
        let mut buffer = String::new();
        (!reader.is_null(0)).then(|| reader.text(0, &mut buffer).to_owned())
    });
    let _ = session.release_page(page.page_id);
    text
}

impl BatchWriter for DriverWriter {
    fn batch_bytes_cap(&self) -> usize {
        self.packet_cap.unwrap_or(super::run::BATCH_BYTES_CAP)
    }

    fn begin(&mut self) -> Result<(), WriteFailure> {
        // MySQL and MariaDB spell it START TRANSACTION; BEGIN would also work, but only that is a transaction
        // in every mode of the server.
        let sql = if matches!(self.kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) { "START TRANSACTION" } else { "BEGIN" };
        self.statement(sql, true)
    }

    fn insert(&mut self, rows: Vec<Map<String, Value>>) -> (Vec<Map<String, Value>>, Result<u64, WriteFailure>) {
        let count = rows.len();
        let identity = SqlTableIdentity { schema: Some(self.table.schema.clone()).filter(|schema| !schema.is_empty()) };
        let (database, object) = (Some(self.table.database.clone()).filter(|database| !database.is_empty()), self.table.object.clone());
        let Ok(identity) = serde_json::to_value(identity) else { return (Vec::new(), Err(WriteFailure::Lost("the table identity cannot be encoded".to_owned()))) };
        // The rows move into the request (a `SqlBatch` of inserts, as a value) and come out of it again afterwards.
        let changes: Vec<Value> = rows
            .into_iter()
            .map(|values| {
                let mut change = Map::with_capacity(2);
                change.insert("op".to_owned(), Value::String(SqlRowOp::Insert.kind().to_owned()));
                change.insert("values".to_owned(), Value::Object(values));
                Value::Object(change)
            })
            .collect();
        let payload = Value::Object(Map::from_iter([("changes".to_owned(), Value::Array(changes))]));
        let mut sent = None;
        let result = self.operation(true, count, |session, operation_id| {
            let request = MutationRequest { operation_id, kind: "batch".into(), database, object: Some(object), identity, payload };
            let result = session.mutate(&request);
            sent = Some(request.payload);
            result
        });
        (rows_of(sent), result.map(|result| result.affected_rows))
    }

    fn commit(&mut self) -> Result<(), WriteFailure> {
        self.statement("COMMIT", false)
    }

    fn rollback(&mut self) {
        let _ = self.statement("ROLLBACK", false);
    }
}

/// The `values` of each change of a batch payload, taken back out of it.
fn rows_of(payload: Option<Value>) -> Vec<Map<String, Value>> {
    let Some(Value::Object(mut payload)) = payload else { return Vec::new() };
    let Some(Value::Array(changes)) = payload.remove("changes") else { return Vec::new() };
    changes
        .into_iter()
        .filter_map(|change| match change {
            Value::Object(mut change) => match change.remove("values") {
                Some(Value::Object(values)) => Some(values),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

impl Drop for DriverWriter {
    fn drop(&mut self) {
        let _ = self.session.disconnect();
    }
}

/// Whether the driver of a connection can take the load at all.
pub(in crate::app) fn insert_supported(capabilities: &engine_driver_api::DriverCapabilities) -> Result<(), &'static str> {
    use engine_driver_api::Capability;
    if !capabilities.has(Capability::MutationBatch) {
        return Err("This driver cannot apply batched row changes.");
    }
    if !capabilities.supports_mutation_kind(SqlRowOp::Insert.kind()) {
        return Err("This driver cannot insert rows.");
    }
    Ok(())
}
