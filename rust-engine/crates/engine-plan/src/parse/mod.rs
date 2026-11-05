//! One module per output family.

pub(crate) mod clickhouse;
pub(crate) mod mysql;
pub(crate) mod mysql_tree;
pub(crate) mod postgres;
pub(crate) mod sqlite;

use serde_json::Value;

use crate::ExplainError;

/// Parse JSON text; the parser's own nesting limit (128) turns hostile
/// nesting into an error instead of a stack overflow.
pub(crate) fn json(raw: &str) -> Result<Value, ExplainError> {
    serde_json::from_str(raw.trim()).map_err(|e| ExplainError::InvalidJson(e.to_string()))
}
