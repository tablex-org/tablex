//! The chart pane: the settings strip (official `Select`s and a multi `Combobox`), the plot drawn
//! by `gpui_component::chart` (or `plots` for scatter and grouped bars), the legend and the notes.
//!
//! The pane renders from the state `sync_chart` keeps; it computes nothing about the rows. While a
//! request is in flight it shows "Preparing chart..." rather than a chart from older inputs.

use super::plots::{GroupedBars, LinePlot, PlotColors, ScatterPlot};
use super::{ChartExportFormat, ChartScene};
use super::super::super::*;
use crate::chart_data::{format_number, format_tick, Aggregation, ChartData, ChartIssue, ChartKind, ChartModel, Slice};
use crate::chart_svg::series_color;
use gpui::{App, ElementId, Hsla, Rgba, SharedString};
use gpui_component::chart::{BarChart, PieChart};
use gpui_component::combobox::Combobox;
use gpui_component::select::Select;
use gpui_component::ActiveTheme;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// A bar's band key: unique per position, so two rows with the same label get two bars.
#[derive(Clone)]
struct Band {
    index: usize,
    label: SharedString,
}

impl PartialEq for Band {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for Band {}

impl Hash for Band {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl From<Band> for SharedString {
    fn from(band: Band) -> Self {
        band.label
    }
}

fn rgb_of(color: Hsla) -> [u8; 3] {
    let rgba: Rgba = color.to_rgb();
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(rgba.r), byte(rgba.g), byte(rgba.b)]
}

/// Series colors: the theme's `chart_1..chart_5`, then tints of them (see `chart_svg::series_color`).
fn series_colors(count: usize, cx: &App) -> Vec<Hsla> {
    let theme = cx.theme();
    let base = [theme.chart_1, theme.chart_2, theme.chart_3, theme.chart_4, theme.chart_5];
    let palette = base.map(rgb_of);
    let background = rgb_of(theme.background);
    (0..count)
        .map(|i| {
            if i < base.len() {
                base[i]
            } else {
                let [r, g, b] = series_color(&palette, background, i);
                Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a: 1.0 }.into()
            }
        })
        .collect()
}

fn categories(scene: &ChartScene) -> &crate::chart_data::Categories {
    match &scene.model.data {
        ChartData::Categories(c) => c,
        _ => unreachable!("a category chart is built from categories"),
    }
}

fn slices(scene: &ChartScene) -> &[Slice] {
    match &scene.model.data {
        ChartData::Slices(s) => s,
        _ => &[],
    }
}

/// The official chart (or the custom plot) for the scene, filling its parent.
fn plot_element(scene: &Rc<ChartScene>, tab_id: u64, cx: &App) -> AnyElement {
    let model: &ChartModel = &scene.model;
    let id: ElementId = ElementId::from(("chart-plot-element", tab_id));
    match model.kind {
        ChartKind::Pie => {
            let colors: Vec<Hsla> = {
                let mut colors = series_colors(slices(scene).len(), cx);
                for (i, slice) in slices(scene).iter().enumerate() {
                    if slice.other {
                        colors[i] = cx.theme().muted_foreground;
                    }
                }
                colors
            };
            let (values, names) = (scene.clone(), scene.clone());
            PieChart::new(0..slices(scene).len())
                .id(id)
                .appear(false)
                .value(move |i| slices(&values)[*i].value as f32)
                .color(move |i| colors[*i])
                .pad_angle(0.01)
                .tooltip_name(move |i| slices(&names)[*i].label.clone())
                .tooltip_value(|_, value, share| format!("{} ({share:.1}%)", format_number(f64::from(value))).into())
                .into_any_element()
        }
        ChartKind::Scatter => {
            let colors = PlotColors::new(series_colors(model.series_names().len(), cx), cx);
            match ScatterPlot::new(scene.clone(), colors, id) {
                Some(plot) => plot.into_any_element(),
                None => div().into_any_element(),
            }
        }
        ChartKind::Bar | ChartKind::Line | ChartKind::Area => category_element(scene, id, cx),
    }
}

fn category_element(scene: &Rc<ChartScene>, id: ElementId, cx: &App) -> AnyElement {
    let data = categories(scene);
    let n = data.labels.len();
    let series = data.series.len();
    let colors = series_colors(series, cx);
    let kind = scene.model.kind;
    let (lo, hi) = data
        .series
        .iter()
        .flat_map(|s| s.values.iter().flatten())
        .fold((0.0f64, 0.0f64), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    let step = (hi - lo) / 4.0;
    let tick = move |v: f64| -> SharedString { format_tick(v, step).into() };

    if kind == ChartKind::Bar && series > 1 {
        let plot = GroupedBars::new(scene.clone(), PlotColors::new(colors, cx), id);
        return plot.map_or_else(|| div().into_any_element(), IntoElement::into_any_element);
    }
    let (titles, labels, values) = (scene.clone(), scene.clone(), scene.clone());
    let full_title = move |i: &usize| categories(&titles).labels[*i].clone();
    match kind {
        ChartKind::Bar => {
            let color = colors[0];
            let name = data.series[0].name.clone();
            BarChart::new(0..n)
                .id(id)
                .appear(false)
                .band(move |i| Band { index: *i, label: labels.axis_labels[*i].clone() })
                .value(move |i| categories(&values).series[0].values[*i].unwrap_or(0.0))
                .fill(move |_, _, _, _| color)
                .value_axis(true)
                .value_tick_format(tick)
                .band_tick_count(n.clamp(1, 12))
                .name(name)
                .tooltip_title(full_title)
                .tooltip_value(|_, v| format_number(v).into())
                .into_any_element()
        }
        // Lines and areas: one custom plot (see `plots`), as the export draws them.
        _ => LinePlot::new(scene.clone(), PlotColors::new(colors, cx), id).map_or_else(|| div().into_any_element(), IntoElement::into_any_element),
    }
}

impl AppRoot {
    pub(in crate::app) fn render_chart_pane(&mut self, tab_id: u64, cx: &mut Context<Self>) -> Div {
        let state = &self.view(tab_id).chart;
        let spec = state.spec.clone();
        let kind = spec.as_ref().map(|s| s.kind);
        let aggregation = spec.as_ref().map(|s| s.aggregation);
        let grouping = kind.is_some_and(ChartKind::groups_series);
        let controls = state.controls.as_ref().map(|c| (c.kind.clone(), c.x.clone(), c.values.clone(), c.aggregation.clone(), c.series_by.clone()));
        let outcome = match &state.outcome {
            None => None,
            Some(Ok(scene)) => Some(Ok(scene.clone())),
            Some(Err(issue)) => Some(Err(issue.clone())),
        };
        let notice = state.notice.clone();

        let label = |text: &'static str| {
            div().text_size(px(11.0)).text_color(theme::text_muted()).whitespace_nowrap().child(text)
        };
        let field = |text: &'static str, selector: &'static str, width: f32, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .flex_shrink_0()
                .child(label(text))
                .child(div().debug_selector(move || selector.into()).w(px(width)).child(control))
        };
        let strip = div()
            .id(("chart-settings", tab_id))
            .debug_selector(|| "chart-settings".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(12.0))
            .gap_y(px(6.0))
            .px(px(12.0))
            .py(px(6.0))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme::border());
        let strip = match controls {
            None => strip,
            Some((kind_state, x_state, values_state, aggregation_state, series_state)) => {
                let count = aggregation == Some(Aggregation::Count);
                strip
                    .child(field(
                        "Type",
                        "chart-kind",
                        110.0,
                        Select::new(&kind_state).id(("chart-kind-select", tab_id)).with_size(Size::Small).menu_width(px(140.0)).into_any_element(),
                    ))
                    .child(field(
                        "X",
                        "chart-x",
                        170.0,
                        Select::new(&x_state)
                            .id(("chart-x-select", tab_id))
                            .with_size(Size::Small)
                            .search_placeholder("Search columns")
                            .menu_width(px(240.0))
                            .into_any_element(),
                    ))
                    .child(field(
                        "Values",
                        "chart-values",
                        190.0,
                        Combobox::new(&values_state)
                            .with_size(Size::Small)
                            .placeholder(if count { "rows" } else { "Choose columns" })
                            .menu_width(px(240.0))
                            .disabled(count)
                            .into_any_element(),
                    ))
                    .child(field(
                        "Aggregate",
                        "chart-aggregation",
                        110.0,
                        Select::new(&aggregation_state)
                            .id(("chart-aggregation-select", tab_id))
                            .with_size(Size::Small)
                            .menu_width(px(140.0))
                            .into_any_element(),
                    ))
                    .child(field(
                        "Series by",
                        "chart-series",
                        150.0,
                        Select::new(&series_state)
                            .id(("chart-series-select", tab_id))
                            .with_size(Size::Small)
                            .search_placeholder("Search columns")
                            .menu_width(px(240.0))
                            .disabled(!grouping)
                            .into_any_element(),
                    ))
            }
        };
        let has_chart = matches!(outcome, Some(Ok(_)));
        let export = |format: ChartExportFormat, selector: &'static str, tooltip: &'static str| {
            Button::new((selector, tab_id))
                .label(format.label())
                .icon(Icon::empty().path("icons/download.svg").size(px(14.0)))
                .with_size(Size::Small)
                .compact()
                .ghost()
                .tooltip(tooltip)
                .disabled(!has_chart)
                .debug_selector(move || selector.into())
                .on_click(cx.listener(move |this, _, _, cx| this.export_chart(tab_id, format, cx)))
        };
        let strip = strip
            .child(div().flex_1().min_w(px(0.0)))
            .child(export(ChartExportFormat::Png, "chart-export-png", "Save the chart as a PNG image"))
            .child(export(ChartExportFormat::Svg, "chart-export-svg", "Save the chart as an SVG file"));

        let body = match &outcome {
            None => message("Preparing chart…", false),
            Some(Err(issue)) => message(&issue.to_string(), !matches!(issue, ChartIssue::Cancelled)),
            Some(Ok(scene)) => chart_body(scene, tab_id, cx),
        };

        let notes: Vec<String> = match &outcome {
            Some(Ok(scene)) => scene.model.notes.iter().map(ToString::to_string).collect(),
            _ => Vec::new(),
        };
        let footer = (!notes.is_empty() || notice.is_some()).then(|| {
            div()
                .debug_selector(|| "chart-notes".into())
                .flex()
                .flex_wrap()
                .gap_x(px(14.0))
                .px(px(12.0))
                .pb(px(6.0))
                .flex_shrink_0()
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .children(notes.into_iter().map(|note| div().child(note)))
                .children(notice.map(|(text, error)| {
                    div()
                        .debug_selector(|| "chart-notice".into())
                        .text_color(if error { theme::danger() } else { theme::text_muted() })
                        .child(text)
                }))
        });
        div()
            .debug_selector(|| "chart-pane".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .child(strip)
            .child(body)
            .children(footer)
    }
}

/// A centered message in place of the plot.
fn message(text: &str, warning: bool) -> Div {
    div()
        .debug_selector(|| "chart-message".into())
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .p(px(24.0))
        .child(
            div()
                .max_w(px(520.0))
                .text_size(px(13.0))
                .text_color(if warning { theme::text() } else { theme::text_muted() })
                .child(text.to_owned()),
        )
}

/// The plot filling the pane, and the legend under it.
fn chart_body(scene: &Rc<ChartScene>, tab_id: u64, cx: &mut Context<AppRoot>) -> Div {
    let model = &scene.model;
    let names = model.series_names();
    let legend = (names.len() > 1 || model.kind == ChartKind::Pie).then(|| {
        let colors = if model.kind == ChartKind::Pie {
            let mut colors = series_colors(names.len(), cx);
            for (i, slice) in slices(scene).iter().enumerate() {
                if slice.other {
                    colors[i] = cx.theme().muted_foreground;
                }
            }
            colors
        } else {
            series_colors(names.len(), cx)
        };
        let total: f64 = slices(scene).iter().map(|s| s.value).sum();
        div()
            .debug_selector(|| "chart-legend".into())
            .flex()
            .flex_wrap()
            .gap_x(px(16.0))
            .gap_y(px(4.0))
            .flex_shrink_0()
            .text_size(px(11.0))
            .text_color(theme::text())
            .children(names.iter().enumerate().map(|(i, name)| {
                let share = (model.kind == ChartKind::Pie && total > 0.0)
                    .then(|| format!("  {:.1}%", slices(scene)[i].value / total * 100.0));
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().w(px(10.0)).h(px(10.0)).rounded(px(2.0)).bg(colors[i]))
                    .child(div().child(format!("{name}{}", share.unwrap_or_default())))
            }))
    });
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(12.0))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .debug_selector(|| "chart-plot".into())
                .child(div().absolute().top_0().left_0().right_0().bottom_0().child(plot_element(scene, tab_id, cx))),
        )
        .children(legend)
}
