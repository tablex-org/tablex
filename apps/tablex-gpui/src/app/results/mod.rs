use super::*;
use crate::pinned_column::PinnedColumn;

pub(super) mod context_menu;
pub(super) mod binary_view;
pub(super) mod chart;
pub(super) mod column_picker;
pub(super) mod column_type;
pub(super) mod columns_view;
pub(super) mod copy;
pub(super) mod detail;
pub(super) mod detail_view;
pub(super) mod diagram;
pub(super) mod editing;
pub(super) mod export;
pub(super) mod find;
pub(super) mod footer;
pub(super) mod grid;
pub(super) mod json_view;
pub(super) mod mongo;
pub(super) mod mutations;
pub(super) mod pinning;
pub(super) mod plan;
pub(super) mod properties;
pub(super) mod properties_edit;
pub(super) mod query_result;
pub(super) mod record_view;
pub(super) mod snapshot;
pub(super) mod sticky;
pub(super) mod snowflake;
pub(super) mod surface;
pub(super) mod sql_columns;
pub(super) mod sql_filter;
pub(super) mod sql_table;

pub(super) use column_type::grid_column_type_color;
use detail::DetailUiState;

/// A window-sized layer above the page (a deferred layer anchored at the
/// window origin): click-away backdrops of popovers that must cover everything
/// but live inside a cached results view, whose own bounds are smaller.
pub(super) fn window_overlay(window: &Window, content: impl IntoElement) -> AnyElement {
    let viewport = window.viewport_size();
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(div().w(viewport.width).h(viewport.height).child(content)),
    )
    .into_any_element()
}

/// User preferences of the result grid: shared by every tab, so they live on
/// `AppRoot`, not in a `TabView`.
pub(super) struct GridPrefs {
    pub(super) copy_default: copy::CopyFormat,
    pub(super) copy_default_path: Option<std::path::PathBuf>,
    pub(super) snowflake_dir: Option<std::path::PathBuf>,
    pub(super) snowflake_request: u64,
}

impl GridPrefs {
    pub(super) fn new(scenario_root: Option<&std::path::Path>) -> Self {
        let copy_default_path = scenario_root
            .map(|root| root.join("grid-copy-default.json"))
            .or_else(copy::copy_default_path);
        Self {
            copy_default: copy_default_path
                .as_deref()
                .map_or(copy::CopyFormat::Smart, copy::read_copy_default),
            copy_default_path,
            snowflake_dir: scenario_root.map(|root| root.join("snowflake")),
            snowflake_request: 0,
        }
    }
}

/// Grid state of one tab.
pub(super) struct GridUiState {
    pub(super) layout: crate::grid::ColumnLayout,
    pub(super) visible_columns: Vec<usize>,
    /// Leading display columns that are pinned (`visible_columns` is ordered
    /// pinned first; the names live in `Tab::pinned_columns`).
    pub(super) pinned: usize,
    /// The grid area shows the current row as a record (see `record_view`).
    pub(super) record: bool,
    /// Pinned columns the last frame drew in the sticky block: `pinned`, or 0
    /// when they would cover most of the viewport (then they scroll like the
    /// rest and stay left of it).
    pub(super) frozen: usize,
    pub(super) column_resize: Option<ColumnResizeState>,
    pub(super) selection: std::rc::Rc<std::cell::RefCell<crate::grid::GridSelection>>,
    pub(super) inline_edit: Option<editing::InlineCellEdit>,
    pub(super) context_cell: Option<CellPosition>,
    /// Display column of the header that was right-clicked (its menu's target).
    pub(super) context_header: Option<usize>,
    /// Display column whose header sort menu is open (its button stays shown).
    pub(super) sort_menu: Option<usize>,
    /// Footer SUM/AVG cache, recomputed only when its key changes.
    pub(super) selection_stats: Option<(footer::SelectionStatsKey, crate::selection_stats::SelectionStats)>,
    /// Resolved header type labels for the tab's result (see `column_type`).
    pub(super) column_types: Option<column_type::ColumnTypeLabels>,
    /// Cmd+F find bar state (see `find`).
    pub(super) find: find::FindState,
}

impl Default for GridUiState {
    fn default() -> Self {
        Self {
            layout: crate::grid::ColumnLayout::new(0),
            visible_columns: Vec::new(),
            pinned: 0,
            record: false,
            frozen: 0,
            column_resize: None,
            selection: std::rc::Rc::new(std::cell::RefCell::new(
                crate::grid::GridSelection::default(),
            )),
            inline_edit: None,
            context_cell: None,
            context_header: None,
            sort_menu: None,
            selection_stats: None,
            column_types: None,
            find: find::FindState::default(),
        }
    }
}

/// The query-history strip under a tab's results.
#[derive(Default)]
pub(super) struct HistoryPanel {
    pub(super) visible: bool,
    pub(super) entries: Vec<engine_storage::history::HistoryEntry>,
}

/// Everything one tab's result area owns besides its data (`TabRuntime`):
/// grid layout / selection / find / inline edit, the cell-detail editor, scroll
/// and focus handles, the JSON viewers and the Mongo toolbar's drag and key
/// state. `AppRoot::tab_views` holds one per tab, created on first use and
/// dropped with the tab, so two tabs shown at once never share a scroll
/// offset, selection or find bar. Entity handles that are already keyed by tab
/// id (filter inputs, SQL bar editors, aggregate stages) stay in their
/// registries on `AppRoot`.
///
/// Logic that needs only this state lives here; anything that also reads the
/// workspace is an `AppRoot` method taking an explicit `tab_id`.
pub(super) struct TabView {
    pub(super) grid_ui: GridUiState,
    pub(super) detail_ui: DetailUiState,
    /// Charts from a result: settings, prepared chart and export notice (see `chart`).
    pub(super) chart: chart::ChartTabState,
    pub(super) json_views: json_view::JsonViews,
    pub(super) mongo: mongo::MongoTabUi,
    pub(super) history: HistoryPanel,
    /// Enter is held in a SQL bar input (suppresses key-repeat runs).
    pub(super) sql_enter_held: bool,
    pub(super) grid_scroll: UniformListScrollHandle,
    /// The record view's field list.
    pub(super) record_scroll: UniformListScrollHandle,
    pub(super) grid_horizontal_scroll: ScrollHandle,
    pub(super) result_action_scroll: ScrollHandle,
    pub(super) mongo_document_scroll: UniformListScrollHandle,
    pub(super) mongo_detail_scroll: UniformListScrollHandle,
    pub(super) cell_detail_row_scroll: UniformListScrollHandle,
    pub(super) column_picker_scroll: UniformListScrollHandle,
    pub(super) columns_scroll: UniformListScrollHandle,
    /// Explain plan view: node list scroll, keyboard focus and the Raw text editor.
    pub(super) plan_scroll: UniformListScrollHandle,
    pub(super) plan_raw: Option<plan::PlanRawEditor>,
    /// MongoDB shell run: scroll of the Output pane and of the text result.
    pub(super) shell_output_scroll: UniformListScrollHandle,
    pub(super) shell_text_scroll: UniformListScrollHandle,
    plan_focus: Option<FocusHandle>,
    grid_focus: Option<FocusHandle>,
    columns_focus: Option<FocusHandle>,
    /// The Statistics table of the tab's script run (see `script_run`).
    pub(super) script_stats: Option<gpui::Entity<gpui_component::table::TableState<super::script_run::StatsDelegate>>>,
    /// The cached view that renders this tab's result area (see `surface`).
    pub(super) surface: Option<gpui::Entity<surface::TabResults>>,
    /// Last measured width of the docked cell-detail panel (0 = not measured).
    pub(super) cell_detail_width: std::rc::Rc<std::cell::Cell<f32>>,
    /// Last measured bounds of the results area (rails + grid), used to keep
    /// the floating cell-detail popup inside it.
    pub(super) results_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
}

impl Default for TabView {
    fn default() -> Self {
        Self {
            grid_ui: GridUiState::default(),
            detail_ui: DetailUiState::default(),
            chart: chart::ChartTabState::default(),
            json_views: json_view::JsonViews::default(),
            mongo: mongo::MongoTabUi::default(),
            history: HistoryPanel::default(),
            sql_enter_held: false,
            grid_scroll: UniformListScrollHandle::new(),
            record_scroll: UniformListScrollHandle::new(),
            grid_horizontal_scroll: ScrollHandle::new(),
            result_action_scroll: ScrollHandle::new(),
            mongo_document_scroll: UniformListScrollHandle::new(),
            mongo_detail_scroll: UniformListScrollHandle::new(),
            cell_detail_row_scroll: UniformListScrollHandle::new(),
            column_picker_scroll: UniformListScrollHandle::new(),
            columns_scroll: UniformListScrollHandle::new(),
            plan_scroll: UniformListScrollHandle::new(),
            plan_raw: None,
            shell_output_scroll: UniformListScrollHandle::new(),
            shell_text_scroll: UniformListScrollHandle::new(),
            plan_focus: None,
            grid_focus: None,
            columns_focus: None,
            script_stats: None,
            surface: None,
            cell_detail_width: Default::default(),
            results_bounds: Default::default(),
        }
    }
}

impl TabView {
    /// Focus target of the grid (created on first use: a handle needs `App`).
    pub(super) fn grid_focus(&mut self, cx: &gpui::App) -> FocusHandle {
        self.grid_focus.get_or_insert_with(|| cx.focus_handle()).clone()
    }

    /// Focus target of the Explain plan view.
    pub(super) fn plan_focus(&mut self, cx: &gpui::App) -> FocusHandle {
        self.plan_focus.get_or_insert_with(|| cx.focus_handle()).clone()
    }

    /// Focus target of the Properties / Columns view.
    pub(super) fn columns_focus(&mut self, cx: &gpui::App) -> FocusHandle {
        self.columns_focus.get_or_insert_with(|| cx.focus_handle()).clone()
    }

    /// Columns the grid shows: the visible subset, or every source column.
    pub(super) fn display_column_count(&self, source_columns: usize) -> usize {
        if self.grid_ui.visible_columns.is_empty() {
            source_columns
        } else {
            self.grid_ui.visible_columns.len()
        }
    }

    pub(super) fn source_column_for_display(
        &self,
        display_column: usize,
        source_columns: usize,
    ) -> Option<usize> {
        self.grid_ui
            .visible_columns
            .get(display_column)
            .copied()
            .or_else(|| (display_column < source_columns).then_some(display_column))
    }

    /// Display cell under a window-space point, from the painted frame's
    /// list bounds (which already include the horizontal scroll translation)
    /// plus the list's vertical offset. `None` over the gutter, the empty
    /// area below the last row, or outside the grid.
    pub(super) fn cell_at(
        &self,
        point: gpui::Point<gpui::Pixels>,
        row_count: usize,
        source_columns: usize,
    ) -> Option<CellPosition> {
        let (bounds, offset) = {
            let state = self.grid_scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let x = f32::from(point.x - bounds.origin.x - offset.x);
        let y = f32::from(point.y - bounds.origin.y - offset.y);
        if y < 0.0 {
            return None;
        }
        let row = (y / theme::ROW_HEIGHT) as usize;
        if row >= row_count {
            return None;
        }
        let column = self.column_at_x(x, source_columns)?;
        Some(CellPosition { row, column })
    }

    /// Display column under a content-space x (from the grid's painted left
    /// edge). The sticky block (gutter and pinned columns) is painted at the
    /// viewport's left edge, not where the content puts it. `None` over the
    /// gutter or past the last column.
    pub(super) fn column_at_x(&self, x: f32, source_columns: usize) -> Option<usize> {
        let layout = &self.grid_ui.layout;
        let frozen = self.grid_ui.frozen;
        let scroll_left = -f32::from(self.grid_horizontal_scroll.offset().x);
        let sticky_width = layout.gutter_width + layout.leading_width(frozen);
        let column = if frozen > 0 && x >= scroll_left && x < scroll_left + sticky_width {
            layout.column_at(x - scroll_left)?
        } else {
            layout.column_at(x)?
        };
        (column < self.display_column_count(source_columns)).then_some(column)
    }

    /// Display column of the header under a window-space point: the body list
    /// shares the header's left edge, so its painted bounds locate it.
    pub(super) fn header_column_at(
        &self,
        point: gpui::Point<gpui::Pixels>,
        source_columns: usize,
    ) -> Option<usize> {
        let (bounds, offset) = {
            let state = self.grid_scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        self.column_at_x(f32::from(point.x - bounds.origin.x - offset.x), source_columns)
    }

    /// Scrolls the grid horizontally so the display column is fully visible
    /// (the minimal move, like keyboard navigation).
    pub(super) fn scroll_to_column(&mut self, column: usize) {
        if column >= self.grid_ui.layout.widths.len() {
            return;
        }
        // A pinned column is always in view.
        if column < self.grid_ui.frozen {
            return;
        }
        let layout = &self.grid_ui.layout;
        let offset = self.grid_horizontal_scroll.offset();
        let max_offset = self.grid_horizontal_scroll.max_offset();
        let viewport_width = layout.content_width() - f32::from(max_offset.x);
        // The sticky block covers the left of the viewport: a column is hidden
        // under it until it is right of the block.
        let frozen_width = if self.grid_ui.frozen > 0 {
            layout.gutter_width + layout.leading_width(self.grid_ui.frozen)
        } else {
            0.0
        };
        let visible_left = -f32::from(offset.x);
        let cell_left = layout.gutter_width + layout.offset(column);
        let cell_right = cell_left + layout.widths[column];
        let target_left = if cell_left < visible_left + frozen_width {
            cell_left - frozen_width
        } else if cell_right > visible_left + viewport_width {
            cell_right - viewport_width
        } else {
            visible_left
        }
        .clamp(0.0, f32::from(max_offset.x));
        if target_left != visible_left {
            self.grid_horizontal_scroll
                .set_offset(gpui::point(px(-target_left), offset.y));
        }
    }

    /// Vertical scroll for a find jump: rows already fully in view stay put,
    /// others are centered (so the match is clear of the bar and header).
    pub(super) fn scroll_to_row(&mut self, row: usize) {
        let (bounds, offset) = {
            let state = self.grid_scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let top = row as f32 * theme::ROW_HEIGHT;
        let visible_top = -f32::from(offset.y);
        let visible_bottom = visible_top + f32::from(bounds.size.height);
        let in_view = bounds.size.height > gpui::px(0.0)
            && top >= visible_top
            && top + theme::ROW_HEIGHT <= visible_bottom;
        if !in_view {
            self.grid_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Center);
        }
    }

    /// Starts dragging the right edge of display `column`.
    pub(super) fn begin_column_resize(&mut self, column: usize, start_x: gpui::Pixels) {
        let Some(start_width) = self.grid_ui.layout.widths.get(column).copied() else {
            return;
        };
        self.grid_ui.column_resize = Some(ColumnResizeState {
            column,
            start_x,
            start_width,
        });
    }

    pub(super) fn update_column_resize(&mut self, current_x: gpui::Pixels) {
        let Some(resize) = self.grid_ui.column_resize else {
            return;
        };
        let delta = f32::from(current_x - resize.start_x);
        self.grid_ui
            .layout
            .resize_by(resize.column, resize.start_width, delta);
    }
}

#[derive(Clone, Copy)]
pub(super) struct ColumnResizeState {
    column: usize,
    start_x: gpui::Pixels,
    start_width: f32,
}

/// What `derived_columns` computes for a result.
struct DerivedColumns {
    visible_columns: Vec<usize>,
    /// Leading entries of `visible_columns` that are pinned.
    pinned: usize,
    layout: crate::grid::ColumnLayout,
}

/// Moves the `pinned` columns (in that order) to the front of the display
/// order `visible` (an empty one is the identity order), keeping the order of
/// the rest. A pin whose name / occurrence the result does not have, or hides,
/// is skipped. Returns how many columns are pinned.
fn pin_to_front(
    grid: &GridState<GridContentSource>,
    visible: &mut Vec<usize>,
    pinned: &[PinnedColumn],
) -> usize {
    if pinned.is_empty() {
        return 0;
    }
    if visible.is_empty() {
        visible.extend(0..grid.column_count());
    }
    // Occurrence of every source column among those sharing its name, once
    // for the result (not per pin or per cell).
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let occurrence: Vec<usize> = (0..grid.column_count())
        .map(|column| {
            let count = seen.entry(grid.column_name(column).unwrap_or_default()).or_insert(0);
            *count += 1;
            *count - 1
        })
        .collect();
    let mut front = 0;
    for pin in pinned {
        let Some(at) = visible[front..].iter().position(|source| {
            grid.column_name(*source).is_some_and(|name| {
                occurrence.get(*source).is_some_and(|occurrence| pin.matches(name, *occurrence))
            })
        }) else {
            continue;
        };
        visible[front..=front + at].rotate_right(1);
        front += 1;
    }
    front
}

/// Source indices of the columns left after hiding `hidden` (by name). Never
/// empty for a result with columns: when every column would be hidden (the
/// result's columns changed under a saved selection) all are shown.
fn visible_source_columns_without(
    grid: &GridState<GridContentSource>,
    hidden: &[String],
) -> Vec<usize> {
    let all = 0..grid.column_count();
    if hidden.is_empty() {
        return all.collect();
    }
    let visible: Vec<usize> = all
        .clone()
        .filter(|column| {
            grid.column_name(*column)
                .is_none_or(|name| !hidden.iter().any(|hidden| hidden == name))
        })
        .collect();
    if visible.is_empty() {
        all.collect()
    } else {
        visible
    }
}

impl AppRoot {
    /// The tab's result view state. A tab without one (never shown, or
    /// closed) reads as a fresh view.
    pub(super) fn view(&self, tab_id: u64) -> &TabView {
        self.tab_views.get(&tab_id).unwrap_or(&self.detached_view)
    }

    /// The tab's result view state, created on first use from the tab's
    /// current result (visible columns, column layout). A closed tab gets the
    /// shared throwaway view (reset on every access), so a late async result
    /// leaves nothing behind.
    pub(super) fn view_mut(&mut self, tab_id: u64) -> &mut TabView {
        if self.workspace.tab(tab_id).is_none() {
            self.detached_view = TabView::default();
            return &mut self.detached_view;
        }
        if !self.tab_views.contains_key(&tab_id) {
            let mut view = TabView::default();
            if let Some(derived) = self.derived_columns(tab_id) {
                view.grid_ui.visible_columns = derived.visible_columns;
                view.grid_ui.pinned = derived.pinned;
                view.grid_ui.layout = derived.layout;
            }
            self.tab_views.insert(tab_id, view);
        }
        self.tab_views
            .get_mut(&tab_id)
            .expect("view inserted above")
    }

    /// The tab's result page (empty when the tab has no runtime).
    pub(super) fn grid(&self, tab_id: u64) -> &GridState<GridContentSource> {
        self.workspace
            .runtime(tab_id)
            .map_or(&self.empty_grid, |runtime| &runtime.grid)
    }

    pub(super) fn display_column_count(&self, tab_id: u64) -> usize {
        self.view(tab_id)
            .display_column_count(self.grid(tab_id).column_count())
    }

    pub(super) fn source_column_for_display(
        &self,
        tab_id: u64,
        display_column: usize,
    ) -> Option<usize> {
        self.view(tab_id)
            .source_column_for_display(display_column, self.grid(tab_id).column_count())
    }

    /// The columns pinned in the tab's current result: the shown result tab's
    /// of a script run, else the tab's own.
    pub(super) fn pinned_columns(&self, tab_id: u64) -> &[PinnedColumn] {
        let shown_result = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.script.as_ref())
            .and_then(|script| match script.shown {
                crate::script_run::ScriptTab::Result(index) => script.results.get(index),
                crate::script_run::ScriptTab::Statistics => None,
            });
        match shown_result {
            Some(result) => &result.pinned,
            None => self
                .workspace
                .tab(tab_id)
                .map_or(&[], |tab| tab.pinned_columns.as_slice()),
        }
    }

    /// Display->source column map and column layout (with the tab's
    /// remembered widths) of the tab's current result. `None` without a runtime.
    fn derived_columns(&mut self, tab_id: u64) -> Option<DerivedColumns> {
        super::results::mongo::query::normalize_mongo_page(&mut self.workspace, tab_id);
        let is_mongo = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find());
        let pinned_names = if is_mongo {
            Vec::new()
        } else {
            self.pinned_columns(tab_id).to_vec()
        };
        let hidden_sql_columns = self
            .workspace
            .tab(tab_id)
            .filter(|tab| tab.query_mode.is_table_data())
            .map(|tab| tab.table_hidden_columns.clone())
            .unwrap_or_default();
        let runtime = self.workspace.runtime(tab_id)?;
        let grid = &runtime.grid;
        let mut visible_columns = if is_mongo && !runtime.mongo.known_columns.is_empty() {
            let source_columns = (0..grid.column_count())
                .filter_map(|column| grid.column_name(column).map(str::to_string))
                .collect::<Vec<_>>();
            runtime.mongo.visible_source_indices(&source_columns)
        } else {
            visible_source_columns_without(grid, &hidden_sql_columns)
        };
        let pinned = pin_to_front(grid, &mut visible_columns, &pinned_names);
        let display_count = if visible_columns.is_empty() {
            grid.column_count()
        } else {
            visible_columns.len()
        };
        let settings = &self.grid_display;
        let mut layout = crate::grid::ColumnLayout::with_limits(
            display_count,
            settings.column_width_default,
            settings.column_width_min,
            settings.column_width_max,
            super::settings_state::gutter_width(settings),
        );
        if settings.auto_size_columns {
            // Once per result (not per frame): the widest of the header and
            // a sample of the first rows.
            const SAMPLE_ROWS: usize = 50;
            const MAX_SAMPLE_CHARS: usize = 80;
            let sampled = grid.row_count().min(SAMPLE_ROWS);
            for display in 0..display_count {
                let source = visible_columns.get(display).copied().unwrap_or(display);
                let header = grid.column_name(source).map_or(0, |name| name.chars().count() + 3);
                let widest = (0..sampled)
                    .filter_map(|row| grid.cell_text(CellPosition { row, column: source }))
                    .map(|text| text.chars().take(MAX_SAMPLE_CHARS).count())
                    .max()
                    .unwrap_or(0);
                layout.fit_column(display, header.max(widest), settings.font_size);
            }
        }
        for (display, source) in visible_columns.iter().enumerate() {
            if let Some((name, width)) = runtime.column_width_overrides.get(source) {
                if grid.column_name(*source) == Some(name.as_str()) {
                    layout.widths[display] = *width;
                }
            }
        }
        Some(DerivedColumns {
            visible_columns,
            pinned,
            layout,
        })
    }

    /// Rebuilds the tab's derived result state after its grid or its column
    /// visibility changed: visible columns, column layout and a cleared
    /// selection / cell detail. Other tabs' views are untouched; a tab that
    /// merely becomes shown needs no call (its view already holds its state).
    pub(super) fn reset_tab_view(&mut self, tab_id: u64) {
        let Some(derived) = self.derived_columns(tab_id) else {
            return;
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.close();
        }
        let view = self.view_mut(tab_id);
        view.grid_ui.visible_columns = derived.visible_columns;
        view.grid_ui.pinned = derived.pinned;
        view.grid_ui.layout = derived.layout;
        view.grid_ui.selection.borrow_mut().clear();
        view.grid_ui.context_cell = None;
        view.detail_ui.edit = None;
        view.detail_ui.error = None;
    }

    pub(super) fn end_column_resize(&mut self, tab_id: u64) {
        let Some(resize) = self.view_mut(tab_id).grid_ui.column_resize.take() else {
            return;
        };
        let Some(source) = self.source_column_for_display(tab_id, resize.column) else {
            return;
        };
        let Some(name) = self.grid(tab_id).column_name(source).map(str::to_string) else {
            return;
        };
        let Some(width) = self
            .view(tab_id)
            .grid_ui
            .layout
            .widths
            .get(resize.column)
            .copied()
        else {
            return;
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.column_width_overrides.insert(source, (name, width));
        }
    }
}

#[cfg(test)]
impl TabView {
    pub(super) fn grid_is_focused(&self, window: &Window) -> bool {
        self.grid_focus
            .as_ref()
            .is_some_and(|handle| handle.is_focused(window))
    }
}

/// Test helpers: the tests drive the tab the user works in.
#[cfg(test)]
impl AppRoot {
    pub(super) fn active_id(&self) -> u64 {
        self.workspace
            .active_tab()
            .map(|tab| tab.id)
            .expect("test has an active tab")
    }

    pub(super) fn av(&self) -> &TabView {
        self.view(self.active_id())
    }

    pub(super) fn av_mut(&mut self) -> &mut TabView {
        let id = self.active_id();
        self.view_mut(id)
    }

    pub(super) fn active_grid(&self) -> &GridState<GridContentSource> {
        self.grid(self.active_id())
    }

    /// Replaces the active tab's result page and rebuilds its view.
    pub(super) fn set_active_grid(&mut self, grid: GridState<GridContentSource>) {
        let id = self.active_id();
        if let Some(runtime) = self.workspace.runtime_mut(id) {
            runtime.grid = grid;
        }
        self.reset_tab_view(id);
    }

    pub(super) fn reset_active_view(&mut self) {
        let id = self.active_id();
        self.reset_tab_view(id);
    }
}
