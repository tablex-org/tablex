//! The charts `gpui_component::chart` has no counterpart for, built on the plot primitives the
//! official charts are built on (`Plot` + `PlotElement`, `ScaleLinear`, `ScaleBand`, `PlotAxis`,
//! `Grid`, `PlotLabel`, `Tooltip`, the `Line` and `Area` shapes):
//!
//! * [`LinePlot`]: lines and areas that break where a value is missing and are stroked as the official `LineChart` strokes
//!   (2px; an area's outline, which `Area` strokes 1px, is `chart_data::AREA_STROKE`). `AreaChart` joins the points on either side of a missing one and always strokes 1px, and
//!   `LineChart` takes one series, so neither draws the export's picture (`chart_svg`).
//! * [`ScatterPlot`]: points on two numeric (or date) axes. `ScatterChart` does not exist, and every
//!   official point chart places X by index on a point scale.
//! * [`GroupedBars`]: one bar per series side by side in each category. `BarChart` takes a single
//!   `value` accessor, so it cannot draw a group.
//!
//! Each is one element painted in one pass: quads for the marks, no entity, listener or timer per
//! point. Hover is the `PlotElement` hitbox: `tooltip_state` maps the cursor to the nearest mark
//! (a scan of at most `MAX_SCATTER_POINTS` points per series, or one division for bars).

use super::ChartScene;
use crate::chart_data::{self, format_number, format_tick, format_time_axis, nice_ticks, ChartData, LINE_STROKE};
use gpui::{
    fill, point, px, quad, size, AnyElement, App, BorderStyle, Bounds, ElementId, Font, Hsla, IntoElement, Pixels, Point,
    SharedString, Size, TextAlign, Window,
};
use gpui_component::plot::label::{measure_text_width, Text, TEXT_GAP, TEXT_SIZE};
use gpui_component::plot::scale::{Scale, ScaleBand, ScaleLinear};
use gpui_component::plot::shape::{Area, Line};
use gpui_component::plot::{Curve, PathCaches};
use gpui_component::plot::tooltip::{CrossLine, Dot, Tooltip};
use gpui_component::plot::{axis_gutter, AxisText, Grid, Plot, PlotAxis, PlotElement, PlotLabel, TooltipState};
use gpui_component::ActiveTheme;
use std::rc::Rc;

const TOP: f32 = 10.0;
const RIGHT: f32 = 12.0;
/// The least room kept left of the plot for the value labels (the official charts' gutter).
const MIN_GUTTER: f32 = 32.0;
/// How near the cursor has to be to a scatter point to pick it.
const PICK_RADIUS: f32 = 14.0;
const DOT: f32 = 7.0;
const HOVER_DOT: f32 = 8.0;
const HOVER_HALO: f32 = 20.0;

/// The theme colors a custom plot reads, as the official charts read them.
#[derive(Clone)]
pub(super) struct PlotColors {
    pub series: Vec<Hsla>,
    axis: Hsla,
    grid: Hsla,
    muted: Hsla,
    background: Hsla,
}

impl PlotColors {
    pub(super) fn new(series: Vec<Hsla>, cx: &App) -> Self {
        let theme = cx.theme();
        Self { series, axis: theme.border, grid: theme.chart_grid, muted: theme.muted_foreground, background: theme.background }
    }
}

/// The plot rectangle inside the element, relative to its origin.
#[derive(Clone, Copy)]
pub(super) struct Frame {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

impl Frame {
    pub(super) fn new(bounds: Size<Pixels>, gutter: f32) -> Self {
        let left = gutter;
        let top = TOP;
        Self {
            left,
            right: (bounds.width.as_f32() - RIGHT).max(left + 1.0),
            top,
            bottom: (bounds.height.as_f32() - axis_gutter(px(TEXT_SIZE))).max(top + 1.0),
        }
    }

    fn width(&self) -> f32 {
        self.right - self.left
    }

    fn height(&self) -> f32 {
        self.bottom - self.top
    }
}

/// Round tick values for one axis, and the extent they span.
pub(super) struct NiceAxis {
    ticks: Vec<f64>,
    lo: f64,
    hi: f64,
    step: f64,
}

impl NiceAxis {
    fn new(lo: f64, hi: f64, count: usize) -> Self {
        let (lo, hi) = if lo >= hi { (lo - 1.0, hi + 1.0) } else { (lo, hi) };
        let ticks = nice_ticks(lo, hi, count);
        let step = if ticks.len() > 1 { ticks[1] - ticks[0] } else { 1.0 };
        Self { lo: ticks.first().copied().unwrap_or(lo), hi: ticks.last().copied().unwrap_or(hi), ticks, step }
    }

    fn labels(&self, temporal: bool) -> Vec<SharedString> {
        self.ticks
            .iter()
            .map(|t| if temporal { format_time_axis(*t, self.step) } else { format_tick(*t, self.step) }.into())
            .collect()
    }
}

/// The value axis of a line or an area with its labels: worked out once per scene, so a repaint formats nothing.
pub(super) struct LineAxis {
    nice: NiceAxis,
    labels: Vec<SharedString>,
}

impl LineAxis {
    fn new(lo: f64, hi: f64) -> Self {
        let nice = NiceAxis::new(lo, hi, 5);
        let labels = nice.labels(false);
        Self { nice, labels }
    }
}

impl std::ops::Deref for LineAxis {
    type Target = NiceAxis;

    fn deref(&self) -> &NiceAxis {
        &self.nice
    }
}

/// What a line or an area works out from its data and the size it is painted at: where its pieces and its labels are. It
/// changes only with the size (or the label gutter), so it is kept until one does.
pub(super) struct LineLayout {
    size: Size<Pixels>,
    gutter: f32,
    pieces: Pieces,
    baseline: f32,
    /// `(pixel y, label)` of each value tick.
    y_ticks: Vec<(f32, SharedString)>,
    /// `(label, x from the left edge of the plot, alignment)` of each category label shown.
    x_ticks: Vec<(SharedString, f32, TextAlign)>,
}

/// The lines of a scene, kept with it (a plot itself is built again by every render, and a hover renders): its value axis, the
/// gutter its labels need in the font they were measured in, and the layout of the size it was last painted at.
#[derive(Default)]
pub(super) struct LineCache {
    axis: Option<Rc<LineAxis>>,
    gutter: Option<(Font, f32)>,
    layout: Option<Rc<LineLayout>>,
}

#[cfg(test)]
thread_local! {
    /// How many layouts this thread has worked out, and the rows of the last tooltip a line plot gave (tests read both).
    pub(super) static LINE_LAYOUTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static LINE_TOOLTIP: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The gutter the value labels need left of the plot.
fn gutter_for(labels: &[SharedString], window: &mut Window) -> f32 {
    labels
        .iter()
        .map(|label| measure_text_width(label, px(TEXT_SIZE), window) + TEXT_GAP * 2.0)
        .fold(MIN_GUTTER, f32::max)
}

/// Grid, axes and labels shared by both plots. `y_ticks` are `(pixel y, label)`; `x_labels` are
/// positioned from the plot's left edge; `vertical` are grid columns from the plot's left edge.
#[allow(clippy::too_many_arguments)]
fn paint_frame(
    bounds: Bounds<Pixels>,
    frame: Frame,
    y_ticks: &[(f32, SharedString)],
    x_labels: impl IntoIterator<Item = AxisText>,
    vertical: impl IntoIterator<Item = f32>,
    colors: &PlotColors,
    window: &mut Window,
    cx: &mut App,
) {
    let plot = Bounds { origin: bounds.origin + point(px(frame.left), px(0.0)), size: size(px(frame.width()), px(frame.bottom)) };
    let rows = y_ticks.iter().map(|(y, _)| *y).filter(|y| (*y - frame.bottom).abs() > 0.5);
    Grid::new().y(rows).x(vertical).stroke(colors.grid).dash_array(&[px(4.0), px(2.0)]).paint(&plot, window);
    PlotAxis::new().stroke(colors.axis).x(frame.bottom).x_label(x_labels).paint(&plot, window, cx);
    let labels = y_ticks
        .iter()
        .map(|(y, text)| {
            let top = (y - TEXT_SIZE / 2.0).clamp(0.0, (frame.bottom - TEXT_SIZE).max(0.0));
            Text::new(text.clone(), point(frame.left - TEXT_GAP * 2.0, top), colors.muted).align(TextAlign::Right)
        })
        .collect();
    PlotLabel::new(labels).paint(&bounds, window, cx);
}

// ---- scatter -----------------------------------------------------------------------------------

pub(super) struct ScatterPlot {
    scene: Rc<ChartScene>,
    colors: PlotColors,
    id: ElementId,
    x: NiceAxis,
    y: NiceAxis,
    temporal: bool,
    /// Measured in `prepaint`, so `paint` and the hover share one layout.
    gutter: f32,
}

impl ScatterPlot {
    pub(super) fn new(scene: Rc<ChartScene>, colors: PlotColors, id: impl Into<ElementId>) -> Option<Self> {
        let ChartData::Points(points) = &scene.model.data else {
            return None;
        };
        let (mut x_lo, mut x_hi, mut y_lo, mut y_hi) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in points.series.iter().flat_map(|s| s.points.iter()) {
            (x_lo, x_hi, y_lo, y_hi) = (x_lo.min(p[0]), x_hi.max(p[0]), y_lo.min(p[1]), y_hi.max(p[1]));
        }
        let temporal = points.x_temporal;
        Some(Self {
            colors,
            id: id.into(),
            x: NiceAxis::new(x_lo, x_hi, 7),
            y: NiceAxis::new(y_lo, y_hi, 6),
            temporal,
            gutter: MIN_GUTTER,
            scene,
        })
    }

    fn points(&self) -> &chart_data::Points {
        match &self.scene.model.data {
            ChartData::Points(points) => points,
            _ => unreachable!("a scatter plot is built from points"),
        }
    }

    /// The two scales of a frame, built once per paint or hover.
    fn scales(&self, frame: Frame) -> (ScaleLinear<f64>, ScaleLinear<f64>) {
        (
            ScaleLinear::new([self.x.lo, self.x.hi], [frame.left, frame.right]),
            ScaleLinear::new([self.y.lo, self.y.hi], [frame.bottom, frame.top]),
        )
    }

    /// Pixel position (relative to the element) of a data point.
    fn locate(scales: &(ScaleLinear<f64>, ScaleLinear<f64>), frame: Frame, p: [f64; 2]) -> Point<Pixels> {
        point(px(scales.0.tick(&p[0]).unwrap_or(frame.left)), px(scales.1.tick(&p[1]).unwrap_or(frame.bottom)))
    }

    /// What the tooltip of point `index` of series `series` says: the X (a date when X is one), the
    /// series name and the Y.
    pub(super) fn tooltip_text(&self, series: usize, index: usize) -> Option<(String, SharedString, String)> {
        let s = self.points().series.get(series)?;
        let p = s.points.get(index)?;
        let x = if self.temporal { chart_data::format_time(p[0] as i64) } else { format_number(p[0]) };
        Some((x, s.name.clone(), format_number(p[1])))
    }
}

impl IntoElement for ScatterPlot {
    type Element = PlotElement<Self>;

    fn into_element(self) -> Self::Element {
        PlotElement::new(self)
    }
}

impl Plot for ScatterPlot {
    fn prepaint(&mut self, _bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App) -> Vec<AnyElement> {
        self.gutter = gutter_for(&self.y.labels(false), window);
        vec![]
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let frame = Frame::new(bounds.size, self.gutter);
        let y_scale = ScaleLinear::new([self.y.lo, self.y.hi], [frame.bottom, frame.top]);
        let x_scale = ScaleLinear::new([self.x.lo, self.x.hi], [0.0, frame.width()]);
        let scales = self.scales(frame);
        let y_ticks: Vec<(f32, SharedString)> = self
            .y
            .ticks
            .iter()
            .zip(self.y.labels(false))
            .filter_map(|(t, label)| Some((y_scale.tick(t)?, label)))
            .collect();
        let x_labels = self.x.labels(self.temporal);
        let last = self.x.ticks.len().saturating_sub(1);
        let x_axis: Vec<AxisText> = self
            .x
            .ticks
            .iter()
            .zip(x_labels)
            .enumerate()
            .filter_map(|(i, (t, label))| {
                let align = if i == 0 {
                    TextAlign::Left
                } else if i == last {
                    TextAlign::Right
                } else {
                    TextAlign::Center
                };
                Some(AxisText::new(label, x_scale.tick(t)?, self.colors.muted).align(align))
            })
            .collect();
        let columns: Vec<f32> = self.x.ticks.iter().filter_map(|t| x_scale.tick(t)).filter(|x| *x > 0.5).collect();
        paint_frame(bounds, frame, &y_ticks, x_axis, columns, &self.colors, window, cx);

        for (s, series) in self.points().series.iter().enumerate() {
            let color = self.colors.series[s % self.colors.series.len()].opacity(0.78);
            for p in &series.points {
                let at = Self::locate(&scales, frame, *p) + bounds.origin;
                window.paint_quad(quad(
                    gpui::bounds(at - point(px(DOT / 2.0), px(DOT / 2.0)), size(px(DOT), px(DOT))),
                    px(DOT / 2.0),
                    color,
                    px(0.0),
                    gpui::transparent_black(),
                    BorderStyle::default(),
                ));
            }
        }
    }

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn tooltip_state(&self, position: Point<Pixels>, bounds: Bounds<Pixels>, _cx: &App) -> Option<TooltipState> {
        let frame = Frame::new(bounds.size, self.gutter);
        let (x, y) = (position.x.as_f32(), position.y.as_f32());
        if x < frame.left - PICK_RADIUS || x > frame.right + PICK_RADIUS || y < frame.top - PICK_RADIUS || y > frame.bottom + PICK_RADIUS {
            return None;
        }
        let scales = self.scales(frame);
        let mut best: Option<(f32, usize, usize, Point<Pixels>)> = None;
        for (s, series) in self.points().series.iter().enumerate() {
            for (i, p) in series.points.iter().enumerate() {
                let at = Self::locate(&scales, frame, *p);
                let distance = (at.x.as_f32() - x).hypot(at.y.as_f32() - y);
                if distance <= PICK_RADIUS && best.is_none_or(|(d, ..)| distance < d) {
                    best = Some((distance, s, i, at));
                }
            }
        }
        let (_, s, i, at) = best?;
        Some(TooltipState::new((s << 32) | i, at, vec![at]))
    }

    fn tooltip(
        &self,
        state: &TooltipState,
        cursor: Point<Pixels>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<AnyElement> {
        let (s, i) = (state.index >> 32, state.index & 0xffff_ffff);
        let (x, name, y) = self.tooltip_text(s, i)?;
        let frame = Frame::new(bounds.size, self.gutter);
        let color = self.colors.series[s % self.colors.series.len()];
        let tooltip = Tooltip::new(cursor, bounds.size)
            .gap(px(8.0))
            .cross_line(CrossLine::new(state.cross_line).both().span(frame.top, frame.height()).h_span(frame.left, frame.width()))
            .dots(Some(
                Dot::new(*state.dots.first()?)
                    .size(HOVER_DOT)
                    .halo(HOVER_HALO)
                    .stroke(self.colors.background)
                    .fill(color),
            ))
            .title(x)
            .row(color, name, y);
        Some(tooltip.into_any_element())
    }
}

// ---- lines and areas ---------------------------------------------------------------------------

/// Lines and areas over categories: each series is one piece for every stretch of categories that has a value, so a missing
/// value breaks the line (as in the export). The categories are evenly spaced from the left edge of the plot to its right
/// edge; one category sits in the middle.
pub(super) struct LinePlot {
    scene: Rc<ChartScene>,
    colors: PlotColors,
    id: ElementId,
    y: Rc<LineAxis>,
    area: bool,
    gutter: f32,
}

/// The pieces of a drawn series in pixels relative to the element: series, then stretch, then point.
pub(super) type Pieces = Vec<Vec<Vec<[f32; 2]>>>;

impl LinePlot {
    pub(super) fn new(scene: Rc<ChartScene>, colors: PlotColors, id: impl Into<ElementId>) -> Option<Self> {
        if !matches!(scene.model.data, ChartData::Categories(_)) {
            return None;
        }
        let area = scene.model.kind == chart_data::ChartKind::Area;
        let y = scene.lines.borrow_mut().axis.get_or_insert_with(|| Rc::new(LineAxis::new(scene.extent.0, scene.extent.1))).clone();
        Some(Self { y, colors, id: id.into(), area, gutter: MIN_GUTTER, scene })
    }

    fn count(&self) -> usize {
        self.scene.axis_labels.len()
    }

    fn y_scale(&self, frame: Frame) -> ScaleLinear<f64> {
        ScaleLinear::new([self.y.lo, self.y.hi], [frame.bottom, frame.top])
    }

    /// The x of category `index`, relative to the element.
    fn x_at(&self, frame: Frame, index: usize) -> f32 {
        match self.count() {
            0 | 1 => frame.left + frame.width() / 2.0,
            n => frame.left + index as f32 * frame.width() / (n - 1) as f32,
        }
    }

    /// The category nearest to `x`, relative to the element.
    fn nearest(&self, frame: Frame, x: f32) -> usize {
        match self.count() {
            0 | 1 => 0,
            n => (((x - frame.left) / frame.width() * (n - 1) as f32).round().max(0.0) as usize).min(n - 1),
        }
    }

    /// Where the pieces of every series are drawn: the one geometry `paint` and the hover work from.
    pub(super) fn pieces(&self, frame: Frame) -> Pieces {
        let y = self.y_scale(frame);
        self.scene
            .drawn
            .iter()
            .map(|values| {
                chart_data::runs(values)
                    .into_iter()
                    .map(|range| range.map(|i| [self.x_at(frame, i), y.tick(&values[i].unwrap_or_default()).unwrap_or(frame.bottom)]).collect())
                    .collect()
            })
            .collect()
    }

    /// The y of zero, where an area is filled from.
    pub(super) fn baseline(&self, frame: Frame) -> f32 {
        self.y_scale(frame).tick(&0.0f64.clamp(self.y.lo, self.y.hi)).unwrap_or(frame.bottom)
    }

    /// The layout at `size`: the one kept with the scene while the size stays, else worked out and kept. A repaint that moves
    /// nothing (a hover) takes this path and allocates nothing for geometry.
    pub(super) fn layout(&self, size: Size<Pixels>) -> Rc<LineLayout> {
        let kept = self.scene.lines.borrow().layout.clone().filter(|layout| layout.size == size && layout.gutter == self.gutter);
        kept.unwrap_or_else(|| {
            let layout = Rc::new(self.work_out(size));
            self.scene.lines.borrow_mut().layout = Some(layout.clone());
            layout
        })
    }

    fn work_out(&self, size: Size<Pixels>) -> LineLayout {
        #[cfg(test)]
        LINE_LAYOUTS.with(|count| count.set(count.get() + 1));
        let frame = Frame::new(size, self.gutter);
        let y_scale = self.y_scale(frame);
        let y_ticks = self.y.ticks.iter().zip(&self.y.labels).filter_map(|(t, label)| Some((y_scale.tick(t)?, label.clone()))).collect();
        let n = self.count();
        let x_ticks = self
            .x_labels()
            .into_iter()
            .filter_map(|i| {
                let align = if n < 2 { TextAlign::Center } else if i == 0 { TextAlign::Left } else if i == n - 1 { TextAlign::Right } else { TextAlign::Center };
                Some((self.scene.axis_labels.get(i)?.clone(), self.x_at(frame, i) - frame.left, align))
            })
            .collect();
        LineLayout { size, gutter: self.gutter, pieces: self.pieces(frame), baseline: self.baseline(frame), y_ticks, x_ticks }
    }

    /// About six categories carry an axis label, first and last included.
    fn x_labels(&self) -> Vec<usize> {
        let n = self.count();
        let shown = n.min(6);
        let mut at: Vec<usize> = (0..shown).map(|j| if shown < 2 { 0 } else { (j * (n - 1) + (shown - 1) / 2) / (shown - 1) }).collect();
        at.dedup();
        at
    }

    fn colors_of(&self, series: usize) -> Hsla {
        self.colors.series[series % self.colors.series.len()]
    }

    /// The series that have a value at category `index`, in order: one hover dot each.
    fn drawn_at(&self, index: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.scene.drawn.iter().enumerate().filter_map(move |(s, values)| Some((s, (*values.get(index)?)?)))
    }
}

impl IntoElement for LinePlot {
    type Element = PlotElement<Self>;

    fn into_element(self) -> Self::Element {
        PlotElement::new(self)
    }
}

impl Plot for LinePlot {
    fn prepaint(&mut self, _bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App) -> Vec<AnyElement> {
        // The labels are the scene's: measured again only when the font changes.
        let font = window.text_style().font();
        let kept = self.scene.lines.borrow().gutter.as_ref().filter(|(measured_in, _)| *measured_in == font).map(|(_, gutter)| *gutter);
        self.gutter = kept.unwrap_or_else(|| {
            let gutter = gutter_for(&self.y.labels, window);
            self.scene.lines.borrow_mut().gutter = Some((font, gutter));
            gutter
        });
        vec![]
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let frame = Frame::new(bounds.size, self.gutter);
        let layout = self.layout(bounds.size);
        let x_labels = layout.x_ticks.iter().map(|(text, tick, align)| AxisText::new(text.clone(), *tick, self.colors.muted).align(*align));
        paint_frame(bounds, frame, &layout.y_ticks, x_labels, [], &self.colors, window, cx);

        let (pieces, baseline) = (&layout.pieces, layout.baseline);
        let fill_opacity = if pieces.len() == 1 { 0.3 } else { 0.18 };
        let caches = PathCaches::for_paint(if self.area { "areas" } else { "lines" }, window, cx);
        caches.update(cx, |caches, _| {
            let mut slot = 0;
            for (s, series) in pieces.iter().enumerate() {
                let color = self.colors_of(s);
                for piece in series {
                    if let [only] = piece.as_slice() {
                        // A value alone between breaks has no line to be seen by.
                        let at = point(px(only[0]), px(only[1])) + bounds.origin;
                        window.paint_quad(quad(gpui::bounds(at - point(px(1.5), px(1.5)), size(px(3.0), px(3.0))), px(1.5), color, px(0.0), gpui::transparent_black(), BorderStyle::default()));
                    } else if self.area {
                        let (fill, outline) = caches.slot_pair(slot);
                        Area::new()
                            .data(piece.iter().copied())
                            .x(|p| Some(p[0]))
                            .y0(baseline)
                            .y1(|p| Some(p[1]))
                            .stroke(color)
                            .fill(color.opacity(fill_opacity))
                            .curve(Curve::Linear)
                            .paint_cached(&bounds, fill, outline, window);
                    } else {
                        Line::new()
                            .data(piece.iter().copied())
                            .x(|p| Some(p[0]))
                            .y(|p| Some(p[1]))
                            .stroke(color)
                            .stroke_width(px(LINE_STROKE))
                            .curve(Curve::Linear)
                            .paint_cached(&bounds, caches.slot(slot), window);
                    }
                    slot += 1;
                }
            }
        });
    }

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn tooltip_state(&self, position: Point<Pixels>, bounds: Bounds<Pixels>, _cx: &App) -> Option<TooltipState> {
        let frame = Frame::new(bounds.size, self.gutter);
        let (x, y) = (position.x.as_f32(), position.y.as_f32());
        if x < frame.left || y > frame.bottom || self.count() == 0 {
            return None;
        }
        let index = self.nearest(frame, x);
        let at_x = self.x_at(frame, index);
        let y_scale = self.y_scale(frame);
        let dots = self.drawn_at(index).filter_map(|(_, value)| Some(point(px(at_x), px(y_scale.tick(&value)?)))).collect();
        Some(TooltipState::new(index, point(px(at_x), position.y), dots))
    }

    fn tooltip(&self, state: &TooltipState, cursor: Point<Pixels>, bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        let ChartData::Categories(categories) = &self.scene.model.data else { return None };
        let frame = Frame::new(bounds.size, self.gutter);
        let tooltip = Tooltip::new(cursor, bounds.size)
            .gap(px(8.0))
            .cross_line(CrossLine::new(state.cross_line).span(frame.top, frame.height()))
            .dots(self.drawn_at(state.index).zip(state.dots.iter()).map(|((s, _), at)| {
                Dot::new(*at).size(HOVER_DOT).halo(HOVER_HALO).stroke(self.colors.background).fill(self.colors_of(s))
            }))
            .title(categories.labels.get(state.index)?.clone());
        let mut tooltip = tooltip;
        #[cfg(test)]
        LINE_TOOLTIP.with(|rows| rows.borrow_mut().clear());
        for (s, series) in categories.series.iter().enumerate() {
            // A sum with no row is drawn as 0, but there was no data.
            let value = match series.values.get(state.index)? {
                Some(value) => format_number(*value),
                None => "no data".to_owned(),
            };
            #[cfg(test)]
            LINE_TOOLTIP.with(|rows| rows.borrow_mut().push((series.name.to_string(), value.clone())));
            tooltip = tooltip.row(self.colors_of(s), series.name.clone(), value);
        }
        Some(tooltip.into_any_element())
    }
}

// ---- grouped bars ------------------------------------------------------------------------------

pub(super) struct GroupedBars {
    scene: Rc<ChartScene>,
    colors: PlotColors,
    id: ElementId,
    y: NiceAxis,
    gutter: f32,
}

impl GroupedBars {
    pub(super) fn new(scene: Rc<ChartScene>, colors: PlotColors, id: impl Into<ElementId>) -> Option<Self> {
        let ChartData::Categories(categories) = &scene.model.data else {
            return None;
        };
        let (lo, hi) = categories
            .series
            .iter()
            .flat_map(|s| s.values.iter().flatten())
            .fold((0.0f64, 0.0f64), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        Some(Self { y: NiceAxis::new(lo, hi, 5), colors, id: id.into(), gutter: MIN_GUTTER, scene })
    }

    fn categories(&self) -> &chart_data::Categories {
        match &self.scene.model.data {
            ChartData::Categories(categories) => categories,
            _ => unreachable!("grouped bars are built from categories"),
        }
    }

    fn band(&self, frame: Frame) -> ScaleBand<usize> {
        let n = self.categories().labels.len();
        let series = self.categories().series.len().max(1) as f32;
        ScaleBand::new(0..n, [0.0, frame.width()]).padding_inner(0.3).padding_outer(0.15).max_band_width(30.0 * series)
    }
}

impl IntoElement for GroupedBars {
    type Element = PlotElement<Self>;

    fn into_element(self) -> Self::Element {
        PlotElement::new(self)
    }
}

impl Plot for GroupedBars {
    fn prepaint(&mut self, _bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App) -> Vec<AnyElement> {
        self.gutter = gutter_for(&self.y.labels(false), window);
        vec![]
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let frame = Frame::new(bounds.size, self.gutter);
        let y_scale = ScaleLinear::new([self.y.lo, self.y.hi], [frame.bottom, frame.top]);
        let y_ticks: Vec<(f32, SharedString)> =
            self.y.ticks.iter().zip(self.y.labels(false)).filter_map(|(t, label)| Some((y_scale.tick(t)?, label))).collect();
        let band = self.band(frame);
        let width = band.band_width();
        let n = self.categories().labels.len();
        let labeled = label_stride(n);
        let x_labels: Vec<AxisText> = (0..n)
            .filter(|i| i % labeled == 0)
            .filter_map(|i| Some(AxisText::new(self.scene.axis_labels.get(i)?.clone(), band.tick(&i)? + width / 2.0, self.colors.muted).align(TextAlign::Center)))
            .collect();
        paint_frame(bounds, frame, &y_ticks, x_labels, [], &self.colors, window, cx);

        let zero = y_scale.tick(&0.0).unwrap_or(frame.bottom);
        let categories = self.categories();
        let sub = width / categories.series.len().max(1) as f32;
        for i in 0..n {
            let Some(left) = band.tick(&i) else { continue };
            for (s, series) in categories.series.iter().enumerate() {
                let Some(value) = series.values[i] else { continue };
                let Some(top) = y_scale.tick(&value) else { continue };
                let color = self.colors.series[s % self.colors.series.len()];
                let origin = bounds.origin + point(px(frame.left + left + sub * s as f32), px(top.min(zero)));
                let rect = Bounds { origin, size: size(px((sub - 1.0).max(1.0)), px((top - zero).abs().max(1.0))) };
                window.paint_quad(fill(rect, color));
            }
        }
    }

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn tooltip_state(&self, position: Point<Pixels>, bounds: Bounds<Pixels>, _cx: &App) -> Option<TooltipState> {
        let frame = Frame::new(bounds.size, self.gutter);
        let (x, y) = (position.x.as_f32(), position.y.as_f32());
        if x < frame.left || x > frame.right || y < frame.top || y > frame.bottom || self.categories().labels.is_empty() {
            return None;
        }
        let band = self.band(frame);
        let index = band.nearest_index(x - frame.left - band.band_width() / 2.0);
        let center = frame.left + band.tick(&index)? + band.band_width() / 2.0;
        Some(TooltipState::new(index, point(px(center), position.y), vec![]))
    }

    fn tooltip(
        &self,
        state: &TooltipState,
        cursor: Point<Pixels>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<AnyElement> {
        let categories = self.categories();
        let frame = Frame::new(bounds.size, self.gutter);
        let band = self.band(frame);
        let mut tooltip = Tooltip::new(cursor, bounds.size)
            .gap(px(8.0))
            .cross_line(CrossLine::new(state.cross_line).band(band.band_width()).span(frame.top, frame.height()))
            .title(categories.labels.get(state.index)?.clone());
        for (s, series) in categories.series.iter().enumerate() {
            let value = match series.values.get(state.index)? {
                Some(value) => format_number(*value),
                None => "no data".to_owned(),
            };
            tooltip = tooltip.row(self.colors.series[s % self.colors.series.len()], series.name.clone(), value);
        }
        Some(tooltip.into_any_element())
    }
}

/// Every how-many-th category carries an axis label so about twelve fit.
pub(super) fn label_stride(count: usize) -> usize {
    count.div_ceil(12).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart_data::{prepare, Aggregation, ChartKind, ChartSource, ChartSpec};
    use arrow_array::{ArrayRef, Date32Array, Float64Array, RecordBatch, TimestampMicrosecondArray};
    use gpui::{px, size};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn plot(cx: &mut gpui::TestAppContext, x: ArrayRef) -> ScatterPlot {
        cx.update(|app| {
            gpui_component::init(app);
            crate::theme::init(app);
        });
        let y: ArrayRef = Arc::new(Float64Array::from(vec![Some(12.5), Some(3.0)]));
        let batch = RecordBatch::try_from_iter(vec![("x", x), ("v", y)]).unwrap();
        let source = ChartSource::new(&[Arc::new(batch)]);
        let spec = ChartSpec { kind: ChartKind::Scatter, x: 0, values: vec![1], aggregation: Aggregation::None, series_by: None };
        let model = prepare(&source, &spec, 100, &AtomicBool::new(false)).unwrap();
        let scene = Rc::new(ChartScene::new(Arc::new(model)));
        cx.update(|app| ScatterPlot::new(scene, PlotColors::new(vec![gpui::red()], app), "test-scatter").expect("a scatter plot"))
    }

    fn line_plot(cx: &mut gpui::TestAppContext, kind: ChartKind, aggregation: Aggregation) -> LinePlot {
        cx.update(|app| {
            gpui_component::init(app);
            crate::theme::init(app);
        });
        // x 1, 2, 3 by series a / b: "a" has all three, "b" has no row at x = 2.
        let x: ArrayRef = Arc::new(arrow_array::Int64Array::from(vec![1, 2, 3, 1, 3]));
        let by: ArrayRef = Arc::new(arrow_array::StringArray::from(vec!["a", "a", "a", "b", "b"]));
        let v: ArrayRef = Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0, 5.0, 7.0]));
        let batch = RecordBatch::try_from_iter(vec![("x", x), ("by", by), ("v", v)]).unwrap();
        let source = ChartSource::new(&[Arc::new(batch)]);
        let spec = ChartSpec { kind, x: 0, values: vec![2], aggregation, series_by: Some(1) };
        let model = prepare(&source, &spec, 100, &AtomicBool::new(false)).unwrap();
        let scene = Rc::new(ChartScene::new(Arc::new(model)));
        cx.update(|app| LinePlot::new(scene, PlotColors::new(vec![gpui::red(), gpui::blue()], app), "test-line").expect("a line plot"))
    }

    #[gpui::test]
    fn a_line_is_painted_through_the_points_the_scales_give_and_breaks_where_a_mean_is_missing(cx: &mut gpui::TestAppContext) {
        let plot = line_plot(cx, ChartKind::Line, Aggregation::Avg);
        assert_eq!((plot.y.lo, plot.y.hi), (0.0, 8.0), "ticks 0, 2, 4, 6, 8 over the values 1..7");
        let frame = Frame::new(size(px(800.0), px(400.0)), MIN_GUTTER);
        let (left, right, top, bottom) = (frame.left, frame.right, frame.top, frame.bottom);
        assert_eq!((left, right, top), (MIN_GUTTER, 800.0 - RIGHT, TOP));
        // Three categories from the left edge of the plot to its right edge; a value v is `v / 8` of the way up.
        let x = |i: usize| left + i as f32 * (right - left) / 2.0;
        let y = |v: f32| bottom - v / 8.0 * (bottom - top);
        let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01;
        let pieces = plot.pieces(frame);
        assert_eq!(pieces.len(), 2);
        // "a": one piece through all three points.
        assert_eq!(pieces[0].len(), 1);
        let a = &pieces[0][0];
        assert!(a.len() == 3 && a.iter().zip([[x(0), y(1.0)], [x(1), y(2.0)], [x(2), y(3.0)]]).all(|(got, want)| near(*got, want)), "{a:?}");
        // "b": no row at the middle category, so two pieces of one point each: nothing joins them.
        assert_eq!(pieces[1].len(), 2, "{:?}", pieces[1]);
        assert!(near(pieces[1][0][0], [x(0), y(5.0)]) && near(pieces[1][1][0], [x(2), y(7.0)]), "{:?}", pieces[1]);
        assert!(pieces[1].iter().all(|piece| piece.len() == 1));
        assert!(near([0.0, plot.baseline(frame)], [0.0, y(0.0)]), "an area is filled down to zero");
        // The categories on the axis: first, middle, last (three of them).
        assert_eq!(plot.x_labels(), [0, 1, 2]);
        // Where the cursor is picks the nearest category.
        assert_eq!([plot.nearest(frame, left), plot.nearest(frame, x(1) + 40.0), plot.nearest(frame, right + 100.0)], [0, 1, 2]);
    }

    #[gpui::test]
    fn an_idle_repaint_works_out_no_geometry_and_a_resize_works_it_out_again(cx: &mut gpui::TestAppContext) {
        let plot = line_plot(cx, ChartKind::Line, Aggregation::Avg);
        // Allocations of one `layout` call on this thread (the counter is per thread, so the tests beside it do not count).
        let counted = |plot: &LinePlot, at: Size<Pixels>| {
            let before = crate::grid_bench::thread_allocation_count();
            let layout = plot.layout(at);
            (crate::grid_bench::thread_allocation_count() - before, layout)
        };
        let (first_size, second_size) = (size(px(800.0), px(400.0)), size(px(900.0), px(400.0)));
        let worked_out = ChartScene::line_layouts_worked_out();
        let (first, a) = counted(&plot, first_size);
        // The pieces of two series (one with a break), the ticks and their labels: well over a dozen allocations.
        assert!(first >= 12, "the first paint works the geometry out: {first} allocations");
        // The same size again, by this plot or by the one the next render builds (a hover renders): nothing is allocated.
        let (second, b) = counted(&plot, first_size);
        assert_eq!(second, 0, "a repaint at the same size allocates nothing for geometry");
        assert!(Rc::ptr_eq(&a, &b));
        let next_render = LinePlot { y: plot.y.clone(), colors: plot.colors.clone(), id: plot.id.clone(), scene: plot.scene.clone(), area: plot.area, gutter: plot.gutter };
        let (third, c) = counted(&next_render, first_size);
        assert_eq!((third, Rc::ptr_eq(&a, &c)), (0, true));
        assert_eq!(ChartScene::line_layouts_worked_out() - worked_out, 1, "one layout for three paints");
        // Another size, or another label gutter, is another layout: worked out again, and its points are where the size puts them.
        let (resized, d) = counted(&plot, second_size);
        assert!(resized >= 12, "{resized} allocations");
        assert!(!Rc::ptr_eq(&a, &d) && d.pieces[0][0][2][0] > a.pieces[0][0][2][0], "the last category is further right in a wider plot");
        let wider_gutter = LinePlot { gutter: plot.gutter + 10.0, y: plot.y.clone(), colors: plot.colors.clone(), id: plot.id.clone(), scene: plot.scene.clone(), area: plot.area };
        assert!(!Rc::ptr_eq(&d, &counted(&wider_gutter, second_size).1));
        assert_eq!(ChartScene::line_layouts_worked_out() - worked_out, 3);
    }

    #[gpui::test]
    fn a_sum_has_no_break_and_the_axis_labels_are_spread_over_the_categories(cx: &mut gpui::TestAppContext) {
        let plot = line_plot(cx, ChartKind::Area, Aggregation::Sum);
        let frame = Frame::new(size(px(800.0), px(400.0)), MIN_GUTTER);
        let pieces = plot.pieces(frame);
        assert_eq!(pieces[1].len(), 1, "a missing sum is 0, drawn on the line");
        assert_eq!(pieces[1][0].len(), 3);
        // The missing sum sits on the zero line.
        assert!((pieces[1][0][1][1] - plot.baseline(frame)).abs() < 0.01);
        // Many categories: about six labels, the first and the last among them, in order.
        let many = |count: usize| {
            let labels = (0..count).map(|i| format!("c{i}").into()).collect();
            let plot = LinePlot { scene: Rc::new(ChartScene { axis_labels: labels, ..ChartScene::new(plot.scene.model.clone()) }), ..line_plot_like(&plot) };
            plot.x_labels()
        };
        assert_eq!(many(10), [0, 2, 4, 5, 7, 9]);
        assert_eq!(many(100), [0, 20, 40, 59, 79, 99]);
        assert_eq!(many(1), [0]);
        assert_eq!(many(2), [0, 1]);
    }

    fn line_plot_like(plot: &LinePlot) -> LinePlot {
        LinePlot { scene: plot.scene.clone(), colors: plot.colors.clone(), id: plot.id.clone(), y: Rc::new(LineAxis::new(0.0, 1.0)), area: plot.area, gutter: MIN_GUTTER }
    }

    #[gpui::test]
    fn the_tooltip_of_a_scatter_reads_a_date_as_a_date_and_a_number_as_a_number(cx: &mut gpui::TestAppContext) {
        let days: ArrayRef = Arc::new(Date32Array::from(vec![Some(19_727), Some(19_728)]));
        let on_dates = plot(cx, days);
        assert_eq!(on_dates.tooltip_text(0, 0), Some(("2024-01-05".to_owned(), "v".into(), "12.5".to_owned())), "a date, not 00:00:00 or a raw number");
        assert_eq!(on_dates.tooltip_text(0, 1).map(|t| t.0), Some("2024-01-06".to_owned()));

        let micros: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![Some(1_704_450_030_000_000), Some(1_704_450_031_000_000)]));
        let on_times = plot(cx, micros);
        assert_eq!(on_times.tooltip_text(0, 0).map(|t| t.0), Some("2024-01-05 10:20:30".to_owned()), "to the second");

        let numbers: ArrayRef = Arc::new(Float64Array::from(vec![Some(2.5), Some(7.0)]));
        let on_numbers = plot(cx, numbers);
        assert_eq!(on_numbers.tooltip_text(0, 1), Some(("7".to_owned(), "v".into(), "3".to_owned())));
        assert_eq!(on_numbers.tooltip_text(0, 9), None);
        assert_eq!(on_numbers.tooltip_text(3, 0), None);
    }
}
