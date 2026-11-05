//! Cmd+F find bar for the results grid (SQL results, table data, Mongo find
//! table view).
//!
//! Matching is case-insensitive substring over the current page's cells and
//! column names with optional "Match
//! case" / "Whole cell". The scan runs on the background executor against the
//! grid source's raw data (`GridState::search_cells`), never through
//! `cell_text`, with a cancel flag + generation so a newer query supersedes an
//! older scan. Matches are stored sorted row-major so the per-cell highlight
//! lookup while rendering is a binary search with no allocation.
//!
//! One find bar exists per window and follows the active tab: switching tabs
//! (or any change of the grid page / visible columns) re-runs the same query on
//! the new data; there is no per-tab find state to keep in sync.

use super::super::*;
use crate::grid::{GridSearchOptions, GridSearchQuery};
use gpui_component::input::InputEvent;

/// Matches (header + cells) collected before the scan stops; the counter then
/// reads `10000+`.
pub(in crate::app) const MAX_FIND_MATCHES: usize = 10_000;
const FIND_DEBOUNCE: Duration = Duration::from_millis(150);

/// Result of one scan. `cells` is sorted row-major (the `CellPosition`
/// ordering) by construction, so lookups can binary search.
#[derive(Debug, Default)]
pub(in crate::app) struct FindMatches {
    /// Display columns whose header name matches, ascending.
    pub(in crate::app) columns: Vec<usize>,
    /// Matching cells in display coordinates, row-major.
    pub(in crate::app) cells: Vec<CellPosition>,
    /// The scan stopped at the match cap.
    pub(in crate::app) capped: bool,
}

impl FindMatches {
    pub(in crate::app) fn total(&self) -> usize {
        self.columns.len() + self.cells.len()
    }

    /// Matching cells of one display row, ascending by column. Two binary
    /// searches, no allocation; empty when there are no matches.
    pub(in crate::app) fn row_cells(&self, row: usize) -> &[CellPosition] {
        if self.cells.is_empty() {
            return &[];
        }
        let start = self.cells.partition_point(|cell| cell.row < row);
        let len = self.cells[start..].partition_point(|cell| cell.row == row);
        &self.cells[start..start + len]
    }

    pub(in crate::app) fn is_header_match(&self, column: usize) -> bool {
        !self.columns.is_empty() && self.columns.binary_search(&column).is_ok()
    }

    /// Match `index` of the combined sequence: header matches first, then
    /// cells.
    pub(in crate::app) fn target(&self, index: usize) -> Option<FindTarget> {
        match index.checked_sub(self.columns.len()) {
            None => self.columns.get(index).copied().map(FindTarget::Column),
            Some(cell) => self.cells.get(cell).copied().map(FindTarget::Cell),
        }
    }
}

/// What the current match points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum FindTarget {
    Column(usize),
    Cell(CellPosition),
}

/// The grid page a scan ran over, to notice when results were replaced.
struct ScannedGrid {
    grid: GridState<GridContentSource>,
    visible_columns: Vec<usize>,
}

impl FindState {
    /// Forgets the grid page the last scan ran over (it pins that page's
    /// buffers, which a driver unload has to wait for).
    pub(in crate::app) fn forget_scanned_grid(&mut self) {
        self.scanned = None;
    }
}

pub(in crate::app) struct FindState {
    pub(in crate::app) open: bool,
    pub(in crate::app) options_open: bool,
    pub(in crate::app) options: GridSearchOptions,
    pub(in crate::app) input: Option<gpui::Entity<InputState>>,
    /// Debounced, trimmed query the current matches belong to.
    pub(in crate::app) query: String,
    pub(in crate::app) matches: FindMatches,
    /// Index into the combined (header, then cell) match sequence.
    pub(in crate::app) current: Option<usize>,
    pub(in crate::app) scanning: bool,
    pub(in crate::app) last_scan: Option<Duration>,
    pub(in crate::app) max_matches: usize,
    pub(in crate::app) generation: u64,
    cancel: Option<Arc<AtomicBool>>,
    scan_task: Option<Task<()>>,
    debounce_task: Option<Task<()>>,
    scanned: Option<ScannedGrid>,
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            open: false,
            options_open: false,
            options: GridSearchOptions::default(),
            input: None,
            query: String::new(),
            matches: FindMatches::default(),
            current: None,
            scanning: false,
            last_scan: None,
            max_matches: MAX_FIND_MATCHES,
            generation: 0,
            cancel: None,
            scan_task: None,
            debounce_task: None,
            scanned: None,
        }
    }
}

/// What a finished background scan hands back to its tab.
pub(in crate::app) struct FindScan {
    pub(in crate::app) generation: u64,
    pub(in crate::app) matches: FindMatches,
    pub(in crate::app) cancelled: bool,
    pub(in crate::app) elapsed: Duration,
}

impl FindState {
    pub(in crate::app) fn current_target(&self) -> Option<FindTarget> {
        self.current.and_then(|index| self.matches.target(index))
    }

    /// The current match when it is a cell (O(1)).
    pub(in crate::app) fn current_cell(&self) -> Option<CellPosition> {
        match self.current_target() {
            Some(FindTarget::Cell(position)) => Some(position),
            _ => None,
        }
    }

    pub(in crate::app) fn counter_text(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        let total = self.matches.total();
        if self.scanning && total == 0 {
            return "…".into();
        }
        let position = self.current.map_or(0, |index| index + 1);
        let plus = if self.matches.capped || total >= self.max_matches {
            "+"
        } else {
            ""
        };
        format!("{position} / {total}{plus}")
    }

    /// Stops any scan/debounce and forgets results (not the query).
    fn cancel_work(&mut self) {
        self.generation += 1;
        if let Some(flag) = self.cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        self.scan_task = None;
        self.debounce_task = None;
        self.scanning = false;
    }

    fn clear_results(&mut self) {
        self.matches = FindMatches::default();
        self.current = None;
    }
}

impl AppRoot {
    /// True while the table grid (not the document/columns views or an
    /// editor-only tab) is what the results area shows, i.e. the find bar has
    /// something to search.
    pub(in crate::app) fn grid_pane_active(&self, tab_id: u64) -> bool {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return false;
        };
        if tab.kind != TabKind::Query || tab.query_mode.is_mongo_aggregate() {
            return false;
        }
        let editor_only = !tab.query_mode.is_mongo_find()
            && !tab.query_mode.is_table_data()
            && !self
                .workspace
                .runtime(tab.id)
                .is_some_and(|runtime| runtime.has_run());
        !editor_only
            && self.grid(tab_id).column_count() > 0
            && !self.columns_presentation_active(tab_id)
            && !self.diagram_presentation_active(tab_id)
            && !self.plan_presentation_active(tab_id)
            && !self.chart_presentation_active(tab_id)
            && !self.mongo_documents_view_active(tab_id)
    }

    /// Source column of every display column, in display order.
    fn find_display_columns(&self, tab_id: u64) -> Vec<usize> {
        let visible = &self.view(tab_id).grid_ui.visible_columns;
        if visible.is_empty() {
            (0..self.grid(tab_id).column_count()).collect()
        } else {
            visible.clone()
        }
    }

    fn ensure_find_input(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some(input) = self.view(tab_id).grid_ui.find.input.clone() {
            return input;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search…"));
        cx.subscribe_in(
            &input,
            window,
            move |this: &mut Self,
                  _: &gpui::Entity<InputState>,
                  event: &InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| match event {
                InputEvent::Change => this.find_text_changed(tab_id, cx),
                InputEvent::PressEnter { shift, .. } => {
                    this.find_enter(tab_id, *shift, window, cx)
                }
                _ => {}
            },
        )
        .detach();
        self.view_mut(tab_id).grid_ui.find.input = Some(input.clone());
        input
    }

    /// Cmd+F / palette: [`Self::open_grid_find`] on the active tab.
    pub(in crate::app) fn open_active_grid_find(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) {
            self.open_grid_find(tab_id, window, cx);
        }
    }

    /// Open the tab's find bar, or focus it and select its text if already open.
    /// Does nothing unless the results grid is what the tab shows.
    pub(in crate::app) fn open_grid_find(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A diagram has its own search field.
        if self.focus_diagram_search(tab_id, window, cx) || !self.grid_pane_active(tab_id) {
            return;
        }
        let input = self.ensure_find_input(tab_id, window, cx);
        self.view_mut(tab_id).grid_ui.find.open = true;
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        self.notify_tab(tab_id, cx);
    }

    /// Esc / close button: hides the bar, forgets the query and matches, and
    /// returns focus to the grid.
    pub(in crate::app) fn close_grid_find(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.view(tab_id).grid_ui.find.open {
            return;
        }
        let view = self.view_mut(tab_id);
        let find = &mut view.grid_ui.find;
        find.cancel_work();
        find.open = false;
        find.options_open = false;
        find.query.clear();
        find.scanned = None;
        find.clear_results();
        if let Some(input) = find.input.clone() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        view.grid_focus(cx).focus(window, cx);
        self.notify_tab(tab_id, cx);
    }

    fn find_input_text(&self, tab_id: u64, cx: &gpui::App) -> String {
        self.view(tab_id)
            .grid_ui
            .find
            .input
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default()
    }

    /// Typing: an emptied field clears immediately, anything else waits for a
    /// 150 ms pause (the debounce task is replaced on every keystroke).
    fn find_text_changed(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let text = self.find_input_text(tab_id, cx);
        let find = &mut self.view_mut(tab_id).grid_ui.find;
        find.debounce_task = None;
        if text == find.query {
            return;
        }
        if text.is_empty() {
            self.commit_find_query(tab_id, cx);
            return;
        }
        find.debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIND_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.commit_find_query(tab_id, cx));
        }));
    }

    fn commit_find_query(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let text = self.find_input_text(tab_id, cx);
        let find = &mut self.view_mut(tab_id).grid_ui.find;
        find.debounce_task = None;
        if text == find.query {
            return;
        }
        find.query = text;
        self.start_find_scan(tab_id, true, cx);
        self.notify_tab(tab_id, cx);
    }

    /// Enter / Shift+Enter in the field. A query still waiting on the
    /// debounce is committed first (jumping to its first match); otherwise
    /// step to the next/previous match.
    fn find_enter(
        &mut self,
        tab_id: u64,
        backwards: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find_input_text(tab_id, cx) != self.view(tab_id).grid_ui.find.query {
            self.commit_find_query(tab_id, cx);
        } else {
            self.find_navigate(tab_id, if backwards { -1 } else { 1 }, cx);
        }
    }

    /// Steps to the next (`1`) / previous (`-1`) match, wrapping around.
    pub(in crate::app) fn find_navigate(&mut self, tab_id: u64, delta: isize, cx: &mut Context<Self>) {
        let find = &self.view(tab_id).grid_ui.find;
        let total = find.matches.total();
        if total == 0 {
            return;
        }
        let next = match find.current.filter(|index| *index < total) {
            Some(index) => (index as isize + delta).rem_euclid(total as isize) as usize,
            None if delta < 0 => total - 1,
            None => 0,
        };
        self.goto_find_match(tab_id, next, cx);
        self.notify_tab(tab_id, cx);
    }

    /// Makes `index` the current match: a cell becomes the grid selection and
    /// is scrolled into view on both axes; a header match scrolls its column
    /// into view.
    fn goto_find_match(&mut self, tab_id: u64, index: usize, cx: &mut Context<Self>) {
        let find = &mut self.view_mut(tab_id).grid_ui.find;
        let target = find.matches.target(index);
        find.current = target.map(|_| index);
        match target {
            Some(FindTarget::Column(column)) => self.view_mut(tab_id).scroll_to_column(column),
            Some(FindTarget::Cell(position)) => {
                self.select_grid_cell(tab_id, position);
                let view = self.view_mut(tab_id);
                view.scroll_to_row(position.row);
                view.scroll_to_column(position.column);
            }
            None => {}
        }
        self.notify_tab(tab_id, cx);
    }

    /// Toggles "Match case" (`false`) / "Whole cell" (`true`) and re-runs.
    fn set_find_option(&mut self, tab_id: u64, whole_cell: bool, value: bool, cx: &mut Context<Self>) {
        let options = &mut self.view_mut(tab_id).grid_ui.find.options;
        if whole_cell {
            options.whole_cell = value;
        } else {
            options.match_case = value;
        }
        self.start_find_scan(tab_id, true, cx);
        self.notify_tab(tab_id, cx);
    }

    /// Re-runs the committed query when the grid page or its visible columns
    /// changed under the bar (new query/page/refresh, sort, tab switch,
    /// column picker). One pointer compare plus a short vector compare per
    /// frame; the scan itself is asynchronous.
    pub(in crate::app) fn sync_find_with_grid(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let view = self.view(tab_id);
        let find = &view.grid_ui.find;
        if !find.open || find.query.is_empty() {
            return;
        }
        let unchanged = find.scanned.as_ref().is_some_and(|scanned| {
            scanned.grid.same_page(self.grid(tab_id))
                && scanned.visible_columns == view.grid_ui.visible_columns
        });
        if !unchanged {
            self.start_find_scan(tab_id, false, cx);
        }
    }

    /// Starts a scan of the current page for the committed query, superseding
    /// any running one. Header matches are computed here (a handful of
    /// names); the cell scan runs on the background executor. `navigate`
    /// jumps to the first match on completion. Does not notify, so it is safe
    /// to call from render.
    fn start_find_scan(&mut self, tab_id: u64, navigate: bool, cx: &mut Context<Self>) {
        let columns = self.find_display_columns(tab_id);
        let grid = self.grid(tab_id).clone();
        let visible_columns = self.view(tab_id).grid_ui.visible_columns.clone();
        let find = &mut self.view_mut(tab_id).grid_ui.find;
        find.cancel_work();
        find.clear_results();
        find.scanned = Some(ScannedGrid {
            grid: grid.clone(),
            visible_columns,
        });
        let Some(query) = GridSearchQuery::new(&find.query, find.options) else {
            find.last_scan = None;
            return;
        };
        let generation = find.generation;
        let mut scratch = String::new();
        let header_matches: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter_map(|(display, &source)| {
                let name = grid.column_name(source)?;
                query.matches(name, &mut scratch).then_some(display)
            })
            .collect();
        let cell_limit = find.max_matches.saturating_sub(header_matches.len());
        let cancel = Arc::new(AtomicBool::new(false));
        find.cancel = Some(cancel.clone());
        find.scanning = true;
        let scan = cx.background_executor().spawn(async move {
            let started = Instant::now();
            // One extra match tells "exactly at the cap" from "more exist".
            let (mut cells, _) = grid.search_cells(&query, &columns, cell_limit + 1, &cancel);
            let capped = cells.len() > cell_limit;
            cells.truncate(cell_limit);
            (cells, capped, cancel.load(Ordering::Relaxed), started.elapsed())
        });
        find.scan_task = Some(cx.spawn(async move |this, cx| {
            let (cells, capped, cancelled, elapsed) = scan.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_find_scan(
                    tab_id,
                    FindScan {
                        generation,
                        matches: FindMatches {
                            columns: header_matches,
                            cells,
                            capped,
                        },
                        cancelled,
                        elapsed,
                    },
                    navigate,
                    cx,
                );
            });
        }));
    }

    pub(in crate::app) fn finish_find_scan(
        &mut self,
        tab_id: u64,
        scan: FindScan,
        navigate: bool,
        cx: &mut Context<Self>,
    ) {
        let FindScan {
            generation,
            matches,
            cancelled,
            elapsed,
        } = scan;
        let find = &mut self.view_mut(tab_id).grid_ui.find;
        // A newer scan (or a close) superseded this one.
        if find.generation != generation || cancelled {
            return;
        }
        find.scanning = false;
        find.cancel = None;
        find.last_scan = Some(elapsed);
        find.matches = matches;
        find.current = (find.matches.total() > 0).then_some(0);
        if navigate && find.current.is_some() {
            self.goto_find_match(tab_id, 0, cx);
        }
        self.notify_tab(tab_id, cx);
    }

    /// The floating find bar, top-right of the grid area, within the header band:
    /// `[>] [search input] [n / total] [^] [v] [x]`, plus the "Match case" /
    /// "Whole cell" options row when expanded. It occludes only its own
    /// bounds, so the grid keeps every click outside it.
    pub(in crate::app) fn render_grid_find_bar(
        &self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let find = &self.view(tab_id).grid_ui.find;
        if !find.open {
            return None;
        }
        let input = find.input.as_ref()?;
        let has_matches = find.matches.total() > 0;
        let expander = Button::new("grid-find-expand")
            .ghost()
            .xsmall()
            .icon(
                Icon::new(if find.options_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(12.0)),
            )
            .tooltip("Search options")
            .debug_selector(|| "grid-find-expand".into())
            .on_click(cx.listener(move |this, _, _, cx| {
                let find = &mut this.view_mut(tab_id).grid_ui.find;
                find.options_open = !find.options_open;
                this.notify_tab(tab_id, cx);
            }));
        let previous = Button::new("grid-find-prev")
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::ChevronDown).rotate(gpui::percentage(0.5)).size(px(12.0)))
            .tooltip("Previous match (Shift+Enter)")
            .disabled(!has_matches)
            .debug_selector(|| "grid-find-prev".into())
            .on_click(cx.listener(move |this, _, _, cx| this.find_navigate(tab_id, -1, cx)));
        let next = Button::new("grid-find-next")
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::ChevronDown).size(px(12.0)))
            .tooltip("Next match (Enter)")
            .disabled(!has_matches)
            .debug_selector(|| "grid-find-next".into())
            .on_click(cx.listener(move |this, _, _, cx| this.find_navigate(tab_id, 1, cx)));
        let close = Button::new("grid-find-close")
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::Close).size(px(12.0)))
            .tooltip("Close (Esc)")
            .debug_selector(|| "grid-find-close".into())
            .on_click(cx.listener(move |this, _, window, cx| this.close_grid_find(tab_id, window, cx)));
        let counter = div()
            .debug_selector(|| "grid-find-count".into())
            .min_w(px(52.0))
            .flex_shrink_0()
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .child(find.counter_text());

        let bar = div()
            .id("grid-find-bar")
            .debug_selector(|| "grid-find-bar".into())
            .key_context("grid-find")
            .absolute()
            // Inside the header band (the box overlays the header row), so no
            // data row is covered; the list's own scrollbar starts below it.
            .top(px(2.0))
            .right(px(16.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(2.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .rounded(px(6.0))
            .shadow_md()
            .occlude()
            .on_action(cx.listener(move |this, _: &CloseGridFind, window, cx| {
                this.close_grid_find(tab_id, window, cx);
            }))
            .on_action(cx.listener(move |this, _: &gpui_component::input::Escape, window, cx| {
                this.close_grid_find(tab_id, window, cx);
            }))
            .on_action(cx.listener(move |this, _: &OpenGridFind, window, cx| {
                this.open_grid_find(tab_id, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(expander)
                    .child(
                        div()
                            .debug_selector(|| "grid-find-input".into())
                            .w(px(200.0))
                            .child(
                                Input::new(input)
                                    .prefix(
                                        Icon::new(IconName::Search)
                                            .size(px(13.0))
                                            .text_color(theme::text_muted()),
                                    )
                                    .with_size(Size::Small),
                            ),
                    )
                    .child(counter)
                    .child(previous)
                    .child(next)
                    .child(close),
            );
        let bar = bar.when(find.options_open, |this| {
            this.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(6.0))
                    .pb(px(2.0))
                    .child(
                        div().debug_selector(|| "grid-find-match-case".into()).child(
                            Checkbox::new("grid-find-match-case")
                                .label("Match case")
                                .checked(find.options.match_case)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    this.set_find_option(tab_id, false, *checked, cx);
                                })),
                        ),
                    )
                    .child(
                        div().debug_selector(|| "grid-find-whole-cell".into()).child(
                            Checkbox::new("grid-find-whole-cell")
                                .label("Whole cell")
                                .checked(find.options.whole_cell)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    this.set_find_option(tab_id, true, *checked, cx);
                                })),
                        ),
                    ),
            )
        });
        Some(bar.into_any_element())
    }
}
