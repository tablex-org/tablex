//! `GET /v1/models`.
//!
//! The bundled API reference documents the model fields (`id`, `display_name`,
//! `max_input_tokens`, `max_tokens`) and that Models paginates with `after_id` and answers
//! `has_more` / `first_id` / `last_id`. The list wrapper (`data`, with `limit` and `after_id`
//! as query parameters) is from the lead's knowledge of the endpoint, not from the reference.

use super::AnthropicProvider;
use crate::key::ApiKey;
use crate::provider::{ModelInfo, ProtocolError, ProviderError, StreamEvent};
use crate::transport::{HttpRequest, Method};
use crate::wire::errors::{Scrub, MAX_TOKEN_CHARS};
use crate::wire::read_capped;
use crate::wire::retry::RetryState;
use engine_agent::CancelToken;
use serde::Deserialize;
use std::fmt;
use std::sync::Arc;

const MAX_PAGES: usize = 20;
const PAGE_SIZE: usize = 100;
const MAX_PAGE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct Page {
    data: Vec<ModelInfo>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

/// Text written into a query string: everything but the unreserved characters of RFC 3986 is
/// percent-encoded.
struct QueryValue<'a>(&'a str);

impl fmt::Display for QueryValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                write!(f, "{}", char::from(byte))?;
            } else {
                write!(f, "%{byte:02X}")?;
            }
        }
        Ok(())
    }
}

impl AnthropicProvider {
    /// Every page of the list. A list longer than `MAX_PAGES` pages is an error
    /// ([`ProtocolError::TooManyPages`]), not a silently cut list.
    pub(super) fn list_models_paged(
        &self,
        key: &ApiKey,
        cancel: &CancelToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let scrub = Scrub::new(key.clone());
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!("{}?limit={PAGE_SIZE}", self.config.url("/v1/models"));
            if let Some(id) = &after {
                url.push_str(&format!("&after_id={}", QueryValue(id)));
            }
            let build = || HttpRequest {
                method: Method::Get,
                url: url.clone(),
                headers: self.headers(key, None),
                body: Arc::from(&[][..]),
                timeouts: self.config.timeouts,
            };
            let mut state = RetryState::default();
            let mut ignore = |_: StreamEvent<'_>| {};
            let response = self
                .exchange()
                .open(&mut state, &build, &scrub, &mut ignore, cancel)?;
            let request_id = response
                .header("request-id")
                .map(|id| scrub.bounded(id, MAX_TOKEN_CHARS));
            let raw = read_capped(response.body, MAX_PAGE_BYTES, &scrub, &request_id, cancel)?;
            let page: Page =
                serde_json::from_slice(&raw).map_err(|error| ProviderError::Protocol {
                    source: ProtocolError::MalformedBody(format!(
                        "{:?} at line {} column {}",
                        error.classify(),
                        error.line(),
                        error.column()
                    )),
                    request_id: request_id.clone(),
                })?;
            models.extend(page.data.into_iter().map(|model| model.cleaned(&scrub)));
            match page.last_id {
                // The cursor goes back to the server as it is, so one that would have to be
                // changed to be safe is refused.
                Some(last) if page.has_more => {
                    if scrub.bounded(&last, MAX_TOKEN_CHARS) != last {
                        return Err(ProviderError::Protocol {
                            source: ProtocolError::MalformedBody(
                                "the page's last_id is not a plain identifier".to_owned(),
                            ),
                            request_id,
                        });
                    }
                    after = Some(last);
                }
                _ => return Ok(models),
            }
        }
        Err(ProviderError::Protocol {
            source: ProtocolError::TooManyPages { limit: MAX_PAGES },
            request_id: None,
        })
    }
}
