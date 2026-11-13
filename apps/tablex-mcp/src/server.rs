//! The MCP handler: `tools/list` and `tools/call` mapped onto [`ToolLayer`]. It declares no tool
//! and decides nothing; every name, schema, policy check and error code is the layer's. The two
//! failures that are this program's own (too many calls at once, a call that panicked) use the
//! layer's error shape.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine_agent::{CancelToken, ToolError, ToolLayer, ToolOutcome, ToolSpec};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::approver::Caller;

/// What the model reads once, at `initialize`.
const INSTRUCTIONS: &str = "TableX database access for AI agents. Tools list the user's saved \
connections and open sessions on them; access is off by default; the user sets a default for all connections in TableX (Settings \u{2192} AI \u{2192} \
Default agent access) and can set it per connection (connection > Overrides > Agent access). Start with \
list_connections. A statement that changes data may be put to the user for approval, through \
your client, when the connection's access is read_write and the user has allowed changes through \
MCP in TableX (Settings → AI); it is never allowed on a connection of a production type. A refusal or a \
decline is final for that request: do not retry the statement or reword it to get around it, \
tell the user instead.";

/// The layer's code for a tool name it does not have; a protocol error here, not a tool result.
const UNKNOWN_TOOL: &str = "agent.unknown_tool";

/// Tool calls that may run at once. Each runs on tokio's blocking pool (see `main.rs` for the
/// arithmetic that keeps that pool from ever being exhausted); the 17th gets `agent.limit` at once
/// instead of queueing behind a statement that may run for the whole statement timeout.
pub const MAX_CONCURRENT_CALLS: usize = 16;

/// How long end-of-session cleanup (`ToolLayer::close_all`: driver disconnects, tunnels) may take
/// before the process gives up and exits.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// Why a server cannot be built.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("the input schema of tool `{0}` is not a JSON object")]
    InputSchemaNotObject(&'static str),
}

/// One per connection to a client.
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
        })
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
        let call_token = token.clone();
        // The approver asks the user through this call's client; it runs on the blocking thread.
        let caller = Caller::new(client.clone(), tokio::runtime::Handle::current());
        let mut call = tokio::task::spawn_blocking(move || {
            let _caller = caller.enter();
            let outcome = layer.call_cancellable(&name, arguments, &call_token);
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
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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
        Ok(ListToolsResult::with_all_items(self.tools.to_vec()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.into_owned();
        let has_output_schema = self
            .tools
            .iter()
            .find(|tool| tool.name == name.as_str())
            .is_some_and(|tool| tool.output_schema.is_some());
        let arguments = request.arguments.map_or(Value::Null, Value::Object);
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
        Ok(Self::result_of(&outcome, has_output_schema))
    }
}
