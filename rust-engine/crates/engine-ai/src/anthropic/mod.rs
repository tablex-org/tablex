//! The Anthropic Messages API over an injected [`HttpTransport`].
//!
//! * [`request`]: the body of `POST /v1/messages` (fixed field order, cache breakpoints).
//! * [`sse`] and [`stream`]: the incremental SSE parser and the assembly of one assistant turn.
//! * [`errors`] and [`retry`]: typed errors and the bounded, cancellable, injectable retry.
//!
//! Against a base URL other than `https://api.anthropic.com` (a gateway) the request leaves out
//! `eager_input_streaming`, `fallbacks`, `thinking.block_binding` and the beta headers, which a
//! gateway may reject. A base URL must be `https`, or `http` to a loopback host, and carry no
//! credentials, query or fragment; the provider keeps it trimmed and without a trailing `/`, and
//! uses that one text for everything.
//!
//! The two betas (`fallbacks`, `thinking.block_binding`) are optional. If the API answers 400 and
//! names `anthropic-beta`, a beta value this crate sent, or the field of one, the feature is left
//! out of this and every later request of the provider, [`StreamEvent::FeatureDropped`] says so
//! once, and the request is sent again; a second 400 is an error.

mod models;
mod request;
mod stream;
#[cfg(test)]
mod tests;
// The parser lives in `wire`; the tests below this module reach it as `super::sse`.
#[cfg(test)]
use crate::wire::sse;

pub use crate::wire::retry::{RetryPolicy, SleepOutcome, Sleeper, ThreadSleeper};
pub use crate::wire::ConfigError;

use crate::key::ApiKey;
use crate::provider::{
    ApiFeature, ChatProvider, KeyError, KeySource, ModelInfo, ProtocolError, ProviderError,
    RunSession, StreamEvent, TurnRequest, TurnResponse,
};
use crate::transport::{Header, HttpRequest, HttpResponse, HttpTransport, Method, Timeouts};
use crate::wire::errors::{Scrub, MAX_TOKEN_CHARS};
use crate::wire::retry::RetryState;
use crate::wire::sse::SseParser;
use crate::wire::{next_provider_id, pump, validate_base_url, Exchange, PumpError};
use engine_agent::CancelToken;
use request::{Body, FALLBACK_BETA, NATIVE_BASE_URL, THINKING_BINDING_BETA};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
pub(crate) use stream::TurnLimits;
use stream::{StreamError, TurnAssembler};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_MAX_TOKENS: u32 = 64_000;
const API_VERSION: &str = "2023-06-01";

/// What the provider sends. Known limit: whether `fallback` blocks are written back and whether
/// tools carry `eager_input_streaming` follow the *current* values of `base_url` and
/// `server_fallback`, and the thinking-binding field follows `drop_mismatched_thinking`, so a
/// provider setting that is changed in the middle of a conversation (a new provider with another
/// configuration, over a stored conversation) re-renders the earlier turns: it costs the prompt
/// cache once and may drop earlier reasoning (the API drops or refuses a thinking block whose
/// history changed; the recovery in `Assistant` strips it).
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Trimmed, without a trailing `/`, once the provider is built ([`AnthropicProvider::new`]).
    pub base_url: String,
    /// Ask the API to re-run a request its safety classifiers declined on its default fallback
    /// model (`fallbacks: "default"`). Only sent to the real API.
    pub server_fallback: bool,
    /// Ask the API to drop a thinking block that no longer matches the conversation before it
    /// (`thinking.block_binding.prefix_mismatch_behavior: "drop_block"`, beta
    /// `thinking-binding-controls-2026-08-01`) instead of refusing the request. Only sent to the
    /// real API. Switch it off for an organization the beta is not enabled for.
    pub drop_mismatched_thinking: bool,
    pub max_tokens: u32,
    pub retry: RetryPolicy,
    pub timeouts: Timeouts,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: NATIVE_BASE_URL.to_owned(),
            server_fallback: true,
            drop_mismatched_thinking: true,
            max_tokens: DEFAULT_MAX_TOKENS,
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
        }
    }
}

impl AnthropicConfig {
    /// The base URL as it is used everywhere: trimmed, without trailing slashes.
    fn normalized_base_url(&self) -> &str {
        self.base_url.trim().trim_end_matches('/')
    }

    /// The base URL is the real API (not a gateway).
    pub fn is_native(&self) -> bool {
        self.normalized_base_url() == NATIVE_BASE_URL
    }

    /// `fallbacks: "default"` and its beta header go out with the request.
    pub fn sends_fallbacks(&self) -> bool {
        self.is_native() && self.server_fallback
    }

    /// `thinking.block_binding` and its beta header go out with the request.
    pub fn sends_thinking_binding(&self) -> bool {
        self.is_native() && self.drop_mismatched_thinking
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.normalized_base_url())
    }

    /// Checks the base URL: `https`, or `http` to a loopback host (the key must not travel in
    /// clear text), with a host, without credentials, query or fragment.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_base_url(self.normalized_base_url())
    }
}

/// The optional features one request carries: those of the configuration that the API has not
/// refused yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Wire {
    pub fallbacks: bool,
    pub thinking_binding: bool,
}

impl Wire {
    /// The value of the `anthropic-beta` header, when there is one (several betas are separated
    /// by commas in one header).
    fn beta_header(self) -> Option<String> {
        let mut betas = Vec::new();
        if self.fallbacks {
            betas.push(FALLBACK_BETA);
        }
        if self.thinking_binding {
            betas.push(THINKING_BINDING_BETA);
        }
        (!betas.is_empty()).then(|| betas.join(","))
    }

    /// Every optional feature with the beta value and the request field a refusal would name,
    /// and whether this request carries it.
    fn features(self) -> [(ApiFeature, &'static str, &'static str, bool); 2] {
        [
            (
                ApiFeature::ServerFallback,
                FALLBACK_BETA,
                "fallbacks",
                self.fallbacks,
            ),
            (
                ApiFeature::ThinkingBinding,
                THINKING_BINDING_BETA,
                "block_binding",
                self.thinking_binding,
            ),
        ]
    }
}

fn feature_bit(feature: ApiFeature) -> u8 {
    match feature {
        ApiFeature::ThinkingBinding => 1,
        ApiFeature::ServerFallback => 2,
    }
}

pub struct AnthropicProvider {
    transport: Arc<dyn HttpTransport>,
    keys: Arc<dyn KeySource>,
    sleeper: Arc<dyn Sleeper>,
    config: AnthropicConfig,
    limits: TurnLimits,
    /// Tells a [`RunSession`] which provider issued it.
    id: u64,
    /// Bits of the [`ApiFeature`]s the API refused: left out of every later request.
    refused: AtomicU8,
}

/// A stream that failed: before `message_start` it may be retried like an HTTP error.
enum Failure {
    BeforeStart(ProviderError),
    Fatal(ProviderError),
}

impl AnthropicProvider {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        keys: Arc<dyn KeySource>,
        mut config: AnthropicConfig,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        // What was validated is what is used from now on.
        config.base_url = config.normalized_base_url().to_owned();
        Ok(Self {
            transport,
            keys,
            sleeper: Arc::new(ThreadSleeper),
            limits: TurnLimits::for_max_tokens(config.max_tokens),
            config,
            id: next_provider_id(),
            refused: AtomicU8::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_limits(mut self, limits: TurnLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    /// What the next request carries: the configured features minus the refused ones.
    fn wire(&self) -> Wire {
        let refused = self.refused.load(Ordering::Relaxed);
        let allowed = |feature| refused & feature_bit(feature) == 0;
        Wire {
            fallbacks: self.config.sends_fallbacks() && allowed(ApiFeature::ServerFallback),
            thinking_binding: self.config.sends_thinking_binding()
                && allowed(ApiFeature::ThinkingBinding),
        }
    }

    /// After a final error: leaves out the beta features it names (see the module docs) and says
    /// so. True when the request is worth sending again.
    fn drop_refused_features(
        &self,
        error: &ProviderError,
        wire: Wire,
        notify: &mut dyn FnMut(StreamEvent<'_>),
    ) -> bool {
        let features = error.rejected_features(&wire.features());
        for feature in &features {
            self.refused
                .fetch_or(feature_bit(*feature), Ordering::Relaxed);
            notify(StreamEvent::FeatureDropped(*feature));
        }
        !features.is_empty()
    }

    fn headers(&self, key: &ApiKey, wire: Option<Wire>) -> Vec<Header> {
        let mut headers = vec![
            Header::secret("x-api-key", key.clone()),
            Header::plain("anthropic-version", API_VERSION),
        ];
        // `wire` is given for a request with a body; a listing carries no betas.
        if let Some(wire) = wire {
            headers.push(Header::plain("content-type", "application/json"));
            if let Some(betas) = wire.beta_header() {
                headers.push(Header::plain("anthropic-beta", betas));
            }
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

    /// Reads the SSE body to `message_stop`, within the limits of a turn.
    fn consume(
        &self,
        response: HttpResponse,
        scrub: &Scrub,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, Failure> {
        let request_id = response
            .header("request-id")
            .map(|id| scrub.bounded(id, MAX_TOKEN_CHARS));
        let protocol = |source: ProtocolError, request_id: &Option<String>| {
            Failure::Fatal(ProviderError::Protocol {
                source,
                request_id: request_id.clone(),
            })
        };
        let mut body = response.body;
        let mut parser = SseParser::default();
        let mut assembler = TurnAssembler::new(self.limits).with_scrub(scrub.clone());
        let fed = pump(
            &mut body,
            &mut parser,
            self.limits.max_bytes,
            scrub,
            &request_id,
            cancel,
            &mut |event| assembler.handle(event.data, on_event),
        );
        let finished = match fed {
            Ok(finished) => finished,
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
        };
        if !finished {
            return Err(protocol(ProtocolError::Truncated, &request_id));
        }
        let mut turn = assembler
            .finish(request_id.clone())
            .map_err(|source| protocol(source, &request_id))?;
        turn.model = turn
            .model
            .map(|model| scrub.bounded(&model, MAX_TOKEN_CHARS));
        turn.fallback_to = turn
            .fallback_to
            .map(|model| scrub.bounded(&model, MAX_TOKEN_CHARS));
        Ok(turn)
    }

    fn stream_with_key(
        &self,
        key: &ApiKey,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        let scrub = Scrub::new(key.clone());
        let url = self.config.url("/v1/messages");
        let mut state = RetryState::default();
        loop {
            // Serialized again after a feature was refused: the body is not the same any more.
            let wire = self.wire();
            let body: Arc<[u8]> = serde_json::to_vec(&Body {
                config: &self.config,
                request,
                wire,
            })
            .map_err(|error| ProviderError::Serialize(error.to_string()))?
            .into();
            let build = || HttpRequest {
                method: Method::Post,
                url: url.clone(),
                headers: self.headers(key, Some(wire)),
                body: Arc::clone(&body),
                timeouts: self.config.timeouts,
            };
            let exchange = self.exchange();
            let error = match exchange.open(&mut state, &build, &scrub, on_event, cancel) {
                Ok(response) => match self.consume(response, &scrub, on_event, cancel) {
                    Ok(turn) => return Ok(turn),
                    Err(Failure::BeforeStart(error)) => {
                        match exchange.backoff(error, &mut state, on_event, cancel) {
                            Ok(()) => continue,
                            Err(error) => error,
                        }
                    }
                    Err(Failure::Fatal(error)) => return Err(error),
                },
                Err(error) => error,
            };
            if !self.drop_refused_features(&error, wire, on_event) {
                return Err(error);
            }
        }
    }

    /// Asks the key source; an empty key is refused before anything is sent.
    fn key(&self) -> Result<ApiKey, KeyError> {
        let key = self.keys.api_key()?;
        if key.is_empty() {
            return Err(KeyError::Empty);
        }
        Ok(key)
    }
}

impl ChatProvider for AnthropicProvider {
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
            key: Some(self.key()?),
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
        let key = run.key.as_ref().ok_or(KeyError::NotSet)?;
        self.stream_with_key(key, request, on_event, cancel)
    }

    fn list_models(&self, cancel: &CancelToken) -> Result<Vec<ModelInfo>, ProviderError> {
        let key = self.key()?;
        self.list_models_paged(&key, cancel)
    }
}
