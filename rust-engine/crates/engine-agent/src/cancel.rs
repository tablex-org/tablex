//! Cancellation of one tool call.
//!
//! A [`CancelToken`] belongs to one call. Whoever drives the call (the MCP
//! session, the assistant panel) keeps a clone and cancels it when the client
//! goes away; [`CancelToken::cancel_on_drop`] does that from a guard, so
//! dropping the future that awaits the blocking call is enough. A running
//! statement registers a hook with [`CancelToken::on_cancel`] that asks the
//! driver to cancel its operation.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIVE: u8 = 0;
const CANCELLED: u8 = 1;
const EXPIRED: u8 = 2;

type Hook = Box<dyn FnOnce() + Send>;

/// A registered hook. `live` is cleared by its [`HookGuard`] and looked at right before the hook
/// runs: the hooks of a cancelled token have left the list by then (a detached cancel runs them
/// on another thread a moment later), so dropping the guard can no longer remove them.
struct Registered {
    id: u64,
    live: Arc<AtomicBool>,
    hook: Hook,
}

#[derive(Default)]
struct Inner {
    state: AtomicU8,
    next_hook: AtomicU64,
    hooks: Mutex<Vec<Registered>>,
}

/// Cheap to clone; all clones share one state.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Inner>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the call (idempotent) and runs the registered hooks **on the calling thread**,
    /// waiting for them: a hook is the driver's blocking cancel (a new connection to the server).
    /// Do not call it from an async executor thread or a UI thread; there use
    /// [`cancel_detached`](Self::cancel_detached).
    pub fn cancel(&self) {
        if let Some(hooks) = self.mark(CANCELLED) {
            Self::run(hooks);
        }
    }

    /// Like [`cancel`](Self::cancel), but never waits for the hooks. The token is cancelled when
    /// this returns ([`is_cancelled`](Self::is_cancelled), and a hook registered later runs at once
    /// on its registering thread, as with `cancel`); the hooks registered so far run on a
    /// short-lived thread named `agent-cancel` (on this thread only if no thread can be spawned).
    /// No thread is spawned when there is no hook. There is no cap on these threads beyond the
    /// number of calls: one per cancelled call that has a hook (a token is cancelled once), and
    /// each ends when its hooks returned, which for a driver is its own cancel timeout (a few
    /// seconds) at most.
    pub fn cancel_detached(&self) {
        let Some(hooks) = self.mark(CANCELLED) else {
            return;
        };
        if hooks.is_empty() {
            return;
        }
        // The hooks move into the thread; if it cannot start they come back with the error and
        // run here.
        let slot = Arc::new(Mutex::new(Some(hooks)));
        let handed = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("agent-cancel".into())
            .spawn(move || {
                if let Some(hooks) = lock(&handed).take() {
                    Self::run(hooks);
                }
            });
        if spawned.is_err() {
            if let Some(hooks) = lock(&slot).take() {
                Self::run(hooks);
            }
        }
    }

    /// Like [`cancel`](Self::cancel), but records that the statement timeout
    /// fired, so the error says so.
    pub(crate) fn expire(&self) {
        if let Some(hooks) = self.mark(EXPIRED) {
            Self::run(hooks);
        }
    }

    /// Moves the token from live to `state`; the winner gets the hooks registered so far.
    fn mark(&self, state: u8) -> Option<Vec<Registered>> {
        self.0
            .state
            .compare_exchange(LIVE, state, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(std::mem::take(&mut *lock(&self.0.hooks)))
    }

    /// Runs the hooks whose statement has not finished; a hook that already runs cannot be recalled.
    fn run(hooks: Vec<Registered>) {
        for Registered { live, hook, .. } in hooks {
            if live.load(Ordering::Acquire) {
                hook();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.state.load(Ordering::Acquire) != LIVE
    }

    /// The call was cancelled because its statement timeout ran out.
    pub fn timed_out(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == EXPIRED
    }

    /// Runs `hook` once when the token is cancelled (at once if it already
    /// is). The hook is forgotten when the returned guard is dropped, so a
    /// finished statement is not cancelled late: not even when the cancel already took the hook
    /// off the list and has not got to it yet (it is looked at again right before it runs). A
    /// hook that is already running cannot be recalled; the driver's cancel for an operation
    /// that has ended is a no-op by its own contract.
    pub fn on_cancel(&self, hook: impl FnOnce() + Send + 'static) -> HookGuard {
        let id = self.0.next_hook.fetch_add(1, Ordering::Relaxed);
        let live = Arc::new(AtomicBool::new(true));
        {
            let mut hooks = lock(&self.0.hooks);
            if !self.is_cancelled() {
                hooks.push(Registered {
                    id,
                    live: Arc::clone(&live),
                    hook: Box::new(hook),
                });
                return HookGuard {
                    token: self.clone(),
                    id,
                    live,
                };
            }
        }
        hook();
        HookGuard {
            token: self.clone(),
            id,
            live,
        }
    }

    /// A guard that cancels the token when dropped.
    #[must_use = "the token is cancelled when the guard is dropped"]
    pub fn cancel_on_drop(&self) -> CancelOnDrop {
        CancelOnDrop {
            token: self.clone(),
            armed: true,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Forgets a hook when dropped.
#[must_use]
pub struct HookGuard {
    token: CancelToken,
    id: u64,
    live: Arc<AtomicBool>,
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
        lock(&self.token.0.hooks).retain(|registered| registered.id != self.id);
    }
}

/// Cancels its token on drop ([`CancelToken::cancel_detached`]: a guard dropped inside async code
/// must not block on a driver), unless [`disarm`](Self::disarm)ed.
pub struct CancelOnDrop {
    token: CancelToken,
    armed: bool,
}

impl CancelOnDrop {
    /// The call finished; do not cancel.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel_detached();
        }
    }
}

/// Expires a token after a deadline unless dropped first. One short-lived
/// thread per armed statement; dropping the watchdog wakes and ends it.
pub(crate) struct Watchdog {
    done: Option<mpsc::Sender<()>>,
}

impl Watchdog {
    pub(crate) fn arm(token: &CancelToken, timeout: Duration) -> Self {
        let (done, wait) = mpsc::channel::<()>();
        let token = token.clone();
        let spawned = std::thread::Builder::new()
            .name("agent-statement-timeout".into())
            .spawn(move || {
                if let Err(RecvTimeoutError::Timeout) = wait.recv_timeout(timeout) {
                    token.expire();
                }
            });
        Self {
            done: spawned.ok().map(|_| done),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // Closing the channel wakes the thread with `Disconnected`.
        self.done.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn hooks_run_once_on_cancel_and_immediately_when_already_cancelled() {
        let token = CancelToken::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let _guard = token.on_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!token.is_cancelled());
        token.cancel();
        token.cancel();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let counter = Arc::clone(&hits);
        let _late = token.on_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(!token.timed_out());
    }

    /// A hook that blocks until the test lets it go; the channels, not timing, order everything.
    fn blocked_hook(
        token: &CancelToken,
    ) -> (
        HookGuard,
        mpsc::Receiver<Option<String>>,
        mpsc::Sender<()>,
        mpsc::Receiver<()>,
    ) {
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let (finished_tx, finished) = mpsc::channel();
        let guard = token.on_cancel(move || {
            let _ = entered_tx.send(std::thread::current().name().map(str::to_owned));
            let _ = released.recv();
            let _ = finished_tx.send(());
        });
        (guard, entered, release, finished)
    }

    #[test]
    fn a_detached_cancel_does_not_wait_for_a_hook_that_blocks() {
        let token = CancelToken::new();
        let (_guard, entered, release, finished) = blocked_hook(&token);
        // An inline hook would block here for ever: nothing releases it before this returns.
        token.cancel_detached();
        assert!(token.is_cancelled(), "cancelled before it returns");
        let thread = entered.recv().expect("the hook runs");
        assert_eq!(thread.as_deref(), Some("agent-cancel"));
        assert!(finished.try_recv().is_err(), "and it is still blocked");
        // A hook registered now runs at once, on this thread.
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran);
        let _late = token.on_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        finished.recv().expect("the hook finishes once released");
        // Idempotent: a second cancel of either kind finds nothing to do.
        token.cancel_detached();
        token.cancel();
    }

    /// The gap of the review: a detached cancel took the hooks off the list, the runner is busy
    /// with the first one, and the statement of the second finishes (its guard drops) before it
    /// is reached. The channels order everything.
    #[test]
    fn a_statement_that_finishes_after_the_cancel_took_its_hook_is_not_cancelled_late() {
        let token = CancelToken::new();
        let (_first, entered, release, finished) = blocked_hook(&token);
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let second = token.on_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let (last_tx, last) = mpsc::channel();
        let _last = token.on_cancel(move || {
            let _ = last_tx.send(());
        });
        token.cancel_detached();
        entered.recv().expect("the runner is inside the first hook");
        // The second statement ends now; its hook is no longer on the list.
        drop(second);
        release.send(()).unwrap();
        finished.recv().unwrap();
        last.recv().expect("the hooks after it still run");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "cancelled late");
    }

    #[test]
    fn a_dropped_guard_cancels_without_waiting_for_the_hook() {
        let token = CancelToken::new();
        let (_hook, entered, release, finished) = blocked_hook(&token);
        drop(token.cancel_on_drop());
        assert!(token.is_cancelled());
        entered.recv().expect("the hook runs");
        release.send(()).unwrap();
        finished.recv().unwrap();
    }

    #[test]
    fn a_detached_cancel_without_hooks_runs_nothing_and_still_cancels() {
        let token = CancelToken::new();
        token.cancel_detached();
        assert!(token.is_cancelled() && !token.timed_out());
    }

    #[test]
    fn a_dropped_hook_guard_forgets_the_hook() {
        let token = CancelToken::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        drop(token.on_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        token.cancel();
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancel_on_drop_cancels_unless_disarmed() {
        let token = CancelToken::new();
        drop(token.cancel_on_drop());
        assert!(token.is_cancelled());
        let token = CancelToken::new();
        token.cancel_on_drop().disarm();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn the_watchdog_expires_the_token_only_after_the_deadline() {
        let token = CancelToken::new();
        let watchdog = Watchdog::arm(&token, Duration::from_millis(30));
        let start = std::time::Instant::now();
        while !token.is_cancelled() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(token.timed_out());
        drop(watchdog);

        let token = CancelToken::new();
        drop(Watchdog::arm(&token, Duration::from_millis(30)));
        std::thread::sleep(Duration::from_millis(80));
        assert!(!token.is_cancelled());
    }
}
