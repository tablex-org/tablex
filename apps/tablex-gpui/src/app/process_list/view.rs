//! The Process List tab: its entity, what its controls do, and how it looks.
//!
//! The list is read on the tab's session (see `session`), off the UI thread, and drawn by a `gpui-component` `DataTable`, which
//! builds only the rows in view; a row is cells of text, with no entity, listener or timer of its own. The only clock is the tab's
//! auto refresh: one task, at the interval chosen, that stops when the tab is not shown (it starts again, with a read, when the tab
//! is) and goes with the tab.
//!
//! An action never runs from its button. The button asks; the dialog shows the statement as it will be sent, names the connection,
//! and (on a Production connection or one that confirms executions) wants the connection's explicit confirmation. A read-only
//! connection cannot act at all; the gate is read again when the dialog's button is pressed.

use std::sync::Arc;

use gpui::{ClipboardItem, Entity, SharedString, Subscription, Task, WeakEntity};
use gpui_component::dialog::DialogFooter;
use gpui_component::input::InputEvent;
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_component::IndexPath;

use super::super::*;
use super::engine::{shown, ActionKind, ProcessEngine, ProcessRow};
use super::{GateCache, CLOSED};
use super::session::{release, with_session, SessionSource, Slot};
use crate::sql_account::catalog::ReadError;

/// The auto refresh choices: label and seconds.
const INTERVALS: [(&str, Option<u64>); 4] = [("Off", None), ("2 s", Some(2)), ("5 s", Some(5)), ("10 s", Some(10))];

/// An action waiting for the user's answer in the dialog.
#[derive(Clone)]
struct Pending {
    label: &'static str,
    question: &'static str,
    statement: String,
    summary: String,
    query: SharedString,
    subject: String,
    /// The connection wants its explicit confirmation ticked.
    needs_ack: bool,
    acked: bool,
}

/// The rows the table draws: the list as read, and which of them pass Hide idle and the filter.
struct ProcessDelegate {
    engine: &'static ProcessEngine,
    rows: Arc<Vec<ProcessRow>>,
    shown: Vec<usize>,
    empty: SharedString,
}

impl TableDelegate for ProcessDelegate {
    fn columns_count(&self, _: &gpui::App) -> usize {
        self.engine.columns.len()
    }

    fn rows_count(&self, _: &gpui::App) -> usize {
        self.shown.len()
    }

    fn column(&self, ix: usize, _: &gpui::App) -> Column {
        let column = &self.engine.columns[ix];
        let built = Column::new(column.title, column.title).width(px(column.width)).movable(false);
        if column.right {
            built.text_right()
        } else {
            built
        }
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> gpui::Stateful<Div> {
        let row = div().id(("process-row", row_ix));
        // The table also draws filler rows past the last session.
        if row_ix < self.shown.len() {
            row.debug_selector(move || format!("process-row-{row_ix}"))
        } else {
            row
        }
    }

    fn render_td(&mut self, row_ix: usize, col_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        let Some(row) = self.shown.get(row_ix).and_then(|at| self.rows.get(*at)) else {
            return div().into_any_element();
        };
        div()
            .size_full()
            .truncate()
            .text_size(px(12.0))
            .text_color(if row.idle { theme::text_faint() } else { theme::text() })
            .debug_selector(move || format!("process-cell-{row_ix}-{col_ix}"))
            .child(row.cells[col_ix].clone())
            .into_any_element()
    }

    fn render_empty(&mut self, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        div().size_full().flex().items_center().justify_center().text_size(px(12.0)).text_color(theme::text_muted()).child(self.empty.clone())
    }
}

pub(in crate::app) struct ProcessListView {
    root: WeakEntity<AppRoot>,
    tab_id: u64,
    connection: String,
    engine: &'static ProcessEngine,
    table: Entity<TableState<ProcessDelegate>>,
    filter: Entity<InputState>,
    interval_select: Entity<SelectState<Vec<SharedString>>>,
    hide_idle: bool,
    /// The list as the server last answered.
    rows: Arc<Vec<ProcessRow>>,
    /// The selected row, as a position in `rows`.
    selected: Option<usize>,
    session: Slot,
    loading: bool,
    /// A read was asked for while one was running: it follows.
    refresh_queued: bool,
    /// An action is running.
    acting: bool,
    loaded: bool,
    /// Why the last read failed (the rows stay as they were).
    error: Option<String>,
    /// What the last action did, and whether it failed.
    notice: Option<(String, bool)>,
    interval: Option<u64>,
    /// The auto refresh clock: one task, `None` while it is off or the tab is not shown.
    timer: Option<Task<()>>,
    /// The tab was not shown when the clock last ran: it starts again at the next frame.
    paused: bool,
    pending: Option<Pending>,
    /// The connection was closed under the tab: it has no session, no clock and no way to act until the connection is open again.
    disconnected: bool,
    /// What the buttons ask of the connection's settings, kept until they change (the render asks every frame).
    gate: Option<GateCache>,
    subtitle: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl ProcessListView {
    pub(super) fn new(root: WeakEntity<AppRoot>, tab_id: u64, connection: String, engine: &'static ProcessEngine, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = ProcessDelegate { engine, rows: Arc::new(Vec::new()), shown: Vec::new(), empty: "Reading the sessions\u{2026}".into() };
        let table = cx.new(|cx| TableState::new(delegate, window, cx).col_resizable(true).col_movable(false).sortable(false).row_header(false));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by user, database or query"));
        let labels: Vec<SharedString> = INTERVALS.iter().map(|(label, _)| SharedString::from(*label)).collect();
        let interval_select = cx.new(|cx| SelectState::new(labels, Some(IndexPath::default().row(0)), window, cx));
        let subscriptions = vec![
            cx.subscribe(&filter, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.apply_view(cx);
                }
            }),
            cx.subscribe(&table, |this: &mut Self, table, event: &TableEvent, cx| {
                if let TableEvent::SelectRow(row) = event {
                    this.selected = table.read(cx).delegate().shown.get(*row).copied();
                    cx.notify();
                }
            }),
            cx.subscribe(&interval_select, |this: &mut Self, _, event: &SelectEvent<Vec<SharedString>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    let seconds = INTERVALS.iter().find(|(text, _)| *text == label.as_ref()).and_then(|(_, seconds)| *seconds);
                    this.set_interval(seconds, cx);
                }
            }),
        ];
        Self {
            root,
            tab_id,
            connection,
            engine,
            table,
            filter,
            interval_select,
            hide_idle: true,
            rows: Arc::new(Vec::new()),
            selected: None,
            session: Slot::default(),
            loading: false,
            refresh_queued: false,
            acting: false,
            loaded: false,
            error: None,
            notice: None,
            interval: None,
            timer: None,
            paused: false,
            pending: None,
            disconnected: false,
            gate: None,
            subtitle: SharedString::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Where the tab's session comes from, asked of the app (not while the app is being updated: `start` is handed it instead).
    fn source(&self, cx: &gpui::App) -> Result<SessionSource, String> {
        self.root.read_with(cx, |root, _| root.process_list_source(&self.connection)).unwrap_or_else(|_| Err("The window is closing.".to_owned()))
    }

    // ---- reading ---------------------------------------------------------------------------------------------

    /// The first read, with the session source the app made.
    pub(in crate::app) fn start(&mut self, source: Result<SessionSource, String>, cx: &mut Context<Self>) {
        self.refresh_with(source, cx);
    }

    /// The Refresh button and the clock.
    pub(in crate::app) fn refresh(&mut self, cx: &mut Context<Self>) {
        let source = self.source(cx);
        // A source is only made for an open connection.
        self.disconnected &= source.is_err();
        self.refresh_with(source, cx);
    }

    fn refresh_with(&mut self, source: Result<SessionSource, String>, cx: &mut Context<Self>) {
        if self.loading {
            self.refresh_queued = true;
            return;
        }
        self.loading = true;
        cx.notify();
        let (slot, engine) = (Arc::clone(&self.session), self.engine);
        cx.spawn(async move |this, cx| {
            let work = cx.background_executor().spawn(async move {
                smol::unblock(move || {
                    let answer = with_session(&slot, source, |session| session.rows(engine.list))?;
                    engine.rows(&answer).map_err(ReadError::Malformed)
                })
                .await
            });
            let result = work.await;
            let _ = this.update(cx, |this, cx| this.read(result, cx));
        })
        .detach();
    }

    fn read(&mut self, result: Result<Vec<ProcessRow>, ReadError>, cx: &mut Context<Self>) {
        self.loading = false;
        if self.disconnected {
            // A read that was running when the connection closed has just opened or kept a session: it goes too.
            release(&self.session);
            self.refresh_queued = false;
        }
        self.loaded = true;
        match result {
            Ok(rows) => {
                self.error = None;
                // The selection follows its session across the read.
                let target = self.selected.and_then(|at| self.rows.get(at)).map(|row| row.target.clone());
                self.selected = target.and_then(|target| rows.iter().position(|row| row.target == target));
                self.rows = Arc::new(rows);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        if self.disconnected {
            self.error = Some(CLOSED.to_owned());
        }
        self.apply_view(cx);
        if std::mem::take(&mut self.refresh_queued) {
            self.refresh(cx);
        }
    }

    /// The table shows what passes Hide idle and the filter; the selection stays on its row if that is still shown.
    fn apply_view(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().to_string();
        let shown = shown(&self.rows, self.hide_idle, &filter);
        let at = self.selected.and_then(|selected| shown.iter().position(|row| *row == selected));
        if at.is_none() {
            self.selected = None;
        }
        let empty = match (self.loaded, self.error.is_some(), self.rows.is_empty()) {
            (false, ..) => "Reading the sessions\u{2026}",
            (true, true, true) => "The sessions could not be read.",
            (true, _, true) => "No sessions.",
            _ => "No session matches. Idle sessions are hidden, and the filter applies.",
        };
        let rows = Arc::clone(&self.rows);
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.rows = rows;
            delegate.shown = shown;
            delegate.empty = empty.into();
            match at {
                Some(at) if table.selected_row() != Some(at) => table.set_selected_row(at, cx),
                Some(_) => {}
                None => table.clear_selection(cx),
            }
            cx.notify();
        });
        cx.notify();
    }

    /// The connection closed: the tab lets go of its session, stops its clock and says so. What it shows stays as it was read.
    pub(in crate::app) fn connection_released(&mut self, cx: &mut Context<Self>) {
        self.disconnected = true;
        self.timer = None;
        self.paused = false;
        self.refresh_queued = false;
        release(&self.session);
        self.error = Some(CLOSED.to_owned());
        cx.notify();
    }

    /// The connection is open again: the tab reads again, and its clock (if one was set) starts with the next frame.
    pub(in crate::app) fn connection_reopened(&mut self, source: Result<SessionSource, String>, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.disconnected) {
            return;
        }
        self.error = None;
        if self.interval.is_some() {
            self.paused = true;
            cx.notify();
        } else {
            self.refresh_with(source, cx);
        }
    }

    // ---- the clock -------------------------------------------------------------------------------------------

    pub(in crate::app) fn set_interval(&mut self, seconds: Option<u64>, cx: &mut Context<Self>) {
        self.interval = seconds;
        self.timer = None;
        self.paused = false;
        if let Some(seconds) = seconds {
            self.start_timer(seconds, cx);
            self.refresh(cx);
        }
        cx.notify();
    }

    fn start_timer(&mut self, seconds: u64, cx: &mut Context<Self>) {
        let every = crate::auto_refresh::interval_duration(seconds);
        self.timer = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(every).await;
            if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        }));
    }

    /// The clock ran: a tab that is not shown stops it (it starts again when the tab is shown).
    fn tick(&mut self, cx: &mut Context<Self>) {
        let tab_id = self.tab_id;
        if !self.root.read_with(cx, |root, _| root.process_list_shown(tab_id)).unwrap_or(false) {
            self.timer = None;
            self.paused = true;
            return;
        }
        // The rows must not move under a Cancel / Kill confirmation: the clock skips its read while one is waiting.
        if self.pending.is_some() {
            return;
        }
        self.refresh(cx);
    }

    /// `render` calls this first: a clock that stopped because the tab was hidden starts again, with a read.
    fn resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.paused) {
            return;
        }
        if let Some(seconds) = self.interval {
            cx.defer_in(window, move |this, _, cx| {
                this.start_timer(seconds, cx);
                this.refresh(cx);
            });
        }
    }

    // ---- acting ----------------------------------------------------------------------------------------------

    fn selected_row(&self) -> Option<&ProcessRow> {
        self.selected.and_then(|at| self.rows.get(at))
    }

    /// Why `kind` cannot be asked for on the selected row; `None`: it can.
    fn refusal(&self, kind: ActionKind, read_only: &Option<String>) -> Option<String> {
        let Some(action) = self.engine.action(kind) else {
            return Some(format!("{} has no way to {}.", self.engine.label, if kind == ActionKind::Kill { "end a session" } else { "cancel a query" }));
        };
        if self.disconnected {
            return Some(CLOSED.to_owned());
        }
        if let Some(message) = read_only {
            return Some(message.clone());
        }
        let Some(row) = self.selected_row() else {
            return Some(format!("Select a session to use {}.", action.label));
        };
        if row.own {
            return Some("This is the session this list is read through.".to_owned());
        }
        if self.acting {
            return Some("An action is running.".to_owned());
        }
        None
    }

    /// The button: builds the statement and asks. Nothing is sent from here.
    fn ask(&mut self, kind: ActionKind, window: &mut Window, cx: &mut Context<Self>) {
        let gate = self.root.read_with(cx, |root, _| root.process_list_action_gate(&self.connection)).ok();
        let Some(gate) = gate else { return };
        if let Some(message) = self.refusal(kind, &gate.read_only) {
            self.notice = Some((message, true));
            return cx.notify();
        }
        let (Some(action), Some(row)) = (self.engine.action(kind), self.selected_row()) else { return };
        let Some(statement) = action.statement(&row.target) else { return };
        self.pending = Some(Pending {
            label: action.label,
            question: action.question,
            statement,
            summary: row.summary.clone(),
            query: row.query.clone(),
            subject: gate.subject,
            needs_ack: gate.needs_ack,
            acked: false,
        });
        self.notice = None;
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(pending) = view.read(cx).pending.clone() else { return dialog };
            let ready = !pending.needs_ack || pending.acked;
            let (ack_view, copy_view, run_view, close_view, cancel_view) = (view.clone(), view.clone(), view.clone(), view.clone(), view.clone());
            let statement = pending.statement.clone();
            let body = div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(div().debug_selector(|| "process-confirm-summary".into()).text_size(px(12.5)).child(format!("{} on {}", pending.summary, pending.subject)))
                .children((!pending.query.is_empty()).then(|| {
                    div().debug_selector(|| "process-confirm-query".into()).max_h(px(60.0)).overflow_hidden().text_size(px(11.5)).text_color(theme::text_muted()).child(pending.query.clone())
                }))
                .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child("This statement will be sent:"))
                .child(
                    div()
                        .debug_selector(|| "process-confirm-sql".into())
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::chrome())
                        .text_size(px(12.0))
                        .child(pending.statement.clone()),
                )
                .children(pending.needs_ack.then(|| {
                    div().debug_selector(|| "process-confirm-ack".into()).child(
                        Checkbox::new("process-confirm-ack-box")
                            .checked(pending.acked)
                            .label(format!("Run it on {}", pending.subject))
                            .on_click(move |checked: &bool, _, cx| {
                                let checked = *checked;
                                ack_view.update(cx, |this, cx| {
                                    if let Some(pending) = this.pending.as_mut() {
                                        pending.acked = checked;
                                    }
                                    cx.notify();
                                });
                            }),
                    )
                }));
            dialog
                .w(px(560.0))
                .title(pending.question)
                .child(body)
                .on_close(move |_, _, cx| {
                    close_view.update(cx, |this, _| this.pending = None);
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("process-confirm-copy")
                                .label("Copy SQL")
                                .compact()
                                .ghost()
                                .debug_selector(|| "process-confirm-copy".into())
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(statement.clone()));
                                    copy_view.update(cx, |_, cx| cx.notify());
                                }),
                        )
                        .child(
                            Button::new("process-confirm-cancel")
                                .label("Cancel")
                                .compact()
                                .ghost()
                                .debug_selector(|| "process-confirm-cancel".into())
                                .on_click(move |_, window, cx| {
                                    // `close_dialog` does not call `on_close`: the question is answered here.
                                    cancel_view.update(cx, |this, _| this.pending = None);
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("process-confirm-run")
                                .label(pending.label)
                                .compact()
                                .danger()
                                .disabled(!ready)
                                .debug_selector(|| "process-confirm-run".into())
                                .on_click(move |_, window, cx| {
                                    run_view.update(cx, |this, cx| this.run_pending(cx));
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
        cx.notify();
    }

    /// The dialog's button: sends the statement that was shown, once the connection's gate still lets it.
    fn run_pending(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else { return };
        // The connection may have become read-only, or Production, while the dialog was open.
        let gate = self.root.read_with(cx, |root, _| root.process_list_action_gate(&self.connection)).ok();
        let blocked = match gate {
            _ if self.disconnected => Some(CLOSED.to_owned()),
            None => Some("The window is closing.".to_owned()),
            Some(gate) if gate.read_only.is_some() => gate.read_only,
            Some(gate) if gate.needs_ack && !pending.acked => Some("Confirm the connection first.".to_owned()),
            Some(_) => None,
        };
        if let Some(message) = blocked {
            self.notice = Some((message, true));
            return cx.notify();
        }
        self.acting = true;
        self.notice = None;
        cx.notify();
        let (slot, source, statement) = (Arc::clone(&self.session), self.source(cx), pending.statement.clone());
        let what = format!("{} ({})", pending.label, pending.summary);
        cx.spawn(async move |this, cx| {
            let work = cx.background_executor().spawn(async move { smol::unblock(move || with_session(&slot, source, |session| session.run(&statement))).await });
            let result = work.await;
            let _ = this.update(cx, |this, cx| this.acted(what, result, cx));
        })
        .detach();
    }

    fn acted(&mut self, what: String, result: Result<(), ReadError>, cx: &mut Context<Self>) {
        self.acting = false;
        self.notice = Some(match result {
            Ok(()) => (format!("{what}: sent. The server stops it on its own time."), false),
            Err(error) => (format!("{what} failed: {error}"), true),
        });
        if self.disconnected {
            release(&self.session);
            return cx.notify();
        }
        self.refresh(cx);
    }

    /// Reads the gate and the connection's name again when what they come from changed (settings, the connection's profile, its name).
    fn keep_gate_current(&mut self, cx: &gpui::App) {
        let connection = &self.connection;
        let cache = self.root.read_with(cx, |root, _| match &self.gate {
            Some(cache) if root.process_list_gate_current(connection, cache) => None,
            _ => Some(root.process_list_gate_cache(connection)),
        });
        if let Ok(Some(cache)) = cache {
            self.subtitle = format!("{} \u{00B7} {}", cache.name().unwrap_or("the connection"), self.engine.label).into();
            self.gate = Some(cache);
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn snapshot(&self, cx: &gpui::App) -> Snapshot {
        let table = self.table.read(cx);
        let delegate = table.delegate();
        let read_only = self.root.read_with(cx, |root, _| root.process_list_action_gate(&self.connection).read_only).ok().flatten();
        Snapshot {
            cancel_enabled: self.refusal(ActionKind::Cancel, &read_only).is_none(),
            kill_enabled: self.refusal(ActionKind::Kill, &read_only).is_none(),
            keys: delegate.shown.iter().map(|at| self.rows[*at].key()).collect(),
            total: self.rows.len(),
            cells: delegate.shown.iter().map(|at| self.rows[*at].cells.iter().map(|cell| cell.to_string()).collect()).collect(),
            loading: self.loading,
            error: self.error.clone(),
            notice: self.notice.clone(),
            selected: self.selected_row().map(ProcessRow::key),
            interval: self.interval,
            timer_running: self.timer.is_some(),
            hide_idle: self.hide_idle,
        }
    }
}

/// What the tests read of a tab.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(in crate::app) struct Snapshot {
    pub(in crate::app) keys: Vec<String>,
    pub(in crate::app) total: usize,
    pub(in crate::app) cells: Vec<Vec<String>>,
    pub(in crate::app) loading: bool,
    pub(in crate::app) error: Option<String>,
    pub(in crate::app) notice: Option<(String, bool)>,
    pub(in crate::app) selected: Option<String>,
    pub(in crate::app) interval: Option<u64>,
    pub(in crate::app) timer_running: bool,
    pub(in crate::app) hide_idle: bool,
    /// Whether the two buttons are enabled for the selected row.
    pub(in crate::app) cancel_enabled: bool,
    pub(in crate::app) kill_enabled: bool,
}

impl Drop for ProcessListView {
    /// Closing the tab ends its session (off the UI thread) and, with the task, its clock.
    fn drop(&mut self) {
        release(&self.session);
    }
}

impl Render for ProcessListView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.resume(window, cx);
        let tab_id = self.tab_id;
        self.keep_gate_current(cx);
        let gate_read_only = self.gate.as_ref().and_then(|cache| cache.gate.read_only.clone());
        let button = |kind: ActionKind, id: &'static str, icon: &'static str, cx: &mut Context<Self>| {
            let action = self.engine.action(kind);
            let refusal = self.refusal(kind, &gate_read_only);
            Button::new((id, tab_id))
                .debug_selector(move || id.to_owned())
                .label(action.map_or(if kind == ActionKind::Kill { "Kill Session" } else { "Cancel Query" }, |action| action.label))
                .icon(Icon::empty().path(icon).size(px(12.0)))
                .compact()
                .disabled(refusal.is_some())
                .when_some(refusal.filter(|_| action.is_none() || gate_read_only.is_some()), |this, reason| this.tooltip(reason))
                .on_click(cx.listener(move |this, _, window, cx| this.ask(kind, window, cx)))
        };
        let subtitle = self.subtitle.clone();
        let count = if self.loading && !self.loaded {
            "Reading\u{2026}".to_owned()
        } else {
            let showing = self.table.read(cx).delegate().shown.len();
            format!("{showing} of {}", self.rows.len())
        };
        let cancel = button(ActionKind::Cancel, "process-list-cancel", "icons/circle-x.svg", cx);
        let kill = button(ActionKind::Kill, "process-list-kill", "icons/trash.svg", cx);
        let message = self.error.clone().map(|error| (error, "process-list-error")).or_else(|| self.notice.clone().filter(|(_, failed)| *failed).map(|(text, _)| (text, "process-list-error")));
        let info = self.notice.clone().filter(|(_, failed)| !*failed).map(|(text, _)| text);
        div()
            .id(("process-list-view", tab_id))
            .debug_selector(|| "process-list-view".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_size(px(12.0))
            .text_color(theme::text())
            .child(
                div()
                    .h(px(48.0))
                    .flex_shrink_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .bg(theme::chrome())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(Icon::empty().path("icons/list-checks.svg").size(px(16.0)).text_color(theme::accent()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().debug_selector(|| "process-list-title".into()).truncate().text_size(px(13.0)).child("Process List"))
                            .child(div().debug_selector(|| "process-list-subtitle".into()).truncate().text_size(px(11.0)).text_color(theme::text_muted()).child(subtitle)),
                    )
                    .child(cancel)
                    .child(kill),
            )
            .child(
                div()
                    .h(px(40.0))
                    .flex_shrink_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        Button::new(("process-list-refresh", tab_id))
                            .debug_selector(|| "process-list-refresh".into())
                            .icon(Icon::empty().path("icons/refresh-cw.svg").size(px(12.0)))
                            .tooltip("Refresh")
                            .compact()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child("Auto refresh"))
                    .child(div().debug_selector(|| "process-list-interval".into()).w(px(90.0)).child(Select::new(&self.interval_select).small().w_full()))
                    .child(div().debug_selector(|| "process-list-hide-idle".into()).child(
                        Checkbox::new(("process-list-hide-idle-box", tab_id)).label("Hide idle").checked(self.hide_idle).on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.hide_idle = *checked;
                            this.apply_view(cx);
                        })),
                    ))
                    .child(div().debug_selector(|| "process-list-filter".into()).flex_1().min_w_0().child(Input::new(&self.filter).small()))
                    .child(div().debug_selector(|| "process-list-count".into()).text_size(px(11.0)).text_color(theme::text_muted()).child(count)),
            )
            .children(message.map(|(text, selector)| {
                div()
                    .debug_selector(move || selector.into())
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .text_size(px(11.5))
                    .text_color(theme::danger())
                    .child(text)
            }))
            .children(info.map(|text| {
                div()
                    .debug_selector(|| "process-list-notice".into())
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .text_size(px(11.5))
                    .text_color(theme::text_muted())
                    .child(text)
            }))
            .child(div().debug_selector(|| "process-list-table".into()).flex_1().min_h_0().child(DataTable::new(&self.table).stripe(true)))
    }
}
