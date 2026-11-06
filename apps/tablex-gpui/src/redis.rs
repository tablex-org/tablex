//! Redis model of the key browser: key types, the header facts of a key,
//! TTL text, `:`-namespace splitting, command knowledge (completion, read-only
//! classification) and the wire payloads of the driver's `redis.*` queries and
//! key mutations. Pure Rust with no GPUI, so it is cheap to test.

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{Array, RecordBatch};
use base64::Engine as _;
use serde_json::{json, Value};

/// Query kind of a typed value page.
pub const KIND_KEY: &str = "redis.key";
/// Query kind of the one-row key description (type, TTL, length, encoding).
pub const KIND_KEY_INFO: &str = "redis.key_info";
/// Query kind of the command console.
pub const KIND_COMMAND: &str = "redis.command";
/// Namespace delimiter of the key tree.
pub const DELIMITER: &str = ":";

/// The six Redis data types a value page can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
}

impl KeyType {
    pub const ALL: [KeyType; 6] = [
        Self::String,
        Self::Hash,
        Self::List,
        Self::Set,
        Self::ZSet,
        Self::Stream,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    /// The name Redis reports (`TYPE`) and the driver's `payload.type` filter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::ZSet => "zset",
            Self::Stream => "stream",
        }
    }

    /// What one row of the value page is called.
    pub fn element(self) -> &'static str {
        match self {
            Self::String => "value",
            Self::Hash => "field",
            Self::List => "item",
            Self::Set => "member",
            Self::ZSet => "member",
            Self::Stream => "entry",
        }
    }
}

/// What a key is, for the header of its tab.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyInfo {
    /// `TYPE` as the server reports it (module types included).
    pub type_name: String,
    /// Seconds to live when the info was read; `None` = no expiry.
    pub ttl: Option<i64>,
    /// Elements (bytes for a string).
    pub length: Option<i64>,
    pub encoding: Option<String>,
    /// `MEMORY USAGE` in bytes.
    pub memory: Option<i64>,
}

impl KeyInfo {
    pub fn key_type(&self) -> Option<KeyType> {
        KeyType::parse(&self.type_name)
    }

    /// Reads the one-row page of a `redis.key_info` query.
    pub fn from_batch(batch: &RecordBatch) -> Option<Self> {
        if batch.num_rows() == 0 {
            return None;
        }
        let text = |name: &str| {
            let array = batch.column_by_name(name)?;
            // `_opt`: a schema change in the driver reads as a missing value, not a panic on the UI thread.
            array.as_string_opt::<i32>().filter(|a| !a.is_null(0)).map(|a| a.value(0).to_owned())
        };
        let number = |name: &str| {
            let array = batch.column_by_name(name)?;
            array.as_primitive_opt::<Int64Type>().filter(|a| !a.is_null(0)).map(|a| a.value(0))
        };
        Some(Self {
            type_name: text("type")?,
            ttl: number("ttl"),
            length: number("length"),
            encoding: text("encoding"),
            memory: number("memory"),
        })
    }
}

/// How the string value of a key is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StringView {
    #[default]
    Text,
    /// Pretty-printed when the value is JSON.
    Json,
    /// Hex dump of the exact bytes.
    Hex,
}

/// The string value a string key tab holds (the first page of the key).
#[derive(Debug, Clone)]
pub struct StringValue {
    pub bytes: std::sync::Arc<[u8]>,
    /// The key is longer than the page the driver returned.
    pub truncated: bool,
}

impl StringValue {
    /// The view a value opens in: JSON when it parses, hex when it is not text.
    pub fn default_view(&self) -> StringView {
        match std::str::from_utf8(&self.bytes) {
            Err(_) => StringView::Hex,
            Ok(text) if json_pretty(text).is_some() => StringView::Json,
            Ok(_) => StringView::Text,
        }
    }
}

/// Pretty-printed JSON of an object / array text (`None` for anything else:
/// a bare number or string is better left as it is).
pub fn json_pretty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with(['{', '[']) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

/// `00000000  68 65 6c 6c 6f  |hello|` rows of 16 bytes.
pub fn hex_dump(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() / 16 * 78 + 78);
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", row * 16);
        for column in 0..16 {
            match chunk.get(column) {
                Some(byte) => {
                    let _ = write!(out, "{byte:02x} ");
                }
                None => out.push_str("   "),
            }
            if column == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        out.extend(chunk.iter().map(|&byte| {
            if (0x20..0x7f).contains(&byte) {
                byte as char
            } else {
                '.'
            }
        }));
        out.push_str("|\n");
    }
    out
}

/// Runtime of one Redis key tab (see `TabRuntime::redis`).
#[derive(Debug, Default)]
pub struct TabState {
    pub info: Option<KeyInfo>,
    /// When `info` was read: the TTL counts down from here.
    pub info_read_at: Option<std::time::Instant>,
    pub info_error: Option<String>,
    pub info_loading: bool,
    pub string: Option<StringValue>,
    /// Bumped whenever `string` holds a different value (the editor refills).
    pub revision: u64,
    pub view: StringView,
}

impl TabState {
    /// Seconds the key still lives, counted from when the info was read.
    /// `None` = no expiry (or no info yet).
    pub fn ttl_left(&self, now: std::time::Instant) -> Option<i64> {
        let ttl = self.info.as_ref()?.ttl?;
        let elapsed = self
            .info_read_at
            .map_or(0, |at| now.saturating_duration_since(at).as_secs());
        Some(ttl - i64::try_from(elapsed).unwrap_or(i64::MAX))
    }

    /// The key has a TTL that has not run out: the countdown needs a repaint.
    pub fn counting_down(&self, now: std::time::Instant) -> bool {
        self.ttl_left(now).is_some_and(|left| left > 0)
    }
}

/// `1 field`, `5 fields`, `1 entry`, `8 entries`.
pub fn count_label(count: i64, key_type: KeyType) -> String {
    let element = key_type.element();
    match (count, element) {
        (1, _) => format!("1 {element}"),
        (n, "entry") => format!("{n} entries"),
        (n, _) => format!("{n} {element}s"),
    }
}

/// `45s`, `5m 12s`, `3h 4m`, `2d 5h`: at most two units.
pub fn format_ttl(seconds: i64) -> String {
    if seconds <= 0 {
        return "expired".to_owned();
    }
    let (days, hours, minutes, secs) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
        seconds % 60,
    );
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, _) => format!("{minutes}m {secs}s"),
        (0, _, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

/// What the TTL field of a dialog asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// Remove the expiry (`PERSIST`).
    Persist,
    Seconds(u64),
}

/// `""` / `persist` / `none` / `-1` remove the expiry; `90`, `90s`, `5m`, `2h`
/// and `1d` set it. `0` is refused (it would delete the key).
pub fn parse_ttl(input: &str) -> Result<Ttl, String> {
    let text = input.trim().to_ascii_lowercase();
    if matches!(text.as_str(), "" | "persist" | "none" | "never" | "-1") {
        return Ok(Ttl::Persist);
    }
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text.as_str(), "s"),
    };
    let multiplier = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        _ => return Err(format!("`{}` is not a duration (use 90, 5m, 2h or 1d)", input.trim())),
    };
    let amount: u64 = digits
        .parse()
        .map_err(|_| format!("`{}` is not a duration (use 90, 5m, 2h or 1d)", input.trim()))?;
    match amount.checked_mul(multiplier) {
        Some(0) => Err("A TTL of 0 would delete the key: delete it instead".to_owned()),
        Some(seconds) => Ok(Ttl::Seconds(seconds)),
        None => Err("That TTL is too large".to_owned()),
    }
}

/// Server-side narrowing of a database's key list plus how the loaded keys are
/// shown. `pattern` is a `SCAN MATCH` glob (`user:*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFilter {
    pub pattern: String,
    pub key_type: Option<KeyType>,
    /// Group the loaded keys into a tree by [`DELIMITER`].
    pub group: bool,
}

impl Default for KeyFilter {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            key_type: None,
            group: true,
        }
    }
}

impl KeyFilter {
    /// Narrows the server's answer (the grouping only changes the display).
    pub fn narrows(&self) -> bool {
        !self.pattern.trim().is_empty() || self.key_type.is_some()
    }

    /// Adds `pattern` / `type` to a navigator payload.
    pub fn apply_to(&self, payload: &mut Value) {
        let Some(map) = payload.as_object_mut() else {
            return;
        };
        map.remove("pattern");
        map.remove("type");
        let pattern = self.pattern.trim();
        if !pattern.is_empty() {
            map.insert("pattern".into(), pattern.into());
        }
        if let Some(key_type) = self.key_type {
            map.insert("type".into(), key_type.as_str().into());
        }
    }

    /// `user:* · hash` for the database row.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.pattern.trim().is_empty() {
            parts.push(self.pattern.trim().to_owned());
        }
        if let Some(key_type) = self.key_type {
            parts.push(key_type.as_str().to_owned());
        }
        parts.join(" \u{00B7} ")
    }
}

/// `user:12:name` -> (`user`, `12:name`); `None` when the key has no
/// namespace, or an empty segment (`:a`, `a::b`) that would make an empty
/// folder.
pub fn split_namespace(key: &str) -> Option<(&str, &str)> {
    let (head, rest) = key.split_once(DELIMITER)?;
    (!head.is_empty() && !rest.is_empty()).then_some((head, rest))
}

/// `SCAN MATCH` pattern that finds exactly the keys below `prefix` (every glob
/// character of the prefix is escaped).
pub fn subtree_pattern(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 2);
    for c in prefix.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push_str(DELIMITER);
    pattern.push('*');
    pattern
}

pub fn encode_key(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_key(raw: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(raw).ok()
}

/// A key name for display: valid UTF-8 as is, other bytes as `\xNN` (what the
/// driver shows).
pub fn display_key(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            let mut out = String::new();
            for &byte in bytes {
                match byte {
                    b'\\' => out.push_str("\\\\"),
                    0x20..=0x7e => out.push(byte as char),
                    _ => out.push_str(&format!("\\x{byte:02x}")),
                }
            }
            out
        }
    }
}

/// `key@db0`, the label of a value tab.
pub fn tab_label(key: &str, db: &str) -> String {
    const MAX: usize = 28;
    let key = if key.chars().count() > MAX {
        let head: String = key.chars().take(MAX - 1).collect();
        format!("{head}\u{2026}")
    } else {
        key.to_owned()
    };
    format!("{key}@db{db}")
}

// ---- requests ------------------------------------------------------------

/// `payload` of a value-page / key-info query.
pub fn key_payload(key_raw: &str) -> Value {
    json!({ "key_raw": key_raw })
}

/// `payload` of a console command.
pub fn command_payload(command: &str, confirmed: bool) -> Value {
    if confirmed {
        json!({ "command": command, "confirmed": true })
    } else {
        json!({ "command": command })
    }
}

/// Identity of a key in a mutation request.
pub fn key_identity(db: &str, key_raw: &str) -> Value {
    json!({ "db": db.parse::<u32>().unwrap_or(0), "key_raw": key_raw })
}

/// One edit the UI can ask the driver for. Every variant maps to one
/// `mutation.<kind>`; [`Self::kind`] and [`Self::payload`] are the wire form.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    SetString { value: String },
    SetTtl(Ttl),
    DeleteKey,
    Rename { new_key: String, overwrite: bool },
    HashSet { field: String, value: String },
    HashDelete { field_raw: String },
    ListPush { value: String, left: bool },
    ListSet { index: i64, value: String },
    ListRemove { value_raw: String },
    SetAdd { member: String },
    SetRemove { member_raw: String },
    ZAdd { member: String, score: f64 },
    ZRemove { member_raw: String },
    StreamAdd { fields: Vec<(String, String)> },
    StreamDelete { entry_id: String },
}

impl Edit {
    /// `MutationRequest.kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SetString { .. } => "set_string",
            Self::SetTtl(_) => "set_ttl",
            Self::DeleteKey => "delete_key",
            Self::Rename { .. } => "rename_key",
            Self::HashSet { .. } => "hset",
            Self::HashDelete { .. } => "hdel",
            Self::ListPush { left: true, .. } => "lpush",
            Self::ListPush { left: false, .. } => "rpush",
            Self::ListSet { .. } => "lset",
            Self::ListRemove { .. } => "lrem",
            Self::SetAdd { .. } => "sadd",
            Self::SetRemove { .. } => "srem",
            Self::ZAdd { .. } => "zadd",
            Self::ZRemove { .. } => "zrem",
            Self::StreamAdd { .. } => "xadd",
            Self::StreamDelete { .. } => "xdel",
        }
    }

    /// Whether the edit removes data (it asks for a confirmation first).
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Self::DeleteKey
                | Self::HashDelete { .. }
                | Self::ListRemove { .. }
                | Self::SetRemove { .. }
                | Self::ZRemove { .. }
                | Self::StreamDelete { .. }
        )
    }

    /// `MutationRequest.payload`. A set / list / hash member given by its
    /// exact bytes (`*_raw`) never goes through a lossy text conversion.
    pub fn payload(&self) -> Value {
        match self {
            Self::SetString { value } => json!({ "value": value }),
            Self::SetTtl(Ttl::Persist) => json!({ "ttl_seconds": -1 }),
            Self::SetTtl(Ttl::Seconds(seconds)) => json!({ "ttl_seconds": seconds }),
            Self::DeleteKey => Value::Null,
            Self::Rename { new_key, overwrite } => {
                json!({ "new_key": new_key, "overwrite": overwrite })
            }
            Self::HashSet { field, value } => json!({ "field": field, "value": value }),
            Self::HashDelete { field_raw } => json!({ "field_raw": field_raw }),
            Self::ListPush { value, .. } => json!({ "value": value }),
            Self::ListSet { index, value } => json!({ "index": index, "value": value }),
            Self::ListRemove { value_raw } => json!({ "value_raw": value_raw, "count": 1 }),
            Self::SetAdd { member } => json!({ "member": member }),
            Self::SetRemove { member_raw } => json!({ "member_raw": member_raw }),
            Self::ZAdd { member, score } => json!({ "member": member, "score": score }),
            Self::ZRemove { member_raw } => json!({ "member_raw": member_raw }),
            Self::StreamAdd { fields } => json!({
                "fields": fields
                    .iter()
                    .map(|(field, value)| json!({ "field": field, "value": value }))
                    .collect::<Vec<_>>(),
            }),
            Self::StreamDelete { entry_id } => json!({ "entry_id": entry_id }),
        }
    }

    /// The capability token the driver must advertise.
    pub fn capability(&self) -> engine_driver_api::Capability {
        engine_driver_api::Capability::for_mutation_kind(self.kind())
            .expect("every Redis edit kind has a capability")
    }
}

// ---- commands --------------------------------------------------------------

/// Redis commands the console completes: name, argument summary, whether the
/// command only reads.
pub const COMMANDS: &[(&str, &str, bool)] = &[
    ("APPEND", "key value", false),
    ("BITCOUNT", "key [start end]", true),
    ("BITPOS", "key bit [start [end]]", true),
    ("COPY", "source destination [DB db] [REPLACE]", false),
    ("DBSIZE", "", true),
    ("DECR", "key", false),
    ("DECRBY", "key decrement", false),
    ("DEL", "key [key ...]", false),
    ("DUMP", "key", true),
    ("ECHO", "message", true),
    ("EXISTS", "key [key ...]", true),
    ("EXPIRE", "key seconds", false),
    ("EXPIREAT", "key unix-time-seconds", false),
    ("EXPIRETIME", "key", true),
    ("FLUSHALL", "[ASYNC|SYNC]", false),
    ("FLUSHDB", "[ASYNC|SYNC]", false),
    ("GET", "key", true),
    ("GETDEL", "key", false),
    ("GETEX", "key [EX seconds|PERSIST]", false),
    ("GETRANGE", "key start end", true),
    ("GETSET", "key value", false),
    ("HDEL", "key field [field ...]", false),
    ("HEXISTS", "key field", true),
    ("HGET", "key field", true),
    ("HGETALL", "key", true),
    ("HINCRBY", "key field increment", false),
    ("HINCRBYFLOAT", "key field increment", false),
    ("HKEYS", "key", true),
    ("HLEN", "key", true),
    ("HMGET", "key field [field ...]", true),
    ("HMSET", "key field value [field value ...]", false),
    ("HRANDFIELD", "key [count [WITHVALUES]]", true),
    ("HSCAN", "key cursor [MATCH pattern] [COUNT count]", true),
    ("HSET", "key field value [field value ...]", false),
    ("HSETNX", "key field value", false),
    ("HSTRLEN", "key field", true),
    ("HVALS", "key", true),
    ("INCR", "key", false),
    ("INCRBY", "key increment", false),
    ("INCRBYFLOAT", "key increment", false),
    ("INFO", "[section]", true),
    ("KEYS", "pattern", false),
    ("LINDEX", "key index", true),
    ("LINSERT", "key BEFORE|AFTER pivot element", false),
    ("LLEN", "key", true),
    ("LMOVE", "source destination LEFT|RIGHT LEFT|RIGHT", false),
    ("LPOP", "key [count]", false),
    ("LPOS", "key element", true),
    ("LPUSH", "key element [element ...]", false),
    ("LPUSHX", "key element [element ...]", false),
    ("LRANGE", "key start stop", true),
    ("LREM", "key count element", false),
    ("LSET", "key index element", false),
    ("LTRIM", "key start stop", false),
    ("MEMORY", "USAGE key", true),
    ("MGET", "key [key ...]", true),
    ("MOVE", "key db", false),
    ("MSET", "key value [key value ...]", false),
    ("MSETNX", "key value [key value ...]", false),
    ("OBJECT", "ENCODING|FREQ|IDLETIME|REFCOUNT key", true),
    ("PERSIST", "key", false),
    ("PEXPIRE", "key milliseconds", false),
    ("PEXPIREAT", "key unix-time-milliseconds", false),
    ("PFADD", "key element [element ...]", false),
    ("PFCOUNT", "key [key ...]", false),
    ("PING", "[message]", true),
    ("PSETEX", "key milliseconds value", false),
    ("PTTL", "key", true),
    ("PUBLISH", "channel message", true),
    ("RANDOMKEY", "", true),
    ("RENAME", "key newkey", false),
    ("RENAMENX", "key newkey", false),
    ("RPOP", "key [count]", false),
    ("RPUSH", "key element [element ...]", false),
    ("RPUSHX", "key element [element ...]", false),
    ("SADD", "key member [member ...]", false),
    ("SCAN", "cursor [MATCH pattern] [COUNT count] [TYPE type]", true),
    ("SCARD", "key", true),
    ("SDIFF", "key [key ...]", false),
    ("SDIFFSTORE", "destination key [key ...]", false),
    ("SELECT", "index", true),
    ("SET", "key value [EX seconds] [NX|XX] [KEEPTTL]", false),
    ("SETEX", "key seconds value", false),
    ("SETNX", "key value", false),
    ("SETRANGE", "key offset value", false),
    ("SINTER", "key [key ...]", true),
    ("SINTERSTORE", "destination key [key ...]", false),
    ("SISMEMBER", "key member", true),
    ("SMEMBERS", "key", true),
    ("SMISMEMBER", "key member [member ...]", true),
    ("SMOVE", "source destination member", false),
    ("SORT_RO", "key [BY pattern] [LIMIT offset count] [ASC|DESC] [ALPHA]", true),
    ("SPOP", "key [count]", false),
    ("SRANDMEMBER", "key [count]", false),
    ("SREM", "key member [member ...]", false),
    ("SSCAN", "key cursor [MATCH pattern] [COUNT count]", true),
    ("STRLEN", "key", true),
    ("SUNION", "key [key ...]", true),
    ("SUNIONSTORE", "destination key [key ...]", false),
    ("TIME", "", true),
    ("TOUCH", "key [key ...]", false),
    ("TTL", "key", true),
    ("TYPE", "key", true),
    ("UNLINK", "key [key ...]", false),
    ("XACK", "key group id [id ...]", false),
    ("XADD", "key [MAXLEN n] *|id field value [field value ...]", false),
    ("XDEL", "key id [id ...]", false),
    ("XGROUP", "CREATE|DESTROY|SETID ...", false),
    ("XINFO", "STREAM|GROUPS|CONSUMERS key", true),
    ("XLEN", "key", true),
    ("XRANGE", "key start end [COUNT count]", true),
    ("XREAD", "[COUNT count] STREAMS key [key ...] id [id ...]", true),
    ("XREVRANGE", "key end start [COUNT count]", true),
    ("XTRIM", "key MAXLEN|MINID threshold", false),
    ("ZADD", "key [NX|XX] score member [score member ...]", false),
    ("ZCARD", "key", true),
    ("ZCOUNT", "key min max", true),
    ("ZINCRBY", "key increment member", false),
    ("ZPOPMAX", "key [count]", false),
    ("ZPOPMIN", "key [count]", false),
    ("ZRANGE", "key start stop [BYSCORE|BYLEX] [REV] [WITHSCORES]", true),
    ("ZRANGEBYSCORE", "key min max [WITHSCORES] [LIMIT offset count]", true),
    ("ZRANK", "key member", true),
    ("ZREM", "key member [member ...]", false),
    ("ZREMRANGEBYSCORE", "key min max", false),
    ("ZREVRANGE", "key start stop [WITHSCORES]", true),
    ("ZREVRANK", "key member", true),
    ("ZSCAN", "key cursor [MATCH pattern] [COUNT count]", true),
    ("ZSCORE", "key member", true),
];

/// The non-blank line `cursor` is on, or `None` when the script is a single
/// line (or the cursor is on a blank line of several): then the whole script
/// runs.
pub fn line_at(script: &str, cursor: usize) -> Option<String> {
    if !script.trim().contains('\n') {
        return None;
    }
    let cursor = cursor.min(script.len());
    let start = script[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let end = script[cursor..].find('\n').map_or(script.len(), |at| cursor + at);
    let line = script[start..end].trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// `true` when the first word of `command` is a command that only reads. An
/// unknown or empty command counts as a write (the safe answer for a
/// read-only connection). Mirrors the driver's `Allowed` class; the driver is
/// the authority (it also blocks or asks to confirm), this only decides what a
/// read-only connection may send at all.
pub fn is_read_only(command: &str) -> bool {
    let Some(verb) = command.split_whitespace().next() else {
        return false;
    };
    let verb = verb.to_ascii_uppercase();
    // Verbs the driver treats as read-only that are not in the completion table.
    const EXTRA_READ_ONLY: &[&str] = &[
        "ASKING", "AUTH", "COMMAND", "GETBIT", "LATENCY", "LOLWUT", "PUBSUB", "SINTERCARD",
        "ZDIFF", "ZINTER", "ZLEXCOUNT", "ZMSCORE", "ZRANDMEMBER", "ZRANGEBYLEX",
        "ZREVRANGEBYLEX", "ZREVRANGEBYSCORE", "ZUNION",
    ];
    COMMANDS
        .iter()
        .find(|(name, _, _)| *name == verb)
        .map(|(_, _, read_only)| *read_only)
        .unwrap_or_else(|| EXTRA_READ_ONLY.contains(&verb.as_str()))
}

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub detail: String,
    pub kind: CandidateKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Command,
    Key,
}

/// Completions at `cursor` (a byte offset into `text`): command names for the
/// first word of the line, key names for the later ones. Returns the byte
/// range the chosen item replaces and at most `max` candidates, best first.
pub fn complete<'a>(
    text: &str,
    cursor: usize,
    keys: impl IntoIterator<Item = &'a str>,
    max: usize,
) -> (std::ops::Range<usize>, Vec<Candidate>) {
    let cursor = cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let word_start = text[line_start..cursor]
        .rfind(char::is_whitespace)
        .map_or(line_start, |at| line_start + at + 1);
    let prefix = &text[word_start..cursor];
    let range = word_start..cursor;
    let first_word = text[line_start..word_start].trim().is_empty();
    if first_word {
        let upper = prefix.to_ascii_uppercase();
        let mut items: Vec<Candidate> = COMMANDS
            .iter()
            .filter(|(name, _, _)| name.starts_with(&upper))
            .map(|(name, syntax, _)| Candidate {
                label: (*name).to_owned(),
                detail: (*syntax).to_owned(),
                kind: CandidateKind::Command,
            })
            .take(max)
            .collect();
        // An empty prefix offers nothing: the menu would list every command.
        if prefix.is_empty() {
            items.clear();
        }
        return (range, items);
    }
    if prefix.starts_with(['"', '\'']) {
        return (range, Vec::new());
    }
    let mut items: Vec<Candidate> = keys
        .into_iter()
        .filter(|key| key.len() >= prefix.len() && key.starts_with(prefix) && !key.contains(char::is_whitespace))
        .map(|key| Candidate {
            label: key.to_owned(),
            detail: "key".to_owned(),
            kind: CandidateKind::Key,
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    items.truncate(max);
    (range, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_pluralised_by_element() {
        assert_eq!(count_label(1, KeyType::Hash), "1 field");
        assert_eq!(count_label(5, KeyType::Hash), "5 fields");
        assert_eq!(count_label(0, KeyType::List), "0 items");
        assert_eq!(count_label(1, KeyType::Stream), "1 entry");
        assert_eq!(count_label(8, KeyType::Stream), "8 entries");
    }

    #[test]
    fn ttl_text_has_at_most_two_units() {
        assert_eq!(format_ttl(0), "expired");
        assert_eq!(format_ttl(45), "45s");
        assert_eq!(format_ttl(312), "5m 12s");
        assert_eq!(format_ttl(3 * 3600 + 4 * 60 + 9), "3h 4m");
        assert_eq!(format_ttl(2 * 86_400 + 5 * 3600 + 59), "2d 5h");
    }

    #[test]
    fn ttl_input_parses_units_and_refuses_zero() {
        assert_eq!(parse_ttl("90"), Ok(Ttl::Seconds(90)));
        assert_eq!(parse_ttl(" 5m "), Ok(Ttl::Seconds(300)));
        assert_eq!(parse_ttl("2H"), Ok(Ttl::Seconds(7_200)));
        assert_eq!(parse_ttl("1d"), Ok(Ttl::Seconds(86_400)));
        for none in ["", "persist", "NONE", "-1"] {
            assert_eq!(parse_ttl(none), Ok(Ttl::Persist), "{none}");
        }
        assert!(parse_ttl("0").unwrap_err().contains("delete"));
        assert!(parse_ttl("0m").is_err());
        assert!(parse_ttl("12 parsecs").is_err());
        assert!(parse_ttl("-5").is_err());
        assert!(parse_ttl("99999999999999999999999").is_err());
        assert!(parse_ttl("18446744073709551615d").unwrap_err().contains("large"));
    }

    #[test]
    fn filter_payload_carries_pattern_and_type_only_when_set() {
        let mut payload = json!({"database": "0", "db": 0, "pattern": "old*", "type": "set"});
        KeyFilter::default().apply_to(&mut payload);
        assert!(payload.get("pattern").is_none() && payload.get("type").is_none());
        let filter = KeyFilter {
            pattern: " user:* ".into(),
            key_type: Some(KeyType::Hash),
            group: false,
        };
        filter.apply_to(&mut payload);
        assert_eq!(payload["pattern"], "user:*");
        assert_eq!(payload["type"], "hash");
        assert_eq!(payload["db"], 0);
        assert!(filter.narrows() && !KeyFilter::default().narrows());
        assert_eq!(filter.summary(), "user:* \u{00B7} hash");
    }

    #[test]
    fn namespaces_split_at_the_first_delimiter_only_when_both_sides_exist() {
        assert_eq!(split_namespace("user:12:name"), Some(("user", "12:name")));
        assert_eq!(split_namespace("plain"), None);
        assert_eq!(split_namespace(":lead"), None);
        assert_eq!(split_namespace("tail:"), None);
        assert_eq!(subtree_pattern("a*b:c"), "a\\*b:c:*");
    }

    #[test]
    fn key_names_round_trip_and_tab_labels_are_short() {
        let bytes = b"\xac\xed:1";
        assert_eq!(decode_key(&encode_key(bytes)).as_deref(), Some(&bytes[..]));
        assert_eq!(display_key(bytes), "\\xac\\xed:1");
        assert_eq!(display_key(b"user:1"), "user:1");
        assert_eq!(tab_label("user:1", "0"), "user:1@db0");
        let long = tab_label(&"k".repeat(60), "12");
        assert!(long.ends_with("\u{2026}@db12") && long.chars().count() <= 28 + 5);
    }

    #[test]
    fn every_edit_has_its_kind_a_capability_and_payload() {
        let edits = [
            Edit::SetString { value: "v".into() },
            Edit::SetTtl(Ttl::Persist),
            Edit::SetTtl(Ttl::Seconds(30)),
            Edit::DeleteKey,
            Edit::Rename { new_key: "n".into(), overwrite: false },
            Edit::HashSet { field: "f".into(), value: "v".into() },
            Edit::HashDelete { field_raw: "Zg==".into() },
            Edit::ListPush { value: "v".into(), left: true },
            Edit::ListPush { value: "v".into(), left: false },
            Edit::ListSet { index: 2, value: "v".into() },
            Edit::ListRemove { value_raw: "dg==".into() },
            Edit::SetAdd { member: "m".into() },
            Edit::SetRemove { member_raw: "bQ==".into() },
            Edit::ZAdd { member: "m".into(), score: 1.5 },
            Edit::ZRemove { member_raw: "bQ==".into() },
            Edit::StreamAdd { fields: vec![("a".into(), "b".into())] },
            Edit::StreamDelete { entry_id: "1-0".into() },
        ];
        for edit in &edits {
            let _ = edit.capability();
            assert!(!edit.kind().is_empty());
        }
        assert_eq!(edits[1].payload()["ttl_seconds"], -1);
        assert_eq!(edits[2].payload()["ttl_seconds"], 30);
        assert_eq!(edits[7].kind(), "lpush");
        assert_eq!(edits[8].kind(), "rpush");
        assert_eq!(edits[13].payload()["score"], 1.5);
        assert_eq!(edits[15].payload()["fields"][0]["value"], "b");
        let destructive: Vec<_> = edits.iter().filter(|e| e.destructive()).map(Edit::kind).collect();
        assert_eq!(destructive, ["delete_key", "hdel", "lrem", "srem", "zrem", "xdel"]);
    }

    #[test]
    fn read_only_classification_defaults_to_write() {
        for read in ["get k", "  HGETALL h", "ttl k", "scan 0 match a*", "ZRANGEBYSCORE z 0 1", "info"] {
            assert!(is_read_only(read), "{read}");
        }
        for write in ["set k v", "FLUSHDB", "del a", "config set maxmemory 1", "mystery x", "", "  ", "KEYS *"] {
            assert!(!is_read_only(write), "{write}");
        }
    }

    #[test]
    fn completion_offers_commands_first_and_loaded_keys_after() {
        let keys = ["user:1", "user:2", "order:9"];
        let (range, items) = complete("hg", 2, keys, 10);
        assert_eq!(range, 0..2);
        assert_eq!(
            items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["HGET", "HGETALL"]
        );
        assert!(items.iter().all(|i| i.kind == CandidateKind::Command));
        let text = "GET us";
        let (range, items) = complete(text, text.len(), keys, 10);
        assert_eq!(range, 4..6);
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["user:1", "user:2"]);
        assert_eq!(items[0].kind, CandidateKind::Key);
        // The second line starts a new command.
        let text = "GET a\nse";
        let (range, items) = complete(text, text.len(), keys, 10);
        assert_eq!(range, 6..8);
        assert!(items.iter().any(|i| i.label == "SET") && items.iter().all(|i| i.kind == CandidateKind::Command));
        // Nothing for an empty first word, nor inside a quoted argument.
        assert!(complete("", 0, keys, 10).1.is_empty());
        assert!(complete("GET \"us", 7, keys, 10).1.is_empty());
        // Capped.
        assert_eq!(complete("s", 1, keys, 3).1.len(), 3);
    }

    #[test]
    fn string_values_pick_a_view_and_dump_hex() {
        let value = |bytes: &[u8]| StringValue { bytes: bytes.into(), truncated: false };
        assert_eq!(value(br#"{"a":[1,2]}"#).default_view(), StringView::Json);
        assert_eq!(value(b"plain text").default_view(), StringView::Text);
        assert_eq!(value(b"42").default_view(), StringView::Text);
        assert_eq!(value(b"{broken").default_view(), StringView::Text);
        assert_eq!(value(b"\xff\x00").default_view(), StringView::Hex);
        assert_eq!(json_pretty(r#"{"a":1}"#).as_deref(), Some("{\n  \"a\": 1\n}"));
        assert_eq!(
            hex_dump(b"hello, redis world!\x00"),
            "00000000  68 65 6c 6c 6f 2c 20 72  65 64 69 73 20 77 6f 72  |hello, redis wor|\n\
             00000010  6c 64 21 00                                       |ld!.|\n"
        );
        assert_eq!(hex_dump(b""), "");
    }

    #[test]
    fn the_ttl_counts_down_from_when_it_was_read() {
        use std::time::{Duration, Instant};
        let read = Instant::now();
        let state = TabState {
            info: Some(KeyInfo {
                type_name: "string".into(),
                ttl: Some(90),
                length: Some(1),
                encoding: None,
                memory: None,
            }),
            info_read_at: Some(read),
            ..TabState::default()
        };
        assert_eq!(state.ttl_left(read + Duration::from_secs(30)), Some(60));
        assert!(state.counting_down(read));
        assert_eq!(state.ttl_left(read + Duration::from_secs(100)), Some(-10));
        assert!(!state.counting_down(read + Duration::from_secs(100)));
        assert_eq!(TabState::default().ttl_left(read), None);
    }

    #[test]
    fn key_info_reads_the_driver_page() {
        use arrow_array::{ArrayRef, Int64Array, StringArray};
        use arrow_schema::{DataType, Field, Schema};
        use std::sync::Arc;
        let schema = Arc::new(Schema::new(vec![
            Field::new("type", DataType::Utf8, false),
            Field::new("ttl", DataType::Int64, true),
            Field::new("length", DataType::Int64, true),
            Field::new("encoding", DataType::Utf8, true),
            Field::new("memory", DataType::Int64, true),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec!["hash"])),
            Arc::new(Int64Array::from(vec![None])),
            Arc::new(Int64Array::from(vec![Some(3)])),
            Arc::new(StringArray::from(vec![Some("listpack")])),
            Arc::new(Int64Array::from(vec![None])),
        ];
        let batch = RecordBatch::try_new(schema, columns).unwrap();
        let info = KeyInfo::from_batch(&batch).unwrap();
        assert_eq!(info.key_type(), Some(KeyType::Hash));
        assert_eq!((info.ttl, info.length, info.memory), (None, Some(3), None));
        assert_eq!(info.encoding.as_deref(), Some("listpack"));
        assert!(KeyInfo::from_batch(&batch.slice(0, 0)).is_none());
    }
}
