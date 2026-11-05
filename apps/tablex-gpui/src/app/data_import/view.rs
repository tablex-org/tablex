//! The import wizard of a tab: the file and how to read it (Source), which column goes where (Mapping), and
//! the load (Load), as one entity the tab renders. The state machine and the actions are here; the panels
//! are drawn by `panels`.
//!
//! The wizard is a tab, not a dialog: it holds a file, a preview grid (the app's own result grid), a mapping
//! of as many rows as the file has columns and a load that runs for minutes. A modal would block the rest of
//! the window for all of it.
//!
//! Reading is always off the UI thread and never of more than a sample ([`SAMPLE_ROWS`] rows); the load
//! re-reads the file from the start, streamed.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui::{Context, Subscription, WeakEntity, Window};
use gpui_component::input::InputEvent;
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::IndexPath;

use super::super::structure_edit::apply::{ApplyControl, TableOrigin};
use super::super::structure_edit::EditScope;
use super::super::*;
use super::mapping::{self, NewColumn, Note, TargetColumn};
use super::run::{LoadSettings, OnError, Progress, ProgressSnapshot, Report, TransactionMode};
use super::tab::LoadRequest;
use crate::import::delimited::DelimitedFormat;
use crate::import::encoding::Encoding;
use crate::import::infer::{Hints, Inferred};
use crate::import::source::{Layout, Sample, SourceOptions};
use crate::sql_ddl::{DdlDialect, TableId};

/// Rows read to show, to infer types and to check the mapping.
pub(super) const SAMPLE_ROWS: usize = 1000;

/// A file with more columns than this cannot be mapped (the mapping is a list, and only the rows on screen have widgets).
pub(super) const MAX_COLUMNS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Source,
    Mapping,
    Load,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormatChoice {
    Csv,
    Tsv,
    Custom,
    Json,
}

impl FormatChoice {
    pub(super) const ALL: [Self; 4] = [Self::Csv, Self::Tsv, Self::Custom, Self::Json];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV (comma)",
            Self::Tsv => "TSV (tab)",
            Self::Custom => "Delimited (custom character)",
            Self::Json => "JSON (array or one object per line)",
        }
    }

    /// The format a file name suggests.
    fn of_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("tsv" | "tab") => Self::Tsv,
            Some("json" | "ndjson" | "jsonl") => Self::Json,
            _ => Self::Csv,
        }
    }
}

/// Where the sample of the file stands.
pub(super) enum SampleState {
    NoFile,
    Reading,
    Ready(Arc<Sample>),
    Failed(String),
}

pub(super) enum TargetsState {
    Loading,
    Ready(Vec<TargetColumn>),
    Failed(String),
}

type SelectEntity = gpui::Entity<SelectState<Vec<gpui::SharedString>>>;

/// The widgets of one row of the new table's columns.
pub(super) struct NewColumnInputs {
    pub(super) name: gpui::Entity<InputState>,
    pub(super) data_type: gpui::Entity<InputState>,
}

pub(in crate::app) struct ImportView {
    pub(super) root: WeakEntity<AppRoot>,
    pub(super) tab_id: u64,
    pub(super) origin: TableOrigin,
    pub(super) kind: ConnectionKind,
    pub(super) dialect: &'static dyn DdlDialect,
    pub(super) new_table: bool,
    /// The table and view names the namespace holds, as far as the navigator has loaded them.
    pub(super) taken: HashSet<String>,
    pub(super) step: Step,

    // ---- source
    pub(super) path: Option<PathBuf>,
    pub(super) sample: SampleState,
    reading: u64,
    pub(super) format: FormatChoice,
    pub(super) format_select: SelectEntity,
    pub(super) encoding_select: SelectEntity,
    pub(super) delimiter: gpui::Entity<InputState>,
    pub(super) quote: gpui::Entity<InputState>,
    pub(super) escape: gpui::Entity<InputState>,
    pub(super) header: bool,
    pub(super) skip_lines: gpui::Entity<InputState>,
    pub(super) null_text: gpui::Entity<InputState>,
    pub(super) empty_null: bool,
    pub(super) trim: bool,
    pub(super) decimal_select: SelectEntity,
    pub(super) date_format: gpui::Entity<InputState>,
    pub(super) timestamp_format: gpui::Entity<InputState>,

    // ---- mapping
    pub(super) targets: TargetsState,
    pub(super) assignments: Vec<Option<usize>>,
    /// The select of each file column, made when its row is first drawn.
    pub(super) map_selects: Vec<Option<SelectEntity>>,
    /// `(skip)` and the names of the target columns: what every select lists.
    map_items: Vec<gpui::SharedString>,
    /// The table's storage engine when the dialect has several (MySQL, MariaDB).
    pub(super) engine: Option<String>,
    /// The table was just created: describe it at the next draw.
    pub(super) describe_pending: bool,
    pub(super) new_name: gpui::Entity<InputState>,
    pub(super) new_columns: Vec<NewColumn>,
    pub(super) new_inputs: Vec<Option<NewColumnInputs>>,
    /// The sample and hints the mapping rows were built from.
    mapped_for: Option<(Vec<String>, Hints)>,
    pub(super) notes: Vec<Note>,
    pub(super) mapping_scroll: gpui::UniformListScrollHandle,
    /// The table of a new-table import exists (its review was applied).
    pub(super) created: bool,

    // ---- load
    pub(super) batch_size: gpui::Entity<InputState>,
    pub(super) transaction_select: SelectEntity,
    pub(super) on_error_select: SelectEntity,
    pub(super) run: Option<RunningLoad>,
    pub(super) report: Option<Report>,
    pub(super) errors_scroll: gpui::UniformListScrollHandle,
    /// Why the wizard cannot go on (a gate, a bad number), shown above the buttons.
    pub(super) message: Option<String>,
    _subscriptions: Vec<Subscription>,
    mapping_subscriptions: Vec<Subscription>,
}

/// A load in flight.
pub(super) struct RunningLoad {
    pub(super) progress: Arc<Progress>,
    pub(super) control: Arc<ApplyControl>,
    pub(super) started: Instant,
    pub(super) last: ProgressSnapshot,
    pub(super) total_bytes: u64,
    pub(super) cancelling: bool,
}

const ENCODINGS: [Encoding; 5] = Encoding::ALL;
const DECIMALS: [&str; 2] = ["1,234.56 (point)", "1.234,56 (comma)"];
const TRANSACTIONS: [&str; 2] = ["Commit every batch", "One transaction"];
const ON_ERRORS: [&str; 2] = ["Stop at the first error", "Skip the row and log it"];

fn select_of(labels: Vec<gpui::SharedString>, selected: usize, window: &mut Window, cx: &mut Context<ImportView>) -> SelectEntity {
    cx.new(|cx| SelectState::new(labels, Some(IndexPath::default().row(selected)), window, cx))
}

fn input(placeholder: &'static str, value: &str, window: &mut Window, cx: &mut Context<ImportView>) -> gpui::Entity<InputState> {
    let value = value.to_owned();
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
}

fn labels<const N: usize>(texts: [&'static str; N]) -> Vec<gpui::SharedString> {
    texts.into_iter().map(gpui::SharedString::from).collect()
}

impl ImportView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        root: WeakEntity<AppRoot>,
        tab_id: u64,
        origin: TableOrigin,
        kind: ConnectionKind,
        dialect: &'static dyn DdlDialect,
        new_table: bool,
        taken: HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let format_select = select_of(FormatChoice::ALL.iter().map(|format| format.label().into()).collect(), 0, window, cx);
        let encoding_select = select_of(ENCODINGS.iter().map(|encoding| encoding.label().into()).collect(), 0, window, cx);
        let decimal_select = select_of(labels(DECIMALS), 0, window, cx);
        let transaction_select = select_of(labels(TRANSACTIONS), 0, window, cx);
        let on_error_select = select_of(labels(ON_ERRORS), 0, window, cx);
        let mut view = Self {
            root,
            tab_id,
            origin,
            kind,
            dialect,
            new_table,
            taken,
            step: Step::Source,
            path: None,
            sample: SampleState::NoFile,
            reading: 0,
            format: FormatChoice::Csv,
            format_select,
            encoding_select,
            delimiter: input("Delimiter", ";", window, cx),
            quote: input("Quote", "\"", window, cx),
            escape: input("= quote", "", window, cx),
            header: true,
            skip_lines: input("0", "0", window, cx),
            null_text: input("e.g. NULL or \\N", "", window, cx),
            empty_null: false,
            trim: false,
            decimal_select,
            date_format: input("e.g. %d/%m/%Y", "", window, cx),
            timestamp_format: input("e.g. %d/%m/%Y %H:%M", "", window, cx),
            targets: if new_table { TargetsState::Ready(Vec::new()) } else { TargetsState::Loading },
            assignments: Vec::new(),
            map_selects: Vec::new(),
            map_items: Vec::new(),
            engine: None,
            describe_pending: false,
            new_name: input("Table name", "", window, cx),
            new_columns: Vec::new(),
            new_inputs: Vec::new(),
            mapped_for: None,
            notes: Vec::new(),
            mapping_scroll: gpui::UniformListScrollHandle::new(),
            created: false,
            batch_size: input("1000", &LoadSettings::DEFAULT_BATCH.to_string(), window, cx),
            transaction_select,
            on_error_select,
            run: None,
            report: None,
            errors_scroll: gpui::UniformListScrollHandle::new(),
            message: None,
            _subscriptions: Vec::new(),
            mapping_subscriptions: Vec::new(),
        };
        view.wire(window, cx);
        view
    }

    /// Every option widget re-reads the sample when it changes.
    fn wire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = [&self.delimiter, &self.quote, &self.escape, &self.skip_lines, &self.null_text, &self.date_format, &self.timestamp_format];
        let mut subscriptions: Vec<Subscription> = inputs
            .into_iter()
            .map(|state| {
                cx.subscribe_in(state, window, |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.options_changed(window, cx),
                    InputEvent::PressEnter { .. } => this.next(window, cx),
                    _ => {}
                })
            })
            .collect();
        subscriptions.push(cx.subscribe_in(&self.format_select, window, |this: &mut Self, select, event: &SelectEvent<Vec<gpui::SharedString>>, window, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                this.format = FormatChoice::ALL.get(row).copied().unwrap_or(FormatChoice::Csv);
                this.options_changed(window, cx);
            }
        }));
        for select in [&self.encoding_select, &self.decimal_select] {
            subscriptions.push(cx.subscribe_in(select, window, |this: &mut Self, _, event: &SelectEvent<Vec<gpui::SharedString>>, window, cx| {
                if matches!(event, SelectEvent::Confirm(Some(_))) {
                    this.options_changed(window, cx);
                }
            }));
        }
        for select in [&self.transaction_select, &self.on_error_select] {
            subscriptions.push(cx.subscribe(select, |this: &mut Self, _, _: &SelectEvent<Vec<gpui::SharedString>>, cx| {
                this.message = None;
                cx.notify();
            }));
        }
        subscriptions.push(cx.subscribe_in(&self.batch_size, window, |this: &mut Self, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) && this.message.take().is_some() {
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe_in(&self.new_name, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Change) {
                this.message = None;
                cx.notify();
            }
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.next(window, cx);
            }
        }));
        self._subscriptions = subscriptions;
    }

    pub(in crate::app) fn running(&self) -> bool {
        self.run.is_some()
    }

    // ---- the file ----------------------------------------------------------------------------------

    /// Asks for the file (the platform's own picker) and reads it.
    pub(super) fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.new_table {
            self.load_targets(window, cx);
        }
        self.browse(window, cx);
    }

    pub(super) fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import file".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.message = Some("Could not open the file picker.".to_owned());
                        cx.notify();
                    });
                    None
                }
            };
            if let Some(path) = path {
                let _ = this.update_in(cx, |this, window, cx| this.set_file(path, window, cx));
            }
        })
        .detach();
    }

    pub(in crate::app) fn set_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // A load in flight keeps its file.
        if self.run.is_some() {
            return;
        }
        // The format the name suggests, unless the user already picked one.
        self.format = FormatChoice::of_path(&path);
        let row = FormatChoice::ALL.iter().position(|format| *format == self.format).unwrap_or(0);
        self.format_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(row)), window, cx));
        self.path = Some(path);
        self.message = None;
        // A new file starts over: the summary of the last load is not this file's.
        self.report = None;
        self.step = Step::Source;
        self.options_changed(window, cx);
    }

    /// The options the widgets hold, or what is wrong with them.
    pub(super) fn options(&self, cx: &gpui::App) -> Result<(SourceOptions, Hints), String> {
        let text = |state: &gpui::Entity<InputState>| state.read(cx).value().to_string();
        let character = |state: &gpui::Entity<InputState>, name: &str| -> Result<Option<u8>, String> {
            let value = text(state);
            let mut chars = value.chars();
            match (chars.next(), chars.next()) {
                (None, _) => Ok(None),
                (Some(only), None) if only.is_ascii() => Ok(Some(only as u8)),
                _ => Err(format!("The {name} must be a single ASCII character")),
            }
        };
        let encoding = ENCODINGS[self.encoding_select.read(cx).selected_index(cx).map_or(0, |index| index.row).min(ENCODINGS.len() - 1)];
        let mut options = match self.format {
            FormatChoice::Json => SourceOptions::json(),
            choice => {
                let delimiter = match choice {
                    FormatChoice::Csv => b',',
                    FormatChoice::Tsv => b'\t',
                    _ => character(&self.delimiter, "delimiter")?.ok_or("Enter the delimiter")?,
                };
                let format = DelimitedFormat { delimiter, quote: character(&self.quote, "quote")?, escape: character(&self.escape, "escape")? };
                let mut options = SourceOptions::delimited(format.checked().map_err(|error| error.to_string())?);
                options.layout = Layout::Delimited { format, header: self.header };
                // An empty field (being retyped) skips nothing.
                let skip = text(&self.skip_lines);
                options.skip_lines = match skip.trim() {
                    "" => 0,
                    digits => digits.parse().map_err(|_| "Lines to skip must be a number".to_owned())?,
                };
                options
            }
        };
        options.encoding = encoding;
        options.null_text = Some(text(&self.null_text)).filter(|text| !text.is_empty());
        options.empty_as_null = self.empty_null;
        options.trim = self.trim;
        let non_empty = |text: String| Some(text.trim().to_owned()).filter(|text| !text.is_empty());
        let hints = Hints {
            decimal_comma: self.decimal_select.read(cx).selected_index(cx).is_some_and(|index| index.row == 1),
            date_format: non_empty(text(&self.date_format)),
            timestamp_format: non_empty(text(&self.timestamp_format)),
            offsets_to_utc: false,
            passthrough_temporal: false,
        };
        if let Some(problem) = hints.timestamp_format_problem() {
            return Err(problem.to_owned());
        }
        Ok((options, hints))
    }

    /// An option changed: read the sample again.
    pub(super) fn options_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.message = None;
        let Some(path) = self.path.clone() else { return };
        let options = match self.options(cx) {
            Ok((options, _)) => options,
            Err(message) => {
                self.sample = SampleState::Failed(message);
                self.reading += 1;
                self.sample_changed(window, cx);
                return;
            }
        };
        self.reading += 1;
        let generation = self.reading;
        self.sample = SampleState::Reading;
        cx.notify();
        let work = cx.background_executor().spawn(async move { smol::unblock(move || Sample::read(&path, &options, SAMPLE_ROWS)).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                // An answer for options that are not the current ones is dropped.
                if this.reading != generation {
                    return;
                }
                this.sample = match result {
                    Ok(sample) if sample.columns.len() > MAX_COLUMNS => {
                        SampleState::Failed(format!("The file has {} columns; at most {MAX_COLUMNS} can be mapped.", sample.columns.len()))
                    }
                    Ok(sample) => SampleState::Ready(Arc::new(sample)),
                    Err(error) => SampleState::Failed(error.to_string()),
                };
                this.sample_changed(window, cx);
            });
        })
        .detach();
    }

    /// The sample is new: the grid shows it, and a mapping already built is checked against it.
    fn sample_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (sample, labels) = match &self.sample {
            SampleState::Ready(sample) => (Some(sample.clone()), self.type_labels(sample, cx)),
            _ => (None, Vec::new()),
        };
        let (root, tab_id) = (self.root.clone(), self.tab_id);
        let _ = root.update(cx, |root, cx| root.show_import_preview(tab_id, sample.as_deref(), &labels, cx));
        if self.step != Step::Source {
            self.refresh_mapping(window, cx);
        }
        cx.notify();
    }

    /// The type each column of the sample reads as, for the grid's headers.
    fn type_labels(&self, sample: &Sample, cx: &gpui::App) -> Vec<String> {
        let hints = self.options(cx).map(|(_, hints)| hints).unwrap_or_default();
        (0..sample.columns.len())
            .map(|index| {
                let cells = sample.rows.iter().map(|row| row.cells.get(index).and_then(|cell| cell.as_deref()));
                match Inferred::of(cells, &hints) {
                    Inferred::Integer => "integer",
                    Inferred::Decimal => "decimal",
                    Inferred::Float => "float",
                    Inferred::Boolean => "boolean",
                    Inferred::Date => "date",
                    Inferred::Timestamp => "timestamp",
                    Inferred::Text => "text",
                }
                .to_owned()
            })
            .collect()
    }

    pub(super) fn ready_sample(&self) -> Option<Arc<Sample>> {
        match &self.sample {
            SampleState::Ready(sample) => Some(sample.clone()),
            _ => None,
        }
    }

    // ---- the table -----------------------------------------------------------------------------------

    pub(super) fn load_targets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let origin = self.origin.clone();
        let Ok(task) = self.root.update(cx, |root, cx| root.describe_import_target(&origin, cx)) else { return };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                // The columns already mapped keep their targets, found by name in the table as it is described now.
                let previous: Vec<Option<String>> = this.assignments.iter().map(|target| target.and_then(|index| this.targets().get(index)).map(|target| target.name.clone())).collect();
                this.targets = match (result, std::mem::replace(&mut this.targets, TargetsState::Loading)) {
                    (Ok((columns, engine)), _) => {
                        this.engine = engine;
                        TargetsState::Ready(columns)
                    }
                    // Described before (the table was just created): keep those columns and the mapping, so a later
                    // describe has something to restore; the engine stays unknown, so "One transaction" stays off.
                    (Err(message), TargetsState::Ready(columns)) => {
                        this.message = Some(format!("Could not read the table again: {message}"));
                        TargetsState::Ready(columns)
                    }
                    (Err(message), _) => TargetsState::Failed(message),
                };
                if previous.iter().any(Option::is_some) {
                    this.assignments = mapping::remap_assignments(&previous, this.targets());
                    this.map_items = std::iter::once("(skip)".into()).chain(this.targets().iter().map(|target| target.name.clone().into())).collect();
                    this.map_selects = vec![None; this.assignments.len()];
                    this.mapping_subscriptions.clear();
                }
                if this.step != Step::Source {
                    this.refresh_mapping(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The columns of the table (none until they are known).
    pub(super) fn targets(&self) -> &[TargetColumn] {
        match &self.targets {
            TargetsState::Ready(columns) => columns,
            _ => &[],
        }
    }

    // ---- the steps -------------------------------------------------------------------------------------

    pub(super) fn can_leave_source(&self) -> bool {
        matches!(self.sample, SampleState::Ready(_)) && (self.new_table || matches!(self.targets, TargetsState::Ready(_)))
    }

    pub(super) fn can_leave_mapping(&self) -> bool {
        if self.new_table {
            self.new_columns.iter().any(|column| column.include)
        } else {
            self.assignments.iter().any(Option::is_some)
        }
    }

    pub(in crate::app) fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        match self.step {
            Step::Source if self.can_leave_source() => {
                self.step = Step::Mapping;
                self.message = None;
                self.refresh_mapping(window, cx);
            }
            Step::Mapping if self.can_leave_mapping() => {
                self.step = Step::Load;
                self.message = None;
            }
            _ => {}
        }
        cx.notify();
    }

    pub(in crate::app) fn back(&mut self, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        self.step = match self.step {
            Step::Load => Step::Mapping,
            _ => Step::Source,
        };
        self.message = None;
        self.report = None;
        cx.notify();
    }

    /// Builds the mapping rows for the sample, unless they were built for the same columns and hints.
    pub(super) fn refresh_mapping(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sample) = self.ready_sample() else { return };
        let hints = self.options(cx).map(|(_, hints)| hints).unwrap_or_default();
        let key = (sample.columns.clone(), hints.clone());
        if self.new_table {
            if self.mapped_for.as_ref() != Some(&key) && !self.created {
                self.build_new_columns(&sample, &hints, window, cx);
            }
        } else if self.mapped_for.as_ref().map(|(columns, _)| columns) != Some(&key.0) || self.map_selects.len() != sample.columns.len() {
            self.build_map_rows(&sample, window, cx);
        }
        self.mapped_for = Some(key);
        self.recheck(cx);
    }

    /// The file's columns mapped to the table's by name. The selects are made when their rows are drawn
    /// (`ensure_map_select`): a file with a thousand columns has widgets for the screenful.
    fn build_map_rows(&mut self, sample: &Sample, _window: &mut Window, _cx: &mut Context<Self>) {
        self.assignments = mapping::auto_map(&sample.columns, self.targets());
        self.map_items = std::iter::once("(skip)".into()).chain(self.targets().iter().map(|target| target.name.clone().into())).collect();
        self.mapping_subscriptions.clear();
        self.map_selects = vec![None; self.assignments.len()];
        self.notes.clear();
    }

    /// The select of file column `source`, made on first use.
    pub(super) fn ensure_map_select(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) -> Option<SelectEntity> {
        let slot = self.map_selects.get(source)?;
        if let Some(select) = slot {
            return Some(select.clone());
        }
        let assigned = self.assignments.get(source).copied().flatten();
        let select = select_of(self.map_items.clone(), assigned.map_or(0, |target| target + 1), window, cx);
        self.mapping_subscriptions.push(cx.subscribe_in(&select, window, move |this: &mut Self, select, event: &SelectEvent<Vec<gpui::SharedString>>, window, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                this.assign(source, row.checked_sub(1), window, cx);
            }
        }));
        self.map_selects[source] = Some(select.clone());
        Some(select)
    }

    /// Source column `source` goes to target column `target` (`None`: it is skipped). A target takes one source
    /// at a time: the column that had it is skipped.
    fn assign(&mut self, source: usize, target: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = target {
            let others: Vec<usize> = (0..self.assignments.len()).filter(|other| *other != source && self.assignments[*other] == Some(target)).collect();
            for other in others {
                self.assignments[other] = None;
                if let Some(select) = &self.map_selects[other] {
                    select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(0)), window, cx));
                }
            }
        }
        self.assignments[source] = target;
        self.recheck(cx);
        cx.notify();
    }

    /// One row per column of the file for the table to create, typed by its sample. The inputs of a row are made
    /// when it is first drawn (`ensure_new_inputs`).
    fn build_new_columns(&mut self, sample: &Sample, hints: &Hints, _window: &mut Window, _cx: &mut Context<Self>) {
        self.new_columns = mapping::propose_columns(sample, self.kind, hints);
        self.mapping_subscriptions.clear();
        self.new_inputs = (0..self.new_columns.len()).map(|_| None).collect();
        self.notes.clear();
    }

    /// The name and type inputs of new column `index`, made on first use.
    pub(super) fn ensure_new_inputs(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> Option<(gpui::Entity<InputState>, gpui::Entity<InputState>)> {
        if let Some(Some(inputs)) = self.new_inputs.get(index) {
            return Some((inputs.name.clone(), inputs.data_type.clone()));
        }
        let column = self.new_columns.get(index)?;
        let (name, data_type) = (input("column", &column.name, window, cx), input("type", &column.data_type, window, cx));
        for (state, is_name) in [(&name, true), (&data_type, false)] {
            self.mapping_subscriptions.push(cx.subscribe_in(state, window, move |this: &mut Self, state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let value = state.read(cx).value().to_string();
                    if let Some(column) = this.new_columns.get_mut(index) {
                        if is_name {
                            column.name = value;
                        } else {
                            column.data_type = value;
                        }
                    }
                    this.message = None;
                    this.recheck(cx);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.next(window, cx),
                _ => {}
            }));
        }
        self.new_inputs[index] = Some(NewColumnInputs { name: name.clone(), data_type: data_type.clone() });
        Some((name, data_type))
    }

    pub(super) fn set_include(&mut self, index: usize, include: bool, cx: &mut Context<Self>) {
        if let Some(column) = self.new_columns.get_mut(index) {
            column.include = include;
        }
        self.message = None;
        self.recheck(cx);
        cx.notify();
    }

    /// The notes the sample has for the mapping as it is.
    fn recheck(&mut self, cx: &gpui::App) {
        let Some(sample) = self.ready_sample() else { return };
        let hints = self.options(cx).map(|(_, hints)| hints).unwrap_or_default();
        self.notes = if self.new_table {
            let included: Vec<(usize, TargetColumn)> = self.new_columns.iter().enumerate().filter(|(_, column)| column.include).map(|(index, column)| (index, column.target())).collect();
            let mut assignments = vec![None; sample.columns.len()];
            let targets: Vec<TargetColumn> = included.iter().map(|(_, target)| target.clone()).collect();
            for (position, (source, _)) in included.iter().enumerate() {
                assignments[*source] = Some(position);
            }
            mapping::check_sample(&sample, &assignments, &targets, &hints)
        } else {
            mapping::check_sample(&sample, &self.assignments, self.targets(), &hints)
        };
    }

    // ---- creating the table ---------------------------------------------------------------------------

    /// The `CreateTable` change of the columns as they stand, or what is wrong with them.
    pub(super) fn create_change(&self, cx: &gpui::App) -> Result<crate::sql_ddl::Change, String> {
        let name = self.new_name.read(cx).value().to_string();
        let table: TableId = EditScope { kind: self.kind, database: self.origin.database.clone(), schema: Some(self.origin.schema.clone()).filter(|schema| !schema.is_empty()) }
            .table_id(&self.origin.database, Some(self.origin.schema.as_str()), &name);
        mapping::create_table_change(self.dialect, table, &self.taken, &self.new_columns, &[])
    }

    /// "Create table…": the review of the `CreateTable`, then the load.
    pub(super) fn create_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let change = match self.create_change(cx) {
            Ok(change) => change,
            Err(message) => {
                self.message = Some(message);
                return cx.notify();
            }
        };
        let (root, tab_id, origin) = (self.root.clone(), self.tab_id, self.origin.clone());
        let _ = root.update(cx, |root, cx| root.open_import_create_review(tab_id, origin, change, window, cx));
    }

    /// The review was applied: the table exists, and the wizard goes on as for an existing table, mapped to the columns just
    /// created (so a changed file is matched by name, like any other).
    pub(super) fn table_created(&mut self, cx: &mut Context<Self>) {
        self.created = true;
        self.origin.object = self.new_name.read(cx).value().to_string();
        self.taken.insert(self.origin.object.clone());
        let included: Vec<(usize, TargetColumn)> = self.new_columns.iter().enumerate().filter(|(_, column)| column.include).map(|(index, column)| (index, column.target())).collect();
        let mut assignments = vec![None; self.new_columns.len()];
        for (position, (source, _)) in included.iter().enumerate() {
            assignments[*source] = Some(position);
        }
        let targets: Vec<TargetColumn> = included.into_iter().map(|(_, target)| target).collect();
        self.map_items = std::iter::once("(skip)".into()).chain(targets.iter().map(|target| target.name.clone().into())).collect();
        self.map_selects = vec![None; assignments.len()];
        self.mapping_subscriptions.clear();
        self.assignments = assignments;
        self.targets = TargetsState::Ready(targets);
        // The engine is the server's default, whatever that is: unknown (so "One transaction" is off) until the table
        // is described, which is asked for now.
        self.engine = None;
        self.new_table = false;
        self.message = None;
        // Asked for at the next draw, which has the window.
        self.describe_pending = true;
        cx.notify();
    }

    // ---- the load -----------------------------------------------------------------------------------------

    fn load_settings(&self, cx: &gpui::App) -> Result<LoadSettings, String> {
        let batch_size: usize = self.batch_size.read(cx).value().trim().parse().map_err(|_| "The batch size must be a number".to_owned())?;
        if !(1..=LoadSettings::MAX_BATCH).contains(&batch_size) {
            return Err(format!("The batch size must be between 1 and {}", LoadSettings::MAX_BATCH));
        }
        let row = |select: &SelectEntity| select.read(cx).selected_index(cx).map_or(0, |index| index.row);
        // Without transactions (ClickHouse) every batch is its own step whatever is chosen.
        let transaction = if row(&self.transaction_select) == 1 && self.transactions() { TransactionMode::Whole } else { TransactionMode::PerBatch };
        let on_error = if row(&self.on_error_select) == 1 { OnError::SkipAndLog } else { OnError::Stop };
        Ok(LoadSettings { batch_size, transaction, on_error })
    }

    /// Whether the table has transactions to make one of: not ClickHouse, and in MySQL and MariaDB not a table of
    /// an engine without them (MyISAM, Aria, MEMORY, ...): a `BEGIN` there would promise a rollback nothing keeps.
    pub(super) fn transactions(&self) -> bool {
        match self.kind {
            ConnectionKind::Clickhouse => false,
            ConnectionKind::Mysql | ConnectionKind::Mariadb => self.engine.as_deref().is_some_and(mapping::is_transactional_engine),
            _ => true,
        }
    }

    /// Why "One transaction" is off, when it is.
    pub(super) fn no_transactions_note(&self) -> Option<String> {
        if self.transactions() {
            return None;
        }
        Some(match (self.kind, self.engine.as_deref()) {
            (ConnectionKind::Clickhouse, _) => "This database has no transactions: every batch is loaded on its own and stays when the import stops.".to_owned(),
            (_, Some(engine)) => format!("This table uses the {engine} engine, which has no transactions: every batch is loaded on its own and stays when the import stops."),
            _ => "The storage engine of this table is not known to have transactions: every batch is loaded on its own and stays when the import stops.".to_owned(),
        })
    }

    /// How many columns of the file are loaded.
    pub(super) fn mapped_count(&self) -> usize {
        if self.new_table {
            self.new_columns.iter().filter(|column| column.include).count()
        } else {
            self.assignments.iter().flatten().count()
        }
    }

    pub(super) fn plans(&self) -> Vec<super::run::ColumnPlan> {
        if self.new_table {
            mapping::plans_for_new(&self.new_columns)
        } else {
            mapping::plans_for(&self.assignments, self.targets())
        }
    }

    /// Import: reads the settings and hands the load to the app (which gates and, for Production, asks first).
    pub(super) fn begin_load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() || (self.new_table && !self.created) {
            return;
        }
        let (Some(path), Some(sample)) = (self.path.clone(), self.ready_sample()) else { return };
        let settings = match self.load_settings(cx) {
            Ok(settings) => settings,
            Err(message) => {
                self.message = Some(message);
                return cx.notify();
            }
        };
        let (source, hints) = match self.options(cx) {
            Ok(options) => options,
            Err(message) => {
                self.message = Some(message);
                return cx.notify();
            }
        };
        let plans = self.plans();
        let extras = if matches!(source.layout, Layout::Json) { mapping::extras_for(&self.assignments, self.targets()) } else { Vec::new() };
        let expected_columns = self.mapped_for.as_ref().map(|(columns, _)| columns.clone()).unwrap_or_default();
        if plans.is_empty() {
            self.message = Some("Map at least one column of the file to a column of the table.".to_owned());
            return cx.notify();
        }
        self.message = None;
        self.report = None;
        let size = human_bytes(sample.file_size);
        let table = if self.new_table { self.new_name.read(cx).value().to_string() } else { self.origin.object.clone() };
        let summary = format!("The rows of {} ({size}) into {table}", path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned()));
        let request = LoadRequest {
            tab_id: self.tab_id,
            origin: self.origin.clone(),
            path,
            source,
            json_columns: sample.columns.clone(),
            expected_columns,
            hints,
            plans,
            extras,
            settings,
            summary,
        };
        let root = self.root.clone();
        let _ = root.update(cx, |root, cx| root.start_import(request, window, cx));
        cx.notify();
    }

    pub(super) fn refused(&mut self, message: String, cx: &mut Context<Self>) {
        self.message = Some(message);
        cx.notify();
    }

    pub(super) fn load_started(&mut self, progress: Arc<Progress>, control: Arc<ApplyControl>, cx: &mut Context<Self>) {
        let total_bytes = self.ready_sample().map_or(0, |sample| sample.file_size);
        self.run = Some(RunningLoad { progress, control, started: Instant::now(), last: ProgressSnapshot::default(), total_bytes, cancelling: false });
        self.report = None;
        self.message = None;
        cx.notify();
    }

    /// The worker reported: read the counters.
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = &mut self.run {
            run.last = run.progress.snapshot();
            cx.notify();
        }
    }

    pub(super) fn finished(&mut self, report: Report, cx: &mut Context<Self>) {
        self.run = None;
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

    /// "Import another file": back to the first step on the same table.
    pub(super) fn again(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.report = None;
        self.step = Step::Source;
        if self.created {
            // The table exists now: the next file goes into it, by its columns as the server describes them.
            self.mapped_for = None;
            self.load_targets(window, cx);
        }
        self.browse(window, cx);
        cx.notify();
    }

    pub(super) fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Closing asks the wizard whether it is running: after this update ends.
        let (root, tab_id) = (self.root.clone(), self.tab_id);
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |root, cx| root.close_import_tab(tab_id, window, cx));
        });
    }
}

/// What a test can see of a wizard.
#[cfg(test)]
pub(in crate::app) struct Snapshot {
    pub(in crate::app) step: &'static str,
    pub(in crate::app) columns: Vec<String>,
    pub(in crate::app) rows: usize,
    pub(in crate::app) bad: usize,
    pub(in crate::app) reading: bool,
    pub(in crate::app) sample_error: Option<String>,
    pub(in crate::app) targets_ready: bool,
    pub(in crate::app) assignments: Vec<Option<usize>>,
    pub(in crate::app) notes: Vec<String>,
    pub(in crate::app) new_columns: Vec<(bool, String, String)>,
    pub(in crate::app) created: bool,
    pub(in crate::app) running: bool,
    pub(in crate::app) progress: Option<super::run::ProgressSnapshot>,
    pub(in crate::app) report: Option<Report>,
    pub(in crate::app) message: Option<String>,
    pub(in crate::app) transactions: bool,
    pub(in crate::app) engine: Option<String>,
    /// Mapping widgets made so far (selects of the existing-table mapping, inputs of the new one).
    pub(in crate::app) widgets: usize,
}

#[cfg(test)]
impl ImportView {
    /// Performance runs (a release build draws no debug selectors to click): the steps, the settings, Import.
    pub(in crate::app) fn start_for_test(&mut self, batch: usize, whole: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.next(window, cx);
        self.next(window, cx);
        self.batch_size.update(cx, |state, cx| state.set_value(batch.to_string(), window, cx));
        if whole {
            self.transaction_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(1)), window, cx));
        }
        self.begin_load(window, cx);
    }

    /// Capture: a load in flight with these counters.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_progress(&mut self, snapshot: ProgressSnapshot, total_bytes: u64, elapsed: std::time::Duration, cx: &mut Context<Self>) {
        self.run = Some(RunningLoad {
            progress: Arc::new(Progress::default()),
            control: Arc::new(ApplyControl::default()),
            started: Instant::now() - elapsed,
            last: snapshot,
            total_bytes,
            cancelling: false,
        });
        cx.notify();
    }

    /// Capture: a load that ended with `report`.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_report(&mut self, report: Report, cx: &mut Context<Self>) {
        self.finished(report, cx);
    }

    pub(in crate::app) fn snapshot(&self) -> Snapshot {
        let sample = self.ready_sample();
        Snapshot {
            step: match self.step {
                Step::Source => "source",
                Step::Mapping => "mapping",
                Step::Load => "load",
            },
            columns: sample.as_ref().map(|sample| sample.columns.clone()).unwrap_or_default(),
            rows: sample.as_ref().map_or(0, |sample| sample.rows.len()),
            bad: sample.as_ref().map_or(0, |sample| sample.bad.len()),
            reading: matches!(self.sample, SampleState::Reading),
            sample_error: match &self.sample {
                SampleState::Failed(message) => Some(message.clone()),
                _ => None,
            },
            targets_ready: matches!(self.targets, TargetsState::Ready(_)),
            assignments: self.assignments.clone(),
            notes: self.notes.iter().map(|note| note.message.clone()).collect(),
            new_columns: self.new_columns.iter().map(|column| (column.include, column.name.clone(), column.data_type.clone())).collect(),
            created: self.created,
            running: self.run.is_some(),
            progress: self.run.as_ref().map(|run| run.progress.snapshot()),
            report: self.report.clone(),
            message: self.message.clone(),
            transactions: self.transactions(),
            engine: self.engine.clone(),
            widgets: self.map_selects.iter().flatten().count() + self.new_inputs.iter().flatten().count(),
        }
    }
}

impl Drop for ImportView {
    fn drop(&mut self) {
        // A tab closed under a running load stops it (rows already committed stay; a one-transaction load rolls back).
        if let Some(run) = self.run.take() {
            std::thread::spawn(move || run.control.cancel());
        }
    }
}

/// `12.3 MB`, `640 KB`.
pub(in crate::app) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `1,234,567`.
pub(in crate::app) fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_sizes_read_naturally() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(80 * 1024 * 1024), "80.0 MB");
    }

    #[test]
    fn a_file_name_suggests_its_format() {
        for (name, format) in [("a.csv", FormatChoice::Csv), ("a.TSV", FormatChoice::Tsv), ("a.json", FormatChoice::Json), ("a.ndjson", FormatChoice::Json), ("a.txt", FormatChoice::Csv)] {
            assert_eq!(FormatChoice::of_path(std::path::Path::new(name)), format, "{name}");
        }
    }
}
