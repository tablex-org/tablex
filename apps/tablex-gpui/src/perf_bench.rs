//! Shared measuring helpers of the on-demand structure-editor benchmarks
//! (`app/structure_edit/bench.rs`, `app/tests/properties_bench_context.rs`,
//! `app/tests/properties_perf.rs`). Measurements only: nothing here asserts a number.
//!
//! The counting allocator is the one of `grid_bench` (a global test allocator): its atomics inflate
//! absolute times a little on both sides of an A/B, so compare runs of the same build, not
//! against a build without the counter. Every result is also printed as one machine-readable
//! `PERF {json}` line, which `scripts/perf_properties.py` collects.

#![cfg(test)]

use crate::grid_bench::{allocation_bytes, allocation_count};
use std::time::{Duration, Instant};

/// A point in time and in the allocator's counters.
#[derive(Clone, Copy)]
pub(crate) struct Mark {
    at: Instant,
    allocations: u64,
    bytes: u64,
}

pub(crate) fn mark() -> Mark {
    Mark { at: Instant::now(), allocations: allocation_count(), bytes: allocation_bytes() }
}

/// What ran between a [`Mark`] and now.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cost {
    pub(crate) elapsed: Duration,
    pub(crate) allocations: u64,
    /// Bytes handed out (churn), freed or not.
    pub(crate) bytes: u64,
}

impl Mark {
    pub(crate) fn since(self) -> Cost {
        let elapsed = self.at.elapsed();
        Cost {
            elapsed,
            allocations: allocation_count().saturating_sub(self.allocations),
            bytes: allocation_bytes().saturating_sub(self.bytes),
        }
    }
}

#[cfg(feature = "bench")]
impl Cost {
    pub(crate) fn ms(&self) -> f64 {
        self.elapsed.as_secs_f64() * 1000.0
    }
}

/// Runs `work` once and returns its result with what it cost.
pub(crate) fn measure<R>(work: impl FnOnce() -> R) -> (R, Cost) {
    let started = mark();
    let result = work();
    (result, started.since())
}

/// Many runs of one operation, summarised.
pub(crate) struct Stats {
    pub(crate) runs: usize,
    pub(crate) median_us: f64,
    pub(crate) min_us: f64,
    pub(crate) max_us: f64,
    /// Allocations and bytes of the median run.
    pub(crate) allocations: u64,
    pub(crate) bytes: u64,
}

/// Runs `work` at least five and at most 400 times (about 0.4 s in all) after one warm-up run.
/// `work` returns what it computed so the optimiser cannot drop it.
pub(crate) fn bench<R>(mut work: impl FnMut() -> R) -> Stats {
    std::hint::black_box(work());
    let started = Instant::now();
    let mut costs = Vec::new();
    while costs.len() < 5 || (costs.len() < 400 && started.elapsed() < Duration::from_millis(400)) {
        let (result, cost) = measure(&mut work);
        std::hint::black_box(result);
        costs.push(cost);
    }
    costs.sort_by_key(|cost| cost.elapsed);
    let median = costs[costs.len() / 2];
    let micros = |cost: &Cost| cost.elapsed.as_secs_f64() * 1e6;
    Stats {
        runs: costs.len(),
        median_us: micros(&median),
        min_us: micros(&costs[0]),
        max_us: micros(costs.last().expect("at least five runs")),
        allocations: median.allocations,
        bytes: median.bytes,
    }
}

/// Prints and records one result: `group` and `case` name it, `fields` are its numbers.
pub(crate) fn record(group: &str, case: &str, fields: &[(&str, f64)]) {
    let metrics: serde_json::Map<String, serde_json::Value> =
        fields.iter().map(|(name, value)| ((*name).to_owned(), serde_json::json!(value))).collect();
    let line = serde_json::json!({ "group": group, "case": case, "metrics": metrics });
    eprintln!("PERF {line}");
}

/// [`record`] for a [`Stats`] (plus extra fields).
pub(crate) fn record_stats(group: &str, case: &str, stats: &Stats, extra: &[(&str, f64)]) {
    let mut fields = vec![
        ("median_us", stats.median_us),
        ("min_us", stats.min_us),
        ("max_us", stats.max_us),
        ("allocs", stats.allocations as f64),
        ("bytes", stats.bytes as f64),
        ("runs", stats.runs as f64),
    ];
    fields.extend_from_slice(extra);
    record(group, case, &fields);
}

/// `sorted` must be sorted ascending and not empty.
#[cfg(feature = "bench")]
pub(crate) fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * quantile).round() as usize]
}
