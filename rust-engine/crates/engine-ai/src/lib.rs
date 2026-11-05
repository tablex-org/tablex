//! The UI-free AI assistant core for TableX.
//!
//! * [`transport`]: the HTTP seam ([`HttpTransport`]); the app brings the client, tests script it.
//! * [`provider`]: the provider-neutral types and [`ChatProvider`], [`ProviderError`], [`KeySource`].
//! * [`anthropic`]: the Messages API (request, incremental SSE, retry) over the transport.
//! * [`openai`]: the Chat Completions API for OpenAI, OpenAI-compatible servers and Ollama (adapted
//!   from dbx, Apache-2.0), over the same transport, parser and retry.
//! * [`conversation`]: the append-only [`Conversation`], persisted as JSON.
//! * [`assistant`]: [`Assistant::run`], the tool loop over the agent [`engine_agent::ToolLayer`].
//!
//! No HTTP client, no async runtime, no UI. The API key is read from a [`KeySource`] when a request
//! is built and appears in no error, event, `Debug` output or log of this crate.

pub mod anthropic;
pub mod assistant;
pub mod conversation;
pub mod key;
pub mod openai;
pub mod prompt;
pub mod provider;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
pub mod transport;
mod wire;

pub use anthropic::{
    AnthropicConfig, AnthropicProvider, ConfigError, RetryPolicy, SleepOutcome, Sleeper,
    ThreadSleeper, DEFAULT_MAX_TOKENS, DEFAULT_MODEL,
};
pub use assistant::{
    Assistant, AssistantConfig, AssistantError, AssistantEvent, DoneReason, Notice, RunSummary,
    UserInput,
};
pub use conversation::{
    Conversation, ConversationError, HistoryForm, Message, Scope, SystemContext, ToolUse, UserBlock,
};
pub use key::ApiKey;
pub use openai::{Flavor, OpenAiConfig, OpenAiProvider, OLLAMA_BASE_URL, OPENAI_BASE_URL};
pub use provider::{
    ApiFailure, ApiFeature, ChatProvider, ContextBlock, Effort, InvalidToolInput, KeyError,
    KeySource, ModelInfo, ProtocolError, ProviderError, RetryCause, RunSession, StopReason,
    StreamEvent, SystemPrompt, TurnRequest, TurnResponse, Usage,
};
pub use transport::{
    Header, HeaderValue, HttpRequest, HttpResponse, HttpTransport, Method, Timeouts, TransportError,
};
