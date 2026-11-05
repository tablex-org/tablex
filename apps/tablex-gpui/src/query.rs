use arrow_array::RecordBatch;
use std::collections::HashMap;
use std::sync::Arc;

/// Rows a result tab shows: Arrow batches plus the row total. Built by the UI
/// scenario fixtures (drivers hand pages over as Arrow IPC leases instead).
#[derive(Debug, Clone)]
pub struct QueryResultBatch {
    pub batches: Vec<Arc<RecordBatch>>,
    pub total_count: i64,
}

/// The page size nothing else decides (shared with the connection profile that `tablex-mcp` reads).
pub const DEFAULT_OUTPUT_LIMIT: i64 = engine_connect::profile::DEFAULT_PAGE_SIZE as i64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaginationState {
    page: u64,
    page_size: i64,
    has_next: bool,
    exact_total: Option<u64>,
    /// The page a SQL run in flight is fetching. `page` keeps naming the page
    /// on screen until that run's result arrives (a refused, failed or
    /// cancelled run leaves it alone).
    pending_page: Option<u64>,
}

impl Default for PaginationState {
    fn default() -> Self {
        Self::new(DEFAULT_OUTPUT_LIMIT)
    }
}

impl PaginationState {
    pub fn new(page_size: i64) -> Self {
        Self {
            page: 0,
            page_size: page_size.max(1),
            has_next: false,
            exact_total: None,
            pending_page: None,
        }
    }

    pub fn page(&self) -> u64 {
        self.page
    }

    pub fn page_size(&self) -> i64 {
        self.page_size
    }

    pub fn offset(&self) -> i64 {
        self.offset_of(self.page)
    }

    pub fn offset_of(&self, page: u64) -> i64 {
        page.saturating_mul(self.page_size as u64)
            .min(i64::MAX as u64) as i64
    }

    /// The page a Next click would fetch, without moving to it.
    pub fn next_page(&self) -> Option<u64> {
        self.has_next.then(|| self.page.saturating_add(1))
    }

    /// The page a Previous click would fetch, without moving to it.
    pub fn previous_page(&self) -> Option<u64> {
        self.page.checked_sub(1)
    }

    /// A run is about to fetch `page`; it becomes the shown page when the
    /// result lands (any of the `set_*` below), and is dropped by
    /// [`Self::abandon_page`] / [`Self::stop_paging`] / [`Self::reset`].
    pub fn begin_page(&mut self, page: u64) {
        self.pending_page = Some(page);
    }

    /// The run in flight ended without a result (failed or cancelled): the
    /// page on screen is still the shown one.
    pub fn abandon_page(&mut self) {
        self.pending_page = None;
    }

    fn commit_page(&mut self) {
        if let Some(page) = self.pending_page.take() {
            self.page = page;
        }
    }

    pub fn has_previous(&self) -> bool {
        self.page > 0
    }

    pub fn has_next(&self) -> bool {
        self.has_next
    }

    #[cfg(any(test, feature = "ui-scenarios"))]
    pub fn exact_total_count(&self) -> Option<u64> {
        self.exact_total
    }

    pub fn set_result_count(&mut self, count: usize) {
        self.commit_page();
        self.exact_total = None;
        self.has_next = count >= self.page_size as usize;
    }

    pub fn set_page_result_count(
        &mut self,
        row_count: usize,
        total_count: Option<u64>,
        total_is_exact: bool,
    ) {
        self.commit_page();
        self.exact_total = total_count.filter(|_| total_is_exact);
        self.has_next = match total_count {
            Some(total) if total_is_exact => {
                (self.offset().max(0) as u64).saturating_add(row_count as u64) < total
            }
            _ => row_count >= self.page_size as usize,
        };
    }

    /// A page served by a SQL driver: its own `truncated` flag (an exact
    /// lookahead) says whether a next page exists. A statement the driver
    /// cannot page reports `false`, however many rows it returned.
    pub fn set_driver_page(
        &mut self,
        total_count: Option<u64>,
        total_is_exact: bool,
        truncated: bool,
    ) {
        self.commit_page();
        self.exact_total = total_count.filter(|_| total_is_exact);
        self.has_next = truncated;
    }

    /// The driver refused to page this result (`query.not_pageable`): the page
    /// that is still shown stays, with no next page.
    pub fn stop_paging(&mut self) {
        self.pending_page = None;
        self.has_next = false;
    }

    pub fn next(&mut self) -> bool {
        if !self.has_next {
            return false;
        }
        self.page = self.page.saturating_add(1);
        true
    }

    pub fn previous(&mut self) -> bool {
        if !self.has_previous() {
            return false;
        }
        self.page -= 1;
        true
    }

    pub fn reset(&mut self) {
        self.page = 0;
        self.pending_page = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct QueryRunId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryStatus {
    Idle,
    Running {
        run: QueryRunId,
    },
    Ready {
        rows: usize,
    },
    Failed(String),
    /// The run was cancelled by the user; results from it are ignored.
    Cancelled,
}

/// What a run was asked to do and where: kept with the run's state, so what "Fix" sends cannot
/// belong to a different run than the failure it sends with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    pub sql: String,
    pub connection: String,
    pub database: String,
    pub schema: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryState {
    status: QueryStatus,
    /// The live run's statement and place (`None` when the run did not say).
    running: Option<RunContext>,
    /// The statement and place of the run that failed, while the state is `Failed`.
    failed: Option<RunContext>,
    next_run: u64,
    /// What the tab showed when the live run began: `Ready` keeps its result,
    /// anything else rests as `Idle`. A run withdrawn before it started
    /// (see [`Self::withdraw`]) returns here.
    resting: QueryStatus,
}

#[derive(Debug, Default)]
pub struct QueryTaskRegistry {
    active: HashMap<u64, QueryRunId>,
}

impl QueryTaskRegistry {
    pub fn register(&mut self, tab_id: u64, run: QueryRunId) -> bool {
        if self.active.contains_key(&tab_id) {
            return false;
        }
        self.active.insert(tab_id, run);
        true
    }

    pub fn owns(&self, tab_id: u64, run: QueryRunId) -> bool {
        self.active.get(&tab_id).copied() == Some(run)
    }

    pub fn clear(&mut self, tab_id: u64, run: QueryRunId) -> bool {
        if !self.owns(tab_id, run) {
            return false;
        }
        self.active.remove(&tab_id);
        true
    }

    pub fn remove_tab(&mut self, tab_id: u64) -> Option<QueryRunId> {
        self.active.remove(&tab_id)
    }
}

impl Default for QueryState {
    fn default() -> Self {
        Self {
            status: QueryStatus::Idle,
            running: None,
            failed: None,
            next_run: 0,
            resting: QueryStatus::Idle,
        }
    }
}

impl QueryState {
    /// Starts a new run. Returns the run id, or `None` when a run is already
    /// active (duplicate-run guard).
    pub fn begin(&mut self) -> Option<QueryRunId> {
        if matches!(self.status, QueryStatus::Running { .. }) {
            return None;
        }
        let run = QueryRunId(self.next_run);
        self.next_run += 1;
        self.resting = match self.status {
            QueryStatus::Ready { rows } => QueryStatus::Ready { rows },
            _ => QueryStatus::Idle,
        };
        self.status = QueryStatus::Running { run };
        self.running = None;
        self.failed = None;
        Some(run)
    }

    /// Says what the live `run` is running and where (false when `run` is not live).
    pub fn describe(&mut self, run: QueryRunId, context: RunContext) -> bool {
        if !self.is_live_run(run) {
            return false;
        }
        self.running = Some(context);
        true
    }

    /// The statement and place of the run that failed, while the state is `Failed` (`None` when
    /// the failed run did not say, e.g. a refusal before it was described).
    pub fn failed_run(&self) -> Option<&RunContext> {
        match self.status {
            QueryStatus::Failed(_) => self.failed.as_ref(),
            _ => None,
        }
    }

    /// The live `run` did not start (the driver asked for confirmation, or
    /// refused to page): the tab returns to what it showed before, which is
    /// not a failure. `false` when `run` is no longer the live run.
    pub fn withdraw(&mut self, run: QueryRunId) -> bool {
        if !self.is_live_run(run) {
            return false;
        }
        self.status = self.resting.clone();
        self.running = None;
        true
    }

    /// Marks the active run as cancelled and ignores its future completion.
    /// Returns the cancelled run id when a run was active.
    pub fn cancel(&mut self) -> Option<QueryRunId> {
        match self.status {
            QueryStatus::Running { run } => {
                self.status = QueryStatus::Cancelled;
                self.running = None;
                Some(run)
            }
            _ => None,
        }
    }

    /// Applies a successful result only if `run` is still the live run.
    /// Stale completions from superseded runs are rejected.
    pub fn complete(&mut self, run: QueryRunId, rows: usize) -> bool {
        if !self.is_live_run(run) {
            return false;
        }
        self.status = QueryStatus::Ready { rows };
        self.running = None;
        true
    }

    /// Applies a failure only if `run` is still the live run.
    pub fn fail(&mut self, run: QueryRunId, error: impl Into<String>) -> bool {
        if !self.is_live_run(run) {
            return false;
        }
        self.status = QueryStatus::Failed(error.into());
        self.failed = self.running.take();
        true
    }

    fn is_live_run(&self, run: QueryRunId) -> bool {
        matches!(self.status, QueryStatus::Running { run: current } if current == run)
    }

    pub fn can_run(&self) -> bool {
        !matches!(self.status, QueryStatus::Running { .. })
    }

    pub fn status(&self) -> &QueryStatus {
        &self.status
    }

    /// Monotonic query generation; increments whenever a run is begun.
    pub fn generation(&self) -> u64 {
        self.next_run
    }

    #[cfg(any(test, feature = "ui-scenarios"))]
    pub fn status_name(&self) -> &'static str {
        match self.status {
            QueryStatus::Idle => "Idle",
            QueryStatus::Running { .. } => "Running",
            QueryStatus::Ready { .. } => "Ready",
            QueryStatus::Failed(_) => "Failed",
            QueryStatus::Cancelled => "Cancelled",
        }
    }

    pub fn status_text(&self) -> String {
        match &self.status {
            QueryStatus::Idle => "Ready".to_string(),
            QueryStatus::Running { .. } => "Running query…".to_string(),
            // The row count belongs to the results footer; the app status bar
            // would otherwise repeat it.
            QueryStatus::Ready { .. } => "Ready".to_string(),
            QueryStatus::Failed(error) => error.clone(),
            QueryStatus::Cancelled => "Cancelled".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PaginationState, QueryRunId, QueryState, QueryStatus, QueryTaskRegistry,
    };

    #[test]
    fn a_failed_run_keeps_what_it_ran_until_the_next_run_or_a_new_state() {
        use super::RunContext;
        let context = |sql: &str| RunContext {
            sql: sql.to_owned(),
            connection: "c".to_owned(),
            database: "d".to_owned(),
            schema: None,
        };
        let mut state = QueryState::default();
        assert!(state.failed_run().is_none());
        let run = state.begin().unwrap();
        // A run that was not described, or a stale run id, sets nothing.
        assert!(!state.describe(QueryRunId(99), context("stale")));
        assert!(state.describe(run, context("SELECT 1")));
        assert!(state.fail(run, "boom"));
        assert_eq!(
            state.failed_run().map(|run| run.sql.as_str()),
            Some("SELECT 1")
        );
        // A run that succeeds, is cancelled or withdrawn leaves no failed statement behind.
        let run = state.begin().unwrap();
        assert!(state.failed_run().is_none(), "begin clears it");
        assert!(state.describe(run, context("SELECT 2")));
        assert!(state.complete(run, 1));
        assert!(state.failed_run().is_none());
        let run = state.begin().unwrap();
        state.describe(run, context("SELECT 3"));
        assert!(state.withdraw(run));
        let run = state.begin().unwrap();
        assert!(state.fail(run, "no statement said"));
        assert!(
            state.failed_run().is_none(),
            "nothing of the withdrawn run is the failure of this one"
        );
        // A new state has none.
        assert!(QueryState::default().failed_run().is_none());
    }

    #[test]
    fn exact_total_is_separate_from_page_rows_and_cleared_for_unknown_results() {
        let mut page = PaginationState::new(50);
        page.set_page_result_count(50, Some(240), true);
        assert_eq!(page.exact_total_count(), Some(240));
        assert!(page.has_next());
        page.set_page_result_count(50, Some(240), false);
        assert_eq!(page.exact_total_count(), None);
        page.set_page_result_count(0, Some(0), true);
        assert_eq!(page.exact_total_count(), Some(0));
        assert!(!page.has_next());
        page.set_result_count(50);
        assert_eq!(page.exact_total_count(), None);
    }

    #[test]
    fn pagination_tracks_offsets_and_available_navigation() {
        let mut pagination = PaginationState::new(2);

        assert_eq!(pagination.offset(), 0);
        assert!(!pagination.previous());
        pagination.set_result_count(2);
        assert!(pagination.has_next());
        assert!(pagination.next());
        assert_eq!(pagination.page(), 1);
        assert_eq!(pagination.offset(), 2);
        assert!(pagination.previous());
        assert_eq!(pagination.page(), 0);
    }

    #[test]
    fn pagination_stops_next_navigation_on_short_page() {
        let mut pagination = PaginationState::new(3);
        pagination.set_result_count(2);

        assert!(!pagination.has_next());
        assert!(!pagination.next());
        assert_eq!(pagination.offset(), 0);
    }

    #[test]
    fn query_generation_changes_on_refresh_and_rejects_stale_finish() {
        let mut state = QueryState::default();
        assert_eq!(state.generation(), 0);
        let first = state.begin().expect("first query");
        assert_eq!(state.generation(), 1);
        assert!(state.complete(first, 50));
        let second = state.begin().expect("refresh query");
        assert_eq!(state.generation(), 2);
        assert!(!state.complete(first, 1));
        assert!(state.complete(second, 50));
        assert_eq!(state.status_name(), "Ready");
    }

    #[test]
    fn exact_total_count_controls_next_page_at_boundaries() {
        let mut pagination = PaginationState::new(200);
        pagination.set_page_result_count(200, Some(200), true);
        assert!(!pagination.has_next());

        pagination.set_page_result_count(200, Some(250), true);
        assert!(pagination.has_next());
        assert!(pagination.next());
        pagination.set_page_result_count(50, Some(250), true);
        assert!(!pagination.has_next());
    }

    #[test]
    fn driver_truncation_alone_controls_next_page() {
        let mut pagination = PaginationState::new(200);
        // A full page that is not truncated (a non-pageable statement, or a
        // result of exactly one page) never offers a next page.
        pagination.set_driver_page(None, false, false);
        assert!(!pagination.has_next());
        // A short page the driver still marks truncated does.
        pagination.set_driver_page(None, false, true);
        assert!(pagination.has_next());
        assert_eq!(pagination.next_page(), Some(1));
        assert_eq!(pagination.offset_of(1), 200);
        // The driver refused to page: the shown page stays, no next.
        pagination.begin_page(1);
        pagination.stop_paging();
        assert_eq!((pagination.page(), pagination.has_next()), (0, false));
    }

    #[test]
    fn a_page_in_flight_becomes_the_shown_page_only_when_its_result_lands() {
        let mut pagination = PaginationState::new(2);
        pagination.set_driver_page(None, false, true);
        assert_eq!((pagination.next_page(), pagination.previous_page()), (Some(1), None));

        // Fetching page 1: page 0 stays the shown one until the result lands.
        pagination.begin_page(1);
        assert_eq!(pagination.page(), 0);
        pagination.set_driver_page(None, false, true);
        assert_eq!((pagination.page(), pagination.offset()), (1, 2));
        assert_eq!(pagination.previous_page(), Some(0));

        // A run that ended without a result leaves the shown page alone.
        pagination.begin_page(2);
        pagination.abandon_page();
        pagination.set_driver_page(None, false, false);
        assert_eq!(pagination.page(), 1, "nothing pending: the setter does not move the page");

        // Reset drops a pending page too.
        pagination.begin_page(3);
        pagination.reset();
        pagination.set_result_count(1);
        assert_eq!(pagination.page(), 0);
    }

    #[test]
    fn withdrawn_run_restores_the_previous_result_or_idle() {
        let mut state = QueryState::default();
        let first = state.begin().unwrap();
        assert!(state.withdraw(first));
        assert_eq!(state.status(), &QueryStatus::Idle);

        let run = state.begin().unwrap();
        assert!(state.complete(run, 7));
        let second = state.begin().unwrap();
        assert!(state.withdraw(second));
        assert_eq!(state.status(), &QueryStatus::Ready { rows: 7 });
        assert!(!state.withdraw(second), "only the live run can be withdrawn");

        let failing = state.begin().unwrap();
        assert!(state.fail(failing, "boom"));
        let after_failure = state.begin().unwrap();
        assert!(state.withdraw(after_failure));
        assert_eq!(state.status(), &QueryStatus::Idle, "a failure is not restored");
    }

    #[test]
    fn pagination_reset_returns_to_first_page() {
        let mut pagination = PaginationState::new(20);
        pagination.set_result_count(20);
        assert!(pagination.next());
        assert_eq!(pagination.offset(), 20);

        pagination.reset();

        assert_eq!(pagination.page(), 0);
        assert_eq!(pagination.offset(), 0);
    }

    #[test]
    fn rejects_second_run_while_running() {
        let mut state = QueryState::default();

        let first = state.begin().expect("first run starts");
        assert!(state.begin().is_none());
        assert_eq!(state.status(), &QueryStatus::Running { run: first });
    }

    #[test]
    fn completed_run_records_rows_but_status_text_leaves_the_count_to_the_footer() {
        let mut state = QueryState::default();

        let run = state.begin().expect("run starts");
        assert!(state.complete(run, 200));

        assert_eq!(state.status(), &QueryStatus::Ready { rows: 200 });
        assert_eq!(state.status_text(), "Ready");
    }

    #[test]
    fn exposes_failures_and_allows_retry() {
        let mut state = QueryState::default();

        let run = state.begin().expect("run starts");
        assert!(state.fail(run, "connection failed"));

        assert_eq!(
            state.status(),
            &QueryStatus::Failed("connection failed".into())
        );
        assert!(state.begin().is_some(), "failure allows retry");
    }

    #[test]
    fn stale_completion_from_superseded_run_is_ignored() {
        let mut state = QueryState::default();

        let first = state.begin().expect("first run");
        // User cancels, then starts a second run that becomes live.
        assert!(state.cancel().is_some());
        let second = state.begin().expect("second run");

        // The first run's result arrives late and must not clobber the live run.
        assert!(!state.complete(first, 999));
        assert_eq!(state.status(), &QueryStatus::Running { run: second });

        assert!(state.complete(second, 5));
        assert_eq!(state.status(), &QueryStatus::Ready { rows: 5 });
    }

    #[test]
    fn cancelled_run_ignores_late_completion_and_failure() {
        let mut state = QueryState::default();

        let run = state.begin().expect("run");
        assert_eq!(state.cancel(), Some(run));

        assert!(!state.complete(run, 10), "late completion ignored");
        assert!(!state.fail(run, "late error"), "late failure ignored");
        assert_eq!(state.status(), &QueryStatus::Cancelled);
        assert!(state.begin().is_some(), "cancel allows a fresh run");
    }

    #[test]
    fn cancel_without_active_run_is_a_no_op() {
        let mut state = QueryState::default();

        assert_eq!(state.cancel(), None);
        assert_eq!(state.status(), &QueryStatus::Idle);
    }

    #[test]
    fn query_tasks_are_owned_per_tab() {
        let mut tasks = QueryTaskRegistry::default();
        let first = QueryRunId(10);
        let second = QueryRunId(20);

        assert!(tasks.register(1, first));
        assert!(tasks.register(2, second));
        assert!(!tasks.register(1, QueryRunId(11)));
        assert!(tasks.owns(1, first));
        assert!(tasks.owns(2, second));
    }

    #[test]
    fn removing_one_tab_does_not_cancel_another_tab() {
        let mut tasks = QueryTaskRegistry::default();
        let first = QueryRunId(10);
        let second = QueryRunId(20);
        assert!(tasks.register(1, first));
        assert!(tasks.register(2, second));

        assert_eq!(tasks.remove_tab(1), Some(first));
        assert!(!tasks.owns(1, first));
        assert!(tasks.owns(2, second));
    }

    #[test]
    fn stale_completion_cannot_clear_a_newer_run() {
        let mut tasks = QueryTaskRegistry::default();
        let first = QueryRunId(10);
        let second = QueryRunId(11);
        assert!(tasks.register(1, first));
        assert_eq!(tasks.remove_tab(1), Some(first));
        assert!(tasks.register(1, second));

        assert!(!tasks.clear(1, first));
        assert!(tasks.owns(1, second));
        assert!(tasks.clear(1, second));
        assert!(!tasks.owns(1, second));
    }
}
