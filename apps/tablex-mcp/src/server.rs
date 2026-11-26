//! The MCP handler: `tools/list` and `tools/call` mapped onto [`ToolLayer`]. It declares no tool
//! and decides nothing; every name, schema, policy check and error code is the layer's. The two
//! failures that are this program's own (too many calls at once, a call that panicked) use the
//! layer's error shape.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine_agent::{CallOrigin, CancelToken, ToolError, ToolLayer, ToolOutcome, ToolSpec};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, InitializeRequestParams, InitializeResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, ResourceContents, ServerCapabilities, ServerConfig,
    Tool,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::approver::Caller;

/// What the model reads once, at `initialize`.
const INSTRUCTIONS: &str = "tablex database access for AI agents. Tools list the user's saved \
connections and open sessions on them; access is off by default; the user sets a default for all connections in tablex (Settings \u{2192} AI \u{2192} \
Default agent access) and can set it per connection (connection > Overrides > Agent access). Start with \
list_connections (the same list is the resource tablex://connections; tablex://<id>/schema and tablex://<id>/<database>/tables read a connection's schema). Query with execute_query on a SQL connection (execute_batch runs several statements in \
one call, with the changes in it approved once), execute_redis_command on Redis, and on MongoDB \
execute_query with a mongosh-style command such as db.orders.find({}) or db.orders.getIndexes() (or \
mongo_find, mongo_aggregate and mongo_count; mongo_insert, mongo_update and mongo_delete change documents). A statement or command that changes data may be put to the user for approval, \
through your client, or in the tablex app when your client cannot ask, when the connection's access is read_write (the user sets it in tablex: Settings \u{2192} AI, or per connection); it is never allowed on a connection of a production type. A refusal or a \
decline is final for that request: do not retry the statement or reword it to get around it, \
tell the user instead. On a PostgreSQL, MySQL, MariaDB, SQLite or DuckDB connection with read_write access, \
begin_transaction opens a transaction: each change in it is approved one by one and is not final until \
commit_transaction, which the user approves with the list of changes; roll back (rollback_transaction) \
what you do not need. execute_batch with use_transaction runs a whole script as one transaction that \
is committed, or rolled back when a statement fails, after a single approval. An unused transaction is \
rolled back after 5 minutes. MongoDB writes are not part of transactions. On DuckDB, SQL cannot read or write files or URLs for you \
(read_csv, read_parquet, COPY, ATTACH, INSTALL, LOAD and the like are refused at every access level): \
ask the user to open a file in tablex. To keep a `USE` or a harmless `SET` between calls (a database, a search_path, a time zone), open_session with pinned true returns a session_id for execute_query and the other statement tools; close_session ends it, and everything it held is gone. To show something to the \
user in the tablex app (which has to be running), open_table opens a table, view or collection in its \
sidebar and a data tab, and execute_and_show puts a query in an editor tab and runs it there, under \
the app's own rules: it runs a single read only; for a query that changes data, or one the user \
should review first, pass run false and tell the user to run it; on DuckDB it never runs, the \
user runs it. Neither returns data: use \
execute_query when you need the rows yourself. To change the user's saved connections, add_connection \
adds one (a uri or host, port, database and user; SQLite or DuckDB a path; the password as its own argument, \
never inside the uri), duplicate_connection copies one under a new name and remove_connection removes \
one with its saved passwords. The user is asked to approve every call, once per call, in your client \
(an \
unanswered or declined question changes nothing and is final: do not retry it). A new or copied \
connection has no access setting of its own and follows the user's settings for its type, so you may \
not be able to use it until the user enables agent access for it.";

/// The layer's code for a tool name it does not have; a protocol error here, not a tool result.
const UNKNOWN_TOOL: &str = "agent.unknown_tool";

/// Tool calls that may run at once. Each runs on tokio's blocking pool (see `main.rs` for the
/// arithmetic that keeps that pool from ever being exhausted); the 17th gets `agent.limit` at once
/// instead of queueing behind a statement that may run for the whole statement timeout.
pub const MAX_CONCURRENT_CALLS: usize = 16;

/// How long end-of-session cleanup (`ToolLayer::close_all`: driver disconnects, tunnels) may take
/// before the process gives up and exits.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// How often open transactions are checked for the idle timeout (the layer also checks at the
/// start of every call). Each check runs on the blocking pool, one at a time.
pub const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// Why a server cannot be built.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("the input schema of tool `{0}` is not a JSON object")]
    InputSchemaNotObject(&'static str),
}

/// How the client reaches the server; recorded in every audit line of its calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
}

impl Transport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}

/// One per connection to a client (over HTTP: one per session, see [`McpServer::for_session`]).
#[derive(Clone)]
pub struct McpServer {
    layer: Arc<ToolLayer>,
    tools: Arc<[Tool]>,
    /// Cancelled when the client goes away (stdin closed or unreadable) or the process is told to
    /// stop; every running call is cancelled with it.
    gone: CancellationToken,
    /// One permit per running call, held until the layer has returned.
    calls: Arc<Semaphore>,
    /// The client's `initialize` was answered; a second one is refused.
    initialized: Arc<AtomicBool>,
    shutdown_deadline: Duration,
    sweep_interval: Duration,
    /// Who the layer takes this client to be (see [`ToolLayer::call_as`]): its transactions and
    /// pinned sessions are its own. One client on stdio, so `""`; a server that shares the layer
    /// between clients gives each its own (see [`with_owner`](Self::with_owner)).
    owner: Arc<str>,
    transport: Transport,
    /// Over HTTP: ends what the session's client holds in the layer when the session is gone (the
    /// last handler of it is dropped: `DELETE`, idle timeout, shutdown).
    _end_of_client: Option<Arc<EndOfClient>>,
}

/// The pinned sessions and transactions of a client of the tool layer end with the client: nobody
/// else can use them, and each holds a connection to the user's database server.
struct EndOfClient {
    layer: Arc<ToolLayer>,
    owner: Arc<str>,
}

impl Drop for EndOfClient {
    fn drop(&mut self) {
        if !self.layer.holds_client(&self.owner) {
            return;
        }
        // Rolling back waits for the server: not on the thread that drops the handler.
        let (layer, owner) = (Arc::clone(&self.layer), Arc::clone(&self.owner));
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || layer.end_client(&owner));
            }
            Err(_) => layer.end_client(&owner),
        }
    }
}

/// Why `begin_transaction` and a pinned `open_session` are refused to a request that is not part
/// of a session.
const STATELESS_REFUSAL: &str = "this request is not part of an MCP session (the client did not \
initialize one: protocol revision 2026-07-28 and later are served one request at a time), so what \
it opens could never be named again by a later request and would only hold a connection; \
begin_transaction and open_session with pinned true need a client that keeps a session. Use \
execute_batch with use_transaction for a script that has to run as one transaction";

/// Whether the call opens something a later call has to name: a transaction or a pinned session.
fn opens_state(name: &str, arguments: &Value) -> bool {
    name == "begin_transaction"
        || (name == "open_session" && arguments.get("pinned").and_then(Value::as_bool) == Some(true))
}

impl McpServer {
    pub fn new(layer: Arc<ToolLayer>) -> Result<Self, ServerError> {
        let tools = layer
            .tools()
            .iter()
            .map(Self::describe)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            layer,
            tools,
            gone: CancellationToken::new(),
            calls: Arc::new(Semaphore::new(MAX_CONCURRENT_CALLS)),
            initialized: Arc::new(AtomicBool::new(false)),
            shutdown_deadline: SHUTDOWN_DEADLINE,
            sweep_interval: IDLE_SWEEP_INTERVAL,
            owner: Arc::from(""),
            transport: Transport::Stdio,
            _end_of_client: None,
        })
    }

    /// Names the client this server talks to for the layer: an opaque id, unique among the clients
    /// that share the layer, which only they can use or end the transactions and pinned sessions
    /// they open under.
    #[must_use]
    pub fn with_owner(mut self, owner: impl Into<Arc<str>>) -> Self {
        self.owner = owner.into();
        self
    }

    /// Records `transport` in the audit line of every call (stdio unless said otherwise).
    #[must_use]
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// The handler of one HTTP session: the layer, the tool list and the budget of running calls
    /// are shared with every other session (the blocking pool is sized for 16 calls in all, see
    /// `main.rs`), the handshake state is the session's own, and its calls are cancelled when the
    /// server as a whole is told to stop. It is also a client of its own for the layer (see
    /// [`ToolLayer::call_as`]): the transactions and pinned sessions it opens cannot be used,
    /// committed or closed from another session, whatever id that one sends.
    #[must_use]
    pub fn for_session(&self) -> Self {
        static SESSIONS: AtomicU64 = AtomicU64::new(0);
        let owner: Arc<str> = Arc::from(format!("http-{}", SESSIONS.fetch_add(1, Ordering::Relaxed)));
        Self {
            gone: self.gone.child_token(),
            initialized: Arc::new(AtomicBool::new(false)),
            _end_of_client: Some(Arc::new(EndOfClient {
                layer: Arc::clone(&self.layer),
                owner: Arc::clone(&owner),
            })),
            owner,
            ..self.clone()
        }
    }

    /// Replaces [`IDLE_SWEEP_INTERVAL`] (tests).
    #[must_use]
    pub fn with_idle_sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }

    pub(crate) fn sweep_interval(&self) -> Duration {
        self.sweep_interval
    }

    /// The permits of the running calls: all [`MAX_CONCURRENT_CALLS`] are back once every call
    /// has returned.
    pub(crate) fn calls(&self) -> Arc<Semaphore> {
        Arc::clone(&self.calls)
    }

    /// Replaces [`SHUTDOWN_DEADLINE`] (tests).
    #[must_use]
    pub fn with_shutdown_deadline(mut self, deadline: Duration) -> Self {
        self.shutdown_deadline = deadline;
        self
    }

    /// The token [`serve`](Self::serve) cancels on disconnect.
    pub fn gone(&self) -> &CancellationToken {
        &self.gone
    }

    pub fn layer(&self) -> &Arc<ToolLayer> {
        &self.layer
    }

    pub(crate) fn shutdown_deadline(&self) -> Duration {
        self.shutdown_deadline
    }

    /// The layer's spec as an MCP tool: name, description and both schemas as the layer has them.
    fn describe(spec: &ToolSpec) -> Result<Tool, ServerError> {
        let input = spec
            .input_schema
            .as_object()
            .ok_or(ServerError::InputSchemaNotObject(spec.name))?;
        let mut tool = Tool::new(spec.name, spec.description, input.clone());
        if let Some(output) = spec.output_schema.as_object() {
            tool = tool.with_raw_output_schema(Arc::new(output.clone()));
        }
        Ok(tool)
    }

    /// `isError` for a failure (the typed error stays in the content, for the model); the result
    /// as `structuredContent` too when the tool declares an output schema.
    fn result_of(outcome: &ToolOutcome, has_output_schema: bool) -> CallToolResult {
        if outcome.is_error() {
            CallToolResult::error(vec![ContentBlock::text(outcome.to_text())])
        } else if has_output_schema {
            CallToolResult::structured(outcome.to_json())
        } else {
            CallToolResult::success(vec![ContentBlock::text(outcome.to_text())])
        }
    }

    fn failure(code: &str, message: String, retryable: bool) -> ToolOutcome {
        ToolOutcome::Failure(ToolError {
            code: code.to_owned(),
            message,
            retryable,
        })
    }

    /// Runs the call on a blocking thread and waits for it. It is cancelled when the MCP request
    /// is (`notifications/cancelled`), when the client disconnects, and when this future is
    /// dropped; the wait continues after a cancel until the layer has returned, so the statement
    /// is really stopped and the session left consistent.
    async fn run_cancellable(
        &self,
        name: String,
        arguments: Value,
        request_cancelled: &CancellationToken,
        client: &Peer<RoleServer>,
    ) -> ToolOutcome {
        // What the client called itself at the handshake; the audit line records it as a hint.
        let origin = CallOrigin::new(
            self.transport.as_str(),
            client
                .peer_info()
                .as_deref()
                .map(|info| info.client_info.name.as_str()),
        );
        // No queueing: a full house is answered at once.
        let Ok(permit) = Arc::clone(&self.calls).try_acquire_owned() else {
            return Self::failure(
                "agent.limit",
                format!(
                    "too many tool calls are running ({MAX_CONCURRENT_CALLS}); wait for one to \
                     finish or cancel one"
                ),
                true,
            );
        };
        let token = CancelToken::new();
        let guard = token.cancel_on_drop();
        let layer = Arc::clone(&self.layer);
        let owner = Arc::clone(&self.owner);
        let call_token = token.clone();
        // The approver asks the user through this call's client; it runs on the blocking thread.
        let caller = Caller::new(client.clone(), tokio::runtime::Handle::current());
        let mut call = tokio::task::spawn_blocking(move || {
            let _caller = caller.enter();
            let outcome = layer.call_as_from(&owner, &name, arguments, &call_token, &origin);
            // Released when the layer has really returned, not when the handler gave up waiting.
            drop(permit);
            outcome
        });
        let joined = tokio::select! {
            joined = &mut call => joined,
            // Detached: the statement's hook is the driver's blocking cancel (a new connection to
            // the server), which must never run on this runtime's only thread.
            () = request_cancelled.cancelled() => {
                token.cancel_detached();
                call.await
            }
            () = self.gone.cancelled() => {
                token.cancel_detached();
                call.await
            }
        };
        guard.disarm();
        // A panic inside the call: no payload goes out (it may carry connection text).
        joined.unwrap_or_else(|_| {
            Self::failure(
                "mcp.internal",
                "the tool call failed inside tablex-mcp".to_owned(),
                false,
            )
        })
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(Implementation::new(crate::NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    /// The first `initialize` is answered with the server's info (rmcp has already recorded the
    /// client's capabilities from it). A second one is refused: the default handler would replace
    /// the capabilities the client declared, and with them what the approver believes about it
    /// (can it be asked?) for the rest of the session.
    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        if self.initialized.swap(true, Ordering::SeqCst) {
            return Err(ErrorData::invalid_request(
                "this session is already initialized",
                None,
            ));
        }
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Only the tools the user's allowlist lets through now.
        let allowed = self
            .tools
            .iter()
            .filter(|tool| self.layer.tool_allowed(&tool.name))
            .cloned()
            .collect();
        Ok(ListToolsResult::with_all_items(allowed))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let (layer, owner) = (Arc::clone(&self.layer), Arc::clone(&self.owner));
        let resources = tokio::task::spawn_blocking(move || crate::resources::list(&layer, &owner))
            .await
            .map_err(|_| ErrorData::internal_error("listing the resources failed inside tablex-mcp", None))?;
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(crate::resources::templates()))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        // Cancelled with the request, like a tool call.
        let token = CancelToken::new();
        let guard = token.cancel_on_drop();
        let (layer, owner, uri) = (Arc::clone(&self.layer), Arc::clone(&self.owner), request.uri);
        let call_token = token.clone();
        let mut read = tokio::task::spawn_blocking(move || crate::resources::read(&layer, &owner, &uri, &call_token));
        let joined = tokio::select! {
            joined = &mut read => joined,
            () = context.ct.cancelled() => {
                token.cancel_detached();
                read.await
            }
            () = self.gone.cancelled() => {
                token.cancel_detached();
                read.await
            }
        };
        guard.disarm();
        match joined {
            Ok(Ok(contents)) => Ok(ReadResourceResult::new(vec![ResourceContents::text(contents.text, contents.uri)
                .with_mime_type(contents.mime_type)])
            .into()),
            Ok(Err(crate::resources::ReadError::NotFound(message))) => Err(ErrorData::invalid_params(message, None)),
            Ok(Err(crate::resources::ReadError::Failed(message))) => Err(ErrorData::invalid_params(message, None)),
            Err(_) => Err(ErrorData::internal_error("reading the resource failed inside tablex-mcp", None)),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.into_owned();
        let has_output_schema = self
            .tools
            .iter()
            .find(|tool| tool.name == name.as_str())
            .is_some_and(|tool| tool.output_schema.is_some());
        let arguments = request.arguments.map_or(Value::Null, Value::Object);
        // A handler that never answered an `initialize` serves one request (HTTP, revision
        // 2026-07-28 and later): its owner is gone with the request.
        if self.transport == Transport::Http
            && !self.initialized.load(Ordering::SeqCst)
            && opens_state(&name, &arguments)
        {
            let outcome = Self::failure("agent.unsupported", STATELESS_REFUSAL.to_owned(), false);
            return Ok(Self::result_of(&outcome, has_output_schema).into());
        }
        let outcome = self
            .run_cancellable(name, arguments, &context.ct, &context.peer)
            .await;
        if outcome.code() == Some(UNKNOWN_TOOL) {
            let message = match &outcome {
                ToolOutcome::Failure(error) => error.message.clone(),
                ToolOutcome::Success(_) => String::new(),
            };
            return Err(ErrorData::invalid_params(message, None));
        }
        Ok(Self::result_of(&outcome, has_output_schema).into())
    }
}
