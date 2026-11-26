//! MCP resources: the read-only views of the connection list and of one connection's schema, for
//! clients that offer resources to the model instead of (or as well as) tool calls.
//!
//! | URI | what | the tool it is |
//! |---|---|---|
//! | `tablex://connections` | the saved connections with their access | `list_connections` |
//! | `tablex://<connection_id>/schema` | the schema digest of the connection's only database (or `?database=` and `&schema=`) | `get_schema_context` |
//! | `tablex://<connection_id>/<database>/tables` | the tables and views of a database (`?schema=`, `?cursor=`) | `list_tables` |
//!
//! A resource decides nothing: reading one is that tool, called through the same layer, so the
//! access of the connection, the user's tool allowlist, the connection's allowed databases and
//! every limit apply exactly as for the tool. A read opens the connection's session when none is
//! open, as the tools need one.

use engine_agent::{CancelToken, ToolLayer, ToolOutcome};
use rmcp::model::{Resource, ResourceTemplate};
use serde_json::{json, Value};

pub const SCHEME: &str = "tablex://";
pub const CONNECTIONS_URI: &str = "tablex://connections";
const JSON: &str = "application/json";
const TEXT: &str = "text/plain";

/// What a resource read returns, or why it cannot.
pub struct Contents {
    pub uri: String,
    pub mime_type: &'static str,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Not a resource of this server.
    NotFound(String),
    /// The tool the resource stands for failed: its error, as the tool gives it.
    Failed(String),
}

/// The fixed resources: the connection list, and the schema of every connection the agent may
/// look at.
pub fn list(layer: &ToolLayer, owner: &str) -> Vec<Resource> {
    let mut resources = vec![Resource::new(CONNECTIONS_URI, "connections")
        .with_title("Saved connections")
        .with_description("The saved connections with the access the user gave the agent on each.")
        .with_mime_type(JSON)];
    if !layer.tool_allowed("get_schema_context") {
        return resources;
    }
    let outcome = layer.call_as(owner, "list_connections", json!({}), &CancelToken::new());
    if let ToolOutcome::Success(value) = outcome {
        for connection in value["connections"].as_array().into_iter().flatten() {
            let (Some(id), Some(name)) = (connection["id"].as_str(), connection["name"].as_str()) else {
                continue;
            };
            // `off` is listed by name only: nothing to read there.
            if connection["access"].as_str().is_none_or(|access| access == "off") {
                continue;
            }
            resources.push(
                Resource::new(format!("{SCHEME}{}/schema", encode(id)), format!("{name} schema"))
                    .with_title(format!("Schema of {name}"))
                    .with_description("A compact digest of the tables, columns and keys.")
                    .with_mime_type(TEXT),
            );
        }
    }
    resources
}

pub fn templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new("tablex://{connection_id}/schema{?database,schema}", "schema")
            .with_description("The schema digest of a connection (the database is needed when it has several).")
            .with_mime_type(TEXT),
        ResourceTemplate::new("tablex://{connection_id}/{database}/tables{?schema,cursor}", "tables")
            .with_description("The tables and views of a database.")
            .with_mime_type(JSON),
    ]
}

/// Reads the resource `uri`.
pub fn read(layer: &ToolLayer, owner: &str, uri: &str, cancel: &CancelToken) -> Result<Contents, ReadError> {
    let not_found = || ReadError::NotFound(format!("no resource `{}`", uri.chars().take(200).collect::<String>()));
    let rest = uri.strip_prefix(SCHEME).ok_or_else(not_found)?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let params = parse_query(query);
    let parts: Vec<&str> = path.split('/').collect();
    let run = |tool: &str, arguments: Value| -> Result<ToolOutcome, ReadError> {
        Ok(layer.call_as(owner, tool, arguments, cancel))
    };
    match parts.as_slice() {
        ["connections"] => {
            let outcome = run("list_connections", json!({}))?;
            finish(uri, JSON, &outcome, false)
        }
        [id, "schema"] => {
            let id = segment(id).ok_or_else(not_found)?;
            open(layer, owner, &id, cancel)?;
            let mut arguments = json!({"connection_id": id});
            for key in ["database", "schema"] {
                if let Some(value) = param(&params, key) {
                    arguments[key] = json!(value);
                }
            }
            let outcome = run("get_schema_context", arguments)?;
            finish(uri, TEXT, &outcome, true)
        }
        [id, database, "tables"] => {
            let (id, database) = (segment(id).ok_or_else(not_found)?, segment(database).ok_or_else(not_found)?);
            open(layer, owner, &id, cancel)?;
            let mut arguments = json!({"connection_id": id, "database": database});
            for key in ["schema", "cursor"] {
                if let Some(value) = param(&params, key) {
                    arguments[key] = json!(value);
                }
            }
            let outcome = run("list_tables", arguments)?;
            finish(uri, JSON, &outcome, false)
        }
        _ => Err(not_found()),
    }
}

/// The session the tools need: opened when it is not (the call is `open_session`, which needs
/// `schema_only` and does nothing when one is open).
fn open(layer: &ToolLayer, owner: &str, id: &str, cancel: &CancelToken) -> Result<(), ReadError> {
    match layer.call_as(owner, "open_session", json!({"connection_id": id}), cancel) {
        ToolOutcome::Success(_) => Ok(()),
        failure => Err(ReadError::Failed(failure.to_text())),
    }
}

fn finish(uri: &str, mime_type: &'static str, outcome: &ToolOutcome, text_field: bool) -> Result<Contents, ReadError> {
    match outcome {
        ToolOutcome::Success(value) => {
            let text = if text_field {
                value["text"].as_str().map_or_else(|| value.to_string(), str::to_owned)
            } else {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            };
            Ok(Contents {
                uri: uri.to_owned(),
                mime_type,
                text,
            })
        }
        failure => Err(ReadError::Failed(failure.to_text())),
    }
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode(key)?, decode(value)?))
        })
        .collect()
}

/// Percent-encodes everything but unreserved characters.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The text a percent-encoded `text` stands for; `None` when it is not valid.
pub fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = text.get(at + 1..at + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A path segment: decoded, and not empty.
fn segment(text: &str) -> Option<String> {
    decode(text).filter(|decoded| !decoded.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_through_a_uri() {
        for name in ["shop", "my db", "a/b", "ünï", "100%", "x?y=z&w"] {
            assert_eq!(decode(&encode(name)).as_deref(), Some(name), "{name}");
        }
        assert_eq!(decode("%"), None);
        assert_eq!(decode("%zz"), None);
        assert_eq!(decode("%FF"), None);
        assert_eq!(decode("").as_deref(), Some(""));
        assert_eq!(segment(""), None);
    }

    #[test]
    fn a_query_is_read_into_pairs() {
        let pairs = parse_query("database=my%20db&schema=public&flag");
        assert_eq!(param(&pairs, "database"), Some("my db"));
        assert_eq!(param(&pairs, "schema"), Some("public"));
        assert_eq!(param(&pairs, "flag"), None, "an empty value is no value");
        assert!(parse_query("").is_empty());
    }
}
