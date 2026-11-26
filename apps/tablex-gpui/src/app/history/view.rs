//! The Query History tab: its entity, its filters, and how it looks.
//!
//! The list is a `gpui-component` `DataTable`, which builds only the rows in view; a row is cells of
//! text made when the row was loaded, with no entity, listener or timer of its own. Pages of 200 come
//! from the database (or the audit log) on the background executor; scrolling near the bottom, or the
//! Load more button, reads the next one. The only clock is the 250 ms the search box waits after a
//! key before it asks again, one task that a newer key replaces.
//!
//! The view only reads and decides; what an action does to the app (a new tab, the editor, a run)
//! is `AppRoot`'s, in `mod.rs`.

use std::sync::Arc;

use engine_storage::history::{self, Scope};
use gpui::{ClipboardItem, Entity, KeyDownEvent, SharedString, Subscription, Task, WeakEntity};
use gpui_component::button::ButtonGroup;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::InputEvent;
use gpui_component::menu::PopupMenu;
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_component::IndexPath;

use super::super::*;
use super::model::{
    self, KindFilter, McpCall, Names, Pager, Query, Quick, RangeError, Row, RowView, Source, PAGE,
};

/// What the user can do with a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Act {
    OpenInTab,
    Insert,
    Copy,
    Run,
    Analyze,
    Details,
    Delete,
}

/// A connection that has runs, with the databases they ran in.
#[derive(Debug, Clone)]
struct ScopeOption {
    /// The id as the history stores it (what the filter compares).
    id: String,
    name: String,
    databases: Vec<String>,
}

/// What a read of the first page brings back.
struct Loaded {
    generation: u64,
    pager: Result<Pager, String>,
    rows: Vec<RowView>,
    options: Vec<ScopeOption>,
    mcp: Option<Arc<(Vec<McpCall>, Vec<String>)>>,
    error: Option<String>,
}

const COLUMNS: [(&str, f32); 5] = [("time", 104.0), ("duration", 66.0), ("result", 120.0), ("connection", 118.0), ("statement", 640.0)];

/// The rows the table draws.
struct HistoryDelegate {
    view: WeakEntity<HistoryView>,
    root: WeakEntity<AppRoot>,
    rows: Vec<RowView>,
    empty: SharedString,
    more: bool,
}

impl TableDelegate for HistoryDelegate {
    fn columns_count(&self, _: &gpui::App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _: &gpui::App) -> usize {
        self.rows.len()
    }

    fn column(&self, ix: usize, _: &gpui::App) -> Column {
        let (key, width) = COLUMNS[ix];
        let title = match ix {
            0 => t!("history.col_time"),
            1 => t!("history.col_duration"),
            2 => t!("history.col_result"),
            3 => t!("history.col_connection"),
            _ => t!("history.col_statement"),
        };
        Column::new(key, title.into_owned()).width(px(width)).movable(false)
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> gpui::Stateful<Div> {
        let row = div().id(("history-row", row_ix));
        // The table also draws filler rows past the last entry.
        if row_ix < self.rows.len() {
            row.debug_selector(move || format!("history-row-{row_ix}"))
        } else {
            row
        }
    }

    fn render_td(&mut self, row_ix: usize, col_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        let (text, color) = match col_ix {
            0 => (row.time.clone(), theme::text_muted()),
            1 => (row.duration.clone(), theme::text_muted()),
            2 => (row.result.clone(), if row.failed { theme::danger() } else { theme::text_muted() }),
            3 => (row.place.clone(), theme::text_muted()),
            _ => (row.line.clone(), theme::text()),
        };
        div().size_full().truncate().text_size(px(12.0)).text_color(color).child(text).into_any_element()
    }

    fn render_empty(&mut self, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        div()
            .debug_selector(|| "history-empty".into())
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(self.empty.clone())
    }

    fn has_more(&self, _: &gpui::App) -> bool {
        self.more
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let view = self.view.clone();
        cx.defer(move |app| {
            let _ = view.update(app, |view, cx| view.load_more(cx));
        });
    }

    fn context_menu(&mut self, row_ix: usize, menu: PopupMenu, _: &mut Window, cx: &mut Context<TableState<Self>>) -> PopupMenu {
        let Some(row) = self.rows.get(row_ix).cloned() else {
            return menu;
        };
        let analyze = !row.row.is_mcp()
            && self
                .root
                .read_with(cx, |root, _| root.history_ai_available(row.row.connection_id().unwrap_or_default()))
                .unwrap_or(false);
        let mut menu = menu;
        for act in actions_for(&row.row, analyze) {
            let (view, row) = (self.view.clone(), row.clone());
            menu = menu.item(PopupMenuItem::new(act_label(act)).on_click(move |_, window, app| {
                let _ = view.update(app, |view, cx| view.act(act, row.clone(), window, cx));
            }));
        }
        menu
    }
}

fn act_label(act: Act) -> String {
    match act {
        Act::OpenInTab => t!("history.open_in_tab"),
        Act::Insert => t!("history.insert"),
        Act::Copy => t!("history.copy"),
        Act::Run => t!("history.run"),
        Act::Analyze => t!("history.analyze"),
        Act::Details => t!("history.details"),
        Act::Delete => t!("history.delete"),
    }
    .into_owned()
}

/// The actions a row offers: an MCP call is a record to read and copy, a run is also something to
/// open, insert, run, ask about and delete.
fn actions_for(row: &Row, analyze: bool) -> Vec<Act> {
    match row {
        Row::Mcp(call) if call.statement.as_deref().is_some_and(|s| !s.trim().is_empty()) => vec![Act::Copy, Act::Details],
        Row::Mcp(_) => vec![Act::Details],
        Row::Sql(_) => {
            let mut acts = vec![Act::OpenInTab, Act::Insert, Act::Copy, Act::Run];
            if analyze {
                acts.push(Act::Analyze);
            }
            acts.extend([Act::Details, Act::Delete]);
            acts
        }
    }
}

pub(in crate::app) struct HistoryView {
    root: WeakEntity<AppRoot>,
    table: Entity<TableState<HistoryDelegate>>,
    search: Entity<InputState>,
    scope_search: Entity<InputState>,
    start_date: Entity<InputState>,
    end_date: Entity<InputState>,
    tool_select: Entity<SelectState<Vec<SharedString>>>,
    preview: Entity<EditorState>,
    query: Query,
    /// The quick range that is on; `None` for typed dates (or none at all, when `query.since` is `None` too).
    quick: Option<Quick>,
    scope_open: bool,
    range_open: bool,
    range_error: Option<RangeError>,
    options: Vec<ScopeOption>,
    mcp: Option<Arc<(Vec<McpCall>, Vec<String>)>>,
    generation: u64,
    /// The rest of the rows of the current query; `None` while a read has it.
    pager: Option<Pager>,
    loading: bool,
    loaded: bool,
    error: Option<String>,
    notice: Option<String>,
    selected: Option<RowView>,
    search_task: Option<Task<()>>,
    /// Tool names the audit log has, waiting for the next frame to go into the select.
    pending_tools: Option<Vec<String>>,
    #[cfg(test)]
    clear_asked: Option<i64>,
    /// The connection of the tab the user came from ("Current" in the scope picker).
    current: Option<String>,
    /// What the app said when the tab was opened or refreshed: names of the connections, the audit log.
    context: (Arc<Names>, Option<std::path::PathBuf>),
    _subscriptions: Vec<Subscription>,
}

impl HistoryView {
    pub(super) fn new(root: WeakEntity<AppRoot>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.entity().downgrade();
        let delegate = HistoryDelegate { view, root: root.clone(), rows: Vec::new(), empty: t!("history.loading").into_owned().into(), more: false };
        let table = cx.new(|cx| TableState::new(delegate, window, cx).col_resizable(true).col_movable(false).sortable(false).row_header(false));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("history.search_placeholder").into_owned()));
        let scope_search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("history.scope_search").into_owned()));
        let start_date = cx.new(|cx| InputState::new(window, cx).placeholder(t!("history.range_start").into_owned()));
        let end_date = cx.new(|cx| InputState::new(window, cx).placeholder(t!("history.range_end").into_owned()));
        let tool_select = cx.new(|cx| SelectState::new(vec![SharedString::from(t!("history.all_tools").into_owned())], Some(IndexPath::default().row(0)), window, cx));
        let preview = cx.new(|cx| EditorState::new(window, cx).language("sql").line_number(false).soft_wrap(true));
        let subscriptions = vec![
            cx.subscribe(&search, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search_changed(cx);
                }
            }),
            cx.subscribe(&scope_search, |_: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&table, window, |this: &mut Self, table, event: &TableEvent, window, cx| match event {
                TableEvent::SelectRow(row) => {
                    let picked = table.read(cx).delegate().rows.get(*row).cloned();
                    this.select(picked, window, cx);
                }
                TableEvent::DoubleClickedRow(row) => {
                    if let Some(picked) = table.read(cx).delegate().rows.get(*row).cloned() {
                        if actions_for(&picked.row, false).contains(&Act::OpenInTab) {
                            this.act(Act::OpenInTab, picked, window, cx);
                        }
                    }
                }
                _ => {}
            }),
            cx.subscribe(&tool_select, |this: &mut Self, _, event: &SelectEvent<Vec<SharedString>>, cx| {
                let SelectEvent::Confirm(value) = event;
                let tool = value.as_ref().map(|text| text.to_string()).filter(|text| *text != t!("history.all_tools"));
                if this.query.tool != tool {
                    this.query.tool = tool;
                    this.reload(cx);
                }
            }),
        ];
        Self {
            root,
            table,
            search,
            scope_search,
            start_date,
            end_date,
            tool_select,
            preview,
            query: Query::default(),
            quick: Some(Quick::AllTime),
            scope_open: false,
            range_open: false,
            range_error: None,
            options: Vec::new(),
            mcp: None,
            generation: 0,
            pager: None,
            loading: false,
            loaded: false,
            error: None,
            notice: None,
            selected: None,
            search_task: None,
            pending_tools: None,
            #[cfg(test)]
            clear_asked: None,
            current: None,
            context: (Arc::new(Names::new()), None),
            _subscriptions: subscriptions,
        }
    }

    // ---- the rows ----------------------------------------------------------------------------------------

    /// The connection of the tab the user came from, and what the app knows (the view cannot ask
    /// while the app is being updated, which is when this is called).
    pub(in crate::app) fn set_context(&mut self, current: Option<String>, names: Names, audit: Option<std::path::PathBuf>) {
        self.current = current;
        self.context = (Arc::new(names), audit);
    }

    /// Reads the first page again, and the audit log.
    pub(in crate::app) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.mcp = None;
        self.reload(cx);
    }

    /// The Refresh button: the names of the connections are asked of the app again.
    fn refresh_from_button(&mut self, cx: &mut Context<Self>) {
        if let Ok((names, audit)) = self.root.read_with(cx, |root, _| root.history_read_context()) {
            self.context = (Arc::new(names), audit);
        }
        self.refresh(cx);
    }
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.pager = None;
        let (names, audit) = (Arc::clone(&self.context.0), self.context.1.clone());
        let query = self.query.clone();
        let cached = self.mcp.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let work = cx.background_executor().spawn(async move { read_first_page(query, names, audit, cached, generation) });
            let loaded = work.await;
            let _ = this.update(cx, |this, cx| this.loaded_page(loaded, cx));
        })
        .detach();
    }

    fn loaded_page(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        if loaded.generation != self.generation {
            return;
        }
        self.loading = false;
        self.loaded = true;
        self.options = loaded.options;
        if let Some(mcp) = loaded.mcp {
            self.sync_tools(&mcp.1);
            self.mcp = Some(mcp);
        }
        self.error = loaded.error;
        let rows = loaded.rows;
        match loaded.pager {
            Ok(pager) => {
                self.pager = Some(pager);
            }
            Err(error) => self.error = Some(error),
        }
        let more = self.pager.as_ref().is_some_and(|pager| !pager.exhausted());
        let empty = self.empty_text();
        // The selection stays on its row when the row is still there.
        let keep = self.selected.as_ref().and_then(|selected| rows.iter().position(|row| same_row(&row.row, &selected.row)));
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.rows = rows;
            delegate.more = more;
            delegate.empty = empty.into();
            match keep {
                Some(at) => table.set_selected_row(at, cx),
                None => table.clear_selection(cx),
            }
            cx.notify();
        });
        if keep.is_none() {
            self.selected = None;
        }
        cx.notify();
    }

    /// The next page, appended below what is loaded.
    pub(in crate::app) fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some(mut pager) = self.pager.take() else {
            return;
        };
        if pager.exhausted() {
            self.pager = Some(pager);
            return;
        }
        self.loading = true;
        let generation = self.generation;
        let names = Arc::clone(&self.context.0);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let work = cx.background_executor().spawn(async move {
                let rows = pager.next(PAGE as usize).map(|rows| rows.into_iter().map(|row| RowView::new(row, &names)).collect::<Vec<_>>());
                (pager, rows)
            });
            let (pager, rows) = work.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.loading = false;
                let more = !pager.exhausted();
                this.pager = Some(pager);
                match rows {
                    Ok(rows) => this.table.update(cx, |table, cx| {
                        let delegate = table.delegate_mut();
                        delegate.rows.extend(rows);
                        delegate.more = more;
                        cx.notify();
                    }),
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn empty_text(&self) -> String {
        if !self.loaded {
            t!("history.loading").into_owned()
        } else if self.error.is_some() {
            t!("history.read_failed").into_owned()
        } else if self.is_filtered() {
            t!("history.empty_filtered").into_owned()
        } else {
            t!("history.empty").into_owned()
        }
    }

    fn is_filtered(&self) -> bool {
        let q = &self.query;
        !q.text.trim().is_empty() || !q.scopes.is_empty() || q.kind != KindFilter::All || q.since.is_some() || q.until.is_some() || q.tool.is_some()
    }

    fn sync_tools(&mut self, tools: &[String]) {
        self.pending_tools = Some(tools.to_vec());
    }

    /// The tool select holds "All tools" and the tools the audit log has (a select is changed with a window).
    fn apply_tools(&mut self, tools: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let mut items = vec![SharedString::from(t!("history.all_tools").into_owned())];
        items.extend(tools.iter().cloned().map(SharedString::from));
        let at = self.query.tool.as_ref().and_then(|tool| tools.iter().position(|candidate| candidate == tool)).map_or(0, |at| at + 1);
        self.tool_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(Some(IndexPath::default().row(at)), window, cx);
        });
    }

    // ---- filters -----------------------------------------------------------------------------------------

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let text = self.search.read(cx).value().to_string();
        if text == self.query.text {
            return;
        }
        self.query.text = text;
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let _ = this.update(cx, |this, cx| {
                this.search_task = None;
                this.reload(cx);
            });
        }));
    }

    fn set_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if self.query.source == source {
            return;
        }
        self.query.source = source;
        if !KindFilter::offered(source).contains(&self.query.kind) {
            self.query.kind = KindFilter::All;
        }
        if source != Source::Mcp {
            self.query.tool = None;
        }
        self.reload(cx);
    }

    fn set_kind(&mut self, kind: KindFilter, cx: &mut Context<Self>) {
        if self.query.kind != kind {
            self.query.kind = kind;
            self.reload(cx);
        }
    }

    fn set_quick(&mut self, quick: Quick, cx: &mut Context<Self>) {
        self.quick = Some(quick);
        self.query.since = quick.since(chrono::Local::now());
        self.query.until = None;
        self.range_error = None;
        self.range_open = false;
        self.reload(cx);
    }

    fn apply_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (start, end) = (self.start_date.read(cx).value().to_string(), self.end_date.read(cx).value().to_string());
        match model::range_bounds(&start, &end) {
            Ok((since, until)) => {
                self.quick = if since.is_none() && until.is_none() { Some(Quick::AllTime) } else { None };
                self.query.since = since;
                self.query.until = until;
                self.range_error = None;
                self.range_open = false;
                self.reload(cx);
            }
            Err(error) => {
                self.range_error = Some(error);
                cx.notify();
            }
        }
        let _ = window;
    }

    fn clear_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.start_date, &self.end_date] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.set_quick(Quick::AllTime, cx);
    }

    fn toggle_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let scopes = &mut self.query.scopes;
        match scopes.iter().position(|scope| scope.conn_uuid == id) {
            // Narrowed to databases: back to the whole connection.
            Some(at) if !scopes[at].databases.is_empty() => scopes[at].databases.clear(),
            Some(at) => {
                scopes.remove(at);
            }
            None => scopes.push(Scope { conn_uuid: id.to_owned(), databases: Vec::new() }),
        }
        self.reload(cx);
    }

    fn toggle_database(&mut self, id: &str, database: &str, cx: &mut Context<Self>) {
        let scopes = &mut self.query.scopes;
        let at = match scopes.iter().position(|scope| scope.conn_uuid == id) {
            Some(at) => at,
            None => {
                scopes.push(Scope { conn_uuid: id.to_owned(), databases: Vec::new() });
                scopes.len() - 1
            }
        };
        match scopes[at].databases.iter().position(|candidate| candidate == database) {
            Some(found) => {
                scopes[at].databases.remove(found);
                // A connection with none of its databases left is not wanted at all.
                if scopes[at].databases.is_empty() {
                    scopes.remove(at);
                }
            }
            None => scopes[at].databases.push(database.to_owned()),
        }
        self.reload(cx);
    }

    fn select_current_scope(&mut self, cx: &mut Context<Self>) {
        let Some(current) = self.current.clone() else {
            return;
        };
        let Some(option) = self.options.iter().find(|option| option.id.eq_ignore_ascii_case(&current)) else {
            return;
        };
        self.query.scopes = vec![Scope { conn_uuid: option.id.clone(), databases: Vec::new() }];
        self.scope_open = false;
        self.reload(cx);
    }

    fn reset_scope(&mut self, cx: &mut Context<Self>) {
        self.query.scopes.clear();
        self.reload(cx);
    }

    /// Opens the manager on one connection's runs (a menu on a connection), or on all of them.
    pub(in crate::app) fn show_connection(&mut self, connection: Option<String>, cx: &mut Context<Self>) {
        self.query.scopes = connection.map(|id| vec![Scope { conn_uuid: id, databases: Vec::new() }]).unwrap_or_default();
        self.reload(cx);
    }

    // ---- selection and actions ---------------------------------------------------------------------------

    fn select(&mut self, row: Option<RowView>, window: &mut Window, cx: &mut Context<Self>) {
        let text = row.as_ref().map(|row| row.row.statement().to_owned()).unwrap_or_default();
        self.selected = row;
        self.notice = None;
        self.preview.update(cx, |editor, cx| editor.set_value(text, window, cx));
        cx.notify();
    }

    #[cfg(test)]
    pub(in crate::app) fn selected_row(&self) -> Option<&RowView> {
        self.selected.as_ref()
    }

    pub(in crate::app) fn row_count(&self, cx: &gpui::App) -> usize {
        self.table.read(cx).delegate().rows.len()
    }

    /// The statements of the loaded rows, newest first.
    #[cfg(test)]
    pub(in crate::app) fn statements(&self, cx: &gpui::App) -> Vec<String> {
        self.table.read(cx).delegate().rows.iter().map(|row| row.row.statement().to_owned()).collect()
    }

    /// The rows loaded, as they read (time, duration, result, connection, line).
    #[cfg(test)]
    pub(in crate::app) fn row_views(&self, cx: &gpui::App) -> Vec<RowView> {
        self.table.read(cx).delegate().rows.clone()
    }

    /// What the preview editor shows.
    #[cfg(test)]
    pub(in crate::app) fn preview_text(&self, cx: &gpui::App) -> String {
        self.preview.read(cx).value().to_string()
    }

    /// The tab has read what it was last asked for and waits for no key.
    #[cfg(test)]
    pub(in crate::app) fn settled(&self) -> bool {
        self.loaded && !self.loading && self.search_task.is_none()
    }

    /// Selects the row at `ix` as a click on it would (the captures).
    #[cfg(all(test, feature = "bench"))]
    pub(in crate::app) fn select_row_for_test(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.table.read(cx).delegate().rows.get(ix).cloned();
        self.table.update(cx, |table, cx| table.set_selected_row(ix, cx));
        self.select(picked, window, cx);
    }

    /// The names of the connections the scope picker offers.
    #[cfg(test)]
    pub(in crate::app) fn scope_names(&self) -> Vec<String> {
        self.options.iter().map(|option| option.name.clone()).collect()
    }

    /// The count the last Clear confirmation was asked with.
    #[cfg(test)]
    pub(in crate::app) fn clear_asked(&self) -> Option<i64> {
        self.clear_asked
    }

    #[cfg(test)]
    pub(in crate::app) fn query(&self) -> &Query {
        &self.query
    }

    fn act(&mut self, act: Act, row: RowView, window: &mut Window, cx: &mut Context<Self>) {
        self.notice = None;
        match act {
            Act::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(row.row.statement().to_owned()));
                self.notice = Some(t!("history.copied").into_owned());
                cx.notify();
            }
            Act::Details => self.open_details(row, window, cx),
            Act::Delete => self.ask_delete(row, window, cx),
            Act::OpenInTab | Act::Insert | Act::Run | Act::Analyze => {
                let root = self.root.clone();
                // The app acts through the window: not while this view is being updated.
                window.defer(cx, move |window, cx| {
                    let _ = root.update(cx, |root, cx| match act {
                        Act::OpenInTab => root.history_open_in_tab(&row.row, window, cx),
                        Act::Insert => root.history_insert(&row.row, window, cx),
                        Act::Run => root.history_run(&row.row, window, cx),
                        _ => root.history_analyze(&row.row, window, cx),
                    });
                });
            }
        }
    }

    /// The keys of the list: Enter opens the row, Cmd+Enter runs it, Cmd+C copies its SQL, Delete deletes.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.table.read(cx).focus_handle(cx).contains_focused(window, cx) {
            return;
        }
        let Some(row) = self.selected.clone() else {
            return;
        };
        let key = event.keystroke.key.as_str();
        let command = event.keystroke.modifiers.secondary(cx);
        let act = match key {
            "enter" if command => Act::Run,
            "enter" => Act::OpenInTab,
            "c" if command => Act::Copy,
            "delete" | "backspace" => Act::Delete,
            _ => return,
        };
        if actions_for(&row.row, false).contains(&act) {
            cx.stop_propagation();
            self.act(act, row, window, cx);
        }
    }

    fn ask_delete(&mut self, row: RowView, window: &mut Window, cx: &mut Context<Self>) {
        let Row::Sql(entry) = &row.row else {
            return;
        };
        let id = entry.id;
        let view = cx.entity().downgrade();
        let line = row.line.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title(t!("history.delete_title").into_owned())
                .child(div().debug_selector(|| "history-delete-body".into()).text_size(px(12.0)).child(line.clone()))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text(t!("history.delete").into_owned()))
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = view.update(cx, |view, cx| view.delete_one(id, cx));
                    true
                })
        });
    }

    fn delete_one(&mut self, id: i64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { history::delete_entry(id) }).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    let mut gone_selected = false;
                    this.table.update(cx, |table, cx| {
                        let delegate = table.delegate_mut();
                        delegate.rows.retain(|row| !matches!(&row.row, Row::Sql(entry) if entry.id == id));
                        if delegate.rows.is_empty() {
                            delegate.empty = t!("history.empty_filtered").into_owned().into();
                        }
                        table.clear_selection(cx);
                        cx.notify();
                    });
                    if let Some(Row::Sql(entry)) = this.selected.as_ref().map(|row| &row.row) {
                        gone_selected = entry.id == id;
                    }
                    if gone_selected {
                        this.selected = None;
                    }
                    this.notice = Some(t!("history.deleted").into_owned());
                    cx.notify();
                }
                Err(error) => {
                    this.error = Some(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Clear: asks with the number of runs the current filter shows, then deletes exactly those.
    fn ask_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(filter) = self.query.sql_filter() else {
            return;
        };
        let view = cx.entity().downgrade();
        let counted = filter.clone();
        cx.spawn_in(window, async move |this, cx| {
            let count = cx.background_executor().spawn(async move { history::count(&counted) }).await;
            let _ = this.update_in(cx, |this, window, cx| match count {
                Ok(0) => {
                    this.notice = Some(t!("history.nothing_to_clear").into_owned());
                    cx.notify();
                }
                Ok(count) => {
                    #[cfg(test)]
                    {
                        this.clear_asked = Some(count);
                    }
                    let body = if this.is_filtered() {
                        t!("history.clear_filtered_body", count = count).into_owned()
                    } else {
                        t!("history.clear_all_body", count = count).into_owned()
                    };
                    window.open_alert_dialog(cx, move |dialog, _, _| {
                        let (view, filter, body) = (view.clone(), filter.clone(), body.clone());
                        dialog
                            .title(t!("history.clear_title").into_owned())
                            .child(div().debug_selector(|| "history-clear-body".into()).text_size(px(12.0)).child(body))
                            .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text(t!("history.clear").into_owned()))
                            .confirm()
                            .on_ok(move |_, _, cx| {
                                let (view, filter) = (view.clone(), filter.clone());
                                let _ = view.update(cx, |view, cx| view.clear_confirmed(filter, cx));
                                true
                            })
                    });
                }
                Err(error) => {
                    this.error = Some(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn clear_confirmed(&mut self, filter: history::HistoryFilter, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { history::delete_matching(&filter) }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(count) => this.notice = Some(t!("history.cleared", count = count).into_owned()),
                    Err(error) => this.error = Some(error.to_string()),
                }
                this.reload(cx);
            });
        })
        .detach();
    }

    /// View details: every field of the row, the request and the response of an MCP call.
    fn open_details(&mut self, row: RowView, window: &mut Window, cx: &mut Context<Self>) {
        let names = Arc::clone(&self.context.0);
        let fields = detail_fields(&row.row, &names);
        let request = row.row.statement().to_owned();
        let error = match &row.row {
            Row::Sql(entry) => entry.error.clone(),
            Row::Mcp(_) => None,
        };
        let mcp = matches!(row.row, Row::Mcp(_));
        window.open_dialog(cx, move |dialog, _, _| {
            let list = fields.iter().fold(div().flex().flex_col().gap(px(4.0)), |list, (label, value)| {
                list.child(
                    div()
                        .flex()
                        .gap(px(10.0))
                        .text_size(px(12.0))
                        .child(div().w(px(120.0)).flex_shrink_0().text_color(theme::text_muted()).child(label.clone()))
                        .child(div().flex_1().min_w_0().child(value.clone())),
                )
            });
            let block = |selector: &'static str, title: String, text: String| {
                div()
                    .debug_selector(move || selector.into())
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(title))
                    .child(
                        div()
                            .max_h(px(160.0))
                            .overflow_hidden()
                            .px(px(8.0))
                            .py(px(6.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(theme::border())
                            .bg(theme::chrome())
                            .text_size(px(12.0))
                            .child(text),
                    )
            };
            let body = div()
                .debug_selector(|| "history-details".into())
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(list)
                .when(!request.is_empty(), |body| {
                    body.child(block("history-details-request", if mcp { t!("history.mcp_request") } else { t!("history.statement") }.into_owned(), request.clone()))
                })
                .when_some(error.clone(), |body, error| body.child(block("history-details-error", t!("history.d_error").into_owned(), error)))
                .when(mcp, |body| {
                    body.child(div().debug_selector(|| "history-details-legacy".into()).text_size(px(11.0)).text_color(theme::text_muted()).child(t!("history.mcp_response_note").into_owned()))
                });
            let copy_text = request.clone();
            dialog
                .w(px(600.0))
                .title(t!("history.details").into_owned())
                .child(body)
                .footer(
                    DialogFooter::new()
                        .when(!copy_text.is_empty(), |footer| {
                            footer.child(
                                Button::new("history-details-copy")
                                    .label(t!("history.copy").into_owned())
                                    .compact()
                                    .ghost()
                                    .debug_selector(|| "history-details-copy".into())
                                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()))),
                            )
                        })
                        .child(
                            Button::new("history-details-close")
                                .label(t!("history.close").into_owned())
                                .compact()
                                .debug_selector(|| "history-details-close".into())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
        });
    }
}

fn same_row(a: &Row, b: &Row) -> bool {
    match (a, b) {
        (Row::Sql(a), Row::Sql(b)) => a.id == b.id,
        (Row::Mcp(a), Row::Mcp(b)) => a == b,
        _ => false,
    }
}

/// The label and value of every field of a row that has one.
fn detail_fields(row: &Row, names: &Names) -> Vec<(SharedString, SharedString)> {
    let mut fields: Vec<(SharedString, SharedString)> = Vec::new();
    let mut add = |label: &str, value: String| {
        if !value.is_empty() {
            fields.push((t!(label).into_owned().into(), value.into()));
        }
    };
    let status = |failed: bool| if failed { t!("history.status_failed") } else { t!("history.status_ok") }.into_owned();
    match row {
        Row::Sql(entry) => {
            add("history.d_kind", kind_name(row).to_owned());
            add("history.d_connection", model::connection_name(names, &entry.conn_uuid));
            add("history.d_database", entry.database.clone());
            add("history.d_table", entry.collection.clone());
            add("history.d_time", model::format_full_time(entry.timestamp));
            add("history.d_duration", entry.duration_ms.map(|ms| model::format_duration(ms.max(0) as u64)).unwrap_or_default());
            if !entry.failed() {
                add("history.d_rows", entry.result_count.to_string());
            }
            add("history.d_status", status(entry.failed()));
        }
        Row::Mcp(call) => {
            add("history.d_tool", call.tool.clone());
            add("history.d_kind", kind_name(row).to_owned());
            add("history.d_connection", call.connection_id.as_deref().map(|id| model::connection_name(names, id)).unwrap_or_default());
            add("history.d_time", model::format_full_time(call.time));
            add("history.d_duration", model::format_duration(call.duration_ms));
            add("history.d_status", status(!call.ok()));
            add("history.d_outcome", call.outcome.clone());
            add("history.d_rows", call.rows.map(|rows| rows.to_string()).unwrap_or_default());
            add("history.d_approval", call.approval.clone().unwrap_or_default());
        }
    }
    fields
}

fn kind_name(row: &Row) -> String {
    if row.is_mcp() {
        return t!("history.k_mcp").into_owned();
    }
    match row.kind() {
        history::KIND_DATA_CHANGE => t!("history.k_data"),
        history::KIND_SCHEMA_CHANGE => t!("history.k_schema"),
        history::KIND_REDIS_COMMAND => t!("history.k_redis"),
        _ => t!("history.k_query"),
    }
    .into_owned()
}

/// The background half of a reload: the audit log (once), the scope choices and the first page.
fn read_first_page(
    query: Query,
    names: Arc<Names>,
    audit: Option<std::path::PathBuf>,
    cached: Option<Arc<(Vec<McpCall>, Vec<String>)>>,
    generation: u64,
) -> Loaded {
    let mut error = None;
    let wants_mcp = query.takes_mcp() || query.source == Source::Mcp;
    let mcp = match (cached, wants_mcp, audit) {
        (Some(cached), ..) => Some(cached),
        (None, true, Some(path)) => match model::read_mcp(&path) {
            Ok(read) => Some(Arc::new(read)),
            Err(why) => {
                error = Some(why);
                None
            }
        },
        _ => None,
    };
    let empty = (Vec::new(), Vec::new());
    let calls = mcp.as_deref().unwrap_or(&empty);
    let mut pager = Pager::new(&query, &calls.0, model::storage_fetch());
    let options = history::scope_options()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, databases)| ScopeOption { name: model::connection_name(&names, &id), id, databases })
        .collect();
    let (pager, rows) = match pager.next(PAGE as usize) {
        Ok(rows) => (Ok(pager), rows.into_iter().map(|row| RowView::new(row, &names)).collect()),
        Err(why) => (Err(why.to_string()), Vec::new()),
    };
    Loaded { generation, pager, rows, options, mcp, error }
}

// ---- drawing -------------------------------------------------------------------------------------------

impl HistoryView {
    fn scope_summary(&self) -> String {
        let scopes = &self.query.scopes;
        if scopes.is_empty() {
            return t!("history.scope_all").into_owned();
        }
        let whole = scopes.iter().filter(|scope| scope.databases.is_empty()).count();
        let databases: usize = scopes.iter().map(|scope| scope.databases.len()).sum();
        let mut parts = Vec::new();
        if whole > 0 {
            parts.push(t!("history.scope_count_connections", count = whole).into_owned());
        }
        if databases > 0 {
            parts.push(t!("history.scope_count_databases", count = databases).into_owned());
        }
        parts.join(" / ")
    }

    fn range_summary(&self) -> String {
        match self.quick {
            Some(Quick::AllTime) => t!("history.range_all").into_owned(),
            Some(Quick::Today) => t!("history.range_today").into_owned(),
            Some(Quick::Week) => t!("history.range_week").into_owned(),
            Some(Quick::Month) => t!("history.range_month").into_owned(),
            None => {
                let (start, end) = (self.start_date_text(), self.end_date_text());
                format!("{} \u{2192} {}", if start.is_empty() { "\u{2026}" } else { &start }, if end.is_empty() { "\u{2026}" } else { &end })
            }
        }
    }

    fn start_date_text(&self) -> String {
        self.query.since.map(|secs| model::format_full_time(secs)[..10].to_owned()).unwrap_or_default()
    }

    fn end_date_text(&self) -> String {
        self.query
            .until
            .map(|secs| model::format_full_time(secs - 1.0)[..10].to_owned())
            .unwrap_or_default()
    }

    fn render_scope_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let needle = self.scope_search.read(cx).value().to_lowercase();
        let wanted = |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle);
        let selected = |id: &str| self.query.scopes.iter().find(|scope| scope.conn_uuid == id);
        let mut connections = div().flex().flex_col().gap(px(2.0));
        let mut databases = div().flex().flex_col().gap(px(2.0));
        let any_selected = !self.query.scopes.is_empty();
        let mut shown = 0;
        for (index, option) in self.options.iter().enumerate() {
            let matches = wanted(&option.name) || option.databases.iter().any(|database| wanted(database));
            if !matches {
                continue;
            }
            shown += 1;
            let id = option.id.clone();
            let scope = selected(&option.id);
            connections = connections.child(
                div().debug_selector(move || format!("history-scope-connection-{index}")).self_start().child(
                    Checkbox::new(("history-scope-connection", index))
                        .label(option.name.clone())
                        .checked(scope.is_some())
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_connection(&id, cx))),
                ),
            );
            if !any_selected || scope.is_some() {
                for (at, database) in option.databases.iter().enumerate() {
                    if !wanted(database) && !wanted(&option.name) {
                        continue;
                    }
                    let (id, name) = (option.id.clone(), database.clone());
                    let on = scope.is_some_and(|scope| scope.databases.contains(database));
                    databases = databases.child(
                        div().debug_selector(move || format!("history-scope-database-{index}-{at}")).self_start().child(
                            Checkbox::new(("history-scope-database", index * 1000 + at))
                                .label(format!("{} \u{b7} {}", option.name, database))
                                .checked(on)
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle_database(&id, &name, cx))),
                        ),
                    );
                }
            }
        }
        let section = |title: String, body: Div| {
            div().flex().flex_col().gap(px(4.0)).child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(title)).child(body)
        };
        div()
            .debug_selector(|| "history-scope-panel".into())
            .w(px(360.0))
            .max_h(px(420.0))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().debug_selector(|| "history-scope-search".into()).child(Input::new(&self.scope_search).small()))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        Button::new("history-scope-current")
                            .label(t!("history.scope_current").into_owned())
                            .compact()
                            .outline()
                            .small()
                            .debug_selector(|| "history-scope-current".into())
                            .disabled(!self.current.as_ref().is_some_and(|current| self.options.iter().any(|option| option.id.eq_ignore_ascii_case(current))))
                            .on_click(cx.listener(|this, _, _, cx| this.select_current_scope(cx))),
                    )
                    .child(
                        Button::new("history-scope-reset")
                            .label(t!("history.scope_reset").into_owned())
                            .compact()
                            .ghost()
                            .small()
                            .debug_selector(|| "history-scope-reset".into())
                            .disabled(!any_selected)
                            .on_click(cx.listener(|this, _, _, cx| this.reset_scope(cx))),
                    ),
            )
            .child(
                div()
                    .id("history-scope-list")
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .overflow_y_scroll()
                    .when(shown == 0, |list| {
                        list.child(div().text_size(px(12.0)).text_color(theme::text_muted()).child(t!("history.scope_none").into_owned()))
                    })
                    .when(shown > 0, |list| {
                        list.child(section(t!("history.scope_connections").into_owned(), connections))
                            .child(section(t!("history.scope_databases").into_owned(), databases))
                    }),
            )
    }

    fn render_range_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self.range_error.map(|error| match error {
            RangeError::BadStart | RangeError::BadEnd => t!("history.range_bad_date").into_owned(),
            RangeError::Inverted => t!("history.range_invalid").into_owned(),
        });
        let quick_buttons = Quick::ALL.iter().fold(div().flex().flex_wrap().gap(px(6.0)), |row, quick| {
            let quick = *quick;
            row.child(
                Button::new(("history-range-quick", quick as usize))
                    .label(match quick {
                        Quick::Today => t!("history.range_today"),
                        Quick::Week => t!("history.range_week"),
                        Quick::Month => t!("history.range_month"),
                        Quick::AllTime => t!("history.range_all"),
                    }
                    .into_owned())
                    .compact()
                    .small()
                    .outline()
                    .selected(self.quick == Some(quick))
                    .debug_selector(move || format!("history-range-{}", quick.key()))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_quick(quick, cx))),
            )
        });
        div()
            .debug_selector(|| "history-range-panel".into())
            .w(px(320.0))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(quick_buttons)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(div().debug_selector(|| "history-range-start".into()).flex_1().child(Input::new(&self.start_date).small()))
                    .child(div().debug_selector(|| "history-range-end".into()).flex_1().child(Input::new(&self.end_date).small())),
            )
            .children(error.map(|text| div().debug_selector(|| "history-range-error".into()).text_size(px(11.5)).text_color(theme::danger()).child(text)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        Button::new("history-range-clear")
                            .label(t!("history.range_clear").into_owned())
                            .compact()
                            .ghost()
                            .small()
                            .debug_selector(|| "history-range-clear".into())
                            .on_click(cx.listener(|this, _, window, cx| this.clear_range(window, cx))),
                    )
                    .child(
                        Button::new("history-range-apply")
                            .label(t!("history.range_apply").into_owned())
                            .compact()
                            .small()
                            .debug_selector(|| "history-range-apply".into())
                            .on_click(cx.listener(|this, _, window, cx| this.apply_range(window, cx))),
                    ),
            )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.selected.clone() else {
            return div()
                .debug_selector(|| "history-preview".into())
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(px(16.0))
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(t!("history.select_hint").into_owned())
                .into_any_element();
        };
        let names = Arc::clone(&self.context.0);
        let analyze = !row.row.is_mcp()
            && self.root.read_with(cx, |root, _| root.history_ai_available(row.row.connection_id().unwrap_or_default())).unwrap_or(false);
        let fields = detail_fields(&row.row, &names);
        let buttons = actions_for(&row.row, analyze).into_iter().fold(div().flex().flex_wrap().gap(px(6.0)), |bar, act| {
            let (selector, icon) = match act {
                Act::OpenInTab => ("history-act-open", "icons/square-terminal.svg"),
                Act::Insert => ("history-act-insert", "icons/list-ordered.svg"),
                Act::Copy => ("history-act-copy", "icons/copy.svg"),
                Act::Run => ("history-act-run", "icons/play.svg"),
                Act::Analyze => ("history-act-analyze", "icons/wand-sparkles.svg"),
                Act::Details => ("history-act-details", "icons/info.svg"),
                Act::Delete => ("history-act-delete", "icons/trash.svg"),
            };
            let picked = row.clone();
            bar.child(
                Button::new(selector)
                    .label(act_label(act))
                    .icon(Icon::empty().path(icon).size(px(12.0)))
                    .compact()
                    .small()
                    .outline()
                    .debug_selector(move || selector.into())
                    .on_click(cx.listener(move |this, _, window, cx| this.act(act, picked.clone(), window, cx))),
            )
        });
        let list = fields.iter().fold(div().flex().flex_col().gap(px(2.0)), |list, (label, value)| {
            list.child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .text_size(px(11.5))
                    .child(div().w(px(84.0)).flex_shrink_0().text_color(theme::text_muted()).child(label.clone()))
                    .child(div().flex_1().min_w_0().truncate().child(value.clone())),
            )
        });
        let error = match &row.row {
            Row::Sql(entry) => entry.error.clone(),
            Row::Mcp(_) => None,
        };
        div()
            .debug_selector(|| "history-preview".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .child(
                div()
                    .debug_selector(|| "history-preview-sql".into())
                    .flex_1()
                    .min_h(px(120.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme::border())
                    .overflow_hidden()
                    .child(Editor::new(&self.preview).readonly(true).appearance(false).bordered(false).h_full()),
            )
            .children(error.map(|error| {
                div()
                    .debug_selector(|| "history-preview-error".into())
                    .max_h(px(64.0))
                    .overflow_hidden()
                    .text_size(px(11.5))
                    .text_color(theme::danger())
                    .child(error)
            }))
            .child(div().debug_selector(|| "history-preview-fields".into()).child(list))
            .child(div().debug_selector(|| "history-preview-actions".into()).child(buttons))
            .into_any_element()
    }
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(tools) = self.pending_tools.take() {
            cx.defer_in(window, move |this, window, cx| this.apply_tools(tools, window, cx));
        }
        let source = self.query.source;
        let rows = self.row_count(cx);
        let more = self.pager.as_ref().is_some_and(|pager| !pager.exhausted());
        let can_clear = self.query.sql_filter().is_some();
        let source_group = ButtonGroup::new("history-source-group")
            .outline()
            .compact()
            .with_size(Size::XSmall)
            .children(Source::ALL.iter().enumerate().map(|(at, option)| {
                let key = option.key();
                Button::new(("history-source", at))
                    .debug_selector(move || format!("history-source-{key}"))
                    .label(match option {
                        Source::All => t!("history.source_all"),
                        Source::Sql => t!("history.source_sql"),
                        Source::Mcp => t!("history.source_mcp"),
                    }
                    .into_owned())
                    .selected(source == *option)
            }))
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                if let Some(option) = selected.first().and_then(|index| Source::ALL.get(*index)) {
                    this.set_source(*option, cx);
                }
            }));
        let offered = KindFilter::offered(source);
        let kind_group = ButtonGroup::new("history-kind-group")
            .outline()
            .compact()
            .with_size(Size::XSmall)
            .children(offered.iter().enumerate().map(|(at, kind)| {
                let key = kind.key();
                Button::new(("history-kind", at))
                    .debug_selector(move || format!("history-kind-{key}"))
                    .label(match kind {
                        KindFilter::All => t!("history.kind_all"),
                        KindFilter::Query => t!("history.kind_query"),
                        KindFilter::DataChange => t!("history.kind_data"),
                        KindFilter::SchemaChange => t!("history.kind_schema"),
                        KindFilter::RedisCommand => t!("history.kind_redis"),
                        KindFilter::Failed => t!("history.kind_failed"),
                        KindFilter::Succeeded => t!("history.kind_succeeded"),
                    }
                    .into_owned())
                    .selected(self.query.kind == *kind)
            }))
            .on_click(cx.listener(move |this, selected: &Vec<usize>, _, cx| {
                if let Some(kind) = selected.first().and_then(|index| KindFilter::offered(this.query.source).get(*index)) {
                    this.set_kind(*kind, cx);
                }
            }));
        let scope_button = Button::new("history-scope")
            .debug_selector(|| "history-scope".into())
            .icon(Icon::empty().path("icons/database.svg").size(px(12.0)))
            .label(self.scope_summary())
            .compact()
            .small()
            .outline()
            .selected(!self.query.scopes.is_empty());
        let range_button = Button::new("history-range")
            .debug_selector(|| "history-range".into())
            .icon(Icon::empty().path("icons/calendar-clock.svg").size(px(12.0)))
            .label(self.range_summary())
            .compact()
            .small()
            .outline()
            .selected(self.quick != Some(Quick::AllTime));
        let scope_panel = self.scope_open.then(|| self.render_scope_panel(cx));
        let range_panel = self.range_open.then(|| self.render_range_panel(cx));
        let scope = Popover::new("history-scope-popover")
            .trigger(scope_button)
            .open(self.scope_open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.scope_open = *open;
                cx.notify();
            }))
            .children(scope_panel);
        let range = Popover::new("history-range-popover")
            .trigger(range_button)
            .open(self.range_open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(cx.listener(|this, open: &bool, window, cx| {
                this.range_open = *open;
                if *open {
                    let (start, end) = (this.start_date_text(), this.end_date_text());
                    this.start_date.update(cx, |input, cx| input.set_value(start, window, cx));
                    this.end_date.update(cx, |input, cx| input.set_value(end, window, cx));
                    this.range_error = None;
                }
                cx.notify();
            }))
            .children(range_panel);
        let count = if self.loading && !self.loaded {
            t!("history.loading").into_owned()
        } else if more {
            t!("history.shown_more", count = rows).into_owned()
        } else {
            t!("history.shown", count = rows).into_owned()
        };
        let message = self.error.clone().map(|error| t!("history.read_error", error = error).into_owned());
        let notice = self.notice.clone();
        let clear_note = (source == Source::Mcp).then(|| t!("history.mcp_read_only").into_owned());
        div()
            .id("history-view")
            .debug_selector(|| "history-view".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_size(px(12.0))
            .text_color(theme::text())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| this.key_down(event, window, cx)))
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
                    .child(Icon::empty().path("icons/list-ordered.svg").size(px(16.0)).text_color(theme::accent()))
                    .child(div().debug_selector(|| "history-title".into()).flex_1().truncate().text_size(px(13.0)).child(t!("history.title").into_owned()))
                    .child(
                        Button::new("history-refresh")
                            .debug_selector(|| "history-refresh".into())
                            .icon(Icon::empty().path("icons/refresh-cw.svg").size(px(12.0)))
                            .tooltip(t!("history.refresh").into_owned())
                            .compact()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_from_button(cx))),
                    )
                    .child(
                        Button::new("history-clear")
                            .debug_selector(|| "history-clear".into())
                            .icon(Icon::empty().path("icons/trash.svg").size(px(12.0)))
                            .label(t!("history.clear").into_owned())
                            .compact()
                            .small()
                            .outline()
                            .disabled(!can_clear)
                            .when_some(clear_note, |button, note| button.tooltip(note))
                            .on_click(cx.listener(|this, _, window, cx| this.ask_clear(window, cx))),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(6.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().debug_selector(|| "history-source".into()).child(source_group))
                    .child(div().debug_selector(|| "history-kinds".into()).child(kind_group)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().debug_selector(|| "history-search".into()).flex_1().min_w(px(160.0)).child(Input::new(&self.search).small()))
                    .child(div().flex_shrink_0().child(scope))
                    .child(div().flex_shrink_0().child(range))
                    .when(source == Source::Mcp, |bar| {
                        bar.child(div().debug_selector(|| "history-tool".into()).w(px(170.0)).flex_shrink_0().child(Select::new(&self.tool_select).small().w_full()))
                    }),
            )
            .children(message.map(|text| {
                div()
                    .debug_selector(|| "history-error".into())
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .text_size(px(11.5))
                    .text_color(theme::danger())
                    .child(text)
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().debug_selector(|| "history-table".into()).flex_1().min_h_0().child(DataTable::new(&self.table).stripe(true)))
                            .child(
                                div()
                                    .h(px(34.0))
                                    .flex_shrink_0()
                                    .px(px(12.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .border_t_1()
                                    .border_color(theme::border())
                                    .child(div().debug_selector(|| "history-count".into()).text_size(px(11.0)).text_color(theme::text_muted()).child(count))
                                    .when(more, |footer| {
                                        footer.child(
                                            Button::new("history-load-more")
                                                .debug_selector(|| "history-load-more".into())
                                                .label(t!("history.load_more").into_owned())
                                                .compact()
                                                .small()
                                                .outline()
                                                .disabled(self.loading)
                                                .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                                        )
                                    })
                                    .children(notice.map(|text| div().debug_selector(|| "history-notice".into()).text_size(px(11.0)).text_color(theme::text_muted()).child(text))),
                            ),
                    )
                    .child(
                        div()
                            .w(px(340.0))
                            .min_w(px(240.0))
                            .flex_shrink_0()
                            .border_l_1()
                            .border_color(theme::border())
                            .bg(theme::surface())
                            .child(self.render_preview(cx)),
                    ),
            )
    }
}
