//! The app side of a SQL file run: opening the tab, the gates (read-only, Production), the dedicated session and
//! the worker thread of the run, and what follows it.
//!
//! The run uses a second session of the connection, opened for it and closed after, never a tab's own (so a
//! transaction a tab has open is neither joined nor ended) and never the shared one. The window stays free: the
//! worker reports at most every ~120 ms through a channel, and the window reads the counters when it hears.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Context, Entity, Window};

use super::super::data_import::Gate;
use super::super::data_transfer::{order, Namespace, TargetConnection};
use super::super::structure_edit::apply::ApplyControl;
use super::super::*;
use super::executor::DriverExecutor;
use super::run::{self, excerpt, FileRun, Outcome, Progress, Report, Settings};
use super::view::{FileRunView, Sample, SAMPLE_STATEMENTS};
use crate::import::encoding::{CountingReader, Encoding};
use crate::sql_script::{Piece, ScriptError, ScriptReader, Syntax};

/// The most a statement of the sample may hold: the start of a file is not worth megabytes.
const SAMPLE_STATEMENT_LIMIT: usize = 1024 * 1024;

/// Everything a run needs, captured when it is started.
pub(super) struct RunRequest {
    pub(super) tab_id: u64,
    pub(super) target: Namespace,
    pub(super) path: PathBuf,
    pub(super) encoding: Encoding,
    pub(super) settings: Settings,
    /// What the user will see run: for the confirmation of a Production connection.
    pub(super) summary: String,
}

/// Where the wizard starts.
#[derive(Debug, Clone)]
pub(in crate::app) struct FileRunInit {
    pub(in crate::app) target: Namespace,
    /// The file, when it is known (dropped, or a test); else the wizard asks.
    pub(in crate::app) path: Option<PathBuf>,
}

/// The wizard's entity of a SQL file tab.
pub(in crate::app) type FileRunViews = HashMap<u64, Entity<FileRunView>>;

/// What the splitter reads besides `;` in a script for `kind`.
pub(super) fn syntax_for(kind: ConnectionKind) -> Syntax {
    Syntax { delimiter_command: matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb), psql_commands: kind == ConnectionKind::Postgres }
}

/// Why the start of the file could not be read.
#[derive(Debug)]
pub(super) enum SampleError {
    Open { path: PathBuf, source: std::io::Error },
    Read { path: PathBuf, source: std::io::Error },
    Script(ScriptError),
}

impl std::fmt::Display for SampleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { path, source } => write!(f, "Could not open {}: {source}", path.display()),
            Self::Read { path, source } => write!(f, "Could not read {}: {source}", path.display()),
            Self::Script(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SampleError {}

/// The size and the first statements of the file, as the run will read it. Blocking.
pub(super) fn read_sample(path: &Path, encoding: Encoding, kind: ConnectionKind) -> Result<Sample, SampleError> {
    let file = std::fs::File::open(path).map_err(|source| SampleError::Open { path: path.to_owned(), source })?;
    let size = file.metadata().map_err(|source| SampleError::Read { path: path.to_owned(), source })?.len();
    let text = encoding.decode(file).map_err(|source| SampleError::Read { path: path.to_owned(), source })?;
    let mut reader = ScriptReader::with_limits(text, order::session_lex_options(kind, None, None), syntax_for(kind), crate::sql_script::CHUNK, SAMPLE_STATEMENT_LIMIT);
    let mut first = Vec::new();
    let mut more = false;
    loop {
        match reader.next() {
            Ok(Some(_)) if first.len() == SAMPLE_STATEMENTS => {
                more = true;
                break;
            }
            Ok(Some(item)) => first.push(match item.piece {
                Piece::CopyFromStdin(_) => format!("{} (its data lines are left out)", excerpt(item.text)),
                _ => excerpt(item.text),
            }),
            Ok(None) => break,
            Err(ScriptError::StatementTooLarge { .. }) => {
                first.push("A statement of more than 1 MiB".to_owned());
                more = true;
                break;
            }
            Err(error) => return Err(SampleError::Script(error)),
        }
    }
    Ok(Sample { size, first, more })
}

/// Why no SQL file can be run on a connection.
#[derive(Debug)]
pub(in crate::app) enum GateError {
    Gone,
    NotSql,
    /// ClickHouse has no transactions, `DELIMITER` or session the run could set up: not part of this feature yet.
    ClickHouse,
    /// The connection is read-only: the message says why and where to change it.
    ReadOnly(String),
    InMemorySqlite,
    NotOpen,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => f.write_str("This connection no longer exists."),
            Self::NotSql => f.write_str("SQL files run on SQL connections."),
            Self::ClickHouse => f.write_str("Running a SQL file is not available for ClickHouse yet."),
            Self::ReadOnly(message) => f.write_str(message),
            Self::InMemorySqlite => f.write_str("Running a SQL file is off for an in-memory SQLite database: the run would use a second connection that sees a different, empty database."),
            Self::NotOpen => f.write_str("The connection is not open."),
        }
    }
}

impl std::error::Error for GateError {}

impl AppRoot {
    /// What the connection allows, or why no SQL file can be run on it.
    pub(in crate::app) fn file_run_gate(&self, connection: &str) -> Result<Gate, GateError> {
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).ok_or(GateError::Gone)?;
        if crate::sql_ddl::dialect_for(saved.kind).is_none() {
            return Err(GateError::NotSql);
        }
        if saved.kind == ConnectionKind::Clickhouse {
            return Err(GateError::ClickHouse);
        }
        if let Some(message) = self.read_only_connection_message(connection, "Running a SQL file is") {
            return Err(GateError::ReadOnly(message));
        }
        if saved.kind == ConnectionKind::Sqlite && super::super::structure_edit::apply::sqlite_is_in_memory(&saved.uri) {
            return Err(GateError::InMemorySqlite);
        }
        if self.scenario_ui.service.is_none() && self.driver_capabilities(connection, saved.kind.driver_id()).is_none() {
            return Err(GateError::NotOpen);
        }
        let effective = self.effective_for(connection);
        let confirm = effective.confirm_execute || effective.connection_type.as_ref().is_some_and(|ty| ty.is_production());
        Ok(Gate { kind: saved.kind, confirm, subject: self.connection_subject(connection, &effective) })
    }

    /// The connections a file can run on: the open ones, and `current` (it may be about to connect).
    fn file_run_connections(&self, current: &str) -> Vec<TargetConnection> {
        let mut connections = self.transfer_connections();
        connections.retain(|connection| connection.kind != ConnectionKind::Clickhouse);
        if !connections.iter().any(|connection| connection.id == current) {
            if let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(current) && saved.kind != ConnectionKind::Clickhouse) {
                connections.insert(0, TargetConnection { id: saved.id.to_string(), name: saved.name.clone().into(), kind: saved.kind });
            }
        }
        connections
    }

    /// "Run SQL File…" and the editor's "Run File…": opens the wizard on `init`.
    pub(in crate::app) fn open_file_run(&mut self, init: FileRunInit, window: &mut Window, cx: &mut Context<Self>) {
        let gate = match self.file_run_gate(&init.target.connection) {
            Ok(gate) => gate,
            Err(error) => {
                self.sidebar.notice = Some(error.to_string());
                return cx.notify();
            }
        };
        let FileRunInit { target, path } = init;
        let schema = Some(target.schema.as_str()).filter(|schema| !schema.is_empty());
        let tab_id = self.workspace.open_file_run_tab(&target.connection, &target.database, schema, "Run SQL file");
        let connections = self.file_run_connections(&target.connection);
        let mut namespaces = self.transfer_namespaces(&target.connection, &target);
        if !namespaces.contains(&target) {
            namespaces.insert(0, target.clone());
        }
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| FileRunView::new(root, tab_id, gate.kind, target, connections, namespaces, window, cx));
        self.file_runs.insert(tab_id, view.clone());
        self.activate_tab_ui(tab_id, cx);
        // After this update ends: the wizard asks for the file.
        window.defer(cx, move |window, cx| {
            view.update(cx, |view, cx| match path {
                Some(path) => view.set_file(path, window, cx),
                None => view.start(window, cx),
            })
        });
        cx.notify();
    }

    /// The namespace a file is run in when it is asked of a whole connection: the first database the navigator lists
    /// (with its default schema), else none.
    pub(in crate::app) fn file_run_target_of_connection(&self, connection: &str) -> Namespace {
        let database = self
            .navigator
            .loaded_children(&NavigatorNodeId::Connection { connection: connection.to_owned() })
            .into_iter()
            .flatten()
            .find_map(|child| match child {
                NavigatorNodeId::Database { database, .. } => Some(database.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let schema = if database.is_empty() { String::new() } else { self.default_schema_of_database(connection, &database) };
        Namespace { connection: connection.to_owned(), database, schema }
    }

    /// The editor's "Run File…" and the palette's: a file on the connection of the tab `tab_id`, else of the row selected
    /// in the navigator.
    pub(in crate::app) fn run_sql_file_from_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.workspace.active_tab().map(|tab| tab.id);
        self.run_sql_file_from_tab(tab, window, cx);
    }

    /// The namespace an action on "the current connection" works on: that of the tab `tab_id` when it has a connection, else of the
    /// row selected in the navigator. `None` when there is neither.
    pub(in crate::app) fn namespace_from_context(&self, tab_id: Option<u64>) -> Option<Namespace> {
        let tab = tab_id.and_then(|id| self.workspace.tab(id)).filter(|tab| !tab.connection_id.is_empty()).cloned();
        match tab {
            Some(tab) => {
                let known = Namespace { connection: tab.connection_id.clone(), database: tab.database.clone(), schema: tab.schema.clone().unwrap_or_default() };
                Some(if known.database.is_empty() {
                    self.file_run_target_of_connection(&tab.connection_id)
                } else if known.schema.is_empty() {
                    let schema = self.default_schema_of_database(&tab.connection_id, &tab.database);
                    Namespace { schema, ..known }
                } else {
                    known
                })
            }
            None => self.navigator.selected().cloned().and_then(|id| self.object_origin(&id)).map(|origin| Namespace { connection: origin.connection, database: origin.database, schema: origin.schema }),
        }
    }

    /// [`Self::run_sql_file_from_context`] for the tab `tab_id` (the editor's button).
    pub(in crate::app) fn run_sql_file_from_tab(&mut self, tab_id: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.namespace_from_context(tab_id) else {
            self.sidebar.notice = Some("Select a connection, a schema or a table in the navigator first.".to_owned());
            return cx.notify();
        };
        self.open_file_run(FileRunInit { target, path: None }, window, cx);
    }

    /// Whether the run on `tab_id` is going right now.
    pub(in crate::app) fn file_run_running(&self, tab_id: u64, cx: &gpui::App) -> bool {
        // The control is kept from the moment the worker starts, before the wizard hears of it.
        self.file_run_controls.contains_key(&tab_id) || self.file_runs.get(&tab_id).is_some_and(|view| view.read(cx).running())
    }

    /// The tab of a wizard whose target was changed there names the connection and namespace it will run on.
    pub(in crate::app) fn file_run_target_changed(&mut self, tab_id: u64, target: &Namespace, cx: &mut Context<Self>) {
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.connection_id = target.connection.clone();
            tab.database = target.database.clone();
            tab.schema = Some(target.schema.clone()).filter(|schema| !schema.is_empty());
        }
        cx.notify();
    }

    /// A tab that goes away takes its run with it: the statement that runs is interrupted, an open transaction rolled back.
    pub(in crate::app) fn cancel_file_run_of_tab(&mut self, tab_id: u64) {
        if let Some(control) = self.file_run_controls.remove(&tab_id) {
            std::thread::spawn(move || control.cancel());
        }
    }

    pub(super) fn close_file_run_tab(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close_tab(tab_id, window, cx);
    }

    /// Run button: refuses on a connection that went read-only, asks a Production one, then runs.
    pub(super) fn start_file_run(&mut self, request: RunRequest, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = request.tab_id;
        let gate = match self.file_run_gate(&request.target.connection) {
            Ok(gate) => gate,
            Err(error) => return self.file_run_refused(tab_id, error.to_string(), cx),
        };
        if !gate.confirm {
            return self.run_file(request, false, cx);
        }
        let root = cx.entity().downgrade();
        let summary = request.summary.clone();
        let request = std::rc::Rc::new(std::cell::RefCell::new(Some(request)));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root, request) = (root.clone(), request.clone());
            dialog
                .title("Run this SQL file?")
                .child(div().debug_selector(|| "filerun-confirm".into()).child(format!("{summary} will run on {}. The statements in it can change or delete data.", gate.subject)))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Run"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    if let Some(request) = request.borrow_mut().take() {
                        let _ = root.update(cx, |this, cx| this.run_file(request, true, cx));
                    }
                    true
                })
        });
    }

    /// Said to the wizard once the update that asked for the run is over (the wizard is being updated by its own
    /// button right now).
    fn file_run_refused(&mut self, tab_id: u64, message: String, cx: &mut Context<Self>) {
        if let Some(view) = self.file_runs.get(&tab_id).cloned() {
            cx.defer(move |cx| view.update(cx, |view, cx| view.refused(message, cx)));
        }
    }

    /// Opens the session and runs the file on a worker thread. `confirmed`: the user said yes to a confirmation. The
    /// gates are looked at again here, the moment the run starts: the connection may have become read-only, or one
    /// that asks, while a dialog was open.
    fn run_file(&mut self, request: RunRequest, confirmed: bool, cx: &mut Context<Self>) {
        let RunRequest { tab_id, target, path, encoding, settings, .. } = request;
        let Some(view) = self.file_runs.get(&tab_id).cloned() else { return };
        let kind = match self.file_run_gate(&target.connection) {
            Err(error) => return self.file_run_refused(tab_id, error.to_string(), cx),
            Ok(gate) if gate.confirm && !confirmed => {
                let message = format!("{} now asks for a confirmation before it changes anything. Run the file again to confirm it.", gate.subject);
                return self.file_run_refused(tab_id, message, cx);
            }
            Ok(gate) => gate.kind,
        };
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(&target.connection)).cloned() else {
            return self.file_run_refused(tab_id, "This connection no longer exists.".to_owned(), cx);
        };
        let spec = match self.secondary_session_spec(&saved, &target.connection) {
            Ok(spec) => spec,
            Err(message) => return self.file_run_refused(tab_id, message, cx),
        };
        let progress = Arc::new(Progress::default());
        let control = Arc::new(ApplyControl::default());
        // Kept before the worker starts: a tab closed from now on cancels it.
        self.file_run_controls.insert(tab_id, control.clone());
        let started = view.clone();
        let (started_progress, started_control) = (progress.clone(), control.clone());
        cx.defer(move |cx| started.update(cx, |view, cx| view.run_started(started_progress, started_control, cx)));
        let (tick, ticks) = smol::channel::bounded::<()>(1);
        let (worker_progress, worker_control) = (progress.clone(), control.clone());
        let (database, schema) = (Some(target.database.clone()), Some(target.schema.clone()));
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let started = std::time::Instant::now();
                let failed = |message: String| Report::failed(message, started.elapsed());
                let mut executor = match DriverExecutor::open(spec, kind, database, schema.as_deref(), worker_control.clone()) {
                    Ok(executor) => executor,
                    Err(error) => return failed(format!("Could not open a session for the run: {error}")),
                };
                // The server's version decides which versioned comments are code.
                let mut settings = settings;
                if settings.mysql.is_some() {
                    settings.mysql = executor.comments();
                }
                let file = match std::fs::File::open(&path) {
                    Ok(file) => file,
                    Err(error) => return failed(format!("Could not open {}: {error}", path.display())),
                };
                let text = match encoding.decode(CountingReader::new(file, worker_progress.bytes.clone())) {
                    Ok(text) => text,
                    Err(error) => return failed(format!("Could not read {}: {error}", path.display())),
                };
                let lex = order::session_lex_options(kind, executor.dialect(), executor.server_version());
                let mut reader = ScriptReader::new(text, lex, syntax_for(kind));
                let cancelled = || worker_control.is_cancelled();
                let notify = || {
                    let _ = tick.try_send(());
                };
                let job = FileRun { settings, progress: &worker_progress, cancelled: &cancelled, notify: &notify };
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run::run(&job, &mut reader, &mut executor)))
                    .unwrap_or_else(|_| failed("The run stopped unexpectedly. Statements may have been run: check the database before running again.".to_owned()))
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
            let _ = this.update(cx, |this, cx| this.finish_file_run(tab_id, &target, report, cx));
        })
        .detach();
        cx.notify();
    }

    /// The run ended: tell the wizard, the Error Log, and bring the navigator and the open tables in line.
    fn finish_file_run(&mut self, tab_id: u64, target: &Namespace, report: Report, cx: &mut Context<Self>) {
        self.file_run_controls.remove(&tab_id);
        let connection = target.connection.clone();
        match &report.outcome {
            Outcome::Failed(message) => self.report_error(Area::Query, Some(&connection), format!("Running the SQL file failed: {message}")),
            Outcome::Stopped(reason) => self.report_warning(Area::Query, Some(&connection), format!("Running the SQL file stopped: {reason}")),
            Outcome::Completed if report.failed > 0 => self.report_warning(Area::Query, Some(&connection), format!("Running the SQL file: {} statements ran and {} failed.", report.ok, report.failed)),
            Outcome::Cancelled if report.ok > 0 => {
                self.report_warning(Area::Query, Some(&connection), format!("Running the SQL file was cancelled: {} statements had run{}.", report.ok, if report.rolled_back > 0 { ", rolled back" } else { " and stay" }))
            }
            Outcome::Completed | Outcome::Cancelled => {}
        }
        if report.ok > report.rolled_back {
            // A file may have created, dropped or filled anything in its namespace: what the navigator lists there is loaded
            // again where it stands (nothing collapses).
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
        if let Some(view) = self.file_runs.get(&tab_id).cloned() {
            view.update(cx, |view, cx| view.finished(report, cx));
        }
        cx.notify();
    }
}
