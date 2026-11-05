//! The wizard of a SQL file run: the file and where it runs (Source), how it runs (Settings), and the run
//! itself (Run), as one entity the tab renders. The state machine and the actions are here; the panels are
//! drawn by `panels`.
//!
//! The wizard is a tab, not a dialog: a file of gigabytes runs for minutes or hours, and the rest of the window
//! stays free meanwhile. The file is never shown in a text widget: Source lists its first statements as plain
//! lines, read off the UI thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui::{Context, SharedString, Subscription, WeakEntity, Window};
use gpui_component::input::InputEvent;
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::IndexPath;

use super::super::data_import::view::human_bytes;
use super::super::data_transfer::{Namespace, TargetConnection};
use super::super::structure_edit::apply::ApplyControl;
use super::super::*;
use super::run::{OnError, Progress, ProgressSnapshot, Report, Settings, Transactions};
use super::tab::RunRequest;
use crate::import::encoding::Encoding;

/// The statements Source lists.
pub(super) const SAMPLE_STATEMENTS: usize = 5;

/// The most statements a commit-every-N run may batch.
pub(super) const MAX_EVERY: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Source,
    Settings,
    Run,
}

/// The start of the file, as the run will read it.
pub(super) struct Sample {
    pub(super) size: u64,
    /// The first statements, each on one line.
    pub(super) first: Vec<String>,
    /// There are more than listed.
    pub(super) more: bool,
}

pub(super) enum SampleState {
    NoFile,
    Reading,
    Ready(Arc<Sample>),
    Failed(String),
}

/// What is wrong with the settings the widgets hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsError {
    NotANumber(&'static str),
    TooManyPerCommit,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotANumber(name) => write!(f, "{name} must be a number from 1"),
            Self::TooManyPerCommit => write!(f, "At most {MAX_EVERY} statements can be committed together"),
        }
    }
}

impl std::error::Error for SettingsError {}

type SelectEntity = gpui::Entity<SelectState<Vec<SharedString>>>;

const ENCODINGS: [Encoding; 5] = Encoding::ALL;
const TRANSACTIONS: [&str; 3] = ["Autocommit: each statement on its own", "One transaction for the whole file", "Commit every N statements"];
const ON_ERRORS: [&str; 2] = ["Stop at the first error", "Log the error and continue"];

fn select_of(labels: Vec<SharedString>, selected: usize, window: &mut Window, cx: &mut Context<FileRunView>) -> SelectEntity {
    cx.new(|cx| SelectState::new(labels, Some(IndexPath::default().row(selected)), window, cx))
}

fn input(placeholder: &'static str, value: &str, window: &mut Window, cx: &mut Context<FileRunView>) -> gpui::Entity<InputState> {
    let value = value.to_owned();
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
}

fn labels<const N: usize>(texts: [&'static str; N]) -> Vec<SharedString> {
    texts.into_iter().map(SharedString::from).collect()
}

pub(in crate::app) struct FileRunView {
    pub(super) root: WeakEntity<AppRoot>,
    pub(super) tab_id: u64,
    pub(super) kind: ConnectionKind,
    pub(super) step: Step,

    // ---- source
    pub(super) path: Option<PathBuf>,
    pub(super) sample: SampleState,
    reading: u64,
    pub(super) encoding_select: SelectEntity,
    pub(super) start_at: gpui::Entity<InputState>,
    pub(super) connections: Vec<TargetConnection>,
    pub(super) connection_select: SelectEntity,
    pub(super) target: Namespace,
    pub(super) namespaces: Vec<Namespace>,
    pub(super) namespace_select: SelectEntity,

    // ---- settings
    pub(super) transactions_select: SelectEntity,
    pub(super) every: gpui::Entity<InputState>,
    pub(super) on_error_select: SelectEntity,

    // ---- run
    pub(super) run: Option<RunningFile>,
    pub(super) report: Option<Report>,
    pub(super) entries_scroll: gpui::UniformListScrollHandle,
    /// Why the wizard cannot go on (a gate, a bad number), shown above the buttons.
    pub(super) message: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// A run in flight.
pub(super) struct RunningFile {
    pub(super) progress: Arc<Progress>,
    pub(super) control: Arc<ApplyControl>,
    pub(super) started: Instant,
    pub(super) last: ProgressSnapshot,
    pub(super) total_bytes: u64,
    pub(super) cancelling: bool,
}

impl FileRunView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(root: WeakEntity<AppRoot>, tab_id: u64, kind: ConnectionKind, target: Namespace, connections: Vec<TargetConnection>, namespaces: Vec<Namespace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = connections.iter().position(|connection| connection.id == target.connection).unwrap_or(0);
        let connection_select = select_of(connections.iter().map(|connection| connection.name.clone()).collect(), current, window, cx);
        let position = namespaces.iter().position(|namespace| *namespace == target).unwrap_or(0);
        let namespace_select = select_of(namespaces.iter().map(Namespace::label).collect(), position, window, cx);
        let mut view = Self {
            root,
            tab_id,
            kind,
            step: Step::Source,
            path: None,
            sample: SampleState::NoFile,
            reading: 0,
            encoding_select: select_of(ENCODINGS.iter().map(|encoding| encoding.label().into()).collect(), 0, window, cx),
            start_at: input("1", "1", window, cx),
            connections,
            connection_select,
            target,
            namespaces,
            namespace_select,
            transactions_select: select_of(labels(TRANSACTIONS), 0, window, cx),
            every: input("1000", "1000", window, cx),
            on_error_select: select_of(labels(ON_ERRORS), 0, window, cx),
            run: None,
            report: None,
            entries_scroll: gpui::UniformListScrollHandle::new(),
            message: None,
            _subscriptions: Vec::new(),
        };
        view.wire(window, cx);
        view
    }

    fn wire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(&self.encoding_select, window, |this: &mut Self, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                this.read_sample(window, cx);
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
                    this.sync_tab(cx);
                    cx.notify();
                }
            }
        }));
        for select in [&self.transactions_select, &self.on_error_select] {
            subscriptions.push(cx.subscribe(select, |this: &mut Self, _, _: &SelectEvent<Vec<SharedString>>, cx| {
                this.message = None;
                cx.notify();
            }));
        }
        for state in [&self.start_at, &self.every] {
            subscriptions.push(cx.subscribe_in(state, window, |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.message = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.next(window, cx),
                _ => {}
            }));
        }
        self._subscriptions = subscriptions;
    }

    pub(in crate::app) fn running(&self) -> bool {
        self.run.is_some()
    }

    // ---- the file ----------------------------------------------------------------------------------

    /// Asks for the file (the platform's own picker).
    pub(super) fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path.is_none() {
            self.browse(window, cx);
        }
    }

    pub(super) fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Run SQL file".into()) });
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
        // A run in flight keeps its file.
        if self.run.is_some() {
            return;
        }
        self.path = Some(path);
        self.message = None;
        // A new file starts over: the summary of the last run is not this file's.
        self.report = None;
        self.step = Step::Source;
        self.start_at.update(cx, |state, cx| state.set_value("1", window, cx));
        self.read_sample(window, cx);
    }

    pub(super) fn encoding(&self, cx: &gpui::App) -> Encoding {
        ENCODINGS[self.encoding_select.read(cx).selected_index(cx).map_or(0, |index| index.row).min(ENCODINGS.len() - 1)]
    }

    /// Reads the size and the first statements of the file, off the UI thread.
    fn read_sample(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.message = None;
        let Some(path) = self.path.clone() else { return };
        self.reading += 1;
        let generation = self.reading;
        self.sample = SampleState::Reading;
        cx.notify();
        let (encoding, kind) = (self.encoding(cx), self.kind);
        let work = cx.background_executor().spawn(async move { smol::unblock(move || super::tab::read_sample(&path, encoding, kind)).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                // An answer for a file or encoding that is not the current one is dropped.
                if this.reading != generation {
                    return;
                }
                this.sample = match result {
                    Ok(sample) => SampleState::Ready(Arc::new(sample)),
                    Err(error) => SampleState::Failed(error.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ready_sample(&self) -> Option<Arc<Sample>> {
        match &self.sample {
            SampleState::Ready(sample) => Some(sample.clone()),
            _ => None,
        }
    }

    // ---- the target --------------------------------------------------------------------------------

    fn choose_connection(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connections.get(row).cloned() else { return };
        let namespaces = self.root.read_with(cx, |root, _| root.transfer_namespaces(&connection.id, &self.target)).unwrap_or_default();
        self.kind = connection.kind;
        self.target = namespaces.first().cloned().unwrap_or(Namespace { connection: connection.id.clone(), database: String::new(), schema: String::new() });
        let labels: Vec<SharedString> = namespaces.iter().map(Namespace::label).collect();
        self.namespaces = namespaces;
        let select = self.namespace_select.clone();
        select.update(cx, |select, cx| {
            select.set_items(labels, window, cx);
            select.set_selected_index(Some(IndexPath::default().row(0)), window, cx);
        });
        // The first statements are read as this dialect reads them.
        if self.path.is_some() {
            self.read_sample(window, cx);
        }
        self.message = None;
        self.sync_tab(cx);
        cx.notify();
    }

    /// The tab names the connection the run will use.
    fn sync_tab(&self, cx: &mut Context<Self>) {
        let (root, tab_id, target) = (self.root.clone(), self.tab_id, self.target.clone());
        let _ = root.update(cx, |root, cx| root.file_run_target_changed(tab_id, &target, cx));
    }

    pub(super) fn connection_name(&self) -> SharedString {
        self.connections.iter().find(|connection| connection.id == self.target.connection).map_or_else(|| "the connection".into(), |connection| connection.name.clone())
    }

    /// Whether this connection has transactions to make one of.
    pub(super) fn transactions_available(&self) -> bool {
        self.kind != ConnectionKind::Clickhouse
    }

    /// A transaction mode on a server that commits by itself at DDL cannot be rolled back whole.
    pub(super) fn implicit_commit_note(&self) -> Option<&'static str> {
        matches!(self.kind, ConnectionKind::Mysql | ConnectionKind::Mariadb).then_some(
            "MySQL and MariaDB commit by themselves before CREATE, DROP, ALTER, TRUNCATE and similar statements. The file runs as one transaction between two of them: a rollback undoes only the data changes since the last, so a file with DDL cannot be rolled back whole.",
        )
    }

    // ---- the steps ---------------------------------------------------------------------------------

    pub(super) fn can_leave_source(&self) -> bool {
        matches!(self.sample, SampleState::Ready(_)) && self.connections.iter().any(|connection| connection.id == self.target.connection)
    }

    /// The settings the widgets hold, or what is wrong with them.
    pub(super) fn settings(&self, cx: &gpui::App) -> Result<Settings, SettingsError> {
        let number = |state: &gpui::Entity<InputState>, name: &'static str| -> Result<u64, SettingsError> {
            let text = state.read(cx).value().to_string();
            match text.trim() {
                "" => Ok(1),
                digits => digits.parse::<u64>().ok().filter(|value| *value >= 1).ok_or(SettingsError::NotANumber(name)),
            }
        };
        let start_at = number(&self.start_at, "The first statement")?;
        let row = |select: &SelectEntity| select.read(cx).selected_index(cx).map_or(0, |index| index.row);
        let transactions = match row(&self.transactions_select) {
            1 if self.transactions_available() => Transactions::Whole,
            2 if self.transactions_available() => {
                let every = number(&self.every, "The number of statements per commit")?;
                if every > MAX_EVERY {
                    return Err(SettingsError::TooManyPerCommit);
                }
                Transactions::Every(every)
            }
            _ => Transactions::Autocommit,
        };
        let on_error = if row(&self.on_error_select) == 1 && transactions == Transactions::Autocommit { OnError::Continue } else { OnError::Stop };
        Ok(Settings { transactions, on_error, start_at, mysql: matches!(self.kind, ConnectionKind::Mysql | ConnectionKind::Mariadb).then_some(super::head::Comments { version: None, mariadb: self.kind == ConnectionKind::Mariadb, ansi_quotes: false }) })
    }

    pub(in crate::app) fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        match self.step {
            Step::Source if self.can_leave_source() => match self.settings(cx) {
                Ok(_) => {
                    self.step = Step::Settings;
                    self.message = None;
                }
                Err(error) => self.message = Some(error.to_string()),
            },
            Step::Settings => match self.settings(cx) {
                Ok(_) => {
                    self.step = Step::Run;
                    self.message = None;
                }
                Err(error) => self.message = Some(error.to_string()),
            },
            Step::Run if self.report.is_none() => self.begin_run(window, cx),
            _ => {}
        }
        cx.notify();
    }

    pub(in crate::app) fn back(&mut self, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        self.step = match self.step {
            Step::Run => Step::Settings,
            _ => Step::Source,
        };
        self.message = None;
        self.report = None;
        cx.notify();
    }

    // ---- the run -----------------------------------------------------------------------------------

    /// Run: reads the settings and hands the run to the app (which gates and, for Production, asks first).
    pub(super) fn begin_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        let (Some(path), Some(sample)) = (self.path.clone(), self.ready_sample()) else { return };
        let settings = match self.settings(cx) {
            Ok(settings) => settings,
            Err(error) => {
                self.message = Some(error.to_string());
                return cx.notify();
            }
        };
        self.message = None;
        self.report = None;
        let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let request = RunRequest {
            tab_id: self.tab_id,
            target: self.target.clone(),
            path,
            encoding: self.encoding(cx),
            settings,
            summary: format!("The statements of {name} ({})", human_bytes(sample.size)),
        };
        let root = self.root.clone();
        let _ = root.update(cx, |root, cx| root.start_file_run(request, window, cx));
        cx.notify();
    }

    pub(super) fn refused(&mut self, message: String, cx: &mut Context<Self>) {
        self.message = Some(message);
        cx.notify();
    }

    pub(super) fn run_started(&mut self, progress: Arc<Progress>, control: Arc<ApplyControl>, cx: &mut Context<Self>) {
        let total_bytes = self.ready_sample().map_or(0, |sample| sample.size);
        self.run = Some(RunningFile { progress, control, started: Instant::now(), last: ProgressSnapshot::default(), total_bytes, cancelling: false });
        self.report = None;
        self.message = None;
        self.step = Step::Run;
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

    /// "Run from statement N": the same file and settings, starting where the last run left off.
    pub(super) fn resume(&mut self, from: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        self.start_at.update(cx, |state, cx| state.set_value(from.to_string(), window, cx));
        self.report = None;
        self.step = Step::Run;
        self.message = None;
        cx.notify();
    }

    /// "Run another file": back to the first step.
    pub(super) fn again(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.report = None;
        self.step = Step::Source;
        self.browse(window, cx);
        cx.notify();
    }

    pub(super) fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Closing asks the wizard whether it is running: after this update ends.
        let (root, tab_id) = (self.root.clone(), self.tab_id);
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |root, cx| root.close_file_run_tab(tab_id, window, cx));
        });
    }
}

/// What a test can see of a wizard.
#[cfg(test)]
pub(in crate::app) struct Snapshot {
    pub(in crate::app) step: &'static str,
    pub(in crate::app) size: Option<u64>,
    pub(in crate::app) first: Vec<String>,
    pub(in crate::app) more: bool,
    pub(in crate::app) reading: bool,
    pub(in crate::app) sample_error: Option<String>,
    pub(in crate::app) running: bool,
    pub(in crate::app) progress: Option<ProgressSnapshot>,
    pub(in crate::app) report: Option<Report>,
    pub(in crate::app) message: Option<String>,
    pub(in crate::app) target: Namespace,
    pub(in crate::app) connections: Vec<String>,
}

#[cfg(test)]
impl FileRunView {
    pub(in crate::app) fn snapshot(&self) -> Snapshot {
        let sample = self.ready_sample();
        Snapshot {
            step: match self.step {
                Step::Source => "source",
                Step::Settings => "settings",
                Step::Run => "run",
            },
            size: sample.as_ref().map(|sample| sample.size),
            first: sample.as_ref().map(|sample| sample.first.clone()).unwrap_or_default(),
            more: sample.as_ref().is_some_and(|sample| sample.more),
            reading: matches!(self.sample, SampleState::Reading),
            sample_error: match &self.sample {
                SampleState::Failed(message) => Some(message.clone()),
                _ => None,
            },
            running: self.run.is_some(),
            progress: self.run.as_ref().map(|run| run.progress.snapshot()),
            report: self.report.clone(),
            message: self.message.clone(),
            target: self.target.clone(),
            connections: self.connections.iter().map(|connection| connection.name.to_string()).collect(),
        }
    }

    /// Performance runs (a release build draws no debug selectors to click): the steps, the transaction row of the
    /// select (0 autocommit, 1 whole file, 2 every `every`), Run.
    pub(in crate::app) fn start_for_test(&mut self, transactions: usize, every: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.next(window, cx);
        self.transactions_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(transactions)), window, cx));
        self.every.update(cx, |state, cx| state.set_value(every.to_string(), window, cx));
        self.next(window, cx);
        self.begin_run(window, cx);
    }

    /// Captures: a run in flight with these counters.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_progress(&mut self, snapshot: ProgressSnapshot, total_bytes: u64, elapsed: std::time::Duration, cx: &mut Context<Self>) {
        self.run = Some(RunningFile { progress: Arc::new(Progress::default()), control: Arc::new(ApplyControl::default()), started: Instant::now() - elapsed, last: snapshot, total_bytes, cancelling: false });
        self.step = Step::Run;
        cx.notify();
    }

    /// Captures: a run that ended with `report`.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_report(&mut self, report: Report, cx: &mut Context<Self>) {
        self.step = Step::Run;
        self.finished(report, cx);
    }
}

impl Drop for FileRunView {
    fn drop(&mut self) {
        // A tab closed under a running file stops it (statements already committed stay; a transaction rolls back).
        if let Some(run) = self.run.take() {
            std::thread::spawn(move || run.control.cancel());
        }
    }
}
