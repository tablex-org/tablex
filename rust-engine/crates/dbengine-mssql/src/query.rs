//! Query execution under the shared SQL contract (`engine_driver_api::sql`).
//!
//! * `sql::analyze` decides what the text is (see its docs: T-SQL needs no `;`, so the words of
//!   every statement are scanned too). Only a single pure read is pageable. When it has a
//!   top-level `ORDER BY` and nothing that cannot be followed by `OFFSET`, the server pages it
//!   (`OFFSET n ROWS FETCH NEXT limit+1 ROWS ONLY`); otherwise it runs as written and the rows
//!   before `offset` are skipped as they arrive (never buffered). At `limit + 1` rows the rest is
//!   not read: the statement is stopped with an attention signal, so `SELECT * FROM big_table` is
//!   not read to its end for a page of 200. Anything else runs once: `offset > 0` is refused with
//!   `query.not_pageable`.
//! * `GO` lines split the text into batches that run one after the other (an SSMS script
//!   pastes and runs); the page is the rows of the LAST result set of the whole text.
//! * `limit == 0` is the default page of 200 rows, pages are capped at 100 000, and at most one
//!   page (+1 lookahead row) is ever buffered.
//! * Dangerous statements need `payload.confirmed == true` (`query.confirmation_required`).
//! * `payload.read_only`: SQL Server has no read-only transaction, so the text is refused with
//!   `query.read_only_violation`, before anything runs, unless it has no word that writes or
//!   executes (T-SQL functions cannot write tables, so a text without such words cannot).
//! * A statement without a result set reports the session's `@@ROWCOUNT` as its row count, read
//!   in one more round trip (the TDS row counts are not exposed by the client library).
//! * The session's default database follows the request's `database` (`USE [db]` when it
//!   differs) and is forgotten after user SQL that may change it.

use crate::conn::{block_on, cancellable, map_error, use_database, Area, Client, MsConnection};
use crate::convert::Collector;
use crate::sql::{analyze, lex_options, with_offset_fetch, Analysis};
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::{PageColumn, PageFlags, QueryRequest};
use engine_driver_kit::{BackendError, OperationGuard, QueryOutput};
use futures_util::TryStreamExt;
use std::borrow::Cow;
use tiberius::QueryItem;

/// Rows of a page when the request says `limit: 0`.
const DEFAULT_PAGE_ROWS: u64 = 200;
/// No page is larger than this, whatever the request asks.
const MAX_PAGE_ROWS: u64 = 100_000;

/// The page size a request's `limit` stands for.
pub(crate) fn page_rows(limit: u64) -> u64 {
    match limit {
        0 => DEFAULT_PAGE_ROWS,
        limit => limit.min(MAX_PAGE_ROWS),
    }
}

fn flag(request: &QueryRequest, name: &str) -> bool {
    request.payload.get(name).and_then(serde_json::Value::as_bool) == Some(true)
}

enum Outcome {
    Rows {
        batch: arrow_array::RecordBatch,
        columns: Vec<PageColumn>,
        truncated: bool,
    },
    Command {
        affected_rows: u64,
    },
}

struct Job<'a> {
    database: Option<&'a str>,
    sql: &'a str,
    analysis: &'a Analysis,
    offset: u64,
    limit: u64,
    run_to_end: bool,
}

/// Runs `request` on the connection and returns the page.
pub(crate) fn open_query(
    connection: &MsConnection,
    request: &QueryRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<QueryOutput, BackendError> {
    if request.kind != "sql" && request.kind != "sql_query" {
        return Err(BackendError::with_code("query.unsupported", "driver accepts kind=sql"));
    }
    let sql = request
        .payload
        .get("sql")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if sql.trim().is_empty() {
        return Err(BackendError::with_code("query.bad_request", "missing SQL payload"));
    }
    let analysis = analyze(sql, &lex_options());
    if flag(request, "read_only") {
        if let Some(why) = &analysis.read_only_refusal {
            return Err(BackendError::with_code(codes::QUERY_READ_ONLY_VIOLATION, why.clone()));
        }
    }
    if let Some(reason) = analysis.danger {
        if !flag(request, "confirmed") {
            return Err(BackendError::with_code(codes::QUERY_CONFIRMATION_REQUIRED, reason));
        }
    }
    if !analysis.pageable && request.offset > 0 {
        return Err(BackendError::with_code(
            codes::QUERY_NOT_PAGEABLE,
            "only a single read-only statement can be paged; this statement runs once, at offset 0",
        ));
    }
    if analysis.batches.is_empty() {
        // Nothing but comments and `GO` lines.
        return Ok(QueryOutput::command_complete(0, ()));
    }
    let job = Job {
        database: request.database.as_deref(),
        sql,
        analysis: &analysis,
        offset: request.offset,
        limit: page_rows(request.limit),
        run_to_end: flag(request, "run_to_end"),
    };
    let guard = connection.lock(Area::Query)?;
    // `active` owns the connection guard: the operation stops being active before the
    // connection is released.
    let mut active = operation.activate(guard)?;
    let mut client = active
        .client
        .take()
        .ok_or_else(|| BackendError::with_code(Area::Query.not_connected(), "connection closed"))?;
    let mut current_db = active.current_db.take();
    let result = block_on(cancellable(
        &mut client,
        connection.shared(),
        &|| active.cancel_requested(),
        async |client: &mut Client| execute(client, &mut current_db, &job).await,
    ));
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(stopped) => {
            // Cancelled: the attention was acknowledged and the session lives on. Lost: the
            // client is dropped and the connection is closed for good.
            if stopped == crate::conn::Stopped::Cancelled {
                active.client = Some(client);
                active.current_db = current_db;
            } else {
                active.mark_broken();
            }
            return Err(stopped.error(Area::Query));
        }
    };
    match outcome {
        Ok(outcome) => {
            active.client = Some(client);
            active.current_db = current_db;
            match outcome {
                Outcome::Command { affected_rows } => Ok(QueryOutput::command_complete(affected_rows, ())),
                // A statement that "succeeded" while a cancel was requested did not produce
                // the rows the caller asked for.
                Outcome::Rows { .. } if active.cancel_requested() => Err(BackendError::with_code(
                    codes::QUERY_CANCELLED,
                    "the statement was cancelled",
                )),
                Outcome::Rows {
                    batch,
                    columns,
                    truncated,
                } => Ok(QueryOutput {
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
            }
        }
        Err(error) => {
            if error.code.as_deref() == Some(Area::Query.not_connected()) {
                // A lost connection is closed for good: every later call says so.
                active.mark_broken();
            } else {
                active.client = Some(client);
                active.current_db = current_db;
            }
            Err(error)
        }
    }
}

/// One result set being read: its rows so far and how many to skip.
struct Set {
    collector: Collector,
    skip: u64,
    /// A row past the page was seen.
    more: bool,
}

async fn execute(
    client: &mut Client,
    current_db: &mut Option<String>,
    job: &Job<'_>,
) -> Result<Outcome, BackendError> {
    use_database(client, current_db, Area::Query, job.database).await?;
    if job.analysis.changes_database {
        // `USE` / `DROP DATABASE` change which database is current without telling us.
        *current_db = None;
    }
    let pageable = job.analysis.pageable && !job.run_to_end;
    let mut kept: Option<Set> = None;
    for batch in &job.analysis.batches {
        let text = &job.sql[batch.range.clone()];
        for _ in 0..batch.repeat {
            let (statement, skip, server_paged) = if pageable {
                match with_offset_fetch(text, &lex_options(), job.offset, job.limit) {
                    Some(paged) => (Cow::Owned(paged), 0, true),
                    None => (Cow::Borrowed(text), job.offset, false),
                }
            } else {
                (Cow::Borrowed(text), 0, false)
            };
            // Only a statement the client pages may be stopped at the page's end.
            let stop_at_page_end = pageable && !server_paged;
            let mut stream = client
                .simple_query(statement)
                .await
                .map_err(|error| map_error(Area::Query, &error))?;
            let mut current: Option<Set> = None;
            let mut stopped = false;
            while let Some(item) = stream
                .try_next()
                .await
                .map_err(|error| map_error(Area::Query, &error))?
            {
                match item {
                    // A later result set replaces an earlier one: the page is the last.
                    QueryItem::Metadata(meta) => {
                        current = Some(Set {
                            collector: Collector::new(meta.columns(), job.limit),
                            skip,
                            more: false,
                        });
                    }
                    QueryItem::Row(row) => {
                        let Some(set) = current.as_mut() else { continue };
                        if set.skip > 0 {
                            set.skip -= 1;
                        } else if (set.collector.rows() as u64) < job.limit {
                            set.collector.push_row(row.cells().map(|(_, cell)| cell))?;
                        } else {
                            set.more = true;
                            if stop_at_page_end {
                                stopped = true;
                                break;
                            }
                        }
                    }
                }
            }
            drop(stream);
            if stopped {
                // The rest of the result is not wanted: attention, then drain to its ack.
                client
                    .cancel_query()
                    .await
                    .map_err(|error| map_error(Area::Query, &error))?;
            }
            if current.is_some() {
                kept = current;
            }
        }
    }
    match kept {
        Some(set) => {
            let truncated = set.more && pageable;
            let (batch, columns) = set.collector.finish()?;
            Ok(Outcome::Rows {
                batch,
                columns,
                truncated,
            })
        }
        None => Ok(Outcome::Command {
            affected_rows: rowcount(client).await,
        }),
    }
}

/// The session's `@@ROWCOUNT`: the rows the last statement affected (0 when it cannot be read).
async fn rowcount(client: &mut Client) -> u64 {
    let Ok(stream) = client.simple_query("SELECT CAST(@@ROWCOUNT AS bigint)").await else {
        return 0;
    };
    let Ok(rows) = stream.into_first_result().await else {
        return 0;
    };
    rows.first()
        .map_or(0, |row| u64::try_from(crate::catalog::int(row, 0)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_sizes_default_and_cap() {
        assert_eq!(page_rows(0), 200);
        assert_eq!(page_rows(7), 7);
        assert_eq!(page_rows(10_000_000), 100_000);
    }
}
