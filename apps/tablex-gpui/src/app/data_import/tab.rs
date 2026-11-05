//! The app side of an import: opening the tab, the preview grid, the gates, the dedicated session and the
//! worker thread of the load, and what happens when it ends.
//!
//! The load runs like a structure apply: on a second session of the connection, opened for it and closed
//! after, never on a tab's own session (so a transaction a tab has open is neither joined nor ended) and never
//! on the shared one. The window stays free: the worker reports at most every ~120 ms through a channel, and
//! the window reads the counters when it hears.

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use gpui::{Context, Entity, Window};

use super::super::structure_edit::apply::{ApplyControl, TableOrigin};
use super::super::*;
use super::mapping::TargetColumn;
use super::run::{self, ColumnPlan, ExtraColumn, Load, LoadSettings, Outcome, Progress, Report};
use super::view::ImportView;
use super::writer::{insert_supported, DriverWriter, TargetTable};
use crate::import::infer::Hints;
use crate::import::source::{Sample, SourceOptions, SourceReader};

/// Rows of the sample the grid shows.
pub(super) const PREVIEW_ROWS: usize = 100;

/// Everything a load needs, captured when it is started.
pub(super) struct LoadRequest {
    pub(super) tab_id: u64,
    /// The table the rows go to (`object` is its name; a new table exists by now).
    pub(super) origin: TableOrigin,
    pub(super) path: std::path::PathBuf,
    pub(super) source: SourceOptions,
    /// The names a JSON file is read by.
    pub(super) json_columns: Vec<String>,
    /// The columns of the file the mapping was made for: a delimited file that reads otherwise now is not the one that was
    /// previewed.
    pub(super) expected_columns: Vec<String>,
    pub(super) hints: Hints,
    pub(super) plans: Vec<ColumnPlan>,
    pub(super) extras: Vec<ExtraColumn>,
    pub(super) settings: LoadSettings,
    /// Rows the user will see loaded: for the confirmation of a Production connection.
    pub(super) summary: String,
}

/// What the connection allows.
pub(in crate::app) struct Gate {
    pub(in crate::app) kind: ConnectionKind,
    /// The connection asks for an explicit confirmation before it changes anything.
    pub(in crate::app) confirm: bool,
    pub(in crate::app) subject: String,
}

impl AppRoot {
    /// The kind of the connection and what it asks, or why nothing can be imported into it.
    pub(in crate::app) fn import_gate(&self, connection: &str) -> Result<Gate, String> {
        self.write_gate(connection, "Importing")
    }

    /// What the connection allows when rows are written to its tables by a load (`activity`: "Importing", "Transferring").
    pub(in crate::app) fn write_gate(&self, connection: &str, activity: &str) -> Result<Gate, String> {
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).ok_or("This connection no longer exists.")?;
        if crate::sql_ddl::dialect_for(saved.kind).is_none() {
            return Err("Data can be written to the tables of SQL connections.".to_owned());
        }
        if let Some(message) = self.read_only_connection_message(connection, &format!("{activity} data is")) {
            return Err(message);
        }
        if saved.kind == ConnectionKind::Sqlite && super::super::structure_edit::apply::sqlite_is_in_memory(&saved.uri) {
            return Err(format!(
                "{activity} is off for an in-memory SQLite database: the load would run on a second connection that sees a different, empty database."
            ));
        }
        if self.scenario_ui.service.is_none() {
            match self.driver_capabilities(connection, saved.kind.driver_id()) {
                Some(capabilities) => insert_supported(capabilities).map_err(str::to_owned)?,
                None => return Err("The connection is not open.".to_owned()),
            }
        }
        let effective = self.effective_for(connection);
        let confirm = effective.confirm_execute || effective.connection_type.as_ref().is_some_and(|ty| ty.is_production());
        Ok(Gate { kind: saved.kind, confirm, subject: self.connection_subject(connection, &effective) })
    }

    /// "Import Data…" on the row `origin` stands for: a table row loads into that table, a namespace row into a
    /// new one.
    pub(in crate::app) fn open_import_data(&mut self, origin: TableOrigin, window: &mut Window, cx: &mut Context<Self>) {
        let gate = match self.import_gate(&origin.connection) {
            Ok(gate) => gate,
            Err(message) => {
                self.sidebar.notice = Some(message);
                return cx.notify();
            }
        };
        let Some(dialect) = crate::sql_ddl::dialect_for(gate.kind) else { return };
        let new_table = origin.object.is_empty();
        let namespace = if origin.schema.is_empty() { origin.database.clone() } else { origin.schema.clone() };
        let title = if new_table { format!("Import into {namespace}") } else { format!("Import into {}", origin.object) };
        let schema = Some(origin.schema.as_str()).filter(|schema| !schema.is_empty());
        let tab_id = self.workspace.open_import_tab(&origin.connection, &origin.database, schema, &title);
        let taken = self.loaded_object_names(&origin);
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| ImportView::new(root, tab_id, origin, gate.kind, dialect, new_table, taken, window, cx));
        self.imports.insert(tab_id, view.clone());
        self.activate_tab_ui(tab_id, cx);
        // After this update ends: the wizard asks the app for the table's columns.
        let started = view.clone();
        window.defer(cx, move |window, cx| started.update(cx, |view, cx| view.start(window, cx)));
        cx.notify();
    }

    /// Whether the import on `tab_id` is loading right now.
    pub(in crate::app) fn import_running(&self, tab_id: u64, cx: &gpui::App) -> bool {
        self.imports.get(&tab_id).is_some_and(|view| view.read(cx).running())
    }

    /// Shows the first rows of the file in the tab's grid (the result grid of the app, read-only here). Every
    /// column is text; its header names the type the sample reads as.
    pub(super) fn show_import_preview(&mut self, tab_id: u64, sample: Option<&Sample>, labels: &[String], cx: &mut Context<Self>) {
        let source = sample.filter(|sample| !sample.columns.is_empty()).map(|sample| {
            let fields: Vec<Field> = sample
                .columns
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    let metadata = labels
                        .get(index)
                        .map(|label| std::collections::HashMap::from([(crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_owned(), label.clone())]))
                        .unwrap_or_default();
                    Field::new(name, DataType::Utf8, true).with_metadata(metadata)
                })
                .collect();
            let shown = sample.rows.len().min(PREVIEW_ROWS);
            let arrays: Vec<ArrayRef> = (0..sample.columns.len())
                .map(|column| {
                    Arc::new(StringArray::from_iter(sample.rows[..shown].iter().map(|row| row.cells.get(column).and_then(|cell| cell.as_deref())))) as ArrayRef
                })
                .collect();
            let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("columns of equal length");
            ArrowGridSource::new(vec![Arc::new(batch)])
        });
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else { return };
        runtime.grid = GridState::new(GridContentSource::Arrow(source.unwrap_or_else(|| ArrowGridSource::new(Vec::new()))));
        self.reset_tab_view(tab_id);
        self.notify_tab(tab_id, cx);
    }

    /// The columns of the table, off the UI thread.
    pub(super) fn describe_import_target(&mut self, origin: &TableOrigin, cx: &mut Context<Self>) -> gpui::Task<Result<DescribedTable, String>> {
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = crate::ui_fixture_properties::table_properties(service.scenario().fixture, &origin.connection, &origin.schema, &origin.object);
            return cx.spawn(async move |_, _| result.map(|properties| (properties.columns.iter().map(TargetColumn::from).collect(), None)));
        }
        let key = crate::properties_model::PropertiesKey {
            connection: origin.connection.clone(),
            database: origin.database.clone(),
            schema: origin.schema.clone(),
            object: origin.object.clone(),
            session: self.driver_runtime.connection_ids.get(&origin.connection).copied().unwrap_or(0),
        };
        match self.spawn_describe(&key, cx) {
            Ok(work) => cx.spawn(async move |_, _| {
                work.await
                    .map(|properties| (properties.columns.iter().map(TargetColumn::from).collect(), properties.ddl.as_deref().and_then(super::mapping::engine_of_ddl)))
                    .map_err(|error| crate::sessions::redact_secrets(&error.to_string()))
            }),
            Err(error) => cx.spawn(async move |_, _| Err(error.to_string())),
        }
    }

    /// The table the New table review of the import `tab_id` creates was applied: the wizard goes on to the load.
    pub(in crate::app) fn import_table_created(&mut self, host: u64, cx: &mut Context<Self>) {
        let Some(pending) = self.import_create.take_if(|pending| pending.host == host) else { return };
        if let Some(view) = self.imports.get(&pending.tab_id).cloned() {
            view.update(cx, |view, cx| view.table_created(cx));
        }
    }

    /// Opens the review of the `CreateTable` of the import `tab_id`; applying it creates the table and
    /// tells the wizard.
    pub(super) fn open_import_create_review(&mut self, tab_id: u64, origin: TableOrigin, change: crate::sql_ddl::Change, window: &mut Window, cx: &mut Context<Self>) {
        self.open_object_review(origin, move |_, _| vec![change], window, cx);
        if let Some(host) = &self.object_action {
            self.import_create = Some(ImportCreate { host: host.entity_id().as_u64(), tab_id });
        }
    }

    /// Start button: refuses on a connection that went read-only, asks a Production one, then loads.
    pub(super) fn start_import(&mut self, request: LoadRequest, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = request.tab_id;
        let gate = match self.import_gate(&request.origin.connection) {
            Ok(gate) => gate,
            Err(message) => return self.import_refused(tab_id, message, cx),
        };
        if !gate.confirm {
            return self.run_import(request, false, cx);
        }
        let root = cx.entity().downgrade();
        let request = std::rc::Rc::new(std::cell::RefCell::new(Some(request)));
        let (subject, summary) = (gate.subject, request.borrow().as_ref().map(|request| request.summary.clone()).unwrap_or_default());
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root, request) = (root.clone(), request.clone());
            dialog
                .title("Import this data?")
                .child(div().debug_selector(|| "import-confirm".into()).child(format!("{summary} will be added on {subject}.")))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Import"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    if let Some(request) = request.borrow_mut().take() {
                        let _ = root.update(cx, |this, cx| this.run_import(request, true, cx));
                    }
                    true
                })
        });
    }

    /// Said to the wizard once the update that asked for the load is over (the wizard is being updated by
    /// its own button right now).
    fn import_refused(&mut self, tab_id: u64, message: String, cx: &mut Context<Self>) {
        if let Some(view) = self.imports.get(&tab_id).cloned() {
            cx.defer(move |cx| view.update(cx, |view, cx| view.refused(message, cx)));
        }
    }

    /// Opens the session and runs the load on a worker thread. `confirmed`: the user said yes to a confirmation. The
    /// gates are looked at again here, the moment the load starts: the connection may have become read-only, or one
    /// that asks, while a dialog was open.
    fn run_import(&mut self, request: LoadRequest, confirmed: bool, cx: &mut Context<Self>) {
        let LoadRequest { tab_id, origin, path, source, json_columns, expected_columns, hints, plans, extras, settings, .. } = request;
        let Some(view) = self.imports.get(&tab_id).cloned() else { return };
        match self.import_gate(&origin.connection) {
            Err(message) => return self.import_refused(tab_id, message, cx),
            Ok(gate) if gate.confirm && !confirmed => {
                let message = format!("{} now asks for a confirmation before it changes anything. Start the import again to confirm it.", gate.subject);
                return self.import_refused(tab_id, message, cx);
            }
            Ok(_) => {}
        }
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(&origin.connection)).cloned() else {
            return self.import_refused(tab_id, "This connection no longer exists.".to_owned(), cx);
        };
        let spec = match self.secondary_session_spec(&saved, &origin.connection) {
            Ok(spec) => spec,
            Err(message) => return self.import_refused(tab_id, message, cx),
        };
        let progress = Arc::new(Progress::default());
        let control = Arc::new(ApplyControl::default());
        let started = view.clone();
        let (started_progress, started_control) = (progress.clone(), control.clone());
        cx.defer(move |cx| started.update(cx, |view, cx| view.load_started(started_progress, started_control, cx)));
        let (tick, ticks) = smol::channel::bounded::<()>(1);
        let table = TargetTable { database: origin.database.clone(), schema: origin.schema.clone(), object: origin.object.clone() };
        let (worker_progress, worker_control) = (progress.clone(), control.clone());
        let kind = saved.kind;
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let started = std::time::Instant::now();
                let failed = |message: String| Report {
                    outcome: Outcome::Failed(message),
                    inserted: 0,
                    rolled_back: 0,
                    skipped: 0,
                    rows_read: 0,
                    errors: Vec::new(),
                    errors_total: 0,
                    uncertain: 0,
                    bytes: 0,
                    elapsed: started.elapsed(),
                };
                let mut writer = match DriverWriter::open(spec, table, kind, worker_control.clone()) {
                    Ok(writer) => writer,
                    Err(message) => return failed(format!("Could not open a session for the import: {message}")),
                };
                let mut reader = match SourceReader::open(&path, &source, &json_columns, worker_progress.bytes.clone()) {
                    Ok(reader) => reader,
                    Err(error) => return failed(error.to_string()),
                };
                // The file is read again from the start: it must be the one the mapping was made for (it may have been
                // edited since the preview, or another file chosen after the table was created).
                if !matches!(source.layout, crate::import::source::Layout::Json) && reader.columns() != expected_columns {
                    return failed(format!(
                        "The file has changed since it was previewed: its columns are now {} and the mapping was made for {}. Go back, and map it again.",
                        list_of(reader.columns()),
                        list_of(&expected_columns)
                    ));
                }
                let cancelled = || worker_control.is_cancelled();
                let notify = || {
                    let _ = tick.try_send(());
                };
                let load = Load { columns: &plans, extras: &extras, hints: &hints, settings, progress: &worker_progress, cancelled: &cancelled, notify: &notify };
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run::run(&load, &mut reader, &mut writer)))
                    .unwrap_or_else(|_| failed("The import stopped unexpectedly. Rows may have been added: check the table before trying again.".to_owned()))
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
            let _ = this.update(cx, |this, cx| this.finish_import(tab_id, origin, report, cx));
        })
        .detach();
        cx.notify();
    }

    /// The load ended: tell the wizard, the Error Log, and bring the open tabs of the table in line.
    fn finish_import(&mut self, tab_id: u64, origin: TableOrigin, report: Report, cx: &mut Context<Self>) {
        let table = origin.object.clone();
        match &report.outcome {
            Outcome::Failed(message) => self.report_error(Area::Mutation, Some(&origin.connection), format!("Import into {table} failed: {message}")),
            Outcome::Stopped(reason) => self.report_warning(Area::Mutation, Some(&origin.connection), format!("Import into {table} stopped: {reason}")),
            Outcome::Completed if report.errors_total > 0 => self.report_warning(
                Area::Mutation,
                Some(&origin.connection),
                format!("Import into {table}: {} rows were added and {} skipped.", report.inserted, report.errors_total),
            ),
            // What was committed before the cancel stays: said, also when the tab is gone by now.
            Outcome::Cancelled if report.inserted > 0 => self.report_warning(
                Area::Mutation,
                Some(&origin.connection),
                format!("Import into {table} was cancelled: {} rows were already added and stay in the table.", report.inserted),
            ),
            Outcome::Completed | Outcome::Cancelled => {}
        }
        if report.inserted > 0 {
            self.refresh_table_tabs_after_apply(&origin, cx);
        }
        if let Some(view) = self.imports.get(&tab_id).cloned() {
            view.update(cx, |view, cx| view.finished(report, cx));
        }
        cx.notify();
    }

    pub(super) fn close_import_tab(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close_tab(tab_id, window, cx);
    }
}

/// The columns of the table and, for MySQL and MariaDB, its storage engine.
pub(super) type DescribedTable = (Vec<TargetColumn>, Option<String>);

/// `"a", "b"` for a message.
fn list_of(columns: &[String]) -> String {
    columns.iter().map(|column| format!("\"{column}\"")).collect::<Vec<_>>().join(", ")
}

/// The review of the table an import creates, until it is answered.
#[derive(Debug, Clone, Copy)]
pub(in crate::app) struct ImportCreate {
    host: u64,
    tab_id: u64,
}

/// The wizard's entity of an import tab.
pub(in crate::app) type ImportViews = std::collections::HashMap<u64, Entity<ImportView>>;
