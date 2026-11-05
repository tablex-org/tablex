//! The Redis stand-in of the UI fixture: an in-memory server with a few
//! databases of typed keys, answering what the Redis driver answers (database
//! and key navigator pages with `SCAN`-style cursors, pattern and type filters,
//! typed value pages, the key description, console commands with the
//! confirmation / blocked classes, and the key edits). It backs the `redis` UI
//! scenario and the interaction tests that must not need a server; the live
//! tests use the real driver instead.

use crate::redis::{encode_key, Edit, KeyInfo, KeyType, Ttl};
use arrow_array::builder::BinaryBuilder;
use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use engine_driver_api::{NavigatorNode, NavigatorPage};
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Hash(Vec<(String, String)>),
    List(Vec<String>),
    Set(Vec<String>),
    ZSet(Vec<(String, f64)>),
    Stream(Vec<(String, Vec<(String, String)>)>),
}

impl Value {
    pub fn key_type(&self) -> KeyType {
        match self {
            Self::Text(_) => KeyType::String,
            Self::Hash(_) => KeyType::Hash,
            Self::List(_) => KeyType::List,
            Self::Set(_) => KeyType::Set,
            Self::ZSet(_) => KeyType::ZSet,
            Self::Stream(_) => KeyType::Stream,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Hash(items) => items.len(),
            Self::List(items) | Self::Set(items) => items.len(),
            Self::ZSet(items) => items.len(),
            Self::Stream(items) => items.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub db: u32,
    pub name: String,
    pub ttl: Option<i64>,
    pub value: Value,
}

/// Why the fixture refused a console command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRefusal {
    /// `query.confirmation_required`
    Confirm(String),
    /// `query.blocked`
    Blocked(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct RedisFixture {
    keys: Vec<Key>,
    /// Commands the fixture executed after a confirmation.
    pub confirmed_commands: Vec<String>,
}

impl Default for RedisFixture {
    fn default() -> Self {
        Self::sample()
    }
}

fn text(value: &str) -> String {
    value.to_owned()
}

impl RedisFixture {
    /// 120 keys in db0 (users, sessions, caches, queues ...) and a few in db1.
    pub fn sample() -> Self {
        let mut keys = Vec::new();
        let mut add = |db, name: String, ttl, value| keys.push(Key { db, name, ttl, value });
        let plans = ["free", "team", "pro", "enterprise"];
        for index in 0..24_u32 {
            add(
                0,
                format!("user:{}", 1001 + index),
                None,
                Value::Hash(vec![
                    (text("name"), format!("User {}", 1001 + index)),
                    (text("email"), format!("user{}@example.com", 1001 + index)),
                    (text("plan"), plans[index as usize % 4].to_owned()),
                    (text("created"), format!("2026-0{}-1{}", 1 + index % 9, index % 9)),
                    (text("logins"), (index * 7 + 3).to_string()),
                ]),
            );
        }
        for index in 0..10_u32 {
            add(
                0,
                format!("session:{:08x}", 0x9e37_79b9_u32.wrapping_mul(index + 1)),
                Some(1800 - i64::from(index) * 90),
                Value::Text(format!("{{\"user\":{},\"ip\":\"192.0.2.{}\"}}", 1001 + index, 10 + index)),
            );
        }
        for index in 0..12_u32 {
            add(
                0,
                format!("cache:product:{}", 200 + index),
                Some(3600),
                Value::Text(format!(
                    "{{\"id\":{},\"sku\":\"SKU-{}\",\"price\":{}.99,\"tags\":[\"new\",\"sale\"]}}",
                    200 + index,
                    4000 + index,
                    9 + index
                )),
            );
        }
        add(0, text("cache:page:home"), Some(300), Value::Text(text("<html><body>fixture</body></html>")));
        add(
            0,
            text("config:app"),
            None,
            Value::Text(text(
                "{\"name\":\"storefront\",\"version\":\"2.4.1\",\"features\":{\"search\":true,\"beta\":false},\"regions\":[\"eu\",\"us\"],\"limits\":{\"rps\":250,\"burst\":40}}",
            )),
        );
        add(0, text("counter:visits"), None, Value::Text(text("48213")));
        add(0, text("maintenance"), None, Value::Text(text("off")));
        add(
            0,
            text("queue:emails"),
            None,
            Value::List((0..40).map(|index| format!("job-{index:04}")).collect()),
        );
        add(0, text("queue:billing"), None, Value::List(vec![text("invoice-77"), text("invoice-78"), text("refund-9")]));
        add(
            0,
            text("tags:featured"),
            None,
            Value::Set(["red", "blue", "green", "gold", "teal", "plum"].iter().map(|tag| (*tag).to_owned()).collect()),
        );
        add(
            0,
            text("leaderboard:weekly"),
            None,
            Value::ZSet((0..15).map(|index| (format!("player{index:02}"), 1200.0 - f64::from(index) * 37.5)).collect()),
        );
        add(
            0,
            text("events:orders"),
            None,
            Value::Stream(
                (0..8)
                    .map(|index| {
                        (
                            format!("17{index:02}000000-0"),
                            vec![
                                (text("order"), format!("A-{}", 100 + index)),
                                (text("status"), ["created", "paid", "shipped", "done"][index % 4].to_owned()),
                            ],
                        )
                    })
                    .collect(),
            ),
        );
        for index in 0..3 {
            add(1, format!("scratch:{index}"), Some(60), Value::Text(format!("tmp-{index}")));
        }
        Self { keys, confirmed_commands: Vec::new() }
    }

    #[cfg(test)]
    /// `count` string keys in db0 named `ns07:grp12:item004567` (50 namespaces
    /// of 20 groups), for the key-list measurements.
    pub fn generated(count: usize) -> Self {
        let keys = (0..count)
            .map(|index| Key {
                db: 0,
                name: format!("ns{:02}:grp{:02}:item{index:06}", index % 50, (index / 50) % 20),
                ttl: None,
                value: Value::Text("v".into()),
            })
            .collect();
        Self { keys, confirmed_commands: Vec::new() }
    }

    pub fn key(&self, db: u32, name: &str) -> Option<&Key> {
        self.keys.iter().find(|key| key.db == db && key.name == name)
    }

    pub fn db_size(&self, db: u32) -> usize {
        self.keys.iter().filter(|key| key.db == db).count()
    }

    // ---- navigator -------------------------------------------------------

    /// The navigator answer for the root (databases) or a database (keys).
    pub fn navigator_page(
        &self,
        parent: Option<&str>,
        payload: Option<&serde_json::Value>,
        cursor: Option<&str>,
        limit: u64,
    ) -> Result<NavigatorPage, String> {
        let Some(parent) = parent else {
            let nodes = (0..16_u32)
                .map(|db| {
                    let keys = self.db_size(db);
                    NavigatorNode {
                        id: format!("database:{db}"),
                        kind: "database".into(),
                        label: format!("db{db} ({keys} keys)"),
                        subtitle: None,
                        icon: Some("database".into()),
                        has_children: keys > 0,
                        payload: json!({"database": db.to_string(), "db": db, "keys": keys}),
                        actions: vec!["refresh".into()],
                    }
                })
                // The fixture shows the databases that hold keys, like the
                // first screen of a real server.
                .filter(|node| node.has_children)
                .collect();
            return Ok(NavigatorPage { nodes, next_cursor: None });
        };
        let db: u32 = parent
            .strip_prefix("database:")
            .and_then(|db| db.parse().ok())
            .ok_or_else(|| format!("unknown Redis navigator parent `{parent}`"))?;
        let filter = |name: &str| {
            payload
                .and_then(|payload| payload.get(name))
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
        };
        let (pattern, key_type) = (filter("pattern"), filter("type"));
        let matching: Vec<&Key> = self
            .keys
            .iter()
            .filter(|key| key.db == db)
            .filter(|key| pattern.is_none_or(|pattern| glob_match(pattern, &key.name)))
            .filter(|key| key_type.is_none_or(|wanted| key.value.key_type().as_str() == wanted))
            .collect();
        let start: usize = cursor.and_then(|cursor| cursor.parse().ok()).unwrap_or(0);
        let take = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
        let page: Vec<&Key> = matching.iter().skip(start).take(take).copied().collect();
        let next = start + page.len();
        Ok(NavigatorPage {
            nodes: page.into_iter().map(key_node).collect(),
            next_cursor: (next < matching.len()).then(|| next.to_string()),
        })
    }

    // ---- values ---------------------------------------------------------

    /// What `redis.key_info` answers.
    pub fn key_info(&self, db: u32, name: &str) -> Result<KeyInfo, String> {
        let key = self.key(db, name).ok_or_else(|| "This key no longer exists".to_owned())?;
        Ok(KeyInfo {
            type_name: key.value.key_type().as_str().to_owned(),
            ttl: key.ttl,
            length: Some(key.value.len() as i64),
            encoding: Some(
                match &key.value {
                    Value::Text(text) if text.parse::<i64>().is_ok() => "int",
                    Value::Text(_) => "embstr",
                    Value::Hash(_) | Value::ZSet(_) => "listpack",
                    Value::List(_) => "quicklist",
                    Value::Set(_) => "hashtable",
                    Value::Stream(_) => "stream",
                }
                .to_owned(),
            ),
            memory: Some(64 + key.value.len() as i64 * 24),
        })
    }

    /// What `redis.key` answers for rows `offset..offset + limit`; also the
    /// element count.
    pub fn value_page(&self, db: u32, name: &str, offset: usize, limit: usize) -> Result<(RecordBatch, usize), String> {
        let key = self.key(db, name).ok_or_else(|| "This key no longer exists".to_owned())?;
        let window = |len: usize| offset.min(len)..(offset + limit.max(1)).min(len);
        Ok(match &key.value {
            Value::Text(value) => (binary_batch(&[("value", vec![value.as_bytes().to_vec()])]), 1),
            Value::Hash(items) => {
                let range = window(items.len());
                (
                    binary_batch(&[
                        ("field", items[range.clone()].iter().map(|(f, _)| f.as_bytes().to_vec()).collect()),
                        ("value", items[range].iter().map(|(_, v)| v.as_bytes().to_vec()).collect()),
                    ]),
                    items.len(),
                )
            }
            Value::List(items) => {
                let range = window(items.len());
                let indexes: ArrayRef = Arc::new(Int64Array::from_iter_values(range.clone().map(|i| i as i64)));
                let values = binary_array(items[range].iter().map(|v| v.as_bytes().to_vec()).collect());
                (
                    RecordBatch::try_new(
                        schema(vec![
                            Field::new("index", DataType::Int64, false),
                            Field::new("value", DataType::Binary, false),
                        ]),
                        vec![indexes, values],
                    )
                    .expect("list batch"),
                    items.len(),
                )
            }
            Value::Set(items) => {
                let range = window(items.len());
                (binary_batch(&[("member", items[range].iter().map(|v| v.as_bytes().to_vec()).collect())]), items.len())
            }
            Value::ZSet(items) => {
                let range = window(items.len());
                let scores: ArrayRef = Arc::new(Float64Array::from_iter_values(items[range.clone()].iter().map(|(_, s)| *s)));
                let members = binary_array(items[range].iter().map(|(m, _)| m.as_bytes().to_vec()).collect());
                (
                    RecordBatch::try_new(
                        schema(vec![
                            Field::new("member", DataType::Binary, false),
                            Field::new("score", DataType::Float64, false),
                        ]),
                        vec![members, scores],
                    )
                    .expect("zset batch"),
                    items.len(),
                )
            }
            Value::Stream(items) => {
                let range = window(items.len());
                let ids: ArrayRef = Arc::new(StringArray::from_iter_values(items[range.clone()].iter().map(|(id, _)| id.as_str())));
                let fields: ArrayRef = Arc::new(StringArray::from_iter_values(items[range].iter().map(|(_, fields)| {
                    let body: Vec<String> = fields.iter().map(|(f, v)| format!("{f:?}: {v:?}")).collect();
                    format!("{{{}}}", body.join(", "))
                })));
                (
                    RecordBatch::try_new(
                        schema(vec![
                            Field::new("id", DataType::Utf8, false),
                            Field::new("fields", DataType::Utf8, false),
                        ]),
                        vec![ids, fields],
                    )
                    .expect("stream batch"),
                    items.len(),
                )
            }
        })
    }

    // ---- console ------------------------------------------------------------

    /// What `redis.command` answers on database `db`.
    pub fn command(&mut self, db: u32, command: &str, confirmed: bool) -> Result<RecordBatch, CommandRefusal> {
        let mut words = command.split_whitespace();
        let verb = words.next().unwrap_or("").to_ascii_uppercase();
        let args: Vec<&str> = words.collect();
        let scalar = |value: String| binary_batch(&[("value", vec![value.into_bytes()])]);
        match verb.as_str() {
            "KEYS" | "FLUSHALL" | "CONFIG" | "SHUTDOWN" | "EVAL" => {
                Err(CommandRefusal::Blocked(format!("Redis command blocked: {command}")))
            }
            "GET" => match self.key(db, args.first().copied().unwrap_or("")) {
                Some(Key { value: Value::Text(value), .. }) => Ok(scalar(value.clone())),
                Some(_) => Err(CommandRefusal::Failed(
                    "WRONGTYPE Operation against a key holding the wrong kind of value".into(),
                )),
                None => Ok(binary_batch(&[("value", vec![b"(nil)".to_vec()])])),
            },
            "HGETALL" => match self.key(db, args.first().copied().unwrap_or("")) {
                Some(Key { value: Value::Hash(items), .. }) => Ok(binary_batch(&[
                    ("field", items.iter().map(|(f, _)| f.as_bytes().to_vec()).collect()),
                    ("value", items.iter().map(|(_, v)| v.as_bytes().to_vec()).collect()),
                ])),
                _ => Ok(binary_batch(&[("field", vec![]), ("value", vec![])])),
            },
            "DBSIZE" => Ok(scalar(self.db_size(db).to_string())),
            "PING" => Ok(scalar(text("PONG"))),
            "TTL" => Ok(scalar(
                self.key(db, args.first().copied().unwrap_or(""))
                    .map_or(-2, |key| key.ttl.unwrap_or(-1))
                    .to_string(),
            )),
            "TYPE" => Ok(scalar(
                self.key(db, args.first().copied().unwrap_or(""))
                    .map_or("none".to_owned(), |key| key.value.key_type().as_str().to_owned()),
            )),
            "INFO" => Ok(binary_batch(&[
                ("field", ["redis_version", "uptime_in_days", "connected_clients", "used_memory_human", "role"].map(|f| f.as_bytes().to_vec()).to_vec()),
                ("value", ["7.4.1", "12", "6", "1.84M", "master"].map(|v| v.as_bytes().to_vec()).to_vec()),
            ])),
            "FLUSHDB" | "DEL" | "SET" | "EXPIRE" | "RENAME" | "HSET" | "LPUSH" | "RPUSH" | "SADD" | "ZADD" => {
                if !confirmed && !matches!(verb.as_str(), "SET" | "HSET" | "LPUSH" | "RPUSH" | "SADD" | "ZADD" | "EXPIRE") {
                    return Err(CommandRefusal::Confirm(format!("Redis command requires confirmation: {command}")));
                }
                self.confirmed_commands.push(command.to_owned());
                Ok(scalar(text("OK")))
            }
            _ if !crate::redis::is_read_only(command) && !confirmed => {
                Err(CommandRefusal::Confirm(format!("Redis command requires confirmation: {command}")))
            }
            _ => Ok(scalar(text("OK"))),
        }
    }

    // ---- edits ----------------------------------------------------------------

    /// Applies one key edit; the affected count the driver would report.
    pub fn apply(&mut self, db: u32, name: &str, edit: &Edit) -> Result<u64, String> {
        match edit {
            Edit::DeleteKey => {
                let before = self.keys.len();
                self.keys.retain(|key| !(key.db == db && key.name == name));
                return Ok((before - self.keys.len()) as u64);
            }
            Edit::Rename { new_key, overwrite } => {
                if self.key(db, new_key).is_some() && !overwrite {
                    return Ok(0);
                }
                self.keys.retain(|key| !(key.db == db && key.name == *new_key));
                let key = self
                    .keys
                    .iter_mut()
                    .find(|key| key.db == db && key.name == name)
                    .ok_or_else(|| "ERR no such key".to_owned())?;
                key.name = new_key.clone();
                return Ok(1);
            }
            _ => {}
        }
        let exists = self.key(db, name).is_some();
        if !exists && !matches!(edit, Edit::SetString { .. } | Edit::HashSet { .. } | Edit::ListPush { .. } | Edit::SetAdd { .. } | Edit::ZAdd { .. } | Edit::StreamAdd { .. }) {
            return Ok(0);
        }
        if !exists {
            let value = match edit {
                Edit::SetString { .. } => Value::Text(String::new()),
                Edit::HashSet { .. } => Value::Hash(Vec::new()),
                Edit::ListPush { .. } => Value::List(Vec::new()),
                Edit::SetAdd { .. } => Value::Set(Vec::new()),
                Edit::ZAdd { .. } => Value::ZSet(Vec::new()),
                _ => Value::Stream(Vec::new()),
            };
            self.keys.push(Key { db, name: name.to_owned(), ttl: None, value });
        }
        let key = self
            .keys
            .iter_mut()
            .find(|key| key.db == db && key.name == name)
            .ok_or_else(|| "ERR no such key".to_owned())?;
        let decode = |raw: &str| crate::redis::decode_key(raw).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
        let wrong = || "WRONGTYPE Operation against a key holding the wrong kind of value".to_owned();
        match (edit, &mut key.value) {
            (Edit::SetString { value }, Value::Text(current)) => *current = value.clone(),
            (Edit::SetTtl(ttl), _) => {
                key.ttl = match ttl {
                    Ttl::Persist => None,
                    Ttl::Seconds(seconds) => Some(*seconds as i64),
                }
            }
            (Edit::HashSet { field, value }, Value::Hash(items)) => match items.iter_mut().find(|(f, _)| f == field) {
                Some(entry) => entry.1 = value.clone(),
                None => items.push((field.clone(), value.clone())),
            },
            (Edit::HashDelete { field_raw }, Value::Hash(items)) => {
                let field = decode(field_raw);
                let before = items.len();
                items.retain(|(f, _)| *f != field);
                return Ok((before - items.len()) as u64);
            }
            (Edit::ListPush { value, left }, Value::List(items)) => {
                if *left {
                    items.insert(0, value.clone());
                } else {
                    items.push(value.clone());
                }
            }
            (Edit::ListSet { index, value }, Value::List(items)) => {
                let at = usize::try_from(*index).ok().filter(|at| *at < items.len()).ok_or("ERR index out of range")?;
                items[at] = value.clone();
            }
            (Edit::ListRemove { value_raw }, Value::List(items)) => {
                let value = decode(value_raw);
                return Ok(items.iter().position(|item| *item == value).map(|at| items.remove(at)).map_or(0, |_| 1));
            }
            (Edit::SetAdd { member }, Value::Set(items)) => {
                if items.contains(member) {
                    return Ok(0);
                }
                items.push(member.clone());
            }
            (Edit::SetRemove { member_raw }, Value::Set(items)) => {
                let member = decode(member_raw);
                let before = items.len();
                items.retain(|item| *item != member);
                return Ok((before - items.len()) as u64);
            }
            (Edit::ZAdd { member, score }, Value::ZSet(items)) => {
                match items.iter_mut().find(|(m, _)| m == member) {
                    Some(entry) => entry.1 = *score,
                    None => items.push((member.clone(), *score)),
                }
                items.sort_by(|a, b| a.1.total_cmp(&b.1));
            }
            (Edit::ZRemove { member_raw }, Value::ZSet(items)) => {
                let member = decode(member_raw);
                let before = items.len();
                items.retain(|(m, _)| *m != member);
                return Ok((before - items.len()) as u64);
            }
            (Edit::StreamAdd { fields }, Value::Stream(items)) => {
                let id = format!("18{:08}-0", items.len());
                items.push((id, fields.clone()));
            }
            (Edit::StreamDelete { entry_id }, Value::Stream(items)) => {
                let before = items.len();
                items.retain(|(id, _)| id != entry_id);
                return Ok((before - items.len()) as u64);
            }
            _ => return Err(wrong()),
        }
        Ok(1)
    }
}

fn key_node(key: &Key) -> NavigatorNode {
    let raw = encode_key(key.name.as_bytes());
    let key_type = key.value.key_type().as_str();
    NavigatorNode {
        id: format!("key:{}:{raw}", key.db),
        kind: "key".into(),
        label: key.name.clone(),
        subtitle: Some(key_type.into()),
        icon: Some(key_type.into()),
        has_children: false,
        payload: json!({
            "database": key.db.to_string(),
            "db": key.db,
            "key_raw": raw,
            "key_type": key_type,
            "ttl": key.ttl,
            "size": key.value.len(),
        }),
        actions: vec!["open_query".into()],
    }
}

fn schema(fields: Vec<Field>) -> Arc<Schema> {
    Arc::new(Schema::new(fields))
}

fn binary_array(values: Vec<Vec<u8>>) -> ArrayRef {
    let mut builder = BinaryBuilder::new();
    for value in values {
        builder.append_value(value);
    }
    Arc::new(builder.finish())
}

fn binary_batch(columns: &[(&str, Vec<Vec<u8>>)]) -> RecordBatch {
    let fields = columns.iter().map(|(name, _)| Field::new(*name, DataType::Binary, false)).collect();
    let arrays = columns.iter().map(|(_, values)| binary_array(values.clone())).collect();
    RecordBatch::try_new(schema(fields), arrays).expect("equal-length Binary columns")
}

/// Redis `MATCH` glob: `*`, `?`, `[abc]`, `\x`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(pattern: &[char], text: &[char]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some('*') => (0..=text.len()).any(|skip| go(&pattern[1..], &text[skip..])),
            Some('?') => !text.is_empty() && go(&pattern[1..], &text[1..]),
            Some('[') => {
                let Some(end) = pattern.iter().position(|c| *c == ']') else {
                    return false;
                };
                !text.is_empty() && pattern[1..end].contains(&text[0]) && go(&pattern[end + 1..], &text[1..])
            }
            Some('\\') if pattern.len() > 1 => {
                !text.is_empty() && text[0] == pattern[1] && go(&pattern[2..], &text[1..])
            }
            Some(c) => !text.is_empty() && text[0] == *c && go(&pattern[1..], &text[1..]),
        }
    }
    go(&pattern.chars().collect::<Vec<_>>(), &text.chars().collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_redis() {
        assert!(glob_match("user:*", "user:12"));
        assert!(glob_match("user:?", "user:1"));
        assert!(!glob_match("user:?", "user:12"));
        assert!(glob_match("h[ae]llo", "hallo") && !glob_match("h[ae]llo", "hillo"));
        assert!(glob_match("a\\*b", "a*b") && !glob_match("a\\*b", "axb"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn the_navigator_pages_filters_and_describes_keys() {
        let fixture = RedisFixture::sample();
        let root = fixture.navigator_page(None, None, None, 0).unwrap();
        assert_eq!(root.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["database:0", "database:1"]);
        let page = fixture.navigator_page(Some("database:0"), None, None, 50).unwrap();
        assert_eq!(page.nodes.len(), 50);
        let next = page.next_cursor.clone().expect("more keys");
        let rest = fixture.navigator_page(Some("database:0"), None, Some(&next), 500).unwrap();
        assert_eq!(page.nodes.len() + rest.nodes.len(), fixture.db_size(0));
        assert!(rest.next_cursor.is_none());
        let hashes = fixture
            .navigator_page(Some("database:0"), Some(&json!({"pattern": "user:*", "type": "hash"})), None, 500)
            .unwrap();
        assert_eq!(hashes.nodes.len(), 24);
        let none = fixture
            .navigator_page(Some("database:0"), Some(&json!({"pattern": "user:*", "type": "set"})), None, 500)
            .unwrap();
        assert!(none.nodes.is_empty());
        let info = fixture.key_info(0, "queue:emails").unwrap();
        assert_eq!((info.key_type(), info.length), (Some(KeyType::List), Some(40)));
    }

    #[test]
    fn values_page_and_edits_apply_like_the_driver() {
        let mut fixture = RedisFixture::sample();
        let (batch, total) = fixture.value_page(0, "queue:emails", 10, 5).unwrap();
        assert_eq!((batch.num_rows(), total), (5, 40));
        assert_eq!(fixture.apply(0, "queue:billing", &Edit::ListPush { value: "x".into(), left: false }), Ok(1));
        assert_eq!(fixture.value_page(0, "queue:billing", 0, 10).unwrap().1, 4);
        assert_eq!(fixture.apply(0, "tags:featured", &Edit::SetAdd { member: "red".into() }), Ok(0), "a duplicate member");
        assert_eq!(
            fixture.apply(0, "config:app", &Edit::Rename { new_key: "maintenance".into(), overwrite: false }),
            Ok(0)
        );
        assert_eq!(fixture.apply(0, "maintenance", &Edit::DeleteKey), Ok(1));
        assert_eq!(fixture.apply(0, "maintenance", &Edit::DeleteKey), Ok(0));
        assert!(fixture.apply(0, "counter:visits", &Edit::HashSet { field: "a".into(), value: "b".into() }).is_err());
        let refusal = fixture.command(0, "FLUSHDB", false).unwrap_err();
        assert!(matches!(refusal, CommandRefusal::Confirm(_)));
        assert!(matches!(fixture.command(0, "KEYS *", true), Err(CommandRefusal::Blocked(_))));
        assert!(fixture.command(0, "FLUSHDB", true).is_ok());
        assert_eq!(fixture.confirmed_commands, ["FLUSHDB"]);
    }
}
