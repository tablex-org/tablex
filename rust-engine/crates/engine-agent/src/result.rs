//! The data a statement tool returns.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
}

/// Why a result holds fewer rows than the statement produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    /// The row limit was reached.
    RowLimit,
    /// The total byte cap was reached.
    ByteLimit,
}

/// Result of `execute_query`, `explain_query` and `execute_redis_command`.
///
/// Cells are text as the driver renders it (`NULL` for SQL NULL). A cell
/// longer than the cell limit is cut and ends with a `[truncated, N bytes in
/// total]` marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<String>>,
    pub row_count: usize,
    /// More rows exist than were returned.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,
    /// Cells that were cut.
    pub cells_truncated: usize,
    /// Rows changed by a write; absent for statements that return rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<u64>,
    /// The row limit that applied.
    pub row_limit: usize,
    /// The driver ran the statement read-only (a write would have failed in the
    /// database). `false` for writes, for Redis and for drivers without the
    /// `query.read_only` capability: the denylist and classifier were then the
    /// only guards.
    #[serde(default)]
    pub read_only_enforced: bool,
    pub elapsed_ms: u64,
}
