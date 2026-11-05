//! The model of a SQL script run: the statements in order, what each one did, and the
//! result pages the run keeps.
//!
//! A run is a [`ScriptRun`] while it is going; it ends as a [`ScriptView`], which the result
//! area shows as one tab per statement that returned a result set ("Result 1" ...) and a
//! Statistics tab. Only [`ScriptView::results`] hold row data, as the Arrow batches of a page
//! the driver has already handed over (the driver's page is released as each statement ends);
//! a grid is built for the one result on screen, from its page, by the app.

use crate::grid_arrow::ArrowGridSource;
use crate::query::QueryRunId;
use crate::settings::OnScriptError;
use gpui::SharedString;
use std::ops::Range;
use std::time::{Duration, Instant};

/// How one statement ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementStatus {
    Ok,
    Error(String),
    /// Not run: an earlier statement failed or the run was cancelled.
    Skipped,
    /// Stopped by Cancel while it ran.
    Cancelled,
}

impl StatementStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error(_) => "error",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One line of the Statistics table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementReport {
    /// The statement's place in the script text at run time.
    pub range: Range<usize>,
    pub sql: String,
    pub status: StatementStatus,
    /// Rows a result set returned.
    pub rows: Option<u64>,
    /// Rows a statement without a result set changed, when the driver says.
    pub affected: Option<u64>,
    pub duration: Option<Duration>,
    /// The result tab holding its rows. `None` for a statement without a result set, and for
    /// one past the result-tab cap ([`Self::rows_not_kept`]).
    pub result: Option<usize>,
    /// A result set the run did not keep: it ran, but the cap on result tabs was reached.
    pub rows_not_kept: bool,
}

impl StatementReport {
    /// The statement as one line of at most `limit` characters, for a table cell.
    pub fn one_line(&self, limit: usize) -> String {
        one_line(&self.sql, limit)
    }

    /// The "rows" cell: rows returned, rows affected, the reason it has none, or nothing.
    pub fn rows_text(&self) -> String {
        match (&self.status, self.rows, self.affected) {
            (StatementStatus::Error(message), ..) => one_line(message, 80),
            (_, Some(rows), _) if self.rows_not_kept => format!("{rows} (rows not kept)"),
            (_, Some(rows), _) => rows.to_string(),
            (_, None, Some(affected)) => format!("{affected} affected"),
            _ => String::new(),
        }
    }
}

/// Longest statement text a result tab's tooltip shows.
const TOOLTIP_LINE: usize = 120;

fn summarize(reports: &[StatementReport]) -> String {
    let failed = reports
        .iter()
        .filter(|report| matches!(report.status, StatementStatus::Error(_) | StatementStatus::Cancelled))
        .count();
    let skipped = reports.iter().filter(|report| report.status == StatementStatus::Skipped).count();
    let total: Duration = reports.iter().filter_map(|report| report.duration).sum();
    let mut summary = format!("{} statements", reports.len());
    if failed > 0 {
        summary.push_str(&format!(" \u{00B7} {failed} failed"));
    }
    if skipped > 0 {
        summary.push_str(&format!(" \u{00B7} {skipped} skipped"));
    }
    summary.push_str(&format!(" \u{00B7} {}", format_duration(total)));
    summary
}

/// `850 ms`, `1.25 s`, `2 min 5 s`.
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    match millis {
        0..=999 => format!("{millis} ms"),
        1000..=59_999 => format!("{:.2} s", duration.as_secs_f64()),
        _ => format!("{} min {} s", millis / 60_000, (millis % 60_000) / 1000),
    }
}

/// `text` on one line: whitespace runs become one space, cut with an ellipsis past `limit`
/// characters.
pub fn one_line(text: &str, limit: usize) -> String {
    let mut line = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if count > 0 {
            line.push(' ');
            count += 1;
        }
        for c in word.chars() {
            if count >= limit {
                line.push('\u{2026}');
                return line;
            }
            line.push(c);
            count += 1;
        }
    }
    line
}

/// Where the next page of a result starts, and whether there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResultPage {
    pub page: u64,
    pub total: Option<u64>,
    pub total_is_exact: bool,
    pub truncated: bool,
}

/// The rows of one statement, kept so its tab can be shown again.
#[derive(Debug, Clone)]
pub struct ScriptResult {
    /// The statement (index into [`ScriptView::reports`]) that produced it.
    pub statement: usize,
    pub source: ArrowGridSource,
    pub page: ResultPage,
    /// The columns pinned to the left of this result tab's grid.
    pub pinned: Vec<crate::pinned_column::PinnedColumn>,
}

/// What is shown in the result area of a script run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptTab {
    Result(usize),
    Statistics,
}

/// What a result tab says: "Result 2", and the start of its statement for the tooltip.
#[derive(Debug, Clone)]
pub struct TabLabel {
    pub title: SharedString,
    pub tooltip: SharedString,
}

/// A finished script run.
#[derive(Debug)]
pub struct ScriptView {
    pub reports: Vec<StatementReport>,
    pub results: Vec<ScriptResult>,
    pub shown: ScriptTab,
    /// One per result tab, formatted once when the run ends (drawing only clones them).
    pub tabs: Vec<TabLabel>,
    /// "12 statements · 1 failed · 340 ms", formatted once.
    pub summary: SharedString,
}

impl ScriptView {
    /// Bytes of row data the run keeps.
    #[cfg(test)]
    pub fn retained_bytes(&self) -> usize {
        self.results.iter().map(|result| result.source.memory_size()).sum()
    }

    /// Statements that did not succeed (errors and cancellations).
    #[cfg(test)]
    pub fn failures(&self) -> usize {
        self.reports
            .iter()
            .filter(|report| matches!(report.status, StatementStatus::Error(_) | StatementStatus::Cancelled))
            .count()
    }

    /// The statement behind the result tab `result`.
    pub fn statement_of(&self, result: usize) -> Option<&StatementReport> {
        self.reports.get(self.results.get(result)?.statement)
    }

    /// Where the statement `statement` shows: its result tab, else Statistics.
    #[cfg(test)]
    pub fn tab_of(&self, statement: usize) -> ScriptTab {
        self.reports
            .get(statement)
            .and_then(|report| report.result)
            .map_or(ScriptTab::Statistics, ScriptTab::Result)
    }

    /// The result tab a page lands in when its statement is paged or refreshed.
    pub fn replace_page(&mut self, result: usize, source: ArrowGridSource, page: ResultPage) {
        let Some(kept) = self.results.get_mut(result) else {
            return;
        };
        if let Some(report) = self.reports.get_mut(kept.statement) {
            report.rows = Some(source_rows(&source));
        }
        kept.source = source;
        kept.page = page;
    }
}

fn source_rows(source: &ArrowGridSource) -> u64 {
    use crate::grid::GridSource as _;
    source.row_count() as u64
}

/// One statement of a script about to run.
#[derive(Debug, Clone)]
pub struct PlannedStatement {
    pub range: Range<usize>,
    pub sql: String,
    /// The user (or the connection's settings) allowed it to run although the driver flags it as
    /// dangerous. A statement without it that the driver flags anyway fails; it is never
    /// confirmed on the user's behalf.
    pub confirmed: bool,
}

/// What a finished statement produced.
pub enum StatementOutcome {
    /// A result set.
    Rows { source: ArrowGridSource, page: ResultPage },
    /// A statement without one; `affected` is what the driver reports.
    Command { affected: Option<u64> },
    Failed {
        message: String,
        /// The user stopped it.
        cancelled: bool,
        /// Nothing after it can run (the session is gone).
        fatal: bool,
    },
}

/// What the run does after a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Next,
    Done,
}

/// Where and how every statement of a run is sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptTarget {
    pub connection: String,
    pub database: String,
    /// Query-history context (the tab's collection key).
    pub collection: String,
    pub limit: i64,
    /// The connection is read-only: every statement also runs read-only in the driver.
    pub read_only: bool,
}

/// A script run in progress.
#[derive(Debug)]
pub struct ScriptRun {
    pub run: QueryRunId,
    pub target: ScriptTarget,
    statements: Vec<PlannedStatement>,
    next: usize,
    on_error: OnScriptError,
    max_results: usize,
    reports: Vec<StatementReport>,
    results: Vec<ScriptResult>,
    started: Option<Instant>,
}

impl ScriptRun {
    pub fn new(
        run: QueryRunId,
        target: ScriptTarget,
        statements: Vec<PlannedStatement>,
        on_error: OnScriptError,
        max_results: usize,
    ) -> Self {
        Self {
            run,
            target,
            reports: Vec::with_capacity(statements.len()),
            statements,
            next: 0,
            on_error,
            max_results,
            results: Vec::new(),
            started: None,
        }
    }

    /// The statement to send now (the one after the last recorded), and its index.
    pub fn current(&self) -> Option<(usize, &PlannedStatement)> {
        self.statements.get(self.next).map(|statement| (self.next, statement))
    }

    pub fn len(&self) -> usize {
        self.statements.len()
    }

    /// Statements that have ended.
    pub fn done(&self) -> usize {
        self.reports.len()
    }

    /// The current statement is being sent: its duration counts from here.
    pub fn begin_statement(&mut self) {
        self.started = Some(Instant::now());
    }

    /// Records how the current statement ended; says whether the next one runs.
    pub fn record(&mut self, outcome: StatementOutcome) -> Flow {
        let Some(statement) = self.statements.get(self.next) else {
            return Flow::Done;
        };
        let index = self.next;
        let mut report = StatementReport {
            range: statement.range.clone(),
            sql: statement.sql.clone(),
            status: StatementStatus::Ok,
            rows: None,
            affected: None,
            duration: self.started.take().map(|started| started.elapsed()),
            result: None,
            rows_not_kept: false,
        };
        let mut stop = false;
        match outcome {
            StatementOutcome::Rows { source, page } => {
                report.rows = Some(source_rows(&source));
                if self.results.len() < self.max_results {
                    report.result = Some(self.results.len());
                    self.results.push(ScriptResult { statement: index, source, page, pinned: Vec::new() });
                } else {
                    report.rows_not_kept = true;
                }
            }
            StatementOutcome::Command { affected } => report.affected = affected,
            StatementOutcome::Failed { message, cancelled, fatal } => {
                report.status =
                    if cancelled { StatementStatus::Cancelled } else { StatementStatus::Error(message) };
                stop = cancelled || fatal || self.on_error == OnScriptError::Stop;
            }
        }
        self.reports.push(report);
        self.next += 1;
        if stop || self.next >= self.statements.len() {
            Flow::Done
        } else {
            Flow::Next
        }
    }

    /// The run is over (it ran out of statements, stopped, or was cancelled): statements that
    /// did not run are skipped, and the first failure, else the first result, is shown.
    pub fn finish(mut self) -> ScriptView {
        for statement in self.statements.drain(self.next..) {
            self.reports.push(StatementReport {
                range: statement.range,
                sql: statement.sql,
                status: StatementStatus::Skipped,
                rows: None,
                affected: None,
                duration: None,
                result: None,
                rows_not_kept: false,
            });
        }
        let shown = if self
            .reports
            .iter()
            .any(|report| matches!(report.status, StatementStatus::Error(_) | StatementStatus::Cancelled))
            || self.results.is_empty()
        {
            ScriptTab::Statistics
        } else {
            ScriptTab::Result(0)
        };
        let tabs = self
            .results
            .iter()
            .enumerate()
            .map(|(index, result)| TabLabel {
                title: format!("Result {}", index + 1).into(),
                tooltip: self.reports[result.statement].one_line(TOOLTIP_LINE).into(),
            })
            .collect();
        let summary = summarize(&self.reports);
        ScriptView { reports: self.reports, results: self.results, shown, tabs, summary: summary.into() }
    }

    /// Cancel: the statement that was running ends as cancelled, the rest are skipped.
    pub fn cancel(mut self) -> ScriptView {
        if self.next < self.statements.len() {
            self.record(StatementOutcome::Failed { message: String::new(), cancelled: true, fatal: true });
        }
        self.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Int64Array, RecordBatch};
    use std::sync::Arc;

    fn planned(sql: &str) -> PlannedStatement {
        PlannedStatement { range: 0..sql.len(), sql: sql.into(), confirmed: false }
    }

    fn rows(count: i64) -> StatementOutcome {
        let batch = RecordBatch::try_from_iter([("n", Arc::new(Int64Array::from_iter_values(0..count)) as _)]).unwrap();
        StatementOutcome::Rows {
            source: ArrowGridSource::new(vec![Arc::new(batch)]),
            page: ResultPage::default(),
        }
    }

    fn failed(message: &str) -> StatementOutcome {
        StatementOutcome::Failed { message: message.into(), cancelled: false, fatal: false }
    }

    fn run(statements: &[&str], on_error: OnScriptError, cap: usize) -> ScriptRun {
        ScriptRun::new(QueryRunId(1), ScriptTarget::default(), statements.iter().map(|sql| planned(sql)).collect(), on_error, cap)
    }

    #[test]
    fn results_are_numbered_for_statements_with_rows_only() {
        let mut script = run(&["select 1", "update t", "create t", "select 2"], OnScriptError::Stop, 20);
        assert_eq!(script.record(rows(3)), Flow::Next);
        assert_eq!(script.record(StatementOutcome::Command { affected: Some(2) }), Flow::Next);
        assert_eq!(script.record(StatementOutcome::Command { affected: Some(0) }), Flow::Next);
        assert_eq!(script.record(rows(1)), Flow::Done);
        let view = script.finish();
        assert_eq!(view.results.len(), 2);
        assert_eq!(view.reports.iter().map(|report| report.result).collect::<Vec<_>>(), [Some(0), None, None, Some(1)]);
        assert_eq!(view.reports[0].rows, Some(3));
        assert_eq!(view.reports[1].rows_text(), "2 affected");
        assert_eq!(view.reports[3].rows_text(), "1");
        assert_eq!(view.shown, ScriptTab::Result(0));
        assert_eq!(view.tab_of(3), ScriptTab::Result(1));
        assert_eq!(view.tab_of(1), ScriptTab::Statistics);
        assert_eq!(view.statement_of(1).unwrap().sql, "select 2");
    }

    #[test]
    fn an_error_stops_the_script_or_not_by_setting() {
        let mut stop = run(&["a", "b", "c"], OnScriptError::Stop, 20);
        assert_eq!(stop.record(rows(1)), Flow::Next);
        assert_eq!(stop.record(failed("boom")), Flow::Done);
        let view = stop.finish();
        let statuses: Vec<_> = view.reports.iter().map(|report| report.status.label()).collect();
        assert_eq!(statuses, ["ok", "error", "skipped"]);
        // The failure is what the user needs to see first.
        assert_eq!(view.shown, ScriptTab::Statistics);
        assert_eq!(view.reports[1].rows_text(), "boom");

        let mut carry_on = run(&["a", "b", "c"], OnScriptError::Continue, 20);
        assert_eq!(carry_on.record(failed("boom")), Flow::Next);
        assert_eq!(carry_on.record(rows(1)), Flow::Next);
        assert_eq!(carry_on.record(rows(1)), Flow::Done);
        let view = carry_on.finish();
        let statuses: Vec<_> = view.reports.iter().map(|report| report.status.label()).collect();
        assert_eq!(statuses, ["error", "ok", "ok"]);
        assert_eq!(view.results.len(), 2);
        assert_eq!(view.failures(), 1);
    }

    #[test]
    fn a_fatal_error_stops_even_when_the_script_carries_on() {
        let mut script = run(&["a", "b"], OnScriptError::Continue, 20);
        let fatal = StatementOutcome::Failed { message: "gone".into(), cancelled: false, fatal: true };
        assert_eq!(script.record(fatal), Flow::Done);
        assert_eq!(script.finish().reports[1].status, StatementStatus::Skipped);
    }

    #[test]
    fn cancel_marks_the_running_statement_and_skips_the_rest() {
        let mut script = run(&["a", "b", "c", "d"], OnScriptError::Continue, 20);
        script.record(rows(1));
        let view = script.cancel();
        let statuses: Vec<_> = view.reports.iter().map(|report| report.status.label()).collect();
        assert_eq!(statuses, ["ok", "cancelled", "skipped", "skipped"]);
        assert_eq!(view.results.len(), 1);
        assert_eq!(view.shown, ScriptTab::Statistics);
    }

    #[test]
    fn statements_past_the_cap_run_but_keep_no_rows() {
        let mut script = run(&["a", "b", "c"], OnScriptError::Stop, 2);
        script.record(rows(1));
        script.record(rows(2));
        script.record(rows(5));
        let view = script.finish();
        assert_eq!(view.results.len(), 2);
        assert_eq!(view.reports[2].status, StatementStatus::Ok);
        assert!(view.reports[2].rows_not_kept);
        assert_eq!(view.reports[2].result, None);
        assert_eq!(view.reports[2].rows_text(), "5 (rows not kept)");
        assert_eq!(view.tab_of(2), ScriptTab::Statistics);
    }

    #[test]
    fn a_script_without_a_result_set_shows_statistics() {
        let mut script = run(&["a"], OnScriptError::Stop, 20);
        script.record(StatementOutcome::Command { affected: None });
        let view = script.finish();
        assert_eq!(view.shown, ScriptTab::Statistics);
        assert_eq!(view.reports[0].rows_text(), "");
    }

    #[test]
    fn a_statement_is_cut_to_one_line() {
        assert_eq!(one_line("select  a,\n\tb\nfrom t", 100), "select a, b from t");
        assert_eq!(one_line("select abcdefghij", 10), "select abc\u{2026}");
        assert_eq!(one_line("   ", 10), "");
        assert_eq!(one_line("select 1", 8), "select 1");
    }

    #[test]
    fn a_replaced_page_updates_the_row_count() {
        let mut script = run(&["a"], OnScriptError::Stop, 20);
        script.record(rows(3));
        let mut view = script.finish();
        let StatementOutcome::Rows { source, page } = rows(7) else { unreachable!() };
        view.replace_page(0, source, ResultPage { page: 1, ..page });
        assert_eq!(view.reports[0].rows, Some(7));
        assert_eq!(view.results[0].page.page, 1);
        assert!(view.retained_bytes() > 0);
    }
}
