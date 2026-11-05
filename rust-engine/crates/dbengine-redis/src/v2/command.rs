//! `redis.command`: run one command and show its reply as a page.
//!
//! Every page request runs the command again (there is no server-side
//! cursor), so `offset > 0` is only accepted for read-only (`Allowed`)
//! commands and a read page re-runs the read. The whole reply is buffered, so
//! it is capped: more than 100 000 elements or 16 MiB of bulk strings (nested
//! levels included) fail with `query.reply_too_large` (never truncated silently).
//!
//! What is bounded, precisely. The client library decodes a whole reply before
//! handing it over, so a reply can only be refused before it is sent if the
//! command is sized first:
//!
//! * `GET`, `HGET`, `GETRANGE` and `MGET`: exact byte size (`STRLEN`/`HSTRLEN`
//!   per key) and element count, checked before the command runs.
//! * `HGETALL`, `HKEYS`, `HVALS`, `SMEMBERS`, `LRANGE`, rank `ZRANGE`/`ZREVRANGE`
//!   and `XRANGE`/`XREVRANGE`: exact element count (`HLEN`, `SCARD`, `LLEN`,
//!   `ZCARD`, `XLEN`, the requested range or `COUNT`), and a byte *estimate*
//!   from `MEMORY USAGE` (Redis 4.0+) scaled by the share of the collection the
//!   reply covers: an upper bound for a whole collection, proportional for a
//!   range (an uneven range can be mis-estimated). `XRANGE` without `COUNT`
//!   counts the whole stream. If the server refuses `MEMORY USAGE`, only the
//!   element count is enforced up front.
//! * Every other command (`SCAN`, `ZRANGEBYSCORE`, `SORT_RO`, `INFO`, ...) is
//!   only checked after the reply arrived: the page never holds oversized data,
//!   but the transient memory of decoding it is not bounded.

use super::cells::display;
use super::{page_rows, parse_db, query_output, redis_error, RedisPage};
use crate::{classify_command, parse_redis_command, RedisCommandSafety};
use arrow_array::builder::BinaryBuilder;
use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use engine_driver_kit::api::QueryRequest;
use engine_driver_kit::{BackendError, QueryOutput};
use redis::aio::MultiplexedConnection;
use redis::Value;
use std::fmt::Write;
use std::sync::Arc;

/// Verbs that would block the shared connection or change its state for every
/// other operation. There is no cancel, so they are refused up front.
const UNSUPPORTED_VERBS: &[&str] = &[
    "BLPOP",
    "BRPOP",
    "BRPOPLPUSH",
    "BLMOVE",
    "BLMPOP",
    "BZPOPMIN",
    "BZPOPMAX",
    "BZMPOP",
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "UNSUBSCRIBE",
    "PUNSUBSCRIBE",
    "SUNSUBSCRIBE",
    "MONITOR",
    "SYNC",
    "PSYNC",
    "WAIT",
    "WAITAOF",
    "MULTI",
    "EXEC",
    "DISCARD",
    "WATCH",
    "UNWATCH",
    "HELLO",
    "RESET",
    "QUIT",
];

/// A validated `redis.command` request.
pub(super) struct Plan {
    db: u32,
    args: Vec<String>,
}

impl Plan {
    pub(super) fn from_request(request: &QueryRequest) -> Result<Self, BackendError> {
        let bad = |message: String| BackendError::with_code("query.bad_request", message);
        let command = request
            .payload
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let db = match request
            .database
            .as_deref()
            .filter(|database| !database.is_empty())
        {
            Some(text) => text
                .trim()
                .parse()
                .map_err(|_| bad("database must be a Redis db index".into()))?,
            None => request.payload.get("db").and_then(parse_db).unwrap_or(0),
        };
        let args = parse_redis_command(command).map_err(bad)?;
        let Some(verb) = args.first() else {
            return Err(bad("redis.command needs payload.command".into()));
        };
        let safety = classify_command(command);
        let confirmed = request
            .payload
            .get("confirmed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        match safety {
            RedisCommandSafety::Blocked => {
                return Err(BackendError::with_code(
                    "query.blocked",
                    format!("Redis command blocked: {command}"),
                ))
            }
            RedisCommandSafety::Confirm if !confirmed => {
                return Err(BackendError::with_code(
                    "query.confirmation_required",
                    format!("Redis command requires confirmation: {command}"),
                ))
            }
            _ => {}
        }
        if is_unsupported(&args) {
            return Err(BackendError::with_code(
                "query.unsupported",
                format!("Redis command `{verb}` blocks or changes the shared connection"),
            ));
        }
        if safety != RedisCommandSafety::Allowed && request.offset > 0 {
            return Err(BackendError::with_code(
                "query.unsupported",
                "paging re-runs the command; only read-only commands can be paged",
            ));
        }
        Ok(Self { db, args })
    }
}

fn is_unsupported(args: &[String]) -> bool {
    let verb = args[0].to_ascii_uppercase();
    UNSUPPORTED_VERBS.contains(&verb.as_str())
        || (matches!(verb.as_str(), "XREAD" | "XREADGROUP")
            && args.iter().any(|arg| arg.eq_ignore_ascii_case("BLOCK")))
}

/// Most elements (array items, or twice the pairs of a map) one command reply
/// may hold. Larger values are paged with `redis.key`.
const MAX_REPLY_ELEMENTS: u64 = 100_000;
/// Most bytes of bulk strings one command reply may hold.
const MAX_REPLY_BYTES: u64 = 16 << 20;

fn too_large(what: String) -> BackendError {
    BackendError::with_code(
        "query.reply_too_large",
        format!(
            "reply too large ({what}; limit {MAX_REPLY_ELEMENTS} elements / {} MiB): \
             page the value with a redis.key query or use a ranged command",
            MAX_REPLY_BYTES >> 20
        ),
    )
}

/// How to learn the size of a reply before running a reply-heavy command.
enum Probe<'a> {
    /// `length_command args..` (`STRLEN`, `HSTRLEN`, `HLEN`, `SCARD`, `LLEN`,
    /// `ZCARD`, `XLEN`) gives the length of the value the reply is made of;
    /// `args[0]` is its key.
    Sized {
        length_command: &'static str,
        args: Vec<&'a str>,
        shape: ReplyShape,
    },
    /// `MGET`: the `STRLEN` of every key.
    Keys(&'a [String]),
}

enum ReplyShape {
    /// A string reply: the whole value, or at most `at_most` bytes of it.
    Bytes { at_most: Option<u64> },
    /// The reply holds `per_item` elements per collection member.
    Members { per_item: u64 },
    /// `LRANGE`/`ZRANGE` `start stop`: members `start..=stop` (negative counts
    /// from the end), `per_item` elements each.
    Range {
        per_item: u64,
        start: i64,
        stop: i64,
    },
    /// `XRANGE`/`XREVRANGE`: `count` entries (`COUNT`), else the whole stream.
    Entries { count: Option<u64> },
}

/// Commands whose reply grows with a value, sized before they run: `GET`,
/// `HGET`, `GETRANGE`, `MGET`, `HGETALL`/`HKEYS`/`HVALS`, `SMEMBERS`, `LRANGE`,
/// rank-form `ZRANGE`/`ZREVRANGE` and `XRANGE`/`XREVRANGE`. Others are only
/// checked after the reply arrived (see [`check_reply_size`]).
fn probe(args: &[String]) -> Option<Probe<'_>> {
    let verb = args[0].to_ascii_uppercase();
    let key = args.get(1)?.as_str();
    let has = |word: &str| {
        args.iter()
            .skip(2)
            .any(|arg| arg.eq_ignore_ascii_case(word))
    };
    let int = |index: usize| args.get(index)?.parse::<i64>().ok();
    let range = |per_item| {
        Some(ReplyShape::Range {
            per_item,
            start: int(2)?,
            stop: int(3)?,
        })
    };
    let (length_command, args, shape) = match verb.as_str() {
        "MGET" => return Some(Probe::Keys(&args[1..])),
        "GET" if args.len() == 2 => ("STRLEN", vec![key], ReplyShape::Bytes { at_most: None }),
        "HGET" if args.len() == 3 => (
            "HSTRLEN",
            vec![key, args[2].as_str()],
            ReplyShape::Bytes { at_most: None },
        ),
        "GETRANGE" if args.len() == 4 => {
            let at_most = match (int(2), int(3)) {
                (Some(start), Some(end)) if start >= 0 && end >= 0 => {
                    Some((end - start + 1).max(0) as u64)
                }
                _ => None,
            };
            ("STRLEN", vec![key], ReplyShape::Bytes { at_most })
        }
        "HGETALL" => ("HLEN", vec![key], ReplyShape::Members { per_item: 2 }),
        "HKEYS" | "HVALS" => ("HLEN", vec![key], ReplyShape::Members { per_item: 1 }),
        "SMEMBERS" => ("SCARD", vec![key], ReplyShape::Members { per_item: 1 }),
        "LRANGE" => ("LLEN", vec![key], range(1)?),
        "ZRANGE" | "ZREVRANGE" if !has("BYSCORE") && !has("BYLEX") => (
            "ZCARD",
            vec![key],
            range(if has("WITHSCORES") { 2 } else { 1 })?,
        ),
        "XRANGE" | "XREVRANGE" => {
            let count = args
                .iter()
                .position(|arg| arg.eq_ignore_ascii_case("COUNT"))
                .and_then(|at| args.get(at + 1)?.parse().ok());
            ("XLEN", vec![key], ReplyShape::Entries { count })
        }
        _ => return None,
    };
    Some(Probe::Sized {
        length_command,
        args,
        shape,
    })
}

impl ReplyShape {
    /// `Err` when the reply to a value of `length` (bytes for strings, members
    /// otherwise) would hold too many elements or too many bytes.
    fn check(&self, length: u64) -> Result<(), BackendError> {
        match *self {
            ReplyShape::Bytes { at_most } => {
                let bytes = at_most.map_or(length, |limit| limit.min(length));
                if bytes > MAX_REPLY_BYTES {
                    Err(too_large(format!("a string of {bytes} bytes")))
                } else {
                    Ok(())
                }
            }
            ReplyShape::Members { per_item } => elements(length.saturating_mul(per_item)),
            ReplyShape::Entries { .. } => elements(self.selected(length).unwrap_or(0)),
            ReplyShape::Range { per_item, .. } => {
                elements(self.selected(length).unwrap_or(0).saturating_mul(per_item))
            }
        }
    }

    /// How many of the `length` members of a collection the reply covers (`None`
    /// for a string).
    fn selected(&self, length: u64) -> Option<u64> {
        match *self {
            ReplyShape::Bytes { .. } => None,
            ReplyShape::Members { .. } => Some(length),
            ReplyShape::Entries { count } => Some(count.map_or(length, |n| n.min(length))),
            ReplyShape::Range { start, stop, .. } => {
                let last = i128::from(length) - 1;
                let resolve = |index: i64| {
                    if index < 0 {
                        (i128::from(length) + i128::from(index)).max(0)
                    } else {
                        i128::from(index)
                    }
                };
                let count = (resolve(stop).min(last) - resolve(start) + 1).max(0);
                Some(u64::try_from(count).unwrap_or(u64::MAX))
            }
        }
    }

    /// Estimated reply bytes from the key's `MEMORY USAGE` (`usage`): the
    /// covered share of the collection's memory. Exact for a whole collection
    /// (an upper bound: memory includes overhead), proportional for a range.
    fn estimate_bytes(&self, usage: u64, length: u64) -> Option<u64> {
        let selected = self.selected(length)?;
        if length == 0 {
            return Some(0);
        }
        Some((u128::from(usage) * u128::from(selected) / u128::from(length)) as u64)
    }
}

fn elements(count: u64) -> Result<(), BackendError> {
    if count > MAX_REPLY_ELEMENTS {
        Err(too_large(format!("{count} elements")))
    } else {
        Ok(())
    }
}

/// Elements and bulk-string bytes of a whole reply, nested levels included.
fn tally(value: &Value, elements: &mut u64, bytes: &mut u64) {
    match value {
        Value::BulkString(data) => *bytes += data.len() as u64,
        Value::VerbatimString { text, .. } => *bytes += text.len() as u64,
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            *elements += items.len() as u64;
            for item in items {
                tally(item, elements, bytes);
            }
        }
        Value::Map(pairs) => {
            *elements += 2 * pairs.len() as u64;
            for (key, value) in pairs {
                tally(key, elements, bytes);
                tally(value, elements, bytes);
            }
        }
        Value::Attribute { data, .. } => tally(data, elements, bytes),
        _ => {}
    }
}

/// Post-flight cap for replies the probe cannot size, nested levels included.
/// The reply is already in memory when this runs (the client library decodes a
/// whole reply before returning it): it keeps oversized data out of the Arrow
/// page, it does not bound the transient memory of an unprobed command.
fn check_reply_size(reply: &Value) -> Result<(), BackendError> {
    let (mut element_count, mut bytes) = (0, 0);
    tally(reply, &mut element_count, &mut bytes);
    elements(element_count)?;
    if bytes > MAX_REPLY_BYTES {
        return Err(too_large(format!("{bytes} bytes")));
    }
    Ok(())
}

/// Sizes the reply of `probe` before the command runs, so a value over the
/// budget is refused without the server sending it: element counts from the
/// length commands (exact), string bytes from `STRLEN`/`HSTRLEN` (exact),
/// collection bytes from `MEMORY USAGE` (an estimate, skipped when the server
/// refuses the command).
async fn presize(
    con: &mut MultiplexedConnection,
    db: u32,
    probe: &Probe<'_>,
) -> Result<(), BackendError> {
    let failed = |error| redis_error("Redis command failed", error);
    match probe {
        Probe::Keys(keys) => {
            elements(keys.len() as u64)?;
            let mut pipe = redis::pipe();
            pipe.cmd("SELECT").arg(db).ignore();
            for key in keys.iter() {
                pipe.cmd("STRLEN").arg(key.as_str());
            }
            let lengths: Vec<u64> = pipe.query_async(con).await.map_err(failed)?;
            let total: u64 = lengths.iter().sum();
            if total > MAX_REPLY_BYTES {
                return Err(too_large(format!("{total} bytes over {} keys", keys.len())));
            }
        }
        Probe::Sized {
            length_command,
            args,
            shape,
        } => {
            let mut pipe = redis::pipe();
            pipe.cmd("SELECT").arg(db).ignore().cmd(length_command);
            for arg in args {
                pipe.arg(*arg);
            }
            let (length,): (u64,) = pipe.query_async(con).await.map_err(failed)?;
            shape.check(length)?;
            if shape.selected(length).is_some_and(|selected| selected > 0) {
                // Best effort: MEMORY USAGE is Redis 4.0+ and may be disabled.
                let usage: Option<u64> = redis::cmd("MEMORY")
                    .arg("USAGE")
                    .arg(args[0])
                    .query_async(con)
                    .await
                    .ok()
                    .flatten();
                if let Some(bytes) = usage.and_then(|usage| shape.estimate_bytes(usage, length)) {
                    if bytes > MAX_REPLY_BYTES {
                        return Err(too_large(format!("about {bytes} bytes, estimated")));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn run(
    con: &mut MultiplexedConnection,
    plan: &Plan,
    request: &QueryRequest,
) -> Result<QueryOutput<RedisPage>, BackendError> {
    if let Some(probe) = probe(&plan.args) {
        // Extra round trips, only for reply-heavy reads.
        presize(con, plan.db, &probe).await?;
    }
    let mut command = redis::cmd(&plan.args[0]);
    for arg in &plan.args[1..] {
        command.arg(arg.as_str());
    }
    let (reply,): (Value,) = redis::pipe()
        .cmd("SELECT")
        .arg(plan.db)
        .ignore()
        .add_command(command)
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis command failed", error))?;
    check_reply_size(&reply)?;
    let context = RedisPage {
        db: plan.db,
        key: None,
    };
    let verb = plan.args[0].to_ascii_uppercase();
    match reply_rows(reply, &verb, request.offset, page_rows(request.limit))? {
        Reply::Complete => Ok(QueryOutput::command_complete(0, context)),
        Reply::Rows {
            batch,
            total,
            truncated,
        } => Ok(query_output(batch, total, truncated, true, context)),
    }
}

enum Reply {
    /// A status reply (`OK`): nothing to show.
    Complete,
    Rows {
        batch: RecordBatch,
        total: u64,
        truncated: bool,
    },
}

fn reply_rows(value: Value, verb: &str, offset: u64, limit: u64) -> Result<Reply, BackendError> {
    let window = |len: usize| {
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(len);
        let end = start
            .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
            .min(len);
        (start, end)
    };
    Ok(match value {
        Value::Okay => Reply::Complete,
        Value::SimpleString(text) if text == "OK" => Reply::Complete,
        Value::ServerError(error) => {
            return Err(BackendError::new(format!(
                "Redis command failed: {error:?}"
            )))
        }
        Value::Attribute { data, .. } => return reply_rows(*data, verb, offset, limit),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            if verb == "HGETALL" && items.len() % 2 == 0 {
                let mut keys = Vec::with_capacity(items.len() / 2);
                let mut values = Vec::with_capacity(items.len() / 2);
                let mut iter = items.into_iter();
                while let (Some(key), Some(value)) = (iter.next(), iter.next()) {
                    keys.push(key);
                    values.push(value);
                }
                return Ok(pair_rows(("field", keys), ("value", values), window));
            }
            let total = items.len();
            let (start, end) = window(total);
            let slice = &items[start..end];
            let index: ArrayRef = Arc::new(Int64Array::from_iter_values(
                (start..end).map(|row| row as i64),
            ));
            let values = values_array(slice);
            let schema = schema(&[
                Field::new("index", DataType::Int64, false),
                value_field("value", &values),
            ]);
            rows(schema, vec![index, values], total, end)
        }
        Value::Map(pairs) => {
            let (keys, values): (Vec<Value>, Vec<Value>) = pairs.into_iter().unzip();
            pair_rows(("key", keys), ("value", values), window)
        }
        scalar => {
            let values = values_array(std::slice::from_ref(&scalar));
            let schema = schema(&[value_field("value", &values)]);
            rows(schema, vec![values], 1, 1)
        }
    })
}

fn pair_rows(
    (key_name, keys): (&str, Vec<Value>),
    (value_name, values): (&str, Vec<Value>),
    window: impl Fn(usize) -> (usize, usize),
) -> Reply {
    let total = keys.len();
    let (start, end) = window(total);
    let keys = values_array(&keys[start..end]);
    let values = values_array(&values[start..end]);
    let schema = schema(&[
        value_field(key_name, &keys),
        value_field(value_name, &values),
    ]);
    rows(schema, vec![keys, values], total, end)
}

fn rows(schema: Arc<Schema>, columns: Vec<ArrayRef>, total: usize, end: usize) -> Reply {
    Reply::Rows {
        batch: RecordBatch::try_new(schema, columns).expect("equal-length reply columns"),
        total: total as u64,
        truncated: end < total,
    }
}

fn schema(fields: &[Field]) -> Arc<Schema> {
    Arc::new(Schema::new(fields.to_vec()))
}

fn value_field(name: &str, array: &ArrayRef) -> Field {
    Field::new(name, array.data_type().clone(), true)
}

#[derive(PartialEq, Clone, Copy)]
enum Shape {
    Int,
    Double,
    Bool,
    Bytes,
    Other,
}

fn shape(value: &Value) -> Shape {
    match value {
        Value::Int(_) => Shape::Int,
        Value::Double(_) => Shape::Double,
        Value::Boolean(_) => Shape::Bool,
        Value::BulkString(_) | Value::Nil => Shape::Bytes,
        _ => Shape::Other,
    }
}

/// One column for `items`: Int64, Float64, Boolean or Binary when every item has
/// that shape (nil is a null), otherwise Utf8 text.
fn values_array(items: &[Value]) -> ArrayRef {
    let first = items.first().map_or(Shape::Bytes, shape);
    let uniform = items
        .iter()
        .all(|item| shape(item) == first || matches!(item, Value::Nil));
    match first {
        Shape::Int if uniform && items.iter().all(|item| matches!(item, Value::Int(_))) => {
            Arc::new(Int64Array::from_iter_values(items.iter().map(
                |item| match item {
                    Value::Int(number) => *number,
                    _ => unreachable!("checked above"),
                },
            )))
        }
        Shape::Double if uniform && items.iter().all(|item| matches!(item, Value::Double(_))) => {
            Arc::new(Float64Array::from_iter_values(items.iter().map(
                |item| match item {
                    Value::Double(number) => *number,
                    _ => unreachable!("checked above"),
                },
            )))
        }
        Shape::Bool if uniform && items.iter().all(|item| matches!(item, Value::Boolean(_))) => {
            Arc::new(BooleanArray::from(
                items
                    .iter()
                    .map(|item| matches!(item, Value::Boolean(true)))
                    .collect::<Vec<_>>(),
            ))
        }
        Shape::Bytes if uniform => {
            let mut builder = BinaryBuilder::with_capacity(items.len(), 0);
            for item in items {
                match item {
                    Value::BulkString(bytes) => builder.append_value(bytes),
                    _ => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
        _ => Arc::new(StringArray::from_iter(items.iter().map(
            |item| match item {
                Value::Nil => None,
                other => Some(value_text(other)),
            },
        ))),
    }
}

/// Text of one reply element; nested arrays and maps are shown inline.
fn value_text(value: &Value) -> String {
    let mut text = String::new();
    write_value(value, &mut text);
    text
}

fn write_value(value: &Value, out: &mut String) {
    // `write!` into a `String` cannot fail.
    match value {
        Value::Nil => out.push_str("(nil)"),
        Value::Int(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Double(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Boolean(flag) => {
            let _ = write!(out, "{flag}");
        }
        Value::BulkString(bytes) => out.push_str(&display(bytes)),
        Value::SimpleString(text) => out.push_str(text),
        Value::Okay => out.push_str("OK"),
        Value::VerbatimString { text, .. } => out.push_str(text),
        Value::BigNumber(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Map(pairs) => {
            out.push('{');
            for (index, (key, value)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(key, out);
                out.push_str(": ");
                write_value(value, out);
            }
            out.push('}');
        }
        Value::Attribute { data, .. } => write_value(data, out),
        Value::ServerError(error) => {
            let _ = write!(out, "{error:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::Int64Type;
    use arrow_array::Array;
    use engine_driver_kit::api::OperationId;

    fn bulk(text: &[u8]) -> Value {
        Value::BulkString(text.to_vec())
    }

    fn plan(command: &str, confirmed: bool, offset: u64) -> Result<Plan, BackendError> {
        Plan::from_request(&QueryRequest {
            kind: "redis.command".into(),
            operation_id: OperationId(1),
            database: Some("9".into()),
            object: None,
            offset,
            limit: 0,
            payload: serde_json::json!({"command": command, "confirmed": confirmed}),
        })
    }

    fn code(result: Result<Plan, BackendError>) -> String {
        result
            .err()
            .and_then(|error| error.code)
            .unwrap()
            .into_owned()
    }

    #[test]
    fn safety_rules_gate_the_command() {
        assert!(plan("GET k", false, 0).is_ok());
        assert_eq!(code(plan("KEYS *", true, 0)), "query.blocked");
        assert_eq!(
            code(plan("SET k v", false, 0)),
            "query.confirmation_required"
        );
        assert!(plan("SET k v", true, 0).is_ok());
        assert_eq!(code(plan("", false, 0)), "query.bad_request");
        assert_eq!(code(plan("SET \"x", true, 0)), "query.bad_request");
        assert_eq!(code(plan("BLPOP q 0", true, 0)), "query.unsupported");
        assert_eq!(
            code(plan("xread BLOCK 0 STREAMS s $", true, 0)),
            "query.unsupported"
        );
        assert_eq!(code(plan("MULTI", true, 0)), "query.unsupported");
        assert!(plan("XREAD COUNT 1 STREAMS s 0", false, 0).is_ok());
        // Paging would re-run a write.
        assert_eq!(code(plan("INCR n", true, 5)), "query.unsupported");
        assert!(plan("LRANGE k 0 -1", false, 5).is_ok());
    }

    fn args(command: &str) -> Vec<String> {
        parse_redis_command(command).unwrap()
    }

    fn probe_check(command: &str, length: u64) -> Result<(), BackendError> {
        let args = args(command);
        match probe(&args).expect("probed command") {
            Probe::Sized { shape, .. } => shape.check(length),
            Probe::Keys(_) => panic!("{command} is probed per key"),
        }
    }

    fn estimate(command: &str, usage: u64, length: u64) -> Option<u64> {
        let args = args(command);
        match probe(&args).expect("probed command") {
            Probe::Sized { shape, .. } => shape.estimate_bytes(usage, length),
            Probe::Keys(_) => None,
        }
    }

    #[test]
    fn reply_heavy_reads_are_sized_before_they_run() {
        assert!(probe(&args("PING")).is_none());
        assert!(probe(&args("GET")).is_none());
        assert!(probe(&args("ZRANGE z 0 -1 BYSCORE")).is_none());
        assert!(probe_check("GET k", MAX_REPLY_BYTES).is_ok());
        assert!(probe_check("GET k", MAX_REPLY_BYTES + 1).is_err());
        assert!(probe_check("HGETALL h", 50_000).is_ok());
        assert!(probe_check("HGETALL h", 50_001).is_err());
        assert!(probe_check("HKEYS h", 100_000).is_ok());
        assert!(probe_check("SMEMBERS s", 100_001).is_err());
        assert!(probe_check("LRANGE l 0 -1", 100_000).is_ok());
        let error = probe_check("LRANGE l 0 -1", 100_001).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("query.reply_too_large"));
        // The requested range, not the list length, decides.
        assert!(probe_check("LRANGE l 0 99", 10_000_000).is_ok());
        assert!(probe_check("LRANGE l -100 -1", 10_000_000).is_ok());
        assert!(probe_check("LRANGE l 5 -1", 10_000_000).is_err());
        assert!(probe_check("LRANGE l 500 400", 10_000_000).is_ok());
        assert!(probe_check("ZRANGE z 0 -1", 100_000).is_ok());
        assert!(probe_check("ZRANGE z 0 -1 WITHSCORES", 50_001).is_err());
        assert!(probe_check("ZREVRANGE z 0 9 WITHSCORES", 1_000_000).is_ok());
    }

    #[test]
    fn byte_budgets_cover_scalars_keys_streams_and_ranges() {
        // HGET / GETRANGE / MGET are probed by bytes.
        assert!(probe_check("HGET h f", MAX_REPLY_BYTES).is_ok());
        assert!(probe_check("HGET h f", MAX_REPLY_BYTES + 1).is_err());
        assert!(probe_check("GETRANGE k 0 99", MAX_REPLY_BYTES * 10).is_ok());
        assert!(probe_check("GETRANGE k 0 -1", MAX_REPLY_BYTES + 1).is_err());
        let mget = args("MGET a b c");
        assert!(matches!(probe(&mget), Some(Probe::Keys(keys)) if keys.len() == 3));
        // XRANGE counts entries: COUNT bounds it, otherwise the whole stream.
        assert!(probe_check("XRANGE s - + COUNT 10", 10_000_000).is_ok());
        assert!(probe_check("XRANGE s - +", 100_000).is_ok());
        assert!(probe_check("XRANGE s - +", 100_001).is_err());
        assert!(probe_check("XREVRANGE s + - COUNT 200000", 1_000_000).is_err());
        // Byte estimates: whole collections use all of MEMORY USAGE, ranges their share.
        assert_eq!(estimate("HGETALL h", 5_000, 100), Some(5_000));
        assert_eq!(estimate("LRANGE l 0 9", 1_000_000, 1_000), Some(10_000));
        assert_eq!(estimate("LRANGE l 0 -1", 1_000_000, 1_000), Some(1_000_000));
        assert_eq!(estimate("XRANGE s - + COUNT 5", 100, 10), Some(50));
        assert_eq!(estimate("HGETALL h", 5_000, 0), Some(0));
        assert_eq!(
            estimate("GET k", 5_000, 100),
            None,
            "strings are exact, not estimated"
        );
        // 1 GiB of memory over 1 000 members: a 10 member range is fine, all of it is not.
        assert!(estimate("LRANGE l 0 9", 1 << 30, 1_000).unwrap() < MAX_REPLY_BYTES);
        assert!(estimate("LRANGE l 0 -1", 1 << 30, 1_000).unwrap() > MAX_REPLY_BYTES);
    }

    #[test]
    fn nested_levels_count_towards_the_reply_budget() {
        // An XRANGE-shaped reply: few entries, huge field values.
        let entry = |size: usize| {
            Value::Array(vec![
                Value::BulkString(b"1-0".to_vec()),
                Value::Array(vec![
                    Value::BulkString(b"f".to_vec()),
                    Value::BulkString(vec![0; size]),
                ]),
            ])
        };
        let fat = Value::Array(vec![entry((MAX_REPLY_BYTES / 2) as usize + 1); 2]);
        let error = check_reply_size(&fat).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("query.reply_too_large"));
        assert!(check_reply_size(&Value::Array(vec![entry(1024); 3])).is_ok());
        // Many small nested elements.
        let wide = Value::Array(vec![
            Value::Array(vec![
                Value::Int(1);
                MAX_REPLY_ELEMENTS as usize / 2
            ]);
            3
        ]);
        assert!(check_reply_size(&wide).is_err());
        let attribute = Value::Attribute {
            data: Box::new(Value::BulkString(vec![0; MAX_REPLY_BYTES as usize + 1])),
            attributes: vec![],
        };
        assert!(check_reply_size(&attribute).is_err());
    }

    #[test]
    fn oversized_replies_fail_after_arrival_for_unprobed_commands() {
        let many = Value::Array(vec![Value::Int(1); MAX_REPLY_ELEMENTS as usize + 1]);
        let error = check_reply_size(&many).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("query.reply_too_large"));
        assert!(check_reply_size(&Value::Array(vec![Value::Int(1); 10])).is_ok());
        let map = Value::Map(vec![
            (Value::Int(1), Value::Int(2));
            MAX_REPLY_ELEMENTS as usize / 2 + 1
        ]);
        assert!(check_reply_size(&map).is_err());
        let big = Value::BulkString(vec![0; MAX_REPLY_BYTES as usize + 1]);
        assert!(check_reply_size(&big).is_err());
        let fat = Value::Array(vec![
            Value::BulkString(vec![
                0;
                (MAX_REPLY_BYTES / 2) as usize
                    + 1
            ]);
            2
        ]);
        assert!(check_reply_size(&fat).is_err());
        assert!(check_reply_size(&Value::BulkString(vec![0; 1024])).is_ok());
    }

    #[test]
    fn status_reply_is_command_complete() {
        assert!(matches!(
            reply_rows(Value::Okay, "SET", 0, 10),
            Ok(Reply::Complete)
        ));
        let pong = reply_rows(Value::SimpleString("PONG".into()), "PING", 0, 10)
            .ok()
            .unwrap();
        let Reply::Rows { batch, total, .. } = pong else {
            panic!("PONG is a row")
        };
        assert_eq!(total, 1);
        assert_eq!(batch.column(0).as_string::<i32>().value(0), "PONG");
    }

    #[test]
    fn scalars_become_one_typed_row() {
        let Ok(Reply::Rows { batch, .. }) = reply_rows(Value::Int(7), "DEL", 0, 10) else {
            panic!()
        };
        assert_eq!(batch.column(0).as_primitive::<Int64Type>().value(0), 7);
        let Ok(Reply::Rows { batch, .. }) = reply_rows(bulk(b"a\xff"), "GET", 0, 10) else {
            panic!()
        };
        assert_eq!(batch.column(0).as_binary::<i32>().value(0), b"a\xff");
        let Ok(Reply::Rows { batch, .. }) = reply_rows(Value::Nil, "GET", 0, 10) else {
            panic!()
        };
        assert!(batch.column(0).is_null(0));
        assert_eq!(batch.column(0).data_type(), &DataType::Binary);
    }

    #[test]
    fn arrays_are_sliced_and_typed_by_content() {
        let items = (0..10).map(|n| bulk(format!("v{n}").as_bytes())).collect();
        let Ok(Reply::Rows {
            batch,
            total,
            truncated,
        }) = reply_rows(Value::Array(items), "LRANGE", 3, 4)
        else {
            panic!()
        };
        assert_eq!((total, truncated, batch.num_rows()), (10, true, 4));
        assert_eq!(batch.column(0).as_primitive::<Int64Type>().value(0), 3);
        assert_eq!(batch.column(1).as_binary::<i32>().value(0), b"v3");

        let ints = Value::Array(vec![Value::Int(1), Value::Int(2)]);
        let Ok(Reply::Rows {
            batch, truncated, ..
        }) = reply_rows(ints, "SMISMEMBER", 0, 10)
        else {
            panic!()
        };
        assert!(!truncated);
        assert_eq!(batch.column(1).data_type(), &DataType::Int64);

        let mixed = Value::Array(vec![
            bulk(b"x"),
            Value::Int(2),
            Value::Nil,
            Value::Array(vec![bulk(b"y")]),
        ]);
        let Ok(Reply::Rows { batch, .. }) = reply_rows(mixed, "SCAN", 0, 10) else {
            panic!()
        };
        let text = batch.column(1).as_string::<i32>();
        assert_eq!(
            (text.value(0), text.value(1), text.is_null(2), text.value(3)),
            ("x", "2", true, "[y]")
        );
    }

    #[test]
    fn hgetall_flat_reply_becomes_field_value_rows() {
        let flat = Value::Array(vec![bulk(b"f1"), bulk(b"v1"), bulk(b"f2"), bulk(b"v2")]);
        let Ok(Reply::Rows { batch, total, .. }) = reply_rows(flat, "HGETALL", 1, 10) else {
            panic!()
        };
        assert_eq!((total, batch.num_rows()), (2, 1));
        assert_eq!(batch.schema().field(0).name(), "field");
        assert_eq!(batch.column(0).as_binary::<i32>().value(0), b"f2");
        assert_eq!(batch.column(1).as_binary::<i32>().value(0), b"v2");
    }

    #[test]
    fn maps_and_server_errors() {
        let map = Value::Map(vec![(bulk(b"k"), Value::Int(1))]);
        let Ok(Reply::Rows { batch, .. }) = reply_rows(map, "CONFIG", 0, 10) else {
            panic!()
        };
        assert_eq!(batch.schema().field(0).name(), "key");
        assert_eq!(batch.column(1).data_type(), &DataType::Int64);
        assert!(reply_rows(Value::Array(vec![]), "LRANGE", 0, 10).is_ok());
    }
}
