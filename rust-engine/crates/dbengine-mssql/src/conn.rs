//! The shared Tokio runtime, one exclusively locked TDS client per kit connection (plus a lazily
//! opened second one for catalog calls), error mapping and cancellation.
//!
//! Cancellation is the TDS *attention* signal: the running statement's future is dropped and
//! `Client::cancel_query` sends the attention packet and drains the reply up to its
//! acknowledgement, so the session stays usable and keeps its transaction. The canceller only
//! wakes the running operation (`Shared`); it needs no second connection and takes no lock.

use crate::config::{Config, Encrypt};
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::OperationId;
use engine_driver_kit::BackendError;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use tiberius::error::Error as TdsError;
use tiberius::{AuthMethod, EncryptionLevel};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub(crate) type Client = tiberius::Client<Compat<TcpStream>>;

/// What every client library of SQL Server sets and a bare TDS login does not: without them DML on
/// a table with a filtered index, an indexed view, an indexed computed column or an XML index
/// fails (error 1934), and `"name"` is not an identifier.
const SESSION_SETUP: &str = "SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON; SET ANSI_PADDING ON; \
    SET ANSI_WARNINGS ON; SET ANSI_NULL_DFLT_ON ON; SET CONCAT_NULL_YIELDS_NULL ON; \
    SET ARITHABORT ON; SET NUMERIC_ROUNDABORT OFF";
/// How long a cancel waits for the server to acknowledge the attention signal. A server that
/// does not answer (a dead peer) would otherwise hold the calling thread and the connection
/// for ever: past this the connection is closed (`<area>.not_connected`, the app reconnects).
pub(crate) const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How long a call waits for the connection while another call still holds it.
const GATE_WAIT: Duration = Duration::from_secs(30);
/// Upper bound of one catalog call (navigator, describe), connect included.
const META_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle time before TCP keepalive probes start, and the pause between probes: a vanished
/// server is noticed in a few minutes, not hours.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// How long `disconnect` waits for a statement it just asked to stop.
const DISCONNECT_WAIT: Duration = Duration::from_secs(3);

/// One runtime per loaded driver image (`dbengine_shutdown` must not tear down process-wide
/// statics: the host may `init` again in the same image).
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tablex-mssql-driver")
            .build()
            .expect("failed to create the SQL Server driver runtime")
    })
}

pub(crate) fn block_on<T>(future: impl Future<Output = T>) -> T {
    runtime().block_on(future)
}

/// Installs the `ring` rustls provider as the process default, once. tiberius uses an installed
/// provider as it is; without one it would fall back to aws-lc-rs.
pub(crate) fn install_ring() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The API area of a request, for the error codes the contract names per area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Area {
    Query,
    Mutation,
    Navigator,
    Describe,
}

impl Area {
    pub(crate) fn failed(self) -> &'static str {
        match self {
            Self::Query => "query.failed",
            Self::Mutation => codes::MUTATION_FAILED,
            Self::Navigator => "navigator.failed",
            Self::Describe => "describe.failed",
        }
    }

    pub(crate) fn cancelled(self) -> &'static str {
        match self {
            Self::Mutation => codes::MUTATION_CANCELLED,
            _ => codes::QUERY_CANCELLED,
        }
    }

    pub(crate) fn not_connected(self) -> &'static str {
        match self {
            Self::Query => "query.not_connected",
            Self::Mutation => "mutation.not_connected",
            Self::Navigator => "navigator.not_connected",
            Self::Describe => "describe.not_connected",
        }
    }

    fn busy(self) -> &'static str {
        match self {
            Self::Query => "query.busy",
            Self::Mutation => "mutation.busy",
            Self::Navigator => "navigator.busy",
            Self::Describe => "describe.busy",
        }
    }
}

/// A server error as SQL Server's own tools print it.
pub(crate) fn server_message(error: &tiberius::error::TokenError) -> String {
    format!(
        "Msg {}, Level {}, State {}: {}",
        error.code(),
        error.class(),
        error.state(),
        error.message()
    )
}

/// The connection itself is gone (as opposed to the server refusing a statement).
pub(crate) fn is_connection_lost(error: &TdsError) -> bool {
    matches!(
        error,
        TdsError::Io { .. } | TdsError::Tls(_) | TdsError::Protocol(_) | TdsError::Routing { .. }
    )
}

/// `error` as the wire error of `area`: a lost connection is `<area>.not_connected`.
pub(crate) fn map_error(area: Area, error: &TdsError) -> BackendError {
    match error {
        TdsError::Server(token) => BackendError::with_code(area.failed(), server_message(token)),
        other if is_connection_lost(other) => BackendError::with_code(
            area.not_connected(),
            format!("the connection to the server was lost: {other}"),
        ),
        other => BackendError::with_code(area.failed(), other.to_string()),
    }
}

/// Opens a TDS session: TCP connect, keepalive, TLS and login, all within the connect timeout.
pub(crate) async fn open_client(config: &Config) -> Result<Client, BackendError> {
    install_ring();
    let mut tds = tiberius::Config::new();
    tds.host(&config.host);
    tds.port(config.port);
    if let Some(database) = &config.database {
        tds.database(database);
    }
    tds.authentication(AuthMethod::sql_server(&config.user, &config.password));
    tds.application_name(&config.application_name);
    tds.encryption(match config.encrypt {
        Encrypt::Off => EncryptionLevel::Off,
        Encrypt::On => EncryptionLevel::Required,
        Encrypt::Strict => EncryptionLevel::Strict,
    });
    if config.trust_server_certificate {
        tds.trust_cert();
    } else if let Some(path) = &config.ca_cert {
        tds.trust_cert_ca(path);
    }
    if let Some(name) = &config.certificate_host {
        tds.hostname_in_certificate(name);
    }
    tds.readonly(config.read_only_intent);
    tds.handshake_timeout(Some(config.connect_timeout));
    let connect = async {
        let tcp = TcpStream::connect(tds.get_addr()).await?;
        tcp.set_nodelay(true)?;
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL);
        socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive)?;
        let mut client = Client::connect(tds, tcp.compat_write()).await?;
        client.simple_query(SESSION_SETUP).await?.into_results().await?;
        Ok::<_, TdsError>(client)
    };
    match tokio::time::timeout(config.connect_timeout, connect).await {
        Err(_) => Err(BackendError::new("SQL Server connect timed out")),
        Ok(Err(error)) => Err(BackendError::new(format!(
            "SQL Server connect failed: {}",
            match &error {
                TdsError::Server(token) => server_message(token),
                other => other.to_string(),
            }
        ))),
        Ok(Ok(client)) => Ok(client),
    }
}

/// Wakes the running operation when a cancel arrives. Shared by the connection and its canceller.
#[derive(Default)]
pub(crate) struct Shared {
    notify: Notify,
}

/// Cross-thread cancel handle. It holds no reference to the connection, so cancelling never
/// takes the connection lock.
pub struct MsCanceller {
    shared: Arc<Shared>,
}

impl MsCanceller {
    pub(crate) fn cancel(&self, _operation: OperationId) {
        // A permit left for an operation that already ended is told apart by the kit's own
        // flag (`ActiveOperation::cancel_requested`), checked by `cancellable`.
        self.shared.notify.notify_one();
    }
}

/// Why `cancellable` returned without a result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// The statement was stopped by an attention signal and the session is reusable.
    Cancelled,
    /// The server did not acknowledge the attention signal in time (or the connection broke):
    /// the session cannot be used again.
    Lost(String),
}

impl Stopped {
    pub(crate) fn error(self, area: Area) -> BackendError {
        match self {
            Self::Cancelled => BackendError::with_code(area.cancelled(), "the statement was cancelled"),
            Self::Lost(why) => BackendError::with_code(
                area.not_connected(),
                format!("the server did not acknowledge the cancel ({why}); the connection was dropped, reconnect"),
            ),
        }
    }
}

/// Runs `work` on `client` until it finishes or `cancel_requested()` turns true after a cancel
/// woke it. On a cancel the work is dropped, the attention signal is sent and its
/// acknowledgement awaited (at most [`CANCEL_GRACE`]).
pub(crate) async fn cancellable<T>(
    client: &mut Client,
    shared: &Shared,
    cancel_requested: &dyn Fn() -> bool,
    work: impl AsyncFnOnce(&mut Client) -> T,
) -> Result<T, Stopped> {
    // The cancel may have landed before the work started: look before sending anything.
    let outcome = if cancel_requested() {
        None
    } else {
        let mut work = std::pin::pin!(work(&mut *client));
        loop {
            tokio::select! {
                biased;
                out = &mut work => break Some(out),
                () = shared.notify.notified() => {
                    if cancel_requested() {
                        break None;
                    }
                }
            }
        }
    };
    match outcome {
        Some(out) => Ok(out),
        None => match tokio::time::timeout(CANCEL_GRACE, client.cancel_query()).await {
            Ok(Ok(())) => Err(Stopped::Cancelled),
            Ok(Err(error)) => Err(Stopped::Lost(error.to_string())),
            Err(_) => Err(Stopped::Lost("timed out".into())),
        },
    }
}

/// Mutable per-connection state, guarded by [`MsConnection::state`].
pub(crate) struct ConnState {
    /// `None` once disconnected or after the connection was marked broken.
    pub(crate) client: Option<Client>,
    /// Default database as far as this driver knows; `None` when uncertain (after `USE` /
    /// `DROP DATABASE` in user SQL), so the next request re-selects its database.
    pub(crate) current_db: Option<String>,
    broken: Arc<AtomicBool>,
}

impl ConnState {
    pub(crate) fn forget_current_db(&mut self) {
        self.current_db = None;
    }

    /// The connection can no longer be trusted: close it. Every later call fails with
    /// `<area>.not_connected`.
    pub(crate) fn mark_broken(&mut self) {
        self.broken.store(true, Ordering::SeqCst);
        let _runtime = runtime().enter();
        self.client = None;
    }
}

/// `USE [database]` unless it is already the default database (`current`, updated).
pub(crate) async fn use_database(
    client: &mut Client,
    current: &mut Option<String>,
    area: Area,
    database: Option<&str>,
) -> Result<(), BackendError> {
    let Some(database) = database.filter(|database| !database.is_empty()) else {
        return Ok(());
    };
    if current
        .as_deref()
        .is_some_and(|current| current.eq_ignore_ascii_case(database))
    {
        return Ok(());
    }
    let mut statement = String::from("USE ");
    crate::sql::push_ident(&mut statement, database);
    client
        .simple_query(statement)
        .await
        .map_err(|error| map_error(area, &error))?
        .into_results()
        .await
        .map_err(|error| map_error(area, &error))?;
    *current = Some(database.to_owned());
    Ok(())
}

/// A kit connection: one client, locked for the duration of every operation so requests of
/// one connection never interleave on the wire.
pub struct MsConnection {
    state: Mutex<ConnState>,
    /// The catalog connection (navigator, describe): opened lazily, so a catalog call never
    /// waits behind a running user statement.
    meta: Mutex<Option<Client>>,
    config: Arc<Config>,
    closed: AtomicBool,
    broken: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl MsConnection {
    pub(crate) fn connect(uri: &str) -> Result<Self, BackendError> {
        let config = Config::parse(uri)?;
        let client = block_on(open_client(&config))?;
        let broken = Arc::new(AtomicBool::new(false));
        Ok(Self {
            state: Mutex::new(ConnState {
                client: Some(client),
                current_db: config.database.clone(),
                broken: Arc::clone(&broken),
            }),
            meta: Mutex::new(None),
            config: Arc::new(config),
            closed: AtomicBool::new(false),
            broken,
            shared: Arc::new(Shared::default()),
        })
    }

    /// The database named in the URI, if any.
    pub(crate) fn configured_database(&self) -> Option<&str> {
        self.config.database.as_deref()
    }

    pub(crate) fn shared(&self) -> &Shared {
        &self.shared
    }

    pub(crate) fn canceller(&self) -> MsCanceller {
        MsCanceller {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The connection lock. Waits (blocking the calling thread) for at most [`GATE_WAIT`]: a
    /// call that still holds it is stuck on a server that does not answer, and the next call
    /// must fail (`<area>.busy`) instead of waiting with it.
    pub(crate) fn lock(&self, area: Area) -> Result<MutexGuard<'_, ConnState>, BackendError> {
        self.lock_within(area, GATE_WAIT)
    }

    fn lock_within(&self, area: Area, wait: Duration) -> Result<MutexGuard<'_, ConnState>, BackendError> {
        let deadline = Instant::now() + wait;
        let mut pause = Duration::from_millis(1);
        loop {
            match self.state.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(BackendError::with_code(
                        area.not_connected(),
                        "the connection state is poisoned",
                    ))
                }
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(50));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(BackendError::with_code(
                        area.busy(),
                        "the connection is busy with another statement that has not finished; try again",
                    )
                    .retryable())
                }
            }
        }
    }

    /// The connection was closed by the host or as untrustworthy.
    pub(crate) fn is_broken(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.broken.load(Ordering::SeqCst)
    }

    /// Runs a catalog call on the separate metadata connection: it never waits behind a running
    /// user statement and is bounded by [`META_TIMEOUT`] (connect included). A failed or
    /// timed-out call drops the metadata connection; the next call opens a fresh one.
    pub(crate) fn with_meta<T>(
        &self,
        area: Area,
        work: impl AsyncFnOnce(&mut Client) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        if self.is_broken() {
            return Err(BackendError::with_code(area.not_connected(), "connection closed"));
        }
        let mut meta = self
            .meta
            .lock()
            .map_err(|_| BackendError::new("metadata connection lock poisoned"))?;
        let outcome = block_on(async { tokio::time::timeout(META_TIMEOUT, async {
            if meta.is_none() {
                let client = open_client(&self.config).await.map_err(|error| {
                    BackendError::with_code(area.failed(), format!("metadata {error}"))
                })?;
                *meta = Some(client);
            }
            let client = meta.as_mut().expect("opened above");
            work(client).await
        })
        .await });
        let result = outcome.unwrap_or_else(|_| {
            Err(BackendError::with_code(area.failed(), "metadata query timed out"))
        });
        if result.is_err() || self.is_broken() {
            // The statement may have left the connection mid-protocol.
            *meta = None;
        }
        result
    }

    /// Closes the client. Waits a bounded time for a statement that was just cancelled to release
    /// the lock; if it does not, the client is closed once the last reference goes away.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.shared.notify.notify_one();
        if let Ok(mut meta) = self.meta.try_lock() {
            *meta = None;
        }
        let deadline = Instant::now() + DISCONNECT_WAIT;
        let mut state = loop {
            match self.state.try_lock() {
                Ok(state) => break state,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => return,
            }
        };
        state.forget_current_db();
        if let Some(client) = state.client.take() {
            let _ = block_on(async { tokio::time::timeout(DISCONNECT_WAIT, client.close()).await });
        }
    }
}

impl Drop for MsConnection {
    fn drop(&mut self) {
        let _runtime = runtime().enter();
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        state.client = None;
        if let Ok(meta) = self.meta.get_mut() {
            *meta = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_connection_is_not_connected_and_a_server_refusal_is_a_failure() {
        let io = TdsError::Io {
            kind: tiberius::error::IoErrorKind::ConnectionReset,
            message: "reset".into(),
        };
        let error = map_error(Area::Query, &io);
        assert_eq!(error.code.as_deref(), Some("query.not_connected"));
        let conversion = TdsError::Conversion("nope".into());
        let error = map_error(Area::Mutation, &conversion);
        assert_eq!(error.code.as_deref(), Some("mutation.failed"));
    }

    #[test]
    fn areas_name_their_codes() {
        assert_eq!(Area::Query.cancelled(), "query.cancelled");
        assert_eq!(Area::Mutation.cancelled(), "mutation.cancelled");
        assert_eq!(Area::Navigator.not_connected(), "navigator.not_connected");
        assert_eq!(Area::Describe.failed(), "describe.failed");
    }
}
