//! Where a query tab shows its results: below or right of its editor, in a tab of their own or in
//! a window of their own. Each tab picks from the Results header; a tab that picked nothing follows
//! the setting (below or right).
//!
//! The result area is always the tab's one cached view (`surface::TabResults`): moving it changes
//! only where that view is embedded, so a run, a sort or a page of the query tab shows wherever its
//! results are. It is shown in one place at a time, and the query tab shows a note where it was.
//! Closing the Results tab or window puts the results back beside the editor; closing the query tab
//! closes them.
//!
//! The Results header and a key (Toggle Results, Cmd/Ctrl+6) also hide the results beside the
//! editor, which then has the whole panel. The hiding lasts until the tab runs again (it holds the
//! run it was made for), is kept for the session only, and never applies to results in a tab or a
//! window: there the key brings the Results tab or window to the front, and from the Results tab
//! it goes back to the query tab.
//!
//! The aggregate tab (pipeline editor + output) uses the same places: its editor is the query
//! tab's editor area, and its output (`render_aggregate_output`) is the result view.
//!
//! The cell detail panel has the same fifth place (`CellDetailPlacement::Window`): the same window
//! type shows it (`ResultsWindow` with `WindowContent::CellDetail`), opened and kept in step by the
//! same helpers. It keeps following the selection of its tab, closing the window puts the panel back
//! where it was, and closing the tab closes the window.
//!
//! Known limit: gpui runs `AppRoot`'s `subscribe_in` callbacks (the editors, the find bar, the cell
//! editors) in the window that drew `AppRoot` last, and the results window's draw updates `AppRoot`.
//! The results window therefore has the app's window draw again right after it draws (only its root
//! view is notified: the cached views are not rendered again), which gives `AppRoot` back to it; in
//! between (one frame) such a callback may still get the results window.
//! The inputs a tab made for one window are dropped when its results move into or out of a window
//! (`forget_window_inputs`), so they are made again in the window that shows them; the layout,
//! selection, record view and chart settings stay.

use super::super::*;

/// Where one query tab shows its results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ResultsPlace {
    Below,
    Right,
    /// A Results tab of its own.
    Tab,
    /// A window of its own.
    Window,
}

impl ResultsPlace {
    pub(in crate::app) const ALL: [Self; 4] = [Self::Below, Self::Right, Self::Tab, Self::Window];

    /// Beside the editor, in the query tab itself.
    pub(in crate::app) fn is_beside(self) -> bool {
        matches!(self, Self::Below | Self::Right)
    }

    pub(in crate::app) fn slug(self) -> &'static str {
        match self {
            Self::Below => "below",
            Self::Right => "right",
            Self::Tab => "tab",
            Self::Window => "window",
        }
    }

    pub(in crate::app) fn label(self) -> String {
        match self {
            Self::Below => t!("grid.results_below").into_owned(),
            Self::Right => t!("grid.results_right").into_owned(),
            Self::Tab => t!("grid.results_in_tab").into_owned(),
            Self::Window => t!("grid.results_in_window").into_owned(),
        }
    }

    pub(in crate::app) fn icon(self) -> &'static str {
        match self {
            Self::Below => "icons/panel-bottom.svg",
            Self::Right => "icons/panel-right.svg",
            Self::Tab => "icons/rows-3.svg",
            Self::Window => "icons/maximize-2.svg",
        }
    }
}

/// The places tabs picked, and where moved results are shown.
#[derive(Default)]
pub(in crate::app) struct ResultsPlacement {
    /// The place each tab picked; a tab not listed follows the setting.
    places: HashMap<u64, ResultsPlace>,
    /// A Results tab -> the query tab whose results it shows.
    hosts: HashMap<u64, u64>,
    /// A query tab -> the window that shows its results (set once the window is open).
    windows: HashMap<u64, gpui::AnyWindowHandle>,
    /// A query tab -> the run (`QueryState::generation`) whose results beside the editor it hid.
    hidden: HashMap<u64, u64>,
    /// A tab -> the window that shows its cell detail panel (set once the window is open).
    detail_windows: HashMap<u64, DetailWindow>,
}

/// The window of a cell detail panel and the cached view that renders the panel in it.
struct DetailWindow {
    handle: gpui::AnyWindowHandle,
    surface: gpui::Entity<CellDetailSurface>,
}

/// What a window of its own shows for a tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowContent {
    Results,
    CellDetail,
}

impl AppRoot {
    /// A tab whose results can move: a query tab with an editor above its results (SQL, MongoDB
    /// script, Redis console, aggregate pipeline). Table data and find tabs are results already.
    pub(in crate::app) fn results_movable(&self, tab_id: u64) -> bool {
        self.workspace.tab(tab_id).is_some_and(|tab| {
            tab.kind == TabKind::Query
                && !tab.query_mode.is_mongo_find()
                && !tab.query_mode.is_table_data()
                && !tab.query_mode.is_redis_key()
        })
    }

    /// Where `tab_id` shows its results now.
    pub(in crate::app) fn results_place(&self, tab_id: u64) -> ResultsPlace {
        self.results_placement.places.get(&tab_id).copied().unwrap_or(
            match self.settings.editor.results_position {
                crate::settings::ResultsPosition::Below => ResultsPlace::Below,
                crate::settings::ResultsPosition::Right => ResultsPlace::Right,
            },
        )
    }

    /// `tab_id` hid its results beside the editor, and has not run since.
    pub(in crate::app) fn results_hidden(&self, tab_id: u64) -> bool {
        self.results_placement.hidden.get(&tab_id).is_some_and(|run| {
            self.results_movable(tab_id)
                && self.results_place(tab_id).is_beside()
                && self.workspace.runtime(tab_id).map_or(0, |runtime| runtime.query.generation()) == *run
        })
    }

    /// Hides the results beside the editor, or shows them again; results in a Results tab or window
    /// come to the front instead (from the Results tab: the query tab does).
    pub(in crate::app) fn toggle_results(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if !self.results_movable(tab_id) {
            return;
        }
        if !self.results_place(tab_id).is_beside() {
            let host = self.results_host(tab_id);
            if host.is_some() && host == self.workspace.active_tab().map(|tab| tab.id) {
                self.activate_tab_ui(tab_id, cx);
            } else {
                self.show_moved_results(tab_id, cx);
            }
            return;
        }
        if self.results_hidden(tab_id) {
            self.results_placement.hidden.remove(&tab_id);
        } else {
            let run = self.workspace.runtime(tab_id).map_or(0, |runtime| runtime.query.generation());
            self.results_placement.hidden.insert(tab_id, run);
        }
        cx.notify();
    }

    /// The Toggle Results key and palette command: acts on the results of the active tab.
    pub(in crate::app) fn toggle_active_results(&mut self, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.active_results_tab() {
            self.toggle_results(tab_id, cx);
        }
    }

    /// The query tab a Results tab shows.
    pub(in crate::app) fn results_source(&self, host: u64) -> Option<u64> {
        self.results_placement.hosts.get(&host).copied()
    }

    /// What a bulk close (Close Others, ...) from the tab `anchor` closes: a Results tab does not
    /// close its own query tab (that would close the anchor).
    pub(in crate::app) fn tab_close_targets(
        &self,
        anchor: u64,
        scope: crate::workspace::TabCloseScope,
    ) -> Vec<u64> {
        let source = self.results_source(anchor);
        let mut ids = self.workspace.close_targets(anchor, scope);
        ids.retain(|id| Some(*id) != source);
        ids
    }

    /// The Results tab that shows `tab_id`'s results.
    pub(in crate::app) fn results_host(&self, tab_id: u64) -> Option<u64> {
        self.results_placement
            .hosts
            .iter()
            .find(|(_, source)| **source == tab_id)
            .map(|(host, _)| *host)
    }

    /// The query tab whose results the active tab shows: the active tab itself, or the query tab
    /// of an active Results tab.
    pub(in crate::app) fn active_results_tab(&self) -> Option<u64> {
        let active = self.workspace.active_tab()?.id;
        Some(self.results_source(active).unwrap_or(active))
    }

    /// `tab_id`'s results are what `window` shows to the user now: in their own window, or in the
    /// active tab of the app's window (the query tab or its Results tab).
    pub(in crate::app) fn results_in_view(&self, tab_id: u64, window: &Window) -> bool {
        match self.results_window(tab_id) {
            Some(handle) => handle == window.window_handle(),
            None => self.active_results_tab() == Some(tab_id),
        }
    }

    /// The window that shows `tab_id`'s results.
    pub(in crate::app) fn results_window(&self, tab_id: u64) -> Option<gpui::AnyWindowHandle> {
        self.results_placement.windows.get(&tab_id).copied()
    }

    /// Moves `tab_id`'s results to `place`. Picking the place they are in shows them there.
    pub(in crate::app) fn set_results_place(
        &mut self,
        tab_id: u64,
        place: ResultsPlace,
        cx: &mut Context<Self>,
    ) {
        if !self.results_movable(tab_id) {
            return;
        }
        self.results_placement.hidden.remove(&tab_id);
        if self.results_place(tab_id) == place {
            self.show_moved_results(tab_id, cx);
            cx.notify();
            return;
        }
        let window_change = place == ResultsPlace::Window || self.results_window(tab_id).is_some();
        self.leave_results_place(tab_id, cx);
        if window_change {
            self.forget_window_inputs(tab_id);
        }
        self.results_placement.places.insert(tab_id, place);
        match place {
            ResultsPlace::Tab => {
                let title = self
                    .workspace
                    .tab(tab_id)
                    .map(|tab| format!("{} · {}", t!("grid.results"), tab.title))
                    .unwrap_or_else(|| t!("grid.results").into_owned());
                let host = self.workspace.open_results_tab(tab_id, &title);
                self.results_placement.hosts.insert(host, tab_id);
                self.activate_tab_ui(host, cx);
            }
            ResultsPlace::Window => self.open_results_window(tab_id, cx),
            ResultsPlace::Below | ResultsPlace::Right => {}
        }
        cx.notify();
    }

    /// Puts `tab_id`'s results back beside its editor (where the setting puts them).
    pub(in crate::app) fn bring_results_back(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.results_window(tab_id).is_some() {
            self.forget_window_inputs(tab_id);
        }
        self.leave_results_place(tab_id, cx);
        self.results_placement.places.remove(&tab_id);
        self.results_placement.hidden.remove(&tab_id);
        cx.notify();
    }

    /// Brings the Results tab or window of `tab_id` to the front.
    pub(in crate::app) fn show_moved_results(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(host) = self.results_host(tab_id) {
            self.activate_tab_ui(host, cx);
        } else if let Some(handle) = self.results_window(tab_id) {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
    }

    /// Closes the Results tab or window of `tab_id`, if it has one. The window closes after this
    /// update: this may run from a menu or a key inside that window, which is being updated then.
    fn leave_results_place(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(host) = self.results_host(tab_id) {
            self.results_placement.hosts.remove(&host);
            self.close_tab_unremembered(host, cx);
        }
        if let Some(handle) = self.results_placement.windows.remove(&tab_id) {
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            });
        }
    }

    /// Drops what the tab's result view made for the window that showed it (find bar, cell and
    /// document editors, plan and JSON viewers, chart controls, focus handles, the cached view): the
    /// next render makes them in the window that shows the results now. The rest of the view (column
    /// widths, selection, record view, chart spec) stays. Kept whole while a Mongo document edit
    /// holds unsaved text.
    fn forget_window_inputs(&mut self, tab_id: u64) {
        if self.view(tab_id).mongo.document_edit.is_some() {
            return;
        }
        if let Some(view) = self.tab_views.get_mut(&tab_id) {
            view.forget_window_bound();
        }
        self.columns_filter_inputs.remove(&tab_id);
    }

    /// A tab closes: a Results tab gives its results back to the query tab; a query tab takes its
    /// Results tab or window with it.
    pub(in crate::app) fn results_tab_closing(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(source) = self.results_placement.hosts.remove(&tab_id) {
            self.results_placement.places.remove(&source);
            cx.notify();
            return;
        }
        self.leave_results_place(tab_id, cx);
        self.results_placement.places.remove(&tab_id);
        self.results_placement.hidden.remove(&tab_id);
        // The panel's window goes with the tab too.
        self.close_cell_detail_window(tab_id, cx);
    }

    /// The user closed the results window of `tab_id`: they go back beside the editor.
    fn results_window_closed(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.results_placement.windows.remove(&tab_id).is_some() {
            self.forget_window_inputs(tab_id);
            self.results_placement.places.remove(&tab_id);
            cx.notify();
        }
    }

    /// Opens the window that shows `tab_id`'s results. Opened after this update: a window draws
    /// as it opens, and its results render through this `AppRoot`.
    fn open_results_window(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.open_tab_window(tab_id, WindowContent::Results, cx);
    }

    /// Opens the window that shows `tab_id`'s cell detail panel (same way as the results window).
    pub(in crate::app) fn open_cell_detail_window(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.open_tab_window(tab_id, WindowContent::CellDetail, cx);
    }

    fn open_tab_window(&mut self, tab_id: u64, content: WindowContent, cx: &mut Context<Self>) {
        let weak_root = cx.entity().downgrade();
        let (view, title, size, min_size): (gpui::AnyView, gpui::SharedString, _, _) = match content {
            WindowContent::Results => (
                self.tab_results_view(tab_id, cx).into(),
                self.workspace
                    .tab(tab_id)
                    .map(|tab| format!("{} · {}", t!("grid.results"), tab.title))
                    .unwrap_or_else(|| t!("grid.results").into_owned())
                    .into(),
                (960.0, 620.0),
                (480.0, 320.0),
            ),
            WindowContent::CellDetail => (
                cx.new(|_| CellDetailSurface::new(weak_root, tab_id)).into(),
                self.cell_detail_window_title(tab_id).into(),
                crate::cell_detail::POPUP_DEFAULT_SIZE,
                crate::cell_detail::POPUP_MIN_SIZE,
            ),
        };
        let root = cx.entity();
        cx.defer(move |cx| {
            let options = gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
                    None,
                    gpui::size(px(size.0), px(size.1)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title),
                    ..Default::default()
                }),
                is_resizable: true,
                window_min_size: Some(gpui::size(px(min_size.0), px(min_size.1))),
                ..Default::default()
            };
            let content_root = root.clone();
            let shown = view.clone();
            let opened = cx.open_window(options, move |window, cx| {
                let content = cx.new(|cx| ResultsWindow::new(content_root, tab_id, shown, content, window, cx));
                cx.new(|cx| gpui_component::Root::new(content, window, cx))
            });
            root.update(cx, |this, cx| {
                // Still wanted: the place may have changed while the window opened.
                let wanted = match content {
                    WindowContent::Results => this.results_place(tab_id) == ResultsPlace::Window,
                    WindowContent::CellDetail => this.cell_detail_in_window(tab_id),
                };
                match opened {
                    Ok(handle) if wanted => {
                        match content {
                            WindowContent::Results => {
                                this.results_placement.windows.insert(tab_id, handle.into());
                            }
                            WindowContent::CellDetail => {
                                if let Ok(surface) = view.downcast::<CellDetailSurface>() {
                                    this.results_placement
                                        .detail_windows
                                        .insert(tab_id, DetailWindow { handle: handle.into(), surface });
                                }
                            }
                        }
                        cx.notify();
                    }
                    Ok(handle) => {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                    Err(error) => {
                        match content {
                            WindowContent::Results if wanted => {
                                this.results_placement.places.remove(&tab_id);
                            }
                            WindowContent::CellDetail if wanted => {
                                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                                    runtime.cell_detail.leave_window();
                                }
                            }
                            _ => {}
                        }
                        let message = match content {
                            WindowContent::Results => {
                                t!("grid.could_not_open_results_window", error = error.to_string())
                            }
                            WindowContent::CellDetail => {
                                t!("grid.could_not_open_detail_window", error = error.to_string())
                            }
                        };
                        this.report_error(crate::error_log::Area::App, None, message);
                        cx.notify();
                    }
                }
            });
        });
    }

    /// `tab_id`'s cell detail panel is set to be in a window of its own (open or opening).
    pub(in crate::app) fn cell_detail_in_window(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.cell_detail.open && runtime.cell_detail.placement == CellDetailPlacement::Window)
    }

    /// The window that shows `tab_id`'s cell detail panel.
    pub(in crate::app) fn cell_detail_window(&self, tab_id: u64) -> Option<gpui::AnyWindowHandle> {
        self.results_placement.detail_windows.get(&tab_id).map(|window| window.handle)
    }

    /// "Row 22 · tab": what the panel of the tab shows, and which tab.
    fn cell_detail_window_title(&self, tab_id: u64) -> String {
        let tab = self.workspace.tab(tab_id).map(|tab| tab.title.as_str()).unwrap_or_default();
        let state = self.tab_cell_detail(tab_id);
        let what = match state {
            Some(state) if state.tab == CellDetailTab::Column => t!("grid.column_details"),
            Some(state) if state.detail.is_some() => {
                std::borrow::Cow::Owned(format!("{} {}", t!("grid.row"), state.detail.as_ref().map_or(0, |detail| detail.row_number)))
            }
            _ => t!("grid.cell_details"),
        };
        format!("{what} · {tab}")
    }

    /// The title the window of the panel's tab shows now (tests only).
    #[cfg(test)]
    pub(in crate::app) fn cell_detail_window_title_shown(&self, tab_id: u64, cx: &gpui::App) -> Option<String> {
        let window = self.results_placement.detail_windows.get(&tab_id)?;
        Some(window.surface.read(cx).title.clone())
    }

    /// Brings the panel's window to the front (picking Window again).
    pub(in crate::app) fn show_cell_detail_window(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(handle) = self.cell_detail_window(tab_id) {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
    }

    /// Keeps the panel's window in step with the state of its tab (runs on each notification of the
    /// tab): the window follows the selection, and closes, with the panel back where it was, once
    /// the panel is no longer a window one (closed, or put somewhere else). Free without a window.
    pub(in crate::app) fn sync_cell_detail_window(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(window) = self.results_placement.detail_windows.get(&tab_id) else {
            return;
        };
        if self.cell_detail_in_window(tab_id) {
            let id = window.surface.entity_id();
            let app: &mut gpui::App = cx;
            app.notify(id);
            return;
        }
        self.close_cell_detail_window(tab_id, cx);
    }

    /// Closes the panel's window (after this update: it may be running from a key or menu inside
    /// it) and puts the panel back where it was before the window.
    pub(in crate::app) fn close_cell_detail_window(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.leave_window();
        }
        if let Some(window) = self.results_placement.detail_windows.remove(&tab_id) {
            let handle = window.handle;
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            });
            self.forget_cell_detail_inputs(tab_id);
            self.notify_tab(tab_id, cx);
        }
    }

    /// The user closed the panel's window: the panel goes back where it was.
    fn cell_detail_window_closed(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.results_placement.detail_windows.remove(&tab_id).is_some() {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.cell_detail.leave_window();
            }
            self.forget_cell_detail_inputs(tab_id);
            self.notify_tab(tab_id, cx);
        }
    }

    /// Drops the viewers the panel made for the window that showed it: the next render makes them in
    /// the window that shows the panel now. A cell edit in progress is the user's text and stays.
    pub(in crate::app) fn forget_cell_detail_inputs(&mut self, tab_id: u64) {
        if let Some(view) = self.tab_views.get_mut(&tab_id) {
            view.forget_detail_viewers();
        }
    }

    /// The panel of every tab with a window follows the app (`AppRoot` itself was notified).
    pub(in crate::app) fn sync_cell_detail_windows(&mut self, cx: &mut Context<Self>) {
        if self.results_placement.detail_windows.is_empty() {
            return;
        }
        let tabs: Vec<u64> = self.results_placement.detail_windows.keys().copied().collect();
        for tab_id in tabs {
            self.sync_cell_detail_window(tab_id, cx);
        }
    }
}

/// The content of a window of its own: the query tab's result view or the tab's cell detail panel,
/// both of which render through `AppRoot`. `AppRoot`'s key handlers are not in this window's
/// dispatch tree, so the few commands such a window needs are handled here.
pub(in crate::app) struct ResultsWindow {
    root: gpui::WeakEntity<AppRoot>,
    tab_id: u64,
    content: WindowContent,
    view: gpui::AnyView,
    focus: gpui::FocusHandle,
    _root_released: gpui::Subscription,
}

impl ResultsWindow {
    fn new(
        root: gpui::Entity<AppRoot>,
        tab_id: u64,
        view: gpui::AnyView,
        content: WindowContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Closed with the close box: the results (the panel) go back to their tab.
        let weak = root.downgrade();
        window.on_window_should_close(cx, {
            let weak = weak.clone();
            move |_, cx| {
                let _ = weak.update(cx, |this, cx| match content {
                    WindowContent::Results => this.results_window_closed(tab_id, cx),
                    WindowContent::CellDetail => this.cell_detail_window_closed(tab_id, cx),
                });
                true
            }
        });
        // The app's window is gone: nothing renders these results any more.
        let released = cx.observe_release_in(&root, window, |_, _, window, _| window.remove_window());
        // Focused, so that the actions below reach this window with nothing else focused.
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            root: weak,
            tab_id,
            content,
            view,
            focus,
            _root_released: released,
        }
    }
}

impl Render for ResultsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drawing this window made it `AppRoot`'s current window (the results update `AppRoot`
        // while they render); the app's window takes that back once this frame is done.
        let root = self.root.entity_id();
        cx.defer(move |cx| give_app_window_its_root(root, cx));
        let tab_id = self.tab_id;
        let results = self.content == WindowContent::Results;
        let body = if results {
            self.view.clone().into_any_element()
        } else {
            // The panel is built when its tab notifies it, not on every draw of this window.
            self.view
                .clone()
                .cached(gpui::StyleRefinement::default().size_full())
                .into_any_element()
        };
        div()
            .debug_selector(move || if results { "results-window" } else { "cell-detail-window" }.into())
            .track_focus(&self.focus)
            .when(results, |this| {
                this.on_action(cx.listener(move |this, _: &RunActiveQuery, window, cx| {
                    let _ = this.root.update(cx, |root, cx| root.run_tab_as_its_run_button(tab_id, window, cx));
                }))
                .on_action(cx.listener(move |this, _: &CommitTransaction, window, cx| {
                    let _ = this.root.update(cx, |root, cx| root.end_tab_transaction(tab_id, transaction::TxEnd::Commit, window, cx));
                }))
                .on_action(cx.listener(move |this, _: &RollbackTransaction, window, cx| {
                    let _ = this.root.update(cx, |root, cx| root.end_tab_transaction(tab_id, transaction::TxEnd::Rollback, window, cx));
                }))
                .on_action(cx.listener(move |this, _: &ToggleAutoCommit, window, cx| {
                    let _ = this.root.update(cx, |root, cx| root.toggle_tab_commit_mode(tab_id, window, cx));
                }))
                .on_action(cx.listener(move |this, _: &OpenGridFind, window, cx| {
                    let _ = this.root.update(cx, |root, cx| root.open_grid_find(tab_id, window, cx));
                }))
            })
            // Closing the tab of the window gives the window's content back to the tab.
            .on_action(cx.listener(move |this, _: &CloseActiveTab, _, cx| {
                let content = this.content;
                let _ = this.root.update(cx, |root, cx| match content {
                    WindowContent::Results => root.bring_results_back(tab_id, cx),
                    WindowContent::CellDetail => root.close_cell_detail_window(tab_id, cx),
                });
            }))
            // Save and Format act on an editor, and this window has none: stop them here so they do
            // not reach the app's window and change a tab the user is not looking at.
            .on_action(cx.listener(|_, _: &SaveActiveSql, _, _| {}))
            .on_action(cx.listener(|_, _: &SaveSqlFileAs, _, _| {}))
            .on_action(cx.listener(|_, _: &FormatSql, _, _| {}))
            // Toggle Results in the results window goes back to the app's window.
            .on_action(cx.listener(|_, _: &ToggleResults, _, cx| {
                cx.defer(|cx| {
                    if let Some((main, _)) = super::super::menus::app_window(cx) {
                        let _ = main.update(cx, |_, window, _| window.activate_window());
                    }
                });
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .child(div().flex_1().min_h_0().min_w_0().flex().flex_col().child(body))
    }
}

/// The cached view of a cell detail panel in its window: it owns nothing, `render` forwards to
/// [`AppRoot::render_cell_detail_window`].
pub(in crate::app) struct CellDetailSurface {
    root: gpui::WeakEntity<AppRoot>,
    tab_id: u64,
    /// The window's title as last set: only a changed one is set again.
    title: String,
}

impl CellDetailSurface {
    fn new(root: gpui::WeakEntity<AppRoot>, tab_id: u64) -> Self {
        Self { root, tab_id, title: String::new() }
    }
}

impl Render for CellDetailSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let root_id = self.root.entity_id();
        // The panel renders through `AppRoot`, which makes this window its current one; the app's
        // window takes that back (see `give_app_window_its_root`).
        cx.defer(move |cx| give_app_window_its_root(root_id, cx));
        let title = &mut self.title;
        self.root
            .update(cx, |root, root_cx| {
                let new_title = root.cell_detail_window_title(tab_id);
                if new_title != *title {
                    window.set_window_title(&new_title);
                    *title = new_title;
                }
                root.render_cell_detail_window(tab_id, window, root_cx)
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

/// Draws the app's window again when `AppRoot` (`root`) took a results window as its current
/// window: any draw of the app's window gives it back (`AppRoot` renders under the window's root
/// view, which every draw renders), so only the window's root view is notified, not the whole window
/// (`Window::refresh` would render every cached view again, on every frame of the results window:
/// a scroll there would redraw the editors, tab strips and sidebar here). Nothing in the app's
/// window notifies a results view, so it does not loop.
fn give_app_window_its_root(root: gpui::EntityId, cx: &mut gpui::App) {
    let Some((main, _)) = super::super::menus::app_window(cx) else {
        return;
    };
    let current = cx.with_window(root, |window, _| window.window_handle());
    if current != Some(main) {
        let _ = main.update(cx, |root_view, _, cx| cx.notify(root_view.entity_id()));
    }
}

/// One checked item of a placement dropdown (the Results header's and the Error Log's): the place's
/// icon and `label`, with the debug selector `<selector>-<slug>`.
pub(in crate::app) fn place_menu_item(
    selector: &'static str,
    place: ResultsPlace,
    label: String,
    checked: bool,
    on_pick: impl Fn(&mut gpui::App) + 'static,
) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        div()
            .debug_selector(move || format!("{selector}-{}", place.slug()))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(Icon::empty().path(place.icon()).size(px(13.0)))
            .child(label.clone())
    })
    .checked(checked)
    .on_click(move |_, _, app| on_pick(app))
}

impl AppRoot {
    /// The control in the Results header that picks where the results of `tab_id` go.
    pub(in crate::app) fn render_results_place_menu(
        &self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.results_place(tab_id);
        let root = cx.entity().downgrade();
        let hide_key = shortcuts::label(&self.settings.shortcuts, "results.toggle");
        Button::new(("results-place", tab_id))
            .debug_selector(|| "results-place".into())
            .icon(Icon::empty().path(current.icon()).size(px(12.0)))
            .compact()
            .ghost()
            .tooltip(t!("grid.results_position"))
            .dropdown_menu(move |menu, _, _| {
                let menu = ResultsPlace::ALL.iter().fold(menu, |menu, place| {
                    let place = *place;
                    let root = root.clone();
                    menu.item(place_menu_item("results-place", place, place.label(), place == current, move |app| {
                        let _ = root.update(app, |this, cx| this.set_results_place(tab_id, place, cx));
                    }))
                });
                // Results in a Results tab or window are not "hidden": they have a place already.
                if !current.is_beside() {
                    return menu;
                }
                let root = root.clone();
                let hide_key = hide_key.clone();
                menu.separator().item(
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .debug_selector(|| "results-hide".into())
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(Icon::empty().path("icons/eye-off.svg").size(px(13.0)))
                            .child(div().flex_1().child(t!("grid.hide_results")))
                            .children(hide_key.clone().map(|key| {
                                div().text_size(px(11.0)).text_color(theme::text_muted()).child(key)
                            }))
                    })
                    .on_click(move |_, _, app| {
                        let _ = root.update(app, |this, cx| this.toggle_results(tab_id, cx));
                    }),
                )
            })
            .into_any_element()
    }

    /// What a query tab shows under its editor while its results are hidden (Toggle Results): that
    /// they are, the key, and a button that shows them again.
    pub(in crate::app) fn render_hidden_results_note(&self, tab_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let key = shortcuts::label(&self.settings.shortcuts, "results.toggle");
        div()
            .debug_selector(|| "results-hidden-note".into())
            .flex_shrink_0()
            .h(px(36.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .bg(theme::chrome())
            .border_t_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(Icon::empty().path("icons/eye-off.svg").size(px(13.0)))
            .child(div().flex_1().min_w_0().truncate().child(t!("grid.results_are_hidden")))
            .child(
                Button::new(("results-hidden-show", tab_id))
                    .debug_selector(|| "results-hidden-show".into())
                    .label(t!("grid.show_hidden_results"))
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_results(tab_id, cx))),
            )
            .children(key.map(|key| {
                div()
                    .debug_selector(|| "results-hidden-key".into())
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .child(key)
            }))
            .into_any_element()
    }

    /// What a query tab shows where its results were, while they are in a tab or a window of
    /// their own: where they are, a button that shows them and one that brings them back.
    pub(in crate::app) fn render_moved_results_note(
        &self,
        tab_id: u64,
        place: ResultsPlace,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let text = match place {
            ResultsPlace::Window => t!("grid.results_are_in_a_window"),
            _ => t!("grid.results_are_in_a_tab"),
        };
        div()
            .debug_selector(|| "results-moved-note".into())
            .flex_shrink_0()
            .h(px(36.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .bg(theme::chrome())
            .border_t_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(Icon::empty().path(place.icon()).size(px(13.0)))
            .child(div().flex_1().min_w_0().truncate().child(text))
            .child(
                Button::new(("results-moved-show", tab_id))
                    .debug_selector(|| "results-moved-show".into())
                    .label(t!("grid.show_results"))
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| this.show_moved_results(tab_id, cx))),
            )
            .child(
                Button::new(("results-moved-back", tab_id))
                    .debug_selector(|| "results-moved-back".into())
                    .label(t!("grid.bring_results_back"))
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| this.bring_results_back(tab_id, cx))),
            )
            .into_any_element()
    }
}
