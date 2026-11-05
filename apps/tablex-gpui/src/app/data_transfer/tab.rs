//! The app side of a transfer: opening the tab and finding where the data is (the tables the navigator lists, the
//! connections and namespaces it knows), the gates, the dedicated sessions and the worker thread of the run, and what
//! follows it.
//!
//! The run uses two sessions of its own, opened for it and closed after: one reads the source in pages, one writes
//! the target (none for files). Neither is a tab's own session (so a transaction a tab has open is neither joined nor
//! ended) and neither is the shared one. The window stays free: the worker reports at most every ~120 ms through a
//! channel, and the window reads the counters when it hears.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, Entity, Window};

use super::super::data_import::run::{LoadSettings, Outcome};
use super::super::data_import::{DriverWriter, TargetTable};
use super::super::results::copy::SqlTarget;
use super::super::structure_edit::apply::{ApplyControl, TableOrigin};
use super::super::*;
use super::job::{run_jobs, FileSettings, JobPlan, JobResult, JobSink, TransferOutcome, TransferProgress, TransferReport, TransferRun, TransferSettings};
use super::reader::{OpenError, PageFeed, PageReader, Query};
use super::sink::FileFormat;
use super::problem::Problem;
use super::view::{Namespace, TargetConnection, TransferInit, TransferView};
use crate::catalog::CatalogColumn;
use crate::navigator::NodeLoadState;

/// What describing an object found.
#[derive(Debug)]
pub(super) enum Described {
    /// `primary_key`: the names of the key columns, in key order.
    Found { columns: Vec<CatalogColumn>, engine: Option<String>, primary_key: Vec<String> },
    /// The server has no such table.
    Missing,
    Failed(String),
}

/// The tables of a namespace, as the navigator has them.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::app) enum TableNames {
    Loading,
    Loaded(Vec<String>),
    Failed(String),
}

/// Everything a run needs, captured when it is started.
pub(super) struct TransferRequest {
    pub(super) tab_id: u64,
    pub(super) source: Namespace,
    /// The namespace the tables are written to; `None` for files.
    pub(super) target: Option<Namespace>,
    pub(super) jobs: Vec<JobPlan>,
    pub(super) load: LoadSettings,
    pub(super) format: FileFormat,
    pub(super) overwrite: bool,
    /// The dialect a SQL file is written for; `None`: the source connection's own.
    pub(super) sql_dialect: Option<ConnectionKind>,
    /// What the user will see written: for the confirmation of a Production connection.
    pub(super) summary: String,
}

/// The review of the tables a transfer creates, until it is answered.
#[derive(Debug, Clone)]
pub(in crate::app) struct TransferCreate {
    host: u64,
    tab_id: u64,
    /// The job each change of the review stands for.
    jobs: Vec<usize>,
}

/// The wizard's entity of a transfer tab.
pub(in crate::app) type TransferViews = HashMap<u64, Entity<TransferView>>;

impl AppRoot {
    // ---- opening --------------------------------------------------------------------------------------

    /// Why nothing can be transferred out of `connection`, or its kind.
    fn transfer_source_gate(&self, connection: &str) -> Result<ConnectionKind, Problem> {
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).ok_or(Problem::ConnectionGone)?;
        if crate::sql_ddl::dialect_for(saved.kind).is_none() {
            return Err(Problem::NotSql);
        }
        if saved.kind == ConnectionKind::Sqlite && super::super::structure_edit::apply::sqlite_is_in_memory(&saved.uri) {
            return Err(Problem::InMemorySqlite);
        }
        if self.scenario_ui.service.is_none() {
            let capabilities = self.driver_capabilities(connection, saved.kind.driver_id()).ok_or(Problem::NotOpen)?;
            if !capabilities.has(engine_driver_api::Capability::QuerySql) || !capabilities.has(engine_driver_api::Capability::QueryArrowCData) {
                return Err(Problem::CannotPage);
            }
        }
        Ok(saved.kind)
    }

    /// "Transfer Data…" / "Export Data…" and the toolbar's: opens the wizard on `init`.
    pub(in crate::app) fn open_data_transfer(&mut self, init: TransferInit, window: &mut Window, cx: &mut Context<Self>) {
        let kind = match self.transfer_source_gate(&init.source.connection) {
            Ok(kind) => kind,
            Err(problem) => {
                self.sidebar.notice = Some(problem.to_string());
                return cx.notify();
            }
        };
        let dialect = self.sql_dialect_for_connection(&init.source.connection).unwrap_or(crate::sql_table::SqlDialect::for_kind(kind));
        let version = self.driver_runtime.server_versions.get(&init.source.connection).copied();
        let place = if init.source.schema.is_empty() { init.source.database.clone() } else { init.source.schema.clone() };
        let title = match (&init.query, init.tables.as_slice()) {
            (Some(_), _) => "Transfer a query".to_owned(),
            (None, [only]) => format!("Transfer {only}"),
            _ => format!("Transfer from {place}"),
        };
        let schema = Some(init.source.schema.as_str()).filter(|schema| !schema.is_empty());
        let tab_id = self.workspace.open_transfer_tab(&init.source.connection, &init.source.database, schema, &title);
        let connections = self.transfer_connections();
        let folder = Some(self.settings.results.export.folder.trim().to_owned())
            .filter(|folder| !folder.is_empty())
            .or_else(|| crate::platform::exports_dir().map(|dir| dir.display().to_string()))
            .unwrap_or_default();
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| TransferView::new(root, tab_id, init, kind, dialect, version, connections, folder, window, cx));
        self.transfers.insert(tab_id, view.clone());
        self.activate_tab_ui(tab_id, cx);
        // After this update ends: the wizard asks the app for the tables.
        let started = view.clone();
        window.defer(cx, move |window, cx| started.update(cx, |view, cx| view.start(window, cx)));
        cx.notify();
    }

    /// The toolbar's "Data Transfer": from the row selected in the navigator, else from the active tab.
    pub(in crate::app) fn open_transfer_from_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let init = self
            .navigator
            .selected()
            .cloned()
            .and_then(|id| self.transfer_init_for_node(&id, false))
            .or_else(|| {
                let tab = self.workspace.active_tab()?;
                if tab.connection_id.is_empty() {
                    return None;
                }
                let schema = tab.schema.clone().unwrap_or_else(|| self.default_schema_of_database(&tab.connection_id, &tab.database));
                Some(TransferInit {
                    source: Namespace { connection: tab.connection_id.clone(), database: tab.database.clone(), schema },
                    tables: Vec::new(),
                    all: false,
                    to_file: false,
                    query: None,
                })
            });
        match init {
            Some(init) => self.open_data_transfer(init, window, cx),
            None => {
                self.sidebar.notice = Some("Select a connection, a schema or a table in the navigator first.".to_owned());
                cx.notify();
            }
        }
    }

    /// The wizard's start for a navigator row: a table (with the tables selected next to it), or all the tables of a
    /// schema.
    pub(in crate::app) fn transfer_init_for_node(&self, id: &NavigatorNodeId, to_file: bool) -> Option<TransferInit> {
        let origin = self.object_origin(id)?;
        let source = Namespace { connection: origin.connection.clone(), database: origin.database.clone(), schema: origin.schema.clone() };
        let (tables, all) = match id {
            NavigatorNodeId::Object { object, .. } => {
                let mut tables = vec![object.clone()];
                // Several tables of the same schema selected in the navigator travel together.
                let selected = self.navigator.selected_ids();
                if selected.contains(id) {
                    for other in selected {
                        if let NavigatorNodeId::Object { connection, database, schema, object } = other {
                            if *connection == origin.connection && *database == origin.database && *schema == origin.schema && !tables.contains(object) {
                                tables.push(object.clone());
                            }
                        }
                    }
                }
                (tables, false)
            }
            _ => (Vec::new(), true),
        };
        Some(TransferInit { source, tables, all, to_file, query: None })
    }

    /// Whether the result of `tab_id` can be transferred whole: a SQL tab of a connection that can be read in pages.
    pub(in crate::app) fn transfer_available(&self, tab_id: u64) -> bool {
        let Some(tab) = self.workspace.tab(tab_id) else { return false };
        tab.kind == TabKind::Query
            && !tab.connection_id.is_empty()
            && self.saved_connections.iter().find(|saved| saved.matches_id(&tab.connection_id)).is_some_and(|saved| crate::sql_ddl::dialect_for(saved.kind).is_some())
    }

    /// "Whole result…" of a result: the statement the tab shows, or runs from the editor.
    pub(in crate::app) fn open_transfer_from_tab(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else { return };
        let sql = if tab.query_mode.is_table_data() { self.sql_table_statement(&tab).unwrap_or_default() } else { self.explain_target(tab_id, cx) };
        let sql = sql.trim().trim_end_matches(';').trim_end().to_owned();
        if sql.is_empty() {
            self.sidebar.notice = Some("There is no query to transfer.".to_owned());
            return cx.notify();
        }
        let schema = tab.schema.clone().unwrap_or_else(|| self.default_schema_of_database(&tab.connection_id, &tab.database));
        let init = TransferInit {
            source: Namespace { connection: tab.connection_id.clone(), database: tab.database.clone(), schema },
            tables: Vec::new(),
            all: false,
            to_file: true,
            query: Some(sql),
        };
        self.open_data_transfer(init, window, cx);
    }

    /// Whether the transfer on `tab_id` is running right now.
    pub(in crate::app) fn transfer_running(&self, tab_id: u64, cx: &gpui::App) -> bool {
        self.transfers.get(&tab_id).is_some_and(|view| view.read(cx).running())
    }

    pub(super) fn close_transfer_tab(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close_tab(tab_id, window, cx);
    }

    // ---- what the navigator knows -----------------------------------------------------------------------

    /// Asks the navigator to list the tables of `ns` (nothing happens when it has, or is).
    pub(in crate::app) fn transfer_load_tables(&mut self, ns: &Namespace, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let group = navigator::object_group_id(&ns.connection, &ns.database, &ns.schema, ObjectGroup::Tables);
        if let Some(request) = self.navigator.begin_background_load(&group) {
            self.navigator_ui.generation = self.navigator.generation();
            self.ensure_connected_then_load(request, cx);
        }
    }

    /// The tables of `ns` the navigator lists.
    pub(in crate::app) fn transfer_table_names(&self, ns: &Namespace) -> TableNames {
        let group = navigator::object_group_id(&ns.connection, &ns.database, &ns.schema, ObjectGroup::Tables);
        match self.navigator.loaded_children(&group) {
            Some(children) => {
                let mut names: Vec<String> = children
                    .iter()
                    .filter_map(|child| match child {
                        NavigatorNodeId::Object { object, .. } => Some(object.clone()),
                        _ => None,
                    })
                    .collect();
                names.sort_unstable();
                TableNames::Loaded(names)
            }
            None => match self.navigator.load_state(&group) {
                Some(NodeLoadState::Failed(message)) => TableNames::Failed(message.clone()),
                _ => TableNames::Loading,
            },
        }
    }

    /// The SQL connections a transfer can write to: the ones that are open.
    pub(in crate::app) fn transfer_connections(&self) -> Vec<TargetConnection> {
        self.saved_connections
            .iter()
            .filter(|saved| crate::sql_ddl::dialect_for(saved.kind).is_some())
            .filter(|saved| self.scenario_ui.service.is_some() || self.driver_runtime.sessions.contains_key(&saved.id.to_string()))
            .map(|saved| TargetConnection { id: saved.id.to_string(), name: saved.name.clone().into(), kind: saved.kind })
            .collect()
    }

    /// The databases and schemas of `connection` the navigator has loaded, `current` included.
    pub(in crate::app) fn transfer_namespaces(&self, connection: &str, current: &Namespace) -> Vec<Namespace> {
        let mut namespaces: Vec<Namespace> = Vec::new();
        let databases: Vec<String> = self
            .navigator
            .loaded_children(&NavigatorNodeId::Connection { connection: connection.to_owned() })
            .into_iter()
            .flatten()
            .filter_map(|child| match child {
                NavigatorNodeId::Database { database, .. } => Some(database.clone()),
                _ => None,
            })
            .collect();
        for database in databases {
            let id = NavigatorNodeId::Database { connection: connection.to_owned(), database: database.clone() };
            let schemas: Vec<String> = self
                .navigator
                .loaded_children(&id)
                .into_iter()
                .flatten()
                .filter_map(|child| match child {
                    NavigatorNodeId::Schema { schema, .. } => Some(schema.clone()),
                    _ => None,
                })
                .collect();
            if schemas.is_empty() {
                let schema = self.default_schema_of_database(connection, &database);
                namespaces.push(Namespace { connection: connection.to_owned(), database, schema });
            } else {
                namespaces.extend(schemas.into_iter().map(|schema| Namespace { connection: connection.to_owned(), database: database.clone(), schema }));
            }
        }
        if current.connection == connection && !namespaces.contains(current) && !(current.database.is_empty() && current.schema.is_empty()) {
            namespaces.insert(0, current.clone());
        }
        namespaces
    }

    // ---- describing -------------------------------------------------------------------------------------

    /// The columns of the table `origin` names, off the UI thread.
    pub(super) fn describe_for_transfer(&mut self, origin: TableOrigin, cx: &mut Context<Self>) -> gpui::Task<Described> {
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = crate::ui_fixture_properties::table_properties(service.scenario().fixture, &origin.connection, &origin.schema, &origin.object);
            return cx.spawn(async move |_, _| match result {
                Ok(properties) => {
                    let primary_key = primary_key_of(&properties);
                    Described::Found { columns: properties.columns, engine: None, primary_key }
                }
                Err(_) => Described::Missing,
            });
        }
        let key = crate::properties_model::PropertiesKey {
            connection: origin.connection.clone(),
            database: origin.database.clone(),
            schema: origin.schema.clone(),
            object: origin.object.clone(),
            session: self.driver_runtime.connection_ids.get(&origin.connection).copied().unwrap_or(0),
        };
        match self.spawn_describe(&key, cx) {
            Ok(work) => cx.spawn(async move |_, _| match work.await {
                Ok(properties) => {
                    let engine = properties.ddl.as_deref().and_then(super::super::data_import::mapping::engine_of_ddl);
                    let primary_key = primary_key_of(&properties);
                    Described::Found { columns: properties.columns, engine, primary_key }
                }
                Err(error) => describe_failure(&error),
            }),
            Err(error) => cx.spawn(async move |_, _| describe_failure(&error)),
        }
    }

    /// The columns of a query: its first row, read over a session of its own.
    pub(super) fn probe_query(&mut self, connection: &str, query: Query, cx: &mut Context<Self>) -> gpui::Task<Described> {
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).cloned() else {
            return cx.spawn(async move |_, _| Described::Failed("This connection no longer exists.".to_owned()));
        };
        let spec = match self.secondary_session_spec(&saved, connection) {
            Ok(spec) => spec,
            Err(message) => return cx.spawn(async move |_, _| Described::Failed(message)),
        };
        let (read_only, kind) = (self.transfer_read_only(connection, saved.kind), saved.kind);
        cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let mut reader = match PageReader::open(spec, Arc::new(ApplyControl::default()), read_only, kind, query.database.clone()) {
                    Ok(reader) => reader,
                    Err(OpenError(message)) => return Described::Failed(format!("Could not open a session to read the query: {message}")),
                };
                match reader.fetch(&query, 0, 1) {
                    Ok(page) => Described::Found {
                        columns: page.columns.iter().map(|column| CatalogColumn { name: column.name.clone(), data_type: column.data_type.clone(), nullable: true, ..CatalogColumn::default() }).collect(),
                        engine: None,
                        primary_key: Vec::new(),
                    },
                    Err(error) => Described::Failed(error.to_string()),
                }
            })
            .await
        })
    }

    /// Whether the source runs its statements read-only (a query typed by the user cannot write).
    fn transfer_read_only(&self, connection: &str, kind: ConnectionKind) -> bool {
        self.driver_capabilities(connection, kind.driver_id()).is_some_and(|capabilities| capabilities.has(engine_driver_api::Capability::QueryReadOnly))
    }

    // ---- creating the tables ------------------------------------------------------------------------------

    /// Opens the review of the `CreateTable`s of the transfer `tab_id`; applying it creates the tables and tells the
    /// wizard which.
    pub(super) fn open_transfer_create_review(&mut self, tab_id: u64, origin: TableOrigin, changes: Vec<(usize, crate::sql_ddl::Change)>, window: &mut Window, cx: &mut Context<Self>) {
        let (jobs, changes): (Vec<usize>, Vec<crate::sql_ddl::Change>) = changes.into_iter().unzip();
        self.open_object_review(origin, move |_, _| changes, window, cx);
        if let Some(host) = &self.object_action {
            self.transfer_create.insert(tab_id, TransferCreate { host: host.entity_id().as_u64(), tab_id, jobs });
        }
    }

    /// An apply asked from the review `host` ended with the changes `done` applied.
    pub(in crate::app) fn transfer_tables_created(&mut self, host: u64, done: &[usize], cx: &mut Context<Self>) {
        let Some(tab_id) = self.transfer_create.iter().find(|(_, pending)| pending.host == host).map(|(tab_id, _)| *tab_id) else { return };
        let Some(pending) = self.transfer_create.remove(&tab_id) else { return };
        let created: Vec<usize> = done.iter().filter_map(|change| pending.jobs.get(*change).copied()).collect();
        if let Some(view) = self.transfers.get(&pending.tab_id).cloned() {
            view.update(cx, |view, cx| view.tables_created(&created, cx));
        }
    }

    // ---- running ----------------------------------------------------------------------------------------

    /// Start button: refuses on a connection that went read-only, asks a Production one, then runs.
    pub(super) fn start_transfer(&mut self, request: TransferRequest, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = request.tab_id;
        if let Err(problem) = self.transfer_source_gate(&request.source.connection) {
            return self.transfer_refused(tab_id, problem, cx);
        }
        let Some(target) = request.target.clone() else { return self.run_transfer(request, false, cx) };
        let gate = match self.write_gate(&target.connection, "Transferring") {
            Ok(gate) => gate,
            Err(message) => return self.transfer_refused(tab_id, Problem::Refused(message), cx),
        };
        if !gate.confirm {
            return self.run_transfer(request, false, cx);
        }
        let root = cx.entity().downgrade();
        let summary = request.summary.clone();
        let request = std::rc::Rc::new(std::cell::RefCell::new(Some(request)));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root, request) = (root.clone(), request.clone());
            dialog
                .title("Transfer this data?")
                .child(div().debug_selector(|| "transfer-confirm".into()).child(format!("{summary} on {}.", gate.subject)))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Transfer"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    if let Some(request) = request.borrow_mut().take() {
                        let _ = root.update(cx, |this, cx| this.run_transfer(request, true, cx));
                    }
                    true
                })
        });
    }

    /// Said to the wizard once the update that asked for the run is over (the wizard is being updated by its own
    /// button right now).
    fn transfer_refused(&mut self, tab_id: u64, problem: Problem, cx: &mut Context<Self>) {
        if let Some(view) = self.transfers.get(&tab_id).cloned() {
            cx.defer(move |cx| view.update(cx, |view, cx| view.refused(problem.to_string(), cx)));
        }
    }

    /// The dialect a SQL file is written for.
    fn transfer_sql_target(&self, request: &TransferRequest) -> SqlTarget {
        match request.sql_dialect {
            Some(kind) => SqlTarget { dialect: crate::sql_table::SqlDialect::for_kind(kind), binary: crate::sql_table::BinaryLiteral::for_kind(kind) },
            None => SqlTarget {
                dialect: self.sql_dialect_for_connection(&request.source.connection).unwrap_or(crate::sql_table::SqlDialect::POSTGRES),
                binary: self.binary_literal_for_connection(&request.source.connection),
            },
        }
    }

    /// Opens the sessions and runs the jobs on a worker thread. `confirmed`: the user said yes to a confirmation. The
    /// gates are looked at again here, the moment the run starts: the connection may have become read-only, or one
    /// that asks, while a dialog was open.
    fn run_transfer(&mut self, request: TransferRequest, confirmed: bool, cx: &mut Context<Self>) {
        let tab_id = request.tab_id;
        let Some(view) = self.transfers.get(&tab_id).cloned() else { return };
        let mut target_kind = None;
        if let Some(target) = &request.target {
            match self.write_gate(&target.connection, "Transferring") {
                Err(message) => return self.transfer_refused(tab_id, Problem::Refused(message), cx),
                Ok(gate) if gate.confirm && !confirmed => {
                    let message = format!("{} now asks for a confirmation before it changes anything. Start the transfer again to confirm it.", gate.subject);
                    return self.transfer_refused(tab_id, Problem::Refused(message), cx);
                }
                Ok(gate) => target_kind = Some(gate.kind),
            }
        }
        if let Err(problem) = self.transfer_source_gate(&request.source.connection) {
            return self.transfer_refused(tab_id, problem, cx);
        }
        let session_for = |this: &Self, connection: &str| -> Result<(crate::connections::SavedConnection, super::super::structure_edit::apply::SessionSpec), Problem> {
            let saved = this.saved_connections.iter().find(|saved| saved.matches_id(connection)).cloned().ok_or(Problem::ConnectionGone)?;
            let spec = this.secondary_session_spec(&saved, connection).map_err(Problem::Refused)?;
            Ok((saved, spec))
        };
        let (source_saved, source_spec) = match session_for(self, &request.source.connection) {
            Ok(session) => session,
            Err(problem) => return self.transfer_refused(tab_id, problem, cx),
        };
        let target_spec = match &request.target {
            Some(target) => match session_for(self, &target.connection) {
                Ok((_, spec)) => Some(spec),
                Err(problem) => return self.transfer_refused(tab_id, problem, cx),
            },
            None => None,
        };
        let read_only = self.transfer_read_only(&request.source.connection, source_saved.kind);
        let source_kind = source_saved.kind;
        let source_database = request.jobs.first().and_then(|job| job.query.database.clone());
        let same_dialect = target_kind.is_some_and(|kind| super::typemap::same_family(source_kind, kind));
        let settings = TransferSettings {
            same_dialect,
            load: request.load,
            file: FileSettings {
                format: request.format,
                dsv: super::super::results::copy::DsvOptions {
                    column_separator: self.settings.results.export.delimiter.clone(),
                    null_text: self.settings.results.export.null_text.clone(),
                    include_headers: self.settings.results.export.header,
                    ..Default::default()
                },
                overwrite: request.overwrite,
                sql: self.transfer_sql_target(&request),
            },
        };
        let progress = Arc::new(TransferProgress::new(request.jobs.len()));
        let control = Arc::new(ApplyControl::default());
        let (started_progress, started_control) = (progress.clone(), control.clone());
        let started = view.clone();
        cx.defer(move |cx| started.update(cx, |view, cx| view.transfer_started(started_progress, started_control, cx)));
        let (tick, ticks) = smol::channel::bounded::<()>(1);
        let jobs = request.jobs.clone();
        let (worker_progress, worker_control) = (progress.clone(), control.clone());
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let started = std::time::Instant::now();
                let ended = |outcome: TransferOutcome, jobs: &[JobPlan]| TransferReport { outcome, jobs: vec![None; jobs.len()], elapsed: started.elapsed() };
                let cancelled = || worker_control.is_cancelled();
                let notify = || {
                    let _ = tick.try_send(());
                };
                let mut feed = match PageReader::open(source_spec, worker_control.clone(), read_only, source_kind, source_database.clone()) {
                    Ok(feed) => feed,
                    Err(error) => return ended(TransferOutcome::Failed(format!("Could not open a session to read the source: {error}")), &jobs),
                };
                let mut writer = match (target_spec, target_kind) {
                    (Some(spec), Some(kind)) => {
                        let first = jobs.iter().find_map(|job| match &job.sink {
                            JobSink::Table { table, .. } => Some(table.clone()),
                            JobSink::File { .. } => None,
                        });
                        let table = first.unwrap_or(TargetTable { database: String::new(), schema: String::new(), object: String::new() });
                        match DriverWriter::open(spec, table, kind, worker_control.clone()) {
                            Ok(mut writer) => {
                                if let Err(message) = writer.pin_utc() {
                                    return ended(TransferOutcome::Failed(message), &jobs);
                                }
                                writer.learn_packet_limit();
                                Some(writer)
                            }
                            Err(message) => return ended(TransferOutcome::Failed(format!("Could not open a session to write the target: {message}")), &jobs),
                        }
                    }
                    _ => None,
                };
                // The folder of the files exists before the first of them is made.
                for job in &jobs {
                    if let JobSink::File { path, .. } = &job.sink {
                        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                            if let Err(error) = std::fs::create_dir_all(parent) {
                                return ended(TransferOutcome::Failed(format!("Could not create the folder {}: {error}", parent.display())), &jobs);
                            }
                        }
                    }
                }
                let run = TransferRun { jobs: &jobs, settings: &settings, progress: &worker_progress, cancelled: &cancelled, notify: &notify };
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_jobs(&run, &mut feed, writer.as_mut())))
                    .unwrap_or_else(|_| ended(TransferOutcome::Failed("The transfer stopped unexpectedly. Rows may have been added: check the target before trying again.".to_owned()), &jobs))
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
        let target = request.target.clone();
        let planned = request.jobs;
        let source = request.source;
        cx.spawn(async move |this, cx| {
            let report = work.await;
            let _ = this.update(cx, |this, cx| this.finish_transfer(tab_id, &source, target.as_ref(), &planned, report, cx));
        })
        .detach();
        cx.notify();
    }

    /// The run ended: tell the wizard, the Error Log, and bring the open tabs of the tables in line.
    fn finish_transfer(&mut self, tab_id: u64, source: &Namespace, target: Option<&Namespace>, planned: &[JobPlan], report: TransferReport, cx: &mut Context<Self>) {
        let (area, connection) = (if target.is_some() { Area::Mutation } else { Area::Export }, source.connection.clone());
        match &report.outcome {
            TransferOutcome::Failed(message) => self.report_error(area, Some(&connection), format!("The transfer failed: {message}")),
            TransferOutcome::Stopped(reason) => self.report_warning(area, Some(&connection), format!("The transfer stopped: {reason}")),
            TransferOutcome::Cancelled => self.report_warning(area, Some(&connection), "The transfer was cancelled."),
            TransferOutcome::Completed => {
                for (plan, result) in planned.iter().zip(&report.jobs) {
                    let Some(JobResult { report: job, .. }) = result else { continue };
                    match &job.outcome {
                        Outcome::Failed(reason) | Outcome::Stopped(reason) => self.report_error(area, Some(&connection), format!("{} was not transferred: {reason}", plan.label)),
                        Outcome::Completed if job.errors_total > 0 => {
                            self.report_warning(area, Some(&connection), format!("{}: {} rows were written and {} skipped.", plan.label, job.inserted, job.errors_total))
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(target) = target {
            for (plan, result) in planned.iter().zip(&report.jobs) {
                if let (JobSink::Table { table, .. }, Some(result)) = (&plan.sink, result) {
                    if result.report.inserted > 0 {
                        let origin = TableOrigin { connection: target.connection.clone(), database: table.database.clone(), schema: table.schema.clone(), object: table.object.clone() };
                        self.refresh_table_tabs_after_apply(&origin, cx);
                    }
                }
            }
        }
        if let Some(view) = self.transfers.get(&tab_id).cloned() {
            view.update(cx, |view, cx| view.finished(report, cx));
        }
        cx.notify();
    }
}

/// The key columns of a described table in key order: the primary key constraint's own order, else the columns flagged.
fn primary_key_of(properties: &crate::catalog::TableProperties) -> Vec<String> {
    let declared = properties.keys.iter().flatten().find(|key| key.kind == crate::catalog::CatalogConstraintKind::PrimaryKey).map(|key| key.columns.clone());
    declared.filter(|columns| !columns.is_empty()).unwrap_or_else(|| properties.columns.iter().filter(|column| column.primary_key).map(|column| column.name.clone()).collect())
}

/// What a failed describe means: the server has no such table, or something else is wrong.
fn describe_failure(error: &super::super::results::properties::DescribeError) -> Described {
    use super::super::results::properties::DescribeError;
    match error {
        DescribeError::Driver(engine_driver_host::LoaderError::DriverRejected { code, .. }) if code == engine_driver_api::sql::codes::DESCRIBE_NOT_FOUND => Described::Missing,
        other => Described::Failed(crate::sessions::redact_secrets(&other.to_string())),
    }
}
