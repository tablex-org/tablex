//! The wizard of a backup or a restore: where it is (Source), how it is done (Settings) and the run itself (Run),
//! as one entity the tab renders. The state machine and the actions are here; `panels` draws it.
//!
//! The wizard is a tab, not a dialog: a dump of a large database runs for minutes and the rest of the window stays free.
//! The native tool runs as a separate process on a worker thread; the window reads its counters when the worker tells (at
//! most every ~120 ms) and draws only the log lines on screen.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui::{Context, SharedString, Subscription, WeakEntity, Window};
use gpui_component::input::InputEvent;
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::IndexPath;

use super::super::data_transfer::{Namespace, TableNames, TargetConnection};
use super::super::*;
use super::model::{ArchiveState, CheckItem, Checklist, Flag, Loading, Mode, Scope, Step, ToolState};
use super::tab::{RunRequest, Work};
use crate::backup::command::{Content, MysqlDumpOptions, MysqlScope, OptionsError, PgDumpOptions, PgFormat, PgRestoreOptions, PgScope};
use crate::backup::job::{DumpOptions, Report};
use crate::backup::process::{CancelToken, RunState, Snapshot};
use crate::backup::toc::{ArchiveKind, Toc};

type SelectEntity = gpui::Entity<SelectState<Vec<SharedString>>>;

/// What is wrong with the options the widgets hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SettingsError {
    NotANumber(&'static str),
    Options(OptionsError),
    NoOutput,
    NoArchive,
    ArchiveNotReady,
    /// Every entry of the archive was cleared.
    NothingToRestore,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotANumber(name) => write!(f, "{name} must be a whole number"),
            Self::Options(error) => error.fmt(f),
            Self::NoOutput => f.write_str("Choose the file to write the backup to."),
            Self::NoArchive => f.write_str("Choose the backup to restore."),
            Self::ArchiveNotReady => f.write_str("The backup is still being read."),
            Self::NothingToRestore => f.write_str("Nothing is selected to restore: select at least one entry of the backup."),
        }
    }
}

impl std::error::Error for SettingsError {}

impl From<OptionsError> for SettingsError {
    fn from(error: OptionsError) -> Self {
        Self::Options(error)
    }
}

fn select_of(labels: Vec<SharedString>, selected: usize, window: &mut Window, cx: &mut Context<BackupView>) -> SelectEntity {
    cx.new(|cx| SelectState::new(labels, Some(IndexPath::default().row(selected)), window, cx))
}

fn input(placeholder: &'static str, value: &str, window: &mut Window, cx: &mut Context<BackupView>) -> gpui::Entity<InputState> {
    let value = value.to_owned();
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
}

fn labels<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<SharedString> {
    texts.into_iter().map(|text| SharedString::from(text.to_owned())).collect()
}

/// Where the wizard starts.
#[derive(Debug, Clone)]
pub(in crate::app) struct BackupInit {
    pub(in crate::app) mode: Mode,
    pub(in crate::app) target: Namespace,
    /// Tables selected from the start (a backup opened on a table).
    pub(in crate::app) tables: Vec<String>,
    /// The backup to restore, when it is known (dropped, or a test); else the wizard asks.
    pub(in crate::app) archive: Option<PathBuf>,
}

/// The file chosen to restore.
pub(super) struct Archive {
    pub(super) path: PathBuf,
    pub(super) state: ArchiveState,
}

/// A run in flight.
pub(super) struct Running {
    pub(super) state: Arc<RunState>,
    pub(super) cancel: Arc<CancelToken>,
    pub(super) started: Instant,
    pub(super) last: Snapshot,
    pub(super) cancelling: bool,
}

pub(in crate::app) struct BackupView {
    pub(super) root: WeakEntity<AppRoot>,
    pub(super) tab_id: u64,
    pub(super) mode: Mode,
    pub(super) kind: ConnectionKind,
    pub(super) step: Step,

    // ---- source
    pub(super) connections: Vec<TargetConnection>,
    pub(super) connection_select: SelectEntity,
    pub(super) target: Namespace,
    pub(super) namespaces: Vec<Namespace>,
    pub(super) namespace_select: SelectEntity,
    pub(super) scope_select: SelectEntity,
    preselect: Vec<String>,
    pub(super) listing: Loading,
    pub(super) list: Checklist,
    pub(super) filter: gpui::Entity<InputState>,
    pub(super) list_scroll: gpui::UniformListScrollHandle,
    pub(super) archive: Option<Archive>,
    archive_generation: u64,

    // ---- settings
    pub(super) format_select: SelectEntity,
    pub(super) content_select: SelectEntity,
    pub(super) flags: HashSet<Flag>,
    pub(super) compression: gpui::Entity<InputState>,
    pub(super) jobs: gpui::Entity<InputState>,
    pub(super) output: Option<PathBuf>,
    pub(super) toc: Option<Arc<Toc>>,
    pub(super) toc_loading: Loading,
    /// The rows (start, end) the log and the checklist drew last: only the ones on screen.
    #[cfg(test)]
    pub(super) drawn: std::cell::Cell<[(usize, usize); 2]>,
    toc_generation: u64,
    /// Ends the `pg_restore --list` in flight (a new archive, a closed tab).
    toc_cancel: Arc<CancelToken>,


    // ---- run
    pub(super) tool: ToolState,
    tool_generation: u64,
    pub(super) run: Option<Running>,
    pub(super) report: Option<Report>,
    /// The log of the last run, kept for the summary.
    pub(super) log: Option<Arc<RunState>>,
    pub(in crate::app) log_scroll: gpui::UniformListScrollHandle,
    /// The log follows its end. Once the window has scrolled it there (`log_scrolled`), where it is at each report decides: at its end
    /// it follows, moved off it (the wheel, a scrollbar drag, a key) it is left where the user put it.
    log_following: bool,
    log_scrolled: bool,
    /// Why the wizard cannot go on, shown above the buttons.
    pub(super) message: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl BackupView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(root: WeakEntity<AppRoot>, tab_id: u64, init: BackupInit, kind: ConnectionKind, connections: Vec<TargetConnection>, namespaces: Vec<Namespace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let BackupInit { mode, target, tables, archive } = init;
        let current = connections.iter().position(|connection| connection.id == target.connection).unwrap_or(0);
        let connection_select = select_of(connections.iter().map(|connection| connection.name.clone()).collect(), current, window, cx);
        let position = namespaces.iter().position(|namespace| *namespace == target).unwrap_or(0);
        let namespace_select = select_of(namespaces.iter().map(Namespace::label).collect(), position, window, cx);
        let scope = if tables.is_empty() { 0 } else { Scope::choices(kind).iter().position(|scope| *scope == Scope::Tables).unwrap_or(0) };
        let mut view = Self {
            root,
            tab_id,
            mode,
            kind,
            step: Step::Source,
            connections,
            connection_select,
            target,
            namespaces,
            namespace_select,
            scope_select: select_of(labels(Scope::choices(kind).iter().map(|scope| scope.label())), scope, window, cx),
            preselect: tables,
            listing: Loading::Idle,
            list: Checklist::default(),
            filter: input("Filter", "", window, cx),
            list_scroll: gpui::UniformListScrollHandle::new(),
            archive: archive.map(|path| Archive { path, state: ArchiveState::Detecting }),
            archive_generation: 0,
            format_select: select_of(labels(PgFormat::ALL.iter().map(|format| format.label())), 0, window, cx),
            content_select: select_of(labels(Content::ALL.iter().map(|content| content.label())), 0, window, cx),
            flags: Flag::defaults(),
            compression: input("default", "", window, cx),
            jobs: input("1", "", window, cx),
            output: None,
            toc: None,
            toc_loading: Loading::Idle,
            #[cfg(test)]
            drawn: Default::default(),
            toc_generation: 0,
            toc_cancel: Arc::new(CancelToken::default()),
            tool: ToolState::Unknown,
            tool_generation: 0,
            run: None,
            report: None,
            log: None,
            log_scroll: gpui::UniformListScrollHandle::new(),
            log_following: true,
            log_scrolled: false,
            message: None,
            _subscriptions: Vec::new(),
        };
        view.wire(window, cx);
        view
    }

    fn wire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
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
                    this.target_changed(cx);
                }
            }
        }));
        subscriptions.push(cx.subscribe(&self.scope_select, |this: &mut Self, _, event: &SelectEvent<Vec<SharedString>>, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                this.reload_list(cx);
            }
        }));
        for select in [&self.format_select, &self.content_select] {
            subscriptions.push(cx.subscribe(select, |this: &mut Self, _, _: &SelectEvent<Vec<SharedString>>, cx| {
                this.message = None;
                this.tool = ToolState::Unknown;
                cx.notify();
            }));
        }
        subscriptions.push(cx.subscribe(&self.filter, |this: &mut Self, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let needle = state.read(cx).value().to_string();
                this.active_list_mut().refilter(&needle);
                cx.notify();
            }
        }));
        for state in [&self.compression, &self.jobs] {
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

    // ---- what the choices mean -----------------------------------------------------------------------

    fn row(&self, select: &SelectEntity, cx: &gpui::App) -> usize {
        select.read(cx).selected_index(cx).map_or(0, |index| index.row)
    }

    pub(super) fn scope(&self, cx: &gpui::App) -> Scope {
        let choices = Scope::choices(self.kind);
        choices[self.row(&self.scope_select, cx).min(choices.len() - 1)]
    }

    pub(super) fn format(&self, cx: &gpui::App) -> PgFormat {
        PgFormat::ALL[self.row(&self.format_select, cx).min(PgFormat::ALL.len() - 1)]
    }

    pub(super) fn content(&self, cx: &gpui::App) -> Content {
        Content::ALL[self.row(&self.content_select, cx).min(Content::ALL.len() - 1)]
    }

    pub(super) fn flag(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }

    pub(super) fn toggle(&mut self, flag: Flag, on: bool, cx: &mut Context<Self>) {
        if on {
            self.flags.insert(flag);
        } else {
            self.flags.remove(&flag);
        }
        self.message = None;
        cx.notify();
    }

    pub(super) fn postgres(&self) -> bool {
        self.kind == ConnectionKind::Postgres
    }

    /// Whether `flag` can be set now (the format or another option rules it out).
    pub(super) fn flag_enabled(&self, flag: Flag, cx: &gpui::App) -> bool {
        match flag {
            Flag::Clean | Flag::Create => self.format(cx) == PgFormat::Plain && self.content(cx) != Content::DataOnly,
            Flag::CreateDatabase => self.scope(cx) == Scope::Whole,
            Flag::AddDropDatabase => self.scope(cx) == Scope::Whole && self.flag(Flag::CreateDatabase),
            Flag::RestoreSingleTransaction => self.jobs_value(cx).is_none_or(|jobs| jobs <= 1),
            _ => true,
        }
    }

    /// The options drawn on the Settings step, in order.
    pub(super) fn visible_flags(&self) -> &'static [Flag] {
        match (self.mode, self.postgres()) {
            (Mode::Backup, true) => &[Flag::NoOwner, Flag::NoPrivileges, Flag::Clean, Flag::Create],
            (Mode::Backup, false) => &[Flag::SingleTransaction, Flag::Routines, Flag::Triggers, Flag::Events, Flag::NoData, Flag::AddDropTable, Flag::ExtendedInsert, Flag::CreateDatabase, Flag::AddDropDatabase],
            (Mode::Restore, _) => &[Flag::RestoreClean, Flag::RestoreNoOwner, Flag::RestoreSingleTransaction],
        }
    }

    pub(super) fn shows_compression(&self, cx: &gpui::App) -> bool {
        self.mode == Mode::Backup && self.postgres() && matches!(self.format(cx), PgFormat::Custom | PgFormat::Directory)
    }

    /// Parallel jobs: a directory backup, and the restore of a custom or directory archive (`pg_restore` refuses them for a tar).
    pub(super) fn shows_jobs(&self, cx: &gpui::App) -> bool {
        self.postgres()
            && match self.mode {
                Mode::Backup => self.format(cx) == PgFormat::Directory,
                Mode::Restore => matches!(self.restorable(), Some((_, ArchiveKind::Custom | ArchiveKind::Directory))),
            }
    }

    fn number(state: &gpui::Entity<InputState>, cx: &gpui::App) -> Result<Option<u32>, ()> {
        let text = state.read(cx).value().to_string();
        match text.trim() {
            "" => Ok(None),
            digits => digits.parse::<u32>().map(Some).map_err(|_| ()),
        }
    }

    fn jobs_value(&self, cx: &gpui::App) -> Option<u32> {
        Self::number(&self.jobs, cx).ok().flatten()
    }

    /// The archive chosen, once it is known to be one `pg_restore` reads: of a PostgreSQL connection (MySQL and MariaDB have
    /// no archives, and a connection switched to one after the choice leaves nothing to restore).
    pub(super) fn restorable(&self) -> Option<(&PathBuf, ArchiveKind)> {
        match &self.archive {
            Some(Archive { path, state: ArchiveState::Ready(kind) }) if *kind != ArchiveKind::Plain && self.postgres() => Some((path, *kind)),
            _ => None,
        }
    }

    /// A pg_dump archive chosen for a MySQL or MariaDB connection: it restores into PostgreSQL only.
    pub(super) fn archive_for_other_server(&self) -> bool {
        matches!(&self.archive, Some(Archive { state: ArchiveState::Ready(kind), .. }) if *kind != ArchiveKind::Plain) && !self.postgres()
    }

    /// An SQL script, which Run SQL File restores: every file of a MySQL or MariaDB connection, a plain dump of PostgreSQL.
    pub(super) fn script(&self) -> Option<&PathBuf> {
        match &self.archive {
            Some(Archive { path, state: ArchiveState::Ready(ArchiveKind::Plain) }) => Some(path),
            _ => None,
        }
    }

    // ---- the checklist -------------------------------------------------------------------------------

    pub(super) fn active_list_mut(&mut self) -> &mut Checklist {
        &mut self.list
    }

    fn filter_text(&self, cx: &gpui::App) -> String {
        self.filter.read(cx).value().to_string()
    }

    /// What the scope select asks to list: the schemas of the database, or the tables of the namespace.
    pub(super) fn reload_list(&mut self, cx: &mut Context<Self>) {
        self.message = None;
        self.list = Checklist::default();
        match self.scope(cx) {
            Scope::Whole => self.listing = Loading::Idle,
            Scope::Schemas => {
                let schemas: Vec<String> = self.namespaces.iter().filter(|namespace| namespace.database == self.target.database && !namespace.schema.is_empty()).map(|namespace| namespace.schema.clone()).collect();
                let wanted = std::mem::take(&mut self.preselect);
                let items = schemas.into_iter().enumerate().map(|(index, schema)| CheckItem { selected: wanted.contains(&schema), label: schema.into(), id: index as u32 }).collect();
                let needle = self.filter_text(cx);
                self.list.replace(items, &needle);
                self.listing = Loading::Ready;
            }
            Scope::Tables => {
                self.listing = Loading::Pending;
                let target = self.target.clone();
                let _ = self.root.update(cx, |root, cx| root.transfer_load_tables(&target, cx));
                self.poll_listing(cx);
            }
        }
        cx.notify();
    }

    /// The tables of the namespace, once the navigator has them.
    pub(super) fn poll_listing(&mut self, cx: &mut Context<Self>) {
        if self.listing != Loading::Pending {
            return;
        }
        let target = self.target.clone();
        let Ok(names) = self.root.read_with(cx, |root, _| root.transfer_table_names(&target)) else { return };
        match names {
            TableNames::Loading => {}
            TableNames::Failed(message) => self.listing = Loading::Failed(message),
            TableNames::Loaded(names) => {
                let wanted = std::mem::take(&mut self.preselect);
                let items = names.into_iter().enumerate().map(|(index, name)| CheckItem { selected: wanted.contains(&name), label: name.into(), id: index as u32 }).collect();
                let needle = self.filter_text(cx);
                self.list.replace(items, &needle);
                self.listing = Loading::Ready;
            }
        }
    }

    pub(super) fn set_checked(&mut self, index: usize, selected: bool, cx: &mut Context<Self>) {
        self.list.set(index, selected);
        self.message = None;
        cx.notify();
    }

    pub(super) fn select_shown(&mut self, selected: bool, cx: &mut Context<Self>) {
        self.list.set_shown(selected);
        self.message = None;
        cx.notify();
    }

    // ---- the source ----------------------------------------------------------------------------------

    /// Asks for the backup to restore; a backup starts with the lists of its scope.
    pub(super) fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Backup => self.reload_list(cx),
            Mode::Restore => match self.archive.as_ref().map(|archive| archive.path.clone()) {
                Some(path) => self.set_archive(path, window, cx),
                None => self.browse_archive(false, window, cx),
            },
        }
    }

    /// The platform's own picker: a file, or (a directory archive) a folder.
    pub(super) fn browse_archive(&mut self, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions { files: !folder, directories: folder, multiple: false, prompt: Some("Restore".into()) });
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
                let _ = this.update_in(cx, |this, window, cx| this.set_archive(path, window, cx));
            }
        })
        .detach();
    }

    /// Looks at the file off the UI thread: which kind of backup it is.
    pub(in crate::app) fn set_archive(&mut self, path: PathBuf, _window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        self.archive_generation += 1;
        let generation = self.archive_generation;
        self.archive = Some(Archive { path: path.clone(), state: ArchiveState::Detecting });
        self.toc_cancel.cancel();
        self.toc = None;
        self.toc_loading = Loading::Idle;
        self.report = None;
        self.message = None;
        self.step = Step::Source;
        cx.notify();
        let work = cx.background_executor().spawn(async move { smol::unblock(move || crate::backup::toc::detect_archive(&path)).await });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                // An answer for a file that is not the current one is dropped.
                if this.archive_generation != generation {
                    return;
                }
                if let Some(archive) = &mut this.archive {
                    archive.state = match result {
                        Ok(kind) => ArchiveState::Ready(kind),
                        Err(error) => ArchiveState::Failed(error.to_string()),
                    };
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_connection(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connections.get(row).cloned() else { return };
        let namespaces = self.root.read_with(cx, |root, _| root.transfer_namespaces(&connection.id, &self.target)).unwrap_or_default();
        self.kind = connection.kind;
        self.target = namespaces.first().cloned().unwrap_or(Namespace { connection: connection.id.clone(), database: String::new(), schema: String::new() });
        let names: Vec<SharedString> = namespaces.iter().map(Namespace::label).collect();
        self.namespaces = namespaces;
        let (select, scope) = (self.namespace_select.clone(), self.scope_select.clone());
        let scope_labels = labels(Scope::choices(self.kind).iter().map(|scope| scope.label()));
        select.update(cx, |select, cx| {
            select.set_items(names, window, cx);
            select.set_selected_index(Some(IndexPath::default().row(0)), window, cx);
        });
        // Another kind of server has other choices and other options.
        scope.update(cx, |select, cx| {
            select.set_items(scope_labels, window, cx);
            select.set_selected_index(Some(IndexPath::default().row(0)), window, cx);
        });
        self.output = None;
        self.target_changed(cx);
    }

    /// The tab names the connection and namespace the run will use; the lists are read again.
    fn target_changed(&mut self, cx: &mut Context<Self>) {
        self.message = None;
        self.tool = ToolState::Unknown;
        self.reload_list(cx);
        let (root, tab_id, target) = (self.root.clone(), self.tab_id, self.target.clone());
        let _ = root.update(cx, |root, cx| root.backup_target_changed(tab_id, &target, cx));
        cx.notify();
    }

    pub(super) fn connection_name(&self) -> SharedString {
        self.connections.iter().find(|connection| connection.id == self.target.connection).map_or_else(|| "the connection".into(), |connection| connection.name.clone())
    }

    /// The database the run works on.
    pub(super) fn database(&self) -> &str {
        &self.target.database
    }

    // ---- the steps -----------------------------------------------------------------------------------

    pub(super) fn can_leave_source(&self) -> bool {
        let connected = self.connections.iter().any(|connection| connection.id == self.target.connection);
        match self.mode {
            Mode::Backup => connected && !self.target.database.is_empty(),
            Mode::Restore => connected && !self.target.database.is_empty() && self.restorable().is_some(),
        }
    }

    pub(in crate::app) fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        match self.step {
            Step::Source if self.can_leave_source() => {
                if self.mode == Mode::Backup && self.scope(cx) != Scope::Whole && self.list.selected() == 0 && self.listing == Loading::Ready {
                    self.message = Some(OptionsError::NothingSelected.to_string());
                } else {
                    self.step = Step::Settings;
                    self.message = None;
                    self.tool = ToolState::Unknown;
                    if self.mode == Mode::Restore {
                        self.load_toc(cx);
                    }
                }
            }
            Step::Settings => match self.check_settings(cx) {
                Ok(()) => {
                    self.step = Step::Run;
                    self.message = None;
                    self.check_tool(cx);
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

    // ---- the settings --------------------------------------------------------------------------------

    /// The suggested name of the file: the database and the moment, with the extension of the format.
    pub(super) fn default_file_name(&self, cx: &gpui::App) -> String {
        let database: String = self.target.database.chars().map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') { ch } else { '_' }).collect();
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let extension = if self.postgres() { self.format(cx).extension() } else { Some("sql") };
        match extension {
            Some(extension) => format!("{database}-{stamp}.{extension}"),
            None => format!("{database}-{stamp}"),
        }
    }

    pub(super) fn choose_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.default_file_name(cx);
        let directory = self.output.as_ref().and_then(|path| path.parent().map(PathBuf::from)).unwrap_or_else(|| {
            let folder = self.root.read_with(cx, |root, _| root.settings.results.export.folder.trim().to_owned()).unwrap_or_default();
            Some(folder).filter(|folder| !folder.is_empty()).map(PathBuf::from).filter(|folder| folder.is_dir()).unwrap_or_else(crate::platform::save_dialog_dir)
        });
        let prompt = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn_in(window, async move |this, cx| match prompt.await {
            Ok(Ok(Some(path))) => {
                let _ = this.update(cx, |this, cx| {
                    this.output = Some(path);
                    this.message = None;
                    cx.notify();
                });
            }
            Ok(Ok(None)) => {}
            _ => {
                let _ = this.update(cx, |this, cx| {
                    this.message = Some("Could not open the save dialog.".to_owned());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn pg_scope(&self, cx: &gpui::App) -> Result<PgScope, SettingsError> {
        Ok(match self.scope(cx) {
            Scope::Whole => PgScope::Everything,
            Scope::Schemas => PgScope::Schemas(self.list.selected_labels().into_iter().map(str::to_owned).collect()),
            Scope::Tables => PgScope::Tables(self.list.selected_labels().into_iter().map(|table| (self.target.schema.clone(), table.to_owned())).collect()),
        })
    }

    fn pg_dump_options(&self, cx: &gpui::App) -> Result<PgDumpOptions, SettingsError> {
        let format = self.format(cx);
        let compression = if self.shows_compression(cx) { Self::number(&self.compression, cx).map_err(|()| SettingsError::NotANumber("The compression level"))? } else { None };
        let jobs = if self.shows_jobs(cx) { Self::number(&self.jobs, cx).map_err(|()| SettingsError::NotANumber("The number of jobs"))? } else { None };
        let plain_structure = self.flag_enabled(Flag::Clean, cx);
        let options = PgDumpOptions {
            format,
            content: self.content(cx),
            scope: self.pg_scope(cx)?,
            no_owner: self.flag(Flag::NoOwner),
            no_privileges: self.flag(Flag::NoPrivileges),
            clean: plain_structure && self.flag(Flag::Clean),
            create: plain_structure && self.flag(Flag::Create),
            compression,
            jobs,
        };
        options.validate()?;
        Ok(options)
    }

    fn mysql_dump_options(&self, cx: &gpui::App) -> Result<MysqlDumpOptions, SettingsError> {
        let whole = self.scope(cx) == Scope::Whole;
        let options = MysqlDumpOptions {
            scope: if whole { MysqlScope::Database } else { MysqlScope::Tables(self.list.selected_labels().into_iter().map(str::to_owned).collect()) },
            single_transaction: self.flag(Flag::SingleTransaction),
            routines: self.flag(Flag::Routines),
            triggers: self.flag(Flag::Triggers),
            events: self.flag(Flag::Events),
            no_data: self.flag(Flag::NoData),
            add_drop_table: self.flag(Flag::AddDropTable),
            extended_insert: self.flag(Flag::ExtendedInsert),
            create_database: whole && self.flag(Flag::CreateDatabase),
            add_drop_database: whole && self.flag(Flag::CreateDatabase) && self.flag(Flag::AddDropDatabase),
        };
        options.validate()?;
        Ok(options)
    }

    fn restore_options(&self, cx: &gpui::App) -> Result<PgRestoreOptions, SettingsError> {
        let jobs = if self.shows_jobs(cx) { Self::number(&self.jobs, cx).map_err(|()| SettingsError::NotANumber("The number of jobs"))? } else { None };
        let options = PgRestoreOptions { clean: self.flag(Flag::RestoreClean), no_owner: self.flag(Flag::RestoreNoOwner), single_transaction: self.flag(Flag::RestoreSingleTransaction), jobs, list_file: None };
        options.validate()?;
        Ok(options)
    }

    /// What is wrong with the settings, if anything.
    fn check_settings(&self, cx: &gpui::App) -> Result<(), SettingsError> {
        self.work(cx).map(|_| ())
    }

    /// The work the settings ask for.
    pub(super) fn work(&self, cx: &gpui::App) -> Result<Work, SettingsError> {
        match self.mode {
            Mode::Backup => {
                let options = if self.postgres() { DumpOptions::Postgres(self.pg_dump_options(cx)?) } else { DumpOptions::Mysql(self.mysql_dump_options(cx)?) };
                let output = self.output.clone().ok_or(SettingsError::NoOutput)?;
                Ok(Work::Backup { options, output })
            }
            Mode::Restore => {
                let (path, _) = self.restorable().ok_or(if self.archive.is_some() { SettingsError::ArchiveNotReady } else { SettingsError::NoArchive })?;
                let selection = match &self.toc {
                    Some(_) if self.list.selected() == 0 => return Err(SettingsError::NothingToRestore),
                    Some(toc) if self.list.selected() < self.list.items.len() => Some((toc.clone(), self.list.selected_ids())),
                    _ => None,
                };
                Ok(Work::Restore { archive: path.clone(), options: self.restore_options(cx)?, selection })
            }
        }
    }

    // ---- the table of contents -----------------------------------------------------------------------

    /// Reads the table of contents of the archive with `pg_restore --list`, off the UI thread.
    fn load_toc(&mut self, cx: &mut Context<Self>) {
        let Some((path, _)) = self.restorable().map(|(path, kind)| (path.clone(), kind)) else { return };
        self.toc_generation += 1;
        let generation = self.toc_generation;
        self.toc_cancel.cancel();
        let cancel = Arc::new(CancelToken::default());
        self.toc_cancel = cancel.clone();
        self.toc_loading = Loading::Pending;
        self.toc = None;
        self.list = Checklist::default();
        let (paths, server_major) = self.root.read_with(cx, |root, _| (root.settings.query.tool_paths(), root.backup_server_major(&self.target.connection))).unwrap_or_default();
        let work = cx.background_executor().spawn(async move { smol::unblock(move || crate::backup::job::list_toc(&path, &paths, &crate::backup::tools::Locator::system(), server_major, &cancel)).await });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.toc_generation != generation {
                    return;
                }
                match result {
                    Ok(toc) => {
                        let items = toc.entries.iter().map(|entry| CheckItem { label: entry.label().into(), id: entry.id, selected: true }).collect();
                        let needle = this.filter_text(cx);
                        this.list.replace(items, &needle);
                        this.toc = Some(Arc::new(toc));
                        this.toc_loading = Loading::Ready;
                    }
                    Err(error) => this.toc_loading = Loading::Failed(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- the tool ------------------------------------------------------------------------------------

    /// Finds the tool the run would use, so the Run step can say which, or why there is none.
    fn check_tool(&mut self, cx: &mut Context<Self>) {
        self.tool_generation += 1;
        let generation = self.tool_generation;
        self.tool = ToolState::Checking;
        let (kind, restore) = (self.kind, self.mode == Mode::Restore);
        let (paths, server_major) = self.root.read_with(cx, |root, _| (root.settings.query.tool_paths(), root.backup_server_major(&self.target.connection))).unwrap_or_default();
        let work = cx.background_executor().spawn(async move { smol::unblock(move || crate::backup::job::find_tool(kind, restore, &paths, &crate::backup::tools::Locator::system(), server_major)).await });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.tool_generation != generation {
                    return;
                }
                this.tool = match result {
                    Ok(found) => ToolState::Found(found),
                    Err(error) => ToolState::Failed(error.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    // ---- the run -------------------------------------------------------------------------------------

    /// What the user will see run: for the confirmation of a Production connection, and the summary line.
    pub(super) fn summary_text(&self, work: &Work) -> String {
        match work {
            Work::Backup { output, .. } => format!("A backup of {} on {} to {}", self.target.database, self.connection_name(), output.display()),
            Work::Restore { archive, .. } => format!("The backup {} restored into {} on {}", archive.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned()), self.target.database, self.connection_name()),
        }
    }

    /// Run: reads the settings and hands the run to the app (which gates and, for Production, asks first).
    pub(super) fn begin_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        let work = match self.work(cx) {
            Ok(work) => work,
            Err(error) => {
                self.message = Some(error.to_string());
                return cx.notify();
            }
        };
        if matches!(self.tool, ToolState::Failed(_) | ToolState::Checking) {
            self.message = Some(match &self.tool {
                ToolState::Failed(message) => message.clone(),
                _ => "Looking for the tool\u{2026}".to_owned(),
            });
            return cx.notify();
        }
        self.message = None;
        self.report = None;
        let request = RunRequest { tab_id: self.tab_id, target: self.target.clone(), summary: self.summary_text(&work), work };
        let root = self.root.clone();
        let _ = root.update(cx, |root, cx| root.start_backup_run(request, window, cx));
        cx.notify();
    }

    pub(super) fn refused(&mut self, message: String, cx: &mut Context<Self>) {
        self.message = Some(message);
        cx.notify();
    }

    pub(super) fn run_started(&mut self, state: Arc<RunState>, cancel: Arc<CancelToken>, cx: &mut Context<Self>) {
        self.log = Some(state.clone());
        self.run = Some(Running { state, cancel, started: Instant::now(), last: Snapshot::default(), cancelling: false });
        self.report = None;
        self.message = None;
        self.step = Step::Run;
        cx.notify();
    }

    /// The worker reported: read the counters, and follow the end of the log.
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &mut self.run else { return };
        run.last = run.state.snapshot();
        let rows = run.last.log_len;
        self.log_look();
        if rows > 0 && self.log_following {
            self.log_scrolled = true;
            self.log_scroll.scroll_to_item(rows - 1, gpui::ScrollStrategy::Bottom);
        }
        cx.notify();
    }

    /// Decides whether the log follows its end from where it is now, once the scroll that put it there has been applied.
    fn log_look(&mut self) {
        let applied = self.log_scroll.0.borrow().deferred_scroll_to_item.is_none();
        if self.log_scrolled && applied {
            self.log_following = self.log_pinned();
        }
    }

    /// Whether the log is scrolled to its end (or has nothing to scroll yet).
    pub(super) fn log_pinned(&self) -> bool {
        let state = self.log_scroll.0.borrow();
        let max = state.base_handle.max_offset().y;
        let shown = -state.base_handle.offset().y;
        max <= gpui::px(1.0) || shown >= max - gpui::px(3.0)
    }

    pub(super) fn finished(&mut self, report: Report, cx: &mut Context<Self>) {
        if let Some(run) = self.run.take() {
            self.log_look();
            if run.last.log_len > 0 && self.log_following {
                self.log_scroll.scroll_to_item(run.state.log_len().saturating_sub(1), gpui::ScrollStrategy::Bottom);
            }
        }
        self.report = Some(report);
        cx.notify();
    }

    /// Cancel: the tool and what it started are stopped, off the UI thread.
    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &mut self.run else { return };
        if std::mem::replace(&mut run.cancelling, true) {
            return;
        }
        let cancel = run.cancel.clone();
        cx.background_executor().spawn(async move { smol::unblock(move || cancel.cancel()).await }).detach();
        cx.notify();
    }

    /// "Back up again" / "Restore another": the first step.
    pub(super) fn again(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.report = None;
        self.log = None;
        self.step = Step::Source;
        self.tool = ToolState::Unknown;
        if self.mode == Mode::Restore {
            self.browse_archive(false, window, cx);
        }
        cx.notify();
    }

    /// "Open in Run SQL File": the script goes through the file runner.
    pub(super) fn open_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.script().cloned() else { return };
        let (root, tab_id, target) = (self.root.clone(), self.tab_id, self.target.clone());
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |root, cx| root.open_restore_script(tab_id, target, path, window, cx));
        });
    }

    pub(super) fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Closing asks the wizard whether it is running: after this update ends.
        let (root, tab_id) = (self.root.clone(), self.tab_id);
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |root, cx| root.close_backup_tab(tab_id, window, cx));
        });
    }
}

/// What a test can see of a wizard.
#[cfg(test)]
pub(in crate::app) struct ViewSnapshot {
    pub(in crate::app) mode: Mode,
    pub(in crate::app) step: &'static str,
    pub(in crate::app) target: Namespace,
    pub(in crate::app) connections: Vec<String>,
    pub(in crate::app) message: Option<String>,
    pub(in crate::app) listed: Vec<(String, bool)>,
    pub(in crate::app) archive: Option<(PathBuf, String)>,
    pub(in crate::app) toc_entries: usize,
    pub(in crate::app) output: Option<PathBuf>,
    pub(in crate::app) tool: String,
    pub(in crate::app) running: bool,
    pub(in crate::app) progress: Option<Snapshot>,
    pub(in crate::app) report: Option<Report>,
    pub(in crate::app) log: Vec<String>,
    /// The rows (start, end) the log and the checklist drew last.
    pub(in crate::app) drawn_log: (usize, usize),
    pub(in crate::app) drawn_list: (usize, usize),
    pub(in crate::app) log_pinned: bool,
    pub(in crate::app) can_leave_source: bool,
    pub(in crate::app) wrong_server_note: bool,
    pub(in crate::app) log_top: usize,
}

#[cfg(test)]
impl BackupView {
    pub(in crate::app) fn snapshot(&self) -> ViewSnapshot {
        ViewSnapshot {
            mode: self.mode,
            step: match self.step {
                Step::Source => "source",
                Step::Settings => "settings",
                Step::Run => "run",
            },
            target: self.target.clone(),
            connections: self.connections.iter().map(|connection| connection.name.to_string()).collect(),
            message: self.message.clone(),
            listed: self.list.items.iter().map(|item| (item.label.to_string(), item.selected)).collect(),
            archive: self.archive.as_ref().map(|archive| {
                (
                    archive.path.clone(),
                    match &archive.state {
                        ArchiveState::Detecting => "detecting".to_owned(),
                        ArchiveState::Ready(kind) => kind.label().to_owned(),
                        ArchiveState::Failed(message) => format!("failed: {message}"),
                    },
                )
            }),
            toc_entries: self.toc.as_ref().map_or(0, |toc| toc.entries.len()),
            output: self.output.clone(),
            tool: match &self.tool {
                ToolState::Unknown => "unknown".to_owned(),
                ToolState::Checking => "checking".to_owned(),
                ToolState::Found(found) => found.describe(),
                ToolState::Failed(message) => format!("failed: {message}"),
            },
            running: self.run.is_some(),
            progress: self.run.as_ref().map(|run| run.state.snapshot()),
            report: self.report.clone(),
            log: self.log.as_ref().map(|log| log.range(0..log.log_len()).into_iter().map(|line| line.text).collect()).unwrap_or_default(),
            drawn_log: self.drawn.get()[0],
            drawn_list: self.drawn.get()[1],
            log_pinned: self.log_pinned(),
            can_leave_source: self.can_leave_source(),
            wrong_server_note: self.archive_for_other_server(),
            log_top: self.log_scroll.logical_scroll_top_index(),
        }
    }

    /// As if the connection of the wizard was switched to a `kind` one with this database.
    pub(in crate::app) fn switch_for_test(&mut self, kind: ConnectionKind, database: &str, cx: &mut Context<Self>) {
        self.kind = kind;
        self.target.database = database.to_owned();
        cx.notify();
    }

    /// The rows the filter shows.
    pub(in crate::app) fn shown_for_test(&self) -> usize {
        self.list.shown.len()
    }

    /// Captures: the Settings step with this format (a row of the select) and output.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_options(&mut self, format_row: usize, output: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.format_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(format_row)), window, cx));
        self.output = Some(output);
        self.step = Step::Settings;
        cx.notify();
    }

    /// Captures: a run in flight with this log.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_run(&mut self, state: Arc<RunState>, elapsed: std::time::Duration, cx: &mut Context<Self>) {
        let last = state.snapshot();
        self.log = Some(state.clone());
        self.run = Some(Running { state, cancel: Arc::new(CancelToken::default()), started: Instant::now() - elapsed, last, cancelling: false });
        self.step = Step::Run;
        cx.notify();
    }

    /// Captures: a run that ended with `report` and this log.
    #[cfg(feature = "bench")]
    pub(in crate::app) fn seed_report(&mut self, report: Report, state: Arc<RunState>, cx: &mut Context<Self>) {
        self.log = Some(state);
        self.step = Step::Run;
        self.finished(report, cx);
    }
}

impl Drop for BackupView {
    fn drop(&mut self) {
        self.toc_cancel.cancel();
        // A tab closed under a running tool stops it, off the thread that closes the tab.
        if let Some(run) = self.run.take() {
            std::thread::spawn(move || run.cancel.cancel());
        }
    }
}
