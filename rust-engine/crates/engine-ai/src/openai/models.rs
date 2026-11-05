// Adapted from dbx (https://github.com/t8y2/dbx), Apache-2.0; changed. See `mod.rs` and
// `licenses/README.md` at the root of the repository.

//! `GET {base}/models`, and for Ollama the `/api/show` capability filter.
//!
//! Ported from dbx `list_openai_compatible_models` / `parse_model_list_response` and
//! `retain_ollama_completion_models`. dbx probes with 8 requests at a time and no bound on their
//! number; a blocking transport probes one model at a time, at most [`MAX_PROBES`] of them and
//! for no longer than [`PROBE_BUDGET`] in all; the models past either bound are kept unfiltered
//! (as dbx keeps a model whose probe failed). A cancel ends the list with
//! [`ProviderError::Cancelled`].

use super::effort::{
    is_openai_non_assistant_model, ollama_completion_capability, ollama_tool_capability,
};
use super::stream::OptStr;
use super::{Flavor, OpenAiProvider};
use crate::key::ApiKey;
use crate::provider::{ModelInfo, ProtocolError, ProviderError, StreamEvent};
use crate::transport::{HttpRequest, Method, Timeouts};
use crate::wire::errors::Scrub;
use crate::wire::read_capped;
use crate::wire::retry::{RetryPolicy, RetryState};
use crate::wire::Exchange;
use engine_agent::CancelToken;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_LIST_BYTES: u64 = 4 * 1024 * 1024;
/// `/api/show` carries the model's template and licence; its `model_info` has no big arrays
/// unless `verbose` is asked for.
const MAX_SHOW_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const MAX_PROBES: usize = 64;
pub(super) const PROBE_BUDGET: Duration = Duration::from_secs(30);
/// dbx gives one probe 5 seconds.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct List<'a> {
    #[serde(borrow)]
    data: Vec<WireModel<'a>>,
}

#[derive(Deserialize)]
struct WireModel<'a> {
    #[serde(default, borrow)]
    id: OptStr<'a>,
    #[serde(default, borrow)]
    display_name: OptStr<'a>,
    #[serde(default, borrow)]
    name: OptStr<'a>,
}

#[derive(Deserialize)]
struct Show {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

fn malformed(error: &serde_json::Error, request_id: Option<String>) -> ProviderError {
    ProviderError::Protocol {
        source: ProtocolError::MalformedBody(format!(
            "{:?} at line {} column {}",
            error.classify(),
            error.line(),
            error.column()
        )),
        request_id,
    }
}

/// The models of a `data` list (dbx `parse_model_list_response`): an entry without a usable id
/// is skipped, a repeated id is listed once, the display name is `display_name` or `name` when
/// it says more than the id.
fn parse_model_list(raw: &[u8]) -> Result<Vec<ModelInfo>, serde_json::Error> {
    let list: List<'_> = serde_json::from_slice(raw)?;
    let mut seen = HashSet::new();
    let mut models = Vec::with_capacity(list.data.len());
    for item in &list.data {
        let Some(id) = item.id.0.as_deref().filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        let display_name = item
            .display_name
            .0
            .as_deref()
            .or(item.name.0.as_deref())
            .filter(|name| !name.trim().is_empty() && *name != id)
            .map(str::to_owned);
        models.push(ModelInfo {
            id: id.to_owned(),
            display_name,
            max_input_tokens: None,
            max_tokens: None,
            created_at: None,
        });
    }
    Ok(models)
}

impl OpenAiProvider {
    pub(super) fn list_models_with(
        &self,
        key: Option<&ApiKey>,
        cancel: &CancelToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let scrub = Scrub::new(key.cloned().unwrap_or_else(|| ApiKey::new("")));
        let build = || HttpRequest {
            method: Method::Get,
            url: self.models_url.clone(),
            headers: self.headers(key, false),
            body: Arc::from(&[][..]),
            timeouts: self.config.timeouts,
        };
        let mut ignore = |_: StreamEvent<'_>| {};
        let response = self.exchange().open(
            &mut RetryState::default(),
            &build,
            &scrub,
            &mut ignore,
            cancel,
        )?;
        let request_id = self.request_id(&response, &scrub);
        let raw = read_capped(response.body, MAX_LIST_BYTES, &scrub, &request_id, cancel)?;
        let mut models = parse_model_list(&raw)
            .map_err(|error| malformed(&error, request_id))?
            .into_iter()
            .map(|model| model.cleaned(&scrub))
            .collect::<Vec<_>>();
        match self.config.flavor {
            Flavor::OpenAi => {
                models.retain(|model| !is_openai_non_assistant_model(&model.id));
                Ok(models)
            }
            Flavor::Ollama => self.retain_completion_models(models, key, &scrub, cancel),
            Flavor::Compatible => Ok(models),
        }
    }

    /// Ollama's `capabilities` of a model (`POST /api/show`): one attempt, 5 seconds.
    fn show_capabilities(
        &self,
        model: &str,
        key: Option<&ApiKey>,
        scrub: &Scrub,
        cancel: &CancelToken,
    ) -> Result<Vec<String>, ProviderError> {
        let body: Arc<[u8]> = serde_json::to_vec(&serde_json::json!({ "model": model }))
            .map_err(|error| ProviderError::Serialize(error.to_string()))?
            .into();
        let build = || HttpRequest {
            method: Method::Post,
            url: self.show_url.clone(),
            headers: self.headers(key, true),
            body: Arc::clone(&body),
            timeouts: Timeouts {
                connect: PROBE_TIMEOUT,
                first_byte: PROBE_TIMEOUT,
                idle: PROBE_TIMEOUT,
            },
        };
        let once = RetryPolicy {
            max_retries: 0,
            transport_retries: 0,
            ..self.config.retry
        };
        let exchange = Exchange {
            retry: &once,
            ..self.exchange()
        };
        let mut ignore = |_: StreamEvent<'_>| {};
        let response = exchange.open(
            &mut RetryState::default(),
            &build,
            scrub,
            &mut ignore,
            cancel,
        )?;
        let request_id = self.request_id(&response, scrub);
        let raw = read_capped(response.body, MAX_SHOW_BYTES, scrub, &request_id, cancel)?;
        let show: Show =
            serde_json::from_slice(&raw).map_err(|error| malformed(&error, request_id))?;
        Ok(show.capabilities.unwrap_or_default())
    }

    /// Drops the models that say they cannot complete (embedding models and the like); a model
    /// that lists no capabilities, or whose probe failed, stays.
    fn retain_completion_models(
        &self,
        models: Vec<ModelInfo>,
        key: Option<&ApiKey>,
        scrub: &Scrub,
        cancel: &CancelToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let started = Instant::now();
        let mut kept = Vec::with_capacity(models.len());
        for (probed, model) in models.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            if probed >= MAX_PROBES || started.elapsed() > PROBE_BUDGET {
                kept.push(model);
                continue;
            }
            match self.show_capabilities(&model.id, key, scrub, cancel) {
                Ok(capabilities) if ollama_completion_capability(&capabilities) == Some(false) => {}
                Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
                Ok(_) | Err(_) => kept.push(model),
            }
        }
        Ok(kept)
    }

    /// Whether an Ollama model says it takes tools (`Some(false)`: it lists capabilities and
    /// `tools` is not one; `None`: it lists none). Ported from dbx
    /// `ollama_selected_model_tool_support`. Other flavors have no such answer: `Ok(None)`.
    pub fn model_supports_tools(
        &self,
        model: &str,
        cancel: &CancelToken,
    ) -> Result<Option<bool>, ProviderError> {
        if self.config.flavor != Flavor::Ollama {
            return Ok(None);
        }
        let key = self.key()?;
        let scrub = Scrub::new(key.clone().unwrap_or_else(|| ApiKey::new("")));
        let capabilities = self.show_capabilities(model.trim(), key.as_ref(), &scrub, cancel)?;
        Ok(ollama_tool_capability(&capabilities))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // dbx `parses_openai_and_claude_model_list_items`.
    #[test]
    fn a_list_skips_entries_without_an_id_and_repeats() {
        let raw = br#"{"data":[{"id":"gpt-4o-mini"},{"id":"claude-sonnet-4-20250514","display_name":"Claude Sonnet 4"},
            {"id":"gpt-4o-mini"},{"display_name":"Missing ID"},{"id":"  "},{"id":5},{"id":"m","name":"m"},{"id":"n","name":"Name N"}]}"#;
        let ids: Vec<_> = parse_model_list(raw)
            .unwrap()
            .into_iter()
            .map(|m| (m.id, m.display_name))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("gpt-4o-mini".to_owned(), None),
                (
                    "claude-sonnet-4-20250514".to_owned(),
                    Some("Claude Sonnet 4".to_owned())
                ),
                ("m".to_owned(), None),
                ("n".to_owned(), Some("Name N".to_owned())),
            ]
        );
        assert!(parse_model_list(br#"{"models":[]}"#).is_err());
        assert!(parse_model_list(br#"{"data":null}"#).is_err());
    }
}
