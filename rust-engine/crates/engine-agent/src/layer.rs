//! [`ToolLayer`]: the single dispatch entry both front ends call.

use crate::access::AgentAccess;
use crate::audit::{AuditEntry, AuditLog};
use crate::cancel::{CancelToken, Watchdog};
use crate::classify::{classify_redis, classify_sql, first_keyword, Classified, StatementClass};
use crate::context::{
    build_schema_context, CharsPerThree, ContextInput, TokenEstimator, DEFAULT_BUDGET,
};
use crate::error::{self, AgentError, ToolError};
use crate::limits::Limits;
use crate::policy::{check_statement, Need};
use crate::ports::{
    ApprovalDecision, ConnectionCatalog, ConnectionInfo, Language, NodeLevel, NodeQuery,
    OpenGeneration, RefuseWrites, Revision, RunRequest, Session, SessionInfo, SessionProvider,
    TableRef, WriteApproval, WriteApprover,
};
use crate::redact::{redact, Masks};
use crate::result::QueryResult;
use crate::sanitize::{clean, clean_name, is_shapeless, name_form, MAX_NAME_CHARS};
use crate::schema::{self, ToolSpec};
use crate::secret::SecretText;
use crate::tools::*;
use engine_driver_kit::sql::Keyword;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    Success(Value),
    Failure(ToolError),
}

impl ToolOutcome {
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Failure(_))
    }

    /// The error code, for a failure.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Failure(error) => Some(&error.code),
            Self::Success(_) => None,
        }
    }

    /// The JSON to hand back to the model: the result, or
    /// `{"error": {"code", "message", "retryable"}}`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Success(value) => value.clone(),
            Self::Failure(error) => serde_json::json!({ "error": error }),
        }
    }

    pub fn to_text(&self) -> String {
        self.to_json().to_string()
    }
}

struct OpenSession {
    session: Arc<dyn Session>,
    info: SessionInfo,
    connection: ConnectionInfo,
    secrets: Vec<SecretText>,
    /// `secrets` prepared for masking, once per session (not once per text scrubbed).
    masks: Masks,
    /// The revision of the connection the session was opened for.
    revision: Revision,
    /// Which open of the connection this session is; closing it names it to the provider.
    generation: OpenGeneration,
    /// The token of the statement running now, so closing the session can cancel it.
    active: Mutex<Option<CancelToken>>,
    /// Set when the session starts closing: a statement that stored its token after the closer
    /// looked for one sees it and does not run.
    closing: AtomicBool,
    /// One running statement per session.
    busy: AtomicBool,
}

/// The statement running on a session: its token is where a closer finds it to cancel it.
struct ActiveStatement<'a>(&'a OpenSession);

impl<'a> ActiveStatement<'a> {
    /// Stores the token, then checks whether the session started closing before it could be found
    /// (the closer looks at the token after it set the flag, so one of the two always sees the other).
    fn begin(slot: &'a OpenSession, cancel: &CancelToken) -> Result<Self, AgentError> {
        *lock(&slot.active) = Some(cancel.clone());
        let active = Self(slot);
        if slot.closing.load(Ordering::SeqCst) {
            return Err(AgentError::NotConnected(
                "the session was closed before the statement started; call open_session again"
                    .into(),
            ));
        }
        Ok(active)
    }
}

impl Drop for ActiveStatement<'_> {
    fn drop(&mut self) {
        *lock(&self.0.active) = None;
    }
}

/// One operation per connection id at a time that changes whether the id has a session: an open, or
/// a close (disconnect + the provider's teardown). A second one waits (or gives up when its call is
/// cancelled) and then sees the state the first left.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    freed: Condvar,
}

#[derive(Default)]
struct GateState {
    held: bool,
    /// Callers inside `acquire` (waiting or about to take it).
    waiting: usize,
}

impl Gate {
    /// Waits for the gate. `false`: the call was cancelled first (a `None` token never is).
    fn acquire(self: &Arc<Self>, cancel: Option<&CancelToken>) -> bool {
        // Cancelling the call wakes the wait. The hook takes the state lock first, so the wake-up
        // cannot fall between the waiter's check and its wait.
        let _wake = cancel.map(|token| {
            let gate = Arc::clone(self);
            token.on_cancel(move || {
                drop(lock(&gate.state));
                gate.freed.notify_all();
            })
        });
        let mut state = lock(&self.state);
        state.waiting += 1;
        let acquired = loop {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                break false;
            }
            if !state.held {
                state.held = true;
                break true;
            }
            state = self
                .freed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        };
        state.waiting -= 1;
        acquired
    }

    fn release(&self) {
        lock(&self.state).held = false;
        // Everyone is woken, not one: the waiter that one wake-up picks may be leaving because its
        // call was cancelled, and it passes the gate on to nobody, so another waiter would sleep
        // with the gate free. Each waiter re-checks under the state lock and at most one takes it;
        // the rest sleep again (a few waiters per connection at most).
        self.freed.notify_all();
    }
}

/// Holds an id's [`Gate`]; releasing it (also when the holder panics) forgets the entry of an idle id.
struct GateGuard<'a> {
    layer: &'a ToolLayer,
    id: String,
    gate: Arc<Gate>,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.release();
        self.layer.forget_if_idle(&self.id, &self.gate);
    }
}

struct BusyGuard<'a>(&'a AtomicBool);

impl<'a> BusyGuard<'a> {
    fn acquire(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What a call learned that the audit line and the redaction need.
#[derive(Default)]
struct Trace {
    connection_id: Option<String>,
    statement: Option<String>,
    rows: Option<u64>,
    secrets: Vec<SecretText>,
    /// What the approver decided, for the audit line.
    approval: Option<String>,
}

/// Builder-style configuration, then [`call`](Self::call).
pub struct ToolLayer {
    catalog: Arc<dyn ConnectionCatalog>,
    provider: Arc<dyn SessionProvider>,
    approver: Arc<dyn WriteApprover>,
    estimator: Arc<dyn TokenEstimator>,
    audit: Option<Arc<AuditLog>>,
    limits: Limits,
    front_end: String,
    specs: Vec<ToolSpec>,
    sessions: Mutex<HashMap<String, Arc<OpenSession>>>,
    /// The gate of every connection id with an open or a close in progress (or waiting for one);
    /// an id whose gate is idle has no entry, so the map is as large as the work in flight. The
    /// map lock is held for a lookup only, never while connecting or disconnecting.
    opening: Mutex<HashMap<String, Arc<Gate>>>,
    /// The number of the last open (see [`OpenGeneration`]).
    generations: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ToolLayer {
    /// `front_end` names the caller in the audit log (`mcp`, `assistant`).
    /// Writes are refused until [`with_approver`](Self::with_approver).
    pub fn new(
        catalog: Arc<dyn ConnectionCatalog>,
        provider: Arc<dyn SessionProvider>,
        front_end: impl Into<String>,
    ) -> Self {
        Self {
            catalog,
            provider,
            approver: Arc::new(RefuseWrites),
            estimator: Arc::new(CharsPerThree),
            audit: None,
            limits: Limits::default(),
            front_end: front_end.into(),
            specs: schema::tool_specs(),
            sessions: Mutex::new(HashMap::new()),
            opening: Mutex::new(HashMap::new()),
            generations: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn with_approver(mut self, approver: Arc<dyn WriteApprover>) -> Self {
        self.approver = approver;
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_estimator(mut self, estimator: Arc<dyn TokenEstimator>) -> Self {
        self.estimator = estimator;
        self
    }

    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The tools with their schemas, for `tools/list` or an API request.
    pub fn tools(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Whether the connection has an open session; `connection_id` in any spelling the catalog
    /// accepts ([`ConnectionCatalog::canonical_id`]).
    pub fn is_open(&self, connection_id: &str) -> bool {
        lock(&self.sessions).contains_key(&self.catalog.canonical_id(connection_id))
    }

    /// Closes every session that is open, or being opened, when it is called. It waits for an open
    /// that is in flight and closes what that open produced, so when it returns each of those
    /// connections is closed (and its tunnel gone); a call that starts opening a connection after
    /// `close_all` began may leave that connection open, with its tunnel: a session is never open
    /// without its tunnel. Blocks as long as the slowest of those opens or disconnects.
    pub fn close_all(&self) {
        let mut ids: Vec<String> = lock(&self.sessions).keys().cloned().collect();
        ids.extend(lock(&self.opening).keys().cloned());
        ids.sort();
        ids.dedup();
        for id in ids {
            self.revoke(&id);
        }
    }

    /// Closes the connection's agent session (and what the front end built around it, via
    /// [`SessionProvider::close_generation`]) if one is open. Call it when the connection is edited,
    /// deleted or its access changes; the layer itself calls it when a call finds the effective
    /// access `off` or the connection gone. Idempotent. It takes the connection's gate, so it
    /// follows an open of the same connection that is in flight (and the new session is the one
    /// closed) and an open that comes after it starts from nothing.
    /// `connection_id` may be spelled any way the catalog accepts
    /// ([`ConnectionCatalog::canonical_id`]).
    pub fn revoke(&self, connection_id: &str) {
        let id = self.catalog.canonical_id(connection_id);
        let _gate = self.enter_uncancellable(&id);
        self.close_locked(&id, None);
    }

    /// The gate of `connection_id`, waiting for it; `None`: the call was cancelled while waiting.
    fn enter(&self, connection_id: &str, cancel: Option<&CancelToken>) -> Option<GateGuard<'_>> {
        let gate = Arc::clone(
            lock(&self.opening)
                .entry(connection_id.to_owned())
                .or_default(),
        );
        if gate.acquire(cancel) {
            return Some(GateGuard {
                layer: self,
                id: connection_id.to_owned(),
                gate,
            });
        }
        self.forget_if_idle(connection_id, &gate);
        None
    }

    /// The gates in use (every id with an open or close in flight or waiting).
    #[cfg(test)]
    fn gates_in_use(&self) -> usize {
        lock(&self.opening).len()
    }

    /// Callers waiting for the id's gate.
    #[cfg(test)]
    fn gate_waiters(&self, connection_id: &str) -> usize {
        lock(&self.opening)
            .get(connection_id)
            .map_or(0, |gate| lock(&gate.state).waiting)
    }

    fn enter_uncancellable(&self, connection_id: &str) -> GateGuard<'_> {
        self.enter(connection_id, None)
            .expect("a wait without a cancel token ends with the gate")
    }

    /// Removes the id's entry when only the map and the caller's `gate` still refer to it. A new
    /// user clones the entry under the map lock, which is held here, so none can slip in.
    fn forget_if_idle(&self, connection_id: &str, gate: &Arc<Gate>) {
        let mut gates = lock(&self.opening);
        if Arc::strong_count(gate) == 2
            && gates
                .get(connection_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, gate))
        {
            gates.remove(connection_id);
        }
    }

    /// Removes and disconnects the connection's session, or `only` that session if it is still the
    /// registered one. The caller holds the connection's gate.
    fn close_locked(
        &self,
        connection_id: &str,
        only: Option<&Arc<OpenSession>>,
    ) -> Option<Arc<OpenSession>> {
        let slot = {
            let mut sessions = lock(&self.sessions);
            if let Some(only) = only {
                if !sessions
                    .get(connection_id)
                    .is_some_and(|current| Arc::ptr_eq(current, only))
                {
                    return None;
                }
            }
            sessions.remove(connection_id)?
        };
        self.disconnect(&slot);
        Some(slot)
    }

    /// Closes `slot` (taking the gate), unless it was replaced or closed meanwhile.
    fn close_slot(&self, connection_id: &str, slot: &Arc<OpenSession>) {
        let _gate = self.enter_uncancellable(connection_id);
        self.close_locked(connection_id, Some(slot));
    }

    /// Disconnects a session that is no longer wanted: a statement it is running is cancelled first
    /// (best effort, through the token the statement was started with). A statement the server
    /// already executed cannot be undone by this; it only stops waiting for the rest. The provider
    /// is told which open this was, so it drops that open's tunnel and no other.
    fn disconnect(&self, slot: &OpenSession) {
        slot.closing.store(true, Ordering::SeqCst);
        // The guard is gone before the cancel runs: the driver's cancel can take seconds and the
        // statement's own bookkeeping needs this lock to finish.
        let running = lock(&slot.active).take();
        if let Some(running) = running {
            running.cancel();
        }
        slot.session.disconnect();
        self.provider
            .close_generation(&slot.connection, slot.generation);
    }

    /// Runs a tool. Blocking; call it from a worker thread.
    pub fn call(&self, name: &str, arguments: Value) -> ToolOutcome {
        self.call_cancellable(name, arguments, &CancelToken::new())
    }

    /// [`call`](Self::call) for arguments that arrive as JSON text (an LLM's
    /// tool call). Text that is not JSON is an `agent.invalid_arguments` error.
    pub fn call_json(&self, name: &str, arguments: &str) -> ToolOutcome {
        let parsed = if arguments.trim().is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_str(arguments).map_err(|error| {
                AgentError::InvalidArguments(format!("the arguments are not valid JSON: {error}"))
            })
        };
        self.call_parsed(name, parsed, &CancelToken::new())
    }

    /// Like [`call`](Self::call), with a token the caller cancels when the
    /// client goes away (use [`CancelToken::cancel_on_drop`]). The token is
    /// for this call only: the statement timeout fires through it.
    pub fn call_cancellable(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancelToken,
    ) -> ToolOutcome {
        self.call_parsed(name, Ok(arguments), cancel)
    }

    fn call_parsed(
        &self,
        name: &str,
        arguments: Result<Value, AgentError>,
        cancel: &CancelToken,
    ) -> ToolOutcome {
        let started = Instant::now();
        let mut trace = Trace::default();
        let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.dispatch(name, arguments, cancel, &mut trace)
        }));
        let result = match dispatched {
            Ok(result) => result,
            Err(panic) => {
                // A call that panics still leaves its line; the payload may carry connection
                // text, so it is not read, only raised again for the front end to answer.
                self.audit_call(name, trace, error::INTERNAL, started);
                std::panic::resume_unwind(panic);
            }
        };
        let outcome = match result {
            Ok(value) => ToolOutcome::Success(value),
            Err(error) => ToolOutcome::Failure(ToolError {
                code: error.code().to_owned(),
                message: clean(&redact(&error.to_string(), &trace.secrets), 2000),
                retryable: error.retryable(),
            }),
        };
        self.audit_call(name, trace, outcome.code().unwrap_or("ok"), started);
        outcome
    }

    fn audit_call(&self, name: &str, trace: Trace, outcome: &str, started: Instant) {
        let Some(audit) = &self.audit else {
            return;
        };
        let mut entry = AuditEntry::now(&self.front_end, &sanitize_tool_name(name));
        entry.connection_id = trace.connection_id;
        entry.statement = trace.statement.map(|text| redact(&text, &trace.secrets));
        entry.outcome = outcome.to_owned();
        entry.rows = trace.rows;
        entry.approval = trace.approval;
        entry.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        audit.record(entry);
    }

    fn dispatch(
        &self,
        name: &str,
        arguments: Result<Value, AgentError>,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<Value, AgentError> {
        let tool = ToolName::parse(name)
            .ok_or_else(|| AgentError::UnknownTool(sanitize_tool_name(name)))?;
        let mut arguments = arguments?;
        // The one place a model-supplied id is put in the catalog's spelling; everything below
        // (sessions, gates, the audit line) uses that form.
        if let Some(id) = arguments.get_mut("connection_id") {
            if let Some(text) = id.as_str() {
                *id = Value::String(self.catalog.canonical_id(text));
            }
        }
        if let Some(id) = arguments.get("connection_id").and_then(Value::as_str) {
            trace.connection_id = Some(id.chars().take(128).collect());
        }
        match tool {
            ToolName::ListConnections => {
                let _: ListConnectionsRequest = self.parse(tool, arguments)?;
                to_value(self.list_connections())
            }
            ToolName::OpenSession => {
                let request: ConnectionRequest = self.parse(tool, arguments)?;
                to_value(self.open_session(&request.connection_id, cancel, trace)?)
            }
            ToolName::CloseSession => {
                let request: ConnectionRequest = self.parse(tool, arguments)?;
                to_value(self.close_session(&request.connection_id, trace))
            }
            ToolName::ListNamespaces => {
                let request: ListNamespacesRequest = self.parse(tool, arguments)?;
                to_value(self.list_namespaces(request, cancel, trace)?)
            }
            ToolName::ListTables => {
                let request: ListTablesRequest = self.parse(tool, arguments)?;
                to_value(self.list_tables(request, cancel, trace)?)
            }
            ToolName::DescribeTable => {
                let request: DescribeTableRequest = self.parse(tool, arguments)?;
                to_value(self.describe_table(request, cancel, trace)?)
            }
            ToolName::GetSchemaContext => {
                let request: GetSchemaContextRequest = self.parse(tool, arguments)?;
                to_value(self.get_schema_context(request, cancel, trace)?)
            }
            ToolName::ExecuteQuery => {
                let request: ExecuteQueryRequest = self.parse(tool, arguments)?;
                to_value(self.execute_query(request, cancel, trace)?)
            }
            ToolName::ExplainQuery => {
                let request: ExplainQueryRequest = self.parse(tool, arguments)?;
                to_value(self.explain_query(request, cancel, trace)?)
            }
            ToolName::ExecuteRedisCommand => {
                let request: ExecuteRedisCommandRequest = self.parse(tool, arguments)?;
                to_value(self.execute_redis(request, cancel, trace)?)
            }
        }
    }

    /// Validates against the tool's schema (clear messages with a path), then
    /// parses into the typed request.
    fn parse<T: DeserializeOwned>(
        &self,
        tool: ToolName,
        mut arguments: Value,
    ) -> Result<T, AgentError> {
        if arguments.is_null() {
            arguments = Value::Object(serde_json::Map::new());
        }
        if let Some(spec) = self.specs.iter().find(|spec| spec.name == tool.as_str()) {
            schema::validate_request(&spec.input_schema, &arguments)
                .map_err(AgentError::InvalidArguments)?;
        }
        serde_json::from_value(arguments)
            .map_err(|error| AgentError::InvalidArguments(error.to_string()))
    }

    // ------------------------------------------------------------- access

    /// The connection, and whether its effective access allows `tool`.
    fn authorize(
        &self,
        tool: ToolName,
        connection_id: &str,
    ) -> Result<(ConnectionInfo, AgentAccess), AgentError> {
        let Some(info) = self.catalog.get(connection_id) else {
            // The connection is gone: so is its session.
            self.revoke(connection_id);
            return Err(AgentError::invalid(format!(
                "unknown connection_id `{}`; call list_connections for the ids",
                connection_id.chars().take(64).collect::<String>()
            )));
        };
        let access = info.effective_access();
        if access == AgentAccess::Off {
            // Access was lowered to `off` while a session was open: close it, then refuse.
            self.revoke(connection_id);
        }
        // The connection was edited since the session opened (another address, tunnel, type ...):
        // the session goes. `open_session` then simply opens a new one; any other tool is told to.
        let stale = lock(&self.sessions)
            .get(connection_id)
            .filter(|slot| slot.revision.differs_from(info.revision))
            .cloned();
        if let Some(slot) = stale {
            self.close_slot(connection_id, &slot);
            if tool != ToolName::OpenSession {
                return Err(Self::edited(&info));
            }
        }
        let required = tool.required_access();
        if access < required {
            return Err(AgentError::AccessDenied(denial_message(
                &info, access, tool, required,
            )));
        }
        Ok((info, access))
    }

    /// After the approver said yes: the connection as the catalog describes it now must still be
    /// the one that was asked about, with the access that allows a write. A card can wait for a
    /// long time; the user may have changed the connection (or its type) meanwhile.
    fn recheck_after_approval(
        &self,
        tool: ToolName,
        asked: &ConnectionInfo,
        request: &WriteApproval<'_>,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<(), AgentError> {
        let refuse = |trace: &mut Trace, why: &str, error: AgentError| {
            trace.approval = Some(format!("approved, then refused: {why}"));
            Err(error)
        };
        let Some(now) = self.catalog.get(&asked.id) else {
            self.revoke(&asked.id);
            return refuse(
                trace,
                "the connection was removed",
                AgentError::NotConnected(
                    "the connection was removed while the question was open, so the write did not run"
                        .into(),
                ),
            );
        };
        if now.revision.differs_from(asked.revision) {
            return refuse(trace, "the connection was edited", Self::edited(&now));
        }
        let access = now.effective_access();
        if access < AgentAccess::ReadWrite {
            return refuse(
                trace,
                "the access was lowered",
                AgentError::AccessDenied(denial_message(
                    &now,
                    access,
                    tool,
                    AgentAccess::ReadWrite,
                )),
            );
        }
        if cancel.is_cancelled() {
            trace.approval = Some("approved, then cancelled".into());
            return Err(AgentError::Cancelled);
        }
        // Whatever the front end itself needs to hold after a yes (the MCP server's switch).
        if let Err(reason) = self.approver.still_allowed(request) {
            return refuse(
                trace,
                &clean(&reason, 200),
                AgentError::WriteRefused(format!(
                    "the change was approved, but it can no longer run: {reason}"
                )),
            );
        }
        Ok(())
    }

    fn edited(info: &ConnectionInfo) -> AgentError {
        AgentError::NotConnected(format!(
            "connection `{}` was edited, so its session was closed; call open_session again",
            info.name
        ))
    }

    /// The connection's open session, if it is the one for the connection as `info` (from
    /// [`authorize`](Self::authorize)) describes it now: the revision is compared on the very slot
    /// that is about to serve the call, so a session opened under another revision (the open was
    /// authorised before an edit and finished after it) is closed instead of used.
    fn open_slot(
        &self,
        info: &ConnectionInfo,
        trace: &mut Trace,
    ) -> Result<Arc<OpenSession>, AgentError> {
        let slot = lock(&self.sessions).get(&info.id).cloned().ok_or_else(|| {
            AgentError::NotConnected(format!(
                "no open session for `{}`; call open_session first",
                info.id.chars().take(64).collect::<String>()
            ))
        })?;
        if slot.revision.differs_from(info.revision) {
            self.close_slot(&info.id, &slot);
            return Err(Self::edited(info));
        }
        trace.secrets.clone_from(&slot.secrets);
        Ok(slot)
    }

    // -------------------------------------------------------------- tools

    fn list_connections(&self) -> ListConnectionsResult {
        let listed = self.catalog.list();
        // A session whose connection is gone or whose access is now `off` does not stay open.
        let usable: HashMap<&str, Revision> = listed
            .iter()
            .filter(|info| info.effective_access() > AgentAccess::Off)
            .map(|info| (info.id.as_str(), info.revision))
            .collect();
        let stale: Vec<(String, Arc<OpenSession>)> = lock(&self.sessions)
            .iter()
            .filter(|(id, slot)| {
                usable
                    .get(id.as_str())
                    .is_none_or(|revision| slot.revision.differs_from(*revision))
            })
            .map(|(id, slot)| (id.clone(), Arc::clone(slot)))
            .collect();
        for (id, slot) in stale {
            self.close_slot(&id, &slot);
        }
        let open: std::collections::HashSet<String> =
            lock(&self.sessions).keys().cloned().collect();
        let mut connections: Vec<ConnectionSummary> = listed
            .into_iter()
            .map(|info| {
                let access = info.effective_access();
                let visible = access > AgentAccess::Off;
                ConnectionSummary {
                    access,
                    open: open.contains(&info.id),
                    configured_access: info.agent_access.filter(|_| visible),
                    read_only: visible.then_some(info.connection_type.read_only),
                    production: visible.then_some(info.connection_type.production),
                    connection_type: visible
                        .then(|| clean_name(&info.connection_type.name, MAX_NAME_CHARS)),
                    kind: visible.then_some(info.kind),
                    name: clean_name(&info.name, MAX_NAME_CHARS),
                    id: clean_name(&info.id, MAX_NAME_CHARS),
                }
            })
            .collect();
        connections.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        ListConnectionsResult { connections }
    }

    fn open_session(
        &self,
        connection_id: &str,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<OpenSessionResult, AgentError> {
        let (info, access) = self.authorize(ToolName::OpenSession, connection_id)?;
        let timeout = info.statement_timeout(&self.limits).as_secs();
        let result = |slot: &OpenSession, already_open: bool| OpenSessionResult {
            connection_id: info.id.clone(),
            kind: slot.info.kind,
            access,
            language: slot.info.language,
            supports_schemas: slot.info.supports_schemas,
            already_open,
            statement_timeout_secs: timeout,
        };
        // One open or close per connection at a time: a second call waits here (and gives up when
        // its own call is cancelled) and then sees what the first left. Only one open of the
        // connection can be past this point, so nothing else registers a session for it.
        let Some(_gate) = self.enter(connection_id, Some(cancel)) else {
            return Err(AgentError::Cancelled);
        };
        let existing = lock(&self.sessions).get(connection_id).cloned();
        if let Some(existing) = existing {
            if !existing.revision.differs_from(info.revision) {
                return Ok(result(&existing, true));
            }
            // Opened for the connection as it was before an edit: close it the normal way and
            // open the connection as it is now.
            self.close_locked(connection_id, Some(&existing));
        }
        // Known before connecting, so a connect error that echoes one is masked.
        // `secrets` first: a provider reads the connection once for both (`plain_words` docs).
        let known = self.provider.secrets(&info);
        let plain_words = self.provider.plain_words(&info);
        trace.secrets = Self::without_plain_words(known, &plain_words);
        let generation = OpenGeneration::new(self.generations.fetch_add(1, Ordering::Relaxed) + 1);
        let session = self.provider.open_generation(&info, generation)?;
        let mut secrets = Self::without_plain_words(session.secrets(), &plain_words);
        for known in &trace.secrets {
            if !secrets.contains(known) {
                secrets.push(known.clone());
            }
        }
        let slot = Arc::new(OpenSession {
            info: session.info(),
            masks: Masks::new(&secrets),
            secrets,
            session,
            connection: info.clone(),
            revision: info.revision,
            generation,
            active: Mutex::new(None),
            closing: AtomicBool::new(false),
            busy: AtomicBool::new(false),
        });
        trace.secrets.clone_from(&slot.secrets);
        lock(&self.sessions).insert(connection_id.to_owned(), Arc::clone(&slot));
        Ok(result(&slot, false))
    }

    /// The secrets that are not equal to a word of the connection that is not secret (its user and
    /// database name, see [`SessionProvider::plain_words`]).
    fn without_plain_words(secrets: Vec<SecretText>, plain_words: &[String]) -> Vec<SecretText> {
        secrets
            .into_iter()
            .filter(|secret| !plain_words.iter().any(|word| word == secret.expose()))
            .collect()
    }

    fn close_session(&self, connection_id: &str, trace: &mut Trace) -> CloseSessionResult {
        let _gate = self.enter_uncancellable(connection_id);
        let slot = self.close_locked(connection_id, None);
        let closed = slot.is_some();
        if let Some(slot) = slot {
            trace.secrets.clone_from(&slot.secrets);
        }
        CloseSessionResult {
            connection_id: connection_id.to_owned(),
            closed,
        }
    }

    fn list_namespaces(
        &self,
        request: ListNamespacesRequest,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<ListNamespacesResult, AgentError> {
        Self::check_not_cancelled(cancel)?;
        let (info, _) = self.authorize(ToolName::ListNamespaces, &request.connection_id)?;
        let slot = self.open_slot(&info, trace)?;
        let level = if request.database.is_some() {
            NodeLevel::Schemas
        } else {
            NodeLevel::Databases
        };
        let page = slot.session.list_nodes(
            level,
            &NodeQuery {
                database: request.database,
                cursor: request.cursor,
                limit: self.limits.list_items_for(request.limit),
                ..NodeQuery::default()
            },
        )?;
        Ok(ListNamespacesResult {
            namespaces: page
                .nodes
                .into_iter()
                .map(|n| NameItem {
                    name: clean_name(&n.name, MAX_NAME_CHARS),
                    kind: clean(&n.kind, 40),
                })
                .collect(),
            next_cursor: page.next_cursor,
        })
    }

    /// A call that was cancelled before it started does not start. (Once a browse call is in the
    /// driver it cannot be interrupted: `Session::list_nodes` / `describe_table` take no token.)
    fn check_not_cancelled(cancel: &CancelToken) -> Result<(), AgentError> {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        Ok(())
    }

    /// `database` / `schema` for a table-level call, filled in when the
    /// connection has only one choice.
    fn resolve_namespace(
        &self,
        slot: &OpenSession,
        database: Option<String>,
        schema: Option<String>,
    ) -> Result<(Option<String>, Option<String>), AgentError> {
        let database = match database {
            Some(database) => database,
            None => {
                let page = slot.session.list_nodes(
                    NodeLevel::Databases,
                    &NodeQuery {
                        limit: 2,
                        ..NodeQuery::default()
                    },
                )?;
                match page.nodes.as_slice() {
                    [only] if page.next_cursor.is_none() => only.name.clone(),
                    _ => {
                        return Err(AgentError::invalid(
                            "`database` is required; call list_namespaces to see the databases",
                        ))
                    }
                }
            }
        };
        if schema.is_none() && slot.info.supports_schemas {
            return Err(AgentError::invalid(
                "`schema` is required for this connection; call list_namespaces with the database to see its schemas",
            ));
        }
        Ok((Some(database), schema))
    }

    fn require_sql(slot: &OpenSession, tool: ToolName) -> Result<(), AgentError> {
        match slot.info.language {
            Language::Sql => Ok(()),
            Language::Redis => Err(AgentError::Unsupported(format!(
                "`{}` is for SQL databases; this is Redis, use execute_redis_command (e.g. SCAN)",
                tool.as_str()
            ))),
            Language::Other => Err(AgentError::Unsupported(format!(
                "`{}` is not available for {} connections",
                tool.as_str(),
                slot.info.kind.driver_id()
            ))),
        }
    }

    fn list_tables(
        &self,
        request: ListTablesRequest,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<ListTablesResult, AgentError> {
        Self::check_not_cancelled(cancel)?;
        let (info, _) = self.authorize(ToolName::ListTables, &request.connection_id)?;
        let slot = self.open_slot(&info, trace)?;
        if slot.info.language == Language::Redis {
            Self::require_sql(&slot, ToolName::ListTables)?;
        }
        let (database, schema) = self.resolve_namespace(&slot, request.database, request.schema)?;
        let page = slot.session.list_nodes(
            NodeLevel::Tables,
            &NodeQuery {
                database,
                schema,
                filter: request.filter.filter(|filter| !filter.is_empty()),
                cursor: request.cursor,
                limit: self.limits.list_items_for(request.limit),
            },
        )?;
        Ok(ListTablesResult {
            tables: page
                .nodes
                .into_iter()
                .map(|n| NameItem {
                    name: clean_name(&n.name, MAX_NAME_CHARS),
                    kind: clean(&n.kind, 40),
                })
                .collect(),
            next_cursor: page.next_cursor,
        })
    }

    fn describe_table(
        &self,
        request: DescribeTableRequest,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<DescribeTableResult, AgentError> {
        Self::check_not_cancelled(cancel)?;
        let (info, _) = self.authorize(ToolName::DescribeTable, &request.connection_id)?;
        let slot = self.open_slot(&info, trace)?;
        Self::require_sql(&slot, ToolName::DescribeTable)?;
        let (database, schema) = self.resolve_namespace(&slot, request.database, request.schema)?;
        let mut description = slot.session.describe_table(&TableRef {
            database,
            schema,
            name: request.table,
        })?;
        description.sanitize_with(&slot.masks);
        description.cap_ddl(self.limits.max_ddl_bytes);
        Ok(description)
    }

    fn get_schema_context(
        &self,
        request: GetSchemaContextRequest,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<GetSchemaContextResult, AgentError> {
        Self::check_not_cancelled(cancel)?;
        let (info, _) = self.authorize(ToolName::GetSchemaContext, &request.connection_id)?;
        let slot = self.open_slot(&info, trace)?;
        Self::require_sql(&slot, ToolName::GetSchemaContext)?;
        let (database, schema) = self.resolve_namespace(&slot, request.database, request.schema)?;
        let part = |name: &String| name_form(&clean_name(name, MAX_NAME_CHARS));
        let label = match (&database, &schema) {
            (Some(database), Some(schema)) => format!("{}.{}", part(database), part(schema)),
            (Some(name), None) | (None, Some(name)) => part(name),
            (None, None) => String::new(),
        };
        // Every table name (bounded), then the first `max_context_tables` described.
        const NAME_PAGE: usize = 500;
        const MAX_NAMES: usize = 5000;
        let mut names: Vec<String> = Vec::new();
        let mut cursor = None;
        loop {
            let page = slot.session.list_nodes(
                NodeLevel::Tables,
                &NodeQuery {
                    database: database.clone(),
                    schema: schema.clone(),
                    filter: None,
                    cursor: cursor.take(),
                    limit: NAME_PAGE,
                },
            )?;
            names.extend(page.nodes.into_iter().map(|node| node.name));
            cursor = page.next_cursor;
            if cursor.is_none() || names.len() >= MAX_NAMES {
                break;
            }
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
        }
        names.sort();
        names.dedup();
        let mut described = Vec::new();
        let mut undescribed = Vec::new();
        for (index, name) in names.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            if index >= self.limits.max_context_tables {
                undescribed.push(name);
                continue;
            }
            match slot.session.describe_table(&TableRef {
                database: database.clone(),
                schema: schema.clone(),
                name: name.clone(),
            }) {
                Ok(mut description) => {
                    description.sanitize_with(&slot.masks);
                    description.cap_ddl(self.limits.max_ddl_bytes);
                    described.push(description);
                }
                Err(AgentError::Cancelled) => return Err(AgentError::Cancelled),
                Err(AgentError::NotConnected(message)) => {
                    return Err(AgentError::NotConnected(message))
                }
                // One unreadable table does not sink the digest.
                Err(_) => undescribed.push(name),
            }
        }
        let budget = request
            .budget
            .filter(|budget| *budget > 0)
            .unwrap_or(DEFAULT_BUDGET);
        Ok(build_schema_context(
            &ContextInput {
                label: &label,
                dialect: slot.info.kind.driver_id(),
                tables: &described,
                undescribed: &undescribed,
                budget,
            },
            self.estimator.as_ref(),
        ))
    }

    fn execute_query(
        &self,
        request: ExecuteQueryRequest,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<StatementResult, AgentError> {
        let (info, access) = self.authorize(ToolName::ExecuteQuery, &request.connection_id)?;
        Self::check_database(request.database.as_deref())?;
        let slot = self.open_slot(&info, trace)?;
        Self::require_sql(&slot, ToolName::ExecuteQuery)?;
        self.check_length(&request.statement)?;
        trace.statement = Some(request.statement.clone());
        let lex = slot
            .info
            .lex
            .ok_or_else(|| AgentError::Internal("a SQL session without lexer settings".into()))?;
        let classified = classify_sql(&request.statement, slot.info.kind, &lex);
        self.run_policy_checked(
            ToolName::ExecuteQuery,
            &info,
            access,
            &slot,
            &request.statement,
            &classified,
            "SQL statement",
            Language::Sql,
            request.database.as_deref(),
            request.row_limit,
            request.dangerous.unwrap_or(false),
            cancel,
            trace,
        )
    }

    fn explain_query(
        &self,
        request: ExplainQueryRequest,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<StatementResult, AgentError> {
        let (info, access) = self.authorize(ToolName::ExplainQuery, &request.connection_id)?;
        Self::check_database(request.database.as_deref())?;
        let slot = self.open_slot(&info, trace)?;
        Self::require_sql(&slot, ToolName::ExplainQuery)?;
        self.check_length(&request.statement)?;
        let lex = slot
            .info
            .lex
            .ok_or_else(|| AgentError::Internal("a SQL session without lexer settings".into()))?;
        let inner = request.statement.trim();
        let inner_class = classify_sql(inner, slot.info.kind, &lex);
        if inner_class.statements == 0 {
            return Err(AgentError::invalid("the SQL statement is empty"));
        }
        // Clauses that start a statement (`LOAD DATA`, `HANDLER`) are only
        // recognised there, so look at the statement before `EXPLAIN` is put in front.
        if let Some(function) = inner_class.blocked_function {
            trace.statement = Some(request.statement.clone());
            return Err(AgentError::BlockedFunction(function));
        }
        if matches!(
            first_keyword(inner, &lex),
            Some(Keyword::Explain | Keyword::Describe)
        ) {
            trace.statement = Some(request.statement.clone());
            return Err(AgentError::invalid(
                "pass the statement without EXPLAIN; explain_query adds it",
            ));
        }
        let prefix = if slot.info.kind == crate::ports::ConnectionKind::Sqlite {
            "EXPLAIN QUERY PLAN "
        } else {
            "EXPLAIN "
        };
        let text = format!("{prefix}{inner}");
        trace.statement = Some(text.clone());
        // The final text goes through the same classifier as any statement, so
        // `EXPLAIN` of a script or a smuggled `ANALYZE` is caught by the policy.
        let classified = classify_sql(&text, slot.info.kind, &lex);
        self.run_policy_checked(
            ToolName::ExplainQuery,
            &info,
            access,
            &slot,
            &text,
            &classified,
            "SQL statement",
            Language::Sql,
            request.database.as_deref(),
            None,
            false,
            cancel,
            trace,
        )
    }

    fn execute_redis(
        &self,
        request: ExecuteRedisCommandRequest,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<StatementResult, AgentError> {
        let (info, access) =
            self.authorize(ToolName::ExecuteRedisCommand, &request.connection_id)?;
        Self::check_database(request.database.as_deref())?;
        let slot = self.open_slot(&info, trace)?;
        if slot.info.language != Language::Redis {
            return Err(AgentError::Unsupported(format!(
                "execute_redis_command is for Redis connections; this is {}",
                slot.info.kind.driver_id()
            )));
        }
        self.check_length(&request.command)?;
        trace.statement = Some(request.command.clone());
        let command = request.command.trim();
        if command.contains(['\n', '\r']) {
            return Err(AgentError::MultiStatement);
        }
        let classified = classify_redis(command);
        self.run_policy_checked(
            ToolName::ExecuteRedisCommand,
            &info,
            access,
            &slot,
            command,
            &classified,
            "Redis command",
            Language::Redis,
            request.database.as_deref(),
            request.row_limit,
            request.dangerous.unwrap_or(false),
            cancel,
            trace,
        )
    }

    /// The database a write will run in: the request's, else the connection's
    /// only database when the navigator shows exactly one.
    fn approval_database(&self, slot: &OpenSession, requested: Option<&str>) -> Option<String> {
        if let Some(requested) = requested.filter(|name| !name.is_empty()) {
            return Some(clean_name(requested, MAX_NAME_CHARS));
        }
        if slot.info.language != Language::Sql {
            return None;
        }
        let page = slot
            .session
            .list_nodes(
                NodeLevel::Databases,
                &NodeQuery {
                    limit: 2,
                    ..NodeQuery::default()
                },
            )
            .ok()?;
        match page.nodes.as_slice() {
            [only] if page.next_cursor.is_none() => Some(clean_name(&only.name, MAX_NAME_CHARS)),
            _ => None,
        }
    }

    /// A `database` argument is drawn on the approval card and handed to the driver as it is:
    /// a character with no shape of its own, or a name longer than the card shows, would make the
    /// two differ. The model cannot fix either by rewording (a database whose real name has such
    /// a character cannot be written back), so the message says the database cannot be used here.
    fn check_database(database: Option<&str>) -> Result<(), AgentError> {
        let Some(name) = database else {
            return Ok(());
        };
        if let Some(ch) = name.chars().find(|ch| is_shapeless(*ch)) {
            return Err(AgentError::invalid(format!(
                "this database cannot be used by the statement tools: its name contains a \
                 character with no shape of its own (U+{:04X}) that can be neither shown for \
                 approval nor written back. Leave the database argument out to use the \
                 connection's own database, or tell the user",
                u32::from(ch)
            )));
        }
        let length = name.chars().count();
        if length > MAX_NAME_CHARS {
            return Err(AgentError::invalid(format!(
                "this database cannot be used by the statement tools: its name is {length} \
                 characters and at most {MAX_NAME_CHARS} can be shown for approval. Leave the \
                 database argument out to use the connection's own database, or tell the user"
            )));
        }
        Ok(())
    }

    fn check_length(&self, text: &str) -> Result<(), AgentError> {
        if text.contains('\0') {
            return Err(AgentError::invalid(
                "the statement contains a NUL character",
            ));
        }
        if has_bare_carriage_return(text) {
            return Err(AgentError::invalid(
                "the statement contains a carriage return that is not followed by a line feed; \
                 databases and editors disagree on where such a line ends, so send plain \\n or \\r\\n \
                 line breaks. To put a carriage return inside a value, use the dialect's escape or \
                 function (E'\\r' or chr(13) in PostgreSQL, CHAR(13) in MySQL, MariaDB, SQL Server), \
                 not a raw character",
            ));
        }
        if text.len() > self.limits.max_statement_bytes {
            return Err(AgentError::Limit(format!(
                "the statement is {} bytes; the limit is {}",
                text.len(),
                self.limits.max_statement_bytes
            )));
        }
        Ok(())
    }

    /// Policy, approval, then the run, for one classified statement.
    #[allow(clippy::too_many_arguments)]
    fn run_policy_checked(
        &self,
        tool: ToolName,
        info: &ConnectionInfo,
        access: AgentAccess,
        slot: &OpenSession,
        text: &str,
        classified: &Classified,
        what: &str,
        language: Language,
        database: Option<&str>,
        row_limit: Option<usize>,
        flagged_dangerous: bool,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<QueryResult, AgentError> {
        // A front end that never runs dangerous statements must say so at once, not ask the model
        // to call again with the flag (the retry would be refused too): treating every dangerous
        // statement as flagged sends it to the refusal below, with the front end's own words.
        let need = check_statement(
            access,
            classified,
            flagged_dangerous || !self.approver.allows_dangerous(),
            what,
        )?;
        let is_write = matches!(need, Need::Approval { .. });
        let confirmed = match need {
            Need::Nothing => false,
            Need::Approval { dangerous } => {
                if let (Some(reason), false) = (&dangerous, self.approver.allows_dangerous()) {
                    return Err(AgentError::DangerousRefused(format!(
                        "this {what} is dangerous ({reason}) and {} never runs dangerous statements; do not retry it or reword it, ask the user to run it themselves in the TableX app",
                        self.front_end
                    )));
                }
                // No point asking the user for a write that could not be audited.
                if self.audit.as_ref().is_some_and(|audit| !audit.is_healthy()) {
                    return Err(AgentError::AuditUnavailable);
                }
                let resolved_database = self.approval_database(slot, database);
                let request = WriteApproval {
                    connection_id: &info.id,
                    connection_name: &info.name,
                    connection_type: &info.connection_type.name,
                    production: info.connection_type.production,
                    language,
                    database: resolved_database.as_deref(),
                    statement: text,
                    dangerous: dangerous.as_deref(),
                };
                let decision = self.approver.approve(&request, cancel);
                match decision {
                    // SQL: only a dangerous statement is sent as `confirmed`, so
                    // the driver's own check still backs up the classifier for
                    // every other write. Redis asks for it on every write.
                    ApprovalDecision::Approve => {
                        trace.approval = Some("approved".into());
                        // The question may have waited a long time: the connection may have been
                        // edited, its type made read-only or its access lowered since.
                        self.recheck_after_approval(tool, info, &request, cancel, trace)?;
                        dangerous.is_some() || language == Language::Redis
                    }
                    ApprovalDecision::Refuse { reason } => {
                        if cancel.is_cancelled() {
                            trace.approval = Some("cancelled".into());
                            return Err(AgentError::Cancelled);
                        }
                        trace.approval = Some(format!("refused: {}", clean(&reason, 200)));
                        return Err(AgentError::WriteRefused(format!(
                            "this {what} was not run, and the user was not asked: {reason}"
                        )));
                    }
                    ApprovalDecision::Unanswered { reason } => {
                        if cancel.is_cancelled() {
                            trace.approval = Some("cancelled".into());
                            return Err(AgentError::Cancelled);
                        }
                        trace.approval = Some(format!("unanswered: {}", clean(&reason, 200)));
                        return Err(AgentError::WriteRefused(format!(
                            "this {what} was not run: {reason}"
                        )));
                    }
                    ApprovalDecision::Deny { reason } => {
                        if cancel.is_cancelled() {
                            trace.approval = Some("cancelled".into());
                            return Err(AgentError::Cancelled);
                        }
                        trace.approval = Some(match &reason {
                            Some(reason) => format!("declined: {}", clean(reason, 200)),
                            None => "declined".into(),
                        });
                        return Err(AgentError::WriteRefused(format!(
                            "the user did not approve this {what}{}",
                            reason
                                .map(|reason| format!(": {reason}"))
                                .unwrap_or_default()
                        )));
                    }
                }
            }
        };
        let Some(_busy) = BusyGuard::acquire(&slot.busy) else {
            return Err(AgentError::Limit(
                "another statement is still running on this connection; wait for it or cancel it"
                    .into(),
            ));
        };
        // Before anything that can block (the audit write): a closer finds the token, or this sees
        // that the session is closing and the statement does not run.
        let _active = ActiveStatement::begin(slot, cancel)?;
        // A write is audited BEFORE it runs: if the record cannot be written, the
        // write does not happen.
        if is_write {
            if let Some(audit) = &self.audit {
                let mut entry = AuditEntry::now(&self.front_end, tool.as_str());
                entry.connection_id = Some(info.id.chars().take(128).collect());
                entry.statement = Some(slot.masks.redact(text));
                entry.outcome = "started".into();
                entry.approval = trace.approval.clone();
                if audit.record_blocking(entry).is_err() {
                    return Err(AgentError::AuditUnavailable);
                }
            }
        }
        let read_only = classified.class == StatementClass::Read && !is_write;
        let timeout = info.statement_timeout(&self.limits);
        let watchdog = Watchdog::arm(cancel, timeout);
        let started = Instant::now();
        let outcome = slot.session.run(
            &RunRequest {
                language,
                text,
                database,
                limits: &self.limits,
                row_limit: self.limits.rows_for(row_limit),
                confirmed,
                read_only,
            },
            cancel,
        );
        drop(watchdog);
        match outcome {
            Ok(mut result) => {
                result.elapsed_ms =
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                result.read_only_enforced &= read_only;
                for column in &mut result.columns {
                    column.name = clean_name(&column.name, MAX_NAME_CHARS);
                    column.data_type = clean(&column.data_type, MAX_NAME_CHARS);
                }
                trace.rows = Some(result.affected_rows.unwrap_or(result.row_count as u64));
                Ok(result)
            }
            Err(AgentError::Cancelled) if cancel.timed_out() => Err(AgentError::Limit(format!(
                "the statement was cancelled after the {} s statement timeout",
                timeout.as_secs()
            ))),
            Err(error) => Err(error),
        }
    }
}

/// A `\r` that is not the first half of `\r\n`. Some databases end a `--` comment there and the
/// lexer and every editor split lines on `\n` only, so what a person reads and what runs differ.
fn has_bare_carriage_return(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .any(|(at, byte)| *byte == b'\r' && bytes.get(at + 1) != Some(&b'\n'))
}

impl Drop for ToolLayer {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn to_value<T: serde::Serialize>(result: T) -> Result<Value, AgentError> {
    serde_json::to_value(result).map_err(|error| AgentError::Internal(error.to_string()))
}

/// A tool name safe to put in an error message or a log line.
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect()
}

fn denial_message(
    info: &ConnectionInfo,
    access: AgentAccess,
    tool: ToolName,
    required: AgentAccess,
) -> String {
    let mut message = format!(
        "agent access for connection `{}` is `{access}`; `{}` needs `{required}` or more",
        info.name,
        tool.as_str()
    );
    if info.connection_type.read_only
        && info.agent_access.is_some_and(|set| set > AgentAccess::Read)
    {
        message.push_str(" (its connection type is read-only, which caps agent access at `read`)");
    } else if info.connection_type.production && info.agent_access.is_none() {
        message.push_str(" (production connections stay `off` until the user sets agent access on the connection)");
    }
    message.push_str(
        ". Only the user can raise it in TableX: for all connections in Settings \u{2192} AI \u{2192} \
         Default agent access, or for this connection in its Overrides \u{2192} Agent access.",
    );
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::AgentAccess;
    use crate::ports::ConnectionKind;
    use crate::testing::{connection, Behavior, FakeCatalog, FakeSession};
    use serde_json::json;
    use std::collections::{HashSet, VecDeque};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Duration;

    const SQL: &str = "sql1";
    const WAIT: Duration = Duration::from_secs(10);

    /// A one-shot latch the tests use as a barrier.
    #[derive(Default)]
    struct Latch {
        set: Mutex<bool>,
        changed: Condvar,
    }

    impl Latch {
        fn open(&self) {
            *lock(&self.set) = true;
            self.changed.notify_all();
        }

        fn wait(&self) {
            let mut set = lock(&self.set);
            while !*set {
                set = self.changed.wait(set).unwrap();
            }
        }
    }

    /// Spins (yielding) until `condition` holds; panics after [`WAIT`].
    fn until(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::yield_now();
        }
    }

    /// A session like a real driver session: a browse call holds the session lock, and
    /// `disconnect` needs it.
    struct LockedSession {
        inner: FakeSession,
        lock: Mutex<()>,
        browse_entered: Latch,
        browse_release: Latch,
        hang: AtomicBool,
    }

    impl LockedSession {
        fn new(hang: bool) -> Arc<Self> {
            Arc::new(Self {
                inner: FakeSession::new(ConnectionKind::PostgreSql),
                lock: Mutex::new(()),
                browse_entered: Latch::default(),
                browse_release: Latch::default(),
                hang: AtomicBool::new(hang),
            })
        }
    }

    impl Session for LockedSession {
        fn info(&self) -> SessionInfo {
            self.inner.info()
        }

        fn list_nodes(
            &self,
            level: NodeLevel,
            query: &NodeQuery,
        ) -> Result<crate::ports::NodePage, AgentError> {
            let _held = lock(&self.lock);
            if self.hang.load(Ordering::SeqCst) {
                self.browse_entered.open();
                self.browse_release.wait();
            }
            self.inner.list_nodes(level, query)
        }

        fn describe_table(
            &self,
            table: &TableRef,
        ) -> Result<crate::table::TableDescription, AgentError> {
            self.inner.describe_table(table)
        }

        fn run(
            &self,
            request: &RunRequest<'_>,
            cancel: &CancelToken,
        ) -> Result<QueryResult, AgentError> {
            self.inner.run(request, cancel)
        }

        fn disconnect(&self) {
            let _held = lock(&self.lock);
            self.inner.disconnect();
        }
    }

    /// Models `AgentConnections`: a tunnel per open, kept under (connection, generation). The
    /// legacy `close` drops every tunnel of the connection (what closing by id did), so a layer
    /// that closed by id would show up as a missing tunnel.
    #[derive(Default)]
    struct TunnelProvider {
        sessions: Mutex<VecDeque<Arc<dyn Session>>>,
        tunnels: Mutex<HashSet<(String, u64)>>,
        opens: AtomicUsize,
        closed_generations: Mutex<Vec<u64>>,
        /// `open_generation` stops here (after counting) until `gate_open` is set.
        entered: Latch,
        gate_open: Latch,
        block_first_open: AtomicBool,
        fail_next_open: AtomicBool,
        panic_next_open: AtomicBool,
    }

    impl TunnelProvider {
        fn with_sessions(sessions: Vec<Arc<dyn Session>>) -> Arc<Self> {
            Arc::new(Self {
                sessions: Mutex::new(sessions.into()),
                ..Self::default()
            })
        }

        fn tunnels(&self) -> Vec<(String, u64)> {
            let mut all: Vec<_> = lock(&self.tunnels).iter().cloned().collect();
            all.sort();
            all
        }
    }

    impl SessionProvider for TunnelProvider {
        fn open(&self, _connection: &ConnectionInfo) -> Result<Arc<dyn Session>, AgentError> {
            unreachable!("the layer opens by generation")
        }

        fn open_generation(
            &self,
            connection: &ConnectionInfo,
            generation: OpenGeneration,
        ) -> Result<Arc<dyn Session>, AgentError> {
            if self.panic_next_open.swap(false, Ordering::SeqCst) {
                panic!("the provider panicked");
            }
            if self.fail_next_open.swap(false, Ordering::SeqCst) {
                return Err(AgentError::driver("connect.failed", "refused", false));
            }
            let first = self.opens.fetch_add(1, Ordering::SeqCst) == 0;
            if first && self.block_first_open.load(Ordering::SeqCst) {
                self.entered.open();
                self.gate_open.wait();
            }
            let session = lock(&self.sessions)
                .pop_front()
                .ok_or_else(|| AgentError::invalid("the provider ran out of sessions"))?;
            lock(&self.tunnels).insert((connection.id.clone(), generation.get()));
            Ok(session)
        }

        fn close(&self, connection: &ConnectionInfo) {
            lock(&self.tunnels).retain(|(id, _)| *id != connection.id);
        }

        fn close_generation(&self, connection: &ConnectionInfo, generation: OpenGeneration) {
            lock(&self.closed_generations).push(generation.get());
            lock(&self.tunnels).remove(&(connection.id.clone(), generation.get()));
        }
    }

    struct Rig {
        layer: Arc<ToolLayer>,
        catalog: Arc<FakeCatalog>,
        provider: Arc<TunnelProvider>,
    }

    fn rig(access: AgentAccess, provider: Arc<TunnelProvider>) -> Rig {
        let catalog = FakeCatalog::new(vec![connection(
            SQL,
            ConnectionKind::PostgreSql,
            Some(access),
        )]);
        let layer = Arc::new(ToolLayer::new(catalog.clone(), provider.clone(), "test"));
        Rig {
            layer,
            catalog,
            provider,
        }
    }

    fn fake() -> Arc<dyn Session> {
        FakeSession::new(ConnectionKind::PostgreSql).arc()
    }

    fn spawn_call(
        layer: &Arc<ToolLayer>,
        tool: &'static str,
        arguments: Value,
    ) -> std::thread::JoinHandle<ToolOutcome> {
        let layer = Arc::clone(layer);
        std::thread::spawn(move || layer.call(tool, arguments))
    }

    fn id_args() -> Value {
        json!({ "connection_id": SQL })
    }

    // ---- B-1 ---------------------------------------------------------------------------------

    #[test]
    fn a_close_racing_an_open_never_drops_the_new_sessions_tunnel() {
        let hung = LockedSession::new(true);
        let provider = TunnelProvider::with_sessions(vec![hung.clone(), fake()]);
        let rig = rig(AgentAccess::Read, provider);
        let layer = &rig.layer;
        assert!(!spawn_call(layer, "open_session", id_args())
            .join()
            .unwrap()
            .is_error());
        assert_eq!(rig.provider.tunnels(), vec![(SQL.to_owned(), 1)]);
        // A browse call hangs in the driver, holding the session lock.
        let browse = spawn_call(layer, "list_namespaces", id_args());
        hung.browse_entered.wait();
        // The agent closes the session: the slot goes at once, the disconnect waits for the lock.
        let close = spawn_call(layer, "close_session", id_args());
        until("the slot to be removed", || !layer.is_open(SQL));
        // The agent opens the connection again, while the first close is still in progress.
        let reopen = spawn_call(layer, "open_session", id_args());
        until("the reopen to start", || {
            layer.gate_waiters(SQL) == 1 || rig.provider.opens.load(Ordering::SeqCst) == 2
        });
        hung.browse_release.open();
        let reopened = reopen.join().unwrap();
        assert!(!reopened.is_error(), "{reopened:?}");
        assert!(!close.join().unwrap().is_error());
        let _ = browse.join().unwrap();
        // `open: true` means a working session with ITS tunnel; the first close dropped its own only.
        assert!(layer.is_open(SQL));
        assert_eq!(rig.provider.tunnels(), vec![(SQL.to_owned(), 2)]);
        assert_eq!(*lock(&rig.provider.closed_generations), vec![1]);
        assert!(!layer.call("list_namespaces", id_args()).is_error());
        assert_eq!(layer.gates_in_use(), 0);
    }

    #[test]
    fn close_all_waits_for_an_open_in_flight_and_closes_what_it_opened() {
        let provider = TunnelProvider::with_sessions(vec![fake()]);
        provider.block_first_open.store(true, Ordering::SeqCst);
        let rig = rig(AgentAccess::Read, provider);
        let layer = &rig.layer;
        let open = spawn_call(layer, "open_session", id_args());
        rig.provider.entered.wait();
        let closer = {
            let layer = Arc::clone(layer);
            std::thread::spawn(move || layer.close_all())
        };
        until("close_all to queue behind the open", || {
            layer.gate_waiters(SQL) == 1
        });
        rig.provider.gate_open.open();
        assert!(!open.join().unwrap().is_error());
        closer.join().unwrap();
        // Closed, tunnel included; never open without a tunnel.
        assert!(!layer.is_open(SQL));
        assert!(rig.provider.tunnels().is_empty());
        assert_eq!(*lock(&rig.provider.closed_generations), vec![1]);
        assert_eq!(layer.gates_in_use(), 0);
    }

    // ---- B-2 ---------------------------------------------------------------------------------

    #[test]
    fn a_session_found_under_an_older_revision_is_closed_not_used() {
        use crate::ports::Revision;
        let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
        let provider = TunnelProvider::with_sessions(vec![session.clone() as Arc<dyn Session>]);
        let rig = rig(AgentAccess::Read, provider);
        rig.catalog
            .update(SQL, |info| info.revision = Revision::new(1));
        assert!(!rig.layer.call("open_session", id_args()).is_error());
        // The statement's own lookups: the catalog said revision 2 (the connection was edited), the
        // slot it finds was opened under 1.
        let mut edited = rig.catalog.get(SQL).unwrap();
        edited.revision = Revision::new(2);
        let error = rig
            .layer
            .open_slot(&edited, &mut Trace::default())
            .err()
            .expect("not served");
        assert_eq!(error.code(), "agent.not_connected");
        assert!(!rig.layer.is_open(SQL));
        assert_eq!(session.run_count(), 0);
        assert_eq!(*lock(&rig.provider.closed_generations), vec![1]);
        assert!(rig.provider.tunnels().is_empty());
    }

    #[test]
    fn an_open_that_waited_for_an_open_under_an_older_revision_replaces_it() {
        use crate::ports::Revision;
        let (first, second) = (
            FakeSession::new(ConnectionKind::PostgreSql).arc(),
            FakeSession::new(ConnectionKind::PostgreSql).arc(),
        );
        let provider = TunnelProvider::with_sessions(vec![first.clone(), second.clone()]);
        provider.block_first_open.store(true, Ordering::SeqCst);
        let rig = rig(AgentAccess::Read, provider);
        rig.catalog
            .update(SQL, |info| info.revision = Revision::new(1));
        let layer = &rig.layer;
        let early = spawn_call(layer, "open_session", id_args());
        rig.provider.entered.wait();
        // The connection is edited while the first open is connecting; a second open is authorised
        // under the new revision and waits for the first.
        rig.catalog
            .update(SQL, |info| info.revision = Revision::new(2));
        let late = spawn_call(layer, "open_session", id_args());
        until("the second open to wait", || layer.gate_waiters(SQL) == 1);
        rig.provider.gate_open.open();
        let (early, late) = (early.join().unwrap(), late.join().unwrap());
        assert!(!early.is_error() && !late.is_error(), "{early:?} {late:?}");
        // The second call did not report the session of the old target as open: it replaced it.
        let reported = |outcome: &ToolOutcome| match outcome {
            ToolOutcome::Success(value) => value["already_open"].as_bool().unwrap(),
            ToolOutcome::Failure(_) => unreachable!(),
        };
        assert!(!reported(&late), "the old session is not reused");
        assert_eq!(rig.provider.opens.load(Ordering::SeqCst), 2);
        assert_eq!(first.disconnects.load(Ordering::SeqCst), 1);
        assert_eq!(rig.provider.tunnels(), vec![(SQL.to_owned(), 2)]);
        assert!(!layer.call("list_namespaces", id_args()).is_error());
    }

    // ---- B-4 ---------------------------------------------------------------------------------

    #[test]
    fn the_active_lock_is_not_held_while_the_driver_cancels() {
        let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
        let in_hook = Arc::new(Latch::default());
        let hook_release = Arc::new(Latch::default());
        {
            let (in_hook, hook_release) = (Arc::clone(&in_hook), Arc::clone(&hook_release));
            session.set_behavior(Behavior::Script(Box::new(move |_, cancel| {
                // A driver cancel that takes a while.
                let started = Arc::clone(&in_hook);
                let (in_hook, hook_release) = (Arc::clone(&in_hook), Arc::clone(&hook_release));
                let _hook = cancel.on_cancel(move || {
                    in_hook.open();
                    hook_release.wait();
                });
                while !cancel.is_cancelled() {
                    std::thread::yield_now();
                }
                // The statement ends while the driver's cancel is running, not before it
                // started: a hook whose statement already finished is not run at all.
                started.wait();
                Err(AgentError::Cancelled)
            })));
        }
        let provider = TunnelProvider::with_sessions(vec![session.clone() as Arc<dyn Session>]);
        let rig = rig(AgentAccess::Read, provider);
        let layer = &rig.layer;
        assert!(!layer.call("open_session", id_args()).is_error());
        let (sender, receiver) = mpsc::channel();
        let statement = {
            let layer = Arc::clone(layer);
            std::thread::spawn(move || {
                let _ = sender.send(layer.call(
                    "execute_query",
                    json!({"connection_id": SQL, "statement": "SELECT 1"}),
                ));
            })
        };
        until("the statement to run", || session.run_count() == 1);
        let revoker = {
            let layer = Arc::clone(layer);
            std::thread::spawn(move || layer.revoke(SQL))
        };
        in_hook.wait();
        // The driver is still cancelling; the statement can finish all the same.
        let outcome = receiver
            .recv_timeout(WAIT)
            .expect("the statement finished while the cancel was still running");
        assert_eq!(outcome.code(), Some("agent.cancelled"));
        hook_release.open();
        revoker.join().unwrap();
        statement.join().unwrap();
    }

    struct HoldApproval {
        asked: Latch,
        answer: Latch,
    }

    impl WriteApprover for HoldApproval {
        fn approve(&self, _request: &WriteApproval<'_>, _cancel: &CancelToken) -> ApprovalDecision {
            self.asked.open();
            self.answer.wait();
            ApprovalDecision::Approve
        }
    }

    #[test]
    fn a_session_revoked_before_the_statement_stores_its_token_does_not_run_it() {
        let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
        let provider = TunnelProvider::with_sessions(vec![session.clone() as Arc<dyn Session>]);
        let approver = Arc::new(HoldApproval {
            asked: Latch::default(),
            answer: Latch::default(),
        });
        let catalog = FakeCatalog::new(vec![connection(
            SQL,
            ConnectionKind::PostgreSql,
            Some(AgentAccess::ReadWrite),
        )]);
        let layer = Arc::new(
            ToolLayer::new(catalog, provider.clone(), "test").with_approver(approver.clone()),
        );
        assert!(!layer.call("open_session", id_args()).is_error());
        let statement = spawn_call(
            &layer,
            "execute_query",
            json!({"connection_id": SQL, "statement": "INSERT INTO t VALUES (1)"}),
        );
        approver.asked.wait();
        // The statement holds its slot and its approval is pending: no token stored yet.
        layer.revoke(SQL);
        approver.answer.open();
        let outcome = statement.join().unwrap();
        assert_eq!(outcome.code(), Some("agent.not_connected"), "{outcome:?}");
        assert_eq!(session.run_count(), 0, "nothing ran on the closed session");
        assert!(provider.tunnels().is_empty());
    }

    // ---- B-7 ---------------------------------------------------------------------------------

    #[test]
    fn the_gate_map_is_as_large_as_the_work_in_flight_and_survives_errors_and_panics() {
        let provider = TunnelProvider::with_sessions(vec![fake(), fake(), fake()]);
        let rig = rig(AgentAccess::Read, provider);
        let layer = &rig.layer;
        // An open that fails leaves the gate usable and no entry behind.
        rig.provider.fail_next_open.store(true, Ordering::SeqCst);
        assert_eq!(
            layer.call("open_session", id_args()).code(),
            Some("connect.failed")
        );
        assert_eq!(layer.gates_in_use(), 0);
        // So does one that panics inside the provider.
        rig.provider.panic_next_open.store(true, Ordering::SeqCst);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            layer.call("open_session", id_args())
        }));
        assert!(panicked.is_err());
        assert_eq!(layer.gates_in_use(), 0);
        // The gate still works: open, a repeated open, close, revoke.
        assert!(!layer.call("open_session", id_args()).is_error());
        assert!(!layer.call("open_session", id_args()).is_error());
        assert_eq!(layer.gates_in_use(), 0, "idle while the session is open");
        layer.revoke(SQL);
        layer.revoke("never-opened");
        assert!(!layer.call("close_session", id_args()).is_error());
        assert_eq!(layer.gates_in_use(), 0);
        assert!(!layer.is_open(SQL));
    }

    // ---- A-2b --------------------------------------------------------------------------------

    #[test]
    fn a_cancelled_open_waiting_behind_another_open_returns_cancelled_at_once() {
        let provider = TunnelProvider::with_sessions(vec![fake()]);
        provider.block_first_open.store(true, Ordering::SeqCst);
        let rig = rig(AgentAccess::Read, provider);
        let layer = &rig.layer;
        let first = spawn_call(layer, "open_session", id_args());
        rig.provider.entered.wait();
        let token = CancelToken::new();
        let (sender, receiver) = mpsc::channel();
        let waiting = {
            let (layer, token) = (Arc::clone(layer), token.clone());
            std::thread::spawn(move || {
                let _ = sender.send(layer.call_cancellable("open_session", id_args(), &token));
            })
        };
        until("the second open to wait", || layer.gate_waiters(SQL) == 1);
        token.cancel();
        // The first open is still connecting.
        let outcome = receiver
            .recv_timeout(WAIT)
            .expect("the cancelled call came back while the gate was held");
        assert_eq!(outcome.code(), Some("agent.cancelled"));
        waiting.join().unwrap();
        assert_eq!(rig.provider.opens.load(Ordering::SeqCst), 1);
        rig.provider.gate_open.open();
        assert!(!first.join().unwrap().is_error());
        assert!(layer.is_open(SQL));
        assert_eq!(layer.gates_in_use(), 0);
        // A token cancelled before the call even starts never takes the gate or connects.
        layer.revoke(SQL);
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let outcome = layer.call_cancellable("open_session", id_args(), &cancelled);
        assert_eq!(outcome.code(), Some("agent.cancelled"));
        assert_eq!(rig.provider.opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_browse_call_whose_token_is_cancelled_does_not_start() {
        let session = LockedSession::new(true);
        let provider = TunnelProvider::with_sessions(vec![session.clone() as Arc<dyn Session>]);
        let rig = rig(AgentAccess::Read, provider);
        assert!(!rig.layer.call("open_session", id_args()).is_error());
        let cancelled = CancelToken::new();
        cancelled.cancel();
        // `hang` is set: had a call reached the driver it would block forever.
        for (tool, args) in [
            ("list_namespaces", id_args()),
            ("list_tables", id_args()),
            (
                "describe_table",
                json!({"connection_id": SQL, "table": "t", "database": "main"}),
            ),
            (
                "get_schema_context",
                json!({"connection_id": SQL, "database": "main"}),
            ),
        ] {
            let outcome = rig.layer.call_cancellable(tool, args, &cancelled);
            assert_eq!(outcome.code(), Some("agent.cancelled"), "{tool}");
        }
    }
}
