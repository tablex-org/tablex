//! The wizard of a transfer: what is copied (Source), where it goes (Target), which column goes where (Mapping), and
//! the run (Run), as one entity the tab renders. The state machine and the actions are here; `panels` draws it.
//!
//! The wizard is a tab, not a dialog: it lists the tables of a schema, maps the columns of each, and runs for
//! minutes. Reading the catalog is always off the UI thread, one table after the other, and the widgets of a table's
//! mapping are made when it is selected and its rows drawn, so a schema of hundreds of tables costs the widgets of the
//! one on screen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui::{Context, SharedString, Subscription, WeakEntity, Window};
use gpui_component::input::InputEvent;
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::IndexPath;

use super::super::data_import::mapping::{self, NewColumn};
use super::super::data_import::run::{LoadSettings, OnError, TransactionMode};
use super::super::structure_edit::apply::{ApplyControl, TableOrigin};
use super::super::structure_edit::EditScope;
use super::super::*;
use super::job::{JobSink, TransferProgress, TransferReport};
use super::problem::Problem;
use super::model::{self, Action, Job, Loaded, SourceColumn, TargetDesc};
use super::order::{self, OrderColumn, RowOrder};
use super::reader::Query;
use super::sink::FileFormat;
use super::tab::{Described, TableNames};
use crate::sql_ddl::{DdlDialect, TableId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Source,
    Target,
    Mapping,
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceMode {
    Tables,
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetKind {
    Table,
    File,
}

/// A schema (or database) of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Namespace {
    pub(in crate::app) connection: String,
    pub(in crate::app) database: String,
    /// `""` for an engine without schemas.
    pub(in crate::app) schema: String,
}

impl Namespace {
    pub(in crate::app) fn label(&self) -> SharedString {
        if self.schema.is_empty() || self.schema == self.database {
            self.database.clone().into()
        } else {
            format!("{}.{}", self.database, self.schema).into()
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn label_for_test(&self) -> String {
        self.label().to_string()
    }

    pub(in crate::app) fn origin(&self, object: &str) -> TableOrigin {
        TableOrigin { connection: self.connection.clone(), database: self.database.clone(), schema: self.schema.clone(), object: object.to_owned() }
    }
}

/// Where the wizard starts.
#[derive(Debug, Clone)]
pub(in crate::app) struct TransferInit {
    pub(in crate::app) source: Namespace,
    /// Tables selected from the start.
    pub(in crate::app) tables: Vec<String>,
    /// All the tables of the namespace are selected from the start.
    pub(in crate::app) all: bool,
    pub(in crate::app) to_file: bool,
    /// Starts on a query instead of the tables.
    pub(in crate::app) query: Option<String>,
}

pub(super) struct SourceTable {
    pub(super) name: String,
    pub(super) selected: bool,
}

/// A connection a transfer can write to.
#[derive(Debug, Clone)]
pub(in crate::app) struct TargetConnection {
    pub(in crate::app) id: String,
    pub(in crate::app) name: SharedString,
    pub(in crate::app) kind: ConnectionKind,
}

type SelectEntity = gpui::Entity<SelectState<Vec<SharedString>>>;

/// The widgets of the mapping of the selected table, made when it is selected.
pub(super) struct JobWidgets {
    pub(super) name: gpui::Entity<InputState>,
    pub(super) action: SelectEntity,
    /// The select of each source column (`Append`), made when its row is drawn.
    pub(super) map_selects: Vec<Option<SelectEntity>>,
    /// `(skip)` and the names of the target columns: what every select lists.
    pub(super) map_items: Vec<SharedString>,
    /// The name and type inputs of each column of a table to create, made when its row is drawn.
    pub(super) new_inputs: Vec<Option<(gpui::Entity<InputState>, gpui::Entity<InputState>)>>,
    _subscriptions: Vec<Subscription>,
    pub(super) columns_subscriptions: Vec<Subscription>,
}

/// A transfer in flight.
pub(super) struct RunningTransfer {
    pub(super) progress: Arc<TransferProgress>,
    pub(super) control: Arc<ApplyControl>,
    pub(super) started: Instant,
    pub(super) cancelling: bool,
}

pub(in crate::app) struct TransferView {
    pub(super) root: WeakEntity<AppRoot>,
    pub(super) tab_id: u64,
    pub(super) step: Step,

    // ---- source
    pub(super) source: Namespace,
    pub(super) source_kind: ConnectionKind,
    pub(super) source_dialect: crate::sql_table::SqlDialect,
    /// The source server's version (`major * 10_000 + minor * 100 + patch`), when the session reported one.
    pub(super) source_version: Option<u32>,
    pub(super) mode: SourceMode,
    pub(super) mode_select: SelectEntity,
    pub(super) tables: Vec<SourceTable>,
    pub(super) listing: Loaded<()>,
    /// Indexes into `tables` of those that match the filter.
    pub(super) shown: Vec<usize>,
    pub(super) filter: gpui::Entity<InputState>,
    pub(super) tables_scroll: gpui::UniformListScrollHandle,
    pub(super) query: gpui::Entity<TextareaState>,
    /// Tables to select when the list arrives.
    preselect: Vec<String>,
    preselect_all: bool,

    // ---- target
    pub(super) target_kind: TargetKind,
    pub(super) kind_select: SelectEntity,
    pub(super) connections: Vec<TargetConnection>,
    pub(super) connection_select: SelectEntity,
    pub(super) target: Namespace,
    pub(super) target_conn_kind: ConnectionKind,
    pub(super) namespaces: Vec<Namespace>,
    pub(super) namespace_select: SelectEntity,
    pub(super) format: FileFormat,
    pub(super) format_select: SelectEntity,
    pub(super) folder: gpui::Entity<InputState>,
    pub(super) file_name: gpui::Entity<InputState>,
    pub(super) overwrite: bool,
    pub(super) sql_dialect_select: SelectEntity,

    // ---- mapping
    pub(super) jobs: Vec<Job>,
    pub(super) selected: usize,
    pub(super) jobs_scroll: gpui::UniformListScrollHandle,
    pub(super) columns_scroll: gpui::UniformListScrollHandle,
    pub(super) widgets: HashMap<usize, JobWidgets>,
    /// What the jobs were built for: going back and forward again changes nothing in them unless this changed.
    jobs_key: Option<String>,
    /// The generation of the reading of the columns: a newer one (the jobs were built again) drops an older answer.
    describing: u64,
    /// The name of the target each job was last looked up under: leaving the field unchanged looks nothing up.
    last_described: HashMap<usize, String>,

    // ---- run
    pub(super) batch_size: gpui::Entity<InputState>,
    pub(super) transaction_select: SelectEntity,
    pub(super) on_error_select: SelectEntity,
    /// The tables of the run in the order of its plan: what the progress and the summary list.
    pub(super) run_labels: Vec<SharedString>,
    pub(super) run: Option<RunningTransfer>,
    pub(super) report: Option<TransferReport>,
    /// The job whose errors the summary lists.
    pub(super) result_selected: usize,
    pub(super) results_scroll: gpui::UniformListScrollHandle,
    pub(super) errors_scroll: gpui::UniformListScrollHandle,
    /// The tables to create were created (their review was applied).
    pub(super) message: Option<String>,
    _subscriptions: Vec<Subscription>,
}

pub(super) const SOURCE_MODES: [&str; 2] = ["Tables of a schema", "The result of a SQL query"];
pub(super) const TARGET_KINDS: [&str; 2] = ["Tables of a database", "Files"];
pub(super) const TRANSACTIONS: [&str; 2] = ["Commit every batch", "One transaction per table"];
pub(super) const ON_ERRORS: [&str; 2] = ["Stop at the first error", "Skip the row or table and log it"];
/// The dialects a SQL file can be written for, after "this connection's".
pub(super) const SQL_DIALECTS: [(&str, ConnectionKind); 4] = [
    ("PostgreSQL", ConnectionKind::Postgres),
    ("MySQL / MariaDB", ConnectionKind::Mysql),
    ("SQLite", ConnectionKind::Sqlite),
    ("ClickHouse", ConnectionKind::Clickhouse),
];

fn select_of(labels: Vec<SharedString>, selected: usize, window: &mut Window, cx: &mut Context<TransferView>) -> SelectEntity {
    cx.new(|cx| SelectState::new(labels, Some(IndexPath::default().row(selected)), window, cx))
}

fn input(placeholder: &'static str, value: &str, window: &mut Window, cx: &mut Context<TransferView>) -> gpui::Entity<InputState> {
    let value = value.to_owned();
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
}

fn labels<const N: usize>(texts: [&'static str; N]) -> Vec<SharedString> {
    texts.into_iter().map(SharedString::from).collect()
}

/// Whether two namespaces are the same place on the same connection.
/// A query that mentions the name of the table it writes to reads what it writes, whatever namespace it names it in: the name is all
/// the text shows (a search path or a default database fills in the rest), so any query of the same connection is refused.
fn query_reads_its_target(source: &Namespace, target: &Namespace, sql: &str, name: &str) -> bool {
    source.connection == target.connection && order::query_mentions(sql, name)
}

fn same_place(a: &Namespace, b: &Namespace) -> bool {
    a.connection == b.connection && a.database == b.database && a.schema == b.schema
}

impl TransferView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        root: WeakEntity<AppRoot>,
        tab_id: u64,
        init: TransferInit,
        source_kind: ConnectionKind,
        source_dialect: crate::sql_table::SqlDialect,
        source_version: Option<u32>,
        connections: Vec<TargetConnection>,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_text = init.query.clone().unwrap_or_default();
        let mode = if init.query.is_some() { SourceMode::Query } else { SourceMode::Tables };
        let connection_labels: Vec<SharedString> = connections.iter().map(|connection| connection.name.clone()).collect();
        let current = connections.iter().position(|connection| connection.id == init.source.connection).unwrap_or(0);
        let mode_select = select_of(labels(SOURCE_MODES), usize::from(mode == SourceMode::Query), window, cx);
        let kind_select = select_of(labels(TARGET_KINDS), usize::from(init.to_file), window, cx);
        let connection_select = select_of(connection_labels, current, window, cx);
        let namespace_select = select_of(vec![init.source.label()], 0, window, cx);
        let format_select = select_of(FileFormat::ALL.iter().map(|format| format.label().into()).collect(), 0, window, cx);
        let sql_dialect_select = select_of(std::iter::once("This connection's".into()).chain(SQL_DIALECTS.iter().map(|(label, _)| (*label).into())).collect(), 0, window, cx);
        let transaction_select = select_of(labels(TRANSACTIONS), 0, window, cx);
        let on_error_select = select_of(labels(ON_ERRORS), 0, window, cx);
        let query = cx.new(|cx| TextareaState::new(window, cx).auto_grow(6, 14).placeholder("SELECT \u{2026}").default_value(query_text));
        let mut view = Self {
            root,
            tab_id,
            step: Step::Source,
            source: init.source.clone(),
            source_kind,
            source_dialect,
            source_version,
            mode,
            mode_select,
            tables: Vec::new(),
            listing: Loaded::Pending,
            shown: Vec::new(),
            filter: input("Filter tables", "", window, cx),
            tables_scroll: gpui::UniformListScrollHandle::new(),
            query,
            preselect: init.tables,
            preselect_all: init.all,
            target_kind: if init.to_file { TargetKind::File } else { TargetKind::Table },
            kind_select,
            target: init.source.clone(),
            target_conn_kind: connections.get(current).map_or(source_kind, |connection| connection.kind),
            connections,
            connection_select,
            namespaces: vec![init.source],
            namespace_select,
            format: FileFormat::Csv,
            format_select,
            folder: input("Folder", &folder, window, cx),
            file_name: input("File name", "", window, cx),
            overwrite: false,
            sql_dialect_select,
            jobs: Vec::new(),
            selected: 0,
            jobs_scroll: gpui::UniformListScrollHandle::new(),
            columns_scroll: gpui::UniformListScrollHandle::new(),
            widgets: HashMap::new(),
            jobs_key: None,
            describing: 0,
            last_described: HashMap::new(),
            batch_size: input("2000", "2000", window, cx),
            transaction_select,
            on_error_select,
            run_labels: Vec::new(),
            run: None,
            report: None,
            result_selected: 0,
            results_scroll: gpui::UniformListScrollHandle::new(),
            errors_scroll: gpui::UniformListScrollHandle::new(),
            message: None,
            _subscriptions: Vec::new(),
        };
        view.wire(window, cx);
        view
    }

    fn wire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(&self.mode_select, window, |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                this.mode = if select.read(cx).selected_index(cx).is_some_and(|index| index.row == 1) { SourceMode::Query } else { SourceMode::Tables };
                this.message = None;
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe_in(&self.kind_select, window, |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                this.target_kind = if select.read(cx).selected_index(cx).is_some_and(|index| index.row == 1) { TargetKind::File } else { TargetKind::Table };
                this.message = None;
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe_in(&self.connection_select, window, |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, window, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                this.choose_connection(row, window, cx);
            }
        }));
        subscriptions.push(cx.subscribe_in(&self.namespace_select, window, |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                if let Some(namespace) = this.namespaces.get(row) {
                    this.target = namespace.clone();
                    this.message = None;
                    cx.notify();
                }
            }
        }));
        subscriptions.push(cx.subscribe_in(&self.format_select, window, |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, window, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                this.format = FileFormat::ALL.get(row).copied().unwrap_or(FileFormat::Csv);
                this.message = None;
                this.default_file_name(window, cx);
                cx.notify();
            }
        }));
        for select in [&self.sql_dialect_select, &self.transaction_select, &self.on_error_select] {
            subscriptions.push(cx.subscribe(select, |this: &mut Self, _, _: &SelectEvent<Vec<SharedString>>, cx| {
                this.message = None;
                cx.notify();
            }));
        }
        subscriptions.push(cx.subscribe_in(&self.filter, window, |this: &mut Self, _, event: &InputEvent, window, cx| match event {
            InputEvent::Change => {
                this.refilter(cx);
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.next(window, cx),
            _ => {}
        }));
        subscriptions.push(cx.subscribe_in(&self.query, window, |this: &mut Self, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) && this.message.take().is_some() {
                cx.notify();
            }
        }));
        for state in [&self.folder, &self.file_name, &self.batch_size] {
            subscriptions.push(cx.subscribe_in(state, window, |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change if this.message.take().is_some() => cx.notify(),
                InputEvent::PressEnter { .. } => this.next(window, cx),
                _ => {}
            }));
        }
        self._subscriptions = subscriptions;
    }

    pub(in crate::app) fn running(&self) -> bool {
        self.run.is_some()
    }

    // ---- the source ----------------------------------------------------------------------------------

    /// Starts reading the list of tables.
    pub(super) fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.source.clone();
        let _ = self.root.update(cx, |root, cx| root.transfer_load_tables(&source, cx));
        self.poll_tables(cx);
        self.default_file_name(window, cx);
    }

    /// The list of tables, once the navigator has it.
    pub(super) fn poll_tables(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.listing, Loaded::Pending) {
            return;
        }
        let source = self.source.clone();
        let Ok(names) = self.root.read_with(cx, |root, _| root.transfer_table_names(&source)) else { return };
        match names {
            TableNames::Loading => {}
            TableNames::Failed(message) => self.listing = Loaded::Failed(message),
            TableNames::Loaded(names) => {
                let wanted = std::mem::take(&mut self.preselect);
                let all = self.preselect_all;
                self.tables = names.into_iter().map(|name| SourceTable { selected: all || wanted.contains(&name), name }).collect();
                self.listing = Loaded::Ready(());
                self.refilter(cx);
            }
        }
    }

    pub(super) fn refilter(&mut self, cx: &gpui::App) {
        let needle = self.filter.read(cx).value().trim().to_lowercase();
        self.shown = self.tables.iter().enumerate().filter(|(_, table)| needle.is_empty() || table.name.to_lowercase().contains(&needle)).map(|(index, _)| index).collect();
    }

    pub(super) fn set_selected(&mut self, table: usize, selected: bool, cx: &mut Context<Self>) {
        if let Some(table) = self.tables.get_mut(table) {
            table.selected = selected;
        }
        self.message = None;
        cx.notify();
    }

    /// Selects (or clears) every table that matches the filter.
    pub(super) fn select_shown(&mut self, selected: bool, cx: &mut Context<Self>) {
        for index in self.shown.clone() {
            self.tables[index].selected = selected;
        }
        self.message = None;
        cx.notify();
    }

    pub(super) fn selected_tables(&self) -> usize {
        self.tables.iter().filter(|table| table.selected).count()
    }

    /// The lexer the source server reads its statements with: its dialect's preset moved by what the session reported and, for MySQL
    /// and MariaDB, by its version (a `/*!50700 … */` comment is code to a server of 5.7 and a comment to one of 5.6).
    fn source_lex(&self) -> engine_driver_kit::sql::SqlLexOptions {
        order::dialect_lex_options(self.source_kind, self.source_dialect, self.source_version)
    }

    pub(super) fn query_text(&self, cx: &gpui::App) -> String {
        // The `;`s and comments after the last token would end, or swallow the end of, the statement the driver wraps to page it.
        order::strip_tail(self.query.read(cx).value().as_ref(), &self.source_lex()).to_owned()
    }

    // ---- the target ------------------------------------------------------------------------------------

    fn choose_connection(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connections.get(row).cloned() else { return };
        let namespaces = self.root.read_with(cx, |root, _| root.transfer_namespaces(&connection.id, &self.source)).unwrap_or_default();
        self.target_conn_kind = connection.kind;
        self.target = namespaces.first().cloned().unwrap_or(Namespace { connection: connection.id.clone(), database: String::new(), schema: String::new() });
        let labels: Vec<SharedString> = namespaces.iter().map(Namespace::label).collect();
        self.namespaces = namespaces;
        let select = self.namespace_select.clone();
        select.update(cx, |select, cx| {
            select.set_items(labels, window, cx);
            select.set_selected_index(Some(IndexPath::default().row(0)), window, cx);
        });
        self.message = None;
        cx.notify();
    }

    /// The name a file gets while the user has not typed one: the table's (or `query`) with the format's extension.
    pub(super) fn default_file_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stem = self.tables.iter().find(|table| table.selected).map_or("query".to_owned(), |table| table.name.clone());
        let name = format!("{}.{}", super::sink::safe_file_stem(&stem), self.format.extension());
        self.file_name.update(cx, |state, cx| state.set_value(name, window, cx));
    }

    pub(super) fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Choose folder".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let folder = match prompt.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.message = Some("Could not open the folder picker.".to_owned());
                        cx.notify();
                    });
                    None
                }
            };
            if let Some(folder) = folder {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.folder.update(cx, |state, cx| state.set_value(folder.display().to_string(), window, cx));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn set_overwrite(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        self.overwrite = overwrite;
        self.message = None;
        cx.notify();
    }

    // ---- the steps -------------------------------------------------------------------------------------

    pub(super) fn dialect(&self) -> Option<&'static dyn DdlDialect> {
        crate::sql_ddl::dialect_for(self.target_conn_kind)
    }

    pub(super) fn can_leave_source(&self, cx: &gpui::App) -> bool {
        match self.mode {
            SourceMode::Tables => self.selected_tables() > 0,
            SourceMode::Query => !self.query_text(cx).is_empty(),
        }
    }

    pub(super) fn can_leave_mapping(&self) -> bool {
        self.jobs.iter().any(Job::active) && self.jobs.iter().all(Job::ready) && model::duplicate_targets(&self.jobs).is_empty() && !self.jobs.iter().any(|job| self.onto_itself(job))
    }

    /// Reading `job`'s table and writing it back to itself would read the rows it just wrote.
    pub(super) fn onto_itself(&self, job: &Job) -> bool {
        if !(job.active() && self.target_kind == TargetKind::Table) {
            return false;
        }
        if job.source_table.is_empty() {
            // A query that reads the target's name reads what it writes, whatever namespace it names it in: the name is all the
            // text shows (a search path or a default database fills in the rest), so a query of the same connection is refused.
            query_reads_its_target(&self.source, &self.target, &job.query.sql, &job.target_name)
        } else {
            // A table is read back as it is written.
            same_place(&self.source, &self.target) && job.source_table.eq_ignore_ascii_case(&job.target_name)
        }
    }

    /// Why `job` may not go where it goes.
    pub(super) fn onto_itself_message(&self, job: &Job) -> String {
        if job.source_table.is_empty() {
            format!("The query reads from \"{}\", the table it would write to: give the target another name.", job.target_name)
        } else {
            format!("\"{}\" would be copied onto itself: give the target another name or namespace.", job.label)
        }
    }

    pub(in crate::app) fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        self.message = None;
        match self.step {
            Step::Source if self.can_leave_source(cx) => {
                // The target namespaces the navigator has loaded, for the connection already chosen.
                let row = self.connection_select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                let current = self.target.clone();
                self.choose_connection(row, window, cx);
                if let Some(position) = self.namespaces.iter().position(|namespace| *namespace == current) {
                    self.target = current;
                    self.namespace_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(position)), window, cx));
                }
                self.step = Step::Target;
            }
            Step::Target => match self.leave_target(cx) {
                Ok(()) => {
                    self.step = Step::Mapping;
                    // The mapping the user made stays while what it was made for does.
                    let key = self.plan_key(cx);
                    if self.jobs_key.as_ref() != Some(&key) || self.jobs.is_empty() {
                        self.build_jobs(cx);
                        self.jobs_key = Some(key);
                    }
                }
                Err(problem) => self.message = Some(problem.to_string()),
            },
            Step::Mapping if self.can_leave_mapping() => {
                self.step = Step::Run;
            }
            Step::Mapping => self.message = Some(self.mapping_problem()),
            _ => {}
        }
        cx.notify();
    }

    pub(in crate::app) fn back(&mut self, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        self.step = match self.step {
            Step::Run => Step::Mapping,
            Step::Mapping => Step::Target,
            _ => Step::Source,
        };
        self.message = None;
        self.report = None;
        cx.notify();
    }

    /// What the jobs are for: the source, and where the rows go.
    fn plan_key(&self, cx: &gpui::App) -> String {
        let source = match self.mode {
            SourceMode::Query => format!("query:{}", self.query_text(cx)),
            SourceMode::Tables => format!("tables:{}", self.tables.iter().filter(|table| table.selected).map(|table| table.name.as_str()).collect::<Vec<_>>().join("\u{1f}")),
        };
        let target = match self.target_kind {
            TargetKind::Table => format!("table:{}:{}:{}", self.target.connection, self.target.database, self.target.schema),
            TargetKind::File => "file".to_owned(),
        };
        format!("{source}|{target}")
    }

    fn leave_target(&self, cx: &gpui::App) -> Result<(), Problem> {
        match self.target_kind {
            TargetKind::Table => {
                if self.dialect().is_none() {
                    return Err(Problem::NoTargetConnection);
                }
                if self.target.database.is_empty() && self.target.schema.is_empty() {
                    return Err(Problem::NoTargetNamespace);
                }
                Ok(())
            }
            TargetKind::File => {
                if self.folder.read(cx).value().trim().is_empty() {
                    return Err(Problem::NoFolder);
                }
                if self.single_file() && self.file_name.read(cx).value().trim().is_empty() {
                    return Err(Problem::NoFileName);
                }
                Ok(())
            }
        }
    }

    /// One table (or a query) is exported: its file has a name of its own to choose.
    pub(super) fn single_file(&self) -> bool {
        match self.mode {
            SourceMode::Query => true,
            SourceMode::Tables => self.selected_tables() == 1,
        }
    }

    fn mapping_problem(&self) -> String {
        let duplicates = model::duplicate_targets(&self.jobs);
        if let Some(name) = duplicates.first() {
            return format!("Two tables go to \"{name}\": give one another name, or leave it out.");
        }
        if let Some(job) = self.jobs.iter().find(|job| self.onto_itself(job)) {
            return self.onto_itself_message(job);
        }
        if let Some(job) = self.jobs.iter().find(|job| matches!(job.columns, Loaded::Failed(_))) {
            return format!("\"{}\" cannot be read.", job.label);
        }
        if !self.jobs.iter().any(Job::active) {
            return "Every table is left out: there is nothing to transfer.".to_owned();
        }
        "The columns of the tables are still being read.".to_owned()
    }

    // ---- the jobs ---------------------------------------------------------------------------------------

    /// One job per table (or the query), and the reading of their columns.
    fn build_jobs(&mut self, cx: &mut Context<Self>) {
        let database = Some(self.source.database.clone()).filter(|database| !database.is_empty());
        self.jobs = match self.mode {
            SourceMode::Query => {
                let sql = self.query_text(cx);
                let mut job = Job::new("Query", String::new(), Query::new(database, sql));
                job.target_name = "query_result".to_owned();
                vec![job]
            }
            SourceMode::Tables => self
                .tables
                .iter()
                .filter(|table| table.selected)
                .map(|table| {
                    let sql = crate::sql_table::base_select(self.source_dialect, Some(self.source.schema.as_str()), &table.name).unwrap_or_default();
                    Job::new(table.name.clone(), table.name.clone(), Query::new(database.clone(), sql))
                })
                .collect(),
        };
        self.selected = 0;
        self.widgets.clear();
        self.last_described.clear();
        self.report = None;
        self.describe_jobs(cx);
    }

    /// Reads the columns of each source table (and of the table it goes to, when that exists), one after the other.
    fn describe_jobs(&mut self, cx: &mut Context<Self>) {
        self.describing += 1;
        let generation = self.describing;
        let count = self.jobs.len();
        cx.spawn(async move |this, cx| {
            for index in 0..count {
                let Ok(task) = this.update(cx, |this, cx| this.source_task(index, generation, cx)) else { return };
                let Some(task) = task else { continue };
                let described = task.await;
                let Ok(next) = this.update(cx, |this, cx| this.source_described(index, generation, described, cx)) else { return };
                if let Some((task, epoch)) = next {
                    let described = task.await;
                    if this.update(cx, |this, cx| this.target_described(index, epoch, described, cx)).is_err() {
                        return;
                    }
                }
            }
        })
        .detach();
    }

    fn source_task(&mut self, index: usize, generation: u64, cx: &mut Context<Self>) -> Option<gpui::Task<Described>> {
        if self.describing != generation {
            return None;
        }
        let job = self.jobs.get(index)?;
        let (origin, query) = (self.source.origin(&job.source_table), job.query.clone());
        let table = !job.source_table.is_empty();
        self.root.update(cx, |root, cx| if table { root.describe_for_transfer(origin, cx) } else { root.probe_query(&origin.connection, query, cx) }).ok()
    }

    /// The columns of a source table are known (or why not). `Some`: the target has to be described, and this is how.
    fn source_described(&mut self, index: usize, generation: u64, described: Described, cx: &mut Context<Self>) -> Option<(gpui::Task<Described>, u64)> {
        if self.describing != generation {
            return None;
        }
        let kind_from = self.source_kind;
        let lex = self.source_lex();
        let job = self.jobs.get_mut(index)?;
        match described {
            Described::Found { columns, primary_key, .. } => {
                let columns: Vec<SourceColumn> = columns.iter().map(SourceColumn::from).collect();
                let dialect = self.source_dialect;
                let kind = self.source_kind;
                let order_columns: Vec<OrderColumn> = columns.iter().map(|column| OrderColumn { name: column.name.clone(), data_type: column.data_type.clone() }).collect();
                if job.source_table.is_empty() {
                    // A query: wrapped, and ordered by its output columns, unless it has an order of its own.
                    let (sql, order) = order::query_statement(kind, &lex, &job.query.sql, &order_columns, |name| dialect.checked_quote(name).ok());
                    job.query.sql = sql;
                    job.query.order = order;
                } else {
                    // A table: in the order of its key, which the server reads from its index. A unique key whose columns can be
                    // compared through a literal also lets each page start after the last key of the one before (see `reader`);
                    // a table without one is ordered by every column the server can order. The key of a ClickHouse table is a
                    // sorting key, not a unique one.
                    let unique = kind != ConnectionKind::Clickhouse;
                    let select = crate::sql_table::base_select(dialect, Some(self.source.schema.as_str()), &job.source_table).unwrap_or_default();
                    let target = super::super::results::copy::SqlTarget { dialect, binary: crate::sql_table::BinaryLiteral::for_kind(kind) };
                    let key_pairs: Vec<(String, String)> = primary_key.iter().filter_map(|name| columns.iter().find(|column| column.name == *name)).map(|column| (column.name.clone(), column.data_type.clone())).collect();
                    match order::table_order(kind, &order_columns, &primary_key, unique, |name| dialect.checked_quote(name).ok()) {
                        Some((list, order)) => {
                            job.query.sql = format!("{select} ORDER BY {list}");
                            job.query.order = order;
                        }
                        None => {
                            job.query.sql = select.clone();
                            job.query.order = RowOrder::Unordered("no column can be ordered");
                        }
                    }
                    job.query.keyset = super::reader::Keyset::for_table(&select, &key_pairs, unique, target, kind);
                    if job.query.keyset.is_some() {
                        job.query.order = RowOrder::Key;
                    }
                    job.source_key = primary_key;
                }
                job.columns = Loaded::Ready(columns);
            }
            Described::Missing => job.columns = Loaded::Failed("The table does not exist (any more).".to_owned()),
            Described::Failed(message) => job.columns = Loaded::Failed(message),
        }
        self.widgets.remove(&index);
        cx.notify();
        let (target_kind, to) = (self.target_kind, self.target_conn_kind);
        let job = self.jobs.get_mut(index)?;
        if target_kind == TargetKind::File || job.columns.ready().is_none() {
            return None;
        }
        // Does the table exist? The navigator knows when it has listed the namespace; otherwise the server is asked.
        let target = self.target.clone();
        let name = job.target_name.clone();
        self.last_described.insert(index, name.clone());
        let known = self.root.read_with(cx, |root, _| root.transfer_table_names(&target)).ok();
        let job = self.jobs.get_mut(index)?;
        match known {
            Some(TableNames::Loaded(names)) if !names.contains(&name) => {
                job.exists = Some(false);
                job.create_as(kind_from, to);
                self.widgets.remove(&index);
                None
            }
            _ => {
                job.epoch += 1;
                let epoch = job.epoch;
                let origin = target.origin(&name);
                self.root.update(cx, |root, cx| root.describe_for_transfer(origin, cx)).ok().map(|task| (task, epoch))
            }
        }
    }

    /// The table the rows go to was looked for: it exists (append to it) or not (create it). An answer for a name
    /// that was typed over since is dropped.
    fn target_described(&mut self, index: usize, epoch: u64, described: Described, cx: &mut Context<Self>) {
        let (from, to) = (self.source_kind, self.target_conn_kind);
        let Some(job) = self.jobs.get_mut(index) else { return };
        if job.epoch != epoch {
            return;
        }
        match described {
            Described::Found { columns, engine, .. } => {
                job.exists = Some(true);
                job.append_to(TargetDesc { columns: columns.iter().map(mapping::TargetColumn::from).collect(), engine });
            }
            Described::Missing => {
                job.exists = Some(false);
                job.create_as(from, to);
            }
            Described::Failed(message) => {
                job.exists = None;
                job.target = Loaded::Failed(message);
                job.action = Action::Append;
            }
        }
        self.widgets.remove(&index);
        cx.notify();
    }

    /// The table the job goes to changed (its name, or the action chosen): look again.
    pub(super) fn redescribe_target(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.target_kind != TargetKind::Table || self.jobs.get(index).is_none_or(|job| job.columns.ready().is_none()) {
            return;
        }
        let origin = self.target.origin(&self.jobs[index].target_name);
        let Ok(task) = self.root.update(cx, |root, cx| root.describe_for_transfer(origin, cx)) else { return };
        let job = &mut self.jobs[index];
        job.epoch += 1;
        let epoch = job.epoch;
        job.target = Loaded::Pending;
        job.exists = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let described = task.await;
            let _ = this.update(cx, |this, cx| this.target_described(index, epoch, described, cx));
        })
        .detach();
    }

    // ---- the mapping --------------------------------------------------------------------------------------

    /// The box of a table in the list: transferred, or left out.
    pub(super) fn toggle_job(&mut self, index: usize, include: bool, cx: &mut Context<Self>) {
        let Some(job) = self.jobs.get_mut(index) else { return };
        job.action = match (include, job.exists, job.target.ready().is_some()) {
            (false, ..) => Action::Leave,
            (true, Some(true), true) => Action::Append,
            (true, ..) if job.new_columns.is_empty() && self.target_kind == TargetKind::Table => Action::Create,
            (true, ..) => if job.exists == Some(true) { Action::Append } else { Action::Create },
        };
        self.widgets.remove(&index);
        self.message = None;
        cx.notify();
    }

    pub(super) fn select_job(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.jobs.len() && self.selected != index {
            self.selected = index;
            cx.notify();
        }
    }

    /// The widgets of the selected job's mapping, made on first use (and again after what they show changed).
    pub(super) fn ensure_widgets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.selected;
        if self.widgets.contains_key(&index) {
            return;
        }
        let Some(job) = self.jobs.get(index) else { return };
        let name = input("Table name", &job.target_name, window, cx);
        let action = select_of(Action::ALL.iter().map(|action| action.label().into()).collect(), Action::ALL.iter().position(|action| *action == job.action).unwrap_or(0), window, cx);
        let map_items: Vec<SharedString> = match job.target.ready() {
            Some(target) => std::iter::once("(skip)".into()).chain(target.columns.iter().map(|column| column.name.clone().into())).collect(),
            None => vec!["(skip)".into()],
        };
        let columns = job.column_count();
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(&name, window, move |this: &mut Self, state, event: &InputEvent, _, cx| match event {
            InputEvent::Change => {
                let value = state.read(cx).value().to_string();
                if let Some(job) = this.jobs.get_mut(index).filter(|job| job.target_name != value) {
                    job.target_name = value;
                    // What was asked about the name before is no longer the question: its answer is dropped when it comes, and
                    // what is known about the table is not known of this name. (The input announces its own first value too:
                    // that is not a new name.)
                    job.epoch += 1;
                    job.exists = None;
                    job.target = Loaded::Pending;
                    this.last_described.remove(&index);
                }
                this.message = None;
                cx.notify();
            }
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.name_committed(index, cx),
            _ => {}
        }));
        subscriptions.push(cx.subscribe_in(&action, window, move |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                this.set_action(index, Action::ALL.get(row).copied().unwrap_or(Action::Append), cx);
            }
        }));
        self.widgets.insert(
            index,
            JobWidgets { name, action, map_selects: vec![None; columns], map_items, new_inputs: (0..job.new_columns.len()).map(|_| None).collect(), _subscriptions: subscriptions, columns_subscriptions: Vec::new() },
        );
    }

    /// The name of the target was typed: the table of that name is looked for.
    fn name_committed(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(job) = self.jobs.get(index) else { return };
        if job.action == Action::Leave || self.last_described.get(&index) == Some(&job.target_name) {
            return;
        }
        self.last_described.insert(index, job.target_name.clone());
        self.redescribe_target(index, cx);
    }

    fn set_action(&mut self, index: usize, action: Action, cx: &mut Context<Self>) {
        let (from, to) = (self.source_kind, self.target_conn_kind);
        let Some(job) = self.jobs.get_mut(index) else { return };
        match action {
            Action::Leave => job.action = Action::Leave,
            Action::Create => {
                if job.new_columns.is_empty() {
                    job.create_as(from, to);
                }
                job.action = Action::Create;
            }
            Action::Append => {
                job.action = Action::Append;
                if job.target.ready().is_none() {
                    self.redescribe_target(index, cx);
                }
            }
        }
        self.widgets.remove(&index);
        self.message = None;
        cx.notify();
    }

    /// The select of source column `source` of the selected job, made on first use.
    pub(super) fn ensure_map_select(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) -> Option<SelectEntity> {
        let index = self.selected;
        let assigned = self.jobs.get(index)?.assignments.get(source).copied().flatten();
        let widgets = self.widgets.get_mut(&index)?;
        if let Some(Some(select)) = widgets.map_selects.get(source) {
            return Some(select.clone());
        }
        let select = select_of(widgets.map_items.clone(), assigned.map_or(0, |target| target + 1), window, cx);
        let subscription = cx.subscribe_in(&select, window, move |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, window, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                this.assign(source, row.checked_sub(1), window, cx);
            }
        });
        let widgets = self.widgets.get_mut(&index)?;
        widgets.columns_subscriptions.push(subscription);
        *widgets.map_selects.get_mut(source)? = Some(select.clone());
        Some(select)
    }

    /// Source column `source` of the selected job goes to target column `target` (`None`: it is skipped); a target
    /// takes one source at a time.
    fn assign(&mut self, source: usize, target: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.selected;
        let Some(job) = self.jobs.get_mut(index) else { return };
        if source >= job.assignments.len() {
            return;
        }
        if let Some(target) = target {
            for other in 0..job.assignments.len() {
                if other != source && job.assignments[other] == Some(target) {
                    job.assignments[other] = None;
                    if let Some(Some(select)) = self.widgets.get(&index).and_then(|widgets| widgets.map_selects.get(other)) {
                        select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(0)), window, cx));
                    }
                }
            }
        }
        job.assignments[source] = target;
        self.message = None;
        cx.notify();
    }

    /// The name and type inputs of new column `column` of the selected job, made on first use.
    pub(super) fn ensure_new_inputs(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) -> Option<(gpui::Entity<InputState>, gpui::Entity<InputState>)> {
        let index = self.selected;
        if let Some(Some(inputs)) = self.widgets.get(&index)?.new_inputs.get(column) {
            return Some(inputs.clone());
        }
        let new = self.jobs.get(index)?.new_columns.get(column)?.clone();
        let (name, data_type) = (input("column", &new.name, window, cx), input("type", &new.data_type, window, cx));
        let mut subscriptions = Vec::new();
        for (state, is_name) in [(&name, true), (&data_type, false)] {
            subscriptions.push(cx.subscribe_in(state, window, move |this: &mut Self, state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let value = state.read(cx).value().to_string();
                    if let Some(new) = this.jobs.get_mut(index).and_then(|job| job.new_columns.get_mut(column)) {
                        if is_name {
                            new.name = value;
                        } else {
                            new.data_type = value;
                        }
                    }
                    this.message = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.next(window, cx),
                _ => {}
            }));
        }
        let widgets = self.widgets.get_mut(&index)?;
        widgets.columns_subscriptions.extend(subscriptions);
        *widgets.new_inputs.get_mut(column)? = Some((name.clone(), data_type.clone()));
        Some((name, data_type))
    }

    pub(super) fn set_include(&mut self, column: usize, include: bool, cx: &mut Context<Self>) {
        if let Some(new) = self.jobs.get_mut(self.selected).and_then(|job| job.new_columns.get_mut(column)) {
            new.include = include;
        }
        self.message = None;
        cx.notify();
    }

    // ---- creating the tables ----------------------------------------------------------------------------

    /// The `CreateTable` of every table to create, checked the way the New table form checks its own.
    fn create_changes(&self, cx: &gpui::App) -> Result<Vec<(usize, crate::sql_ddl::Change)>, Problem> {
        let dialect = self.dialect().ok_or(Problem::NoTargetConnection)?;
        let taken = self.root.read_with(cx, |root, _| root.transfer_table_names(&self.target)).ok();
        let taken: std::collections::HashSet<String> = match taken {
            Some(TableNames::Loaded(names)) => names.into_iter().collect(),
            _ => Default::default(),
        };
        let scope = EditScope { kind: self.target_conn_kind, database: self.target.database.clone(), schema: Some(self.target.schema.clone()).filter(|schema| !schema.is_empty()) };
        let mut changes = Vec::new();
        for (index, job) in self.jobs.iter().enumerate().filter(|(_, job)| job.action == Action::Create) {
            let table: TableId = scope.table_id(&self.target.database, Some(self.target.schema.as_str()).filter(|schema| !schema.is_empty()), &job.target_name);
            let change = mapping::create_table_change(dialect, table, &taken, &job.new_columns, &job.primary_key).map_err(|reason| Problem::CreateRefused { table: job.label.to_string(), reason })?;
            changes.push((index, change));
        }
        Ok(changes)
    }

    /// "Create tables…": the review of their `CREATE TABLE`s, then the transfer.
    pub(super) fn create_tables(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changes = match self.create_changes(cx) {
            Ok(changes) if !changes.is_empty() => changes,
            Ok(_) => return,
            Err(problem) => {
                self.message = Some(problem.to_string());
                return cx.notify();
            }
        };
        let (root, tab_id, origin) = (self.root.clone(), self.tab_id, self.target.origin(""));
        let _ = root.update(cx, |root, cx| root.open_transfer_create_review(tab_id, origin, changes, window, cx));
    }

    /// The review was applied for the changes at `created`: those tables exist, and the transfer goes on into them,
    /// mapped to the columns just created (found by name).
    pub(in crate::app) fn tables_created(&mut self, created: &[usize], cx: &mut Context<Self>) {
        for index in created.iter().copied() {
            let Some(job) = self.jobs.get_mut(index) else { continue };
            let included: Vec<&NewColumn> = job.new_columns.iter().filter(|column| column.include).collect();
            let targets: Vec<mapping::TargetColumn> = included.iter().map(|column| column.target()).collect();
            let mut assignments = vec![None; job.new_columns.len()];
            let mut position = 0;
            for (slot, column) in job.new_columns.iter().enumerate() {
                if column.include {
                    assignments[slot] = Some(position);
                    position += 1;
                }
            }
            job.assignments = assignments;
            job.target = Loaded::Ready(TargetDesc { columns: targets, engine: None });
            job.exists = Some(true);
            job.action = Action::Append;
            self.widgets.remove(&index);
        }
        self.message = None;
        cx.notify();
        // Described again, so the storage engine of a MySQL table is known (it decides whether a transaction can be rolled back).
        // Later: this runs inside the update of the app, which the lookup needs.
        let created = created.to_vec();
        cx.spawn(async move |this, cx| {
            for index in created {
                let _ = this.update(cx, |this, cx| this.redescribe_target(index, cx));
            }
        })
        .detach();
    }

    pub(super) fn tables_to_create(&self) -> usize {
        if self.target_kind == TargetKind::File {
            return 0;
        }
        self.jobs.iter().filter(|job| job.action == Action::Create).count()
    }

    // ---- the run ------------------------------------------------------------------------------------------

    /// Whether "One transaction per table" can be offered: the target has transactions, and each table of a MySQL or
    /// MariaDB target is in an engine that rolls back.
    pub(super) fn transactions(&self) -> bool {
        match self.target_conn_kind {
            _ if self.target_kind == TargetKind::File => false,
            ConnectionKind::Clickhouse => false,
            ConnectionKind::Mysql | ConnectionKind::Mariadb => {
                self.jobs.iter().filter(|job| job.active()).all(|job| job.target.ready().and_then(|target| target.engine.as_deref()).is_some_and(mapping::is_transactional_engine))
            }
            _ => true,
        }
    }

    pub(super) fn no_transactions_note(&self) -> Option<String> {
        if self.transactions() || self.target_kind == TargetKind::File {
            return None;
        }
        Some(match self.target_conn_kind {
            ConnectionKind::Clickhouse => "This database has no transactions: every batch is loaded on its own and stays when the transfer stops.".to_owned(),
            _ => "A table of this transfer uses a storage engine without transactions (or one that is not known yet): every batch is loaded on its own and stays when the transfer stops.".to_owned(),
        })
    }

    fn load_settings(&self, cx: &gpui::App) -> Result<LoadSettings, Problem> {
        let batch_size: usize = self.batch_size.read(cx).value().trim().parse().map_err(|_| Problem::BatchNotNumber)?;
        if !(1..=LoadSettings::MAX_BATCH).contains(&batch_size) {
            return Err(Problem::BatchOutOfRange { max: LoadSettings::MAX_BATCH });
        }
        let row = |select: &SelectEntity| select.read(cx).selected_index(cx).map_or(0, |index| index.row);
        let transaction = if row(&self.transaction_select) == 1 && self.transactions() { TransactionMode::Whole } else { TransactionMode::PerBatch };
        let on_error = if row(&self.on_error_select) == 1 { OnError::SkipAndLog } else { OnError::Stop };
        Ok(LoadSettings { batch_size, transaction, on_error })
    }

    /// The file a job writes, in `folder`.
    pub(super) fn file_paths(&self, folder: &std::path::Path, cx: &gpui::App) -> Vec<(usize, PathBuf)> {
        let active: Vec<usize> = (0..self.jobs.len()).filter(|index| self.jobs[*index].active()).collect();
        if self.single_file() && active.len() == 1 {
            let typed = self.file_name.read(cx).value().trim().to_owned();
            return vec![(active[0], folder.join(super::sink::safe_file_stem(&typed)))];
        }
        let names: Vec<String> = active.iter().map(|index| self.jobs[*index].label.to_string()).collect();
        active.into_iter().zip(super::sink::file_names(&names, self.format)).map(|(index, name)| (index, folder.join(name))).collect()
    }

    /// Start: reads the settings and hands the transfer to the app (which gates and, for Production, asks first).
    pub(super) fn begin_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() || !self.can_leave_mapping() {
            return;
        }
        if self.tables_to_create() > 0 && self.target_kind == TargetKind::Table {
            return self.create_tables(window, cx);
        }
        let load = match self.load_settings(cx) {
            Ok(load) => load,
            Err(problem) => {
                self.message = Some(problem.to_string());
                return cx.notify();
            }
        };
        let mut plans = Vec::new();
        let mut summary = String::new();
        match self.target_kind {
            TargetKind::Table => {
                for job in self.jobs.iter().filter(|job| job.active()) {
                    let columns = job.mapped_columns();
                    if columns.is_empty() {
                        self.message = Some(format!("Map at least one column of \"{}\" to a column of its table.", job.label));
                        return cx.notify();
                    }
                    plans.push(super::job::JobPlan {
                        label: job.label.to_string(),
                        query: job.query.clone(),
                        types: job.columns.ready().map(|columns| columns.iter().map(|column| (column.name.clone(), column.data_type.clone())).collect()).unwrap_or_default(),
                        sink: JobSink::Table { table: super::super::data_import::TargetTable { database: self.target.database.clone(), schema: self.target.schema.clone(), object: job.target_name.clone() }, columns },
                    });
                }
                summary = format!("{} will be written to {}", plural(plans.len(), "table"), self.target.label());
            }
            TargetKind::File => {
                let folder = PathBuf::from(self.folder.read(cx).value().trim());
                for (index, path) in self.file_paths(&folder, cx) {
                    let job = &self.jobs[index];
                    let types = job.columns.ready().map(|columns| columns.iter().map(|column| (column.name.clone(), column.data_type.clone())).collect()).unwrap_or_default();
                    let table = if job.source_table.is_empty() { "query_result".to_owned() } else { job.source_table.clone() };
                    let schema = Some(self.source.schema.clone()).filter(|schema| !schema.is_empty() && !job.source_table.is_empty());
                    plans.push(super::job::JobPlan { label: job.label.to_string(), query: job.query.clone(), types, sink: JobSink::File { path, sql_table: (schema, table) } });
                }
            }
        }
        self.run_labels = plans.iter().map(|plan| plan.label.clone().into()).collect();
        let sql_choice = self.sql_dialect_select.read(cx).selected_index(cx).map_or(0, |index| index.row);
        let request = super::tab::TransferRequest {
            tab_id: self.tab_id,
            source: self.source.clone(),
            target: (self.target_kind == TargetKind::Table).then(|| self.target.clone()),
            jobs: plans,
            load,
            format: self.format,
            overwrite: self.overwrite,
            sql_dialect: sql_choice.checked_sub(1).and_then(|index| SQL_DIALECTS.get(index)).map(|(_, kind)| *kind),
            summary,
        };
        self.message = None;
        self.report = None;
        let root = self.root.clone();
        let _ = root.update(cx, |root, cx| root.start_transfer(request, window, cx));
        cx.notify();
    }

    pub(super) fn refused(&mut self, message: String, cx: &mut Context<Self>) {
        self.message = Some(message);
        cx.notify();
    }

    pub(super) fn transfer_started(&mut self, progress: Arc<TransferProgress>, control: Arc<ApplyControl>, cx: &mut Context<Self>) {
        self.run = Some(RunningTransfer { progress, control, started: Instant::now(), cancelling: false });
        self.report = None;
        self.message = None;
        self.result_selected = 0;
        cx.notify();
    }

    /// The worker reported: the counters are read at the next draw.
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) {
        if self.run.is_some() {
            cx.notify();
        }
    }

    pub(super) fn finished(&mut self, report: TransferReport, cx: &mut Context<Self>) {
        self.run = None;
        self.result_selected = report.jobs.iter().position(|job| job.as_ref().is_some_and(|job| !matches!(job.report.outcome, super::super::data_import::run::Outcome::Completed) || job.report.errors_total > 0)).unwrap_or(0);
        self.report = Some(report);
        cx.notify();
    }

    /// Cancel: the running statement is stopped through the driver, off the UI thread.
    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &mut self.run else { return };
        if std::mem::replace(&mut run.cancelling, true) {
            return;
        }
        let control = run.control.clone();
        cx.background_executor().spawn(async move { smol::unblock(move || control.cancel()).await }).detach();
        cx.notify();
    }

    /// "New transfer": back to the first step.
    pub(super) fn again(&mut self, cx: &mut Context<Self>) {
        self.report = None;
        self.step = Step::Source;
        self.jobs_key = None;
        self.jobs.clear();
        self.widgets.clear();
        cx.notify();
    }

    pub(super) fn select_result(&mut self, index: usize, cx: &mut Context<Self>) {
        self.result_selected = index;
        cx.notify();
    }

    pub(super) fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (root, tab_id) = (self.root.clone(), self.tab_id);
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |root, cx| root.close_transfer_tab(tab_id, window, cx));
        });
    }
}

/// What a test can see of a job.
#[cfg(test)]
pub(in crate::app) struct JobSnapshot {
    pub(in crate::app) label: String,
    pub(in crate::app) action: &'static str,
    pub(in crate::app) target_name: String,
    pub(in crate::app) columns: Option<Vec<(String, String)>>,
    pub(in crate::app) columns_error: Option<String>,
    pub(in crate::app) target_ready: bool,
    pub(in crate::app) target_error: Option<String>,
    pub(in crate::app) assignments: Vec<Option<usize>>,
    pub(in crate::app) new_columns: Vec<(bool, String, String, bool)>,
    pub(in crate::app) primary_key: Vec<String>,
    pub(in crate::app) notes: Vec<String>,
    pub(in crate::app) exists: Option<bool>,
    pub(in crate::app) sql: String,
    /// The key the pages are read by (the key columns), when the table is read by key.
    pub(in crate::app) keyset: Option<Vec<String>>,
    /// How well the pages follow each other.
    pub(in crate::app) order: RowOrder,
}

/// What a test can see of a wizard.
#[cfg(test)]
pub(in crate::app) struct Snapshot {
    pub(in crate::app) step: &'static str,
    pub(in crate::app) mode: &'static str,
    pub(in crate::app) listing: &'static str,
    pub(in crate::app) tables: Vec<(String, bool)>,
    pub(in crate::app) shown: usize,
    pub(in crate::app) target_kind: &'static str,
    pub(in crate::app) format: &'static str,
    pub(in crate::app) target: Namespace,
    pub(in crate::app) namespaces: Vec<String>,
    pub(in crate::app) connections: Vec<String>,
    pub(in crate::app) jobs: Vec<JobSnapshot>,
    pub(in crate::app) selected: usize,
    pub(in crate::app) running: bool,
    pub(in crate::app) progress_rows: Option<u64>,
    pub(in crate::app) report: Option<TransferReport>,
    pub(in crate::app) message: Option<String>,
    /// Mapping widgets made so far (selects, inputs).
    pub(in crate::app) widgets: usize,
    pub(in crate::app) to_create: usize,
    /// The rows left out as the summary lists them.
    pub(in crate::app) error_lines: Vec<String>,
    pub(in crate::app) errors_heading: Option<String>,
}

#[cfg(test)]
impl TransferView {
    pub(in crate::app) fn snapshot(&self) -> Snapshot {
        Snapshot {
            step: match self.step {
                Step::Source => "source",
                Step::Target => "target",
                Step::Mapping => "mapping",
                Step::Run => "run",
            },
            mode: match self.mode {
                SourceMode::Tables => "tables",
                SourceMode::Query => "query",
            },
            listing: match self.listing {
                Loaded::Pending => "loading",
                Loaded::Ready(()) => "ready",
                Loaded::Failed(_) => "failed",
            },
            tables: self.tables.iter().map(|table| (table.name.clone(), table.selected)).collect(),
            shown: self.shown.len(),
            target_kind: match self.target_kind {
                TargetKind::Table => "table",
                TargetKind::File => "file",
            },
            format: self.format.label(),
            target: self.target.clone(),
            namespaces: self.namespaces.iter().map(|namespace| namespace.label().to_string()).collect(),
            connections: self.connections.iter().map(|connection| connection.name.to_string()).collect(),
            jobs: self
                .jobs
                .iter()
                .map(|job| JobSnapshot {
                    label: job.label.to_string(),
                    action: match job.action {
                        Action::Append => "append",
                        Action::Create => "create",
                        Action::Leave => "leave",
                    },
                    target_name: job.target_name.clone(),
                    columns: job.columns.ready().map(|columns| columns.iter().map(|column| (column.name.clone(), column.data_type.clone())).collect()),
                    columns_error: match &job.columns {
                        Loaded::Failed(message) => Some(message.clone()),
                        _ => None,
                    },
                    target_ready: job.target.ready().is_some(),
                    target_error: match &job.target {
                        Loaded::Failed(message) => Some(message.clone()),
                        _ => None,
                    },
                    assignments: job.assignments.clone(),
                    new_columns: job.new_columns.iter().map(|column| (column.include, column.name.clone(), column.data_type.clone(), column.nullable)).collect(),
                    primary_key: job.primary_key.clone(),
                    notes: job.notes(),
                    exists: job.exists,
                    sql: job.query.sql.clone(),
                    keyset: job.query.keyset.as_ref().map(|keyset| keyset.columns.iter().map(|column| column.name.clone()).collect()),
                    order: job.query.order.clone(),
                })
                .collect(),
            selected: self.selected,
            running: self.run.is_some(),
            progress_rows: self.run.as_ref().map(|run| run.progress.jobs.iter().map(|job| job.snapshot().rows_read).sum()),
            report: self.report.clone(),
            message: self.message.clone(),
            widgets: self.widgets.values().map(|widgets| 2 + widgets.map_selects.iter().flatten().count() + widgets.new_inputs.iter().flatten().count()).sum(),
            to_create: self.tables_to_create(),
            error_lines: (0..).map_while(|index| self.error_line(index)).collect(),
            errors_heading: self.left_out_heading(),
        }
    }

    /// Performance runs: the namespace the tables go to and their kind (the select would set both).
    pub(in crate::app) fn set_target_for_test(&mut self, namespace: Namespace, kind: ConnectionKind, cx: &mut Context<Self>) {
        self.target = namespace;
        self.target_conn_kind = kind;
        self.target_kind = TargetKind::Table;
        cx.notify();
    }

    /// Performance runs: files of `format` in `folder`.
    pub(in crate::app) fn set_files_for_test(&mut self, folder: &str, format: FileFormat, window: &mut Window, cx: &mut Context<Self>) {
        self.target_kind = TargetKind::File;
        self.format = format;
        self.folder.update(cx, |state, cx| state.set_value(folder.to_owned(), window, cx));
        self.default_file_name(window, cx);
        cx.notify();
    }

    /// Capture: the action of job `index`, as the select in its detail would choose it.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn set_action_for_test(&mut self, index: usize, create: bool, cx: &mut Context<Self>) {
        self.set_action(index, if create { Action::Create } else { Action::Append }, cx);
    }

    /// Capture: the tables the filter shows are ticked.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn select_all_for_test(&mut self, selected: bool, cx: &mut Context<Self>) {
        self.select_shown(selected, cx);
    }

    /// Capture: only the first table is ticked.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn pick_first_for_test(&mut self, cx: &mut Context<Self>) {
        self.select_shown(false, cx);
        self.set_selected(0, true, cx);
    }

    /// Performance runs: the table job `index` goes to the table `name`.
    pub(in crate::app) fn rename_job_for_test(&mut self, index: usize, name: &str, cx: &mut Context<Self>) {
        if let Some(job) = self.jobs.get_mut(index) {
            job.target_name = name.to_owned();
        }
        self.redescribe_target(index, cx);
    }

    pub(in crate::app) fn file_name_for_test(&self, cx: &gpui::App) -> String {
        self.file_name.read(cx).value().to_string()
    }

    /// The target kind the Write to select would choose.
    pub(in crate::app) fn choose_target_kind_for_test(&mut self, files: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.target_kind = if files { TargetKind::File } else { TargetKind::Table };
        self.kind_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(usize::from(files))), window, cx));
        cx.notify();
    }

    /// Performance runs (a release build draws no debug selectors to click): the steps, the settings, Start.
    pub(in crate::app) fn start_for_test(&mut self, batch: usize, whole: bool, window: &mut Window, cx: &mut Context<Self>) {
        for _ in 0..4 {
            if self.step != Step::Run {
                self.next(window, cx);
            }
        }
        self.batch_size.update(cx, |state, cx| state.set_value(batch.to_string(), window, cx));
        if whole {
            self.transaction_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(1)), window, cx));
        }
        self.begin_run(window, cx);
    }

    /// Capture: a transfer in flight with these counters.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_progress(&mut self, progress: Arc<TransferProgress>, elapsed: std::time::Duration, labels: Vec<SharedString>, cx: &mut Context<Self>) {
        self.run_labels = labels;
        self.step = Step::Run;
        self.message = None;
        self.run = Some(RunningTransfer { progress, control: Arc::new(ApplyControl::default()), started: Instant::now() - elapsed, cancelling: false });
        cx.notify();
    }

    /// Capture: a transfer that ended with `report`.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_report(&mut self, labels: Vec<SharedString>, report: TransferReport, cx: &mut Context<Self>) {
        self.run_labels = labels;
        self.run = None;
        self.step = Step::Run;
        self.message = None;
        self.finished(report, cx);
    }
}

impl Drop for TransferView {
    fn drop(&mut self) {
        // A tab closed under a running transfer stops it (rows already committed stay).
        if let Some(run) = self.run.take() {
            std::thread::spawn(move || run.control.cancel());
        }
    }
}

pub(super) use super::super::data_import::view::{grouped, human_bytes};

/// `1 table`, `3 tables`.
pub(super) fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(connection: &str, schema: &str) -> Namespace {
        Namespace { connection: connection.into(), database: "db".into(), schema: schema.into() }
    }

    #[test]
    fn a_query_that_names_the_table_it_writes_to_is_refused_whatever_the_namespaces() {
        let sql = "SELECT * FROM archive.events WHERE id > 5";
        // Read from `public`, written to `archive.events`: the query names it with its schema.
        assert!(query_reads_its_target(&place("c", "public"), &place("c", "archive"), sql, "events"));
        assert!(query_reads_its_target(&place("c", "public"), &place("c", "public"), sql, "EVENTS"));
        assert!(query_reads_its_target(&place("c", ""), &place("c", "archive"), "SELECT * FROM \"events\"", "events"));
        // Another connection is another server; another name is another table.
        assert!(!query_reads_its_target(&place("c", "public"), &place("other", "archive"), sql, "events"));
        assert!(!query_reads_its_target(&place("c", "public"), &place("c", "archive"), sql, "events_copy"));
    }
}
