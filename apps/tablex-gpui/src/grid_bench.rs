//! Allocation-churn A/B bench: legacy per-cell `to_string()` vs lazy typed
//! Arrow source. Run with:
//!
//! ```text
//! cargo test --release -p tablex-gpui grid_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Uses a counting global allocator (test-only) to measure allocs/bytes/in-flight.
//! Wall-time via `Instant` (5 reps, median + worst). No perf asserts — prints
//! an A/B table + verdict. Counting-allocator atomic overhead inflates absolute
//! times equally on both sides, so deltas remain valid.

#![cfg(test)]

use crate::grid::{CellPosition, GridSelection, GridSource, GridState};
use crate::grid_arrow::{array_value_to_string, ArrowGridSource, RowMap};
use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use gpui::SharedString;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

// ── counting allocator ────────────────────────────────────────────────

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES_ALLOC: AtomicU64 = AtomicU64::new(0);
static BYTES_FREE: AtomicU64 = AtomicU64::new(0);
static DEALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    // A `Cell` with no destructor: reading it never allocates, so the allocator can count through it.
    static THREAD_ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn allocation_count() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

/// Allocations made on the calling thread: a test that measures single-threaded work is not counted for the tests beside it.
pub(crate) fn thread_allocation_count() -> u64 {
    THREAD_ALLOCS.with(std::cell::Cell::get)
}

/// Cumulative bytes handed out by the counting allocator.
pub(crate) fn allocation_bytes() -> u64 {
    BYTES_ALLOC.load(Ordering::Relaxed)
}

/// Bytes handed out and not freed yet (retained memory of a section under measurement).
pub(crate) fn live_bytes() -> u64 {
    in_flight()
}

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            let _ = THREAD_ALLOCS.try_with(|count| count.set(count.get() + 1));
            BYTES_ALLOC.fetch_add(l.size() as u64, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        DEALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES_FREE.fetch_add(l.size() as u64, Ordering::Relaxed);
        System.dealloc(p, l);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let q = System.realloc(p, l, new_size);
        if !q.is_null() {
            REALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES_ALLOC.fetch_add(new_size as u64, Ordering::Relaxed);
            BYTES_FREE.fetch_add(l.size() as u64, Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[derive(Clone, Copy, Default)]
struct Snap {
    allocs: u64,
    bytes_alloc: u64,
    bytes_free: u64,
    deallocs: u64,
    reallocs: u64,
}

fn snap() -> Snap {
    Snap {
        allocs: ALLOCS.load(Ordering::Relaxed),
        bytes_alloc: BYTES_ALLOC.load(Ordering::Relaxed),
        bytes_free: BYTES_FREE.load(Ordering::Relaxed),
        deallocs: DEALLOCS.load(Ordering::Relaxed),
        reallocs: REALLOCS.load(Ordering::Relaxed),
    }
}

fn delta(before: Snap, after: Snap) -> Snap {
    Snap {
        allocs: after.allocs.saturating_sub(before.allocs),
        bytes_alloc: after.bytes_alloc.saturating_sub(before.bytes_alloc),
        bytes_free: after.bytes_free.saturating_sub(before.bytes_free),
        deallocs: after.deallocs.saturating_sub(before.deallocs),
        reallocs: after.reallocs.saturating_sub(before.reallocs),
    }
}

fn in_flight() -> u64 {
    BYTES_ALLOC
        .load(Ordering::Relaxed)
        .saturating_sub(BYTES_FREE.load(Ordering::Relaxed))
}

// ── legacy source (verbatim old implementation) ──────────────────────

struct LegacySource {
    batches: Vec<Arc<RecordBatch>>,
    rows: RowMap,
}

impl GridSource for LegacySource {
    fn row_count(&self) -> usize {
        self.rows.total_rows()
    }
    fn column_count(&self) -> usize {
        self.batches.first().map(|b| b.num_columns()).unwrap_or(0)
    }
    fn column_name(&self, col: usize) -> Option<&str> {
        self.batches
            .first()
            .and_then(|b| b.schema_ref().fields().get(col))
            .map(|f| f.name().as_str())
    }
    fn cell_text(&self, position: CellPosition) -> Option<SharedString> {
        let location = self.rows.locate(position.row)?;
        let batch = self.batches.get(location.batch)?;
        if position.column >= batch.num_columns() {
            return None;
        }
        let array = batch.column(position.column);
        if array.is_null(location.row) {
            return Some(SharedString::from("NULL"));
        }
        Some(SharedString::from(array_value_to_string(
            array,
            location.row,
        )))
    }
    fn row_values(&self, row: usize) -> Vec<(String, String)> {
        (0..self.column_count())
            .filter_map(|col| {
                Some((
                    self.column_name(col)?.to_string(),
                    self.cell_text(CellPosition { row, column: col })?
                        .to_string(),
                ))
            })
            .collect()
    }
}

// ── data builders ─────────────────────────────────────────────────────

fn mixed_schema_batch(rows: usize, cols: usize) -> RecordBatch {
    let fields: Vec<_> = (0..cols)
        .map(|c| match c % 4 {
            0 => Field::new(format!("i{c}"), DataType::Int64, true),
            1 => Field::new(format!("s{c}"), DataType::Utf8, true),
            2 => Field::new(format!("f{c}"), DataType::Float64, true),
            _ => Field::new(format!("b{c}"), DataType::Boolean, true),
        })
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let arrays: Vec<ArrayRef> = (0..cols)
        .map(|c| match c % 4 {
            0 => {
                let v: Vec<Option<i64>> = (0..rows).map(|r| Some(r as i64)).collect();
                Arc::new(Int64Array::from(v)) as ArrayRef
            }
            1 => {
                let v: Vec<Option<&str>> = (0..rows)
                    .map(|r| Some(if r % 10 == 0 { "" } else { "val" }))
                    .collect();
                Arc::new(StringArray::from(v)) as ArrayRef
            }
            2 => {
                let v: Vec<Option<f64>> = (0..rows).map(|r| Some(r as f64 * 1.5)).collect();
                Arc::new(Float64Array::from(v)) as ArrayRef
            }
            _ => {
                let v: Vec<Option<bool>> = (0..rows).map(|r| Some(r % 2 == 0)).collect();
                Arc::new(BooleanArray::from(v)) as ArrayRef
            }
        })
        .collect();
    RecordBatch::try_new(schema, arrays).expect("valid mixed batch")
}

fn huge_batch(rows: usize, cell_bytes: usize) -> RecordBatch {
    let huge = "z".repeat(cell_bytes);
    let schema = Arc::new(Schema::new(vec![Field::new("blob", DataType::Utf8, false)]));
    let arr = Arc::new(StringArray::from(vec![huge.as_str(); rows]));
    RecordBatch::try_new(schema, vec![arr as ArrayRef]).expect("valid huge batch")
}

fn string_batch(rows: usize, cols: usize, val: &str) -> RecordBatch {
    let fields: Vec<_> = (0..cols)
        .map(|c| Field::new(format!("c{c}"), DataType::Utf8, true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let arrays: Vec<ArrayRef> = (0..cols)
        .map(|_| {
            let v: Vec<Option<&str>> = (0..rows).map(|_| Some(val)).collect();
            Arc::new(StringArray::from(v)) as ArrayRef
        })
        .collect();
    RecordBatch::try_new(schema, arrays).expect("valid string batch")
}

// ── timing helper ─────────────────────────────────────────────────────

fn timed<F: FnMut()>(reps: usize, mut f: F) -> (u128, u128) {
    // warmup
    f();
    let mut samples = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        f();
        samples.push(t.elapsed().as_nanos());
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    let worst = *samples.last().unwrap_or(&0);
    (median, worst)
}

// ── scroll churn helper ───────────────────────────────────────────────

fn scroll_churn<S: GridSource>(grid: &GridState<S>, frames: usize, vrows: usize, vcols: usize) {
    let total_rows = grid.row_count();
    let total_cols = grid.column_count();
    for _ in 0..frames {
        for r in 0..vrows.min(total_rows) {
            for c in 0..vcols.min(total_cols) {
                let _ = grid.cell_text(CellPosition { row: r, column: c });
            }
        }
        let _ = total_rows; // suppress unused
    }
}

// ── pretty-print ──────────────────────────────────────────────────────

fn header(title: &str) {
    eprintln!("\n═══ {title} ═══");
}

fn row(label: &str, legacy: impl std::fmt::Display, new: impl std::fmt::Display) {
    eprintln!("  {label:<28} legacy={legacy:<16} new={new}");
}

fn fmt_ns(ns: u128) -> String {
    if ns > 1_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else if ns > 1_000 {
        format!("{:.1} µs", ns as f64 / 1_000.0)
    } else {
        format!("{ns} ns")
    }
}

fn fmt_bytes(b: u64) -> String {
    if b > 1_000_000 {
        format!("{:.1} MB", b as f64 / 1_000_000.0)
    } else if b > 1_000 {
        format!("{:.1} KB", b as f64 / 1_000.0)
    } else {
        format!("{b} B")
    }
}

// ── scenarios ─────────────────────────────────────────────────────────

#[test]
#[ignore]
fn bench_sorted_page_scroll_churn() {
    let batch = mixed_schema_batch(200, 20);
    let mut sorted = GridState::new(ArrowGridSource::new(vec![Arc::new(batch)]));
    let unsorted = sorted.clone();
    let start = Instant::now();
    sorted.sort_page(0, crate::grid::SortKind::Integer, true).expect("sortable page");
    eprintln!("200-row sort time: {:?}", start.elapsed());
    scroll_churn(&unsorted, 1, 30, 20);
    scroll_churn(&sorted, 1, 30, 20);
    let before = snap();
    scroll_churn(&unsorted, 60, 30, 20);
    let unsorted_allocs = delta(before, snap()).allocs;
    let before = snap();
    scroll_churn(&sorted, 60, 30, 20);
    let sorted_allocs = delta(before, snap()).allocs;
    let (unsorted_median, _) = timed(5, || scroll_churn(&unsorted, 60, 30, 20));
    let (sorted_median, _) = timed(5, || scroll_churn(&sorted, 60, 30, 20));
    eprintln!("60-frame scroll: unsorted {unsorted_allocs} allocs / {unsorted_median} ns median; sorted {sorted_allocs} allocs / {sorted_median} ns median");
}

#[test]
#[ignore]
fn bench_scroll_churn_mixed() {
    header("Scroll churn: 200×20 mixed-schema, 60 frames × 30×20 viewport");
    let batch = mixed_schema_batch(200, 20);

    let legacy = GridState::new(LegacySource {
        batches: vec![Arc::new(batch.clone())],
        rows: RowMap::from_batches(&[Arc::new(batch.clone())]),
    });
    let new = GridState::new(ArrowGridSource::new(vec![Arc::new(batch)]));

    let b0 = snap();
    scroll_churn(&legacy, 60, 30, 20);
    let b1 = snap();
    let ld = delta(b0, b1);

    let b2 = snap();
    scroll_churn(&new, 60, 30, 20);
    let b3 = snap();
    let nd = delta(b2, b3);

    let (l_med, l_worst) = timed(5, || scroll_churn(&legacy, 60, 30, 20));
    let (n_med, n_worst) = timed(5, || scroll_churn(&new, 60, 30, 20));

    row("allocs", ld.allocs, nd.allocs);
    row(
        "bytes alloc",
        fmt_bytes(ld.bytes_alloc),
        fmt_bytes(nd.bytes_alloc),
    );
    row("deallocs", ld.deallocs, nd.deallocs);
    row("reallocs", ld.reallocs, nd.reallocs);
    row("time median", fmt_ns(l_med), fmt_ns(n_med));
    row("time worst", fmt_ns(l_worst), fmt_ns(n_worst));
    let ratio = if nd.allocs > 0 {
        ld.allocs as f64 / nd.allocs as f64
    } else {
        f64::INFINITY
    };
    eprintln!("  → churn reduction: {ratio:.0}×");
}

#[test]
#[ignore]
fn bench_scroll_scale_rows() {
    header("Scroll scale by row count: 200 vs 10k rows, 60 frames × 30×20");
    let small = mixed_schema_batch(200, 20);
    let big = mixed_schema_batch(10_000, 20);

    let legacy_small = GridState::new(LegacySource {
        batches: vec![Arc::new(small.clone())],
        rows: RowMap::from_batches(&[Arc::new(small.clone())]),
    });
    let legacy_big = GridState::new(LegacySource {
        batches: vec![Arc::new(big.clone())],
        rows: RowMap::from_batches(&[Arc::new(big.clone())]),
    });
    let new_small = GridState::new(ArrowGridSource::new(vec![Arc::new(small)]));
    let new_big = GridState::new(ArrowGridSource::new(vec![Arc::new(big)]));

    let b0 = snap();
    scroll_churn(&legacy_small, 60, 30, 20);
    let b1 = snap();
    let ls = delta(b0, b1);

    let b2 = snap();
    scroll_churn(&legacy_big, 60, 30, 20);
    let b3 = snap();
    let lb = delta(b2, b3);

    let b4 = snap();
    scroll_churn(&new_small, 60, 30, 20);
    let b5 = snap();
    let ns = delta(b4, b5);

    let b6 = snap();
    scroll_churn(&new_big, 60, 30, 20);
    let b7 = snap();
    let nb = delta(b6, b7);

    eprintln!("  legacy  200 rows: {} allocs", ls.allocs);
    eprintln!("  legacy  10k rows: {} allocs", lb.allocs);
    eprintln!("  new     200 rows: {} allocs", ns.allocs);
    eprintln!("  new     10k rows: {} allocs", nb.allocs);
    eprintln!("  → per-frame cost must NOT grow with total rows (virtualization guard)");
}

#[test]
#[ignore]
fn bench_scroll_scale_cols() {
    header("Scroll scale by column count: 20 vs 200 cols, 60 frames × 30 rows");
    let narrow = mixed_schema_batch(200, 20);
    let wide = mixed_schema_batch(200, 200);

    let legacy_narrow = GridState::new(LegacySource {
        batches: vec![Arc::new(narrow.clone())],
        rows: RowMap::from_batches(&[Arc::new(narrow.clone())]),
    });
    let legacy_wide = GridState::new(LegacySource {
        batches: vec![Arc::new(wide.clone())],
        rows: RowMap::from_batches(&[Arc::new(wide.clone())]),
    });
    let new_narrow = GridState::new(ArrowGridSource::new(vec![Arc::new(narrow)]));
    let new_wide = GridState::new(ArrowGridSource::new(vec![Arc::new(wide)]));

    let b0 = snap();
    scroll_churn(&legacy_narrow, 60, 30, 20);
    let b1 = snap();
    let ln = delta(b0, b1);

    let b2 = snap();
    scroll_churn(&legacy_wide, 60, 30, 200);
    let b3 = snap();
    let lw = delta(b2, b3);

    let b4 = snap();
    scroll_churn(&new_narrow, 60, 30, 20);
    let b5 = snap();
    let nn = delta(b4, b5);

    let b6 = snap();
    scroll_churn(&new_wide, 60, 30, 200);
    let b7 = snap();
    let nw = delta(b6, b7);

    eprintln!("  legacy  20 cols: {} allocs", ln.allocs);
    eprintln!("  legacy  200 cols: {} allocs", lw.allocs);
    eprintln!("  new     20 cols: {} allocs", nn.allocs);
    eprintln!("  new     200 cols: {} allocs", nw.allocs);
    eprintln!("  → both scale with cols (no horizontal culling yet) — evidence for future work");
}

#[test]
#[ignore]
fn bench_construct_retained() {
    header("Construct retained: 200×20 × ~4-byte strings");
    let batch = string_batch(200, 20, "val");

    let b0 = snap();
    let i0 = in_flight();
    let legacy = LegacySource {
        batches: vec![Arc::new(batch.clone())],
        rows: RowMap::from_batches(&[Arc::new(batch.clone())]),
    };
    let b1 = snap();
    let i1 = in_flight();
    let ld = delta(b0, b1);
    let legacy_retained = i1.saturating_sub(i0);
    drop(legacy);

    let b2 = snap();
    let i2 = in_flight();
    let new = ArrowGridSource::new(vec![Arc::new(batch)]);
    let b3 = snap();
    let i3 = in_flight();
    let nd = delta(b2, b3);
    let new_retained = i3.saturating_sub(i2);
    drop(new);

    row("construct allocs", ld.allocs, nd.allocs);
    row(
        "construct bytes",
        fmt_bytes(ld.bytes_alloc),
        fmt_bytes(nd.bytes_alloc),
    );
    row(
        "retained bytes",
        fmt_bytes(legacy_retained),
        fmt_bytes(new_retained),
    );
    let ratio = new_retained as f64 / legacy_retained.max(1) as f64;
    eprintln!("  → retained ratio: {ratio:.1}× (trade-off; flag if >5×)");
}

#[test]
#[ignore]
fn bench_huge_cells_scroll() {
    header("Huge cells scroll: 20 rows × 1MB, 10 frames × 20×1 viewport");
    let batch = huge_batch(20, 1_000_000);

    let legacy = GridState::new(LegacySource {
        batches: vec![Arc::new(batch.clone())],
        rows: RowMap::from_batches(&[Arc::new(batch.clone())]),
    });
    let new = GridState::new(ArrowGridSource::new(vec![Arc::new(batch)]));

    let b0 = snap();
    scroll_churn(&legacy, 10, 20, 1);
    let b1 = snap();
    let ld = delta(b0, b1);

    let b2 = snap();
    scroll_churn(&new, 10, 20, 1);
    let b3 = snap();
    let nd = delta(b2, b3);

    row("allocs", ld.allocs, nd.allocs);
    row(
        "bytes alloc",
        fmt_bytes(ld.bytes_alloc),
        fmt_bytes(nd.bytes_alloc),
    );
    row("deallocs", ld.deallocs, nd.deallocs);
    let ratio = if nd.bytes_alloc > 0 {
        ld.bytes_alloc as f64 / nd.bytes_alloc as f64
    } else {
        f64::INFINITY
    };
    eprintln!("  → bytes-churn reduction: {ratio:.0}×");
}

#[test]
#[ignore]
fn bench_huge_cells_retained() {
    header("Huge cells retained: 20 rows × 1MB");
    let batch = huge_batch(20, 1_000_000);

    let i0 = in_flight();
    let legacy = LegacySource {
        batches: vec![Arc::new(batch.clone())],
        rows: RowMap::from_batches(&[Arc::new(batch.clone())]),
    };
    let legacy_retained = in_flight().saturating_sub(i0);
    drop(legacy);

    let i1 = in_flight();
    let new = ArrowGridSource::new(vec![Arc::new(batch)]);
    let new_retained = in_flight().saturating_sub(i1);
    drop(new);

    row(
        "retained bytes",
        fmt_bytes(legacy_retained),
        fmt_bytes(new_retained),
    );
    let ratio = new_retained as f64 / legacy_retained.max(1) as f64;
    eprintln!("  → retained ratio: {ratio:.2}× (expect ~parity — typed batches keep originals)");
}

#[test]
#[ignore]
fn bench_tsv_copy() {
    header("TSV copy: 100×20 selection, mixed schema");
    let batch = mixed_schema_batch(100, 20);

    let legacy = GridState::new(LegacySource {
        batches: vec![Arc::new(batch.clone())],
        rows: RowMap::from_batches(&[Arc::new(batch.clone())]),
    });
    let new = GridState::new(ArrowGridSource::new(vec![Arc::new(batch)]));

    let mut sel = GridSelection::default();
    sel.select(CellPosition { row: 0, column: 0 });
    sel.extend_to(CellPosition {
        row: 99,
        column: 19,
    });

    let b0 = snap();
    let _legacy_tsv = crate::grid::selection_as_tsv(&legacy, &sel);
    let b1 = snap();
    let ld = delta(b0, b1);

    let b2 = snap();
    let _new_tsv = crate::grid::selection_as_tsv(&new, &sel);
    let b3 = snap();
    let nd = delta(b2, b3);

    let (l_med, _) = timed(5, || {
        crate::grid::selection_as_tsv(&legacy, &sel);
    });
    let (n_med, _) = timed(5, || {
        crate::grid::selection_as_tsv(&new, &sel);
    });

    row("allocs", ld.allocs, nd.allocs);
    row(
        "bytes alloc",
        fmt_bytes(ld.bytes_alloc),
        fmt_bytes(nd.bytes_alloc),
    );
    row("time median", fmt_ns(l_med), fmt_ns(n_med));
    eprintln!("  → both build a final String; delta expected to be modest");
}
