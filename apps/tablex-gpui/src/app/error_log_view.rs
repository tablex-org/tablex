//! Error Log pane: the status-bar icon, the resizable bottom pane (toolbar,
//! severity pills, search, virtualized table), the Event details dialog and
//! the `report_*` entry points every failure site calls.
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
use gpui_component::resizable::{resizable_panel, v_resizable, ResizableState};
use std::time::SystemTime;

const DEFAULT_PANE_HEIGHT: f32 = 200.0;
const MIN_PANE_HEIGHT: f32 = 96.0;
const SOURCE_COL_WIDTH: f32 = 200.0;
const DATE_COL_WIDTH: f32 = 170.0;
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

impl Column {
    /// An empty cell with this column's width rule: Message takes the rest
    /// (and clips), Source and Date are fixed and never shrink.
    fn cell(self) -> gpui::Div {
        let cell = div().pr(px(CELL_GAP)).overflow_hidden();
        match self {
            Self::Message => cell.flex_1().min_w_0(),
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
    scroll: UniformListScrollHandle,
    split: Entity<ResizableState>,
    search: Option<Entity<InputState>>,
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
        if self.error_log.open {
            self.close_error_log(cx);
        } else {
            self.open_error_log(None, window, cx);
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
        ui.focus.focus(window, cx);
        self.notify_chrome(cx);
    }

    pub(super) fn close_error_log(&mut self, cx: &mut Context<Self>) {
        self.error_log.open = false;
        self.notify_chrome(cx);
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
            .when(open, |this| this.bg(theme::selected()))
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

    /// The whole pane inside a vertical resizable split under `upper`.
    pub(super) fn with_error_log_pane(
        &mut self,
        upper: impl IntoElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if !self.error_log.open {
            return upper.into_any_element();
        }
        let pane = self.render_error_log_pane(window, cx);
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                v_resizable("error-log-split")
                    .with_state(&self.error_log.split)
                    .child(
                        resizable_panel()
                            .size_range(px(120.0)..gpui::Pixels::MAX)
                            .child(upper.into_any_element()),
                    )
                    .child(
                        resizable_panel()
                            .size(px(DEFAULT_PANE_HEIGHT))
                            .size_range(px(MIN_PANE_HEIGHT)..gpui::Pixels::MAX)
                            .child(pane),
                    ),
            )
            .into_any_element()
    }

    fn render_error_log_pane(
        &mut self,
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
            .child(
                toolbar_button("error-log-close", ErrorLogGlyph::Close, "Close", false)
                    .on_click(cx.listener(|this, _, _, cx| this.close_error_log(cx))),
            );

        let pills = SeverityFilter::ALL.into_iter().fold(
            div().flex().items_center().gap(px(2.0)),
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
        let filter_row = div()
            .h(px(32.0))
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
                    .w(px(260.0))
                    .child(
                        Input::new(&input)
                            .small()
                            .cleanable(true)
                            .h(px(24.0))
                            .text_size(px(12.0))
                            .prefix(Icon::new(IconName::Search).size(px(12.0))),
                    )
            }));

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
            .child(
                Column::Source
                    .cell()
                    .debug_selector(|| Column::Source.header_selector().into())
                    .child(sort_cell("Source", SortKey::Source, "error-sort-source")),
            )
            .child(
                Column::Date
                    .cell()
                    .debug_selector(|| Column::Date.header_selector().into())
                    .child(sort_cell("Date", SortKey::Date, "error-sort-date")),
            );

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
                cx.processor(|this, range: Range<usize>, _window, _cx| {
                    let ui = &this.error_log;
                    range
                        .filter_map(|position| {
                            let entry = ui.log.get(*ui.visible.get(position)?)?;
                            Some(entry.row(position, ui.selected == Some(entry.id)))
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
            .border_t_1()
            .border_color(theme::border())
            .child(header)
            .child(filter_row)
            .child(columns)
            .child(body)
            .into_any_element()
    }
}

impl LogEntry {
    /// One table row. Clones stored `SharedString`s only.
    fn row(&self, position: usize, selected: bool) -> gpui::Div {
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
        .child(
            Column::Source
                .cell()
                .debug_selector(move || Column::Source.row_selector(position))
                .truncate()
                .text_color(theme::text_muted())
                .child(entry.source.clone()),
        )
        .child(
            Column::Date
                .cell()
                .debug_selector(move || Column::Date.row_selector(position))
                .text_color(theme::text_muted())
                .child(entry.time_text.clone()),
        )
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
