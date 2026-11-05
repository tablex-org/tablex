//! ABI v2 Redis driver on `engine-driver-kit`.
//!
//! Redis is not relational, so its data model is mapped onto the navigator and
//! Arrow pages as follows. Everything is binary safe: keys and values are
//! arbitrary bytes and never go through a lossy UTF-8 conversion.
//!
//! # Capabilities
//!
//! `navigator`, `query.arrow_c_data`, `query.redis_key`, `query.redis_command`
//! and one `mutation.<kind>` token per mutation kind (see [`mutation`]). Editors
//! `["redis_command"]`, navigator kinds `["database", "key"]`, no `sql_dialect`.
//! `operation.cancel` is deliberately **not** advertised: Redis has no statement
//! cancel, and a blocking command cannot be interrupted without closing the
//! connection (blocking verbs are therefore rejected, see [`command`]).
//!
//! # Navigator ([`navigator`])
//!
//! * root (`parent` and `database` absent) lists databases: `id`
//!   `database:<n>`, `kind` `database`, label `db<n> (<keys> keys)`, payload
//!   `{"database":"<n>","db":<n>,"keys":<keys>}`.
//! * a database (`database`, `payload.database`/`payload.db` or `parent`
//!   `database:<n>`) lists keys with `SCAN`. `cursor` is the Redis cursor,
//!   `limit` the wanted page size (a hint: `SCAN` may return a few more keys;
//!   `0` means 500, capped at 10 000). `payload.pattern` is a `MATCH` glob and
//!   `payload.type` a `TYPE` filter (`string`, `hash`, `list`, `set`, `zset`,
//!   `stream`). `next_cursor` is `null` once the keyspace is exhausted. As with
//!   every `SCAN`, a key can in rare cases be returned twice across pages.
//! * a key node has `id` `key:<n>:<base64 key>`, `kind` `key`, `subtitle` and
//!   `icon` the Redis type, and payload `{"database","db","key_raw"(base64),
//!   "key_type","ttl"(seconds|null),"size"(elements|bytes|null),"preview"?}`.
//!
//! # Queries ([`key_page`], [`command`])
//!
//! `QueryRequest.kind`:
//!
//! * `redis.key` — `database` = db index (default `0`), key = `payload.key_raw`
//!   (base64) or, when absent, the UTF-8 bytes of `object`. Returns one typed
//!   page of the key's value, honoring `offset`/`limit` (`0` = 1000 rows, max
//!   100 000) with the native ranged command, so a page of a 100k element value
//!   is fetched without loading the rest. `total_count` is the exact element
//!   count (`HLEN`/`LLEN`/`SCARD`/`ZCARD`/`XLEN`; a string is one row).
//!
//!   | type   | columns                                   | paging command |
//!   |--------|-------------------------------------------|----------------|
//!   | string | `value` Binary                            | `GETRANGE` (first `payload.max_bytes`, default 1 MiB, flagged `truncated`) |
//!   | hash   | `field` Binary, `value` Binary            | `HSCAN` (skips `offset` entries; cost O(offset)) |
//!   | list   | `index` Int64, `value` Binary             | `LRANGE` |
//!   | set    | `member` Binary                           | `SSCAN` (skips `offset` entries) |
//!   | zset   | `member` Binary, `score` Float64          | `ZRANGE .. WITHSCORES` (by rank) |
//!   | stream | `id` Utf8, `fields` Utf8                  | `XRANGE` (exclusive `(id` cursor, skips `offset`) |
//!
//!   `HSCAN`/`SSCAN` order is the server's cursor order, stable while the value
//!   is not modified. `fields` is a JSON object text `{"field":"value",..}` (a
//!   stream entry may repeat a field name; repeats are kept) in display form.
//! * `redis.key_info` — the same key addressing as `redis.key`; one row that
//!   describes the key without reading its value (the header of a value view):
//!   `type` Utf8, `ttl` Int64 (seconds, null = no expiry), `length` Int64
//!   (elements; the byte length of a string), `encoding` Utf8 and `memory`
//!   Int64 (bytes), both null when the server does not report them.
//! * `redis.command` — `payload.command` (`GET k`, `HGETALL k`, quoted args
//!   allowed), optional `payload.confirmed`. Safety comes from
//!   [`crate::classify_command`]: `Blocked` fails with `query.blocked`,
//!   `Confirm` fails with `query.confirmation_required` unless confirmed. The
//!   reply becomes a page: `OK` -> `command_complete`; a scalar -> one row
//!   `value`; an array -> rows `index`, `value` (`HGETALL` and RESP3 maps ->
//!   `field`, `value`). `value` is Int64 when every element is an integer,
//!   Binary when every element is a bulk string or nil, otherwise Utf8 text.
//!   `offset`/`limit` slice the reply; only read-only (`Allowed`) commands can
//!   be paged past offset 0, because paging re-runs the command.
//!
//! # Binary safety
//!
//! Byte strings (keys, hash fields, values, members, list items) are Arrow
//! `Binary`, so the page holds the exact bytes. `render_cell_batch` and
//! `inspect_row` show them through [`cells::write_display`]: valid UTF-8 as is,
//! anything else with non-printable and non-UTF-8 bytes as `\xNN` (and `\\` for
//! a backslash), which is for display only. The exact bytes are available from
//! the Arrow page and, in `inspect_row`, as `{"base64": ".."}` when the value is
//! not UTF-8. Keys in requests use base64 (`key_raw`, `field_raw`, ...).
//!
//! # Mutations ([`mutation`])
//!
//! Kinds `set_string`, `set_ttl`, `delete_key`, `delete_keys`, `hset`, `hdel`,
//! `lpush`, `rpush`, `lset`, `lrem`, `sadd`, `srem`, `zadd`, `zrem`, `xadd`, `xdel`,
//! `rename_key`. The
//! identity is `{"db":<n>,"key_raw":"<base64>"|"key":"<utf8>"}` (what
//! `inspect_row` returns), arguments come from `payload` and fall back to the
//! identity, so an `inspect_row` identity plus the new value is a valid request.
//! Every mutation invalidates the connection's open pages.

mod cells;
mod command;
mod key_page;
mod mutation;
mod navigator;

use crate::{RedisConn, RUNTIME};
use arrow_array::RecordBatch;
use engine_driver_kit::api::{
    Capability, DriverCapabilities, MutationRequest, NavigatorPage, NavigatorRequest, PageFlags,
    QueryRequest, RowFields,
};
use engine_driver_kit::{
    page_columns, ActiveOperation, BackendError, CellFormatter, ConnectRequest, DriverBackend,
    MutationOutcome, OperationGuard, Page, QueryOutput,
};
use redis::aio::MultiplexedConnection;
use std::future::Future;
use std::time::Duration;
use tokio::sync::MutexGuard;

const DRIVER: &str = "redis";
/// Rows of a page when the request's `limit` is `0`.
const DEFAULT_PAGE_ROWS: u64 = 1_000;
/// Hard cap on the rows of one page.
const MAX_PAGE_ROWS: u64 = 100_000;
/// One driver call (including waiting for the connection) may not exceed this.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-page data kept by the kit and handed back to `inspect_row`.
pub(crate) struct RedisPage {
    pub(crate) db: u32,
    /// `Some` for a `redis.key` page.
    pub(crate) key: Option<key_page::KeyContext>,
}

pub(crate) struct RedisBackend;

impl DriverBackend for RedisBackend {
    type Connection = RedisConn;
    type PageContext = RedisPage;
    type Canceller = ();

    fn capabilities() -> DriverCapabilities {
        let mut features = vec![
            Capability::Navigator,
            Capability::QueryArrowCData,
            Capability::QueryRedisKey,
            Capability::QueryRedisCommand,
        ];
        features.extend_from_slice(mutation::CAPABILITIES);
        let mut capabilities = DriverCapabilities::new(
            DRIVER,
            "Redis",
            features
                .into_iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
        );
        capabilities.uri_schemes = vec!["redis".into(), "rediss".into()];
        capabilities.editors = vec!["redis_command".into()];
        capabilities.navigator_kinds = vec!["database".into(), "key".into()];
        capabilities
    }

    fn connect(request: &ConnectRequest) -> Result<RedisConn, BackendError> {
        block_on(RedisConn::connect(&request.uri)).map_err(|message| {
            if message.starts_with("Redis URI must start") {
                BackendError::with_code("connect.bad_request", message)
            } else {
                BackendError::new(message)
            }
        })
    }

    fn list_nav_nodes(
        connection: &RedisConn,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        timed(navigator::list(connection, request))
    }

    fn open_query(
        connection: &RedisConn,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput<RedisPage>, BackendError> {
        match request.kind.as_str() {
            "redis.key" => {
                let target = key_page::Target::from_request(request)?;
                timed(async {
                    let mut active = lock(connection, operation).await?;
                    key_page::open(&mut active, &target, request).await
                })
            }
            "redis.key_info" => {
                let target = key_page::Target::from_request(request)?;
                timed(async {
                    let mut active = lock(connection, operation).await?;
                    key_page::info(&mut active, &target).await
                })
            }
            "redis.command" => {
                let plan = command::Plan::from_request(request)?;
                timed(async {
                    let mut active = lock(connection, operation).await?;
                    command::run(&mut active, &plan, request).await
                })
            }
            other => Err(BackendError::with_code(
                "query.unsupported",
                format!(
                    "Redis accepts kind=redis.key, redis.key_info or redis.command, got `{other}`"
                ),
            )),
        }
    }

    /// Redis has no statement cancel.
    fn canceller(_connection: &RedisConn) -> Result<(), BackendError> {
        Ok(())
    }

    fn cell_formatter(page: &Page<RedisPage>, column: usize) -> CellFormatter<'_> {
        cells::formatter(&page.batch, column)
    }

    fn inspect_row(page: &Page<RedisPage>, row: usize) -> Result<RowFields, BackendError> {
        Ok(cells::inspect_row(page, row))
    }

    fn mutate(
        connection: &RedisConn,
        request: &MutationRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        let plan = mutation::Plan::from_request(request)?;
        plan.check_server(connection.server_major)?;
        let affected = timed_mutation(async {
            let mut active = lock(connection, operation).await?;
            mutation::run(&mut active, &plan).await
        })?;
        Ok(MutationOutcome::new(affected))
    }
}

type Active<'a, 'op> = ActiveOperation<'a, MutexGuard<'op, MultiplexedConnection>>;

/// Takes the connection lock, then marks the operation active (it owns the
/// guard, so the operation ends before the lock is released).
async fn lock<'a, 'op: 'a>(
    connection: &'op RedisConn,
    operation: &'a mut OperationGuard<'_>,
) -> Result<Active<'a, 'op>, BackendError> {
    let guard = connection.connection.lock().await;
    operation.activate(guard)
}

fn block_on<F: Future>(future: F) -> F::Output {
    RUNTIME.block_on(future)
}

/// Runs a read-only `future` on the driver runtime under [`OPERATION_TIMEOUT`].
/// A timeout is retryable: nothing was written.
fn timed<T>(future: impl Future<Output = Result<T, BackendError>>) -> Result<T, BackendError> {
    timed_with(OPERATION_TIMEOUT, true, future)
}

/// Like [`timed`] for a write. A timeout is **not** retryable: the server may
/// have applied the command (`LPUSH`, `RPUSH`, `XADD`, `HSET`, ...) before the
/// answer was lost, and repeating it would apply it twice.
fn timed_mutation<T>(
    future: impl Future<Output = Result<T, BackendError>>,
) -> Result<T, BackendError> {
    timed_with(OPERATION_TIMEOUT, false, future)
}

fn timed_with<T>(
    timeout: Duration,
    retryable: bool,
    future: impl Future<Output = Result<T, BackendError>>,
) -> Result<T, BackendError> {
    block_on(async {
        tokio::time::timeout(timeout, future)
            .await
            .unwrap_or_else(|_| {
                let error = BackendError::with_code(
                    "operation.timeout",
                    format!("Redis did not answer within {}s", timeout.as_secs_f32()),
                );
                Err(if retryable { error.retryable() } else { error })
            })
    })
}

pub(crate) fn redis_error(context: &str, error: redis::RedisError) -> BackendError {
    BackendError::new(format!("{context}: {error}"))
}

/// `limit` of a request as a row count (`0` = the default page).
pub(crate) fn page_rows(limit: u64) -> u64 {
    if limit == 0 {
        DEFAULT_PAGE_ROWS
    } else {
        limit.min(MAX_PAGE_ROWS)
    }
}

pub(crate) fn type_label(data_type: &arrow_schema::DataType) -> String {
    use arrow_schema::DataType;
    match data_type {
        DataType::Binary => "bytes",
        DataType::Utf8 => "text",
        DataType::Int64 => "integer",
        DataType::Float64 => "double",
        DataType::Boolean => "boolean",
        _ => "unknown",
    }
    .into()
}

/// Wraps a batch into a [`QueryOutput`]. `truncated` means more rows exist
/// beyond this page.
pub(crate) fn query_output(
    batch: RecordBatch,
    total: u64,
    truncated: bool,
    read_only: bool,
    context: RedisPage,
) -> QueryOutput<RedisPage> {
    let columns = page_columns(&batch, type_label);
    QueryOutput {
        batch,
        columns,
        total_count: Some(total),
        total_is_exact: true,
        flags: PageFlags {
            truncated,
            cancelled: false,
            read_only,
        },
        context,
    }
}

/// Database index of a request: a number or numeric string.
pub(crate) fn parse_db(value: &serde_json::Value) -> Option<u32> {
    match value {
        serde_json::Value::Number(number) => number.as_u64().and_then(|n| u32::try_from(n).ok()),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_advertise_redis_tokens_without_cancel() {
        let capabilities = RedisBackend::capabilities();
        assert_eq!(capabilities.driver, "redis");
        assert!(capabilities.has(Capability::Navigator));
        assert!(capabilities.has(Capability::QueryArrowCData));
        assert!(capabilities.has(Capability::QueryRedisKey));
        assert!(capabilities.has(Capability::QueryRedisCommand));
        assert!(!capabilities.has(Capability::OperationCancel));
        assert!(!capabilities.has(Capability::QuerySql));
        assert!(capabilities.sql_dialect.is_none());
        assert_eq!(capabilities.editors, ["redis_command"]);
        assert_eq!(capabilities.navigator_kinds, ["database", "key"]);
        assert_eq!(capabilities.uri_schemes, ["redis", "rediss"]);
        assert!(capabilities.has_any_mutation());
        assert!(!capabilities.has_any_export());
        for kind in mutation::KINDS {
            assert!(capabilities.supports_mutation_kind(kind), "{kind}");
        }
        assert!(!capabilities.supports_mutation_kind("insert"));
    }

    #[test]
    fn timeouts_are_retryable_for_reads_and_never_for_writes() {
        let slow = || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<(), BackendError>(())
        };
        let read = timed_with(Duration::from_millis(20), true, slow()).unwrap_err();
        assert_eq!(read.code.as_deref(), Some("operation.timeout"));
        assert!(read.retryable);
        let write = timed_with(Duration::from_millis(20), false, slow()).unwrap_err();
        assert_eq!(write.code.as_deref(), Some("operation.timeout"));
        assert!(!write.retryable, "a timed-out write may have been applied");
        // The public wrappers pick the right flag (a fast future is unaffected).
        assert!(timed_mutation(async { Ok::<_, BackendError>(1) }).is_ok());
    }

    #[test]
    fn page_rows_defaults_and_caps() {
        assert_eq!(page_rows(0), DEFAULT_PAGE_ROWS);
        assert_eq!(page_rows(25), 25);
        assert_eq!(page_rows(u64::MAX), MAX_PAGE_ROWS);
    }

    #[test]
    fn db_index_accepts_numbers_and_strings() {
        assert_eq!(parse_db(&serde_json::json!(9)), Some(9));
        assert_eq!(parse_db(&serde_json::json!(" 3 ")), Some(3));
        assert_eq!(parse_db(&serde_json::json!(-1)), None);
        assert_eq!(parse_db(&serde_json::json!("x")), None);
        assert_eq!(parse_db(&serde_json::Value::Null), None);
    }
}
