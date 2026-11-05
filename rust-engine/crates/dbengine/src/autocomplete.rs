use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

lazy_static::lazy_static! {
    pub static ref SCHEMA_CACHE: RwLock<SchemaCache> = RwLock::new(SchemaCache::default());
}

#[derive(Default)]
pub struct SchemaCache {
    pub mongo_fields: HashMap<String, Vec<MongoFieldInfo>>,
    pub sql_tables: Vec<String>,
    pub sql_columns: HashMap<String, Vec<String>>,
    pub redis_commands: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MongoFieldInfo {
    pub name: String,
    pub data_type: Option<String>,
}

impl SchemaCache {
    pub fn set_mongo_fields(&mut self, collection: &str, fields: Vec<MongoFieldInfo>) {
        self.mongo_fields.insert(collection.to_string(), fields);
    }

    pub fn set_sql_schema(&mut self, tables: Vec<String>, columns: HashMap<String, Vec<String>>) {
        self.sql_tables = tables;
        self.sql_columns = columns;
    }

    pub fn init_redis_commands(&mut self) {
        self.redis_commands = vec![
            "GET",
            "SET",
            "DEL",
            "EXISTS",
            "TYPE",
            "KEYS",
            "SCAN",
            "TTL",
            "EXPIRE",
            "INCR",
            "DECR",
            "INCRBY",
            "DECRBY",
            "HGET",
            "HSET",
            "HDEL",
            "HGETALL",
            "HKEYS",
            "HVALS",
            "HEXISTS",
            "HLEN",
            "LPUSH",
            "RPUSH",
            "LPOP",
            "RPOP",
            "LRANGE",
            "LLEN",
            "LINDEX",
            "SADD",
            "SMEMBERS",
            "SREM",
            "SCARD",
            "SISMEMBER",
            "ZADD",
            "ZRANGE",
            "ZREM",
            "ZCARD",
            "ZSCORE",
            "PING",
            "INFO",
            "CONFIG",
            "DBSIZE",
            "FLUSHDB",
            "FLUSHALL",
            "PUBLISH",
            "SUBSCRIBE",
            "UNSUBSCRIBE",
            "MULTI",
            "EXEC",
            "DISCARD",
            "WATCH",
            "UNWATCH",
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub kind: String,
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct CompletionResponse {
    pub items: Vec<CompletionItem>,
    pub trigger_range: (usize, usize),
}

const MONGO_OPERATORS: &[&str] = &[
    "$eq",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$ne",
    "$in",
    "$nin",
    "$regex",
    "$options",
    "$exists",
    "$type",
    "$elemMatch",
    "$all",
    "$size",
    "$or",
    "$and",
    "$nor",
    "$not",
    "$expr",
    "$jsonSchema",
];

const SQL_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "TABLE",
    "INTO", "VALUES", "SET", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "ON", "GROUP", "BY",
    "ORDER", "ASC", "DESC", "LIMIT", "OFFSET", "HAVING", "DISTINCT", "AS", "AND", "OR", "NOT",
    "NULL", "IS", "IN", "BETWEEN", "LIKE", "COUNT", "SUM", "AVG", "MIN", "MAX", "EXISTS", "CASE",
    "WHEN", "THEN", "ELSE", "END",
];

pub fn get_completions(
    db_type: &str,
    query_text: &str,
    cursor_pos: usize,
    schema_json: &str,
) -> CompletionResponse {
    if query_text.len() < 2 {
        return CompletionResponse {
            items: vec![],
            trigger_range: (0, 0),
        };
    }

    let pos = clamp_to_char_boundary(query_text, cursor_pos);
    let text_before = &query_text[..pos];

    let (trigger_start, partial) = find_partial_token(text_before);
    let trigger_range = (trigger_start, pos);

    let char_before_cursor = if pos > 0 {
        text_before.chars().last()
    } else {
        None
    };

    let should_show = should_show_completions(text_before, char_before_cursor, &partial);
    if !should_show {
        return CompletionResponse {
            items: vec![],
            trigger_range,
        };
    }

    if db_type == "mongodb" {
        if let Ok(fields) = serde_json::from_str::<HashMap<String, String>>(schema_json) {
            if !fields.is_empty() {
                if let Ok(mut cache) = SCHEMA_CACHE.write() {
                    cache.set_mongo_fields("__request__", mongo_fields_from_type_map(fields));
                }
            }
        }
    }

    match db_type {
        "mongodb" => mongodb_completions(&partial, trigger_range),
        "sql" => sql_completions(&partial, trigger_range),
        "redis" => redis_completions(&partial, trigger_range),
        _ => CompletionResponse {
            items: vec![],
            trigger_range,
        },
    }
}

fn should_show_completions(text_before: &str, char_before: Option<char>, partial: &str) -> bool {
    if partial.starts_with('$') {
        return true;
    }

    if let Some(c) = char_before {
        if c == '{' || c == ',' {
            return true;
        }
        if c == ':' {
            let trimmed = text_before.trim_end();
            if trimmed.ends_with(':') {
                return false;
            }
        }
    }

    if partial.is_empty() {
        return false;
    }

    true
}

fn find_partial_token(text: &str) -> (usize, String) {
    let mut start = 0;
    for (idx, c) in text.char_indices().rev() {
        if c.is_whitespace()
            || c == '{'
            || c == '}'
            || c == '['
            || c == ']'
            || c == ','
            || c == ':'
            || c == '('
            || c == ')'
        {
            start = idx + c.len_utf8();
            break;
        }
    }

    (start, text[start..].to_string())
}

fn clamp_to_char_boundary(text: &str, cursor_pos: usize) -> usize {
    let mut pos = cursor_pos.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn mongodb_completions(partial: &str, trigger_range: (usize, usize)) -> CompletionResponse {
    let mut items: Vec<CompletionItem> = Vec::new();

    let cache = SCHEMA_CACHE.read().unwrap();

    if partial.starts_with('$') {
        for &op in MONGO_OPERATORS {
            if op.starts_with(partial) {
                items.push(CompletionItem {
                    label: op.to_string(),
                    kind: "operator".to_string(),
                    detail: Some("MongoDB operator".to_string()),
                });
            }
        }
    } else if !partial.is_empty() {
        for (_, fields) in cache.mongo_fields.iter() {
            for field in fields {
                if field.name.starts_with(partial) && !items.iter().any(|i| i.label == field.name) {
                    items.push(CompletionItem {
                        label: field.name.clone(),
                        kind: "field".to_string(),
                        detail: field
                            .data_type
                            .clone()
                            .or_else(|| Some("Document field".to_string())),
                    });
                }
            }
        }
    } else {
        for (_, fields) in cache.mongo_fields.iter() {
            for field in fields {
                if !items.iter().any(|i| i.label == field.name) {
                    items.push(CompletionItem {
                        label: field.name.clone(),
                        kind: "field".to_string(),
                        detail: field
                            .data_type
                            .clone()
                            .or_else(|| Some("Document field".to_string())),
                    });
                }
            }
        }
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    if items.len() > 50 {
        items.truncate(50);
    }

    CompletionResponse {
        items,
        trigger_range,
    }
}

fn sql_completions(partial: &str, trigger_range: (usize, usize)) -> CompletionResponse {
    let mut items: Vec<CompletionItem> = Vec::new();
    let cache = SCHEMA_CACHE.read().unwrap();

    let upper = partial.to_uppercase();

    if !partial.is_empty() {
        for &kw in SQL_KEYWORDS {
            if kw.starts_with(&upper) {
                items.push(CompletionItem {
                    label: kw.to_string(),
                    kind: "keyword".to_string(),
                    detail: Some("SQL keyword".to_string()),
                });
            }
        }

        for table in &cache.sql_tables {
            if table.starts_with(partial) {
                items.push(CompletionItem {
                    label: table.clone(),
                    kind: "table".to_string(),
                    detail: Some("Table name".to_string()),
                });
            }
        }

        for (_, columns) in cache.sql_columns.iter() {
            for col in columns {
                if col.starts_with(partial) && !items.iter().any(|i| i.label == *col) {
                    items.push(CompletionItem {
                        label: col.clone(),
                        kind: "column".to_string(),
                        detail: Some("Column name".to_string()),
                    });
                }
            }
        }
    } else {
        for &kw in SQL_KEYWORDS {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: "keyword".to_string(),
                detail: Some("SQL keyword".to_string()),
            });
        }
        for table in &cache.sql_tables {
            items.push(CompletionItem {
                label: table.clone(),
                kind: "table".to_string(),
                detail: Some("Table name".to_string()),
            });
        }
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    if items.len() > 50 {
        items.truncate(50);
    }

    CompletionResponse {
        items,
        trigger_range,
    }
}

fn redis_completions(partial: &str, trigger_range: (usize, usize)) -> CompletionResponse {
    let mut items: Vec<CompletionItem> = Vec::new();
    let cache = SCHEMA_CACHE.read().unwrap();

    let upper = partial.to_uppercase();

    for cmd in &cache.redis_commands {
        if cmd.starts_with(&upper) {
            items.push(CompletionItem {
                label: cmd.clone(),
                kind: "command".to_string(),
                detail: Some("Redis command".to_string()),
            });
        }
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    if items.len() > 50 {
        items.truncate(50);
    }

    CompletionResponse {
        items,
        trigger_range,
    }
}

pub fn update_mongo_schema(collection: &str, fields: Vec<MongoFieldInfo>) {
    let mut cache = SCHEMA_CACHE.write().unwrap();
    cache.set_mongo_fields(collection, fields);
}

pub fn mongo_fields_from_names(fields: Vec<String>) -> Vec<MongoFieldInfo> {
    fields
        .into_iter()
        .map(|name| MongoFieldInfo {
            name,
            data_type: None,
        })
        .collect()
}

pub fn mongo_fields_from_type_map(fields: HashMap<String, String>) -> Vec<MongoFieldInfo> {
    fields
        .into_iter()
        .map(|(name, data_type)| MongoFieldInfo {
            name,
            data_type: Some(data_type),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mongo_field_completion() {
        let mut cache = SCHEMA_CACHE.write().unwrap();
        cache.set_mongo_fields(
            "users",
            mongo_fields_from_type_map(HashMap::from([
                ("_id".to_string(), "objectId".to_string()),
                ("name".to_string(), "string".to_string()),
                ("email".to_string(), "string".to_string()),
            ])),
        );
        drop(cache);

        // cursor at 5 = after '{name', inside the key token → should suggest field names
        let resp = get_completions("mongodb", "{name: 'test'}", 5, "{}");
        assert!(
            !resp.items.is_empty(),
            "should suggest mongo fields when cursor is inside a key token"
        );
        assert!(
            resp.items.iter().any(|i| i.label == "name"),
            "field 'name' should be in completions"
        );
        assert!(
            resp.items
                .iter()
                .any(|i| i.label == "name" && i.detail.as_deref() == Some("string")),
            "field 'name' should include schema type detail"
        );
    }

    #[test]
    fn test_mongo_operator_completion() {
        let resp = get_completions("mongodb", "{$gt: 25}", 4, "{}");
        let has_gt = resp.items.iter().any(|i| i.label == "$gt");
        assert!(has_gt);
    }

    #[test]
    fn test_unicode_cursor_does_not_panic() {
        let resp = get_completions("mongodb", "{mẻ}", 3, "{}");
        assert!(resp.trigger_range.0 <= resp.trigger_range.1);

        let byte_cursor = "{mẻ".len();
        let resp = get_completions("mongodb", "{mẻ}", byte_cursor, "{}");
        assert!(resp.trigger_range.0 <= resp.trigger_range.1);
    }

    #[test]
    fn test_sql_keyword_completion() {
        let resp = get_completions("sql", "SEL", 3, "{}");
        let has_select = resp.items.iter().any(|i| i.label == "SELECT");
        assert!(has_select);
    }

    #[test]
    fn test_redis_command_completion() {
        let mut cache = SCHEMA_CACHE.write().unwrap();
        cache.init_redis_commands();
        drop(cache);

        let resp = get_completions("redis", "HGE", 3, "{}");
        let has_hget = resp.items.iter().any(|i| i.label == "HGET");
        assert!(has_hget);
    }
}
