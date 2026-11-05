//! Mutations under the shared SQL contract (`engine_driver_api::sql`): row
//! changes by key, and `create_database`.
//!
//! | kind              | database | object | identity                    | payload                          |
//! |-------------------|----------|--------|-----------------------------|----------------------------------|
//! | `insert`          | database | table  | `SqlTableIdentity` (ignored) | one `SqlRowChange`, `op: insert` |
//! | `update`/`delete` | database | table  | same                        | one `SqlRowChange` with a `key`  |
//! | `batch`           | database | table  | same                        | `SqlBatch`: ordered changes      |
//! | `create_database` | name     | -      | ignored                     | absent or `{}`                   |
//!
//! # What ClickHouse can and cannot do here
//!
//! * **Values are typed here, not by the server** (see `value`): numbers of any
//!   width and decimals travel as exact digit text, never through `f64` or
//!   `i64`; `nan` / `inf` / `-inf` are floats; an enum takes its entry name;
//!   a date-time is an UTC instant (the text the grid shows, with an optional
//!   `UTC` / `Z` / `±HH:MM` suffix) whatever the zone of the column; a value the
//!   column cannot hold is `mutation.invalid_value` and nothing is written (the
//!   server would wrap `300` into a `UInt8`). `Array`, `Map`, `Tuple`, `JSON`,
//!   `Variant`, `Dynamic` and aggregate-state columns are not editable as text
//!   (`mutation.value_not_editable`): no text the grid shows round-trips.
//! * **Inserts** are `INSERT ... FORMAT JSONEachRow` with the values in the
//!   request body, never in the SQL text. `null` is NULL (a non-nullable column
//!   refuses it), numbers are exact text, and arrays and objects are sent as JSON
//!   text for scalar columns (natively for `Array`, `Map`, `Tuple`, `JSON`
//!   columns). An empty `values` inserts a row of defaults.
//! * **Updates and deletes** are `ALTER TABLE ... UPDATE/DELETE WHERE <key>`
//!   mutations with `mutations_sync = 2` (the request returns when every replica
//!   has applied them), and only on `MergeTree`-family tables that have a
//!   sorting key; any other table is refused with `mutation.unsupported`.
//!   ClickHouse cannot update a primary, sorting or partition key column, so
//!   such an update is refused with `mutation.unsupported` too. The key and the
//!   new values are `{name:Type}` query parameters typed from `system.columns`;
//!   a `null` key value is `IS NULL`.
//! * ClickHouse reports no affected-row count for a mutation, so the driver
//!   counts the rows the key matches first: none is `mutation.no_rows_affected`,
//!   several `mutation.identity_not_unique` (nothing is changed in either case).
//!   **The window**: another client can insert a second matching row between that
//!   count and the `ALTER`. ClickHouse has no way to make a mutation fail on a
//!   row count, but it can make it match nothing: the `ALTER`'s `WHERE` repeats
//!   the key and adds `AND (SELECT count() FROM t WHERE <key>) = 1`, which
//!   `mutations_execute_subqueries_on_initiator = 1` evaluates once, when the
//!   `ALTER` is accepted. A key that then matches two rows changes *nothing*
//!   (instead of both); a `DELETE` is checked afterwards (the key must match no
//!   row) and reports `mutation.identity_not_unique` otherwise; an `UPDATE` that
//!   was skipped this way cannot be told from one that ran (the server gives no
//!   count), so it still reports success. What is left is the span between the
//!   subquery and the part snapshot of the mutation itself: milliseconds.
//! * A **row key** on a `String` / `FixedString` column that reads `0x` + hex
//!   matches the row whose value is that text *or* those bytes (the grid shows
//!   a column whose page holds non-UTF-8 bytes as hex, and sends its keys that
//!   way); a table holding both rows is `mutation.identity_not_unique`. A value
//!   written to a `String` column is always the text: `0xdeadbeef` is a valid
//!   string, so it is never turned into bytes.
//!
//! # `batch` is validated, not atomic
//!
//! ClickHouse has no transactions. A batch is applied as: validate **every**
//! change against the current table first (unknown columns, key counts, key
//! columns, unsupported tables), then insert all new rows in one `INSERT`, then
//! run the update/delete mutations in order. Updates and deletes that address a
//! row inserted earlier in the same batch are folded into that insert, so the
//! row is written once and finished. Everything that can be checked is checked
//! before the first write (every value is validated and every statement built
//! first), so a bad batch changes nothing and is the plain error. The writes
//! run as stages: the one `INSERT`, then the `ALTER`s in order.
//!
//! **Once a write has been accepted, a stop is `mutation.partially_applied`**
//! (never the plain error or a plain `mutation.cancelled`, or the app would
//! apply the same rows again), with the tail the MySQL driver uses and the
//! request's own change indexes: `applied: [..]; failed: [..]; unknown: [..];
//! not attempted: [..]`. A change goes to `unknown` (never `failed`) when the
//! server may have applied it: the connection broke, or the wait of an
//! accepted `ALTER` was cut short by a cancel or a timeout (the mutation keeps
//! running). A stop with an unknown outcome is `partially_applied` even when it
//! is the first write. The plain error stays for a first write the server
//! refused (or a cancel before anything was sent). Changes that cancel each
//! other (an insert and the delete of that row) are `applied`. Later changes
//! are matched against the table as it is before the batch: address rows by
//! columns that earlier changes of the batch do not modify (the primary key),
//! and an update or delete whose key names a column an earlier insert of the
//! batch leaves to its default is refused (`mutation.unsupported`).
//!
//! Cancelling a mutation stops waiting for the server (`KILL QUERY` of the
//! `ALTER`); a mutation the server already accepted still runs (its change is
//! `unknown`, see above).
//!
//! `mutations_execute_subqueries_on_initiator` (the guard, above) is sent with
//! every `ALTER`; a server that does not know it gets the `ALTER` again without
//! it, and the driver remembers (the guard still protects: the subquery then
//! runs per data part while the mutation does). After the `ALTER` the key is
//! counted again, because a guarded `ALTER` that matched two rows changes nothing
//! and still succeeds: a `DELETE` must leave no row, an `UPDATE` exactly one
//! (skipped when the update rewrites a column the key names). A skipped one is
//! `mutation.identity_not_unique` (or `no_rows_affected`), nothing applied.
//! A table with unmerged duplicates (`ReplacingMergeTree`) counts two rows for
//! one logical row and is refused, which fails closed.

use crate::http::{Area, HttpError, Plane, Request};
use crate::query::Active;
use crate::sql::{escape_param, push_ident, qualified, NULL_PARAM};
use crate::types::ChType;
use crate::value::{self, Kind, Role};
use crate::ChConnection;
use engine_driver_kit::api::sql::{codes, SqlBatch, SqlRowChange, SqlRowOp, SqlTableIdentity};
use engine_driver_kit::api::{MutationRequest, OperationId};
use engine_driver_kit::{BackendError, MutationOutcome, OperationGuard};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt::Write;

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::with_code(codes::MUTATION_BAD_REQUEST, message)
}

fn unsupported(message: impl Into<String>) -> BackendError {
    BackendError::with_code(codes::MUTATION_UNSUPPORTED, message)
}

fn not_editable(message: impl Into<String>) -> BackendError {
    BackendError::with_code(codes::MUTATION_VALUE_NOT_EDITABLE, message)
}

/// A mutation after validation of the request (no network).
#[derive(Debug, PartialEq)]
pub(crate) enum Plan {
    CreateDatabase {
        name: String,
    },
    Rows {
        database: Option<String>,
        table: String,
        changes: Vec<SqlRowChange>,
    },
}

impl Plan {
    pub(crate) fn parse(request: &MutationRequest) -> Result<Self, BackendError> {
        if request.kind == "create_database" {
            let empty = match &request.payload {
                Value::Null => true,
                Value::Object(options) => options.is_empty(),
                _ => false,
            };
            if !empty {
                return Err(bad_request("create_database takes no options"));
            }
            let name = request
                .database
                .clone()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| bad_request("missing database name"))?;
            return Ok(Self::CreateDatabase { name });
        }
        let changes = match request.kind.as_str() {
            "insert" | "update" | "delete" => {
                let change: SqlRowChange = serde_json::from_value(request.payload.clone())
                    .map_err(|error| {
                        bad_request(format!("payload is not a SqlRowChange: {error}"))
                    })?;
                if change.op.kind() != request.kind {
                    return Err(bad_request(format!(
                        "op `{}` does not match the mutation kind `{}`",
                        change.op.kind(),
                        request.kind
                    )));
                }
                vec![change]
            }
            "batch" => {
                let batch: SqlBatch = serde_json::from_value(request.payload.clone())
                    .map_err(|error| bad_request(format!("payload is not a SqlBatch: {error}")))?;
                if batch.changes.is_empty() {
                    return Err(bad_request("the batch has no changes"));
                }
                batch.changes
            }
            other => {
                return Err(unsupported(format!("unsupported mutation kind `{other}`")));
            }
        };
        if !request.identity.is_null() {
            serde_json::from_value::<SqlTableIdentity>(request.identity.clone()).map_err(
                |error| bad_request(format!("identity is not a SqlTableIdentity: {error}")),
            )?;
        }
        let table = request
            .object
            .clone()
            .filter(|table| !table.is_empty())
            .ok_or_else(|| bad_request("missing table (object)"))?;
        for change in &changes {
            match change.op {
                SqlRowOp::Insert => {}
                SqlRowOp::Update if change.key.is_empty() => {
                    return Err(bad_request("key is empty"))
                }
                SqlRowOp::Update if change.values.is_empty() => {
                    return Err(bad_request("nothing to update: values is empty"));
                }
                SqlRowOp::Delete if change.key.is_empty() => {
                    return Err(bad_request("key is empty"))
                }
                SqlRowOp::Update | SqlRowOp::Delete => {}
            }
        }
        Ok(Self::Rows {
            database: request
                .database
                .clone()
                .filter(|database| !database.is_empty()),
            table,
            changes,
        })
    }
}

/// One column of the target table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    pub ty: ChType,
    /// Part of the primary, sorting or partition key: ClickHouse cannot update it.
    pub in_key: bool,
}

/// What the catalog says about the target table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    pub name: String,
    pub engine: String,
    pub sorting_key: String,
    pub columns: HashMap<String, Column>,
}

impl Table {
    /// `ALTER TABLE ... UPDATE/DELETE` needs a MergeTree with a sorting key.
    fn require_mutable(&self) -> Result<(), BackendError> {
        if self.engine.ends_with("MergeTree") && !self.sorting_key.trim().is_empty() {
            return Ok(());
        }
        Err(unsupported(format!(
            "`{}` cannot be edited by key: ClickHouse updates and deletes rows only in \
             MergeTree-family tables that have a sorting key (this table uses {}{})",
            self.name,
            if self.engine.is_empty() {
                "an unknown engine"
            } else {
                &self.engine
            },
            if self.engine.ends_with("MergeTree") {
                " without an ORDER BY key"
            } else {
                ""
            },
        )))
    }

    fn column(&self, name: &str) -> Result<&Column, BackendError> {
        self.columns
            .get(name)
            .ok_or_else(|| bad_request(format!("`{}` has no column `{name}`", self.name)))
    }

    fn check_key(&self, key: &Map<String, Value>) -> Result<(), BackendError> {
        for name in key.keys() {
            if !Kind::of(&self.column(name)?.ty).is_editable() {
                return Err(not_editable(format!(
                    "column `{name}` is an Array, Map, Tuple, JSON or aggregate column and cannot address a row"
                )));
            }
        }
        Ok(())
    }

    fn check_update_values(&self, values: &Map<String, Value>) -> Result<(), BackendError> {
        for name in values.keys() {
            let column = self.column(name)?;
            if column.in_key {
                return Err(unsupported(format!(
                    "column `{name}` is part of the table's primary, sorting or partition key; \
                     ClickHouse cannot update key columns"
                )));
            }
            if !Kind::of(&column.ty).is_editable() {
                return Err(not_editable(format!(
                    "column `{name}` is an Array, Map, Tuple, JSON or aggregate column; its value \
                     cannot be written back from the text the grid shows"
                )));
            }
        }
        Ok(())
    }
}

/// Whether a pending insert row is the row `key` addresses.
enum Match {
    Yes,
    No,
    /// The row leaves this key column to the table's default.
    Unknown(String),
}

fn key_match(row: &Map<String, Value>, key: &Map<String, Value>) -> Match {
    for (column, wanted) in key {
        match row.get(column) {
            None => return Match::Unknown(column.clone()),
            Some(value) if value != wanted => return Match::No,
            Some(_) => {}
        }
    }
    Match::Yes
}

/// An update or delete that runs as an `ALTER` on rows already in the table.
/// `change` is the index of the request's change it came from.
#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    Update {
        change: usize,
        key: Map<String, Value>,
        values: Map<String, Value>,
    },
    Delete {
        change: usize,
        key: Map<String, Value>,
    },
}

impl Action {
    fn change(&self) -> usize {
        match self {
            Self::Update { change, .. } | Self::Delete { change, .. } => *change,
        }
    }
}

/// A validated batch: the rows to insert (updates and deletes of rows inserted
/// earlier in the batch already folded in) and the `ALTER`s, in order. Every
/// change of the request is accounted for exactly once: in `insert_changes`
/// (written by the one `INSERT`), as an action, or in `netted` (an insert and
/// the delete of that row cancelled each other: nothing is written).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Planned {
    pub inserts: Vec<Map<String, Value>>,
    /// Per row of `inserts`, the request's changes that became that row (its
    /// insert and the updates folded into it).
    pub insert_changes: Vec<Vec<usize>>,
    pub actions: Vec<Action>,
    pub netted: Vec<usize>,
    /// Number of changes the request carried (each affects exactly one row).
    pub changes: u64,
}

/// Counts the rows of the table a key matches now.
type CountRows<'a> = &'a mut dyn FnMut(&Map<String, Value>) -> Result<u64, BackendError>;

/// Validates `changes` against `table` without writing anything. `count` returns
/// how many rows of the table a key matches *now*.
pub(crate) fn plan_changes(
    changes: &[SqlRowChange],
    table: &Table,
    count: CountRows<'_>,
) -> Result<Planned, BackendError> {
    let mut planned = Planned {
        changes: changes.len() as u64,
        ..Planned::default()
    };
    // Keys of rows already deleted from the table by an earlier change.
    let mut deleted: Vec<&Map<String, Value>> = Vec::new();
    for (position, change) in changes.iter().enumerate() {
        if change.op == SqlRowOp::Insert {
            for name in change.values.keys() {
                table.column(name)?;
            }
            planned.inserts.push(change.values.clone());
            planned.insert_changes.push(vec![position]);
            continue;
        }
        table.require_mutable()?;
        table.check_key(&change.key)?;
        if change.op == SqlRowOp::Update {
            table.check_update_values(&change.values)?;
        }
        let mut pending = Vec::new();
        for (index, row) in planned.inserts.iter().enumerate() {
            match key_match(row, &change.key) {
                Match::Yes => pending.push(index),
                Match::No => {}
                Match::Unknown(column) => {
                    return Err(unsupported(format!(
                        "the key names column `{column}`, which an earlier insert of the batch \
                         leaves to its default; send that insert separately"
                    )));
                }
            }
        }
        let stored = if deleted.contains(&&change.key) {
            0
        } else {
            count(&change.key)?
        };
        match stored + pending.len() as u64 {
            0 => {
                return Err(BackendError::with_code(
                    codes::MUTATION_NO_ROWS_AFFECTED,
                    "the key matches no row",
                ));
            }
            1 => {}
            matches => {
                return Err(BackendError::with_code(
                    codes::MUTATION_IDENTITY_NOT_UNIQUE,
                    format!("the key matches {matches} rows; nothing was changed"),
                ));
            }
        }
        match (change.op, pending.first()) {
            (SqlRowOp::Update, Some(&index)) => {
                planned.inserts[index].extend(change.values.clone());
                planned.insert_changes[index].push(position);
            }
            (SqlRowOp::Delete, Some(&index)) => {
                planned.inserts.remove(index);
                planned.netted.extend(planned.insert_changes.remove(index));
                planned.netted.push(position);
            }
            (SqlRowOp::Update, None) => planned.actions.push(Action::Update {
                change: position,
                key: change.key.clone(),
                values: change.values.clone(),
            }),
            (_, None) => {
                deleted.push(&change.key);
                planned.actions.push(Action::Delete {
                    change: position,
                    key: change.key.clone(),
                });
            }
            (SqlRowOp::Insert, _) => {}
        }
    }
    Ok(planned)
}

/// `` `k1` = {k0:T} AND `k2` IS NULL ``, with the parameters it refers to.
fn predicate(
    key: &Map<String, Value>,
    table: &Table,
    prefix: &str,
    params: &mut Vec<(String, String)>,
) -> Result<String, BackendError> {
    let mut sql = String::new();
    for (index, (name, value)) in key.iter().enumerate() {
        if index > 0 {
            sql.push_str(" AND ");
        }
        let mut ident = String::new();
        push_ident(&mut ident, name);
        if value.is_null() {
            let _ = write!(sql, "{ident} IS NULL");
            continue;
        }
        let ty = &table.column(name)?.ty;
        let bound = value::bind(name, ty, value, Role::Key)?;
        // NaN is not equal to itself.
        if bound.nan {
            let _ = write!(sql, "isNaN({ident})");
            continue;
        }
        let param = format!("{prefix}{index}");
        let bytes = value.as_str().and_then(value::hex_digits);
        match bytes.filter(|_| matches!(ty.peeled().name(), "String" | "FixedString")) {
            // `0x` + hex on a text column: the text itself, or those bytes.
            Some(digits) => {
                let hex = format!("{param}h");
                let _ = write!(
                    sql,
                    "({ident} = {{{param}:String}} OR {ident} = unhex({{{hex}:String}}))"
                );
                params.push((hex, digits.to_owned()));
            }
            None => {
                let _ = write!(sql, "{ident} = {}", bound.expression(&param));
            }
        }
        params.push((param, escape_param(&bound.text)));
    }
    Ok(sql)
}

/// The `WHERE` of an `ALTER`: the key, and that the key matches exactly one row
/// when the `ALTER` is accepted (see the module docs, "the window").
fn guarded_where(
    key: &Map<String, Value>,
    table_sql: &str,
    table: &Table,
    params: &mut Vec<(String, String)>,
) -> Result<String, BackendError> {
    let key_sql = predicate(key, table, "k", params)?;
    Ok(format!(
        "{key_sql} AND (SELECT count() FROM {table_sql} WHERE {key_sql}) = 1"
    ))
}

fn update_sql(
    table_sql: &str,
    values: &Map<String, Value>,
    key: &Map<String, Value>,
    table: &Table,
) -> Result<(String, Vec<(String, String)>), BackendError> {
    let mut params = Vec::new();
    let mut sql = format!("ALTER TABLE {table_sql} UPDATE ");
    for (index, (name, value)) in values.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        push_ident(&mut sql, name);
        let ty = &table.column(name)?.ty;
        let param = format!("v{index}");
        if value.is_null() {
            if !ty.is_nullable() {
                return Err(bad_request(format!("column `{name}` is not nullable")));
            }
            let _ = write!(sql, " = {{{param}:Nullable({})}}", ty.peeled());
            params.push((param, NULL_PARAM.to_owned()));
        } else {
            let bound = value::bind(name, ty, value, Role::Value)?;
            let _ = write!(sql, " = {}", bound.expression(&param));
            params.push((param, escape_param(&bound.text)));
        }
    }
    let _ = write!(
        sql,
        " WHERE {}",
        guarded_where(key, table_sql, table, &mut params)?
    );
    Ok((sql, params))
}

fn delete_sql(
    table_sql: &str,
    key: &Map<String, Value>,
    table: &Table,
) -> Result<(String, Vec<(String, String)>), BackendError> {
    let mut params = Vec::new();
    let sql = format!(
        "ALTER TABLE {table_sql} DELETE WHERE {}",
        guarded_where(key, table_sql, table, &mut params)?
    );
    Ok((sql, params))
}

/// The `INSERT` statement and its JSONEachRow rows.
fn insert_statement(
    table_sql: &str,
    rows: &[Map<String, Value>],
    table: &Table,
) -> Result<(String, String), BackendError> {
    let mut columns: Vec<&str> = Vec::new();
    for row in rows {
        for name in row.keys() {
            if !columns.contains(&name.as_str()) {
                columns.push(name);
            }
        }
    }
    let mut sql = format!("INSERT INTO {table_sql}");
    if !columns.is_empty() {
        sql.push_str(" (");
        for (index, name) in columns.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            push_ident(&mut sql, name);
        }
        sql.push(')');
    }
    sql.push_str(" FORMAT JSONEachRow");
    let mut data = String::new();
    for row in rows {
        let mut object = Map::with_capacity(row.len());
        for (name, value) in row {
            let ty = &table.column(name)?.ty;
            object.insert(name.clone(), value::insert_json(name, ty, value)?);
        }
        data.push_str(&Value::Object(object).to_string());
        data.push('\n');
    }
    Ok((sql, data))
}

pub(crate) fn mutate(
    connection: &ChConnection,
    request: &MutationRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<MutationOutcome, BackendError> {
    let plan = Plan::parse(request)?;
    connection.check_open(Area::Mutation)?;
    let guard = connection.lock(Area::Mutation)?;
    let active = operation.activate(guard)?;
    let operation_id = active.operation_id();
    let mut writer = Writer {
        connection,
        active: &active,
        operation_id,
        statements: 0,
    };
    match plan {
        Plan::CreateDatabase { name } => {
            let mut sql = String::from("CREATE DATABASE ");
            push_ident(&mut sql, &name);
            writer
                .write(&sql, "", &[], &[])
                .map_err(|Stop { error, .. }| {
                    // The server refusing is the contract's `rejected`; a cancel or a
                    // lost connection keeps its own code.
                    if error.code.as_deref() == Some(codes::MUTATION_FAILED) {
                        BackendError::with_code(codes::CREATE_DATABASE_REJECTED, error.message)
                    } else {
                        error
                    }
                })?;
            Ok(MutationOutcome::new(0))
        }
        Plan::Rows {
            database,
            table,
            changes,
        } => {
            let database = database
                .or_else(|| connection.client.database.clone())
                .ok_or_else(|| bad_request("no database selected"))?;
            let meta = load_table(connection, &database, &table)?;
            let table_sql = qualified(Some(&database), &table);
            let planned = plan_changes(&changes, &meta, &mut |key| {
                count_rows(connection, &table_sql, key, &meta)
            })?;
            let applied = apply(&mut writer, &table_sql, &meta, &planned);
            // Described column types may be stale after a write (a failed one
            // too: it may have got part of the way).
            active.types.borrow_mut().clear();
            applied?;
            Ok(MutationOutcome::new(planned.changes))
        }
    }
}

/// A write that did not complete.
#[derive(Debug)]
struct Stop {
    error: BackendError,
    /// The server may or may not have applied it (the connection broke, or the
    /// wait was cut short after the statement was accepted: an accepted `ALTER`
    /// keeps running when the client stops waiting). `false`: it refused, or
    /// was never sent; nothing of it was applied.
    unknown: bool,
}

impl Stop {
    fn refused(error: BackendError) -> Self {
        Self {
            error,
            unknown: false,
        }
    }

    fn unknown(error: BackendError) -> Self {
        Self {
            error,
            unknown: true,
        }
    }
}

/// The setting that makes the guard's `count()` evaluate once, when an `ALTER`
/// is accepted (see the module docs). A server that does not know it still
/// evaluates the guard, per data part while the mutation runs: that is the
/// same protection for a key that matches one row, so it is sent again without
/// the setting (and remembered).
const INITIATOR_SUBQUERIES: &str = "mutations_execute_subqueries_on_initiator";

/// Sends write statements, each with its own `query_id` (cancellable).
struct Writer<'a, 'b, 'c> {
    connection: &'a ChConnection,
    active: &'a Active<'b, 'c>,
    operation_id: OperationId,
    statements: usize,
}

impl Writer<'_, '_, '_> {
    fn cancelled() -> Stop {
        Stop::refused(BackendError::with_code(
            codes::MUTATION_CANCELLED,
            "operation cancelled",
        ))
    }

    /// Runs one statement to completion. `data` follows the statement in the
    /// body (an `INSERT`'s rows); `params` are the `{name:Type}` values.
    fn write(
        &mut self,
        sql: &str,
        data: &str,
        params: &[(String, String)],
        settings: &[(&str, String)],
    ) -> Result<(), Stop> {
        if self.active.cancel_requested() {
            return Err(Self::cancelled());
        }
        let query_id = self.connection.query_id(self.operation_id, self.statements);
        self.statements += 1;
        let _mark = self.connection.mark(&query_id);
        // Close the window between the check above and the mark.
        if self.active.cancel_requested() {
            return Err(Self::cancelled());
        }
        let client = &self.connection.client;
        let result = loop {
            let sent: Vec<(&str, String)> = settings
                .iter()
                .filter(|(name, _)| {
                    *name != INITIATOR_SUBQUERIES || client.initiator_subqueries_allowed()
                })
                .cloned()
                .collect();
            let result = client
                .send(&Request {
                    sql,
                    data,
                    query_id: Some(&query_id),
                    params,
                    settings: &sent,
                    plane: Plane::Query,
                    ..Request::default()
                })
                .and_then(|reply| reply.text().map(drop));
            match &result {
                Err(error)
                    if client.initiator_subqueries_allowed()
                        && error.is_unknown_setting(INITIATOR_SUBQUERIES) =>
                {
                    client.refuse_initiator_subqueries();
                }
                _ => break result,
            }
        };
        result.map_err(|error: HttpError| {
            let unknown = error.outcome_unknown();
            let error = error.into_backend(Area::Mutation, self.active.cancel_requested());
            Stop { error, unknown }
        })
    }
}

/// One write of a batch, with the request's changes it carries out.
enum Stage<'a> {
    Insert {
        sql: String,
        data: String,
    },
    Action {
        action: &'a Action,
        sql: String,
        params: Vec<(String, String)>,
    },
}

/// The `mutation.partially_applied` answer: the request's change indexes, by
/// outcome. `here` are the changes of the write that stopped (`failed`, or
/// `unknown` when the server may have applied them), `later` never ran. The
/// tail is stable text the app parses: `applied: [..]; failed: [..]; unknown:
/// [..]; not attempted: [..]` (`unknown` is the one list MySQL's message does not
/// have; a parser that ignores it keeps those changes staged, as it must).
fn partial_error(
    table: &str,
    stop: &Stop,
    mut applied: Vec<usize>,
    here: &[usize],
    later: &[usize],
) -> BackendError {
    let list = |indexes: &[usize]| {
        let mut sorted = indexes.to_vec();
        sorted.sort_unstable();
        sorted
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    applied.sort_unstable();
    let (failed, unknown): (&[usize], &[usize]) = if stop.unknown {
        (&[], here)
    } else {
        (here, &[])
    };
    let what = if stop.unknown {
        "the outcome of the changes (the server may have applied them)"
    } else {
        "the changes"
    };
    BackendError::with_code(
        codes::MUTATION_PARTIALLY_APPLIED,
        format!(
            "stopped at {what} {} ({}). `{table}` has no transactions in ClickHouse, so the \
             changes before are NOT rolled back. applied: [{}]; failed: [{}]; unknown: [{}]; \
             not attempted: [{}]",
            list(here),
            stop.error.message,
            list(&applied),
            list(failed),
            list(unknown),
            list(later),
        ),
    )
}

fn apply(
    writer: &mut Writer<'_, '_, '_>,
    table_sql: &str,
    meta: &Table,
    planned: &Planned,
) -> Result<(), BackendError> {
    // Every statement is built, and so every value validated, before the first
    // write: a bad value changes nothing.
    let mut stages: Vec<(Vec<usize>, Stage<'_>)> = Vec::new();
    if !planned.inserts.is_empty() {
        let (sql, data) = insert_statement(table_sql, &planned.inserts, meta)?;
        let changes = planned.insert_changes.iter().flatten().copied().collect();
        stages.push((changes, Stage::Insert { sql, data }));
    }
    for action in &planned.actions {
        let (sql, params) = match action {
            Action::Update { key, values, .. } => update_sql(table_sql, values, key, meta)?,
            Action::Delete { key, .. } => delete_sql(table_sql, key, meta)?,
        };
        stages.push((
            vec![action.change()],
            Stage::Action {
                action,
                sql,
                params,
            },
        ));
    }
    // Changes that needed no write (an insert and the delete of that row) are
    // done as soon as the batch is.
    let mut applied = planned.netted.clone();
    let mut wrote = false;
    for (position, (changes, stage)) in stages.iter().enumerate() {
        match run_stage(writer, table_sql, meta, stage) {
            Ok(()) => {
                applied.extend(changes);
                wrote = true;
            }
            // Nothing was written and this write is known not to have been:
            // the plain error, nothing to reconcile.
            Err(stop) if !wrote && !stop.unknown => return Err(stop.error),
            Err(stop) => {
                let later: Vec<usize> = stages[position + 1..]
                    .iter()
                    .flat_map(|(changes, _)| changes.iter().copied())
                    .collect();
                return Err(partial_error(&meta.name, &stop, applied, changes, &later));
            }
        }
    }
    Ok(())
}

fn run_stage(
    writer: &mut Writer<'_, '_, '_>,
    table_sql: &str,
    meta: &Table,
    stage: &Stage<'_>,
) -> Result<(), Stop> {
    let (action, sql, params) = match stage {
        Stage::Insert { sql, data } => {
            return writer.write(
                sql,
                data,
                &[],
                &[
                    ("input_format_null_as_default", "0".into()),
                    ("date_time_input_format", "best_effort".into()),
                ],
            );
        }
        Stage::Action {
            action,
            sql,
            params,
        } => (action, sql, params),
    };
    writer.write(
        sql,
        "",
        params,
        &[
            ("mutations_sync", "2".into()),
            (INITIATOR_SUBQUERIES, "1".into()),
        ],
    )?;
    // A guarded `ALTER` whose key matched two rows when it was accepted changed
    // nothing and still succeeded: look at the key again. A delete leaves no
    // row; an update leaves exactly one (not checked when it rewrites a column
    // the key names: the key then no longer says what it said).
    let (key, expected, what) = match action {
        Action::Delete { key, .. } => (key, 0, "delete"),
        Action::Update { key, values, .. } if key.keys().all(|name| !values.contains_key(name)) => {
            (key, 1, "update")
        }
        Action::Update { .. } => return Ok(()),
    };
    // The statement was accepted: if the count fails, its outcome is unknown.
    let found = count_rows(writer.connection, table_sql, key, meta).map_err(Stop::unknown)?;
    match found {
        found if found == expected => Ok(()),
        0 => Err(Stop::refused(BackendError::with_code(
            codes::MUTATION_NO_ROWS_AFFECTED,
            format!("the key matches no row after the {what}: another client changed or deleted it meanwhile"),
        ))),
        found => Err(Stop::refused(BackendError::with_code(
            codes::MUTATION_IDENTITY_NOT_UNIQUE,
            format!(
                "the key matches {found} row(s) after the {what}: it matched more than one when the \
                 {what} ran, so the {what} was not applied (or another client added a matching row since)"
            ),
        ))),
    }
}

fn catalog_error(error: HttpError) -> BackendError {
    error.into_backend(Area::Mutation, false)
}

fn load_table(
    connection: &ChConnection,
    database: &str,
    table: &str,
) -> Result<Table, BackendError> {
    let params = vec![
        ("db".to_owned(), escape_param(database)),
        ("table".to_owned(), escape_param(table)),
    ];
    let client = &connection.client;
    let tables = client
        .rows(
            "SELECT engine, sorting_key FROM system.tables \
             WHERE database = {db:String} AND name = {table:String}",
            &params,
        )
        .map_err(catalog_error)?;
    let Some(row) = tables.first() else {
        return Err(bad_request(format!(
            "`{database}`.`{table}` does not exist"
        )));
    };
    let text = |index: usize| {
        row.get(index)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let columns = client
        .rows(
            "SELECT name, type, is_in_partition_key, is_in_sorting_key, is_in_primary_key \
             FROM system.columns WHERE database = {db:String} AND table = {table:String}",
            &params,
        )
        .map_err(catalog_error)?
        .iter()
        .filter_map(|row| {
            let flag = |index: usize| row.get(index).and_then(Value::as_u64) == Some(1);
            Some((
                row.first()?.as_str()?.to_owned(),
                Column {
                    ty: ChType::parse(row.get(1)?.as_str()?),
                    in_key: flag(2) || flag(3) || flag(4),
                },
            ))
        })
        .collect();
    Ok(Table {
        name: table.to_owned(),
        engine: text(0),
        sorting_key: text(1),
        columns,
    })
}

fn count_rows(
    connection: &ChConnection,
    table_sql: &str,
    key: &Map<String, Value>,
    meta: &Table,
) -> Result<u64, BackendError> {
    let mut params = Vec::new();
    let sql = format!(
        "SELECT count() FROM {table_sql} WHERE {}",
        predicate(key, meta, "k", &mut params)?
    );
    let rows = connection
        .client
        .rows(&sql, &params)
        .map_err(catalog_error)?;
    rows.first()
        .and_then(|row| row.first())
        .and_then(Value::as_u64)
        .ok_or_else(|| BackendError::new("the server returned no row count"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(kind: &str, payload: Value) -> MutationRequest {
        MutationRequest {
            operation_id: OperationId(1),
            kind: kind.into(),
            database: Some("app".into()),
            object: Some("items".into()),
            identity: json!({}),
            payload,
        }
    }

    fn table() -> Table {
        let column = |ty: &str, in_key: bool| Column {
            ty: ChType::parse(ty),
            in_key,
        };
        Table {
            name: "items".into(),
            engine: "MergeTree".into(),
            sorting_key: "tenant, id".into(),
            columns: HashMap::from([
                ("tenant".to_owned(), column("Int32", true)),
                ("id".to_owned(), column("Int32", true)),
                ("name".to_owned(), column("Nullable(String)", false)),
                ("qty".to_owned(), column("Nullable(Int32)", false)),
                ("flag".to_owned(), column("Bool", false)),
                ("tags".to_owned(), column("Array(String)", false)),
                ("note".to_owned(), column("LowCardinality(String)", false)),
            ]),
        }
    }

    fn change(value: Value) -> SqlRowChange {
        serde_json::from_value(value).unwrap()
    }

    /// `value` as the driver receives it. The driver is built on its own, where serde_json's `Map` is ordered
    /// by key; a workspace build turns on `preserve_order` (engine-ai, engine-plan, the Mongo shell), which
    /// would keep the literal's order instead. Sorting here tests the driver's order in both builds.
    fn map(value: Value) -> Map<String, Value> {
        let mut entries: Vec<(String, Value)> = value.as_object().unwrap().clone().into_iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        entries.into_iter().collect()
    }

    /// Plans `changes` against a table whose stored rows are `stored` (each a
    /// full key); a key matches the stored rows that carry all its values.
    fn plan(changes: &[SqlRowChange], stored: &[Value]) -> Result<Planned, BackendError> {
        let stored: Vec<Map<String, Value>> = stored.iter().map(|key| map(key.clone())).collect();
        plan_changes(changes, &table(), &mut |key| {
            let matching = stored
                .iter()
                .filter(|row| key.iter().all(|(name, value)| row.get(name) == Some(value)))
                .count();
            Ok(matching as u64)
        })
    }

    #[test]
    fn requests_are_validated_before_the_network() {
        let parsed = Plan::parse(&request(
            "update",
            json!({"op": "update", "key": {"id": 1}, "values": {"qty": 2}}),
        ))
        .unwrap();
        assert!(
            matches!(parsed, Plan::Rows { ref table, ref changes, .. } if table == "items" && changes.len() == 1)
        );
        let code = |request: MutationRequest| {
            Plan::parse(&request)
                .unwrap_err()
                .code
                .unwrap()
                .into_owned()
        };
        assert_eq!(
            code(request("frobnicate", json!({}))),
            codes::MUTATION_UNSUPPORTED
        );
        assert_eq!(
            code(request("update", json!({"op": "insert", "values": {}}))),
            codes::MUTATION_BAD_REQUEST
        );
        assert_eq!(
            code(request(
                "update",
                json!({"op": "update", "values": {"a": 1}})
            )),
            codes::MUTATION_BAD_REQUEST
        );
        assert_eq!(
            code(request("update", json!({"op": "update", "key": {"id": 1}}))),
            codes::MUTATION_BAD_REQUEST
        );
        assert_eq!(
            code(request("delete", json!({"op": "delete"}))),
            codes::MUTATION_BAD_REQUEST
        );
        assert_eq!(
            code(request("batch", json!({"changes": []}))),
            codes::MUTATION_BAD_REQUEST
        );
        let mut no_table = request("insert", json!({"op": "insert", "values": {"id": 1}}));
        no_table.object = None;
        assert_eq!(code(no_table), codes::MUTATION_BAD_REQUEST);
        let mut bad_identity = request("insert", json!({"op": "insert", "values": {}}));
        bad_identity.identity = json!("nope");
        assert_eq!(code(bad_identity), codes::MUTATION_BAD_REQUEST);
    }

    #[test]
    fn create_database_takes_a_name_and_no_options() {
        let mut create = request("create_database", Value::Null);
        create.object = None;
        assert_eq!(
            Plan::parse(&create).unwrap(),
            Plan::CreateDatabase { name: "app".into() }
        );
        create.payload = json!({});
        assert!(Plan::parse(&create).is_ok());
        create.payload = json!({"unexpected": true});
        assert_eq!(
            Plan::parse(&create).unwrap_err().code.as_deref(),
            Some(codes::MUTATION_BAD_REQUEST)
        );
        create.payload = Value::Null;
        create.database = None;
        assert_eq!(
            Plan::parse(&create).unwrap_err().code.as_deref(),
            Some(codes::MUTATION_BAD_REQUEST)
        );
    }

    #[test]
    fn only_mergetree_tables_with_a_sorting_key_can_be_edited_by_key() {
        let update = [change(
            json!({"op": "update", "key": {"id": 1}, "values": {"qty": 2}}),
        )];
        for (engine, key, allowed) in [
            ("MergeTree", "id", true),
            ("ReplicatedReplacingMergeTree", "id", true),
            ("MergeTree", "", false),
            ("Memory", "", false),
            ("Log", "id", false),
            ("Distributed", "", false),
        ] {
            let mut table = table();
            table.engine = engine.into();
            table.sorting_key = key.into();
            let result = plan_changes(&update, &table, &mut |_| Ok(1));
            match (allowed, result) {
                (true, Ok(_)) => {}
                (false, Err(error)) => {
                    assert_eq!(
                        error.code.as_deref(),
                        Some(codes::MUTATION_UNSUPPORTED),
                        "{engine}"
                    );
                    assert!(error.message.contains("MergeTree"), "{}", error.message);
                }
                (allowed, other) => panic!("{engine}/{key}: allowed {allowed}, got {other:?}"),
            }
        }
        // Inserts need neither.
        let mut memory = table();
        memory.engine = "Memory".into();
        let insert = [change(json!({"op": "insert", "values": {"id": 1}}))];
        assert!(plan_changes(&insert, &memory, &mut |_| Ok(0)).is_ok());
    }

    #[test]
    fn keys_must_match_exactly_one_row() {
        let update = |id: i64| {
            change(json!({"op": "update", "key": {"tenant": 1, "id": id}, "values": {"qty": 2}}))
        };
        let rows = [json!({"tenant": 1, "id": 1}), json!({"tenant": 1, "id": 2})];
        assert_eq!(plan(&[update(1)], &rows).unwrap().actions.len(), 1);
        let missing = plan(&[update(99)], &rows).unwrap_err();
        assert_eq!(
            missing.code.as_deref(),
            Some(codes::MUTATION_NO_ROWS_AFFECTED)
        );
        let ambiguous = plan_changes(&[update(1)], &table(), &mut |_| Ok(2)).unwrap_err();
        assert_eq!(
            ambiguous.code.as_deref(),
            Some(codes::MUTATION_IDENTITY_NOT_UNIQUE)
        );
        let empty_delete = [change(json!({"op": "delete", "key": {"id": 7}}))];
        assert_eq!(
            plan_changes(&empty_delete, &table(), &mut |_| Ok(0))
                .unwrap_err()
                .code
                .as_deref(),
            Some(codes::MUTATION_NO_ROWS_AFFECTED)
        );
    }

    #[test]
    fn updates_of_key_columns_unknown_columns_and_composites_are_refused() {
        let attempt = |change_json: Value| {
            plan_changes(&[change(change_json)], &table(), &mut |_| Ok(1))
                .unwrap_err()
                .code
                .unwrap()
                .into_owned()
        };
        assert_eq!(
            attempt(json!({"op": "update", "key": {"id": 1}, "values": {"tenant": 5}})),
            codes::MUTATION_UNSUPPORTED
        );
        // No text the grid shows for an Array / Map / Tuple round-trips.
        assert_eq!(
            attempt(json!({"op": "update", "key": {"id": 1}, "values": {"tags": ["a"]}})),
            codes::MUTATION_VALUE_NOT_EDITABLE
        );
        assert_eq!(
            attempt(json!({"op": "update", "key": {"id": 1}, "values": {"tags": "[a, b]"}})),
            codes::MUTATION_VALUE_NOT_EDITABLE
        );
        assert_eq!(
            attempt(json!({"op": "delete", "key": {"tags": ["a"]}})),
            codes::MUTATION_VALUE_NOT_EDITABLE
        );
        assert_eq!(
            attempt(json!({"op": "update", "key": {"nope": 1}, "values": {"qty": 1}})),
            codes::MUTATION_BAD_REQUEST
        );
        assert_eq!(
            attempt(json!({"op": "insert", "values": {"nope": 1}})),
            codes::MUTATION_BAD_REQUEST
        );
    }

    #[test]
    fn a_batch_folds_changes_to_its_own_inserts_and_checks_everything_first() {
        let changes = [
            change(
                json!({"op": "insert", "values": {"tenant": 2, "id": 1, "name": "a", "qty": 1}}),
            ),
            change(
                json!({"op": "insert", "values": {"tenant": 2, "id": 2, "name": "b", "qty": 2}}),
            ),
            change(json!({"op": "update", "key": {"tenant": 2, "id": 1}, "values": {"qty": 10}})),
        ];
        let planned = plan_changes(&changes, &table(), &mut |_| Ok(0)).unwrap();
        assert_eq!(planned.changes, 3);
        assert!(
            planned.actions.is_empty(),
            "the update was folded into the insert"
        );
        assert_eq!(planned.inserts.len(), 2);
        assert_eq!(planned.inserts[0]["qty"], 10);
        // Deleting a row inserted in the same batch cancels the insert.
        let cancelled = [
            change(json!({"op": "insert", "values": {"tenant": 3, "id": 1}})),
            change(json!({"op": "delete", "key": {"tenant": 3, "id": 1}})),
        ];
        let planned = plan_changes(&cancelled, &table(), &mut |_| Ok(0)).unwrap();
        assert!(planned.inserts.is_empty() && planned.actions.is_empty());
        // A failing change after valid ones fails the whole batch before any write.
        let failing = [
            change(json!({"op": "insert", "values": {"tenant": 3, "id": 1}})),
            change(json!({"op": "update", "key": {"tenant": 3, "id": 99}, "values": {"qty": 2}})),
        ];
        let error = plan_changes(&failing, &table(), &mut |_| Ok(0)).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some(codes::MUTATION_NO_ROWS_AFFECTED)
        );
        // A key naming a column the insert left to its default is ambiguous.
        let ambiguous = [
            change(json!({"op": "insert", "values": {"tenant": 3}})),
            change(json!({"op": "update", "key": {"tenant": 3, "id": 1}, "values": {"qty": 2}})),
        ];
        let error = plan_changes(&ambiguous, &table(), &mut |_| Ok(1)).unwrap_err();
        assert_eq!(error.code.as_deref(), Some(codes::MUTATION_UNSUPPORTED));
        // A row deleted earlier in the batch cannot be updated afterwards.
        let twice = [
            change(json!({"op": "delete", "key": {"tenant": 1, "id": 1}})),
            change(json!({"op": "update", "key": {"tenant": 1, "id": 1}, "values": {"qty": 2}})),
        ];
        let error = plan_changes(&twice, &table(), &mut |_| Ok(1)).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some(codes::MUTATION_NO_ROWS_AFFECTED)
        );
    }

    #[test]
    fn statements_bind_values_as_typed_parameters() {
        let meta = table();
        let (sql, params) = update_sql(
            "`app`.`items`",
            &map(json!({"name": "o'brien\\x", "qty": null, "note": "n"})),
            &map(json!({"id": 7, "tenant": null})),
            &meta,
        )
        .unwrap();
        let key = "`id` = {k0:Int32} AND `tenant` IS NULL";
        assert_eq!(
            sql,
            format!(
                "ALTER TABLE `app`.`items` UPDATE `name` = {{v0:String}}, `note` = {{v1:String}}, \
                 `qty` = {{v2:Nullable(Int32)}} WHERE {key} AND (SELECT count() FROM \
                 `app`.`items` WHERE {key}) = 1"
            )
        );
        let by_name: HashMap<_, _> = params.into_iter().collect();
        assert_eq!(by_name["v0"], "o'brien\\\\x");
        assert_eq!(by_name["v2"], "\\N");
        assert_eq!(by_name["k0"], "7");
        assert!(!sql.contains("brien"), "values never reach the SQL text");
        let (sql, params) =
            delete_sql("`app`.`items`", &map(json!({"flag": true, "id": 1})), &meta).unwrap();
        let key = "`flag` = {k0:Bool} AND `id` = {k1:Int32}";
        assert_eq!(
            sql,
            format!(
                "ALTER TABLE `app`.`items` DELETE WHERE {key} AND (SELECT count() FROM \
                 `app`.`items` WHERE {key}) = 1"
            )
        );
        assert_eq!(params[0], ("k0".to_owned(), "true".to_owned()));
        let error = update_sql(
            "t",
            &map(json!({"id": null})),
            &map(json!({"tenant": 1})),
            &meta,
        )
        .unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some(codes::MUTATION_BAD_REQUEST),
            "id is not nullable"
        );
    }

    fn typed_table() -> Table {
        let column = |ty: &str| Column {
            ty: ChType::parse(ty),
            in_key: false,
        };
        Table {
            name: "t".into(),
            engine: "MergeTree".into(),
            sorting_key: "id".into(),
            columns: HashMap::from([
                ("big".to_owned(), column("Int256")),
                ("u8".to_owned(), column("UInt8")),
                ("dec".to_owned(), column("Nullable(Decimal(38, 10))")),
                ("f".to_owned(), column("Float64")),
                ("e".to_owned(), column("Enum8('a,b' = 1, 'it''s' = 2)")),
                ("tokyo".to_owned(), column("DateTime('Asia/Tokyo')")),
                ("plain".to_owned(), column("Nullable(DateTime)")),
                ("ms".to_owned(), column("DateTime64(3, 'America/New_York')")),
                ("s".to_owned(), column("String")),
                ("fs".to_owned(), column("FixedString(4)")),
                ("lc".to_owned(), column("LowCardinality(String)")),
                ("arr".to_owned(), column("Array(String)")),
                ("agg".to_owned(), column("AggregateFunction(sum, UInt64)")),
            ]),
        }
    }

    #[test]
    fn exact_numbers_enums_and_instants_are_bound_with_their_own_types() {
        let meta = typed_table();
        let (sql, params) = update_sql(
            "t",
            &map(json!({
                "big": "-57896044618658097711785492504343953926634992332820282019728792003956564819968",
                "dec": "1234567890123456789012345678.1234567890",
                "f": "-Infinity",
                "e": "it's",
                "tokyo": "2020-01-02 03:04:05 UTC",
                "plain": "2020-01-02T12:04:05+09:00",
                "ms": "2020-01-02 03:04:05.12"
            })),
            &map(json!({"u8": "007"})),
            &meta,
        )
        .unwrap();
        for typed in [
            "`big` = {v0:Int256}",
            "`dec` = {v1:Decimal(38, 10)}",
            "`e` = {v2:Enum8('a,b' = 1, 'it''s' = 2)}",
            "`f` = {v3:Float64}",
            "`ms` = fromUnixTimestamp64Milli({v4:Int64}, 'UTC')",
            "`plain` = {v5:DateTime('UTC')}",
            "`tokyo` = {v6:DateTime('UTC')}",
            "WHERE `u8` = {k0:UInt8}",
        ] {
            assert!(sql.contains(typed), "{typed} in {sql}");
        }
        let by_name: HashMap<_, _> = params.into_iter().collect();
        assert_eq!(
            by_name["v0"],
            "-57896044618658097711785492504343953926634992332820282019728792003956564819968"
        );
        assert_eq!(by_name["v1"], "1234567890123456789012345678.123456789");
        assert_eq!(by_name["v2"], "it's");
        assert_eq!(by_name["v3"], "-inf");
        assert_eq!(by_name["v4"], "1577934245120");
        assert_eq!(by_name["v5"], "2020-01-02 03:04:05");
        assert_eq!(by_name["v6"], "2020-01-02 03:04:05");
        assert_eq!(by_name["k0"], "7");
        // What the server would wrap, truncate or saturate is refused up front.
        for (column, value) in [
            ("u8", json!("300")),
            ("u8", json!("-1")),
            ("big", json!("1.5")),
            ("dec", json!("1.12345678901")),
            ("f", json!("abc")),
            ("e", json!("zzz")),
            ("tokyo", json!("2106-02-07 06:28:16")),
            ("ms", json!("2020-01-02 03:04:05.1234")),
            ("tokyo", json!("yesterday")),
        ] {
            let error = update_sql(
                "t",
                &map(json!({column: value.clone()})),
                &map(json!({"u8": 1})),
                &meta,
            )
            .unwrap_err();
            assert_eq!(
                error.code.as_deref(),
                Some(codes::MUTATION_INVALID_VALUE),
                "{column} {value}: {}",
                error.message
            );
        }
        let error = update_sql(
            "t",
            &map(json!({"s": "x"})),
            &map(json!({"agg": "0x00"})),
            &meta,
        )
        .unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some(codes::MUTATION_VALUE_NOT_EDITABLE)
        );
    }

    #[test]
    fn keys_compare_instants_floats_and_hex_as_the_data_says() {
        let meta = typed_table();
        let mut params = Vec::new();
        let sql = predicate(
            &map(json!({"tokyo": "2020-01-02 03:04:05 UTC", "f": "NaN", "ms": "2020-01-02T12:04:05.5+09:00"})),
            &meta,
            "k",
            &mut params,
        )
        .unwrap();
        assert_eq!(
            sql,
            "isNaN(`f`) AND `ms` = fromUnixTimestamp64Milli({k1:Int64}, 'UTC') AND `tokyo` = {k2:DateTime('UTC')}"
        );
        let by_name: HashMap<_, _> = params.into_iter().collect();
        assert_eq!(by_name["k1"], "1577934245500");
        assert_eq!(by_name["k2"], "2020-01-02 03:04:05");
        assert!(!by_name.contains_key("k0"), "NaN needs no parameter");
        // A hex key on a text column is the text or those bytes; a FixedString
        // key is compared as a String (a longer text is no match, not an error).
        for (column, ty) in [("s", "String"), ("fs", "String"), ("lc", "String")] {
            let mut params = Vec::new();
            let sql =
                predicate(&map(json!({column: "0xff00fe"})), &meta, "k", &mut params).unwrap();
            assert_eq!(
                sql,
                format!("(`{column}` = {{k0:{ty}}} OR `{column}` = unhex({{k0h:String}}))")
            );
            assert_eq!(
                params,
                [
                    ("k0h".to_owned(), "ff00fe".to_owned()),
                    ("k0".to_owned(), "0xff00fe".to_owned())
                ]
            );
        }
        // Not hex (odd digits, other text), or not a text column: a plain comparison.
        for (column, value) in [("s", "0xf"), ("s", "hello"), ("fs", "ab"), ("u8", "7")] {
            let mut params = Vec::new();
            let sql = predicate(&map(json!({column: value})), &meta, "k", &mut params).unwrap();
            assert!(!sql.contains("unhex"), "{column} {value}: {sql}");
        }
        // Row keys of Array columns and aggregate states are refused.
        let error =
            predicate(&map(json!({"arr": "[a]"})), &meta, "k", &mut Vec::new()).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some(codes::MUTATION_VALUE_NOT_EDITABLE)
        );
    }

    #[test]
    fn inserts_send_rows_as_json_each_row() {
        let meta = table();
        let rows = [
            map(json!({"tenant": 4, "id": 1, "name": "x'); DROP TABLE t; --", "tags": ["a", "b"]})),
            map(json!({"tenant": 4, "id": 2, "qty": null, "name": [1, {"a": null}]})),
        ];
        let (sql, data) = insert_statement("`app`.`items`", &rows, &meta).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO `app`.`items` (`id`, `name`, `tags`, `tenant`, `qty`) FORMAT JSONEachRow"
        );
        assert!(!sql.contains("DROP"), "values never reach the SQL text");
        let lines: Vec<Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["name"], "x'); DROP TABLE t; --");
        assert_eq!(
            lines[0]["tags"],
            json!(["a", "b"]),
            "composite columns take native JSON"
        );
        assert_eq!(
            lines[1]["name"], "[1,{\"a\":null}]",
            "scalar columns take JSON text"
        );
        assert_eq!(lines[1]["qty"], Value::Null);
        assert_eq!(lines[0]["id"], "1", "numbers are exact text");
        // No values: one row of defaults.
        let (sql, data) = insert_statement("t", &[Map::new()], &meta).unwrap();
        assert_eq!(
            (sql.as_str(), data.as_str()),
            ("INSERT INTO t FORMAT JSONEachRow", "{}\n")
        );
        // Typed values: an instant is ISO text in UTC whatever the column's zone,
        // a number out of range is refused before anything is sent.
        let typed = typed_table();
        let (_, data) = insert_statement(
            "t",
            &[map(json!({"tokyo": "2020-01-02 12:04:05+09:00", "ms": "2020-01-02 03:04:05.5", "u8": "255", "e": "a,b"}))],
            &typed,
        )
        .unwrap();
        let row: Value = serde_json::from_str(data.lines().next().unwrap()).unwrap();
        assert_eq!(row["tokyo"], "2020-01-02T03:04:05Z");
        assert_eq!(row["ms"], "2020-01-02T03:04:05.500Z");
        assert_eq!(row["u8"], "255");
        let error = insert_statement("t", &[map(json!({"u8": 256}))], &typed).unwrap_err();
        assert_eq!(error.code.as_deref(), Some(codes::MUTATION_INVALID_VALUE));
    }

    #[test]
    fn every_change_of_a_batch_is_accounted_for_exactly_once() {
        let changes = [
            // 0, 1: two inserts; 2 updates the first (folded into its insert)
            change(json!({"op": "insert", "values": {"tenant": 2, "id": 1, "qty": 1}})),
            change(json!({"op": "insert", "values": {"tenant": 2, "id": 2, "qty": 2}})),
            change(json!({"op": "update", "key": {"tenant": 2, "id": 1}, "values": {"qty": 10}})),
            // 3 updates a stored row, 4 deletes a stored row
            change(json!({"op": "update", "key": {"tenant": 1, "id": 1}, "values": {"qty": 5}})),
            change(json!({"op": "delete", "key": {"tenant": 1, "id": 2}})),
            // 5, 6: an insert and the delete of that very row cancel out
            change(json!({"op": "insert", "values": {"tenant": 3, "id": 1}})),
            change(json!({"op": "delete", "key": {"tenant": 3, "id": 1}})),
        ];
        let stored = [json!({"tenant": 1, "id": 1}), json!({"tenant": 1, "id": 2})];
        let planned = plan(&changes, &stored).unwrap();
        assert_eq!(planned.insert_changes, [vec![0, 2], vec![1]]);
        assert_eq!(planned.inserts.len(), 2);
        assert_eq!(planned.inserts[0]["qty"], 10);
        let action_changes: Vec<usize> = planned.actions.iter().map(Action::change).collect();
        assert_eq!(action_changes, [3, 4]);
        assert_eq!(planned.netted, [5, 6]);
        let mut all: Vec<usize> = planned.insert_changes.iter().flatten().copied().collect();
        all.extend(&action_changes);
        all.extend(&planned.netted);
        all.sort_unstable();
        assert_eq!(all, [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(planned.changes, 7);
    }

    #[test]
    fn a_stopped_batch_names_the_changes_by_outcome() {
        let refused =
            |text: &str| Stop::refused(BackendError::with_code(codes::MUTATION_FAILED, text));
        // The insert (changes 0 and 2) and a netted pair (5, 6) are applied; the
        // update (3) was refused; the delete (4) never ran.
        let error = partial_error("t", &refused("Code: 131"), vec![5, 6, 2, 0], &[3], &[4]);
        assert_eq!(
            error.code.as_deref(),
            Some(codes::MUTATION_PARTIALLY_APPLIED)
        );
        assert!(
            error
                .message
                .ends_with("applied: [0, 2, 5, 6]; failed: [3]; unknown: []; not attempted: [4]"),
            "{}",
            error.message
        );
        assert!(error.message.contains("Code: 131") && error.message.contains("NOT rolled back"));
        // A write that may or may not have happened is `unknown`, never `failed`.
        let unknown = Stop::unknown(BackendError::with_code(
            codes::MUTATION_CANCELLED,
            "cancelled",
        ));
        let error = partial_error("t", &unknown, vec![0, 1], &[2], &[3, 4]);
        assert!(
            error
                .message
                .ends_with("applied: [0, 1]; failed: []; unknown: [2]; not attempted: [3, 4]"),
            "{}",
            error.message
        );
        assert!(!error.retryable);
        // The labels the app reads appear once, in the tail.
        for label in ["applied: [", "failed: [", "not attempted: ["] {
            assert_eq!(error.message.matches(label).count(), 1, "{label}");
        }
    }
}
