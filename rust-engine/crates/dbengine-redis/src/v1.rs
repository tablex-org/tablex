//! ABI 1 exports (JSON over C strings) kept for the Swift app. Compiled only
//! without the `abi-v2` feature.

#![allow(clippy::missing_safety_doc)]

use super::*;
use engine_ffi_util::{error_json, string_to_c_ptr, success_json};
use futures_util::StreamExt;
use std::ffi::{c_char, CStr};
use std::sync::{Arc, RwLock};

pub const ABI_VERSION: u32 = 1;
const VALUE_PREVIEW_LIMIT: usize = 200;
type RedisBytes = Vec<u8>;
type RedisHashEntry = (RedisBytes, RedisBytes);
type RedisStreamEntry = (RedisBytes, Vec<RedisHashEntry>);

lazy_static::lazy_static! {
    static ref ENGINE: RwLock<EngineState> = RwLock::new(EngineState::new());
}

struct EngineState {
    connections: HashMap<u64, Arc<RedisConn>>,
    next_conn_id: u64,
}

impl EngineState {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
            next_conn_id: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RedisKeyInfo {
    display: String,
    raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RedisScanResponse {
    cursor: String,
    keys: Vec<RedisKeyInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_keys: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RedisValueResponse {
    key: RedisKeyInfo,
    kind: String,
    ttl_seconds: Option<i64>,
    value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RedisCommandResponse {
    command: String,
    safety: RedisCommandSafety,
    value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RedisMutationResponse {
    ok: bool,
    changed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RedisPubSubChannel {
    name: String,
    subscribers: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RedisPubSubMessageResponse {
    channel: String,
    payload: Option<String>,
    payload_raw: Option<String>,
    timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RedisSlowLogEntry {
    id: i64,
    timestamp: i64,
    duration_micros: i64,
    command: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct RedisKeyMutationRequest {
    op: String,
    raw_key: String,
    field: Option<String>,
    value: Option<String>,
    index: Option<i64>,
    member: Option<String>,
    score: Option<f64>,
    entry_id: Option<String>,
    fields: Option<Vec<RedisStreamFieldInput>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct RedisStreamFieldInput {
    field: String,
    value: String,
}

impl RedisConn {
    async fn server_info(&self) -> Result<serde_json::Value, String> {
        let mut connection = self.connection.lock().await;
        let info: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut *connection)
            .await
            .map_err(|e| format!("Redis server info failed: {e}"))?;
        let version = info
            .lines()
            .find_map(|line| line.strip_prefix("redis_version:"))
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        Ok(serde_json::json!({
            "version": version,
            "git_version": "",
            "engine": "Redis",
            "host": ""
        }))
    }

    async fn scan_keys(
        &self,
        db: u32,
        cursor: &str,
        pattern: Option<&str>,
        count: u32,
    ) -> Result<RedisScanResponse, String> {
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;

        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor);
        if let Some(pattern) = pattern.filter(|value| !value.is_empty()) {
            cmd.arg("MATCH").arg(pattern);
        }
        cmd.arg("COUNT").arg(count.clamp(1, 1_000));

        let (next_cursor, keys): (String, Vec<Vec<u8>>) =
            cmd.query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis SCAN failed: {e}"))?;
        let total_keys = redis::cmd("DBSIZE")
            .query_async::<u64>(&mut *connection)
            .await
            .ok();
        let metadata = scan_key_metadata(&mut *connection, keys).await?;
        Ok(scan_response_with_metadata(
            next_cursor,
            metadata,
            total_keys,
        ))
    }

    async fn search_values(
        &self,
        db: u32,
        cursor: &str,
        query: &str,
        count: u32,
    ) -> Result<RedisScanResponse, String> {
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;

        let (next_cursor, keys): (String, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("COUNT")
            .arg(count.clamp(1, 1_000))
            .query_async(&mut *connection)
            .await
            .map_err(|e| format!("Redis SCAN failed: {e}"))?;
        let total_keys = redis::cmd("DBSIZE")
            .query_async::<u64>(&mut *connection)
            .await
            .ok();
        let mut metadata = Vec::new();
        for key in keys {
            let Ok(value) = redis_value_response_for_key(&mut *connection, &key).await else {
                continue;
            };
            if redis_value_matches_query(&value.value, query) {
                metadata.push(redis_search_metadata_from_value(value));
            }
        }
        Ok(scan_response_with_metadata(
            next_cursor,
            metadata,
            total_keys,
        ))
    }

    async fn get_value(&self, db: u32, raw_key: &str) -> Result<RedisValueResponse, String> {
        let key = redis_key_raw_to_bytes(raw_key)?;
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;

        redis_value_response_for_key(&mut *connection, &key).await
    }

    async fn execute_command(
        &self,
        db: u32,
        command: &str,
        confirmed: bool,
    ) -> Result<RedisCommandResponse, String> {
        let safety = classify_command(command);
        match safety {
            RedisCommandSafety::Blocked => return Err(format!("Redis command blocked: {command}")),
            RedisCommandSafety::Confirm if !confirmed => {
                return Err(format!("Redis command requires confirmation: {command}"))
            }
            _ => {}
        }

        let args = parse_redis_command(command)?;
        let Some((verb, rest)) = args.split_first() else {
            return Err("Redis command cannot be empty".to_string());
        };

        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;

        let mut cmd = redis::cmd(verb);
        for arg in rest {
            cmd.arg(arg);
        }
        let value: RedisRawValue = cmd
            .query_async(&mut *connection)
            .await
            .map_err(|e| format!("Redis command failed: {e}"))?;

        Ok(redis_command_response(
            command,
            safety,
            redis_raw_value_to_json(value),
        ))
    }

    async fn delete_key(&self, db: u32, raw_key: &str) -> Result<RedisMutationResponse, String> {
        let key = redis_key_raw_to_bytes(raw_key)?;
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;
        let changed: u64 = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut *connection)
            .await
            .map_err(|e| format!("Redis DEL failed: {e}"))?;
        Ok(RedisMutationResponse { ok: true, changed })
    }

    async fn delete_keys(
        &self,
        db: u32,
        raw_keys_json: &str,
    ) -> Result<RedisMutationResponse, String> {
        let keys = redis_raw_keys_json_to_bytes(raw_keys_json)?;
        if keys.is_empty() {
            return Ok(RedisMutationResponse {
                ok: true,
                changed: 0,
            });
        }
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;
        let changed: u64 = redis::cmd("DEL")
            .arg(keys)
            .query_async(&mut *connection)
            .await
            .map_err(|e| format!("Redis DEL failed: {e}"))?;
        Ok(RedisMutationResponse { ok: true, changed })
    }

    async fn set_ttl(
        &self,
        db: u32,
        raw_key: &str,
        ttl_seconds: i64,
    ) -> Result<RedisMutationResponse, String> {
        let key = redis_key_raw_to_bytes(raw_key)?;
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;
        let changed: bool = if ttl_seconds < 0 {
            redis::cmd("PERSIST")
                .arg(&key)
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis PERSIST failed: {e}"))?
        } else {
            redis::cmd("EXPIRE")
                .arg(&key)
                .arg(ttl_seconds)
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis EXPIRE failed: {e}"))?
        };
        Ok(RedisMutationResponse {
            ok: true,
            changed: u64::from(changed),
        })
    }

    async fn set_string(
        &self,
        db: u32,
        raw_key: &str,
        value: &str,
    ) -> Result<RedisMutationResponse, String> {
        let key = redis_key_raw_to_bytes(raw_key)?;
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;
        redis::cmd("SET")
            .arg(&key)
            .arg(value)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SET failed: {e}"))?;
        Ok(RedisMutationResponse {
            ok: true,
            changed: 1,
        })
    }

    async fn mutate_key(
        &self,
        db: u32,
        request: RedisKeyMutationRequest,
    ) -> Result<RedisMutationResponse, String> {
        let key = redis_key_raw_to_bytes(&request.raw_key)?;
        let mut connection = self.connection.lock().await;
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut *connection)
            .await
            .map_err(|e| format!("Redis SELECT failed: {e}"))?;
        let changed = match request.op.as_str() {
            "hset" => redis::cmd("HSET")
                .arg(&key)
                .arg(required(&request.field, "field")?)
                .arg(required(&request.value, "value")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis HSET failed: {e}"))?,
            "hdel" => redis::cmd("HDEL")
                .arg(&key)
                .arg(required(&request.field, "field")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis HDEL failed: {e}"))?,
            "lpush" => redis::cmd("LPUSH")
                .arg(&key)
                .arg(required(&request.value, "value")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis LPUSH failed: {e}"))?,
            "rpush" => redis::cmd("RPUSH")
                .arg(&key)
                .arg(required(&request.value, "value")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis RPUSH failed: {e}"))?,
            "lset" => {
                redis::cmd("LSET")
                    .arg(&key)
                    .arg(request.index.ok_or_else(|| "Missing index".to_string())?)
                    .arg(required(&request.value, "value")?)
                    .query_async::<()>(&mut *connection)
                    .await
                    .map_err(|e| format!("Redis LSET failed: {e}"))?;
                1
            }
            "sadd" => redis::cmd("SADD")
                .arg(&key)
                .arg(required(&request.member, "member")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis SADD failed: {e}"))?,
            "srem" => redis::cmd("SREM")
                .arg(&key)
                .arg(required(&request.member, "member")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis SREM failed: {e}"))?,
            "zadd" => redis::cmd("ZADD")
                .arg(&key)
                .arg(request.score.ok_or_else(|| "Missing score".to_string())?)
                .arg(required(&request.member, "member")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis ZADD failed: {e}"))?,
            "zrem" => redis::cmd("ZREM")
                .arg(&key)
                .arg(required(&request.member, "member")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis ZREM failed: {e}"))?,
            "xadd" => {
                let fields = request
                    .fields
                    .as_deref()
                    .filter(|fields| !fields.is_empty())
                    .ok_or_else(|| "Missing stream fields".to_string())?;
                let mut cmd = redis::cmd("XADD");
                cmd.arg(&key)
                    .arg(request.entry_id.as_deref().unwrap_or("*"));
                for pair in fields {
                    cmd.arg(&pair.field).arg(&pair.value);
                }
                let _: RedisRawValue = cmd
                    .query_async(&mut *connection)
                    .await
                    .map_err(|e| format!("Redis XADD failed: {e}"))?;
                1
            }
            "xdel" => redis::cmd("XDEL")
                .arg(&key)
                .arg(required(&request.entry_id, "entry_id")?)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|e| format!("Redis XDEL failed: {e}"))?,
            other => return Err(format!("Unsupported Redis mutation op: {other}")),
        };
        Ok(RedisMutationResponse { ok: true, changed })
    }

    async fn pubsub_channels(
        &self,
        pattern: Option<&str>,
    ) -> Result<Vec<RedisPubSubChannel>, String> {
        let mut connection = self.connection.lock().await;
        let mut cmd = redis::cmd("PUBSUB");
        cmd.arg("CHANNELS");
        if let Some(pattern) = pattern.filter(|value| !value.is_empty()) {
            cmd.arg(pattern);
        }
        let channels: Vec<Vec<u8>> = cmd
            .query_async(&mut *connection)
            .await
            .map_err(|e| format!("Redis PUBSUB CHANNELS failed: {e}"))?;
        let subscriber_counts = if channels.is_empty() {
            HashMap::new()
        } else {
            let mut numsub = redis::cmd("PUBSUB");
            numsub.arg("NUMSUB");
            for channel in &channels {
                numsub.arg(channel);
            }
            let value: RedisRawValue = numsub
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis PUBSUB NUMSUB failed: {e}"))?;
            parse_pubsub_numsub(value)
        };
        Ok(channels
            .into_iter()
            .map(|channel| RedisPubSubChannel {
                name: redis_key_bytes_to_display(&channel),
                subscribers: subscriber_counts
                    .get(&redis_key_bytes_to_display(&channel))
                    .copied()
                    .unwrap_or(0),
            })
            .collect())
    }

    async fn slowlog(&self, count: u32) -> Result<Vec<RedisSlowLogEntry>, String> {
        let mut connection = self.connection.lock().await;
        let value: RedisRawValue = redis::cmd("SLOWLOG")
            .arg("GET")
            .arg(count.clamp(1, 128))
            .query_async(&mut *connection)
            .await
            .map_err(|e| format!("Redis SLOWLOG GET failed: {e}"))?;
        Ok(parse_slowlog(value))
    }

    async fn listen_once(
        &self,
        channel: &str,
        timeout_ms: u64,
    ) -> Result<RedisPubSubMessageResponse, String> {
        if channel.trim().is_empty() {
            return Err("Redis Pub/Sub channel cannot be empty".to_string());
        }
        let client = redis::Client::open(self.url.as_str())
            .map_err(|e| format!("Redis Pub/Sub connection failed: {e}"))?;
        let mut pubsub = client
            .get_async_pubsub()
            .await
            .map_err(|e| format!("Redis Pub/Sub connection failed: {e}"))?;
        pubsub
            .subscribe(channel)
            .await
            .map_err(|e| format!("Redis SUBSCRIBE failed: {e}"))?;
        let mut stream = pubsub.on_message();
        let timeout = Duration::from_millis(timeout_ms.clamp(100, 30_000));
        match tokio::time::timeout(timeout, stream.next()).await {
            Ok(Some(message)) => {
                let payload = message.get_payload_bytes().to_vec();
                let channel_name = message.get_channel_name().to_string();
                Ok(redis_pubsub_message_response(
                    &channel_name,
                    Some(payload),
                    false,
                ))
            }
            Ok(None) | Err(_) => Ok(redis_pubsub_message_response(channel, None, true)),
        }
    }
}

#[cfg(test)]
fn scan_response(cursor: String, keys: Vec<Vec<u8>>) -> RedisScanResponse {
    RedisScanResponse {
        cursor,
        keys: keys
            .into_iter()
            .map(|key| RedisKeyInfo {
                display: redis_key_bytes_to_display(&key),
                raw: redis_key_bytes_to_raw(&key),
                key_type: None,
                ttl: None,
                size: None,
                value_preview: None,
            })
            .collect(),
        total_keys: None,
    }
}

fn scan_response_with_metadata(
    cursor: String,
    metadata: Vec<RedisKeyMetadata>,
    total_keys: Option<u64>,
) -> RedisScanResponse {
    RedisScanResponse {
        cursor,
        keys: metadata
            .into_iter()
            .map(|item| RedisKeyInfo {
                display: redis_key_bytes_to_display(&item.key),
                raw: redis_key_bytes_to_raw(&item.key),
                key_type: Some(item.key_type),
                ttl: item.ttl,
                size: item.size,
                value_preview: item.value_preview,
            })
            .collect(),
        total_keys,
    }
}

fn redis_value_response(
    key: &[u8],
    kind: &str,
    ttl_seconds: Option<i64>,
    value: serde_json::Value,
) -> RedisValueResponse {
    RedisValueResponse {
        key: RedisKeyInfo {
            display: redis_key_bytes_to_display(key),
            raw: redis_key_bytes_to_raw(key),
            key_type: Some(kind.to_string()),
            ttl: ttl_seconds,
            size: None,
            value_preview: None,
        },
        kind: kind.to_string(),
        ttl_seconds,
        value,
    }
}

async fn redis_value_response_for_key<C>(
    connection: &mut C,
    key: &[u8],
) -> Result<RedisValueResponse, String>
where
    C: ConnectionLike + Send + Sync + Unpin,
{
    let kind: String = redis::cmd("TYPE")
        .arg(key)
        .query_async(&mut *connection)
        .await
        .map_err(|e| format!("Redis TYPE failed: {e}"))?;
    let ttl: i64 = redis::cmd("TTL")
        .arg(key)
        .query_async(&mut *connection)
        .await
        .unwrap_or(-2);
    let ttl_seconds = (ttl >= 0).then_some(ttl);

    let value = match kind.as_str() {
        "string" => {
            let bytes: Vec<u8> = redis::cmd("GET")
                .arg(key)
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis GET failed: {e}"))?;
            serde_json::json!({
                "text": redis_key_bytes_to_display(&bytes),
                "raw": redis_key_bytes_to_raw(&bytes)
            })
        }
        "hash" => {
            let (_cursor, entries): (String, Vec<RedisHashEntry>) = redis::cmd("HSCAN")
                .arg(key)
                .arg("0")
                .arg("COUNT")
                .arg(VALUE_PREVIEW_LIMIT)
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis HSCAN failed: {e}"))?;
            serde_json::json!(entries
                .into_iter()
                .map(|(field, value)| serde_json::json!({
                    "field": redis_key_bytes_to_display(&field),
                    "field_raw": redis_key_bytes_to_raw(&field),
                    "value": redis_key_bytes_to_display(&value),
                    "value_raw": redis_key_bytes_to_raw(&value)
                }))
                .collect::<Vec<_>>())
        }
        "list" => {
            let values: Vec<Vec<u8>> = redis::cmd("LRANGE")
                .arg(key)
                .arg(0)
                .arg(199)
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis LRANGE failed: {e}"))?;
            serde_json::json!(values
                .into_iter()
                .map(|value| serde_json::json!({
                    "text": redis_key_bytes_to_display(&value),
                    "raw": redis_key_bytes_to_raw(&value)
                }))
                .collect::<Vec<_>>())
        }
        "set" => {
            let (_cursor, values): (String, Vec<Vec<u8>>) = redis::cmd("SSCAN")
                .arg(key)
                .arg("0")
                .arg("COUNT")
                .arg(VALUE_PREVIEW_LIMIT)
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis SSCAN failed: {e}"))?;
            redis_member_values_json(values)
        }
        "zset" => {
            let values: Vec<(Vec<u8>, f64)> = redis::cmd("ZRANGE")
                .arg(key)
                .arg(0)
                .arg(199)
                .arg("WITHSCORES")
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis ZRANGE failed: {e}"))?;
            redis_zset_values_json(values)
        }
        "stream" => {
            let values: Vec<RedisStreamEntry> = redis::cmd("XRANGE")
                .arg(key)
                .arg("-")
                .arg("+")
                .arg("COUNT")
                .arg(200)
                .query_async(&mut *connection)
                .await
                .map_err(|e| format!("Redis XRANGE failed: {e}"))?;
            redis_stream_values_json(values)
        }
        _ => serde_json::json!(null),
    };

    Ok(redis_value_response(key, &kind, ttl_seconds, value))
}

fn redis_value_matches_query(value: &serde_json::Value, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return false;
    }
    redis_search_value_text(value)
        .to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

fn redis_search_value_text(value: &serde_json::Value) -> String {
    fn append_display_text(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::String(text) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    append_display_text(value, out);
                }
            }
            serde_json::Value::Object(object) => {
                for key in ["text", "field", "value", "member", "id", "score"] {
                    if let Some(value) = object.get(key) {
                        append_display_text(value, out);
                    }
                }
                if let Some(fields) = object.get("fields") {
                    append_display_text(fields, out);
                }
            }
            other => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&other.to_string());
            }
        }
    }

    match value {
        serde_json::Value::String(text) => text.clone(),
        other => {
            let mut text = String::new();
            append_display_text(other, &mut text);
            text
        }
    }
}

fn redis_search_value_preview(value: &serde_json::Value) -> String {
    const MAX_PREVIEW_LEN: usize = 160;
    let text = redis_search_value_text(value);
    if text.chars().count() <= MAX_PREVIEW_LEN {
        return text;
    }
    let mut preview = text.chars().take(MAX_PREVIEW_LEN).collect::<String>();
    preview.push('…');
    preview
}

fn redis_search_value_size(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(text) => Some(text.len() as u64),
        serde_json::Value::Array(values) => Some(values.len() as u64),
        _ => None,
    }
}

fn redis_search_metadata_from_value(value: RedisValueResponse) -> RedisKeyMetadata {
    RedisKeyMetadata {
        key: redis_key_raw_to_bytes(&value.key.raw).unwrap_or_default(),
        key_type: value.kind,
        ttl: value.ttl_seconds,
        size: redis_search_value_size(&value.value),
        value_preview: Some(redis_search_value_preview(&value.value)),
    }
}

fn redis_command_response(
    command: &str,
    safety: RedisCommandSafety,
    value: serde_json::Value,
) -> RedisCommandResponse {
    RedisCommandResponse {
        command: command.to_string(),
        safety,
        value,
    }
}

fn redis_member_values_json(values: Vec<Vec<u8>>) -> serde_json::Value {
    serde_json::json!(values
        .into_iter()
        .map(|value| serde_json::json!({
            "text": redis_key_bytes_to_display(&value),
            "raw": redis_key_bytes_to_raw(&value)
        }))
        .collect::<Vec<_>>())
}

fn redis_zset_values_json(values: Vec<(Vec<u8>, f64)>) -> serde_json::Value {
    serde_json::json!(values
        .into_iter()
        .map(|(member, score)| serde_json::json!({
            "member": redis_key_bytes_to_display(&member),
            "member_raw": redis_key_bytes_to_raw(&member),
            "score": score
        }))
        .collect::<Vec<_>>())
}

fn redis_stream_values_json(values: Vec<RedisStreamEntry>) -> serde_json::Value {
    serde_json::json!(values
        .into_iter()
        .map(|(id, fields)| serde_json::json!({
            "id": redis_key_bytes_to_display(&id),
            "fields": fields.into_iter().map(|(field, value)| serde_json::json!({
                "field": redis_key_bytes_to_display(&field),
                "field_raw": redis_key_bytes_to_raw(&field),
                "value": redis_key_bytes_to_display(&value),
                "value_raw": redis_key_bytes_to_raw(&value)
            })).collect::<Vec<_>>()
        }))
        .collect::<Vec<_>>())
}

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, String> {
    value
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Missing {name}"))
}

fn redis_raw_value_to_json(value: RedisRawValue) -> serde_json::Value {
    match value {
        RedisRawValue::Nil => serde_json::Value::Null,
        RedisRawValue::Int(value) => serde_json::json!(value),
        RedisRawValue::BulkString(bytes) => serde_json::json!({
            "text": redis_key_bytes_to_display(&bytes),
            "raw": redis_key_bytes_to_raw(&bytes)
        }),
        RedisRawValue::SimpleString(value) => serde_json::json!(value),
        RedisRawValue::Okay => serde_json::json!("OK"),
        RedisRawValue::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(redis_raw_value_to_json).collect())
        }
        RedisRawValue::Map(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(|(key, value)| {
                    serde_json::json!({
                        "key": redis_raw_value_to_json(key),
                        "value": redis_raw_value_to_json(value)
                    })
                })
                .collect(),
        ),
        RedisRawValue::Set(values) => {
            serde_json::Value::Array(values.into_iter().map(redis_raw_value_to_json).collect())
        }
        RedisRawValue::Attribute { data, attributes } => serde_json::json!({
            "data": redis_raw_value_to_json(*data),
            "attributes": redis_raw_value_to_json(RedisRawValue::Map(attributes))
        }),
        RedisRawValue::Double(value) => serde_json::json!(value),
        RedisRawValue::Boolean(value) => serde_json::json!(value),
        RedisRawValue::VerbatimString { format, text } => {
            serde_json::json!({"format": format!("{format:?}"), "text": text})
        }
        RedisRawValue::BigNumber(value) => serde_json::json!(format!("{value:?}")),
        RedisRawValue::Push { kind, data } => {
            serde_json::json!({"kind": format!("{kind:?}"), "data": data.into_iter().map(redis_raw_value_to_json).collect::<Vec<_>>() })
        }
        RedisRawValue::ServerError(error) => serde_json::json!({"error": format!("{error:?}")}),
    }
}

fn redis_pubsub_message_response(
    channel: &str,
    payload: Option<Vec<u8>>,
    timed_out: bool,
) -> RedisPubSubMessageResponse {
    RedisPubSubMessageResponse {
        channel: channel.to_string(),
        payload: payload.as_deref().map(redis_key_bytes_to_display),
        payload_raw: payload.as_deref().map(redis_key_bytes_to_raw),
        timed_out,
    }
}

fn parse_pubsub_numsub(value: RedisRawValue) -> HashMap<String, u64> {
    let RedisRawValue::Array(values) = value else {
        return HashMap::new();
    };
    let mut counts = HashMap::new();
    for pair in values.chunks(2) {
        if pair.len() != 2 {
            continue;
        }
        let name = Vec::<u8>::from_redis_value(&pair[0])
            .map(|bytes| redis_key_bytes_to_display(&bytes))
            .unwrap_or_default();
        let count = u64::from_redis_value(&pair[1]).unwrap_or(0);
        if !name.is_empty() {
            counts.insert(name, count);
        }
    }
    counts
}

fn parse_slowlog(value: RedisRawValue) -> Vec<RedisSlowLogEntry> {
    let RedisRawValue::Array(entries) = value else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let RedisRawValue::Array(fields) = entry else {
                return None;
            };
            let id = fields
                .first()
                .and_then(|value| i64::from_redis_value(value).ok())?;
            let timestamp = fields
                .get(1)
                .and_then(|value| i64::from_redis_value(value).ok())?;
            let duration_micros = fields
                .get(2)
                .and_then(|value| i64::from_redis_value(value).ok())?;
            let command = fields.get(3).map(slowlog_command_text).unwrap_or_default();
            Some(RedisSlowLogEntry {
                id,
                timestamp,
                duration_micros,
                command,
            })
        })
        .collect()
}

fn slowlog_command_text(value: &RedisRawValue) -> String {
    let RedisRawValue::Array(parts) = value else {
        return String::from_redis_value(value).unwrap_or_default();
    };
    parts
        .iter()
        .filter_map(|part| Vec::<u8>::from_redis_value(part).ok())
        .map(|bytes| redis_key_bytes_to_display(&bytes))
        .collect::<Vec<_>>()
        .join(" ")
}

fn redis_raw_keys_json_to_bytes(value: &str) -> Result<Vec<Vec<u8>>, String> {
    let raw_keys: Vec<String> =
        serde_json::from_str(value).map_err(|e| format!("Invalid Redis key list: {e}"))?;
    raw_keys
        .iter()
        .map(|raw| redis_key_raw_to_bytes(raw))
        .collect()
}

fn capabilities_json() -> String {
    serde_json::json!({
        "abi_version": ABI_VERSION,
        "driver": "redis",
        "display_name": "Redis",
        "uri_schemes": ["redis", "rediss"],
        "features": ["redis_connect", "redis_databases", "redis_keys", "redis_values", "redis_commands"],
        "dependencies": [
            {"id":"redis_protocol","display_name":"Redis Protocol","kind":"bundled","state":"installed","required":true,"features":["redis_connect", "redis_keys"]}
        ]
    })
    .to_string()
}

#[no_mangle]
pub extern "C" fn dbengine_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
pub extern "C" fn dbengine_capabilities() -> *mut c_char {
    string_to_c_ptr(capabilities_json())
}

#[no_mangle]
pub extern "C" fn dbengine_init() -> *mut c_char {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    success_json(&serde_json::json!({ "ok": true }))
}

#[no_mangle]
pub extern "C" fn dbengine_shutdown() -> *mut c_char {
    success_json(&serde_json::json!({ "ok": true }))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_connect(uri: *const c_char, _name: *const c_char) -> *mut c_char {
    let uri = match cstr(uri) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    match RUNTIME.block_on(RedisConn::connect(&uri)) {
        Ok(conn) => {
            let mut engine = match ENGINE.write() {
                Ok(engine) => engine,
                Err(_) => return error_json("engine lock poisoned"),
            };
            let id = engine.next_conn_id;
            engine.next_conn_id += 1;
            engine.connections.insert(id, Arc::new(conn));
            success_json(&serde_json::json!({ "id": id }))
        }
        Err(error) => error_json(&error),
    }
}

#[no_mangle]
pub extern "C" fn dbengine_disconnect(conn_id: u64) -> *mut c_char {
    match ENGINE.write() {
        Ok(mut engine) => match engine.connections.remove(&conn_id) {
            Some(_) => success_json(&serde_json::json!({ "ok": true })),
            None => error_json(&format!("Connection {conn_id} not found")),
        },
        Err(_) => error_json("engine lock poisoned"),
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_databases(conn_id: u64) -> *mut c_char {
    with_conn(conn_id, |conn| {
        let infos = RUNTIME.block_on(conn.list_database_infos())?;
        Ok(infos
            .into_iter()
            .map(|info| info.db.to_string())
            .collect::<Vec<_>>())
    })
}

#[no_mangle]
pub extern "C" fn dbengine_redis_list_databases(conn_id: u64) -> *mut c_char {
    with_conn(conn_id, |conn| RUNTIME.block_on(conn.list_database_infos()))
}

#[no_mangle]
pub extern "C" fn dbengine_server_info(conn_id: u64) -> *mut c_char {
    with_conn(conn_id, |conn| RUNTIME.block_on(conn.server_info()))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_scan_keys(
    conn_id: u64,
    db: u32,
    cursor: *const c_char,
    pattern: *const c_char,
    count: u32,
) -> *mut c_char {
    let cursor = match cstr(cursor) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let pattern = optional_cstr(pattern);
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.scan_keys(db, &cursor, pattern.as_deref(), count))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_search_values(
    conn_id: u64,
    db: u32,
    cursor: *const c_char,
    query: *const c_char,
    count: u32,
) -> *mut c_char {
    let cursor = match cstr(cursor) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let query = match cstr(query) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.search_values(db, &cursor, &query, count))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_get_value(
    conn_id: u64,
    db: u32,
    raw_key: *const c_char,
) -> *mut c_char {
    let raw_key = match cstr(raw_key) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.get_value(db, &raw_key))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_execute_command(
    conn_id: u64,
    db: u32,
    command: *const c_char,
    confirmed: bool,
) -> *mut c_char {
    let command = match cstr(command) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.execute_command(db, &command, confirmed))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_delete_key(
    conn_id: u64,
    db: u32,
    raw_key: *const c_char,
) -> *mut c_char {
    let raw_key = match cstr(raw_key) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.delete_key(db, &raw_key))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_delete_keys(
    conn_id: u64,
    db: u32,
    raw_keys_json: *const c_char,
) -> *mut c_char {
    let raw_keys_json = match cstr(raw_keys_json) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.delete_keys(db, &raw_keys_json))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_set_ttl(
    conn_id: u64,
    db: u32,
    raw_key: *const c_char,
    ttl_seconds: i64,
) -> *mut c_char {
    let raw_key = match cstr(raw_key) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.set_ttl(db, &raw_key, ttl_seconds))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_set_string(
    conn_id: u64,
    db: u32,
    raw_key: *const c_char,
    value: *const c_char,
) -> *mut c_char {
    let raw_key = match cstr(raw_key) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let value = match cstr(value) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.set_string(db, &raw_key, &value))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_mutate_key(
    conn_id: u64,
    db: u32,
    request_json: *const c_char,
) -> *mut c_char {
    let request_json = match cstr(request_json) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let request = match serde_json::from_str::<RedisKeyMutationRequest>(&request_json) {
        Ok(value) => value,
        Err(error) => return error_json(&format!("Invalid Redis mutation request: {error}")),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.mutate_key(db, request))
    })
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_pubsub_channels(
    conn_id: u64,
    pattern: *const c_char,
) -> *mut c_char {
    let pattern = optional_cstr(pattern);
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.pubsub_channels(pattern.as_deref()))
    })
}

#[no_mangle]
pub extern "C" fn dbengine_redis_slowlog(conn_id: u64, count: u32) -> *mut c_char {
    with_conn(conn_id, |conn| RUNTIME.block_on(conn.slowlog(count)))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_redis_pubsub_listen_once(
    conn_id: u64,
    channel: *const c_char,
    timeout_ms: u64,
) -> *mut c_char {
    let channel = match cstr(channel) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| {
        RUNTIME.block_on(conn.listen_once(&channel, timeout_ms))
    })
}

fn with_conn<T: serde::Serialize>(
    conn_id: u64,
    f: impl FnOnce(Arc<RedisConn>) -> Result<T, String>,
) -> *mut c_char {
    let conn = match get_conn(conn_id) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    match f(conn) {
        Ok(value) => success_json(&value),
        Err(error) => error_json(&error),
    }
}

fn get_conn(conn_id: u64) -> Result<Arc<RedisConn>, String> {
    ENGINE
        .read()
        .map_err(|_| "engine lock poisoned".to_string())?
        .connections
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| format!("Connection {conn_id} not found"))
}

unsafe fn cstr(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("null string pointer".to_string());
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|value| value.to_string())
        .map_err(|e| e.to_string())
}

fn optional_cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(value: &str) -> RedisRawValue {
        RedisRawValue::BulkString(value.as_bytes().to_vec())
    }

    #[test]
    fn capabilities_json_describes_redis_driver() {
        let value: serde_json::Value = serde_json::from_str(&capabilities_json()).unwrap();

        assert_eq!(value["driver"], "redis");
        assert!(value["uri_schemes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("redis")));
        assert!(value["uri_schemes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("rediss")));
    }

    #[test]
    fn decodes_batch_delete_raw_key_list() {
        let raw_keys = format!(
            r#"["{}","{}"]"#,
            redis_key_bytes_to_raw(b"user:1"),
            redis_key_bytes_to_raw(b"session:2")
        );

        assert_eq!(
            redis_raw_keys_json_to_bytes(&raw_keys).unwrap(),
            vec![b"user:1".to_vec(), b"session:2".to_vec()]
        );
        assert!(redis_raw_keys_json_to_bytes(r#"["not-base64"]"#).is_err());
    }

    #[test]
    fn serializes_scan_key_metadata_like_desktop_client() {
        let response = scan_response_with_metadata(
            "0".to_string(),
            vec![RedisKeyMetadata {
                key: b"user:1".to_vec(),
                key_type: "hash".to_string(),
                ttl: Some(120),
                size: Some(8),
                value_preview: Some("name=Ana".to_string()),
            }],
            Some(1827),
        );

        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["total_keys"], 1827);
        assert_eq!(value["keys"][0]["display"], "user:1");
        assert_eq!(value["keys"][0]["raw"], "dXNlcjox");
        assert_eq!(value["keys"][0]["key_type"], "hash");
        assert_eq!(value["keys"][0]["ttl"], 120);
        assert_eq!(value["keys"][0]["size"], 8);
        assert_eq!(value["keys"][0]["value_preview"], "name=Ana");
    }

    #[test]
    fn builds_command_result_json_with_safety() {
        let result = redis_command_response(
            "GET session:1",
            RedisCommandSafety::Allowed,
            serde_json::json!({"text":"hello","raw":"aGVsbG8="}),
        );

        let value = serde_json::to_value(result).unwrap();

        assert_eq!(value["command"], "GET session:1");
        assert_eq!(value["safety"], "allowed");
        assert_eq!(value["value"]["text"], "hello");
    }

    #[test]
    fn builds_pubsub_channel_json() {
        let channel = RedisPubSubChannel {
            name: "events:user".to_string(),
            subscribers: 2,
        };

        let value = serde_json::to_value(channel).unwrap();

        assert_eq!(value["name"], "events:user");
        assert_eq!(value["subscribers"], 2);
    }

    #[test]
    fn builds_slowlog_entry_json() {
        let entry = RedisSlowLogEntry {
            id: 7,
            timestamp: 1_700_000_000,
            duration_micros: 42,
            command: "GET session:1".to_string(),
        };

        let value = serde_json::to_value(entry).unwrap();

        assert_eq!(value["id"], 7);
        assert_eq!(value["duration_micros"], 42);
        assert_eq!(value["command"], "GET session:1");
    }

    #[test]
    fn matches_value_json_case_insensitively() {
        assert!(redis_value_matches_query(
            &serde_json::json!({"field": "Ada Lovelace"}),
            "lovelace"
        ));
        assert!(redis_value_matches_query(
            &serde_json::json!("Hello Redis"),
            "redis"
        ));
        assert!(!redis_value_matches_query(
            &serde_json::json!("Hello Redis"),
            "mysql"
        ));
        assert!(!redis_value_matches_query(
            &serde_json::json!("Hello Redis"),
            ""
        ));
    }

    #[test]
    fn value_search_ignores_transport_fields_and_json_keys() {
        let value = serde_json::json!({
            "text": "Hello Redis",
            "raw": "aGVsbG8="
        });

        assert!(redis_value_matches_query(&value, "redis"));
        assert!(!redis_value_matches_query(&value, "raw"));
        assert!(!redis_value_matches_query(&value, "aGVsbG8"));
        assert!(!redis_value_matches_query(&value, "text"));
    }

    #[test]
    fn value_search_matches_nested_display_fields_only() {
        let value = serde_json::json!([{
            "field": "name",
            "field_raw": "bmFtZQ==",
            "value": "Ada Lovelace",
            "value_raw": "QWRh"
        }]);

        assert!(redis_value_matches_query(&value, "ada"));
        assert!(redis_value_matches_query(&value, "name"));
        assert!(!redis_value_matches_query(&value, "field_raw"));
        assert!(!redis_value_matches_query(&value, "bmFtZQ"));
        assert!(!redis_value_matches_query(&value, "value_raw"));
    }

    #[test]
    fn builds_value_search_metadata_from_value_response() {
        let value = redis_value_response(
            b"user:1",
            "hash",
            Some(30),
            serde_json::json!([{"field":"name","value":"Ana"}]),
        );
        let metadata = redis_search_metadata_from_value(value);

        assert_eq!(metadata.key, b"user:1".to_vec());
        assert_eq!(metadata.key_type, "hash");
        assert_eq!(metadata.ttl, Some(30));
        assert_eq!(metadata.value_preview, Some("name\nAna".to_string()));
    }

    #[test]
    fn parses_pubsub_numsub_response() {
        let value = RedisRawValue::Array(vec![bulk("events:user"), RedisRawValue::Int(3)]);

        assert_eq!(parse_pubsub_numsub(value).get("events:user"), Some(&3));
    }

    #[test]
    fn builds_pubsub_message_response_json() {
        let response = redis_pubsub_message_response("events", Some(b"hello".to_vec()), false);

        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["channel"], "events");
        assert_eq!(value["payload"], "hello");
        assert_eq!(value["payload_raw"], "aGVsbG8=");
        assert_eq!(value["timed_out"], false);
    }

    #[test]
    fn maps_scan_keys_to_display_and_raw_key_info() {
        let keys = vec![b"user:1".to_vec(), b"\xAC\xED\x00\x05t\x00token".to_vec()];

        let response = scan_response("17".to_string(), keys);

        assert_eq!(response.cursor, "17");
        assert_eq!(response.keys[0].display, "user:1");
        assert_eq!(response.keys[0].raw, "dXNlcjox");
        assert_eq!(response.keys[1].display, "\\xac\\xed\\x00\\x05t\\x00token");
        assert_eq!(
            redis_key_raw_to_bytes(&response.keys[1].raw).unwrap(),
            b"\xAC\xED\x00\x05t\x00token"
        );
    }

    #[test]
    fn builds_string_value_response_with_display_and_raw_value() {
        let key = b"session:1";
        let value = b"hello";

        let response = redis_value_response(
            key,
            "string",
            Some(120),
            serde_json::json!({
                "text": redis_key_bytes_to_display(value),
                "raw": redis_key_bytes_to_raw(value)
            }),
        );

        assert_eq!(response.key.display, "session:1");
        assert_eq!(response.key.raw, "c2Vzc2lvbjox");
        assert_eq!(response.kind, "string");
        assert_eq!(response.ttl_seconds, Some(120));
        assert_eq!(response.value["text"], "hello");
        assert_eq!(response.value["raw"], "aGVsbG8=");
    }

    #[test]
    fn builds_set_value_json() {
        let value = redis_member_values_json(vec![b"a".to_vec(), b"b".to_vec()]);

        assert_eq!(value[0]["text"], "a");
        assert_eq!(value[1]["raw"], "Yg==");
    }

    #[test]
    fn builds_zset_value_json_with_scores() {
        let value = redis_zset_values_json(vec![(b"a".to_vec(), 1.5)]);

        assert_eq!(value[0]["member"], "a");
        assert_eq!(value[0]["member_raw"], "YQ==");
        assert_eq!(value[0]["score"], 1.5);
    }

    #[test]
    fn builds_stream_value_json() {
        let value = redis_stream_values_json(vec![(
            b"1-0".to_vec(),
            vec![(b"event".to_vec(), b"login".to_vec())],
        )]);

        assert_eq!(value[0]["id"], "1-0");
        assert_eq!(value[0]["fields"][0]["field"], "event");
        assert_eq!(value[0]["fields"][0]["value"], "login");
    }

    #[test]
    fn decodes_binary_safe_mutation_request() {
        let request: RedisKeyMutationRequest = serde_json::from_str(
            r#"{
            "op":"hset",
            "raw_key":"dXNlcjox",
            "field":"name",
            "value":"Ana"
        }"#,
        )
        .unwrap();

        assert_eq!(request.op, "hset");
        assert_eq!(request.raw_key, "dXNlcjox");
        assert_eq!(request.field.as_deref(), Some("name"));
        assert_eq!(request.value.as_deref(), Some("Ana"));
    }
}
