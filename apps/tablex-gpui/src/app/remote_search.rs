//! Sidebar "global" (remote) search: with
//! the Local-search switch off and a non-empty query, every EXPANDED
//! container of a connected connection has its object groups (Tables/Views)
//! reloaded through the driver's server-side name filter
//! (case-insensitive substring, capped at `REMOTE_FILTER_LIMIT`). Collapsed containers are left alone. Clearing the
//! query (or turning Local search back on) restores the unfiltered children.
//!
//! Bounded and cancellable: 250ms debounce, `MAX_IN_FLIGHT` concurrent loads,
//! a generation counter that drops stale timers and pumps. All driver
//! calls run off the UI thread. SQL drivers receive the filter as the
//! navigator request's `payload.filter` (`navigator::sql_driver`).
//!
//! Not covered: MongoDB lists its catalog through provider nodes, which have no
//! name-filter parameter, so it is skipped by remote search.

use super::*;
use std::time::Duration;

pub(super) const DEBOUNCE: Duration = Duration::from_millis(250);
pub(super) const MAX_IN_FLIGHT: usize = 4;

#[derive(Default)]
pub(super) struct RemoteSearch {
    /// Bumped by every search change; timers and pumps carrying an older
    /// value do nothing.
    pub(super) generation: u64,
    /// A filtered search or its restore is running (drives the spinner).
    pub(super) loading: bool,
    /// The active server-side filter (a remote search is in effect).
    pub(super) filter: Option<String>,
    pumping: bool,
    repump: bool,
}

/// Remote search needs a query, regex off and the Local-search
/// switch off.
pub(super) fn remote_search_wanted(query: &str, regex: bool, local_search: bool) -> bool {
    !query.trim().is_empty() && !regex && !local_search
}

impl AppRoot {
    /// Restarts (or cancels/undoes) remote search after any search/option change.
    pub(super) fn schedule_remote_search(&mut self, cx: &mut Context<Self>) {
        let remote = &mut self.sidebar.remote;
        remote.generation += 1;
        remote.loading = false;
        let generation = remote.generation;
        let options = self.navigator.search_options();
        let query = options.query.trim().to_string();
        if !remote_search_wanted(
            &options.query,
            options.regex,
            self.navigator.preferences().local_search(),
        ) {
            // Leaving remote mode: restore unfiltered children.
            self.sidebar.remote.filter = None;
            if self.navigator.has_filtered_groups() {
                self.sidebar.remote.loading = true;
                self.pump_remote_search(cx);
            }
            self.notify_chrome(cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.sidebar.remote.generation == generation {
                    this.sidebar.remote.filter = Some(query);
                    this.sidebar.remote.loading = true;
                    this.pump_remote_search(cx);
                }
            });
        })
        .detach();
        self.notify_chrome(cx);
    }

    /// Starts as many filtered (or restoring) group loads as the concurrency
    /// budget allows and stops when nothing is left to load. Called at the
    /// start of a search and after every finished catalog load.
    pub(super) fn pump_remote_search(&mut self, cx: &mut Context<Self>) {
        if !self.sidebar.remote.loading {
            return;
        }
        if self.sidebar.remote.pumping {
            // A synchronous (fixture) load finished inside the loop below.
            self.sidebar.remote.repump = true;
            return;
        }
        self.sidebar.remote.pumping = true;
        loop {
            self.sidebar.remote.repump = false;
            if !self.sidebar.remote.loading {
                break;
            }
            let connected: HashSet<String> =
                self.sessions.connected().map(str::to_string).collect();
            let budget = MAX_IN_FLIGHT.saturating_sub(self.navigator.loading_count());
            let step = match self.sidebar.remote.filter.clone() {
                Some(filter) => self.navigator.filter_step(&connected, &filter, budget),
                None => self.navigator.restore_step(budget),
            };
            if step.finished() {
                self.sidebar.remote.loading = false;
            }
            for request in step.requests {
                self.dispatch_group_load(request, cx);
            }
            if !self.sidebar.remote.repump {
                break;
            }
        }
        self.sidebar.remote.pumping = false;
        self.notify_chrome(cx);
    }

    /// The server-side filter a catalog load must carry, if a remote search is
    /// active: user expansions of a group during a search are filtered too,
    /// and group loads planned by the search
    /// already carry theirs.
    pub(super) fn group_filter_for(&mut self, request: &LoadRequest) -> Option<String> {
        if let Some(filter) = self.sidebar.remote.filter.clone() {
            self.navigator.note_group_filter(request, &filter);
        }
        self.navigator.request_filter(request)
    }

    /// Notice for groups showing a truncated filtered listing.
    pub(super) fn remote_truncated_notice(&self) -> Option<String> {
        (self.navigator.truncated_group_count() > 0).then(|| {
            format!(
                "More than {} matches: some groups show only the first {}. Refine the query.",
                crate::navigator::REMOTE_FILTER_LIMIT,
                crate::navigator::REMOTE_FILTER_LIMIT
            )
        })
    }

    fn dispatch_group_load(&mut self, request: LoadRequest, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            self.dispatch_scenario_load(request, cx);
            return;
        }
        if self.sessions.is_connected(&request.connection_id()) {
            self.dispatch_provider_load(request, cx);
        } else if let Some(generation) = self.navigator.load_generation(&request) {
            // The session vanished between planning and dispatch.
            self.apply_navigator_load(
                &request,
                generation,
                Err("connection is not connected".into()),
                cx,
            );
        }
    }

    /// Persists the Local-search switch (in memory only when there is no
    /// preferences file, e.g. plain scenario runs) and restarts the search.
    pub(super) fn set_sidebar_local_search(&mut self, local: bool, cx: &mut Context<Self>) {
        if self.sidebar.path.is_some() {
            let _ = self.change_sidebar_preferences(
                |prefs| {
                    prefs.set_local_search(local);
                    Ok(())
                },
                cx,
            );
        } else {
            let mut prefs = self.navigator.preferences().clone();
            prefs.set_local_search(local);
            let _ = self.navigator.set_preferences(prefs);
        }
        self.schedule_remote_search(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::remote_search_wanted;

    #[test]
    fn remote_search_needs_a_query_no_regex_and_the_switch_off() {
        assert!(remote_search_wanted("orders", false, false));
        assert!(!remote_search_wanted("orders", false, true), "local mode");
        assert!(!remote_search_wanted("orders", true, false), "regex disables it");
        assert!(!remote_search_wanted("   ", false, false), "blank query");
        assert!(!remote_search_wanted("", false, false));
    }
}
