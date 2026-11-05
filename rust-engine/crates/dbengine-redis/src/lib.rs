//! Redis driver.
//!
//! * Default build: ABI 1 JSON exports for the Swift app (`v1`).
//! * `--features abi-v2`: ABI 2 driver on `engine-driver-kit` (`v2`), with Redis'
//!   data model mapped onto navigator nodes and Arrow pages.
//!
//! This file holds what both ABIs share: the connection, key/value byte
//! helpers, command parsing and safety classification, database/keyspace
//! listing and key metadata.

use base64::Engine;
use redis::{aio::ConnectionLike, FromRedisValue, Value as RedisRawValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::Mutex;

#[cfg(not(feature = "abi-v2"))]
mod v1;
#[cfg(not(feature = "abi-v2"))]
pub use v1::ABI_VERSION;

#[cfg(feature = "abi-v2")]
mod v2;
#[cfg(feature = "abi-v2")]
pub use engine_driver_kit::api::ABI_VERSION;

#[cfg(feature = "abi-v2")]
engine_driver_kit::export_driver!(v2::RedisBackend; mutate);

const CONNECTION_TIMEOUT_SECS: u64 = 30;
const DEFAULT_REDIS_DATABASES: u32 = 16;

lazy_static::lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .thread_name("dbengine-redis-worker")
        .build()
        .expect("Failed to create Redis driver tokio runtime");
}

struct RedisConn {
    pub(crate) connection: Mutex<redis::aio::MultiplexedConnection>,
    /// Only the ABI 1 pub/sub listener opens a second connection.
    #[cfg(not(feature = "abi-v2"))]
    pub(crate) url: String,
    /// Major version of the server (`INFO server`), `0` when it did not say.
    #[cfg(feature = "abi-v2")]
    pub(crate) server_major: u32,
}

/// `redis_version:7.0.5` -> `7`.
#[cfg(feature = "abi-v2")]
fn parse_server_major(info: &str) -> u32 {
    info.lines()
        .find_map(|line| line.strip_prefix("redis_version:"))
        .and_then(|version| version.trim().split('.').next()?.parse().ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RedisDatabaseInfo {
    db: u32,
    keys: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RedisKeyMetadata {
    key: Vec<u8>,
    key_type: String,
    ttl: Option<i64>,
    size: Option<u64>,
    value_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisCommandSafety {
    Allowed,
    Confirm,
    Blocked,
}

impl RedisConn {
    async fn connect(url: &str) -> Result<Self, String> {
        let lower = url.trim().to_ascii_lowercase();
        if !lower.starts_with("redis://") && !lower.starts_with("rediss://") {
            return Err("Redis URI must start with redis:// or rediss://".to_string());
        }

        let client =
            redis::Client::open(url).map_err(|e| format!("Redis connection failed: {e}"))?;
        let mut connection = tokio::time::timeout(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            client.get_multiplexed_async_connection(),
        )
        .await
        .map_err(|_| format!("Redis connection timed out ({CONNECTION_TIMEOUT_SECS}s)"))?
        .map_err(|e| format!("Redis connection failed: {e}"))?;

        tokio::time::timeout(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            redis::cmd("PING").query_async::<String>(&mut connection),
        )
        .await
        .map_err(|_| format!("Redis ping timed out ({CONNECTION_TIMEOUT_SECS}s)"))?
        .map_err(|e| format!("Redis authentication failed or command rejected: {e}"))?;

        // A proxy may refuse INFO: an unknown version is treated as modern.
        #[cfg(feature = "abi-v2")]
        let server_major = tokio::time::timeout(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            redis::cmd("INFO")
                .arg("server")
                .query_async::<String>(&mut connection),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map_or(0, |info| parse_server_major(&info));

        Ok(Self {
            connection: Mutex::new(connection),
            #[cfg(not(feature = "abi-v2"))]
            url: url.to_string(),
            #[cfg(feature = "abi-v2")]
            server_major,
        })
    }

    async fn list_database_infos(&self) -> Result<Vec<RedisDatabaseInfo>, String> {
        let mut connection = self.connection.lock().await;
        list_databases(&mut *connection).await
    }
}

fn parse_redis_command(command: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ch if ch.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped || in_quotes {
        return Err("Invalid Redis command quoting".to_string());
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

async fn scan_key_metadata<C>(
    connection: &mut C,
    keys: Vec<Vec<u8>>,
) -> Result<Vec<RedisKeyMetadata>, String>
where
    C: ConnectionLike + Send + Sync + Unpin,
{
    if keys.is_empty() {
        return Ok(Vec::new());
    }

    let mut type_ttl_pipe = redis::pipe();
    for key in &keys {
        type_ttl_pipe.cmd("TYPE").arg(key).cmd("TTL").arg(key);
    }
    let type_ttl: Vec<RedisRawValue> = type_ttl_pipe
        .query_async(connection)
        .await
        .map_err(|e| format!("Redis metadata failed: {e}"))?;

    let mut items = Vec::with_capacity(keys.len());
    for (idx, key) in keys.into_iter().enumerate() {
        let raw_idx = idx * 2;
        let key_type = type_ttl
            .get(raw_idx)
            .and_then(|value| String::from_redis_value(value).ok())
            .unwrap_or_else(|| "unknown".to_string());
        let ttl_raw = type_ttl
            .get(raw_idx + 1)
            .and_then(|value| i64::from_redis_value(value).ok())
            .unwrap_or(-2);
        items.push(RedisKeyMetadata {
            key,
            key_type,
            ttl: (ttl_raw >= 0).then_some(ttl_raw),
            size: None,
            value_preview: None,
        });
    }

    let mut size_pipe = redis::pipe();
    let mut plans = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        match item.key_type.as_str() {
            "string" => {
                size_pipe
                    .cmd("STRLEN")
                    .arg(&item.key)
                    .cmd("GETRANGE")
                    .arg(&item.key)
                    .arg(0)
                    .arg(160);
                plans.push((idx, true));
            }
            "hash" => {
                size_pipe.cmd("HLEN").arg(&item.key);
                plans.push((idx, false));
            }
            "list" => {
                size_pipe.cmd("LLEN").arg(&item.key);
                plans.push((idx, false));
            }
            "set" => {
                size_pipe.cmd("SCARD").arg(&item.key);
                plans.push((idx, false));
            }
            "zset" => {
                size_pipe.cmd("ZCARD").arg(&item.key);
                plans.push((idx, false));
            }
            "stream" => {
                size_pipe.cmd("XLEN").arg(&item.key);
                plans.push((idx, false));
            }
            _ => {}
        }
    }

    if !plans.is_empty() {
        let values: Vec<RedisRawValue> = size_pipe
            .query_async(connection)
            .await
            .map_err(|e| format!("Redis metadata size failed: {e}"))?;
        let mut value_idx = 0;
        for (item_idx, has_preview) in plans {
            items[item_idx].size = values
                .get(value_idx)
                .and_then(|value| u64::from_redis_value(value).ok());
            value_idx += 1;
            if has_preview {
                items[item_idx].value_preview = values
                    .get(value_idx)
                    .and_then(|value| Vec::<u8>::from_redis_value(value).ok())
                    .map(|bytes| redis_key_bytes_to_display(&bytes));
                value_idx += 1;
            }
        }
    }

    Ok(items)
}

async fn list_databases<C>(connection: &mut C) -> Result<Vec<RedisDatabaseInfo>, String>
where
    C: ConnectionLike + Send + Sync + Unpin,
{
    let configured_count = redis::cmd("CONFIG")
        .arg("GET")
        .arg("databases")
        .query_async(connection)
        .await
        .ok()
        .and_then(parse_database_count);

    let keyspace_dbs = list_keyspace_databases(connection)
        .await
        .unwrap_or_default();
    let database_count = configured_count.unwrap_or(DEFAULT_REDIS_DATABASES);
    let max_db = keyspace_dbs
        .iter()
        .map(|db| db.db)
        .max()
        .map(|db| db + 1)
        .unwrap_or(0);
    let visible_count = database_count.max(max_db).max(1);
    let keyspace_counts = keyspace_dbs
        .into_iter()
        .map(|db| (db.db, db.keys))
        .collect::<HashMap<_, _>>();

    Ok((0..visible_count)
        .map(|db| RedisDatabaseInfo {
            db,
            keys: keyspace_counts.get(&db).copied().unwrap_or(0),
        })
        .collect())
}

fn parse_database_count(value: RedisRawValue) -> Option<u32> {
    let RedisRawValue::Array(values) = value else {
        return None;
    };
    values.windows(2).find_map(|pair| {
        let key = String::from_redis_value(&pair[0]).ok()?;
        if key.eq_ignore_ascii_case("databases") {
            String::from_redis_value(&pair[1]).ok()?.parse().ok()
        } else {
            None
        }
    })
}

async fn list_keyspace_databases<C>(connection: &mut C) -> Result<Vec<RedisDatabaseInfo>, String>
where
    C: ConnectionLike + Send + Sync + Unpin,
{
    let info: String = redis::cmd("INFO")
        .arg("keyspace")
        .query_async(connection)
        .await
        .map_err(|e| e.to_string())?;

    let mut dbs = Vec::new();
    for line in info.lines() {
        if let Some((db_part, stats_part)) = line
            .strip_prefix("db")
            .and_then(|rest| rest.split_once(':'))
        {
            if let Ok(db) = db_part.parse::<u32>() {
                let keys = stats_part
                    .split(',')
                    .find_map(|part| {
                        part.strip_prefix("keys=")
                            .and_then(|value| value.parse::<u64>().ok())
                    })
                    .unwrap_or(0);
                dbs.push(RedisDatabaseInfo { db, keys });
            }
        }
    }
    Ok(dbs)
}

pub fn parse_redis_endpoint(endpoint: &str, default_port: u16) -> Result<(String, u16), String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err("Redis node cannot be empty".to_string());
    }
    let endpoint = endpoint
        .strip_prefix("redis://")
        .or_else(|| endpoint.strip_prefix("rediss://"))
        .unwrap_or(endpoint);
    let endpoint = endpoint
        .rsplit_once('@')
        .map(|(_, tail)| tail)
        .unwrap_or(endpoint);
    let endpoint = endpoint.split(['/', '?', '#']).next().unwrap_or(endpoint);

    if let Some(rest) = endpoint.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err(format!("Invalid Redis node '{endpoint}'"));
        };
        let port = tail
            .strip_prefix(':')
            .filter(|value| !value.is_empty())
            .map(parse_redis_port)
            .transpose()?;
        return Ok((host.to_string(), port.unwrap_or(default_port)));
    }

    if let Some((host, port)) = endpoint.rsplit_once(':') {
        if !host.contains(':') && port.chars().all(|ch| ch.is_ascii_digit()) {
            return Ok((host.to_string(), parse_redis_port(port)?));
        }
    }

    Ok((endpoint.to_string(), default_port))
}

pub fn parse_redis_port(port: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .map_err(|_| format!("Invalid Redis port '{port}'"))
}

pub fn redis_key_bytes_to_display(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.replace('\\', "\\\\");
    }

    let mut output = String::new();
    for &byte in bytes {
        match byte {
            b'\\' => output.push_str("\\\\"),
            0x20..=0x7e => output.push(byte as char),
            _ => output.push_str(&format!("\\x{byte:02x}")),
        }
    }
    output
}

pub fn redis_key_bytes_to_raw(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn redis_key_raw_to_bytes(value: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| format!("Invalid Redis key encoding: {e}"))
}

pub fn classify_command(command: &str) -> RedisCommandSafety {
    let verb = parse_redis_command(command)
        .ok()
        .and_then(|args| args.into_iter().next())
        .unwrap_or_default()
        .to_ascii_uppercase();
    match verb.as_str() {
        "" => RedisCommandSafety::Allowed,
        "ACL" | "CLIENT" | "CLUSTER" | "KEYS" | "FLUSHALL" | "SHUTDOWN" | "CONFIG" | "SAVE"
        | "BGSAVE" | "SLAVEOF" | "REPLICAOF" | "MIGRATE" | "MODULE" | "SCRIPT" | "EVAL"
        | "EVALSHA" => RedisCommandSafety::Blocked,
        "ASKING" | "AUTH" | "COMMAND" | "DBSIZE" | "ECHO" | "EXISTS" | "GET" | "GETBIT"
        | "GETRANGE" | "HEXISTS" | "HGET" | "HGETALL" | "HKEYS" | "HLEN" | "HMGET"
        | "HRANDFIELD" | "HSCAN" | "HSTRLEN" | "HVALS" | "INFO" | "LATENCY" | "LINDEX" | "LLEN"
        | "LOLWUT" | "LPOS" | "LRANGE" | "MEMORY" | "MGET" | "OBJECT" | "PING" | "PUBLISH"
        | "PUBSUB" | "RANDOMKEY" | "SCAN" | "SCARD" | "SELECT" | "SINTER" | "SINTERCARD"
        | "SISMEMBER" | "SMEMBERS" | "SMISMEMBER" | "SORT_RO" | "SSCAN" | "STRLEN" | "SUNION"
        | "TIME" | "TTL" | "TYPE" | "XINFO" | "XLEN" | "XRANGE" | "XREAD" | "XREVRANGE"
        | "ZCARD" | "ZCOUNT" | "ZDIFF" | "ZINTER" | "ZLEXCOUNT" | "ZMSCORE" | "ZRANDMEMBER"
        | "ZRANGE" | "ZRANGEBYLEX" | "ZRANGEBYSCORE" | "ZRANK" | "ZREVRANGE" | "ZREVRANGEBYLEX"
        | "ZREVRANGEBYSCORE" | "ZREVRANK" | "ZSCAN" | "ZSCORE" | "ZUNION" => {
            RedisCommandSafety::Allowed
        }
        _ => RedisCommandSafety::Confirm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(value: &str) -> RedisRawValue {
        RedisRawValue::BulkString(value.as_bytes().to_vec())
    }

    #[cfg(feature = "abi-v2")]
    #[test]
    fn parses_the_server_major_version() {
        assert_eq!(
            parse_server_major("# Server\r\nredis_version:7.0.5\r\nos:x"),
            7
        );
        assert_eq!(parse_server_major("redis_version:5.0.14\r\n"), 5);
        assert_eq!(parse_server_major("redis_version:6.2.0-rc1"), 6);
        assert_eq!(parse_server_major("garbage"), 0);
        assert_eq!(parse_server_major(""), 0);
    }

    #[test]
    fn parses_configured_database_count() {
        let value = RedisRawValue::Array(vec![bulk("databases"), bulk("32")]);

        assert_eq!(parse_database_count(value), Some(32));
    }

    #[test]
    fn parses_redis_endpoints_with_default_ports() {
        assert_eq!(
            parse_redis_endpoint("cache.local:6380", 6379).unwrap(),
            ("cache.local".to_string(), 6380)
        );
        assert_eq!(
            parse_redis_endpoint("redis://user:pass@cache.local:6380/0", 6379).unwrap(),
            ("cache.local".to_string(), 6380)
        );
        assert_eq!(
            parse_redis_endpoint("cache.local", 6379).unwrap(),
            ("cache.local".to_string(), 6379)
        );
        assert_eq!(
            parse_redis_endpoint("[::1]:6380", 6379).unwrap(),
            ("::1".to_string(), 6380)
        );
        assert_eq!(
            parse_redis_endpoint("::1", 6379).unwrap(),
            ("::1".to_string(), 6379)
        );
    }

    #[test]
    fn round_trips_raw_key_transport() {
        let bytes = b"\xAC\xED\x00\x05t\x00token";
        let encoded = redis_key_bytes_to_raw(bytes);

        assert_eq!(redis_key_raw_to_bytes(&encoded).unwrap(), bytes);
    }

    #[test]
    fn formats_binary_keys_with_hex_escapes() {
        let bytes = [0xAC, 0xED, 0x00, 0x05, b't', 0x00, b'A', b'\\'];

        assert_eq!(
            redis_key_bytes_to_display(&bytes),
            "\\xac\\xed\\x00\\x05t\\x00A\\\\"
        );
    }

    #[test]
    fn classifies_safe_confirmed_and_blocked_commands() {
        assert_eq!(classify_command("GET"), RedisCommandSafety::Allowed);
        assert_eq!(classify_command("INFO server"), RedisCommandSafety::Allowed);
        assert_eq!(classify_command("set"), RedisCommandSafety::Confirm);
        assert_eq!(
            classify_command("SET session:1 value"),
            RedisCommandSafety::Confirm
        );
        assert_eq!(
            classify_command("INCR counter"),
            RedisCommandSafety::Confirm
        );
        assert_eq!(
            classify_command("NOT_A_REAL_COMMAND key"),
            RedisCommandSafety::Confirm
        );
        assert_eq!(classify_command("flushdb"), RedisCommandSafety::Confirm);
        assert_eq!(classify_command("KEYS *"), RedisCommandSafety::Blocked);
        assert_eq!(classify_command("KEYS"), RedisCommandSafety::Blocked);
        assert_eq!(classify_command("flushall"), RedisCommandSafety::Blocked);
        assert_eq!(classify_command("eval"), RedisCommandSafety::Blocked);
    }

    #[test]
    fn parses_redis_command_arguments_with_quotes() {
        assert_eq!(
            parse_redis_command("SET session:1 \"hello world\"").unwrap(),
            vec!["SET", "session:1", "hello world"]
        );
        assert_eq!(
            parse_redis_command("HGETALL user:1").unwrap(),
            vec!["HGETALL", "user:1"]
        );
        assert!(parse_redis_command("SET \"unterminated").is_err());
    }
}
