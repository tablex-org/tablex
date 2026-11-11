//! `dbengine_open_query` for SQL.
//!
//! `kind: "sql"` (or `"sql_query"`), `payload: {"sql": "...", "confirmed"?: bool}`,
//! `database` = the database to run on (default: the connection's),
//! `offset`/`limit` = the page window. The rules are the shared SQL contract
//! (`engine_driver_api::sql`); the kit's SQL classifier decides everything
//! below, so a comment, string or dollar quote can never change the outcome.
//!
//! * **Confirmation.** A dangerous statement anywhere in the script (DROP,
//!   TRUNCATE, ALTER, GRANT/REVOKE, CREATE/DROP DATABASE, DELETE/UPDATE without
//!   WHERE) fails with `query.confirmation_required` (message = the reason)
//!   unless `payload.confirmed == true`. Nothing is sent to the server first.
//! * **Paging.** `limit` 0 means 200, at most 100 000. Only a single pure read
//!   is pageable. `SELECT`/`WITH ... SELECT`/`VALUES`/`TABLE` is wrapped as
//!   `SELECT * FROM (<sql>) LIMIT limit+1 OFFSET offset`; `SHOW` and `EXPLAIN`
//!   run as written and skip/take client-side. The extra row is the lookahead:
//!   `truncated` is exact. `total_count` is `None` (the total is not known).
//! * **Re-paging is READ ONLY.** A page at `offset > 0` re-runs the read, so it
//!   runs inside `BEGIN READ ONLY` ... `ROLLBACK`; if the user has a transaction
//!   open (its mode cannot change) inside a savepoint with
//!   `SET LOCAL transaction_read_only = on`, undone with `ROLLBACK TO SAVEPOINT`,
//!   leaving the user's transaction alone. A read that writes (`nextval`, a
//!   function with side effects, `FOR UPDATE`) fails there with SQLSTATE 25006,
//!   which becomes `query.not_pageable`: it ran once at offset 0 and is never
//!   executed again.
//! * **`payload.read_only == true`** (additive, capability `query.read_only`; the
//!   agent tool layer sets it for every statement it classified as a read) runs
//!   the statement in the READ ONLY scope below, whatever its plan; a write
//!   attempt fails with `query.read_only_violation`. Before anything is sent the
//!   text is refused with the same code when a statement controls the transaction
//!   (`COMMIT`, `ROLLBACK`, `END`, `SET TRANSACTION`, `RELEASE`, ...), a script has a
//!   statement that is not a plain read (several plain reads are fine), or a single
//!   statement is neither a read nor `EXPLAIN`/`ANALYZE` of one (`CALL`, `DO`, `COPY ...
//!   TO PROGRAM`, `SELECT ... INTO`, a write): the scope does not stop everything
//!   those can do. (`DO` and `CALL` could not commit in the driver's transaction block
//!   anyway: the server answers "invalid transaction termination".) It is classified as `standard_conforming_strings =
//!   on` text, and runs with that setting `SET LOCAL`.
//! * **`payload.run_to_end == true`** (additive, capability `query.run_to_end`) runs the single
//!   statement as written (never wrapped in a paging subquery) and to its end: every row is read,
//!   `limit` of them are kept and the rest dropped. A script's statements are sent this way, so a
//!   function called per row of a `SELECT` runs for every row.
//! * **Never re-run to serve a page.** If the server rejects the wrapper for a
//!   pageable read, at `offset == 0` it runs once as written (a non-pageable
//!   page); at `offset > 0` it is refused with `query.not_pageable` without being
//!   executed.
//! * **Everything else runs once.** `offset > 0` fails with
//!   `query.not_pageable` and nothing runs. DDL/DML without result columns
//!   return `QueryOutput::command_complete(affected)`. A statement with result
//!   columns (`INSERT ... RETURNING`, `WITH ... DELETE ... SELECT`) returns up
//!   to one page of them; rows beyond the page are discarded and the page says
//!   `truncated = true` (a later page is refused, never re-run).
//! * **Scripts** (several statements) go through the simple query protocol
//!   *streaming*: every statement runs once, only the last result set is kept
//!   (never more than one page of it); the page reports `truncated = false`. The
//!   protocol carries no column types, so once the script is done the statement
//!   that produced the kept rows is described (prepared, never run again) and the
//!   integer, float and boolean columns are typed like those of a single
//!   statement. That is skipped (the columns stay the server's text, so a boolean
//!   reads `t`/`f`) when a later statement could have changed the answer (anything
//!   after it that is not a read or a COMMIT/ROLLBACK) or the description does not
//!   match the rows.
//! * **Server text for every other type.** Types without a binary decoder
//!   (`timestamptz`, `timetz`, arrays, intervals, inet, money, ranges, composites,
//!   extension types, ...) are printed by the server, whatever the statement:
//!   a pageable select is re-selected with `::text` casts and
//!   `INSERT/UPDATE/DELETE ... RETURNING` is re-prepared as
//!   `WITH w AS (<stmt>) SELECT <casts> FROM w` (both stay on the binary
//!   protocol: the other columns keep their exact decoders and text arrives as the
//!   bytes the server sent). Statements that cannot be wrapped (`EXECUTE`,
//!   `FETCH`, `CALL`, a CTE with a data-modifying statement, `MERGE`) run once over
//!   the simple query protocol instead, whose rows are the server's own text for
//!   every column, and [`super::convert::retype`] turns the integer, float and
//!   boolean columns back into native ones. No cell is ever a placeholder and no
//!   statement runs twice. The simple protocol hands over text only: a value that
//!   is not valid UTF-8 (a SQL_ASCII database) cannot be delivered there, the
//!   statement fails with `query.invalid_utf8` (naming the column) after it ran.
//! * **Session settings.** Text is read under `extra_float_digits = 3`,
//!   `DateStyle = ISO` and `IntervalStyle = postgres`, set with `SET`s right
//!   after every connection opens (see `V2_SESSION_SETUP`; never as startup
//!   `options`, which poolers refuse). SQL that may change them (or
//!   end a transaction, which can undo a `SET`) makes the connection put them
//!   back before its next statement, and in a script right after the statement
//!   that may have changed them.
//!
//! Rows stream into Arrow builders (see [`super::convert`]): no `Vec<Row>`, no
//! per-cell `String`. Cancellation is the connection's `CancelToken`, keyed by
//! operation id (see [`super::connection::CancelState`]); a requested cancel is
//! `query.cancelled`, an interrupt nobody asked for is a retryable
//! `query.interrupted`.

use super::connection::{
    batch_execute_uninterrupted, session_scope, ArmedCancel, PgConnection, Scope,
};
use super::convert::{retype, BatchBuilder, ColumnPlan, ConvertError, Kind, Raw};
use super::runtime;
use crate::db::postgres::v2_error_detail;
use crate::db::postgres::V2_SESSION_RESTORE;
use crate::db::sql_builder::quote_ident;
use arrow_array::RecordBatch;
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::{OperationId, PageFlags, QueryRequest};
use engine_driver_kit::sql::{classify, Keyword, SqlLexOptions, Statement as SqlStatement};
use engine_driver_kit::{BackendError, OperationGuard, QueryOutput};
use futures_util::TryStreamExt;
use std::borrow::Cow;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Column, SimpleQueryMessage, Statement};

const DEFAULT_LIMIT: u64 = 200;
const MAX_LIMIT: u64 = 100_000;

/// How a request is executed (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// A pageable `SELECT`-like read: wrapped in a subquery with `LIMIT`/`OFFSET`.
    Wrapped,
    /// A pageable read that cannot be a subquery (`SHOW`, `EXPLAIN`): as
    /// written, skipping `offset` rows client-side.
    ClientPaged,
    /// A single non-pageable statement: runs once. `rewrite_returning`: a
    /// `INSERT/UPDATE/DELETE` whose `RETURNING` columns can be re-selected from a
    /// `WITH` (`MERGE` cannot be inside one).
    Once { rewrite_returning: bool },
    /// Several statements.
    Script,
}

/// What `open_query` decided about a request, before anything runs.
#[derive(Debug, PartialEq, Eq)]
struct Decision<'a> {
    plan: Plan,
    /// The statement text (single-statement plans) without the trailing `;`.
    sql: &'a str,
    /// The SQL may change a session setting the driver reads text under.
    changes_settings: bool,
    /// `payload.run_to_end`: every row of the statement is read, `limit` of them kept.
    run_to_end: bool,
}

/// Whether `sql` may change a setting the driver reads text under
/// (`extra_float_digits`, `DateStyle`, `IntervalStyle`, `client_encoding`). A loose
/// lexical test: a false positive costs one `SET` round trip.
fn may_change_settings(sql: &str) -> bool {
    const NAMES: [&str; 6] = [
        "extra_float_digits",
        "datestyle",
        "intervalstyle",
        "client_encoding",
        "set_config",
        "set names",
    ];
    NAMES.iter().any(|name| {
        sql.as_bytes()
            .windows(name.len())
            .any(|window| window.eq_ignore_ascii_case(name.as_bytes()))
    })
}

/// A statement after which the settings must be put back: one that may set them,
/// or end a transaction (a rollback undoes a `SET`).
fn unsettles(sql: &str, statement: &SqlStatement) -> bool {
    statement.ends_transaction || may_change_settings(&sql[statement.range.clone()])
}

/// Classifies `sql` and applies the contract's refusals (empty, dangerous
/// without confirmation, not pageable at a non-zero offset).
fn decide<'a>(
    sql: &'a str,
    confirmed: bool,
    read_only: bool,
    offset: u64,
) -> Result<Decision<'a>, BackendError> {
    let classification = classify(sql, &SqlLexOptions::postgres());
    if classification.is_empty() {
        return Err(BackendError::with_code(
            "query.bad_request",
            "missing SQL payload",
        ));
    }
    // `payload.read_only`: the text runs with `standard_conforming_strings = on`
    // (see [`read_only_scope`]), which is how it was just classified; a statement
    // that could end the READ ONLY scope, or a script with a write, never runs.
    if read_only {
        if let Some(refusal) = classification.read_only_refusal() {
            return Err(refusal.into());
        }
    }
    if let Some(danger) = classification.danger {
        if !confirmed {
            return Err(BackendError::with_code(
                codes::QUERY_CONFIRMATION_REQUIRED,
                danger.reason,
            ));
        }
    }
    let changes_settings = classification
        .statements
        .iter()
        .any(|statement| unsettles(sql, statement));
    let Some(statement) = classification.single() else {
        if offset > 0 {
            return Err(not_pageable());
        }
        return Ok(Decision {
            plan: Plan::Script,
            sql,
            changes_settings,
            run_to_end: false,
        });
    };
    let text = sql[statement.range.clone()].trim();
    let plan = if classification.pageable {
        match statement.keyword {
            Some(Keyword::Show | Keyword::Explain | Keyword::Describe) => Plan::ClientPaged,
            _ => Plan::Wrapped,
        }
    } else {
        if offset > 0 {
            return Err(not_pageable());
        }
        Plan::Once {
            rewrite_returning: matches!(
                statement.keyword,
                Some(Keyword::Insert | Keyword::Update | Keyword::Delete)
            ),
        }
    };
    Ok(Decision {
        plan,
        sql: text,
        changes_settings,
        run_to_end: false,
    })
}

/// `payload.run_to_end`: a single statement runs as written (never in a paging subquery) and to its end.
fn run_to_end(decision: &mut Decision<'_>, offset: u64) -> Result<(), BackendError> {
    if !matches!(decision.plan, Plan::Wrapped | Plan::ClientPaged | Plan::Once { .. }) {
        // A script already runs as written, every statement to its end.
        return Ok(());
    }
    if offset > 0 {
        return Err(not_pageable());
    }
    let rewrite_returning = matches!(decision.plan, Plan::Once { rewrite_returning: true });
    decision.plan = Plan::Once { rewrite_returning };
    decision.run_to_end = true;
    Ok(())
}

fn not_pageable() -> BackendError {
    BackendError::with_code(
        codes::QUERY_NOT_PAGEABLE,
        "only a single read-only SELECT/VALUES/SHOW/EXPLAIN can be paged; this statement runs once, at offset 0",
    )
}

pub(crate) fn open_query(
    connection: &PgConnection,
    request: &QueryRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<QueryOutput, BackendError> {
    if request.kind != "sql" && request.kind != "sql_query" {
        return Err(BackendError::with_code(
            "query.unsupported",
            "PostgreSQL accepts kind=sql",
        ));
    }
    let sql = request
        .payload
        .get("sql")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let confirmed = request
        .payload
        .get("confirmed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let read_only = request
        .payload
        .get("read_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut decision = decide(sql, confirmed, read_only, request.offset)?;
    // Capability `query.run_to_end`: the one statement runs as written and to its end, whatever it is.
    if request
        .payload
        .get("run_to_end")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        run_to_end(&mut decision, request.offset)?;
    }
    let limit = if request.limit == 0 {
        DEFAULT_LIMIT
    } else {
        request.limit.min(MAX_LIMIT)
    };
    let database = connection
        .database_or_default(request.database.as_deref())
        .to_owned();
    let operation_id = operation.operation_id();

    // `active` owns the connection gate: the operation stops being active
    // before the next call may start.
    let active = operation.activate(connection.lock_gate("query")?)?;
    let cancel_requested = || active.cancel_requested();
    let result = runtime::block_on(run(
        connection,
        operation_id,
        &database,
        &decision,
        limit,
        request.offset,
        read_only,
        &cancel_requested,
    ))
    .map_err(BackendError::new)?;
    // A script puts the settings back itself, after each statement that may have
    // changed them, as long as it ran to the end.
    if decision.changes_settings && !(decision.plan == Plan::Script && result.is_ok()) {
        connection.mark_settings_stale(&database);
    }
    if result.as_ref().is_err_and(is_lost) {
        connection.mark_broken();
    }
    let outcome = result?;
    drop(active);

    Ok(match outcome {
        Outcome::Command { affected_rows } => QueryOutput::command_complete(affected_rows, ()),
        Outcome::Rows { batch, truncated } => {
            let columns = super::page_columns(&batch);
            QueryOutput {
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
            }
        }
    })
}

enum Outcome {
    /// At most one page; `truncated` = a lookahead row existed (pageable reads only).
    Rows {
        batch: RecordBatch,
        truncated: bool,
    },
    Command {
        affected_rows: u64,
    },
}

impl Outcome {
    /// The page of a statement that is not pageable: it reports no further page.
    fn never_truncated(self) -> Self {
        match self {
            Self::Rows { batch, .. } => Self::Rows {
                batch,
                truncated: false,
            },
            other => other,
        }
    }
}

/// A failure while running a statement, before it is mapped to a wire error.
enum Failure {
    Pg(tokio_postgres::Error),
    Other(BackendError),
}

impl From<tokio_postgres::Error> for Failure {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Pg(error)
    }
}

impl From<ConvertError> for Failure {
    fn from(error: ConvertError) -> Self {
        Self::Other(BackendError::new(error.to_string()))
    }
}

impl From<BackendError> for Failure {
    fn from(error: BackendError) -> Self {
        Self::Other(error)
    }
}

/// Maps a driver failure. An interrupt (SQLSTATE 57014) nobody asked for
/// (`pg_cancel_backend`, a server timeout) is a retryable `<area>.interrupted`;
/// one the host requested is `<area>.cancelled`.
pub(crate) fn classify_error(
    error: &tokio_postgres::Error,
    cancel_requested: bool,
    area: &str,
) -> BackendError {
    if error.code() == Some(&SqlState::QUERY_CANCELED) {
        return if cancel_requested {
            cancelled(area)
        } else {
            BackendError::with_code(format!("{area}.interrupted"), v2_error_detail(error))
                .retryable()
        };
    }
    if connection_lost(error) {
        return BackendError::with_code(
            format!("{area}.not_connected"),
            format!("the connection to the server was lost: {}", v2_error_detail(error)),
        );
    }
    BackendError::new(v2_error_detail(error))
}

/// Whether the failure says the connection itself is gone (the server restarted,
/// the network dropped, the peer closed it) rather than that the server refused
/// a statement. The session never recovers: the app has to reconnect.
pub(crate) fn connection_lost(error: &tokio_postgres::Error) -> bool {
    error.is_closed()
        || std::error::Error::source(error)
            .is_some_and(|source| source.downcast_ref::<std::io::Error>().is_some())
}

/// `true` for the error of a lost connection (see [`classify_error`]).
pub(crate) fn is_lost(error: &BackendError) -> bool {
    error
        .code
        .as_deref()
        .is_some_and(|code| code.ends_with(".not_connected"))
}

pub(crate) fn cancelled(area: &str) -> BackendError {
    BackendError::with_code(format!("{area}.cancelled"), format!("{area} cancelled"))
}

async fn run(
    connection: &PgConnection,
    operation: OperationId,
    database: &str,
    decision: &Decision<'_>,
    limit: u64,
    offset: u64,
    force_read_only: bool,
    cancel_requested: &dyn Fn() -> bool,
) -> Result<Outcome, BackendError> {
    let client = connection
        .inner
        .v2_client(database)
        .await
        .map_err(BackendError::new)?;
    connection.restore_settings(&client, database).await;
    let armed = connection.cancel.arm(operation, client.cancel_token());
    // A cancel that arrived before the token was armed reached nobody.
    if cancel_requested() {
        return Err(cancelled("query"));
    }
    let run = Run {
        client: &client,
        armed: &armed,
    };
    // Re-paging a read (offset > 0) runs it again: do it READ ONLY so a function
    // with side effects (`nextval`, a user function that writes) cannot write.
    // `payload.read_only == true` (an agent's read) runs ANY plan READ ONLY: a
    // lexically pure SELECT that calls a writing function fails with 25006
    // instead of writing.
    let rerun_read_only = force_read_only
        || (offset > 0 && matches!(decision.plan, Plan::Wrapped | Plan::ClientPaged));
    let result = if rerun_read_only {
        read_only_page(connection, &run, decision, limit, offset, force_read_only).await
    } else {
        let Ok(result) = armed.watch(execute(&run, decision, limit, offset)).await else {
            return Err(connection.abandoned("query"));
        };
        armed.disarm();
        result
    };
    match result {
        Ok(outcome) => Ok(outcome),
        Err(Failure::Pg(error)) if force_read_only && is_read_only_violation(&error) => {
            Err(BackendError::with_code(
                "query.read_only_violation",
                "the statement tried to write in a read-only execution (SQLSTATE 25006)",
            ))
        }
        Err(Failure::Pg(error)) if rerun_read_only && is_read_only_violation(&error) => {
            Err(not_pageable_read_only())
        }
        Err(Failure::Pg(error)) => Err(classify_error(&error, cancel_requested(), "query")),
        Err(Failure::Other(error)) => Err(error),
    }
}

/// The session an operation runs on, with its cancel state.
struct Run<'a> {
    client: &'a Client,
    armed: &'a ArmedCancel,
}

impl Run<'_> {
    /// Looked at before every round trip: a cancel the server would ignore (it
    /// reached the backend between two commands) stops the operation here, and
    /// nothing more is sent.
    fn check(&self) -> Result<(), Failure> {
        self.armed.check("query").map_err(Failure::Other)
    }
}

/// SQLSTATE 25006: a write was attempted inside a READ ONLY transaction.
fn is_read_only_violation(error: &tokio_postgres::Error) -> bool {
    error.code() == Some(&SqlState::READ_ONLY_SQL_TRANSACTION)
}

fn not_pageable_read_only() -> BackendError {
    BackendError::with_code(
        codes::QUERY_NOT_PAGEABLE,
        "this read has side effects (it wrote when re-run read-only), so it cannot be paged; it only runs once, at offset 0",
    )
}

const READ_SAVEPOINT: &str = "tablex_read";

/// Runs the page inside a READ ONLY scope and always leaves it:
/// * no user transaction: `BEGIN READ ONLY` ... `ROLLBACK`;
/// * the user has a transaction open: its mode cannot be changed, so a
///   savepoint is opened, `transaction_read_only` is switched on *locally* (it
///   may always be turned on) and both are undone afterwards with
///   `ROLLBACK TO SAVEPOINT` + `RELEASE`. The user's transaction is untouched.
///
/// The scope is left once the cancellable part ended (see
/// [`ArmedCancel::watch`]), by a clean-up that a late cancel cannot break (it
/// is tried again). If the scope cannot be left, the connection is marked
/// broken. A cancel inside a transaction the user opened leaves it as the
/// server does: the interrupted statement is rolled back to the savepoint (or,
/// when the cancel hit the savepoint itself or the probe, the transaction is
/// aborted until the user rolls back).
async fn read_only_page(
    connection: &PgConnection,
    run: &Run<'_>,
    decision: &Decision<'_>,
    limit: u64,
    offset: u64,
    force_read_only: bool,
) -> Result<Outcome, Failure> {
    let mut entered = None;
    let mut begun = false;
    let work = async {
        let scope = session_scope(run.client).await?;
        run.check()?;
        let (begin, _) = read_only_scope(scope, force_read_only);
        // From here the scope may exist on the server, even if `begin` fails.
        entered = Some(scope);
        run.client.batch_execute(&begin).await?;
        begun = true;
        run.check()?;
        execute(run, decision, limit, offset).await
    };
    let Ok(result) = run.armed.watch(work).await else {
        // Nothing more is sent: the leave-the-scope clean-up would wait for the same dead server.
        return Err(Failure::Other(connection.abandoned("query")));
    };
    run.armed.disarm();
    if let Some(scope) = entered {
        let (_, end) = read_only_scope(scope, force_read_only);
        let left = batch_execute_uninterrupted(run.client, &end).await;
        // A savepoint that was never opened leaves the user's (aborted)
        // transaction to the user; everything else must be undone.
        if left.is_err() && (begun || scope == Scope::Own) {
            connection.mark_broken();
        }
    }
    result
}

/// The statements that open and close the READ ONLY scope. `payload.read_only` text
/// also runs with `standard_conforming_strings = on` (undone with the scope): that is
/// how the classifier read it, and the server parses the whole text before it runs
/// any of it, so a session that had the setting off would split `'\'; COMMIT; ...`
/// differently.
fn read_only_scope(scope: Scope, strict_strings: bool) -> (String, String) {
    let strings = if strict_strings {
        "; SET LOCAL standard_conforming_strings = on"
    } else {
        ""
    };
    match scope {
        Scope::Own => (format!("BEGIN READ ONLY{strings}"), "ROLLBACK".to_owned()),
        Scope::Savepoint => (
            format!("SAVEPOINT {READ_SAVEPOINT}; SET LOCAL transaction_read_only = on{strings}"),
            format!("ROLLBACK TO SAVEPOINT {READ_SAVEPOINT}; RELEASE SAVEPOINT {READ_SAVEPOINT}"),
        ),
    }
}

async fn execute(
    run: &Run<'_>,
    decision: &Decision<'_>,
    limit: u64,
    offset: u64,
) -> Result<Outcome, Failure> {
    let sql = decision.sql;
    let to_end = decision.run_to_end;
    match decision.plan {
        Plan::Wrapped => select_page(run, sql, limit, offset).await,
        Plan::ClientPaged => as_written(run, sql, offset, limit, false, to_end).await,
        // Non-pageable pages never offer a next page: rows beyond `limit` are
        // dropped and `truncated` stays false.
        Plan::Once { rewrite_returning } => {
            as_written(run, sql, 0, limit, rewrite_returning, to_end)
                .await
                .map(Outcome::never_truncated)
        }
        Plan::Script => script(run, sql, limit).await.map(Outcome::never_truncated),
    }
}

/// `SELECT <projection> FROM (<sql>) AS t<aliases> LIMIT .. OFFSET ..`.
fn wrap(sql: &str, projection: &str, aliases: &str, limit: u64, offset: u64) -> String {
    // The newlines keep a trailing `-- comment` in `sql` from swallowing `)`.
    format!(
        "SELECT {projection} FROM (\n{sql}\n) AS tableleaf_raw_query{aliases} LIMIT {limit} OFFSET {offset}"
    )
}

/// `WITH w AS (<dml> RETURNING ...) SELECT <projection> FROM w AS t(<aliases>)`.
fn wrap_returning(sql: &str, projection: &str, aliases: &str) -> String {
    format!("WITH tableleaf_dml AS (\n{sql}\n) SELECT {projection} FROM tableleaf_dml AS tableleaf_raw_query{aliases}")
}

/// The positional alias list and the projection that casts the columns the
/// client cannot decode (`kinds[i] == None`) to text. Positional aliases make
/// the re-select safe even when the result repeats a column name.
fn text_projection(columns: &[Column], kinds: &[Option<Kind>]) -> (String, String) {
    let aliases = (0..columns.len())
        .map(|index| format!("c{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let projection = columns
        .iter()
        .zip(kinds)
        .enumerate()
        .map(|(index, (column, kind))| {
            let name = quote_ident(column.name());
            match kind {
                Some(_) => format!("c{index} AS {name}"),
                // `inet::text` appends `/32` to a host address; the output function
                // (what psql prints, and what the simple protocol returns) does not.
                None if *column.type_() == Type::INET => format!(
                    "CASE WHEN masklen(c{index}) = CASE family(c{index}) WHEN 4 THEN 32 ELSE 128 END THEN host(c{index}) ELSE c{index}::text END AS {name}"
                ),
                None => format!("c{index}::text AS {name}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    (format!("({aliases})"), projection)
}

fn decoders(columns: &[Column]) -> Vec<Option<Kind>> {
    columns
        .iter()
        .map(|column| Kind::classify(column.type_()))
        .collect()
}

/// The plan of each column: the binary decoder where there is one, otherwise the
/// server's own text (see [`simple_page`]).
fn plans_for(columns: &[Column], kinds: &[Option<Kind>]) -> Vec<ColumnPlan> {
    columns
        .iter()
        .zip(kinds)
        .map(|(column, kind)| {
            ColumnPlan::new(column.name(), kind.unwrap_or(Kind::Text), column.type_())
        })
        .collect()
}

/// What to do with a pageable read the server rejects inside the wrapper (its
/// *prepare* failed, so nothing ran yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unwrappable {
    /// `offset == 0`: run it once as written, as a non-pageable statement.
    RunOnce,
    /// `offset > 0`: never run it again to serve a later page.
    Refuse,
}

fn unwrappable_read(offset: u64) -> Unwrappable {
    if offset == 0 {
        Unwrappable::RunOnce
    } else {
        Unwrappable::Refuse
    }
}

fn wrap_was_rejected(error: &tokio_postgres::Error) -> bool {
    matches!(
        error.code(),
        Some(&SqlState::FEATURE_NOT_SUPPORTED) | Some(&SqlState::SYNTAX_ERROR)
    )
}

/// The `SELECT`-like read, paged in SQL. If the wrapper's *prepare* is rejected
/// the read never ran; [`unwrappable_read`] decides what happens next (a read is
/// never executed again to serve a later page).
async fn select_page(
    run: &Run<'_>,
    sql: &str,
    limit: u64,
    offset: u64,
) -> Result<Outcome, Failure> {
    let client = run.client;
    let wrapped = wrap(sql, "*", "", limit + 1, offset);
    let statement = match client.prepare(&wrapped).await {
        Ok(statement) => statement,
        Err(error) if wrap_was_rejected(&error) => {
            return match unwrappable_read(offset) {
                Unwrappable::RunOnce => as_written(run, sql, 0, limit, false, false)
                    .await
                    .map(Outcome::never_truncated),
                Unwrappable::Refuse => {
                    // Prepare (never execute) the statement as written: a genuine
                    // error is reported as such, and a valid statement the
                    // wrapper cannot take is simply not pageable.
                    run.check()?;
                    client.prepare(sql).await?;
                    Err(Failure::Other(not_pageable()))
                }
            };
        }
        Err(error) => return Err(error.into()),
    };
    let kinds = decoders(statement.columns());
    let plans = plans_for(statement.columns(), &kinds);
    if kinds.iter().all(Option::is_some) {
        return collect(run, &statement, plans, 0, limit, false).await;
    }
    // The server prints the columns without a decoder: re-select them as text.
    let (aliases, projection) = text_projection(statement.columns(), &kinds);
    match client
        .prepare(&wrap(sql, &projection, &aliases, limit + 1, offset))
        .await
    {
        Ok(restated) => collect(run, &restated, plans, 0, limit, false).await,
        // A cast the server refuses: the wrapper still runs, as server text.
        Err(_) => simple_page(run, &wrapped, &plans, 0, limit).await,
    }
}

/// A single statement run as written (DDL/DML, `RETURNING`, `SHOW`, `EXPLAIN`):
/// skip `skip` rows, keep at most `take`, note whether another row followed.
async fn as_written(
    run: &Run<'_>,
    sql: &str,
    skip: u64,
    take: u64,
    rewrite_returning: bool,
    to_end: bool,
) -> Result<Outcome, Failure> {
    let client = run.client;
    run.check()?;
    let statement = client.prepare(sql).await?;
    if statement.columns().is_empty() {
        run.check()?;
        let affected_rows = client.execute(&statement, &[]).await?;
        return Ok(Outcome::Command { affected_rows });
    }
    let kinds = decoders(statement.columns());
    let plans = plans_for(statement.columns(), &kinds);
    if kinds.iter().all(Option::is_some) {
        return collect(run, &statement, plans, skip, take, to_end).await;
    }
    if rewrite_returning {
        // Let the server print the undecodable columns of RETURNING.
        let (aliases, projection) = text_projection(statement.columns(), &kinds);
        if let Ok(rewritten) = client
            .prepare(&wrap_returning(sql, &projection, &aliases))
            .await
        {
            return collect(run, &rewritten, plans, skip, take, to_end).await;
        }
    }
    simple_page(run, sql, &plans, skip, take).await
}

/// Streams `statement`'s rows into one batch: skips `skip` rows, keeps at most
/// `take`, and reports whether a further row existed (the lookahead).
async fn collect(
    run: &Run<'_>,
    statement: &Statement,
    plans: Vec<ColumnPlan>,
    skip: u64,
    take: u64,
    to_end: bool,
) -> Result<Outcome, Failure> {
    run.check()?;
    let mut batch = BatchBuilder::new(plans);
    let stream = run
        .client
        .query_raw(statement, std::iter::empty::<&(dyn ToSql + Sync)>())
        .await?;
    let mut stream = std::pin::pin!(stream);
    let mut seen = 0u64;
    let mut truncated = false;
    while let Some(row) = stream.try_next().await? {
        seen += 1;
        if seen <= skip {
            continue;
        }
        if batch.row_count() as u64 >= take {
            truncated = true;
            // `run_to_end`: the rows past the page are read and dropped, so the statement runs for every row.
            if to_end {
                continue;
            }
            break;
        }
        batch.push_row(|index| {
            row.try_get::<usize, Raw<'_>>(index)
                .map(|cell| cell.0)
                .map_err(|error| ConvertError::Source(error.to_string()))
        })?;
    }
    Ok(Outcome::Rows {
        batch: batch.finish().map_err(Failure::Other)?,
        truncated,
    })
}

/// What the simple query protocol returned: the last result set (text) and the
/// rows affected by the statements.
struct Streamed {
    rows: Option<StreamedRows>,
    affected_rows: u64,
}

struct StreamedRows {
    builder: BatchBuilder,
    names: Vec<String>,
    truncated: bool,
    /// How many statements had completed when this result set began: its index
    /// among the script's statements.
    statement: usize,
}

/// Runs `sql` over the simple query protocol, streaming: every statement runs
/// once and to the end (a late error is still an error), the last result set is
/// kept as text, skipping `skip` rows and keeping at most `take`; `truncated`
/// says another row followed.
async fn stream_simple(
    run: &Run<'_>,
    sql: &str,
    skip: u64,
    take: u64,
) -> Result<Streamed, Failure> {
    run.check()?;
    let stream = run.client.simple_query_raw(sql).await?;
    let mut stream = std::pin::pin!(stream);
    let mut affected_rows = 0u64;
    let mut completed = 0usize;
    let mut seen = 0u64;
    let mut rows: Option<StreamedRows> = None;
    // The first value of the kept result set that is not UTF-8 (the statement still
    // runs to its end: a later error must not be lost).
    let mut invalid: Option<String> = None;
    while let Some(message) = stream.try_next().await? {
        match message {
            SimpleQueryMessage::RowDescription(columns) => {
                let names: Vec<String> = columns
                    .iter()
                    .map(|column| column.name().to_owned())
                    .collect();
                seen = 0;
                invalid = None;
                rows = Some(StreamedRows {
                    builder: BatchBuilder::text_columns(&names),
                    names,
                    truncated: false,
                    statement: completed,
                });
            }
            SimpleQueryMessage::Row(row) => {
                if let Some(rows) = rows.as_mut() {
                    seen += 1;
                    if seen <= skip {
                        continue;
                    }
                    if invalid.is_some() {
                        continue;
                    }
                    if rows.builder.row_count() as u64 >= take {
                        rows.truncated = true;
                        continue;
                    }
                    let mut cells = Vec::with_capacity(row.len());
                    for index in 0..row.len() {
                        match row.try_get(index) {
                            Ok(cell) => cells.push(cell),
                            Err(_) => {
                                invalid = Some(rows.names[index].clone());
                                break;
                            }
                        }
                    }
                    if invalid.is_none() {
                        rows.builder.push_text_row(cells.into_iter());
                    }
                }
            }
            SimpleQueryMessage::CommandComplete(count) => {
                completed += 1;
                affected_rows = affected_rows.saturating_add(count);
            }
            _ => {}
        }
    }
    if let Some(column) = invalid {
        return Err(Failure::Other(BackendError::with_code(
            "query.invalid_utf8",
            format!(
                "column \"{column}\" holds text that is not valid UTF-8 (a SQL_ASCII database, or a changed client_encoding); the statement ran, but its text cannot be shown. Select the column as bytea (convert_to(col, 'UTF8') or col::bytea) to see its bytes"
            ),
        )));
    }
    Ok(Streamed {
        rows,
        affected_rows,
    })
}

/// One statement whose columns include types without a binary decoder, run once
/// over the simple query protocol: the server prints every value (`psql`'s text:
/// intervals, arrays, `timestamptz` in the session's TimeZone with its numeric
/// offset, ranges, ...) and `plans` (from the prepared statement) turn the
/// integer, float and boolean columns back into native ones.
async fn simple_page(
    run: &Run<'_>,
    sql: &str,
    plans: &[ColumnPlan],
    skip: u64,
    take: u64,
) -> Result<Outcome, Failure> {
    let streamed = stream_simple(run, sql, skip, take).await?;
    let Some(rows) = streamed.rows else {
        return Ok(Outcome::Command {
            affected_rows: streamed.affected_rows,
        });
    };
    // The statement already ran (and may have written): a description that does
    // not fit the rows leaves them as the server's text, never an error.
    let batch = rows.builder.finish()?;
    let batch = retype(&batch, plans).unwrap_or(batch);
    Ok(Outcome::Rows {
        batch,
        truncated: rows.truncated,
    })
}

/// Several statements in one request (simple query protocol: no parameters,
/// text results), streamed: the last result set wins and at most one page of it
/// is ever buffered; with no result set, the rows affected add up.
async fn script(run: &Run<'_>, sql: &str, limit: u64) -> Result<Outcome, Failure> {
    let pinned = pin_session_settings(sql);
    let streamed = stream_simple(run, &pinned.sql, 0, limit).await?;
    let Some(rows) = streamed.rows else {
        return Ok(Outcome::Command {
            affected_rows: streamed.affected_rows,
        });
    };
    let plans = script_plans(run, sql, &rows, pinned.completions.as_deref()).await;
    let batch = rows.builder.finish()?;
    // A description that does not fit the rows leaves them as the server's text.
    let batch = match plans {
        Some(plans) => retype(&batch, &plans).unwrap_or(batch),
        None => batch,
    };
    Ok(Outcome::Rows {
        batch,
        truncated: rows.truncated,
    })
}

/// A script as it is sent: with [`V2_SESSION_RESTORE`] after every statement that
/// may have changed a session setting, so the text of the statements after it is
/// read under the settings the driver parses.
struct Pinned<'a> {
    sql: Cow<'a, str>,
    /// For each statement the server will complete, the index of the user's
    /// statement, `None` for one of ours. `None` when nothing was added.
    completions: Option<Vec<Option<usize>>>,
}

fn pin_session_settings(sql: &str) -> Pinned<'_> {
    let classification = classify(sql, &SqlLexOptions::postgres());
    let statements = &classification.statements;
    if !statements.iter().any(|statement| unsettles(sql, statement)) {
        return Pinned {
            sql: Cow::Borrowed(sql),
            completions: None,
        };
    }
    let restore = V2_SESSION_RESTORE.join("; ");
    let mut text = String::with_capacity(sql.len() + restore.len() * 2);
    let mut completions = Vec::with_capacity(statements.len() + V2_SESSION_RESTORE.len());
    let mut copied = 0;
    for (index, statement) in statements.iter().enumerate() {
        completions.push(Some(index));
        if unsettles(sql, statement) {
            let end = statement.range.end;
            text.push_str(&sql[copied..end]);
            // The newline ends a trailing `-- comment`; the statement's own `;`
            // (if any) closes our last `SET`.
            text.push_str("\n; ");
            text.push_str(&restore);
            copied = end;
            completions.extend(std::iter::repeat_n(None, V2_SESSION_RESTORE.len()));
        }
    }
    text.push_str(&sql[copied..]);
    Pinned {
        sql: Cow::Owned(text),
        completions: Some(completions),
    }
}

/// The column plans of the statement that produced a script's rows, learnt by
/// preparing it (nothing runs) once the script is done; `None` when that is not
/// safe or does not match (see the module docs).
async fn script_plans(
    run: &Run<'_>,
    sql: &str,
    rows: &StreamedRows,
    completions: Option<&[Option<usize>]>,
) -> Option<Vec<ColumnPlan>> {
    let classification = classify(sql, &SqlLexOptions::postgres());
    // The index among the user's statements (our own `SET`s complete too).
    let index = match completions {
        Some(map) => map.get(rows.statement).copied().flatten()?,
        None => rows.statement,
    };
    let (produced, later) = classification.statements.get(index..)?.split_first()?;
    // A failed prepare would abort a transaction the script left open, so only
    // describe what nothing after it can have invalidated.
    if !later
        .iter()
        .all(|statement| statement.read_only || statement.ends_transaction)
    {
        return None;
    }
    run.check().ok()?;
    let prepared = run
        .client
        .prepare(sql[produced.range.clone()].trim())
        .await
        .ok()?;
    let columns = prepared.columns();
    let same = columns.len() == rows.names.len()
        && columns
            .iter()
            .zip(&rows.names)
            .all(|(column, name)| column.name() == name);
    same.then(|| plans_for(columns, &decoders(columns)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_connection_is_not_connected_and_a_server_refusal_is_not() {
        // The server is gone: an I/O failure.
        let gone = runtime::block_on(tokio_postgres::connect(
            "postgres://u@127.0.0.1:1/d?connect_timeout=2",
            tokio_postgres::NoTls,
        ))
        .unwrap()
        .err()
        .expect("nothing listens on port 1");
        assert!(connection_lost(&gone), "{gone:?}");
        for area in ["query", "mutation"] {
            let mapped = classify_error(&gone, false, area);
            assert_eq!(
                mapped.code.as_deref(),
                Some(&*format!("{area}.not_connected"))
            );
            assert!(is_lost(&mapped));
        }
        // A server that refuses the login answers: the connection is fine.
        let server = runtime::block_on(crate::v2::tls::tests::fake::start(
            vec![crate::v2::tls::tests::fake::Answer::No],
            crate::v2::tls::tests::fake::Auth::WrongPassword,
        ))
        .unwrap();
        let refused = runtime::block_on(tokio_postgres::connect(
            &format!("postgres://u:pw@127.0.0.1:{}/d?sslmode=disable", server.port),
            tokio_postgres::NoTls,
        ))
        .unwrap()
        .err()
        .expect("wrong password");
        assert!(!connection_lost(&refused), "{refused:?}");
        assert!(!is_lost(&classify_error(&refused, false, "query")));
    }

    fn plan(sql: &str, offset: u64) -> Result<Plan, String> {
        decide(sql, false, false, offset)
            .map(|decision| decision.plan)
            .map_err(|error| error.code.map(|code| code.into_owned()).unwrap_or_default())
    }

    #[test]
    fn wrapped_sql_keeps_a_trailing_line_comment_from_eating_the_paren() {
        let sql = wrap("SELECT 1 -- note", "*", "", 10, 20);
        assert_eq!(
            sql,
            "SELECT * FROM (\nSELECT 1 -- note\n) AS tableleaf_raw_query LIMIT 10 OFFSET 20"
        );
    }

    #[test]
    fn the_per_statement_restore_is_the_connect_setup() {
        use crate::db::postgres::V2_SESSION_SETUP;
        assert_eq!(V2_SESSION_RESTORE.join("; "), V2_SESSION_SETUP);
    }

    #[test]
    fn positional_aliases_survive_duplicate_column_names() {
        let sql = wrap(
            "SELECT 1 AS a, 2 AS a",
            "c0 AS \"a\", c1::text AS \"a\"",
            "(c0, c1)",
            5,
            0,
        );
        assert!(sql.contains("AS tableleaf_raw_query(c0, c1)"));
        let dml = wrap_returning(
            "INSERT INTO t VALUES (1) RETURNING a",
            "c0::text AS \"a\"",
            "(c0)",
        );
        assert!(dml.starts_with("WITH tableleaf_dml AS (\nINSERT INTO t"));
        assert!(dml.ends_with("FROM tableleaf_dml AS tableleaf_raw_query(c0)"));
    }

    #[test]
    fn settings_changes_are_spotted_lexically_and_pinned_inside_scripts() {
        for sql in [
            "SET extra_float_digits = 0",
            "set DateStyle = 'SQL, DMY'",
            "SET IntervalStyle TO iso_8601",
            "SELECT set_config('datestyle', 'German', false)",
            "SET client_encoding = 'LATIN1'",
            "SET NAMES 'latin1'",
            "ROLLBACK",
        ] {
            let classification = classify(sql, &SqlLexOptions::postgres());
            assert!(
                classification
                    .statements
                    .iter()
                    .any(|statement| unsettles(sql, statement)),
                "{sql}"
            );
        }
        for sql in ["SELECT 1", "SELECT 'date style' FROM styles", "BEGIN"] {
            let classification = classify(sql, &SqlLexOptions::postgres());
            assert!(
                !classification
                    .statements
                    .iter()
                    .any(|statement| unsettles(sql, statement)),
                "{sql}"
            );
        }
        // A script that never touches them is sent as written.
        let plain = pin_session_settings("SELECT 1; SELECT 2");
        assert!(matches!(plain.sql, Cow::Borrowed(_)) && plain.completions.is_none());
        // Otherwise the settings come back right after the statement that may
        // have changed them (a trailing comment cannot swallow them), and the
        // statements of ours are skipped when counting the user's.
        let sql = "SET DateStyle = 'SQL' -- german\n; SELECT now(); SELECT 3";
        let pinned = pin_session_settings(sql);
        assert_eq!(
            pinned.sql,
            "SET DateStyle = 'SQL' -- german\n\n; SET extra_float_digits = 3; SET DateStyle = 'ISO'; SET IntervalStyle = 'postgres'; SET client_encoding = 'UTF8'; SELECT now(); SELECT 3"
        );
        assert_eq!(
            pinned.completions,
            Some(vec![Some(0), None, None, None, None, Some(1), Some(2)])
        );
        // The last statement without a `;` is closed by our last SET.
        let last = pin_session_settings("SELECT 1; RESET ALL; ROLLBACK");
        assert!(
            last.sql.ends_with("SET client_encoding = 'UTF8'"),
            "{}",
            last.sql
        );
    }

    #[test]
    fn reads_are_pageable_and_everything_else_runs_once() {
        for sql in [
            "SELECT 1",
            "  select 1;  ",
            "-- note\nSELECT 1",
            "/* a */ SELECT 1 -- b",
            "WITH a AS (SELECT 1) SELECT * FROM a",
            "(SELECT 1) UNION (SELECT 2)",
            "VALUES (1), (2)",
            "TABLE t",
        ] {
            assert_eq!(plan(sql, 0), Ok(Plan::Wrapped), "{sql}");
            assert_eq!(plan(sql, 7), Ok(Plan::Wrapped), "{sql} at an offset");
        }
        for sql in ["SHOW server_version", "EXPLAIN SELECT 1"] {
            assert_eq!(plan(sql, 3), Ok(Plan::ClientPaged), "{sql}");
        }
        // Not pageable: runs once at offset 0, refused at any other offset.
        for (sql, rewrite) in [
            ("INSERT INTO t VALUES (1)", true),
            ("UPDATE t SET a = 1 WHERE b = 2", true),
            ("DELETE FROM t WHERE a = 1", true),
            // MERGE cannot be inside a WITH.
            (
                "MERGE INTO t USING s ON t.a = s.a WHEN NOT MATCHED THEN INSERT VALUES (1)",
                false,
            ),
            (
                "WITH d AS (DELETE FROM t WHERE a = 1 RETURNING *) SELECT * FROM d",
                false,
            ),
            ("SELECT * INTO copy FROM t", false),
            ("EXPLAIN ANALYZE SELECT 1", false),
            ("CREATE TABLE t (a int)", false),
            ("EXECUTE p", false),
            ("FETCH 3 FROM c", false),
        ] {
            assert_eq!(
                plan(sql, 0),
                Ok(Plan::Once {
                    rewrite_returning: rewrite
                }),
                "{sql}"
            );
            assert_eq!(plan(sql, 1), Err("query.not_pageable".into()), "{sql}");
        }
        assert_eq!(plan("SELECT 1; SELECT 2", 0), Ok(Plan::Script));
        assert_eq!(
            plan("SELECT 1; SELECT 2", 1),
            Err("query.not_pageable".into())
        );
        // A comment or a string that mentions a write does not change the plan.
        assert_eq!(
            plan("SELECT 'DELETE FROM t' -- DROP TABLE t", 1),
            Ok(Plan::Wrapped)
        );
        assert_eq!(plan("SELECT $$a; DROP TABLE t$$", 1), Ok(Plan::Wrapped));
    }

    #[test]
    fn run_to_end_turns_a_pageable_read_into_one_run_as_written() {
        let mut read = decide("SELECT my_fn(id) FROM t", false, false, 0).unwrap();
        assert_eq!(read.plan, Plan::Wrapped);
        run_to_end(&mut read, 0).unwrap();
        assert_eq!((read.plan, read.run_to_end), (Plan::Once { rewrite_returning: false }, true));
        let mut returning = decide("INSERT INTO t VALUES (1) RETURNING id", false, false, 0).unwrap();
        run_to_end(&mut returning, 0).unwrap();
        assert_eq!(returning.plan, Plan::Once { rewrite_returning: true });
        let mut script = decide("SELECT 1; SELECT 2", false, false, 0).unwrap();
        run_to_end(&mut script, 0).unwrap();
        assert_eq!((script.plan, script.run_to_end), (Plan::Script, false));
        let mut paged = decide("SELECT 1", false, false, 0).unwrap();
        assert!(run_to_end(&mut paged, 5).is_err(), "a later page of a run to its end does not exist");
    }

    #[test]
    fn dangerous_statements_need_confirmation_with_the_reason() {
        for sql in [
            "DROP TABLE t",
            "TRUNCATE t",
            "ALTER TABLE t ADD c int",
            "DELETE FROM t",
            "UPDATE t SET a = 1",
            "GRANT SELECT ON t TO PUBLIC",
            "CREATE DATABASE x",
            "SELECT 1; DROP TABLE t",
            "/* c */ DROP TABLE t",
            "UPDATE t SET a = (SELECT 1 WHERE true)",
        ] {
            let error = decide(sql, false, false, 0).expect_err(sql);
            assert_eq!(
                error.code.as_deref(),
                Some("query.confirmation_required"),
                "{sql}"
            );
            assert!(!error.message.is_empty(), "{sql}");
            assert!(decide(sql, true, false, 0).is_ok(), "{sql} confirmed");
        }
        assert!(decide("DELETE FROM t WHERE a = 1", false, false, 0).is_ok());
        assert!(decide("SELECT 'DROP TABLE t'", false, false, 0).is_ok());
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment_so_the_statement_after_it_is_seen() {
        // PostgreSQL's scanner ends `--` at `\r` as at `\n`: the DELETE is a statement.
        let sql = "SELECT 2 --x\r; DELETE FROM t";
        let error = decide(sql, false, false, 0).expect_err("dangerous");
        assert_eq!(error.code.as_deref(), Some("query.confirmation_required"));
        assert!(decide(sql, true, false, 0).is_ok());
        // Under `read_only` the script holds a write and never runs.
        let error = decide("SELECT 2 --x\r; DELETE FROM t WHERE a = 1", false, true, 0)
            .expect_err("refused read-only");
        assert_eq!(error.code.as_deref(), Some("query.read_only_violation"));
    }

    #[test]
    fn an_unwrappable_read_is_never_run_again_for_a_later_page() {
        assert_eq!(unwrappable_read(0), Unwrappable::RunOnce);
        for offset in [1, 2, 200, u64::MAX] {
            assert_eq!(
                unwrappable_read(offset),
                Unwrappable::Refuse,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn non_pageable_outcomes_never_report_truncation() {
        let batch = engine_driver_kit::empty_batch();
        let outcome = Outcome::Rows {
            batch,
            truncated: true,
        }
        .never_truncated();
        assert!(matches!(
            outcome,
            Outcome::Rows {
                truncated: false,
                ..
            }
        ));
        assert!(matches!(
            Outcome::Command { affected_rows: 3 }.never_truncated(),
            Outcome::Command { affected_rows: 3 }
        ));
    }

    #[test]
    fn read_only_refuses_text_that_could_leave_its_scope() {
        for sql in [
            "COMMIT; DELETE FROM t WHERE a = 1",
            "SELECT 1; COMMIT; DELETE FROM t WHERE a = 1",
            "SELECT 1; ROLLBACK",
            "SELECT 1; END",
            "SELECT 1; ABORT",
            "BEGIN",
            "COMMIT",
            "START TRANSACTION",
            "SET TRANSACTION READ WRITE",
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            "SET LOCAL transaction_read_only = off",
            "SELECT 1; SET autocommit = on",
            "SELECT 1; RELEASE SAVEPOINT tablex_read",
            "ROLLBACK TO SAVEPOINT tablex_read",
            "SAVEPOINT s",
            "SELECT 1; DELETE FROM t WHERE a = 1",
            "DELETE FROM t WHERE a = 1; SELECT 1",
            // A single statement that is not a read (or EXPLAIN/ANALYZE of one) never runs.
            "DELETE FROM t WHERE a = 1",
            "CALL p()",
            "DO $$ BEGIN COMMIT; END $$",
            "SELECT * INTO new_t FROM t",
            "COPY t TO PROGRAM 'id'",
            // `standard_conforming_strings = on`: `'\'` is a whole string.
            r"SELECT '\'; COMMIT; DELETE FROM t WHERE a = 1",
        ] {
            let error = decide(sql, true, true, 0).expect_err(sql);
            assert_eq!(
                error.code.as_deref(),
                Some("query.read_only_violation"),
                "{sql}"
            );
            // Not read-only, the same text is a normal request.
            assert!(decide(sql, true, false, 0).is_ok(), "{sql}");
        }
    }

    #[test]
    fn read_only_keeps_reads_and_text_that_only_looks_like_a_script() {
        for sql in [
            "SELECT 1",
            "SELECT * FROM t WHERE a = 1 FOR UPDATE",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SHOW server_version",
            "EXPLAIN SELECT * FROM t",
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT * FROM t",
            "SELECT 1; SELECT 2",
            "SELECT 1; SELECT 2;",
            // An EXPLAIN ANALYZE of a write is the server's READ ONLY scope's to refuse.
            "EXPLAIN (ANALYZE) DELETE FROM t WHERE a = 1",
            // Quoted: still one statement.
            "SELECT E'\\'; COMMIT; DELETE FROM t WHERE a = 1; SELECT '",
            "SELECT $$; COMMIT; DELETE FROM t$$",
            "SELECT $tag$ ' ; COMMIT; $tag$",
            "SELECT 'COMMIT; DELETE FROM t', \"END\" FROM t",
            "SELECT 1 /* a /* ; COMMIT; */ */",
            "SELECT 1 -- ; COMMIT",
        ] {
            assert!(decide(sql, true, true, 0).is_ok(), "{sql}");
        }
    }

    #[test]
    fn read_only_text_runs_with_standard_conforming_strings_on() {
        let (begin, end) = read_only_scope(Scope::Own, true);
        assert_eq!(
            begin,
            "BEGIN READ ONLY; SET LOCAL standard_conforming_strings = on"
        );
        assert_eq!(end, "ROLLBACK");
        let (begin, end) = read_only_scope(Scope::Savepoint, true);
        assert!(begin.starts_with("SAVEPOINT tablex_read; SET LOCAL transaction_read_only = on"));
        assert!(
            begin.ends_with("SET LOCAL standard_conforming_strings = on"),
            "{begin}"
        );
        assert!(
            end.starts_with("ROLLBACK TO SAVEPOINT tablex_read"),
            "{end}"
        );
        // A re-page reads the text as the first page did: the setting is left alone.
        assert_eq!(read_only_scope(Scope::Own, false).0, "BEGIN READ ONLY");
        assert!(!read_only_scope(Scope::Savepoint, false)
            .0
            .contains("standard_conforming"));
    }

    #[test]
    fn empty_sql_is_a_bad_request() {
        for sql in ["", "   ", "-- nothing", ";", "/* c */ ;"] {
            let error = decide(sql, true, false, 0).expect_err(sql);
            assert_eq!(error.code.as_deref(), Some("query.bad_request"), "{sql}");
        }
    }
}
