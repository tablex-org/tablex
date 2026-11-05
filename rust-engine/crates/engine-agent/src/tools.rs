//! Typed requests and results of the ten tools.
//!
//! Requests reject unknown fields. Their JSON Schemas (see [`crate::schema`])
//! are the hand-written twins of these structs; a test keeps them in step.

use crate::access::AgentAccess;
use crate::context::SchemaContext;
use crate::ports::{ConnectionKind, Language};
use crate::result::QueryResult;
use crate::table::TableDescription;
use serde::{Deserialize, Serialize};

/// The stable tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolName {
    ListConnections,
    OpenSession,
    CloseSession,
    ListNamespaces,
    ListTables,
    DescribeTable,
    GetSchemaContext,
    ExecuteQuery,
    ExplainQuery,
    ExecuteRedisCommand,
}

impl ToolName {
    pub const ALL: [Self; 10] = [
        Self::ListConnections,
        Self::OpenSession,
        Self::CloseSession,
        Self::ListNamespaces,
        Self::ListTables,
        Self::DescribeTable,
        Self::GetSchemaContext,
        Self::ExecuteQuery,
        Self::ExplainQuery,
        Self::ExecuteRedisCommand,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ListConnections => "list_connections",
            Self::OpenSession => "open_session",
            Self::CloseSession => "close_session",
            Self::ListNamespaces => "list_namespaces",
            Self::ListTables => "list_tables",
            Self::DescribeTable => "describe_table",
            Self::GetSchemaContext => "get_schema_context",
            Self::ExecuteQuery => "execute_query",
            Self::ExplainQuery => "explain_query",
            Self::ExecuteRedisCommand => "execute_redis_command",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == name)
    }

    /// The access a connection needs before this tool may touch it.
    pub const fn required_access(self) -> AgentAccess {
        match self {
            Self::ListConnections | Self::CloseSession => AgentAccess::Off,
            Self::OpenSession
            | Self::ListNamespaces
            | Self::ListTables
            | Self::DescribeTable
            | Self::GetSchemaContext => AgentAccess::SchemaOnly,
            Self::ExecuteQuery | Self::ExplainQuery | Self::ExecuteRedisCommand => {
                AgentAccess::Read
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListConnectionsRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionRequest {
    /// For `open_session` and `close_session`.
    pub connection_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListNamespacesRequest {
    pub connection_id: String,
    /// List the schemas of this database; omit to list databases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListTablesRequest {
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Case-insensitive substring of the table name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeTableRequest {
    pub connection_id: String,
    pub table: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetSchemaContextRequest {
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Size limit in the front end's estimator unit (about tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteQueryRequest {
    pub connection_id: String,
    /// Exactly one SQL statement.
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_limit: Option<usize>,
    /// Set to ask for a dangerous statement (DROP, TRUNCATE, DELETE or UPDATE
    /// without WHERE, ...) to be put to the user. Without it such statements
    /// are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerous: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplainQueryRequest {
    pub connection_id: String,
    /// The statement to explain, without `EXPLAIN`.
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteRedisCommandRequest {
    pub connection_id: String,
    /// One command line, e.g. `GET user:1`.
    pub command: String,
    /// Redis database index, e.g. `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerous: Option<bool>,
}

// ------------------------------------------------------------------ results

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    /// What the agent may do now (after the connection type's caps).
    pub access: AgentAccess,
    /// A session is open.
    pub open: bool,
    /// Absent for a connection with access `off`: it is listed by name only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ConnectionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub production: Option<bool>,
    /// The connection's own setting; absent when it was never set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_access: Option<AgentAccess>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListConnectionsResult {
    pub connections: Vec<ConnectionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenSessionResult {
    pub connection_id: String,
    pub kind: ConnectionKind,
    pub access: AgentAccess,
    /// `sql`, `redis` or `other` (browse only).
    pub language: Language,
    pub supports_schemas: bool,
    pub already_open: bool,
    pub statement_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseSessionResult {
    pub connection_id: String,
    /// A session was open and is now closed.
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameItem {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListNamespacesResult {
    pub namespaces: Vec<NameItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListTablesResult {
    pub tables: Vec<NameItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

pub type DescribeTableResult = TableDescription;
pub type GetSchemaContextResult = SchemaContext;
pub type StatementResult = QueryResult;
