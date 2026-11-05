//! The two windows onto open transactions: a tab's transaction log (opened from the status text of
//! its toolbar) and the list of every open transaction (menu, command palette, and the question
//! asked before a session would be closed), with Commit / Roll Back per row and for the selected.
//!
//! Costs. The log is a snapshot formatted once when the dialog opens (at most
//! [`crate::transaction::LOG_MAX_ENTRIES`] rows, drawn by a `uniform_list`), so nothing in it
//! changes or allocates per frame. The pending list is one entity that re-reads the app's
//! transactions when the app notifies; its rows are drawn by a `uniform_list` too, and a row
//! has no entity or listener of its own beyond its two buttons.

use super::transaction::TxEnd;
use super::*;
use crate::transaction::{elapsed_label, statements_label, LogResult, TabTx};
use gpui::{Entity, SharedString, Subscription, WeakEntity};
use gpui_component::dialog::{DialogClose, DialogFooter};
use std::rc::Rc;

const ROW_HEIGHT: f32 = 28.0;
/// Rows a list shows before it scrolls.
const VISIBLE_ROWS: usize = 10;

/// One statement of the log, ready to draw.
struct LogRow {
    time: SharedString,
    modifying: bool,
    /// The server committed on its own here: not something Roll Back undoes.
    commits: bool,
    sql: SharedString,
    rows: SharedString,
    duration: SharedString,
    result: SharedString,
    failed: bool,
}

/// `12 ms`, `1.25 s`, `2m 05s`.
fn duration_label(duration: Duration) -> String {
    match duration.as_millis() {
        0..=999 => format!("{} ms", duration.as_millis()),
        1000..=59_999 => format!("{:.2} s", duration.as_secs_f64()),
        _ => elapsed_label(duration),
    }
}

impl AppRoot {
    /// The statements the tab's open transaction ran, in a dialog (a snapshot taken now).
    pub(super) fn open_transaction_log(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tx) = self.tab_tx(tab_id) else { return };
        let rows: Vec<LogRow> = tx
            .log
            .entries()
            .map(|entry| LogRow {
                time: chrono::DateTime::<chrono::Local>::from(entry.at).format("%H:%M:%S").to_string().into(),
                modifying: entry.writes,
                commits: entry.commits,
                sql: entry.sql.clone().into(),
                rows: entry.rows.map_or_else(String::new, |rows| rows.to_string()).into(),
                duration: duration_label(entry.duration).into(),
                failed: entry.result != LogResult::Ok,
                result: match (entry.result, &entry.error) {
                    (LogResult::Ok, _) => "OK".into(),
                    (LogResult::Failed, Some(error)) => format!("Failed: {error}"),
                    (LogResult::Failed, None) => "Failed".into(),
                    (LogResult::Cancelled, _) => "Cancelled".into(),
                }
                .into(),
            })
            .collect();
        let summary = log_summary(tx);
        let older = tx.log.dropped();
        let title = format!("Transaction log \u{2014} {}", self.tab_title(tab_id));
        let rows = Rc::new(rows);
        let scroll = UniformListScrollHandle::new();
        window.open_dialog(cx, move |dialog, _, _| {
            let count = rows.len();
            let list_rows = Rc::clone(&rows);
            let list = uniform_list("tx-log-rows", count, move |range, _, _| {
                range.map(|index| log_row(index, &list_rows[index])).collect::<Vec<_>>()
            })
            .size_full()
            // Rows take the list's width (a list sizes itself to its widest row otherwise).
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::FitList)
            .track_scroll(&scroll);
            dialog
                .title(title.clone())
                .width(px(860.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().debug_selector(|| "tx-log-summary".into()).text_size(px(12.0)).text_color(theme::text_muted()).child(summary.clone()))
                        .child(log_header())
                        .child(if count == 0 {
                            div()
                                .debug_selector(|| "tx-log-empty".into())
                                .text_size(px(12.0))
                                .text_color(theme::text_faint())
                                .child("No statement has run in this transaction yet.")
                                .into_any_element()
                        } else {
                            div()
                                .debug_selector(|| "tx-log-list".into())
                                .h(px(ROW_HEIGHT * count.min(VISIBLE_ROWS) as f32))
                                .child(list)
                                .into_any_element()
                        })
                        .children((older > 0).then(|| {
                            div()
                                .debug_selector(|| "tx-log-older".into())
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .child(format!("{older} older statements are not kept."))
                        })),
                )
                .footer(
                    DialogFooter::new()
                        .child(div().debug_selector(|| "tx-log-close".into()).child(DialogClose::new().trigger(|button| button.label("Close")))),
                )
        });
    }

    /// Every open transaction, with Commit / Roll Back.
    pub(super) fn open_pending_transactions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = cx.entity();
        let (rows, seen) = (self.pending_transaction_rows(), self.transactions.revision);
        let view = cx.new(|cx| PendingTransactions::new(&root, rows, seen, cx));
        self.transactions.pending_view = Some(view.downgrade());
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Pending transactions")
                .width(px(900.0))
                .child(view.clone())
                .footer(DialogFooter::new().child(div().debug_selector(|| "tx-pending-close".into()).child(DialogClose::new().trigger(|button| button.label("Close")))))
        });
    }

    /// The tabs that have a transaction open, for the pending list.
    pub(super) fn pending_transaction_rows(&self) -> Vec<PendingRow> {
        let now = Instant::now();
        self.all_open_transaction_tabs()
            .into_iter()
            .filter_map(|tab_id| {
                let (tab, tx) = (self.workspace.tab(tab_id)?, self.tab_tx(tab_id)?);
                let connection = self.saved_of(&tab.connection_id).map_or_else(|| "(unknown)".to_owned(), |saved| saved.name.clone());
                // The database the transaction lives on, else the tab's own choice.
                let database = self.own_session(tab_id, &tab.connection_id).map(|own| own.database.clone()).filter(|name| !name.is_empty()).unwrap_or_else(|| tab.database.clone());
                // A tab that names none is on the connection's own database.
                let database = match self.saved_of(&tab.connection_id) {
                    Some(saved) => crate::transaction::database_name(saved.kind, &saved.uri, &database),
                    None => database,
                };
                Some(PendingRow {
                    tab_id,
                    connection,
                    database: if database.is_empty() { "\u{2014}".to_owned() } else { database },
                    tab: tab.title.clone(),
                    statements: tx.statements,
                    modifying: tx.modifying,
                    open_for: tx.since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since)),
                    blocked: self.end_blocked(tab_id).is_some(),
                })
            })
            .collect()
    }
}

fn log_summary(tx: &TabTx) -> String {
    let elapsed = tx.since.map_or(Duration::ZERO, |since| Instant::now().saturating_duration_since(since));
    format!("{} \u{00B7} {} modifying \u{00B7} open for {}", statements_label(tx.statements), tx.modifying, elapsed_label(elapsed))
}

const TIME_W: f32 = 64.0;
const KIND_W: f32 = 64.0;
const ROWS_W: f32 = 64.0;
const DURATION_W: f32 = 76.0;
const RESULT_W: f32 = 230.0;

fn log_header() -> impl IntoElement {
    let cell = |label: &'static str, width: Option<f32>| {
        let cell = div().text_size(px(11.0)).text_color(theme::text_muted()).child(label);
        match width {
            Some(width) => cell.w(px(width)).flex_shrink_0(),
            None => cell.flex_1().min_w_0(),
        }
    };
    div()
        .flex()
        .gap(px(8.0))
        .px(px(8.0))
        .pb(px(4.0))
        .border_b_1()
        .border_color(theme::border())
        .child(cell("Time", Some(TIME_W)))
        .child(cell("Kind", Some(KIND_W)))
        .child(cell("Statement", None))
        .child(cell("Rows", Some(ROWS_W)))
        .child(cell("Took", Some(DURATION_W)))
        .child(cell("Result", Some(RESULT_W)))
}

/// A cut-off cell that shows its whole text in a tooltip.
fn text_cell(id: (&'static str, usize), text: &SharedString, color: gpui::Rgba) -> gpui::Stateful<Div> {
    let full = text.clone();
    div()
        .id(id)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(px(12.0))
        .text_color(color)
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(full.clone()).build(window, cx))
        .child(text.clone())
}

/// `Reads`, `Modifies`, or `Commits` for a statement the server committed by itself.
fn log_kind(row: &LogRow) -> &'static str {
    if row.commits {
        "Commits"
    } else if row.modifying {
        "Modifies"
    } else {
        "Reads"
    }
}

fn log_row(index: usize, row: &LogRow) -> impl IntoElement {
    let fixed = |width: f32| div().w(px(width)).flex_shrink_0().overflow_hidden().whitespace_nowrap().text_size(px(12.0));
    div()
        .id(("tx-log-row", index))
        .debug_selector(move || format!("tx-log-row-{index}"))
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(8.0))
        .gap(px(8.0))
        .flex()
        .items_center()
        .child(fixed(TIME_W).text_color(theme::text_muted()).child(row.time.clone()))
        .child(
            fixed(KIND_W)
                .debug_selector({
                    let kind = log_kind(row);
                    move || format!("tx-log-kind-{index}-{}", kind.to_lowercase())
                })
                .text_color(if row.commits { theme::danger() } else if row.modifying { theme::warning() } else { theme::text_muted() })
                .child(log_kind(row)),
        )
        .child(text_cell(("tx-log-sql", index), &row.sql, theme::text()).flex_1().min_w_0())
        .child(fixed(ROWS_W).text_color(theme::text_muted()).child(row.rows.clone()))
        .child(fixed(DURATION_W).text_color(theme::text_muted()).child(row.duration.clone()))
        .child(text_cell(("tx-log-result", index), &row.result, if row.failed { theme::danger() } else { theme::success() }).w(px(RESULT_W)).flex_shrink_0())
}

// ---- the pending list ---------------------------------------------------------------------------

pub(super) struct PendingRow {
    pub(super) tab_id: u64,
    connection: String,
    database: String,
    tab: String,
    pub(super) statements: u32,
    modifying: u32,
    open_for: Duration,
    /// A statement runs or Commit / Roll Back is on its way: the row's buttons wait.
    pub(super) blocked: bool,
}

/// The pending-transactions list. Rows are read from the app whenever it notifies; the tabs the
/// user took out of the selection are remembered, so a transaction that opens meanwhile starts
/// selected.
pub(super) struct PendingTransactions {
    root: WeakEntity<AppRoot>,
    deselected: HashSet<u64>,
    scroll: UniformListScrollHandle,
    /// The rows as of `seen`: read again only when the app's transactions changed (or a second
    /// passed, for "Open for"), not on every redraw of the app.
    rows: Rc<Vec<PendingRow>>,
    seen: u64,
    _observe: Subscription,
}

impl PendingTransactions {
    fn new(root: &Entity<AppRoot>, rows: Vec<PendingRow>, seen: u64, cx: &mut Context<Self>) -> Self {
        Self {
            root: root.downgrade(),
            deselected: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            rows: Rc::new(rows),
            seen,
            _observe: cx.observe(root, |this, root, cx| {
                let revision = root.read(cx).transactions.revision;
                if revision != this.seen {
                    this.seen = revision;
                    this.rows = Rc::new(root.read(cx).pending_transaction_rows());
                    cx.notify();
                }
            }),
        }
    }

    fn end(&self, tabs: Vec<u64>, end: TxEnd, cx: &mut gpui::App) {
        if tabs.is_empty() {
            return;
        }
        let _ = self.root.update(cx, |root, cx| root.end_transactions_skipping_running(tabs, end, cx));
    }

    /// The rows as the app's transactions stand now (the ticker, once a second, for "Open for").
    pub(super) fn refresh(&mut self, rows: Vec<PendingRow>, revision: u64, cx: &mut Context<Self>) {
        self.seen = revision;
        self.rows = Rc::new(rows);
        cx.notify();
    }

    /// The rows ticked, and of those the ones a bulk action may take: a tab that is running a
    /// statement (or ending its transaction) is skipped, never cancelled from here.
    fn selected(&self, rows: &[PendingRow]) -> (usize, Vec<u64>) {
        let ticked = rows.iter().filter(|row| !self.deselected.contains(&row.tab_id));
        (ticked.clone().count(), ticked.filter(|row| !row.blocked).map(|row| row.tab_id).collect())
    }
}

impl Render for PendingTransactions {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = Rc::clone(&self.rows);
        let (ticked, selected) = self.selected(&rows);
        if rows.is_empty() {
            return div()
                .debug_selector(|| "tx-pending-empty".into())
                .text_size(px(13.0))
                .text_color(theme::text_muted())
                .child("No transaction is open.")
                .into_any_element();
        }
        let count = rows.len();
        let all = ticked == count;
        let view = cx.entity().downgrade();
        let header_cell = |label: &'static str, width: Option<f32>| {
            let cell = div().text_size(px(11.0)).text_color(theme::text_muted()).child(label);
            match width {
                Some(width) => cell.w(px(width)).flex_shrink_0(),
                None => cell.flex_1().min_w_0(),
            }
        };
        let all_tabs: Vec<u64> = rows.iter().map(|row| row.tab_id).collect();
        let select_all = {
            let view = view.clone();
            div().debug_selector(|| "tx-pending-all".into()).w(px(22.0)).flex_shrink_0().child(
                Checkbox::new("tx-pending-select-all").checked(all).on_click(move |checked: &bool, _, cx| {
                    let (checked, all_tabs) = (*checked, all_tabs.clone());
                    let _ = view.update(cx, |this, cx| {
                        if checked {
                            this.deselected.clear();
                        } else {
                            this.deselected.extend(all_tabs);
                        }
                        cx.notify();
                    });
                }),
            )
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .pb(px(4.0))
            .border_b_1()
            .border_color(theme::border())
            .child(select_all)
            .child(header_cell("Connection", Some(150.0)))
            .child(header_cell("Database", Some(110.0)))
            .child(header_cell("Tab", None))
            .child(header_cell("Statements", Some(84.0)))
            .child(header_cell("Modifying", Some(76.0)))
            .child(header_cell("Open for", Some(72.0)))
            .child(div().w(px(150.0)).flex_shrink_0());
        let list_rows = Rc::clone(&rows);
        let deselected = self.deselected.clone();
        let list = uniform_list(
            "tx-pending-rows",
            count,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let view = cx.entity().downgrade();
                range
                    .map(|index| this.row(index, &list_rows[index], !deselected.contains(&list_rows[index].tab_id), view.clone()))
                    .collect::<Vec<_>>()
            }),
        )
        .size_full()
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::FitList)
        .track_scroll(&self.scroll);
        let (commit_tabs, roll_back_tabs) = (selected.clone(), selected.clone());
        let (commit_view, roll_back_view) = (view.clone(), view);
        let button = |id: &'static str, label: String| Button::new(id).debug_selector(move || id.into()).label(label).compact();
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(header)
            .child(div().debug_selector(|| "tx-pending-list".into()).h(px(ROW_HEIGHT * count.min(VISIBLE_ROWS) as f32)).child(list))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(div().debug_selector(|| "tx-pending-count".into()).flex_1().text_size(px(12.0)).text_color(theme::text_muted()).child(
                        if ticked > selected.len() {
                            format!("{ticked} of {count} selected, {} busy and skipped", ticked - selected.len())
                        } else {
                            format!("{ticked} of {count} selected")
                        },
                    ))
                    .child(
                        button("tx-pending-rollback-selected", "Roll Back selected".into()).ghost().disabled(selected.is_empty()).on_click(move |_, _, cx| {
                            let tabs = roll_back_tabs.clone();
                            let _ = roll_back_view.update(cx, |this, cx| this.end(tabs, TxEnd::Rollback, cx));
                        }),
                    )
                    .child(
                        button("tx-pending-commit-selected", "Commit selected".into()).primary().disabled(selected.is_empty()).on_click(move |_, _, cx| {
                            let tabs = commit_tabs.clone();
                            let _ = commit_view.update(cx, |this, cx| this.end(tabs, TxEnd::Commit, cx));
                        }),
                    ),
            )
            .into_any_element()
    }
}

impl PendingTransactions {
    fn row(&self, index: usize, row: &PendingRow, checked: bool, view: WeakEntity<Self>) -> impl IntoElement {
        let tab_id = row.tab_id;
        let fixed = |width: f32, text: String| div().w(px(width)).flex_shrink_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.0)).child(text);
        let (commit_root, roll_back_root) = (self.root.clone(), self.root.clone());
        let button = |id: &'static str, label: &'static str| {
            Button::new((id, tab_id)).debug_selector(move || format!("{id}-{tab_id}")).label(label).compact().disabled(row.blocked)
        };
        div()
            .id(("tx-pending-row", index))
            .debug_selector(move || format!("tx-pending-row-{tab_id}"))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(8.0))
            .gap(px(8.0))
            .flex()
            .items_center()
            .child(
                div().debug_selector(move || format!("tx-pending-check-{tab_id}")).w(px(22.0)).flex_shrink_0().child(
                    Checkbox::new(("tx-pending-check", tab_id)).checked(checked).on_click(move |checked: &bool, _, cx| {
                        let checked = *checked;
                        let _ = view.update(cx, |this, cx| {
                            if checked {
                                this.deselected.remove(&tab_id);
                            } else {
                                this.deselected.insert(tab_id);
                            }
                            cx.notify();
                        });
                    }),
                ),
            )
            .child(fixed(150.0, row.connection.clone()))
            .child(fixed(110.0, row.database.clone()))
            .child(fixed(0.0, row.tab.clone()).w_auto().flex_1().min_w_0())
            .child(fixed(84.0, statements_label(row.statements)))
            .child(fixed(76.0, row.modifying.to_string()))
            .child(fixed(72.0, elapsed_label(row.open_for)))
            .child(
                div()
                    .w(px(150.0))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .gap(px(4.0))
                    .child(button("tx-pending-commit", "Commit").primary().on_click(move |_, _, cx| {
                        let _ = commit_root.update(cx, |root, cx| root.end_transactions_skipping_running(vec![tab_id], TxEnd::Commit, cx));
                    }))
                    .child(button("tx-pending-rollback", "Roll Back").ghost().on_click(move |_, _, cx| {
                        let _ = roll_back_root.update(cx, |root, cx| root.end_transactions_skipping_running(vec![tab_id], TxEnd::Rollback, cx));
                    })),
            )
    }
}
