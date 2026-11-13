//! The traits a front end implements, and the plain types they exchange with
//! the tool layer. Nothing here knows about keychains, tunnels or a UI.

use crate::access::{AgentAccess, ConnectionTypeInfo};
use crate::cancel::CancelToken;
use crate::error::AgentError;
use crate::limits::Limits;
use crate::result::QueryResult;
use crate::secret::SecretText;
use crate::table::TableDescription;
use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef};
use engine_driver_kit::sql::SqlLexOptions;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// The database engine behind a connection (the driver id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionKind {
    PostgreSql,
    MySql,
    MariaDb,
    Sqlite,
    ClickHouse,
    MongoDb,
    Redis,
}

impl ConnectionKind {
    pub const fn driver_id(self) -> &'static str {
        match self {
            Self::PostgreSql => "postgresql",
            Self::MySql => "mysql",
            Self::MariaDb => "mariadb",
            Self::Sqlite => "sqlite",
            Self::ClickHouse => "clickhouse",
            Self::MongoDb => "mongodb",
            Self::Redis => "redis",
        }
    }

    pub fn from_driver_id(id: &str) -> Option<Self> {
        Some(match id {
            "postgresql" => Self::PostgreSql,
            "mysql" => Self::MySql,
            "mariadb" => Self::MariaDb,
            "sqlite" => Self::Sqlite,
            "clickhouse" => Self::ClickHouse,
            "mongodb" => Self::MongoDb,
            "redis" => Self::Redis,
            _ => return None,
        })
    }

    /// The statement language the agent tools speak for this engine.
    pub const fn language(self) -> Language {
        match self {
            Self::PostgreSql | Self::MySql | Self::MariaDb | Self::Sqlite | Self::ClickHouse => {
                Language::Sql
            }
            Self::Redis => Language::Redis,
            Self::MongoDb => Language::Other,
        }
    }

    /// Lexer settings of the dialect, for the statement classifier.
    pub const fn lex_options(self) -> Option<SqlLexOptions> {
        Some(match self {
            Self::PostgreSql => SqlLexOptions::postgres(),
            Self::MySql => SqlLexOptions::mysql(),
            Self::MariaDb => SqlLexOptions::mariadb(),
            Self::Sqlite => SqlLexOptions::sqlite(),
            Self::ClickHouse => SqlLexOptions::clickhouse(),
            Self::MongoDb | Self::Redis => return None,
        })
    }
}

/// What kind of text `execute_*` tools may send to a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Sql,
    Redis,
    /// Browse only (no execute tool): MongoDB.
    Other,
}

/// An opaque fingerprint of everything that decides where and how a connection goes (address,
/// kind, tunnel, type, overrides). The front end computes it (a hash, not reversible to the
/// address); the layer closes a session opened under another revision, so an edited connection is
/// never served by a session to the old target. `UNKNOWN` (the default) is never compared. It is
/// not serialized, not shown to the model and prints nothing in `Debug`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Revision(u64);

impl Revision {
    pub const UNKNOWN: Self = Self(0);

    /// `0` is reserved for "unknown".
    pub const fn new(value: u64) -> Self {
        Self(if value == 0 { 1 } else { value })
    }

    /// Both known and different.
    pub(crate) fn differs_from(self, other: Self) -> bool {
        self.0 != 0 && other.0 != 0 && self.0 != other.0
    }
}

impl std::fmt::Debug for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Revision(<opaque>)")
    }
}

/// Names one open of a session: the layer numbers its opens (`1`, `2`, ...) and hands the number to
/// [`SessionProvider::open_generation`] and, when the session ends, to
/// [`SessionProvider::close_generation`], so a provider can tell which of two sessions of one
/// connection a close is for: closing the older one never drops what the newer one built (a
/// tunnel). `UNTAGGED` is what a caller that does not number its opens uses. Opaque to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpenGeneration(u64);

impl OpenGeneration {
    pub const UNTAGGED: Self = Self(0);

    pub const fn new(number: u64) -> Self {
        Self(number)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A saved connection as the tool layer sees it. There is deliberately no
/// URI, password or key here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub id: String,
    pub name: String,
    pub kind: ConnectionKind,
    pub connection_type: ConnectionTypeInfo,
    /// The connection's own agent-access setting; `None` = never set.
    #[serde(default)]
    pub agent_access: Option<AgentAccess>,
    /// The connection's effective query timeout, when it has one.
    #[serde(default)]
    pub statement_timeout_secs: Option<u64>,
    /// See [`Revision`].
    #[serde(skip)]
    pub revision: Revision,
}

impl ConnectionInfo {
    pub fn effective_access(&self) -> AgentAccess {
        AgentAccess::effective(self.agent_access, &self.connection_type)
    }

    pub(crate) fn statement_timeout(&self, limits: &Limits) -> Duration {
        self.statement_timeout_secs
            .filter(|seconds| *seconds > 0)
            .map_or(limits.statement_timeout, Duration::from_secs)
    }
}

/// The saved connections. Read on every call, so a setting the user changes
/// takes effect on the next tool call.
pub trait ConnectionCatalog: Send + Sync {
    fn list(&self) -> Vec<ConnectionInfo>;

    fn get(&self, id: &str) -> Option<ConnectionInfo> {
        self.list()
            .into_iter()
            .find(|connection| connection.id == id)
    }

    /// The spelling of `id` that [`list`](Self::list) uses, for a catalog whose store spells one id
    /// in more than one way (upper-case UUIDs on disk, lower-case ones shown). The layer keys its
    /// sessions by this form and applies it to every id it is given (a tool call, `revoke`,
    /// `is_open`), so a caller may use either spelling. Default: the id as it is.
    fn canonical_id(&self, id: &str) -> String {
        id.to_owned()
    }
}

/// Opens driver sessions. The front end owns credentials, SSH tunnels, URI
/// rewriting and driver loading; the tool layer only asks for a session.
pub trait SessionProvider: Send + Sync {
    /// Connects to `connection`. May block (tunnel, TLS, authentication).
    fn open(&self, connection: &ConnectionInfo) -> Result<Arc<dyn Session>, AgentError>;

    /// [`open`](Self::open) for the open numbered `generation`. The layer calls this one. A
    /// provider that builds something around the session (a tunnel) keeps it under
    /// `(connection, generation)` so that [`close_generation`](Self::close_generation) can drop
    /// exactly that. Default: ignores the number and calls `open`.
    fn open_generation(
        &self,
        connection: &ConnectionInfo,
        generation: OpenGeneration,
    ) -> Result<Arc<dyn Session>, AgentError> {
        let _ = generation;
        self.open(connection)
    }

    /// Exact secret values (password, token, key passphrase) `open` will use for
    /// `connection`, known before it connects, so an error that echoes one is
    /// scrubbed. Default: none.
    fn secrets(&self, _connection: &ConnectionInfo) -> Vec<SecretText> {
        Vec::new()
    }

    /// Words that are NOT secret although they may equal one: the connection's user name and
    /// database name. A saved password that equals one of them (`postgres`) is not masked by value
    /// (it would turn `OWNER TO postgres` into `OWNER TO ***` and disclose the password by
    /// comparison); URI user-info and `password=` values are still masked by shape. The layer calls
    /// it right after [`secrets`](Self::secrets), so a provider may reuse what that call read.
    /// Default: none.
    fn plain_words(&self, _connection: &ConnectionInfo) -> Vec<String> {
        Vec::new()
    }

    /// Called after the session was disconnected, to tear down what the front
    /// end built around it (a tunnel). Drops whatever the provider holds for the connection.
    fn close(&self, _connection: &ConnectionInfo) {}

    /// [`close`](Self::close) for the session opened as `generation`: drops what that open built
    /// and nothing a later open of the same connection built. The layer calls this one. Default:
    /// ignores the number and calls `close`.
    fn close_generation(&self, connection: &ConnectionInfo, generation: OpenGeneration) {
        let _ = generation;
        self.close(connection);
    }

    /// Why a transaction cannot be run on a session of its own for `connection` (a SQLite
    /// database that lives in memory for one session only: a second session would not see it),
    /// or `None`. Asked before `begin_transaction` opens that second session. Default: `None`.
    fn transactions_unavailable(&self, _connection: &ConnectionInfo) -> Option<String> {
        None
    }
}

/// What [`Session::control`] sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxControl {
    Begin,
    Commit,
    Rollback,
}

impl TxControl {
    /// The statement the driver runs.
    pub const fn sql(self) -> &'static str {
        match self {
            Self::Begin => "BEGIN",
            Self::Commit => "COMMIT",
            Self::Rollback => "ROLLBACK",
        }
    }
}

/// What the layer needs to know about an open session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub kind: ConnectionKind,
    pub language: Language,
    /// Lexer settings for [`Language::Sql`].
    pub lex: Option<SqlLexOptions>,
    /// The navigator has a schema level between database and table.
    pub supports_schemas: bool,
    /// `describe_table` is backed by the driver's table-properties call (else
    /// by the column nodes of the navigator, without keys).
    pub rich_describe: bool,
}

/// Which level of the navigator to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeLevel {
    Databases,
    Schemas,
    Tables,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeQuery {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub filter: Option<String>,
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Node {
    pub name: String,
    /// `database`, `schema`, `table`, `view`, `materialized_view`, ...
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodePage {
    pub nodes: Vec<Node>,
    pub next_cursor: Option<String>,
}

/// A table-like object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
}

/// Which functions, procedures, triggers or events of a namespace to list. `database` and `schema`
/// are as for [`NodeQuery`]; `filter` is a case-insensitive substring of the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineQuery {
    pub kind: SchemaObjectKind,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub filter: Option<String>,
    pub cursor: Option<String>,
    pub limit: usize,
}

/// One page of objects of the queried kind, ordered by name, as the driver names them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutinePage {
    pub routines: Vec<SchemaObjectRef>,
    pub next_cursor: Option<String>,
}

/// One statement to run.
#[derive(Debug, Clone)]
pub struct RunRequest<'a> {
    pub language: Language,
    pub text: &'a str,
    pub database: Option<&'a str>,
    pub limits: &'a Limits,
    /// Rows to return (already clamped to the limits).
    pub row_limit: usize,
    /// The front end approved a dangerous statement; drivers that ask for
    /// confirmation get `confirmed`.
    pub confirmed: bool,
    /// The policy classified the statement as a read: run it read-only where the
    /// driver can (`QueryResult::read_only_enforced` says whether it did).
    pub read_only: bool,
}

/// An open driver session. Methods block; run them on a worker thread.
///
/// Implementations must stop promptly once `cancel` fires (the layer arms the
/// statement timeout on the same token) and return
/// [`AgentError::Cancelled`].
pub trait Session: Send + Sync {
    fn info(&self) -> SessionInfo;

    fn list_nodes(&self, level: NodeLevel, query: &NodeQuery) -> Result<NodePage, AgentError>;

    fn describe_table(&self, table: &TableRef) -> Result<TableDescription, AgentError>;

    /// Lists functions, procedures, triggers or events (`agent.unsupported` by default). A
    /// database without objects of that kind answers an empty page.
    fn list_routines(&self, _query: &RoutineQuery) -> Result<RoutinePage, AgentError> {
        Err(AgentError::Unsupported(
            "this session cannot list functions, procedures, triggers or events".into(),
        ))
    }

    /// The statement that creates `reference`, as the driver reports it (`agent.unsupported` by
    /// default; `describe.not_found` when the object is gone).
    fn routine_source(&self, _reference: &SchemaObjectRef) -> Result<String, AgentError> {
        Err(AgentError::Unsupported(
            "this session cannot read the source of functions, procedures, triggers or events".into(),
        ))
    }

    /// Runs one statement and collects at most `request.row_limit` rows within
    /// the byte limits (see [`crate::limits::RowCollector`]).
    fn run(
        &self,
        request: &RunRequest<'_>,
        cancel: &CancelToken,
    ) -> Result<QueryResult, AgentError>;

    /// Begins, commits or rolls back a transaction on this session (`agent.unsupported` by
    /// default). Internal to the tool layer: the text goes to the driver as it is, NOT through
    /// the agent policy, which refuses statements that change transaction state. Stops promptly
    /// once `cancel` fires, like [`run`](Self::run).
    fn control(&self, _action: TxControl, _cancel: &CancelToken) -> Result<(), AgentError> {
        Err(AgentError::Unsupported(
            "this session cannot run transactions".into(),
        ))
    }

    /// Closes the driver session. Idempotent.
    fn disconnect(&self);

    /// Exact secret values (passwords, tokens) this session was opened with,
    /// scrubbed from every error text and audit line.
    fn secrets(&self) -> Vec<SecretText> {
        Vec::new()
    }
}

/// A write the front end is asked to approve.
#[derive(Debug, Clone)]
pub struct WriteApproval<'a> {
    pub connection_id: &'a str,
    pub connection_name: &'a str,
    pub connection_type: &'a str,
    /// The connection's type is a production one (from the same type information the policy used).
    pub production: bool,
    /// What kind of text `statement` is.
    pub language: Language,
    /// The database the statement runs in (the request's, else the connection's
    /// only database when that is known); `None` when it is the connection's default.
    pub database: Option<&'a str>,
    /// The exact statement (SQL text or Redis command).
    pub statement: &'a str,
    /// Why the statement is dangerous, if it is.
    pub dangerous: Option<&'a str>,
    /// Set when the question is about a transaction (`execute_query` with a `transaction_id`, or
    /// the commit): the change is not final until the transaction is committed.
    pub transaction: Option<TransactionApproval<'a>>,
}

/// The transaction a [`WriteApproval`] belongs to.
#[derive(Debug, Clone, Copy)]
pub struct TransactionApproval<'a> {
    /// The id the model uses for the transaction.
    pub id: &'a str,
    /// `Some`: the question is the COMMIT itself (`statement` is `COMMIT`), and these are the
    /// write statements that ran in the transaction, each already approved when it ran.
    /// `None`: the statement is one more change inside the transaction, not committed yet.
    pub commit: Option<&'a [String]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    /// A person was asked and said no (`reason` is the front end's words for it).
    Deny {
        reason: Option<String>,
    },
    /// Nobody was asked: the front end cannot, or will not, put this write to a person (no way
    /// to ask, a connection that is not the one the chat is about). The audit line says so.
    Refuse {
        reason: String,
    },
    /// A person may have been asked, but no usable answer came back (the client failed, its reply
    /// was unreadable, the connection ended). Not a person's no, not "nobody was asked": audited
    /// as `unanswered: ...`, and not counted by a front end as a decline.
    Unanswered {
        reason: String,
    },
}

/// Asked before a write when the access is `read + write`. The in-app
/// assistant shows its confirm dialog; the MCP server uses elicitation or
/// refuses. Blocking is fine: it runs on the caller's worker thread, and the
/// token fires if the caller goes away while the question is open.
pub trait WriteApprover: Send + Sync {
    fn approve(&self, request: &WriteApproval<'_>, cancel: &CancelToken) -> ApprovalDecision;

    /// Conditions that must still hold once the answer is yes, checked by the layer right after it
    /// (with the connection re-check): `Err(reason)` stops the write, audited as `approved, then
    /// refused: <reason>`, and the model is told the change was approved but can no longer run.
    /// The MCP server uses it for its switch, which the user may turn off while a question waits.
    /// Default: nothing to check.
    fn still_allowed(&self, _request: &WriteApproval<'_>) -> Result<(), String> {
        Ok(())
    }

    /// `Err(reason)`: a write to this connection would be refused without asking right now (the
    /// switch is off, a production connection, no way to ask). Checked before a transaction is
    /// begun, so it is refused up front with the reason a write would get. Only the connection
    /// facts of `request` are meaningful. Default: nothing to check.
    fn check_writable(&self, _request: &WriteApproval<'_>) -> Result<(), String> {
        Ok(())
    }

    /// `Err(reason)`: the COMMIT question for a transaction that holds exactly these write
    /// statements could not be reviewed (too long to show). The layer asks before it puts the next
    /// write of a transaction to the user, so a transaction never ends up with changes that were
    /// each approved but cannot be committed. Default: always reviewable.
    fn commit_reviewable(&self, _statements: &[String]) -> Result<(), String> {
        Ok(())
    }

    /// `false`: dangerous statements are refused without asking (MCP mode).
    fn allows_dangerous(&self) -> bool {
        false
    }
}

/// The default approver: refuses every write.
#[derive(Debug, Clone, Copy, Default)]
pub struct RefuseWrites;

impl WriteApprover for RefuseWrites {
    fn check_writable(&self, _request: &WriteApproval<'_>) -> Result<(), String> {
        Err("this front end cannot ask the user to approve writes".into())
    }

    fn approve(&self, _request: &WriteApproval<'_>, _cancel: &CancelToken) -> ApprovalDecision {
        ApprovalDecision::Refuse {
            reason: "this front end cannot ask the user to approve writes".into(),
        }
    }
}
