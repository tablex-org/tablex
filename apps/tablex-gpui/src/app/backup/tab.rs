//! The app side of a backup or restore: opening the tab, the gates (read-only, Production), the worker thread of the run and
//! what follows it.
//!
//! The run happens on a worker thread; the window stays free. The worker tells at most every ~120 ms through a channel and the
//! window reads the counters when it hears. Backup is a read and is allowed on a read-only connection; a restore is a write: a
//! read-only connection refuses it and a Production one asks first, and both are looked at again the moment the run starts.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, Entity, Window};

use super::super::data_import::Gate;
use super::super::data_transfer::{Namespace, TargetConnection};
use super::super::*;
use super::model::Mode;
use super::view::{BackupInit, BackupView};
use crate::backup::command::PgRestoreOptions;
use crate::backup::job::{self, BackupJob, DumpOptions, Operation, Outcome, Report, RestoreJob, ToolConnection};
use crate::backup::process::{CancelToken, RunState};
use crate::backup::toc::Toc;
use crate::backup::tools::Locator;

/// How long closing the window or quitting waits for the tools to be stopped and cleaned up after.
const BACKUP_STOP_STEP: std::time::Duration = std::time::Duration::from_millis(50);
const BACKUP_STOP_POLLS: u32 = 100;
/// The platform lets a quit wait 200 ms: shorter than that.
const BACKUP_QUIT_STEP: std::time::Duration = std::time::Duration::from_millis(10);
const BACKUP_QUIT_POLLS: u32 = 15;

/// Counts a worker thread of a backup or restore for as long as it lives (what a quit waits for).
struct WorkerGuard(Arc<std::sync::atomic::AtomicUsize>);

impl WorkerGuard {
    fn new(count: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What a run does.
pub(super) enum Work {
    Backup { options: DumpOptions, output: PathBuf },
    Restore { archive: PathBuf, options: PgRestoreOptions, selection: Option<(Arc<Toc>, HashSet<u32>)> },
}

impl Work {
    fn mode(&self) -> Mode {
        match self {
            Self::Backup { .. } => Mode::Backup,
            Self::Restore { .. } => Mode::Restore,
        }
    }
}

/// Everything a run needs, captured when it is started.
pub(super) struct RunRequest {
    pub(super) tab_id: u64,
    pub(super) target: Namespace,
    pub(super) work: Work,
    /// What the user will see run: for the confirmation of a Production connection.
    pub(super) summary: String,
}

/// The wizard's entity of a backup or restore tab.
pub(in crate::app) type BackupViews = HashMap<u64, Entity<BackupView>>;

/// Why a connection cannot be backed up or restored.
#[derive(Debug)]
pub(in crate::app) enum GateError {
    Gone,
    /// Only PostgreSQL, MySQL and MariaDB have native tools here.
    Unsupported(ConnectionKind),
    /// A restore on a read-only connection: the message says why and where to change it.
    ReadOnly(String),
    NotOpen,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => f.write_str("This connection no longer exists."),
            Self::Unsupported(ConnectionKind::Sqlite) => f.write_str("Backup uses the server's own tools, which SQLite does not have: copy the database file instead."),
            Self::Unsupported(_) => f.write_str("Backup and restore use the native tools of PostgreSQL, MySQL and MariaDB."),
            Self::ReadOnly(message) => f.write_str(message),
            Self::NotOpen => f.write_str("The connection is not open."),
        }
    }
}

impl std::error::Error for GateError {}

/// Whether a native backup tool exists for `kind`.
pub(in crate::app) fn tools_exist(kind: ConnectionKind) -> bool {
    matches!(kind, ConnectionKind::Postgres | ConnectionKind::Mysql | ConnectionKind::Mariadb)
}

impl AppRoot {
    /// Whether the connection can be backed up (its kind has native tools).
    pub(in crate::app) fn backup_supported(&self, connection: &str) -> bool {
        self.saved_connections.iter().find(|saved| saved.matches_id(connection)).is_some_and(|saved| tools_exist(saved.kind))
    }

    /// What the connection allows, or why it cannot be backed up (`Mode::Backup`) or restored into.
    pub(in crate::app) fn backup_gate(&self, connection: &str, mode: Mode) -> Result<Gate, GateError> {
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).ok_or(GateError::Gone)?;
        if !tools_exist(saved.kind) {
            return Err(GateError::Unsupported(saved.kind));
        }
        if mode == Mode::Restore {
            if let Some(message) = self.read_only_connection_message(connection, "Restoring a backup is") {
                return Err(GateError::ReadOnly(message));
            }
        }
        if self.scenario_ui.service.is_none() && !self.driver_runtime.sessions.contains_key(connection) {
            return Err(GateError::NotOpen);
        }
        let effective = self.effective_for(connection);
        // A backup only reads: it never asks.
        let confirm = mode == Mode::Restore && (effective.confirm_execute || effective.connection_type.as_ref().is_some_and(|ty| ty.is_production()));
        Ok(Gate { kind: saved.kind, confirm, subject: self.connection_subject(connection, &effective) })
    }

    /// The connections a backup can be made of: the open ones whose server has native tools, and `current` (it may be about to connect).
    fn backup_connections(&self, current: &str) -> Vec<TargetConnection> {
        let mut connections = self.transfer_connections();
        connections.retain(|connection| tools_exist(connection.kind));
        if !connections.iter().any(|connection| connection.id == current) {
            if let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(current) && tools_exist(saved.kind)) {
                connections.insert(0, TargetConnection { id: saved.id.to_string(), name: saved.name.clone().into(), kind: saved.kind });
            }
        }
        connections
    }

    /// The major version of the server of `connection` (PostgreSQL's `pg_dump` may not be older), when it told.
    pub(in crate::app) fn backup_server_major(&self, connection: &str) -> Option<u32> {
        self.driver_runtime.server_versions.get(connection).map(|version| version / 10_000)
    }

    /// "Backup…" / "Restore…": opens the wizard on `init`.
    pub(in crate::app) fn open_backup(&mut self, init: BackupInit, window: &mut Window, cx: &mut Context<Self>) {
        let gate = match self.backup_gate(&init.target.connection, init.mode) {
            Ok(gate) => gate,
            Err(error) => {
                self.sidebar.notice = Some(error.to_string());
                return cx.notify();
            }
        };
        let target = init.target.clone();
        let schema = Some(target.schema.as_str()).filter(|schema| !schema.is_empty());
        let title = format!("{} {}", init.mode.title(), if target.database.is_empty() { "database" } else { target.database.as_str() });
        let tab_id = self.workspace.open_backup_tab(init.mode == Mode::Restore, &target.connection, &target.database, schema, &title);
        let connections = self.backup_connections(&target.connection);
        let mut namespaces = self.transfer_namespaces(&target.connection, &target);
        if !namespaces.contains(&target) {
            namespaces.insert(0, target.clone());
        }
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| BackupView::new(root, tab_id, init, gate.kind, connections, namespaces, window, cx));
        self.backups.insert(tab_id, view.clone());
        self.activate_tab_ui(tab_id, cx);
        // After this update ends: the wizard reads its lists, or asks for the backup to restore.
        window.defer(cx, move |window, cx| view.update(cx, |view, cx| view.start(window, cx)));
        cx.notify();
    }

    /// "Backup…" / "Restore…" on a navigator row: the namespace of the row, and for a table row that table.
    pub(in crate::app) fn open_backup_for_node(&mut self, mode: Mode, id: &NavigatorNodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(origin) = self.object_origin(id) else { return };
        let tables = Some(origin.object.clone()).filter(|table| !table.is_empty()).into_iter().collect();
        let target = Namespace { connection: origin.connection, database: origin.database, schema: origin.schema };
        self.open_backup(BackupInit { mode, target, tables, archive: None }, window, cx);
    }

    /// "Backup…" / "Restore…" on a connection's own menu.
    pub(in crate::app) fn open_backup_for_connection(&mut self, mode: Mode, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut target = self.file_run_target_of_connection(connection);
        // The navigator has not listed the databases yet (the connection was never expanded): the one the address names.
        if target.database.is_empty() {
            target.database = self.address_database(connection);
        }
        self.open_backup(BackupInit { mode, target, tables: Vec::new(), archive: None }, window, cx);
    }

    /// The database a connection's saved address names (it holds no password), or none.
    fn address_database(&self, connection: &str) -> String {
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(connection)) else { return String::new() };
        crate::backup::endpoint::Endpoint::parse(saved.kind, &saved.uri).map(|endpoint| endpoint.database).unwrap_or_default()
    }

    /// The File menu's and the palette's: the connection of the active tab, else of the row selected in the navigator.
    pub(in crate::app) fn open_backup_from_context(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.workspace.active_tab().map(|tab| tab.id);
        match self.namespace_from_context(tab) {
            Some(target) => self.open_backup(BackupInit { mode, target, tables: Vec::new(), archive: None }, window, cx),
            None => {
                self.sidebar.notice = Some("Select a connection, a schema or a table in the navigator first.".to_owned());
                cx.notify();
            }
        }
    }

    /// Whether the run on `tab_id` is going right now.
    pub(in crate::app) fn backup_running(&self, tab_id: u64, cx: &gpui::App) -> bool {
        // The token is kept from the moment the worker starts, before the wizard hears of it.
        self.backup_controls.contains_key(&tab_id) || self.backups.get(&tab_id).is_some_and(|view| view.read(cx).running())
    }

    /// The tab of a wizard whose target was changed there names the connection and namespace it will use.
    pub(in crate::app) fn backup_target_changed(&mut self, tab_id: u64, target: &Namespace, cx: &mut Context<Self>) {
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.connection_id = target.connection.clone();
            tab.database = target.database.clone();
            tab.schema = Some(target.schema.clone()).filter(|schema| !schema.is_empty());
        }
        cx.notify();
    }

    /// A tab that goes away takes its run with it: the tool and what it started are stopped, an unfinished file removed.
    pub(in crate::app) fn cancel_backup_of_tab(&mut self, tab_id: u64) {
        if let Some(cancel) = self.backup_controls.remove(&tab_id) {
            std::thread::spawn(move || cancel.cancel());
        }
    }

    /// Closing the window or quitting while a backup or a restore runs: asks first. `true`: asked, and `then` runs once the
    /// answer is Discard and the tools have been stopped.
    pub(in crate::app) fn guard_backups(&mut self, consequence: &'static str, window: &mut Window, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static) -> bool {
        if self.backup_controls.is_empty() {
            return false;
        }
        let count = self.backup_controls.len();
        let then = std::rc::Rc::new(std::cell::RefCell::new(Some(then)));
        self.open_discard_dialog(
            if count == 1 { "Cancel the running backup or restore?".to_owned() } else { format!("Cancel the {count} running backups and restores?") },
            format!("{consequence} The tool is stopped and an unfinished backup file is removed; what a restore had already restored stays in the database."),
            move |this, window, cx| {
                if let Some(then) = then.borrow_mut().take() {
                    this.stop_backups_then(window, cx, then);
                }
            },
            window,
            cx,
        );
        true
    }

    /// Cancels every running backup and restore, waits (a few seconds at most) for the workers to remove their private files and
    /// unfinished backups, then runs `then`.
    fn stop_backups_then(&mut self, window: &mut Window, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static) {
        self.cancel_all_backups();
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..BACKUP_STOP_POLLS {
                if this.update(cx, |this, _| this.backup_controls.is_empty()).unwrap_or(true) {
                    break;
                }
                cx.background_executor().timer(BACKUP_STOP_STEP).await;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                // A worker that did not answer in time is left to finish on its own: it must not be asked about again.
                this.backup_controls.clear();
                then(this, window, cx);
            });
        })
        .detach();
    }

    /// Cancels every run. On Unix a cancel is a signal and is sent right here, so it has been sent before a quit goes on; on Windows
    /// it runs `taskkill` and waits for it, so it is sent off the UI thread.
    fn cancel_all_backups(&self) {
        let tokens: Vec<Arc<CancelToken>> = self.backup_controls.values().cloned().collect();
        if tokens.is_empty() {
            return;
        }
        #[cfg(unix)]
        tokens.iter().for_each(|cancel| cancel.cancel());
        #[cfg(not(unix))]
        std::thread::spawn(move || tokens.iter().for_each(|cancel| cancel.cancel()));
    }

    /// The application is quitting (with or without a window): the private directories of the runs are removed right here, on the
    /// calling thread, so that no password file stays in the temporary folder however little time the quit leaves; every tool is
    /// stopped; and the quit waits (the little the platform gives it) for the workers to have cleaned up the rest. The future
    /// touches no app state: the app is borrowed while it is polled.
    pub(in crate::app) fn backups_stop_for_quit(&mut self, _cx: &mut Context<Self>) -> impl std::future::Future<Output = ()> + 'static {
        // Cancelled first, so that a worker still connecting sees the cancel and makes no password file.
        self.cancel_all_backups();
        self.backup_dirs.remove_all();
        let workers = self.backup_workers.clone();
        async move {
            for _ in 0..BACKUP_QUIT_POLLS {
                if workers.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    break;
                }
                smol::Timer::after(BACKUP_QUIT_STEP).await;
            }
        }
    }

    pub(super) fn close_backup_tab(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close_tab(tab_id, window, cx);
    }

    /// "Open in Run SQL File": the script of a restore goes through the file runner, prefilled, and the restore tab is done.
    pub(super) fn open_restore_script(&mut self, tab_id: u64, target: Namespace, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.file_run_gate(&target.connection) {
            if let Some(view) = self.backups.get(&tab_id).cloned() {
                view.update(cx, |view, cx| view.refused(error.to_string(), cx));
            }
            return;
        }
        self.open_file_run(super::super::file_run::FileRunInit { target, path: Some(path) }, window, cx);
        self.close_tabs_now(&[tab_id], None, cx);
        cx.notify();
    }

    /// What a second connection of the tool needs: the saved connection, the open session's own address (a password typed and not
    /// saved is only there) and the tunnel's settings. The credential store is read later, on the worker.
    fn tool_connection(&self, connection: &str) -> Result<ToolConnection, GateError> {
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).cloned().ok_or(GateError::Gone)?;
        Ok(ToolConnection { live_uri: self.driver_runtime.session_uris.get(connection).cloned(), tunnel: self.secondary_tunnel_access(connection), saved })
    }

    /// Run: refuses a restore on a connection that went read-only, asks a Production one, then runs.
    pub(super) fn start_backup_run(&mut self, request: RunRequest, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = request.tab_id;
        let mode = request.work.mode();
        let gate = match self.backup_gate(&request.target.connection, mode) {
            Ok(gate) => gate,
            Err(error) => return self.backup_refused(tab_id, error.to_string(), cx),
        };
        if !gate.confirm {
            return self.run_backup_job(request, false, cx);
        }
        let root = cx.entity().downgrade();
        let summary = request.summary.clone();
        let request = std::rc::Rc::new(std::cell::RefCell::new(Some(request)));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root, request) = (root.clone(), request.clone());
            dialog
                .title("Restore this backup?")
                .child(div().debug_selector(|| "backup-confirm".into()).child(format!("{summary}. Objects it holds are created, and with the drop option replace the ones that exist, on {}.", gate.subject)))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Restore"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    if let Some(request) = request.borrow_mut().take() {
                        let _ = root.update(cx, |this, cx| this.run_backup_job(request, true, cx));
                    }
                    true
                })
        });
    }

    /// Said to the wizard once the update that asked for the run is over (the wizard is being updated by its own button right now).
    fn backup_refused(&mut self, tab_id: u64, message: String, cx: &mut Context<Self>) {
        if let Some(view) = self.backups.get(&tab_id).cloned() {
            cx.defer(move |cx| view.update(cx, |view, cx| view.refused(message, cx)));
        }
    }

    /// Starts the tool on a worker thread. `confirmed`: the user said yes to a confirmation. The gates are looked at again here, the
    /// moment the run starts: the connection may have become read-only, or one that asks, while a dialog was open.
    fn run_backup_job(&mut self, request: RunRequest, confirmed: bool, cx: &mut Context<Self>) {
        let RunRequest { tab_id, target, work, .. } = request;
        let Some(view) = self.backups.get(&tab_id).cloned() else { return };
        let mode = work.mode();
        match self.backup_gate(&target.connection, mode) {
            Err(error) => return self.backup_refused(tab_id, error.to_string(), cx),
            Ok(gate) if gate.confirm && !confirmed => {
                let message = format!("{} now asks for a confirmation before it changes anything. Restore again to confirm it.", gate.subject);
                return self.backup_refused(tab_id, message, cx);
            }
            Ok(_) => {}
        }
        let connection = match self.tool_connection(&target.connection) {
            Ok(connection) => connection,
            Err(error) => return self.backup_refused(tab_id, error.to_string(), cx),
        };
        let (paths, server_major) = (self.settings.query.tool_paths(), self.backup_server_major(&target.connection));
        let database = target.database.clone();
        let dirs = self.backup_dirs.clone();
        let state = RunState::new();
        let cancel = Arc::new(CancelToken::default());
        // Kept before the worker starts: a tab closed from now on cancels it.
        self.backup_controls.insert(tab_id, cancel.clone());
        let started = view.clone();
        let (started_state, started_cancel) = (state.clone(), cancel.clone());
        cx.defer(move |cx| started.update(cx, |view, cx| view.run_started(started_state, started_cancel, cx)));
        let (tick, ticks) = smol::channel::bounded::<()>(1);
        let (worker_state, worker_cancel) = (state.clone(), cancel.clone());
        let alive = WorkerGuard::new(&self.backup_workers);
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                // Counted until the job has removed its private files and its unfinished backup.
                let _alive = alive;
                let started = std::time::Instant::now();
                let notify = || {
                    let _ = tick.try_send(());
                };
                let locator = Locator::system();
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match work {
                    Work::Backup { options, output } => job::run_backup(BackupJob { connection, database, options, output, paths, server_major, dirs }, &locator, &worker_state, &worker_cancel, &notify),
                    Work::Restore { archive, options, selection } => {
                        let selection = selection.map(|(toc, wanted)| ((*toc).clone(), wanted));
                        job::run_restore(RestoreJob { connection, database, archive, options, selection, paths, server_major, dirs }, &locator, &worker_state, &worker_cancel, &notify)
                    }
                }))
                .unwrap_or_else(|_| Report::panicked(if mode == Mode::Backup { Operation::Backup } else { Operation::Restore }, started))
            })
            .await
        });
        let ticking = view.downgrade();
        cx.spawn(async move |_, cx| {
            while ticks.recv().await.is_ok() {
                if ticking.update(cx, |view, cx| view.tick(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let report = work.await;
            let _ = this.update(cx, |this, cx| this.finish_backup_run(tab_id, &target, report, cx));
        })
        .detach();
        cx.notify();
    }

    /// The run ended: tell the wizard and the Error Log, and after a restore bring the navigator and the open tables in line.
    fn finish_backup_run(&mut self, tab_id: u64, target: &Namespace, report: Report, cx: &mut Context<Self>) {
        self.backup_controls.remove(&tab_id);
        let connection = target.connection.clone();
        let (verb, noun) = match report.operation {
            Operation::Backup => ("Backup", "backup"),
            Operation::Restore => ("Restore", "restore"),
        };
        match &report.outcome {
            Outcome::Failed(message) => self.report_error(Area::Admin, Some(&connection), format!("{verb} of {} failed: {message}", target.database)),
            Outcome::Cancelled => self.report_warning(Area::Admin, Some(&connection), format!("The {noun} of {} was cancelled.", target.database)),
            Outcome::Completed => {}
        }
        // The tool may have created, dropped or filled anything before it stopped: what the navigator lists is loaded again where it
        // stands (nothing collapses).
        if report.operation == Operation::Restore && report.command.is_some() {
            let namespace = [
                NavigatorNodeId::Connection { connection: connection.clone() },
                NavigatorNodeId::Database { connection: connection.clone(), database: target.database.clone() },
                NavigatorNodeId::Schema { connection: connection.clone(), database: target.database.clone(), schema: target.schema.clone() },
            ];
            let groups = ObjectGroup::ALL.into_iter().map(|group| navigator::object_group_id(&connection, &target.database, &target.schema, group));
            for node in namespace.into_iter().chain(groups) {
                self.reload_navigator_node(&node, cx);
            }
            self.invalidate_table_properties(&NavigatorNodeId::Schema { connection: connection.clone(), database: target.database.clone(), schema: target.schema.clone() });
            let open: Vec<u64> = self
                .workspace
                .tabs()
                .iter()
                .filter(|tab| tab.query_mode.is_table_data() && tab.connection_id == connection && !self.workspace.tab_has_staged_changes(tab.id))
                .map(|tab| tab.id)
                .collect();
            for id in open {
                self.rerun_shown_result(id, false, cx);
            }
        }
        if let Some(view) = self.backups.get(&tab_id).cloned() {
            view.update(cx, |view, cx| view.finished(report, cx));
        }
        cx.notify();
    }
}
