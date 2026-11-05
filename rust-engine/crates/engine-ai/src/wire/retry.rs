//! Retry policy and the injectable wait.

use engine_agent::CancelToken;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// How hard a request is retried. A 429 or a 5xx (529 included) is retried up to `max_retries`
/// times, waiting `retry-after` when the API gave it and an exponential backoff otherwise; the sum
/// of all waits never passes `max_total_wait` (when the next wait would, the error is returned).
/// A failure before the first response byte is retried `transport_retries` times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub max_total_wait: Duration,
    pub transport_retries: u32,
    pub transport_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            max_total_wait: Duration::from_secs(60),
            transport_retries: 1,
            transport_delay: Duration::from_millis(500),
        }
    }
}

impl RetryPolicy {
    /// The wait before retry number `retry` (counting from 0) when the API named none.
    pub(crate) fn backoff(&self, retry: u32) -> Duration {
        self.base_delay
            .saturating_mul(2u32.saturating_pow(retry))
            .min(self.max_delay)
    }
}

#[derive(Debug, Default)]
pub(crate) struct RetryState {
    pub status_retries: u32,
    pub transport_retries: u32,
    pub waited: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepOutcome {
    Elapsed,
    Cancelled,
}

/// Waits between attempts. Injected so tests never sleep; an implementation must return as soon as
/// `cancel` is cancelled.
pub trait Sleeper: Send + Sync {
    fn sleep(&self, duration: Duration, cancel: &CancelToken) -> SleepOutcome;
}

/// Sleeps the calling thread, waking at once on cancel (no polling).
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration, cancel: &CancelToken) -> SleepOutcome {
        let (wake, asleep) = mpsc::channel::<()>();
        let _hook = cancel.on_cancel(move || {
            let _ = wake.send(());
        });
        match asleep.recv_timeout(duration) {
            Ok(()) => SleepOutcome::Cancelled,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                if cancel.is_cancelled() {
                    SleepOutcome::Cancelled
                } else {
                    SleepOutcome::Elapsed
                }
            }
        }
    }
}
