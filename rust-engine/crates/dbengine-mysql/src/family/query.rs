//! Query execution under the shared SQL contract (`engine_driver_api::sql`).
//!
//! * `engine_driver_kit::sql::classify` decides what the SQL is. Only a single
//!   pure read is pageable: it gets `LIMIT n+1 OFFSET m` appended server-side
//!   when the statement can take it, or is paged by skipping rows client-side
//!   (rows are discarded as they arrive, never buffered). Anything else runs
//!   once: `offset > 0` is refused with `query.not_pageable`.
//! * `limit == 0` is the default page of 200 rows, pages are capped at 100 000,
//!   and at most one page (+1 lookahead row) is ever buffered.
//! * Dangerous statements need `payload.confirmed == true`
//!   (`query.confirmation_required`, message = reason).
//! * `payload.read_only`: the text runs in `START TRANSACTION READ ONLY` ... `ROLLBACK`.
//!   Classified with the session's `sql_mode` (re-read for every such request), it is
//!   refused with `query.read_only_violation` before anything is sent when a
//!   statement controls the transaction (`COMMIT`, `SET autocommit`, ...), a script has
//!   a statement that is not a plain read, or a single statement is neither a read nor
//!   `EXPLAIN`/`ANALYZE` of one (`CALL` may `COMMIT` inside, `KILL`, `FLUSH`,
//!   `SET GLOBAL`, `SELECT ... INTO OUTFILE` need no transaction)
//!   ([`engine_driver_kit::sql::Classification::read_only_refusal`]). Versioned
//!   comments are read with the server's version (`@@version`, read at connect).
//! * The default database is re-selected whenever user SQL may have changed it.
//!
//! Every value arrives as text (`Value::Bytes`); numbers are parsed straight
//! from those bytes and strings are appended to the builder without an
//! intermediate `String`. DECIMAL, dates, times and JSON stay `Utf8` exactly as
//! the server printed them, so the grid never formats an Arrow decimal per cell.
//!
//! Value rules:
//!
//! * `TINYINT(1)` is an integer column like any other `TINYINT` (never boolean).
//! * `BIT(n)` arrives as big-endian bytes and is decoded to an unsigned integer
//!   (`UInt64`, label `bit(n)`).
//! * `GEOMETRY` (SRID + WKB) is shown as WKT, prefixed `SRID=n;` when the SRID is
//!   not 0 (label `geometry`); an unknown WKB kind is `WKB(0x...)`.
//! * Text is never repaired: the session talks utf8mb4, and a text column whose
//!   page holds a value that is not valid UTF-8 is delivered as `Binary` bytes,
//!   label `<type> (invalid utf-8)`, so the stored bytes are shown as bytes and
//!   cannot be mistaken for (or written back as) text.

use super::conn::{
    cancelled, is_connection_lost, map_error, other_error, Abandoned, ActiveMark, Area, ConnState,
};
use super::sql::{scan, with_limit};
use super::MyConnection;
use arrow::datatypes::{Field, Schema};
use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, Float64Builder, Int64Builder, StringBuilder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::{PageColumn, PageFlags, QueryRequest};
use engine_driver_kit::sql::{classify, Keyword};
use engine_driver_kit::{batch_with_row_count, BackendError, OperationGuard, QueryOutput};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{Column, QueryResult, TextProtocol, Value};
use std::borrow::Cow;
use std::fmt::Write;
use std::sync::Arc;

/// Collation id of the `binary` character set: BLOB/VARCHAR columns that carry
/// it hold bytes, not text.
const BINARY_CHARSET: u16 = 63;
/// Rows of a page when the request says `limit: 0`.
const DEFAULT_PAGE_ROWS: u64 = 200;
/// No page is larger than this, whatever the request asks.
const MAX_PAGE_ROWS: u64 = 100_000;
const INITIAL_ROWS: usize = 256;

/// The page size a request's `limit` stands for.
pub(super) fn page_rows(limit: u64) -> u64 {
    match limit {
        0 => DEFAULT_PAGE_ROWS,
        limit => limit.min(MAX_PAGE_ROWS),
    }
}

/// Runs `request` on the connection and returns the page.
pub(super) fn open_query(
    connection: &MyConnection,
    request: &QueryRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<QueryOutput, BackendError> {
    if request.kind != "sql" && request.kind != "sql_query" {
        return Err(BackendError::with_code(
            "query.unsupported",
            "driver accepts kind=sql",
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
    let confirmed = request
        .payload
        .get("confirmed")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    // Additive (capability `query.read_only`): run THIS statement in a READ ONLY
    // transaction, whatever it is. A write fails with `query.read_only_violation`.
    let force_read_only = request
        .payload
        .get("read_only")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    // Additive (capability `query.run_to_end`): the statement runs as written and to its end.
    let run_to_end = request
        .payload
        .get("run_to_end")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let guard = connection.lock(Area::Query)?;
    // `active` owns the connection guard: the operation stops being active
    // before the connection is released.
    let mut active = operation.activate(guard)?;
    // Declared after `active`, so it ends first.
    let running = connection.mark_running(active.operation_id());
    if active.cancel_requested() {
        // Cancelled between activation and the mark: no kill was sent.
        return Err(cancelled(Area::Query, None));
    }
    if active.conn.is_none() {
        return Err(BackendError::with_code(
            Area::Query.not_connected(),
            "connection closed",
        ));
    }
    let job = Job {
        database: request.database.as_deref(),
        sql,
        offset: request.offset,
        limit: page_rows(request.limit),
        confirmed,
        force_read_only,
        run_to_end,
    };
    let result = super::conn::block_on(run(&mut active, &job, &running));
    match result {
        Ok(Outcome::Command { affected_rows }) => {
            Ok(QueryOutput::command_complete(affected_rows, ()))
        }
        // A statement that "succeeded" while a cancel was requested (for
        // example `SELECT SLEEP()`, which returns 1 when killed) did not
        // produce the rows the caller asked for.
        Ok(Outcome::Rows { .. }) if active.cancel_requested() => Err(cancelled(Area::Query, None)),
        Ok(Outcome::Rows {
            batch,
            columns,
            truncated,
        }) => Ok(QueryOutput {
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
        }),
        Err(RunError::Refused(error)) => Err(error),
        // ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION
        Err(RunError::Db(mysql_async::Error::Server(error)))
            if force_read_only && error.code == 1792 =>
        {
            Err(BackendError::with_code(
                codes::QUERY_READ_ONLY_VIOLATION,
                "the statement tried to write in a read-only execution (error 1792)",
            ))
        }
        Err(RunError::Db(error)) => {
            // A lost connection is closed for good: every later call says so.
            if is_connection_lost(&error) {
                active.mark_broken();
            }
            Err(map_error(&error, active.cancel_requested(), Area::Query))
        }
    }
}

enum Outcome {
    Rows {
        batch: RecordBatch,
        columns: Vec<PageColumn>,
        truncated: bool,
    },
    Command {
        affected_rows: u64,
    },
}

enum RunError {
    Db(mysql_async::Error),
    /// The contract refused the request before it reached the server, or the
    /// host cancelled it before the next round trip.
    Refused(BackendError),
}

impl From<mysql_async::Error> for RunError {
    fn from(error: mysql_async::Error) -> Self {
        Self::Db(error)
    }
}

/// What one request asks of the statement.
struct Job<'a> {
    database: Option<&'a str>,
    sql: &'a str,
    offset: u64,
    limit: u64,
    /// `payload.confirmed`: a dangerous statement may run.
    confirmed: bool,
    /// `payload.read_only`: run it in a READ ONLY transaction, whatever it is.
    force_read_only: bool,
    /// `payload.run_to_end`: as written, no `LIMIT` appended; the rows past the page are read and dropped.
    run_to_end: bool,
}

/// Runs the statement (the cancellable part, see [`ActiveMark::watch`]), then
/// rolls back the READ ONLY transaction it may have opened. A kill cannot
/// reach the `ROLLBACK`: the mark ends first, and the `ROLLBACK` is tried again
/// if a kill that was already on its way interrupts it.
async fn run(
    state: &mut ConnState,
    job: &Job<'_>,
    mark: &ActiveMark<'_>,
) -> Result<Outcome, RunError> {
    // `START TRANSACTION` may have been sent, even if the statement then failed.
    let mut transaction = false;
    let Ok(outcome) = mark
        .watch(attempt(state, job, mark, &mut transaction))
        .await
    else {
        // The server is not answering: no `ROLLBACK` can reach it. Close the session.
        state.mark_broken();
        return Err(RunError::Refused(Abandoned::error(Area::Query)));
    };
    if transaction {
        mark.end();
        if state.control_uninterrupted("ROLLBACK").await.is_err() {
            // The session may still be inside our transaction: do not reuse it.
            state.mark_broken();
        }
    }
    outcome
}

async fn attempt(
    state: &mut ConnState,
    job: &Job<'_>,
    mark: &ActiveMark<'_>,
    transaction: &mut bool,
) -> Result<Outcome, RunError> {
    let Job {
        database,
        sql,
        offset,
        limit,
        confirmed,
        force_read_only,
        run_to_end,
    } = *job;
    mark.check(Area::Query).map_err(RunError::Refused)?;
    if force_read_only {
        // The text must be split the way the server will split it, or a `COMMIT`
        // can hide in what looks like one statement: read `sql_mode` afresh.
        state.mark_lex_stale();
    }
    state.refresh_lex().await;
    if force_read_only && state.lex_is_uncertain() {
        return Err(RunError::Refused(BackendError::with_code(
            "query.read_only_unavailable",
            "the session's sql_mode could not be read, so the text cannot be checked for a statement that leaves the READ ONLY scope",
        )));
    }
    let classification = classify(sql, &state.lex);
    if force_read_only {
        if let Some(refusal) = classification.read_only_refusal() {
            return Err(RunError::Refused(refusal.into()));
        }
    }
    if let Some(danger) = classification.danger {
        if !confirmed {
            return Err(RunError::Refused(BackendError::with_code(
                codes::QUERY_CONFIRMATION_REQUIRED,
                danger.reason,
            )));
        }
    }
    if !classification.pageable && offset > 0 {
        return Err(RunError::Refused(BackendError::with_code(
            codes::QUERY_NOT_PAGEABLE,
            "only a single read-only statement can be paged; this statement runs once, at offset 0",
        )));
    }
    mark.check(Area::Query).map_err(RunError::Refused)?;
    state.use_database(database).await?;
    // `USE`, and `DROP DATABASE` of the default database, change which database
    // is current without telling us: re-select on the next request.
    if classification.changes_database
        || classification
            .statements
            .iter()
            .any(|statement| statement.keyword == Some(Keyword::Drop))
    {
        state.forget_current_db();
    }
    state.note_user_statement(sql);
    let lex = state.lex;
    let (statement, skip) = match classification.single().filter(|_| classification.pageable) {
        Some(single) => {
            let text = &sql[single.range.clone()];
            // SHOW, EXPLAIN and DESCRIBE have no LIMIT clause.
            let takes_limit = matches!(
                single.keyword,
                Some(Keyword::Select | Keyword::With | Keyword::Values | Keyword::Table)
            );
            let paged = (takes_limit && !run_to_end)
                .then(|| with_limit(text, &scan(text, &lex), offset, limit))
                .flatten();
            match paged {
                // The server applied LIMIT/OFFSET (plus one lookahead row).
                Some(paged) => (paged, 0),
                // SHOW, a statement with its own LIMIT, FOR UPDATE ...: skip here.
                None => (sql.to_owned(), offset),
            }
        }
        None => (sql.to_owned(), 0),
    };
    // A re-page (offset > 0) of a pure read runs again; run it in a READ ONLY
    // transaction so a side effect hidden in the read (a function that writes,
    // `NEXTVAL`) fails instead of repeating as a write. Inside a transaction of
    // the user's the statement just runs there (their transaction decides).
    let wants_read_only = force_read_only || (classification.pageable && offset > 0);
    mark.check(Area::Query).map_err(RunError::Refused)?;
    let in_user_transaction =
        wants_read_only && state.in_user_transaction().await.map_err(RunError::Db)?;
    if force_read_only && in_user_transaction {
        // The mode of an open transaction cannot be changed: refuse rather than run unguarded.
        return Err(RunError::Refused(BackendError::with_code(
            "query.read_only_unavailable",
            "a transaction is open on this session, so the statement cannot run READ ONLY",
        )));
    }
    let read_only = wants_read_only && !in_user_transaction;
    mark.check(Area::Query).map_err(RunError::Refused)?;
    if read_only {
        *transaction = true;
        state.control("START TRANSACTION READ ONLY").await?;
        mark.check(Area::Query).map_err(RunError::Refused)?;
    }
    // Run to its end, no page follows: `truncated` is false, as the contract says.
    execute(state, statement, skip, limit, classification.pageable && !run_to_end).await
}

/// Sends `statement` and collects the page from its result sets.
async fn execute(
    state: &mut ConnState,
    statement: String,
    skip: u64,
    limit: u64,
    pageable: bool,
) -> Result<Outcome, RunError> {
    let conn = state
        .conn
        .as_mut()
        .ok_or_else(|| other_error("connection closed"))?;
    let mut result = conn.query_iter(statement).await?;
    let mut affected_rows = 0_u64;
    let mut collected = None;
    // Every statement of a script runs once, in order; the page is the LAST
    // result set (earlier ones are dropped as soon as a later one starts, so only
    // one page is ever held). Only a single pure read is paged: its page
    // `truncated` flag is exact. A script or a write that returns rows is cut at
    // the page size and reports `truncated = false`: there is no next page.
    while let Some(columns) = result.columns() {
        if columns.is_empty() {
            affected_rows += result.affected_rows();
            // An empty set has no rows: `next` just moves on to the next set.
            result.next().await?;
            continue;
        }
        drop(collected.take());
        let (batch, page_columns, more) = collect(&mut result, &columns, skip, limit).await?;
        if more {
            // Rows past the page: discard them (and move on to the next set).
            while result.next().await?.is_some() {}
        }
        collected = Some((batch, page_columns, more && pageable));
    }
    // Surfaces an error raised by a later statement and finishes the exchange.
    result.drop_result().await?;
    Ok(match collected {
        Some((batch, columns, truncated)) => Outcome::Rows {
            batch,
            columns,
            truncated,
        },
        None => Outcome::Command { affected_rows },
    })
}

async fn collect(
    result: &mut QueryResult<'_, 'static, TextProtocol>,
    columns: &[Column],
    skip: u64,
    take: u64,
) -> mysql_async::Result<(RecordBatch, Vec<PageColumn>, bool)> {
    let capacity = usize::try_from(take).map_or(INITIAL_ROWS, |take| take.min(4096));
    let mut sinks: Vec<Sink> = columns
        .iter()
        .map(|column| Sink::new(column, capacity))
        .collect();
    let mut skipped = 0_u64;
    let mut rows = 0_u64;
    let mut truncated = false;
    while let Some(row) = result.next().await? {
        if skipped < skip {
            skipped += 1;
            continue;
        }
        if rows >= take {
            // The lookahead row: the result continues past this page.
            truncated = true;
            break;
        }
        for (index, sink) in sinks.iter_mut().enumerate() {
            sink.push(row.as_ref(index))?;
        }
        rows += 1;
    }
    let mut fields = Vec::with_capacity(sinks.len());
    let mut arrays = Vec::with_capacity(sinks.len());
    let mut page_columns = Vec::with_capacity(sinks.len());
    for (index, (sink, column)) in sinks.into_iter().zip(columns).enumerate() {
        let (array, label) = sink.finish();
        let name = column.name_str().into_owned();
        fields.push(Field::new(&name, array.data_type().clone(), true));
        page_columns.push(PageColumn {
            id: index.to_string(),
            name,
            data_type: label.into_owned(),
            nullable: !column.flags().contains(ColumnFlags::NOT_NULL_FLAG),
        });
        arrays.push(array);
    }
    let batch = batch_with_row_count(Arc::new(Schema::new(fields)), arrays, rows as usize)
        .map_err(|error| other_error(&error.message))?;
    Ok((batch, page_columns, truncated))
}

/// How a result column is stored in Arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// Integers up to 64 bits signed (`TINYINT(1)` included).
    Int,
    UInt,
    Float,
    Text,
    Binary,
    /// `BIT(n)`: big-endian bytes, delivered as `UInt64`.
    Bit,
    /// SRID + WKB, delivered as WKT text.
    Geometry,
}

/// Arrow storage of `column`.
pub(super) fn kind_of(column: &Column) -> Kind {
    use ColumnType::*;
    let binary = column.character_set() == BINARY_CHARSET
        || column.flags().contains(ColumnFlags::BINARY_FLAG) && column.character_set() == 0;
    match column.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_YEAR => Kind::Int,
        MYSQL_TYPE_LONGLONG if column.flags().contains(ColumnFlags::UNSIGNED_FLAG) => Kind::UInt,
        MYSQL_TYPE_LONGLONG => Kind::Int,
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => Kind::Float,
        MYSQL_TYPE_BIT => Kind::Bit,
        MYSQL_TYPE_GEOMETRY => Kind::Geometry,
        MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_STRING
            if binary
                && !column
                    .flags()
                    .intersects(ColumnFlags::ENUM_FLAG | ColumnFlags::SET_FLAG) =>
        {
            Kind::Binary
        }
        // DECIMAL, DATE/TIME types, JSON, ENUM/SET, text: the server's text.
        _ => Kind::Text,
    }
}

/// Native type name shown in the grid header and used by the grid to pick a
/// numeric or text sort (`int`, `decimal`, `float`/`double` sort numerically).
pub(super) fn type_label(column: &Column) -> Cow<'static, str> {
    match column.column_type() {
        ColumnType::MYSQL_TYPE_BIT => Cow::Owned(format!("bit({})", column.column_length().max(1))),
        _ => Cow::Borrowed(static_type_label(column)),
    }
}

fn static_type_label(column: &Column) -> &'static str {
    use ColumnType::*;
    let unsigned = column.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = kind_of(column) == Kind::Binary;
    let flags = column.flags();
    match column.column_type() {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG if unsigned => "bigint unsigned",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_TINY_BLOB if binary => "tinyblob",
        MYSQL_TYPE_TINY_BLOB => "tinytext",
        MYSQL_TYPE_MEDIUM_BLOB if binary => "mediumblob",
        MYSQL_TYPE_MEDIUM_BLOB => "mediumtext",
        MYSQL_TYPE_LONG_BLOB if binary => "longblob",
        MYSQL_TYPE_LONG_BLOB => "longtext",
        MYSQL_TYPE_BLOB if binary => "blob",
        MYSQL_TYPE_BLOB => "text",
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR if binary => "varbinary",
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => "varchar",
        MYSQL_TYPE_STRING if flags.contains(ColumnFlags::ENUM_FLAG) => "enum",
        MYSQL_TYPE_STRING if flags.contains(ColumnFlags::SET_FLAG) => "set",
        MYSQL_TYPE_STRING if binary => "binary",
        MYSQL_TYPE_STRING => "char",
        _ => "text",
    }
}

/// One column's Arrow builder plus its grid type label.
enum Sink {
    Int(Int64Builder, Cow<'static, str>),
    UInt(UInt64Builder, Cow<'static, str>),
    Float(Float64Builder, Cow<'static, str>),
    Text(StringBuilder, Cow<'static, str>),
    /// A text column that held a value which is not UTF-8: all of it as bytes.
    TextBytes(BinaryBuilder, Cow<'static, str>),
    Binary(BinaryBuilder, Cow<'static, str>),
    Bit(UInt64Builder, Cow<'static, str>),
    Geometry {
        builder: StringBuilder,
        /// One reused buffer: a WKT is built here, then appended.
        scratch: String,
    },
}

/// Appended to the label of a text column delivered as bytes.
pub(super) const INVALID_UTF8_SUFFIX: &str = " (invalid utf-8)";

impl Sink {
    fn new(column: &Column, capacity: usize) -> Self {
        let label = type_label(column);
        match kind_of(column) {
            Kind::Int => Self::Int(Int64Builder::with_capacity(capacity), label),
            Kind::UInt => Self::UInt(UInt64Builder::with_capacity(capacity), label),
            Kind::Float => Self::Float(Float64Builder::with_capacity(capacity), label),
            Kind::Text => Self::Text(
                StringBuilder::with_capacity(capacity, capacity.saturating_mul(16)),
                label,
            ),
            Kind::Binary => Self::Binary(
                BinaryBuilder::with_capacity(capacity, capacity.saturating_mul(16)),
                label,
            ),
            Kind::Bit => Self::Bit(UInt64Builder::with_capacity(capacity), label),
            Kind::Geometry => Self::Geometry {
                builder: StringBuilder::with_capacity(capacity, capacity.saturating_mul(32)),
                scratch: String::new(),
            },
        }
    }

    fn push(&mut self, value: Option<&Value>) -> mysql_async::Result<()> {
        let bytes = match value {
            None | Some(Value::NULL) => {
                match self {
                    Self::Int(builder, _) => builder.append_null(),
                    Self::UInt(builder, _) | Self::Bit(builder, _) => builder.append_null(),
                    Self::Float(builder, _) => builder.append_null(),
                    Self::Text(builder, _) => builder.append_null(),
                    Self::TextBytes(builder, _) | Self::Binary(builder, _) => {
                        builder.append_null();
                    }
                    Self::Geometry { builder, .. } => builder.append_null(),
                }
                return Ok(());
            }
            Some(Value::Bytes(bytes)) => bytes.as_slice(),
            Some(_) => return Err(other_error("unexpected non-text value in a text result")),
        };
        match self {
            Self::Int(builder, _) => builder.append_value(
                parse_i64(bytes).ok_or_else(|| other_error("invalid integer in result"))?,
            ),
            Self::UInt(builder, _) => builder.append_value(
                parse_u64(bytes)
                    .ok_or_else(|| other_error("invalid unsigned integer in result"))?,
            ),
            Self::Bit(builder, _) => builder
                .append_value(be_u64(bytes).ok_or_else(|| other_error("invalid BIT in result"))?),
            Self::Float(builder, _) => builder.append_value(
                std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|text| text.parse::<f64>().ok())
                    .ok_or_else(|| other_error("invalid float in result"))?,
            ),
            Self::Text(builder, label) => match std::str::from_utf8(bytes) {
                Ok(text) => builder.append_value(text),
                Err(_) => {
                    // Not text: the whole column becomes bytes (one copy of what
                    // was read so far) and this value is kept exactly as stored.
                    let mut bytes_builder = BinaryBuilder::with_capacity(
                        builder.len() + 1,
                        builder.values_slice().len() + bytes.len(),
                    );
                    let earlier = builder.finish();
                    for value in earlier.iter() {
                        bytes_builder.append_option(value.map(str::as_bytes));
                    }
                    bytes_builder.append_value(bytes);
                    let label = Cow::Owned(format!("{label}{INVALID_UTF8_SUFFIX}"));
                    *self = Self::TextBytes(bytes_builder, label);
                }
            },
            Self::TextBytes(builder, _) | Self::Binary(builder, _) => builder.append_value(bytes),
            Self::Geometry { builder, scratch } => {
                scratch.clear();
                write_geometry(bytes, scratch);
                builder.append_value(&scratch);
            }
        }
        Ok(())
    }

    /// The finished array and the column's type label.
    fn finish(self) -> (ArrayRef, Cow<'static, str>) {
        match self {
            Self::Int(mut builder, label) => (Arc::new(builder.finish()), label),
            Self::UInt(mut builder, label) | Self::Bit(mut builder, label) => {
                (Arc::new(builder.finish()), label)
            }
            Self::Float(mut builder, label) => (Arc::new(builder.finish()), label),
            Self::Text(mut builder, label) => (Arc::new(builder.finish()), label),
            Self::TextBytes(mut builder, label) | Self::Binary(mut builder, label) => {
                (Arc::new(builder.finish()), label)
            }
            Self::Geometry { mut builder, .. } => (Arc::new(builder.finish()), "geometry".into()),
        }
    }
}

/// Big-endian bytes (a `BIT(n)` value, n <= 64) as an unsigned integer.
fn be_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > 8 {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0_u64, |value, byte| value << 8 | u64::from(*byte)),
    )
}

/// Writes the WKT of a MySQL/MariaDB internal geometry (4-byte little-endian
/// SRID, then WKB) as readable text (`POINT (1 2)`), `SRID=n;` first when
/// the SRID is not 0. Point, linestring, polygon, their multi- forms and
/// geometry collections are 2-D WKT; anything else (an unknown kind, a short or
/// trailing byte) is `WKB(0x...)` of the whole value.
fn write_geometry(value: &[u8], out: &mut String) {
    let start = out.len();
    let parsed = value.split_first_chunk::<4>().and_then(|(srid, wkb)| {
        let srid = u32::from_le_bytes(*srid);
        if srid != 0 {
            let _ = write!(out, "SRID={srid};");
        }
        let mut reader = Wkb { bytes: wkb };
        reader.geometry(out, 0, true)?;
        reader.bytes.is_empty().then_some(())
    });
    if parsed.is_none() {
        out.truncate(start);
        out.push_str("WKB(0x");
        for byte in value {
            let _ = write!(out, "{byte:02X}");
        }
        out.push(')');
    }
}

/// A cursor over WKB.
struct Wkb<'a> {
    bytes: &'a [u8],
}

/// Deepest geometry collection nesting read (a corrupt value cannot recurse far).
const MAX_WKB_DEPTH: usize = 16;

impl Wkb<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.bytes.split_first_chunk::<N>()?;
        self.bytes = rest;
        Some(*head)
    }

    fn u32(&mut self, little: bool) -> Option<u32> {
        let raw = self.take::<4>()?;
        Some(if little {
            u32::from_le_bytes(raw)
        } else {
            u32::from_be_bytes(raw)
        })
    }

    fn f64(&mut self, little: bool) -> Option<f64> {
        let raw = self.take::<8>()?;
        Some(if little {
            f64::from_le_bytes(raw)
        } else {
            f64::from_be_bytes(raw)
        })
    }

    /// An element count that the remaining bytes can hold at `min_size` each.
    fn count(&mut self, little: bool, min_size: usize) -> Option<usize> {
        let count = usize::try_from(self.u32(little)?).ok()?;
        (count.checked_mul(min_size)? <= self.bytes.len()).then_some(count)
    }

    fn point(&mut self, little: bool, out: &mut String) -> Option<()> {
        let (x, y) = (self.f64(little)?, self.f64(little)?);
        let _ = write!(out, "{x} {y}");
        Some(())
    }

    /// `(x y, x y, ...)` of a count-prefixed point sequence; `EMPTY` without points.
    fn points(&mut self, little: bool, out: &mut String) -> Option<()> {
        let count = self.count(little, 16)?;
        if count == 0 {
            out.push_str("EMPTY");
            return Some(());
        }
        out.push('(');
        for index in 0..count {
            if index > 0 {
                out.push_str(", ");
            }
            self.point(little, out)?;
        }
        out.push(')');
        Some(())
    }

    /// One geometry with its own byte-order mark and kind; `named` writes the kind
    /// word first (a member of a multi-geometry has none: `MULTIPOINT ((1 2))`).
    fn geometry(&mut self, out: &mut String, depth: usize, named: bool) -> Option<()> {
        let little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let kind = self.u32(little)?;
        let name = match kind {
            1 => "POINT",
            2 => "LINESTRING",
            3 => "POLYGON",
            4 => "MULTIPOINT",
            5 => "MULTILINESTRING",
            6 => "MULTIPOLYGON",
            7 => "GEOMETRYCOLLECTION",
            _ => return None,
        };
        if named {
            out.push_str(name);
            out.push(' ');
        }
        match kind {
            1 => {
                out.push('(');
                self.point(little, out)?;
                out.push(')');
            }
            2 => self.points(little, out)?,
            3 => self.rings(little, out)?,
            4..=7 => {
                if depth >= MAX_WKB_DEPTH {
                    return None;
                }
                let count = self.count(little, 9)?;
                if count == 0 {
                    out.push_str("EMPTY");
                    return Some(());
                }
                out.push('(');
                for index in 0..count {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    // A collection holds named geometries, a multi-geometry bare ones.
                    self.geometry(out, depth + 1, kind == 7)?;
                }
                out.push(')');
            }
            _ => return None,
        }
        Some(())
    }

    /// `((x y, ...), (x y, ...))` of a polygon; `EMPTY` without rings.
    fn rings(&mut self, little: bool, out: &mut String) -> Option<()> {
        let count = self.count(little, 4)?;
        if count == 0 {
            out.push_str("EMPTY");
            return Some(());
        }
        out.push('(');
        for index in 0..count {
            if index > 0 {
                out.push_str(", ");
            }
            self.points(little, out)?;
        }
        out.push(')');
        Some(())
    }
}

/// Parses an ASCII decimal integer without a UTF-8 round trip.
fn parse_i64(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes.split_first()? {
        (b'-', rest) => (true, rest),
        (b'+', rest) => (false, rest),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    // Accumulate negatively so i64::MIN parses.
    let mut value: i64 = 0;
    for byte in digits {
        let digit = i64::from(byte.checked_sub(b'0').filter(|d| *d <= 9)?);
        value = value.checked_mul(10)?.checked_sub(digit)?;
    }
    if negative {
        Some(value)
    } else {
        value.checked_neg()
    }
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for byte in bytes {
        let digit = u64::from(byte.checked_sub(b'0').filter(|d| *d <= 9)?);
        value = value.checked_mul(10)?.checked_add(digit)?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::DataType;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Float64Type, Int64Type, UInt64Type};
    use arrow_array::Array;

    fn column(column_type: ColumnType) -> Column {
        Column::new(column_type).with_name(b"c")
    }

    fn bytes(text: &str) -> Value {
        Value::Bytes(text.as_bytes().to_vec())
    }

    fn fill(sink: &mut Sink, values: &[Option<&str>]) {
        for value in values {
            let value = value.map(bytes);
            sink.push(value.as_ref()).expect("push");
        }
    }

    #[test]
    fn parses_integers_from_text_bytes() {
        assert_eq!(parse_i64(b"0"), Some(0));
        assert_eq!(parse_i64(b"-42"), Some(-42));
        assert_eq!(parse_i64(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse_i64(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse_i64(b"9223372036854775808"), None);
        assert_eq!(parse_i64(b""), None);
        assert_eq!(parse_i64(b"-"), None);
        assert_eq!(parse_i64(b"1.5"), None);
        assert_eq!(parse_u64(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_u64(b"18446744073709551616"), None);
        assert_eq!(parse_u64(b"-1"), None);
    }

    #[test]
    fn page_rows_defaults_and_caps() {
        assert_eq!(page_rows(0), 200);
        assert_eq!(page_rows(10), 10);
        assert_eq!(page_rows(100_000), 100_000);
        assert_eq!(page_rows(1_000_000), 100_000);
        assert_eq!(page_rows(u64::MAX), 100_000);
    }

    #[test]
    fn integers_floats_and_unsigned_keep_native_arrow_types() {
        let mut ints = Sink::new(&column(ColumnType::MYSQL_TYPE_LONG), 4);
        fill(&mut ints, &[Some("-7"), None, Some("9")]);
        let (array, label) = ints.finish();
        assert_eq!(label, "int");
        assert_eq!(array.as_primitive::<Int64Type>().value(0), -7);
        assert!(array.is_null(1));

        let unsigned =
            column(ColumnType::MYSQL_TYPE_LONGLONG).with_flags(ColumnFlags::UNSIGNED_FLAG);
        let mut big = Sink::new(&unsigned, 4);
        fill(&mut big, &[Some("18446744073709551615")]);
        let (array, label) = big.finish();
        assert_eq!(label, "bigint unsigned");
        assert_eq!(array.as_primitive::<UInt64Type>().value(0), u64::MAX);

        let mut floats = Sink::new(&column(ColumnType::MYSQL_TYPE_DOUBLE), 4);
        fill(&mut floats, &[Some("1.25"), Some("-1e-3")]);
        let (array, label) = floats.finish();
        assert_eq!(label, "double");
        assert_eq!(array.as_primitive::<Float64Type>().value(1), -0.001);
    }

    #[test]
    fn decimal_datetime_and_json_stay_text_exactly_as_the_server_printed_them() {
        for (column_type, label, text) in [
            (
                ColumnType::MYSQL_TYPE_NEWDECIMAL,
                "decimal",
                "12345678901234567890.123456789",
            ),
            (
                ColumnType::MYSQL_TYPE_DATETIME,
                "datetime",
                "2024-02-29 23:59:59.500",
            ),
            (ColumnType::MYSQL_TYPE_JSON, "json", "{\"a\": [1, 2]}"),
            (ColumnType::MYSQL_TYPE_TIME, "time", "-838:59:59"),
        ] {
            let mut sink = Sink::new(&column(column_type), 2);
            fill(&mut sink, &[Some(text), None]);
            let (array, found) = sink.finish();
            assert_eq!(found, label);
            assert_eq!(array.data_type(), &DataType::Utf8);
            assert_eq!(array.as_string::<i32>().value(0), text);
            assert!(array.is_null(1));
        }
    }

    #[test]
    fn tinyint_one_is_always_an_integer_whatever_the_page_holds() {
        // TINYINT(1) is a number, also when the
        // page happens to hold only 0, 1 and NULL.
        let flag = column(ColumnType::MYSQL_TYPE_TINY).with_column_length(1);
        for values in [
            vec![Some("1"), Some("0"), None],
            vec![Some("1"), Some("5")],
            vec![],
        ] {
            let mut sink = Sink::new(&flag, 4);
            fill(&mut sink, &values);
            let (array, label) = sink.finish();
            assert_eq!(label, "tinyint");
            assert_eq!(array.data_type(), &DataType::Int64);
        }
        let mut sink = Sink::new(&flag, 4);
        fill(&mut sink, &[Some("1"), Some("-128")]);
        let (array, _) = sink.finish();
        assert_eq!(array.as_primitive::<Int64Type>().value(0), 1);
        assert_eq!(array.as_primitive::<Int64Type>().value(1), -128);
    }

    #[test]
    fn bit_is_decoded_from_big_endian_bytes_to_an_unsigned_integer() {
        let bit = |width| column(ColumnType::MYSQL_TYPE_BIT).with_column_length(width);
        assert_eq!(type_label(&bit(8)), "bit(8)");
        let mut sink = Sink::new(&bit(1), 4);
        for value in [Some(&[1_u8][..]), Some(&[0][..]), None] {
            let value = value.map(|bytes| Value::Bytes(bytes.to_vec()));
            sink.push(value.as_ref()).unwrap();
        }
        let (array, label) = sink.finish();
        assert_eq!(label, "bit(1)");
        assert_eq!(array.data_type(), &DataType::UInt64);
        assert_eq!(array.as_primitive::<UInt64Type>().value(0), 1);
        assert_eq!(array.as_primitive::<UInt64Type>().value(1), 0);
        assert!(array.is_null(2));
        // 65 is the byte `A`: it is the number 65, not text.
        let mut sink = Sink::new(&bit(8), 1);
        sink.push(Some(&Value::Bytes(vec![0x41]))).unwrap();
        assert_eq!(sink.finish().0.as_primitive::<UInt64Type>().value(0), 65);
        let mut sink = Sink::new(&bit(64), 2);
        sink.push(Some(&Value::Bytes(vec![0xff; 8]))).unwrap();
        sink.push(Some(&Value::Bytes(vec![0x01, 0x00, 0x00])))
            .unwrap();
        let (array, _) = sink.finish();
        assert_eq!(array.as_primitive::<UInt64Type>().value(0), u64::MAX);
        assert_eq!(array.as_primitive::<UInt64Type>().value(1), 65_536);
        let mut sink = Sink::new(&bit(64), 1);
        assert!(sink.push(Some(&Value::Bytes(vec![0; 9]))).is_err());
    }

    fn wkt(value: &[u8]) -> String {
        let mut out = String::new();
        write_geometry(value, &mut out);
        out
    }

    /// SRID (little-endian) + little-endian WKB, built from f64 coordinates.
    fn wkb(srid: u32, kind: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = srid.to_le_bytes().to_vec();
        bytes.push(1);
        bytes.extend(kind.to_le_bytes());
        bytes.extend(body);
        bytes
    }

    fn coordinates(points: &[(f64, f64)]) -> Vec<u8> {
        let mut bytes = (points.len() as u32).to_le_bytes().to_vec();
        for (x, y) in points {
            bytes.extend(x.to_le_bytes());
            bytes.extend(y.to_le_bytes());
        }
        bytes
    }

    fn member(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![1];
        bytes.extend(kind.to_le_bytes());
        bytes.extend(body);
        bytes
    }

    #[test]
    fn geometry_is_shown_as_wkt_with_the_srid() {
        let point = |x: f64, y: f64| [x.to_le_bytes(), y.to_le_bytes()].concat();
        assert_eq!(wkt(&wkb(0, 1, &point(1.0, 2.0))), "POINT (1 2)");
        assert_eq!(
            wkt(&wkb(4326, 1, &point(1.5, -2.25))),
            "SRID=4326;POINT (1.5 -2.25)"
        );
        let line = coordinates(&[(0.0, 0.0), (1.0, 1.0), (2.0, 0.5)]);
        assert_eq!(wkt(&wkb(0, 2, &line)), "LINESTRING (0 0, 1 1, 2 0.5)");
        assert_eq!(wkt(&wkb(0, 2, &coordinates(&[]))), "LINESTRING EMPTY");
        let ring = coordinates(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 0.0)]);
        let hole = coordinates(&[(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 1.0)]);
        let polygon = [2_u32.to_le_bytes().to_vec(), ring.clone(), hole.clone()].concat();
        assert_eq!(
            wkt(&wkb(0, 3, &polygon)),
            "POLYGON ((0 0, 4 0, 4 4, 0 0), (1 1, 2 1, 2 2, 1 1))"
        );
        let multipoint = [
            2_u32.to_le_bytes().to_vec(),
            member(1, &point(1.0, 2.0)),
            member(1, &point(3.0, 4.0)),
        ]
        .concat();
        assert_eq!(wkt(&wkb(0, 4, &multipoint)), "MULTIPOINT ((1 2), (3 4))");
        let multiline = [
            1_u32.to_le_bytes().to_vec(),
            member(2, &coordinates(&[(0.0, 0.0), (1.0, 1.0)])),
        ]
        .concat();
        assert_eq!(wkt(&wkb(0, 5, &multiline)), "MULTILINESTRING ((0 0, 1 1))");
        let multipolygon = [
            1_u32.to_le_bytes().to_vec(),
            member(3, &[1_u32.to_le_bytes().to_vec(), ring].concat()),
        ]
        .concat();
        assert_eq!(
            wkt(&wkb(0, 6, &multipolygon)),
            "MULTIPOLYGON (((0 0, 4 0, 4 4, 0 0)))"
        );
        let collection = [
            2_u32.to_le_bytes().to_vec(),
            member(1, &point(1.0, 2.0)),
            member(2, &coordinates(&[(0.0, 0.0), (1.0, 1.0)])),
        ]
        .concat();
        assert_eq!(
            wkt(&wkb(0, 7, &collection)),
            "GEOMETRYCOLLECTION (POINT (1 2), LINESTRING (0 0, 1 1))"
        );
        assert_eq!(
            wkt(&wkb(0, 7, &0_u32.to_le_bytes())),
            "GEOMETRYCOLLECTION EMPTY"
        );
        // Big-endian WKB is read too.
        let mut big = 0_u32.to_le_bytes().to_vec();
        big.push(0);
        big.extend(1_u32.to_be_bytes());
        big.extend(7.0_f64.to_be_bytes());
        big.extend(8.0_f64.to_be_bytes());
        assert_eq!(wkt(&big), "POINT (7 8)");
    }

    #[test]
    fn geometry_that_is_not_understood_falls_back_to_marked_hex() {
        // An unknown kind (a 3-D point, 1001), a truncated value, trailing bytes,
        // a bad byte-order mark, a value shorter than the SRID.
        let mut unknown = wkb(0, 1001, &[0; 24]);
        assert!(wkt(&unknown).starts_with("WKB(0x00000000"));
        unknown.truncate(8);
        assert_eq!(wkt(&unknown), "WKB(0x0000000001E90300)");
        let mut trailing = wkb(0, 1, &[0; 16]);
        trailing.push(0);
        assert!(wkt(&trailing).starts_with("WKB(0x"));
        let mut bad_order = wkb(0, 1, &[0; 16]);
        bad_order[4] = 9;
        assert!(wkt(&bad_order).starts_with("WKB(0x"));
        assert_eq!(wkt(&[1, 2]), "WKB(0x0102)");
        // A count that the bytes cannot hold is refused, not allocated.
        assert!(wkt(&wkb(0, 2, &u32::MAX.to_le_bytes())).starts_with("WKB(0x"));
        let mut sink = Sink::new(&column(ColumnType::MYSQL_TYPE_GEOMETRY), 2);
        sink.push(Some(&Value::Bytes(wkb(0, 1, &[0; 16])))).unwrap();
        sink.push(None).unwrap();
        let (array, label) = sink.finish();
        assert_eq!(label, "geometry");
        assert_eq!(array.as_string::<i32>().value(0), "POINT (0 0)");
        assert!(array.is_null(1));
    }

    #[test]
    fn text_that_is_not_utf8_is_delivered_as_bytes_not_repaired() {
        let text = column(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(255);
        let mut sink = Sink::new(&text, 4);
        sink.push(Some(&bytes("fine"))).unwrap();
        sink.push(None).unwrap();
        // 0xE9 is latin1 `é`, not UTF-8: never replaced by U+FFFD.
        sink.push(Some(&Value::Bytes(vec![b'c', b'a', b'f', 0xe9])))
            .unwrap();
        sink.push(Some(&bytes("after"))).unwrap();
        let (array, label) = sink.finish();
        assert_eq!(label, "varchar (invalid utf-8)");
        assert_eq!(array.data_type(), &DataType::Binary);
        let values = array.as_binary::<i32>();
        assert_eq!(values.value(0), b"fine");
        assert!(values.is_null(1));
        assert_eq!(values.value(2), &[b'c', b'a', b'f', 0xe9]);
        assert_eq!(values.value(3), b"after");
        // A page of valid text is untouched.
        let mut sink = Sink::new(&text, 2);
        sink.push(Some(&bytes("caf\u{e9}"))).unwrap();
        let (array, label) = sink.finish();
        assert_eq!(
            (label.as_ref(), array.data_type()),
            ("varchar", &DataType::Utf8)
        );
    }

    #[test]
    fn binary_charset_blobs_are_binary_and_text_blobs_are_text() {
        let blob = column(ColumnType::MYSQL_TYPE_BLOB)
            .with_character_set(BINARY_CHARSET)
            .with_flags(ColumnFlags::BINARY_FLAG);
        assert_eq!(kind_of(&blob), Kind::Binary);
        assert_eq!(type_label(&blob), "blob");
        let mut sink = Sink::new(&blob, 2);
        sink.push(Some(&Value::Bytes(vec![0, 255]))).unwrap();
        assert_eq!(sink.finish().0.as_binary::<i32>().value(0), &[0, 255]);

        let text = column(ColumnType::MYSQL_TYPE_BLOB).with_character_set(45);
        assert_eq!(kind_of(&text), Kind::Text);
        assert_eq!(type_label(&text), "text");

        let enumeration = column(ColumnType::MYSQL_TYPE_STRING)
            .with_character_set(BINARY_CHARSET)
            .with_flags(ColumnFlags::ENUM_FLAG);
        assert_eq!(kind_of(&enumeration), Kind::Text);
    }

    #[test]
    fn rejects_values_that_do_not_match_the_column_type() {
        let mut sink = Sink::new(&column(ColumnType::MYSQL_TYPE_LONG), 1);
        assert!(sink.push(Some(&bytes("abc"))).is_err());
        assert!(sink.push(Some(&Value::Int(3))).is_err());
    }
}
