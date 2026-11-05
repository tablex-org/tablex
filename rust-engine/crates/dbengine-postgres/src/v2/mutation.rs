//! `dbengine_mutate`: row edits and `CREATE DATABASE`.
//!
//! The request shapes are the shared SQL contract
//! (`engine_driver_api::sql`), identical for every SQL driver.
//!
//! # Row mutations
//!
//! ```json
//! {"operation_id": 7, "kind": "update", "database": "app", "object": "orders",
//!  "identity": {"schema": "public"},
//!  "payload": {"op": "update", "key": {"id": 42}, "values": {"status": "shipped", "note": null}}}
//! ```
//!
//! * `database`: database to write to (default: the connection's).
//! * `object`: the table name (required); `identity` is a `SqlTableIdentity`
//!   (`schema`, default `public`).
//! * Kinds `insert`, `update`, `delete`: `payload` is one `SqlRowChange` whose
//!   `op` equals the kind. Kind `batch`: `payload` is a `SqlBatch`
//!   (`{"changes": [SqlRowChange, ...]}`), applied in order, in **one
//!   transaction**, all or nothing. Every op of a batch must be advertised
//!   (`mutation.<op>`; all three are).
//! * `insert`: `values` is the new row (empty = `DEFAULT VALUES`).
//!   `update`: `key` (column -> value, composite keys allowed) identifies
//!   exactly one row and `values` holds the changed columns (non-empty).
//!   `delete`: `key` identifies exactly one row.
//! * Failure codes (the whole request is rolled back): `mutation.bad_request`
//!   (malformed request, nothing sent to the server),
//!   `mutation.no_rows_affected` (a statement matched no row),
//!   `mutation.identity_not_unique` (an update/delete key matched more than one
//!   row), `mutation.failed` (server error, with its message). Cancelled:
//!   `query.cancelled`.
//!
//! The result's `affected_rows` is the total over all statements; the
//! connection's open pages are invalidated.
//!
//! # Transactions
//!
//! If the session has no open transaction the request runs in its own
//! (`BEGIN` ... `COMMIT`, `ROLLBACK` on failure). If the user opened one on this
//! session (`BEGIN` through the query path), the request joins it through a
//! `SAVEPOINT`: released on success, rolled back to on failure, and the user's
//! transaction is never committed or rolled back by the driver. If an undo
//! (`ROLLBACK` / `ROLLBACK TO SAVEPOINT`) fails, the session's state is unknown:
//! the connection is marked broken and later calls fail with `*.not_connected`.
//!
//! # Values are bound parameters
//!
//! Identifiers are quoted; every value is a bind parameter (`$1`, ...), never
//! interpolated, so no escaping is involved. The statement is prepared without
//! parameter types, so the server infers each from its column, and the value is
//! sent in the *text* format (`TextParam`) which the server parses with that
//! type's input function: this handles every column type (numeric, timestamps,
//! uuid, json, bytea `\x..`, arrays, enums, ...) without client-side type
//! tables. JSON `null` -> NULL; booleans, numbers and strings -> their text
//! (`true`, `1.5`, the string itself); objects and arrays -> JSON text. A `null`
//! key value matches with `IS NULL`.
//!
//! * **`bytea`** is the text `\x` + hex that a read returns, sent back as the
//!   string: the server's input function turns it into the same bytes (cell
//!   edits and keys alike; NUL, backslash and non-UTF-8 bytes included).
//! * **`timestamptz`** is the text a read returns (`2024-01-02 04:04:05.25+01`):
//!   its numeric offset pins the instant, whatever the session's TimeZone.
//! * **Exact decimals (`numeric`, `money`)** must arrive as JSON *strings* or JSON
//!   integers. A JSON number with a fraction or exponent has been parsed into a
//!   double before the driver sees it (this crate's `serde_json` has no
//!   `arbitrary_precision`), so its text can differ from what was typed in the
//!   last digit; when such a number carries more than 15 significant digits and
//!   the column is exact, the request fails with `mutation.invalid_value` and
//!   nothing is written. Up to 15 digits a double reads back exactly, so those are
//!   bound as given (`1.10` arrives as `1.1`: the same number).
//!
//! # `create_database`
//!
//! `database` = the new name; `object`, `identity` and `payload` must be unset
//! (null or empty), otherwise the request is `mutation.bad_request`.
//! Runs `CREATE DATABASE "<name>"` outside a transaction. Failure codes:
//! `mutation.create_database.rejected` (the server refused: bad name, exists,
//! no privilege) and `mutation.create_database.unknown` (the outcome is unknown:
//! connection lost or timed out). Open pages are kept.

use super::connection::{
    batch_execute_uninterrupted, session_scope, ArmedCancel, PgConnection, Scope,
};
use super::query::{cancelled, classify_error};
use super::runtime;
use crate::db::postgres::{v2_create_database_sql, v2_error_detail};
use crate::db::sql_builder::{quote_ident, quote_qualified_table};
use bytes::BytesMut;
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::sql::{SqlBatch, SqlRowChange, SqlRowOp, SqlTableIdentity};
use engine_driver_kit::api::{MutationRequest, OperationId};
use engine_driver_kit::{BackendError, MutationOutcome, OperationGuard};
use serde_json::{Map, Value};
use std::error::Error;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{to_sql_checked, Format, IsNull, ToSql, Type};
use tokio_postgres::Client;

const DEFAULT_SCHEMA: &str = "public";
const CREATE_DATABASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A value sent in the text format: the server parses it with the type it
/// inferred for the placeholder.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TextParam {
    text: Option<String>,
    /// A JSON number that went through a double and may have lost digits: an
    /// exact decimal column refuses it (see the module docs).
    maybe_rounded: bool,
}

impl ToSql for TextParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match &self.text {
            None => Ok(IsNull::Yes),
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

impl TextParam {
    fn null() -> Self {
        Self {
            text: None,
            maybe_rounded: false,
        }
    }

    fn exact(text: String) -> Self {
        Self {
            text: Some(text),
            maybe_rounded: false,
        }
    }

    fn from_json(value: &Value) -> Result<Self, BackendError> {
        Ok(match value {
            Value::Null => Self::null(),
            Value::String(text) if text.contains('\0') => {
                return Err(bad_request("mutation text cannot contain NUL"))
            }
            Value::String(text) => Self::exact(text.clone()),
            Value::Bool(flag) => Self::exact(flag.to_string()),
            Value::Number(number) => {
                let text = number.to_string();
                // Integers keep every digit; a double is exact only while its
                // shortest text has at most 15 significant digits.
                let maybe_rounded = number.is_f64() && significant_digits(&text) > 15;
                Self {
                    text: Some(text),
                    maybe_rounded,
                }
            }
            other => Self::exact(other.to_string()),
        })
    }

    /// The error for a possibly rounded number bound to `ty`, if `ty` is an exact
    /// decimal type.
    ///
    /// `place` names where the value is: the column, and the row of a joined multi-row insert.
    fn check_exactness(&self, ty: &Type, place: &str) -> Result<(), BackendError> {
        if self.maybe_rounded && is_exact_decimal(ty) {
            return Err(BackendError::with_code(
                codes::MUTATION_INVALID_VALUE,
                format!(
                    "the value for {place} is a JSON number with more digits than a double keeps, bound to a {} column; nothing was written. Send exact decimals as JSON strings holding the decimal text (\"0.30000000000000004\"), not as JSON numbers",
                    ty.name()
                ),
            ));
        }
        Ok(())
    }

    /// The text bytes the value takes in a statement (what a statement's size budget counts).
    fn bytes(&self) -> usize {
        self.text.as_ref().map_or(0, String::len) + PARAM_OVERHEAD
    }
}

/// Digits of `number` (a JSON number's text) from the first to the last nonzero
/// digit of its mantissa.
fn significant_digits(number: &str) -> usize {
    let mantissa = number.split(['e', 'E']).next().unwrap_or(number);
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let start = digits.iter().position(|digit| *digit != b'0');
    let end = digits.iter().rposition(|digit| *digit != b'0');
    match (start, end) {
        (Some(start), Some(end)) => end - start + 1,
        _ => 0,
    }
}

/// `numeric` and `money` (also through a domain) hold decimals a double cannot.
fn is_exact_decimal(ty: &Type) -> bool {
    match ty.kind() {
        tokio_postgres::types::Kind::Domain(base) => is_exact_decimal(base),
        _ => matches!(*ty, Type::NUMERIC | Type::MONEY),
    }
}

/// One prepared-statement-to-be: SQL text with `$n` placeholders and its values.
#[derive(Debug, PartialEq)]
pub(crate) struct Statement {
    pub(crate) sql: String,
    pub(crate) params: Vec<TextParam>,
    /// The column each parameter is written to or compared with (the same length as `params`), for messages.
    pub(crate) columns: Vec<String>,
    /// Whether the key must match exactly one row (update/delete).
    pub(crate) unique: bool,
    /// The rows an insert writes: 1, or the rows of a joined multi-row insert.
    pub(crate) rows: u64,
}

impl Statement {
    /// `column "x"`, or `column "x" of row 3` in a joined insert: where parameter `index` is.
    fn place_of(&self, index: usize) -> String {
        let column = self.columns.get(index).map_or_else(|| format!("parameter {}", index + 1), |column| format!("column \"{column}\""));
        match self.rows {
            0 | 1 => column,
            rows => format!("{column} of row {}", index / (self.params.len() / rows as usize).max(1) + 1),
        }
    }

    /// Whether `affected` rows is what the statement had to write. A joined insert must write every row it carries
    /// (a rule that does nothing, or a trigger that returns NULL, would otherwise lose rows silently); an update or
    /// a delete must touch exactly one.
    fn check_affected(&self, affected: u64) -> Result<(), BackendError> {
        if affected == 0 {
            return Err(BackendError::with_code(
                codes::MUTATION_NO_ROWS_AFFECTED,
                "SQL mutation affected no rows; transaction rolled back",
            ));
        }
        // A joined insert must write exactly the rows it carries: fewer means a rule or trigger swallowed some, more
        // cannot be told apart from a rule that wrote elsewhere. A single-row insert keeps its old rule (any count
        // from one up), as before joining existed.
        if self.rows > 1 {
            return if affected == self.rows {
                Ok(())
            } else {
                Err(BackendError::with_code(
                    codes::MUTATION_NO_ROWS_AFFECTED,
                    format!("SQL mutation wrote {affected} of {} rows; transaction rolled back", self.rows),
                ))
            };
        }
        if affected > 1 && self.unique {
            return Err(BackendError::with_code(
                codes::MUTATION_IDENTITY_NOT_UNIQUE,
                format!("SQL mutation key matched {affected} rows; transaction rolled back"),
            ));
        }
        Ok(())
    }
}

pub(crate) fn mutate(
    connection: &PgConnection,
    request: &MutationRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<MutationOutcome, BackendError> {
    if request.kind == "create_database" {
        return create_database(connection, request, operation);
    }
    let statements = join_inserts(build_statements(request)?);
    let database = connection
        .database_or_default(request.database.as_deref())
        .to_owned();
    let operation_id = operation.operation_id();
    let active = operation.activate(connection.lock_gate("mutation")?)?;
    let cancel_requested = || active.cancel_requested();
    let affected_rows = runtime::block_on(run_transaction(
        connection,
        operation_id,
        &database,
        &statements,
        &cancel_requested,
    ))
    .map_err(BackendError::new)??;
    drop(active);
    Ok(MutationOutcome::new(affected_rows))
}

/// The classified failure of one statement inside the transaction.
fn statement_error(
    error: &tokio_postgres::Error,
    cancel_requested: &dyn Fn() -> bool,
) -> BackendError {
    let mapped = classify_error(error, cancel_requested(), "mutation");
    if mapped.code.is_some() {
        mapped
    } else {
        BackendError::with_code(
            codes::MUTATION_FAILED,
            format!("SQL mutation failed and rolled back: {}", mapped.message),
        )
    }
}

const SAVEPOINT: &str = "tablex_mutation";

async fn run_transaction(
    connection: &PgConnection,
    operation: OperationId,
    database: &str,
    statements: &[Statement],
    cancel_requested: &dyn Fn() -> bool,
) -> Result<u64, BackendError> {
    let client = connection
        .inner
        .v2_client(database)
        .await
        .map_err(BackendError::new)?;
    connection.restore_settings(&client, database).await;
    let armed = connection.cancel.arm(operation, client.cancel_token());
    if cancel_requested() {
        return Err(cancelled("mutation"));
    }
    // What closes the transaction, once `begin` was sent; `begun` = it succeeded.
    let mut open: Option<(String, String)> = None;
    let (mut begun, mut own) = (false, false);
    let work = async {
        let scope = session_scope(&client)
            .await
            .map_err(|error| statement_error(&error, cancel_requested))?;
        armed.check("mutation")?;
        let (begin, commit, abort) = match scope {
            Scope::Own => (
                "BEGIN".to_owned(),
                "COMMIT".to_owned(),
                "ROLLBACK".to_owned(),
            ),
            Scope::Savepoint => (
                format!("SAVEPOINT {SAVEPOINT}"),
                format!("RELEASE SAVEPOINT {SAVEPOINT}"),
                // Undo the mutation, then forget the savepoint; the user's own
                // transaction stays open.
                format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}; RELEASE SAVEPOINT {SAVEPOINT}"),
            ),
        };
        // From here the transaction may exist on the server, even if `begin` fails.
        open = Some((commit, abort));
        own = scope == Scope::Own;
        if let Err(error) = client.batch_execute(&begin).await {
            return Err(if error.code() == Some(&SqlState::QUERY_CANCELED) {
                classify_error(&error, cancel_requested(), "mutation")
            } else {
                BackendError::new(format!(
                    "mutation transaction failed to begin: {}",
                    v2_error_detail(&error)
                ))
            });
        }
        begun = true;
        armed.check("mutation")?;
        apply(&client, &armed, statements, cancel_requested).await
    };
    let applied = armed.watch(work).await;
    // The cancellable part is over: from here a cancel cannot interrupt the
    // undo or the commit (see `ArmedCancel::disarm`).
    armed.disarm();
    let Some((commit, abort)) = open else {
        return applied;
    };
    match applied {
        // A cancel that arrived during the last statement, which still
        // completed: nothing is committed, and no COMMIT is sent with a cancel
        // request possibly still on its way.
        Ok(_) if cancel_requested() || armed.requested() => {
            undo(connection, &client, &abort, true).await;
            Err(cancelled("mutation"))
        }
        Ok(affected) => match client.batch_execute(&commit).await {
            Ok(()) => Ok(affected),
            Err(error) => {
                undo(connection, &client, &abort, true).await;
                Err(if own && commit_outcome_unknown(&error) {
                    // An interrupted COMMIT may still have been applied: the
                    // server raises a pending cancel at its next interrupt check,
                    // which can come after the commit record was written.
                    commit_unknown(&error)
                } else if error.code() == Some(&SqlState::QUERY_CANCELED) {
                    classify_error(&error, cancel_requested(), "mutation")
                } else {
                    BackendError::with_code(
                        codes::MUTATION_FAILED,
                        format!("SQL mutation commit failed: {}", v2_error_detail(&error)),
                    )
                })
            }
        },
        Err(error) => {
            undo(connection, &client, &abort, begun || own).await;
            Err(error)
        }
    }
}

/// `mutation.commit_unknown`: the COMMIT did not answer with a success, but it
/// may have been applied. Not retryable: repeating the change could apply it twice.
/// Whether a failed `COMMIT` may nevertheless have committed: it was interrupted
/// (SQLSTATE 57014) or the connection broke before the server answered.
fn commit_outcome_unknown(error: &tokio_postgres::Error) -> bool {
    error.code() == Some(&SqlState::QUERY_CANCELED) || error.as_db_error().is_none()
}

fn commit_unknown(error: &tokio_postgres::Error) -> BackendError {
    BackendError::with_code(
        codes::MUTATION_COMMIT_UNKNOWN,
        format!(
            "the COMMIT was interrupted ({}); the change may or may not have been applied: reload the data and check before repeating it",
            v2_error_detail(error)
        ),
    )
}

/// Undoes the mutation. A late cancel cannot break it (it is tried again). A
/// failed undo leaves the session in an unknown transaction state, so the
/// connection is marked broken -- unless `must_undo` is false: the savepoint
/// was never opened, so the user's own (aborted) transaction is theirs.
async fn undo(connection: &PgConnection, client: &Client, abort: &str, must_undo: bool) {
    if batch_execute_uninterrupted(client, abort).await.is_err() && must_undo {
        connection.mark_broken();
    }
}

async fn apply(
    client: &Client,
    armed: &ArmedCancel,
    statements: &[Statement],
    cancel_requested: &dyn Fn() -> bool,
) -> Result<u64, BackendError> {
    let mut total = 0u64;
    // The full chunks of a joined insert share one statement text: prepared once.
    let mut last: Option<(&str, tokio_postgres::Statement)> = None;
    for statement in statements {
        armed.check("mutation")?;
        let prepared = match &last {
            Some((sql, prepared)) if *sql == statement.sql => prepared.clone(),
            _ => {
                let prepared = client
                    .prepare(&statement.sql)
                    .await
                    .map_err(|error| statement_error(&error, cancel_requested))?;
                last = Some((&statement.sql, prepared.clone()));
                prepared
            }
        };
        let params: Vec<&(dyn ToSql + Sync)> = statement
            .params
            .iter()
            .map(|param| param as &(dyn ToSql + Sync))
            .collect();
        for (index, (param, ty)) in statement.params.iter().zip(prepared.params()).enumerate() {
            param.check_exactness(ty, &statement.place_of(index))?;
        }
        armed.check("mutation")?;
        let affected = client
            .execute(&prepared, &params)
            .await
            .map_err(|error| statement_error(&error, cancel_requested))?;
        statement.check_affected(affected)?;
        total = total.saturating_add(affected);
    }
    Ok(total)
}

fn create_database(
    connection: &PgConnection,
    request: &MutationRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<MutationOutcome, BackendError> {
    let reject =
        |message: String| BackendError::with_code(codes::CREATE_DATABASE_REJECTED, message);
    // `create_database` names the database and nothing else: any other content
    // is a malformed request, not a server refusal.
    let blank = |value: &Value| match value {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if request
        .object
        .as_deref()
        .is_some_and(|object| !object.is_empty())
        || !blank(&request.identity)
        || !blank(&request.payload)
    {
        return Err(bad_request(
            "create_database takes only a database name (no object, identity or payload)",
        ));
    }
    let name = request.database.as_deref().unwrap_or_default();
    let statement = v2_create_database_sql(name).map_err(&reject)?;
    let operation_id = operation.operation_id();
    let active = operation.activate(connection.lock_gate("mutation")?)?;
    let cancel_requested = || active.cancel_requested();
    runtime::block_on(async {
        let client = connection.inner.v2_client("").await.map_err(unknown)?;
        let armed = connection.cancel.arm(operation_id, client.cancel_token());
        if cancel_requested() {
            return Err(cancelled("mutation"));
        }
        // PostgreSQL rejects CREATE DATABASE inside a transaction block: run it
        // alone, outside BEGIN/COMMIT.
        let outcome = armed
            .watch(tokio::time::timeout(
                CREATE_DATABASE_TIMEOUT,
                client.batch_execute(&statement),
            ))
            .await;
        armed.disarm();
        match outcome {
            Err(_) => Err(unknown(
                "Create database timed out; server outcome is unknown".into(),
            )),
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) if error.as_db_error().is_some() => {
                if error.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
                    && cancel_requested()
                {
                    Err(cancelled("mutation"))
                } else {
                    Err(reject(v2_error_detail(&error)))
                }
            }
            Ok(Err(error)) => Err(unknown(error.to_string())),
        }
    })
    .map_err(BackendError::new)??;
    drop(active);
    // A new database cannot affect any open result page.
    Ok(MutationOutcome::new(0).keep_pages())
}

fn unknown(message: String) -> BackendError {
    BackendError::with_code("mutation.create_database.unknown", message)
}

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::with_code("mutation.bad_request", message)
}

/// The statements of a row-mutation request, in order (see the module docs).
pub(crate) fn build_statements(request: &MutationRequest) -> Result<Vec<Statement>, BackendError> {
    let table = request
        .object
        .as_deref()
        .filter(|table| !table.is_empty())
        .ok_or_else(|| bad_request("mutation needs `object` (the table name)"))?;
    let identity: SqlTableIdentity = match &request.identity {
        Value::Null => SqlTableIdentity::default(),
        other => serde_json::from_value(other.clone())
            .map_err(|error| bad_request(format!("invalid mutation identity: {error}")))?,
    };
    let schema = identity
        .schema
        .as_deref()
        .filter(|schema| !schema.is_empty())
        .unwrap_or(DEFAULT_SCHEMA);
    check_identifier(schema, "schema")?;
    check_identifier(table, "table")?;
    let target = quote_qualified_table(schema, table);

    let changes: Vec<SqlRowChange> = match request.kind.as_str() {
        "batch" => {
            let batch: SqlBatch = serde_json::from_value(request.payload.clone())
                .map_err(|error| bad_request(format!("invalid batch payload: {error}")))?;
            if batch.changes.is_empty() {
                return Err(bad_request("batch has no changes"));
            }
            batch.changes
        }
        "insert" | "update" | "delete" => {
            let change: SqlRowChange = serde_json::from_value(request.payload.clone())
                .map_err(|error| bad_request(format!("invalid row change payload: {error}")))?;
            if change.op.kind() != request.kind {
                return Err(bad_request(format!(
                    "payload op `{}` does not match kind `{}`",
                    change.op.kind(),
                    request.kind
                )));
            }
            vec![change]
        }
        other => {
            return Err(BackendError::with_code(
                codes::MUTATION_UNSUPPORTED,
                format!("unsupported mutation kind `{other}`"),
            ))
        }
    };
    changes
        .iter()
        .map(|change| build_statement(&target, change))
        .collect()
}

fn build_statement(target: &str, change: &SqlRowChange) -> Result<Statement, BackendError> {
    let mut params = Vec::new();
    let mut columns_of_params: Vec<String> = Vec::new();
    let mut bind = |column: &str, value: &Value| -> Result<String, BackendError> {
        params.push(TextParam::from_json(value)?);
        columns_of_params.push(column.to_owned());
        Ok(format!("${}", params.len()))
    };
    let sql = match change.op {
        SqlRowOp::Insert if change.values.is_empty() => {
            format!("INSERT INTO {target} DEFAULT VALUES")
        }
        SqlRowOp::Insert => {
            let (columns, placeholders) = names_and_placeholders(&change.values, &mut bind)?;
            format!("INSERT INTO {target} ({columns}) VALUES ({placeholders})")
        }
        SqlRowOp::Update => {
            if change.values.is_empty() {
                return Err(bad_request("update needs a non-empty `values`"));
            }
            let mut assignments = Vec::with_capacity(change.values.len());
            for (column, value) in &change.values {
                check_identifier(column, "column")?;
                assignments.push(format!("{} = {}", quote_ident(column), bind(column, value)?));
            }
            format!(
                "UPDATE {target} SET {} WHERE {}",
                assignments.join(", "),
                key_condition(&change.key, &mut bind)?
            )
        }
        SqlRowOp::Delete => format!(
            "DELETE FROM {target} WHERE {}",
            key_condition(&change.key, &mut bind)?
        ),
    };
    Ok(Statement {
        sql,
        params,
        columns: columns_of_params,
        unique: change.op != SqlRowOp::Insert,
        rows: 1,
    })
}

/// The most rows one joined `INSERT … VALUES (…), (…)` carries.
const MAX_ROWS_PER_INSERT: usize = 1000;
/// What a bound value costs besides its text: its length prefix and type in the protocol.
const PARAM_OVERHEAD: usize = 8;
/// The most text one joined insert carries: the statement is one message to the server, and the driver holds it
/// (and its copy) whole.
const MAX_INSERT_BYTES: usize = 16 * 1024 * 1024;
/// The protocol numbers bind parameters with 16 bits.
const MAX_BIND_PARAMS: usize = 65_535;

/// `(the SQL up to the first tuple, the parameters of one row)` of a single-row insert with values.
fn insert_shape(statement: &Statement) -> Option<(&str, usize)> {
    if statement.unique || statement.rows != 1 || statement.params.is_empty() || !statement.sql.starts_with("INSERT INTO ") {
        return None;
    }
    let at = statement.sql.rfind(") VALUES (")? + ") VALUES ".len();
    Some((&statement.sql[..at], statement.params.len()))
}

/// Joins each run of single-row inserts that write the same columns into multi-row inserts of at most
/// [`MAX_ROWS_PER_INSERT`] rows and [`MAX_BIND_PARAMS`] parameters: one statement and one round trip for
/// a chunk of rows instead of two per row. Everything else (updates, deletes, an insert of other columns or
/// of `DEFAULT VALUES`) keeps its own statement and its place.
pub(crate) fn join_inserts(statements: Vec<Statement>) -> Vec<Statement> {
    struct Run {
        prefix: String,
        width: usize,
        rows: usize,
        bytes: usize,
        params: Vec<TextParam>,
        columns: Vec<String>,
    }
    impl Run {
        fn finish(self) -> Statement {
            use std::fmt::Write as _;
            let mut sql = self.prefix;
            let mut next = 1;
            for row in 0..self.rows {
                sql.push_str(if row == 0 { "(" } else { ", (" });
                for column in 0..self.width {
                    if column > 0 {
                        sql.push_str(", ");
                    }
                    let _ = write!(sql, "${next}");
                    next += 1;
                }
                sql.push(')');
            }
            Statement { sql, params: self.params, columns: self.columns, unique: false, rows: self.rows as u64 }
        }
    }
    let mut out = Vec::with_capacity(statements.len().min(64));
    let mut run: Option<Run> = None;
    for mut statement in statements {
        let Some((prefix, width)) = insert_shape(&statement) else {
            out.extend(run.take().map(Run::finish));
            out.push(statement);
            continue;
        };
        let bytes: usize = statement.params.iter().map(TextParam::bytes).sum();
        let fits = |run: &Run| {
            run.prefix == prefix && run.rows < MAX_ROWS_PER_INSERT && (run.rows + 1) * width <= MAX_BIND_PARAMS && run.bytes + bytes <= MAX_INSERT_BYTES
        };
        if run.as_ref().is_some_and(|run| !fits(run)) {
            out.extend(run.take().map(Run::finish));
        }
        let run = run.get_or_insert_with(|| Run { prefix: prefix.to_owned(), width, rows: 0, bytes: 0, params: Vec::new(), columns: Vec::new() });
        run.rows += 1;
        run.bytes += bytes;
        run.params.append(&mut statement.params);
        run.columns.append(&mut statement.columns);
    }
    out.extend(run.map(Run::finish));
    out
}

fn names_and_placeholders(
    values: &Map<String, Value>,
    bind: &mut impl FnMut(&str, &Value) -> Result<String, BackendError>,
) -> Result<(String, String), BackendError> {
    let mut columns = Vec::with_capacity(values.len());
    let mut placeholders = Vec::with_capacity(values.len());
    for (column, value) in values {
        check_identifier(column, "column")?;
        columns.push(quote_ident(column));
        placeholders.push(bind(column, value)?);
    }
    Ok((columns.join(", "), placeholders.join(", ")))
}

/// `"a" = $1 AND "b" IS NULL`: every key column must match.
fn key_condition(
    key: &Map<String, Value>,
    bind: &mut impl FnMut(&str, &Value) -> Result<String, BackendError>,
) -> Result<String, BackendError> {
    if key.is_empty() {
        return Err(bad_request("update and delete need a non-empty `key`"));
    }
    let mut conditions = Vec::with_capacity(key.len());
    for (column, value) in key {
        check_identifier(column, "key column")?;
        conditions.push(if value.is_null() {
            format!("{} IS NULL", quote_ident(column))
        } else {
            format!("{} = {}", quote_ident(column), bind(column, value)?)
        });
    }
    Ok(conditions.join(" AND "))
}

fn check_identifier(name: &str, what: &str) -> Result<(), BackendError> {
    if name.is_empty() || name.contains('\0') {
        return Err(bad_request(format!(
            "{what} name cannot be empty or contain NUL"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_kit::api::OperationId;
    use serde_json::json;

    fn request(kind: &str, identity: Value, payload: Value) -> MutationRequest {
        MutationRequest {
            operation_id: OperationId(1),
            kind: kind.into(),
            database: Some("app".into()),
            object: Some("orders".into()),
            identity,
            payload,
        }
    }

    fn param(text: &str) -> TextParam {
        TextParam::exact(text.into())
    }

    #[test]
    fn insert_update_delete_bind_every_value() {
        let identity = json!({"schema": "sales"});
        let insert = build_statements(&request(
            "insert",
            identity.clone(),
            json!({"op": "insert", "values": {"name": "it's \\ ok", "qty": 3, "paid": true, "note": null}}),
        ))
        .unwrap();
        assert_eq!(
            insert,
            [Statement {
                sql: r#"INSERT INTO "sales"."orders" ("name", "note", "paid", "qty") VALUES ($1, $2, $3, $4)"#.into(),
                params: vec![param("it's \\ ok"), TextParam::null(), param("true"), param("3")],
                columns: ["name", "note", "paid", "qty"].map(String::from).to_vec(),
                unique: false,
                rows: 1,
            }]
        );
        let update = build_statements(&request(
            "update",
            identity.clone(),
            json!({"op": "update", "key": {"id": 42}, "values": {"status": "shipped"}}),
        ))
        .unwrap();
        assert_eq!(
            update,
            [Statement {
                sql: r#"UPDATE "sales"."orders" SET "status" = $1 WHERE "id" = $2"#.into(),
                params: vec![param("shipped"), param("42")],
                columns: ["status", "id"].map(String::from).to_vec(),
                unique: true,
                rows: 1,
            }]
        );
        let delete = build_statements(&request(
            "delete",
            identity,
            json!({"op": "delete", "key": {"id": "a'b"}}),
        ))
        .unwrap();
        assert_eq!(
            delete[0].sql,
            r#"DELETE FROM "sales"."orders" WHERE "id" = $1"#
        );
        assert_eq!(delete[0].params, [param("a'b")]);
    }

    #[test]
    fn composite_and_null_keys() {
        let statements = build_statements(&request(
            "delete",
            Value::Null,
            json!({"op": "delete", "key": {"tenant": 1, "id": 7, "gone": null}}),
        ))
        .unwrap();
        assert_eq!(
            statements[0].sql,
            r#"DELETE FROM "public"."orders" WHERE "gone" IS NULL AND "id" = $1 AND "tenant" = $2"#
        );
        assert_eq!(statements[0].params, [param("7"), param("1")]);
    }

    #[test]
    fn schema_defaults_to_public_and_empty_insert_uses_default_values() {
        for payload in [
            json!({"op": "insert"}),
            json!({"op": "insert", "values": {}}),
        ] {
            let statements = build_statements(&request("insert", Value::Null, payload)).unwrap();
            assert_eq!(
                statements[0].sql,
                r#"INSERT INTO "public"."orders" DEFAULT VALUES"#
            );
            assert!(statements[0].params.is_empty());
        }
    }

    #[test]
    fn batches_keep_order_and_mix_ops() {
        let statements = build_statements(&request(
            "batch",
            json!({"schema": "s"}),
            json!({"changes": [
                {"op": "update", "key": {"id": 1}, "values": {"a": 1}},
                {"op": "insert", "values": {"a": {"k": [1]}}},
                {"op": "delete", "key": {"id": 3}}
            ]}),
        ))
        .unwrap();
        assert_eq!(statements.len(), 3);
        assert!(statements[0].sql.starts_with("UPDATE"));
        assert_eq!(statements[1].params, [param(r#"{"k":[1]}"#)]);
        assert!(statements[2].sql.starts_with("DELETE"));
        // Placeholders restart per statement.
        assert!(statements[2].sql.ends_with("= $1"));
    }

    #[test]
    fn identifiers_are_quoted_not_spliced() {
        let mut req = request(
            "update",
            json!({"schema": "s\"x"}),
            json!({"op": "update", "key": {"k\"c": 1}, "values": {"c\"d": 1}}),
        );
        req.object = Some("we\"ird".into());
        let statements = build_statements(&req).unwrap();
        assert_eq!(
            statements[0].sql,
            r#"UPDATE "s""x"."we""ird" SET "c""d" = $1 WHERE "k""c" = $2"#
        );
    }

    #[test]
    fn malformed_requests_are_rejected_with_a_specific_code() {
        let code = |request: MutationRequest| {
            build_statements(&request)
                .expect_err("rejected")
                .code
                .map(|code| code.into_owned())
        };
        let bad = Some("mutation.bad_request".to_owned());
        let update = |payload: Value| request("update", Value::Null, payload);
        // missing/empty key or values
        assert_eq!(
            code(update(json!({"op": "update", "values": {"a": 1}}))),
            bad
        );
        assert_eq!(code(update(json!({"op": "update", "key": {"id": 1}}))), bad);
        assert_eq!(
            code(request("delete", Value::Null, json!({"op": "delete"}))),
            bad
        );
        // op/kind mismatch, missing op, unknown kind, wrong payload shapes
        assert_eq!(
            code(request(
                "delete",
                Value::Null,
                json!({"op": "update", "key": {"id": 1}, "values": {"a": 1}})
            )),
            bad
        );
        assert_eq!(code(request("insert", Value::Null, Value::Null)), bad);
        assert_eq!(
            code(request("insert", Value::Null, json!([{"op": "insert"}]))),
            bad
        );
        assert_eq!(
            code(request("drop_table", Value::Null, Value::Null)),
            Some("mutation.unsupported".to_owned())
        );
        assert_eq!(
            code(request("batch", Value::Null, json!({"changes": []}))),
            bad
        );
        assert_eq!(
            code(request("batch", Value::Null, json!({"op": "insert"}))),
            bad
        );
        // identity / object problems
        assert_eq!(
            code(request("insert", json!([1]), json!({"op": "insert"}))),
            bad
        );
        assert_eq!(
            code(MutationRequest {
                object: None,
                ..request("insert", Value::Null, json!({"op": "insert"}))
            }),
            bad
        );
        assert_eq!(
            code(request(
                "insert",
                Value::Null,
                json!({"op": "insert", "values": {"a\u{0}": 1}})
            )),
            bad
        );
        assert_eq!(
            code(request(
                "insert",
                Value::Null,
                json!({"op": "insert", "values": {"a": "x\u{0}y"}})
            )),
            bad
        );
    }

    #[test]
    fn json_numbers_that_a_double_may_have_rounded_are_marked() {
        let rounded = |value: Value| TextParam::from_json(&value).unwrap().maybe_rounded;
        // Integers (i64 and u64) are exact whatever their size.
        assert!(!rounded(json!(9_007_199_254_740_993_i64)));
        assert!(!rounded(json!(18_446_744_073_709_551_615_u64)));
        // Doubles that read back from at most 15 digits are exact decimals.
        for exact in [0.1, 12.5, -3.25, 123456789012345.0, 1.5e-7, 1e21] {
            assert!(!rounded(json!(exact)), "{exact}");
        }
        // 0.30000000000000004 needs 17 digits: the typed text is unknowable.
        assert!(rounded(json!(0.1 + 0.2)));
        assert!(rounded(json!(1.2345678901234567e25)));
        // Strings, booleans and structures are never marked.
        assert!(!rounded(json!("0.30000000000000004")));
        assert_eq!(significant_digits("0.00120"), 2);
        assert_eq!(significant_digits("-1.5e-7"), 2);
        assert_eq!(significant_digits("0"), 0);
    }

    #[test]
    fn a_rounded_number_is_refused_only_by_exact_decimal_columns() {
        let rounded = TextParam::from_json(&json!(0.1 + 0.2)).unwrap();
        for ty in [Type::NUMERIC, Type::MONEY] {
            let error = rounded.check_exactness(&ty, "column \"a\"").unwrap_err();
            assert_eq!(error.code.as_deref(), Some("mutation.invalid_value"));
        }
        for ty in [Type::FLOAT8, Type::FLOAT4, Type::TEXT, Type::INT8] {
            assert!(rounded.check_exactness(&ty, "column \"a\"").is_ok(), "{ty}");
        }
        assert!(param("0.30000000000000004")
            .check_exactness(&Type::NUMERIC, "column \"a\"")
            .is_ok());
    }

    #[test]
    fn bytea_and_timestamptz_text_pass_through_untouched() {
        // What a read returns is bound as is: backslash, NUL-free hex, offsets.
        for text in [
            "\\x005cfffe80",
            "\\x",
            "2024-01-02 04:04:05.25+01",
            "0044-03-15 12:00:00+00:53:28 BC",
        ] {
            assert_eq!(TextParam::from_json(&json!(text)).unwrap(), param(text));
        }
    }

    #[test]
    fn text_params_encode_as_text_format() {
        assert!(matches!(
            param("1").encode_format(&Type::INT4),
            Format::Text
        ));
        let mut out = BytesMut::new();
        assert!(matches!(
            param("héllo").to_sql(&Type::TEXT, &mut out).unwrap(),
            IsNull::No
        ));
        assert_eq!(&out[..], "héllo".as_bytes());
        assert!(matches!(
            TextParam::null().to_sql(&Type::TEXT, &mut out).unwrap(),
            IsNull::Yes
        ));
    }

    /// Needs a server: `TABLEX_TEST_PG_URI`. A cancel that arrived while the last
    /// statement ran (it still completed) must not be followed by a COMMIT. The
    /// request is made visible through the closure the kit's flag is read by:
    /// it says "not cancelled" at the start of the call and "cancelled" from then
    /// on, which is what the decision after the statements sees.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
    fn a_cancel_requested_during_the_last_statement_rolls_back_instead_of_committing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let uri = std::env::var("TABLEX_TEST_PG_URI")
            .expect("set TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)");
        let session = runtime::block_on(crate::db::postgres::PostgresConn::v2_connect(&uri))
            .unwrap()
            .unwrap_or_else(|error| panic!("{}", error.message));
        let connection = PgConnection::new(session);
        let table = format!("p1_a6_{}", std::process::id());
        let rows = |sql: &str| {
            runtime::block_on(async {
                let client = connection.inner.v2_client("").await.unwrap();
                client.batch_execute(sql).await.unwrap();
            })
            .unwrap()
        };
        let count = || -> i64 {
            runtime::block_on(async {
                let client = connection.inner.v2_client("").await.unwrap();
                client
                    .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                    .await
                    .unwrap()
                    .get(0)
            })
            .unwrap()
        };
        rows(&format!("CREATE TABLE {table} (id int PRIMARY KEY)"));
        let statements = build_statements(&MutationRequest {
            operation_id: OperationId(1),
            kind: "insert".into(),
            database: None,
            object: Some(table.clone()),
            identity: json!({"schema": "public"}),
            payload: json!({"op": "insert", "values": {"id": 1}}),
        })
        .unwrap();
        let run = |operation: u64, cancel_from_call: usize| {
            let calls = AtomicUsize::new(0);
            let cancel_requested = || calls.fetch_add(1, Ordering::SeqCst) >= cancel_from_call;
            runtime::block_on(run_transaction(
                &connection,
                OperationId(operation),
                "",
                &statements,
                &cancel_requested,
            ))
            .unwrap()
        };
        // Cancelled only after the statement completed: rolled back, not committed.
        let error = run(1, 1).expect_err("cancelled");
        assert_eq!(error.code.as_deref(), Some("mutation.cancelled"), "{error}");
        assert_eq!(count(), 0, "the mutation was committed");
        // The same call without the request commits (the test can see the difference).
        assert_eq!(run(2, usize::MAX).expect("committed"), 1);
        assert_eq!(count(), 1);
        rows(&format!("DROP TABLE {table}"));
    }

    fn inserts(count: usize, columns: &[&str]) -> Vec<Statement> {
        (0..count)
            .map(|row| {
                let values: serde_json::Map<String, Value> = columns.iter().map(|column| ((*column).to_owned(), json!(row))).collect();
                build_statements(&request("insert", Value::Null, json!({"op": "insert", "values": values}))).unwrap().remove(0)
            })
            .collect()
    }

    #[test]
    fn a_run_of_inserts_becomes_one_multi_row_statement_with_continuing_placeholders() {
        let joined = join_inserts(inserts(3, &["a", "b"]));
        assert_eq!(joined.len(), 1);
        assert_eq!(
            joined[0].sql,
            r#"INSERT INTO "public"."orders" ("a", "b") VALUES ($1, $2), ($3, $4), ($5, $6)"#
        );
        assert_eq!((joined[0].rows, joined[0].params.len(), joined[0].unique), (3, 6, false));
        // A run of one row is the statement it was.
        let one = join_inserts(inserts(1, &["a"]));
        assert_eq!(one[0].sql, r#"INSERT INTO "public"."orders" ("a") VALUES ($1)"#);
    }

    #[test]
    fn chunks_stop_at_the_row_cap_and_the_parameter_limit_and_full_chunks_share_their_text() {
        let sizes: Vec<u64> = join_inserts(inserts(2500, &["id"])).iter().map(|statement| statement.rows).collect();
        assert_eq!(sizes, [1000, 1000, 500]);
        let chunks = join_inserts(inserts(2500, &["id"]));
        assert_eq!(chunks[0].sql, chunks[1].sql, "full chunks are the same statement");
        assert!(chunks[0].sql.ends_with("($999), ($1000)"), "{}", &chunks[0].sql[chunks[0].sql.len() - 20..]);
        // 70 columns: 65535 / 70 = 936 rows fit.
        let columns: Vec<String> = (0..70).map(|column| format!("c{column}")).collect();
        let refs: Vec<&str> = columns.iter().map(String::as_str).collect();
        let wide = join_inserts(inserts(1000, &refs));
        assert_eq!(wide.iter().map(|statement| statement.rows).collect::<Vec<_>>(), [936, 64]);
        assert!(wide.iter().all(|statement| statement.params.len() <= MAX_BIND_PARAMS));
    }

    #[test]
    fn other_changes_and_other_columns_break_a_run_and_keep_their_order() {
        let mut statements = inserts(2, &["a"]);
        statements.extend(build_statements(&request("update", Value::Null, json!({"op": "update", "key": {"id": 1}, "values": {"a": 2}}))).unwrap());
        statements.extend(inserts(2, &["b"]));
        statements.extend(build_statements(&request("insert", Value::Null, json!({"op": "insert", "values": {}}))).unwrap());
        statements.extend(inserts(1, &["a"]));
        let joined = join_inserts(statements);
        let shapes: Vec<(&str, u64)> = joined.iter().map(|statement| (statement.sql.split_whitespace().next().unwrap(), statement.rows)).collect();
        assert_eq!(shapes, [("INSERT", 2), ("UPDATE", 1), ("INSERT", 2), ("INSERT", 1), ("INSERT", 1)]);
        assert!(joined[3].sql.ends_with("DEFAULT VALUES"));
        assert!(joined[2].sql.contains(r#"("b") VALUES ($1), ($2)"#));
        assert!(joined[1].unique);
    }

    #[test]
    fn a_joined_insert_must_write_every_row_it_carries_even_when_the_server_reports_one() {
        let joined = join_inserts(inserts(3, &["a"]));
        let statement = &joined[0];
        assert!(statement.check_affected(3).is_ok());
        // A rule that does nothing, or a BEFORE trigger that returns NULL, writes fewer rows: refused, not accepted.
        for wrote in [1, 2] {
            let error = statement.check_affected(wrote).unwrap_err();
            assert_eq!(error.code.as_deref(), Some("mutation.no_rows_affected"), "{wrote}");
            assert!(error.message.contains(&format!("wrote {wrote} of 3 rows")), "{}", error.message);
        }
        assert_eq!(statement.check_affected(0).unwrap_err().code.as_deref(), Some("mutation.no_rows_affected"));
        // The single-row rules are as they were.
        let single = &join_inserts(inserts(1, &["a"]))[0];
        assert!(single.check_affected(1).is_ok());
        assert!(single.check_affected(0).is_err());
        let update = &build_statements(&request("update", Value::Null, json!({"op": "update", "key": {"id": 1}, "values": {"a": 2}}))).unwrap()[0];
        assert!(update.check_affected(1).is_ok());
        assert_eq!(update.check_affected(2).unwrap_err().code.as_deref(), Some("mutation.identity_not_unique"));
    }

    #[test]
    fn a_chunk_stops_at_the_byte_budget_and_one_oversized_row_goes_alone() {
        let big = |text: String| {
            let values: serde_json::Map<String, Value> = [("t".to_owned(), json!(text))].into_iter().collect();
            build_statements(&request("insert", Value::Null, json!({"op": "insert", "values": values}))).unwrap().remove(0)
        };
        let half = "x".repeat(MAX_INSERT_BYTES / 2 - PARAM_OVERHEAD);
        // Two rows that fill the budget exactly share a statement; a third starts the next.
        let sizes: Vec<u64> = join_inserts(vec![big(half.clone()), big(half.clone()), big(half.clone())]).iter().map(|statement| statement.rows).collect();
        assert_eq!(sizes, [2, 1]);
        // One byte more and the second row no longer fits.
        let over = "x".repeat(MAX_INSERT_BYTES / 2 - PARAM_OVERHEAD + 1);
        let sizes: Vec<u64> = join_inserts(vec![big(over.clone()), big(over.clone())]).iter().map(|statement| statement.rows).collect();
        assert_eq!(sizes, [1, 1]);
        // A row over the whole budget is still sent (alone).
        let huge = "x".repeat(MAX_INSERT_BYTES + 1);
        let sizes: Vec<u64> = join_inserts(vec![big("a".into()), big(huge), big("b".into())]).iter().map(|statement| statement.rows).collect();
        assert_eq!(sizes, [1, 1, 1]);
    }

    #[test]
    fn an_inexact_number_names_its_column_and_the_row_of_a_joined_insert() {
        let mut statements = inserts(2, &["a", "price"]);
        statements[1].params[1] = TextParam::from_json(&json!(0.1 + 0.2)).unwrap();
        let joined = join_inserts(statements);
        let statement = &joined[0];
        assert_eq!(statement.columns, ["a", "price", "a", "price"]);
        assert_eq!(statement.place_of(1), "column \"price\" of row 1");
        assert_eq!(statement.place_of(3), "column \"price\" of row 2");
        let error = statement.params[3].check_exactness(&Type::NUMERIC, &statement.place_of(3)).unwrap_err();
        assert!(error.message.contains("column \"price\" of row 2"), "{}", error.message);
        // A single statement names only the column.
        assert_eq!(join_inserts(inserts(1, &["a"]))[0].place_of(0), "column \"a\"");
    }
}
