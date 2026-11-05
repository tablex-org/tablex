//! `dbengine_mutate`: key mutations.
//!
//! The identity names the key: `{"db":<n>, "key_raw":"<base64>" | "key":"<utf8>"}`
//! (`object` is the fallback key, `database` the fallback db; the db is
//! mandatory so a write never lands in an unintended database). Arguments are
//! read from `payload`, then from the identity, so the `mutation_identity` of
//! `inspect_row` plus the new value is a complete request. Every byte argument
//! `x` is given as `x_raw` (base64, binary safe) or `x` (UTF-8 text).
//!
//! | kind | arguments | `affected_rows` |
//! |---|---|---|
//! | `set_string` | `value`; optional `ttl_seconds`: absent/null keeps the key's TTL (`KEEPTTL`; on a server before Redis 6.0 that is refused with `mutation.unsupported`, see `Plan::check_server`), `> 0` sets it, `< 0` clears it, `0` is refused | 1 |
//! | `set_ttl` | `ttl_seconds`: `> 0` sets it, null/absent/`< 0` removes it (`PERSIST`), `0` is refused with `mutation.bad_request` (it would delete the key) | 1 if the key exists and a TTL was set or removed |
//! | `delete_key` | | keys deleted |
//! | `delete_keys` | `keys` (text) and/or `keys_raw` (base64) lists, no key identity | keys deleted |
//! | `hset` | `field`, `value` | 1 |
//! | `hdel` | `field` | fields removed |
//! | `lpush` / `rpush` | `value` | 1 |
//! | `lset` | `index` (integer), `value` | 1 |
//! | `sadd` | `member` | members added |
//! | `srem` | `member` | members removed |
//! | `zadd` | `score` (number or numeric text), `member` | 1 |
//! | `zrem` | `member` | members removed |
//! | `xadd` | `entry_id` (default `*`), `fields`: `[{field, value}, ..]` | 1 |
//! | `xdel` | `entry_id` | entries removed |
//! | `rename_key` | `new_key` / `new_key_raw`; optional `overwrite` (default `false`: `RENAMENX`, an existing target is left alone) | 1 if renamed, 0 if the target exists |
//! | `lrem` | `value`; optional `count` (default `1`: first match from the head; `0` all; negative from the tail) | items removed |

use super::{parse_db, redis_error};
use crate::redis_key_raw_to_bytes;
use engine_driver_kit::api::{Capability, MutationRequest};
use engine_driver_kit::BackendError;
use redis::aio::MultiplexedConnection;

/// Mutation kinds, equal to the suffix of their `mutation.<kind>` token.
#[cfg(test)]
pub(super) const KINDS: &[&str] = &[
    "set_string",
    "set_ttl",
    "delete_key",
    "delete_keys",
    "hset",
    "hdel",
    "lpush",
    "rpush",
    "lset",
    "sadd",
    "srem",
    "zadd",
    "zrem",
    "xadd",
    "xdel",
    "rename_key",
    "lrem",
];

pub(super) const CAPABILITIES: &[Capability] = &[
    Capability::MutationSetString,
    Capability::MutationSetTtl,
    Capability::MutationDeleteKey,
    Capability::MutationDeleteKeys,
    Capability::MutationHset,
    Capability::MutationHdel,
    Capability::MutationLpush,
    Capability::MutationRpush,
    Capability::MutationLset,
    Capability::MutationSadd,
    Capability::MutationSrem,
    Capability::MutationZadd,
    Capability::MutationZrem,
    Capability::MutationXadd,
    Capability::MutationXdel,
    Capability::MutationRenameKey,
    Capability::MutationLrem,
];

/// `DEL` batch size for `delete_keys`.
const DELETE_BATCH: usize = 1_000;

#[derive(Debug, PartialEq)]
enum Op {
    SetString {
        key: Vec<u8>,
        value: Vec<u8>,
        ttl: StringTtl,
    },
    /// `ttl_seconds: None` removes the TTL (`PERSIST`).
    SetTtl {
        key: Vec<u8>,
        ttl_seconds: Option<u64>,
    },
    DeleteKey {
        key: Vec<u8>,
    },
    DeleteKeys {
        keys: Vec<Vec<u8>>,
    },
    Hset {
        key: Vec<u8>,
        field: Vec<u8>,
        value: Vec<u8>,
    },
    Hdel {
        key: Vec<u8>,
        field: Vec<u8>,
    },
    Push {
        left: bool,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Lset {
        key: Vec<u8>,
        index: i64,
        value: Vec<u8>,
    },
    Sadd {
        key: Vec<u8>,
        member: Vec<u8>,
    },
    Srem {
        key: Vec<u8>,
        member: Vec<u8>,
    },
    Zadd {
        key: Vec<u8>,
        score: f64,
        member: Vec<u8>,
    },
    Zrem {
        key: Vec<u8>,
        member: Vec<u8>,
    },
    Xadd {
        key: Vec<u8>,
        entry_id: Vec<u8>,
        fields: StreamFields,
    },
    Xdel {
        key: Vec<u8>,
        entry_id: Vec<u8>,
    },
    RenameKey {
        key: Vec<u8>,
        new_key: Vec<u8>,
        overwrite: bool,
    },
    Lrem {
        key: Vec<u8>,
        value: Vec<u8>,
        count: i64,
    },
}

/// What `set_string` does with the key's TTL.
#[derive(Debug, PartialEq)]
enum StringTtl {
    /// `SET .. KEEPTTL` (Redis 6.0+): the default.
    Keep,
    /// `SET .. EX n`.
    Seconds(u64),
    /// Plain `SET`: the key becomes persistent.
    Clear,
}

/// A validated mutation: the database and the operation.
pub(super) struct Plan {
    db: u32,
    op: Op,
}

/// `EXPIRE key 0` and `SET .. EX 0` delete or fail; never do that silently.
fn zero_ttl() -> BackendError {
    bad("ttl_seconds 0 is refused (it would delete the key): use delete_key, a positive TTL, or null/negative to remove the TTL")
}

fn bad(message: impl Into<String>) -> BackendError {
    BackendError::with_code("mutation.bad_request", message)
}

/// Looks `name` up in `payload`, then in `identity`.
struct Args<'a> {
    payload: &'a serde_json::Value,
    identity: &'a serde_json::Value,
}

impl Args<'_> {
    fn get(&self, name: &str) -> Option<&serde_json::Value> {
        self.payload
            .get(name)
            .or_else(|| self.identity.get(name))
            .filter(|value| !value.is_null())
    }

    /// Bytes from `<name>_raw` (base64) or `<name>` (text).
    fn bytes(&self, name: &str) -> Result<Vec<u8>, BackendError> {
        if let Some(raw) = self.get(&format!("{name}_raw")) {
            let raw = raw
                .as_str()
                .ok_or_else(|| bad(format!("{name}_raw must be a base64 string")))?;
            return redis_key_raw_to_bytes(raw)
                .map_err(|error| bad(format!("{name}_raw: {error}")));
        }
        match self.get(name) {
            Some(serde_json::Value::String(text)) => Ok(text.clone().into_bytes()),
            Some(_) => Err(bad(format!("{name} must be a string"))),
            None => Err(bad(format!("missing {name}"))),
        }
    }

    /// `None` when `name` is absent or `null`; an error when it is not an integer.
    fn optional_integer(&self, name: &str) -> Result<Option<i64>, BackendError> {
        match self.get(name) {
            None => Ok(None),
            Some(_) => self.integer(name).map(Some),
        }
    }

    fn integer(&self, name: &str) -> Result<i64, BackendError> {
        self.get(name)
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str()?.trim().parse().ok())
            })
            .ok_or_else(|| bad(format!("missing or invalid integer {name}")))
    }

    fn number(&self, name: &str) -> Result<f64, BackendError> {
        self.get(name)
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str()?.trim().parse().ok())
            })
            .filter(|number| !number.is_nan())
            .ok_or_else(|| bad(format!("missing or invalid number {name}")))
    }
}

impl Plan {
    /// Refuses what the connected server cannot do. `SET .. KEEPTTL` (the
    /// default of `set_string`, so that editing a value never drops its expiry)
    /// needs Redis 6.0. Older servers have no atomic way to keep a TTL over a
    /// multiplexed connection (read the TTL, then `SET`, then `PEXPIRE` is not
    /// atomic, and `WATCH` state would be shared with other operations), so the
    /// request is refused with `mutation.unsupported` instead of silently
    /// dropping the TTL; `ttl_seconds` set explicitly (`> 0` to set a new one,
    /// `< 0` to clear it) works on every version. `server_major` 0 = unknown.
    pub(super) fn check_server(&self, server_major: u32) -> Result<(), BackendError> {
        let keeps_ttl = matches!(
            self.op,
            Op::SetString {
                ttl: StringTtl::Keep,
                ..
            }
        );
        if keeps_ttl && (1..6).contains(&server_major) {
            return Err(BackendError::with_code(
                "mutation.unsupported",
                format!(
                    "this Redis server (version {server_major}.x) cannot keep a key's TTL when \
                     overwriting it (SET .. KEEPTTL needs 6.0): pass ttl_seconds (positive to set \
                     a new TTL, negative to clear it)"
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn from_request(request: &MutationRequest) -> Result<Self, BackendError> {
        let args = Args {
            payload: &request.payload,
            identity: &request.identity,
        };
        let db = request
            .identity
            .get("db")
            .and_then(parse_db)
            .or_else(|| {
                request
                    .database
                    .as_deref()
                    .and_then(|database| database.trim().parse().ok())
            })
            .ok_or_else(|| bad("missing db (identity.db or database)"))?;
        let key = || -> Result<Vec<u8>, BackendError> {
            let key = match request
                .identity
                .get("key_raw")
                .and_then(serde_json::Value::as_str)
            {
                Some(raw) => {
                    redis_key_raw_to_bytes(raw).map_err(|error| bad(format!("key_raw: {error}")))?
                }
                None => match request
                    .identity
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                {
                    Some(text) => text.as_bytes().to_vec(),
                    None => request.object.clone().unwrap_or_default().into_bytes(),
                },
            };
            if key.is_empty() {
                Err(bad(
                    "missing key (identity.key_raw, identity.key or object)",
                ))
            } else {
                Ok(key)
            }
        };
        let op = match request.kind.as_str() {
            "set_string" => Op::SetString {
                key: key()?,
                value: args.bytes("value")?,
                ttl: match args.optional_integer("ttl_seconds")? {
                    None => StringTtl::Keep,
                    Some(seconds) if seconds < 0 => StringTtl::Clear,
                    Some(0) => return Err(zero_ttl()),
                    Some(seconds) => StringTtl::Seconds(seconds.unsigned_abs()),
                },
            },
            "set_ttl" => Op::SetTtl {
                key: key()?,
                ttl_seconds: match args.optional_integer("ttl_seconds")? {
                    None => None,
                    Some(seconds) if seconds < 0 => None,
                    Some(0) => return Err(zero_ttl()),
                    Some(seconds) => Some(seconds.unsigned_abs()),
                },
            },
            "delete_key" => Op::DeleteKey { key: key()? },
            "delete_keys" => Op::DeleteKeys {
                keys: key_list(&request.payload)?,
            },
            "hset" => Op::Hset {
                key: key()?,
                field: args.bytes("field")?,
                value: args.bytes("value")?,
            },
            "hdel" => Op::Hdel {
                key: key()?,
                field: args.bytes("field")?,
            },
            "lpush" => Op::Push {
                left: true,
                key: key()?,
                value: args.bytes("value")?,
            },
            "rpush" => Op::Push {
                left: false,
                key: key()?,
                value: args.bytes("value")?,
            },
            "lset" => Op::Lset {
                key: key()?,
                index: args.integer("index")?,
                value: args.bytes("value")?,
            },
            "sadd" => Op::Sadd {
                key: key()?,
                member: args.bytes("member")?,
            },
            "srem" => Op::Srem {
                key: key()?,
                member: args.bytes("member")?,
            },
            "zadd" => Op::Zadd {
                key: key()?,
                score: args.number("score")?,
                member: args.bytes("member")?,
            },
            "zrem" => Op::Zrem {
                key: key()?,
                member: args.bytes("member")?,
            },
            "xadd" => Op::Xadd {
                key: key()?,
                entry_id: match args.bytes("entry_id") {
                    Ok(id) if !id.is_empty() => id,
                    _ => b"*".to_vec(),
                },
                fields: stream_fields(&request.payload)?,
            },
            "xdel" => Op::Xdel {
                key: key()?,
                entry_id: args.bytes("entry_id")?,
            },
            "rename_key" => {
                let new_key = args.bytes("new_key")?;
                if new_key.is_empty() {
                    return Err(bad("new_key must not be empty"));
                }
                Op::RenameKey {
                    key: key()?,
                    new_key,
                    overwrite: args
                        .get("overwrite")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                }
            }
            "lrem" => Op::Lrem {
                key: key()?,
                value: args.bytes("value")?,
                count: args.optional_integer("count")?.unwrap_or(1),
            },
            other => {
                return Err(BackendError::with_code(
                    "mutation.unsupported",
                    format!("Redis does not support mutation `{other}`"),
                ))
            }
        };
        Ok(Self { db, op })
    }
}

fn key_list(payload: &serde_json::Value) -> Result<Vec<Vec<u8>>, BackendError> {
    let mut keys = Vec::new();
    for (name, raw) in [("keys", false), ("keys_raw", true)] {
        let Some(list) = payload.get(name).filter(|value| !value.is_null()) else {
            continue;
        };
        for item in list
            .as_array()
            .ok_or_else(|| bad(format!("{name} must be a list")))?
        {
            let text = item
                .as_str()
                .ok_or_else(|| bad(format!("{name} must hold strings")))?;
            keys.push(if raw {
                redis_key_raw_to_bytes(text).map_err(|error| bad(format!("{name}: {error}")))?
            } else {
                text.as_bytes().to_vec()
            });
        }
    }
    if keys.iter().any(Vec::is_empty) {
        return Err(bad("keys must not be empty"));
    }
    Ok(keys)
}

type StreamFields = Vec<(Vec<u8>, Vec<u8>)>;

fn stream_fields(payload: &serde_json::Value) -> Result<StreamFields, BackendError> {
    let list = payload
        .get("fields")
        .and_then(serde_json::Value::as_array)
        .filter(|list| !list.is_empty())
        .ok_or_else(|| bad("missing stream fields"))?;
    list.iter()
        .map(|pair| {
            let args = Args {
                payload: pair,
                identity: &serde_json::Value::Null,
            };
            Ok((args.bytes("field")?, args.bytes("value")?))
        })
        .collect()
}

pub(super) async fn run(con: &mut MultiplexedConnection, plan: &Plan) -> Result<u64, BackendError> {
    redis::cmd("SELECT")
        .arg(plan.db)
        .query_async::<()>(con)
        .await
        .map_err(|error| redis_error("Redis SELECT failed", error))?;
    let failed = |command: &str| {
        let command = command.to_owned();
        move |error| redis_error(&format!("Redis {command} failed"), error)
    };
    Ok(match &plan.op {
        Op::SetString { key, value, ttl } => {
            let mut command = redis::cmd("SET");
            command.arg(key).arg(value);
            match ttl {
                StringTtl::Keep => {
                    command.arg("KEEPTTL");
                }
                StringTtl::Seconds(seconds) => {
                    command.arg("EX").arg(*seconds);
                }
                StringTtl::Clear => {}
            }
            command
                .query_async::<()>(con)
                .await
                .map_err(failed("SET"))?;
            1
        }
        Op::SetTtl { key, ttl_seconds } => {
            let changed: bool = match ttl_seconds {
                None => redis::cmd("PERSIST")
                    .arg(key)
                    .query_async(con)
                    .await
                    .map_err(failed("PERSIST"))?,
                Some(seconds) => redis::cmd("EXPIRE")
                    .arg(key)
                    .arg(seconds)
                    .query_async(con)
                    .await
                    .map_err(failed("EXPIRE"))?,
            };
            u64::from(changed)
        }
        Op::DeleteKey { key } => redis::cmd("DEL")
            .arg(key)
            .query_async(con)
            .await
            .map_err(failed("DEL"))?,
        Op::DeleteKeys { keys } => {
            let mut deleted = 0;
            for batch in keys.chunks(DELETE_BATCH) {
                let count: u64 = redis::cmd("DEL")
                    .arg(batch)
                    .query_async(con)
                    .await
                    .map_err(failed("DEL"))?;
                deleted += count;
            }
            deleted
        }
        Op::Hset { key, field, value } => {
            redis::cmd("HSET")
                .arg(key)
                .arg(field)
                .arg(value)
                .query_async::<u64>(con)
                .await
                .map_err(failed("HSET"))?;
            1
        }
        Op::Hdel { key, field } => redis::cmd("HDEL")
            .arg(key)
            .arg(field)
            .query_async(con)
            .await
            .map_err(failed("HDEL"))?,
        Op::Push { left, key, value } => {
            let name = if *left { "LPUSH" } else { "RPUSH" };
            redis::cmd(name)
                .arg(key)
                .arg(value)
                .query_async::<u64>(con)
                .await
                .map_err(failed(name))?;
            1
        }
        Op::Lset { key, index, value } => {
            redis::cmd("LSET")
                .arg(key)
                .arg(index)
                .arg(value)
                .query_async::<()>(con)
                .await
                .map_err(failed("LSET"))?;
            1
        }
        Op::Sadd { key, member } => redis::cmd("SADD")
            .arg(key)
            .arg(member)
            .query_async(con)
            .await
            .map_err(failed("SADD"))?,
        Op::Srem { key, member } => redis::cmd("SREM")
            .arg(key)
            .arg(member)
            .query_async(con)
            .await
            .map_err(failed("SREM"))?,
        Op::Zadd { key, score, member } => {
            redis::cmd("ZADD")
                .arg(key)
                .arg(score)
                .arg(member)
                .query_async::<u64>(con)
                .await
                .map_err(failed("ZADD"))?;
            1
        }
        Op::Zrem { key, member } => redis::cmd("ZREM")
            .arg(key)
            .arg(member)
            .query_async(con)
            .await
            .map_err(failed("ZREM"))?,
        Op::Xadd {
            key,
            entry_id,
            fields,
        } => {
            let mut command = redis::cmd("XADD");
            command.arg(key).arg(entry_id);
            for (field, value) in fields {
                command.arg(field).arg(value);
            }
            command
                .query_async::<Vec<u8>>(con)
                .await
                .map_err(failed("XADD"))?;
            1
        }
        Op::Xdel { key, entry_id } => redis::cmd("XDEL")
            .arg(key)
            .arg(entry_id)
            .query_async(con)
            .await
            .map_err(failed("XDEL"))?,
        Op::RenameKey {
            key,
            new_key,
            overwrite: true,
        } => {
            redis::cmd("RENAME")
                .arg(key)
                .arg(new_key)
                .query_async::<()>(con)
                .await
                .map_err(failed("RENAME"))?;
            1
        }
        Op::RenameKey {
            key,
            new_key,
            overwrite: false,
        } => redis::cmd("RENAMENX")
            .arg(key)
            .arg(new_key)
            .query_async(con)
            .await
            .map_err(failed("RENAMENX"))?,
        Op::Lrem { key, value, count } => redis::cmd("LREM")
            .arg(key)
            .arg(count)
            .arg(value)
            .query_async(con)
            .await
            .map_err(failed("LREM"))?,
    })
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Plan(db {}, {:?})", self.db, self.op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_kit::api::OperationId;

    fn request(
        kind: &str,
        identity: serde_json::Value,
        payload: serde_json::Value,
    ) -> MutationRequest {
        MutationRequest {
            operation_id: OperationId(1),
            kind: kind.into(),
            database: None,
            object: None,
            identity,
            payload,
        }
    }

    fn plan(
        kind: &str,
        identity: serde_json::Value,
        payload: serde_json::Value,
    ) -> Result<Plan, BackendError> {
        Plan::from_request(&request(kind, identity, payload))
    }

    #[test]
    fn kinds_and_capabilities_line_up() {
        assert_eq!(KINDS.len(), CAPABILITIES.len());
        for (kind, capability) in KINDS.iter().zip(CAPABILITIES) {
            assert_eq!(
                Capability::for_mutation_kind(kind),
                Some(*capability),
                "{kind}"
            );
        }
    }

    #[test]
    fn every_kind_parses() {
        let id = serde_json::json!({"db": 9, "key": "k"});
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("set_string", serde_json::json!({"value": "v"})),
            ("set_ttl", serde_json::json!({"ttl_seconds": 30})),
            ("delete_key", serde_json::Value::Null),
            ("hset", serde_json::json!({"field": "f", "value": "v"})),
            ("hdel", serde_json::json!({"field": "f"})),
            ("lpush", serde_json::json!({"value": "v"})),
            ("rpush", serde_json::json!({"value": "v"})),
            ("lset", serde_json::json!({"index": -1, "value": "v"})),
            ("sadd", serde_json::json!({"member": "m"})),
            ("srem", serde_json::json!({"member": "m"})),
            ("zadd", serde_json::json!({"score": "1.5", "member": "m"})),
            ("zrem", serde_json::json!({"member": "m"})),
            (
                "xadd",
                serde_json::json!({"fields": [{"field": "a", "value": "b"}]}),
            ),
            ("xdel", serde_json::json!({"entry_id": "1-0"})),
            ("rename_key", serde_json::json!({"new_key": "k2"})),
            ("lrem", serde_json::json!({"value": "v"})),
        ];
        for (kind, payload) in cases {
            assert!(plan(kind, id.clone(), payload).is_ok(), "{kind}");
        }
        assert!(plan(
            "delete_keys",
            serde_json::json!({"db": 9}),
            serde_json::json!({"keys": ["a"], "keys_raw": ["Yg=="]})
        )
        .is_ok());
    }

    #[test]
    fn set_string_keeps_the_ttl_unless_asked() {
        let id = serde_json::json!({"db": 9, "key": "k"});
        let ttl_of = |payload: serde_json::Value| match plan("set_string", id.clone(), payload) {
            Ok(Plan {
                op: Op::SetString { ttl, .. },
                ..
            }) => ttl,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(ttl_of(serde_json::json!({"value": "v"})), StringTtl::Keep);
        assert_eq!(
            ttl_of(serde_json::json!({"value": "v", "ttl_seconds": null})),
            StringTtl::Keep
        );
        assert_eq!(
            ttl_of(serde_json::json!({"value": "v", "ttl_seconds": 60})),
            StringTtl::Seconds(60)
        );
        assert_eq!(
            ttl_of(serde_json::json!({"value": "v", "ttl_seconds": -1})),
            StringTtl::Clear
        );
        let zero = plan(
            "set_string",
            id,
            serde_json::json!({"value": "v", "ttl_seconds": 0}),
        );
        assert_eq!(
            zero.err().and_then(|e| e.code).as_deref(),
            Some("mutation.bad_request")
        );
    }

    #[test]
    fn keepttl_is_refused_on_servers_before_6_0_unless_the_ttl_is_explicit() {
        let id = serde_json::json!({"db": 9, "key": "k"});
        let keep = plan("set_string", id.clone(), serde_json::json!({"value": "v"}))
            .ok()
            .unwrap();
        for major in [6, 7, 8, 0] {
            assert!(keep.check_server(major).is_ok(), "major {major}");
        }
        for major in [1, 2, 3, 4, 5] {
            let error = keep.check_server(major).unwrap_err();
            assert_eq!(error.code.as_deref(), Some("mutation.unsupported"));
            assert!(error.message.contains("ttl_seconds"), "{}", error.message);
        }
        for payload in [
            serde_json::json!({"value": "v", "ttl_seconds": 30}),
            serde_json::json!({"value": "v", "ttl_seconds": -1}),
        ] {
            let explicit = plan("set_string", id.clone(), payload).ok().unwrap();
            assert!(explicit.check_server(5).is_ok());
        }
        let other = plan("hset", id, serde_json::json!({"field": "f", "value": "v"}))
            .ok()
            .unwrap();
        assert!(other.check_server(4).is_ok());
    }

    #[test]
    fn set_ttl_zero_is_refused_and_null_persists() {
        let id = serde_json::json!({"db": 9, "key": "k"});
        let ttl_of = |payload: serde_json::Value| match plan("set_ttl", id.clone(), payload) {
            Ok(Plan {
                op: Op::SetTtl { ttl_seconds, .. },
                ..
            }) => ttl_seconds,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(ttl_of(serde_json::json!({"ttl_seconds": 30})), Some(30));
        assert_eq!(ttl_of(serde_json::json!({"ttl_seconds": null})), None);
        assert_eq!(ttl_of(serde_json::Value::Null), None);
        assert_eq!(ttl_of(serde_json::json!({"ttl_seconds": -5})), None);
        assert_eq!(ttl_of(serde_json::json!({"ttl_seconds": "45"})), Some(45));
        let zero = plan("set_ttl", id, serde_json::json!({"ttl_seconds": 0}));
        assert_eq!(
            zero.err().and_then(|e| e.code).as_deref(),
            Some("mutation.bad_request")
        );
    }

    #[test]
    fn binary_identity_and_arguments_are_decoded() {
        let plan = plan(
            "hset",
            serde_json::json!({"db": "3", "key_raw": "rO0=", "field_raw": "ZgA="}),
            serde_json::json!({"value_raw": "/w=="}),
        )
        .ok()
        .unwrap();
        assert_eq!(plan.db, 3);
        assert_eq!(
            plan.op,
            Op::Hset {
                key: vec![0xac, 0xed],
                field: b"f\0".to_vec(),
                value: vec![0xff]
            }
        );
    }

    #[test]
    fn validation_errors_use_bad_request() {
        let code = |result: Result<Plan, BackendError>| {
            result.err().and_then(|e| e.code).unwrap().into_owned()
        };
        assert_eq!(
            code(plan(
                "set_string",
                serde_json::json!({"key": "k"}),
                serde_json::json!({"value": "v"})
            )),
            "mutation.bad_request"
        );
        assert_eq!(
            code(plan(
                "set_string",
                serde_json::json!({"db": 1}),
                serde_json::json!({"value": "v"})
            )),
            "mutation.bad_request"
        );
        assert_eq!(
            code(plan(
                "set_string",
                serde_json::json!({"db": 1, "key": "k"}),
                serde_json::Value::Null
            )),
            "mutation.bad_request"
        );
        assert_eq!(
            code(plan(
                "zadd",
                serde_json::json!({"db": 1, "key": "k"}),
                serde_json::json!({"score": "NaN", "member": "m"})
            )),
            "mutation.bad_request"
        );
        assert_eq!(
            code(plan(
                "xadd",
                serde_json::json!({"db": 1, "key": "k"}),
                serde_json::json!({"fields": []})
            )),
            "mutation.bad_request"
        );
        assert_eq!(
            code(plan(
                "set_string",
                serde_json::json!({"db": 1, "key_raw": "!!"}),
                serde_json::json!({"value": "v"})
            )),
            "mutation.bad_request"
        );
        assert_eq!(
            code(plan(
                "insert",
                serde_json::json!({"db": 1, "key": "k"}),
                serde_json::Value::Null
            )),
            "mutation.unsupported"
        );
    }

    #[test]
    fn rename_key_and_lrem_parse_their_arguments() {
        let id = serde_json::json!({"db": 1, "key": "k"});
        let rename = plan("rename_key", id.clone(), serde_json::json!({"new_key_raw": "/w=="}))
            .ok()
            .unwrap();
        assert_eq!(
            rename.op,
            Op::RenameKey {
                key: b"k".to_vec(),
                new_key: vec![0xff],
                overwrite: false
            }
        );
        let overwrite = plan(
            "rename_key",
            id.clone(),
            serde_json::json!({"new_key": "n", "overwrite": true}),
        )
        .ok()
        .unwrap();
        assert!(matches!(overwrite.op, Op::RenameKey { overwrite: true, .. }));
        for payload in [serde_json::json!({}), serde_json::json!({"new_key": ""})] {
            let error = plan("rename_key", id.clone(), payload).unwrap_err();
            assert_eq!(error.code.as_deref(), Some("mutation.bad_request"));
        }
        let lrem = |payload| match plan("lrem", id.clone(), payload) {
            Ok(Plan {
                op: Op::Lrem { count, .. },
                ..
            }) => count,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(lrem(serde_json::json!({"value": "v"})), 1);
        assert_eq!(lrem(serde_json::json!({"value": "v", "count": 0})), 0);
        assert_eq!(lrem(serde_json::json!({"value": "v", "count": "-2"})), -2);
    }

    #[test]
    fn xadd_defaults_to_auto_id_and_empty_values_are_allowed() {
        let plan = plan(
            "xadd",
            serde_json::json!({"db": 1, "key": "s"}),
            serde_json::json!({"fields": [{"field": "a", "value": ""}]}),
        )
        .ok()
        .unwrap();
        assert_eq!(
            plan.op,
            Op::Xadd {
                key: b"s".to_vec(),
                entry_id: b"*".to_vec(),
                fields: vec![(b"a".to_vec(), vec![])]
            }
        );
    }
}
