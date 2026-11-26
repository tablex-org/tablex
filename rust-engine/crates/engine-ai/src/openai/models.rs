//! `GET <base>/models`, and Ollama's `POST /api/show`.
//!
//! The list is the OpenAI shape, `{"data": [{"id": ..}]}`, which Ollama and the compatible servers
//! answer too. OpenAI's list also holds embedding, image, audio and moderation models that cannot
//! chat, so for the OpenAi flavor those are left out by name. Ollama lists what is installed,
//! embedding models included, so each model is asked what it can do (`capabilities`, which also
//! says whether it takes tools).

use super::{request_id, scrub_for, Flavor, OpenAiProvider};
use crate::key::ApiKey;
use crate::provider::{ModelInfo, ProtocolError, ProviderError};
use crate::transport::{HttpRequest, Method, Timeouts};
use crate::wire::errors::Scrub;
use crate::wire::retry::{RetryPolicy, RetryState};
use crate::wire::{read_capped, Exchange};
use engine_agent::CancelToken;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Most bytes read from a list or a show answer.
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
/// Most models of an Ollama list that are asked for their capabilities, and how long all of the
/// asking may take; the models beyond either bound are kept as they are.
pub(super) const MAX_PROBES: usize = 64;
pub(super) const PROBE_BUDGET: Duration = Duration::from_secs(30);
/// Limit of each phase of one probe (a local server answers at once, or not at all).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Name prefixes (of the base model, lowercase) of OpenAI models that do not chat.
const NON_CHAT_PREFIXES: &[&str] = &[
    "babbage-002",
    "chatgpt-image",
    "computer-use",
    "dall-e",
    "davinci-002",
    "gpt-image",
    "omni-moderation",
    "sora",
    "text-embedding",
    "text-moderation",
    "tts",
    "whisper",
];
/// Fragments of the names of the audio and realtime variants, which the Chat Completions stream
/// does not serve.
const NON_CHAT_PARTS: &[&str] = &["-audio", "-realtime", "-transcribe", "-tts"];

/// The id as the filter reads it: trimmed, without a leading `models/`, lowercase, the last
/// `/` segment, and for a fine-tune (`ft:<base>:<org>::<id>`) its base model.
fn base_model(id: &str) -> String {
    let id = id.trim();
    let id = id.strip_prefix("models/").unwrap_or(id).to_lowercase();
    let id = id.rsplit('/').next().unwrap_or_default();
    match id.strip_prefix("ft:") {
        Some(rest) => rest.split(':').next().unwrap_or_default().to_owned(),
        None => id.to_owned(),
    }
}

pub(super) fn is_chat_model(id: &str) -> bool {
    let base = base_model(id);
    !NON_CHAT_PREFIXES
        .iter()
        .any(|prefix| base.starts_with(prefix))
        && !NON_CHAT_PARTS.iter().any(|part| base.contains(part))
}

/// A short description of why `error` is no valid JSON of the expected shape; never quotes the
/// body.
fn diagnostic(error: &serde_json::Error) -> ProtocolError {
    ProtocolError::MalformedBody(format!(
        "{:?} at line {} column {}",
        error.classify(),
        error.line(),
        error.column()
    ))
}

/// The models of an OpenAI-style list: entries without a usable id (a non-blank string) are
/// skipped, a repeated id is listed once.
fn parse_list(raw: &[u8], scrub: &Scrub) -> Result<Vec<ModelInfo>, ProtocolError> {
    let list: Value = serde_json::from_slice(raw).map_err(|error| diagnostic(&error))?;
    let Some(data) = list.get("data").and_then(Value::as_array) else {
        return Err(ProtocolError::MalformedBody(
            "the list is not an object with a data array".to_owned(),
        ));
    };
    let mut seen = HashSet::new();
    Ok(data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            if id.trim().is_empty() {
                return None;
            }
            let text = |key: &str| entry.get(key).and_then(Value::as_str);
            let display_name = text("display_name")
                .or_else(|| text("name"))
                .filter(|name| !name.trim().is_empty() && *name != id)
                .map(str::to_owned);
            Some(ModelInfo {
                id: id.to_owned(),
                display_name,
                max_input_tokens: None,
                max_tokens: None,
                created_at: None,
            })
        })
        .map(|model| model.cleaned(scrub))
        .filter(|model| seen.insert(model.id.clone()))
        .collect())
}

impl OpenAiProvider {
    pub(super) fn list(
        &self,
        key: Option<&ApiKey>,
        cancel: &CancelToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let scrub = scrub_for(key);
        let build = || HttpRequest {
            method: Method::Get,
            url: self.endpoints.models.clone(),
            headers: self.headers(key, false),
            body: Arc::from(&[][..]),
            timeouts: self.config.timeouts,
        };
        let mut state = RetryState::default();
        let response =
            self.exchange()
                .open(&mut state, &build, &scrub, &mut |_| {}, cancel)?;
        let request_id = request_id(&response, &scrub);
        let raw = read_capped(response.body, MAX_BODY_BYTES, &scrub, &request_id, cancel)?;
        let mut models = parse_list(&raw, &scrub).map_err(|source| ProviderError::Protocol {
            source,
            request_id,
        })?;
        match self.config.flavor {
            Flavor::OpenAi => models.retain(|model| is_chat_model(&model.id)),
            Flavor::Compatible => {}
            Flavor::Ollama => {
                return self.with_completion(key, &scrub, models, MAX_PROBES, PROBE_BUDGET, cancel)
            }
        }
        Ok(models)
    }

    /// The models of `models` that can complete: one asked, in list order, up to `max_probes` of
    /// them and while `budget` lasts. A model is dropped only when it lists capabilities and
    /// `completion` is not among them; one that lists none (an old server), or whose probe
    /// failed, is kept. A cancel ends the call.
    pub(super) fn with_completion(
        &self,
        key: Option<&ApiKey>,
        scrub: &Scrub,
        models: Vec<ModelInfo>,
        max_probes: usize,
        budget: Duration,
        cancel: &CancelToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let started = Instant::now();
        let mut kept = Vec::with_capacity(models.len());
        for (position, model) in models.into_iter().enumerate() {
            if position < max_probes && started.elapsed() < budget {
                if cancel.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                match self.show(key, scrub, &model.id, cancel) {
                    Ok(capabilities)
                        if !capabilities.is_empty()
                            && !capabilities.iter().any(|name| name == "completion") =>
                    {
                        continue;
                    }
                    Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
                    Err(_) if cancel.is_cancelled() => return Err(ProviderError::Cancelled),
                    Ok(_) | Err(_) => {}
                }
            }
            kept.push(model);
        }
        Ok(kept)
    }

    /// The `capabilities` Ollama lists for `model` (empty when it lists none). One attempt: no
    /// retry, short timeouts.
    pub(super) fn show(
        &self,
        key: Option<&ApiKey>,
        scrub: &Scrub,
        model: &str,
        cancel: &CancelToken,
    ) -> Result<Vec<String>, ProviderError> {
        #[derive(Deserialize)]
        struct Show {
            #[serde(default)]
            capabilities: Option<Vec<String>>,
        }
        let body: Arc<[u8]> = serde_json::to_vec(&json!({ "model": model }))
            .map_err(|error| ProviderError::Serialize(error.to_string()))?
            .into();
        let build = || HttpRequest {
            method: Method::Post,
            url: self.endpoints.show.clone(),
            headers: self.headers(key, true),
            body: Arc::clone(&body),
            timeouts: Timeouts {
                connect: PROBE_TIMEOUT,
                first_byte: PROBE_TIMEOUT,
                idle: PROBE_TIMEOUT,
            },
        };
        let retry = RetryPolicy {
            max_retries: 0,
            transport_retries: 0,
            ..self.config.retry
        };
        let exchange = Exchange {
            transport: &*self.transport,
            sleeper: &*self.sleeper,
            retry: &retry,
        };
        let mut state = RetryState::default();
        let response = exchange.open(&mut state, &build, scrub, &mut |_| {}, cancel)?;
        let request_id = request_id(&response, scrub);
        let raw = read_capped(response.body, MAX_BODY_BYTES, scrub, &request_id, cancel)?;
        let show: Show = serde_json::from_slice(&raw).map_err(|error| ProviderError::Protocol {
            source: diagnostic(&error),
            request_id,
        })?;
        Ok(show.capabilities.unwrap_or_default())
    }

    /// Whether `model` takes tools, when the server can tell: Ollama lists the capabilities of
    /// each model; for any other flavor `None` (nothing is asked). A model that lists no
    /// capabilities is `None` too. A failed probe is an error.
    pub fn model_supports_tools(
        &self,
        model: &str,
        cancel: &CancelToken,
    ) -> Result<Option<bool>, ProviderError> {
        if self.config.flavor != Flavor::Ollama {
            return Ok(None);
        }
        let key = self.key()?;
        let scrub = scrub_for(key.as_ref());
        let capabilities = self.show(key.as_ref(), &scrub, model.trim(), cancel)?;
        Ok((!capabilities.is_empty()).then(|| capabilities.iter().any(|name| name == "tools")))
    }
}
