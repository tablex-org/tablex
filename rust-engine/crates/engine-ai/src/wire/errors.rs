//! API error answers (HTTP status and in-stream `error` events) as [`ProviderError`]s.
//!
//! Every text that comes from the server is scrubbed of the API key first and cut to a bound
//! after (the other way round, a key straddling the cut would leave a prefix of itself). The
//! request body is never part of an error.

use crate::key::ApiKey;
use crate::provider::{ApiFailure, ProviderError};
use crate::transport::{HttpResponse, TransportError};
use serde::Deserialize;
use std::io::Read;
use std::time::Duration;

const MAX_MESSAGE_CHARS: usize = 1000;
/// Most characters of a `request-id`, model name or other short server-supplied token.
pub(crate) const MAX_TOKEN_CHARS: usize = 128;
const MAX_ERROR_BODY: u64 = 64 * 1024;
const MAX_RETRY_AFTER_SECS: f64 = 3600.0;

/// Removes the API key from text that came from the server, and bounds it.
#[derive(Clone)]
pub(crate) struct Scrub(ApiKey);

impl Scrub {
    pub fn new(key: ApiKey) -> Self {
        Self(key)
    }

    /// `text` without the key, without control characters (a newline stays), at most
    /// `max_chars` characters long.
    pub fn bounded(&self, text: &str, max_chars: usize) -> String {
        let key = self.0.expose();
        let scrubbed;
        let text = if key.is_empty() || !text.contains(key) {
            text
        } else {
            scrubbed = text.replace(key, "***");
            &scrubbed
        };
        text.chars()
            .filter(|ch| !ch.is_control() || *ch == '\n')
            .take(max_chars)
            .collect()
    }

    pub fn transport(&self, error: TransportError) -> TransportError {
        match error {
            TransportError::Failed(message) => {
                TransportError::Failed(self.bounded(&message, MAX_MESSAGE_CHARS))
            }
            other => other,
        }
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: Option<ErrorDetail>,
    #[serde(default)]
    request_id: Option<String>,
}

/// `{"message": ..}` (both APIs) or the bare text of `{"error": "..."}` (Ollama).
#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorDetail {
    Object {
        #[serde(default)]
        message: Option<String>,
    },
    Text(String),
}

impl ErrorDetail {
    fn into_message(self) -> Option<String> {
        match self {
            Self::Object { message } => message,
            Self::Text(text) => Some(text),
        }
    }
}

impl HttpResponse {
    /// `retry-after` as a wait: a number of seconds (fractions allowed), at most an hour. A
    /// negative or non-finite number or an HTTP date is no wait.
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        let seconds: f64 = self.header("retry-after")?.trim().parse().ok()?;
        (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::from_secs_f64(seconds.min(MAX_RETRY_AFTER_SECS)))
    }
}

impl ProviderError {
    /// The error for a non-2xx response; reads (a bounded part of) its body.
    pub(crate) fn from_response(mut response: HttpResponse, scrub: &Scrub) -> Self {
        let status = response.status;
        let header_id = response
            .header("request-id")
            .or_else(|| response.header("x-request-id"))
            .map(str::to_owned);
        let retry_after = response.retry_after();
        let mut raw = Vec::new();
        // A body that cannot be read leaves the generic message.
        let _ = response
            .body
            .by_ref()
            .take(MAX_ERROR_BODY)
            .read_to_end(&mut raw);
        let (message, body_id) = match serde_json::from_slice::<ErrorBody>(&raw) {
            Ok(ErrorBody { error, request_id }) => {
                (error.and_then(ErrorDetail::into_message), request_id)
            }
            Err(_) => (None, None),
        };
        let message = message
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| format!("the server answered HTTP {status}"));
        let failure = ApiFailure {
            status: Some(status),
            message: scrub.bounded(&message, MAX_MESSAGE_CHARS),
            request_id: header_id
                .or(body_id)
                .map(|id| scrub.bounded(&id, MAX_TOKEN_CHARS)),
            retry_after,
        };
        match status {
            400 => Self::BadRequest(failure),
            401 => Self::Authentication(failure),
            403 => Self::Permission(failure),
            404 => Self::NotFound(failure),
            413 => Self::TooLarge(failure),
            429 => Self::RateLimited(failure),
            529 => Self::Overloaded(failure),
            500..=599 => Self::Server(failure),
            _ => Self::Rejected(failure),
        }
    }

    /// The error for an `error` event inside a stream that answered 200.
    pub(crate) fn from_stream_event(
        kind: &str,
        message: &str,
        request_id: Option<&str>,
        scrub: &Scrub,
    ) -> Self {
        let message = if message.trim().is_empty() {
            format!(
                "the stream reported {}",
                scrub.bounded(kind, MAX_TOKEN_CHARS)
            )
        } else {
            scrub.bounded(message, MAX_MESSAGE_CHARS)
        };
        let failure = ApiFailure {
            status: None,
            message,
            request_id: request_id.map(|id| scrub.bounded(id, MAX_TOKEN_CHARS)),
            retry_after: None,
        };
        match kind {
            "invalid_request_error" => Self::BadRequest(failure),
            "authentication_error" => Self::Authentication(failure),
            "permission_error" => Self::Permission(failure),
            "not_found_error" => Self::NotFound(failure),
            "request_too_large" => Self::TooLarge(failure),
            "rate_limit_error" => Self::RateLimited(failure),
            "overloaded_error" => Self::Overloaded(failure),
            "api_error" => Self::Server(failure),
            _ => Self::Rejected(failure),
        }
    }
}
