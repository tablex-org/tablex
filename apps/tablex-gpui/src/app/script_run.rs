//! Running a script of several statements, and showing what it did.
//!
//! A script (the editor's text with more than one statement) is one run of its tab that sends
//! the statements one after the other on the connection's session, each as its own request, so
//! session state (`USE`, `SET`, a transaction) carries from one to the next. After each
//! statement the run records how it ended (`crate::script_run`) and sends the next, or stops:
//! on a failure when the setting says so, always on Cancel.
//!
//! The result area shows one tab per statement that returned a result set plus Statistics.
//! A page is kept as the Arrow batches the driver handed over: the driver's page is released
//! the moment the statement ends. Only the tab on screen has a grid (`TabRuntime::grid`); a
//! tab shown again builds its grid from the kept page. Paging or refreshing a result tab runs
//! its statement again, like a single result, and the new page replaces the kept one.

use super::*;
use crate::script_run::{
    one_line, Flow, PlannedStatement, ResultPage, ScriptRun, ScriptTab, ScriptTarget, ScriptView,
    StatementOutcome, StatementStatus,
};
use crate::grid::GridSource as _;
use gpui::{SharedString, Stateful};
use gpui_component::tab::{Tab, TabBar};
use gpui_component::table::{Column, DataTable, TableDelegate, TableState};

/// Longest statement text a Statistics row or a result tab's tooltip shows.
const STATEMENT_LINE: usize = 120;

impl AppRoot {
    // ---- starting a run -------------------------------------------------------------

    /// Runs the editor's text after its gates (read-only, production confirmation): one
    /// statement as it always ran; several as a script. Only plain SQL tabs split.
    pub(super) fn run_editor_sql(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
        let splits = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| RunKind::of_tab(tab) == RunKind::Sql && !tab.query_mode.is_mongo());
        let ranges = if splits {
            crate::sql_script::statement_ranges(&sql, &self.sql_lex(tab_id))
        } else {
            Vec::new()
        };
        if ranges.len() < 2 {
            // A new result of its own: it replaces the script's tabs when it lands.
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.editor_run_pending = true;
            }
            self.run_sql(tab_id, sql, None, cx);
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.editor_run_pending &= runtime.pending_sql.is_some();
            }
            return;
        }
        let statements = ranges
            .into_iter()
            .map(|range| PlannedStatement { sql: sql[range.clone()].to_owned(), range, confirmed: false })
            .collect();
        self.confirm_and_run_script(tab_id, statements, cx);
    }

    /// The driver asks before it runs a dangerous statement (`DROP`, `DELETE` without `WHERE`,
    /// ...). A script is asked about once, listing them; the answer covers exactly the
    /// statements listed, so a statement the app did not flag but the driver does still fails.
    fn confirm_and_run_script(
        &mut self,
        tab_id: u64,
        mut statements: Vec<PlannedStatement>,
        cx: &mut Context<Self>,
    ) {
        let connection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
            .unwrap_or_default();
        // "Confirm dangerous statements" off: the driver's refusal is answered with the
        // confirmation the user pre-approved in Settings.
        if !self.effective_for(&connection).confirm_dangerous {
            statements.iter_mut().for_each(|statement| statement.confirmed = true);
            self.run_planned_script(tab_id, statements, cx);
            return;
        }
        let lex = self.sql_lex(tab_id);
        let dangers: Vec<(usize, &'static str)> = statements
            .iter()
            .enumerate()
            .filter_map(|(index, statement)| {
                engine_driver_kit::sql::classify(&statement.sql, &lex)
                    .danger
                    .map(|danger| (index, danger.reason))
            })
            .collect();
        if dangers.is_empty() {
            self.run_planned_script(tab_id, statements, cx);
            return;
        }
        let Some(window) = self.driver_runtime.window else {
            let text = statements.iter().map(|statement| statement.sql.as_str()).collect::<Vec<_>>().join(";\n");
            self.refuse_statement(tab_id, &text, "Could not ask for confirmation: no window to ask in".into(), cx);
            return;
        };
        let root = cx.entity().downgrade();
        // The window cannot be updated while this entity is: open the dialog once the current
        // update has finished.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| {
                    this.open_script_confirmation(tab_id, statements, dangers, window, cx);
                });
            });
        });
    }

    fn open_script_confirmation(
        &mut self,
        tab_id: u64,
        statements: Vec<PlannedStatement>,
        dangers: Vec<(usize, &'static str)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const LISTED: usize = 6;
        let root = cx.entity().downgrade();
        let total = statements.len();
        let lines: Vec<String> = dangers
            .iter()
            .take(LISTED)
            .map(|(index, reason)| {
                format!("{}. {} \u{2014} {reason}", index + 1, one_line(&statements[*index].sql, 70))
            })
            .collect();
        let more = dangers.len().saturating_sub(LISTED);
        let flagged: Vec<usize> = dangers.iter().map(|(index, _)| *index).collect();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let mut statements = statements.clone();
            for index in &flagged {
                statements[*index].confirmed = true;
            }
            dialog
                .width(px(560.0))
                .title("Run this script?")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().debug_selector(|| "script-confirm-summary".into()).child(format!(
                            "The script has {total} statements; {} of them need your confirmation:",
                            flagged.len()
                        )))
                        .child(
                            div()
                                .debug_selector(|| "script-confirm-list".into())
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .children(lines.iter().cloned().map(|line| div().truncate().child(line)))
                                .when(more > 0, |list| list.child(format!("and {more} more"))),
                        ),
                )
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Run anyway"))
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div().debug_selector(|| "script-confirm-cancel".into()).child(
                                gpui_component::dialog::DialogClose::new().trigger(|button| button.label("Cancel")),
                            ),
                        )
                        .child(
                            div().debug_selector(|| "script-confirm-run".into()).child(
                                gpui_component::dialog::DialogAction::new()
                                    .child(Button::new("script-confirm-run").label("Run anyway").danger()),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let statements = statements.clone();
                    let _ = root.update(cx, |this, cx| this.run_planned_script(tab_id, statements, cx));
                    true
                })
        });
        cx.notify();
    }

    fn run_planned_script(&mut self, tab_id: u64, statements: Vec<PlannedStatement>, cx: &mut Context<Self>) {
        let Some(first) = statements.first().map(|statement| statement.sql.clone()) else {
            return;
        };
        self.start_run(tab_id, first, None, None, Some(statements), cx);
    }

    /// The run `run` of `request.tab_id` is a script: its statements, and where they go.
    pub(super) fn begin_script(&mut self, run: crate::query::QueryRunId, request: &SqlRun, statements: Vec<PlannedStatement>) {
        let on_error = self.settings.editor.on_script_error;
        let max_results = self.settings.editor.max_result_tabs.max(1) as usize;
        let target = ScriptTarget {
            connection: request.connection.clone(),
            database: request.database.clone(),
            collection: request.collection.clone(),
            limit: request.limit,
            read_only: request.read_only,
        };
        if let Some(runtime) = self.workspace.runtime_mut(request.tab_id) {
            runtime.script_run = Some(ScriptRun::new(run, target, statements, on_error, max_results));
        }
    }

    /// `run` is a script's run: its statement is about to be sent.
    pub(super) fn script_statement_sent(&mut self, tab_id: u64, run: crate::query::QueryRunId) {
        if let Some(script) = self.script_in_flight(tab_id, run) {
            script.begin_statement();
        }
    }

    fn script_in_flight(&mut self, tab_id: u64, run: crate::query::QueryRunId) -> Option<&mut ScriptRun> {
        self.workspace
            .runtime_mut(tab_id)?
            .script_run
            .as_mut()
            .filter(|script| script.run == run)
    }

    pub(super) fn is_script_run(&self, tab_id: u64, run: crate::query::QueryRunId) -> bool {
        self.workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.script_run.as_ref())
            .is_some_and(|script| script.run == run)
    }

    // ---- statement by statement -------------------------------------------------------

    /// A script statement came back from the driver.
    pub(super) fn finish_script_statement(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        result: Result<
            (
                engine_driver_api::QueryPage,
                engine_driver_host::ImportedArrowBatch,
                DriverPageGuard,
            ),
            DriverQueryError,
        >,
        cx: &mut Context<Self>,
    ) {
        let tab_id = request.tab_id;
        self.finish_query_timing(tab_id, run);
        self.finish_query_task(tab_id, run);
        let outcome = match result {
            Ok((page, imported, guard)) => {
                let outcome = if page.columns.is_empty() {
                    StatementOutcome::Command { affected: page.total_count.filter(|_| page.total_is_exact) }
                } else {
                    StatementOutcome::Rows {
                        source: ArrowGridSource::from_imported_page(imported)
                            .with_driver_types(page.columns.iter().map(|column| column.data_type.clone()))
                            .with_binary_display(crate::grid_arrow::BinaryDisplay::Hex),
                        page: ResultPage {
                            page: 0,
                            total: page.total_count,
                            total_is_exact: page.total_is_exact,
                            truncated: page.flags.truncated,
                        },
                    }
                };
                // The rows are ours now: the driver's page goes back.
                drop(guard);
                self.record_query_history(&request, page.row_count as i64, cx);
                outcome
            }
            Err(error) => {
                let message = crate::sessions::redact_secrets(&error.status_text());
                let cancelled = error.is_cancelled();
                if !cancelled {
                    self.report_query_failure(tab_id, &error.to_string());
                    self.note_driver_connection_lost(tab_id, &error);
                }
                // Nothing after a lost session can run.
                let fatal = error.is_connection_lost()
                    || matches!(
                        error,
                        DriverQueryError::NotConnected
                            | DriverQueryError::ConnectionMissing
                            | DriverQueryError::DriverNotLoaded
                            | DriverQueryError::ConnectionIdMissing
                            | DriverQueryError::SessionPoisoned
                            | DriverQueryError::Unsupported(_)
                    );
                StatementOutcome::Failed { message, cancelled, fatal }
            }
        };
        self.advance_script(tab_id, run, outcome, cx);
    }

    /// Records how the current statement ended, then sends the next or lands the run.
    fn advance_script(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        outcome: StatementOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(flow) = self.script_in_flight(tab_id, run).map(|script| script.record(outcome)) else {
            return;
        };
        match flow {
            Flow::Next => self.send_next_script_statement(tab_id, run, cx),
            Flow::Done => {
                let view = self
                    .workspace
                    .runtime_mut(tab_id)
                    .and_then(|runtime| runtime.script_run.take())
                    .map(ScriptRun::finish);
                if let Some(view) = view {
                    self.land_script(tab_id, Some(run), view, cx);
                }
            }
        }
    }

    fn send_next_script_statement(&mut self, tab_id: u64, run: crate::query::QueryRunId, cx: &mut Context<Self>) {
        let schema = self.workspace.tab(tab_id).and_then(|tab| tab.schema.clone());
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        let Some(script) = runtime.script_run.as_ref().filter(|script| script.run == run) else {
            return;
        };
        let Some((index, statement)) = script.current() else {
            return;
        };
        let target = &script.target;
        let request = SqlRun {
            tab_id,
            kind: RunKind::Sql,
            connection: target.connection.clone(),
            sql: statement.sql.clone(),
            database: target.database.clone(),
            collection: target.collection.clone(),
            limit: target.limit,
            offset: 0,
            confirmed: statement.confirmed,
            read_only: target.read_only,
        };
        let _ = index;
        let context = crate::query::RunContext {
            sql: request.sql.clone(),
            connection: request.connection.clone(),
            database: request.database.clone(),
            schema,
        };
        runtime.query.describe(run, context);
        self.start_driver_sql_run(run, request, cx);
    }

    /// The global query timeout ended the statement in flight (the caller has already stopped
    /// it): it fails, and the script goes on or stops by its setting.
    pub(super) fn script_statement_timed_out(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_script_run(tab_id, run) {
            return false;
        }
        self.finish_query_timing(tab_id, run);
        self.report_query_failure(tab_id, message);
        let outcome = StatementOutcome::Failed { message: message.to_owned(), cancelled: false, fatal: false };
        self.advance_script(tab_id, run, outcome, cx);
        true
    }

    /// Cancel: the run in flight ends where it is. The statement that was running is cancelled,
    /// the rest skipped, and what the script produced so far is shown.
    pub(super) fn cancel_script(&mut self, tab_id: u64) -> Option<ScriptView> {
        self.workspace
            .runtime_mut(tab_id)?
            .script_run
            .take()
            .map(ScriptRun::cancel)
    }

    /// A script ended (`run` completes it) or was cancelled (`run` is `None`: the tab's status is
    /// already Cancelled): its tabs replace what the tab showed.
    pub(super) fn land_script(
        &mut self,
        tab_id: u64,
        run: Option<crate::query::QueryRunId>,
        view: ScriptView,
        cx: &mut Context<Self>,
    ) {
        let shown = view.shown;
        let rows = view.results.iter().map(|result| result.source.row_count()).sum();
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            if let Some(run) = run {
                runtime.query.complete(run, rows);
            }
            runtime.pending_sql = None;
            runtime.editor_run_pending = false;
            runtime.script = Some(view);
        }
        if let Some(run) = run {
            self.finish_query_task(tab_id, run);
        }
        self.view_mut(tab_id).script_stats = None;
        self.auto_run_ended(tab_id, cx);
        self.present_script_tab(tab_id, shown);
        self.notify_tab(tab_id, cx);
        cx.notify();
    }

    // ---- the tabs ---------------------------------------------------------------------

    /// What a click on a result tab or Statistics does: shows it, unless staged changes belong to
    /// the rows on screen.
    pub(super) fn show_script_tab(&mut self, tab_id: u64, tab: ScriptTab, cx: &mut Context<Self>) {
        if self.refuse_run_over_staged_changes(tab_id, cx) {
            return;
        }
        self.present_script_tab(tab_id, tab);
        self.notify_tab(tab_id, cx);
    }

    /// Puts `tab` on screen: a result tab gets a grid built from its kept page (and becomes the
    /// statement on screen, so paging, Refresh, sort and Export work on it as on a single
    /// result); Statistics drops the grid.
    fn present_script_tab(&mut self, tab_id: u64, tab: ScriptTab) {
        let binary = self.binary_display(tab_id);
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        let Some(script) = runtime.script.as_mut() else {
            return;
        };
        let shown = match tab {
            ScriptTab::Result(index) if index < script.results.len() => Some(index),
            ScriptTab::Result(_) => return,
            ScriptTab::Statistics => None,
        };
        script.shown = tab;
        let (grid, sql, page) = match shown.and_then(|index| script.results.get(index)) {
            Some(result) => (
                GridState::new(GridContentSource::Arrow(result.source.clone().with_binary_display(binary))),
                Some(script.reports[result.statement].sql.clone()),
                result.page,
            ),
            None => (
                GridState::new(GridContentSource::Arrow(ArrowGridSource::new(Vec::new()))),
                None,
                ResultPage::default(),
            ),
        };
        runtime.grid = grid;
        runtime.last_executed_query = sql;
        runtime.context_sql_sort = None;
        runtime.pagination.reset();
        runtime.pagination.begin_page(page.page);
        runtime.pagination.set_driver_page(page.total, page.total_is_exact, page.truncated);
        if runtime.columns_panel.presentation == crate::columns_panel::Presentation::Plan {
            runtime.columns_panel.presentation = crate::columns_panel::Presentation::Grid;
        }
        let view = self.view_mut(tab_id);
        view.grid_scroll.scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
        self.reset_tab_view(tab_id);
    }

    /// A single statement's page landed on a tab that shows a script: when the statement is the
    /// one behind the result tab on screen (paging, Refresh), the page replaces the kept one; a
    /// run typed in the editor replaces the script itself.
    pub(super) fn single_page_landed(
        &mut self,
        tab_id: u64,
        sql: Option<&str>,
        source: &ArrowGridSource,
        page: ResultPage,
    ) {
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        if std::mem::take(&mut runtime.editor_run_pending) {
            runtime.script = None;
        } else if let Some(script) = runtime.script.as_mut() {
            if let ScriptTab::Result(index) = script.shown {
                if script.statement_of(index).map(|report| report.sql.as_str()) == sql {
                    script.replace_page(index, source.clone(), page);
                }
            }
        } else {
            return;
        }
        self.view_mut(tab_id).script_stats = None;
    }

    /// Click on a Statistics row: the statement in the editor, and its result tab if it has one.
    pub(super) fn focus_script_statement(
        &mut self,
        tab_id: u64,
        statement: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((range, sql, result)) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.script.as_ref())
            .and_then(|script| script.reports.get(statement))
            .map(|report| (report.range.clone(), report.sql.clone(), report.result))
        else {
            return;
        };
        if let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() {
            let text = editor.read(cx).value().to_string();
            // The text may have been edited since the run: the statement is where it was, else
            // wherever the same text is now, else nowhere.
            let found = (text.get(range.clone()) == Some(sql.as_str()))
                .then_some(range)
                .or_else(|| text.find(&sql).map(|start| start..start + sql.len()));
            editor.update(cx, |state, cx| {
                if let Some(found) = found {
                    state.set_selected_range(found, cx);
                }
                state.focus(window, cx);
            });
        }
        if let Some(index) = result {
            self.show_script_tab(tab_id, ScriptTab::Result(index), cx);
        }
        cx.notify();
    }

    /// Whether Statistics is on screen (the grid and footer give way to it).
    pub(super) fn script_stats_active(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.script.as_ref())
            .is_some_and(|script| script.shown == ScriptTab::Statistics)
    }

    // ---- drawing ----------------------------------------------------------------------

    /// The Statistics table of the tab's script, built when the script lands and whenever its
    /// rows change. Rows are formatted once here, never while drawing.
    pub(super) fn sync_script_stats(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(script) = self.workspace.runtime(tab_id).and_then(|runtime| runtime.script.as_ref()) else {
            self.view_mut(tab_id).script_stats = None;
            return;
        };
        if self.view(tab_id).script_stats.is_some() || script.shown != ScriptTab::Statistics {
            return;
        }
        let rows = script
            .reports
            .iter()
            .enumerate()
            .map(|(index, report)| StatRow {
                statement: index,
                number: (index + 1).to_string().into(),
                sql: report.one_line(STATEMENT_LINE).into(),
                status: report.status.label().into(),
                tone: match report.status {
                    StatementStatus::Ok => Tone::Normal,
                    StatementStatus::Error(_) | StatementStatus::Cancelled => Tone::Bad,
                    StatementStatus::Skipped => Tone::Faint,
                },
                rows: report.rows_text().into(),
                time: report.duration.map_or_else(String::new, crate::script_run::format_duration).into(),
            })
            .collect();
        let delegate = StatsDelegate { root: cx.entity().downgrade(), tab_id, rows };
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .col_resizable(true)
                .col_movable(false)
                .sortable(false)
                .row_header(false)
        });
        self.view_mut(tab_id).script_stats = Some(table);
    }

    /// The Statistics table (`None` while the tab shows something else).
    pub(super) fn render_script_stats(&mut self, tab_id: u64) -> Option<AnyElement> {
        let table = self.view(tab_id).script_stats.clone()?;
        Some(
            div()
                .debug_selector(|| "script-stats".into())
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(DataTable::new(&table).stripe(true))
                .into_any_element(),
        )
    }

    /// The strip above the results: the result tabs, Statistics and a one-line summary — or the
    /// progress of the script in flight.
    pub(super) fn render_script_strip(&mut self, tab_id: u64, cx: &mut Context<Self>) -> Option<AnyElement> {
        let runtime = self.workspace.runtime(tab_id)?;
        if let Some(script) = &runtime.script_run {
            return Some(
                div()
                    .debug_selector(|| "script-progress".into())
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(format!("Running statement {} of {}\u{2026}", script.done() + 1, script.len()))
                    .into_any_element(),
            );
        }
        let script = runtime.script.as_ref()?;
        let shown = script.shown;
        let tabs = script.tabs.iter().enumerate().map(|(index, label)| {
            let tooltip = label.tooltip.clone();
            Tab::new()
                .label(label.title.clone())
                .debug_selector(move || format!("script-tab-{index}"))
                .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx))
        });
        let has_results = !script.tabs.is_empty();
        let summary = script.summary.clone();
        let root = cx.entity().downgrade();
        // Statistics is pinned beside the summary, outside the scrolling result tabs, so it stays in
        // reach however many results the run has.
        let statistics = Button::new(("script-statistics", tab_id))
            .label("Statistics")
            .small()
            .ghost()
            .selected(shown == ScriptTab::Statistics)
            .debug_selector(|| "script-tab-stats".into())
            .on_click(cx.listener(move |this, _, _, cx| this.show_script_tab(tab_id, ScriptTab::Statistics, cx)));
        Some(
            div()
                .debug_selector(|| "script-strip".into())
                .flex_shrink_0()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(theme::border())
                .child(div().flex_1().min_w_0().when(has_results, |this| {
                    this.child(
                        TabBar::new(("script-tabs", tab_id))
                            .underline()
                            .menu(true)
                            .when_some(
                                match shown {
                                    ScriptTab::Result(index) => Some(index),
                                    ScriptTab::Statistics => None,
                                },
                                |bar, index| bar.selected_index(index),
                            )
                            .on_click(move |index, _, cx| {
                                let _ = root.update(cx, |this, cx| this.show_script_tab(tab_id, ScriptTab::Result(*index), cx));
                            })
                            .children(tabs),
                    )
                }))
                .child(
                    div()
                        .debug_selector(|| "script-summary".into())
                        .flex_shrink_0()
                        .px(px(12.0))
                        .text_size(px(11.0))
                        .text_color(theme::text_muted())
                        .child(summary),
                )
                .child(div().flex_shrink_0().px(px(6.0)).child(statistics))
                .into_any_element(),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Normal,
    Bad,
    Faint,
}

/// One Statistics row, formatted once.
struct StatRow {
    statement: usize,
    number: SharedString,
    sql: SharedString,
    status: SharedString,
    tone: Tone,
    rows: SharedString,
    time: SharedString,
}

/// The Statistics table: a virtualized `gpui-component` table over rows built once per script.
pub(super) struct StatsDelegate {
    root: gpui::WeakEntity<AppRoot>,
    tab_id: u64,
    rows: Vec<StatRow>,
}

impl TableDelegate for StatsDelegate {
    fn columns_count(&self, _: &gpui::App) -> usize {
        5
    }

    fn rows_count(&self, _: &gpui::App) -> usize {
        self.rows.len()
    }

    fn column(&self, ix: usize, _: &gpui::App) -> Column {
        let (key, name, width) = [
            ("number", "#", 44.0),
            ("statement", "Statement", 520.0),
            ("status", "Status", 90.0),
            ("rows", "Rows", 200.0),
            ("time", "Time", 90.0),
        ][ix];
        let column = Column::new(key, name).width(px(width)).movable(false);
        if ix == 0 || ix == 4 {
            column.text_right()
        } else {
            column
        }
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let (root, tab_id) = (self.root.clone(), self.tab_id);
        // The table also draws filler rows past the last statement: they go nowhere.
        let statement = self.rows.get(row_ix).map(|row| row.statement);
        div()
            .id(("script-stat-row", row_ix))
            .debug_selector(move || format!("script-stat-row-{row_ix}"))
            .on_click(move |_, window, cx| {
                if let Some(statement) = statement {
                    let _ = root.update(cx, |this, cx| this.focus_script_statement(tab_id, statement, window, cx));
                }
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        let (text, colour) = match col_ix {
            0 => (row.number.clone(), theme::text_faint()),
            1 => (row.sql.clone(), if row.tone == Tone::Faint { theme::text_faint() } else { theme::text() }),
            2 => (
                row.status.clone(),
                match row.tone {
                    Tone::Normal => theme::success(),
                    Tone::Bad => theme::danger(),
                    Tone::Faint => theme::text_faint(),
                },
            ),
            3 => (row.rows.clone(), if row.tone == Tone::Bad { theme::danger() } else { theme::text_muted() }),
            _ => (row.time.clone(), theme::text_muted()),
        };
        div()
            .size_full()
            .truncate()
            .text_size(px(12.0))
            .text_color(colour)
            .debug_selector(move || format!("script-stat-{row_ix}-{col_ix}"))
            .child(text)
            .into_any_element()
    }
}
