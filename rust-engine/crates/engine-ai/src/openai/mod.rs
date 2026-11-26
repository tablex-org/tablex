//! The Chat Completions API over an injected [`HttpTransport`]: OpenAI itself, any server that
//! speaks its protocol (vLLM, LM Studio, gateways) and Ollama through its OpenAI-compatible `/v1`.
//!
//! * [`endpoint`]: the chat, model-list and Ollama show URLs derived from the one base URL.
//! * [`request`] and [`effort`]: the body of `POST .../chat/completions`, and the model-name rules
//!   for the token-limit field and the thinking switch.
//! * [`stream`] and [`body`]: the assembly of one assistant turn from the SSE stream, and the
//!   grace period for servers that never close it.
//! * [`models`]: the model list, and Ollama's capability probe.
//!
//! The shared parser, retry and error mapping are those of the Anthropic provider
//! ([`crate::wire`]). A base URL must be `https`, or `http` to a loopback host, and carry no
//! credentials, query or fragment; the provider keeps it trimmed and without a trailing `/`. The
//! key (a bearer token) goes to the host of the base URL and nowhere else: every URL the provider
//! derives is checked to have the origin of the base. Only OpenAI needs a key; a key that is set
//! is sent to the other flavors too (a proxy may want it).

mod body;
mod effort;
mod endpoint;
mod models;
mod request;
mod stream;
#[cfg(test)]
mod tests;

use crate::conversation::{Block, Message};
use crate::key::ApiKey;
use crate::provider::{
    ChatProvider, KeyError, KeySource, ModelInfo, ProtocolError, ProviderError, RunSession,
    StreamEvent, TurnRequest, TurnResponse,
};
use crate::transport::{Header, HttpRequest, HttpResponse, HttpTransport, Method, Timeouts};
use crate::wire::errors::{Scrub, MAX_TOKEN_CHARS};
use crate::wire::retry::{RetryPolicy, RetryState, Sleeper, ThreadSleeper};
use crate::wire::sse::SseParser;
use crate::wire::{
    next_provider_id, pump, validate_base_url, ConfigError, Exchange, PumpError, StreamError,
    TurnLimits,
};
use body::GraceBody;
use endpoint::Endpoints;
use engine_agent::CancelToken;
use request::Body;
use std::cell::Cell;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream::TurnAssembler;

pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";

/// How long the reader waits for the rest of a body once the turn's finish reason has come.
const FINISH_GRACE: Duration = Duration::from_secs(1);

/// Which server the provider talks to; they differ in the key, the model list and the thinking
/// switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    OpenAi,
    /// Any other server that speaks the protocol.
    Compatible,
    Ollama,
}

impl Flavor {
    /// The address to start from; there is none for a server of the user's own.
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some(OPENAI_BASE_URL),
            Self::Ollama => Some(OLLAMA_BASE_URL),
            Self::Compatible => None,
        }
    }

    pub fn requires_key(self) -> bool {
        self == Self::OpenAi
    }
}

/// Holds no key: its `Debug` output is safe to log.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Trimmed, without a trailing `/`, once the provider is built ([`OpenAiProvider::new`]).
    pub base_url: String,
    pub flavor: Flavor,
    pub max_tokens: u32,
    /// Ask the model to think (with the request's effort where the model has levels); off asks it
    /// not to, where the server has a way to say so.
    pub enable_thinking: bool,
    pub retry: RetryPolicy,
    pub timeouts: Timeouts,
}

impl OpenAiConfig {
    pub fn for_flavor(flavor: Flavor) -> Self {
        Self {
            base_url: flavor.default_base_url().unwrap_or_default().to_owned(),
            flavor,
            max_tokens: 4096,
            enable_thinking: true,
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
        }
    }

    /// The base URL as it is used everywhere: trimmed, without trailing slashes.
    fn normalized_base_url(&self) -> &str {
        self.base_url.trim().trim_end_matches('/')
    }

    /// Checks the base URL like the Anthropic provider does: `https`, or `http` to a loopback host
    /// (the key must not travel in clear text), with a host, without credentials, query or
    /// fragment.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_base_url(self.normalized_base_url())
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self::for_flavor(Flavor::OpenAi)
    }
}

pub struct OpenAiProvider {
    transport: Arc<dyn HttpTransport>,
    keys: Arc<dyn KeySource>,
    sleeper: Arc<dyn Sleeper>,
    config: OpenAiConfig,
    endpoints: Endpoints,
    limits: TurnLimits,
    finish_grace: Duration,
    /// Tells a [`RunSession`] which provider issued it.
    id: u64,
}

/// A stream that failed: before anything was delivered it may be retried like an HTTP error.
enum Failure {
    BeforeDelivery(ProviderError),
    Fatal(ProviderError),
}

impl OpenAiProvider {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        keys: Arc<dyn KeySource>,
        mut config: OpenAiConfig,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        // What was validated is what is used from now on.
        config.base_url = config.normalized_base_url().to_owned();
        let endpoints = Endpoints::new(&config.base_url, config.flavor);
        if !endpoints.same_origin_as(&config.base_url) {
            return Err(ConfigError::InvalidBaseUrl);
        }
        Ok(Self {
            transport,
            keys,
            sleeper: Arc::new(ThreadSleeper),
            limits: TurnLimits::for_max_tokens(config.max_tokens),
            finish_grace: FINISH_GRACE,
            config,
            endpoints,
            id: next_provider_id(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_limits(mut self, limits: TurnLimits) -> Self {
        self.limits = limits;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_finish_grace(mut self, grace: Duration) -> Self {
        self.finish_grace = grace;
        self
    }

    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    pub fn config(&self) -> &OpenAiConfig {
        &self.config
    }

    /// The key to send, if any. OpenAI needs one: none or an empty one is an error before
    /// anything is sent. The other flavors do without. Any other failure of the key source is
    /// always returned.
    fn key(&self) -> Result<Option<ApiKey>, KeyError> {
        let needed = self.config.flavor.requires_key();
        match self.keys.api_key() {
            Ok(key) if !key.is_empty() => Ok(Some(key)),
            Ok(_) if needed => Err(KeyError::Empty),
            Err(error @ (KeyError::NotSet | KeyError::Empty)) if needed => Err(error),
            Ok(_) | Err(KeyError::NotSet | KeyError::Empty) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn headers(&self, key: Option<&ApiKey>, has_body: bool) -> Vec<Header> {
        let mut headers = Vec::with_capacity(2);
        if let Some(key) = key {
            headers.push(Header::secret(
                "authorization",
                ApiKey::new(format!("Bearer {}", key.expose())),
            ));
        }
        if has_body {
            headers.push(Header::plain("content-type", "application/json"));
        }
        headers
    }

    fn exchange(&self) -> Exchange<'_> {
        Exchange {
            transport: &*self.transport,
            sleeper: &*self.sleeper,
            retry: &self.config.retry,
        }
    }

    /// Reads the SSE body to `[DONE]`, or to its end once a finish reason was seen, within the
    /// limits of a turn.
    fn consume(
        &self,
        response: HttpResponse,
        scrub: &Scrub,
        taken: &HashSet<&str>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        connection: &CancelToken,
    ) -> Result<TurnResponse, Failure> {
        let request_id = request_id(&response, scrub);
        let protocol = |source: ProtocolError, request_id: &Option<String>| {
            Failure::Fatal(ProviderError::Protocol {
                source,
                request_id: request_id.clone(),
            })
        };
        let finished_at = Cell::new(None);
        let mut body = GraceBody::new(
            response.body,
            &finished_at,
            self.finish_grace,
            connection.clone(),
        );
        let mut parser = SseParser::default();
        let mut assembler = TurnAssembler::new(self.limits, scrub, taken);
        let fed = pump(
            &mut body,
            &mut parser,
            self.limits.max_bytes,
            scrub,
            &request_id,
            connection,
            &mut |event| {
                let done = assembler.handle(event.data, on_event)?;
                if assembler.has_finish() && finished_at.get().is_none() {
                    finished_at.set(Some(Instant::now()));
                }
                Ok::<_, StreamError>(done)
            },
        );
        // The turn is complete: a server that keeps the connection open is let go.
        if finished_at.get().is_some() {
            connection.cancel_detached();
        }
        let done = match fed {
            Ok(done) => done,
            Err(PumpError::Failed(error)) => return Err(Failure::Fatal(error)),
            Err(PumpError::Event(StreamError::Protocol(source))) => {
                return Err(protocol(source, &request_id))
            }
            Err(PumpError::Event(StreamError::Api {
                kind,
                message,
                before_start,
            })) => {
                let error =
                    ProviderError::from_stream_event(&kind, &message, request_id.as_deref(), scrub);
                return Err(if before_start {
                    Failure::BeforeDelivery(error)
                } else {
                    Failure::Fatal(error)
                });
            }
        };
        if !done && !assembler.has_finish() {
            return Err(protocol(ProtocolError::Truncated, &request_id));
        }
        Ok(assembler.finish(request_id, on_event))
    }

    fn stream_with_key(
        &self,
        key: Option<&ApiKey>,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        let scrub = scrub_for(key);
        let body: Arc<[u8]> = serde_json::to_vec(&Body {
            config: &self.config,
            request,
        })
        .map_err(|error| ProviderError::Serialize(error.to_string()))?
        .into();
        let taken = history_ids(request.messages);
        // One token for the connection of this turn: the transport watches it, the caller's
        // cancel is linked to it, and it is cancelled by the provider to close a connection the
        // server keeps open.
        let connection = CancelToken::new();
        let _link = cancel.on_cancel({
            let connection = connection.clone();
            move || connection.cancel()
        });
        let build = || HttpRequest {
            method: Method::Post,
            url: self.endpoints.chat.clone(),
            headers: self.headers(key, true),
            body: Arc::clone(&body),
            timeouts: self.config.timeouts,
        };
        let exchange = self.exchange();
        let mut state = RetryState::default();
        loop {
            let response = exchange.open(&mut state, &build, &scrub, on_event, &connection)?;
            match self.consume(response, &scrub, &taken, on_event, &connection) {
                Ok(turn) => return Ok(turn),
                Err(Failure::BeforeDelivery(error)) => {
                    exchange.backoff(error, &mut state, on_event, &connection)?;
                }
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
    }
}

/// Scrubs `key` from server text; with no key there is nothing to remove, but the text is still
/// bounded.
fn scrub_for(key: Option<&ApiKey>) -> Scrub {
    Scrub::new(key.cloned().unwrap_or_else(|| ApiKey::new("")))
}

/// The request id the server gave, scrubbed.
fn request_id(response: &HttpResponse, scrub: &Scrub) -> Option<String> {
    response
        .header("x-request-id")
        .or_else(|| response.header("request-id"))
        .map(|id| scrub.bounded(id, MAX_TOKEN_CHARS))
}

/// The ids of the tool calls of the history: a call of the next turn must not take one.
fn history_ids(messages: &[Message]) -> HashSet<&str> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content } => Some(content),
            _ => None,
        })
        .flatten()
        .map(Block)
        .filter(|block| block.kind() == "tool_use")
        .filter_map(|block| block.str("id"))
        .collect()
}

impl ChatProvider for OpenAiProvider {
    fn stream_turn(
        &self,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        let run = self.begin_run()?;
        self.stream_turn_in_run(&run, request, on_event, cancel)
    }

    fn begin_run(&self) -> Result<RunSession, ProviderError> {
        Ok(RunSession {
            key: self.key()?,
            issuer: self.id,
        })
    }

    fn stream_turn_in_run(
        &self,
        run: &RunSession,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        if run.issuer != self.id {
            return Err(ProviderError::ForeignRunSession);
        }
        if run.key.is_none() && self.config.flavor.requires_key() {
            return Err(KeyError::NotSet.into());
        }
        self.stream_with_key(run.key.as_ref(), request, on_event, cancel)
    }

    fn list_models(&self, cancel: &CancelToken) -> Result<Vec<ModelInfo>, ProviderError> {
        let key = self.key()?;
        self.list(key.as_ref(), cancel)
    }
}
