//! One kit connection: the user session (serialized, cancellable), a lazily
//! opened metadata connection, and the cancel side channel.
//!
//! * **User session** (`PgConnection::inner`): queries and mutations. Every
//!   call goes through the gate, so one statement runs at a time per backend
//!   process, a `CancelToken` always hits the statement the host asked to
//!   cancel, and a user's own `BEGIN ... COMMIT` spans calls on the same client.
//! * **Metadata connection**: navigator and describe run on a second
//!   connection (same configuration) opened on first use, so a catalog request
//!   never waits behind a long user query. It is bounded by
//!   [`CATALOG_TIMEOUT`]; a timed-out or failed metadata connection is dropped
//!   and reopened by the next call.
//! * **Broken**: if a `ROLLBACK` fails or a panic poisoned the gate, the
//!   session's transaction state is unknown; the connection is marked broken
//!   and every later call fails with `<area>.not_connected`.

use super::runtime;
use super::tls::{Delivery, Transport};
use crate::db::postgres::PostgresConn;
use engine_driver_kit::api::OperationId;
use engine_driver_kit::BackendError;
use std::cell::Cell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_postgres::error::SqlState;
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage};

/// Connect, navigator and describe calls give up after this long. Queries and
/// mutations are bounded by `operation.cancel` instead: users run long statements.
pub(crate) const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a statement may keep running after the host asked to cancel it. A
/// cancel the server never answers (a dead peer, a sleeping laptop) would
/// otherwise hold the calling thread and the connection forever: past this the
/// statement is dropped locally and the connection is marked broken (see
/// [`ArmedCancel::watch`]).
pub(crate) const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How long a call waits for the session while another call still holds it
/// (see [`Gate::lock`]).
pub(crate) const GATE_WAIT: Duration = Duration::from_secs(30);
/// How long finishing an operation waits for a cancel request that targets it.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(6);

/// After a cancel was requested and the statement is still running, the cancel
/// request is sent again after a pause that starts at [`REDELIVERY_FIRST`] and
/// doubles up to [`REDELIVERY_CEILING`]: PostgreSQL ignores a cancel that reaches
/// a backend between two commands. A cancel that did land on the executing
/// statement may still be rolling it back, and every request opens a connection.
const REDELIVERY_FIRST: Duration = Duration::from_millis(50);
const REDELIVERY_CEILING: Duration = Duration::from_secs(5);

/// The pauses between redeliveries: 50, 100, 200 ... 3200, 5000, 5000 ... ms.
fn redelivery_pauses() -> impl Iterator<Item = Duration> {
    std::iter::successors(Some(REDELIVERY_FIRST), |pause| {
        Some((*pause * 2).min(REDELIVERY_CEILING))
    })
}
/// The pause that ends an operation to which an UNACKNOWLEDGED cancel request was
/// sent. The server handles a cancel request in a process of its own, after the
/// client finished writing it, and signals the backend then; an idle backend
/// discards the signal, so waiting here (before the next statement) lets a late one
/// land harmlessly instead of interrupting the next operation. A request the server
/// acknowledged (it closed the connection, which it does after the signal) needs no
/// pause: see [`Transport::cancel_acknowledged`].
const SETTLE: Duration = Duration::from_millis(15);
/// Attempts of a clean-up statement that keeps being interrupted by a late cancel.
const CLEANUP_ATTEMPTS: usize = 3;

/// The cancel request of the armed operation: a flag for the re-checks between
/// round trips and a wake-up for the task that awaits the statement.
#[derive(Default)]
struct Requested {
    flag: AtomicBool,
    wake: Notify,
}

struct Running {
    operation: OperationId,
    /// The token that cancels the operation's backend (one allocation per
    /// operation: it holds the host name).
    token: Arc<CancelToken>,
    requested: Arc<Requested>,
}

#[derive(Default)]
struct Armed {
    /// The statement currently running.
    running: Option<Running>,
    /// Cancel requests that are being delivered right now.
    in_flight: usize,
    /// A cancel request was sent for the armed operation that the server did not
    /// acknowledge (or whose outcome is unknown): the operation ends with a [`SETTLE`].
    unacknowledged: bool,
}

/// The running operation's cancel token, keyed by operation id.
///
/// `cancel` only touches this small state, never the gate (the running
/// statement holds it). A cancel names an operation id: it is delivered only if
/// that operation is the armed one, so a late cancel for a finished operation
/// is dropped. Finishing an operation *drains* the cancels still being
/// delivered ([`ArmedCancel::disarm`]), so a request in flight can never reach
/// the next statement on the same backend.
///
/// A cancel the server ignores (it reached a backend between two commands) is
/// not lost: `cancel` raises the operation's [`Requested`] flag and the task
/// that runs the operation ([`ArmedCancel::watch`]) sends the request again
/// until the statement returns.
pub(crate) struct CancelState {
    state: Mutex<Armed>,
    idle: Condvar,
    /// The session's transport: a cancel request is encrypted (and verified)
    /// exactly like the session it interrupts.
    transport: Transport,
}

#[cfg(test)]
impl Default for CancelState {
    /// A plain-TCP canceller, for tests that need no server URI.
    fn default() -> Self {
        Self::new(Transport::default())
    }
}

impl CancelState {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            state: Mutex::default(),
            idle: Condvar::new(),
            transport,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Armed> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `token` as the statement of `operation`; cleared (after draining
    /// in-flight cancels) when the guard drops.
    pub(crate) fn arm(self: &Arc<Self>, operation: OperationId, token: CancelToken) -> ArmedCancel {
        let requested = Arc::new(Requested::default());
        let token = Arc::new(token);
        let mut state = self.lock();
        state.unacknowledged = false;
        state.running = Some(Running {
            operation,
            token: Arc::clone(&token),
            requested: Arc::clone(&requested),
        });
        drop(state);
        ArmedCancel {
            state: Arc::clone(self),
            token,
            requested,
        }
    }

    /// Asks the server to cancel `operation` if it is the running one.
    pub(crate) fn cancel(&self, operation: OperationId) -> Result<(), BackendError> {
        let token = {
            let mut state = self.lock();
            let Some(running) = state
                .running
                .as_ref()
                .filter(|running| running.operation == operation)
            else {
                return Ok(());
            };
            running.requested.raise();
            let token = Arc::clone(&running.token);
            state.in_flight += 1;
            token
        };
        let _delivered = Delivered(self);
        runtime::block_on(async { self.deliver(&token).await })
            .map_err(BackendError::new)?
            .map_err(|_| BackendError::new("cancel request timed out"))?
            .map_err(|error| BackendError::new(format!("cancel request failed: {error}")))
    }

    /// One cancel request on the wire, bounded by [`CANCEL_TIMEOUT`] as a whole.
    ///
    /// It goes to the address the session is pinned to and returns after the server
    /// closed the connection, i.e. after it signalled the backend
    /// ([`Transport::cancel_acknowledged`]). When that is not possible - no pinned
    /// address, the dial or handshake failed or took longer than its own short budget -
    /// the exact request of the token (which the server does not acknowledge) is sent
    /// in the same delivery. Anything but an acknowledgement - the exact request, an
    /// error, the timeout, this future being dropped - leaves `unacknowledged` set.
    async fn deliver(
        &self,
        token: &CancelToken,
    ) -> Result<Result<(), tokio_postgres::Error>, tokio::time::error::Elapsed> {
        let unsettled = Unsettled {
            state: self,
            acknowledged: Cell::new(false),
        };
        tokio::time::timeout(CANCEL_TIMEOUT, async {
            if self.transport.cancel_acknowledged(token).await == Delivery::Acknowledged {
                unsettled.acknowledged.set(true);
                return Ok(());
            }
            self.transport.cancel(token).await
        })
        .await
    }

    /// Ends the armed operation: no new cancel is delivered, the ones on the
    /// wire are waited for, and if an unacknowledged cancel request was sent for it
    /// the calling thread waits [`SETTLE`] so that a request the server handles late
    /// lands on an *idle* backend. Blocks the calling thread (the one that runs
    /// the operation, never a runtime worker). Idempotent.
    fn disarm(&self) {
        if self.stop_delivering() {
            std::thread::sleep(SETTLE);
        }
    }

    pub(super) fn stop_delivering(&self) -> bool {
        self.stop_delivering_within(DRAIN_TIMEOUT)
    }

    /// A delivery still in flight when the drain gave up may have a request on the
    /// wire: that is unacknowledged.
    fn stop_delivering_within(&self, drain: Duration) -> bool {
        let mut state = self.lock();
        state.running = None;
        // No new cancel can arm now; wait for the ones already on the wire.
        while state.in_flight > 0 {
            let (next, timeout) = self
                .idle
                .wait_timeout(state, drain)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if timeout.timed_out() {
                break;
            }
        }
        let still_in_flight = state.in_flight > 0;
        std::mem::take(&mut state.unacknowledged) || still_in_flight
    }
}

impl Requested {
    fn raise(&self) {
        self.flag.store(true, Ordering::Release);
        // A permit is kept if nobody waits yet.
        self.wake.notify_one();
    }
}

/// Sets `unacknowledged` when a delivery ends (or is dropped) without the server's
/// acknowledgement.
struct Unsettled<'a> {
    state: &'a CancelState,
    acknowledged: Cell<bool>,
}

impl Drop for Unsettled<'_> {
    fn drop(&mut self) {
        if !self.acknowledged.get() {
            self.state.lock().unacknowledged = true;
        }
    }
}

/// Marks one in-flight cancel delivered when dropped (also on early return).
struct Delivered<'a>(&'a CancelState);

impl Drop for Delivered<'_> {
    fn drop(&mut self) {
        self.0.lock().in_flight -= 1;
        self.0.idle.notify_all();
    }
}

/// The statement did not end within [`CANCEL_GRACE`] of a cancel request: the server is
/// not answering, and the session cannot be used again.
#[derive(Debug)]
pub(crate) struct Abandoned;

impl Abandoned {
    /// The error of `area`: the connection is lost (the app reconnects on it).
    pub(crate) fn error(area: &str) -> BackendError {
        BackendError::with_code(
            format!("{area}.not_connected"),
            "the server did not react to the cancel; the connection was dropped, reconnect",
        )
    }
}

/// The armed operation. Dropping it disarms (see [`CancelState`]).
pub(crate) struct ArmedCancel {
    state: Arc<CancelState>,
    token: Arc<CancelToken>,
    requested: Arc<Requested>,
}

impl ArmedCancel {
    /// Whether a cancel for this operation reached the driver after it was
    /// armed. Cheap (one atomic load): the operation looks before every round
    /// trip, so a cancel that the server would ignore stops it before the next
    /// statement is sent.
    pub(crate) fn requested(&self) -> bool {
        self.requested.flag.load(Ordering::Acquire)
    }

    /// Runs `work`, the cancellable part of the operation. Once a cancel was
    /// requested and `work` is still running, the cancel request is sent again
    /// (after 50, 100, 200 ... ms, up to every 5 s) until `work` returns: a
    /// request the server ignored because the backend was between two commands
    /// is not lost. A request is never cut off half way (when `work` returns
    /// while one is being written it is still finished before this returns) and
    /// none is started after `work` returned. Without a cancel this only polls
    /// `work`.
    ///
    /// A server that does not react to the cancel within [`CANCEL_GRACE`] is
    /// not coming back: `work` is dropped and [`Abandoned`] returned. The
    /// session is then in an unknown state; the caller marks it broken and
    /// sends nothing more on it.
    pub(crate) async fn watch<T>(&self, work: impl Future<Output = T>) -> Result<T, Abandoned> {
        self.watch_within(CANCEL_GRACE, work).await
    }

    async fn watch_within<T>(
        &self,
        grace: Duration,
        work: impl Future<Output = T>,
    ) -> Result<T, Abandoned> {
        let mut work = std::pin::pin!(work);
        tokio::select! {
            biased;
            out = &mut work => return Ok(out),
            () = self.requested.wake.notified() => {}
        }
        let deadline = tokio::time::Instant::now() + grace;
        for pause in redelivery_pauses() {
            tokio::select! {
                biased;
                out = &mut work => return Ok(out),
                () = tokio::time::sleep(pause) => {}
                () = tokio::time::sleep_until(deadline) => return Err(Abandoned),
            }
            // `work` is polled first, and a delivery `work` beat to it was never
            // polled: it is dropped, not started. One that was polled may be
            // half written and is finished.
            let started = Cell::new(false);
            let mut delivery = std::pin::pin!(async {
                started.set(true);
                self.state.deliver(&self.token).await
            });
            tokio::select! {
                biased;
                out = &mut work => {
                    if started.get() {
                        let _ = delivery.await;
                    }
                    return Ok(out);
                }
                _ = &mut delivery => {}
                () = tokio::time::sleep_until(deadline) => return Err(Abandoned),
            }
        }
        unreachable!("the pauses never run out")
    }

    /// Ends the cancellable part of the operation, before its clean-up (see
    /// `CancelState::disarm`). Dropping the guard does the same, so no way out
    /// of the operation can skip it.
    pub(crate) fn disarm(&self) {
        self.state.disarm();
    }

    /// Looked at before every round trip: the cancelled error of `area`, so
    /// nothing more is sent.
    pub(crate) fn check(&self, area: &str) -> Result<(), BackendError> {
        if self.requested() {
            Err(super::query::cancelled(area))
        } else {
            Ok(())
        }
    }
}

impl Drop for ArmedCancel {
    fn drop(&mut self) {
        self.state.disarm();
    }
}

/// Runs a clean-up statement (`ROLLBACK`, closing the read-only scope). A late
/// cancel can still interrupt it; it is tried again, a few times.
pub(crate) async fn batch_execute_uninterrupted(
    client: &Client,
    sql: &str,
) -> Result<(), tokio_postgres::Error> {
    let mut attempts = 1;
    loop {
        match client.batch_execute(sql).await {
            Err(error)
                if error.code() == Some(&SqlState::QUERY_CANCELED)
                    && attempts < CLEANUP_ATTEMPTS =>
            {
                attempts += 1;
            }
            other => return other,
        }
    }
}

/// Whether the session is inside a user transaction block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// No transaction is open: a call may open its own.
    Own,
    /// The user opened a transaction on this session: a call joins it through a
    /// savepoint and never commits or rolls back the user's work.
    Savepoint,
}

/// Probes the session. `transaction_timestamp()` is fixed when a transaction
/// starts while `statement_timestamp()` moves with every command, so over the
/// simple protocol (one command, one implicit transaction outside a block) they
/// differ exactly inside a block. A transaction that already failed rejects the
/// probe (25P02): that is "inside" as well.
pub(crate) async fn session_scope(client: &Client) -> Result<Scope, tokio_postgres::Error> {
    match client
        .simple_query("SELECT transaction_timestamp() <> statement_timestamp()")
        .await
    {
        Ok(messages) => {
            let inside = messages.iter().any(|message| {
                matches!(message, SimpleQueryMessage::Row(row) if row.try_get(0).ok().flatten() == Some("t"))
            });
            Ok(if inside { Scope::Savepoint } else { Scope::Own })
        }
        Err(error) if error.code() == Some(&SqlState::IN_FAILED_SQL_TRANSACTION) => {
            Ok(Scope::Savepoint)
        }
        Err(error) => Err(error),
    }
}

/// Serializes the user session and remembers that it is broken.
#[derive(Default)]
pub(crate) struct Gate {
    lock: Mutex<()>,
    broken: AtomicBool,
}

impl Gate {
    fn not_connected(area: &str) -> BackendError {
        BackendError::with_code(
            format!("{area}.not_connected"),
            "the connection is broken (its transaction state is unknown); reconnect",
        )
    }

    pub(crate) fn mark_broken(&self) {
        self.broken.store(true, Ordering::Release);
    }

    /// Fails with `<area>.not_connected` once the connection is broken.
    pub(crate) fn ensure_usable(&self, area: &str) -> Result<(), BackendError> {
        if self.broken.load(Ordering::Acquire) {
            Err(Self::not_connected(area))
        } else {
            Ok(())
        }
    }

    /// Waits for the session (blocking the calling thread) for at most
    /// [`GATE_WAIT`]: a call that still holds it is stuck on a server that does
    /// not answer, and the next call must fail (`<area>.busy`) instead of
    /// waiting with it. A gate poisoned by a panic marks the connection broken.
    pub(crate) fn lock(&self, area: &str) -> Result<MutexGuard<'_, ()>, BackendError> {
        self.lock_within(area, GATE_WAIT)
    }

    fn lock_within(&self, area: &str, wait: Duration) -> Result<MutexGuard<'_, ()>, BackendError> {
        self.ensure_usable(area)?;
        let deadline = Instant::now() + wait;
        let mut pause = Duration::from_millis(1);
        loop {
            match self.lock.try_lock() {
                Ok(guard) => {
                    // Another call may have broken the connection while we waited.
                    self.ensure_usable(area)?;
                    return Ok(guard);
                }
                Err(TryLockError::Poisoned(_)) => {
                    self.mark_broken();
                    return Err(Self::not_connected(area));
                }
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(50));
                    // The holder may have broken the connection (abandoned its statement).
                    self.ensure_usable(area)?;
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(BackendError::with_code(
                        format!("{area}.busy"),
                        "the connection is busy with another statement that has not finished; try again",
                    )
                    .retryable());
                }
            }
        }
    }
}

pub(crate) struct PgConnection {
    /// The user session.
    pub(crate) inner: PostgresConn,
    gate: Gate,
    pub(crate) cancel: Arc<CancelState>,
    metadata: tokio::sync::Mutex<Option<Arc<PostgresConn>>>,
    /// Databases whose client may hold session settings the user changed (see
    /// [`PgConnection::restore_settings`]).
    settings_stale: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl PgConnection {
    pub(crate) fn new(inner: PostgresConn) -> Self {
        let cancel = Arc::new(CancelState::new(inner.v2_transport()));
        Self {
            inner,
            gate: Gate::default(),
            cancel,
            metadata: tokio::sync::Mutex::new(None),
            settings_stale: std::sync::Mutex::default(),
        }
    }

    /// User SQL may have changed the settings the driver reads text under on the
    /// client of `database`.
    pub(crate) fn mark_settings_stale(&self, database: &str) {
        self.settings_stale
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(database.to_owned());
    }

    /// Puts the driver's session settings back on `client` if user SQL may have
    /// changed them. Tried again at the next call when it fails (an aborted
    /// transaction, a cancel); it never fails the call it precedes.
    pub(crate) async fn restore_settings(&self, client: &Client, database: &str) {
        let stale = self
            .settings_stale
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(database);
        if stale
            && client
                .batch_execute(crate::db::postgres::V2_SESSION_SETUP)
                .await
                .is_err()
        {
            self.mark_settings_stale(database);
        }
    }

    pub(crate) fn mark_broken(&self) {
        self.gate.mark_broken();
    }

    /// A statement was dropped after the server ignored a cancel for
    /// [`CANCEL_GRACE`]: the connection can no longer be used.
    pub(crate) fn abandoned(&self, area: &str) -> BackendError {
        self.mark_broken();
        Abandoned::error(area)
    }

    pub(crate) fn ensure_usable(&self, area: &str) -> Result<(), BackendError> {
        self.gate.ensure_usable(area)
    }

    /// Waits for the user session; see [`Gate::lock`].
    pub(crate) fn lock_gate(&self, area: &str) -> Result<MutexGuard<'_, ()>, BackendError> {
        self.gate.lock(area)
    }

    /// `database` or the connection's default database.
    pub(crate) fn database_or_default<'a>(&'a self, database: Option<&'a str>) -> &'a str {
        database
            .filter(|database| !database.trim().is_empty())
            .unwrap_or_else(|| self.inner.default_database())
    }

    async fn metadata(&self) -> Result<Arc<PostgresConn>, String> {
        let mut slot = self.metadata.lock().await;
        if let Some(connection) = slot.as_ref() {
            return Ok(Arc::clone(connection));
        }
        // Same configuration *and transport* as the session: the TLS pin (`prefer` never
        // goes clear text after the session used TLS) and the session's TLS info are shared.
        let connection = Arc::new(self.inner.v2_reconnect().await?);
        *slot = Some(Arc::clone(&connection));
        Ok(connection)
    }

    /// Runs a catalog call on the metadata connection (never behind the gate),
    /// bounded by [`CATALOG_TIMEOUT`]. A timeout or a failure other than
    /// `keep_on` drops the metadata connection so the next call starts fresh.
    pub(crate) fn with_metadata<T, Fut>(
        &self,
        area: &str,
        what: &str,
        keep_on: impl Fn(&str) -> bool,
        call: impl FnOnce(Arc<PostgresConn>) -> Fut,
    ) -> Result<T, BackendError>
    where
        Fut: Future<Output = Result<T, String>>,
    {
        self.ensure_usable(area)?;
        let outcome = runtime::block_on(async {
            let attempt = tokio::time::timeout(CATALOG_TIMEOUT, async {
                let connection = self.metadata().await?;
                call(connection).await
            })
            .await;
            let failed = !matches!(&attempt, Ok(Ok(_)));
            let keep = matches!(&attempt, Ok(Err(message)) if keep_on(message));
            if failed && !keep {
                *self.metadata.lock().await = None;
            }
            attempt
        })
        .map_err(BackendError::new)?;
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(BackendError::new(message)),
            Err(_) => Err(BackendError::new(format!("{what} timed out"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio_postgres::NoTls;

    #[test]
    fn a_panic_that_poisons_the_gate_breaks_the_connection() {
        let gate = Arc::new(Gate::default());
        assert!(gate.lock("query").is_ok());
        let poisoner = Arc::clone(&gate);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock("query").unwrap();
            panic!("backend panicked while holding the session");
        })
        .join();
        for area in ["query", "mutation", "navigator"] {
            let error = gate.lock(area).expect_err("broken");
            assert_eq!(
                error.code.as_deref(),
                Some(&*format!("{area}.not_connected"))
            );
            assert!(gate.ensure_usable(area).is_err());
        }
    }

    #[test]
    fn marking_the_connection_broken_fails_every_area() {
        let gate = Gate::default();
        assert!(gate.ensure_usable("describe").is_ok());
        gate.mark_broken();
        let error = gate.lock("mutation").expect_err("broken");
        assert_eq!(error.code.as_deref(), Some("mutation.not_connected"));
        assert_eq!(
            gate.ensure_usable("describe").unwrap_err().code.as_deref(),
            Some("describe.not_connected")
        );
    }

    #[test]
    fn a_cancel_request_that_was_sent_is_settled_before_the_operation_ends() {
        let state = Arc::new(CancelState::default());
        // Nothing was sent: there is nothing to settle (checked on the state, not
        // on the clock: a loaded runner can deschedule a thread for longer than SETTLE).
        assert!(!state.stop_delivering());
        // A request was sent (it may be on its way to the backend): ending waits.
        state.lock().unacknowledged = true;
        let started = Instant::now();
        state.disarm();
        assert!(started.elapsed() >= SETTLE, "{:?}", started.elapsed());
        assert!(!state.lock().unacknowledged, "the mark was not taken");
        // Once: the guard's drop after an explicit disarm finds nothing to settle.
        assert!(!state.stop_delivering());
    }

    /// Needs a server: `TABLEX_TEST_PG_URI`. The way out of the operation does not
    /// matter: dropping the guard (an early return) settles like `disarm`.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
    fn dropping_the_guard_after_a_cancel_waits_for_the_request_to_settle() {
        let uri = std::env::var("TABLEX_TEST_PG_URI")
            .expect("set TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)");
        let client = runtime::block_on(async {
            let (client, connection) = tokio_postgres::connect(&uri, NoTls).await.unwrap();
            tokio::spawn(connection);
            Arc::new(client)
        })
        .unwrap();
        let state = Arc::new(CancelState::default());
        let armed = state.arm(OperationId(1), client.cancel_token());
        state.cancel(OperationId(1)).unwrap();
        assert!(armed.requested());
        let started = Instant::now();
        drop(armed);
        assert!(started.elapsed() >= SETTLE, "{:?}", started.elapsed());
        // The request was discarded by the idle backend (or landed before this).
        let ok = runtime::block_on(client.simple_query("SELECT pg_sleep(0.1)")).unwrap();
        assert!(ok.is_ok(), "{ok:?}");
    }

    // ---- acknowledged cancel against a fake server (no PostgreSQL needed) ----------------------------

    use crate::v2::tls::tests::{fake, fake_uri, write_ca};
    use crate::v2::tls::DialTarget;

    /// A fake server that serves a session and then `cancels` cancel connections.
    fn fake_server(cancels: usize, linger: fake::Linger) -> fake::Server {
        let script = vec![fake::Answer::No; 1 + cancels];
        runtime::block_on(fake::start_with(script, fake::Auth::Ok, linger)).unwrap()
    }

    fn uri(server: &fake::Server) -> String {
        format!("postgres://u@127.0.0.1:{}/d?sslmode=disable", server.port)
    }

    /// A session on `server`; its cancels are dialled as `dial` says (the transport
    /// built from that URI), which is `server` itself unless a test says otherwise.
    fn session_on(server: &fake::Server, dial: &str) -> (Arc<CancelState>, Client) {
        let (config, transport) = Transport::prepare(&uri(server)).unwrap();
        let client = runtime::block_on(transport.connect(&config))
            .unwrap()
            .unwrap();
        let (_, dialled) = Transport::prepare(dial).unwrap();
        (Arc::new(CancelState::new(dialled)), client)
    }

    fn closed(server: &fake::Server) -> usize {
        server.closed.load(Ordering::Acquire)
    }

    /// Waits until `server` has closed `count` cancel connections: the exact request is
    /// not acknowledged, so the client is done before the server has read it.
    fn wait_for_closed(server: &fake::Server, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while closed(server) < count {
            assert!(
                Instant::now() < deadline,
                "closed {} of {count}",
                closed(server)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    const CLOSES_AFTER: Duration = Duration::from_millis(300);

    #[test]
    fn an_acknowledged_cancel_returns_after_the_close_and_needs_no_settle() {
        let server = fake_server(1, fake::Linger::After(CLOSES_AFTER));
        let (state, client) = session_on(&server, &uri(&server));
        let _armed = state.arm(OperationId(1), client.cancel_token());
        state.cancel(OperationId(1)).unwrap();
        // Counted by the server right before it closed: the delivery waited for it.
        assert_eq!(closed(&server), 1);
        // Nothing is left to settle (state, not clock).
        assert!(!state.stop_delivering());
    }

    #[test]
    fn a_cancel_the_server_never_closes_ends_at_the_timeout_and_is_settled() {
        let server = fake_server(1, fake::Linger::Forever);
        let (state, client) = session_on(&server, &uri(&server));
        let _armed = state.arm(OperationId(1), client.cancel_token());
        let started = Instant::now();
        let error = state.cancel(OperationId(1)).unwrap_err();
        assert!(error.message.contains("timed out"), "{}", error.message);
        // Bounded by CANCEL_TIMEOUT, not by the server.
        assert!(started.elapsed() >= CANCEL_TIMEOUT);
        assert!(started.elapsed() < CANCEL_TIMEOUT + Duration::from_secs(3));
        // The request may still be on its way: the operation settles.
        assert!(state.stop_delivering());
    }

    fn dead_uri() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=disable",
            listener.local_addr().unwrap().port()
        )
    }

    #[test]
    fn a_delivery_nobody_accepts_falls_back_to_the_exact_request_and_is_settled() {
        let server = fake_server(1, fake::Linger::Close);
        let (state, client) = session_on(&server, &dead_uri());
        let _armed = state.arm(OperationId(1), client.cancel_token());
        state.cancel(OperationId(1)).unwrap();
        // The token's own socket (the session's server) got the request.
        wait_for_closed(&server, 1);
        assert!(state.stop_delivering(), "an unacknowledged request settles");
    }

    #[test]
    fn a_configuration_without_a_target_keeps_the_unacknowledged_cancel_and_the_settle() {
        // Several hosts: no acknowledged path; the exact request is sent and settled.
        let server = fake_server(1, fake::Linger::Close);
        let (config, transport) = Transport::prepare(&uri(&server)).unwrap();
        let client = runtime::block_on(transport.connect(&config))
            .unwrap()
            .unwrap();
        let (_, many) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{0},127.0.0.1:{0}/d?sslmode=disable",
            server.port
        ))
        .unwrap();
        let state = Arc::new(CancelState::new(many));
        let _armed = state.arm(OperationId(1), client.cancel_token());
        state.cancel(OperationId(1)).unwrap();
        wait_for_closed(&server, 1);
        assert!(state.stop_delivering());
    }

    /// The session's server `a` (TLS, every certificate for `127.0.0.1` signed by the CA in
    /// `ca`) and a `Transport` for the cancel state that trusts that CA and dials `dial_port`.
    fn tls_session(
        dial: fake::Answer,
        ca: &std::path::Path,
        acceptor: tokio_rustls::TlsAcceptor,
    ) -> (fake::Server, fake::Server, Arc<CancelState>, Client) {
        let session = runtime::block_on(fake::start_with(
            vec![
                fake::Answer::Yes(acceptor.clone()),
                fake::Answer::Yes(acceptor),
            ],
            fake::Auth::Ok,
            fake::Linger::Close,
        ))
        .unwrap();
        let dialled = runtime::block_on(fake::start(vec![dial], fake::Auth::Ok)).unwrap();
        let query = "sslmode=verify-full&sslrootcert={ca}";
        let (config, transport) = Transport::prepare(&fake_uri(session.port, query, ca)).unwrap();
        let client = runtime::block_on(transport.connect(&config))
            .unwrap()
            .unwrap();
        let (_, dialling) = Transport::prepare(&fake_uri(dialled.port, query, ca)).unwrap();
        (
            session,
            dialled,
            Arc::new(CancelState::new(dialling)),
            client,
        )
    }

    #[test]
    fn a_dial_that_hangs_still_sends_the_exact_request_within_the_cancel_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (acceptor, ca_pem) = fake::acceptor(&["127.0.0.1"]);
        let ca = write_ca(dir.path(), &ca_pem);
        // The dialled server accepts and never answers the `SSLRequest`.
        let (session, _dialled, state, client) = tls_session(fake::Answer::Silent, &ca, acceptor);
        let _armed = state.arm(OperationId(1), client.cancel_token());
        let started = Instant::now();
        state.cancel(OperationId(1)).unwrap();
        assert!(
            started.elapsed() < CANCEL_TIMEOUT,
            "{:?}",
            started.elapsed()
        );
        wait_for_closed(&session, 1);
        assert!(state.stop_delivering(), "the exact request settles");
    }

    #[test]
    fn a_handshake_error_on_the_acknowledged_path_sends_the_exact_request() {
        let dir = tempfile::tempdir().unwrap();
        let (acceptor, ca_pem) = fake::acceptor(&["127.0.0.1"]);
        let ca = write_ca(dir.path(), &ca_pem);
        // The dialled server's certificate comes from a CA the session does not trust.
        let (untrusted, _) = fake::acceptor(&["127.0.0.1"]);
        let (session, dialled, state, client) =
            tls_session(fake::Answer::Yes(untrusted), &ca, acceptor);
        let _armed = state.arm(OperationId(1), client.cancel_token());
        state.cancel(OperationId(1)).unwrap();
        wait_for_closed(&session, 1);
        assert_eq!(closed(&dialled), 0, "nothing reached the untrusted server");
        assert!(state.stop_delivering());
    }

    #[test]
    fn every_delivery_is_acknowledged_and_none_sends_the_exact_request() {
        let server = fake_server(8, fake::Linger::Close);
        let (state, client) = session_on(&server, &uri(&server));
        let armed = state.arm(OperationId(1), client.cancel_token());
        state.cancel(OperationId(1)).unwrap();
        assert_eq!(closed(&server), 1);
        // The statement ignores every cancel: redeliveries after 50, 100, 200 ... ms.
        // The wait is the watched work itself, so a delivery that has started is finished
        // (not dropped half way, which would count as unacknowledged) before `watch` returns.
        runtime::block_on(armed.watch(async {
            while closed(&server) < 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }))
        .unwrap()
        .unwrap();
        // One request per delivery (the exact request would be a second per delivery,
        // and would mark the operation), all acknowledged: nothing to settle.
        assert!(!state.lock().unacknowledged);
        assert!(!state.stop_delivering());
    }

    #[test]
    fn a_drain_that_gives_up_with_a_delivery_in_flight_is_unacknowledged() {
        let state = CancelState::default();
        state.lock().in_flight = 1;
        assert!(state.stop_delivering_within(Duration::from_millis(50)));
        state.lock().in_flight = 0;
        assert!(!state.stop_delivering_within(Duration::from_millis(50)));
    }

    #[test]
    fn finishing_an_operation_waits_for_cancels_still_on_the_wire() {
        let state = Arc::new(CancelState::default());
        // A cancel for the running operation is being delivered ...
        state.lock().in_flight = 1;
        let deliverer = Arc::clone(&state);
        let delivery = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(Delivered(&deliverer));
        });
        // ... so the operation cannot finish (and the next one start) before it lands.
        let started = Instant::now();
        state.disarm();
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(state.lock().in_flight, 0);
        delivery.join().unwrap();
        // With nothing in flight finishing is immediate.
        let started = Instant::now();
        state.disarm();
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// `uri` with the literal host `127.0.0.1` replaced by the name `localhost`.
    fn localhost_uri(uri: &str) -> String {
        let (head, tail) = uri
            .split_once("@127.0.0.1")
            .expect("TABLEX_TEST_PG_URI must name the host 127.0.0.1");
        assert!(tail.starts_with([':', '/', '?']), "{uri}");
        format!("{head}@localhost{tail}")
    }

    /// Needs a server: `TABLEX_TEST_PG_URI`. The cancel goes over the acknowledged path
    /// (a literal address, then the host name `localhost`, which the session resolves
    /// and pins): the server closed the connection after it signalled the backend, so
    /// the operation has nothing to settle and the next statement is safe.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
    fn a_cancel_over_the_acknowledged_path_leaves_no_settle_wait() {
        let literal = std::env::var("TABLEX_TEST_PG_URI")
            .expect("set TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)");
        for (uri, named) in [(literal.clone(), false), (localhost_uri(&literal), true)] {
            let (config, mut transport) = Transport::prepare(&uri).unwrap();
            let client = Arc::new(
                runtime::block_on(transport.connect_session(&config))
                    .unwrap()
                    .unwrap(),
            );
            // The observer is a second connection of the same transport.
            let observer = runtime::block_on(transport.connect(&config))
                .unwrap()
                .unwrap();
            if named {
                assert!(
                    matches!(transport.dial_target(), Some(DialTarget::Tcp { host, .. }) if host == "localhost"),
                    "the name was not pinned: {:?}",
                    transport.dial_target()
                );
            }
            let pid = {
                let rows = runtime::block_on(client.simple_query("SELECT pg_backend_pid()"))
                    .unwrap()
                    .unwrap();
                rows.iter()
                    .find_map(|message| match message {
                        SimpleQueryMessage::Row(row) => row.get(0)?.parse::<i32>().ok(),
                        _ => None,
                    })
                    .expect("the backend pid")
            };
            let state = Arc::new(CancelState::new(transport));
            for round in 0..20 {
                let armed = state.arm(OperationId(round), client.cancel_token());
                let sleeper = {
                    let client = Arc::clone(&client);
                    std::thread::spawn(move || {
                        runtime::block_on(client.simple_query("SELECT pg_sleep(30)")).unwrap()
                    })
                };
                // The statement is executing on the server (not merely written).
                let running = format!(
                    "SELECT 1 FROM pg_stat_activity WHERE pid = {pid} AND state = 'active' \
                     AND query LIKE 'SELECT pg_sleep(30)%'"
                );
                let deadline = Instant::now() + Duration::from_secs(10);
                while runtime::block_on(observer.simple_query(&running))
                    .unwrap()
                    .unwrap()
                    .iter()
                    .all(|message| !matches!(message, SimpleQueryMessage::Row(_)))
                {
                    assert!(
                        Instant::now() < deadline,
                        "{uri} round {round}: not running"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                state.cancel(OperationId(round)).unwrap();
                let error = sleeper.join().unwrap().unwrap_err();
                assert_eq!(error.code(), Some(&SqlState::QUERY_CANCELED));
                assert!(
                    !state.stop_delivering(),
                    "{uri} round {round}: an acknowledged request must leave nothing to settle"
                );
                drop(armed);
                // The next statement runs undisturbed.
                let next = runtime::block_on(client.simple_query("SELECT pg_sleep(0.05)")).unwrap();
                assert!(next.is_ok(), "{uri} round {round}: {next:?}");
            }
        }
    }

    /// Needs a server: `TABLEX_TEST_PG_URI`.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
    fn a_cancel_reaches_only_the_armed_operation() {
        let uri = std::env::var("TABLEX_TEST_PG_URI")
            .expect("set TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)");
        let client = runtime::block_on(async {
            let (client, connection) = tokio_postgres::connect(&uri, NoTls).await.unwrap();
            tokio::spawn(connection);
            Arc::new(client)
        })
        .unwrap();
        let state = Arc::new(CancelState::default());
        let sleep = |client: Arc<tokio_postgres::Client>| {
            std::thread::spawn(move || {
                runtime::block_on(async move { client.simple_query("SELECT pg_sleep(1.5)").await })
                    .unwrap()
            })
        };

        // Operation 1 runs; a cancel for operation 2 changes nothing.
        let armed = state.arm(OperationId(1), client.cancel_token());
        let query = sleep(Arc::clone(&client));
        std::thread::sleep(Duration::from_millis(300));
        state.cancel(OperationId(2)).unwrap();
        assert!(
            query.join().unwrap().is_ok(),
            "a cancel for another operation must not land"
        );
        drop(armed);

        // The same statement is cancelled when its own id is named, and a cancel
        // after the operation finished is dropped.
        let armed = state.arm(OperationId(3), client.cancel_token());
        let query = sleep(Arc::clone(&client));
        std::thread::sleep(Duration::from_millis(300));
        state.cancel(OperationId(3)).unwrap();
        let error = query.join().unwrap().unwrap_err();
        assert_eq!(
            error.code(),
            Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
        );
        drop(armed);
        state.cancel(OperationId(3)).unwrap();
        let again = sleep(Arc::clone(&client));
        assert!(
            again.join().unwrap().is_ok(),
            "a late cancel must not hit the next statement"
        );
    }

    /// Needs a server: `TABLEX_TEST_PG_URI`.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)"]
    fn a_cancel_the_server_ignored_is_sent_again_while_the_statement_runs() {
        let uri = std::env::var("TABLEX_TEST_PG_URI")
            .expect("set TABLEX_TEST_PG_URI (dedicated PostgreSQL test server)");
        let client = runtime::block_on(async {
            let (client, connection) = tokio_postgres::connect(&uri, NoTls).await.unwrap();
            tokio::spawn(connection);
            Arc::new(client)
        })
        .unwrap();
        let state = Arc::new(CancelState::default());
        let armed = state.arm(OperationId(1), client.cancel_token());
        // The backend is idle: the server discards this request.
        assert!(!armed.requested());
        state.cancel(OperationId(1)).unwrap();
        assert!(
            armed.requested(),
            "the operation sees the request before its next round trip"
        );
        std::thread::sleep(Duration::from_millis(200));
        // The statement starts after the request was discarded; the watcher sends it again.
        let started = Instant::now();
        let error = runtime::block_on(armed.watch(client.simple_query("SELECT pg_sleep(30)")))
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.code(),
            Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // Nothing is sent once the statement returned, and the connection is clean.
        armed.disarm();
        drop(armed);
        let again = runtime::block_on(client.simple_query("SELECT pg_sleep(0.3)")).unwrap();
        assert!(again.is_ok(), "{again:?}");
    }

    #[test]
    fn a_statement_the_server_never_answers_is_dropped_after_the_grace_period() {
        let server = fake_server(16, fake::Linger::Close);
        let (state, client) = session_on(&server, &uri(&server));
        let armed = state.arm(OperationId(1), client.cancel_token());
        // Nobody asked to cancel: a slow statement is polled for as long as it takes.
        let slow = armed.watch_within(Duration::from_millis(50), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            7
        });
        assert_eq!(runtime::block_on(slow).unwrap().unwrap(), 7);
        // Cancelled, and the server does not react (this work never ends): dropped.
        state.cancel(OperationId(1)).unwrap();
        let started = Instant::now();
        let outcome = runtime::block_on(
            armed.watch_within(Duration::from_millis(300), std::future::pending::<()>()),
        )
        .unwrap();
        assert!(outcome.is_err(), "the work was not dropped");
        assert!(
            started.elapsed() >= Duration::from_millis(300)
                && started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // A cancel the server does answer inside the grace period is not an abandon.
        let armed = state.arm(OperationId(2), client.cancel_token());
        state.cancel(OperationId(2)).unwrap();
        let answered = armed.watch_within(Duration::from_secs(5), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert!(runtime::block_on(answered).unwrap().is_ok());
    }

    #[test]
    fn an_abandoned_statement_breaks_the_connection_for_every_area() {
        let error = Abandoned::error("query");
        assert_eq!(error.code.as_deref(), Some("query.not_connected"));
        let gate = Gate::default();
        gate.mark_broken();
        assert_eq!(
            gate.lock("mutation").unwrap_err().code.as_deref(),
            Some("mutation.not_connected")
        );
    }

    #[test]
    fn waiting_for_a_busy_session_gives_up_instead_of_blocking_forever() {
        let gate = Arc::new(Gate::default());
        let holder = gate.lock("query").unwrap();
        let started = Instant::now();
        let error = gate
            .lock_within("query", Duration::from_millis(150))
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("query.busy"));
        assert!(error.retryable);
        assert!(
            started.elapsed() >= Duration::from_millis(150)
                && started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // A holder that finishes inside the wait hands the session over.
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.lock_within("query", Duration::from_secs(5)).map(|_| ()))
        };
        std::thread::sleep(Duration::from_millis(100));
        drop(holder);
        assert!(waiter.join().unwrap().is_ok());
        // A holder that broke the connection (abandoned its statement) frees the waiters at once.
        let holder = gate.lock("query").unwrap();
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.lock_within("query", Duration::from_secs(30))
                    .map(|_| ())
                    .unwrap_err()
                    .code
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        gate.mark_broken();
        assert_eq!(
            waiter.join().unwrap().as_deref(),
            Some("query.not_connected")
        );
        drop(holder);
    }

    #[test]
    fn a_cancel_for_another_operation_is_dropped_without_a_request() {
        let state = CancelState::default();
        // Nothing armed: late cancels for finished operations are no-ops.
        assert!(state.cancel(OperationId(7)).is_ok());
        assert_eq!(state.lock().in_flight, 0);
    }
}
