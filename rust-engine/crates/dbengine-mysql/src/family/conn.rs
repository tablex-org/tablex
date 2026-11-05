//! The shared Tokio runtime, one exclusively locked `mysql_async::Conn` per kit
//! connection, error mapping and the `KILL QUERY` canceller.

use super::tls::{connect_error as tls_connect_error, TlsParams};
use super::{Dialect, Flavor};
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::api::OperationId;
use engine_driver_kit::sql::SqlLexOptions;
use engine_driver_kit::BackendError;
use mysql_async::consts::StatusFlags;
use mysql_async::{Conn, Opts, OptsBuilder};
use std::cell::Cell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// `ER_QUERY_INTERRUPTED`: the statement was stopped by `KILL QUERY`.
pub(super) const ER_QUERY_INTERRUPTED: u16 = 1317;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// After a cancel was requested and the statement is still running, `KILL QUERY`
/// is sent again after a pause that starts at [`REDELIVERY_FIRST`] and doubles up
/// to [`REDELIVERY_CEILING`]: a kill that did land may still be rolling the
/// statement back for minutes, and every kill opens a connection.
const REDELIVERY_FIRST: Duration = Duration::from_millis(50);
const REDELIVERY_CEILING: Duration = Duration::from_secs(5);

/// The pauses between redeliveries: 50, 100, 200 ... 3200, 5000, 5000 ... ms.
fn redelivery_pauses() -> impl Iterator<Item = Duration> {
    std::iter::successors(Some(REDELIVERY_FIRST), |pause| {
        Some((*pause * 2).min(REDELIVERY_CEILING))
    })
}
/// Attempts of a clean-up statement that keeps being interrupted by a late kill.
const CLEANUP_ATTEMPTS: usize = 3;
/// How long `disconnect` waits for a statement it just asked to stop.
const DISCONNECT_WAIT: Duration = Duration::from_secs(3);
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
/// Upper bound of one catalog call (navigator, describe), connect included.
const META_TIMEOUT: Duration = Duration::from_secs(10);

/// One runtime per loaded driver image. `dbengine_shutdown` must not tear down
/// process-wide statics (the host may `init` again in the same image), so it is
/// built once and lives for the process.
pub(super) fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tablex-mysql-driver")
            .build()
            .expect("failed to create the MySQL driver runtime")
    })
}

pub(super) fn block_on<T>(future: impl Future<Output = T>) -> T {
    runtime().block_on(future)
}

/// The API area of a request, for the error codes the contract names per area
/// (`<area>.cancelled`, `<area>.interrupted`, `<area>.not_connected`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Area {
    Query,
    Mutation,
}

impl Area {
    fn cancelled(self) -> &'static str {
        match self {
            Self::Query => codes::QUERY_CANCELLED,
            Self::Mutation => codes::MUTATION_CANCELLED,
        }
    }

    fn interrupted(self) -> &'static str {
        match self {
            Self::Query => codes::QUERY_INTERRUPTED,
            Self::Mutation => "mutation.interrupted",
        }
    }

    pub(super) fn not_connected(self) -> &'static str {
        match self {
            Self::Query => "query.not_connected",
            Self::Mutation => "mutation.not_connected",
        }
    }
}

/// Mutable per-connection state, guarded by [`MyConnection::state`].
pub(super) struct ConnState {
    /// `None` once disconnected or after the connection was marked broken.
    pub(super) conn: Option<Conn>,
    /// Default database as far as this driver knows; `None` when uncertain
    /// (after `USE`/`DROP` in user SQL), so the next request re-selects its
    /// database. Mirrored to [`MyConnection::db_hint`] for catalog calls.
    current_db: Option<String>,
    db_hint: Arc<Mutex<Option<String>>>,
    /// How the lexer reads this session's SQL (`sql_mode`).
    pub(super) lex: SqlLexOptions,
    base_lex: SqlLexOptions,
    /// `sql_mode` may have changed: re-read it before the next classification.
    lex_stale: bool,
    /// User SQL may have changed the session character set (`SET NAMES latin1`):
    /// talk utf8mb4 again before the next statement.
    charset_stale: bool,
    /// Set (shared with [`MyConnection`]) once the connection was closed as
    /// untrustworthy; catalog calls then fail with `not_connected` too.
    broken: Arc<AtomicBool>,
}

impl ConnState {
    pub(super) fn set_current_db(&mut self, database: Option<String>) {
        if let Ok(mut hint) = self.db_hint.lock() {
            hint.clone_from(&database);
        }
        self.current_db = database;
    }

    /// The default database is no longer known.
    pub(super) fn forget_current_db(&mut self) {
        self.set_current_db(None);
    }

    /// `USE database` unless it is already the default database.
    pub(super) async fn use_database(&mut self, database: Option<&str>) -> mysql_async::Result<()> {
        use mysql_async::prelude::Queryable;
        let Some(database) = database.filter(|database| !database.is_empty()) else {
            return Ok(());
        };
        if self.current_db.as_deref() == Some(database) {
            return Ok(());
        }
        let mut statement = String::from("USE ");
        super::sql::push_ident(&mut statement, database);
        let Some(conn) = self.conn.as_mut() else {
            return Err(other_error("connection closed"));
        };
        conn.query_drop(statement).await?;
        self.set_current_db(Some(database.to_owned()));
        Ok(())
    }

    /// Whether the session is inside a transaction the user opened, as the
    /// server says: the status flags of its last answer. After an error those
    /// are gone (and the error itself may have changed the answer: a deadlock
    /// rolls the transaction back, `COMMIT; BEGIN; <failing statement>` leaves a
    /// new one open), so a no-op `DO 0` is sent to fetch fresh flags instead of
    /// guessing from the statement text.
    ///
    /// If the state cannot be established the answer is an error, never "no
    /// transaction": a caller that then started its own transaction would commit
    /// the user's open one implicitly.
    pub(super) async fn in_user_transaction(&mut self) -> mysql_async::Result<bool> {
        use mysql_async::prelude::Queryable;
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| other_error("connection closed"))?;
        if conn.last_ok_packet().is_none() {
            conn.query_drop("DO 0").await?;
        }
        in_transaction(conn.last_ok_packet())
            .ok_or_else(|| other_error("the server did not report the transaction state"))
    }

    /// One statement without parameters or rows (transaction control).
    pub(super) async fn control(&mut self, statement: &str) -> mysql_async::Result<()> {
        use mysql_async::prelude::Queryable;
        self.conn
            .as_mut()
            .ok_or_else(|| other_error("connection closed"))?
            .query_drop(statement)
            .await
    }

    /// A clean-up statement (`ROLLBACK`, `COMMIT`'s undo): a kill that outlived
    /// its statement can still reach it (error 1317), so it is tried again, a
    /// few times.
    pub(super) async fn control_uninterrupted(
        &mut self,
        statement: &str,
    ) -> mysql_async::Result<()> {
        let mut attempts = 1;
        loop {
            match self.control(statement).await {
                Err(mysql_async::Error::Server(error))
                    if error.code == ER_QUERY_INTERRUPTED && attempts < CLEANUP_ATTEMPTS =>
                {
                    attempts += 1;
                }
                other => return other,
            }
        }
    }

    /// Records what a user statement may have done to `sql_mode` and to the
    /// session character set.
    pub(super) fn note_user_statement(&mut self, sql: &str) {
        if contains_ignore_case(sql, "sql_mode") {
            self.lex_stale = true;
        }
        if may_change_charset(sql) {
            self.charset_stale = true;
        }
    }

    /// The next [`refresh_lex`](Self::refresh_lex) re-reads `sql_mode`.
    pub(super) fn mark_lex_stale(&mut self) {
        self.lex_stale = true;
    }

    /// `sql_mode` could not be read at the last [`refresh_lex`](Self::refresh_lex): the
    /// lexer options may not be the session's.
    pub(super) fn lex_is_uncertain(&self) -> bool {
        self.lex_stale
    }

    /// Re-reads `sql_mode` if a user statement may have changed it, and puts the
    /// session back on utf8mb4 if one may have changed the character set. Text is
    /// decoded as UTF-8 and never repaired, so the session must be utf8mb4.
    pub(super) async fn refresh_lex(&mut self) {
        use mysql_async::prelude::Queryable;
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        if self.charset_stale {
            // A failure (a kill landed on it) leaves the flag set: tried again next time.
            self.charset_stale = conn.query_drop(SET_NAMES).await.is_err();
        }
        if !self.lex_stale {
            return;
        }
        let mode = read_sql_mode(conn).await;
        (self.lex, self.lex_stale) = lex_after_read(self.lex, self.base_lex, mode.as_deref());
    }

    /// The connection can no longer be trusted (a `ROLLBACK` failed, so the
    /// session may hold half a change): close it. Every later call fails with
    /// `<area>.not_connected`.
    pub(super) fn mark_broken(&mut self) {
        self.broken.store(true, Ordering::SeqCst);
        if let Some(conn) = self.conn.take() {
            // `Conn::drop` spawns the close handshake and needs a runtime context.
            let _runtime = runtime().enter();
            drop(conn);
        }
    }
}

/// `SERVER_STATUS_IN_TRANS` of the last OK packet; `None` without one.
fn in_transaction(ok: Option<&mysql_async::OkPacket<'_>>) -> Option<bool> {
    ok.map(|ok| {
        ok.status_flags()
            .contains(StatusFlags::SERVER_STATUS_IN_TRANS)
    })
}

/// A failure that means the connection itself is gone (as opposed to the
/// server refusing a statement).
pub(super) fn is_connection_lost(error: &mysql_async::Error) -> bool {
    matches!(
        error,
        mysql_async::Error::Io(_) | mysql_async::Error::Driver(_)
    )
}

/// The session character set the driver talks: text arrives as UTF-8.
pub(super) const SET_NAMES: &str = "SET NAMES utf8mb4";

/// Whether `sql` may change the session character set: `SET NAMES`, `SET CHARACTER
/// SET`, `SET [@@]character_set_*`, `SET charset`. A loose lexical test, so a
/// false positive costs one extra `SET NAMES`.
fn may_change_charset(sql: &str) -> bool {
    contains_ignore_case(sql, "character_set")
        || contains_ignore_case(sql, "charset")
        || (contains_ignore_case(sql, "names") && contains_ignore_case(sql, "set"))
        || (contains_ignore_case(sql, "character") && contains_ignore_case(sql, "set"))
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// The session's `sql_mode`; `None` if it could not be read (a kill landed on
/// the read, the connection broke).
async fn read_sql_mode(conn: &mut Conn) -> Option<String> {
    use mysql_async::prelude::Queryable;
    conn.query_first("SELECT @@SESSION.sql_mode")
        .await
        .ok()
        .flatten()
}

/// `sql_mode` and `@@version` of the session; `None` for whatever could not be read.
async fn read_session_settings(conn: &mut Conn) -> (Option<String>, Option<String>) {
    use mysql_async::prelude::Queryable;
    conn.query_first::<(String, String), _>("SELECT @@SESSION.sql_mode, @@version")
        .await
        .ok()
        .flatten()
        .unzip()
}

/// `base` knowing the server's `@@version`; a version that could not be read (the query
/// failed) or parsed leaves it unknown, and the classifier then treats any versioned
/// comment as ambiguous (a read-only run refuses it).
pub(super) fn base_with_version(base: SqlLexOptions, version: Option<&str>) -> SqlLexOptions {
    match version.and_then(server_version_number) {
        Some(version) => base.with_server_version(version),
        None => base,
    }
}

/// A server's `@@version` in the numeric form versioned comments use: `8.4.11` is
/// `80411`, MariaDB's `11.8.9-MariaDB-ubu2404` is `110809` (MariaDB 10 and older may
/// announce `5.5.5-10.6.12-MariaDB`: the prefix is a replication hack, not the version).
pub(super) fn server_version_number(version: &str) -> Option<u32> {
    let version = version.strip_prefix("5.5.5-").unwrap_or(version);
    let mut parts = version.splitn(3, '.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch_digits: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let patch = patch_digits.parse::<u32>().ok()?;
    (minor < 100 && patch < 100).then(|| major * 10_000 + minor * 100 + patch)
}

/// The lexer options after `sql_mode` was read, and whether they must be read
/// again. A failed read keeps the options in force (falling back to the base
/// ones would drop `ANSI_QUOTES` / `NO_BACKSLASH_ESCAPES`) and stays stale.
fn lex_after_read(
    current: SqlLexOptions,
    base: SqlLexOptions,
    mode: Option<&str>,
) -> (SqlLexOptions, bool) {
    match mode {
        Some(mode) => (lex_for_mode(base, mode), false),
        None => (current, true),
    }
}

pub(super) fn lex_for_mode(base: SqlLexOptions, sql_mode: &str) -> SqlLexOptions {
    let mut lex = base;
    for mode in sql_mode.split(',') {
        match mode.trim().to_ascii_uppercase().as_str() {
            "NO_BACKSLASH_ESCAPES" => lex = lex.without_backslash_escapes(),
            "ANSI_QUOTES" | "ANSI" => lex = lex.with_ansi_quotes(),
            // MariaDB `sql_mode=MSSQL` (also lists ANSI_QUOTES): `[name]` quotes an identifier,
            // `]]` escapes `]` (checked on MariaDB 11.8; MySQL has no such mode).
            "MSSQL" => lex.bracket_identifiers = true,
            _ => {}
        }
    }
    lex
}

pub(super) fn base_lex(dialect: Dialect) -> SqlLexOptions {
    match dialect {
        Dialect::MySql => SqlLexOptions::mysql(),
        Dialect::MariaDb => SqlLexOptions::mariadb(),
    }
}

/// The catalog connection: opened lazily, separate from the user's connection so
/// navigator and describe never wait behind a running query.
struct MetaState {
    conn: Option<Conn>,
}

/// A kit connection: one client, locked for the duration of every operation so
/// requests of one connection never interleave on the wire.
pub struct MyConnection {
    state: Mutex<ConnState>,
    /// The catalog connection (navigator, describe); see [`MetaState`].
    meta: Mutex<MetaState>,
    /// Set by `close`: catalog calls fail with `not_connected` afterwards.
    closed: AtomicBool,
    /// See [`ConnState::mark_broken`].
    broken: Arc<AtomicBool>,
    db_hint: Arc<Mutex<Option<String>>>,
    /// Options for the short-lived `KILL QUERY` connection and the catalog
    /// connection (no default database).
    kill_opts: Opts,
    thread_id: u32,
    /// The operation whose statement is running and whether a cancel was
    /// requested for it, in one word shared with the canceller (see
    /// [`running_word`]; 0 = none). One word, so a request is made with a
    /// compare-and-swap that can only ever mark the operation it names.
    running: Arc<AtomicU64>,
    /// `KILL QUERY`s between "this operation is still running" and "the server
    /// has processed the kill". See [`MyConnection::lock`].
    kills_in_flight: Arc<AtomicUsize>,
    /// Wakes [`ActiveMark::watch`] when a cancel is requested.
    wake: Arc<Notify>,
}

/// Cross-thread cancel handle: enough to open a second connection and kill the
/// first one's current statement. It holds no reference to the connection, so
/// cancelling never touches the connection lock.
pub struct MyCanceller {
    opts: Opts,
    thread_id: u32,
    running: Arc<AtomicU64>,
    kills_in_flight: Arc<AtomicUsize>,
    wake: Arc<Notify>,
}

/// Counts a `KILL QUERY` as in flight for as long as it lives.
struct KillGate<'a>(&'a AtomicUsize);

impl<'a> KillGate<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for KillGate<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Marks an operation as the one running on the connection for as long as it
/// lives. `KILL QUERY` targets a thread, not a statement, so the canceller
/// re-checks the mark right before it sends the kill: a cancel that arrives
/// after its operation ended (the kit's documented window) is dropped instead
/// of killing whatever statement the next operation started. The check and the
/// kill are not atomic; [`MyConnection::lock`] closes the remaining gap.
///
/// Create it right after `OperationGuard::activate` and check
/// `cancel_requested()` of the *kit* once it exists: a cancel that landed
/// between the two found no mark and sent no kill, but did set the kit's flag.
///
/// A kill the server receives while the thread is between two statements, or
/// during a statement too short to look at its kill flag, is discarded. The
/// operation therefore also looks at [`ActiveMark::cancel_requested`] before
/// every round trip it makes, and calls [`ActiveMark::end`] before its clean-up.
#[must_use = "the mark ends when the value is dropped"]
pub(super) struct ActiveMark<'a> {
    connection: &'a MyConnection,
    token: u64,
}

impl ActiveMark<'_> {
    /// Whether a cancel for this operation reached the driver after the mark
    /// was set. One atomic load, no lock: the cancel sets it as soon as it
    /// arrives, long before the kill (which needs a connection of its own) lands.
    pub(super) fn cancel_requested(&self) -> bool {
        self.connection.running.load(Ordering::SeqCst) == running_word(self.token) | REQUESTED
    }

    /// Looked at before every round trip: the cancelled error of `area`, so
    /// nothing more is sent.
    pub(super) fn check(&self, area: Area) -> Result<(), BackendError> {
        if self.cancel_requested() {
            Err(cancelled(area, None))
        } else {
            Ok(())
        }
    }

    /// Forgets the request (and that this is the running operation), unless
    /// another operation has been marked since.
    fn clear(&self) {
        end_running(&self.connection.running, self.token);
    }

    /// Runs `work`, the cancellable part of the operation. Once a cancel was
    /// requested and `work` is still running, the `KILL QUERY` is sent again
    /// (after 50, 100, 200 ... ms, up to every 5 s) until `work` returns: a
    /// kill the server discarded (the thread was between two round trips of a
    /// statement) is not lost. A kill whose connection is open is never cut off
    /// half way (it is finished when `work` returns first; one still in its
    /// handshake is dropped, it holds nothing on the server), none is started
    /// once `work` returned or after [`ActiveMark::end`], and the first one is
    /// the canceller's own. Without a cancel this only polls `work`.
    pub(super) async fn watch<T>(&self, work: impl Future<Output = T>) -> T {
        let mut work = std::pin::pin!(work);
        let canceller = loop {
            tokio::select! {
                biased;
                out = &mut work => return out,
                // A permit left by an earlier operation's cancel is not ours.
                () = self.connection.wake.notified() => {
                    if self.cancel_requested() {
                        break self.connection.canceller();
                    }
                }
            }
        };
        for pause in redelivery_pauses() {
            tokio::select! {
                biased;
                out = &mut work => return out,
                () = tokio::time::sleep(pause) => {}
            }
            // `committed`: the kill's connection is open and the gate entered, so
            // the kill is (about to be) on the wire and must be finished. Before
            // that - the handshake, which can hang - there is nothing to finish
            // and `work` is not held up by it. `work` is polled first: a kill is
            // never started for a statement that has already answered.
            let committed = Cell::new(false);
            let mut kill = std::pin::pin!(tokio::time::timeout(KILL_TIMEOUT, async {
                let killer = canceller.connect().await?;
                committed.set(true);
                canceller.send_kill(killer, self.token).await
            }));
            tokio::select! {
                biased;
                out = &mut work => {
                    if committed.get() {
                        let _ = kill.await;
                    }
                    return out;
                }
                _ = &mut kill => {}
            }
        }
        unreachable!("the pauses never run out")
    }

    /// The statement part of the operation is over: no kill is sent for it
    /// from now on, and the ones on their way are waited for (one round trip),
    /// so the clean-up statements (`ROLLBACK`, `COMMIT`) cannot be killed.
    pub(super) fn end(&self) {
        self.clear();
        self.connection.wait_for_kills();
    }
}

impl Drop for ActiveMark<'_> {
    fn drop(&mut self) {
        self.clear();
    }
}

fn running_token(operation_id: OperationId) -> u64 {
    // Leaves room for the flag of `running_word`.
    operation_id.0.min(u64::MAX >> 2) + 1
}

/// Bit 0 of the `running` word: a cancel was requested for the operation named by the rest.
const REQUESTED: u64 = 1;

/// The `running` word of the operation `token`, no cancel requested.
fn running_word(token: u64) -> u64 {
    token << 1
}

/// Clears the `running` word if it is still `token`'s (another operation's is left alone).
fn end_running(running: &AtomicU64, token: u64) {
    let _ = running.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |word| {
        (word >> 1 == token).then_some(0)
    });
}

/// Records a cancel request for `token` if it is the running operation. The
/// check and the store are one compare-and-swap: a canceller that was
/// descheduled before it, while its operation ended and another began, finds a
/// word that is not its own and changes nothing.
fn request_cancel(running: &AtomicU64, token: u64) -> bool {
    running
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |word| {
            (word >> 1 == token).then_some(word | REQUESTED)
        })
        .is_ok()
}

impl MyConnection {
    pub(super) fn connect<F: Flavor>(uri: &str) -> Result<Self, BackendError> {
        let config = F::CONFIG;
        let normalized = normalize_uri(uri)
            .map_err(|message| BackendError::with_code("connect.bad_request", message))?;
        let (url, mut tls) = TlsParams::from_url(&normalized)?;
        let opts = Opts::from_url(&url).map_err(|_| {
            BackendError::with_code(
                "connect.bad_request",
                format!("{} URI could not be parsed", config.display_name),
            )
        })?;
        tls.reconcile(&opts)?;
        // `prefer_socket` would otherwise run an extra `SELECT @@socket` and may
        // switch a loopback connection to a local unix socket.
        let opts: Opts = OptsBuilder::from_opts(opts)
            .prefer_socket(false)
            // Affected rows = rows matched, so an update that sets a column to its
            // current value still counts (keyed mutations tell "no such row" apart).
            .client_found_rows(true)
            // The session talks utf8mb4: run on every connection this URI opens
            // (the session, the catalog and the `KILL QUERY` ones) and again
            // after any `reset`.
            .init(vec![SET_NAMES])
            .tcp_keepalive(Some(TCP_KEEPALIVE))
            .into();
        let current_db = opts.db_name().map(str::to_owned);
        let base = base_lex(config.dialect);
        let (conn, lex, base, worked) = block_on(async {
            let (mut conn, worked) = tokio::time::timeout(CONNECT_TIMEOUT, tls.open(&opts))
                .await
                .map_err(|_| {
                    BackendError::new(format!("{} connect timed out", config.display_name))
                })?
                .map_err(|error| {
                    tls_connect_error(&error, &tls).unwrap_or_else(|| {
                        BackendError::new(format!(
                            "{} connect failed: {}",
                            config.display_name,
                            describe_error(&error)
                        ))
                    })
                })?;
            // The version decides which versioned comments (`/*!50000 ... */`) the
            // server runs, so the lexer must know it (unknown: both readings are tried).
            let (mode, version) = read_session_settings(&mut conn).await;
            let base = base_with_version(base, version.as_deref());
            let (lex, _) = lex_after_read(base, base, mode.as_deref().or(Some("")));
            Ok::<_, BackendError>((conn, lex, base, worked))
        })?;
        // The catalog and `KILL QUERY` connections open exactly like the session
        // did (TLS settings, or the clear-text fallback `PREFERRED` ended up with).
        let kill_opts: Opts = OptsBuilder::from_opts(worked)
            .db_name(None::<String>)
            .into();
        let thread_id = conn.id();
        let db_hint = Arc::new(Mutex::new(current_db.clone()));
        let broken = Arc::new(AtomicBool::new(false));
        Ok(Self {
            state: Mutex::new(ConnState {
                conn: Some(conn),
                current_db,
                db_hint: Arc::clone(&db_hint),
                lex,
                base_lex: base,
                lex_stale: false,
                charset_stale: false,
                broken: Arc::clone(&broken),
            }),
            meta: Mutex::new(MetaState { conn: None }),
            closed: AtomicBool::new(false),
            broken,
            db_hint,
            kill_opts,
            thread_id,
            running: Arc::new(AtomicU64::new(0)),
            kills_in_flight: Arc::new(AtomicUsize::new(0)),
            wake: Arc::new(Notify::new()),
        })
    }

    /// The lexer options the session runs under, as read from its `sql_mode`
    /// (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`, `MSSQL`); `None` when the state
    /// lock is poisoned.
    pub(super) fn lex(&self) -> Option<SqlLexOptions> {
        self.state.lock().ok().map(|state| state.lex)
    }

    /// The server's version in MySQL's numeric form (`80411`, MariaDB `110809`), as read at
    /// connect; `None` when it could not be read.
    pub(super) fn server_version(&self) -> Option<u32> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.lex.server_version)
    }

    pub(super) fn canceller(&self) -> MyCanceller {
        MyCanceller {
            // The kill connection runs `KILL QUERY` and nothing else: no `SET NAMES`.
            opts: OptsBuilder::from_opts(self.kill_opts.clone())
                .init(Vec::<String>::new())
                .into(),
            thread_id: self.thread_id,
            running: Arc::clone(&self.running),
            kills_in_flight: Arc::clone(&self.kills_in_flight),
            wake: Arc::clone(&self.wake),
        }
    }

    /// See [`ActiveMark`].
    pub(super) fn mark_running(&self, operation_id: OperationId) -> ActiveMark<'_> {
        let token = running_token(operation_id);
        // The whole word: an earlier request under this id cannot start it cancelled.
        self.running.store(running_word(token), Ordering::SeqCst);
        ActiveMark {
            connection: self,
            token,
        }
    }

    /// Waits (bounded) for every `KILL QUERY` that is in flight.
    fn wait_for_kills(&self) {
        let deadline = Instant::now() + KILL_TIMEOUT;
        while self.kills_in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    /// The connection lock. A poisoned lock means a panic left the wire in an
    /// unknown state, so the connection is refused rather than reused.
    ///
    /// Before it returns, it waits for every `KILL QUERY` that is in flight
    /// (one round trip). `KILL QUERY` hits whatever statement the thread is
    /// running; a kill that was aimed at the previous holder's statement and is
    /// still travelling would otherwise land on the first statement of this
    /// holder. A kill that reaches the server while the connection is idle is
    /// discarded by the server when the next statement starts (proven by the
    /// live tests on MySQL 8.4 and MariaDB 11), so once none is in flight the
    /// connection is safe to use. The wait is a single atomic load unless a
    /// cancel is in progress. The Dekker pairing (the gate increments, then
    /// reads the running mark; the lock holder has been released after the mark
    /// was cleared, then reads the counter) is `SeqCst` on both sides.
    pub(super) fn lock(&self) -> Result<MutexGuard<'_, ConnState>, BackendError> {
        let guard = self
            .state
            .lock()
            .map_err(|_| BackendError::new("connection lock poisoned"))?;
        self.wait_for_kills();
        Ok(guard)
    }

    /// The default database as far as the driver knows, readable without
    /// waiting for a running statement.
    pub(super) fn current_database(&self) -> Option<String> {
        self.db_hint.lock().ok().and_then(|hint| hint.clone())
    }

    /// Runs a catalog call (`navigator`, `describe`) on the separate metadata
    /// connection: it never waits behind a running user statement, and it is
    /// bounded by [`META_TIMEOUT`] (connect included). A failed or timed-out call
    /// drops the metadata connection; the next call opens a fresh one.
    pub(super) fn with_meta<T>(
        &self,
        not_connected_code: &'static str,
        work: impl AsyncFnOnce(&mut Conn) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let closed = || BackendError::with_code(not_connected_code, "connection closed");
        // A connection that was closed as untrustworthy is not served metadata
        // either: the session is gone as far as the host is concerned.
        if self.closed.load(Ordering::SeqCst) || self.broken.load(Ordering::SeqCst) {
            if let Ok(mut meta) = self.meta.try_lock() {
                let _runtime = runtime().enter();
                meta.conn = None;
            }
            return Err(closed());
        }
        let mut meta = self
            .meta
            .lock()
            .map_err(|_| BackendError::new("metadata connection lock poisoned"))?;
        let outcome = block_on(async {
            tokio::time::timeout(META_TIMEOUT, async {
                if meta.conn.is_none() {
                    let conn = Conn::new(self.kill_opts.clone()).await.map_err(|error| {
                        BackendError::new(format!(
                            "metadata connect failed: {}",
                            describe_error(&error)
                        ))
                    })?;
                    meta.conn = Some(conn);
                }
                let conn = meta.conn.as_mut().ok_or_else(closed)?;
                work(conn).await
            })
            .await
        });
        let result = outcome.unwrap_or_else(|_| Err(BackendError::new("metadata query timed out")));
        if result.is_err()
            || self.closed.load(Ordering::SeqCst)
            || self.broken.load(Ordering::SeqCst)
        {
            // The statement may have left the connection mid-protocol.
            let _runtime = runtime().enter();
            meta.conn = None;
        }
        result
    }

    /// Closes the client. Waits a bounded time for a statement that was just
    /// cancelled to release the lock; if it does not, the client is closed by
    /// `Drop` once the last reference goes away.
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // A catalog call in flight finishes within its timeout and then drops
        // its connection (it sees `closed`).
        if let Ok(mut meta) = self.meta.try_lock() {
            if let Some(conn) = meta.conn.take() {
                let _ = block_on(conn.disconnect());
            }
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
        if let Some(conn) = state.conn.take() {
            let _ = block_on(conn.disconnect());
        }
    }
}

impl Drop for MyConnection {
    fn drop(&mut self) {
        if let Ok(meta) = self.meta.get_mut() {
            if let Some(conn) = meta.conn.take() {
                let _runtime = runtime().enter();
                drop(conn);
            }
        }
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(conn) = state.conn.take() {
            // `Conn::drop` spawns the close handshake and needs a runtime context.
            let _runtime = runtime().enter();
            drop(conn);
        }
    }
}

impl MyCanceller {
    /// `KILL QUERY <thread id>` over a second, short-lived connection. Takes no
    /// connection lock and no kit lock; the network round trips happen here, not
    /// under any lock.
    ///
    /// The second connection is opened first, then `operation_id` is checked
    /// against the running mark, then the kill is sent: the next statement of
    /// the connection waits for that last round trip (see
    /// [`MyConnection::lock`]) instead of for a whole handshake.
    pub(super) fn kill_query(&self, operation_id: OperationId) -> Result<(), BackendError> {
        let token = running_token(operation_id);
        // Said first, before the kill's own connection is open: the operation
        // looks at it before its next round trip and stops there, and its
        // watcher starts to send the kill again if this one is discarded.
        if request_cancel(&self.running, token) {
            self.wake.notify_one();
        }
        block_on(async { tokio::time::timeout(KILL_TIMEOUT, self.kill(token)).await })
            .map_err(|_| BackendError::new("cancel timed out"))?
            .map_err(|error| {
                BackendError::new(format!("cancel failed: {}", describe_error(&error)))
            })
    }

    /// One `KILL QUERY` over a connection of its own, if `token` is still the
    /// running operation.
    async fn kill(&self, token: u64) -> mysql_async::Result<()> {
        let killer = self.connect().await?;
        self.send_kill(killer, token).await
    }

    /// The kill's connection (the handshake): nothing is sent yet, so dropping
    /// this future part way has no effect on the server's side.
    async fn connect(&self) -> mysql_async::Result<Conn> {
        Conn::new(self.opts.clone()).await
    }

    /// Sends the kill over `killer`, if `token` is still the running operation.
    async fn send_kill(&self, mut killer: Conn, token: u64) -> mysql_async::Result<()> {
        use mysql_async::prelude::Queryable;
        // From here until the kill is processed, `MyConnection::lock`
        // makes the next statement wait.
        let _gate = KillGate::enter(&self.kills_in_flight);
        if self.running.load(Ordering::SeqCst) >> 1 == token {
            killer
                .query_drop(format!("KILL QUERY {}", self.thread_id))
                .await?;
        }
        killer.disconnect().await
    }
}

/// Accepts `mysql://` and `mariadb://` (case-insensitive scheme) and returns a
/// `mysql://` URL for `mysql_async`.
pub(super) fn normalize_uri(uri: &str) -> Result<String, String> {
    let trimmed = uri.trim();
    for scheme in ["mysql://", "mariadb://"] {
        if trimmed
            .get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
        {
            return Ok(format!("mysql://{}", &trimmed[scheme.len()..]));
        }
    }
    Err("URI must start with mysql:// or mariadb://".to_owned())
}

pub(super) fn other_error(message: &str) -> mysql_async::Error {
    mysql_async::Error::Other(message.into())
}

/// Error text without the connection URL (credentials never appear in it).
pub(super) fn describe_error(error: &mysql_async::Error) -> String {
    match error {
        mysql_async::Error::Server(server) => format!(
            "{} (error {}, SQLSTATE {})",
            server.message, server.code, server.state
        ),
        other => other.to_string(),
    }
}

/// Maps a driver error. MySQL error 1317 is "query interrupted": when the host
/// asked for the cancel it is `<area>.cancelled`, otherwise (a server-side kill,
/// a foreign `KILL QUERY`) a retryable `<area>.interrupted`. Whatever else
/// failed after a cancel was requested is still `<area>.cancelled`.
pub(super) fn map_error(
    error: &mysql_async::Error,
    cancel_requested: bool,
    area: Area,
) -> BackendError {
    let interrupted = matches!(
        error,
        mysql_async::Error::Server(server) if server.code == ER_QUERY_INTERRUPTED
    );
    if cancel_requested {
        // The interrupt is the cancel itself; any other failure is worth showing.
        return cancelled(area, (!interrupted).then(|| describe_error(error)));
    }
    if interrupted {
        return BackendError::with_code(area.interrupted(), describe_error(error)).retryable();
    }
    BackendError::new(describe_error(error))
}

/// The error of a requested cancel.
pub(super) fn cancelled(area: Area, detail: Option<String>) -> BackendError {
    let base = match area {
        Area::Query => "query cancelled",
        Area::Mutation => "mutation cancelled",
    };
    let message = match detail {
        None => base.to_owned(),
        Some(detail) => format!("{base} ({detail})"),
    };
    BackendError::with_code(area.cancelled(), message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `@@version` that could not be read (or parsed) leaves the version unknown, and
    /// then a versioned comment is ambiguous: a read-only run refuses the text.
    #[test]
    fn an_unreadable_server_version_refuses_versioned_comments_under_read_only() {
        use engine_driver_kit::sql::{classify, ReadOnlyRefusal};
        let probe = "SELECT 1 /*!90000 ' */ /*!50000 ; COMMIT; DELETE FROM t WHERE 1 */";
        for dialect in [Dialect::MySql, Dialect::MariaDb] {
            for version in [None, Some(""), Some("garbage"), Some("8.x")] {
                let lex = base_with_version(base_lex(dialect), version);
                assert_eq!(lex.server_version, None, "{version:?}");
                // Also after a `sql_mode` refresh, which keeps the version it was given.
                let lex = lex_for_mode(lex, "ANSI_QUOTES");
                let c = classify(probe, &lex);
                assert!(c.version_ambiguous, "{version:?}");
                assert_eq!(
                    c.read_only_refusal(),
                    Some(ReadOnlyRefusal::AmbiguousVersionedComment)
                );
                assert!(c.danger.is_some() && !c.pageable);
            }
            let lex = base_with_version(base_lex(dialect), Some("8.4.11"));
            let lex = lex_for_mode(lex, "ANSI_QUOTES");
            assert_eq!(lex.server_version, Some(80411));
            let c = classify(probe, &lex);
            assert!(!c.version_ambiguous);
            assert_eq!(
                c.read_only_refusal(),
                Some(ReadOnlyRefusal::TransactionControl { statement: 1 })
            );
        }
    }

    #[test]
    fn the_server_version_is_the_number_versioned_comments_use() {
        for (version, number) in [
            ("8.4.11", Some(80411)),
            ("8.0.36-0ubuntu0.22.04.1", Some(80036)),
            ("5.7.44-log", Some(50744)),
            ("11.8.9-MariaDB-ubu2404", Some(110809)),
            ("10.6.12-MariaDB", Some(100612)),
            ("5.5.5-10.6.12-MariaDB", Some(100612)),
            ("9.99.99", Some(99999)),
            ("", None),
            ("garbage", None),
            ("8.4", None),
            ("8.x.1", None),
        ] {
            assert_eq!(server_version_number(version), number, "{version:?}");
        }
    }

    #[test]
    fn normalizes_both_schemes_case_insensitively() {
        assert_eq!(
            normalize_uri(" MariaDB://u:p@h:3306/db ").unwrap(),
            "mysql://u:p@h:3306/db"
        );
        assert_eq!(normalize_uri("mysql://h/db").unwrap(), "mysql://h/db");
        assert!(normalize_uri("postgres://x").is_err());
        assert!(normalize_uri("my").is_err());
    }

    #[test]
    fn interrupt_is_retryable_only_when_nobody_asked_for_it() {
        let interrupted = mysql_async::Error::Server(mysql_async::ServerError {
            code: ER_QUERY_INTERRUPTED,
            message: "Query execution was interrupted".into(),
            state: "70100".into(),
        });
        let spontaneous = map_error(&interrupted, false, Area::Query);
        assert_eq!(spontaneous.code.as_deref(), Some("query.interrupted"));
        assert!(spontaneous.retryable);
        let mutation = map_error(&interrupted, false, Area::Mutation);
        assert_eq!(mutation.code.as_deref(), Some("mutation.interrupted"));
        let requested = map_error(&interrupted, true, Area::Query);
        assert_eq!(requested.code.as_deref(), Some("query.cancelled"));
        assert!(!requested.retryable);
        let requested = map_error(&interrupted, true, Area::Mutation);
        assert_eq!(requested.code.as_deref(), Some("mutation.cancelled"));
    }

    struct Probe;
    impl Flavor for Probe {
        const CONFIG: super::super::FlavorConfig = super::super::FlavorConfig {
            driver: "probe",
            display_name: "Probe",
            uri_schemes: &["mysql", "mariadb"],
            dialect: Dialect::MySql,
        };
    }

    /// Needs a server (`TABLEX_TEST_MYSQL_URI` / `TABLEX_TEST_MARIADB_URI`, each one that is set
    /// is tried). A kill that reaches an idle thread is discarded by the server when the next
    /// statement starts: the cancel arrives *before* the statement, the watcher kills it anyway.
    #[test]
    #[ignore = "needs TABLEX_TEST_MYSQL_URI / TABLEX_TEST_MARIADB_URI (dedicated servers)"]
    fn a_kill_the_server_discarded_is_sent_again_while_the_statement_runs() {
        use mysql_async::prelude::Queryable;
        let uris: Vec<String> = ["TABLEX_TEST_MYSQL_URI", "TABLEX_TEST_MARIADB_URI"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .collect();
        assert!(
            !uris.is_empty(),
            "set TABLEX_TEST_MYSQL_URI and/or TABLEX_TEST_MARIADB_URI"
        );
        for uri in uris {
            let connection = MyConnection::connect::<Probe>(&uri).expect("connect");
            let mut state = connection.lock().expect("lock");
            let mark = connection.mark_running(OperationId(41));
            let canceller = connection.canceller();
            assert!(!mark.cancel_requested());
            // The thread is idle: the server discards this kill ...
            canceller.kill_query(OperationId(41)).expect("kill");
            assert!(mark.cancel_requested());
            std::thread::sleep(Duration::from_millis(200));
            // ... so the statement that starts afterwards would run for 30 s.
            let started = Instant::now();
            let slept: mysql_async::Result<Option<i64>> = block_on(mark.watch(async {
                state
                    .conn
                    .as_mut()
                    .unwrap()
                    .query_first("SELECT SLEEP(30)")
                    .await
            }));
            eprintln!(
                "kill test: the statement ended after {:?}",
                started.elapsed()
            );
            assert!(
                // SLEEP returns 1 when it is killed, other statements fail with 1317.
                matches!(&slept, Ok(Some(1))) || slept.is_err(),
                "{slept:?}"
            );
            // The bound only has to tell "the kill was sent again" from "the statement ran its
            // 30 s". It cannot be tighter: a MariaDB server whose `KILL QUERY` is busy with
            // several kills at once (two runs of this test, a full crate run next to another
            // suite) answers one in 2 to 3 s while the connect and the driver's pause take
            // milliseconds (measured with the `mariadb` client alone, no driver involved).
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );
            // Nothing is sent once the statement returned; the session is clean.
            mark.end();
            drop(mark);
            let after: Option<i64> =
                block_on(state.conn.as_mut().unwrap().query_first("SELECT 7")).unwrap();
            assert_eq!(after, Some(7));
        }
    }

    /// The interleaving of the review: a canceller of operation 11 is preempted
    /// before its request; 11 ends; operation 10 (begun earlier, locked later)
    /// runs and is cancelled. With the request being one compare-and-swap on the
    /// running word, the late request of 11 changes nothing and 10's is seen.
    #[test]
    fn a_late_request_of_an_earlier_operation_neither_hides_nor_makes_a_request() {
        let running = AtomicU64::new(0);
        let (op10, op11) = (
            running_token(OperationId(10)),
            running_token(OperationId(11)),
        );
        let requested = |token| running.load(Ordering::SeqCst) == running_word(token) | REQUESTED;
        let clear = |token| end_running(&running, token);

        running.store(running_word(op11), Ordering::SeqCst);
        assert!(request_cancel(&running, op11));
        assert!(requested(op11));
        clear(op11);

        // 10 runs: 11's canceller (the one that was preempted) finally acts.
        running.store(running_word(op10), Ordering::SeqCst);
        assert!(!request_cancel(&running, op11));
        assert!(!requested(op10), "a request of 11 marked 10");
        // 10's own cancel is seen, and a repeat of 11's late request cannot erase it.
        assert!(request_cancel(&running, op10));
        assert!(!request_cancel(&running, op11));
        assert!(requested(op10), "10's request was masked");
        // Nothing is left for the next operation, and 11's end does not clear 10's word.
        clear(op11);
        assert!(requested(op10));
        clear(op10);
        assert_eq!(running.load(Ordering::SeqCst), 0);
        assert!(!request_cancel(&running, op10));
    }

    /// A kill whose handshake hangs holds nothing on the server and must not
    /// hold back a statement that has already answered.
    #[test]
    fn a_statement_that_answered_does_not_wait_for_a_kill_whose_handshake_hangs() {
        // Accepts and never greets: `Conn::new` waits for the server's packet.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let opts = Opts::from_url(&format!(
            "mysql://u@127.0.0.1:{}/",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let connection = MyConnection {
            state: Mutex::new(ConnState {
                conn: None,
                current_db: None,
                db_hint: Arc::new(Mutex::new(None)),
                lex: base_lex(Dialect::MySql),
                base_lex: base_lex(Dialect::MySql),
                lex_stale: false,
                charset_stale: false,
                broken: Arc::new(AtomicBool::new(false)),
            }),
            meta: Mutex::new(MetaState { conn: None }),
            closed: AtomicBool::new(false),
            broken: Arc::new(AtomicBool::new(false)),
            db_hint: Arc::new(Mutex::new(None)),
            kill_opts: opts,
            thread_id: 1,
            running: Arc::new(AtomicU64::new(0)),
            kills_in_flight: Arc::new(AtomicUsize::new(0)),
            wake: Arc::new(Notify::new()),
        };
        let mark = connection.mark_running(OperationId(7));
        assert!(request_cancel(
            &connection.running,
            running_token(OperationId(7))
        ));
        connection.wake.notify_one();
        let started = Instant::now();
        // The statement answers after the first redelivery was started (50 ms).
        let out = block_on(mark.watch(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            42
        }));
        assert_eq!(out, 42);
        // KILL_TIMEOUT (5 s) was not waited for.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(connection.kills_in_flight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failed_sql_mode_read_keeps_the_options_in_force_and_stays_stale() {
        let base = SqlLexOptions::mysql();
        let ansi = lex_for_mode(base, "ANSI_QUOTES,NO_BACKSLASH_ESCAPES");
        assert_ne!(ansi, base);
        // The read was killed: neither forget ANSI_QUOTES nor stop re-reading.
        assert_eq!(lex_after_read(ansi, base, None), (ansi, true));
        // A successful read replaces the options, also by plain ones.
        assert_eq!(lex_after_read(ansi, base, Some("")), (base, false));
        assert_eq!(
            lex_after_read(base, base, Some("ANSI_QUOTES")),
            (lex_for_mode(base, "ANSI_QUOTES"), false)
        );
    }

    #[test]
    fn sql_mode_selects_the_lexer_options() {
        let base = SqlLexOptions::mysql();
        assert_eq!(lex_for_mode(base, "STRICT_TRANS_TABLES"), base);
        assert!(!lex_for_mode(base, "ONLY_FULL_GROUP_BY,no_backslash_escapes").backslash_escapes);
        assert!(lex_for_mode(base, "ANSI_QUOTES").double_quote_is_identifier);
        assert!(lex_for_mode(base, "ANSI").double_quote_is_identifier);
        // MariaDB reports MSSQL expanded: `[a;b]` is one identifier there.
        let mssql = lex_for_mode(
            base_lex(Dialect::MariaDb),
            "PIPES_AS_CONCAT,ANSI_QUOTES,IGNORE_SPACE,MSSQL,NO_KEY_OPTIONS",
        );
        assert!(mssql.bracket_identifiers && mssql.double_quote_is_identifier);
        let sql = "SELECT 1 AS [a;b]]c]; COMMIT";
        assert_eq!(
            engine_driver_kit::sql::classify(sql, &mssql)
                .statements
                .len(),
            2
        );
        assert_eq!(
            engine_driver_kit::sql::classify(sql, &base)
                .statements
                .len(),
            3
        );
        assert!(!lex_for_mode(base, "ANSI_QUOTES").bracket_identifiers);
        assert!(contains_ignore_case("set SQL_MODE = ''", "sql_mode"));
        for changes in [
            "SET NAMES latin1",
            "set  names 'latin1' collate latin1_bin",
            "SET CHARACTER SET utf8",
            "SET character_set_results = NULL",
            "set @@session.character_set_client = 'latin1'",
            "SET CHARSET latin1",
        ] {
            assert!(may_change_charset(changes), "{changes}");
        }
        assert!(!may_change_charset("SELECT 1"));
        assert!(!may_change_charset("SELECT name FROM t"));
        assert!(!contains_ignore_case("select 1", "sql_mode"));
    }

    #[test]
    fn an_unknown_transaction_state_is_not_a_negative_answer() {
        // No OK packet (the status refresh failed): the caller gets "unknown",
        // never "not in a transaction".
        assert_eq!(in_transaction(None), None);
        assert!(is_connection_lost(&mysql_async::Error::Io(
            mysql_async::IoError::Io(std::io::Error::other("gone"))
        )));
        assert!(!is_connection_lost(&mysql_async::Error::Server(
            mysql_async::ServerError {
                code: 1064,
                message: "syntax".into(),
                state: "42000".into(),
            }
        )));
    }

    #[test]
    fn other_server_errors_are_plain_failures() {
        let duplicate = mysql_async::Error::Server(mysql_async::ServerError {
            code: 1062,
            message: "Duplicate entry".into(),
            state: "23000".into(),
        });
        let mapped = map_error(&duplicate, false, Area::Mutation);
        assert_eq!(mapped.code, None);
        assert!(!mapped.retryable);
        assert!(mapped.message.contains("1062"));
        // Whatever fails after a requested cancel is the cancel.
        assert_eq!(
            map_error(&duplicate, true, Area::Query).code.as_deref(),
            Some("query.cancelled")
        );
    }
}
