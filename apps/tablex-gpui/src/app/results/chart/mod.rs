//! Charts from a result: the "Chart" presentation of a tab's results (rail button next to Grid,
//! Properties, Diagram and Plan). The grid area shows the result as a bar, line, area, scatter or
//! pie chart instead of the table; the table keeps its scroll, selection and column layout, so
//! Grid brings it back as it was.
//!
//! * Data: the rows the result already holds. [`ChartSource`] shares the Arrow batches (`Arc`
//!   clones, never a copy of the rows); [`chart_data::prepare`] aggregates and bounds them on the
//!   background executor. A request is identified by the page, the spec and the point budget; a
//!   newer request cancels the older one and a late answer for an older request is dropped, so a
//!   chart from old inputs is never shown.
//! * Settings: official `Select`s for type, X, aggregation and series-by, a multi `Combobox` for
//!   the value columns. They live per tab (`TabView`), keep their choices while the tab lives and
//!   are rebuilt only when the result's columns change.
//! * Drawing: `gpui_component::chart` (bar, line, area, pie). Where a chart has no official
//!   counterpart (scatter, bars grouped by a series) `plots` builds it on the same plot
//!   primitives. Nothing here is an entity per point.
//! * Export: PNG and SVG of the same prepared model, in the colors on screen (see `chart_svg`).

use super::super::*;
use super::diagram::export::{rasterize, ExportError};
use crate::chart_data::{
    self, Aggregation, ChartColumn, ChartIssue, ChartKind, ChartModel, ChartSource, ChartSpec, ColumnClass,
};
use crate::chart_svg::{self, Rgb, SvgStyle};
use crate::columns_panel::Presentation;
use gpui::{Rgba, SharedString};
use gpui_component::combobox::{ComboboxEvent, ComboboxState};
use gpui_component::searchable_list::SearchableListItem;
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::IndexPath;
use std::rc::Rc;

mod plots;
mod view;

pub(in crate::app) const CHART_UNAVAILABLE_MONGO: &str = "Charts are not available for MongoDB results";
pub(in crate::app) const CHART_UNAVAILABLE_REDIS: &str = "Charts are not available for Redis results";
const CHART_UNAVAILABLE_EMPTY: &str = "No result to chart";

/// Rows looked at to decide whether the category repeats (Sum instead of None by default).
const REPEAT_SAMPLE_ROWS: usize = 2_000;

/// An item of a chart Select: a label and the number the handler gets back (a column index, a
/// type or an aggregation), so two columns with the same name never collide.
#[derive(Clone, Debug)]
struct Choice {
    title: SharedString,
    value: usize,
}

impl SearchableListItem for Choice {
    type Value = usize;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &usize {
        &self.value
    }
}

type ChoiceSelect = gpui::Entity<SelectState<Vec<Choice>>>;
type ChoiceCombobox = gpui::Entity<ComboboxState<Vec<Choice>>>;

/// The settings controls of one tab, built for one list of columns.
struct ChartControls {
    kind: ChoiceSelect,
    x: ChoiceSelect,
    values: ChoiceCombobox,
    aggregation: ChoiceSelect,
    series_by: ChoiceSelect,
    /// The spec the controls show; only the differing parts are pushed on a change.
    shown: ChartSpec,
}

/// What the chart pane draws: the prepared model plus what is derived from it once per model (the
/// axis labels, the series as they are drawn) so a repaint rebuilds nothing.
pub(in crate::app) struct ChartScene {
    pub(in crate::app) model: Arc<ChartModel>,
    /// Category labels as the axis shows them (cut short).
    pub(in crate::app) axis_labels: Vec<SharedString>,
    /// Category series as they are drawn (`chart_data::drawn_values`): a missing value is a break in the line.
    pub(in crate::app) drawn: Vec<Vec<Option<f64>>>,
    /// The span of the drawn values, zero included: the value axis of a line or an area.
    pub(in crate::app) extent: (f64, f64),
    /// What a line or an area works out once for this scene (see [`plots::LineCache`]).
    lines: std::cell::RefCell<plots::LineCache>,
}

impl ChartScene {
    fn new(model: Arc<ChartModel>) -> Self {
        let (axis_labels, drawn) = match &model.data {
            chart_data::ChartData::Categories(categories) => (
                categories.labels.iter().map(|label| axis_label(label)).collect(),
                categories.series.iter().map(|s| chart_data::drawn_values(&s.values, model.aggregation)).collect(),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let extent = drawn.iter().flatten().flatten().fold((0.0f64, 0.0f64), |(lo, hi), v: &f64| (lo.min(*v), hi.max(*v)));
        Self { model, axis_labels, drawn, extent, lines: Default::default() }
    }
}

#[cfg(test)]
impl ChartScene {
    /// How many layouts of lines and areas this thread has worked out (see [`plots::LineLayout`]).
    pub(in crate::app) fn line_layouts_worked_out() -> usize {
        plots::LINE_LAYOUTS.with(std::cell::Cell::get)
    }

    /// `(series, value)` of each row of the last tooltip a line or an area gave on this thread.
    pub(in crate::app) fn last_line_tooltip() -> Vec<(String, String)> {
        plots::LINE_TOOLTIP.with(|rows| rows.borrow().clone())
    }
}

fn axis_label(label: &str) -> SharedString {
    const MAX: usize = 14;
    match label.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &label[..cut]).into(),
        None => label.to_owned().into(),
    }
}

/// Identity of a request: the page, the spec and the point budget.
struct ChartKey {
    page: GridState<GridContentSource>,
    spec: ChartSpec,
    max_points: usize,
}

struct Pending {
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Drop for Pending {
    /// A request that is replaced, or whose tab is closed or left, stops the work in flight.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Chart state of one tab (`TabView::chart`).
#[derive(Default)]
pub(in crate::app) struct ChartTabState {
    /// The grid page the source was taken from.
    page: Option<GridState<GridContentSource>>,
    source: Option<ChartSource>,
    /// The columns the spec and the controls were built for.
    columns: Vec<ChartColumn>,
    /// The catalog the column types were read from (`TabRuntime::structure_generation`): a catalog that lands or is replaced
    /// after the chart was first shown moves it, and the columns are classed again.
    catalog: u64,
    spec: Option<ChartSpec>,
    controls: Option<ChartControls>,
    /// The request the outcome (or the pending computation) answers.
    key: Option<ChartKey>,
    outcome: Option<Result<Rc<ChartScene>, ChartIssue>>,
    pending: Option<Pending>,
    /// Bumped by every request; an answer carrying an older one is stale.
    generation: u64,
    /// Result of the last export: text and whether it failed.
    notice: Option<(String, bool)>,
    /// Computations started (tests).
    #[cfg(test)]
    pub(in crate::app) prepares: usize,
}

impl ChartTabState {
    fn cancel_pending(&mut self) {
        self.pending = None;
    }

    /// Leaving Chart: the settings stay, the data does not.
    fn release(&mut self) {
        self.cancel_pending();
        self.page = None;
        self.source = None;
        self.key = None;
        self.outcome = None;
    }

    #[cfg(test)]
    pub(in crate::app) fn values_selection_for_test(&self, cx: &gpui::App) -> Vec<usize> {
        self.controls.as_ref().map(|c| c.values.read(cx).selected_values()).unwrap_or_default()
    }

    /// The cancel flag of the computation in flight (tests).
    #[cfg(test)]
    pub(in crate::app) fn pending_cancel_for_test(&self) -> Option<Arc<AtomicBool>> {
        self.pending.as_ref().map(|pending| pending.cancel.clone())
    }

    #[cfg(test)]
    pub(in crate::app) fn notice_for_test(&self) -> Option<&(String, bool)> {
        self.notice.as_ref()
    }

    #[cfg(test)]
    pub(in crate::app) fn values_query_for_test(&self, cx: &gpui::App) -> String {
        self.controls.as_ref().map(|c| c.values.read(cx).query(cx).to_string()).unwrap_or_default()
    }

    #[cfg(test)]
    pub(in crate::app) fn edit_spec_for_test(&mut self, edit: impl FnOnce(&mut ChartSpec)) {
        if let Some(spec) = self.spec.as_mut() {
            edit(spec);
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn spec_for_test(&self) -> Option<&ChartSpec> {
        self.spec.as_ref()
    }

    #[cfg(test)]
    pub(in crate::app) fn outcome_for_test(&self) -> Option<&Result<Rc<ChartScene>, ChartIssue>> {
        self.outcome.as_ref()
    }

    /// The catalog generation the columns were classed by.
    #[cfg(test)]
    pub(in crate::app) fn catalog_for_test(&self) -> u64 {
        self.catalog
    }

    #[cfg(test)]
    pub(in crate::app) fn generation_for_test(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(in crate::app) fn source_for_test(&self) -> Option<&ChartSource> {
        self.source.as_ref()
    }

    #[cfg(test)]
    pub(in crate::app) fn controls_for_test(&self) -> Option<[gpui::EntityId; 5]> {
        self.controls
            .as_ref()
            .map(|c| [c.kind.entity_id(), c.x.entity_id(), c.values.entity_id(), c.aggregation.entity_id(), c.series_by.entity_id()])
    }
}

/// Points a line / area chart keeps: about twice the pixels it can be drawn across, in steps so a
/// resize does not recompute at every pixel.
fn point_budget(width: f32) -> usize {
    ((width.max(1.0) * 2.0).ceil() as usize).next_multiple_of(512).clamp(512, 8_192)
}

fn kind_choices() -> Vec<Choice> {
    ChartKind::ALL.iter().enumerate().map(|(value, kind)| Choice { title: kind.label().into(), value }).collect()
}

fn aggregation_choices() -> Vec<Choice> {
    Aggregation::ALL.iter().enumerate().map(|(value, a)| Choice { title: a.label().into(), value }).collect()
}

fn column_choices(columns: &[ChartColumn], leading_none: bool) -> Vec<Choice> {
    let shift = usize::from(leading_none);
    leading_none
        .then(|| Choice { title: "None".into(), value: 0 })
        .into_iter()
        .chain(columns.iter().enumerate().map(|(i, c)| Choice { title: c.name.clone().into(), value: i + shift }))
        .collect()
}

fn row(index: usize) -> Option<IndexPath> {
    Some(IndexPath::default().row(index))
}

impl ChartControls {
    fn build(tab_id: u64, columns: &[ChartColumn], spec: &ChartSpec, window: &mut Window, cx: &mut Context<AppRoot>) -> Self {
        let kind_row = ChartKind::ALL.iter().position(|k| *k == spec.kind);
        let kind = cx.new(|cx| SelectState::new(kind_choices(), kind_row.and_then(row), window, cx));
        let x = cx.new(|cx| SelectState::new(column_choices(columns, false), row(spec.x), window, cx).searchable(true));
        let aggregation_row = Aggregation::ALL.iter().position(|a| *a == spec.aggregation);
        let aggregation = cx.new(|cx| SelectState::new(aggregation_choices(), aggregation_row.and_then(row), window, cx));
        let series_by = cx.new(|cx| {
            SelectState::new(column_choices(columns, true), row(spec.series_by.map_or(0, |c| c + 1)), window, cx)
                .searchable(true)
        });
        let values = cx.new(|cx| {
            ComboboxState::new(
                column_choices(columns, false),
                spec.values.iter().filter_map(|c| row(*c)).collect(),
                window,
                cx,
            )
            .multiple(true)
            .searchable(true)
        });
        cx.subscribe(&kind, move |this: &mut AppRoot, _, event: &SelectEvent<Vec<Choice>>, cx| {
            if let SelectEvent::Confirm(Some(choice)) = event {
                let kind = ChartKind::ALL[*choice];
                this.chart_shown(tab_id, |shown| shown.kind = kind);
                this.chart_edit(tab_id, cx, move |spec, columns| {
                    spec.kind = kind;
                    // A scatter plots numbers or dates along X: move off a text category, onto a
                    // column that is not already a value.
                    let ok = |i: usize| matches!(columns[i].class, ColumnClass::Numeric | ColumnClass::Temporal);
                    if kind == ChartKind::Scatter && !ok(spec.x) {
                        let free = (0..columns.len()).find(|i| ok(*i) && !spec.values.contains(i));
                        if let Some(x) = free {
                            spec.x = x;
                        }
                    }
                });
            }
        })
        .detach();
        cx.subscribe(&x, move |this: &mut AppRoot, _, event: &SelectEvent<Vec<Choice>>, cx| {
            if let SelectEvent::Confirm(Some(column)) = event {
                let column = *column;
                this.chart_shown(tab_id, |shown| shown.x = column);
                this.chart_edit(tab_id, cx, move |spec, _| spec.x = column);
            }
        })
        .detach();
        cx.subscribe(&aggregation, move |this: &mut AppRoot, _, event: &SelectEvent<Vec<Choice>>, cx| {
            if let SelectEvent::Confirm(Some(choice)) = event {
                let aggregation = Aggregation::ALL[*choice];
                this.chart_shown(tab_id, |shown| shown.aggregation = aggregation);
                this.chart_edit(tab_id, cx, move |spec, columns| {
                    spec.aggregation = aggregation;
                    // Coming from Count: a numeric column to aggregate, unless one is chosen.
                    if aggregation != Aggregation::Count && spec.values.is_empty() {
                        spec.values.extend(columns.iter().position(|c| c.class == ColumnClass::Numeric));
                    }
                });
            }
        })
        .detach();
        cx.subscribe(&series_by, move |this: &mut AppRoot, _, event: &SelectEvent<Vec<Choice>>, cx| {
            if let SelectEvent::Confirm(Some(choice)) = event {
                let by = choice.checked_sub(1);
                this.chart_shown(tab_id, |shown| shown.series_by = by);
                this.chart_edit(tab_id, cx, move |spec, _| spec.series_by = by);
            }
        })
        .detach();
        cx.subscribe(&values, move |this: &mut AppRoot, _, event: &ComboboxEvent<Vec<Choice>>, cx| {
            if let ComboboxEvent::Change(columns) = event {
                // The control already shows this selection (and its search text): only changes made from
                // outside are pushed back into it, as that clears the query.
                let columns = columns.clone();
                this.chart_shown(tab_id, |shown| shown.values = columns.clone());
                this.chart_edit(tab_id, cx, move |spec, _| spec.values = columns);
            }
        })
        .detach();
        Self { kind, x, values, aggregation, series_by, shown: spec.clone() }
    }

    /// Pushes a spec that changed outside the controls (new defaults, a scatter moving X) into them.
    fn show(&mut self, spec: &ChartSpec, window: &mut Window, cx: &mut Context<AppRoot>) {
        if self.shown == *spec {
            return;
        }
        if self.shown.kind != spec.kind {
            let value = ChartKind::ALL.iter().position(|k| *k == spec.kind).unwrap_or(0);
            self.kind.update(cx, |state, cx| state.set_selected_value(&value, window, cx));
        }
        if self.shown.x != spec.x {
            self.x.update(cx, |state, cx| state.set_selected_value(&spec.x, window, cx));
        }
        if self.shown.aggregation != spec.aggregation {
            let value = Aggregation::ALL.iter().position(|a| *a == spec.aggregation).unwrap_or(0);
            self.aggregation.update(cx, |state, cx| state.set_selected_value(&value, window, cx));
        }
        if self.shown.series_by != spec.series_by {
            let value = spec.series_by.map_or(0, |c| c + 1);
            self.series_by.update(cx, |state, cx| state.set_selected_value(&value, window, cx));
        }
        if self.shown.values != spec.values {
            self.values.update(cx, |state, cx| state.set_selected_values(&spec.values, window, cx));
        }
        self.shown = spec.clone();
    }
}

impl AppRoot {
    /// `Ok` when the tab's result can be charted, else why not (the rail button's tooltip).
    pub(in crate::app) fn chart_support(&self, tab_id: u64) -> Result<(), &'static str> {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return Err(CHART_UNAVAILABLE_EMPTY);
        };
        if tab.query_mode.is_mongo() {
            return Err(CHART_UNAVAILABLE_MONGO);
        }
        if tab.query_mode.is_redis() {
            return Err(CHART_UNAVAILABLE_REDIS);
        }
        let grid = self.grid(tab_id);
        if grid.column_count() == 0 {
            return Err(CHART_UNAVAILABLE_EMPTY);
        }
        match grid.source() {
            GridContentSource::Arrow(_) => Ok(()),
            GridContentSource::Bson(_) => Err(CHART_UNAVAILABLE_MONGO),
        }
    }

    /// True while the tab's grid area shows the chart.
    pub(in crate::app) fn chart_presentation_active(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.columns_panel.presentation == Presentation::Chart)
            && self.chart_support(tab_id).is_ok()
    }

    /// Rail "Chart": shows the chart, or goes back to the grid when it is already shown.
    pub(in crate::app) fn toggle_chart_presentation(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let shown = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.columns_panel.presentation == Presentation::Chart);
        if shown {
            self.show_grid_presentation(tab_id, cx);
            return;
        }
        if self.chart_support(tab_id).is_err() {
            return;
        }
        // A cell editor open in the grid belongs to the view being left.
        self.commit_inline_edit(tab_id, InlineEditExit::FocusLoss, None, cx);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.presentation = Presentation::Chart;
        }
        self.notify_tab(tab_id, cx);
    }

    /// Records what a control now shows, so `ChartControls::show` pushes only what changed elsewhere.
    fn chart_shown(&mut self, tab_id: u64, update: impl FnOnce(&mut ChartSpec)) {
        if let Some(controls) = self.view_mut(tab_id).chart.controls.as_mut() {
            update(&mut controls.shown);
        }
    }

    /// Applies an edit to the tab's spec and redraws; the next frame asks for the new chart.
    fn chart_edit(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut ChartSpec, &[ChartColumn]),
    ) {
        let state = &mut self.view_mut(tab_id).chart;
        let (Some(spec), Some(source)) = (state.spec.as_mut(), state.source.as_ref()) else {
            return;
        };
        edit(spec, source.columns());
        state.notice = None;
        self.notify_tab(tab_id, cx);
    }

    /// Called every frame of a tab's results (see `surface`): keeps the chart's source, controls and
    /// request in step with the grid page and the spec, and frees the data when Chart is left.
    pub(in crate::app) fn sync_chart(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chart_presentation_active(tab_id) {
            if let Some(view) = self.tab_views.get_mut(&tab_id) {
                if view.chart.page.is_some() || view.chart.pending.is_some() {
                    view.chart.release();
                }
            }
            return;
        }
        let grid = self.grid(tab_id).clone();
        let max_points = point_budget(f32::from(window.viewport_size().width));
        if !self.view(tab_id).chart.page.as_ref().is_some_and(|page| page.same_source(&grid)) || self.view(tab_id).chart.catalog != self.chart_catalog(tab_id) {
            self.chart_take_source(tab_id, grid);
        }
        let state = &mut self.view_mut(tab_id).chart;
        let (Some(source), Some(spec)) = (state.source.as_ref(), state.spec.clone()) else {
            return;
        };
        match state.controls.as_mut() {
            Some(controls) => controls.show(&spec, window, cx),
            None => {
                let columns = source.columns().to_vec();
                let controls = ChartControls::build(tab_id, &columns, &spec, window, cx);
                self.view_mut(tab_id).chart.controls = Some(controls);
            }
        }
        self.chart_request(tab_id, max_points, cx);
    }

    /// Which catalog the column types come from right now (see `ChartTabState::catalog`).
    fn chart_catalog(&self, tab_id: u64) -> u64 {
        self.workspace.runtime(tab_id).map_or(0, |runtime| runtime.structure_generation)
    }

    /// A new page, or a catalog that arrived late: shares the batches, and starts the spec over when the columns changed.
    fn chart_take_source(&mut self, tab_id: u64, grid: GridState<GridContentSource>) {
        let GridContentSource::Arrow(arrow) = grid.source() else {
            return;
        };
        // The database's type name of each column: what says that text is a number or a date.
        let hints: Vec<Option<String>> = (0..grid.column_count())
            .map(|column| {
                grid.native_type(column)
                    .or_else(|| arrow.driver_type(column))
                    .map(str::to_owned)
                    .or_else(|| self.resolved_column_type(tab_id, column))
            })
            .collect();
        let partial = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.pagination.has_next() || runtime.pagination.has_previous());
        let source = ChartSource::with_types(arrow.batches(), &hints).partial(partial);
        let catalog = self.chart_catalog(tab_id);
        let state = &mut self.view_mut(tab_id).chart;
        state.cancel_pending();
        state.key = None;
        state.outcome = None;
        let columns = source.columns().to_vec();
        if state.spec.is_none() || state.columns != columns {
            let kind = state.spec.as_ref().map_or(ChartKind::Bar, |spec| spec.kind);
            // The same columns under the same names, but a type that was not known now says a column is a number or a date: the
            // spec the user left alone is made over for the new classes; one they changed stays, the controls keep their choices.
            let reclassed = state.columns.len() == columns.len() && state.columns.iter().zip(&columns).all(|(old, new)| old.name == new.name);
            let defaults = |columns: &[ChartColumn]| {
                let mut spec = ChartSpec::defaults(columns, |x| source.repeats_within(x, REPEAT_SAMPLE_ROWS));
                spec.kind = kind;
                spec
            };
            let untouched = reclassed && state.spec.as_ref() == Some(&defaults(&state.columns));
            if state.spec.is_none() || !reclassed || untouched {
                state.spec = Some(defaults(&columns));
                state.controls = None;
            }
            state.columns = columns;
            state.notice = None;
        }
        state.catalog = catalog;
        state.page = Some(grid);
        state.source = Some(source);
    }

    /// Starts the computation for the current page, spec and budget unless it is the one the state
    /// already answers. Whatever was in flight is cancelled and its answer will be dropped.
    fn chart_request(&mut self, tab_id: u64, max_points: usize, cx: &mut Context<Self>) {
        let state = &mut self.view_mut(tab_id).chart;
        let (Some(page), Some(spec), Some(source)) = (state.page.as_ref(), state.spec.as_ref(), state.source.as_ref())
        else {
            return;
        };
        // Only line and area are thinned to the plot width: the others do not depend on it.
        let max_points = if matches!(spec.kind, ChartKind::Line | ChartKind::Area) { max_points } else { 0 };
        let current = state
            .key
            .as_ref()
            .is_some_and(|key| key.page.same_source(page) && key.spec == *spec && key.max_points == max_points);
        if current {
            return;
        }
        let (source, spec) = (source.clone(), spec.clone());
        state.key = Some(ChartKey { page: page.clone(), spec: spec.clone(), max_points });
        state.cancel_pending();
        state.outcome = None;
        state.generation += 1;
        #[cfg(test)]
        {
            state.prepares += 1;
        }
        let generation = state.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { chart_data::prepare(&source, &spec, max_points, &worker_cancel) })
                .await;
            let _ = this.update(cx, |this, cx| this.finish_chart(tab_id, generation, outcome, cx));
        });
        state.pending = Some(Pending { cancel, _task: task });
    }

    /// The answer to request `generation`; dropped when a newer request replaced it.
    pub(in crate::app) fn finish_chart(
        &mut self,
        tab_id: u64,
        generation: u64,
        outcome: Result<ChartModel, ChartIssue>,
        cx: &mut Context<Self>,
    ) {
        let state = &mut self.view_mut(tab_id).chart;
        if state.generation != generation {
            return;
        }
        state.pending = None;
        if outcome == Err(ChartIssue::Cancelled) {
            return;
        }
        state.outcome = Some(outcome.map(|model| Rc::new(ChartScene::new(Arc::new(model)))));
        self.notify_tab(tab_id, cx);
    }

    /// The chart on screen, when there is one.
    fn chart_model(&self, tab_id: u64) -> Option<Arc<ChartModel>> {
        match self.view(tab_id).chart.outcome.as_ref()? {
            Ok(scene) => Some(scene.model.clone()),
            Err(_) => None,
        }
    }

    /// Export PNG / SVG: asks for a file, then renders and writes the chart off the UI thread.
    pub(in crate::app) fn export_chart(&mut self, tab_id: u64, format: ChartExportFormat, cx: &mut Context<Self>) {
        let Some(model) = self.chart_model(tab_id) else {
            return;
        };
        let style = svg_style();
        let directory = crate::platform::save_dialog_dir();
        let name = format!("{}.{}", file_stem(&model.title), format.extension());
        let prompt = cx.prompt_for_new_path(&directory, Some(&name));
        let connection = self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone());
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.chart_notice(tab_id, "Could not open the save dialog".into(), true, cx)
                    });
                    return;
                }
            };
            let shown = path.display().to_string();
            let result = cx
                .background_executor()
                .spawn(async move { write_chart(&model, &style, format, &path) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.chart_notice(tab_id, format!("Saved {} to {shown}", format.label()), false, cx),
                Err(error) => {
                    let message = format!("Could not save the chart to {shown}: {error}");
                    this.report_error(Area::Export, connection.as_deref(), &message);
                    this.chart_notice(tab_id, message, true, cx);
                }
            });
        })
        .detach();
    }

    fn chart_notice(&mut self, tab_id: u64, text: String, error: bool, cx: &mut Context<Self>) {
        self.view_mut(tab_id).chart.notice = Some((text, error));
        self.notify_tab(tab_id, cx);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ChartExportFormat {
    Png,
    Svg,
}

impl ChartExportFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Svg => "svg",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Svg => "SVG",
        }
    }
}

/// A file name stem from the chart title.
fn file_stem(title: &str) -> String {
    let stem: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let stem: String = stem.chars().take(60).collect();
    if stem.is_empty() {
        "chart".to_owned()
    } else {
        stem
    }
}

/// Renders the chart as the export format and writes it (blocking).
pub(in crate::app) fn write_chart(
    model: &ChartModel,
    style: &SvgStyle,
    format: ChartExportFormat,
    path: &std::path::Path,
) -> Result<(), ExportError> {
    let svg = chart_svg::render(model, style);
    let bytes = match format {
        ChartExportFormat::Svg => svg.into_bytes(),
        ChartExportFormat::Png => rasterize(&svg)?,
    };
    write_beside(path, &bytes).map_err(ExportError::Io)
}

/// Writes `bytes` to a temporary file next to `path` and renames it over `path`, so a failed or
/// interrupted export never leaves a half-written file under the chosen name.
fn write_beside(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map_or_else(|| "chart".into(), |name| name.to_string_lossy().into_owned());
    let temp = path.with_file_name(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
    let result = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn rgb(color: Rgba) -> Rgb {
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(color.r), byte(color.g), byte(color.b)]
}

/// `color` (with its alpha) over an opaque `background`.
fn flatten(color: Rgba, background: Rgb) -> Rgb {
    let mix = |c: f32, b: u8| ((c * color.a + f32::from(b) / 255.0 * (1.0 - color.a)).clamp(0.0, 1.0) * 255.0).round() as u8;
    [mix(color.r, background[0]), mix(color.g, background[1]), mix(color.b, background[2])]
}

/// The colors on screen right now, for the exported chart.
pub(in crate::app) fn svg_style() -> SvgStyle {
    let background = rgb(theme::background());
    SvgStyle {
        background,
        text: rgb(theme::text()),
        muted: rgb(theme::text_muted()),
        grid: flatten(theme::border(), background),
        palette: [rgb(theme::chart_1()), rgb(theme::chart_2()), rgb(theme::chart_3()), rgb(theme::chart_4()), rgb(theme::chart_5())],
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_names_come_from_the_title() {
        assert_eq!(super::file_stem("SUM(amount) by region"), "SUM_amount_by_region");
        assert_eq!(super::file_stem("COUNT(*) by a/b\\c, per d"), "COUNT_by_a_b_c_per_d");
        assert_eq!(super::file_stem("***"), "chart");
        assert_eq!(super::file_stem(&"x".repeat(200)).len(), 60);
    }

    #[test]
    fn an_export_is_written_beside_and_renamed_over_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chart.png");
        std::fs::write(&path, b"old").unwrap();
        super::write_beside(&path, b"new bytes").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new bytes");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temporary file stays: {leftovers:?}");
        // A directory that is not there: an error, and nothing created.
        let missing = dir.path().join("no/such/dir/chart.png");
        assert!(super::write_beside(&missing, b"x").is_err());
        assert!(!dir.path().join("no").exists());
    }

    #[test]
    fn the_point_budget_is_twice_the_width_in_steps() {
        assert_eq!(super::point_budget(0.0), 512);
        assert_eq!(super::point_budget(256.0), 512);
        assert_eq!(super::point_budget(300.0), 1024);
        assert_eq!(super::point_budget(1200.0), 2560);
        assert_eq!(super::point_budget(100_000.0), 8192);
    }

    #[test]
    fn long_axis_labels_are_cut() {
        assert_eq!(super::axis_label("short").as_ref(), "short");
        assert_eq!(super::axis_label("0123456789abcdefgh").as_ref(), "0123456789abcd…");
    }
}
