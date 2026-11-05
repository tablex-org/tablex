//! Call counters of the structure editor's hot paths, bumped by one `#[cfg(test)]` line in each
//! (so release builds carry nothing). Read by the on-demand benchmarks (`perf_bench`) to tell how
//! often an edit or a frame reruns the expensive work.

use std::sync::atomic::{AtomicU64, Ordering};

/// `PropertiesEdit::rebuild`: rows, staleness, plan, lists.
pub(crate) static REBUILD: AtomicU64 = AtomicU64::new(0);
/// `sql_ddl::plan_against`.
pub(crate) static PLAN_AGAINST: AtomicU64 = AtomicU64::new(0);
/// `AppRoot::load_structure_catalog`.
pub(crate) static LOAD_STRUCTURE_CATALOG: AtomicU64 = AtomicU64::new(0);

pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}
