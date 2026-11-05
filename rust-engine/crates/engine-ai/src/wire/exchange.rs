//! The loops both providers run over the transport: send a request with bounded, cancellable
//! retries, and read an event stream within a byte budget.

use super::errors::{Scrub, MAX_TOKEN_CHARS};
use super::retry::{RetryPolicy, RetryState, SleepOutcome, Sleeper};
use super::sse::{SseEvent, SseParser};
use crate::provider::{ProtocolError, ProviderError, RetryCause, StreamEvent};
use crate::transport::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use engine_agent::CancelToken;
use std::io::Read;
use std::time::Duration;

const READ_BUFFER: usize = 16 * 1024;

/// Sends requests for a provider: its transport, its wait and its retry policy.
pub(crate) struct Exchange<'a> {
    pub transport: &'a dyn HttpTransport,
    pub sleeper: &'a dyn Sleeper,
    pub retry: &'a RetryPolicy,
}

impl Exchange<'_> {
    fn sleep(&self, duration: Duration, cancel: &CancelToken) -> Result<(), ProviderError> {
        match self.sleeper.sleep(duration, cancel) {
            SleepOutcome::Elapsed if !cancel.is_cancelled() => Ok(()),
            _ => Err(ProviderError::Cancelled),
        }
    }

    /// Sends the request; returns the 2xx response, or the error once retrying is over.
    pub fn open(
        &self,
        state: &mut RetryState,
        build: &dyn Fn() -> HttpRequest,
        scrub: &Scrub,
        notify: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<HttpResponse, ProviderError> {
        loop {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            match self.transport.send(build(), cancel) {
                Ok(response) if (200..300).contains(&response.status) => return Ok(response),
                Ok(response) => {
                    let error = ProviderError::from_response(response, scrub);
                    self.backoff(error, state, notify, cancel)?;
                }
                Err(source) => self.transport_failed(source, scrub, state, notify, cancel)?,
            }
        }
    }

    /// Waits before the next attempt, or gives the error back when it is not retried (not a 429 /
    /// 5xx, attempts used up, or the next wait would pass the total bound).
    pub fn backoff(
        &self,
        error: ProviderError,
        state: &mut RetryState,
        notify: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<(), ProviderError> {
        let policy = self.retry;
        let Some(cause) = error.retry_cause() else {
            return Err(error);
        };
        if state.status_retries >= policy.max_retries {
            return Err(error);
        }
        let wait = error
            .api_failure()
            .and_then(|failure| failure.retry_after)
            .unwrap_or_else(|| policy.backoff(state.status_retries));
        if state.waited.saturating_add(wait) > policy.max_total_wait {
            return Err(error);
        }
        state.status_retries += 1;
        state.waited += wait;
        notify(StreamEvent::Retrying {
            attempt: state.status_retries,
            after: wait,
            cause,
        });
        self.sleep(wait, cancel)
    }

    fn transport_failed(
        &self,
        source: TransportError,
        scrub: &Scrub,
        state: &mut RetryState,
        notify: &mut dyn FnMut(StreamEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<(), ProviderError> {
        if source == TransportError::Cancelled || cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let policy = self.retry;
        if state.transport_retries >= policy.transport_retries {
            return Err(ProviderError::Transport {
                source: scrub.transport(source),
                request_id: None,
            });
        }
        state.transport_retries += 1;
        notify(StreamEvent::Retrying {
            attempt: state.transport_retries,
            after: policy.transport_delay,
            cause: RetryCause::Transport,
        });
        self.sleep(policy.transport_delay, cancel)
    }
}

/// Why [`pump`] stopped without a verdict of the stream.
pub(crate) enum PumpError<E> {
    /// The event handler (or the SSE parser, through `E: From<ProtocolError>`) failed.
    Event(E),
    /// The read failed, the budget ran out or the call was cancelled: the final error.
    Failed(ProviderError),
}

/// Reads the body into `parser` within `max_bytes`, calling `on_event` for each event until it
/// returns `Ok(true)` (then this returns `Ok(true)`) or the body ends (`Ok(false)`). Looks at
/// `cancel` before every read.
pub(crate) fn pump<E: From<ProtocolError>>(
    body: &mut dyn Read,
    parser: &mut SseParser,
    max_bytes: usize,
    scrub: &Scrub,
    request_id: &Option<String>,
    cancel: &CancelToken,
    on_event: &mut impl FnMut(SseEvent<'_>) -> Result<bool, E>,
) -> Result<bool, PumpError<E>> {
    let mut buffer = vec![0u8; READ_BUFFER];
    let mut received = 0usize;
    loop {
        if cancel.is_cancelled() {
            return Err(PumpError::Failed(ProviderError::Cancelled));
        }
        let read = match body.read(&mut buffer) {
            Ok(0) => return Ok(false),
            Ok(read) => read,
            Err(error) => {
                return Err(PumpError::Failed(if cancel.is_cancelled() {
                    ProviderError::Cancelled
                } else {
                    ProviderError::Transport {
                        source: TransportError::Failed(scrub.bounded(
                            &format!("the response stream broke: {error}"),
                            MAX_TOKEN_CHARS * 4,
                        )),
                        request_id: request_id.clone(),
                    }
                }))
            }
        };
        received += read;
        if received > max_bytes {
            return Err(PumpError::Failed(ProviderError::Protocol {
                source: ProtocolError::TurnTooLarge { limit: max_bytes },
                request_id: request_id.clone(),
            }));
        }
        if parser
            .feed::<E>(&buffer[..read], on_event)
            .map_err(PumpError::Event)?
        {
            return Ok(true);
        }
    }
}

/// The whole body, at most `cap` bytes: one byte more is read to tell a body cut at the cap from
/// one that fits exactly, and that is an error ([`ProtocolError::TurnTooLarge`]).
pub(crate) fn read_capped(
    body: impl Read,
    cap: u64,
    scrub: &Scrub,
    request_id: &Option<String>,
    cancel: &CancelToken,
) -> Result<Vec<u8>, ProviderError> {
    let mut raw = Vec::new();
    if let Err(error) = body.take(cap + 1).read_to_end(&mut raw) {
        return Err(if cancel.is_cancelled() {
            ProviderError::Cancelled
        } else {
            ProviderError::Transport {
                source: TransportError::Failed(
                    scrub.bounded(&format!("the response broke: {error}"), MAX_TOKEN_CHARS * 4),
                ),
                request_id: request_id.clone(),
            }
        });
    }
    if raw.len() as u64 > cap {
        return Err(ProviderError::Protocol {
            source: ProtocolError::TurnTooLarge {
                limit: usize::try_from(cap).unwrap_or(usize::MAX),
            },
            request_id: request_id.clone(),
        });
    }
    Ok(raw)
}
