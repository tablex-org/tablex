//! Typed requests and results of the tools (nineteen, three transaction tools, two tools that
//! show things in the app and three that manage the saved connections, which a front end may
//! switch on).
//!
//! Requests reject unknown fields. Their JSON Schemas (see [`crate::schema`])
//! are the hand-written twins of these structs; a test keeps them in step.

use crate::access::AgentAccess;
use crate::context::SchemaContext;
use crate::ports::{ConnectionKind, Language};
use crate::result::{QueryResult, Truncation};
use crate::table::TableDescription;
use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
    ExecuteBatch,
    ExplainQuery,
    ExecuteRedisCommand,
    MongoFind,
    MongoAggregate,
    MongoCount,
    MongoInsert,
    MongoUpdate,
    MongoDelete,
    ListRoutines,
    GetRoutineSource,
    BeginTransaction,
    CommitTransaction,
    RollbackTransaction,
    OpenTable,
    ExecuteAndShow,
    AddConnection,
    DuplicateConnection,
    RemoveConnection,
}

impl ToolName {
    pub const ALL: [Self; 27] = [
        Self::ListConnections,
        Self::OpenSession,
        Self::CloseSession,
        Self::ListNamespaces,
        Self::ListTables,
        Self::DescribeTable,
        Self::GetSchemaContext,
        Self::ExecuteQuery,
        Self::ExecuteBatch,
        Self::ExplainQuery,
        Self::ExecuteRedisCommand,
        Self::MongoFind,
        Self::MongoAggregate,
        Self::MongoCount,
        Self::MongoInsert,
        Self::MongoUpdate,
        Self::MongoDelete,
        Self::ListRoutines,
        Self::GetRoutineSource,
        Self::BeginTransaction,
        Self::CommitTransaction,
        Self::RollbackTransaction,
        Self::OpenTable,
        Self::ExecuteAndShow,
        Self::AddConnection,
        Self::DuplicateConnection,
        Self::RemoveConnection,
    ];

    /// The tools only a layer built with `with_transactions` offers.
    pub const fn is_transaction_tool(self) -> bool {
        matches!(
            self,
            Self::BeginTransaction | Self::CommitTransaction | Self::RollbackTransaction
        )
    }

    /// The tools only a layer built with `with_app` offers (they act on the running TableX app).
    pub const fn is_app_tool(self) -> bool {
        matches!(self, Self::OpenTable | Self::ExecuteAndShow)
    }

    /// The tools only a layer built with `with_connection_admin` offers (they change the user's
    /// saved connections, each change put to the user).
    pub const fn is_admin_tool(self) -> bool {
        matches!(
            self,
            Self::AddConnection | Self::DuplicateConnection | Self::RemoveConnection
        )
    }

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
            Self::ExecuteBatch => "execute_batch",
            Self::ExplainQuery => "explain_query",
            Self::ExecuteRedisCommand => "execute_redis_command",
            Self::MongoFind => "mongo_find",
            Self::MongoAggregate => "mongo_aggregate",
            Self::MongoCount => "mongo_count",
            Self::MongoInsert => "mongo_insert",
            Self::MongoUpdate => "mongo_update",
            Self::MongoDelete => "mongo_delete",
            Self::ListRoutines => "list_routines",
            Self::GetRoutineSource => "get_routine_source",
            Self::BeginTransaction => "begin_transaction",
            Self::CommitTransaction => "commit_transaction",
            Self::RollbackTransaction => "rollback_transaction",
            Self::OpenTable => "open_table",
            Self::ExecuteAndShow => "execute_and_show",
            Self::AddConnection => "add_connection",
            Self::DuplicateConnection => "duplicate_connection",
            Self::RemoveConnection => "remove_connection",
        }
    }

    /// The MongoDB tools that change data (always put to the user).
    pub const fn is_mongo_write(self) -> bool {
        matches!(
            self,
            Self::MongoInsert | Self::MongoUpdate | Self::MongoDelete
        )
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == name)
    }

    /// The access a connection needs before this tool may touch it.
    pub const fn required_access(self) -> AgentAccess {
        match self {
            // The three connection tools name no session: they need no access to the connection
            // (a removal or a copy is the user's decision, put to them each time).
            Self::ListConnections
            | Self::CloseSession
            | Self::AddConnection
            | Self::DuplicateConnection
            | Self::RemoveConnection => AgentAccess::Off,
            Self::OpenSession
            | Self::ListNamespaces
            | Self::ListTables
            | Self::DescribeTable
            | Self::GetSchemaContext
            | Self::ListRoutines
            | Self::GetRoutineSource
            | Self::OpenTable => AgentAccess::SchemaOnly,
            // A script is judged statement by statement: a write in it needs `read_write`.
            Self::ExecuteQuery
            | Self::ExecuteBatch
            | Self::ExplainQuery
            | Self::ExecuteRedisCommand
            | Self::MongoFind
            | Self::MongoAggregate
            | Self::MongoCount
            | Self::ExecuteAndShow => AgentAccess::Read,
            Self::MongoInsert | Self::MongoUpdate | Self::MongoDelete | Self::BeginTransaction => {
                AgentAccess::ReadWrite
            }
            // They name a transaction, not a connection: the layer checks the transaction's
            // connection itself (commit needs `read_write` again; rollback is always allowed).
            Self::CommitTransaction | Self::RollbackTransaction => AgentAccess::Off,
        }
    }
}

/// The kinds of schema object `list_routines` and `get_routine_source` cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineKind {
    Function,
    Procedure,
    Trigger,
    Event,
}

impl RoutineKind {
    pub const fn object_kind(self) -> SchemaObjectKind {
        match self {
            Self::Function => SchemaObjectKind::Function,
            Self::Procedure => SchemaObjectKind::Procedure,
            Self::Trigger => SchemaObjectKind::Trigger,
            Self::Event => SchemaObjectKind::Event,
        }
    }

    pub const fn from_object_kind(kind: SchemaObjectKind) -> Option<Self> {
        match kind {
            SchemaObjectKind::Function => Some(Self::Function),
            SchemaObjectKind::Procedure => Some(Self::Procedure),
            SchemaObjectKind::Trigger => Some(Self::Trigger),
            SchemaObjectKind::Event => Some(Self::Event),
            SchemaObjectKind::MaterializedView
            | SchemaObjectKind::Sequence
            | SchemaObjectKind::Type => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListConnectionsRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionRequest {
    /// For `open_session`, `close_session`, `begin_transaction` and `remove_connection`.
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
    /// Run the statement inside this open transaction (see `begin_transaction`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
}

/// `execute_batch`: several SQL statements in one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteBatchRequest {
    pub connection_id: String,
    /// The statements, separated by `;`.
    pub script: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Rows to return per statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_limit: Option<usize>,
    /// Go on after a statement that fails (default: stop there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
    /// Run the whole script in a transaction of its own: committed if every statement succeeds,
    /// rolled back if one fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_transaction: Option<bool>,
    /// Run the script inside this open transaction (see `begin_transaction`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
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

/// `mongo_find`. `filter`, `projection` and `sort` are MongoDB documents in relaxed or canonical
/// Extended JSON; their key order is kept (a sort is its keys in order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MongoFindRequest {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// `mongo_aggregate`: `pipeline` is the stage documents in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MongoAggregateRequest {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    pub pipeline: Vec<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MongoCountRequest {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Map<String, Value>>,
}

/// `mongo_insert`: `documents` are inserted in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MongoInsertRequest {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    pub documents: Vec<Map<String, Value>>,
}

/// `mongo_update`: the first of `update` and `pipeline` that is given, and only one of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MongoUpdateRequest {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    pub filter: Map<String, Value>,
    /// Update operators, e.g. `{"$set": {"status": "paid"}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Map<String, Value>>,
    /// An update pipeline (stages such as `$set`), instead of `update`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<Vec<Map<String, Value>>>,
    /// Update every match, not only the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub many: Option<bool>,
    /// Insert a document when nothing matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upsert: Option<bool>,
    /// Allow an empty `filter` (every document); the user is asked about it as a dangerous change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MongoDeleteRequest {
    pub connection_id: String,
    pub database: String,
    pub collection: String,
    pub filter: Map<String, Value>,
    /// Delete every match, not only the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub many: Option<bool>,
    /// Allow an empty `filter` (every document); the user is asked about it as a dangerous change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRoutinesRequest {
    pub connection_id: String,
    pub kind: RoutineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Case-insensitive substring of the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Names the object either by the `ref` `list_routines` returned, or by `kind` and `name` (with
/// the rest as needed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRoutineSourceRequest {
    pub connection_id: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<SchemaObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RoutineKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// PostgreSQL overloads: the input argument types, e.g. `integer, text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    /// The table a trigger belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
}

/// For `commit_transaction` and `rollback_transaction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionRequest {
    pub transaction_id: String,
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
    /// `sql`, `redis`, `mongo` or `other` (browse only).
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineItem {
    pub name: String,
    pub kind: RoutineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    /// Pass to `get_routine_source`.
    #[serde(rename = "ref")]
    pub reference: SchemaObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRoutinesResult {
    pub routines: Vec<RoutineItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetRoutineSourceResult {
    pub kind: RoutineKind,
    pub name: String,
    pub source: String,
    /// The source was cut to the byte limit.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeginTransactionResult {
    pub transaction_id: String,
    pub connection_id: String,
    /// The transaction is rolled back after this many seconds without a call on it.
    pub idle_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitTransactionResult {
    pub transaction_id: String,
    pub committed: bool,
    /// Write statements that were sent in the transaction.
    pub changes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackTransactionResult {
    pub transaction_id: String,
    /// `false`: the transaction was already gone (rolled back after the idle timeout or with its
    /// session, or already ended).
    pub rolled_back: bool,
    /// Write statements that were sent in the transaction (all undone).
    pub changes: usize,
}

/// Result of `mongo_find` and `mongo_aggregate`: the documents as relaxed Extended JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MongoDocumentsResult {
    pub documents: Vec<Value>,
    pub document_count: usize,
    /// More documents exist than were returned.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,
    /// Strings that were cut to the cell limit, and documents too large to show (each is replaced
    /// by `{"$truncated": "..."}`).
    pub cells_truncated: usize,
    /// The document limit that applied.
    pub limit: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MongoCountResult {
    pub count: u64,
    pub elapsed_ms: u64,
}

/// Result of `mongo_insert`, `mongo_update` and `mongo_delete`, from the server's reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MongoWriteResult {
    /// Documents inserted, matched (update) or deleted.
    pub n: u64,
    /// Documents an update changed; absent for insert and delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_modified: Option<u64>,
    /// Documents an update inserted (`upsert`): `{index, _id}` each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upserted: Vec<Value>,
    /// Writes the server refused (`index`, `code`, `errmsg`); the rest of the command may have
    /// been applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_errors: Vec<Value>,
    pub elapsed_ms: u64,
}

/// Why one statement of a batch did not run to the end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchError {
    pub code: String,
    pub message: String,
}

/// One statement of an `execute_batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchStatement {
    /// Position in the script, from 1.
    pub index: usize,
    /// The start of the statement.
    pub statement: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<QueryResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchError>,
}

/// How the transaction of an `execute_batch` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchTransaction {
    Committed,
    RolledBack,
}

/// Result of `execute_batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteBatchResult {
    /// Statements in the script.
    pub statement_count: usize,
    /// One entry for each statement that ran, in order (the last one is the one that failed, when
    /// one did).
    pub statements: Vec<BatchStatement>,
    /// Statements that were not run because an earlier one failed.
    pub skipped: usize,
    /// Every statement ran and none failed.
    pub ok: bool,
    /// Present when the script ran in a transaction of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<BatchTransaction>,
    /// Write statements that took effect (none when the transaction was rolled back).
    pub changes: usize,
    /// Rows changed by those statements; present when `changes` is above zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<u64>,
    pub elapsed_ms: u64,
}

pub type DescribeTableResult = TableDescription;
pub type GetSchemaContextResult = SchemaContext;
pub type StatementResult = QueryResult;

/// `open_table`: show a table, view or collection in the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTableRequest {
    pub connection_id: String,
    pub database: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub object: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTableResult {
    pub connection_id: String,
    pub object: String,
    /// TableX took the request up; the table itself may still be loading.
    pub requested: bool,
}

/// `execute_and_show`: a query in an editor tab of the app, run there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteAndShowRequest {
    pub connection_id: String,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Run it (default true); false only puts it in the editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteAndShowResult {
    pub connection_id: String,
    /// TableX took the request up and opened the tab.
    pub shown: bool,
    /// TableX was asked to run it (it may ask the user first, or refuse by its own rules).
    pub run: bool,
}

/// `add_connection`. The password is the one secret in any request: `Debug` leaves it out.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddConnectionRequest {
    pub name: String,
    pub kind: ConnectionKind,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<i64>,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// SQLite: the database file.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// The id or name of a connection type of the user's settings; default `development`.
    #[serde(default)]
    pub connection_type: Option<String>,
    #[serde(default)]
    pub options: Option<ConnectionOptions>,
}

impl std::fmt::Debug for AddConnectionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddConnectionRequest")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("path", &self.path)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .finish_non_exhaustive()
    }
}

/// The options of `add_connection`: what the New Connection dialog offers that needs no file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionOptions {
    /// `disable`, `require` or `verify_full`.
    #[serde(default)]
    pub tls: Option<String>,
}

/// `duplicate_connection`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuplicateConnectionRequest {
    pub connection_id: String,
    pub name: String,
}

/// What `add_connection`, `duplicate_connection` and `remove_connection` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionChangeResult {
    /// The new connection's id (`add_connection`, `duplicate_connection`) or the removed one's.
    pub connection_id: String,
    /// What agents may do on the new connection (`add_connection`, `duplicate_connection`): the
    /// default of its connection type and Settings -> AI, never more than the user chose there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_access: Option<AgentAccess>,
    /// The password was saved in the credential store for the new connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_saved: Option<bool>,
    /// The running TableX app was told to reload its connections. Not an error when it was not
    /// running: it reads them when it starts.
    pub app_notified: bool,
    /// Something that went wrong after the change was made (a secret that could not be cleared).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
