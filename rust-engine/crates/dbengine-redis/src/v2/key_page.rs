//! `redis.key`: one typed Arrow page of a key's value.

use super::{page_rows, parse_db, query_output, redis_error, RedisPage};
use crate::{redis_key_bytes_to_display, redis_key_raw_to_bytes};
use arrow_array::builder::BinaryBuilder;
use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use engine_driver_kit::api::QueryRequest;
use engine_driver_kit::{BackendError, QueryOutput};
use redis::aio::MultiplexedConnection;
use std::sync::Arc;

/// Default and maximum bytes fetched from a string value.
const DEFAULT_STRING_BYTES: u64 = 1 << 20;
const MAX_STRING_BYTES: u64 = 64 << 20;
/// Largest `COUNT` sent to `HSCAN`/`SSCAN`/`XRANGE` in one round trip.
const MAX_CHUNK: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
}

impl KeyType {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "string" => Self::String,
            "hash" => Self::Hash,
            "list" => Self::List,
            "set" => Self::Set,
            "zset" => Self::ZSet,
            "stream" => Self::Stream,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::ZSet => "zset",
            Self::Stream => "stream",
        }
    }
}

/// What `inspect_row` needs to know about a key page.
pub(crate) struct KeyContext {
    pub(crate) key: Vec<u8>,
    pub(crate) key_type: KeyType,
    pub(crate) ttl: Option<i64>,
}

/// Parsed `redis.key` request.
pub(super) struct Target {
    db: u32,
    key: Vec<u8>,
    max_string_bytes: u64,
}

impl Target {
    pub(super) fn from_request(request: &QueryRequest) -> Result<Self, BackendError> {
        let bad = |message: &str| BackendError::with_code("query.bad_request", message.to_owned());
        let db = match request
            .database
            .as_deref()
            .filter(|database| !database.is_empty())
        {
            Some(text) => text
                .trim()
                .parse()
                .map_err(|_| bad("database must be a Redis db index"))?,
            None => request.payload.get("db").and_then(parse_db).unwrap_or(0),
        };
        let key = match request
            .payload
            .get("key_raw")
            .and_then(serde_json::Value::as_str)
        {
            Some(raw) => redis_key_raw_to_bytes(raw)
                .map_err(|error| BackendError::with_code("query.bad_request", error))?,
            None => request.object.clone().unwrap_or_default().into_bytes(),
        };
        if key.is_empty() {
            return Err(bad("redis.key needs a key (payload.key_raw or object)"));
        }
        let max_string_bytes = request
            .payload
            .get("max_bytes")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_STRING_BYTES, |bytes| {
                bytes.clamp(1, MAX_STRING_BYTES)
            });
        Ok(Self {
            db,
            key,
            max_string_bytes,
        })
    }
}

fn select(db: u32) -> redis::Pipeline {
    let mut pipe = redis::pipe();
    pipe.cmd("SELECT").arg(db).ignore();
    pipe
}

pub(super) async fn open(
    con: &mut MultiplexedConnection,
    target: &Target,
    request: &QueryRequest,
) -> Result<QueryOutput<RedisPage>, BackendError> {
    let key = target.key.as_slice();
    let (type_name, ttl): (String, i64) = select(target.db)
        .cmd("TYPE")
        .arg(key)
        .cmd("TTL")
        .arg(key)
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis TYPE failed", error))?;
    let key_type = match type_name.as_str() {
        "none" => {
            return Err(BackendError::with_code(
                "query.key_not_found",
                format!(
                    "Redis key `{}` does not exist",
                    redis_key_bytes_to_display(key)
                ),
            ))
        }
        name => KeyType::parse(name).ok_or_else(|| {
            BackendError::with_code(
                "query.unsupported",
                format!("Redis type `{name}` cannot be shown as a page"),
            )
        })?,
    };
    let offset = request.offset;
    let limit = page_rows(request.limit);
    let (batch, total, cut) = match key_type {
        KeyType::String => string_page(con, key, offset, target.max_string_bytes).await?,
        KeyType::Hash => with_total(hash_page(con, key, offset, limit).await?),
        KeyType::List => with_total(list_page(con, key, offset, limit).await?),
        KeyType::Set => with_total(set_page(con, key, offset, limit).await?),
        KeyType::ZSet => with_total(zset_page(con, key, offset, limit).await?),
        KeyType::Stream => with_total(stream_page(con, key, offset, limit).await?),
    };
    let truncated = cut || offset.saturating_add(batch.num_rows() as u64) < total;
    Ok(query_output(
        batch,
        total,
        truncated,
        false,
        RedisPage {
            db: target.db,
            key: Some(KeyContext {
                key: target.key.clone(),
                key_type,
                ttl: (ttl >= 0).then_some(ttl),
            }),
        },
    ))
}

/// `redis.key_info`: one row describing the key itself (header of a value
/// view): `type`, `ttl` (seconds, null = no expiry), `length` (elements; the
/// byte length of a string), `encoding` and `memory` (bytes, null when the
/// server does not report them). Never reads the value.
pub(super) async fn info(
    con: &mut MultiplexedConnection,
    target: &Target,
) -> Result<QueryOutput<RedisPage>, BackendError> {
    let key = target.key.as_slice();
    let (type_name, ttl): (String, i64) = select(target.db)
        .cmd("TYPE")
        .arg(key)
        .cmd("TTL")
        .arg(key)
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis TYPE failed", error))?;
    let key_type = match type_name.as_str() {
        "none" => {
            return Err(BackendError::with_code(
                "query.key_not_found",
                format!(
                    "Redis key `{}` does not exist",
                    redis_key_bytes_to_display(key)
                ),
            ))
        }
        // A module type still has a type, a TTL and (maybe) an encoding.
        name => KeyType::parse(name),
    };
    let length_command = match key_type {
        Some(KeyType::String) => Some("STRLEN"),
        Some(KeyType::Hash) => Some("HLEN"),
        Some(KeyType::List) => Some("LLEN"),
        Some(KeyType::Set) => Some("SCARD"),
        Some(KeyType::ZSet) => Some("ZCARD"),
        Some(KeyType::Stream) => Some("XLEN"),
        None => None,
    };
    let length: Option<i64> = match length_command {
        Some(command) => Some(
            redis::cmd(command)
                .arg(key)
                .query_async(con)
                .await
                .map_err(|error| redis_error(&format!("Redis {command} failed"), error))?,
        ),
        None => None,
    };
    // Optional details: an old server or a restricted ACL user may refuse them.
    let encoding: Option<String> = redis::cmd("OBJECT")
        .arg("ENCODING")
        .arg(key)
        .query_async(con)
        .await
        .ok()
        .flatten();
    let memory: Option<i64> = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg(key)
        .query_async(con)
        .await
        .ok()
        .flatten();
    let fields = vec![
        Field::new("type", DataType::Utf8, false),
        Field::new("ttl", DataType::Int64, true),
        Field::new("length", DataType::Int64, true),
        Field::new("encoding", DataType::Utf8, true),
        Field::new("memory", DataType::Int64, true),
    ];
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from(vec![Some(type_name.as_str())])),
        Arc::new(Int64Array::from(vec![(ttl >= 0).then_some(ttl)])),
        Arc::new(Int64Array::from(vec![length])),
        Arc::new(StringArray::from(vec![encoding.as_deref()])),
        Arc::new(Int64Array::from(vec![memory])),
    ];
    let batch = RecordBatch::try_new(schema(fields), arrays).expect("equal-length info columns");
    Ok(query_output(
        batch,
        1,
        false,
        true,
        // An info row is not an element of the key: nothing to inspect.
        RedisPage {
            db: target.db,
            key: None,
        },
    ))
}

type Entries = Vec<Vec<u8>>;

/// `(page, total rows)` of the collection types, `cut` is never set.
fn with_total((batch, total): (RecordBatch, u64)) -> (RecordBatch, u64, bool) {
    (batch, total, false)
}

/// A string is one row; `cut` tells that the value is longer than `max_bytes`.
async fn string_page(
    con: &mut MultiplexedConnection,
    key: &[u8],
    offset: u64,
    max_bytes: u64,
) -> Result<(RecordBatch, u64, bool), BackendError> {
    let (length, value): (u64, Vec<u8>) = redis::pipe()
        .cmd("STRLEN")
        .arg(key)
        .cmd("GETRANGE")
        .arg(key)
        .arg(0)
        .arg(max_bytes - 1)
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis GET failed", error))?;
    if offset > 0 {
        return Ok((bytes_batch::<Vec<u8>>(&[("value", &[])]), 1, false));
    }
    Ok((bytes_batch(&[("value", &[value])]), 1, length > max_bytes))
}

async fn hash_page(
    con: &mut MultiplexedConnection,
    key: &[u8],
    offset: u64,
    limit: u64,
) -> Result<(RecordBatch, u64), BackendError> {
    let (total, first): (u64, (String, Entries)) = redis::pipe()
        .cmd("HLEN")
        .arg(key)
        .cmd("HSCAN")
        .arg(key)
        .arg(0)
        .arg("COUNT")
        .arg(chunk(offset, limit))
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis HSCAN failed", error))?;
    let flat = scan_window(con, "HSCAN", key, offset, limit, first, 2).await?;
    let (fields, values) = split_pairs(flat);
    Ok((
        bytes_batch(&[("field", &fields), ("value", &values)]),
        total,
    ))
}

async fn set_page(
    con: &mut MultiplexedConnection,
    key: &[u8],
    offset: u64,
    limit: u64,
) -> Result<(RecordBatch, u64), BackendError> {
    let (total, first): (u64, (String, Entries)) = redis::pipe()
        .cmd("SCARD")
        .arg(key)
        .cmd("SSCAN")
        .arg(key)
        .arg(0)
        .arg("COUNT")
        .arg(chunk(offset, limit))
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis SSCAN failed", error))?;
    let members = scan_window(con, "SSCAN", key, offset, limit, first, 1).await?;
    Ok((bytes_batch(&[("member", &members)]), total))
}

async fn list_page(
    con: &mut MultiplexedConnection,
    key: &[u8],
    offset: u64,
    limit: u64,
) -> Result<(RecordBatch, u64), BackendError> {
    let (start, stop) = rank_range(offset, limit);
    let (total, values): (u64, Entries) = redis::pipe()
        .cmd("LLEN")
        .arg(key)
        .cmd("LRANGE")
        .arg(key)
        .arg(start)
        .arg(stop)
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis LRANGE failed", error))?;
    Ok((list_batch(offset, &values), total))
}

async fn zset_page(
    con: &mut MultiplexedConnection,
    key: &[u8],
    offset: u64,
    limit: u64,
) -> Result<(RecordBatch, u64), BackendError> {
    let (start, stop) = rank_range(offset, limit);
    let (total, values): (u64, Vec<(Vec<u8>, f64)>) = redis::pipe()
        .cmd("ZCARD")
        .arg(key)
        .cmd("ZRANGE")
        .arg(key)
        .arg(start)
        .arg(stop)
        .arg("WITHSCORES")
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis ZRANGE failed", error))?;
    Ok((zset_batch(&values), total))
}

type StreamEntry = (Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);

async fn stream_page(
    con: &mut MultiplexedConnection,
    key: &[u8],
    offset: u64,
    limit: u64,
) -> Result<(RecordBatch, u64), BackendError> {
    let mut requested = chunk(offset, limit);
    let (total, mut entries): (u64, Vec<StreamEntry>) = redis::pipe()
        .cmd("XLEN")
        .arg(key)
        .cmd("XRANGE")
        .arg(key)
        .arg("-")
        .arg("+")
        .arg("COUNT")
        .arg(requested)
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis XRANGE failed", error))?;
    let mut skip = offset;
    let mut page: Vec<StreamEntry> = Vec::new();
    loop {
        let exhausted = (entries.len() as u64) < requested;
        let last_id = entries.last().map(|(id, _)| id.clone());
        let skipped = skip.min(entries.len() as u64) as usize;
        skip -= skipped as u64;
        let room = (limit - page.len() as u64) as usize;
        page.extend(entries.drain(skipped..).take(room));
        if page.len() as u64 >= limit || exhausted {
            break;
        }
        let Some(last_id) = last_id else { break };
        let mut start = Vec::with_capacity(last_id.len() + 1);
        start.push(b'(');
        start.extend_from_slice(&last_id);
        requested = chunk(skip, limit - page.len() as u64);
        entries = redis::cmd("XRANGE")
            .arg(key)
            .arg(start)
            .arg("+")
            .arg("COUNT")
            .arg(requested)
            .query_async(con)
            .await
            .map_err(|error| redis_error("Redis XRANGE failed", error))?;
    }
    Ok((stream_batch(&page), total))
}

/// `COUNT` hint for a scan that must skip `skip` entries and then read `want`.
fn chunk(skip: u64, want: u64) -> u64 {
    skip.saturating_add(want).clamp(10, MAX_CHUNK)
}

/// Inclusive `[start, stop]` rank range of `LRANGE`/`ZRANGE` for a page.
fn rank_range(offset: u64, limit: u64) -> (i64, i64) {
    let start = i64::try_from(offset).unwrap_or(i64::MAX);
    let stop = start.saturating_add(i64::try_from(limit).unwrap_or(i64::MAX) - 1);
    (start, stop)
}

/// Collects the `[offset, offset + limit)` window of a `*SCAN` iteration whose
/// first reply is `first`. `step` is 2 for `HSCAN` (field, value) and 1 for
/// `SSCAN`. Cursor order is the server's order; entries before `offset` are
/// fetched and dropped, so the cost grows with `offset`, never with the size of
/// the part after the window.
async fn scan_window(
    con: &mut MultiplexedConnection,
    command: &str,
    key: &[u8],
    offset: u64,
    limit: u64,
    first: (String, Entries),
    step: usize,
) -> Result<Entries, BackendError> {
    let (mut cursor, mut items) = first;
    let mut skip = offset;
    let mut out: Entries = Vec::new();
    loop {
        let entries = (items.len() / step) as u64;
        let skipped = skip.min(entries);
        skip -= skipped;
        let taken = (limit - (out.len() / step) as u64).min(entries - skipped);
        let from = skipped as usize * step;
        let to = from + taken as usize * step;
        out.extend(items.drain(from..to));
        if (out.len() / step) as u64 >= limit || cursor == "0" {
            return Ok(out);
        }
        let want = skip + (limit - (out.len() / step) as u64);
        (cursor, items) = redis::cmd(command)
            .arg(key)
            .arg(&cursor)
            .arg("COUNT")
            .arg(want.clamp(10, MAX_CHUNK))
            .query_async(con)
            .await
            .map_err(|error| redis_error(&format!("Redis {command} failed"), error))?;
    }
}

fn split_pairs(flat: Entries) -> (Entries, Entries) {
    let mut fields = Vec::with_capacity(flat.len() / 2);
    let mut values = Vec::with_capacity(flat.len() / 2);
    let mut iter = flat.into_iter();
    while let (Some(field), Some(value)) = (iter.next(), iter.next()) {
        fields.push(field);
        values.push(value);
    }
    (fields, values)
}

fn binary_array<T: AsRef<[u8]>>(values: &[T]) -> ArrayRef {
    let bytes = values.iter().map(|value| value.as_ref().len()).sum();
    let mut builder = BinaryBuilder::with_capacity(values.len(), bytes);
    for value in values {
        builder.append_value(value.as_ref());
    }
    Arc::new(builder.finish())
}

fn schema(fields: Vec<Field>) -> Arc<Schema> {
    Arc::new(Schema::new(fields))
}

/// A batch of Binary columns (all of equal length, non-null).
pub(super) fn bytes_batch<T: AsRef<[u8]>>(columns: &[(&str, &[T])]) -> RecordBatch {
    let fields = columns
        .iter()
        .map(|(name, _)| Field::new(*name, DataType::Binary, false))
        .collect();
    let arrays = columns
        .iter()
        .map(|(_, values)| binary_array(values))
        .collect();
    RecordBatch::try_new(schema(fields), arrays).expect("equal-length Binary columns")
}

fn list_batch(offset: u64, values: &[Vec<u8>]) -> RecordBatch {
    let indexes: ArrayRef = Arc::new(Int64Array::from_iter_values(
        (0..values.len() as u64).map(|row| i64::try_from(offset + row).unwrap_or(i64::MAX)),
    ));
    let fields = vec![
        Field::new("index", DataType::Int64, false),
        Field::new("value", DataType::Binary, false),
    ];
    RecordBatch::try_new(schema(fields), vec![indexes, binary_array(values)])
        .expect("equal-length list columns")
}

fn zset_batch(values: &[(Vec<u8>, f64)]) -> RecordBatch {
    let members: Vec<&[u8]> = values.iter().map(|(member, _)| member.as_slice()).collect();
    let scores: ArrayRef = Arc::new(Float64Array::from_iter_values(
        values.iter().map(|(_, score)| *score),
    ));
    let fields = vec![
        Field::new("member", DataType::Binary, false),
        Field::new("score", DataType::Float64, false),
    ];
    RecordBatch::try_new(schema(fields), vec![binary_array(&members), scores])
        .expect("equal-length zset columns")
}

fn stream_batch(entries: &[StreamEntry]) -> RecordBatch {
    let ids: ArrayRef = Arc::new(StringArray::from_iter_values(
        entries.iter().map(|(id, _)| redis_key_bytes_to_display(id)),
    ));
    let fields: ArrayRef = Arc::new(StringArray::from_iter_values(
        entries.iter().map(|(_, fields)| stream_fields_text(fields)),
    ));
    let schema = schema(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("fields", DataType::Utf8, false),
    ]);
    RecordBatch::try_new(schema, vec![ids, fields]).expect("equal-length stream columns")
}

/// `{"field":"value",..}` in display form; repeated field names are kept.
pub(super) fn stream_fields_text(fields: &[(Vec<u8>, Vec<u8>)]) -> String {
    let mut text = String::from("{");
    for (index, (field, value)) in fields.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        push_json_string(&mut text, &super::cells::display(field));
        text.push(':');
        push_json_string(&mut text, &super::cells::display(value));
    }
    text.push('}');
    text
}

fn push_json_string(out: &mut String, value: &str) {
    // Serializing a `&str` cannot fail.
    out.push_str(&serde_json::to_string(value).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::Float64Type;

    #[test]
    fn key_types_round_trip_and_reject_unknown() {
        for ty in [
            KeyType::String,
            KeyType::Hash,
            KeyType::List,
            KeyType::Set,
            KeyType::ZSet,
            KeyType::Stream,
        ] {
            assert_eq!(KeyType::parse(ty.as_str()), Some(ty));
        }
        assert_eq!(KeyType::parse("ReJSON-RL"), None);
        assert_eq!(KeyType::parse("none"), None);
    }

    #[test]
    fn rank_range_is_inclusive_and_saturates() {
        assert_eq!(rank_range(0, 50), (0, 49));
        assert_eq!(rank_range(100, 1), (100, 100));
        let (start, stop) = rank_range(u64::MAX, u64::MAX);
        assert_eq!((start, stop), (i64::MAX, i64::MAX));
    }

    #[test]
    fn chunk_is_bounded() {
        assert_eq!(chunk(0, 1), 10);
        assert_eq!(chunk(0, 500), 500);
        assert_eq!(chunk(1_000_000, 1_000_000), MAX_CHUNK);
    }

    #[test]
    fn binary_batches_keep_exact_bytes() {
        let batch = bytes_batch(&[("value", &[vec![0xff, 0x00, b'a'], vec![]])]);
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(
            batch.column(0).as_binary::<i32>().value(0),
            [0xff, 0x00, b'a']
        );
        assert_eq!(batch.column(0).as_binary::<i32>().value(1), b"");
        let empty = bytes_batch::<Vec<u8>>(&[("a", &[]), ("b", &[])]);
        assert_eq!((empty.num_rows(), empty.num_columns()), (0, 2));
    }

    #[test]
    fn typed_batches_have_documented_schemas() {
        let list = list_batch(40, &[b"x".to_vec(), b"y".to_vec()]);
        assert_eq!(list.schema().field(0).data_type(), &DataType::Int64);
        assert_eq!(
            list.column(0)
                .as_primitive::<arrow_array::types::Int64Type>()
                .value(1),
            41
        );
        let zset = zset_batch(&[(b"m".to_vec(), 1.5), (b"n".to_vec(), f64::INFINITY)]);
        assert_eq!(zset.schema().field(1).data_type(), &DataType::Float64);
        assert_eq!(
            zset.column(1).as_primitive::<Float64Type>().value(1),
            f64::INFINITY
        );
        let stream = stream_batch(&[(
            b"1-0".to_vec(),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"a".to_vec(), b"\xff".to_vec()),
            ],
        )]);
        assert_eq!(stream.column(0).as_string::<i32>().value(0), "1-0");
        assert_eq!(
            stream.column(1).as_string::<i32>().value(0),
            r#"{"a":"1","a":"\\xff"}"#
        );
    }

    #[test]
    fn split_pairs_drops_a_dangling_field() {
        let (fields, values) = split_pairs(vec![b"f".to_vec(), b"v".to_vec(), b"x".to_vec()]);
        assert_eq!((fields.len(), values.len()), (1, 1));
    }

    fn request(
        database: Option<&str>,
        object: Option<&str>,
        payload: serde_json::Value,
    ) -> QueryRequest {
        QueryRequest {
            kind: "redis.key".into(),
            operation_id: engine_driver_kit::api::OperationId(1),
            database: database.map(str::to_owned),
            object: object.map(str::to_owned),
            offset: 0,
            limit: 0,
            payload,
        }
    }

    #[test]
    fn target_prefers_raw_key_and_validates() {
        let raw = crate::redis_key_bytes_to_raw(b"\xac\xed");
        let target = Target::from_request(&request(
            Some("9"),
            Some("ignored"),
            serde_json::json!({"key_raw": raw, "max_bytes": 10}),
        ))
        .ok()
        .unwrap();
        assert_eq!(
            (target.db, target.key, target.max_string_bytes),
            (9, vec![0xac, 0xed], 10)
        );

        let plain = Target::from_request(&request(None, Some("user:1"), serde_json::Value::Null))
            .ok()
            .unwrap();
        assert_eq!((plain.db, plain.key), (0, b"user:1".to_vec()));
        assert_eq!(plain.max_string_bytes, DEFAULT_STRING_BYTES);

        let from_payload =
            Target::from_request(&request(None, Some("k"), serde_json::json!({"db": 4})))
                .ok()
                .unwrap();
        assert_eq!(from_payload.db, 4);

        for bad in [
            request(Some("x"), Some("k"), serde_json::Value::Null),
            request(Some("1"), None, serde_json::Value::Null),
            request(Some("1"), Some("k"), serde_json::json!({"key_raw": "!!"})),
        ] {
            let error = Target::from_request(&bad).err().unwrap();
            assert_eq!(error.code.as_deref(), Some("query.bad_request"));
        }
    }
}
