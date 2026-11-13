use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use gpui::SharedString;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellPosition {
    pub row: usize,
    pub column: usize,
}

pub(crate) const MAX_PREVIEW_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PREVIEW_CACHE_ENTRIES: usize = 8192;

#[derive(Debug, Default)]
pub(crate) struct PreviewCache {
    values: HashMap<CellPosition, SharedString>,
    order: VecDeque<CellPosition>,
    bytes: usize,
}

impl PreviewCache {
    pub(crate) fn get(&self, position: CellPosition) -> Option<SharedString> {
        self.values.get(&position).cloned()
    }

    pub(crate) fn insert(&mut self, position: CellPosition, value: SharedString) {
        let value_bytes = value.as_ref().len();
        if let Some(previous) = self.values.insert(position, value) {
            self.bytes -= previous.as_ref().len();
        } else {
            self.order.push_back(position);
        }
        self.bytes += value_bytes;

        while self.bytes > MAX_PREVIEW_CACHE_BYTES || self.values.len() > MAX_PREVIEW_CACHE_ENTRIES
        {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.values.remove(&evicted) {
                self.bytes -= value.as_ref().len();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    #[cfg(test)]
    pub(crate) fn byte_len(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod preview_cache_tests {
    use super::*;

    #[test]
    fn preview_cache_bounds_empty_values_by_entry_count() {
        let mut cache = PreviewCache::default();
        for row in 0..(MAX_PREVIEW_CACHE_ENTRIES + 1) {
            cache.insert(
                CellPosition { row, column: 0 },
                SharedString::new_static(""),
            );
        }

        assert_eq!(cache.len(), MAX_PREVIEW_CACHE_ENTRIES);
        assert!(cache.byte_len() <= MAX_PREVIEW_CACHE_BYTES);
    }
}

/// Data source contract for one grid page. The grid never touches Arrow or
/// driver FFI directly; adapters implement this with whatever the backend
/// produced (Arrow batches, driver cell batches, ...).
/// What a cell holds besides text. A document store's field can be absent from
/// a document, which is neither a NULL nor an empty string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellState {
    Value,
    Null,
    /// The row has no such field (a document that lacks the key).
    Missing,
}

pub trait GridSource {
    fn row_count(&self) -> usize;
    fn column_count(&self) -> usize;
    fn column_name(&self, column: usize) -> Option<&str>;
    fn column_type(&self, _column: usize) -> Option<&str> {
        None
    }
    /// Native database type the backend reported for the column (`bigint`,
    /// `text`, ...), when it publishes one; `column_type` stays the logical
    /// type that sort/copy/alignment behavior keys on.
    fn native_type(&self, _column: usize) -> Option<&str> {
        None
    }
    fn cell_text(&self, position: CellPosition) -> Option<SharedString>;
    #[cfg(test)]
    fn row_values(&self, row: usize) -> Vec<(String, String)>;

    /// Full, untruncated text for copy/edit/export paths.
    /// Defaults to the display text; sources that truncate for display
    /// override this to return the original value.
    fn full_cell_text(&self, position: CellPosition) -> Option<String> {
        self.cell_text(position).map(|s| s.to_string())
    }

    fn cell_is_null(&self, _position: CellPosition) -> bool {
        false
    }

    /// The raw bytes of a binary cell (BLOB, bytea, BINARY), borrowed from the
    /// page; `None` for a NULL, a cell of another type, or a source that holds
    /// no bytes (document stores deliver binary values as text).
    fn cell_bytes(&self, _position: CellPosition) -> Option<&[u8]> {
        None
    }

    /// The whole text of a text cell, borrowed from the page (no copy, no
    /// preview cut); `None` for a NULL, another type, or a source without one.
    fn cell_str(&self, _position: CellPosition) -> Option<&str> {
        None
    }

    /// NULL / missing / value in one lookup (the row renderer asks per visible
    /// cell). Sources with absent fields override it.
    fn cell_state(&self, position: CellPosition) -> CellState {
        if self.cell_is_null(position) {
            CellState::Null
        } else {
            CellState::Value
        }
    }

    /// Scans the page for cells whose text matches `scan.query`, appending
    /// display positions in row-major order (the `CellPosition` ordering) and
    /// stopping once `scan.max_matches` are collected or `scan.cancel` is set.
    ///
    /// Sources override this to read their backing data directly without
    /// touching the preview cache or allocating per cell. This default goes
    /// through `cell_text` and is only meant for small, custom sources.
    fn search_cells(&self, scan: &GridSearchScan<'_>, out: &mut Vec<CellPosition>) -> GridSearchStatus {
        let mut scratch = String::new();
        for row in scan.rows(self.row_count()) {
            for (column, &source_column) in scan.columns.iter().enumerate() {
                let position = CellPosition {
                    row: row.source,
                    column: source_column,
                };
                if self.cell_is_null(position) {
                    continue;
                }
                let Some(text) = self.cell_text(position) else {
                    continue;
                };
                if scan.query.matches(text.as_ref(), &mut scratch)
                    && scan.push(out, CellPosition { row: row.display, column })
                {
                    return GridSearchStatus::Capped;
                }
            }
        }
        scan.finish()
    }
}

/// Matching options for the in-grid find bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GridSearchOptions {
    pub match_case: bool,
    pub whole_cell: bool,
}

/// A prepared find query. The needle is normalised once so the per-cell test
/// never allocates: ASCII needles compare byte-wise with ASCII case folding
/// (no lowercase copy of the haystack); only non-ASCII case-insensitive
/// needles lowercase the haystack into a caller-owned, reused scratch buffer.
#[derive(Debug, Clone)]
pub struct GridSearchQuery {
    needle: String,
    options: GridSearchOptions,
    needle_is_ascii: bool,
}

impl GridSearchQuery {
    /// `None` for an empty (after trimming) query, which clears the search.
    pub fn new(text: &str, options: GridSearchOptions) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let needle_is_ascii = text.is_ascii();
        let needle = if options.match_case {
            text.to_string()
        } else if needle_is_ascii {
            text.to_ascii_lowercase()
        } else {
            text.to_lowercase()
        };
        Some(Self {
            needle,
            options,
            needle_is_ascii,
        })
    }

    /// True when `haystack` matches. `scratch` is only touched for non-ASCII
    /// case-insensitive needles and keeps its capacity across calls.
    pub fn matches(&self, haystack: &str, scratch: &mut String) -> bool {
        let needle = self.needle.as_str();
        if self.options.match_case {
            return if self.options.whole_cell {
                haystack == needle
            } else {
                haystack.contains(needle)
            };
        }
        if self.needle_is_ascii {
            let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
            return if self.options.whole_cell {
                haystack.eq_ignore_ascii_case(needle)
            } else {
                contains_ascii_ignore_case(haystack, needle)
            };
        }
        // A non-ASCII needle cannot occur in pure-ASCII text.
        if haystack.is_ascii() {
            return false;
        }
        scratch.clear();
        for ch in haystack.chars() {
            scratch.extend(ch.to_lowercase());
        }
        if self.options.whole_cell {
            scratch.as_str() == needle
        } else {
            scratch.contains(needle)
        }
    }
}

/// `needle` must be non-empty ASCII already lowercased.
fn contains_ascii_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    if needle.len() > haystack.len() {
        return false;
    }
    let first_upper = first.to_ascii_uppercase();
    let last_start = haystack.len() - needle.len();
    (0..=last_start).any(|start| {
        let byte = haystack[start];
        (byte == first || byte == first_upper)
            && haystack[start + 1..start + needle.len()].eq_ignore_ascii_case(rest)
    })
}

/// How a search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridSearchStatus {
    Complete,
    /// Stopped at `max_matches`; more matches may exist.
    Capped,
    Cancelled,
}

/// Parameters of one [`GridSource::search_cells`] scan.
pub struct GridSearchScan<'a> {
    pub query: &'a GridSearchQuery,
    /// Source column index for each display column, in display order.
    pub columns: &'a [usize],
    /// Display row -> source row (a locally sorted page); `None` is identity.
    pub row_order: Option<&'a [usize]>,
    /// Stop as soon as this many matches are collected.
    pub max_matches: usize,
    pub cancel: &'a std::sync::atomic::AtomicBool,
}

/// One row a scan visits, in both coordinate systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanRow {
    pub display: usize,
    /// Row in the backing source (differs after a local page sort).
    pub source: usize,
}

impl GridSearchScan<'_> {
    const CANCEL_POLL_ROWS: usize = 256;

    /// The rows to visit in display order. The cancel flag is polled every
    /// few hundred rows (an atomic load per row would be noise next to the
    /// cell work) and ends the iteration; call [`Self::finish`] afterwards.
    pub fn rows(&self, source_rows: usize) -> impl Iterator<Item = ScanRow> + '_ {
        let count = self.row_order.map_or(source_rows, <[usize]>::len);
        (0..count)
            .take_while(|display| !self.cancelled_at(*display))
            .map(|display| ScanRow {
                display,
                source: self
                    .row_order
                    .and_then(|order| order.get(display))
                    .copied()
                    .unwrap_or(display),
            })
    }

    fn cancelled_at(&self, display_row: usize) -> bool {
        display_row.is_multiple_of(Self::CANCEL_POLL_ROWS)
            && self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Status of a scan whose row iteration ran out without hitting the cap.
    pub fn finish(&self) -> GridSearchStatus {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            GridSearchStatus::Cancelled
        } else {
            GridSearchStatus::Complete
        }
    }

    /// Records a match; true when the cap is reached and the scan must stop.
    pub fn push(&self, out: &mut Vec<CellPosition>, position: CellPosition) -> bool {
        out.push(position);
        out.len() >= self.max_matches
    }
}

/// Pixel geometry for grid columns. Pure math, no GPUI types, so it stays
/// unit-testable.
#[derive(Debug, Clone)]
pub struct ColumnLayout {
    /// Width per column in pixels.
    pub widths: Vec<f32>,
    /// Width of the leading row-number gutter.
    pub gutter_width: f32,
    pub min_width: f32,
    pub max_width: f32,
}

impl ColumnLayout {
    pub const DEFAULT_WIDTH: f32 = 160.0;
    pub const DEFAULT_MIN: f32 = 60.0;
    pub const DEFAULT_MAX: f32 = 480.0;
    pub const DEFAULT_GUTTER: f32 = 48.0;
    /// Gutter without row numbers: only the pending-delete marker needs room.
    pub const HIDDEN_GUTTER: f32 = 24.0;
    /// Extra columns rendered beyond each viewport edge.
    pub const COLUMN_OVERSCAN: usize = 2;

    pub fn new(column_count: usize) -> Self {
        Self {
            widths: vec![Self::DEFAULT_WIDTH; column_count],
            gutter_width: Self::DEFAULT_GUTTER,
            min_width: Self::DEFAULT_MIN,
            max_width: Self::DEFAULT_MAX,
        }
    }

    /// A layout from the user's column settings: every column starts at
    /// `default_width` (kept within `min..=max`).
    pub fn with_limits(
        column_count: usize,
        default_width: f32,
        min_width: f32,
        max_width: f32,
        gutter_width: f32,
    ) -> Self {
        let (min_width, max_width) = (min_width.min(max_width), max_width.max(min_width));
        Self {
            widths: vec![default_width.clamp(min_width, max_width); column_count],
            gutter_width,
            min_width,
            max_width,
        }
    }

    /// Sizes `column` for text of `chars` characters at `font_size` (average
    /// glyph = 0.6 em, plus the cell padding), within the limits.
    pub fn fit_column(&mut self, column: usize, chars: usize, font_size: f32) {
        self.set_width(column, chars as f32 * font_size * 0.6 + 18.0);
    }

    /// Clamps a resize so columns stay within their min/max bounds.
    pub fn set_width(&mut self, column: usize, width: f32) {
        if let Some(slot) = self.widths.get_mut(column) {
            *slot = width.clamp(self.min_width, self.max_width);
        }
    }

    pub fn resize_by(&mut self, column: usize, start_width: f32, delta: f32) {
        self.set_width(column, start_width + delta);
    }

    /// Pixel offset of a column's left edge (excluding the gutter).
    pub fn offset(&self, column: usize) -> f32 {
        self.widths.iter().take(column).sum()
    }

    /// Total pixel width the grid content occupies.
    pub fn content_width(&self) -> f32 {
        self.gutter_width + self.widths.iter().sum::<f32>()
    }

    /// Display column under a content-space x (pixels from the grid's left
    /// edge, gutter included). `None` over the gutter or past the last column.
    pub fn column_at(&self, x: f32) -> Option<usize> {
        let mut left = self.gutter_width;
        if x < left {
            return None;
        }
        for (column, width) in self.widths.iter().enumerate() {
            left += width;
            if x < left {
                return Some(column);
            }
        }
        None
    }

    /// Pixel width of the first `count` columns (the pinned block, gutter excluded).
    pub fn leading_width(&self, count: usize) -> f32 {
        self.widths.iter().take(count).sum()
    }

    /// Columns to materialise for the horizontal viewport
    /// `[scroll_left, scroll_left + viewport_width)` (content space).
    /// One pass over the widths, no allocation. `editing` (the inline-edit
    /// column) is always included so its editor keeps its element state.
    ///
    /// The first `frozen` columns are pinned: they are drawn in the sticky
    /// block (gutter and pinned columns stay at the viewport's left edge), so
    /// they are never part of a segment, and the scrolling columns are those
    /// visible right of that block. A segment's spacer starts after the block.
    pub fn column_window(
        &self,
        scroll_left: f32,
        viewport_width: f32,
        editing: Option<usize>,
        frozen: usize,
    ) -> ColumnWindow {
        let count = self.widths.len();
        let frozen = frozen.min(count);
        let mut left = self.gutter_width;
        let mut total = self.gutter_width;
        let (mut first, mut last) = (usize::MAX, 0);
        // The sticky block covers the left of the viewport.
        let view_left = scroll_left + if frozen > 0 { self.gutter_width + self.leading_width(frozen) } else { 0.0 };
        let view_right = scroll_left + viewport_width;
        for (column, width) in self.widths.iter().enumerate() {
            let right = left + width;
            if column >= frozen && right > view_left && left < view_right {
                first = first.min(column);
                last = column;
            }
            left = right;
            total = right;
        }
        if first == usize::MAX {
            // Viewport past every column (or no columns): keep the tail.
            first = count.saturating_sub(1);
            last = first;
        }
        let start = first.saturating_sub(Self::COLUMN_OVERSCAN).max(frozen);
        let end = if count == 0 {
            0
        } else {
            (last + 1 + Self::COLUMN_OVERSCAN).min(count)
        };
        let spacer = |from: usize, to: usize| self.widths[from.max(frozen)..to.max(frozen)].iter().sum::<f32>();
        let mut segments = [
            ColumnSegment {
                columns: start..end.max(start),
                spacer: spacer(0, start.min(count)),
            },
            ColumnSegment {
                columns: 0..0,
                spacer: 0.0,
            },
        ];
        if let Some(editing) = editing.filter(|editing| (frozen..count).contains(editing)) {
            let main = segments[0].columns.clone();
            if editing + 1 == main.start {
                segments[0].columns.start = editing;
                segments[0].spacer = spacer(0, editing);
            } else if editing == main.end {
                segments[0].columns.end = editing + 1;
            } else if editing < main.start {
                segments[1] = ColumnSegment {
                    columns: main.clone(),
                    spacer: spacer(editing + 1, main.start),
                };
                segments[0] = ColumnSegment {
                    columns: editing..editing + 1,
                    spacer: spacer(0, editing),
                };
            } else if editing > main.end {
                segments[1] = ColumnSegment {
                    columns: editing..editing + 1,
                    spacer: spacer(main.end, editing),
                };
            }
        }
        ColumnWindow {
            segments,
            total_width: total,
        }
    }
}

/// A run of display columns to render, preceded by `spacer` pixels of
/// culled columns.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSegment {
    pub columns: Range<usize>,
    pub spacer: f32,
}

/// Horizontally culled column set for one frame (see
/// [`ColumnLayout::column_window`]). The second segment is empty unless an
/// off-screen column is pinned.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnWindow {
    pub segments: [ColumnSegment; 2],
    /// Full content width, gutter included, so culling never moves the
    /// scrollbar or header alignment.
    pub total_width: f32,
}

impl ColumnWindow {
    pub fn iter(&self) -> impl Iterator<Item = &ColumnSegment> {
        self.segments
            .iter()
            .filter(|segment| !segment.columns.is_empty())
    }

    #[cfg(test)]
    pub fn contains(&self, column: usize) -> bool {
        self.iter().any(|segment| segment.columns.contains(&column))
    }
}

#[derive(Debug)]
pub struct GridState<S> {
    source: Arc<S>,
    page_order: Option<Arc<Vec<usize>>>,
    /// Source column and direction `page_order` sorts by.
    page_sort: Option<(usize, bool)>,
}

impl<S> Clone for GridState<S> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            page_order: self.page_order.clone(),
            page_sort: self.page_sort,
        }
    }
}

impl<S: GridSource> GridState<S> {
    pub fn new(source: S) -> Self {
        Self {
            source: Arc::new(source),
            page_order: None,
            page_sort: None,
        }
    }

    pub fn source(&self) -> &S {
        self.source.as_ref()
    }

    pub fn same_page(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source)
            && match (&self.page_order, &other.page_order) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }

    pub fn source_row(&self, row: usize) -> usize {
        self.page_order
            .as_ref()
            .and_then(|order| order.get(row))
            .copied()
            .unwrap_or(row)
    }

    pub fn clear_page_sort(&mut self) {
        self.page_order = None;
        self.page_sort = None;
    }

    /// The source column and direction (`true` = descending) of the active
    /// page sort.
    pub fn page_sort(&self) -> Option<(usize, bool)> {
        self.page_sort
    }

    /// Orders the page locally by `column`, keys parsed once per row (no
    /// allocation per comparison). NULL is the greatest value:
    /// last ascending, first descending. Equal
    /// keys keep their page order.
    pub fn sort_page(
        &mut self,
        column: usize,
        kind: SortKind,
        descending: bool,
    ) -> Result<(), String> {
        if column >= self.column_count() {
            return Err("Sort column is not in this page".into());
        }
        let texts: Vec<Option<String>> = (0..self.row_count())
            .map(|row| {
                let position = CellPosition { row, column };
                if self.source.cell_is_null(position) {
                    None
                } else {
                    self.source.full_cell_text(position)
                }
            })
            .collect();
        let keys = SortKey::column(kind, texts);
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by(|&a, &b| {
            let comparison = match (&keys[a], &keys[b]) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(left), Some(right)) => left.cmp(right),
            };
            if descending {
                comparison.reverse()
            } else {
                comparison
            }
        });
        self.page_order = Some(Arc::new(order));
        self.page_sort = Some((column, descending));
        Ok(())
    }

    pub fn row_count(&self) -> usize {
        self.source.row_count()
    }

    pub fn column_count(&self) -> usize {
        self.source.column_count()
    }

    pub fn column_name(&self, column: usize) -> Option<&str> {
        self.source.column_name(column)
    }

    pub fn column_type(&self, column: usize) -> Option<&str> {
        self.source.column_type(column)
    }

    pub fn native_type(&self, column: usize) -> Option<&str> {
        self.source.native_type(column)
    }

    /// True when both states share the same underlying result source. The
    /// pointer comparison cannot alias a freed result while one is retained.
    pub fn same_source(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source)
    }

    #[cfg(test)]
    pub fn visible_rows(&self, start: usize, count: usize) -> Range<usize> {
        let start = start.min(self.row_count());
        start..start.saturating_add(count).min(self.row_count())
    }

    pub fn cell_text(&self, position: CellPosition) -> Option<SharedString> {
        self.source.cell_text(CellPosition {
            row: self.source_row(position.row),
            ..position
        })
    }

    pub fn full_cell_text(&self, position: CellPosition) -> Option<String> {
        self.source.full_cell_text(CellPosition {
            row: self.source_row(position.row),
            ..position
        })
    }

    pub fn cell_is_null(&self, position: CellPosition) -> bool {
        self.source.cell_is_null(CellPosition {
            row: self.source_row(position.row),
            ..position
        })
    }

    pub fn cell_bytes(&self, position: CellPosition) -> Option<&[u8]> {
        self.source.cell_bytes(CellPosition {
            row: self.source_row(position.row),
            ..position
        })
    }

    pub fn cell_str(&self, position: CellPosition) -> Option<&str> {
        self.source.cell_str(CellPosition {
            row: self.source_row(position.row),
            ..position
        })
    }

    pub fn cell_state(&self, position: CellPosition) -> CellState {
        self.source.cell_state(CellPosition {
            row: self.source_row(position.row),
            ..position
        })
    }

    #[cfg(test)]
    pub fn row_values(&self, row: usize) -> Vec<(String, String)> {
        self.source.row_values(self.source_row(row))
    }

    /// Finds up to `max_matches` matching cells in display coordinates
    /// (display row after any local page sort, display column index into
    /// `columns`). Reads the backing data directly; it never goes through
    /// `cell_text`, so the preview cache is untouched. Meant to run on a
    /// background executor: `cancel` stops it early.
    pub fn search_cells(
        &self,
        query: &GridSearchQuery,
        columns: &[usize],
        max_matches: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> (Vec<CellPosition>, GridSearchStatus) {
        let mut out = Vec::new();
        if max_matches == 0 {
            return (out, GridSearchStatus::Capped);
        }
        let scan = GridSearchScan {
            query,
            columns,
            row_order: self.page_order.as_deref().map(Vec::as_slice),
            max_matches,
            cancel,
        };
        let status = self.source.search_cells(&scan, &mut out);
        (out, status)
    }
}

/// How the values of a column are ordered, from the column's resolved type
/// (never from its display label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKind {
    /// Whole numbers of any width (`UInt64` above 2^53, `Int128`).
    Integer,
    /// Decimals and floats: exact decimal order, `NaN` greatest.
    Number,
    Boolean,
    /// Dates and timestamps, by value.
    Temporal,
    Text,
}

enum SortKey {
    Integer(i128),
    Number(NumberKey),
    Boolean(bool),
    /// Year (BC negative) and the rest of the ISO text, which orders itself:
    /// a date or a zone-less timestamp, ordered by its wall time.
    Temporal(i64, Box<str>),
    /// A timestamp that carries its offset, ordered by the instant it names
    /// (seconds since the epoch, nanoseconds).
    Instant(i64, u32),
    Text(String),
}

impl SortKey {
    /// The keys of one column. A value that does not read as the column's kind
    /// moves the WHOLE column to the next coarser order (integer, number,
    /// text), so one odd value cannot leave the order inconsistent.
    fn column(kind: SortKind, texts: Vec<Option<String>>) -> Vec<Option<SortKey>> {
        let attempt = |parse: fn(&str) -> Option<SortKey>| -> Option<Vec<Option<SortKey>>> {
            texts
                .iter()
                .map(|text| match text {
                    None => Some(None),
                    Some(text) => parse(text.trim()).map(Some),
                })
                .collect()
        };
        let typed = match kind {
            SortKind::Integer => attempt(Self::integer).or_else(|| attempt(Self::number)),
            SortKind::Number => attempt(Self::number),
            SortKind::Boolean => attempt(Self::boolean),
            // Offsets are applied when every value has one; else wall time.
            SortKind::Temporal => attempt(Self::instant).or_else(|| attempt(Self::temporal)),
            SortKind::Text => None,
        };
        typed.unwrap_or_else(|| {
            texts
                .into_iter()
                .map(|text| text.map(SortKey::Text))
                .collect()
        })
    }

    fn integer(text: &str) -> Option<Self> {
        text.parse().ok().map(Self::Integer)
    }

    fn number(text: &str) -> Option<Self> {
        NumberKey::parse(text).map(Self::Number)
    }

    fn boolean(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Some(Self::Boolean(true)),
            "false" | "f" | "0" => Some(Self::Boolean(false)),
            _ => None,
        }
    }

    /// `YYYY-MM-DD HH:MM:SS[.f]±HH[:MM]` (also `T`, `Z` and ` UTC`): the
    /// instant, so `02:30+02` is before `02:15+01` of the same night.
    fn instant(text: &str) -> Option<Self> {
        let normalized;
        let text = if let Some(local) = text.strip_suffix(" UTC").or_else(|| text.strip_suffix('Z')) {
            normalized = format!("{local}+00");
            normalized.as_str()
        } else {
            text
        };
        ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"]
            .iter()
            .find_map(|format| chrono::DateTime::parse_from_str(text, format).ok())
            .map(|moment| Self::Instant(moment.timestamp(), moment.timestamp_subsec_nanos()))
    }

    /// `[-]YYYY-MM-DD…` (any year width); PostgreSQL's ` BC` marker (after the
    /// date, before any time) makes the year negative.
    fn temporal(text: &str) -> Option<Self> {
        let bc = text.contains(" BC");
        let text = if bc {
            std::borrow::Cow::Owned(text.replacen(" BC", "", 1))
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (bc, text.as_ref()),
        };
        let year_len = unsigned.bytes().take_while(u8::is_ascii_digit).count();
        let rest = unsigned.get(year_len..)?;
        if year_len == 0
            || year_len > 9
            || !rest.starts_with('-')
            || !rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
        {
            return None;
        }
        let year: i64 = unsigned[..year_len].parse().ok()?;
        Some(Self::Temporal(
            if negative { -year } else { year },
            rest.into(),
        ))
    }

    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Integer(a), Self::Integer(b)) => a.cmp(b),
            (Self::Number(a), Self::Number(b)) => a.cmp(b),
            (Self::Boolean(a), Self::Boolean(b)) => a.cmp(b),
            (Self::Temporal(ay, ar), Self::Temporal(by, br)) => ay.cmp(by).then_with(|| ar.cmp(br)),
            (Self::Instant(a_secs, a_nanos), Self::Instant(b_secs, b_nanos)) => {
                a_secs.cmp(b_secs).then_with(|| a_nanos.cmp(b_nanos))
            }
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// A number in the order a database gives it: `-Infinity` < finite values <
/// `Infinity` < `NaN` (PostgreSQL's, which `f64::total_cmp` also gives).
enum NumberKey {
    NegativeInfinity,
    Finite(DecimalKey),
    PositiveInfinity,
    NaN,
}

impl NumberKey {
    fn parse(text: &str) -> Option<Self> {
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        if unsigned.eq_ignore_ascii_case("nan") {
            return Some(Self::NaN);
        }
        if unsigned.eq_ignore_ascii_case("inf") || unsigned.eq_ignore_ascii_case("infinity") {
            return Some(if negative {
                Self::NegativeInfinity
            } else {
                Self::PositiveInfinity
            });
        }
        DecimalKey::parse(negative, unsigned).map(Self::Finite)
    }

    fn rank(&self) -> u8 {
        match self {
            Self::NegativeInfinity => 0,
            Self::Finite(_) => 1,
            Self::PositiveInfinity => 2,
            Self::NaN => 3,
        }
    }

    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Finite(a), Self::Finite(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

/// Largest exponent read exactly (`f64` stops at 308; a longer one is text).
const MAX_EXPONENT: u64 = 400;

/// An exact decimal: sign, and the digits without leading zeros before the
/// point or trailing zeros after it.
struct DecimalKey {
    negative: bool,
    /// Integral digits followed by fractional digits.
    digits: String,
    integral_len: usize,
}

impl DecimalKey {
    /// `digits[.digits][(e|E)[+-]digits]` after its sign.
    fn parse(negative: bool, unsigned: &str) -> Option<Self> {
        let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
            Some(at) => {
                let exponent: i64 = unsigned[at + 1..].parse().ok()?;
                if exponent.unsigned_abs() > MAX_EXPONENT {
                    return None;
                }
                (&unsigned[..at], exponent)
            }
            None => (unsigned, 0),
        };
        let (integral, fractional) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let plain = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if (integral.is_empty() && fractional.is_empty()) || !plain(integral) || !plain(fractional) {
            return None;
        }
        let mut digits = String::with_capacity(integral.len() + fractional.len());
        digits.push_str(integral);
        digits.push_str(fractional);
        // Where the point is once the exponent is applied.
        let point = integral.len() as i64 + exponent;
        let integral_len = if point <= 0 {
            digits.insert_str(0, &"0".repeat((-point) as usize));
            0
        } else if point as usize > digits.len() {
            digits.push_str(&"0".repeat(point as usize - digits.len()));
            point as usize
        } else {
            point as usize
        };
        let leading = digits[..integral_len]
            .bytes()
            .take_while(|byte| *byte == b'0')
            .count();
        digits.drain(..leading);
        let integral_len = integral_len - leading;
        let trimmed = digits.trim_end_matches('0').len();
        digits.truncate(trimmed.max(integral_len));
        let zero = digits.bytes().all(|byte| byte == b'0');
        if zero {
            digits.clear();
        }
        Some(Self {
            negative: negative && !zero,
            integral_len: if zero { 0 } else { integral_len },
            digits,
        })
    }

    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self.negative, other.negative) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let (left_int, left_frac) = self.digits.split_at(self.integral_len);
        let (right_int, right_frac) = other.digits.split_at(other.integral_len);
        // Fractions have no trailing zero, so a prefix is the smaller one.
        let order = left_int
            .len()
            .cmp(&right_int.len())
            .then_with(|| left_int.cmp(right_int))
            .then_with(|| left_frac.cmp(right_frac));
        if self.negative {
            order.reverse()
        } else {
            order
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRange {
    pub start: CellPosition,
    pub end: CellPosition,
}

impl CellRange {
    pub fn normalized(self) -> Self {
        Self {
            start: CellPosition {
                row: self.start.row.min(self.end.row),
                column: self.start.column.min(self.end.column),
            },
            end: CellPosition {
                row: self.start.row.max(self.end.row),
                column: self.start.column.max(self.end.column),
            },
        }
    }

    /// Rectangular containment (both axes). `CellPosition`'s derived ordering
    /// is row-major, so comparing against `start`/`end` directly would treat a
    /// multi-column range as a linear span of the grid.
    pub fn contains(self, position: CellPosition) -> bool {
        let range = self.normalized();
        position.row >= range.start.row
            && position.row <= range.end.row
            && position.column >= range.start.column
            && position.column <= range.end.column
    }

    /// Iterates the normalized range in row-major order.
    pub fn cells(self) -> impl Iterator<Item = CellPosition> {
        let normalized = self.normalized();
        let rows = normalized.start.row..=normalized.end.row;
        let columns = normalized.start.column..=normalized.end.column;
        rows.flat_map(move |row| {
            columns
                .clone()
                .map(move |column| CellPosition { row, column })
        })
    }
}

/// Selection + clipboard export for one grid. Selection ranges are kept
/// normalized so rendering and copying never re-derive them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GridSelection {
    pub anchor: Option<CellPosition>,
    pub range: Option<CellRange>,
}

impl GridSelection {
    pub fn select(&mut self, position: CellPosition) {
        self.anchor = Some(position);
        self.range = Some(CellRange {
            start: position,
            end: position,
        });
    }

    pub fn extend_to(&mut self, position: CellPosition) {
        let Some(anchor) = self.anchor else {
            self.select(position);
            return;
        };
        self.range = Some(CellRange {
            start: anchor,
            end: position,
        });
    }

    pub fn clear(&mut self) {
        self.anchor = None;
        self.range = None;
    }

    pub fn context_target(&mut self, position: CellPosition) {
        if !self.range.is_some_and(|range| range.contains(position)) {
            self.select(position);
        }
    }

    #[cfg(test)]
    pub fn selected_cells(&self) -> Vec<CellPosition> {
        self.range
            .map(|range| range.cells().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
/// Tab-separated values for the selected cells, ready for Cmd+C.
/// NULL renders as the literal text `NULL`, matching the grid.
pub fn selection_as_tsv<S: GridSource>(grid: &GridState<S>, selection: &GridSelection) -> String {
    let Some(range) = selection.range else {
        return String::new();
    };
    let range = range.normalized();
    let mut rows: Vec<String> = Vec::with_capacity(range.end.row - range.start.row + 1);
    for row in range.start.row..=range.end.row {
        let line = (range.start.column..=range.end.column)
            .map(|column| {
                grid.full_cell_text(CellPosition { row, column })
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\t");
        rows.push(line);
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory source with no Arrow dependency: proves the grid renders
    /// from any page source, not just RecordBatch.
    struct MockSource {
        columns: Vec<&'static str>,
        cells: Vec<Vec<Option<String>>>,
    }

    impl MockSource {
        fn new(columns: Vec<&'static str>, cells: Vec<Vec<Option<String>>>) -> Self {
            Self { columns, cells }
        }
    }

    impl GridSource for MockSource {
        fn row_count(&self) -> usize {
            self.cells.len()
        }

        fn column_count(&self) -> usize {
            self.columns.len()
        }

        fn column_name(&self, column: usize) -> Option<&str> {
            self.columns.get(column).copied()
        }

        fn cell_text(&self, position: CellPosition) -> Option<SharedString> {
            self.cells
                .get(position.row)?
                .get(position.column)?
                .clone()
                .map(SharedString::from)
                .or_else(|| Some(SharedString::from("NULL")))
        }

        fn cell_is_null(&self, position: CellPosition) -> bool {
            self.cells
                .get(position.row)
                .and_then(|row| row.get(position.column))
                .is_some_and(Option::is_none)
        }

        fn row_values(&self, row: usize) -> Vec<(String, String)> {
            (0..self.column_count())
                .filter_map(|column| {
                    Some((
                        self.column_name(column)?.to_string(),
                        self.cell_text(CellPosition { row, column })?.to_string(),
                    ))
                })
                .collect()
        }
    }

    fn query(text: &str, match_case: bool, whole_cell: bool) -> GridSearchQuery {
        GridSearchQuery::new(
            text,
            GridSearchOptions {
                match_case,
                whole_cell,
            },
        )
        .expect("non-empty query")
    }

    #[test]
    fn search_query_is_case_insensitive_substring_by_default() {
        let mut scratch = String::new();
        let q = query("ada", false, false);
        assert!(q.matches("Ada Lovelace", &mut scratch));
        assert!(q.matches("my ADA", &mut scratch));
        assert!(!q.matches("Alan", &mut scratch));
        assert!(!q.matches("ad", &mut scratch));
        assert!(!q.matches("", &mut scratch));
        // The query is trimmed; blank means "no search".
        assert!(query("  ada \t", false, false).matches("Ada", &mut scratch));
        assert!(GridSearchQuery::new("   ", GridSearchOptions::default()).is_none());
        assert!(GridSearchQuery::new("", GridSearchOptions::default()).is_none());
    }

    #[test]
    fn search_query_match_case_and_whole_cell() {
        let mut scratch = String::new();
        let sensitive = query("Ada", true, false);
        assert!(sensitive.matches("Dr Ada", &mut scratch));
        assert!(!sensitive.matches("dr ada", &mut scratch));
        let whole = query("ada", false, true);
        assert!(whole.matches("ADA", &mut scratch));
        assert!(!whole.matches("Ada L", &mut scratch));
        let both = query("Ada", true, true);
        assert!(both.matches("Ada", &mut scratch));
        assert!(!both.matches("ada", &mut scratch));
        assert!(!both.matches("Adam", &mut scratch));
    }

    #[test]
    fn search_query_folds_non_ascii_case_through_the_scratch_buffer() {
        let mut scratch = String::new();
        let q = query("ÄB", false, false);
        assert!(q.matches("xx äb yy", &mut scratch));
        assert!(q.matches("ÄB", &mut scratch));
        assert!(!q.matches("ab", &mut scratch));
        assert!(!q.matches("plain ascii", &mut scratch));
        assert!(query("äb", false, true).matches("ÄB", &mut scratch));
        assert!(!query("äb", false, true).matches("ÄBC", &mut scratch));
        // A non-ASCII needle never matches inside ASCII text, and an ASCII
        // needle still finds ASCII inside non-ASCII text.
        assert!(query("é", true, false).matches("café", &mut scratch));
        assert!(query("caf", false, false).matches("CAFÉ", &mut scratch));
    }

    #[test]
    fn default_search_scans_display_rows_and_columns_and_skips_nulls() {
        let grid = GridState::new(MockSource::new(
            vec!["id", "name", "note"],
            vec![
                vec![Some("1".into()), Some("Ada".into()), None],
                vec![Some("2".into()), Some("Lin".into()), Some("ada's".into())],
                vec![Some("3".into()), Some("Bob".into()), Some("n/a".into())],
            ],
        ));
        let never = std::sync::atomic::AtomicBool::new(false);
        // Display columns reorder to [note, name]; source cols are 2 and 1.
        let (cells, status) = grid.search_cells(&query("ada", false, false), &[2, 1], 100, &never);
        assert_eq!(status, GridSearchStatus::Complete);
        assert_eq!(
            cells,
            [
                CellPosition { row: 0, column: 1 },
                CellPosition { row: 1, column: 0 }
            ]
        );
        // NULL is rendered as "NULL" text but is never a match.
        let (cells, _) = grid.search_cells(&query("null", false, false), &[0, 1, 2], 100, &never);
        assert!(cells.is_empty());
        // The cap stops the scan early.
        let (cells, status) = grid.search_cells(&query("a", false, false), &[0, 1, 2], 2, &never);
        assert_eq!((cells.len(), status), (2, GridSearchStatus::Capped));
        // A set cancel flag is honoured (polled at row 0).
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let (cells, status) = grid.search_cells(&query("a", false, false), &[0, 1, 2], 100, &cancel);
        assert_eq!((cells.len(), status), (0, GridSearchStatus::Cancelled));
    }

    #[test]
    fn search_maps_display_rows_through_a_local_page_sort() {
        let mut grid = GridState::new(MockSource::new(
            vec!["n", "name"],
            vec![
                vec![Some("3".into()), Some("Ada".into())],
                vec![Some("1".into()), Some("Lin".into())],
                vec![Some("2".into()), Some("Ada".into())],
            ],
        ));
        grid.sort_page(0, SortKind::Integer, false).unwrap(); // source rows 1, 2, 0
        let never = std::sync::atomic::AtomicBool::new(false);
        let (cells, _) = grid.search_cells(&query("ada", false, false), &[0, 1], 100, &never);
        assert_eq!(
            cells,
            [
                CellPosition { row: 1, column: 1 },
                CellPosition { row: 2, column: 1 }
            ],
            "matches are reported in display rows"
        );
    }

    #[test]
    fn grid_renders_from_an_arbitrary_page_source() {
        let source = MockSource::new(
            vec!["_id", "name"],
            vec![
                vec![Some("1".into()), Some("Ada".into())],
                vec![Some("2".into()), None],
            ],
        );
        let grid = GridState::new(source);

        assert_eq!(grid.row_count(), 2);
        assert_eq!(grid.column_count(), 2);
        assert_eq!(grid.visible_rows(1, 10), 1..2);
        assert_eq!(
            grid.cell_text(CellPosition { row: 0, column: 0 }),
            Some("1".into())
        );
        assert_eq!(
            grid.cell_text(CellPosition { row: 1, column: 1 }),
            Some("NULL".into())
        );
        assert_eq!(grid.cell_text(CellPosition { row: 2, column: 0 }), None);
        assert_eq!(grid.column_name(0), Some("_id"));
        assert_eq!(grid.column_name(2), None);
    }

    #[test]
    fn cloning_grid_state_shares_the_page_source() {
        let grid = GridState::new(MockSource::new(vec!["id"], vec![vec![Some("1".into())]]));
        let cloned = grid.clone();

        assert!(std::ptr::eq(grid.source(), cloned.source()));
    }

    #[test]
    fn local_page_sort_is_stable_typed_and_maps_every_read_to_the_original_row() {
        let grid = GridState::new(MockSource::new(
            vec!["id", "score"],
            vec![
                vec![Some("a".into()), Some("10".into())],
                vec![Some("b".into()), None],
                vec![Some("c".into()), Some("2".into())],
                vec![Some("d".into()), Some("2".into())],
            ],
        ));
        let mut sorted = grid.clone();
        assert_eq!(sorted.page_sort(), None);
        sorted.sort_page(1, SortKind::Integer, false).unwrap();
        assert_eq!(sorted.page_sort(), Some((1, false)));
        assert_eq!(grid.page_sort(), None, "the clone's sort stays its own");
        assert_eq!(
            (0..4).map(|r| sorted.source_row(r)).collect::<Vec<_>>(),
            [2, 3, 0, 1]
        );
        assert_eq!(sorted.row_values(0)[0].1, "c");
        assert_eq!(
            sorted
                .full_cell_text(CellPosition { row: 2, column: 0 })
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            sorted
                .cell_text(CellPosition { row: 3, column: 0 })
                .as_deref(),
            Some("b")
        );
        sorted.sort_page(1, SortKind::Integer, true).unwrap();
        assert_eq!(sorted.page_sort(), Some((1, true)));
        // NULL is the greatest value: first when descending.
        assert_eq!(
            (0..4).map(|r| sorted.source_row(r)).collect::<Vec<_>>(),
            [1, 0, 2, 3]
        );
        sorted.clear_page_sort();
        assert_eq!(sorted.page_sort(), None);
        assert_eq!(
            (0..4).map(|r| sorted.source_row(r)).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(
            grid.source_row(0),
            0,
            "cloning must not mutate the source page"
        );
    }

    #[test]
    fn decimal_sort_does_not_round_distinct_database_values_to_the_same_float() {
        let mut grid = GridState::new(MockSource::new(
            vec!["amount"],
            vec![
                vec![Some("12345678901234567891.25".into())],
                vec![Some("12345678901234567890.25".into())],
                vec![Some("-0.1".into())],
            ],
        ));
        grid.sort_page(0, SortKind::Number, false).unwrap();
        assert_eq!(
            (0..3).map(|row| grid.source_row(row)).collect::<Vec<_>>(),
            [2, 1, 0]
        );
    }

    /// The source rows of one column of `values` in sorted order.
    fn sorted_rows(kind: SortKind, values: &[Option<&str>], descending: bool) -> Vec<usize> {
        let mut grid = GridState::new(MockSource::new(
            vec!["v"],
            values
                .iter()
                .map(|value| vec![value.map(str::to_string)])
                .collect(),
        ));
        grid.sort_page(0, kind, descending).unwrap();
        (0..values.len()).map(|row| grid.source_row(row)).collect()
    }

    #[test]
    fn decimal_text_sorts_as_numbers_not_as_text() {
        let values = [Some("9"), Some("10"), Some("100.5")];
        assert_eq!(sorted_rows(SortKind::Number, &values, false), [0, 1, 2]);
        assert_eq!(sorted_rows(SortKind::Number, &values, true), [2, 1, 0]);
        // The text order the old label-based sort produced.
        assert_eq!(sorted_rows(SortKind::Text, &values, false), [1, 2, 0]);
        let mixed = [Some("-0.5"), Some("-10.25"), Some("0.05"), Some("0.5"), Some("-0")];
        assert_eq!(sorted_rows(SortKind::Number, &mixed, false), [1, 0, 4, 2, 3]);
        let equal = [Some("1.50"), Some("1.5"), Some("01.5000")];
        assert_eq!(sorted_rows(SortKind::Number, &equal, false), [0, 1, 2], "equal keys keep page order");
    }

    #[test]
    fn integers_of_any_width_sort_exactly() {
        // Above 2^53 two neighbours are one f64.
        let values = [
            Some("9007199254740993"),
            Some("9007199254740992"),
            Some("18446744073709551615"),
            Some("9007199254740994"),
        ];
        assert_eq!(sorted_rows(SortKind::Integer, &values, false), [1, 0, 3, 2]);
        assert_eq!(sorted_rows(SortKind::Number, &values, false), [1, 0, 3, 2]);
        // Beyond i128 (Int256) the column is read as exact decimals instead.
        let wide = [
            Some("-57896044618658097711785492504343953926634992332820282019728792003956564819968"),
            Some("57896044618658097711785492504343953926634992332820282019728792003956564819967"),
            Some("170141183460469231731687303715884105728"),
        ];
        assert_eq!(sorted_rows(SortKind::Integer, &wide, false), [0, 2, 1]);
    }

    #[test]
    fn floats_order_nan_and_infinities_deterministically() {
        let values = [
            Some("NaN"),
            Some("1"),
            Some("Infinity"),
            Some("-Infinity"),
            Some("-1.5e3"),
            Some("1E2"),
            Some("inf"),
            Some("nan"),
        ];
        // -Infinity < finite < Infinity < NaN, equal keys in page order.
        assert_eq!(
            sorted_rows(SortKind::Number, &values, false),
            [3, 4, 1, 5, 2, 6, 0, 7]
        );
        assert_eq!(
            sorted_rows(SortKind::Number, &values, true),
            [0, 7, 2, 6, 5, 1, 4, 3]
        );
        let exponents = [Some("1e300"), Some("9.9e299"), Some("1e-300"), Some("0")];
        assert_eq!(sorted_rows(SortKind::Number, &exponents, false), [3, 2, 1, 0]);
    }

    #[test]
    fn null_is_the_greatest_value_in_both_directions() {
        let values = [Some("b"), None, Some("a"), None];
        assert_eq!(sorted_rows(SortKind::Text, &values, false), [2, 0, 1, 3]);
        assert_eq!(sorted_rows(SortKind::Text, &values, true), [1, 3, 0, 2]);
        let numbers = [None, Some("2"), Some("1")];
        assert_eq!(sorted_rows(SortKind::Integer, &numbers, false), [2, 1, 0]);
        assert_eq!(sorted_rows(SortKind::Integer, &numbers, true), [0, 1, 2]);
    }

    #[test]
    fn text_keeps_its_byte_order_and_odd_values_fall_back_to_it() {
        let words = [Some("b"), Some("B"), Some("a"), Some("A")];
        assert_eq!(sorted_rows(SortKind::Text, &words, false), [3, 1, 2, 0]);
        // One value that is not a number moves the whole column to text order.
        let odd = [Some("10"), Some("9"), Some("n/a")];
        assert_eq!(sorted_rows(SortKind::Integer, &odd, false), [0, 1, 2]);
        assert_eq!(sorted_rows(SortKind::Number, &odd, false), [0, 1, 2]);
    }

    #[test]
    fn dates_and_booleans_sort_by_value() {
        let dates = [
            Some("2024-01-02 03:04:05.5"),
            Some("0999-12-31"),
            Some("10000-01-01"),
            Some("0044-03-15 BC 12:00:00"),
            Some("2024-01-02 03:04:05"),
        ];
        assert_eq!(sorted_rows(SortKind::Temporal, &dates, false), [3, 1, 4, 0, 2]);
        let flags = [Some("true"), Some("f"), None, Some("false"), Some("1")];
        assert_eq!(sorted_rows(SortKind::Boolean, &flags, false), [1, 3, 0, 4, 2]);
    }

    #[test]
    fn timestamps_with_an_offset_sort_by_instant_zone_less_ones_by_wall_time() {
        // 2024-10-27 is the night Europe/Paris falls back: 02:30+02 (00:30Z)
        // happens BEFORE 02:15+01 (01:15Z) although its wall time is later.
        let paris = [
            Some("2024-10-27 02:15:00+01"),
            Some("2024-10-27 02:30:00+02"),
            Some("2024-10-27 03:00:00+01:00"),
            Some("2024-10-27 01:59:59.5+02"),
            Some("2024-10-27 01:15:00 UTC"),
            Some("2024-10-27T00:30:00Z"),
        ];
        // Instants: 01:15Z, 00:30Z, 02:00Z, 23:59:59.5Z (26th), 01:15Z, 00:30Z.
        assert_eq!(sorted_rows(SortKind::Temporal, &paris, false), [3, 1, 5, 0, 4, 2]);
        assert_eq!(sorted_rows(SortKind::Temporal, &paris, true), [2, 0, 4, 1, 5, 3]);
        // Without an offset the wall time decides.
        let wall = [
            Some("2024-10-27 02:30:00"),
            Some("2024-10-27 02:15:00"),
            Some("2024-10-26 23:00:00"),
        ];
        assert_eq!(sorted_rows(SortKind::Temporal, &wall, false), [2, 1, 0]);
        // A column mixing the two is ordered by wall time throughout.
        let mixed = [Some("2024-10-27 02:30:00+02"), Some("2024-10-27 02:15:00")];
        assert_eq!(sorted_rows(SortKind::Temporal, &mixed, false), [1, 0]);
    }

    #[test]
    fn range_containment_is_rectangular_not_row_major() {
        let range = CellRange {
            start: CellPosition { row: 1, column: 6 },
            end: CellPosition { row: 6, column: 6 },
        };

        // A single-column, multi-row range must not spill into the columns
        // around it on the rows in between (the old row-major comparison did).
        for row in 1..=6 {
            assert!(range.contains(CellPosition { row, column: 6 }));
            assert!(!range.contains(CellPosition { row, column: 5 }));
            assert!(!range.contains(CellPosition { row, column: 7 }));
        }
        assert!(!range.contains(CellPosition { row: 0, column: 6 }));
        assert!(!range.contains(CellPosition { row: 7, column: 6 }));
        // Reversed (bottom-right to top-left) ranges normalize first.
        let reversed = CellRange {
            start: range.end,
            end: range.start,
        };
        assert!(reversed.contains(CellPosition { row: 3, column: 6 }));
    }

    #[test]
    fn right_click_preserves_a_range_only_when_the_hit_is_inside_it() {
        let mut selected = GridSelection::default();
        selected.select(CellPosition { row: 1, column: 0 });
        selected.extend_to(CellPosition { row: 3, column: 2 });
        let range = selected.range;
        selected.context_target(CellPosition { row: 2, column: 1 });
        assert_eq!(selected.range, range);
        assert_eq!(selected.anchor, Some(CellPosition { row: 1, column: 0 }));
        selected.context_target(CellPosition { row: 2, column: 3 });
        assert_eq!(
            selected.range.unwrap().normalized().start,
            CellPosition { row: 2, column: 3 }
        );
    }

    #[test]
    fn tsv_copy_works_against_a_non_arrow_source() {
        let source = MockSource::new(
            vec!["id", "name"],
            vec![
                vec![Some("1".into()), Some("Ada".into())],
                vec![Some("2".into()), None],
            ],
        );
        let grid = GridState::new(source);

        let mut selection = GridSelection::default();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 1, column: 1 });

        assert_eq!(selection_as_tsv(&grid, &selection), "1\tAda\n2\tNULL");
    }

    #[test]
    fn row_values_delegate_to_the_source_for_detail_views() {
        let source = MockSource::new(
            vec!["id", "name"],
            vec![vec![Some("7".into()), Some("Ada".into())]],
        );
        let grid = GridState::new(source);

        assert_eq!(
            grid.row_values(0),
            vec![("id".into(), "7".into()), ("name".into(), "Ada".into())]
        );
        assert!(grid.row_values(1).is_empty());
    }

    #[test]
    fn normalizes_ranges_for_reverse_drag_and_shift_selection() {
        let range = CellRange {
            start: CellPosition { row: 8, column: 5 },
            end: CellPosition { row: 2, column: 1 },
        };

        assert_eq!(
            range.normalized(),
            CellRange {
                start: CellPosition { row: 2, column: 1 },
                end: CellPosition { row: 8, column: 5 },
            }
        );
    }

    #[test]
    fn column_layout_clamps_resizes_and_tracks_offsets() {
        let mut layout = ColumnLayout::new(3);

        layout.set_width(1, 30.0);
        assert_eq!(layout.widths[1], 60.0, "clamped to min");

        layout.set_width(1, 900.0);
        assert_eq!(layout.widths[1], 480.0, "clamped to max");

        layout.set_width(2, 100.0);
        assert_eq!(layout.offset(2), 160.0 + 480.0);
        assert_eq!(layout.content_width(), 48.0 + 160.0 + 480.0 + 100.0);

        // Out-of-range columns are ignored, not panics.
        layout.set_width(9, 50.0);
        assert_eq!(layout.widths.len(), 3);
    }

    #[test]
    fn column_layout_resizes_from_drag_delta() {
        let mut layout = ColumnLayout::new(1);

        layout.resize_by(0, 160.0, 42.0);
        assert_eq!(layout.widths[0], 202.0);

        layout.resize_by(0, 202.0, -300.0);
        assert_eq!(layout.widths[0], 60.0);
    }

    #[test]
    fn selection_extends_from_the_anchor_and_normalizes() {
        let mut selection = GridSelection::default();

        selection.select(CellPosition { row: 3, column: 2 });
        selection.extend_to(CellPosition { row: 1, column: 0 });

        let cells = selection.selected_cells();
        assert_eq!(cells.len(), 9, "3 rows x 3 columns");
        assert_eq!(cells[0], CellPosition { row: 1, column: 0 });
        assert_eq!(
            cells.last(),
            Some(&CellPosition { row: 3, column: 2 }),
            "cells iterate row-major"
        );
    }

    #[test]
    fn tsv_copy_with_no_selection_is_empty() {
        let source = MockSource::new(vec![], vec![]);
        let grid = GridState::new(source);
        assert_eq!(selection_as_tsv(&grid, &GridSelection::default()), "");
    }

    #[test]
    fn column_window_culls_to_viewport_with_overscan() {
        let layout = ColumnLayout::new(100); // 48 gutter + 100 x 160
        let top = layout.column_window(0.0, 800.0, None, 0);
        // 800px shows columns 0..=4 (48 + 5*160 = 848); +2 overscan.
        assert_eq!(top.segments[0].columns, 0..7);
        assert_eq!(top.segments[0].spacer, 0.0);
        assert!(top.segments[1].columns.is_empty());
        assert_eq!(top.total_width, layout.content_width());

        let right = layout.content_width() - 800.0;
        let end = layout.column_window(right, 800.0, None, 0);
        assert_eq!(end.segments[0].columns.end, 100);
        assert!(end.segments[0].columns.start > 90);
        assert_eq!(
            end.segments[0].spacer,
            layout.offset(end.segments[0].columns.start)
        );
        assert!(!end.contains(0));
    }

    #[test]
    fn column_window_pins_an_offscreen_column_in_its_own_segment() {
        let layout = ColumnLayout::new(100);
        let window = layout.column_window(layout.content_width() - 800.0, 800.0, Some(3), 0);
        assert_eq!(window.segments[0].columns, 3..4);
        assert_eq!(window.segments[0].spacer, layout.offset(3));
        let main = &window.segments[1];
        assert_eq!(
            main.spacer,
            layout.offset(main.columns.start) - layout.offset(4)
        );
        assert!(window.contains(3) && window.contains(99) && !window.contains(4));
        // Adjacent pins extend the main run instead of splitting it.
        let adjacent = layout.column_window(0.0, 800.0, Some(7), 0);
        assert_eq!(adjacent.segments[0].columns, 0..8);
        assert!(adjacent.segments[1].columns.is_empty());
    }

    #[test]
    fn frozen_columns_leave_the_window_to_the_sticky_block() {
        let layout = ColumnLayout::new(100);
        // Two frozen columns cover 48 + 320 px of an 800 px viewport: the scrolling
        // columns start right after them and the window never holds a frozen one.
        let top = layout.column_window(0.0, 800.0, None, 2);
        assert_eq!(top.segments[0].columns.start, 2);
        assert_eq!(top.segments[0].spacer, 0.0, "the spacer starts after the sticky block");
        assert!(!top.contains(0) && !top.contains(1));
        // Scrolled far right, the frozen columns stay out and the spacer skips them.
        let scrolled = layout.content_width() - 800.0;
        let end = layout.column_window(scrolled, 800.0, None, 2);
        let first = end.segments[0].columns.start;
        assert!(first > 90 && !end.contains(0));
        assert_eq!(end.segments[0].spacer, layout.offset(first) - layout.leading_width(2));
        assert_eq!(end.total_width, layout.content_width());
        // The block hides what scrolled under it: column 6 starts at 48 + 6*160 = 1008.
        let mid = layout.column_window(900.0, 800.0, None, 2);
        assert!(mid.segments[0].columns.start <= 6 && mid.contains(6));
        assert_eq!(layout.leading_width(2), 320.0);
        // Everything frozen leaves nothing to scroll.
        assert!(layout.column_window(0.0, 800.0, None, 100).segments[0].columns.is_empty());
        // The editing column keeps its element state, frozen ones need none.
        let editing = layout.column_window(scrolled, 800.0, Some(5), 2);
        assert!(editing.contains(5) && editing.contains(99));
        assert!(layout.column_window(0.0, 800.0, Some(1), 2).iter().all(|segment| segment.columns.start >= 2));
    }

    #[test]
    fn column_at_resolves_content_space_x() {
        let layout = ColumnLayout::new(3);
        assert_eq!(layout.column_at(10.0), None, "gutter");
        assert_eq!(layout.column_at(48.0), Some(0));
        assert_eq!(layout.column_at(48.0 + 160.0), Some(1));
        assert_eq!(layout.column_at(48.0 + 3.0 * 160.0 - 0.5), Some(2));
        assert_eq!(layout.column_at(48.0 + 3.0 * 160.0), None);
    }
}
