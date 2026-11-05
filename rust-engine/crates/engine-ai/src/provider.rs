//! The provider-neutral side: what one model turn takes and returns, the typed errors, the key
//! source. [`crate::anthropic`] is the first [`ChatProvider`].

use crate::conversation::{HistoryForm, Message};
use crate::key::ApiKey;
use engine_agent::{CancelToken, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::Duration;

/// How much the model thinks. Sent as `output_config.effort`, always explicit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Token counts of one turn (summable).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    /// The model or its safety classifier declined; `category` is `stop_details.category`
    /// (`cyber`, `bio`, ...) when the API gave one.
    Refusal {
        category: Option<String>,
    },
    ModelContextWindowExceeded,
    /// A reason this crate does not know, as sent.
    Other(String),
}

impl StopReason {
    pub(crate) fn from_wire(reason: &str, category: Option<String>) -> Self {
        match reason {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            "stop_sequence" => Self::StopSequence,
            "pause_turn" => Self::PauseTurn,
            "refusal" => Self::Refusal { category },
            "model_context_window_exceeded" => Self::ModelContextWindowExceeded,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// The system prompt: the stable core, then the per-conversation context (connection, schema).
#[derive(Debug, Clone, Copy)]
pub struct SystemPrompt<'a> {
    pub core: &'a str,
    pub context: Option<ContextBlock<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub struct ContextBlock<'a> {
    pub text: &'a str,
    /// The block is big and frozen for the conversation, so it carries a cache breakpoint.
    pub cacheable: bool,
}

/// Everything one model turn is built from. Borrowed: nothing is copied to make a request.
#[derive(Debug, Clone, Copy)]
pub struct TurnRequest<'a> {
    pub model: &'a str,
    pub effort: Effort,
    pub system: SystemPrompt<'a>,
    pub tools: &'a [ToolSpec],
    pub messages: &'a [Message],
    /// How the history is written (see [`HistoryForm`]).
    pub history: HistoryForm,
}

/// A tool call whose input was not valid JSON (an object); the stored block has `input: {}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidToolInput {
    pub tool_use_id: String,
    pub raw: String,
}

/// A completed model turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnResponse {
    /// The content blocks as the provider sent them (see [`crate::Message::Assistant`]).
    pub content: Vec<Value>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// The model that produced the message, as reported.
    pub model: Option<String>,
    /// A fallback model served this turn (`usage.iterations` has a `fallback_message`).
    pub served_by_fallback: bool,
    /// The model the last `fallback` block names as the one that continued, when there is one.
    pub fallback_to: Option<String>,
    pub invalid_tool_inputs: Vec<InvalidToolInput>,
    pub request_id: Option<String>,
    /// How many thinking blocks of the history the API dropped (`input_transformations` entries
    /// of type `thinking_dropped`): they no longer matched the conversation before them, or the
    /// model that made them. Dropped blocks are not billed; the model did not see them.
    pub thinking_dropped: usize,
}

/// Why a request is waiting before it is sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryCause {
    RateLimited,
    Overloaded,
    Server,
    /// The connection failed before any response byte.
    Transport,
}

/// An optional request feature the API turned out not to accept; the provider leaves it out of
/// this and every later request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFeature {
    /// `thinking.block_binding` and its beta header (a stale thinking block is dropped by the API
    /// instead of refusing the request).
    ThinkingBinding,
    /// `fallbacks: "default"` and its beta header (server-side fallback).
    ServerFallback,
}

/// What a provider reports while a turn streams. Text is borrowed from the parser's buffers:
/// copy it if it must outlive the call.
#[derive(Debug)]
pub enum StreamEvent<'a> {
    TextDelta(&'a str),
    ThinkingDelta(&'a str),
    ToolCallStarted {
        id: &'a str,
        name: &'a str,
    },
    /// The tool block is complete. `valid` is false when the input was not a JSON object (then
    /// `input` is `{}`).
    ToolInputReady {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
        valid: bool,
    },
    /// The request failed in a way that is retried; the wait starts now. `attempt` counts from 1.
    Retrying {
        attempt: u32,
        after: Duration,
        cause: RetryCause,
    },
    /// The API refused a feature the request carried; it is left out from now on and the request
    /// is sent again.
    FeatureDropped(ApiFeature),
}

/// One model entry of the provider's catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// What a provider resolves once for a run (the API key), so that the tool rounds of one run do
/// not ask the key store again. Opaque, and bound to the provider that issued it: another provider
/// refuses it ([`ProviderError::ForeignRunSession`]) instead of sending this one's key to its own
/// host.
#[derive(Default)]
pub struct RunSession {
    pub(crate) key: Option<ApiKey>,
    /// The id of the provider that issued the session; `0`: none (the default session).
    pub(crate) issuer: u64,
}

impl std::fmt::Debug for RunSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunSession").finish_non_exhaustive()
    }
}

pub trait ChatProvider: Send + Sync {
    /// Streams one model turn. Returns when the turn is complete; on any error nothing of the
    /// turn is to be kept. Blocking: call it from a worker thread.
    fn stream_turn(
        &self,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError>;

    /// Resolves what every turn of one run needs, once. The assistant calls it at the start of
    /// `run` and `resume`. A provider that needs nothing keeps the default.
    fn begin_run(&self) -> Result<RunSession, ProviderError> {
        Ok(RunSession::default())
    }

    /// [`stream_turn`](Self::stream_turn) inside a run started with
    /// [`begin_run`](Self::begin_run).
    fn stream_turn_in_run(
        &self,
        _run: &RunSession,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        self.stream_turn(request, on_event, cancel)
    }

    fn list_models(&self, cancel: &CancelToken) -> Result<Vec<ModelInfo>, ProviderError>;
}

/// Where the API key comes from (the OS keychain in the app). Asked at request time, never cached
/// by this crate.
pub trait KeySource: Send + Sync {
    fn api_key(&self) -> Result<ApiKey, KeyError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("no API key is set")]
    NotSet,
    #[error("the API key is empty")]
    Empty,
    /// The store could not be read (the text must not contain the key).
    #[error("the API key could not be read: {0}")]
    Unavailable(String),
}

/// What the API said about a failed request.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiFailure {
    /// The HTTP status; `None` for an `error` event inside a stream that answered 200.
    pub status: Option<u16>,
    /// The API's `error.message` (cut to a sane length). Never the request.
    pub message: String,
    /// The `request-id` response header (or the body's `request_id`).
    pub request_id: Option<String>,
    /// The `retry-after` header, when it was a number of seconds.
    pub retry_after: Option<Duration>,
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(status) = self.status {
            write!(f, " (HTTP {status}")?;
            if let Some(id) = &self.request_id {
                write!(f, ", request-id {id}")?;
            }
            f.write_str(")")?;
        } else if let Some(id) = &self.request_id {
            write!(f, " (request-id {id})")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiFailure")
            .field("status", &self.status)
            .field("message", &self.message)
            .field("request_id", &self.request_id)
            .field("retry_after", &self.retry_after)
            .finish()
    }
}

/// The response stream did not follow the protocol.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("an event could not be read: {0}")]
    MalformedEvent(String),
    #[error("an event carried text that is not UTF-8")]
    InvalidUtf8,
    #[error("an event was larger than {limit} bytes")]
    EventTooLarge { limit: usize },
    #[error("`{0}` arrived before message_start")]
    BeforeMessageStart(&'static str),
    #[error("message_start arrived twice")]
    SecondMessageStart,
    #[error("content block {0} started twice")]
    DuplicateBlock(usize),
    #[error("an event refers to content block {0}, which was not started")]
    UnknownBlock(usize),
    #[error("a delta does not fit content block {0}")]
    DeltaMismatch(usize),
    #[error("content block {0} is not an object with a type")]
    MalformedBlock(usize),
    #[error("a tool_use block {0} has no id or no name")]
    BadToolUse(usize),
    #[error("two tool_use blocks carry the id of block {0}")]
    DuplicateToolUseId(usize),
    #[error("content block {0} received a delta after it was closed")]
    DeltaAfterStop(usize),
    #[error("the response is larger than {limit} bytes")]
    TurnTooLarge { limit: usize },
    #[error("the message has more than {limit} content blocks")]
    TooManyBlocks { limit: usize },
    #[error("more than {limit} events in a row carried no content")]
    TooManyIdleEvents { limit: usize },
    #[error("the response body could not be read: {0}")]
    MalformedBody(String),
    #[error("the list has more than {limit} pages")]
    TooManyPages { limit: usize },
    #[error("the message ended without a stop reason")]
    MissingStopReason,
    #[error("the stream ended before message_stop")]
    Truncated,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    Authentication(ApiFailure),
    #[error("permission denied: {0}")]
    Permission(ApiFailure),
    #[error("not found: {0}")]
    NotFound(ApiFailure),
    #[error("the request was refused as invalid: {0}")]
    BadRequest(ApiFailure),
    #[error("the request is too large: {0}")]
    TooLarge(ApiFailure),
    #[error("rate limited: {0}")]
    RateLimited(ApiFailure),
    #[error("the service is overloaded: {0}")]
    Overloaded(ApiFailure),
    #[error("server error: {0}")]
    Server(ApiFailure),
    /// Any other status or error type the API refused with (402, 409, ...).
    #[error("the request was rejected: {0}")]
    Rejected(ApiFailure),
    #[error("transport failure: {source}{}", RequestIdSuffix(.request_id))]
    Transport {
        source: crate::transport::TransportError,
        /// Set when the failure came after the response head (a broken stream).
        request_id: Option<String>,
    },
    #[error("protocol error: {source}{}", RequestIdSuffix(.request_id))]
    Protocol {
        source: ProtocolError,
        request_id: Option<String>,
    },
    /// A [`RunSession`] of another provider was passed in.
    #[error("the run session was issued by another provider")]
    ForeignRunSession,
    /// The request could not be built (a local failure; nothing was sent).
    #[error("the request could not be serialized: {0}")]
    Serialize(String),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Key(#[from] KeyError),
}

/// ` (request-id ..)` when there is an id.
struct RequestIdSuffix<'a>(&'a Option<String>);

impl fmt::Display for RequestIdSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(id) => write!(f, " (request-id {id})"),
            None => Ok(()),
        }
    }
}

impl ProviderError {
    /// The `request-id` of the failed request, when the API gave one.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Authentication(f)
            | Self::Permission(f)
            | Self::NotFound(f)
            | Self::BadRequest(f)
            | Self::TooLarge(f)
            | Self::RateLimited(f)
            | Self::Overloaded(f)
            | Self::Server(f)
            | Self::Rejected(f) => f.request_id.as_deref(),
            Self::Transport { request_id, .. } | Self::Protocol { request_id, .. } => {
                request_id.as_deref()
            }
            Self::Serialize(_) | Self::ForeignRunSession | Self::Cancelled | Self::Key(_) => None,
        }
    }

    /// The API's failure details, for the errors that come from an API answer.
    pub fn api_failure(&self) -> Option<&ApiFailure> {
        match self {
            Self::Authentication(f)
            | Self::Permission(f)
            | Self::NotFound(f)
            | Self::BadRequest(f)
            | Self::TooLarge(f)
            | Self::RateLimited(f)
            | Self::Overloaded(f)
            | Self::Server(f)
            | Self::Rejected(f) => Some(f),
            _ => None,
        }
    }

    /// Whether the request may be sent again as it is (429 and every 5xx incl. 529).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited(_) | Self::Overloaded(_) | Self::Server(_)
        )
    }

    /// The API refused the history because a `thinking` block no longer matches the conversation
    /// before it (the system prompt, the tools or an earlier message changed).
    pub(crate) fn is_thinking_binding_mismatch(&self) -> bool {
        let Self::BadRequest(failure) = self else {
            return false;
        };
        let message = failure.message.to_lowercase();
        message.contains("bound to a different conversation")
            || (message.contains("invalid `signature`") && message.contains("thinking"))
    }

    /// The API refused the request because of a beta feature this crate sends: the message names
    /// the `anthropic-beta` header, a beta value of this crate, or the field of one. `features`
    /// lists every such feature with its beta value, its request field and whether this request
    /// carried it. Returns the carried features to leave out: those the message names, or, when
    /// it names only the header and no beta in particular, all carried ones. A feature that was
    /// left out before and is named again yields nothing (the refusal is not about the
    /// request any more). Not for a stale-thinking refusal.
    pub(crate) fn rejected_features(
        &self,
        features: &[(ApiFeature, &str, &str, bool)],
    ) -> Vec<ApiFeature> {
        let Self::BadRequest(failure) = self else {
            return Vec::new();
        };
        if self.is_thinking_binding_mismatch() {
            return Vec::new();
        }
        let message = failure.message.to_lowercase();
        let named = |beta: &str, field: &str| message.contains(beta) || message.contains(field);
        let carried = features.iter().filter(|(.., sent)| *sent);
        if features
            .iter()
            .any(|(_, beta, field, _)| named(beta, field))
        {
            carried
                .filter(|(_, beta, field, _)| named(beta, field))
                .map(|(feature, ..)| *feature)
                .collect()
        } else if message.contains("anthropic-beta") {
            carried.map(|(feature, ..)| *feature).collect()
        } else {
            Vec::new()
        }
    }

    /// The model does not take `system` messages inside the conversation.
    pub(crate) fn is_system_role_unsupported(&self) -> bool {
        let Self::BadRequest(failure) = self else {
            return false;
        };
        let message = failure.message.to_lowercase();
        message.contains("system") && message.contains("role") && message.contains("not supported")
    }

    pub(crate) fn retry_cause(&self) -> Option<RetryCause> {
        match self {
            Self::RateLimited(_) => Some(RetryCause::RateLimited),
            Self::Overloaded(_) => Some(RetryCause::Overloaded),
            Self::Server(_) => Some(RetryCause::Server),
            _ => None,
        }
    }
}
