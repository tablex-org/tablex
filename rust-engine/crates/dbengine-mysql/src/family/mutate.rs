//! Mutations, using the shared SQL driver contract
//! (`engine_driver_api::sql`): row changes by key inside a transaction, and
//! `create_database`.
//!
//! `MutationRequest`:
//!
//! | kind              | database | object | identity                     | payload                                        |
//! |-------------------|----------|--------|------------------------------|------------------------------------------------|
//! | `insert`          | database | table  | `SqlTableIdentity` (`schema` ignored) | one `SqlRowChange` with `op: "insert"` |
//! | `update`          | database | table  | same                         | one `SqlRowChange`, `op: "update"`: `key` + changed `values` |
//! | `delete`          | database | table  | same                         | one `SqlRowChange`, `op: "delete"`: `key`      |
//! | `batch`           | database | table  | same                         | `SqlBatch`: ordered changes, one transaction   |
//! | `create_database` | name     | -      | ignored                      | absent or `{}` (no options)                    |
//!
//! `batch` is all-or-nothing on a table with transactions (InnoDB ...): the first
//! failing change rolls everything back. On a table without them (MyISAM, Aria,
//! MEMORY, CSV ...: the table's engine is read) nothing can be rolled back, so the
//! changes run one at a time, each guarded so it cannot touch more than one row
//! (a pre-count of the key, then `LIMIT 1`), stopping at the first failure; a
//! failure after at least one change was applied is `mutation.partially_applied`
//! and names the applied, failed and not attempted changes. The driver never says
//! "rolled back" when it was not.
//! Every change's `op` must equal the request kind (single kinds) or be an
//! advertised `mutation.<op>` (batch). Keys may be composite; a `null` key value
//! matches SQL NULL (`<=>`).
//!
//! Failure codes: `mutation.bad_request` (malformed, empty key, empty update,
//! `op` not the kind, options on `create_database`), `mutation.invalid_value` (a
//! binary column given anything but `0x` + hex, a BIT column anything but an
//! unsigned integer or `b'0101'`), `mutation.value_not_editable` (GEOMETRY),
//! `mutation.partially_applied` (see above), `mutation.no_rows_affected`
//! (a change affected no row), `mutation.identity_not_unique` (a key matched
//! several rows), `mutation.failed` (the server refused), `mutation.unsupported`,
//! `mutation.cancelled` / `mutation.interrupted`,
//! `mutation.create_database.rejected` (the server refused `CREATE DATABASE`),
//! and `mutation.not_connected` once a failed rollback closed the connection.
//!
//! Transactions: a mutation is its own transaction (`START TRANSACTION` ..
//! `COMMIT`). When the session is inside a transaction the *user* opened, it
//! joins it through a `SAVEPOINT` instead (released on success, rolled back to on
//! failure) and never commits or rolls back the user's work. If a rollback
//! itself fails the connection is closed: the session may hold half a change.
//!
//! Values are JSON scalars bound as statement parameters (never spliced into
//! SQL): `null` is SQL NULL, booleans are 1/0, numbers keep their integer or
//! floating form, strings are text (the server converts them to the column
//! type, so DECIMAL and temporal values travel as strings), arrays and objects
//! are sent as JSON text. A non-integer JSON number is sent as its decimal text,
//! not as a double, so a DECIMAL column receives the digits the caller wrote.
//!
//! The columns' types are read from `information_schema` before the change
//! (one query, with the table's engine), because three kinds of column cannot
//! take a value as the string it was displayed as: BINARY / VARBINARY / *BLOB
//! (`0x` + hex digits, bound as the real bytes), BIT (an unsigned integer or
//! `b'0101'`, bound as an integer) and GEOMETRY (never written through text). A
//! `null` is always SQL NULL. The same rules apply to key values.
//!
//! The connection enables `CLIENT_FOUND_ROWS`, so an `UPDATE` reports the rows
//! it *matched*: setting a column to its current value is a success, not
//! "no rows".

use super::conn::{
    block_on, cancelled, describe_error, is_connection_lost, map_error, other_error, Abandoned,
    ActiveMark,
    Area, ConnState, ER_QUERY_INTERRUPTED,
};
use super::sql::{count_sql, create_database_sql, delete_sql, insert_sql, update_sql};
use super::MyConnection;
use engine_driver_kit::api::sql::{codes, SqlBatch, SqlRowChange, SqlRowOp, SqlTableIdentity};
use engine_driver_kit::api::{DriverCapabilities, MutationRequest};
use engine_driver_kit::{BackendError, MutationOutcome, OperationGuard};
use mysql_async::prelude::Queryable;
use mysql_async::{Params, Value};
use serde_json::Value as Json;

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::with_code("mutation.bad_request", message)
}

/// How many rows a statement must touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Expect {
    /// At least one (an insert).
    AtLeastOne,
    /// Exactly one (a keyed update or delete).
    OneRow,
}

/// One bound parameter: the column it is written to or compared with, and the
/// value as the caller sent it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Param {
    pub column: String,
    pub value: Json,
}

/// One validated statement and its parameters (the values to write, then the key).
#[derive(Debug, PartialEq)]
pub(super) struct Statement {
    pub sql: String,
    pub params: Vec<Param>,
    /// Where the key parameters start in `params` (`params.len()` for an insert).
    pub key_start: usize,
    /// `SELECT COUNT(*) ...` of at most two rows the key matches; update/delete only.
    pub count_sql: Option<String>,
    pub expect: Expect,
    /// How many rows the statement writes: 1, or the rows of a joined multi-row insert.
    pub rows: usize,
    /// The index of the batch's change the statement starts at (set when the batch is run).
    pub first_change: usize,
}

/// The table a mutation writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub database: Option<String>,
    pub table: String,
}

/// A mutation after validation.
#[derive(Debug, PartialEq)]
pub(super) enum Plan {
    /// DML statements run in order inside one transaction (or one by one on a
    /// table without transactions).
    Dml {
        target: Target,
        statements: Vec<Statement>,
    },
    /// DDL (implicitly committed by the server; no transaction).
    CreateDatabase { sql: String },
}

pub(super) fn json_to_value(value: &Json) -> Value {
    match value {
        Json::Null => Value::NULL,
        Json::Bool(flag) => Value::Int(i64::from(*flag)),
        Json::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::Int(integer)
            } else if let Some(unsigned) = number.as_u64() {
                Value::UInt(unsigned)
            } else {
                // The digits the caller wrote, not a lossy double.
                Value::Bytes(number.to_string().into_bytes())
            }
        }
        Json::String(text) => Value::Bytes(text.clone().into_bytes()),
        Json::Array(_) | Json::Object(_) => Value::Bytes(value.to_string().into_bytes()),
    }
}

/// How a column takes a value (from its `DATA_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ColumnClass {
    /// Written as JSON scalars are (strings are converted by the server).
    Plain,
    /// BINARY, VARBINARY, TINYBLOB .. LONGBLOB: `0x` + hex digits.
    Binary,
    /// BIT(width): an unsigned integer, or `b'0101'`.
    Bit { width: u32 },
    /// GEOMETRY and its subtypes: not editable through text.
    Geometry,
}

impl ColumnClass {
    /// `data_type` is `information_schema` `DATA_TYPE`, `column_type` its `COLUMN_TYPE`.
    pub(super) fn of(data_type: &str, column_type: &str) -> Self {
        match data_type.to_ascii_lowercase().as_str() {
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
                Self::Binary
            }
            "bit" => Self::Bit {
                width: column_type
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_once(')'))
                    .and_then(|(width, _)| width.trim().parse().ok())
                    .unwrap_or(1),
            },
            "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
            | "multipolygon" | "geometrycollection" | "geomcollection" => Self::Geometry,
            _ => Self::Plain,
        }
    }
}

/// What the driver knows about the target table before it writes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TableSchema {
    columns: Vec<(String, ColumnClass)>,
    /// The table's storage engine; `None` when unknown (a view, a temporary table).
    engine: Option<String>,
}

impl TableSchema {
    /// Column names are case-insensitive in MySQL.
    fn class_of(&self, column: &str) -> ColumnClass {
        self.columns
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(column))
            .map_or(ColumnClass::Plain, |(_, class)| *class)
    }

    /// Whether a failed change can be rolled back: only for an engine known to
    /// have transactions. An unknown engine (a view, a temporary table, an engine
    /// not listed here) is treated as having none: the one-by-one path is correct
    /// for every engine and only slower, and it never claims a rollback.
    fn transactional(&self) -> bool {
        self.engine.as_deref().is_some_and(|engine| {
            ["innodb", "xtradb", "tokudb", "rocksdb", "ndbcluster", "ndb"]
                .iter()
                .any(|known| engine.eq_ignore_ascii_case(known))
        })
    }
}

fn invalid_value(column: &str, message: impl std::fmt::Display) -> BackendError {
    BackendError::with_code(
        codes::MUTATION_INVALID_VALUE,
        format!("column `{column}`: {message}"),
    )
}

/// `0x` followed by an even number of hex digits (either case) as bytes.
pub(super) fn parse_hex_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text.trim().strip_prefix("0x")?;
    if digits.len() % 2 != 0 {
        return None;
    }
    digits
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high << 4 | low).ok()
        })
        .collect()
}

/// An unsigned decimal integer, or a binary literal `b'0101'` / `B'0101'` (up to
/// 64 digits), as a BIT value.
fn parse_bit(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Some(digits) = text
        .strip_prefix("b'")
        .or_else(|| text.strip_prefix("B'"))
        .and_then(|rest| rest.strip_suffix('\''))
    {
        if digits.is_empty() || digits.len() > 64 {
            return None;
        }
        return digits.bytes().try_fold(0_u64, |value, digit| match digit {
            b'0' => Some(value << 1),
            b'1' => Some(value << 1 | 1),
            _ => None,
        });
    }
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

impl Param {
    /// The value to bind for `class`.
    pub(super) fn bind(&self, class: ColumnClass) -> Result<Value, BackendError> {
        if self.value.is_null() {
            return Ok(Value::NULL);
        }
        let column = self.column.as_str();
        match class {
            ColumnClass::Plain => Ok(json_to_value(&self.value)),
            ColumnClass::Geometry => Err(BackendError::with_code(
                codes::MUTATION_VALUE_NOT_EDITABLE,
                format!(
                    "column `{column}` is a GEOMETRY column: it cannot be written through text"
                ),
            )),
            ColumnClass::Binary => match &self.value {
                Json::String(text) => parse_hex_bytes(text).map(Value::Bytes).ok_or_else(|| {
                    invalid_value(
                        column,
                        "a binary column takes `0x` followed by an even number of hex digits",
                    )
                }),
                _ => Err(invalid_value(
                    column,
                    "a binary column takes `0x` followed by an even number of hex digits",
                )),
            },
            ColumnClass::Bit { width } => {
                let value = match &self.value {
                    Json::Number(number) => number.as_u64(),
                    Json::Bool(flag) => Some(u64::from(*flag)),
                    Json::String(text) => parse_bit(text),
                    _ => None,
                }
                .ok_or_else(|| {
                    invalid_value(column, "a BIT column takes an unsigned integer or b'0101'")
                })?;
                if width < 64 && value >> width != 0 {
                    return Err(invalid_value(
                        column,
                        format!("{value} does not fit in BIT({width})"),
                    ));
                }
                Ok(Value::UInt(value))
            }
        }
    }
}

impl Statement {
    /// The parameters as values, bound for the columns' classes.
    fn bind(&self, schema: &TableSchema) -> Result<Vec<Value>, BackendError> {
        self.params
            .iter()
            .map(|param| param.bind(schema.class_of(&param.column)))
            .collect()
    }
}

fn table(request: &MutationRequest) -> Result<&str, BackendError> {
    request
        .object
        .as_deref()
        .filter(|table| !table.is_empty())
        .ok_or_else(|| bad_request("missing table (object)"))
}

/// `identity` is a `SqlTableIdentity`; null/absent is the empty identity. MySQL
/// has no schema level, so its `schema` is ignored.
fn check_identity(request: &MutationRequest) -> Result<(), BackendError> {
    if request.identity.is_null() {
        return Ok(());
    }
    serde_json::from_value::<SqlTableIdentity>(request.identity.clone())
        .map(|_| ())
        .map_err(|error| bad_request(format!("identity is not a SqlTableIdentity: {error}")))
}

fn statement(
    database: Option<&str>,
    table: &str,
    change: &SqlRowChange,
) -> Result<Statement, BackendError> {
    let key_columns = || -> Result<Vec<&str>, BackendError> {
        if change.key.is_empty() {
            return Err(bad_request("key is empty"));
        }
        Ok(change.key.keys().map(String::as_str).collect())
    };
    let params = |entries: &serde_json::Map<String, Json>| -> Vec<Param> {
        entries
            .iter()
            .map(|(column, value)| Param {
                column: column.clone(),
                value: value.clone(),
            })
            .collect()
    };
    match change.op {
        SqlRowOp::Insert => {
            let columns: Vec<&str> = change.values.keys().map(String::as_str).collect();
            let params = params(&change.values);
            Ok(Statement {
                sql: insert_sql(database, table, &columns),
                key_start: params.len(),
                params,
                count_sql: None,
                expect: Expect::AtLeastOne,
                rows: 1,
                first_change: 0,
            })
        }
        SqlRowOp::Update => {
            if change.values.is_empty() {
                return Err(bad_request("update has no values"));
            }
            let key = key_columns()?;
            let set: Vec<&str> = change.values.keys().map(String::as_str).collect();
            let mut bound = params(&change.values);
            let key_start = bound.len();
            bound.extend(params(&change.key));
            Ok(Statement {
                sql: update_sql(database, table, &set, &key),
                params: bound,
                key_start,
                count_sql: Some(count_sql(database, table, &key)),
                expect: Expect::OneRow,
                rows: 1,
                first_change: 0,
            })
        }
        SqlRowOp::Delete => {
            let key = key_columns()?;
            Ok(Statement {
                sql: delete_sql(database, table, &key),
                params: params(&change.key),
                key_start: 0,
                count_sql: Some(count_sql(database, table, &key)),
                expect: Expect::OneRow,
                rows: 1,
                first_change: 0,
            })
        }
    }
}

fn target(database: Option<&str>, table: &str) -> Target {
    Target {
        database: database.filter(|name| !name.is_empty()).map(str::to_owned),
        table: table.to_owned(),
    }
}

pub(super) fn plan(
    request: &MutationRequest,
    capabilities: &DriverCapabilities,
) -> Result<Plan, BackendError> {
    let database = request.database.as_deref();
    match request.kind.as_str() {
        kind @ ("insert" | "update" | "delete") => {
            check_identity(request)?;
            let table = table(request)?;
            let change: SqlRowChange = serde_json::from_value(request.payload.clone())
                .map_err(|error| bad_request(format!("payload is not a SqlRowChange: {error}")))?;
            if change.op.kind() != kind {
                return Err(bad_request(format!(
                    "payload op `{}` does not match the mutation kind `{kind}`",
                    change.op.kind()
                )));
            }
            Ok(Plan::Dml {
                target: target(database, table),
                statements: vec![statement(database, table, &change)?],
            })
        }
        "batch" => {
            check_identity(request)?;
            let table = table(request)?;
            let batch: SqlBatch = serde_json::from_value(request.payload.clone())
                .map_err(|error| bad_request(format!("payload is not a SqlBatch: {error}")))?;
            if batch.changes.is_empty() {
                return Err(bad_request("batch has no changes"));
            }
            let mut statements = Vec::with_capacity(batch.changes.len());
            for (index, change) in batch.changes.iter().enumerate() {
                if !capabilities.supports_mutation_kind(change.op.kind()) {
                    return Err(BackendError::with_code(
                        codes::MUTATION_UNSUPPORTED,
                        format!("change {index}: `{}` is not supported", change.op.kind()),
                    ));
                }
                statements.push(
                    statement(database, table, change).map_err(|error| {
                        bad_request(format!("change {index}: {}", error.message))
                    })?,
                );
            }
            Ok(Plan::Dml {
                target: target(database, table),
                statements,
            })
        }
        "create_database" => {
            let name = database
                .filter(|name| !name.is_empty())
                .ok_or_else(|| bad_request("missing database name"))?;
            let no_options = match &request.payload {
                Json::Null => true,
                Json::Object(options) => options.is_empty(),
                _ => false,
            };
            if !no_options {
                return Err(bad_request("create_database takes no payload"));
            }
            Ok(Plan::CreateDatabase {
                sql: create_database_sql(name),
            })
        }
        other => Err(BackendError::with_code(
            codes::MUTATION_UNSUPPORTED,
            format!("unsupported mutation kind `{other}`"),
        )),
    }
}

pub(super) fn mutate(
    connection: &MyConnection,
    request: &MutationRequest,
    operation: &mut OperationGuard<'_>,
    capabilities: &DriverCapabilities,
) -> Result<MutationOutcome, BackendError> {
    let plan = plan(request, capabilities)?;
    let guard = connection.lock(Area::Mutation)?;
    let mut active = operation.activate(guard)?;
    // Declared after `active`, so it ends first.
    let running = connection.mark_running(active.operation_id());
    if active.cancel_requested() {
        // Cancelled between activation and the mark: no kill was sent.
        return Err(cancelled(Area::Mutation, None));
    }
    if active.conn.is_none() {
        return Err(BackendError::with_code(
            Area::Mutation.not_connected(),
            "connection closed",
        ));
    }
    let Ok(probe) = block_on(running.watch(active.in_user_transaction())) else {
        active.mark_broken();
        return Err(Abandoned::error(Area::Mutation));
    };
    let mode = match probe {
        Ok(true) => TxMode::Savepoint,
        Ok(false) => TxMode::Own,
        // Unknown: starting a transaction of our own could commit the user's
        // implicitly. Do nothing; a lost connection is closed for good.
        Err(error) => {
            if is_connection_lost(&error) {
                active.mark_broken();
            }
            return Err(map_error(&error, active.cancel_requested(), Area::Mutation));
        }
    };
    match plan {
        Plan::CreateDatabase { sql } => {
            if mode == TxMode::Savepoint {
                // DDL commits implicitly: it would commit the user's transaction.
                return Err(BackendError::with_code(
                    codes::CREATE_DATABASE_REJECTED,
                    "cannot create a database inside a transaction: it would commit it",
                ));
            }
            // A cancel that came during `DO 0` was not killed (no statement ran).
            running.check(Area::Mutation)?;
            let Ok(result) = block_on(running.watch(control(&mut active, &sql))) else {
                active.mark_broken();
                return Err(Abandoned::error(Area::Mutation));
            };
            if let Err(error) = result {
                if is_connection_lost(&error) {
                    active.mark_broken();
                }
                let mapped = map_error(&error, active.cancel_requested(), Area::Mutation);
                return Err(if mapped.code.is_some() {
                    mapped
                } else {
                    BackendError::with_code(codes::CREATE_DATABASE_REJECTED, mapped.message)
                });
            }
            // A new database cannot make an open result stale; nothing is written
            // to a table.
            Ok(MutationOutcome::new(0).keep_pages())
        }
        Plan::Dml { target, statements } => {
            // Phase 1: the table's schema, BEGIN (or SAVEPOINT) + every statement.
            // Any failure undoes (where the engine can).
            let Ok(executed) =
                block_on(running.watch(execute(&mut active, mode, &target, statements, &running)))
            else {
                // No undo or commit can reach a server that is not answering.
                active.mark_broken();
                return Err(Abandoned::error(Area::Mutation));
            };
            let executed = match executed {
                Ok(executed) => executed,
                Err(Failure::Refused(error)) => return Err(error),
                Err(Failure::Db(error)) => {
                    if is_connection_lost(&error) {
                        active.mark_broken();
                    }
                    return Err(map_error(&error, active.cancel_requested(), Area::Mutation));
                }
            };
            let affected = executed.affected;
            if !executed.transaction_open {
                // A table without transactions: every change is already applied
                // and there is nothing to commit or to undo.
                return Ok(MutationOutcome::new(affected));
            }
            // Phase 2: COMMIT (or RELEASE), unless a cancel was requested meanwhile.
            // From here no kill is sent (so none can land on the COMMIT or the
            // undo); a cancel that came before is seen by the kit's flag.
            running.end();
            if active.cancel_requested() {
                block_on(undo(&mut active, mode, &running));
                return Err(cancelled(Area::Mutation, None));
            }
            if let Err(error) = block_on(control(&mut active, mode.complete())) {
                block_on(undo(&mut active, mode, &running));
                // An interrupted or broken COMMIT of our own transaction may
                // still have been applied (a RELEASE inside the user's
                // transaction is undone with it).
                if mode == TxMode::Own && commit_outcome_unknown(&error) {
                    return Err(commit_unknown(&error));
                }
                return Err(map_error(&error, active.cancel_requested(), Area::Mutation));
            }
            Ok(MutationOutcome::new(affected))
        }
    }
}

/// `mutation.commit_unknown`: the COMMIT did not answer with a success, but it
/// may have been applied. Not retryable: repeating the change could apply it twice.
/// Whether a failed `COMMIT` may nevertheless have committed: it was interrupted
/// (error 1317) or the connection broke before the server answered.
fn commit_outcome_unknown(error: &mysql_async::Error) -> bool {
    is_connection_lost(error)
        || matches!(
            error,
            mysql_async::Error::Server(server) if server.code == ER_QUERY_INTERRUPTED
        )
}

fn commit_unknown(error: &mysql_async::Error) -> BackendError {
    BackendError::with_code(
        codes::MUTATION_COMMIT_UNKNOWN,
        format!(
            "the COMMIT was interrupted ({}); the change may or may not have been applied: reload the data and check before repeating it",
            describe_error(error)
        ),
    )
}

/// How a mutation is wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxMode {
    /// The mutation's own transaction.
    Own,
    /// The session is in a user transaction: join it through a savepoint.
    Savepoint,
}

const SAVEPOINT: &str = "tablex_mutation";

impl TxMode {
    fn begin(self) -> &'static str {
        match self {
            Self::Own => "START TRANSACTION",
            Self::Savepoint => "SAVEPOINT tablex_mutation",
        }
    }

    fn complete(self) -> &'static str {
        match self {
            Self::Own => "COMMIT",
            Self::Savepoint => "RELEASE SAVEPOINT tablex_mutation",
        }
    }
}

/// One statement without parameters (transaction control, DDL).
async fn control(state: &mut ConnState, statement: &str) -> mysql_async::Result<()> {
    state
        .conn
        .as_mut()
        .ok_or_else(|| other_error("connection closed"))?
        .query_drop(statement)
        .await
}

/// Undoes the mutation: `ROLLBACK`, or `ROLLBACK TO SAVEPOINT` for a mutation
/// inside a user transaction (the user's own work is left alone). If the
/// rollback fails the session may hold half a change, so the connection is
/// closed; every later call then fails with `<area>.not_connected`.
async fn undo(state: &mut ConnState, mode: TxMode, mark: &ActiveMark<'_>) {
    // No kill reaches the undo; one that was already on its way is tried again.
    mark.end();
    let undone = match mode {
        TxMode::Own => state.control_uninterrupted("ROLLBACK").await.is_ok(),
        TxMode::Savepoint => {
            let undone = state
                .control_uninterrupted(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                .await
                .is_ok();
            if undone {
                // The savepoint is no longer needed; leaving it would be harmless.
                let _ = state.control_uninterrupted(mode.complete()).await;
            }
            undone
        }
    };
    if !undone {
        state.mark_broken();
    }
}

enum Failure {
    Db(mysql_async::Error),
    /// The request was refused (a value the column cannot take, a statement that
    /// matched the wrong number of rows, a partial apply ...).
    Refused(BackendError),
}

/// What `execute` did.
struct Executed {
    /// Rows affected in total.
    affected: u64,
    /// A transaction (or savepoint) was begun and is still open for the caller
    /// to complete; false on a table without transactions.
    transaction_open: bool,
}

/// One `information_schema` row of a column: schema, table, column, `DATA_TYPE`,
/// `COLUMN_TYPE`, the table's engine, `DATABASE()` and `lower_case_table_names`.
type SchemaRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
);

/// The columns' classes and the engine of the target table, from
/// `information_schema` (empty and engine-less for a table it does not list: a
/// temporary table).
///
/// `information_schema` compares names with a case-insensitive collation on
/// MariaDB and MySQL 5.7, so with `lower_case_table_names=0` the query also
/// returns `Users` for `users`. The query therefore reads the candidates with
/// their names and [`pick_table`] keeps the rows of exactly the table asked for.
/// The join to `TABLES` folds case the same way, so a column of `Users` would
/// also be paired with the engine of `users`: [`same_table`] keeps only the
/// pairs whose two sides name the same table byte for byte.
async fn load_schema(state: &mut ConnState, target: &Target) -> mysql_async::Result<TableSchema> {
    let conn = state
        .conn
        .as_mut()
        .ok_or_else(|| other_error("connection closed"))?;
    let rows: Vec<JoinedSchemaRow> = conn
        .exec(
            "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, c.COLUMN_NAME, c.DATA_TYPE, c.COLUMN_TYPE, \
                    t.ENGINE, DATABASE(), @@lower_case_table_names, t.TABLE_SCHEMA, t.TABLE_NAME \
             FROM information_schema.COLUMNS c \
             JOIN information_schema.TABLES t \
               ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
             WHERE c.TABLE_SCHEMA = COALESCE(?, DATABASE()) AND c.TABLE_NAME = ?",
            (target.database.as_deref(), target.table.as_str()),
        )
        .await?;
    Ok(pick_table(same_table(rows), target))
}

/// A [`SchemaRow`] followed by the schema and name of the `TABLES` row it was
/// joined to.
type JoinedSchemaRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    String,
);

/// The rows whose column and table sides name the same table exactly: the
/// case-insensitive join pairs a column of `Users` with the `TABLES` row of
/// `users` too, which would hand one table the other's engine.
fn same_table(rows: Vec<JoinedSchemaRow>) -> Vec<SchemaRow> {
    rows.into_iter()
        .filter(|row| row.0 == row.8 && row.1 == row.9)
        .map(|(schema, table, column, data_type, column_type, engine, current, folds, _, _)| {
            (schema, table, column, data_type, column_type, engine, current, folds)
        })
        .collect()
}

/// The schema of exactly the table named by `target` among the rows the
/// case-insensitive query returned. Only when the server itself folds case
/// (`lower_case_table_names` 1 or 2, where one stored name stands for every
/// spelling) are rows of a differently spelled table used; with 0 a table that is
/// not listed under exactly its name is not listed at all (a temporary table, or
/// one `information_schema` skipped because of a concurrent DDL): the schema is
/// then empty, which binds plainly and treats the engine as unknown. Another
/// table's columns and engine are never used.
fn pick_table(rows: Vec<SchemaRow>, target: &Target) -> TableSchema {
    let folds = rows.first().is_some_and(|row| row.7 != 0);
    let wanted_schema = target
        .database
        .clone()
        .or_else(|| rows.first().and_then(|row| row.6.clone()));
    let exact = |row: &SchemaRow| {
        row.1 == target.table
            && wanted_schema
                .as_deref()
                .is_none_or(|schema| row.0 == schema)
    };
    let kept: Vec<SchemaRow> = if rows.iter().any(exact) {
        rows.into_iter().filter(exact).collect()
    } else if folds {
        let mut tables: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.0.as_str(), row.1.as_str()))
            .collect();
        tables.sort_unstable();
        tables.dedup();
        if tables.len() > 1 {
            Vec::new()
        } else {
            rows
        }
    } else {
        Vec::new()
    };
    let engine = kept.first().and_then(|row| row.5.clone());
    TableSchema {
        columns: kept
            .into_iter()
            .map(|(_, _, name, data_type, column_type, _, _, _)| {
                (name, ColumnClass::of(&data_type, &column_type))
            })
            .collect(),
        engine,
    }
}

/// "change 3: " in a batch, nothing for a single change.
fn which(index: usize, count: usize) -> String {
    if count > 1 {
        format!("change {index}: ")
    } else {
        String::new()
    }
}

/// "changes 3-9: " for a joined insert, "change 3: " in a batch, nothing for a single change.
fn which_statement(statement: &Statement, count: usize) -> String {
    match statement.rows {
        1 => which(statement.first_change, count),
        rows => format!("changes {}-{}: ", statement.first_change, statement.first_change + rows - 1),
    }
}

/// The most rows one joined insert carries.
const MAX_ROWS_PER_INSERT: usize = 1000;
/// The server refuses a statement with more placeholders than this.
const MAX_PLACEHOLDERS: usize = 65_535;
/// A joined insert never takes more than this, whatever the server allows; a row bigger than the budget goes alone.
const MAX_INSERT_BYTES: usize = 1 << 20;
/// The share of the server's `max_allowed_packet` a joined insert may take: the statement is one packet, and the
/// server counts more than the values (its header and the text of the statement).
const PACKET_SHARE: u64 = 2;

/// What a bound value takes in a statement packet: its bytes, plus the type and the length prefix the protocol adds
/// (up to nine bytes for a long value).
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::NULL => 1,
        Value::Bytes(bytes) => bytes.len() + 9,
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Double(_) => 11,
        Value::Date(..) | Value::Time(..) => 16,
    }
}

/// What the session lets a joined insert be, from its own `sql_mode` and `max_allowed_packet`. `None`: do not join.
///
/// Only a **strict** session joins. A single-row insert of NULL into a NOT NULL column is an error (1048) in any
/// mode; in a multi-row insert a session without `STRICT_TRANS_TABLES` / `STRICT_ALL_TABLES` stores the column's
/// implicit default instead (0, the empty string) and only warns, so joining would change what is written.
fn insert_budget(sql_mode: &str, max_allowed_packet: u64) -> Option<usize> {
    let strict = sql_mode
        .split(',')
        .any(|mode| mode.trim().eq_ignore_ascii_case("STRICT_TRANS_TABLES") || mode.trim().eq_ignore_ascii_case("STRICT_ALL_TABLES"));
    strict.then(|| MAX_INSERT_BYTES.min(usize::try_from(max_allowed_packet / PACKET_SHARE).unwrap_or(usize::MAX)))
}

/// The session's insert budget, read once for a batch (`None`: not strict, or not readable: nothing is joined).
async fn read_insert_budget(state: &mut ConnState) -> Option<usize> {
    let conn = state.conn.as_mut()?;
    let (mode, packet): (String, u64) = conn.query_first("SELECT @@SESSION.sql_mode, @@SESSION.max_allowed_packet").await.ok().flatten()?;
    insert_budget(&mode, packet)
}

/// `(prefix up to and including " VALUES ", the tuple)` of a single-row insert with parameters: what can be joined.
fn insert_shape(statement: &Statement) -> Option<(&str, &str)> {
    if statement.expect != Expect::AtLeastOne || statement.rows != 1 || statement.params.is_empty() {
        return None;
    }
    let at = statement.sql.rfind(") VALUES (")? + ") VALUES ".len();
    Some(statement.sql.split_at(at))
}

/// Whether two neighbouring statements are inserts of the same shape, so [`join_inserts`] would join something.
fn has_joinable_run(statements: &[(Statement, Vec<Value>)]) -> bool {
    statements.windows(2).any(|pair| match (insert_shape(&pair[0].0), insert_shape(&pair[1].0)) {
        (Some((first, _)), Some((second, _))) => first == second,
        _ => false,
    })
}

/// Joins each run of single-row inserts that write the same columns into multi-row inserts
/// (`INSERT … VALUES (?, ?), (?, ?), …`) of at most [`MAX_ROWS_PER_INSERT`] rows, [`MAX_PLACEHOLDERS`]
/// placeholders and `max_bytes` (the session's [`insert_budget`]). Only for a table with transactions: the batch is all or nothing there,
/// so which row of a joined statement failed does not matter. Everything else keeps its place and its own statement.
fn join_inserts(statements: Vec<(Statement, Vec<Value>)>, max_bytes: usize) -> Vec<(Statement, Vec<Value>)> {
    /// A run being joined: the SQL up to the first tuple, the tuple, and what has gathered.
    struct Run {
        prefix: String,
        tuple: String,
        first_change: usize,
        rows: usize,
        bytes: usize,
        values: Vec<Value>,
    }
    impl Run {
        fn finish(self) -> (Statement, Vec<Value>) {
            let mut sql = self.prefix;
            for row in 0..self.rows {
                if row > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&self.tuple);
            }
            let statement = Statement {
                sql,
                params: Vec::new(),
                key_start: 0,
                count_sql: None,
                expect: Expect::AtLeastOne,
                rows: self.rows,
                first_change: self.first_change,
            };
            (statement, self.values)
        }
    }
    let mut out = Vec::with_capacity(statements.len().min(64));
    let mut run: Option<Run> = None;
    for (statement, values) in statements {
        let Some((prefix, tuple)) = insert_shape(&statement) else {
            if let Some(done) = run.take() {
                out.push(done.finish());
            }
            out.push((statement, values));
            continue;
        };
        let width = values.len();
        // The values, the tuple of placeholders in the text and the comma after it.
        let bytes: usize = values.iter().map(value_bytes).sum::<usize>() + tuple.len() + 2;
        let fits = |run: &Run| {
            run.prefix == prefix
                && run.rows < MAX_ROWS_PER_INSERT
                && (run.rows + 1) * width <= MAX_PLACEHOLDERS
                && run.bytes + bytes <= max_bytes
        };
        if run.as_ref().is_some_and(|run| !fits(run)) {
            out.extend(run.take().map(Run::finish));
        }
        let run = run.get_or_insert_with(|| Run {
            prefix: prefix.to_owned(),
            tuple: tuple.to_owned(),
            first_change: statement.first_change,
            rows: 0,
            bytes: prefix.len(),
            values: Vec::new(),
        });
        run.rows += 1;
        run.bytes += bytes;
        run.values.extend(values);
    }
    out.extend(run.map(Run::finish));
    // A run of one row is the statement it came from.
    out
}

/// Reads the table's schema, binds every statement's values (a value the column
/// cannot take refuses the whole request before anything is sent), then runs the
/// statements in order: in one transaction (or savepoint) when the engine has
/// transactions, one at a time otherwise.
///
/// A kill the server receives between two statements, or during one too short to
/// look at its kill flag, is discarded: a batch of small statements would run to
/// its end. The cancel is therefore also looked at before every statement
/// (`mark`, set the moment the cancel arrives) and stops the batch there.
async fn execute(
    state: &mut ConnState,
    mode: TxMode,
    target: &Target,
    statements: Vec<Statement>,
    mark: &ActiveMark<'_>,
) -> Result<Executed, Failure> {
    mark.check(Area::Mutation).map_err(Failure::Refused)?;
    state.refresh_lex().await;
    let schema = match load_schema(state, target).await {
        Ok(schema) => schema,
        Err(error) => {
            if is_connection_lost(&error) {
                // The connection itself is gone: nothing to undo, nothing to reuse.
                state.mark_broken();
            }
            return Err(Failure::Db(error));
        }
    };
    let count = statements.len();
    let mut bound = Vec::with_capacity(count);
    for (index, mut statement) in statements.into_iter().enumerate() {
        let values = statement.bind(&schema).map_err(|mut error| {
            error.message = format!("{}{}", which(index, count), error.message);
            Failure::Refused(error)
        })?;
        statement.first_change = index;
        bound.push((statement, values));
    }
    match schema.transactional() {
        // All or nothing: runs of inserts can go as multi-row statements.
        true => {
            // Only worth a round trip when something would be joined, and only a strict session joins (see `insert_budget`).
            let budget = match has_joinable_run(&bound) {
                true => read_insert_budget(state).await,
                false => None,
            };
            let bound = match budget {
                Some(budget) => join_inserts(bound, budget),
                None => bound,
            };
            run_in_transaction(state, mode, bound, count, mark).await
        }
        false => run_one_by_one(state, target, schema.engine.as_deref(), bound, mark).await,
    }
}

/// How a statement's row count is judged: `Some((code, reason))` when it is wrong.
fn refusal(statement: &Statement, affected: u64) -> Option<(&'static str, &'static str)> {
    match (statement.expect, affected) {
        (_, 0) => Some((
            codes::MUTATION_NO_ROWS_AFFECTED,
            "the change affected no row",
        )),
        // A joined multi-row insert wrote fewer (or more) rows than it was given.
        (Expect::AtLeastOne, affected) if statement.rows > 1 && affected != statement.rows as u64 => Some((
            codes::MUTATION_NO_ROWS_AFFECTED,
            "the insert did not write every row it was given",
        )),
        (Expect::OneRow, 2..) => Some((
            codes::MUTATION_IDENTITY_NOT_UNIQUE,
            "the key matched several rows",
        )),
        _ => None,
    }
}

async fn run_statement(
    state: &mut ConnState,
    sql: String,
    values: Vec<Value>,
) -> mysql_async::Result<u64> {
    let params = if values.is_empty() {
        Params::Empty
    } else {
        Params::Positional(values)
    };
    match state.conn.as_mut() {
        Some(conn) => conn
            .exec_drop(sql, params)
            .await
            .map(|()| conn.affected_rows()),
        None => Err(other_error("connection closed")),
    }
}

/// Begins the transaction (or savepoint) and runs the statements in order. Any
/// failure (server error or wrong row count) is undone before returning. On
/// success the transaction stays open for the caller to complete.
async fn run_in_transaction(
    state: &mut ConnState,
    mode: TxMode,
    statements: Vec<(Statement, Vec<Value>)>,
    count: usize,
    mark: &ActiveMark<'_>,
) -> Result<Executed, Failure> {
    if let Err(error) = control(state, mode.begin()).await {
        if is_connection_lost(&error) {
            // The connection itself is gone: nothing to undo, nothing to reuse.
            state.mark_broken();
        } else if mode == TxMode::Own {
            // A killed `START TRANSACTION` may or may not have started it.
            undo(state, mode, mark).await;
        }
        return Err(Failure::Db(error));
    }
    let mut total = 0_u64;
    for (statement, values) in statements {
        if let Err(error) = mark.check(Area::Mutation) {
            undo(state, mode, mark).await;
            return Err(Failure::Refused(error));
        }
        let affected = match run_statement(state, statement.sql.clone(), values).await {
            Ok(affected) => affected,
            Err(error) => {
                undo(state, mode, mark).await;
                return Err(Failure::Db(error));
            }
        };
        if let Some((code, reason)) = refusal(&statement, affected) {
            undo(state, mode, mark).await;
            let rows = if affected > 1 {
                format!(" ({affected} rows)")
            } else {
                String::new()
            };
            return Err(Failure::Refused(BackendError::with_code(
                code,
                format!(
                    "{}{reason}{rows}; the change was rolled back",
                    which_statement(&statement, count)
                ),
            )));
        }
        total += affected;
    }
    Ok(Executed {
        affected: total,
        transaction_open: true,
    })
}

/// The changes of a table without transactions, one at a time and in order, each
/// guarded so it cannot touch more than one row (the key is counted first, then
/// the statement carries `LIMIT 1`). The first failure stops the run; what was
/// applied stays applied and is reported as such. A change whose statement was
/// cut off (connection lost, interrupted) may or may not have been applied: it is
/// reported as `unknown`, never as `failed`.
async fn run_one_by_one(
    state: &mut ConnState,
    target: &Target,
    engine: Option<&str>,
    statements: Vec<(Statement, Vec<Value>)>,
    mark: &ActiveMark<'_>,
) -> Result<Executed, Failure> {
    let count = statements.len();
    let mut total = 0_u64;
    for (index, (statement, values)) in statements.into_iter().enumerate() {
        let stopped = match one_guarded(state, &statement, values, mark).await {
            Ok(affected) => {
                total += affected;
                continue;
            }
            Err(stopped) => stopped,
        };
        if let Stopped::Db(error) = &stopped {
            if is_connection_lost(error) {
                state.mark_broken();
            }
        }
        let halt = match stopped {
            // The statement never ran, or the server refused it: not applied.
            Stopped::Db(error) if index == 0 => return Err(Failure::Db(error)),
            Stopped::Refused(error) if index == 0 => return Err(Failure::Refused(error)),
            Stopped::Db(error) => Halt::Failed(
                index,
                map_error(&error, mark.cancel_requested(), Area::Mutation).message,
            ),
            Stopped::Refused(error) => Halt::Failed(index, error.message),
            Stopped::Unknown(error) => {
                if is_connection_lost(&error) {
                    // Nothing more can be sent on it, and it is not trusted again.
                    state.mark_broken();
                }
                Halt::Unknown(
                    index,
                    map_error(&error, mark.cancel_requested(), Area::Mutation).message,
                )
            }
        };
        return Err(Failure::Refused(BackendError::with_code(
            codes::MUTATION_PARTIALLY_APPLIED,
            partial_message(&target.table, engine, halt, count),
        )));
    }
    Ok(Executed {
        affected: total,
        transaction_open: false,
    })
}

/// Why a guarded change did not run to the end.
enum Stopped {
    /// A definite failure: the change was not applied (a refusal, a server error,
    /// a failed pre-count, a cancel seen before the statement).
    Db(mysql_async::Error),
    Refused(BackendError),
    /// The statement itself was cut off: it may have been applied.
    Unknown(mysql_async::Error),
}

/// Where a run on a table without transactions stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Halt {
    /// Change `index` was refused or failed: not applied. Carries the cause.
    Failed(usize, String),
    /// Change `index` was cut off: it may or may not have been applied.
    Unknown(usize, String),
}

/// Whether a failed statement may nevertheless have been applied: the connection
/// broke before the server answered, or the statement was interrupted.
fn outcome_unknown(error: &mysql_async::Error) -> bool {
    commit_outcome_unknown(error)
}

/// One change on a table without transactions: cancel check, the key pre-count
/// (update/delete), then the statement itself with `LIMIT 1`. Returns the rows
/// it affected.
async fn one_guarded(
    state: &mut ConnState,
    statement: &Statement,
    values: Vec<Value>,
    mark: &ActiveMark<'_>,
) -> Result<u64, Stopped> {
    mark.check(Area::Mutation).map_err(Stopped::Refused)?;
    let mut sql = statement.sql.clone();
    if let Some(count_sql) = &statement.count_sql {
        let key = values[statement.key_start..].to_vec();
        let conn = state
            .conn
            .as_mut()
            .ok_or_else(|| Stopped::Db(other_error("connection closed")))?;
        let matched: Option<u64> = conn
            .exec_first(count_sql, Params::Positional(key))
            .await
            .map_err(Stopped::Db)?;
        match matched.unwrap_or(0) {
            0 => {
                return Err(Stopped::Refused(BackendError::with_code(
                    codes::MUTATION_NO_ROWS_AFFECTED,
                    "the change affected no row; it was not applied",
                )))
            }
            1 => {}
            _ => {
                return Err(Stopped::Refused(BackendError::with_code(
                    codes::MUTATION_IDENTITY_NOT_UNIQUE,
                    "the key matched several rows; the change was not applied",
                )))
            }
        }
        sql.push_str(" LIMIT 1");
        mark.check(Area::Mutation).map_err(Stopped::Refused)?;
    }
    let affected = run_statement(state, sql, values).await.map_err(|error| {
        if outcome_unknown(&error) {
            Stopped::Unknown(error)
        } else {
            Stopped::Db(error)
        }
    })?;
    match refusal(statement, affected) {
        None => Ok(affected),
        // The row vanished between the count and the statement: nothing changed.
        Some((code, reason)) => Err(Stopped::Refused(BackendError::with_code(
            code,
            format!("{reason}; the change was not applied"),
        ))),
    }
}

/// The text of `mutation.partially_applied`. It always ends with the four lists
/// `applied: [..]; failed: [..]; unknown: [..]; not attempted: [..]` (change
/// indexes; `unknown` and `failed` may be empty), so a reader can parse it from the
/// end: the cause before it can hold any text.
fn partial_message(table: &str, engine: Option<&str>, halt: Halt, count: usize) -> String {
    let list = |range: std::ops::Range<usize>| {
        range
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let what = match engine {
        Some(engine) => format!("uses the {engine} engine, which has no transactions"),
        None => "has no known transactional engine (a view, a temporary table or an unlisted \
                 engine)"
            .to_owned(),
    };
    let (at, headline, failed, unknown) = match &halt {
        Halt::Failed(at, cause) => (
            *at,
            format!("change {at} failed: {cause}."),
            at.to_string(),
            String::new(),
        ),
        Halt::Unknown(at, cause) => (
            *at,
            format!(
                "change {at} may or may not have been applied ({cause}): reload the data and \
                 check before repeating it."
            ),
            String::new(),
            at.to_string(),
        ),
    };
    format!(
        "{headline} Table `{table}` {what}, so the changes before it were NOT rolled back. \
         applied: [{}]; failed: [{failed}]; unknown: [{unknown}]; not attempted: [{}]",
        list(0..at),
        list(at + 1..count),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn capabilities() -> DriverCapabilities {
        struct Probe;
        impl super::super::Flavor for Probe {
            const CONFIG: super::super::FlavorConfig = super::super::FlavorConfig {
                driver: "probe",
                display_name: "Probe",
                uri_schemes: &["probe"],
                dialect: super::super::Dialect::MySql,
            };
        }
        super::super::capabilities::<Probe>()
    }

    fn request(kind: &str, identity: Json, payload: Json) -> MutationRequest {
        MutationRequest {
            operation_id: engine_driver_kit::api::OperationId(1),
            kind: kind.into(),
            database: Some("app".into()),
            object: Some("items".into()),
            identity,
            payload,
        }
    }

    fn plan_of(kind: &str, identity: Json, payload: Json) -> Result<Plan, BackendError> {
        plan(&request(kind, identity, payload), &capabilities())
    }

    /// The bound values for a table the driver knows nothing special about.
    fn plain(statement: &Statement) -> Vec<Value> {
        statement.bind(&TableSchema::default()).unwrap()
    }

    fn only(plan: Plan) -> Statement {
        let Plan::Dml { mut statements, .. } = plan else {
            panic!("DML plan expected");
        };
        assert_eq!(statements.len(), 1);
        statements.remove(0)
    }

    #[test]
    fn json_scalars_become_statement_parameters() {
        assert_eq!(json_to_value(&json!(null)), Value::NULL);
        assert_eq!(json_to_value(&json!(true)), Value::Int(1));
        assert_eq!(json_to_value(&json!(false)), Value::Int(0));
        assert_eq!(json_to_value(&json!(-7)), Value::Int(-7));
        assert_eq!(
            json_to_value(&json!(18_446_744_073_709_551_615_u64)),
            Value::UInt(u64::MAX)
        );
        // A decimal number keeps its digits (no round trip through a double).
        assert_eq!(json_to_value(&json!(1.5)), Value::Bytes(b"1.5".to_vec()));
        assert_eq!(
            json_to_value(&json!("12.50")),
            Value::Bytes(b"12.50".to_vec())
        );
        assert_eq!(
            json_to_value(&json!({"a": [1]})),
            Value::Bytes(br#"{"a":[1]}"#.to_vec())
        );
    }

    #[test]
    fn insert_binds_every_value() {
        let statement = only(
            plan_of(
                "insert",
                json!({}),
                json!({"op": "insert", "values": {"name": "Ada", "qty": 3, "note": null}}),
            )
            .unwrap(),
        );
        assert!(statement.sql.starts_with("INSERT INTO `app`.`items` ("));
        assert_eq!(statement.params.len(), 3);
        assert_eq!(statement.count_sql, None);
        assert_eq!(statement.expect, Expect::AtLeastOne);
        assert_eq!(statement.sql.matches('?').count(), 3);
        // No values: the empty insert.
        let empty = only(plan_of("insert", json!(null), json!({"op": "insert"})).unwrap());
        assert_eq!(empty.sql, "INSERT INTO `app`.`items` () VALUES ()");
    }

    #[test]
    fn update_binds_values_then_composite_key_and_requires_one_row() {
        let statement = only(
            plan_of(
                "update",
                json!({}),
                json!({"op": "update", "key": {"id": 7, "tenant": "a"}, "values": {"qty": 9}}),
            )
            .unwrap(),
        );
        assert_eq!(
            statement.sql,
            "UPDATE `app`.`items` SET `qty` = ? WHERE `id` <=> ? AND `tenant` <=> ?"
        );
        assert_eq!(
            plain(&statement),
            vec![Value::Int(9), Value::Int(7), Value::Bytes(b"a".to_vec())]
        );
        assert_eq!(statement.key_start, 1);
        assert_eq!(
            statement.count_sql.as_deref(),
            Some(
                "SELECT COUNT(*) FROM (SELECT 1 FROM `app`.`items` WHERE `id` <=> ? \
                 AND `tenant` <=> ? LIMIT 2) AS tablex_guard"
            )
        );
        assert_eq!(statement.expect, Expect::OneRow);
    }

    #[test]
    fn delete_is_keyed_and_single_row() {
        let statement = only(
            plan_of(
                "delete",
                json!({"schema": "ignored"}),
                json!({"op": "delete", "key": {"id": 7, "tenant": "a"}}),
            )
            .unwrap(),
        );
        assert_eq!(
            statement.sql,
            "DELETE FROM `app`.`items` WHERE `id` <=> ? AND `tenant` <=> ?"
        );
        assert_eq!(statement.params.len(), 2);
        assert_eq!(statement.expect, Expect::OneRow);
    }

    #[test]
    fn batch_keeps_the_order_of_its_changes() {
        let Plan::Dml { statements, .. } = plan_of(
            "batch",
            json!({}),
            json!({"changes": [
                {"op": "insert", "values": {"id": 1}},
                {"op": "update", "key": {"id": 1}, "values": {"qty": 2}},
                {"op": "delete", "key": {"id": 1}},
            ]}),
        )
        .unwrap() else {
            panic!("DML plan expected");
        };
        let kinds: Vec<_> = statements
            .iter()
            .map(|statement| statement.sql.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(kinds, ["INSERT", "UPDATE", "DELETE"]);
    }

    #[test]
    fn unsafe_or_incomplete_requests_are_refused() {
        let bad = |kind: &str, identity: Json, payload: Json| {
            let error =
                plan_of(kind, identity, payload.clone()).expect_err(&format!("{kind} {payload}"));
            assert_eq!(
                error.code.as_deref(),
                Some("mutation.bad_request"),
                "{kind} {payload}"
            );
        };
        // Empty and missing keys; empty update values.
        bad(
            "update",
            json!({}),
            json!({"op": "update", "values": {"a": 1}}),
        );
        bad(
            "update",
            json!({}),
            json!({"op": "update", "key": {"id": 1}}),
        );
        bad("delete", json!({}), json!({"op": "delete", "key": {}}));
        bad("delete", json!({}), json!(null));
        // The op must be the kind.
        bad(
            "update",
            json!({}),
            json!({"op": "delete", "key": {"id": 1}}),
        );
        bad(
            "insert",
            json!({}),
            json!({"op": "update", "key": {"id": 1}, "values": {"a": 1}}),
        );
        // Identity must be a SqlTableIdentity.
        bad("insert", json!("x"), json!({"op": "insert"}));
        // Batches.
        bad("batch", json!({}), json!({"changes": []}));
        bad(
            "batch",
            json!({}),
            json!({"changes": [{"op": "delete", "key": {}}]}),
        );
        let mut no_table = request(
            "delete",
            json!({}),
            json!({"op": "delete", "key": {"id": 1}}),
        );
        no_table.object = None;
        assert!(plan(&no_table, &capabilities()).is_err());
        let unknown = plan_of("truncate", json!({}), json!({})).unwrap_err();
        assert_eq!(unknown.code.as_deref(), Some(codes::MUTATION_UNSUPPORTED));
    }

    #[test]
    fn batch_change_errors_name_the_change() {
        let error = plan_of(
            "batch",
            json!({}),
            json!({"changes": [
                {"op": "insert", "values": {"id": 1}},
                {"op": "update", "key": {"id": 1}, "values": {}},
            ]}),
        )
        .unwrap_err();
        assert!(error.message.starts_with("change 1:"), "{}", error.message);
    }

    #[test]
    fn a_null_key_value_matches_null() {
        let statement = only(
            plan_of(
                "delete",
                json!({}),
                json!({"op": "delete", "key": {"owner": null}}),
            )
            .unwrap(),
        );
        assert_eq!(
            statement.sql,
            "DELETE FROM `app`.`items` WHERE `owner` <=> ?"
        );
        assert_eq!(plain(&statement), vec![Value::NULL]);
    }

    #[test]
    fn create_database_is_ddl_without_a_transaction_and_takes_no_options() {
        let mut create = request("create_database", json!(null), json!(null));
        create.object = None;
        create.database = Some("reports".into());
        assert_eq!(
            plan(&create, &capabilities()).unwrap(),
            Plan::CreateDatabase {
                sql: "CREATE DATABASE `reports`".into()
            }
        );
        create.payload = json!({});
        assert!(plan(&create, &capabilities()).is_ok());
        create.payload = json!({"charset": "utf8mb4"});
        let error = plan(&create, &capabilities()).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("mutation.bad_request"));
        create.payload = json!(null);
        create.database = None;
        assert!(plan(&create, &capabilities()).is_err());
    }

    #[test]
    fn transaction_modes_wrap_the_right_statements() {
        assert_eq!(TxMode::Own.begin(), "START TRANSACTION");
        assert_eq!(TxMode::Own.complete(), "COMMIT");
        assert_eq!(TxMode::Savepoint.begin(), "SAVEPOINT tablex_mutation");
        assert_eq!(
            TxMode::Savepoint.complete(),
            "RELEASE SAVEPOINT tablex_mutation"
        );
        assert!(TxMode::Savepoint.begin().ends_with(SAVEPOINT));
    }

    fn schema(columns: &[(&str, &str, &str)], engine: Option<&str>) -> TableSchema {
        TableSchema {
            columns: columns
                .iter()
                .map(|(name, data_type, column_type)| {
                    ((*name).to_owned(), ColumnClass::of(data_type, column_type))
                })
                .collect(),
            engine: engine.map(str::to_owned),
        }
    }

    fn bound(schema: &TableSchema, payload: Json) -> Result<Vec<Value>, BackendError> {
        only(plan_of("update", json!({}), payload).unwrap()).bind(schema)
    }

    fn code_of(result: Result<Vec<Value>, BackendError>) -> String {
        result.unwrap_err().code.unwrap().into_owned()
    }

    #[test]
    fn column_classes_come_from_the_data_type() {
        for (data_type, column_type, class) in [
            ("binary", "binary(16)", ColumnClass::Binary),
            ("varbinary", "varbinary(8)", ColumnClass::Binary),
            ("tinyblob", "tinyblob", ColumnClass::Binary),
            ("BLOB", "blob", ColumnClass::Binary),
            ("longblob", "longblob", ColumnClass::Binary),
            ("bit", "bit(1)", ColumnClass::Bit { width: 1 }),
            ("bit", "bit(64)", ColumnClass::Bit { width: 64 }),
            ("bit", "bit", ColumnClass::Bit { width: 1 }),
            ("point", "point", ColumnClass::Geometry),
            ("geometry", "geometry", ColumnClass::Geometry),
            ("multipolygon", "multipolygon", ColumnClass::Geometry),
            (
                "geometrycollection",
                "geometrycollection",
                ColumnClass::Geometry,
            ),
            ("text", "text", ColumnClass::Plain),
            ("varchar", "varchar(8)", ColumnClass::Plain),
            ("tinyint", "tinyint(1)", ColumnClass::Plain),
            ("json", "json", ColumnClass::Plain),
        ] {
            assert_eq!(
                ColumnClass::of(data_type, column_type),
                class,
                "{data_type}"
            );
        }
    }

    #[test]
    fn binary_columns_take_0x_hex_and_bind_real_bytes() {
        let table = schema(
            &[("id", "binary", "binary(4)"), ("b", "blob", "blob")],
            None,
        );
        for (text, bytes) in [
            ("0x00ff10A0", vec![0x00, 0xff, 0x10, 0xa0]),
            ("0x", vec![]),
            (" 0xAB ", vec![0xab]),
        ] {
            let values = bound(
                &table,
                json!({"op": "update", "key": {"id": text}, "values": {"b": text}}),
            )
            .unwrap();
            assert_eq!(
                values,
                vec![Value::Bytes(bytes.clone()), Value::Bytes(bytes)]
            );
        }
        // Anything else is refused, with a typed error, for a value and for a key.
        for text in ["hello", "0x1", "0xzz", "X'AB'", "00ff", "0x 00", ""] {
            for payload in [
                json!({"op": "update", "key": {"id": 1}, "values": {"b": text}}),
                json!({"op": "update", "key": {"ID": text}, "values": {"x": 1}}),
            ] {
                assert_eq!(
                    code_of(bound(&table, payload)),
                    codes::MUTATION_INVALID_VALUE,
                    "{text}"
                );
            }
        }
        for not_text in [json!(5), json!(true), json!([1])] {
            let payload = json!({"op": "update", "key": {"id": 1}, "values": {"b": not_text}});
            assert_eq!(
                code_of(bound(&table, payload)),
                codes::MUTATION_INVALID_VALUE
            );
        }
        // NULL is NULL.
        let values = bound(
            &table,
            json!({"op": "update", "key": {"id": "0x01020304"}, "values": {"b": null}}),
        )
        .unwrap();
        assert_eq!(values, vec![Value::NULL, Value::Bytes(vec![1, 2, 3, 4])]);
        // A plain column keeps the text as it is: `0x41` is just text there.
        let plain_table = schema(&[("t", "text", "text")], None);
        let values = bound(
            &plain_table,
            json!({"op": "update", "key": {"t": "a"}, "values": {"t": "0x41"}}),
        )
        .unwrap();
        assert_eq!(values[0], Value::Bytes(b"0x41".to_vec()));
    }

    #[test]
    fn bit_columns_take_integers_and_binary_literals() {
        let table = schema(
            &[
                ("f", "bit", "bit(1)"),
                ("w", "bit", "bit(8)"),
                ("x", "bit", "bit(64)"),
            ],
            None,
        );
        let set = |column: &str, value: Json| {
            bound(
                &table,
                json!({"op": "update", "key": {"k": 1}, "values": {column: value}}),
            )
        };
        assert_eq!(set("f", json!("1")).unwrap()[0], Value::UInt(1));
        assert_eq!(set("f", json!("0")).unwrap()[0], Value::UInt(0));
        assert_eq!(set("f", json!(true)).unwrap()[0], Value::UInt(1));
        assert_eq!(set("w", json!("65")).unwrap()[0], Value::UInt(65));
        assert_eq!(set("w", json!(255)).unwrap()[0], Value::UInt(255));
        assert_eq!(set("w", json!("b'0101'")).unwrap()[0], Value::UInt(5));
        assert_eq!(set("w", json!("B'11111111'")).unwrap()[0], Value::UInt(255));
        assert_eq!(
            set("x", json!("18446744073709551615")).unwrap()[0],
            Value::UInt(u64::MAX)
        );
        assert_eq!(set("f", Json::Null).unwrap()[0], Value::NULL);
        for bad in [
            json!("2"),
            json!("256"),
            json!("-1"),
            json!("abc"),
            json!("b''"),
            json!("b'012'"),
            json!(""),
            json!(1.5),
            json!([1]),
        ] {
            let column = if bad == json!("256") || bad == json!("2") {
                "f"
            } else {
                "w"
            };
            assert_eq!(
                code_of(set(column, bad.clone())),
                codes::MUTATION_INVALID_VALUE,
                "{bad}"
            );
        }
    }

    #[test]
    fn geometry_columns_refuse_every_value_but_null() {
        let table = schema(
            &[("g", "point", "point"), ("p", "polygon", "polygon")],
            None,
        );
        let write = |value: Json| {
            bound(
                &table,
                json!({"op": "update", "key": {"k": 1}, "values": {"g": value}}),
            )
        };
        assert_eq!(
            code_of(write(json!("POINT (1 2)"))),
            codes::MUTATION_VALUE_NOT_EDITABLE
        );
        assert_eq!(code_of(write(json!(5))), codes::MUTATION_VALUE_NOT_EDITABLE);
        assert_eq!(write(Json::Null).unwrap()[0], Value::NULL);
        let key = bound(
            &table,
            json!({"op": "update", "key": {"p": "POLYGON EMPTY"}, "values": {"k": 1}}),
        );
        assert_eq!(code_of(key), codes::MUTATION_VALUE_NOT_EDITABLE);
    }

    #[test]
    fn only_engines_known_to_have_transactions_are_rolled_back() {
        for engine in [
            "InnoDB",
            "innodb",
            "XtraDB",
            "TokuDB",
            "ROCKSDB",
            "NDBCLUSTER",
        ] {
            assert!(schema(&[], Some(engine)).transactional(), "{engine}");
        }
        for engine in [
            "MyISAM",
            "Aria",
            "MEMORY",
            "CSV",
            "ARCHIVE",
            "BLACKHOLE",
            "MRG_MyISAM",
            "SomethingNew",
        ] {
            assert!(!schema(&[], Some(engine)).transactional(), "{engine}");
        }
        // Unknown (a view, a temporary table): never claimed to roll back.
        assert!(!schema(&[], None).transactional());
    }

    #[test]
    fn a_partial_apply_names_what_was_applied_failed_unknown_and_not_attempted() {
        let message = partial_message(
            "t",
            Some("MyISAM"),
            Halt::Failed(2, "the key matched several rows".into()),
            5,
        );
        assert!(
            message.starts_with("change 2 failed: the key matched several rows."),
            "{message}"
        );
        assert!(
            message.ends_with("applied: [0, 1]; failed: [2]; unknown: []; not attempted: [3, 4]"),
            "{message}"
        );
        assert!(message.contains("NOT rolled back"), "{message}");
        assert!(!message.contains("was rolled back"), "{message}");
        let last = partial_message("t", Some("Aria"), Halt::Failed(1, "x".into()), 2);
        assert!(
            last.ends_with("applied: [0]; failed: [1]; unknown: []; not attempted: []"),
            "{last}"
        );
        // Cut off: the change is unknown, never failed.
        let cut = partial_message(
            "t",
            Some("MyISAM"),
            Halt::Unknown(1, "connection lost".into()),
            4,
        );
        assert!(cut.contains("may or may not have been applied"), "{cut}");
        assert!(
            cut.ends_with("applied: [0]; failed: []; unknown: [1]; not attempted: [2, 3]"),
            "{cut}"
        );
        // The first change can be unknown too; so can a single change.
        let first = partial_message("t", Some("MyISAM"), Halt::Unknown(0, "x".into()), 1);
        assert!(
            first.ends_with("applied: []; failed: []; unknown: [0]; not attempted: []"),
            "{first}"
        );
        // An engine that is not known says so instead of naming one.
        let view = partial_message("v", None, Halt::Failed(1, "x".into()), 2);
        assert!(view.contains("no known transactional engine"), "{view}");
        // The cause may hold anything, also something that looks like the lists.
        let tricky = partial_message(
            "t",
            Some("MyISAM"),
            Halt::Failed(1, "Duplicate entry 'applied: [9]; failed: [8]'".into()),
            3,
        );
        assert!(
            tricky.ends_with("applied: [0]; failed: [1]; unknown: []; not attempted: [2]"),
            "{tricky}"
        );
    }

    #[test]
    fn a_cut_off_statement_has_an_unknown_outcome_a_refusal_does_not() {
        let server = |code| {
            mysql_async::Error::Server(mysql_async::ServerError {
                code,
                message: "x".into(),
                state: "HY000".into(),
            })
        };
        assert!(outcome_unknown(&server(
            super::super::conn::ER_QUERY_INTERRUPTED
        )));
        assert!(outcome_unknown(&mysql_async::Error::Io(
            mysql_async::IoError::Io(std::io::Error::other("gone"))
        )));
        // Duplicate entry, syntax error, lock wait timeout: the server said no.
        for code in [1062, 1064, 1205, 1048] {
            assert!(!outcome_unknown(&server(code)), "{code}");
        }
    }

    #[test]
    fn a_column_is_never_paired_with_the_engine_of_a_table_spelled_differently() {
        let joined = |table: &str, joined_table: &str, engine: &str| {
            let (schema, table, column, data_type, column_type, engine, current, folds) =
                schema_row("app", table, "id", "int", engine, 0);
            (
                schema,
                table,
                column,
                data_type,
                column_type,
                engine,
                current,
                folds,
                "app".to_string(),
                joined_table.to_string(),
            )
        };
        // `Users` (MyISAM) and `users` (InnoDB): the case-insensitive join yields both
        // pairings for the column of `Users`, in an order the server chooses.
        let rows = vec![
            joined("Users", "users", "InnoDB"),
            joined("Users", "Users", "MyISAM"),
        ];
        let picked = pick_table(same_table(rows), &target(Some("app"), "Users"));
        assert_eq!(picked.engine.as_deref(), Some("MyISAM"));
        assert!(!picked.transactional());
    }

    fn schema_row(
        schema: &str,
        table: &str,
        column: &str,
        data_type: &str,
        engine: &str,
        folds: i64,
    ) -> SchemaRow {
        (
            schema.into(),
            table.into(),
            column.into(),
            data_type.into(),
            data_type.into(),
            Some(engine.into()),
            Some("app".into()),
            folds,
        )
    }

    fn target(database: Option<&str>, table: &str) -> Target {
        Target {
            database: database.map(str::to_owned),
            table: table.to_owned(),
        }
    }

    #[test]
    fn a_table_is_matched_exactly_not_case_insensitively() {
        // The case-insensitive query returned both `Users` and `users`.
        let rows = || {
            vec![
                schema_row("app", "Users", "id", "binary", "InnoDB", 0),
                schema_row("app", "users", "id", "text", "MyISAM", 0),
            ]
        };
        let upper = pick_table(rows(), &target(Some("app"), "Users"));
        assert_eq!(upper.class_of("id"), ColumnClass::Binary);
        assert_eq!(upper.engine.as_deref(), Some("InnoDB"));
        let lower = pick_table(rows(), &target(None, "users"));
        assert_eq!(lower.class_of("id"), ColumnClass::Plain);
        assert_eq!(lower.engine.as_deref(), Some("MyISAM"));
        // The schema is compared exactly too (the default database comes from `DATABASE()`).
        let other_schema = vec![
            schema_row("App", "t", "id", "binary", "InnoDB", 0),
            schema_row("app", "t", "id", "text", "MyISAM", 0),
        ];
        let picked = pick_table(other_schema, &target(None, "t"));
        assert_eq!(picked.class_of("id"), ColumnClass::Plain);
        // lower_case_table_names=0 and the table itself not listed (a concurrent DDL made
        // `information_schema` skip it): the other spelling is NOT used.
        let skipped = vec![schema_row("app", "users", "id", "binary", "InnoDB", 0)];
        let picked = pick_table(skipped, &target(Some("app"), "Users"));
        assert_eq!((picked.columns.len(), picked.engine), (0, None));
        // Several tables differing in case, none exact: nothing is guessed.
        let picked = pick_table(rows(), &target(Some("app"), "USERS"));
        assert_eq!((picked.columns.len(), picked.engine), (0, None));
        // lower_case_table_names=1 / 2: the server folds case, one table stands for every
        // spelling, so a differently spelled request is that table.
        let folded = vec![schema_row("app", "users", "id", "binary", "InnoDB", 1)];
        let picked = pick_table(folded, &target(Some("app"), "USERS"));
        assert_eq!(picked.class_of("id"), ColumnClass::Binary);
        assert_eq!(picked.engine.as_deref(), Some("InnoDB"));
        // Nothing listed (a temporary table): empty, engine unknown.
        let none = pick_table(Vec::new(), &target(None, "tmp"));
        assert_eq!((none.columns.len(), none.engine), (0, None));
    }

    #[test]
    fn a_batch_names_the_change_whose_value_is_refused() {
        let plan = plan_of(
            "batch",
            json!({}),
            json!({"changes": [
                {"op": "insert", "values": {"id": 1}},
                {"op": "insert", "values": {"b": "nope"}},
            ]}),
        )
        .unwrap();
        let Plan::Dml { statements, .. } = plan else {
            panic!("DML plan expected");
        };
        let table = schema(&[("b", "blob", "blob")], Some("MyISAM"));
        assert!(statements[0].bind(&table).is_ok());
        assert!(statements[1].bind(&table).is_err());
    }

    fn joined(changes: Json) -> Vec<(Statement, Vec<Value>)> {
        let Plan::Dml { statements, .. } = plan_of("batch", json!({}), json!({ "changes": changes })).unwrap() else {
            panic!("DML plan expected");
        };
        let bound = statements
            .into_iter()
            .enumerate()
            .map(|(index, mut statement)| {
                let values = plain(&statement);
                statement.first_change = index;
                (statement, values)
            })
            .collect();
        join_inserts(bound, MAX_INSERT_BYTES)
    }

    fn planned(changes: Json) -> Vec<(Statement, Vec<Value>)> {
        let Plan::Dml { statements, .. } = plan_of("batch", json!({}), json!({ "changes": changes })).unwrap() else {
            panic!("DML plan expected");
        };
        statements.into_iter().map(|statement| { let values = plain(&statement); (statement, values) }).collect()
    }

    #[test]
    fn the_session_is_asked_for_its_budget_only_when_two_neighbouring_inserts_could_join() {
        assert!(!has_joinable_run(&planned(json!([{"op": "insert", "values": {"a": 1}}]))), "a single insert");
        assert!(!has_joinable_run(&planned(json!([
            {"op": "update", "key": {"id": 1}, "values": {"a": 9}},
            {"op": "delete", "key": {"id": 2}},
        ]))), "no inserts");
        assert!(!has_joinable_run(&planned(json!([
            {"op": "insert", "values": {"a": 1}},
            {"op": "update", "key": {"id": 1}, "values": {"a": 9}},
            {"op": "insert", "values": {"a": 2}},
        ]))), "inserts apart");
        assert!(!has_joinable_run(&planned(json!([
            {"op": "insert", "values": {"a": 1}},
            {"op": "insert", "values": {"b": 1}},
        ]))), "other columns");
        assert!(has_joinable_run(&planned(json!([
            {"op": "insert", "values": {"a": 1}},
            {"op": "insert", "values": {"a": 2}},
        ]))));
    }

    #[test]
    fn a_run_of_inserts_with_the_same_columns_becomes_one_multi_row_statement() {
        let out = joined(json!([
            {"op": "insert", "values": {"a": 1, "b": "x"}},
            {"op": "insert", "values": {"a": 2, "b": "y"}},
            {"op": "insert", "values": {"a": 3, "b": "z"}},
        ]));
        assert_eq!(out.len(), 1);
        let (statement, values) = &out[0];
        assert_eq!(statement.sql, "INSERT INTO `app`.`items` (`a`, `b`) VALUES (?, ?), (?, ?), (?, ?)");
        assert_eq!((statement.rows, statement.first_change, values.len()), (3, 0, 6));
        assert_eq!(values[2], Value::from(2));
    }

    #[test]
    fn inserts_with_other_columns_and_other_changes_keep_their_statements_and_order() {
        let out = joined(json!([
            {"op": "insert", "values": {"a": 1}},
            {"op": "insert", "values": {"a": 2}},
            {"op": "update", "key": {"id": 1}, "values": {"a": 9}},
            {"op": "insert", "values": {"b": 1}},
            {"op": "insert", "values": {}},
            {"op": "delete", "key": {"id": 2}},
        ]));
        let kinds: Vec<(usize, usize, &str)> = out.iter().map(|(statement, _)| (statement.first_change, statement.rows, statement.sql.split_whitespace().next().unwrap())).collect();
        assert_eq!(kinds, [(0, 2, "INSERT"), (2, 1, "UPDATE"), (3, 1, "INSERT"), (4, 1, "INSERT"), (5, 1, "DELETE")]);
        assert_eq!(out[0].0.sql, "INSERT INTO `app`.`items` (`a`) VALUES (?), (?)");
        assert_eq!(out[4].0.expect, Expect::OneRow);
    }

    #[test]
    fn a_joined_insert_stops_at_the_row_and_placeholder_limits() {
        let rows: Vec<Json> = (0..2500).map(|id| json!({"op": "insert", "values": {"id": id}})).collect();
        let out = joined(Json::Array(rows));
        let sizes: Vec<usize> = out.iter().map(|(statement, _)| statement.rows).collect();
        assert_eq!(sizes, [1000, 1000, 500]);
        assert_eq!(out[1].0.first_change, 1000);
        // Wide rows: 65535 placeholders is the other limit (12 columns -> 5461 rows would fit; 1000 is the cap).
        let wide: Vec<Json> = (0..3).map(|id| {
            let values: serde_json::Map<String, Json> = (0..40_000).map(|column| (format!("c{column}"), json!(id))).collect();
            json!({"op": "insert", "values": values})
        }).collect();
        let sizes: Vec<usize> = joined(Json::Array(wide)).iter().map(|(statement, _)| statement.rows).collect();
        assert_eq!(sizes, [1, 1, 1], "two 40000-wide rows would need 80000 placeholders");
    }

    #[test]
    fn a_joined_insert_must_write_every_row_it_carries() {
        let out = joined(json!([{"op": "insert", "values": {"a": 1}}, {"op": "insert", "values": {"a": 2}}]));
        let statement = &out[0].0;
        assert_eq!(refusal(statement, 2), None);
        assert_eq!(refusal(statement, 1).map(|(code, _)| code), Some(codes::MUTATION_NO_ROWS_AFFECTED));
        assert_eq!(refusal(statement, 0).map(|(code, _)| code), Some(codes::MUTATION_NO_ROWS_AFFECTED));
        assert_eq!(which_statement(statement, 2), "changes 0-1: ");
    }

    #[test]
    fn a_blob_counts_its_bytes_against_the_packet_budget() {
        let first = Value::Bytes(vec![0; MAX_INSERT_BYTES / 2 + 1]);
        let out = {
            let statements = ["INSERT INTO `t` (`b`) VALUES (?)"; 2]
                .iter()
                .enumerate()
                .map(|(index, sql)| {
                    let statement = Statement {
                        sql: (*sql).to_owned(),
                        params: vec![Param { column: "b".into(), value: Json::Null }],
                        key_start: 1,
                        count_sql: None,
                        expect: Expect::AtLeastOne,
                        rows: 1,
                        first_change: index,
                    };
                    (statement, vec![first.clone()])
                })
                .collect();
            join_inserts(statements, MAX_INSERT_BYTES)
        };
        // Two values over half the budget never share a statement.
        assert_eq!(out.iter().map(|(statement, _)| statement.rows).collect::<Vec<_>>(), [1, 1]);
    }

    #[test]
    fn only_a_strict_session_joins_inserts_and_the_budget_follows_the_packet_size() {
        // Without strict mode a multi-row insert stores 0 for NULL in a NOT NULL column instead of failing.
        assert_eq!(insert_budget("NO_ENGINE_SUBSTITUTION", 64 << 20), None);
        assert_eq!(insert_budget("", 64 << 20), None);
        assert_eq!(insert_budget("ANSI_QUOTES,NO_ZERO_DATE", 64 << 20), None);
        for mode in ["STRICT_TRANS_TABLES", "strict_all_tables", "ONLY_FULL_GROUP_BY, STRICT_TRANS_TABLES ,NO_ZERO_DATE", "NO_AUTO_VALUE_ON_ZERO,STRICT_ALL_TABLES"] {
            assert_eq!(insert_budget(mode, 64 << 20), Some(MAX_INSERT_BYTES), "{mode}");
        }
        // A small packet limit makes the budget half of it; a huge one never lifts it past the cap.
        assert_eq!(insert_budget("STRICT_TRANS_TABLES", 400_000), Some(200_000));
        assert_eq!(insert_budget("STRICT_TRANS_TABLES", 4 << 30), Some(MAX_INSERT_BYTES));
        assert_eq!(insert_budget("STRICT_TRANS_TABLES", 0), Some(0));
    }

    #[test]
    fn a_small_budget_splits_a_run_and_a_row_over_it_goes_alone() {
        let statement = |index: usize| {
            let statement = Statement {
                sql: "INSERT INTO `t` (`b`) VALUES (?)".to_owned(),
                params: vec![Param { column: "b".into(), value: Json::Null }],
                key_start: 1,
                count_sql: None,
                expect: Expect::AtLeastOne,
                rows: 1,
                first_change: index,
            };
            (statement, vec![Value::Bytes(vec![0; 100])])
        };
        let rows = |budget: usize| join_inserts((0..5).map(statement).collect(), budget).iter().map(|(statement, _)| statement.rows).collect::<Vec<_>>();
        let prefix = "INSERT INTO `t` (`b`) VALUES ".len();
        let one_row = value_bytes(&Value::Bytes(vec![0; 100])) + "(?)".len() + 2;
        // Exactly two rows fit: the boundary is inclusive, and one byte less makes it one row.
        assert_eq!(rows(prefix + 2 * one_row), [2, 2, 1]);
        assert_eq!(rows(prefix + 2 * one_row - 1), [1, 1, 1, 1, 1]);
        // A row bigger than the whole budget is still sent.
        assert_eq!(rows(10), [1, 1, 1, 1, 1]);
        assert_eq!(value_bytes(&Value::Bytes(vec![0; 100])), 109, "the type and the length prefix count");
    }
}
