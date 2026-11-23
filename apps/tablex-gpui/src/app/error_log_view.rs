//! Error Log pane: the status-bar icon, the resizable pane (toolbar,
//! severity pills, search, virtualized table), the Event details dialog and
//! the `report_*` entry points every failure site calls.
//!
//! Where the pane shows is the `appearance.error_log_place` setting (the position button in its
//! toolbar and the Settings page change the same value, so it survives a restart): the bottom or
//! the right side of the main area, or a tab of its own (`TabKind::ErrorLog`). It is one view
//! (`ErrorLogUi` holds its filter, search, selection, sort and scroll) embedded where the place
//! says, so moving it keeps all of that. Closing it, from its button, Escape, the status-bar icon
//! or by closing its tab, closes the Error Log whatever the place; the place stays, and opening it
//! again shows it there. A toggle (icon, Cmd/Ctrl+Shift+L) from another tab brings the Error Log
//! tab forward; from the tab itself it closes it. There is no place of its own window: that would
//! need a second window drawing through `AppRoot` (the results window's machinery, with its
//! window-bound inputs and callback caveats), and the search input and dialogs belong to the app's
//! window.
//!
//! Costs. With the pane closed the only per-frame work is `unseen_*` reads
//! for the icon and one `Vec::is_empty` check for queued file writes. With it
//! open the visible-row index list is rebuilt only when the log revision or
//! the filter/sort spec changes; rows clone stored `SharedString`s. Row
//! interaction is one hit-tested mouse-down handler on the list body (the
//! results-grid pattern), never a listener per row. File I/O runs on the
//! background executor.

use super::*;
use crate::error_log::{
    self as model, Area, ErrorLog, LogEntry, LogStore, Recorded, Report, Severity, SeverityFilter,
    SortKey, ViewSpec, WriteOp,
};
use gpui::{ClipboardItem, Entity, SharedString, Subscription};
use gpui_component::input::InputEvent;
use crate::settings::ErrorLogPlace;
use gpui_component::resizable::{h_resizable, resizable_panel, v_resizable, ResizableState};
use std::time::SystemTime;

const DEFAULT_PANE_HEIGHT: f32 = 200.0;
const MIN_PANE_HEIGHT: f32 = 96.0;
/// The right-hand pane opens at this share of the content area (the window beside the sidebar),
/// but never narrower than `MIN_DEFAULT_PANE_WIDTH`; the divider still moves it down to
/// `MIN_PANE_WIDTH`.
const DEFAULT_PANE_SHARE: f32 = 0.4;
const MIN_DEFAULT_PANE_WIDTH: f32 = 360.0;
const MIN_PANE_WIDTH: f32 = 320.0;
/// What the main area keeps beside a right-hand pane.
const MIN_UPPER_WIDTH: f32 = 320.0;
const SOURCE_COL_WIDTH: f32 = 200.0;
const DATE_COL_WIDTH: f32 = 170.0;
/// The Message column never gets narrower than this, gutter and gap included (it clips its text;
/// it is never one letter per line).
const MESSAGE_COL_MIN_WIDTH: f32 = 160.0;
/// Severity icon (13px) + gap (6px): the Message column's leading gutter.
const ICON_GUTTER: f32 = 19.0;
/// Horizontal padding shared by the header and every row.
const ROW_PADDING_X: f32 = 12.0;
/// Space between a column's text and the next column.
const CELL_GAP: f32 = 8.0;

/// Icon paths registered in `assets.rs`.
#[derive(Clone, Copy)]
pub(super) enum ErrorLogGlyph {
    Alert,
    Info,
    Error,
    Copy,
    Export,
    OpenFile,
    Clear,
    Delete,
    Close,
    SortUp,
    SortDown,
}

impl gpui_component::IconNamed for ErrorLogGlyph {
    fn path(self) -> SharedString {
        match self {
            Self::Alert => "icons/triangle-alert.svg",
            Self::Info => "icons/info.svg",
            Self::Error => "icons/circle-x.svg",
            Self::Copy => "icons/copy.svg",
            Self::Export => "icons/download.svg",
            Self::OpenFile => "icons/folder-open.svg",
            Self::Clear => "icons/eraser.svg",
            Self::Delete => "icons/trash.svg",
            Self::Close => "icons/close.svg",
            Self::SortUp => "icons/arrow-up.svg",
            Self::SortDown => "icons/arrow-down.svg",
        }
        .into()
    }
}

impl ErrorLogPlace {
    const ALL: [Self; 3] = [Self::Bottom, Self::Right, Self::Tab];

    /// The Results place with the same icon and slug (the shared placement control).
    fn results_place(self) -> results::placement::ResultsPlace {
        use results::placement::ResultsPlace;
        match self {
            Self::Bottom => ResultsPlace::Below,
            Self::Right => ResultsPlace::Right,
            Self::Tab => ResultsPlace::Tab,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Bottom => t!("error_log.place_bottom").into_owned(),
            Self::Right => t!("error_log.place_right").into_owned(),
            Self::Tab => t!("error_log.place_tab").into_owned(),
        }
    }
}

impl Severity {
    fn color(self) -> gpui::Rgba {
        match self {
            Self::Error => theme::danger(),
            Self::Warning => theme::warning(),
            Self::Info => theme::accent(),
        }
    }
}

impl From<Severity> for ErrorLogGlyph {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Error => Self::Error,
            Severity::Warning => Self::Alert,
            Severity::Info => Self::Info,
        }
    }
}

/// The table's single column layout: header and rows build their cells from
/// here, so they cannot drift apart.
#[derive(Clone, Copy)]
enum Column {
    Message,
    Source,
    Date,
}

/// Which of the optional columns a pane of a given width shows. The Message column is always
/// there and gets what the others leave, at least `MESSAGE_COL_MIN_WIDTH`. A narrow pane (the
/// Right place at its default width is about 360pt) drops Source first (the Details dialog and the
/// export still have it; sorting by it needs a wider pane), then Date: the columns are hidden
/// rather than squeezed, so what stays is readable and header and rows never drift apart. The
/// Bottom and Tab places are as wide as the window and always show all three.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ColumnLayout {
    pub(super) source: bool,
    pub(super) date: bool,
}

impl ColumnLayout {
    pub(super) const ALL: Self = Self { source: true, date: true };

    /// `pane_width` is the pane's own width (padding included).
    pub(super) fn for_width(pane_width: f32) -> Self {
        let inner = pane_width - 2.0 * ROW_PADDING_X;
        let source_width = SOURCE_COL_WIDTH;
        let date_width = DATE_COL_WIDTH;
        let message = MESSAGE_COL_MIN_WIDTH;
        Self {
            source: inner >= message + source_width + date_width,
            date: inner >= message + date_width,
        }
    }
}

impl Column {
    /// An empty cell with this column's width rule: Message takes the rest
    /// (and clips, down to its minimum), Source and Date are fixed and never shrink.
    fn cell(self) -> gpui::Div {
        let cell = div().pr(px(CELL_GAP)).overflow_hidden().whitespace_nowrap();
        match self {
            Self::Message => cell.flex_1().min_w(px(MESSAGE_COL_MIN_WIDTH)),
            Self::Source => cell.w(px(SOURCE_COL_WIDTH)).flex_shrink_0(),
            Self::Date => cell.w(px(DATE_COL_WIDTH)).flex_shrink_0(),
        }
    }

    fn header_selector(self) -> &'static str {
        match self {
            Self::Message => "error-col-message",
            Self::Source => "error-col-source",
            Self::Date => "error-col-date",
        }
    }

    fn row_selector(self, position: usize) -> String {
        let name = &self.header_selector()["error-col-".len()..];
        format!("error-cell-{position}-{name}")
    }
}

impl SeverityFilter {
    /// Stable element id (also the test selector) of the filter pill.
    fn element_id(self) -> &'static str {
        match self {
            Self::All => "error-filter-all",
            Self::Errors => "error-filter-errors",
            Self::Warnings => "error-filter-warnings",
            Self::Info => "error-filter-info",
        }
    }
}

pub(super) struct ErrorLogUi {
    pub(super) log: ErrorLog,
    pub(super) open: bool,
    /// `None` disables persistence (unit tests, unavailable `$HOME`).
    pub(super) store: Option<Arc<LogStore>>,
    queue: Vec<WriteOp>,
    writer: Option<Task<()>>,
    /// The startup load is in flight; appends wait so the file is not read
    /// back with this session's own lines.
    loading: bool,
    write_failed: bool,
    pub(super) spec: ViewSpec,
    /// Indices into `log` in display order.
    visible: Vec<usize>,
    /// Log revision `visible` was built for; `None` after a spec change.
    built: Option<u64>,
    pub(super) selected: Option<u64>,
    pub(super) scroll: UniformListScrollHandle,
    /// The sizes of the bottom pane and of the right-hand pane (one split each).
    split: Entity<ResizableState>,
    split_right: Entity<ResizableState>,
    /// The tab that shows the pane while the place is a tab.
    pub(super) tab: Option<u64>,
    pub(super) search: Option<Entity<InputState>>,
    focus: FocusHandle,
    pub(super) notice: Option<SharedString>,
    _quit: Option<Subscription>,
    _search: Option<Subscription>,
}

impl ErrorLogUi {
    pub(super) fn new(cx: &mut Context<AppRoot>) -> Self {
        Self {
            log: ErrorLog::default(),
            open: false,
            store: None,
            queue: Vec::new(),
            writer: None,
            loading: false,
            write_failed: false,
            spec: ViewSpec::default(),
            visible: Vec::new(),
            built: None,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            split: cx.new(|_| ResizableState::default()),
            split_right: cx.new(|_| ResizableState::default()),
            tab: None,
            search: None,
            focus: cx.focus_handle(),
            notice: None,
            _quit: None,
            _search: None,
        }
    }

    fn ensure_visible(&mut self) {
        if self.built != Some(self.log.revision()) {
            self.log.visible_indices(&self.spec, &mut self.visible);
            self.built = Some(self.log.revision());
        }
    }

    fn invalidate_view(&mut self) {
        self.built = None;
    }

    pub(super) fn selected_entry(&self) -> Option<&LogEntry> {
        self.selected.and_then(|id| self.log.find(id))
    }

    /// Display position of the selected entry in the cached visible list.
    fn selected_position(&self) -> Option<usize> {
        let index = self.log.index_of(self.selected?)?;
        self.visible
            .iter()
            .position(|candidate| *candidate == index)
    }

    #[cfg(test)]
    pub(super) fn visible_index(&self, position: usize) -> usize {
        self.visible[position]
    }

    #[cfg(test)]
    pub(super) fn visible_len(&mut self) -> usize {
        self.ensure_visible();
        self.visible.len()
    }
}

impl LogStore {
    /// Where scenario / real runs keep `errors.log`. Unit-test roots never
    /// touch the user's Application Support folder.
    pub(super) fn resolve(scenario_root: Option<&std::path::Path>) -> Option<Self> {
        match scenario_root {
            Some(root) => Some(Self::new(root.join("error-log"))),
            None if cfg!(test) => None,
            None => model::default_dir().map(Self::new),
        }
    }
}

impl AppRoot {
    // --- reporting ---------------------------------------------------------

    /// What the storage layer has to say about moving the old files into the app database (an old
    /// file that could not be moved in and is used for this run, one changed after it was moved
    /// in): one non-blocking warning each, in the Error Log and under the sidebar. One atomic
    /// read per frame when there is nothing.
    pub(super) fn report_storage_warnings(&mut self, cx: &mut Context<Self>) {
        if !engine_storage::legacy::warnings_pending() {
            return;
        }
        for message in engine_storage::legacy::take_warnings() {
            self.report_warning(Area::App, None, &message);
            self.sidebar.notice = Some(message);
        }
        cx.notify();
    }

    /// `connection` is the saved-connection key (UUID string) when known.
    pub(super) fn report_error(
        &mut self,
        area: Area,
        connection: Option<&str>,
        message: impl AsRef<str>,
    ) {
        self.report_event(
            connection,
            Report::from_text(Severity::Error, area, message.as_ref()),
        );
    }

    pub(super) fn report_warning(
        &mut self,
        area: Area,
        connection: Option<&str>,
        message: impl AsRef<str>,
    ) {
        self.report_event(
            connection,
            Report::from_text(Severity::Warning, area, message.as_ref()),
        );
    }

    /// A connect attempt that failed. Driver install / load problems (the
    /// message names the driver) are filed under Driver, the rest under Connect.
    pub(super) fn report_connection_failure(&mut self, connection: &str, error: &str) {
        let area = if error.starts_with("Driver '") {
            Area::Driver
        } else {
            Area::Connect
        };
        self.report_error(area, Some(connection), error);
    }

    /// A failed query run of `tab_id`, attributed to the tab's connection.
    pub(super) fn report_query_failure(&mut self, tab_id: u64, error: &str) {
        let connection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone());
        self.report_error(Area::Query, connection.as_deref(), error);
    }

    pub(super) fn report_event(&mut self, connection: Option<&str>, mut report: Report) {
        if report.connection.is_none() {
            report.connection = connection
                .filter(|key| !key.is_empty())
                .map(|key| self.error_log_connection(key));
        }
        let recorded = self.error_log.log.record(report, SystemTime::now());
        if matches!(recorded, Recorded::Appended { .. }) {
            let line = self
                .error_log
                .store
                .as_ref()
                .and(self.error_log.log.last())
                .map(LogEntry::to_line);
            self.error_log.queue.extend(line.map(WriteOp::Append));
        }
    }

    fn error_log_connection(&self, key: &str) -> (String, String) {
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(key))
            .map(|saved| (saved.id.to_string(), saved.name.clone()))
            .unwrap_or_else(|| (key.to_string(), String::new()))
    }

    /// Problems found while the sidebar state was built (before `AppRoot`
    /// existed to report them).
    pub(super) fn report_startup_problems(&mut self) {
        if let Some(error) = self.sidebar.error.clone() {
            self.report_error(Area::App, None, error);
        }
        if let Some(notice) = self.sidebar.notice.clone() {
            self.report_warning(Area::App, None, notice);
        }
    }

    // --- persistence ---------------------------------------------------------

    /// Attaches the log file, restores the newest entries in the background
    /// and arms the quit-time flush.
    pub(super) fn init_error_log(&mut self, store: Option<LogStore>, cx: &mut Context<Self>) {
        let Some(store) = store else {
            return;
        };
        let store = Arc::new(store);
        self.error_log.store = Some(store.clone());
        self.error_log.loading = true;
        self.error_log._quit = Some(cx.on_app_quit(|this, _| {
            this.flush_error_log_blocking();
            async {}
        }));
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { store.load_recent(model::STARTUP_LOAD) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.error_log.log.prepend_loaded(loaded);
                this.error_log.loading = false;
                this.flush_error_log(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Starts the single background writer when lines are queued. Called
    /// every frame; costs one `is_empty` check when idle.
    pub(super) fn flush_error_log(&mut self, cx: &mut Context<Self>) {
        let ui = &mut self.error_log;
        if ui.queue.is_empty() || ui.writer.is_some() || ui.loading {
            return;
        }
        let Some(store) = ui.store.clone() else {
            ui.queue.clear();
            return;
        };
        ui.writer = Some(cx.spawn(async move |this, cx| loop {
            let Ok(ops) = this.update(cx, |this, _| {
                let ops = std::mem::take(&mut this.error_log.queue);
                if ops.is_empty() {
                    // Cleared in the same turn as the emptiness check, so a
                    // report racing the last write is never stranded.
                    this.error_log.writer = None;
                }
                ops
            }) else {
                return;
            };
            if ops.is_empty() {
                return;
            }
            let store = store.clone();
            let result = cx
                .background_executor()
                .spawn(async move { store.apply(ops) })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |this, cx| {
                    // Report once; a failing disk must not feed itself.
                    if !this.error_log.write_failed {
                        this.error_log.write_failed = true;
                        this.report_warning(
                            Area::App,
                            None,
                            format!("Could not write the error log file: {error}"),
                        );
                        cx.notify();
                    }
                });
            }
        }));
    }

    /// Quit-time flush: synchronous so the last report survives.
    fn flush_error_log_blocking(&mut self) {
        let Some(store) = self.error_log.store.clone() else {
            return;
        };
        let ops = std::mem::take(&mut self.error_log.queue);
        let _ = store.apply(ops);
    }

    // --- pane state ------------------------------------------------------------

    pub(super) fn toggle_error_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.error_log.tab {
            // In its tab: from another tab a toggle brings it forward, from the tab it closes it.
            Some(tab) if self.workspace.active_tab().map(|active| active.id) != Some(tab) => {
                self.activate_tab_ui(tab, cx);
                self.error_log.focus.focus(window, cx);
            }
            _ if self.error_log.open => self.close_error_log(cx),
            _ => self.open_error_log(None, window, cx),
        }
    }

    /// Opens the pane (marking everything seen) and selects `select`.
    pub(super) fn open_error_log(
        &mut self,
        select: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_error_log_search(window, cx);
        let ui = &mut self.error_log;
        ui.open = true;
        ui.log.mark_seen();
        if let Some(id) = select {
            ui.selected = Some(id);
            // The entry must be reachable: drop filters that would hide it.
            ui.ensure_visible();
            if ui.selected_position().is_none() {
                ui.spec.severity = SeverityFilter::All;
                ui.spec.query.clear();
                ui.invalidate_view();
                ui.ensure_visible();
            }
            if let Some(position) = ui.selected_position() {
                ui.scroll
                    .scroll_to_item(position, gpui::ScrollStrategy::Nearest);
            }
        }
        self.sync_error_log_place(cx);
        self.error_log.focus.focus(window, cx);
        self.notify_chrome(cx);
    }

    pub(super) fn close_error_log(&mut self, cx: &mut Context<Self>) {
        // Taken first: the tab's closing then has nothing left to report.
        if let Some(tab) = self.error_log.tab.take() {
            self.close_tab_unremembered(tab, cx);
        }
        self.error_log.open = false;
        self.notify_chrome(cx);
    }

    /// Makes the tab match the place: an open Error Log has its tab while the place is a tab
    /// (opened, or brought forward), and none otherwise. The pane itself follows the place when it
    /// renders.
    pub(super) fn sync_error_log_place(&mut self, cx: &mut Context<Self>) {
        let in_tab = self.error_log.open && self.error_log_place() == ErrorLogPlace::Tab;
        match (in_tab, self.error_log.tab) {
            (true, _) => {
                let tab = self.workspace.open_error_log_tab(&t!("menu.error_log"));
                self.error_log.tab = Some(tab);
                self.activate_tab_ui(tab, cx);
            }
            (false, Some(tab)) => {
                self.error_log.tab = None;
                self.close_tab_unremembered(tab, cx);
            }
            (false, None) => {}
        }
        cx.notify();
    }

    /// The Error Log's tab was closed (by the user, or with a bulk close): the Error Log is closed.
    pub(super) fn error_log_tab_closing(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.error_log.tab == Some(tab_id) {
            self.error_log.tab = None;
            self.error_log.open = false;
            self.notify_chrome(cx);
        }
    }

    pub(super) fn error_log_place(&self) -> ErrorLogPlace {
        self.settings.appearance.error_log_place
    }

    /// The position button's pick: the same change as the Settings page's (applied at once, saved
    /// after the debounce); `sync_error_log_place` follows from it.
    fn set_error_log_place(&mut self, place: ErrorLogPlace, cx: &mut Context<Self>) {
        let mut next = (*self.settings).clone();
        next.appearance.error_log_place = place;
        self.apply_settings(next, None, cx);
    }

    fn ensure_error_log_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.error_log.search.is_some() {
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search messages, sources, codes"));
        let subscription = cx.subscribe(
            &input,
            |this: &mut Self, input: Entity<InputState>, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().trim().to_lowercase();
                    if this.error_log.spec.query != query {
                        this.error_log.spec.query = query;
                        this.error_log.invalidate_view();
                        this.notify_chrome(cx);
                    }
                }
            },
        );
        self.error_log.search = Some(input);
        self.error_log._search = Some(subscription);
    }

    fn set_error_filter(&mut self, filter: SeverityFilter, cx: &mut Context<Self>) {
        if self.error_log.spec.severity != filter {
            self.error_log.spec.severity = filter;
            self.error_log.invalidate_view();
            self.notify_chrome(cx);
        }
    }

    fn sort_error_log(&mut self, key: SortKey, cx: &mut Context<Self>) {
        let spec = &mut self.error_log.spec;
        if spec.sort == key {
            spec.descending = !spec.descending;
        } else {
            spec.sort = key;
            // Date opens newest first, Source opens A..Z.
            spec.descending = key == SortKey::Date;
        }
        self.error_log.invalidate_view();
        self.notify_chrome(cx);
    }

    fn move_error_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ui = &mut self.error_log;
        ui.ensure_visible();
        if ui.visible.is_empty() {
            return;
        }
        let next = match ui.selected_position() {
            Some(position) => position
                .saturating_add_signed(delta)
                .min(ui.visible.len() - 1),
            None => 0,
        };
        if let Some(entry) = ui.visible.get(next).and_then(|index| ui.log.get(*index)) {
            ui.selected = Some(entry.id);
            ui.scroll
                .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
            self.notify_chrome(cx);
        }
    }

    /// Visible row under a window-space point, from the painted list bounds
    /// plus its scroll offset (same scheme as `grid_cell_at`).
    fn error_row_at(&self, point: gpui::Point<gpui::Pixels>) -> Option<usize> {
        let (bounds, offset) = {
            let state = self.error_log.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        if !bounds.contains(&point) {
            return None;
        }
        let y = f32::from(point.y - bounds.origin.y - offset.y);
        if y < 0.0 {
            return None;
        }
        let row = (y / theme::ROW_HEIGHT) as usize;
        (row < self.error_log.visible.len()).then_some(row)
    }

    // --- toolbar actions ----------------------------------------------------

    fn copy_selected_error(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.error_log.selected_entry().map(LogEntry::to_text) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.error_log.notice = Some("Copied the selected entry".into());
        self.notify_chrome(cx);
    }

    fn clear_error_viewer(&mut self, cx: &mut Context<Self>) {
        self.error_log.log.clear();
        self.error_log.selected = None;
        self.error_log.notice = Some("Viewer cleared (the log file is kept)".into());
        self.notify_chrome(cx);
    }

    fn confirm_delete_error_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title("Delete the error log?")
                .child("Clears the viewer and deletes the saved log files. This cannot be undone.")
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Delete"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| {
                        this.error_log.log.clear();
                        this.error_log.selected = None;
                        if this.error_log.store.is_some() {
                            this.error_log.queue.push(WriteOp::DeleteAll);
                        }
                        this.error_log.notice = Some("Log deleted".into());
                        this.notify_chrome(cx);
                    });
                    true
                })
        });
    }

    fn open_error_log_file(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.error_log.store.clone() else {
            self.error_log.notice = Some("Saving the log to disk is disabled in this run".into());
            self.notify_chrome(cx);
            return;
        };
        let path = store.path();
        if path.exists() {
            cx.reveal_path(&path);
        } else if store.dir().exists() {
            cx.reveal_path(store.dir());
        } else {
            self.error_log.notice = Some("No log file has been written yet".into());
        }
        self.notify_chrome(cx);
    }

    fn export_error_log(&mut self, cx: &mut Context<Self>) {
        let directory = crate::platform::save_dialog_dir();
        let prompt = cx.prompt_for_new_path(&directory, Some("tablex-error-log.txt"));
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.error_log.notice = Some("Could not open the save dialog".into());
                        this.notify_chrome(cx);
                    });
                    return;
                }
            };
            // Snapshot on the UI thread (<= 1 000 entries), write off it.
            let Ok((text, count)) = this.update(cx, |this, _| {
                (this.error_log.log.export_text(), this.error_log.log.len())
            }) else {
                return;
            };
            let shown = path.display().to_string();
            let result = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path, text.as_bytes()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.error_log.notice =
                            Some(format!("Exported {count} entries to {shown}").into());
                    }
                    Err(error) => {
                        let message = format!("Could not export the log to {shown}: {error}");
                        this.error_log.notice = Some(message.clone().into());
                        this.report_error(Area::Export, None, message);
                    }
                }
                this.notify_chrome(cx);
            });
        })
        .detach();
    }

    // --- details dialog --------------------------------------------------------

    pub(super) fn open_selected_error_details(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.error_log.selected {
            self.open_error_details(id, window, cx);
        }
    }

    pub(super) fn open_error_details(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.error_log.log.find(id).cloned() else {
            return;
        };
        let mut body = entry.message.to_string();
        if let Some(details) = &entry.details {
            body.push_str("\n\n");
            body.push_str(details);
        }
        let text = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 14)
                .soft_wrap(true)
                .default_value(body)
        });
        let copy_text = entry.to_text();
        window.open_dialog(cx, move |dialog, _, _| {
            let copy_text = copy_text.clone();
            dialog
                .title("Event details")
                .w(px(640.0))
                .child(entry.details_body(&text))
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            Button::new("error-details-copy")
                                .label("Copy")
                                .compact()
                                .ghost()
                                .debug_selector(|| "error-details-copy".into())
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_text.clone(),
                                    ));
                                }),
                        )
                        .child(
                            div().debug_selector(|| "error-details-close".into()).child(
                                gpui_component::dialog::DialogClose::new()
                                    .trigger(|button| button.label("Close")),
                            ),
                        ),
                )
        });
        self.notify_chrome(cx);
    }

    // --- rendering ---------------------------------------------------------------

    /// Status-bar warning-sign icon: muted when nothing is new, colored by
    /// the highest unseen severity with a count badge.
    pub(super) fn render_error_log_icon(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let log = &self.error_log.log;
        let unseen = log.unseen_count();
        let color = log
            .unseen_severity()
            .map(Severity::color)
            .unwrap_or_else(theme::text_faint);
        let open = self.error_log.open;
        div()
            .id("error-log-toggle")
            .debug_selector(|| "error-log-toggle".into())
            .h_full()
            .px(px(6.0))
            .mr(px(4.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            // The status bar is the `selected` tone itself: an open log is the accent tint.
            .when(open, |this| this.bg(theme::accent_surface()))
            .hover(|style| style.bg(theme::hover()))
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new("Error Log").build(window, cx)
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_error_log(window, cx)))
            .child(
                Icon::new(ErrorLogGlyph::Alert)
                    .size(px(13.0))
                    .text_color(color),
            )
            .when(unseen > 0, |this| {
                this.child(
                    div()
                        .debug_selector(|| "error-log-badge".into())
                        .min_w(px(14.0))
                        .h(px(14.0))
                        .px(px(4.0))
                        .rounded(px(7.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(color)
                        .text_size(px(9.0))
                        .text_color(gpui::white())
                        .child(if unseen > 99 {
                            "99+".to_string()
                        } else {
                            unseen.to_string()
                        }),
                )
            })
    }

    /// Left status text. While an error or warning is unseen it shows the
    /// newest one and clicking opens the pane on it.
    pub(super) fn render_status_text(
        &self,
        fallback: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(entry) = self.error_log.log.latest_attention() else {
            return div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(fallback)
                .into_any_element();
        };
        let id = entry.id;
        div()
            .id("status-latest-error")
            .debug_selector(|| "status-latest-error".into())
            .flex_1()
            .min_w_0()
            .truncate()
            .cursor_pointer()
            .text_color(entry.severity.color())
            .child(entry.first_line.clone())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_error_log(Some(id), window, cx);
            }))
            .into_any_element()
    }

    /// Where the right-hand pane opens: `DEFAULT_PANE_SHARE` of the content area beside the sidebar.
    fn default_right_pane_width(&self, window: &Window) -> f32 {
        let sidebar = if self.shell.sidebar_collapsed { 36.0 } else { self.shell.sidebar_width };
        ((f32::from(window.viewport_size().width) - sidebar) * DEFAULT_PANE_SHARE)
            .max(MIN_DEFAULT_PANE_WIDTH)
    }

    /// The pane in a resizable split with `upper`: under it, or (place Right) beside it. In a tab
    /// of its own the pane is not part of the main area (`render_error_log_tab`).
    pub(super) fn with_error_log_pane(
        &mut self,
        upper: impl IntoElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let place = self.error_log_place();
        if !self.error_log.open || place == ErrorLogPlace::Tab {
            return upper.into_any_element();
        }
        let pane = self.render_error_log_pane(place, window, cx);
        let default_width = self.default_right_pane_width(window);
        let upper = resizable_panel().child(upper.into_any_element());
        let pane_panel = resizable_panel().child(pane);
        let split = if place == ErrorLogPlace::Right {
            h_resizable("error-log-split-right")
                .with_state(&self.error_log.split_right)
                .child(upper.size_range(px(MIN_UPPER_WIDTH)..gpui::Pixels::MAX))
                .child(
                    pane_panel
                        .size(px(default_width))
                        .size_range(px(MIN_PANE_WIDTH)..gpui::Pixels::MAX),
                )
        } else {
            v_resizable("error-log-split")
                .with_state(&self.error_log.split)
                .child(upper.size_range(px(120.0)..gpui::Pixels::MAX))
                .child(
                    pane_panel
                        .size(px(DEFAULT_PANE_HEIGHT))
                        .size_range(px(MIN_PANE_HEIGHT)..gpui::Pixels::MAX),
                )
        };
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(split)
            .into_any_element()
    }

    /// The content of the Error Log tab: the same pane, filling the tab.
    pub(super) fn render_error_log_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pane = self.render_error_log_pane(ErrorLogPlace::Tab, window, cx);
        div().flex_1().min_h_0().min_w_0().child(pane).into_any_element()
    }

    /// The control that picks where the pane goes: the Results header's, with the same icons.
    fn render_error_log_place_menu(&self, place: ErrorLogPlace, cx: &mut Context<Self>) -> AnyElement {
        let root = cx.entity().downgrade();
        Button::new("error-log-place")
            .debug_selector(|| "error-log-place".into())
            .icon(Icon::empty().path(place.results_place().icon()).size(px(12.0)))
            .compact()
            .ghost()
            .tooltip(t!("error_log.position"))
            .dropdown_menu(move |menu, _, _| {
                ErrorLogPlace::ALL.iter().fold(menu, |menu, choice| {
                    let choice = *choice;
                    let root = root.clone();
                    menu.item(results::placement::place_menu_item(
                        "error-log-place",
                        choice.results_place(),
                        choice.label(),
                        choice == place,
                        move |app| {
                            let _ = root.update(app, |this, cx| this.set_error_log_place(choice, cx));
                        },
                    ))
                })
            })
            .into_any_element()
    }

    fn render_error_log_pane(
        &mut self,
        place: ErrorLogPlace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.ensure_error_log_search(window, cx);
        // The pane is on screen: anything arriving now is seen.
        self.error_log.log.mark_seen();
        self.error_log.ensure_visible();
        let ui = &self.error_log;
        let total = ui.log.len();
        let shown = ui.visible.len();
        let has_selection = ui.selected_entry().is_some();
        let spec = ui.spec.clone();
        let search = ui.search.clone();
        let notice = ui.notice.clone();

        let toolbar_button =
            |id: &'static str, glyph: ErrorLogGlyph, tip: &'static str, disabled: bool| {
                Button::new(id)
                    .icon(Icon::new(glyph).size(px(14.0)))
                    .compact()
                    .ghost()
                    .disabled(disabled)
                    .tooltip(tip)
                    .debug_selector(move || id.to_string())
            };
        let header = div()
            .debug_selector(|| "error-log-header".into())
            .h(px(theme::HEADER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(2.0))
            .pl(px(12.0))
            .pr(px(6.0))
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(11.0))
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .border_b_2()
                    .border_color(theme::accent())
                    .text_color(theme::text())
                    .child("Error Log")
                    .child(
                        div()
                            .debug_selector(|| "error-log-count".into())
                            .text_color(theme::text_muted())
                            .child(if shown == total {
                                format!("{total}")
                            } else {
                                format!("{shown} of {total}")
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .px(px(12.0))
                    .debug_selector(|| "error-log-notice".into())
                    .text_color(theme::text_muted())
                    .children(notice),
            )
            .child(
                toolbar_button(
                    "error-log-copy",
                    ErrorLogGlyph::Copy,
                    "Copy selected",
                    !has_selection,
                )
                .on_click(cx.listener(|this, _, _, cx| this.copy_selected_error(cx))),
            )
            .child(
                toolbar_button(
                    "error-log-export",
                    ErrorLogGlyph::Export,
                    "Export log…",
                    total == 0,
                )
                .on_click(cx.listener(|this, _, _, cx| this.export_error_log(cx))),
            )
            .child(
                toolbar_button(
                    "error-log-open-file",
                    ErrorLogGlyph::OpenFile,
                    "Open log file",
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_error_log_file(cx))),
            )
            .child(
                toolbar_button(
                    "error-log-clear",
                    ErrorLogGlyph::Clear,
                    "Clear viewer",
                    total == 0,
                )
                .on_click(cx.listener(|this, _, _, cx| this.clear_error_viewer(cx))),
            )
            .child(
                toolbar_button(
                    "error-log-delete",
                    ErrorLogGlyph::Delete,
                    "Delete log…",
                    false,
                )
                .on_click(
                    cx.listener(|this, _, window, cx| this.confirm_delete_error_log(window, cx)),
                ),
            )
            .child(ui::separator())
            .child(self.render_error_log_place_menu(place, cx))
            .child(
                toolbar_button("error-log-close", ErrorLogGlyph::Close, "Close", false)
                    .on_click(cx.listener(|this, _, _, cx| this.close_error_log(cx))),
            );

        let pills = SeverityFilter::ALL.into_iter().fold(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(2.0))
                .whitespace_nowrap(),
            |row, filter| {
                let id = filter.element_id();
                row.child(
                    Button::new(id)
                        .label(filter.label())
                        .compact()
                        .ghost()
                        .selected(spec.severity == filter)
                        .debug_selector(move || id.to_string())
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_error_filter(filter, cx)),
                        ),
                )
            },
        );
        // The search field takes what the pills leave (up to 260pt) and shrinks with the pane, so
        // the row stays one line and never reaches into the header above it.
        let filter_row = div()
            .debug_selector(|| "error-log-filters".into())
            .h(px(32.0))
            .overflow_hidden()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(8.0))
            .border_b_1()
            .border_color(theme::border())
            .child(pills)
            .children(search.map(|input| {
                div()
                    .debug_selector(|| "error-log-search".into())
                    .flex_1()
                    .min_w(px(72.0))
                    .max_w(px(260.0))
                    .child(
                        Input::new(&input)
                            .small()
                            .cleanable(true)
                            .h(px(24.0))
                            .text_size(px(12.0))
                            .prefix(Icon::new(IconName::Search).size(px(12.0))),
                    )
            }));

        let layout = match place {
            // Before the split has measured itself the pane is about to open at its default width.
            ErrorLogPlace::Right => ColumnLayout::for_width(
                self.error_log
                    .split_right
                    .read(cx)
                    .sizes()
                    .get(1)
                    .map_or_else(|| self.default_right_pane_width(window), |width| f32::from(*width)),
            ),
            ErrorLogPlace::Bottom | ErrorLogPlace::Tab => ColumnLayout::ALL,
        };
        let sort_cell = |label: &'static str, key: SortKey, id: &'static str| {
            let active = spec.sort == key;
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .flex()
                .items_center()
                .gap(px(4.0))
                .cursor_pointer()
                .when(active, |this| this.text_color(theme::text()))
                .hover(|style| style.text_color(theme::text()))
                .on_click(cx.listener(move |this, _, _, cx| this.sort_error_log(key, cx)))
                .child(label)
                .when(active, |this| {
                    this.child(
                        Icon::new(if spec.descending {
                            ErrorLogGlyph::SortDown
                        } else {
                            ErrorLogGlyph::SortUp
                        })
                        .size(px(11.0)),
                    )
                })
        };
        let columns = div()
            .h(px(24.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px(px(ROW_PADDING_X))
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .child(
                Column::Message
                    .cell()
                    .pl(px(ICON_GUTTER))
                    .debug_selector(|| Column::Message.header_selector().into())
                    .child("Message"),
            )
            .when(layout.source, |this| {
                this.child(
                    Column::Source
                        .cell()
                        .debug_selector(|| Column::Source.header_selector().into())
                        .child(sort_cell("Source", SortKey::Source, "error-sort-source")),
                )
            })
            .when(layout.date, |this| {
                this.child(
                    Column::Date
                        .cell()
                        .debug_selector(|| Column::Date.header_selector().into())
                        .child(sort_cell("Date", SortKey::Date, "error-sort-date")),
                )
            });

        let body = if shown == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .debug_selector(|| "error-log-empty".into())
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(if total == 0 {
                    "No errors logged"
                } else {
                    "No entries match the filter"
                })
                .into_any_element()
        } else {
            let rows = uniform_list(
                "error-log-rows",
                shown,
                cx.processor(move |this, range: Range<usize>, _window, _cx| {
                    let ui = &this.error_log;
                    range
                        .filter_map(|position| {
                            let entry = ui.log.get(*ui.visible.get(position)?)?;
                            Some(entry.row(position, ui.selected == Some(entry.id), layout))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .min_h_0()
            .track_scroll(&self.error_log.scroll);
            div()
                .id("error-log-body")
                .debug_selector(|| "error-log-body".into())
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.error_log.focus.focus(window, cx);
                        let Some(position) = this.error_row_at(event.position) else {
                            return;
                        };
                        let Some(id) = this
                            .error_log
                            .visible
                            .get(position)
                            .and_then(|index| this.error_log.log.get(*index))
                            .map(|entry| entry.id)
                        else {
                            return;
                        };
                        this.error_log.selected = Some(id);
                        if event.click_count >= 2 {
                            this.open_error_details(id, window, cx);
                        }
                        this.notify_chrome(cx);
                    }),
                )
                .child(rows)
                .child(Scrollbar::vertical(&self.error_log.scroll))
                .into_any_element()
        };

        div()
            .id("error-log-pane")
            .debug_selector(|| "error-log-pane".into())
            .key_context("ErrorLog")
            .track_focus(&self.error_log.focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if !this.error_log.focus.is_focused(window) {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "down" => this.move_error_selection(1, cx),
                    "up" => this.move_error_selection(-1, cx),
                    "enter" => this.open_selected_error_details(window, cx),
                    "escape" => this.close_error_log(cx),
                    "c" if event.keystroke.modifiers.secondary(cx) => this.copy_selected_error(cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .bg(theme::background())
            .border_color(theme::border())
            .map(|pane| match place {
                ErrorLogPlace::Bottom => pane.border_t_1(),
                ErrorLogPlace::Right => pane.border_l_1(),
                ErrorLogPlace::Tab => pane,
            })
            .child(header)
            .child(filter_row)
            .child(columns)
            .child(body)
            .into_any_element()
    }
}

impl LogEntry {
    /// One table row. Clones stored `SharedString`s only.
    fn row(&self, position: usize, selected: bool, layout: ColumnLayout) -> gpui::Div {
        let entry = self;
        let color = entry.severity.color();
        let row = div()
            .w_full()
            .h(px(theme::ROW_HEIGHT))
            .flex()
            .items_center()
            .px(px(ROW_PADDING_X))
            .text_size(px(12.0))
            .text_color(theme::text())
            .debug_selector(move || format!("error-row-{position}"));
        let row = if selected {
            row.bg(theme::selected())
        } else {
            row.when(position % 2 == 1, |this| this.bg(theme::grid_alt_row()))
                .hover(|style| style.bg(theme::hover()))
        };
        row.child(
            Column::Message
                .cell()
                .debug_selector(move || Column::Message.row_selector(position))
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    Icon::new(ErrorLogGlyph::from(entry.severity))
                        .size(px(13.0))
                        .flex_shrink_0()
                        .text_color(color),
                )
                .child(div().min_w_0().truncate().child(entry.first_line.clone()))
                .when(entry.repeat > 1, |this| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .px(px(5.0))
                            .rounded(px(7.0))
                            .bg(theme::hover())
                            .text_size(px(10.0))
                            .text_color(theme::text_muted())
                            .child(format!("×{}", entry.repeat)),
                    )
                }),
        )
        .when(layout.source, |this| {
            this.child(
                Column::Source
                    .cell()
                    .debug_selector(move || Column::Source.row_selector(position))
                    .truncate()
                    .text_color(theme::text_muted())
                    .child(entry.source.clone()),
            )
        })
        .when(layout.date, |this| {
            this.child(
                Column::Date
                    .cell()
                    .debug_selector(move || Column::Date.row_selector(position))
                    .text_color(theme::text_muted())
                    .child(entry.time_text.clone()),
            )
        })
    }

    /// Body of the Event details dialog.
    fn details_body(&self, text: &Entity<TextareaState>) -> gpui::Div {
        let entry = self;
        let color = entry.severity.color();
        let connection = match (&entry.connection_name, &entry.connection_id) {
            (Some(name), Some(id)) if !name.is_empty() => Some(format!("{name} ({id})")),
            (None, Some(id)) | (Some(_), Some(id)) => Some(id.clone()),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .debug_selector(|| "error-details".into())
            .child(detail_row(
                "Severity",
                "error-details-severity",
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_color(color)
                    .child(Icon::new(ErrorLogGlyph::from(entry.severity)).size(px(13.0)))
                    .child(entry.severity.label()),
            ))
            .child(detail_row(
                "Time",
                "error-details-time",
                entry.time_text.clone(),
            ))
            .child(detail_row(
                "Source",
                "error-details-source",
                entry.source.clone(),
            ))
            .children(
                connection.map(|connection| {
                    detail_row("Connection", "error-details-connection", connection)
                }),
            )
            .children(
                entry
                    .code
                    .clone()
                    .map(|code| detail_row("Code", "error-details-code", code)),
            )
            .child(detail_row(
                "Retryable",
                "error-details-retryable",
                if entry.retryable { "Yes" } else { "No" },
            ))
            .when(entry.repeat > 1, |this| {
                this.child(detail_row(
                    "Occurrences",
                    "error-details-repeat",
                    entry.repeat.to_string(),
                ))
            })
            .child(
                div()
                    .mt(px(6.0))
                    .debug_selector(|| "error-details-message".into())
                    .child(Textarea::new(text).readonly(true)),
            )
    }
}

fn detail_row(label: &'static str, selector: &'static str, value: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(12.0))
        .child(
            div()
                .w(px(90.0))
                .text_color(theme::text_muted())
                .child(label),
        )
        .child(
            div()
                .debug_selector(move || selector.to_string())
                .min_w_0()
                .child(value),
        )
}
