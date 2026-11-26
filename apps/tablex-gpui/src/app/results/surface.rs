//! One tab's result area as a cached GPUI view.
//!
//! Each tab with results owns a [`TabResults`] entity (in its `TabView`). The
//! shell embeds it with `Entity::cached`, so GPUI replays the previous frame's
//! prepaint and paint for it unless the entity was notified (or the window was
//! refreshed). Two tabs shown at once therefore never rebuild each other's
//! grids, and a change that only touches the chrome (fps counter, sidebar)
//! does not rebuild the grid of the tab beside it.
//!
//! The entity holds no state: its `render` calls back into `AppRoot`
//! (`render_tab_results`) with the tab id, so every handler still runs against
//! `AppRoot`. What invalidates the cache:
//!
//! * [`AppRoot::notify_tab`] — only that tab's view (use it for work that is
//!   scoped to one tab: selection, find, scrolling, editing).
//! * A plain `cx.notify()` on `AppRoot` — every tab view. This is the safe
//!   default: anything that was written before views were cached keeps
//!   refreshing the results.
//! * [`AppRoot::notify_chrome`] — nothing in the results, for changes that
//!   cannot affect them (fps counter, sidebar-only state). It notifies the
//!   window's `Root` instead of `AppRoot`, so the (uncached) shell redraws and
//!   the cached results views are replayed. The left pane is a cached view
//!   too (`navigator_ui::SidebarView`): a plain `AppRoot` notification and this
//!   one refresh it; [`AppRoot::notify_tab`] refreshes it only when the state it
//!   draws moved (see `sidebar_watch`).

use super::super::*;

/// How a tab's result area is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ResultsLayout {
    /// The tab has no result area (connection page, diagram, Driver Manager).
    None,
    /// Query tab before its first run: the editor owns the panel and only the
    /// (normally closed) history strip sits below it.
    EditorOnly,
    /// Aggregate tab: pipeline editor and output (the result view is the output).
    Aggregate,
    /// Toolbar (Mongo / table data), result grid, footer and history strip.
    Full,
    /// The grid alone: the preview of an import tab.
    Preview,
}

impl AppRoot {
    pub(in crate::app) fn results_layout(&self, tab_id: u64) -> ResultsLayout {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return ResultsLayout::None;
        };
        if tab.kind == TabKind::Import {
            return ResultsLayout::Preview;
        }
        if tab.kind != TabKind::Query {
            return ResultsLayout::None;
        }
        if tab.query_mode.is_mongo_aggregate() {
            return ResultsLayout::Aggregate;
        }
        // SQL editor and Mongo script tabs show only the editor, at full
        // height, until their first run; find / table-data tabs always show
        // their result area. An unusable connection replaces the (empty)
        // result area with the reconnect card, so the editor shares the panel.
        let editor_tab = !tab.query_mode.is_mongo_find()
            && !tab.query_mode.is_table_data()
            && !tab.query_mode.is_redis_key();
        let connection_unusable =
            self.result_connection_state(tab).is_some() && !self.tab_holds_results(tab_id);
        let has_run = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.has_run());
        if editor_tab && !connection_unusable && !has_run {
            ResultsLayout::EditorOnly
        } else {
            ResultsLayout::Full
        }
    }

    /// The tab's cached results view, created on first use.
    pub(in crate::app) fn tab_results_view(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<TabResults> {
        let root = cx.entity().downgrade();
        self.view_mut(tab_id)
            .surface
            .get_or_insert_with(|| cx.new(|_| TabResults { root, tab_id }))
            .clone()
    }

    /// The tab's results embedded for the shell: a cached view that fills its
    /// parent.
    pub(in crate::app) fn embed_tab_results(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui::Styled as _;
        self.tab_results_view(tab_id, cx)
            .cached(
                gpui::StyleRefinement::default()
                    .flex_1()
                    .min_h_0()
                    .min_w_0(),
            )
            .into_any_element()
    }

    /// Re-renders only `tab_id`'s result area (and, as ancestors, the window's
    /// uncached views). Other tabs' cached views are replayed.
    pub(in crate::app) fn notify_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.sync_sidebar(cx);
        // The cell detail panel in a window of its own follows its tab (a no-op without one).
        self.sync_cell_detail_window(tab_id, cx);
        match self
            .tab_views
            .get(&tab_id)
            .and_then(|view| view.surface.as_ref())
        {
            // By id, not `update`: this also runs while the view itself renders.
            Some(surface) => {
                let id = surface.entity_id();
                let app: &mut gpui::App = cx;
                app.notify(id);
            }
            // Not shown yet: the next render creates it fresh.
            None => cx.notify(),
        }
    }

    /// Re-renders the chrome without touching any tab's cached results: for a
    /// change that cannot affect them (fps counter, sidebar-only state). The
    /// left pane, a cached view too, is notified with it when what it draws changed. Falls back to a plain
    /// notification before the first render.
    pub(in crate::app) fn notify_chrome(&mut self, cx: &mut Context<Self>) {
        match self
            .window_root
            .as_ref()
            .and_then(gpui::WeakEntity::upgrade)
        {
            Some(root) => {
                root.update(cx, |_, cx| cx.notify());
                // The left pane is cached too: told if what it draws changed (not for the fps counter).
                self.sync_sidebar(cx);
            }
            None => cx.notify(),
        }
    }

    /// Re-renders the left pane (a cached view, see `SidebarView`) and notes that it was told about the state as it
    /// is now. Not for use while the window draws: GPUI drops a notification sent from inside a draw (the root's
    /// render uses [`Self::sidebar_behind`] instead).
    pub(in crate::app) fn notify_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sync_sidebar_marker();
        let app: &mut gpui::App = cx;
        app.notify(self.sidebar_view.entity_id());
    }

    /// Brings the tree's copy of the live set up to date and notes where the pane's state is now.
    fn sync_sidebar_marker(&mut self) {
        if self.sessions.revision() != self.sidebar_seen.1 {
            // The tree filters on the live set (active connections only): it follows the sessions on every path,
            // not only on those that remember to copy them over.
            self.sync_connected_ids();
        }
        self.sidebar_seen = (self.sidebar_epoch.now(), self.sessions.revision());
    }

    /// What the left pane draws changed since it was notified: it notifies it now. The state it draws is held in
    /// `Watched` fields (see `sidebar_watch`: every `&mut` borrow of one moves the epoch) and a connection's status
    /// has its own revision. A path that changes either and then notifies only a tab or nothing would leave the pane
    /// showing the old tree; this runs where those paths end (a tab's or the chrome's notification, the root's
    /// notification). The root's render does the same through [`Self::sidebar_behind`].
    pub(in crate::app) fn sync_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_seen != (self.sidebar_epoch.now(), self.sessions.revision()) {
            self.notify_sidebar(cx);
        }
    }

    /// The root's render asks, before it embeds the pane: has the state the pane draws moved since it last rendered?
    /// If so the root embeds it uncached for this frame, so it renders in this very draw. Notifying it from here
    /// would not do: the root renders inside a draw, GPUI only records the view as dirty then (the draw has already
    /// taken its set of dirty views, and nothing asks for another frame), so the cached pane would replay its old
    /// frame and, `sidebar_seen` being current, nothing would try again. Next frame it is cached again.
    pub(in crate::app) fn sidebar_behind(&mut self) -> bool {
        self.sync_sidebar_marker();
        self.sidebar_drawn != self.sidebar_seen
    }

    /// The pane renders: it shows the state as it is now.
    pub(in crate::app) fn mark_sidebar_drawn(&mut self) {
        self.sidebar_drawn = (self.sidebar_epoch.now(), self.sessions.revision());
    }

    /// `AppRoot` itself was notified: every tab view may be stale. Runs as the
    /// root's own observer.
    pub(in crate::app) fn on_root_notified(&mut self, cx: &mut Context<Self>) {
        // The assistant dock follows the active tab (free while it is closed).
        self.assistant_push_context(cx);
        self.notify_pane_views(cx);
        self.sync_cell_detail_windows(cx);
        self.sync_sidebar(cx);
        let app: &mut gpui::App = cx;
        for view in self.tab_views.values() {
            if let Some(surface) = &view.surface {
                app.notify(surface.entity_id());
            }
        }
    }

    /// The tab's result area: toolbar, grid, footer and history strip.
    pub(in crate::app) fn render_tab_results(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(test)]
        {
            RENDERS.with(|renders| *renders.borrow_mut().entry(tab_id).or_default() += 1);
        }
        self.sync_json_viewers(tab_id, window, cx);
        self.sync_columns_panel(tab_id, window, cx);
        self.sync_plan_view(tab_id, window, cx);
        self.sync_script_stats(tab_id, window, cx);
        self.sync_mongo_row_fields(tab_id, cx);
        self.sync_redis_string(tab_id, window, cx);
        self.sync_chart(tab_id, window, cx);
        match self.results_layout(tab_id) {
            ResultsLayout::None => div().into_any_element(),
            ResultsLayout::EditorOnly => self.render_history_panel(tab_id, cx),
            ResultsLayout::Preview => div().size_full().flex().flex_col().child(self.render_results_body(tab_id, window, cx)).into_any_element(),
            // The output only: the pipeline editor is the query tab's own (`render_aggregate_editor`).
            ResultsLayout::Aggregate => self.render_aggregate_output(tab_id, window, cx),
            ResultsLayout::Full => {
                let (is_mongo, is_table, is_redis_key) = self
                    .workspace
                    .tab(tab_id)
                    .map(|tab| {
                        (
                            tab.query_mode.is_mongo_find(),
                            tab.query_mode.is_table_data(),
                            tab.query_mode.is_redis_key(),
                        )
                    })
                    .unwrap_or_default();
                let toolbar = if is_mongo {
                    self.render_mongo_result_toolbar(tab_id, window, cx)
                } else if is_table {
                    self.render_sql_table_toolbar(tab_id, window, cx)
                } else if is_redis_key {
                    self.render_redis_key_toolbar(tab_id, window, cx)
                } else {
                    div().into_any_element()
                };
                let editor_tab = !is_mongo && !is_table && !is_redis_key;
                let header = if editor_tab {
                    // Right of the title: where these results go (below, right, a tab, a window).
                    ui::section_header(t!("grid.results"))
                        .debug_selector(|| "results-header".into())
                        .justify_between()
                        .pr(px(4.0))
                        .when(self.results_movable(tab_id), |header| {
                            header.child(self.render_results_place_menu(tab_id, cx))
                        })
                        .into_any_element()
                } else {
                    div().into_any_element()
                };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(toolbar)
                    .child(header)
                    .children(self.render_script_strip(tab_id, cx))
                    .children(self.render_shell_strip(tab_id, cx))
                    .child(self.render_results(tab_id, window, cx))
                    .children(self.render_shell_output(tab_id, cx))
                    // The footer (Refresh, paging, Export, Limit) belongs to the rows, not to a plan.
                    .children(
                        (!self.plan_presentation_active(tab_id) && !self.script_stats_active(tab_id))
                            .then(|| self.render_result_footer(tab_id, window, cx)),
                    )
                    .child(self.render_history_panel(tab_id, cx))
                    .into_any_element()
            }
        }
    }
}

/// The view `AppRoot` embeds for one tab's result area. It owns nothing:
/// `render` forwards to [`AppRoot::render_tab_results`].
pub(in crate::app) struct TabResults {
    root: gpui::WeakEntity<AppRoot>,
    tab_id: u64,
}

impl Render for TabResults {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        // The root is not being updated while its children render (its own
        // `render` has returned), so this is not a re-entrant update.
        self.root
            .update(cx, |root, root_cx| {
                root.render_tab_results(tab_id, window, root_cx)
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

#[cfg(test)]
thread_local! {
    /// How many times each tab's results were rendered (tests only).
    pub(in crate::app) static RENDERS: std::cell::RefCell<HashMap<u64, u64>> =
        std::cell::RefCell::new(HashMap::new());
}
