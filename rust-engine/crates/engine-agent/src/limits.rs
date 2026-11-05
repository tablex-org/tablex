//! Result and request limits, and the collector that applies them.

use crate::result::{QueryResult, ResultColumn, Truncation};
use std::ops::ControlFlow;
use std::time::Duration;

/// Limits applied to every tool call. The defaults are what the design asks
/// for; front ends may lower or raise them (never above the hard maximums the
/// tool schemas state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Rows returned when the request names no limit.
    pub default_rows: usize,
    /// Most rows one statement may return.
    pub max_rows: usize,
    /// Bytes of one cell; longer text is cut and marked. The cut happens after
    /// the driver rendered the cell (`render_cell_batch` has no per-cell cap), so
    /// a huge cell is held in memory once before it is shortened: bounded by the
    /// driver's page, not by this limit.
    pub max_cell_bytes: usize,
    /// Bytes of all cells of one result.
    pub max_result_bytes: usize,
    /// Statement timeout when the connection names none.
    pub statement_timeout: Duration,
    /// Longest statement text accepted.
    pub max_statement_bytes: usize,
    /// Items of one `list_*` page when the request names no limit.
    pub default_list_items: usize,
    /// Most items of one `list_*` page.
    pub max_list_items: usize,
    /// Tables `get_schema_context` describes at most.
    pub max_context_tables: usize,
    /// Bytes of DDL `describe_table` returns.
    pub max_ddl_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            default_rows: 200,
            max_rows: 1000,
            max_cell_bytes: 2 * 1024,
            max_result_bytes: 256 * 1024,
            statement_timeout: Duration::from_secs(30),
            max_statement_bytes: 64 * 1024,
            default_list_items: 100,
            max_list_items: 500,
            max_context_tables: 400,
            max_ddl_bytes: 8 * 1024,
        }
    }
}

impl Limits {
    /// The row limit for a request: its own value, clamped to `1..=max_rows`.
    pub fn rows_for(&self, requested: Option<usize>) -> usize {
        requested
            .filter(|rows| *rows > 0)
            .unwrap_or(self.default_rows)
            .min(self.max_rows)
    }

    pub fn list_items_for(&self, requested: Option<usize>) -> usize {
        requested
            .filter(|items| *items > 0)
            .unwrap_or(self.default_list_items)
            .min(self.max_list_items)
    }
}

/// Cuts `text` to at most `max` bytes on a character boundary and appends a
/// marker saying how long it was. Returns `None` when nothing was cut.
pub fn truncate_text(text: &str, max: usize) -> Option<String> {
    if text.len() <= max {
        return None;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!(
        "{}… [truncated, {} bytes in total]",
        &text[..end],
        text.len()
    ))
}

/// Builds a [`QueryResult`] row by row, enforcing the row, cell and total
/// byte limits. Session implementations feed it and stop when it says so, so
/// nothing past the limits is kept.
#[derive(Debug)]
pub struct RowCollector {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<String>>,
    row_limit: usize,
    max_cell_bytes: usize,
    max_result_bytes: usize,
    bytes: usize,
    cells_truncated: usize,
    stopped: Option<Truncation>,
}

impl RowCollector {
    pub fn new(columns: Vec<ResultColumn>, row_limit: usize, limits: &Limits) -> Self {
        let bytes = columns.iter().map(|column| column.name.len()).sum();
        Self {
            columns,
            rows: Vec::with_capacity(row_limit.min(256)),
            row_limit,
            max_cell_bytes: limits.max_cell_bytes,
            max_result_bytes: limits.max_result_bytes,
            bytes,
            cells_truncated: 0,
            stopped: None,
        }
    }

    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Adds one row. `Break` means the row (and every later one) was not
    /// kept: stop reading. The first row is always kept, so a result is never
    /// empty only because its cells are wide.
    pub fn push_row<S: AsRef<str>>(
        &mut self,
        cells: impl IntoIterator<Item = S>,
    ) -> ControlFlow<()> {
        if self.stopped.is_some() {
            return ControlFlow::Break(());
        }
        if self.rows.len() >= self.row_limit {
            self.stopped = Some(Truncation::RowLimit);
            return ControlFlow::Break(());
        }
        let mut row = Vec::with_capacity(self.columns.len());
        let mut row_bytes = 0;
        let mut cut = 0;
        for cell in cells {
            let cell = cell.as_ref();
            let text = match truncate_text(cell, self.max_cell_bytes) {
                Some(shortened) => {
                    cut += 1;
                    shortened
                }
                None => cell.to_owned(),
            };
            row_bytes += text.len();
            row.push(text);
        }
        if !self.rows.is_empty() && self.bytes + row_bytes > self.max_result_bytes {
            self.stopped = Some(Truncation::ByteLimit);
            return ControlFlow::Break(());
        }
        self.bytes += row_bytes;
        self.cells_truncated += cut;
        self.rows.push(row);
        ControlFlow::Continue(())
    }

    /// Ends the collection. `more_rows_exist`: the source reported rows it
    /// did not deliver (the driver's own `truncated` flag).
    pub fn finish(self, more_rows_exist: bool, affected_rows: Option<u64>) -> QueryResult {
        let truncated = self
            .stopped
            .or_else(|| more_rows_exist.then_some(Truncation::RowLimit));
        QueryResult {
            row_count: self.rows.len(),
            columns: self.columns,
            rows: self.rows,
            truncated: truncated.is_some(),
            truncation: truncated,
            cells_truncated: self.cells_truncated,
            affected_rows,
            row_limit: self.row_limit,
            read_only_enforced: false,
            elapsed_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(n: usize) -> Vec<ResultColumn> {
        (0..n)
            .map(|i| ResultColumn {
                name: format!("c{i}"),
                data_type: "text".into(),
            })
            .collect()
    }

    #[test]
    fn defaults_match_the_design() {
        let limits = Limits::default();
        assert_eq!(limits.default_rows, 200);
        assert_eq!(limits.max_rows, 1000);
        assert_eq!(limits.max_cell_bytes, 2048);
        assert_eq!(limits.max_result_bytes, 256 * 1024);
        assert_eq!(limits.rows_for(None), 200);
        assert_eq!(limits.rows_for(Some(0)), 200);
        assert_eq!(limits.rows_for(Some(5)), 5);
        assert_eq!(limits.rows_for(Some(50_000)), 1000);
    }

    #[test]
    fn cells_are_cut_on_a_character_boundary_and_marked() {
        assert_eq!(truncate_text("short", 10), None);
        let cut = truncate_text("ééééé", 3).unwrap();
        assert!(cut.starts_with("é…"), "{cut}");
        assert!(cut.contains("10 bytes in total"));
        let long = "x".repeat(5000);
        let cut = truncate_text(&long, 2048).unwrap();
        assert!(cut.starts_with(&"x".repeat(2048)));
        assert!(cut.ends_with("[truncated, 5000 bytes in total]"));
    }

    #[test]
    fn the_row_limit_stops_the_collection_and_flags_truncation() {
        let mut collector = RowCollector::new(columns(1), 2, &Limits::default());
        assert!(collector.push_row(["a"]).is_continue());
        assert!(collector.push_row(["b"]).is_continue());
        assert!(collector.push_row(["c"]).is_break());
        let result = collector.finish(false, None);
        assert_eq!(result.row_count, 2);
        assert!(result.truncated);
        assert_eq!(result.truncation, Some(Truncation::RowLimit));
    }

    #[test]
    fn a_source_that_reports_more_rows_is_truncated_without_hitting_the_limit() {
        let mut collector = RowCollector::new(columns(1), 10, &Limits::default());
        let _ = collector.push_row(["a"]);
        let result = collector.finish(true, None);
        assert!(result.truncated);
        assert_eq!(result.truncation, Some(Truncation::RowLimit));
        let mut collector = RowCollector::new(columns(1), 10, &Limits::default());
        let _ = collector.push_row(["a"]);
        assert!(!collector.finish(false, None).truncated);
    }

    #[test]
    fn the_byte_cap_stops_before_the_row_that_would_exceed_it() {
        let limits = Limits {
            max_result_bytes: 100,
            max_cell_bytes: 1000,
            ..Limits::default()
        };
        let mut collector = RowCollector::new(columns(1), 100, &limits);
        let wide = "y".repeat(40);
        assert!(collector.push_row([&wide]).is_continue());
        assert!(collector.push_row([&wide]).is_continue());
        assert!(collector.push_row([&wide]).is_break());
        assert!(collector.push_row([&wide]).is_break());
        let result = collector.finish(false, None);
        assert_eq!(result.row_count, 2);
        assert_eq!(result.truncation, Some(Truncation::ByteLimit));
    }

    #[test]
    fn the_first_row_is_always_kept_and_wide_cells_are_counted() {
        let limits = Limits {
            max_result_bytes: 10,
            max_cell_bytes: 8,
            ..Limits::default()
        };
        let mut collector = RowCollector::new(columns(2), 10, &limits);
        let wide = "z".repeat(100);
        assert!(collector.push_row([&wide, &wide]).is_continue());
        let result = collector.finish(false, None);
        assert_eq!(result.row_count, 1);
        assert_eq!(result.cells_truncated, 2);
        assert!(result.rows[0][0].len() < 100);
        assert!(!result.truncated);
    }
}
