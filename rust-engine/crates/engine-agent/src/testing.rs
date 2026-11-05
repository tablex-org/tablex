//! In-memory stand-ins for the front-end traits: a catalog, a provider and a
//! session that needs no driver. Used by this crate's tests and available to
//! the front ends' tests.

use crate::access::{AgentAccess, ConnectionTypeInfo};
use crate::cancel::CancelToken;
use crate::error::AgentError;
use crate::limits::RowCollector;
use crate::ports::{
    ApprovalDecision, ConnectionCatalog, ConnectionInfo, ConnectionKind, Node, NodeLevel, NodePage,
    NodeQuery, Revision, RunRequest, Session, SessionInfo, SessionProvider, TableRef,
    WriteApproval, WriteApprover,
};
use crate::result::{QueryResult, ResultColumn};
use crate::secret::SecretText;
use crate::table::TableDescription;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A connection with a plain type and the given access setting.
pub fn connection(id: &str, kind: ConnectionKind, access: Option<AgentAccess>) -> ConnectionInfo {
    ConnectionInfo {
        id: id.into(),
        name: format!("conn-{id}"),
        kind,
        connection_type: ConnectionTypeInfo {
            name: "Development".into(),
            ..ConnectionTypeInfo::default()
        },
        agent_access: access,
        statement_timeout_secs: None,
        revision: Revision::UNKNOWN,
    }
}

#[derive(Default)]
pub struct FakeCatalog {
    connections: Mutex<Vec<ConnectionInfo>>,
}

impl FakeCatalog {
    pub fn new(connections: Vec<ConnectionInfo>) -> Arc<Self> {
        Arc::new(Self {
            connections: Mutex::new(connections),
        })
    }

    /// Changes a connection the way the user edits its settings.
    pub fn update(&self, id: &str, change: impl FnOnce(&mut ConnectionInfo)) {
        if let Some(connection) = lock(&self.connections).iter_mut().find(|c| c.id == id) {
            change(connection);
        }
    }
}

impl ConnectionCatalog for FakeCatalog {
    fn list(&self) -> Vec<ConnectionInfo> {
        lock(&self.connections).clone()
    }
}

/// Hands out the sessions it was given, by connection id.
#[derive(Default)]
pub struct FakeProvider {
    sessions: Mutex<HashMap<String, Arc<FakeSession>>>,
    pub opened: AtomicUsize,
    pub closed: AtomicUsize,
    failure: Mutex<Option<AgentError>>,
    /// What `SessionProvider::secrets` reports before a connect.
    pub known_secrets: Mutex<Vec<SecretText>>,
}

impl FakeProvider {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn with_session(
        self: &Arc<Self>,
        connection_id: &str,
        session: Arc<FakeSession>,
    ) -> Arc<Self> {
        lock(&self.sessions).insert(connection_id.to_owned(), session);
        Arc::clone(self)
    }

    /// The next `open` fails with `error`.
    pub fn fail_open(&self, error: AgentError) {
        *lock(&self.failure) = Some(error);
    }
}

impl SessionProvider for FakeProvider {
    fn open(&self, connection: &ConnectionInfo) -> Result<Arc<dyn Session>, AgentError> {
        if let Some(error) = lock(&self.failure).take() {
            return Err(error);
        }
        self.opened.fetch_add(1, Ordering::SeqCst);
        let session = lock(&self.sessions)
            .get(&connection.id)
            .cloned()
            .ok_or_else(|| {
                AgentError::invalid("the fake provider has no session for this connection")
            })?;
        Ok(session)
    }

    fn secrets(&self, _connection: &ConnectionInfo) -> Vec<SecretText> {
        lock(&self.known_secrets).clone()
    }

    fn close(&self, _connection: &ConnectionInfo) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}

type RunScript =
    dyn Fn(&RunRequest<'_>, &CancelToken) -> Result<QueryResult, AgentError> + Send + Sync;

/// What the fake does when a statement runs.
pub enum Behavior {
    /// Returns one `n` column with rows `1..=count` (honouring the limits).
    Numbers { count: usize },
    /// Returns a write result with `affected_rows`.
    Affected(u64),
    /// Waits until the token is cancelled, then returns `Cancelled`.
    BlockUntilCancelled,
    /// Fails with this error.
    Fail(AgentError),
    /// One row with a single cell of `bytes` bytes.
    WideCell { bytes: usize },
    /// Anything else.
    Script(Box<RunScript>),
}

pub struct FakeSession {
    pub info: SessionInfo,
    /// The fake driver can run statements read-only.
    pub read_only_capable: bool,
    pub tables: Mutex<Vec<TableDescription>>,
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
    pub behavior: Mutex<Behavior>,
    /// Every statement that reached the session, in order.
    pub runs: Mutex<Vec<(String, bool)>>,
    /// The `read_only` flag of each of `runs`.
    pub read_only_runs: Mutex<Vec<bool>>,
    pub disconnects: AtomicUsize,
    pub secrets: Vec<SecretText>,
}

impl FakeSession {
    pub fn new(kind: ConnectionKind) -> Self {
        Self {
            read_only_capable: true,
            info: SessionInfo {
                kind,
                language: kind.language(),
                lex: kind.lex_options(),
                supports_schemas: false,
                rich_describe: true,
            },
            tables: Mutex::new(Vec::new()),
            databases: vec!["main".into()],
            schemas: Vec::new(),
            behavior: Mutex::new(Behavior::Numbers { count: 3 }),
            runs: Mutex::new(Vec::new()),
            read_only_runs: Mutex::new(Vec::new()),
            disconnects: AtomicUsize::new(0),
            secrets: Vec::new(),
        }
    }

    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    pub fn set_behavior(&self, behavior: Behavior) {
        *lock(&self.behavior) = behavior;
    }

    pub fn run_count(&self) -> usize {
        lock(&self.runs).len()
    }

    fn page_of<T>(items: Vec<T>, query: &NodeQuery) -> (Vec<T>, Option<String>) {
        let offset = query
            .cursor
            .as_deref()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let limit = query.limit.max(1);
        let total = items.len();
        let page: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
        let next = (offset + page.len() < total).then(|| (offset + page.len()).to_string());
        (page, next)
    }
}

impl Session for FakeSession {
    fn info(&self) -> SessionInfo {
        self.info.clone()
    }

    fn list_nodes(&self, level: NodeLevel, query: &NodeQuery) -> Result<NodePage, AgentError> {
        let names: Vec<(String, &str)> = match level {
            NodeLevel::Databases => self
                .databases
                .iter()
                .map(|n| (n.clone(), "database"))
                .collect(),
            NodeLevel::Schemas => self.schemas.iter().map(|n| (n.clone(), "schema")).collect(),
            NodeLevel::Tables => {
                let filter = query.filter.as_deref().map(str::to_lowercase);
                let mut tables: Vec<String> = lock(&self.tables)
                    .iter()
                    .filter(|t| t.schema == query.schema)
                    .filter(|t| {
                        filter
                            .as_ref()
                            .is_none_or(|f| t.name.to_lowercase().contains(f))
                    })
                    .map(|t| t.name.clone())
                    .collect();
                tables.sort();
                tables.into_iter().map(|n| (n, "table")).collect()
            }
        };
        let (page, next_cursor) = Self::page_of(names, query);
        Ok(NodePage {
            nodes: page
                .into_iter()
                .map(|(name, kind)| Node {
                    name,
                    kind: kind.into(),
                })
                .collect(),
            next_cursor,
        })
    }

    fn describe_table(&self, table: &TableRef) -> Result<TableDescription, AgentError> {
        lock(&self.tables)
            .iter()
            .find(|t| t.name == table.name && t.schema == table.schema)
            .cloned()
            .ok_or_else(|| {
                AgentError::driver(
                    "describe.not_found",
                    format!("no table named `{}`", table.name),
                    false,
                )
            })
    }

    fn run(
        &self,
        request: &RunRequest<'_>,
        cancel: &CancelToken,
    ) -> Result<QueryResult, AgentError> {
        lock(&self.runs).push((request.text.to_owned(), request.confirmed));
        lock(&self.read_only_runs).push(request.read_only);
        let enforced = |mut result: QueryResult| {
            result.read_only_enforced = request.read_only && self.read_only_capable;
            Ok(result)
        };
        let columns = vec![ResultColumn {
            name: "n".into(),
            data_type: "integer".into(),
        }];
        match &*lock(&self.behavior) {
            Behavior::Numbers { count } => {
                let mut collector = RowCollector::new(columns, request.row_limit, request.limits);
                for n in 1..=*count {
                    if collector.push_row([n.to_string()]).is_break() {
                        return enforced(collector.finish(true, None));
                    }
                }
                enforced(collector.finish(false, None))
            }
            Behavior::Affected(rows) => {
                Ok(RowCollector::new(Vec::new(), 0, request.limits).finish(false, Some(*rows)))
            }
            Behavior::BlockUntilCancelled => {
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(AgentError::Cancelled)
            }
            Behavior::Fail(error) => Err(error.clone()),
            Behavior::WideCell { bytes } => {
                let mut collector = RowCollector::new(columns, request.row_limit, request.limits);
                let _ = collector.push_row(["w".repeat(*bytes)]);
                enforced(collector.finish(false, None))
            }
            Behavior::Script(script) => script(request, cancel).and_then(enforced),
        }
    }

    fn disconnect(&self) {
        self.disconnects.fetch_add(1, Ordering::SeqCst);
    }

    fn secrets(&self) -> Vec<SecretText> {
        self.secrets.clone()
    }
}

/// An approver with a fixed answer that remembers what it was asked.
pub struct ScriptedApprover {
    pub decision: ApprovalDecision,
    pub allow_dangerous: bool,
    pub asked: Mutex<Vec<(String, Option<String>)>>,
    /// The database of each approval request.
    pub databases: Mutex<Vec<Option<String>>>,
}

impl ScriptedApprover {
    pub fn new(decision: ApprovalDecision, allow_dangerous: bool) -> Arc<Self> {
        Arc::new(Self {
            decision,
            allow_dangerous,
            asked: Mutex::new(Vec::new()),
            databases: Mutex::new(Vec::new()),
        })
    }
}

impl WriteApprover for ScriptedApprover {
    fn approve(&self, request: &WriteApproval<'_>, _cancel: &CancelToken) -> ApprovalDecision {
        lock(&self.databases).push(request.database.map(str::to_owned));
        lock(&self.asked).push((
            request.statement.to_owned(),
            request.dangerous.map(str::to_owned),
        ));
        self.decision.clone()
    }

    fn allows_dangerous(&self) -> bool {
        self.allow_dangerous
    }
}
