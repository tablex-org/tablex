//! Manual commit mode of a SQL tab: the dedicated driver session, Commit / Rollback, the toolbar
//! and the questions asked before an open transaction would be lost.
//!
//! **Sessions.** Tabs in auto-commit mode share the connection's one driver session, as before. A
//! tab switched to Manual (or one that types a `BEGIN`, which then stays Manual) gets a session of
//! its own, opened once, lazily, at its first statement: the same driver handle, URI (TLS
//! included) and credentials as the connection's, through a tunnel of its own when the connection
//! has one ([`AppRoot::secondary_session_spec`], the route the structure apply's session takes).
//! It is closed when the tab is switched back to Auto-commit, when the tab closes and when its
//! connection is closed. The server rolls back what a closed session left open, so the
//! forced paths (a deleted connection, a driver update) lose the transaction; the user-facing
//! ones ask first ([`AppRoot::guard_transactions`]).
//!
//! **Smart commit** (a connection of its own per SQL editor, and the switch only flips
//! autocommit on it). When smart commit is on for the connection type, an editor tab runs on a session of
//! its own from its first statement, even in auto-commit (autocommit on, nothing counted). A statement that
//! modifies starts a transaction on that same session (`SET autocommit = 0` on MySQL / MariaDB, the usual
//! `BEGIN` on PostgreSQL / SQLite), so what the tab set in the session (`SET`, `USE`, `PRAGMA`, temporary
//! tables) is still there and nothing is replayed. Commit / Roll Back (or a typed one, or the server's own
//! commit at MySQL DDL) ends it, and with "return to auto-commit" on the tab goes back to autocommit on the
//! same session. A tab that already ran on the shared session is never moved: smart commit applies to the
//! editors opened after it was turned on. The cost is one more server connection per editor tab that ran
//! a statement, and, as for any manual tab, the AI assistant and MCP clients do not see its uncommitted changes.
//!
//! A COMMIT / ROLLBACK a script types ends the smart transaction like the buttons do, at that statement
//! (the tab recovers at the commit event, then judges each statement afresh): the tab is in auto-commit for
//! the statements after it, and the next one that modifies starts the next transaction. On MySQL / MariaDB
//! the return costs no round trip of its own: the next request on the session starts with `SET autocommit = 1`
//! ([`TabTx::autocommit_restore`]). When the connection type stops asking for the session (smart commit off,
//! or read-only) an idle one is closed and the tab goes back to the shared session; one with an open
//! transaction is kept until it ends ([`AppRoot::release_own_session_if_unwanted`]). A tab the user made
//! manual keeps its session when only smart commit goes off.
//!
//! A Commit / Roll Back asked for while the same one is already on its way (a second click, the idle timer, a
//! list) is absorbed; the other one is refused with the name of the one running.
//!
//! **Routing.** Everything a tab does with a result goes through [`AppRoot::tab_session_for`]: its
//! statements, the pages they leave and Cancel. (A grid Apply is not among them: only SQL editor
//! tabs have a commit mode, and a table-data tab's Apply is a transaction of the driver's own on
//! the connection's shared session.) A structure change (DDL Apply) always has a session of its
//! own too, so it never runs inside a tab's transaction: it can wait for a lock the transaction
//! holds (its lock wait is bounded) but neither commits nor rolls back what is open.
//!
//! **State.** The server owns the transaction: a MySQL / MariaDB tab session runs with
//! `autocommit = 0`, a PostgreSQL / SQLite one gets a `BEGIN` (result ignored) before each
//! statement. `crate::transaction` keeps the mode of each tab and a counter of the statements sent
//! since the last Commit / Rollback; this file keeps the sessions and the I/O. Nothing here runs
//! per frame: the toolbar reads a map entry, and a one-second timer runs only while a counter is
//! above zero (the elapsed time).

use super::structure_edit::apply::sqlite_is_in_memory;
use super::*;
use super::smart_commit::{commits_by_itself, smart_verdict, Smart};
use crate::transaction::{
    begin_statement, elapsed_label, manual_unavailable, mentions_autocommit, same_database, session_setup, statements_label,
    CommitMode, LogResult, Outcome, TabTx, Typed,
};
use engine_connect::connect::LiveTunnel;
use gpui_component::button::ButtonGroup;
use gpui_component::dialog::{DialogClose, DialogFooter};
use std::cell::RefCell;
use std::sync::atomic::AtomicU32;

/// A SQL tab's session of its own.
pub(super) struct TabSession {
    pub(super) session: Arc<engine_driver_host::SharedDriverSession>,
    pub(super) handle: Arc<engine_driver_host::DriverHandle>,
    /// The driver's id of this session (not the connection's).
    pub(super) connection_id: u64,
    /// The saved connection (its key) the session belongs to. A session is only ever used for a
    /// tab that is still on this connection.
    pub(super) connection: String,
    /// The database the open transaction lives on (PostgreSQL keeps a client per database): set
    /// when a transaction opens, and what Commit / Rollback and the other statements must match.
    pub(super) database: String,
    /// Workers that still run something on this session (a statement the user cancelled keeps
    /// running until the driver stops it): Commit / Rollback wait for none.
    inflight: Arc<AtomicU32>,
    /// The session's own tunnel. Dropped after the session has said goodbye through it.
    tunnel: Option<LiveTunnel>,
}

/// Held by the worker thread of a request on a tab's session, for as long as it runs.
pub(super) struct InFlight(Arc<AtomicU32>);

impl InFlight {
    fn enter(counter: &Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(counter))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl TabSession {
    fn is_running(&self) -> bool {
        self.inflight.load(Ordering::Acquire) > 0
    }

    /// Disconnects (the server rolls back whatever is open) and closes the tunnel, off the UI thread.
    /// Whatever drops a `TabSession` does this: a driver session that is merely dropped is never
    /// disconnected, and would stay open on the server.
    fn close(self) {
        drop(self);
    }
}

impl Drop for TabSession {
    fn drop(&mut self) {
        let (session, tunnel) = (Arc::clone(&self.session), self.tunnel.take());
        smol::spawn(async move {
            smol::unblock(move || disconnect_and_close(&session, tunnel)).await;
        })
        .detach();
    }
}

/// Blocking: says goodbye to the server on `session`, then closes its tunnel.
fn disconnect_and_close(session: &engine_driver_host::SharedDriverSession, tunnel: Option<LiveTunnel>) {
    if let Ok(mut session) = session.lock() {
        let _ = session.disconnect();
    }
    drop(tunnel);
}

/// What the toolbar and the run path ask about a tab, worked out from its connection and settings
/// once and kept until one of them changes.
#[derive(Clone, Copy)]
struct Facts {
    kind: Option<ConnectionKind>,
    unavailable: Option<&'static str>,
    default_mode: CommitMode,
    /// Smart commit: an auto-commit tab switches to manual at a statement that modifies.
    smart_commit: bool,
    /// ... and a tab it switched returns to auto-commit after Commit / Roll Back.
    smart_recover: bool,
}

impl Facts {
    const UNCONNECTED: Self = Self {
        kind: None,
        unavailable: Some(NO_CONNECTION),
        default_mode: CommitMode::Auto,
        smart_commit: false,
        smart_recover: false,
    };
}

/// [`Facts`] by tab, valid for one settings snapshot and one revision of the connection types'
/// profiles. The snapshot is held (not just compared by address), so a new one can never be
/// mistaken for the old one by landing at the same address.
#[derive(Default)]
struct FactsCache {
    settings: Option<Arc<crate::settings::Settings>>,
    policy_revision: u64,
    /// By tab: a hash of what its connection is (id, kind, URI) and the facts worked out for it.
    tabs: HashMap<u64, (u64, Facts)>,
}

#[derive(Default)]
pub(super) struct TransactionUi {
    tabs: HashMap<u64, TabTx>,
    /// Tabs with something on its way to the server that ends a transaction or leaves one, and what it is.
    ending: HashMap<u64, Ending>,
    /// Whether the last Commit / Roll Back of each tab ended its transaction (what `finish_end` answered),
    /// kept for a request that waited for it: it goes on only when that one succeeded. Cleared when the
    /// next one starts.
    last_end: HashMap<u64, bool>,
    facts: RefCell<FactsCache>,
    /// The session opening of each tab, by the serial of the opening: a tab, a connection or a
    /// driver that went away meanwhile removes it, and the session that then arrives is closed.
    opening: HashMap<u64, u64>,
    open_serial: u64,
    /// What an auto-commit tab's statement being sent is, when it is a transaction statement.
    auto_sent: HashMap<u64, Typed>,
    /// Auto-commit tabs that left an explicit transaction, or autocommit off, on the shared
    /// session: ended when the run does.
    auto_open: HashMap<u64, AutoOpen>,
    /// Connections whose shared session is being cleaned up after an auto-commit run: the gate
    /// closes when the cleanup ends, and the runs of that connection wait for it.
    cleaning: HashMap<String, smol::channel::Receiver<()>>,
    /// The toolbar's Commit / Roll Back tooltips with the shortcut labels, for one settings snapshot.
    tooltips: RefCell<Option<(Arc<crate::settings::Settings>, gpui::SharedString, gpui::SharedString)>>,
    /// The request whose result is being reported: which session it ran on.
    pub(super) reporting: Option<Reporting>,
    /// Tabs that sent a statement on the connection's shared session in auto-commit: smart commit does
    /// not apply to them (a tab is never moved between sessions on its own; the editors opened after
    /// smart commit was turned on are the ones it applies to).
    shared_ran: HashSet<u64>,
    /// Changes whenever what the pending-transactions list shows may have.
    pub(super) revision: u64,
    /// The pending-transactions list, when it is open: redrawn by the ticker.
    pub(super) pending_view: Option<gpui::WeakEntity<super::transaction_dialogs::PendingTransactions>>,
    /// How many times the tooltips were built (a test counts them).
    #[cfg(test)]
    pub(super) tooltip_builds: std::cell::Cell<usize>,
    /// How many cleanups of an auto-commit run were started (a test counts them).
    #[cfg(test)]
    pub(super) cleanups: usize,
    ticker: Option<Task<()>>,
}

impl TransactionUi {
    /// Notes what is on its way for `tab_id`. `false`: something already is, and it stays.
    fn start_ending(&mut self, tab_id: u64, ending: Ending) -> bool {
        match self.ending.entry(tab_id) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(ending);
                if matches!(ending, Ending::Transaction(_)) {
                    self.last_end.remove(&tab_id);
                }
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    #[cfg(test)]
    pub(super) fn tx(&self, tab_id: u64) -> Option<&TabTx> {
        self.tabs.get(&tab_id)
    }

    /// A tab's state set as a test or a capture needs it (no session behind it).
    #[cfg(test)]
    pub(super) fn set_for_capture(&mut self, tab_id: u64, tx: TabTx) {
        self.tabs.insert(tab_id, tx);
        self.revision += 1;
    }

    /// Commit, Roll Back or a smart commit return to auto-commit is on its way for the tab.
    #[cfg(test)]
    pub(super) fn is_ending(&self, tab_id: u64) -> bool {
        self.ending.contains_key(&tab_id)
    }

    /// The connection's runs wait, as for the cleanup of an auto-commit run: they go on when the
    /// returned sender is dropped (a test holds them so).
    #[cfg(test)]
    pub(super) fn hold(&mut self, connection: &str) -> smol::channel::Sender<()> {
        let (gate, held) = smol::channel::bounded(1);
        self.cleaning.insert(connection.to_owned(), held);
        gate
    }

    #[cfg(test)]
    pub(super) fn is_holding(&self, connection: &str) -> bool {
        self.cleaning.contains_key(connection)
    }

    #[cfg(test)]
    pub(super) fn auto_open_tabs(&self) -> usize {
        self.auto_open.len() + self.auto_sent.len()
    }

    /// The result of a request on `route` is being reported to the tab (a test sets it).
    #[cfg(test)]
    pub(super) fn set_reporting(&mut self, reporting: Option<Reporting>) {
        self.reporting = reporting;
    }

    /// The statement an auto-commit tab has on its way, as a test needs it.
    #[cfg(test)]
    pub(super) fn set_auto_sent(&mut self, tab_id: u64, typed: Typed) {
        self.auto_sent.insert(tab_id, typed);
    }

    /// The tab's last statement is moved `secs` into the past, as a test needs it.
    #[cfg(test)]
    pub(super) fn age(&mut self, tab_id: u64, secs: u64) {
        if let Some(tx) = self.tabs.get_mut(&tab_id) {
            tx.last_active = tx.last_active.map(|last| last - Duration::from_secs(secs));
        }
    }

    /// A Commit "on its way" for `tab_id`, as a test needs it.
    #[cfg(test)]
    pub(super) fn set_ending(&mut self, tab_id: u64, ending: bool) {
        if ending {
            self.ending.insert(tab_id, Ending::Transaction(TxEnd::Commit));
        } else {
            self.ending.remove(&tab_id);
        }
    }

    /// The Commit or Roll Back of `tab_id`'s transaction that is on its way, if one is.
    pub(super) fn ending_of(&self, tab_id: u64) -> Option<TxEnd> {
        match self.ending.get(&tab_id) {
            Some(Ending::Transaction(end)) => Some(*end),
            _ => None,
        }
    }
}

/// The session a request ran on, for the code that turns its failure into a lost session.
#[derive(Clone, Copy)]
pub(super) struct Reporting {
    pub(super) tab_id: u64,
    pub(super) route: TabRoute,
    /// The driver's id of the tab's own session the request ran on (route `Own`).
    pub(super) own_id: Option<u64>,
}

/// What a run of an auto-commit tab left on the shared session.
#[derive(Default)]
struct AutoOpen {
    transaction: bool,
    autocommit_off: bool,
}

/// A statement longer than this is not read for the questions the driver asks.
const DANGER_SCAN_BYTES: usize = 64 * 1024;

/// How a statement ends the transaction of its tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxEnd {
    Commit,
    Rollback,
}

impl TxEnd {
    fn sql(self) -> &'static str {
        match self {
            Self::Commit => "COMMIT",
            Self::Rollback => "ROLLBACK",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Commit => "Commit",
            Self::Rollback => "Rollback",
        }
    }

    /// Said when this end is on its way and the user asked for the other one.
    fn already_running(self) -> &'static str {
        match self {
            Self::Commit => "Commit is already running in this tab. Wait for it to finish.",
            Self::Rollback => "Roll Back is already running in this tab. Wait for it to finish.",
        }
    }
}

/// What is on its way to the server for a tab ([`TransactionUi::ending`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The Commit or Roll Back of its open transaction (a click, a list, the idle timer).
    Transaction(TxEnd),
    /// The `ROLLBACK` / `SET autocommit = 1` that returns smart commit's session to auto-commit.
    Session,
}

/// Why Commit or Roll Back cannot run on a tab now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndBlocked {
    /// A statement runs, or Commit / Roll Back is already on its way.
    Busy,
    /// A cancelled statement is still stopping on the tab's session.
    Stopping,
}

impl EndBlocked {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Busy => "Wait for the running statement to finish, or cancel it.",
            Self::Stopping => "The statement that was cancelled is still stopping on this tab's session. Try again in a moment.",
        }
    }
}

/// Where a statement of a tab goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TabRoute {
    /// The connection's shared session.
    Shared,
    /// The tab's own session, which is open.
    Own,
    /// The tab's own session, which has to be opened first.
    Open,
    /// The connection's shared session, which is being cleaned up after another run: wait for it.
    Hold,
}

/// One request on a tab's own session: held (in flight) by its worker for as long as it runs, and
/// for a statement of the transaction, how it is counted.
pub(super) struct TxRun {
    inflight: Arc<AtomicU32>,
    counted: Option<Counted>,
    /// What to send before the statement (the `BEGIN` of a PostgreSQL / SQLite transaction, the
    /// `SET autocommit` of a MySQL / MariaDB session), with the kind it is for.
    begin: Option<(&'static str, ConnectionKind)>,
}

struct Counted {
    /// The driver's id of the session it was sent on.
    own_id: u64,
    kind: ConnectionKind,
    typed: Typed,
    /// It is not a pure read.
    writes: bool,
    /// MySQL / MariaDB commit before and after it by themselves.
    commits: bool,
}

impl TxRun {
    /// The statement to send before the request; its result is read only for what is not "already
    /// in a transaction".
    pub(super) fn begin(&self) -> Option<(&'static str, ConnectionKind)> {
        self.begin
    }

    /// Taken on the UI thread before the worker starts, and held by it until it is done.
    pub(super) fn enter(&self) -> InFlight {
        InFlight::enter(&self.inflight)
    }
}

/// Whether a failed `BEGIN` before a statement is the statement's failure. It is not when the
/// transaction is already open (SQLite refuses a second `BEGIN` with an error of its own) or
/// PostgreSQL has it aborted (it refuses everything but a rollback, and the statement says so).
pub(super) fn begin_error_matters(kind: ConnectionKind, error: &DriverQueryError) -> bool {
    let text = error.to_string();
    match kind {
        ConnectionKind::Sqlite => !text.contains("within a transaction"),
        ConnectionKind::Postgres => !is_aborted(error),
        _ => true,
    }
}

/// Whether a MySQL / MariaDB statement that commits implicitly failed after the server committed.
/// Only errors of the statement's own work on existing objects, which come after the commit, say so;
/// anything else (refused at parse: a syntax error, a bad name, an unknown character set; a read-only
/// server; an error without the driver's `(error N, SQLSTATE …)` suffix, such as a cancel) keeps the
/// transaction open: taking it for committed would let a return to autocommit commit it.
fn failed_after_implicit_commit(error: &DriverQueryError) -> bool {
    let text = error.to_string();
    let code = text
        .rsplit_once("(error ")
        .and_then(|(_, tail)| tail.split_once(',').filter(|(_, rest)| rest.trim_start().starts_with("SQLSTATE")))
        .and_then(|(code, _)| code.parse::<u32>().ok());
    // Can't create table, database exists / does not, table exists, unknown table, duplicate key /
    // column / key name / foreign key, unknown column, multiple primary keys, key column missing,
    // can't drop, unknown table (1146), key does not exist, foreign key refused.
    code.is_some_and(|code| {
        matches!(code, 1005 | 1007 | 1008 | 1022 | 1050 | 1051 | 1054 | 1060 | 1061 | 1062 | 1068 | 1072 | 1091 | 1146 | 1176 | 1215 | 1826)
    })
}

/// How Commit or Rollback ended.
pub(super) enum EndResult {
    Done,
    /// The check before a PostgreSQL Commit failed for a reason of its own (not an aborted
    /// transaction): the COMMIT was never sent, and whatever the transaction holds is still open.
    NotSent(DriverQueryError),
    /// PostgreSQL: the transaction had been aborted by a failed statement, so COMMIT only rolled
    /// it back (it reports that as a success, with the tag ROLLBACK).
    Aborted,
}

/// Blocking: Commit or Rollback of `session`'s transaction. PostgreSQL's COMMIT of an aborted
/// transaction rolls it back and says nothing, so a Commit first runs a statement that fails in an
/// aborted transaction (and only there): when it does, the transaction is rolled back instead.
fn exec_end(
    session: &engine_driver_host::SharedDriverSession,
    end: TxEnd,
    database: &str,
    postgres: bool,
) -> Result<EndResult, DriverQueryError> {
    if postgres && end == TxEnd::Commit {
        match exec_control(session, "SELECT 1", database) {
            Err(error) if is_aborted(&error) => {
                exec_control(session, "ROLLBACK", database)?;
                return Ok(EndResult::Aborted);
            }
            Err(error) => return Ok(EndResult::NotSent(error)),
            Ok(()) => {}
        }
    }
    exec_control(session, end.sql(), database)?;
    Ok(EndResult::Done)
}

/// PostgreSQL's "current transaction is aborted, commands ignored until end of transaction block".
fn is_aborted(error: &DriverQueryError) -> bool {
    let text = error.to_string();
    text.contains("25P02") || text.contains("current transaction is aborted")
}

/// Runs a transaction statement (`BEGIN`, `COMMIT`, `ROLLBACK`) on `session`, blocking, as an
/// operation of its own. Never under the session lock.
pub(super) fn exec_control(
    session: &engine_driver_host::SharedDriverSession,
    sql: &str,
    database: &str,
) -> Result<(), DriverQueryError> {
    let runner = session.query_runner()?;
    let request = serde_json::to_string(&QueryRequest {
        kind: "sql".into(),
        operation_id: runner.operation_id(),
        database: (!database.is_empty()).then(|| database.to_owned()),
        object: None,
        offset: 0,
        limit: 1,
        payload: serde_json::json!({"sql": sql, "confirmed": true}),
    })
    .expect("a transaction statement request serializes");
    let page = runner.open_query(&request)?;
    runner.release_page(page.page_id);
    Ok(())
}

/// What runs once the transactions a question was about have ended.
type Then = Box<dyn FnOnce(&mut AppRoot, &mut Window, &mut Context<AppRoot>)>;

#[cfg(test)]
thread_local! {
    /// Makes the setup of the next tab session fail on this (test) thread, after it opened.
    pub(super) static FAIL_SETUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const NO_CONNECTION: &str = "This tab has no connection.";

/// Said when an auto-commit tab's run left a transaction open and the app ended it.
const AUTO_TAB_NOTICE: &str =
    "Auto-commit tab: the open transaction was rolled back \u{2014} use Manual mode to keep a transaction";

/// Said when a PostgreSQL tab prepared its transaction: the session has none any more.
/// Said when COMMIT ran in a PostgreSQL transaction a failed statement had aborted.
const ABORTED_COMMIT: &str = "The transaction was aborted; COMMIT rolled it back";

const PREPARED_NOTICE: &str =
    "The transaction was prepared and is no longer this tab's: finish it with COMMIT PREPARED or ROLLBACK PREPARED";

const ABORTED_HINT: &str = "PostgreSQL: the transaction is aborted until you roll back";

/// How long (and how often) the end of a cancelled statement is waited for before its session is
/// closed under it anyway: the server rolls back with the session.
const WORKER_WAIT_POLLS: u32 = 500;
const WORKER_WAIT_STEP: Duration = Duration::from_millis(20);

/// What a tab says when its session is gone with work on it.
const SESSION_LOST: &str = "Session lost: uncommitted changes were rolled back by the server";

impl AppRoot {
    // ---- reading the state ------------------------------------------------------------

    pub(super) fn saved_of(&self, connection: &str) -> Option<&SavedConnection> {
        let id = uuid::Uuid::parse_str(connection).ok()?;
        self.saved_connections.iter().find(|saved| saved.id == id)
    }

    /// A tab that has a commit mode: an editor tab running SQL. A table-data tab does not (its grid
    /// Apply is a transaction of the driver's own, and browsing a table never opens one).
    fn is_transactional_tab(&self, tab_id: u64) -> bool {
        self.workspace.tab(tab_id).is_some_and(assistant::is_sql_editor)
    }

    /// The tab's connection kind and whether manual commit is available on it, kept while nothing
    /// they depend on changes (the saved connection, the settings, the connection types).
    fn facts(&self, tab_id: u64) -> Facts {
        use std::hash::{Hash, Hasher};
        let Some(tab) = self.workspace.tab(tab_id) else {
            return Facts::UNCONNECTED;
        };
        let saved = self.saved_of(&tab.connection_id);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tab.connection_id.hash(&mut hasher);
        if let Some(saved) = saved {
            (saved.kind as u8, &saved.uri).hash(&mut hasher);
        }
        let key = hasher.finish();
        {
            let mut cache = self.transactions.facts.borrow_mut();
            let current = cache.settings.as_ref().is_some_and(|held| Arc::ptr_eq(held, &self.settings))
                && cache.policy_revision == self.connection_policy.revision();
            if !current {
                *cache = FactsCache {
                    settings: Some(Arc::clone(&self.settings)),
                    policy_revision: self.connection_policy.revision(),
                    tabs: HashMap::new(),
                };
            }
            if let Some((cached, facts)) = cache.tabs.get(&tab_id) {
                if *cached == key {
                    return *facts;
                }
            }
        }
        let effective = self.effective_for(&tab.connection_id);
        let facts = match saved {
            None => Facts::UNCONNECTED,
            Some(saved) => {
                let in_memory = saved.kind == ConnectionKind::Sqlite && sqlite_is_in_memory(&saved.uri);
                let unavailable = manual_unavailable(saved.kind, effective.read_only, in_memory);
                Facts {
                    kind: Some(saved.kind),
                    unavailable,
                    default_mode: if effective.auto_commit || unavailable.is_some() { CommitMode::Auto } else { CommitMode::Manual },
                    // Never where manual commit is unavailable (ClickHouse, read-only, ...).
                    smart_commit: effective.smart_commit && unavailable.is_none(),
                    smart_recover: effective.smart_commit_recover && unavailable.is_none(),
                }
            }
        };
        self.transactions.facts.borrow_mut().tabs.insert(tab_id, (key, facts));
        facts
    }

    fn tx_kind(&self, tab_id: u64) -> Option<ConnectionKind> {
        self.facts(tab_id).kind
    }

    /// Why the tab cannot be in manual mode; `None` when it can.
    pub(super) fn manual_unavailable_reason(&self, tab_id: u64) -> Option<&'static str> {
        self.facts(tab_id).unavailable
    }

    pub(super) fn commit_mode(&self, tab_id: u64) -> CommitMode {
        if !self.is_transactional_tab(tab_id) {
            return CommitMode::Auto;
        }
        match self.transactions.tabs.get(&tab_id) {
            Some(tx) => tx.mode,
            None => self.facts(tab_id).default_mode,
        }
    }

    pub(super) fn tab_tx(&self, tab_id: u64) -> Option<&TabTx> {
        self.transactions.tabs.get(&tab_id)
    }

    fn tab_tx_mut(&mut self, tab_id: u64) -> &mut TabTx {
        let default = self.facts(tab_id).default_mode;
        self.transactions.tabs.entry(tab_id).or_insert_with(|| TabTx::new(default))
    }

    /// Tabs of `tabs` that ran a statement since their last Commit / Rollback, reads included:
    /// closing their session ends a transaction they have open. A tab that ran
    /// nothing closes silently.
    pub(super) fn open_transaction_tabs(&self, tabs: impl IntoIterator<Item = u64>) -> Vec<u64> {
        tabs.into_iter()
            .filter(|id| self.transactions.tabs.get(id).is_some_and(TabTx::is_open))
            .collect()
    }

    pub(super) fn all_open_transaction_tabs(&self) -> Vec<u64> {
        let mut tabs: Vec<u64> = self
            .transactions
            .tabs
            .iter()
            .filter(|(_, tx)| tx.is_open())
            .map(|(id, _)| *id)
            .collect();
        tabs.sort_unstable();
        tabs
    }

    /// The tab's session of its own, when it has one for `connection`. A session of another
    /// connection (the tab was moved) is never used.
    pub(super) fn own_session(&self, tab_id: u64, connection: &str) -> Option<&TabSession> {
        self.driver_runtime.tab_sessions.get(&tab_id).filter(|own| own.connection == connection)
    }

    /// The session the tab's statements and pages go through: its own in manual mode, else the
    /// connection's.
    pub(super) fn tab_session_for(
        &self,
        tab_id: u64,
        connection: &str,
    ) -> Option<Arc<engine_driver_host::SharedDriverSession>> {
        self.own_session(tab_id, connection)
            .map(|own| Arc::clone(&own.session))
            .or_else(|| self.driver_runtime.sessions.get(connection).cloned())
    }

    pub(super) fn tab_title(&self, tab_id: u64) -> String {
        self.workspace
            .tab(tab_id)
            .map_or_else(|| "this tab".to_owned(), |tab| tab.title.clone())
    }

    /// A statement (or an Apply) of the tab runs, or Commit / Rollback is on its way.
    fn tab_busy(&self, tab_id: u64) -> bool {
        self.transactions.ending.contains_key(&tab_id)
            || self
                .workspace
                .runtime(tab_id)
                .is_some_and(|runtime| !runtime.query.can_run() || runtime.sql_apply_running)
    }

    /// The tab's transaction changed: its toolbar and (for the buttons that read it) results redraw.
    fn notify_transaction(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.transactions.revision += 1;
        self.notify_pane_views(cx);
        self.notify_tab(tab_id, cx);
        cx.notify();
    }

    // ---- routing a statement ----------------------------------------------------------

    /// Decides the session `request` runs on: the tab's own in manual mode, else the connection's
    /// (after the cleanup of an auto-commit run on it, if one is on its way). `Err`: the statement
    /// is not sent, with the reason. The one decision the run goes by.
    pub(super) fn route_tab_statement(&mut self, request: &SqlRun) -> Result<TabRoute, String> {
        let route = self.route_statement(request)?;
        if route == TabRoute::Shared {
            match self.transactions.cleaning.get(&request.connection) {
                Some(gate) if !gate.is_closed() => return Ok(TabRoute::Hold),
                Some(_) => {
                    self.transactions.cleaning.remove(&request.connection);
                }
                None => {}
            }
        }
        Ok(route)
    }

    fn route_statement(&mut self, request: &SqlRun) -> Result<TabRoute, String> {
        let tab_id = request.tab_id;
        // A session of another connection (the tab was moved) is not this tab's any more: closed,
        // with whatever it had open.
        if self.driver_runtime.tab_sessions.get(&tab_id).is_some_and(|own| own.connection != request.connection) {
            let mode = self.commit_mode(tab_id);
            self.tab_transaction_closed(tab_id);
            self.tab_tx_mut(tab_id).mode = mode;
        }
        // An auto-commit tab uses a session of its own only when smart commit gave it one (it then
        // runs on it from its first statement, and keeps it).
        let wants_own = self.commit_mode(tab_id) == CommitMode::Manual || self.smart_session_wanted(tab_id);
        let own = self.own_session(tab_id, &request.connection).is_some() && wants_own;
        if self.scenario_ui.service.is_some() || request.kind != RunKind::Sql || !self.is_transactional_tab(tab_id) {
            // An Explain (or anything else that is not a statement of the transaction) reads on the
            // tab's session, so it waits for the same things a statement does.
            if own {
                self.tab_ready_for_statement(tab_id)?;
            }
            return Ok(if own { TabRoute::Own } else { TabRoute::Shared });
        }
        let facts = self.facts(tab_id);
        if facts.kind.is_none_or(|kind| manual_unavailable(kind, false, false).is_some()) {
            return Ok(TabRoute::Shared);
        }
        if self.commit_mode(tab_id) == CommitMode::Auto && !wants_own {
            return Ok(TabRoute::Shared);
        }
        if let Some(reason) = facts.unavailable.filter(|_| self.commit_mode(tab_id) == CommitMode::Manual) {
            return Err(format!("Manual commit is not available here: {reason} Switch the tab to Auto-commit."));
        }
        // The session of a manual tab runs with autocommit off, and the app relies on it.
        if matches!(facts.kind, Some(ConnectionKind::Mysql | ConnectionKind::Mariadb))
            && mentions_autocommit(&request.sql)
            && matches!(Typed::of(&request.sql, &self.sql_lex(tab_id)), Typed::Autocommit(_))
        {
            return Err(match self.commit_mode(tab_id) {
                CommitMode::Manual => "SET autocommit would change how this tab's session commits. Switch the tab to Auto-commit instead.",
                CommitMode::Auto => "SET autocommit would change how this tab's session commits. Switch the tab to Manual instead.",
            }
            .into());
        }
        self.tab_ready_for_statement(tab_id)?;
        // Smart commit's tab in auto-commit: a session of its own with autocommit on, until a statement
        // that modifies starts a transaction on it.
        self.tab_tx_mut(tab_id).auto_own |= self.commit_mode(tab_id) == CommitMode::Auto;
        match self.own_session(tab_id, &request.connection) {
            Some(own) => {
                if own.is_running() {
                    return Err("The statement that was cancelled is still stopping on this tab's session. Try again in a moment.".into());
                }
                let open = self.tab_tx(tab_id).is_some_and(TabTx::is_open);
                let uri = self.saved_of(&request.connection).map_or("", |saved| saved.uri.as_str());
                if open && !same_database(facts.kind.unwrap_or(ConnectionKind::Postgres), uri, &own.database, &request.database) {
                    return Err(format!(
                        "This tab's transaction is open on database \"{}\". Commit or roll back before using \"{}\".",
                        own.database, request.database
                    ));
                }
                Ok(TabRoute::Own)
            }
            None => Ok(TabRoute::Open),
        }
    }

    /// `Err` with the reason when the tab's session cannot take a statement now: Commit or Roll Back
    /// is on its way.
    fn tab_ready_for_statement(&self, tab_id: u64) -> Result<(), String> {
        if self.transactions.ending.contains_key(&tab_id) {
            return Err("Commit or Roll Back is still running in this tab. Wait for it to finish.".into());
        }
        Ok(())
    }

    /// Opens the tab's session, then sends the statement of `run` on it. Only the sending is the
    /// tab's task (a Cancel, a timeout or the tab closing drops it): the opening itself always runs
    /// to its end, which registers the session or closes it.
    pub(super) fn open_tab_session_then_run(&mut self, run: crate::query::QueryRunId, request: SqlRun, cx: &mut Context<Self>) {
        let tab_id = request.tab_id;
        let (connection, database) = (request.connection.clone(), request.database.clone());
        let opened = self.open_tab_session(tab_id, &connection, &database, cx);
        let task = cx.spawn(async move |this, cx| {
            let Ok(opened) = opened.recv().await else { return };
            let _ = this.update(cx, move |this, cx| {
                // Cancelled, timed out or superseded while the session opened: nothing to send.
                if !this.query_tasks.registry.owns(tab_id, run) {
                    return;
                }
                match opened {
                    Ok(()) => this.run_driver_sql_query(run, request, TabRoute::Own, cx),
                    Err(error) => {
                        this.smart_commit_undo(tab_id);
                        this.finish_driver_sql_run(run, request, Err(error), cx)
                    }
                }
            });
        });
        self.query_tasks.tasks.insert(tab_id, task);
    }

    /// Waits for the cleanup of an auto-commit run on the connection's shared session, then sends
    /// the statement of `run` there. Like the session opening, only the sending is the tab's task.
    pub(super) fn hold_then_run(&mut self, run: crate::query::QueryRunId, request: SqlRun, cx: &mut Context<Self>) {
        let tab_id = request.tab_id;
        let gate = self.transactions.cleaning.get(&request.connection).cloned();
        let task = cx.spawn(async move |this, cx| {
            if let Some(gate) = gate {
                let _ = gate.recv().await;
            }
            let _ = this.update(cx, move |this, cx| {
                if this.query_tasks.registry.owns(tab_id, run) {
                    this.run_driver_sql_query(run, request, TabRoute::Shared, cx);
                }
            });
        });
        self.query_tasks.tasks.insert(tab_id, task);
    }

    /// Opens a session of its own for the tab (MySQL / MariaDB: with autocommit off) off the UI
    /// thread, and answers on the returned channel once it is registered (or refused). The opening
    /// is detached from whoever asked: when it ends the session is registered if the tab is still
    /// there, in manual mode and on this connection, and closed (disconnected) otherwise, so that
    /// no server connection is left behind by a Cancel, a timeout or a tab that closed meanwhile.
    pub(super) fn open_tab_session(
        &mut self,
        tab_id: u64,
        connection: &str,
        database: &str,
        cx: &mut Context<Self>,
    ) -> smol::channel::Receiver<Result<(), DriverQueryError>> {
        let (answer, opened) = smol::channel::bounded(1);
        self.transactions.open_serial += 1;
        let serial = self.transactions.open_serial;
        self.transactions.opening.insert(tab_id, serial);
        let spec = self
            .saved_of(connection)
            .cloned()
            .ok_or_else(|| "Saved connection not found".to_owned())
            .and_then(|saved| self.secondary_session_spec(&saved, connection).map(|spec| (saved.kind, spec)));
        let (database, connection) = (database.to_owned(), connection.to_owned());
        let (kind, spec) = match spec {
            Ok(spec) => spec,
            Err(message) => {
                self.transactions.opening.remove(&tab_id);
                let _ = answer.try_send(Err(DriverQueryError::Session(message)));
                return opened;
            }
        };
        let handle = Arc::clone(&spec.handle);
        let setup_database = database.clone();
        // A manual tab's session runs with autocommit off; a smart commit tab in auto-commit starts
        // with it on, and turns it off when a statement that modifies starts a transaction.
        let manual = self.commit_mode(tab_id) == CommitMode::Manual;
        #[cfg(test)]
        let fail_setup = FAIL_SETUP.with(std::cell::Cell::get);
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let (session, tunnel) = spec.open()?;
                let connection_id = session.connection_id();
                let session = Arc::new(engine_driver_host::SharedDriverSession::new(session));
                let setup = session_setup(kind).filter(|_| manual).map_or(Ok(()), |setup| {
                    exec_control(&session, setup, &setup_database).map_err(|error| crate::sessions::redact_secrets(&error.to_string()))
                });
                #[cfg(test)]
                let setup = if fail_setup { Err("the session setup failed (test)".to_owned()) } else { setup };
                if let Err(message) = setup {
                    // A driver session that is only dropped stays open on the server.
                    disconnect_and_close(&session, tunnel);
                    return Err(message);
                }
                Ok::<_, String>((session, connection_id, tunnel))
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let opened = work.await;
            let own = opened.map(|(session, connection_id, tunnel)| TabSession {
                connection_id,
                session,
                handle,
                connection,
                database,
                inflight: Arc::new(AtomicU32::new(0)),
                tunnel,
            });
            // If the app is gone `update` fails and the closure, with the session in it, is dropped:
            // a dropped `TabSession` disconnects.
            let _ = this.update(cx, move |this, cx| {
                let result = match own {
                    Ok(own) => this.register_tab_session(tab_id, serial, own, cx),
                    Err(message) => Err(DriverQueryError::Session(message)),
                };
                let _ = answer.try_send(result);
            });
        })
        .detach();
        opened
    }

    /// A tab's new session: kept when the tab still wants one, else closed. `Err` (and the session
    /// closed) when the opening was cancelled by the tab, its connection or its driver going away
    /// (`serial` is no longer the tab's), or the tab is no longer in manual mode on this connection.
    fn register_tab_session(&mut self, tab_id: u64, serial: u64, own: TabSession, cx: &mut Context<Self>) -> Result<(), DriverQueryError> {
        let current = self.transactions.opening.get(&tab_id) == Some(&serial);
        if current {
            self.transactions.opening.remove(&tab_id);
        }
        let wanted = current
            && self.workspace.tab(tab_id).is_some_and(|tab| tab.connection_id == own.connection)
            && (self.commit_mode(tab_id) == CommitMode::Manual || self.tab_tx(tab_id).is_some_and(|tx| tx.auto_own));
        if !wanted {
            own.close();
            return Err(DriverQueryError::Session("the tab no longer wants a session of its own".into()));
        }
        // One already there (it opened first): that one is the tab's, and the answer is yes.
        if self.own_session(tab_id, &own.connection).is_none() {
            self.driver_runtime.tab_sessions.insert(tab_id, own);
        } else {
            own.close();
        }
        self.notify_transaction(tab_id, cx);
        Ok(())
    }

    /// A request is about to be sent on `route`: on the tab's own session it is held (in flight)
    /// and, when it is a statement, counted, and the `BEGIN` it takes (PostgreSQL / SQLite) is
    /// decided. `None` on the shared session. A page of a result the tab already has (a re-run at an
    /// offset) is neither counted nor given a `BEGIN`.
    pub(super) fn tx_sending(&mut self, request: &SqlRun, route: TabRoute, cx: &mut Context<Self>) -> Option<TxRun> {
        let tab_id = request.tab_id;
        if route != TabRoute::Own {
            self.auto_sending(request, route);
            return None;
        }
        let own = self.own_session(tab_id, &request.connection)?;
        let (inflight, own_id) = (Arc::clone(&own.inflight), own.connection_id);
        // A transaction the user starts by hand on smart commit's session is the tab's transaction:
        // counted, shown, and ended by Commit / Roll Back like the one a modifying statement starts.
        if self.commit_mode(tab_id) == CommitMode::Auto && request.kind == RunKind::Sql && request.offset == 0 && self.facts(tab_id).smart_commit {
            let opens = self.tx_kind(tab_id).is_some_and(|kind| match Typed::of(&request.sql, &self.sql_lex(tab_id)) {
                Typed::Begin => true,
                // A SAVEPOINT outside a transaction opens one on SQLite.
                Typed::Savepoint => kind == ConnectionKind::Sqlite,
                _ => false,
            });
            if opens {
                let tx = self.tab_tx_mut(tab_id);
                (tx.mode, tx.smart, tx.smart_fresh) = (CommitMode::Manual, true, true);
            }
        }
        // Smart commit's tab in auto-commit: no transaction, nothing to count.
        if self.commit_mode(tab_id) == CommitMode::Auto {
            if !self.tab_tx(tab_id).is_some_and(TabTx::is_open) {
                if let Some(own) = self.driver_runtime.tab_sessions.get_mut(&tab_id) {
                    own.database = request.database.clone();
                }
            }
            // MySQL / MariaDB: smart commit returned the tab to auto-commit without a round trip, so
            // the session still has autocommit off: the first request after it turns it on again,
            // on the worker, in order with the request (see `TabTx::autocommit_restore`).
            let restore = self.tab_tx(tab_id).is_some_and(|tx| tx.autocommit_restore);
            let begin = self
                .tx_kind(tab_id)
                .filter(|kind| restore && matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb))
                .map(|kind| ("SET autocommit = 1", kind));
            return Some(TxRun { inflight, counted: None, begin });
        }
        let kind = self.tx_kind(tab_id).filter(|_| request.kind == RunKind::Sql && request.offset == 0);
        let Some(kind) = kind else {
            return Some(TxRun { inflight, counted: None, begin: None });
        };
        let lex = self.sql_lex(tab_id);
        let typed = Typed::of(&request.sql, &lex);
        if typed == Typed::Begin && matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) {
            // START TRANSACTION commits what is pending.
            self.tab_tx_mut(tab_id).ended();
        }
        if !self.tab_tx(tab_id).is_some_and(TabTx::is_open) {
            // The transaction lives on the database it was started on.
            if let Some(own) = self.driver_runtime.tab_sessions.get_mut(&tab_id) {
                own.database = request.database.clone();
            }
        }
        // A transaction statement of the user's own changes no data.
        let writes = typed == Typed::Other && crate::sql_write::first_write_keyword(&request.sql, &lex).is_some();
        // MySQL / MariaDB commit by themselves at DDL and the like: the log says so.
        let commits = commits_by_itself(&request.sql, &lex, kind);
        let (tx, now) = (self.tab_tx_mut(tab_id), Instant::now());
        tx.sending(now, writes);
        tx.log.begin(now, writes, commits, &request.sql, &lex);
        self.ensure_tx_ticker(cx);
        self.notify_pane_views(cx);
        let mut begin = typed.takes_begin().then(|| begin_statement(kind)).flatten();
        // A MySQL / MariaDB session smart commit opened in auto-commit: autocommit is turned off before
        // every statement of the transaction, not once. A statement cancelled before it was sent, or a
        // failed `SET`, would otherwise leave the next one committing on its own. (Turning it off when
        // it is off commits nothing; a failed `SET` fails the statement, see `begin_error_matters`.)
        let tx = self.tab_tx_mut(tab_id);
        let pending = std::mem::take(&mut tx.autocommit_pending);
        // Turned off again below; there is nothing left to turn on.
        tx.autocommit_restore = false;
        if pending || (tx.auto_own && matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb)) {
            begin = Some("SET autocommit = 0");
        }
        Some(TxRun { inflight, counted: Some(Counted { own_id, kind, typed, writes, commits }), begin: begin.map(|begin| (begin, kind)) })
    }

    /// A statement of an auto-commit tab is about to be sent on the shared session. When it is a
    /// transaction statement, the run's end will have something to end (see `auto_run_ended`). Noted
    /// when it is really sent: a run that is held back and cancelled never was.
    fn auto_sending(&mut self, request: &SqlRun, route: TabRoute) {
        let tab_id = request.tab_id;
        self.transactions.auto_sent.remove(&tab_id);
        let transactional = route == TabRoute::Shared
            && request.kind == RunKind::Sql
            && self.scenario_ui.service.is_none()
            && self.is_transactional_tab(tab_id)
            && self.commit_mode(tab_id) == CommitMode::Auto
            && self.facts(tab_id).kind.is_some_and(|kind| manual_unavailable(kind, false, false).is_none());
        if transactional {
            self.transactions.shared_ran.insert(tab_id);
        }
        if transactional {
            match Typed::of(&request.sql, &self.sql_lex(tab_id)) {
                Typed::Other | Typed::Prepared => {}
                typed => {
                    self.transactions.auto_sent.insert(tab_id, typed);
                }
            }
        }
    }

    /// The statement sent as `run` ended with `error` (`None`: it ran). `run` is `None` on the
    /// shared session: an auto-commit tab only notes the transaction statements that ran.
    pub(super) fn tx_statement_finished(
        &mut self,
        tab_id: u64,
        run: Option<TxRun>,
        rows: Option<u64>,
        error: Option<&DriverQueryError>,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = run else {
            if let (Some(typed), None) = (self.transactions.auto_sent.remove(&tab_id), error) {
                self.note_auto_statement(tab_id, typed);
            }
            return;
        };
        // The statement smart commit switched the tab for has ended, whatever way it ended.
        let fresh = self.transactions.tabs.get_mut(&tab_id).is_some_and(|tx| std::mem::take(&mut tx.smart_fresh));
        let script = self.workspace.runtime(tab_id).is_some_and(|runtime| runtime.script_run.is_some());
        let Some(counted) = run.counted else {
            // It ran after the `SET autocommit = 1` it started with: the session commits by itself again.
            if error.is_none() && run.begin.is_some() {
                if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
                    tx.autocommit_restore = false;
                }
            }
            if fresh && error.is_some() {
                self.smart_commit_undo_after_send(tab_id, cx);
            }
            return;
        };
        // The session it ran on is gone (closed, or replaced): what happened there is not this tab's
        // transaction any more.
        if self.own_session(tab_id, &self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()).unwrap_or_default()).map(|own| own.connection_id) != Some(counted.own_id) {
            if fresh && error.is_some() {
                self.smart_commit_undo_after_send(tab_id, cx);
            }
            return;
        }
        let outcome = match error {
            None => Outcome::Done,
            Some(error) if error.confirmation_reason().is_some() || error.is_read_only_violation() => Outcome::NotRun,
            Some(_) => Outcome::Failed,
        };
        let postgres = counted.kind == ConnectionKind::Postgres;
        let mut undo_smart = false;
        if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
            let now = Instant::now();
            match (outcome, error) {
                (Outcome::Done, _) => tx.log.finish(LogResult::Ok, rows, None, now),
                (Outcome::Failed, error) => {
                    let message = error.map(|error| crate::sessions::redact_secrets(&error.to_string()));
                    tx.log.finish(LogResult::Failed, None, message.as_deref(), now);
                }
                // `finished` forgets a statement that never ran.
                (Outcome::NotRun, _) => {}
            }
            // The statement smart commit started a transaction for did not run (a script goes on in
            // the transaction: its next statements may modify too).
            undo_smart = fresh && outcome != Outcome::Done && !script;
            tx.finished(outcome, counted.writes, counted.typed == Typed::End, postgres, now);
            if counted.typed == Typed::Chain && outcome == Outcome::Done {
                tx.chained(now);
            }
            // MySQL / MariaDB commit by themselves before DDL and the like run: what the transaction held
            // is on the server, and the statement is where the log starts again. Not when the statement
            // failed before that point (a syntax error is refused at parse, nothing was committed), nor
            // when the error does not say where it failed: the transaction is kept as open.
            let committed = match outcome {
                Outcome::Done => true,
                Outcome::Failed => error.is_some_and(failed_after_implicit_commit),
                Outcome::NotRun => false,
            };
            if counted.commits && committed && !matches!(counted.typed, Typed::End | Typed::Chain) {
                tx.server_committed();
            }
            if outcome == Outcome::Done && postgres && counted.typed == Typed::PrepareTransaction {
                // The session has no transaction any more: the server keeps the prepared one.
                tx.ended();
                self.with_ui_window(cx, |window, app| window.push_notification(PREPARED_NOTICE, app));
            }
        }
        if undo_smart {
            self.smart_leave_transaction(tab_id, true, cx);
        }
        // A COMMIT / ROLLBACK typed in the transaction smart commit started ends it, wherever it stands:
        // the tab is in auto-commit again before the next statement of a script, which is judged afresh
        // (it recovers at the commit event, not at the end of the script).
        if counted.typed == Typed::End {
            self.smart_commit_recover(tab_id, cx);
        }
        if let (Some(error), Outcome::Failed) = (error, outcome) {
            if error.to_string().contains("invalid transaction termination") {
                // A PostgreSQL procedure that COMMITs or ROLLBACKs inside itself cannot run in a transaction block.
                let connection = self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone());
                self.report_warning(
                    Area::Query,
                    connection.as_deref(),
                    format!("{}: a procedure that commits or rolls back by itself cannot run inside this tab's transaction. Run it in a tab in auto-commit, or turn smart commit off for this connection type.", self.tab_title(tab_id)),
                );
            }
        }
        self.notify_transaction(tab_id, cx);
    }

    /// An auto-commit tab's transaction statement ran (or was on its way when a cancel landed).
    fn note_auto_statement(&mut self, tab_id: u64, typed: Typed) {
        let sqlite = self.tx_kind(tab_id) == Some(ConnectionKind::Sqlite);
        let open = self.transactions.auto_open.entry(tab_id).or_default();
        match typed {
            Typed::Begin => open.transaction = true,
            // A SAVEPOINT outside a transaction opens one on SQLite.
            Typed::Savepoint => open.transaction |= sqlite,
            Typed::End => open.transaction = false,
            Typed::Autocommit(on) => open.autocommit_off = !on,
            Typed::Chain | Typed::Prepared | Typed::PrepareTransaction | Typed::Other => {}
        }
    }

    /// The tab's running statement was cancelled, or timed out: on a manual tab it counts as
    /// modifying (nobody knows how far it got) and PostgreSQL has aborted the transaction around it;
    /// on an auto-commit tab it may have been a `BEGIN`.
    pub(super) fn tx_run_cancelled(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let postgres = self.tx_kind(tab_id) == Some(ConnectionKind::Postgres);
        if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
            // Cancelled before it was sent (the session was still opening): nothing was started.
            let unsent = std::mem::take(&mut tx.smart_fresh) && tx.statements == 0;
            tx.cancelled(postgres);
            if unsent {
                self.smart_commit_undo(tab_id);
            }
            self.notify_transaction(tab_id, cx);
        }
        if let Some(typed) = self.transactions.auto_sent.remove(&tab_id) {
            self.note_auto_statement(tab_id, typed);
        }
        self.auto_run_ended(tab_id, cx);
    }

    /// A run of an auto-commit tab ended (or was cancelled). When it left a transaction open, or
    /// turned autocommit off, on the connection's shared session - where every tab of the
    /// connection would share it - the app ends that: ROLLBACK, and autocommit on again. The runs
    /// of that connection wait (`TabRoute::Hold`) until it landed. A run that left nothing open does
    /// nothing at all.
    pub(super) fn auto_run_ended(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.transactions.auto_sent.remove(&tab_id);
        // A COMMIT / ROLLBACK that ended the transaction of a tab smart commit switched has already
        // returned it (see `tx_statement_finished`); this catches the run that was cancelled on one.
        self.smart_commit_recover(tab_id, cx);
        self.release_own_session_if_unwanted(tab_id, cx);
        let Some(open) = self.transactions.auto_open.remove(&tab_id) else { return };
        if !(open.transaction || open.autocommit_off) {
            return;
        }
        let Some((connection, database)) = self.workspace.tab(tab_id).map(|tab| (tab.connection_id.clone(), tab.database.clone())) else {
            return;
        };
        let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else { return };
        let sqlite = self.tx_kind(tab_id) == Some(ConnectionKind::Sqlite);
        let (gate, held) = smol::channel::bounded::<()>(1);
        self.transactions.cleaning.insert(connection, held);
        #[cfg(test)]
        {
            self.transactions.cleanups += 1;
        }
        let work = cx.background_executor().spawn(async move {
            // The gate opens when this is done, whatever it ends in.
            let _gate = gate;
            smol::unblock(move || {
                let rollback = exec_control(&session, "ROLLBACK", &database);
                let autocommit = if open.autocommit_off { exec_control(&session, "SET autocommit = 1", &database) } else { Ok(()) };
                match (rollback, autocommit) {
                    (Ok(()), Ok(())) => Ok(true),
                    // SQLite says so when a SAVEPOINT was released and nothing is open any more.
                    (Err(_), Ok(())) if sqlite => Ok(false),
                    (Err(error), _) | (_, Err(error)) => Err(error),
                }
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(true) => {
                    this.with_ui_window(cx, |window, app| window.push_notification(AUTO_TAB_NOTICE, app));
                }
                Ok(false) => {}
                Err(error) => {
                    let message = crate::sessions::redact_secrets(&error.to_string());
                    let connection = this.workspace.tab(tab_id).map(|tab| tab.connection_id.clone());
                    this.report_warning(Area::Query, connection.as_deref(), format!("Could not end the transaction an auto-commit tab left open: {message}"));
                }
            });
        })
        .detach();
    }

    // ---- smart commit -----------------------------------------------------------------

    /// Smart commit gives the tab a session of its own from its first statement, in auto-commit: it
    /// is an editor tab on a connection with transactions, smart commit is on for the connection
    /// type, and it has not sent a statement on the shared session (a tab is never moved between
    /// sessions on its own: the editors opened after smart commit was turned on are the ones it
    /// applies to). A tab that has such a session keeps it.
    pub(super) fn smart_session_wanted(&self, tab_id: u64) -> bool {
        if self.scenario_ui.service.is_some() || !self.is_transactional_tab(tab_id) {
            return false;
        }
        let facts = self.facts(tab_id);
        facts.unavailable.is_none()
            && facts.kind.is_some()
            && (self.tab_tx(tab_id).is_some_and(|tx| tx.auto_own) || (facts.smart_commit && !self.transactions.shared_ran.contains(&tab_id)))
    }

    /// Smart commit could start a transaction for a statement of the tab: it is in auto-commit on a
    /// session of its own (or will be, at this statement), nothing is ending.
    fn smart_commit_allowed(&self, tab_id: u64) -> Option<ConnectionKind> {
        let facts = self.facts(tab_id);
        facts
            .kind
            .filter(|_| facts.smart_commit || self.tab_tx(tab_id).is_some_and(|tx| tx.auto_own))
            .filter(|_| self.smart_session_wanted(tab_id))
            .filter(|_| self.commit_mode(tab_id) == CommitMode::Auto && !self.transactions.ending.contains_key(&tab_id))
    }

    /// Smart commit: a tab in auto-commit on its own session, about to send a statement that
    /// modifies, starts a transaction first (`SET autocommit = 0` on MySQL / MariaDB, a `BEGIN` before
    /// the statement on the others) so the change waits for Commit or Roll Back. The same session: what
    /// the tab set in it (`SET`, `USE`, `PRAGMA`, temporary tables) is still there. Never where manual
    /// commit is unavailable (ClickHouse, a read-only connection). `true`: the transaction was started,
    /// and the caller undoes it ([`Self::smart_commit_undo`]) if the statement is not sent.
    pub(super) fn smart_commit_switch(&mut self, request: &SqlRun, cx: &mut Context<Self>) -> bool {
        let tab_id = request.tab_id;
        if request.kind != RunKind::Sql || request.offset != 0 || !self.facts(tab_id).smart_commit {
            return false;
        }
        let Some(kind) = self.smart_commit_allowed(tab_id) else { return false };
        let lex = self.sql_lex(tab_id);
        if smart_verdict(&request.sql, &lex, kind) != Smart::Modifies {
            return false;
        }
        // The driver asks before it runs a dangerous statement: the transaction is started when the
        // answer is in.
        if !request.confirmed
            && request.sql.len() <= DANGER_SCAN_BYTES
            && self.effective_for(&request.connection).confirm_dangerous
            && engine_driver_kit::sql::classify(&request.sql, &lex).danger.is_some()
        {
            return false;
        }
        // A session that is open has autocommit on: it is turned off with the statement.
        let on_open_session = self.own_session(tab_id, &request.connection).is_some();
        let tx = self.tab_tx_mut(tab_id);
        (tx.mode, tx.smart, tx.smart_fresh, tx.auto_own) = (CommitMode::Manual, true, true, true);
        tx.autocommit_pending = on_open_session && matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb);
        self.notify_transaction(tab_id, cx);
        true
    }

    /// The statement a smart commit transaction was started for is not sent: auto-commit again, as it
    /// was. Nothing was sent to the server.
    pub(super) fn smart_commit_undo(&mut self, tab_id: u64) {
        if let Some(tx) = self.transactions.tabs.get_mut(&tab_id).filter(|tx| tx.smart_fresh) {
            (tx.mode, tx.smart, tx.smart_fresh, tx.autocommit_pending) = (CommitMode::Auto, false, false, false);
        }
    }

    /// The transaction a smart commit started for a statement is over, the statement did not run
    /// (and what was sent is rolled back).
    fn smart_commit_undo_after_send(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.smart_leave_transaction(tab_id, true, cx);
    }

    /// The tab's transaction ended, or never held anything: auto-commit on the same session. With
    /// `rollback` (the statement that started it did not run, or failed) what it left open is first
    /// rolled back and, on MySQL / MariaDB, autocommit turned on again, in a round trip of its own
    /// (the tab is busy until it is done). Without it nothing is open and nothing is sent: on MySQL /
    /// MariaDB the next request on the session starts with `SET autocommit = 1`
    /// ([`TabTx::autocommit_restore`]), so a script that goes on after its own COMMIT never waits for
    /// a round trip, nor is refused while one is on its way. Nothing is replayed and nothing moves:
    /// the session is the one the tab had.
    pub(super) fn smart_leave_transaction(&mut self, tab_id: u64, rollback: bool, cx: &mut Context<Self>) {
        let mysql = matches!(self.tx_kind(tab_id), Some(ConnectionKind::Mysql | ConnectionKind::Mariadb));
        let tx = self.tab_tx_mut(tab_id);
        (tx.mode, tx.smart, tx.smart_fresh, tx.autocommit_pending) = (CommitMode::Auto, false, false, false);
        if !rollback {
            tx.autocommit_restore = mysql;
            self.notify_transaction(tab_id, cx);
            return;
        }
        tx.ended();
        let connection = self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()).unwrap_or_default();
        let session = self.own_session(tab_id, &connection).map(|own| (Arc::clone(&own.session), own.database.clone(), own.connection_id));
        let has_session = session.is_some();
        let Some((session, database, own_id)) = session.filter(|_| self.transactions.start_ending(tab_id, Ending::Session)) else {
            // Nothing is sent (another end is on its way, or the session is gone): a session that is still
            // there keeps its autocommit off, and the next request on it turns it on.
            self.tab_tx_mut(tab_id).autocommit_restore = mysql && has_session;
            self.notify_transaction(tab_id, cx);
            return;
        };
        self.tab_tx_mut(tab_id).autocommit_restore = false;
        let job = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                // SQLite says so when nothing is open.
                let _ = exec_control(&session, "ROLLBACK", &database);
                if mysql {
                    exec_control(&session, "SET autocommit = 1", &database)
                } else {
                    Ok(())
                }
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.transactions.ending.remove(&tab_id);
                if let Err(error) = result {
                    // A session that cannot say it is in autocommit is not used: the next statement opens another.
                    let message = crate::sessions::redact_secrets(&error.to_string());
                    let connection = this.workspace.tab(tab_id).map(|tab| tab.connection_id.clone());
                    if this.own_session(tab_id, connection.as_deref().unwrap_or_default()).map(|own| own.connection_id) == Some(own_id) {
                        if let Some(own) = this.driver_runtime.tab_sessions.remove(&tab_id) {
                            own.close();
                        }
                    }
                    this.report_warning(Area::Query, connection.as_deref(), format!("Could not return a session to auto-commit, it was closed: {message}"));
                }
                this.release_own_session_if_unwanted(tab_id, cx);
                this.notify_transaction(tab_id, cx);
            });
        })
        .detach();
        self.notify_transaction(tab_id, cx);
    }

    /// Commit or Roll Back (or a typed one, or the server's own commit) ended the transaction of a tab
    /// smart commit started: back to auto-commit on the same session when the connection type asks
    /// for it, at once, also in the middle of a script. (Any tab is switched back, except a tab the user put in manual mode, or a connection
    /// type that starts tabs there, keeps it here.)
    fn smart_commit_recover(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let ended = self.tab_tx(tab_id).is_some_and(|tx| tx.smart && !tx.is_open() && tx.lost.is_none());
        if ended && self.facts(tab_id).smart_recover {
            self.smart_leave_transaction(tab_id, false, cx);
        }
    }

    /// A tab's own session that its connection type asks for no more is closed, and the tab is on the
    /// shared session again: smart commit turned off for a tab smart commit gave its session to, or the
    /// connection read-only (no manual commit there). Only a session that is idle goes - no open
    /// transaction, nothing running, nothing on its way, none being opened - and the others are looked
    /// at again when their transaction ends or their run does. A tab the user made manual keeps its
    /// session when smart commit goes off; it is theirs.
    pub(super) fn release_own_session_if_unwanted(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        // Every run of every tab ends here: most have no session of their own.
        if !self.driver_runtime.tab_sessions.contains_key(&tab_id) {
            return;
        }
        let facts = self.facts(tab_id);
        let smart_only = self.tab_tx(tab_id).is_some_and(|tx| tx.auto_own && (tx.smart || self.commit_mode(tab_id) == CommitMode::Auto));
        let unwanted = facts.unavailable.is_some() || (!facts.smart_commit && smart_only);
        let idle = !self.tab_tx(tab_id).is_some_and(TabTx::is_open)
            && !self.tab_busy(tab_id)
            && !self.own_worker_running(tab_id)
            && !self.transactions.opening.contains_key(&tab_id);
        if !(unwanted && idle) {
            return;
        }
        self.release_driver_page(tab_id);
        if let Some(own) = self.driver_runtime.tab_sessions.remove(&tab_id) {
            own.close();
        }
        self.transactions.tabs.insert(tab_id, TabTx::new(CommitMode::Auto));
        self.notify_transaction(tab_id, cx);
    }

    /// The connection types or a connection's type changed: every tab's own session is looked at
    /// ([`Self::release_own_session_if_unwanted`]).
    pub(super) fn release_idle_own_sessions(&mut self, cx: &mut Context<Self>) {
        let tabs: Vec<u64> = self.driver_runtime.tab_sessions.keys().copied().collect();
        for tab_id in tabs {
            self.release_own_session_if_unwanted(tab_id, cx);
        }
    }

    // ---- the mode ---------------------------------------------------------------------

    /// The Auto-commit / Manual toggle and the palette command end here.
    pub(super) fn set_commit_mode(&mut self, tab_id: u64, mode: CommitMode, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_transactional_tab(tab_id) || self.commit_mode(tab_id) == mode {
            return;
        }
        if self.tab_busy(tab_id) {
            window.push_notification("Wait for the running statement to finish, or cancel it.", cx);
            return;
        }
        match mode {
            CommitMode::Manual => {
                if let Some(reason) = self.manual_unavailable_reason(tab_id) {
                    window.push_notification(reason, cx);
                    return;
                }
                let own_session = self.tab_tx(tab_id).is_some_and(|tx| tx.auto_own);
                let mysql = matches!(self.tx_kind(tab_id), Some(ConnectionKind::Mysql | ConnectionKind::Mariadb));
                // The page on screen belongs to the shared session (a smart commit tab has none).
                if !own_session {
                    self.release_driver_page(tab_id);
                }
                let tx = self.tab_tx_mut(tab_id);
                tx.mode = CommitMode::Manual;
                // Its session has autocommit on: the first statement turns it off.
                tx.autocommit_pending = own_session && mysql;
                self.notify_transaction(tab_id, cx);
            }
            CommitMode::Auto => {
                let asked = self.guard_transactions(
                    vec![tab_id],
                    "Switching to auto-commit ends the tab's session.",
                    window,
                    cx,
                    move |this, _, cx| this.switch_to_auto(tab_id, cx),
                );
                if !asked {
                    self.switch_to_auto(tab_id, cx);
                }
            }
        }
    }

    /// Back to auto-commit. A smart commit tab keeps the session it has (autocommit on again); any other
    /// goes back to the connection's shared session, and its own closes.
    fn switch_to_auto(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.tab_tx(tab_id).is_some_and(|tx| tx.auto_own) {
            self.smart_leave_transaction(tab_id, true, cx);
            return;
        }
        self.release_driver_page(tab_id);
        if let Some(own) = self.driver_runtime.tab_sessions.remove(&tab_id) {
            own.close();
        }
        self.transactions.tabs.insert(tab_id, TabTx::new(CommitMode::Auto));
        self.notify_transaction(tab_id, cx);
    }

    /// Flips the active tab between the two modes (palette).
    pub(super) fn toggle_commit_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) else {
            return;
        };
        let next = match self.commit_mode(tab_id) {
            CommitMode::Auto => CommitMode::Manual,
            CommitMode::Manual => CommitMode::Auto,
        };
        self.set_commit_mode(tab_id, next, window, cx);
    }

    // ---- ending a transaction ---------------------------------------------------------

    /// Commit or Rollback of the active tab (buttons, shortcuts, palette).
    pub(super) fn end_active_transaction(&mut self, end: TxEnd, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) else {
            return;
        };
        self.end_tab_transaction(tab_id, end, window, cx);
    }

    /// Why Commit / Roll Back cannot run on the tab now; `None` when it can.
    pub(super) fn end_blocked(&self, tab_id: u64) -> Option<EndBlocked> {
        if self.tab_busy(tab_id) {
            return Some(EndBlocked::Busy);
        }
        let connection = &self.workspace.tab(tab_id)?.connection_id;
        self.own_session(tab_id, connection).is_some_and(TabSession::is_running).then_some(EndBlocked::Stopping)
    }

    pub(super) fn end_tab_transaction(&mut self, tab_id: u64, end: TxEnd, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tab_tx(tab_id).is_some_and(TabTx::is_open) {
            return;
        }
        // The same end is already on its way (a second click or shortcut, the idle timer): that one is
        // the answer, a second would only fail. Another end is not: say which is running.
        match self.transactions.ending_of(tab_id) {
            Some(running) if running == end => return,
            Some(running) => {
                window.push_notification(running.already_running(), cx);
                return;
            }
            None => {}
        }
        if let Some(blocked) = self.end_blocked(tab_id) {
            window.push_notification(blocked.message(), cx);
            return;
        }
        self.end_transactions(vec![tab_id], end, None, cx);
    }

    /// Commits or rolls back each of `tabs`, one after the other; `then` runs when every one ended
    /// (one that cannot end now, or whose Commit failed, stops it: the user decides).
    pub(super) fn end_transactions(&mut self, tabs: Vec<u64>, end: TxEnd, then: Option<Then>, cx: &mut Context<Self>) {
        self.end_transactions_noting(tabs, end, then, None, false, cx);
    }

    /// [`Self::end_transactions`] for a list the user ticked rows of: a tab that is running a
    /// statement (or ending its transaction) is left alone, whatever the list showed, and nothing is
    /// cancelled.
    pub(super) fn end_transactions_skipping_running(&mut self, tabs: Vec<u64>, end: TxEnd, cx: &mut Context<Self>) {
        self.end_transactions_noting(tabs, end, None, None, true, cx);
    }

    /// [`Self::end_transactions`] saying `notice` once each ended, instead of the usual line.
    fn end_transactions_noting(
        &mut self,
        tabs: Vec<u64>,
        end: TxEnd,
        then: Option<Then>,
        notice: Option<String>,
        skip_running: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let mut all_ended = true;
            for tab_id in tabs {
                if skip_running && this.update(cx, |this, _| this.tab_query_running(tab_id) || this.end_blocked(tab_id).is_some()).unwrap_or(true) {
                    continue;
                }
                // Ended meanwhile (a second click, another list): nothing to commit, roll back or cancel.
                if !this.update(cx, |this, _| this.tab_tx(tab_id).is_some_and(TabTx::is_open)).unwrap_or(false) {
                    continue;
                }
                // An end of this tab is already on its way (a second click, another list, the idle timer).
                // The same end is waited for, not failed: it is the answer to this request too (if it did not
                // end the transaction, this one tries). The other end is not what was asked for, and says so.
                let on_its_way = this.update(cx, |this, _| this.transactions.ending_of(tab_id)).unwrap_or(None);
                if let Some(running) = on_its_way.filter(|running| *running != end) {
                    let _ = this.update(cx, |this, _| {
                        let (title, connection) = (this.tab_title(tab_id), this.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()));
                        this.report_warning(Area::Query, connection.as_deref(), format!("{} was not run in {title}: {}", end.verb(), running.already_running()));
                    });
                    all_ended = false;
                    continue;
                }
                if on_its_way.is_some() {
                    while this.update(cx, |this, _| this.transactions.ending.contains_key(&tab_id)).unwrap_or(false) {
                        smol::Timer::after(WORKER_WAIT_STEP).await;
                    }
                    // The one on its way stops what waits for it when it failed (the user reads the error
                    // first), whatever it left of the transaction: a Commit the server refused ends it too.
                    let ended = this.update(cx, |this, _| this.transactions.last_end.get(&tab_id).copied()).unwrap_or(None);
                    if ended == Some(false) {
                        all_ended = false;
                        continue;
                    }
                }
                if end == TxEnd::Rollback {
                    // A running statement is cancelled and waited for: the user asked for the end.
                    let cancelled = this
                        .update(cx, |this, cx| {
                            let running = this.tab_query_running(tab_id) && !this.transactions.ending.contains_key(&tab_id);
                            if running {
                                this.cancel_tab_query(tab_id, cx);
                            }
                            running
                        })
                        .unwrap_or(false);
                    for _ in 0..(if cancelled { WORKER_WAIT_POLLS } else { 0 }) {
                        if !this.update(cx, |this, _| this.own_worker_running(tab_id)).unwrap_or(false) {
                            break;
                        }
                        smol::Timer::after(WORKER_WAIT_STEP).await;
                    }
                }
                let job = this.update(cx, |this, cx| {
                    // Ended meanwhile (a second click, another list): nothing to commit or roll back.
                    if !this.tab_tx(tab_id).is_some_and(TabTx::is_open) {
                        return Ok(None);
                    }
                    if let Some(blocked) = this.end_blocked(tab_id) {
                        let (title, connection) = (this.tab_title(tab_id), this.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()));
                        this.report_warning(Area::Query, connection.as_deref(), format!("{} was not run in {title}: {}", end.verb(), blocked.message()));
                        return Err(());
                    }
                    if !this.driver_runtime.tab_sessions.contains_key(&tab_id) {
                        // Nothing to talk to (its session is gone): the transaction is over.
                        if let Some(tx) = this.transactions.tabs.get_mut(&tab_id) {
                            tx.ended();
                        }
                        this.notify_transaction(tab_id, cx);
                        return Ok(None);
                    }
                    Ok(this.spawn_end(tab_id, end, cx))
                });
                let job = match job {
                    Ok(Ok(Some(job))) => job,
                    Ok(Ok(None)) => continue,
                    _ => {
                        all_ended = false;
                        continue;
                    }
                };
                let result = job.await;
                let ended = this.update(cx, |this, cx| this.finish_end(tab_id, end, result, notice.clone(), cx)).unwrap_or(false);
                all_ended &= ended;
            }
            let Some(then) = then.filter(|_| all_ended) else { return };
            let _ = this.update(cx, |this, cx| {
                let root = cx.entity().downgrade();
                this.with_ui_window(cx, move |window, app| {
                    let _ = root.update(app, |this, cx| then(this, window, cx));
                });
            });
        })
        .detach();
    }

    pub(super) fn spawn_end(&mut self, tab_id: u64, end: TxEnd, cx: &mut Context<Self>) -> Option<Task<Result<EndResult, DriverQueryError>>> {
        let own = self.driver_runtime.tab_sessions.get(&tab_id)?;
        if !self.transactions.start_ending(tab_id, Ending::Transaction(end)) {
            return None;
        }
        let (session, database) = (Arc::clone(&own.session), own.database.clone());
        let postgres = self.tx_kind(tab_id) == Some(ConnectionKind::Postgres);
        self.notify_transaction(tab_id, cx);
        Some(
            cx.background_executor()
                .spawn(async move { smol::unblock(move || exec_end(&session, end, &database, postgres)).await }),
        )
    }

    /// The answer of Commit or Rollback. `true`: the transaction is over. A Commit the server refused
    /// is over too (it rolled the transaction back), but it stops what was waiting for it: the user
    /// reads the error first. A Rollback that failed counts as ended: its session is dropped, and the
    /// server rolls back with it. A connection that dropped during Commit leaves nobody knowing
    /// whether it was applied: it says so.
    pub(super) fn finish_end(
        &mut self,
        tab_id: u64,
        end: TxEnd,
        result: Result<EndResult, DriverQueryError>,
        notice: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.transactions.ending.remove(&tab_id);
        let ended = match result {
            Ok(EndResult::Aborted) => {
                if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
                    tx.ended();
                }
                let (title, connection) = (self.tab_title(tab_id), self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()));
                self.report_warning(Area::Query, connection.as_deref(), format!("{title}: {ABORTED_COMMIT}"));
                self.with_ui_window(cx, |window, app| window.push_notification(ABORTED_COMMIT, app));
                // What was waiting for the Commit (closing the tab) waits for the user to read this.
                false
            }
            Ok(EndResult::NotSent(error)) => {
                let message = crate::sessions::redact_secrets(&error.to_string());
                let (title, connection) = (self.tab_title(tab_id), self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()));
                self.report_error(Area::Query, connection.as_deref(), format!("Commit was not sent in {title}: {message}"));
                // A dead connection took the transaction with it; any other reason leaves it open.
                if error.is_connection_lost() {
                    self.lose_tab_transaction(tab_id, SESSION_LOST);
                }
                false
            }
            Ok(EndResult::Done) => {
                let done = self.transactions.tabs.get(&tab_id).map(|tx| {
                    let elapsed = tx.since.map_or(Duration::ZERO, |since| Instant::now().saturating_duration_since(since));
                    (tx.statements, elapsed_label(elapsed))
                });
                if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
                    tx.ended();
                }
                let notice = notice.or_else(|| {
                    done.map(|(statements, elapsed)| {
                        let verb = if end == TxEnd::Commit { "Committed" } else { "Rolled back" };
                        format!("{verb} {} ({elapsed})", statements_label(statements))
                    })
                });
                if let Some(notice) = notice {
                    self.with_ui_window(cx, move |window, app| window.push_notification(notice, app));
                }
                true
            }
            Err(error) => {
                let message = crate::sessions::redact_secrets(&error.to_string());
                let (title, connection) = (self.tab_title(tab_id), self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()));
                self.report_error(Area::Query, connection.as_deref(), format!("{} failed in {title}: {message}", end.verb()));
                match (end, error.is_connection_lost()) {
                    // A failed ROLLBACK leaves the session in an unknown state: it is dropped, and the
                    // server rolls back what it left open.
                    (TxEnd::Rollback, _) => self.lose_tab_transaction(
                        tab_id,
                        &format!("Rollback failed: {message}. The session was closed, which rolls the transaction back."),
                    ),
                    (TxEnd::Commit, true) => self.lose_tab_transaction(
                        tab_id,
                        "Session lost during Commit: whether the server applied it is unknown. Check the data before running it again.",
                    ),
                    // Either way the server ended it.
                    (TxEnd::Commit, false) => {
                        if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
                            tx.ended();
                        }
                    }
                }
                end == TxEnd::Rollback
            }
        };
        self.transactions.last_end.insert(tab_id, ended);
        self.smart_commit_recover(tab_id, cx);
        // The type may have stopped asking for the session while the transaction was open.
        self.release_own_session_if_unwanted(tab_id, cx);
        self.notify_transaction(tab_id, cx);
        ended
    }

    /// The tab's session is gone or unusable: it is dropped, and an open transaction is shown as lost
    /// (the server rolled it back with the session).
    pub(super) fn lose_tab_transaction(&mut self, tab_id: u64, reason: &str) {
        let Some(own) = self.driver_runtime.tab_sessions.remove(&tab_id) else {
            return;
        };
        own.close();
        // A new session starts with autocommit on: there is nothing to restore.
        if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
            tx.autocommit_restore = false;
        }
        if !self.tab_tx(tab_id).is_some_and(TabTx::is_open) {
            return;
        }
        if let Some(tx) = self.transactions.tabs.get_mut(&tab_id) {
            tx.lost(reason);
        }
        let (title, connection) = (self.tab_title(tab_id), self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()));
        self.report_warning(Area::Query, connection.as_deref(), format!("The transaction open in {title} was lost: {reason}"));
    }

    /// Runs `report` (which turns the result of a request into what the app shows, and notes a lost
    /// connection) knowing which session the request ran on.
    pub(super) fn transactions_report<R>(&mut self, reporting: Reporting, report: impl FnOnce(&mut Self) -> R) -> R {
        self.transactions.reporting = Some(reporting);
        let result = report(self);
        self.transactions.reporting = None;
        result
    }

    /// The connection's shared session is released: runs held for the cleanup of an auto-commit
    /// run on it are held no longer, and nothing is left to clean up.
    pub(super) fn transactions_released(&mut self, connection: &str) {
        self.transactions.cleaning.remove(connection);
        // The session goes, and the server rolls back with it: nothing to end.
        for tab_id in self.tabs_of_connections(&[connection.to_owned()]) {
            self.transactions.auto_sent.remove(&tab_id);
            self.transactions.auto_open.remove(&tab_id);
        }
    }

    /// A request that ran on the tab's own session `own_id` failed with "not connected": that
    /// session is lost, with its transaction. Nothing happens when the tab has no such session any
    /// more (it was closed, or replaced by a newer one): the request's failure is old news.
    pub(super) fn tab_session_lost(&mut self, tab_id: u64, own_id: Option<u64>) {
        if self.driver_runtime.tab_sessions.get(&tab_id).is_some_and(|own| Some(own.connection_id) == own_id) {
            self.lose_tab_transaction(tab_id, SESSION_LOST);
        }
    }

    /// The failure of a request of `tab_id` says its connection is lost (`request_lost`) or the shared
    /// connection's tunnel is down. The session the request ran on decides what is lost, not whether
    /// the tab has a session now: a request on the tab's own session loses that one only (a down
    /// shared tunnel still marks the shared session), and one on the shared session marks the shared
    /// connection.
    pub(super) fn note_lost(&mut self, tab_id: u64, request_lost: bool, tunnel_down: bool) {
        match self.transactions.reporting.filter(|reporting| reporting.tab_id == tab_id) {
            Some(Reporting { route: TabRoute::Own, own_id, .. }) => {
                if request_lost {
                    self.tab_session_lost(tab_id, own_id);
                }
                if tunnel_down {
                    self.mark_tab_connection_lost(tab_id);
                }
            }
            _ => {
                if request_lost || tunnel_down {
                    self.mark_tab_connection_lost(tab_id);
                }
            }
        }
    }

    // ---- the tab or its connection goes away ----------------------------------------------

    /// The tab closed (or moved to another connection): its session closes, the server rolls back
    /// what was open, and its state is forgotten.
    pub(super) fn tab_transaction_closed(&mut self, tab_id: u64) {
        self.transactions.tabs.remove(&tab_id);
        self.transactions.ending.remove(&tab_id);
        self.transactions.last_end.remove(&tab_id);
        self.transactions.facts.borrow_mut().tabs.remove(&tab_id);
        self.transactions.opening.remove(&tab_id);
        self.transactions.auto_sent.remove(&tab_id);
        self.transactions.auto_open.remove(&tab_id);
        self.transactions.shared_ran.remove(&tab_id);
        self.transactions.revision += 1;
        if let Some(own) = self.driver_runtime.tab_sessions.remove(&tab_id) {
            own.close();
        }
    }

    /// The connection is being closed: the sessions of its tabs go with it (an open transaction is
    /// lost; the user-facing paths asked about it before getting here).
    pub(super) fn connection_transactions_closed(&mut self, connection: &str) {
        let tabs = self.tabs_of_connections(&[connection.to_owned()]);
        for tab_id in tabs {
            // A session still opening for it is closed when it arrives.
            self.transactions.opening.remove(&tab_id);
            self.lose_tab_transaction(tab_id, SESSION_LOST);
        }
    }

    // ---- asking before an open transaction would be lost ----------------------------------

    /// Asks Commit / Roll Back / Cancel when some of `tabs` has statements in a transaction, and runs
    /// `then` once they ended. `true`: the question was opened and the caller stops here.
    /// `consequence` finishes the sentence about what happens to the session. Roll Back is the
    /// default answer (Enter); Escape cancels.
    pub(super) fn guard_transactions(
        &mut self,
        tabs: Vec<u64>,
        consequence: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        let tabs = self.open_transaction_tabs(tabs);
        if tabs.is_empty() {
            return false;
        }
        let now = Instant::now();
        let lines: Vec<String> = tabs
            .iter()
            .filter_map(|id| {
                let tx = self.tab_tx(*id)?;
                Some(format!(
                    "{}: {}, {} modifying, open for {}",
                    self.tab_title(*id),
                    statements_label(tx.statements),
                    tx.modifying,
                    elapsed_label(tx.since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since)))
                ))
            })
            .collect();
        let title = if tabs.len() == 1 {
            "Commit or roll back the open transaction?"
        } else {
            "Commit or roll back the open transactions?"
        };
        let then: std::rc::Rc<RefCell<Option<Then>>> = std::rc::Rc::new(RefCell::new(Some(Box::new(then))));
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (commit_root, rollback_root) = (root.clone(), root.clone());
            let (commit_tabs, rollback_tabs) = (tabs.clone(), tabs.clone());
            let (commit_then, rollback_then) = (then.clone(), then.clone());
            // Roll Back is the default answer: Enter takes it, Escape cancels.
            let roll_back = std::rc::Rc::new(move |cx: &mut gpui::App| {
                let (tabs, then) = (rollback_tabs.clone(), rollback_then.borrow_mut().take());
                let _ = rollback_root.update(cx, |this, cx| this.end_transactions(tabs, TxEnd::Rollback, then, cx));
            });
            let (click_roll_back, enter_roll_back) = (roll_back.clone(), roll_back);
            dialog
                .width(px(460.0))
                .title(title)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(div().debug_selector(|| "tx-guard-tabs".into()).flex().flex_col().children(
                            lines.iter().map(|line| div().child(line.clone())),
                        ))
                        .child(
                            div()
                                .debug_selector(|| "tx-guard-body".into())
                                .text_color(theme::text_muted())
                                .child(consequence),
                        )
                        .child(
                            div().flex().child(
                                Button::new("tx-guard-pending")
                                    .debug_selector(|| "tx-guard-pending".into())
                                    .label("Pending transactions\u{2026}")
                                    .ghost()
                                    .compact()
                                    .on_click({
                                        let root = root.clone();
                                        // Leaves this question (what it was for is not done) for the list.
                                        move |_, window, cx| {
                                            window.close_dialog(cx);
                                            let _ = root.update(cx, |this, cx| this.open_pending_transactions(window, cx));
                                        }
                                    }),
                            ),
                        ),
                )
                .confirm()
                .footer(
                    DialogFooter::new()
                        .child(
                            div()
                                .debug_selector(|| "tx-guard-cancel".into())
                                .child(DialogClose::new().trigger(|button| button.label("Cancel"))),
                        )
                        .child(div().debug_selector(|| "tx-guard-rollback".into()).child(
                            Button::new("tx-guard-rollback").label("Roll Back").primary().on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                click_roll_back(cx);
                            }),
                        ))
                        .child(div().debug_selector(|| "tx-guard-commit".into()).child(
                            Button::new("tx-guard-commit").label("Commit").on_click(
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let (tabs, then) = (commit_tabs.clone(), commit_then.borrow_mut().take());
                                    let _ = commit_root.update(cx, |this, cx| this.end_transactions(tabs, TxEnd::Commit, then, cx));
                                },
                            ),
                        )),
                )
                .on_ok(move |_, _, cx| {
                    enter_roll_back(cx);
                    true
                })
        });
        true
    }

    /// [`Self::guard_transactions`] from code that has no window in hand: the question is opened in
    /// the app's window once the current update is over. `true`: it will be, and the caller stops.
    pub(super) fn guard_tabs_deferred(
        &mut self,
        tabs: Vec<u64>,
        consequence: &'static str,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        if self.open_transaction_tabs(tabs.clone()).is_empty() {
            return false;
        }
        let root = cx.entity().downgrade();
        let then: std::rc::Rc<RefCell<Option<Then>>> = std::rc::Rc::new(RefCell::new(Some(Box::new(then))));
        self.with_ui_window(cx, move |window, app| {
            let _ = root.update(app, |this, cx| {
                let Some(then) = then.borrow_mut().take() else { return };
                // Nothing is open any more: just go on.
                let go_on = std::rc::Rc::new(RefCell::new(Some(then)));
                let later = go_on.clone();
                if !this.guard_transactions(tabs, consequence, window, cx, move |this, window, cx| {
                    if let Some(then) = later.borrow_mut().take() {
                        then(this, window, cx);
                    }
                }) {
                    if let Some(then) = go_on.borrow_mut().take() {
                        then(this, window, cx);
                    }
                }
            });
        })
    }

    /// The tabs of `connections`.
    pub(super) fn tabs_of_connections(&self, connections: &[String]) -> Vec<u64> {
        self.workspace
            .tabs()
            .iter()
            .filter(|tab| connections.contains(&tab.connection_id))
            .map(|tab| tab.id)
            .collect()
    }

    /// [`Self::guard_tabs_deferred`] for every tab of `connections`.
    pub(super) fn guard_connections_deferred(
        &mut self,
        connections: &[String],
        consequence: &'static str,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        let tabs = self.tabs_of_connections(connections);
        self.guard_tabs_deferred(tabs, consequence, cx, then)
    }

    /// Disconnect from the user: asks first when a tab of the connection has a transaction open.
    pub(super) fn request_disconnect(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = self.tabs_of_connections(&[connection.to_owned()]);
        let connection = connection.to_owned();
        let asked = self.guard_transactions(
            tabs,
            "Disconnecting ends their sessions.",
            window,
            cx,
            {
                let connection = connection.clone();
                move |this, _, cx| this.disconnect_connection(&connection, cx)
            },
        );
        if !asked {
            self.disconnect_connection(&connection, cx);
        }
    }

    /// Closing the window or quitting: asks first when any tab has a transaction open. `true`: asked.
    pub(super) fn guard_app_exit(
        &mut self,
        consequence: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        let tabs = self.all_open_transaction_tabs();
        self.guard_transactions(tabs, consequence, window, cx, then)
    }

    /// A saved connection was edited in what its sessions connect with (the endpoint, the
    /// credentials, the tunnel): the sessions of its manual tabs would go on with the old ones.
    /// Every tab that ran a statement is asked about (one still running included); then each
    /// session is closed after its running statement was cancelled and its worker stopped, and the
    /// next statement of the tab opens one on the new settings.
    pub(super) fn connection_edited(&mut self, connection: &str, cx: &mut Context<Self>) {
        // The live session's URI is the old one: a session opened now is resolved from the saved
        // connection (its password from the credential store) on the worker that opens it.
        self.driver_runtime.session_uris.remove(connection);
        let tabs: Vec<u64> = self
            .tabs_of_connections(&[connection.to_owned()])
            .into_iter()
            .filter(|id| self.driver_runtime.tab_sessions.contains_key(id))
            .collect();
        if tabs.is_empty() {
            return;
        }
        let later = tabs.clone();
        let asked = self.guard_tabs_deferred(tabs.clone(), "Saving the new connection settings reopens their sessions.", cx, move |this, _, cx| {
            this.reopen_tab_sessions(later, cx)
        });
        if !asked {
            self.reopen_tab_sessions(tabs, cx);
        }
    }

    /// Closes the sessions of `tabs` (the server rolls back what they left open) and forgets what
    /// they held; the tabs stay in manual mode. A statement still running is cancelled first, and its
    /// worker is waited for: a session is never closed under it.
    fn reopen_tab_sessions(&mut self, tabs: Vec<u64>, cx: &mut Context<Self>) {
        for tab_id in &tabs {
            if self.tab_query_running(*tab_id) {
                self.cancel_tab_query(*tab_id, cx);
            }
            // A session still opening was resolved from the old settings: it is closed on arrival.
            self.transactions.opening.remove(tab_id);
        }
        cx.spawn(async move |this, cx| {
            for tab_id in tabs {
                for _ in 0..WORKER_WAIT_POLLS {
                    let busy = this.update(cx, |this, _| this.own_worker_running(tab_id)).unwrap_or(false);
                    if !busy {
                        break;
                    }
                    smol::Timer::after(WORKER_WAIT_STEP).await;
                }
                let _ = this.update(cx, |this, cx| {
                    if let Some(own) = this.driver_runtime.tab_sessions.remove(&tab_id) {
                        own.close();
                    }
                    if let Some(tx) = this.transactions.tabs.get_mut(&tab_id) {
                        tx.ended();
                    }
                    this.release_driver_page(tab_id);
                    this.notify_transaction(tab_id, cx);
                });
            }
        })
        .detach();
    }

    /// A statement of the tab is running (a script's between its statements included).
    fn tab_query_running(&self, tab_id: u64) -> bool {
        self.workspace.runtime(tab_id).is_some_and(|runtime| !runtime.query.can_run())
    }

    /// A worker is still running something on the tab's own session.
    fn own_worker_running(&self, tab_id: u64) -> bool {
        self.driver_runtime.tab_sessions.get(&tab_id).is_some_and(TabSession::is_running)
    }

    /// Open transactions on the tabs of `connections`, for a delete that has no way back.
    pub(super) fn open_transactions_on(&self, connections: &[String]) -> bool {
        !self.open_transaction_tabs(self.tabs_of_connections(connections)).is_empty()
    }

    // ---- the elapsed time ---------------------------------------------------------------

    fn ensure_tx_ticker(&mut self, cx: &mut Context<Self>) {
        if self.transactions.ticker.is_some() {
            return;
        }
        self.transactions.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if !this.update(cx, |this, cx| this.tx_tick(cx)).unwrap_or(false) {
                break;
            }
        }));
    }

    /// One second passed. Redraws only when a transaction is open on a tab on screen. `false`: no
    /// transaction is open anywhere, the timer stops.
    fn tx_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.transactions.tabs.values().any(TabTx::is_open) {
            self.transactions.ticker = None;
            return false;
        }
        self.roll_back_idle(cx);
        // The pending list shows how long each transaction has been open: it alone is redrawn.
        if let Some(view) = self.transactions.pending_view.as_ref().and_then(|view| view.upgrade()) {
            self.transactions.revision += 1;
            let (rows, revision) = (self.pending_transaction_rows(), self.transactions.revision);
            view.update(cx, |view, cx| view.refresh(rows, revision, cx));
        }
        let visible = self.visible_tab_ids();
        if visible.iter().any(|id| self.transactions.tabs.get(id).is_some_and(TabTx::is_open)) {
            self.notify_pane_views(cx);
        }
        true
    }

    /// Rolls back the tabs whose transaction has changes and sat idle past the limit of their
    /// connection type (when it asks for that). Nothing runs for a tab that is executing.
    pub(super) fn roll_back_idle(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let idle: Vec<(u64, u32)> = self
            .transactions
            .tabs
            .iter()
            .filter(|(_, tx)| tx.has_changes())
            .filter_map(|(id, tx)| {
                let effective = self.effective_for_tab(*id);
                let secs = effective.idle_transaction_secs;
                let due = effective.auto_close_idle_transactions && tx.idle_beyond(Duration::from_secs(u64::from(secs)), now);
                (due && self.end_blocked(*id).is_none()).then_some((*id, secs))
            })
            .collect();
        for (tab_id, secs) in idle {
            let minutes = secs.div_ceil(60);
            let notice = if secs >= 60 {
                format!("Transaction rolled back after being idle {minutes} min")
            } else {
                format!("Transaction rolled back after being idle {secs} s")
            };
            self.end_transactions_noting(vec![tab_id], TxEnd::Rollback, None, Some(notice), false, cx);
        }
    }

    // ---- the toolbar ----------------------------------------------------------------------

    /// Auto-commit / Manual, and in manual mode the state of the transaction with Commit and
    /// Rollback. `None` for a tab that has no commit mode.
    pub(super) fn render_transaction_controls(&self, tab_id: u64, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.is_transactional_tab(tab_id) {
            return None;
        }
        let facts = self.facts(tab_id);
        facts.kind?;
        let mode = self.commit_mode(tab_id);
        let unavailable = facts.unavailable;
        let idle = !self.tab_busy(tab_id);
        let root = cx.entity().downgrade();
        let auto_button = Button::new(("tx-mode-auto", tab_id))
            .debug_selector(|| "tx-mode-auto".into())
            .label("Auto-commit")
            .selected(mode == CommitMode::Auto)
            .disabled(!idle)
            .tooltip(if self.facts(tab_id).smart_commit && self.transactions.shared_ran.contains(&tab_id) {
                AUTO_TOOLTIP_SHARED
            } else {
                "Every statement commits by itself"
            });
        let manual_button = Button::new(("tx-mode-manual", tab_id))
            .debug_selector(|| "tx-mode-manual".into())
            .label("Manual")
            .selected(mode == CommitMode::Manual)
            .disabled(!idle || unavailable.is_some())
            .tooltip(unavailable.unwrap_or(MANUAL_TOOLTIP));
        let group = ButtonGroup::new(("tx-mode", tab_id))
            .outline()
            .compact()
            .with_size(Size::XSmall)
            // `children`, not `child`: the latter overwrites each button's own disabled state.
            .children([auto_button, manual_button])
            .on_click(move |selected, window, app| {
                let next = match selected.first() {
                    Some(0) => CommitMode::Auto,
                    Some(1) => CommitMode::Manual,
                    _ => return,
                };
                let _ = root.update(app, |this, cx| this.set_commit_mode(tab_id, next, window, cx));
            });
        let mut controls = div()
            .debug_selector(|| "tx-controls".into())
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(div().debug_selector(|| "tx-mode".into()).flex_shrink_0().child(group));
        if mode == CommitMode::Manual {
            controls = controls.children(self.render_transaction_state(tab_id, idle, cx));
        }
        // In a narrow toolbar this gives way first (a large shrink factor against its neighbours' basis): the fields and Run beside it keep their room.
        Some(controls.flex_shrink(8.0).min_w(px(0.0)).overflow_hidden().into_any_element())
    }

    /// The Commit and Roll Back tooltips with their shortcuts, built once per settings snapshot (a
    /// toolbar redraws every second while a transaction is open).
    pub(super) fn end_tooltips(&self) -> (gpui::SharedString, gpui::SharedString) {
        let mut cached = self.transactions.tooltips.borrow_mut();
        if !cached.as_ref().is_some_and(|(settings, ..)| Arc::ptr_eq(settings, &self.settings)) {
            #[cfg(test)]
            self.transactions.tooltip_builds.set(self.transactions.tooltip_builds.get() + 1);
            *cached = Some((
                Arc::clone(&self.settings),
                shortcuts::with_label("Commit the transaction", &self.settings.shortcuts, "editor.commit").into(),
                shortcuts::with_label("Roll back the transaction", &self.settings.shortcuts, "editor.rollback").into(),
            ));
        }
        cached.as_ref().map(|(_, commit, rollback)| (commit.clone(), rollback.clone())).expect("just filled")
    }

    /// The state text and the buttons that end the transaction.
    fn render_transaction_state(&self, tab_id: u64, idle: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tx = self.tab_tx(tab_id);
        let open = tx.is_some_and(TabTx::is_open);
        let (text, color, tooltip): (String, gpui::Rgba, Option<&str>) = match tx {
            Some(TabTx { lost: Some(lost), .. }) => (lost.clone(), theme::danger(), None),
            Some(tx) if open => (
                format!(
                    "{} \u{00B7} {} modifying \u{00B7} {}",
                    statements_label(tx.statements),
                    tx.modifying,
                    elapsed_label(tx.since.map_or(Duration::ZERO, |since| Instant::now().saturating_duration_since(since)))
                ),
                // The more it has changed, the louder it gets.
                match tx.modifying {
                    0 => theme::text_muted(),
                    1..=4 => theme::warning(),
                    _ => theme::danger(),
                },
                Some(PRIVATE_TO_TAB),
            ),
            _ => ("No open transaction".into(), theme::text_faint(), None),
        };
        let status = div()
            .id(("tx-status", tab_id))
            .debug_selector(|| "tx-status".into())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.open_transaction_log(tab_id, window, cx)))
            // The text is what gives way in a narrow header (cut with an ellipsis), never the buttons beside it.
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(11.0))
            .text_color(color)
            .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip.unwrap_or(LOG_TOOLTIP)).build(window, cx))
            .child(text)
            .into_any_element();
        let hint = tx.is_some_and(|tx| tx.aborted_hint).then(|| {
            div()
                .id(("tx-hint", tab_id))
                .tooltip(|window, cx| gpui_component::tooltip::Tooltip::new(ABORTED_HINT).build(window, cx))
                .debug_selector(|| "tx-hint".into())
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(11.0))
                .text_color(theme::danger())
                .child(ABORTED_HINT)
                .into_any_element()
        });
        let (commit_tip, rollback_tip) = self.end_tooltips();
        let button = |id: &'static str, label: &'static str, tip: gpui::SharedString| {
            Button::new((id, tab_id)).debug_selector(move || id.into()).label(label).compact().flex_shrink_0().tooltip(tip)
        };
        let commit = button("tx-commit", "Commit", commit_tip)
        .primary()
        .disabled(!open || !idle)
        .on_click(cx.listener(move |this, _, window, cx| this.end_tab_transaction(tab_id, TxEnd::Commit, window, cx)));
        let rollback = button("tx-rollback", "Roll Back", rollback_tip)
        .ghost()
        .disabled(!open || !idle)
        .on_click(cx.listener(move |this, _, window, cx| this.end_tab_transaction(tab_id, TxEnd::Rollback, window, cx)));
        // The hint comes last: in a narrow toolbar it is the one that gives way, not the buttons.
        [Some(status), Some(commit.into_any_element()), Some(rollback.into_any_element()), hint].into_iter().flatten().collect()
    }
}

/// Said when smart commit is on for the connection type but this tab started on the shared session.
const AUTO_TOOLTIP_SHARED: &str = "Every statement commits by itself. Smart commit is on for this connection type, but this tab was \
     already running on the shared session: it applies to editors opened after it was turned on.";

/// What "Manual" does, and who does not see it.
const MANUAL_TOOLTIP: &str = "Commit or roll back yourself: statements run in a transaction on a session of this tab. \
     The AI assistant and MCP clients use sessions of their own: they do not see this tab's uncommitted changes.";

const PRIVATE_TO_TAB: &str =
    "Changes stay private to this tab until you commit. The AI assistant and MCP clients use sessions of their own and do not see them. Click for the statements of the transaction.";

const LOG_TOOLTIP: &str = "Click for the statements of the transaction";

/// The long explanations of the toolbar (a test checks that each says who does not see the tab's
/// uncommitted changes).
#[cfg(test)]
pub(super) fn transaction_tooltips() -> [&'static str; 2] {
    [MANUAL_TOOLTIP, PRIVATE_TO_TAB]
}

#[cfg(test)]
mod implicit_commit_tests {
    use super::{failed_after_implicit_commit, DriverQueryError};

    fn error(text: &str) -> DriverQueryError {
        DriverQueryError::Session(text.into())
    }

    #[test]
    fn a_parse_error_or_an_error_without_a_code_did_not_commit_and_any_other_did() {
        let syntax = "You have an error in your SQL syntax; check the manual near 'TABEL t' at line 1 (error 1064, SQLSTATE 42000)";
        assert!(!failed_after_implicit_commit(&error(syntax)));
        assert!(!failed_after_implicit_commit(&error("Syntax error (error 1149, SQLSTATE 42000)")));
        assert!(failed_after_implicit_commit(&error("Table 't' already exists (error 1050, SQLSTATE 42S01)")));
        assert!(failed_after_implicit_commit(&error("Unknown table 'x' (error 1051, SQLSTATE 42S02)")));
        // Refused before the commit: a bad name, an unknown character set, a read-only server.
        assert!(!failed_after_implicit_commit(&error("Incorrect table name 'a b ' (error 1103, SQLSTATE 42000)")));
        assert!(!failed_after_implicit_commit(&error("Unknown character set: 'utf9' (error 1115, SQLSTATE 42000)")));
        assert!(!failed_after_implicit_commit(&error("The MySQL server is running with the --read-only option (error 1290, SQLSTATE HY000)")));
        // The echo of the statement may hold "(error " too: the driver's suffix is the last one.
        assert!(failed_after_implicit_commit(&error("near '(error 1064, x' (error 1146, SQLSTATE 42S02)")));
        assert!(!failed_after_implicit_commit(&error("Driver session is not connected")));
        assert!(!failed_after_implicit_commit(&DriverQueryError::Cancelled));
    }
}
