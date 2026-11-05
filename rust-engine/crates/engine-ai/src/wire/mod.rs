//! What the chat providers ([`crate::anthropic`], [`crate::openai`]) share: the SSE parser, the
//! retry policy, the error mapping and key scrubbing, the open / retry / read loops over the
//! [`HttpTransport`](crate::HttpTransport), the turn size limits, the base URL rule and the
//! `<system-reminder>` text of an operator note.

pub(crate) mod errors;
mod exchange;
pub(crate) mod retry;
pub(crate) mod sse;

pub(crate) use exchange::{pump, read_capped, Exchange, PumpError};

use crate::provider::{ModelInfo, ProtocolError};
use errors::{Scrub, MAX_TOKEN_CHARS};
use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Ids of providers, so that a [`RunSession`](crate::RunSession) can tell who issued it. `0` is
/// "none".
static NEXT_PROVIDER_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_provider_id() -> u64 {
    NEXT_PROVIDER_ID.fetch_add(1, Ordering::Relaxed)
}

/// The configuration cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("the base URL must be an https URL (or http to a loopback host) with a host")]
    InvalidBaseUrl,
    #[error("the base URL carries credentials; the key is sent in the x-api-key header only")]
    BaseUrlHasUserinfo,
    #[error("the base URL carries a query or a fragment; give the address up to the host and an optional path")]
    BaseUrlHasQueryOrFragment,
    #[error("the base URL is plain http, which would send the API key in clear text; only a loopback host may use http")]
    InsecureBaseUrl,
}

/// The host of `scheme://authority/...` (no brackets for IPv6, no port); `None` without `://`.
pub(crate) fn host_of(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    Some(host_of_authority(
        &rest[..rest.find('/').unwrap_or(rest.len())],
    ))
}

fn host_of_authority(authority: &str) -> &str {
    match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    }
}

/// Checks a base URL (trimmed, no trailing `/`): `https`, or `http` to a loopback host (the key
/// must not travel in clear text), with a host, without credentials, query or fragment.
pub(crate) fn validate_base_url(url: &str) -> Result<(), ConfigError> {
    let (scheme, rest) = url.split_once("://").ok_or(ConfigError::InvalidBaseUrl)?;
    if url.contains(['?', '#']) {
        return Err(ConfigError::BaseUrlHasQueryOrFragment);
    }
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    if authority.contains('@') {
        return Err(ConfigError::BaseUrlHasUserinfo);
    }
    let host = host_of_authority(authority);
    if host.is_empty() || url.chars().any(char::is_whitespace) {
        return Err(ConfigError::InvalidBaseUrl);
    }
    if scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match scheme {
        s if s.eq_ignore_ascii_case("http") && loopback => Ok(()),
        s if s.eq_ignore_ascii_case("http") => Err(ConfigError::InsecureBaseUrl),
        _ => Err(ConfigError::InvalidBaseUrl),
    }
}

/// What bounds one turn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnLimits {
    /// Bytes read from the response body.
    pub max_bytes: usize,
    pub max_blocks: usize,
    /// Consecutive events without content.
    pub max_idle_events: usize,
}

/// The smallest turn budget, in bytes.
const MIN_TURN_BYTES: usize = 16 * 1024 * 1024;
/// The largest turn budget, in bytes: what the budget never exceeds however large `max_tokens`
/// is configured (the largest output of a current model, 128k tokens, still fits).
const MAX_TURN_BYTES: usize = 64 * 1024 * 1024;
/// Bytes of an event stream per output token that the budget allows. A token can arrive as a
/// delta of its own (about 150 bytes of envelope); a fallback in mid-stream re-emits the output,
/// which doubles it. 512 covers both with room.
const TURN_BYTES_PER_TOKEN: usize = 512;

impl TurnLimits {
    /// The limits for a request that may produce `max_tokens` tokens: the byte budget grows with
    /// it, between [`MIN_TURN_BYTES`] and [`MAX_TURN_BYTES`], so a legitimate answer at the
    /// model's maximum output (or one a fallback produced twice) is not cut, while a runaway
    /// stream still is.
    pub fn for_max_tokens(max_tokens: u32) -> Self {
        let wanted = usize::try_from(max_tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(TURN_BYTES_PER_TOKEN);
        Self {
            max_bytes: wanted.clamp(MIN_TURN_BYTES, MAX_TURN_BYTES),
            ..Self::default()
        }
    }
}

impl Default for TurnLimits {
    fn default() -> Self {
        Self {
            max_bytes: MIN_TURN_BYTES,
            max_blocks: 512,
            max_idle_events: 1024,
        }
    }
}

/// What stops a stream from being assembled into a turn.
#[derive(Debug)]
pub(crate) enum StreamError {
    Protocol(ProtocolError),
    /// An `error` event. `before_start`: nothing of the turn was delivered yet, so the request
    /// may be sent again.
    Api {
        kind: String,
        message: String,
        before_start: bool,
    },
}

impl From<ProtocolError> for StreamError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl ModelInfo {
    /// The strings of an entry from the server, scrubbed of the key and cut like a model name.
    pub(crate) fn cleaned(self, scrub: &Scrub) -> Self {
        let clean = |text: String| scrub.bounded(&text, MAX_TOKEN_CHARS);
        Self {
            id: clean(self.id),
            display_name: self.display_name.map(clean),
            created_at: self.created_at.map(clean),
            ..self
        }
    }
}

/// The tag name that wraps a note. A note is operator text, but it may carry names from the
/// database; inside the wrapper the tag must not appear (a closing tag would end the wrapper
/// early). Every occurrence of the name, in any case, is written with a space instead of the
/// hyphen, which is no tag.
const REMINDER_TAG: &str = "system-reminder";

fn neutralize_tag(note: &str) -> Cow<'_, str> {
    // ASCII lower-casing keeps the byte offsets.
    let lower = note.to_ascii_lowercase();
    if !lower.contains(REMINDER_TAG) {
        return Cow::Borrowed(note);
    }
    let mut out = String::with_capacity(note.len());
    let mut rest = 0;
    for (at, _) in lower.match_indices(REMINDER_TAG) {
        out.push_str(&note[rest..at]);
        out.push_str("system reminder");
        rest = at + REMINDER_TAG.len();
    }
    out.push_str(&note[rest..]);
    Cow::Owned(out)
}

/// An operator note as the text of a user turn: `<system-reminder>`, the note, the closing tag.
pub(crate) struct Reminder<'a>(pub &'a str);

impl fmt::Display for Reminder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<system-reminder>\n{}\n</system-reminder>",
            neutralize_tag(self.0)
        )
    }
}
