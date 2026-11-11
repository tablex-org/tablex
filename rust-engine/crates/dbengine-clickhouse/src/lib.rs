//! ClickHouse on the shared driver kit (ABI v2), over the HTTP(S) interface.
//!
//! * Transport: a small synchronous HTTP client (`ureq` + rustls, see `http`).
//!   Results are read as `FORMAT ArrowStream` straight into Arrow batches and
//!   handed to the kit, which exports them through the Arrow C Data Interface;
//!   there is no per-row conversion. See `query` for paging and `types` for the
//!   ClickHouse-to-Arrow mapping.
//! * Connection: `clickhouse://user:pass@host:8123/db?secure=true&ca_cert=...&<setting>=...`
//!   (`config`). HTTP is stateless, so a connection is a configuration, two
//!   connection pools (user statements; catalog calls and `KILL`) and one HTTP
//!   *session* (`session_id`): `SET`, `USE` and temporary tables made by one
//!   statement of a script are visible to the next.
//! * Cancel: every statement carries a `query_id`; cancelling runs
//!   `KILL QUERY WHERE query_id = ...` on the other pool and takes no lock. A
//!   requested cancel is `query.cancelled`, an interruption nobody asked for
//!   (a server-side `KILL`, a connection that broke mid-reply) is retryable
//!   `query.interrupted`. Cancelling a mutation stops *waiting* for it
//!   (`mutations_sync = 2`); a mutation the server already accepted still runs.
//! * Values: what is shown and what is written agree (`types`, `value`,
//!   `datetime`): date-times are UTC instants whatever the column's zone, bytes
//!   that are not UTF-8 are never replaced (Arrow Binary, label
//!   `<type> (invalid utf-8)`), numbers of any width are exact text, and types
//!   the Arrow format cannot carry are the server's text (`<type> (read-only)`).
//! * Navigator and `describe_object`: `navigator`, `describe`.
//! * Mutations: `mutation`. ClickHouse has no transactions, so a `batch` is
//!   validated up front but **not atomic** (see there).
//! * Not supported: `readonly = 1` users (the HTTP interface refuses any
//!   per-request setting for them, and the driver needs the Arrow output
//!   settings); use `readonly = 2`.

mod config;
mod datetime;
mod describe;
mod http;
mod mutation;
mod navigator;
mod query;
mod sql;
mod types;
mod value;

use config::Config;
use engine_driver_kit::api::{
    Capability, DriverCapabilities, MutationRequest, NavigatorPage, NavigatorRequest, OperationId,
    QueryRequest, SqlDialectInfo,
};
use engine_driver_kit::{
    BackendError, CellFormatter, ConnectRequest, DriverBackend, MutationOutcome, OperationGuard,
    Page, QueryOutput,
};
use http::{Area, Client};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};
use types::Described;

const DRIVER: &str = "clickhouse";
/// How long a statement may keep running after the host asked to cancel it. A server that
/// does not react (a dead peer, a sleeping laptop) would otherwise hold the calling thread
/// and the connection for ever: past this the statement is dropped locally
/// (`<area>.not_connected`, the app reconnects).
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How often the statement of a cancel is looked at while it is within its grace period.
const GRACE_POLL: Duration = Duration::from_millis(100);
/// How long a cancel keeps trying `KILL QUERY` (a statement the server has not
/// registered yet, or a kill the server refuses), and how long it pauses between tries.
const KILL_WINDOW: Duration = Duration::from_secs(1);
const KILL_RETRY: Duration = Duration::from_millis(25);
/// How long a call waits for the connection while another call still holds it.
const GATE_WAIT: Duration = Duration::from_secs(30);
/// `DESCRIBE` results kept per connection.
const TYPE_CACHE_STATEMENTS: usize = 16;

/// Everything one user operation may touch; guarded by [`ChConnection::session`]
/// for the whole operation, so requests of one connection never interleave.
pub(crate) struct UserSession {
    /// The HTTP session id.
    pub(crate) id: String,
    /// User SQL may have changed the session's database (`USE`): stop passing
    /// the URI's default database with every request.
    pub(crate) database_unknown: Cell<bool>,
    pub(crate) types: RefCell<TypeCache>,
}

/// ClickHouse column types of recent statements (`DESCRIBE` results), so
/// paging the same statement does not describe it again.
#[derive(Default)]
pub(crate) struct TypeCache(Vec<(String, Arc<Described>)>);

impl TypeCache {
    pub(crate) fn get(&self, key: &str) -> Option<Arc<Described>> {
        self.0
            .iter()
            .find(|(cached, _)| cached == key)
            .map(|(_, types)| Arc::clone(types))
    }

    /// Forgets every answer: something that may change column types ran.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    pub(crate) fn put(&mut self, key: String, types: Arc<Described>) {
        self.0.retain(|(cached, _)| *cached != key);
        if self.0.len() == TYPE_CACHE_STATEMENTS {
            self.0.remove(0);
        }
        self.0.push((key, types));
    }
}

/// The `query_id` of the statement in flight, shared with the canceller, and the flag that
/// ends its blocked wait for the reply (see `http::Guarded`).
#[derive(Default)]
pub(crate) struct Inflight {
    id: Mutex<Option<String>>,
    abandon: Arc<AtomicBool>,
}

impl Inflight {
    fn with_abandon_flag(abandon: Arc<AtomicBool>) -> Self {
        Self {
            id: Mutex::default(),
            abandon,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Another statement (or none) is the one in flight now: the flag of the last
    /// one is not the next one's.
    fn set(&self, id: Option<String>) {
        let mut current = self.lock();
        *current = id;
        self.abandon.store(false, Ordering::Release);
    }

    fn current(&self) -> Option<String> {
        self.lock().clone()
    }

    /// Ends the blocked wait of `query_id`, if it is still the statement in flight (under
    /// the lock `set` takes, so the flag can never reach the next statement).
    fn abandon(&self, query_id: &str) {
        if self.lock().as_deref() == Some(query_id) {
            self.abandon.store(true, Ordering::Release);
        }
    }

    /// Whether the statement in flight was abandoned.
    pub(crate) fn abandoned(&self) -> bool {
        self.abandon.load(Ordering::Acquire)
    }
}

/// Marks a statement as the one in flight until dropped.
#[must_use = "the mark ends when the value is dropped"]
pub(crate) struct InflightMark<'a>(&'a Inflight);

impl Drop for InflightMark<'_> {
    fn drop(&mut self) {
        self.0.set(None);
    }
}

/// A random token, unique per connection (and per process run).
fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    format!(
        "{:016x}",
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    )
}

/// One kit connection.
pub struct ChConnection {
    pub(crate) client: Arc<Client>,
    session: Mutex<UserSession>,
    inflight: Arc<Inflight>,
    /// `tablex-<nonce>`: the prefix of every query id of this connection.
    id_prefix: String,
    closed: AtomicBool,
}

impl ChConnection {
    fn new(client: Client) -> Self {
        let nonce = nonce();
        let inflight = Inflight::with_abandon_flag(client.abandon_flag());
        Self {
            client: Arc::new(client),
            session: Mutex::new(UserSession {
                id: format!("tablex-{nonce}"),
                database_unknown: Cell::new(false),
                types: RefCell::new(TypeCache::default()),
            }),
            inflight: Arc::new(inflight),
            id_prefix: format!("tablex-{nonce}"),
            closed: AtomicBool::new(false),
        }
    }

    /// Catalog calls fail with `<area>.not_connected` after a disconnect.
    pub(crate) fn check_open(&self, area: Area) -> Result<(), BackendError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(BackendError::with_code(
                area.not_connected(),
                "connection closed",
            ));
        }
        Ok(())
    }

    /// The user session, exclusively, for one operation. Waits (blocking the calling
    /// thread) for at most [`GATE_WAIT`]: a call that still holds it is stuck on a
    /// server that does not answer, and the next call must fail (`<area>.busy`)
    /// instead of waiting with it.
    pub(crate) fn lock(&self, area: Area) -> Result<MutexGuard<'_, UserSession>, BackendError> {
        self.lock_within(area, GATE_WAIT)
    }

    fn lock_within(
        &self,
        area: Area,
        wait: Duration,
    ) -> Result<MutexGuard<'_, UserSession>, BackendError> {
        let deadline = Instant::now() + wait;
        let mut pause = Duration::from_millis(1);
        loop {
            match self.session.try_lock() {
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

    /// The error of the statement in flight if its wait was ended by [`ChCanceller::kill`]'s
    /// grace period: the connection is lost as far as the app is concerned.
    pub(crate) fn abandoned(&self, area: Area) -> Option<BackendError> {
        self.inflight.abandoned().then(|| {
            BackendError::with_code(
                area.not_connected(),
                "the server did not react to the cancel; the request was dropped, reconnect",
            )
        })
    }

    /// `tablex-<nonce>-<operation>-<statement>`: unique per statement.
    pub(crate) fn query_id(&self, operation: OperationId, statement: usize) -> String {
        format!("{}-{}-{statement}", self.id_prefix, operation.0)
    }

    /// Makes `query_id` the statement a cancel would kill.
    pub(crate) fn mark(&self, query_id: &str) -> InflightMark<'_> {
        self.inflight.set(Some(query_id.to_owned()));
        InflightMark(&self.inflight)
    }

    /// The database a statement runs in: the request's, else the URI's unless
    /// user SQL may have moved the session since.
    pub(crate) fn database_for(
        &self,
        session: &UserSession,
        requested: Option<&str>,
    ) -> Option<String> {
        requested
            .filter(|database| !database.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                (!session.database_unknown.get())
                    .then(|| self.client.database.clone())
                    .flatten()
            })
    }
}

/// Cross-thread cancel handle: enough to send `KILL QUERY` on the catalog pool.
/// It holds no reference to the connection, so cancelling never takes its lock.
pub struct ChCanceller {
    client: Arc<Client>,
    inflight: Arc<Inflight>,
    /// `tablex-<nonce>`: see [`ChConnection::id_prefix`].
    id_prefix: String,
}

/// What a cancel found out.
#[derive(Debug, PartialEq, Eq)]
enum KillError {
    /// The server refused the kill on every try within the window.
    Refused(String),
    /// The server could not be reached or the reply broke: not worth retrying here.
    Transport(String),
}

/// Whether `query_id` (`<prefix>-<operation>-<statement>`) is a statement of `operation`.
fn belongs_to(query_id: &str, id_prefix: &str, operation: OperationId) -> bool {
    query_id
        .strip_prefix(id_prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_prefix(operation.0.to_string().as_str()))
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|statement| {
            !statement.is_empty() && statement.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Sends `kill` until it finds the statement, for at most `window`, as long as
/// `in_flight` says the statement is still the one running. `kill` returns how
/// many statements it killed: 0 means the server has not registered the
/// statement yet. A kill the server *refuses* (out of memory, busy) is tried again
/// like an empty one and reported only if no try got through; a transport
/// failure ends the cancel at once (with a dead network every try would take as
/// long as the HTTP timeout). `window` bounds the retries, not one try: a try is
/// bounded by the kill's own 5 s timeout (`http::KILL_TIMEOUT`).
fn kill_within(
    window: Duration,
    retry: Duration,
    in_flight: impl Fn() -> bool,
    mut kill: impl FnMut() -> Result<usize, http::HttpError>,
) -> Result<(), KillError> {
    let deadline = std::time::Instant::now() + window;
    loop {
        let refused = match kill() {
            Ok(0) => None,
            Ok(_) => return Ok(()),
            Err(http::HttpError::Server { message, .. }) => Some(message),
            Err(other) => return Err(KillError::Transport(other.to_string())),
        };
        if std::time::Instant::now() + retry >= deadline {
            return refused.map_or(Ok(()), |message| Err(KillError::Refused(message)));
        }
        std::thread::sleep(retry);
        if !in_flight() {
            return Ok(());
        }
    }
}

impl ChCanceller {
    /// Kills the statement of `operation` that is in flight (a cancel for another
    /// operation, or when nothing is in flight, does nothing). See [`kill_within`].
    fn kill(&self, operation: OperationId) -> Result<(), BackendError> {
        let Some(query_id) = self.inflight.current() else {
            return Ok(());
        };
        if !belongs_to(&query_id, &self.id_prefix, operation) {
            return Ok(());
        }
        self.abandon_after_grace(query_id.clone(), CANCEL_GRACE);
        kill_within(
            KILL_WINDOW,
            KILL_RETRY,
            || self.inflight.current().as_deref() == Some(query_id.as_str()),
            || self.client.kill_query(&query_id),
        )
        .map_err(|error| {
            let (KillError::Refused(reason) | KillError::Transport(reason)) = error;
            BackendError::with_code("operation.failed", format!("KILL QUERY failed: {reason}"))
        })
    }
}

impl ChCanceller {
    /// Ends the blocked wait of `query_id` if it is still in flight `grace` from now: the
    /// kill (which has its own short timeouts) did not stop it, so the server is not answering.
    /// A thread of its own, so the cancel itself returns as soon as its kill did.
    fn abandon_after_grace(&self, query_id: String, grace: Duration) {
        let inflight = Arc::clone(&self.inflight);
        let deadline = Instant::now() + grace;
        std::thread::spawn(move || {
            while Instant::now() < deadline {
                if inflight.current().as_deref() != Some(query_id.as_str()) {
                    return;
                }
                std::thread::sleep(GRACE_POLL);
            }
            inflight.abandon(&query_id);
        });
    }
}

/// The capabilities the driver advertises: exactly what it implements.
fn capabilities() -> DriverCapabilities {
    let mut capabilities = DriverCapabilities::new(
        DRIVER,
        "ClickHouse",
        [
            Capability::Navigator,
            Capability::QuerySql,
            Capability::QueryArrowCData,
            Capability::OperationCancel,
            Capability::MutationInsert,
            Capability::MutationUpdate,
            Capability::MutationDelete,
            Capability::MutationBatch,
            Capability::DatabaseCreate,
            Capability::MetadataTableProperties,
            Capability::QueryReadOnly,
        ]
        .map(|capability| capability.as_str().to_owned())
        .into(),
    );
    capabilities.uri_schemes = ["clickhouse", "http", "https"].map(String::from).into();
    capabilities.editors = vec!["sql".into()];
    capabilities.navigator_kinds = ["database", "table", "view", "materialized_view", "column"]
        .map(String::from)
        .into();
    capabilities.sql_dialect = Some(SqlDialectInfo {
        // ClickHouse also accepts `"`, but a backtick never clashes with a string.
        identifier_quote: "`".into(),
        string_literal_quote: "'".into(),
        supports_schemas: false,
        database_qualified_names: true,
        like_requires_text_cast: false,
        string_backslash_escapes: true,
        double_quote_identifiers: true,
        bracket_identifiers: false,
    });
    capabilities
}

/// ClickHouse backend.
struct ClickHouse;

impl DriverBackend for ClickHouse {
    type Connection = ChConnection;
    type PageContext = ();
    type Canceller = ChCanceller;

    fn capabilities() -> DriverCapabilities {
        capabilities()
    }

    fn connect(request: &ConnectRequest) -> Result<ChConnection, BackendError> {
        let config = Config::parse(&request.uri)?;
        Ok(ChConnection::new(Client::connect(&config)?))
    }

    fn disconnect(connection: &ChConnection) {
        connection.closed.store(true, Ordering::Release);
    }

    fn list_nav_nodes(
        connection: &ChConnection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        navigator::list_nav_nodes(connection, request)
    }

    fn open_query(
        connection: &ChConnection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput, BackendError> {
        query::open_query(connection, request, operation)
    }

    fn canceller(connection: &ChConnection) -> Result<ChCanceller, BackendError> {
        Ok(ChCanceller {
            client: Arc::clone(&connection.client),
            inflight: Arc::clone(&connection.inflight),
            id_prefix: connection.id_prefix.clone(),
        })
    }

    fn cancel(canceller: &ChCanceller, operation: OperationId) -> Result<(), BackendError> {
        canceller.kill(operation)
    }

    /// Unsigned BIGINT is the one Arrow type the kit's fast path does not print
    /// directly; format it without Arrow's generic display machinery.
    fn cell_formatter(page: &Page<()>, column: usize) -> CellFormatter<'_> {
        let array = page.batch.column(column);
        match array.as_any().downcast_ref::<arrow_array::UInt64Array>() {
            Some(values) => CellFormatter::custom(array, move |row, out| {
                use std::fmt::Write;
                let _ = write!(out, "{}", values.value(row));
            }),
            None => CellFormatter::new(array),
        }
    }

    fn mutate(
        connection: &ChConnection,
        request: &MutationRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        mutation::mutate(connection, request, operation)
    }

    fn describe_object(
        connection: &ChConnection,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        describe::describe_object(connection, request)
    }
}

engine_driver_kit::export_driver!(ClickHouse; cancel_operation, mutate, describe_object);

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_kit::ExportedSymbols;

    #[test]
    fn waiting_for_a_busy_connection_gives_up_instead_of_blocking_forever() {
        let connection = ChConnection::new(Client::with_kill_agent(http::kill_agent_for_tests()));
        let holder = connection.lock(Area::Query).expect("lock");
        let started = Instant::now();
        let error = connection
            .lock_within(Area::Mutation, Duration::from_millis(150))
            .err()
            .expect("busy");
        assert_eq!(error.code.as_deref(), Some("mutation.busy"));
        assert!(error.retryable);
        assert!(
            started.elapsed() >= Duration::from_millis(150)
                && started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        drop(holder);
        assert!(connection
            .lock_within(Area::Query, Duration::from_millis(150))
            .is_ok());
    }

    #[test]
    fn capabilities_match_the_exported_symbols_and_describe_the_dialect() {
        let capabilities = capabilities();
        ExportedSymbols::CANCEL_OPERATION
            .union(ExportedSymbols::MUTATE)
            .union(ExportedSymbols::DESCRIBE_OBJECT)
            .validate(&capabilities)
            .expect("exports exactly what is advertised");
        assert_eq!(capabilities.driver, "clickhouse");
        assert_eq!(capabilities.uri_schemes, ["clickhouse", "http", "https"]);
        assert_eq!(capabilities.editors, ["sql"]);
        for capability in [
            Capability::Navigator,
            Capability::QuerySql,
            Capability::QueryArrowCData,
            Capability::OperationCancel,
            Capability::MutationInsert,
            Capability::MutationUpdate,
            Capability::MutationDelete,
            Capability::MutationBatch,
            Capability::DatabaseCreate,
            Capability::MetadataTableProperties,
            Capability::QueryReadOnly,
        ] {
            assert!(capabilities.has(capability), "{capability}");
        }
        // Not advertised: there are no transactions, exports or drops here.
        assert!(!capabilities.has(Capability::MutationDropDatabase));
        assert!(!capabilities.has_any_export());
        assert!(capabilities.supports_mutation_kind("batch"));
        let dialect = capabilities.sql_dialect.as_ref().expect("dialect");
        assert_eq!(dialect.identifier_quote, "`");
        assert_eq!(dialect.string_literal_quote, "'");
        assert!(!dialect.supports_schemas && dialect.database_qualified_names);
        assert!(dialect.string_backslash_escapes && !dialect.like_requires_text_cast);
        for kind in ["database", "table", "view", "column"] {
            assert!(
                capabilities.navigator_kinds.iter().any(|k| k == kind),
                "{kind}"
            );
        }
    }

    #[test]
    fn the_type_cache_is_bounded_and_replaces_by_key() {
        let mut cache = TypeCache::default();
        let described = |ty: &str| {
            Arc::new(Described {
                names: vec!["c".into()],
                types: vec![types::ChType::parse(ty)],
            })
        };
        for index in 0..TYPE_CACHE_STATEMENTS + 4 {
            cache.put(format!("q{index}"), described("Int32"));
        }
        assert!(cache.get("q0").is_none() && cache.get("q19").is_some());
        assert_eq!(cache.0.len(), TYPE_CACHE_STATEMENTS);
        cache.put("q19".into(), described("String"));
        assert_eq!(cache.get("q19").unwrap().types[0].name(), "String");
        assert_eq!(cache.0.len(), TYPE_CACHE_STATEMENTS);
        cache.clear();
        assert!(cache.get("q19").is_none());
    }

    fn refusal() -> http::HttpError {
        http::HttpError::Server {
            code: Some(241),
            message: "memory limit exceeded".into(),
        }
    }

    const TINY: Duration = Duration::from_millis(2);

    #[test]
    fn a_refused_kill_is_tried_again_until_one_gets_through() {
        let mut tries = 0;
        let result = kill_within(
            Duration::from_millis(500),
            TINY,
            || true,
            || {
                tries += 1;
                if tries < 4 {
                    Err(refusal())
                } else {
                    Ok(1)
                }
            },
        );
        assert_eq!((result, tries), (Ok(()), 4));
    }

    #[test]
    fn a_kill_that_is_refused_for_the_whole_window_reports_the_refusal() {
        let started = std::time::Instant::now();
        let mut tries = 0;
        let result = kill_within(
            Duration::from_millis(60),
            TINY,
            || true,
            || {
                tries += 1;
                Err(refusal())
            },
        );
        assert_eq!(
            result,
            Err(KillError::Refused("memory limit exceeded".into()))
        );
        assert!(tries > 3, "{tries}");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_transport_failure_ends_the_cancel_at_once() {
        for error in [
            http::HttpError::Connect("timed out".into()),
            http::HttpError::Stream("reset".into()),
        ] {
            let mut tries = 0;
            let mut error = Some(error);
            let result = kill_within(
                Duration::from_secs(30),
                Duration::from_secs(30),
                || true,
                || {
                    tries += 1;
                    Err(error.take().unwrap())
                },
            );
            assert!(matches!(result, Err(KillError::Transport(_))), "{result:?}");
            assert_eq!(tries, 1);
        }
    }

    #[test]
    fn a_statement_that_is_not_registered_is_looked_for_only_while_it_is_in_flight() {
        let mut tries = 0;
        let in_flight = std::cell::Cell::new(true);
        let result = kill_within(
            Duration::from_secs(30),
            TINY,
            || in_flight.get(),
            || {
                tries += 1;
                if tries == 3 {
                    in_flight.set(false);
                }
                Ok(0)
            },
        );
        assert_eq!((result, tries), (Ok(()), 3));
        // Never found within the window: nothing to report.
        let result = kill_within(Duration::from_millis(20), TINY, || true, || Ok(0));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn a_cancel_only_names_the_statements_of_its_own_operation() {
        let prefix = "tablex-00ff";
        assert!(belongs_to("tablex-00ff-7-0", prefix, OperationId(7)));
        assert!(belongs_to("tablex-00ff-7-12", prefix, OperationId(7)));
        assert!(!belongs_to("tablex-00ff-17-0", prefix, OperationId(7)));
        assert!(!belongs_to("tablex-00ff-7-0", prefix, OperationId(17)));
        assert!(!belongs_to("tablex-00ff-77-0", prefix, OperationId(7)));
        assert!(!belongs_to("tablex-aaaa-7-0", prefix, OperationId(7)));
        assert!(!belongs_to("tablex-00ff-7-", prefix, OperationId(7)));
    }

    #[test]
    fn abandoning_reaches_only_the_statement_in_flight() {
        let flag = Arc::new(AtomicBool::new(false));
        let inflight = Inflight::with_abandon_flag(Arc::clone(&flag));
        inflight.set(Some("first".into()));
        // Another statement's id (a cancel that was late): nothing happens.
        inflight.abandon("other");
        assert!(!inflight.abandoned());
        inflight.abandon("first");
        assert!(inflight.abandoned() && flag.load(Ordering::Acquire));
        // The next statement does not start abandoned.
        inflight.set(Some("second".into()));
        assert!(!inflight.abandoned());
        // A statement that ended before the grace period is never abandoned.
        inflight.set(None);
        inflight.abandon("second");
        assert!(!inflight.abandoned());
    }

    #[test]
    fn a_cancel_that_the_server_ignores_abandons_the_statement_after_the_grace_period() {
        let connection = ChConnection::new(Client::with_kill_agent(http::kill_agent_for_tests()));
        let canceller = ChCanceller {
            client: Arc::clone(&connection.client),
            inflight: Arc::clone(&connection.inflight),
            id_prefix: connection.id_prefix.clone(),
        };
        let mark = connection.mark("q-1");
        canceller.abandon_after_grace("q-1".into(), Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(150));
        assert!(connection.abandoned(Area::Query).is_none(), "still within the grace period");
        std::thread::sleep(Duration::from_millis(500));
        let error = connection.abandoned(Area::Query).expect("abandoned after the grace period");
        assert_eq!(error.code.as_deref(), Some("query.not_connected"));
        drop(mark);
        // A statement that ends inside the grace period is left alone.
        let mark = connection.mark("q-2");
        canceller.abandon_after_grace("q-2".into(), Duration::from_millis(300));
        drop(mark);
        std::thread::sleep(Duration::from_millis(600));
        assert!(connection.abandoned(Area::Query).is_none());
    }

    #[test]
    fn the_inflight_mark_clears_itself() {
        let inflight = Inflight::default();
        assert_eq!(inflight.current(), None);
        {
            inflight.set(Some("q".into()));
            let _mark = InflightMark(&inflight);
            assert_eq!(inflight.current().as_deref(), Some("q"));
        }
        assert_eq!(inflight.current(), None);
    }

    #[test]
    fn query_ids_are_unique_per_connection_and_statement() {
        let (a, b) = (nonce(), nonce());
        assert_ne!(a, b);
        let prefix = format!("tablex-{a}");
        let id = format!("{prefix}-{}-{}", 7, 2);
        assert!(id.starts_with("tablex-") && id.ends_with("-7-2"));
        assert!(!id.contains(' '));
    }
}
