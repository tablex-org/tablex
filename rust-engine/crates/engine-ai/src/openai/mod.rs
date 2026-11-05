//! OpenAI, OpenAI-compatible and Ollama chat providers: the Chat Completions API over an injected
//! [`HttpTransport`].
//!
//! # Origin and licence
//!
//! This module is adapted from dbx (<https://github.com/t8y2/dbx>), `crates/dbx-core/src/ai.rs`,
//! `ai_effort.rs` and `ai_model_filter.rs`, which are licensed under the Apache License 2.0 (the
//! text is in `licenses/Apache-2.0.txt` at the root of this repository; `licenses/README.md` lists
//! the files that derive from dbx). Changes: dbx's `reqwest`/`tokio` and `String` errors are
//! replaced by this crate's blocking [`HttpTransport`], [`CancelToken`] and typed
//! [`ProviderError`]s; the request is written from the stored [`Conversation`](crate::Conversation)
//! and the response is stored as content blocks; the code is cut to the Chat Completions path of
//! the `Openai`, `OpenaiCompatible` and `Ollama` providers. Each ported function names its dbx
//! original.
//!
//! * [`request`]: the body of `POST …/chat/completions`.
//! * [`stream`]: the incremental assembly of one assistant turn.
//! * [`models`]: `GET …/models` and Ollama's `/api/show` filter.
//! * [`endpoint`] and [`effort`]: the addresses, and the rules that depend on the model name.
//!
//! A base URL must be `https`, or `http` to a loopback host, and carry no credentials, query or
//! fragment (the rule of the Anthropic provider, shared); the provider keeps it trimmed and
//! without a trailing `/`. A pasted `…/chat/completions` is accepted, a bare origin gets `/v1`.

mod effort;
mod endpoint;
mod models;
mod request;
mod stream;
#[cfg(test)]
mod tests;

use crate::conversation::Message;
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
use engine_agent::CancelToken;
use request::Body;
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;
use stream::{ChatAssembler, GraceReader};

/// OpenAI's API, the default base URL of [`Flavor::OpenAi`].
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Ollama's OpenAI-compatible API on this machine, the default of [`Flavor::Ollama`].
pub const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
/// dbx asks for 4096 tokens in a tool turn.
const DEFAULT_MAX_TOKENS: u32 = 4096;
/// How long the stream may stay silent after its `finish_reason` before it counts as ended
/// (dbx waits one second).
const FINISH_GRACE: Duration = Duration::from_secs(1);

/// Which server speaks the API; the places where dbx's providers differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// OpenAI (or a gateway that keeps its semantics): `max_completion_tokens` for reasoning
    /// models, a key is required, the model list hides non-chat models.
    OpenAi,
    /// Any other server of the API (LM Studio, vLLM, a gateway): a key is optional.
    Compatible,
    /// Ollama under `/v1`: a key is optional, `reasoning_effort: "none"` turns thinking off, the
    /// model list is filtered by `/api/show` capabilities.
    Ollama,
}

impl Flavor {
    /// The base URL to start from; none for a compatible server, which can be anywhere.
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some(OPENAI_BASE_URL),
            Self::Ollama => Some(OLLAMA_BASE_URL),
            Self::Compatible => None,
        }
    }

    /// dbx `provider_requires_api_key`: only OpenAI refuses to start without a key.
    pub fn requires_key(self) -> bool {
        self == Self::OpenAi
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Trimmed, without a trailing `/`, once the provider is built ([`OpenAiProvider::new`]).
    pub base_url: String,
    pub flavor: Flavor,
    /// `max_tokens` (`max_completion_tokens` for an OpenAI reasoning model) of a turn.
    pub max_tokens: u32,
    /// dbx `enable_thinking`. On: the effort of the request goes out as `reasoning_effort` for
    /// the models that take it. Off: the flavor's switch (`reasoning_effort: "none"` for Ollama,
    /// `extra_body.chat_template_kwargs.enable_thinking: false` for other compatible servers;
    /// nothing for OpenAI).
    pub enable_thinking: bool,
    pub retry: RetryPolicy,
    pub timeouts: Timeouts,
}

impl OpenAiConfig {
    /// The defaults of a flavor; the base URL is empty for [`Flavor::Compatible`] (to be set).
    pub fn for_flavor(flavor: Flavor) -> Self {
        Self {
            base_url: flavor.default_base_url().unwrap_or_default().to_owned(),
            flavor,
            max_tokens: DEFAULT_MAX_TOKENS,
            enable_thinking: true,
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
        }
    }

    /// The base URL as it is used everywhere: trimmed, without trailing slashes.
    fn normalized_base_url(&self) -> &str {
        self.base_url.trim().trim_end_matches('/')
    }

    /// Checks the base URL with the rule of the Anthropic provider: `https`, or `http` to a
    /// loopback host, with a host, without credentials, query or fragment.
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
    limits: TurnLimits,
    finish_grace: Duration,
    /// Tells a [`RunSession`] which provider issued it.
    id: u64,
    chat_url: String,
    models_url: String,
    show_url: String,
}

/// A stream that failed: before anything of the turn was delivered it may be retried like an
/// HTTP error.
enum Failure {
    BeforeStart(ProviderError),
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
        let chat_url = endpoint::chat_url(config.flavor, &config.base_url);
        let models_url = endpoint::models_url(&config.base_url);
        let show_url = endpoint::show_url(&models_url);
        // Whatever the base looks like, the requests go where it points: a derived address on
        // another scheme, host or port would send the key there.
        let origin = endpoint::origin(&config.base_url);
        if [&chat_url, &models_url, &show_url]
            .iter()
            .any(|url| endpoint::origin(url) != origin)
        {
            return Err(ConfigError::InvalidBaseUrl);
        }
        Ok(Self {
            transport,
            keys,
            sleeper: Arc::new(ThreadSleeper),
            limits: TurnLimits::for_max_tokens(config.max_tokens),
            finish_grace: FINISH_GRACE,
            id: next_provider_id(),
            chat_url,
            show_url,
            models_url,
            config,
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

    fn exchange(&self) -> Exchange<'_> {
        Exchange {
            transport: &*self.transport,
            sleeper: &*self.sleeper,
            retry: &self.config.retry,
        }
    }

    /// `Authorization: Bearer <key>` when there is a key (dbx `maybe_bearer_headers`).
    fn headers(&self, key: Option<&ApiKey>, with_body: bool) -> Vec<Header> {
        let mut headers = Vec::with_capacity(2);
        if let Some(key) = key {
            // One secret value, so it prints as `***` and is wiped when dropped.
            headers.push(Header::secret(
                "authorization",
                ApiKey::new(format!("Bearer {}", key.expose())),
            ));
        }
        if with_body {
            headers.push(Header::plain("content-type", "application/json"));
        }
        headers
    }

    fn request_id(&self, response: &HttpResponse, scrub: &Scrub) -> Option<String> {
        response
            .header("x-request-id")
            .or_else(|| response.header("request-id"))
            .map(|id| scrub.bounded(id, MAX_TOKEN_CHARS))
    }

    /// Asks the key source. A key that is not set is no error for a flavor that does not need
    /// one (dbx `provider_requires_api_key`); an empty one is refused before anything is sent
    /// when the flavor needs it.
    fn key(&self) -> Result<Option<ApiKey>, KeyError> {
        let required = self.config.flavor.requires_key();
        match self.keys.api_key() {
            Ok(key) if !key.is_empty() => Ok(Some(key)),
            Ok(_) if required => Err(KeyError::Empty),
            Err(KeyError::NotSet | KeyError::Empty) if !required => Ok(None),
            Ok(_) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Reads the SSE body to `[DONE]` or the end after a `finish_reason`, within the limits of a
    /// turn.
    fn consume(
        &self,
        response: HttpResponse,
        scrub: &Scrub,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
        connection: &CancelToken,
        history: &[Message],
    ) -> Result<TurnResponse, Failure> {
        let request_id = self.request_id(&response, scrub);
        let protocol = |source: ProtocolError, request_id: &Option<String>| {
            Failure::Fatal(ProviderError::Protocol {
                source,
                request_id: request_id.clone(),
            })
        };
        let armed = Cell::new(false);
        let mut body =
            GraceReader::new(response.body, &armed, self.finish_grace, connection.clone());
        let mut parser = SseParser::default();
        let mut assembler = ChatAssembler::new(self.limits, scrub.clone(), history);
        let fed = pump(
            &mut body,
            &mut parser,
            self.limits.max_bytes,
            scrub,
            &request_id,
            cancel,
            &mut |event| {
                let done = assembler.handle(event.data, on_event)?;
                if assembler.finish_reason_seen() {
                    armed.set(true);
                }
                Ok::<_, StreamError>(done)
            },
        );
        // A helper that still reads the tail of the body ends with the connection.
        if armed.get() {
            connection.cancel_detached();
        }
        match fed {
            Ok(_) => {}
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
                    Failure::BeforeStart(error)
                } else {
                    Failure::Fatal(error)
                });
            }
        }
        if !assembler.is_complete() {
            return Err(protocol(ProtocolError::Truncated, &request_id));
        }
        let mut turn = assembler
            .finish(request_id.clone(), on_event)
            .map_err(|source| protocol(source, &request_id))?;
        turn.model = turn
            .model
            .map(|model| scrub.bounded(&model, MAX_TOKEN_CHARS));
        Ok(turn)
    }

    fn stream_with_key(
        &self,
        key: Option<&ApiKey>,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        let scrub = Scrub::new(key.cloned().unwrap_or_else(|| ApiKey::new("")));
        let body: Arc<[u8]> = serde_json::to_vec(&Body {
            config: &self.config,
            request,
        })
        .map_err(|error| ProviderError::Serialize(error.to_string()))?
        .into();
        // The token the transport gets: it follows `cancel`, and this provider can also close the
        // connection with it (see `GraceReader`).
        let connection = CancelToken::new();
        let _link = cancel.on_cancel({
            let connection = connection.clone();
            move || connection.cancel()
        });
        let build = || HttpRequest {
            method: Method::Post,
            url: self.chat_url.clone(),
            headers: self.headers(key, true),
            body: Arc::clone(&body),
            timeouts: self.config.timeouts,
        };
        let exchange = self.exchange();
        let mut state = RetryState::default();
        loop {
            let response = exchange.open(&mut state, &build, &scrub, on_event, &connection)?;
            match self.consume(
                response,
                &scrub,
                on_event,
                cancel,
                &connection,
                request.messages,
            ) {
                Ok(turn) => return Ok(turn),
                Err(Failure::BeforeStart(error)) => {
                    exchange.backoff(error, &mut state, on_event, &connection)?;
                }
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
    }
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
        self.stream_with_key(run.key.as_ref(), request, on_event, cancel)
    }

    fn list_models(&self, cancel: &CancelToken) -> Result<Vec<ModelInfo>, ProviderError> {
        let key = self.key()?;
        self.list_models_with(key.as_ref(), cancel)
    }
}
