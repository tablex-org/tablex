//! Query execution under the shared SQL contract (`engine_driver_api::sql`).
//!
//! * `engine_driver_kit::sql::classify` (ClickHouse lexer options) decides what
//!   the SQL is. Only a single pure read is pageable: a `SELECT`/`WITH` gets
//!   `LIMIT n+1 OFFSET m` appended server-side when it has no `LIMIT`,
//!   `SETTINGS` or `FORMAT` clause of its own; anything else pageable (`SHOW`,
//!   `DESCRIBE`, `EXPLAIN`, a `SELECT` with its own `LIMIT`) is paged here by
//!   skipping rows as they arrive, and `max_result_rows` + `result_overflow_mode
//!   = break` stops the server shortly after the last row that is needed.
//!   Anything else runs once: `offset > 0` is refused with `query.not_pageable`.
//! * `limit == 0` is the default page of 200 rows, pages are capped at 100 000
//!   rows, and at most one page (+1 lookahead row) is ever held. A reply larger
//!   than 512 MiB on the wire fails with `query.reply_too_large`.
//! * The HTTP interface runs one statement per request: a script is split with
//!   the classifier and its statements run in order, in one HTTP session
//!   (`session_id`, so `SET` and temporary tables carry over), each exactly
//!   once. The page is the LAST statement that returned a result set.
//! * Dangerous statements need `payload.confirmed == true`
//!   (`query.confirmation_required`, message = reason): the classifier's
//!   (`DROP`, `TRUNCATE`, `ALTER`, `GRANT`, `DELETE`/`UPDATE` without `WHERE`,
//!   `CREATE DATABASE`) plus `OPTIMIZE ... FINAL`, `DETACH`, `RENAME`,
//!   `EXCHANGE`, `KILL` and `SYSTEM`.
//! * Every statement gets its own `query_id`; cancel is `KILL QUERY` on another
//!   connection (see `lib.rs`).
//! * Results are read as `FORMAT ArrowStream` straight into Arrow batches (no
//!   row-by-row conversion); see `types.rs` for what is converted afterwards.
//! * Column types come from `DESCRIBE` when the Arrow types are ambiguous. It
//!   runs *inside the statement's HTTP session* (temporary tables and `SET`
//!   apply) once the statement's reply has been read (a session serves one
//!   request at a time). The first page of a statement always describes it
//!   again; the later pages of the same statement reuse that answer. The
//!   answers are dropped after every statement that is not a plain read (DDL,
//!   `ALTER`, `SET`, `USE`, `INSERT` ...) and after every driver mutation.
//! * Types the Arrow format cannot express (`AggregateFunction`, `JSON`,
//!   `Dynamic`, `Nothing`, `BFloat16` ...) make the server refuse the query
//!   (`output_format_arrow_unsupported_types_as_binary = 0`; a `Variant` is
//!   noticed in the Arrow schema). The statement then runs again as
//!   `SELECT a, toString(b) AS b FROM (<statement>)`: only those columns are the
//!   server's own text, labelled `<type> (read-only)`; everything else is
//!   unchanged. If the statement cannot be wrapped (duplicate column names) the
//!   server's error is returned.

use crate::http::{code, Area, ArrowBody, HttpError, Plane, Request};
use crate::sql::{extra_danger, scan, with_limit};
use crate::types::{self, ChType, Described};
use crate::{ChConnection, UserSession};
use arrow_array::RecordBatch;
use arrow_select::concat::concat_batches;
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::{PageColumn, PageFlags, QueryRequest};
use engine_driver_kit::sql::{classify, Classification, Keyword, SqlLexOptions};
use engine_driver_kit::{ActiveOperation, BackendError, OperationGuard, QueryOutput};
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

/// Rows of a page when the request says `limit: 0`.
const DEFAULT_PAGE_ROWS: u64 = 200;
/// No page is larger than this, whatever the request asks.
const MAX_PAGE_ROWS: u64 = 100_000;

/// The running operation together with the connection's user session.
pub(crate) type Active<'a, 'b> = ActiveOperation<'a, MutexGuard<'b, UserSession>>;

/// The page size a request's `limit` stands for.
pub(crate) fn page_rows(limit: u64) -> usize {
    let rows = match limit {
        0 => DEFAULT_PAGE_ROWS,
        limit => limit.min(MAX_PAGE_ROWS),
    };
    rows as usize
}

/// How the rows of one statement are cut into a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A pageable read the server already cut with `LIMIT take+1 OFFSET skip`.
    ServerPaged { take: usize },
    /// A pageable read paged here: skip `skip` rows, keep `take`, read one more.
    ClientPaged { skip: usize, take: usize },
    /// Anything else: keep the first `take` rows; there is never a next page.
    Once { take: usize },
}

impl Mode {
    fn skip(self) -> usize {
        match self {
            Self::ClientPaged { skip, .. } => skip,
            _ => 0,
        }
    }

    fn take(self) -> usize {
        match self {
            Self::ServerPaged { take } | Self::ClientPaged { take, .. } | Self::Once { take } => {
                take
            }
        }
    }

    fn pageable(self) -> bool {
        !matches!(self, Self::Once { .. })
    }

    /// Rows to read, including the lookahead row of a pageable read.
    fn want(self) -> usize {
        self.skip()
            .saturating_add(self.take())
            .saturating_add(usize::from(self.pageable()))
    }

    /// The server may send more rows than `want`, so reading stops early and the
    /// server is told to stop too (`max_result_rows`).
    fn capped(self) -> bool {
        !matches!(self, Self::ServerPaged { .. })
    }
}

/// One statement ready to send.
struct Step {
    /// The text sent (paged if the server pages it).
    sql: String,
    /// The statement as the user wrote it, for `DESCRIBE`.
    base: String,
    mode: Mode,
    /// A `SELECT`-like statement: `DESCRIBE` can name its column types.
    describable: bool,
    /// The request's offset: page 0 describes the statement afresh.
    offset: u64,
    /// Result columns fetched as the server's own text (see the module docs).
    server_text: Vec<usize>,
    /// The statement's column names and types, when they are already known.
    described: Option<Arc<Described>>,
}

/// What one statement produced.
enum Outcome {
    Rows {
        batch: RecordBatch,
        columns: Vec<PageColumn>,
        truncated: bool,
    },
    Command {
        written_rows: u64,
    },
}

fn refused(code: &'static str, message: impl Into<String>) -> BackendError {
    BackendError::with_code(code, message)
}

/// The classifier's danger plus the ClickHouse-only statements.
fn danger_of(sql: &str, classification: &Classification) -> Option<&'static str> {
    if let Some(danger) = classification.danger {
        return Some(danger.reason);
    }
    classification.statements.iter().find_map(|statement| {
        let first = statement.keyword_range.clone()?;
        extra_danger(&sql[first], &sql[statement.range.clone()])
    })
}

fn plan(
    sql: &str,
    classification: &Classification,
    index: usize,
    offset: u64,
    take: usize,
    options: &SqlLexOptions,
) -> Step {
    let statement = &classification.statements[index];
    let text = sql[statement.range.clone()].trim().to_owned();
    let reads = matches!(
        statement.keyword,
        Some(Keyword::Select | Keyword::With | Keyword::Values | Keyword::Table)
    );
    let describable = reads;
    if !classification.pageable {
        return Step {
            sql: text.clone(),
            base: text,
            mode: Mode::Once { take },
            describable,
            offset,
            server_text: Vec::new(),
            described: None,
        };
    }
    // SHOW, DESCRIBE and EXPLAIN have no LIMIT clause, a SELECT with its own
    // LIMIT/SETTINGS/FORMAT cannot take another one: skip here instead.
    if reads && scan(&text, options).accepts_limit() {
        return Step {
            sql: with_limit(&text, offset, take as u64),
            base: text,
            mode: Mode::ServerPaged { take },
            describable,
            offset,
            server_text: Vec::new(),
            described: None,
        };
    }
    Step {
        sql: text.clone(),
        base: text,
        mode: Mode::ClientPaged {
            skip: usize::try_from(offset).unwrap_or(usize::MAX),
            take,
        },
        describable,
        offset,
        server_text: Vec::new(),
        described: None,
    }
}

/// Runs `request` on the connection and returns the page.
pub(crate) fn open_query(
    connection: &ChConnection,
    request: &engine_driver_kit::api::QueryRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<QueryOutput, BackendError> {
    let sql = parse_request(request)?;
    let options = SqlLexOptions::clickhouse();
    let classification = classify(sql, &options);
    if classification.is_empty() {
        return Err(refused(
            "query.bad_request",
            "the SQL contains no statement",
        ));
    }
    // Additive (capability `query.read_only`): this statement runs with the
    // per-query setting `readonly = 2` (reads only; settings may still change).
    let read_only = request
        .payload
        .get("read_only")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    // Every statement is sent on its own with `readonly = 2`, so a script cannot loosen
    // it; the shared rule still applies: nothing that controls a transaction, and a
    // script of plain reads only.
    if let (true, Some(refusal)) = (read_only, classification.read_only_refusal()) {
        return Err(refusal.into());
    }
    let confirmed = request
        .payload
        .get("confirmed")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if let (Some(reason), false) = (danger_of(sql, &classification), confirmed) {
        return Err(refused(codes::QUERY_CONFIRMATION_REQUIRED, reason));
    }
    if request.offset > 0 && !classification.pageable {
        return Err(refused(
            codes::QUERY_NOT_PAGEABLE,
            "only a single read (SELECT, WITH, SHOW, DESCRIBE, EXPLAIN) can be paged; \
             this statement runs once, at offset 0",
        ));
    }
    connection.check_open(Area::Query)?;
    let take = page_rows(request.limit);
    let guard = connection.lock(Area::Query)?;
    // `active` owns the session guard: the operation stops being active before
    // the connection is released.
    let active = operation.activate(guard)?;
    if classification.changes_database {
        // `USE` moved the session: stop forcing the URI's database.
        active.database_unknown.set(true);
    }
    let database = connection.database_for(&active, request.database.as_deref());
    let mut rows: Option<Outcome> = None;
    let mut written = 0_u64;
    for index in 0..classification.statements.len() {
        if active.cancel_requested() {
            return Err(cancelled());
        }
        let step = plan(sql, &classification, index, request.offset, take, &options);
        let query_id = connection.query_id(active.operation_id(), index);
        // Ids past the script's own statements: the statement may run twice.
        let retry_id = connection.query_id(
            active.operation_id(),
            classification.statements.len() + index,
        );
        let outcome = run_statement(
            connection,
            &active,
            &step,
            [&query_id, &retry_id],
            database.as_deref(),
            read_only,
        );
        // Whatever a statement that is not a plain read did (DDL, `ALTER`, `SET`,
        // `USE` ...), column types described before it may be wrong now. Cleared
        // even when it failed: it may have changed things before it did.
        if !classification.statements[index].read_only {
            active.types.borrow_mut().clear();
        }
        match outcome? {
            outcome @ Outcome::Rows { .. } => rows = Some(outcome),
            Outcome::Command { written_rows } => written += written_rows,
        }
    }
    match rows {
        // A read that "succeeded" while a cancel was requested (the kill raced
        // the last block) did not produce the rows the caller asked for.
        Some(_) if active.cancel_requested() => Err(cancelled()),
        Some(Outcome::Rows {
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
        _ => Ok(QueryOutput::command_complete(written, ())),
    }
}

fn parse_request(request: &QueryRequest) -> Result<&str, BackendError> {
    if request.kind != "sql" && request.kind != "sql_query" {
        return Err(refused("query.unsupported", "driver accepts kind=sql"));
    }
    let sql = request
        .payload
        .get("sql")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if sql.trim().is_empty() {
        return Err(refused("query.bad_request", "missing SQL payload"));
    }
    Ok(sql)
}

fn cancelled() -> BackendError {
    BackendError::with_code(codes::QUERY_CANCELLED, "operation cancelled")
}

/// Sends one statement and collects its page. A type the Arrow format cannot
/// express makes it run once more, under the second of `query_ids` (the first
/// may still be winding down on the server), with those columns as the
/// server's text.
fn run_statement(
    connection: &ChConnection,
    active: &Active<'_, '_>,
    step: &Step,
    [query_id, retry_id]: [&str; 2],
    database: Option<&str>,
    read_only: bool,
) -> Result<Outcome, BackendError> {
    // Mark first, then look at the cancel flag: a cancel that arrives before the
    // mark has set the flag by now, one that arrives after finds the id.
    let _mark = connection.mark(query_id);
    if active.cancel_requested() {
        return Err(cancelled());
    }
    let mut result = attempt_adapting(connection, active, step, query_id, database, read_only);
    if matches!(&result, Err(error) if step.describable && error.is_unsupported_arrow_type()) {
        if let Some(text) = text_step(connection, active, step, database) {
            let _retry = connection.mark(retry_id);
            if active.cancel_requested() {
                return Err(cancelled());
            }
            match attempt_adapting(connection, active, &text, retry_id, database, read_only) {
                Ok(outcome) => result = Ok(outcome),
                // A cancel is the answer, not the first statement's refusal.
                Err(_) if active.cancel_requested() => return Err(cancelled()),
                // Otherwise the server's first refusal is the one worth showing.
                Err(_) => {}
            }
        }
    }
    result.map_err(|error| {
        connection
            .abandoned(Area::Query)
            .unwrap_or_else(|| error.into_backend(Area::Query, active.cancel_requested()))
    })
}

/// [`attempt`], sent again without what the server refuses to take: the
/// `readonly` setting and the result cap in a `readonly` profile (that profile
/// is read-only already, which is what was asked for; a refused *query* says
/// "Cannot execute query in readonly mode" and is returned as the error it is),
/// and the optional Arrow output settings on a server that does not know them.
/// Each refusal is remembered, so each retry happens at most once.
fn attempt_adapting(
    connection: &ChConnection,
    active: &Active<'_, '_>,
    step: &Step,
    query_id: &str,
    database: Option<&str>,
    mut read_only: bool,
) -> Result<Outcome, HttpError> {
    let client = &connection.client;
    loop {
        let result = attempt(connection, active, step, query_id, database, read_only);
        let Err(error) = &result else {
            return result;
        };
        if read_only && error.is_readonly_setting("'readonly'") {
            read_only = false;
        } else if client.result_cap_allowed()
            && (error.is_readonly_setting("max_result_rows")
                || error.is_readonly_setting("result_overflow_mode"))
        {
            client.refuse_result_cap();
        } else if client.arrow_extras_allowed()
            && ARROW_EXTRAS
                .iter()
                .any(|(name, _)| error.is_unknown_setting(name))
        {
            client.refuse_arrow_extras();
        } else {
            return result;
        }
    }
}

/// Arrow output settings that pin the types the driver converts. The server's
/// defaults are the same except for the second one (which turns a type Arrow
/// cannot express into an error the driver can answer, instead of raw bytes
/// that look like a value); a profile may differ, an older server does not
/// know them.
const ARROW_EXTRAS: [(&str, &str); 2] = [
    ("output_format_arrow_date_as_uint16", "0"),
    ("output_format_arrow_unsupported_types_as_binary", "0"),
];

fn attempt(
    connection: &ChConnection,
    active: &Active<'_, '_>,
    step: &Step,
    query_id: &str,
    database: Option<&str>,
    read_only: bool,
) -> Result<Outcome, HttpError> {
    let client = &connection.client;
    let mut settings: Vec<(&str, String)> = vec![
        ("output_format_arrow_compression_method", "none".into()),
        // A ClickHouse String may hold any bytes; Arrow Utf8 must not.
        ("output_format_arrow_string_as_string", "0".into()),
        (
            "output_format_arrow_fixed_string_as_fixed_byte_array",
            "1".into(),
        ),
        (
            "output_format_arrow_low_cardinality_as_dictionary",
            "0".into(),
        ),
        ("cancel_http_readonly_queries_on_client_close", "1".into()),
    ];
    if client.arrow_extras_allowed() {
        settings.extend(
            ARROW_EXTRAS
                .iter()
                .map(|(name, value)| (*name, (*value).to_owned())),
        );
    }
    if read_only {
        settings.push(("readonly", "2".into()));
    }
    if step.mode.capped() && client.result_cap_allowed() {
        settings.push(("max_result_rows", step.mode.want().to_string()));
        settings.push(("result_overflow_mode", "break".into()));
    }
    let reply = client.send(&Request {
        sql: &step.sql,
        format: Some("ArrowStream"),
        query_id: Some(query_id),
        session_id: Some(&active.id),
        database,
        settings: &settings,
        plane: Plane::Query,
        ..Request::default()
    })?;
    let written_rows = reply.written_rows;
    let Some(mut body) = reply.arrow()? else {
        return Ok(Outcome::Command {
            written_rows: written_rows.unwrap_or(0),
        });
    };
    if types::has_union(&body.schema()) && step.server_text.is_empty() {
        // A `Variant`: shown as the server's text. Stop this reply (and the
        // statement) first; the session is busy until the server has.
        drop(body);
        let _ = client.kill_query(query_id);
        return Err(HttpError::NeedsText);
    }
    let (slices, ended) = read_window(&mut body, step.mode)?;
    if ended {
        body.finish();
    } else {
        // Stopped early: the connection is dropped; make sure the server stops too.
        drop(body);
        let _ = client.kill_query(query_id);
    }
    let kept: usize = slices.iter().map(RecordBatch::num_rows).sum();
    let take = step.mode.take();
    let truncated = step.mode.pageable() && kept > take;
    let schema = slices
        .first()
        .map(RecordBatch::schema)
        .ok_or_else(|| HttpError::Protocol("the reply carried no schema".into()))?;
    let batch = if slices.len() == 1 {
        slices[0].clone()
    } else {
        concat_batches(&schema, &slices).map_err(|error| HttpError::Protocol(error.to_string()))?
    };
    let batch = batch.slice(0, kept.min(take));
    // The statement's reply is complete: its session can describe it now.
    let described = match &step.described {
        Some(described) => Some(Arc::clone(described)),
        None if step.describable
            && types::needs_types(
                &schema,
                !client.arrow_extras_allowed() || sets_arrow_output(&step.base),
            ) =>
        {
            describe(
                connection,
                active,
                step,
                database,
                Some(schema.fields().len()),
            )
        }
        None => None,
    };
    let types = described.as_deref().map(|described| &described.types[..]);
    let converted = types::convert_batch(&batch, types, &step.server_text)
        .map_err(|error| HttpError::Protocol(format!("cannot convert the result: {error}")))?;
    let columns = columns_of(&converted, types);
    Ok(Outcome::Rows {
        batch: converted.batch,
        columns,
        truncated,
    })
}

/// The rows of the page window `skip .. skip + want`, as slices of the batches
/// they came in. `true` when the stream ended (so its trailer can be read and
/// the connection reused). Always returns at least one (possibly empty) batch
/// so the schema survives an empty result.
fn read_window(body: &mut ArrowBody, mode: Mode) -> Result<(Vec<RecordBatch>, bool), HttpError> {
    let (skip, window_end) = (mode.skip(), mode.want());
    let mut slices = Vec::new();
    let mut seen = 0_usize;
    let mut ended = true;
    while let Some(batch) = body.next_batch()? {
        let rows = batch.num_rows();
        let (from, to) = (skip.max(seen), window_end.min(seen + rows));
        if from < to {
            slices.push(batch.slice(from - seen, to - from));
        }
        seen += rows;
        if seen >= window_end && mode.capped() {
            ended = false;
            break;
        }
    }
    if slices.is_empty() {
        slices.push(RecordBatch::new_empty(body.schema()));
    }
    Ok((slices, ended))
}

fn columns_of(converted: &types::Converted, types: Option<&[ChType]>) -> Vec<PageColumn> {
    converted
        .batch
        .schema()
        .fields()
        .iter()
        .zip(&converted.notes)
        .enumerate()
        .map(|(index, (field, note))| PageColumn {
            id: index.to_string(),
            name: field.name().clone(),
            data_type: note.label(
                types
                    .and_then(|types| types.get(index))
                    .map_or_else(|| types::arrow_label(field.data_type()), ChType::label),
            ),
            nullable: field.is_nullable(),
        })
        .collect()
}

/// Whether the statement text sets an Arrow output option itself (`SETTINGS
/// output_format_arrow_date_as_uint16 = 1`), which beats the driver's own.
fn sets_arrow_output(statement: &str) -> bool {
    const NEEDLE: &[u8] = b"output_format_arrow";
    statement
        .as_bytes()
        .windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

/// How often and how long `DESCRIBE` waits for a session that the server has
/// not released yet (the previous reply of the session was cut short).
const SESSION_RETRIES: u32 = 40;
const SESSION_RETRY: Duration = Duration::from_millis(25);

/// The column names and ClickHouse types of `step`'s statement, from
/// `DESCRIBE` in the statement's session. Page 0 describes afresh; later pages
/// of the same statement reuse the answer (the cache is cleared by anything
/// that is not a plain read). `columns` is the width of the reply the types are
/// for, when there is one. `None` if the server cannot describe the statement
/// (a `SHOW`); the conversion then falls back to the type-independent rules.
fn describe(
    connection: &ChConnection,
    active: &Active<'_, '_>,
    step: &Step,
    database: Option<&str>,
    columns: Option<usize>,
) -> Option<Arc<Described>> {
    let key = format!("{}\0{}", database.unwrap_or_default(), step.base);
    let fits = |described: &Described| columns.is_none_or(|width| described.types.len() == width);
    if step.offset > 0 {
        if let Some(described) = active.types.borrow().get(&key).filter(|d| fits(d)) {
            return Some(described);
        }
    }
    let sql = format!("DESCRIBE TABLE (\n{}\n)", step.base);
    let mut tries = 0;
    let rows = loop {
        match connection
            .client
            .rows_in_session(&sql, database, &active.id)
        {
            Err(error)
                if error.server_code() == Some(code::SESSION_IS_LOCKED)
                    && tries < SESSION_RETRIES =>
            {
                tries += 1;
                std::thread::sleep(SESSION_RETRY);
            }
            other => break other.ok()?,
        }
    };
    let described = Arc::new(Described::from_rows(&rows).filter(|d| fits(d))?);
    active.types.borrow_mut().put(key, Arc::clone(&described));
    Some(described)
}

/// The statement again with the columns the Arrow format cannot carry replaced
/// by the server's text: `SELECT a, toString(b) AS b FROM (<statement>)`.
/// `None` when nothing needs it or the statement cannot be wrapped.
fn text_step(
    connection: &ChConnection,
    active: &Active<'_, '_>,
    step: &Step,
    database: Option<&str>,
) -> Option<Step> {
    let described = match &step.described {
        Some(described) => Arc::clone(described),
        None => describe(connection, active, step, database, None)?,
    };
    let (sql, server_text) = described.wrap_for_text(&step.base)?;
    let sent = match step.mode {
        Mode::ServerPaged { take } => with_limit(&sql, step.offset, take as u64),
        _ => sql,
    };
    Some(Step {
        sql: sent,
        base: step.base.clone(),
        mode: step.mode,
        describable: step.describable,
        offset: step.offset,
        server_text,
        described: Some(described),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(sql: &str, offset: u64, take: usize) -> Step {
        let options = SqlLexOptions::clickhouse();
        let classification = classify(sql, &options);
        plan(sql, &classification, 0, offset, take, &options)
    }

    #[test]
    fn a_statement_that_sets_arrow_output_asks_for_types() {
        assert!(sets_arrow_output(
            "SELECT d FROM t SETTINGS OUTPUT_FORMAT_ARROW_DATE_AS_UINT16 = 1"
        ));
        assert!(!sets_arrow_output(
            "SELECT d FROM t SETTINGS max_threads = 1"
        ));
    }

    #[test]
    fn page_sizes_default_and_cap() {
        assert_eq!(page_rows(0), 200);
        assert_eq!(page_rows(7), 7);
        assert_eq!(page_rows(100_000), 100_000);
        assert_eq!(page_rows(u64::MAX), 100_000);
    }

    #[test]
    fn a_plain_select_is_paged_by_the_server() {
        let planned = step("SELECT n FROM t ORDER BY n", 20, 10);
        assert_eq!(
            planned.sql,
            "SELECT n FROM t ORDER BY n\nLIMIT 11 OFFSET 20"
        );
        assert_eq!(planned.mode, Mode::ServerPaged { take: 10 });
        assert!(planned.describable);
        let planned = step("WITH x AS (SELECT 1) SELECT * FROM x -- hi", 0, 5);
        assert_eq!(
            planned.sql,
            "WITH x AS (SELECT 1) SELECT * FROM x -- hi\nLIMIT 6"
        );
    }

    #[test]
    fn reads_that_cannot_take_a_limit_are_paged_here() {
        for sql in [
            "SELECT * FROM t LIMIT 5",
            "SELECT * FROM t SETTINGS max_threads = 1",
            "SELECT 1 FORMAT JSON",
            "SHOW TABLES",
            "DESCRIBE TABLE t",
            "EXPLAIN SELECT 1",
        ] {
            let planned = step(sql, 3, 10);
            assert_eq!(planned.sql, sql, "{sql}");
            assert_eq!(
                planned.mode,
                Mode::ClientPaged { skip: 3, take: 10 },
                "{sql}"
            );
        }
        assert!(!step("SHOW TABLES", 0, 1).describable);
    }

    #[test]
    fn writes_and_scripts_run_once() {
        let options = SqlLexOptions::clickhouse();
        let sql = "INSERT INTO t VALUES (1); SELECT 1";
        let classification = classify(sql, &options);
        let first = plan(sql, &classification, 0, 0, 7, &options);
        assert_eq!(first.mode, Mode::Once { take: 7 });
        assert_eq!(first.sql, "INSERT INTO t VALUES (1)");
        let second = plan(sql, &classification, 1, 0, 7, &options);
        assert_eq!(
            (second.sql.as_str(), second.mode),
            ("SELECT 1", Mode::Once { take: 7 })
        );
    }

    #[test]
    fn modes_know_how_many_rows_to_read() {
        assert_eq!(Mode::ServerPaged { take: 10 }.want(), 11);
        assert_eq!(Mode::ClientPaged { skip: 25, take: 10 }.want(), 36);
        assert_eq!(Mode::Once { take: 10 }.want(), 10);
        assert!(Mode::ClientPaged { skip: 0, take: 1 }.capped() && Mode::Once { take: 1 }.capped());
        assert!(!Mode::ServerPaged { take: 1 }.capped());
        assert_eq!(
            Mode::ClientPaged {
                skip: usize::MAX,
                take: 10
            }
            .want(),
            usize::MAX
        );
    }

    #[test]
    fn dangerous_statements_include_the_clickhouse_only_ones() {
        let options = SqlLexOptions::clickhouse();
        let danger = |sql: &str| danger_of(sql, &classify(sql, &options));
        assert!(danger("ALTER TABLE t DELETE WHERE id = 1").is_some());
        assert!(danger("OPTIMIZE TABLE t FINAL").is_some());
        assert!(danger("SELECT 1; KILL QUERY WHERE 1").is_some());
        assert!(danger("DETACH TABLE t").is_some());
        assert_eq!(danger("OPTIMIZE TABLE t"), None);
        assert_eq!(danger("DELETE FROM t WHERE id = 1"), None);
        assert_eq!(danger("SELECT 'DROP TABLE t'"), None);
    }

    #[test]
    fn requests_other_than_sql_are_refused() {
        let request = |kind: &str, payload: serde_json::Value| QueryRequest {
            kind: kind.into(),
            operation_id: engine_driver_kit::api::OperationId(1),
            database: None,
            object: None,
            offset: 0,
            limit: 0,
            payload,
        };
        let error = parse_request(&request(
            "redis.key",
            serde_json::json!({"sql": "SELECT 1"}),
        ))
        .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("query.unsupported"));
        let error = parse_request(&request("sql", serde_json::json!({"sql": "  "}))).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("query.bad_request"));
        assert_eq!(
            parse_request(&request("sql", serde_json::json!({"sql": "SELECT 1"}))).unwrap(),
            "SELECT 1"
        );
    }
}
