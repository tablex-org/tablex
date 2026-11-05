//! A prepared chart as SVG: the export path of Charts from a result. Pure (strings in, string out), so
//! it runs on the background executor; the PNG export rasterises this same SVG with resvg.
//!
//! It draws the [`ChartModel`] the screen draws (the aggregated, downsampled series, the notes), with
//! its own layout: axes with round ticks, a legend, the title and the notes. Colors come from
//! [`SvgStyle`], which the caller fills from the theme that is on screen (light or dark).

use crate::chart_data::{
    drawn_values, format_tick, format_time_axis, nice_ticks, runs, Categories, ChartData, ChartKind, ChartModel, Points, Slice, AREA_STROKE, LINE_STROKE,
};
use std::fmt::Write as _;

/// Logical size of an exported chart; the PNG is rendered at 2x this.
pub const WIDTH: f32 = 960.0;
pub const HEIGHT: f32 = 540.0;

const MARGIN_LEFT: f32 = 76.0;
const MARGIN_RIGHT: f32 = 28.0;
const MARGIN_TOP: f32 = 56.0;
const FONT: &str = "Helvetica, Arial, sans-serif";

pub type Rgb = [u8; 3];

/// The theme colors a chart is exported in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvgStyle {
    pub background: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub grid: Rgb,
    /// The theme's `chart_1..chart_5`.
    pub palette: [Rgb; 5],
}

/// The color of series `index`: the theme's five, then lighter tints of them (blended toward the
/// text color's opposite), so up to twelve series stay apart.
pub fn series_color(palette: &[Rgb; 5], background: Rgb, index: usize) -> Rgb {
    let base = palette[index % 5];
    let round = (index / 5) as f32;
    if round == 0.0 {
        return base;
    }
    let mix = (0.32 * round).min(0.7);
    let blend = |c: u8, b: u8| (f32::from(c) * (1.0 - mix) + f32::from(b) * mix).round() as u8;
    [blend(base[0], background[0]), blend(base[1], background[1]), blend(base[2], background[2])]
}

fn hex(color: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2])
}

/// `text` as XML character data: markup escaped, and the characters XML 1.0 cannot hold at all
/// (most control characters, U+FFFE, U+FFFF, lone surrogates cannot occur in `str`) dropped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if matches!(c, '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}') => out.push(c),
            _ => {}
        }
    }
    out
}

fn short(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

fn n(value: f32) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

struct Canvas<'s> {
    svg: String,
    style: &'s SvgStyle,
}

impl Canvas<'_> {
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, x: f32, y: f32, size: f32, color: Rgb, anchor: &str, weight: &str, content: &str) {
        let _ = write!(
            self.svg,
            "<text x=\"{}\" y=\"{}\" font-family=\"{FONT}\" font-size=\"{}\" font-weight=\"{weight}\" fill=\"{}\" text-anchor=\"{anchor}\">{}</text>",
            n(x),
            n(y),
            n(size),
            hex(color),
            escape(content)
        );
    }

    fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Rgb) {
        let _ = write!(self.svg, "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"1\"/>", n(x1), n(y1), n(x2), n(y2), hex(color));
    }

    fn color(&self, index: usize) -> Rgb {
        series_color(&self.style.palette, self.style.background, index)
    }
}

/// The chart as an SVG document of [`WIDTH`] x [`HEIGHT`] logical pixels.
pub fn render(model: &ChartModel, style: &SvgStyle) -> String {
    let mut canvas = Canvas { svg: String::with_capacity(16 * 1024), style };
    let _ = write!(
        canvas.svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\"><rect width=\"{w}\" height=\"{h}\" fill=\"{}\"/>",
        hex(style.background),
        w = WIDTH,
        h = HEIGHT
    );
    canvas.text(MARGIN_LEFT - 52.0, 30.0, 16.0, style.text, "start", "600", &short(&model.title, 90));

    // Notes along the bottom edge.
    let notes: Vec<String> = model.notes.iter().map(ToString::to_string).collect();
    let mut bottom = HEIGHT - 14.0;
    if !notes.is_empty() {
        canvas.text(MARGIN_LEFT - 52.0, HEIGHT - 12.0, 11.0, style.muted, "start", "400", &short(&notes.join(" · "), 150));
        bottom -= 16.0;
    }

    match &model.data {
        ChartData::Categories(data) => categories(&mut canvas, model, data, bottom),
        ChartData::Points(data) => scatter(&mut canvas, model, data, bottom),
        ChartData::Slices(slices) => pie(&mut canvas, slices, bottom),
    }
    canvas.svg.push_str("</svg>");
    canvas.svg
}

/// Legend row under the plot; returns the height it took.
fn legend(canvas: &mut Canvas<'_>, names: &[String], bottom: f32) -> f32 {
    if names.len() < 2 {
        return 0.0;
    }
    let mut x = MARGIN_LEFT;
    let mut y = bottom - 4.0;
    let mut rows = 1.0f32;
    for (i, name) in names.iter().enumerate() {
        let name = short(name, 24);
        let width = 22.0 + name.chars().count() as f32 * 6.4 + 18.0;
        if x + width > WIDTH - MARGIN_RIGHT {
            x = MARGIN_LEFT;
            y += 16.0;
            rows += 1.0;
        }
        let color = canvas.color(i);
        let _ = write!(canvas.svg, "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"10\" rx=\"2\" fill=\"{}\"/>", n(x), n(y - 9.0), hex(color));
        canvas.text(x + 15.0, y, 11.0, canvas.style.text, "start", "400", &name);
        x += width;
    }
    rows * 16.0 + 4.0
}

struct Plot {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

impl Plot {
    fn width(&self) -> f32 {
        self.right - self.left
    }
    fn height(&self) -> f32 {
        self.bottom - self.top
    }
}

fn value_extent(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let (lo, hi) = values.fold((0.0f64, 0.0f64), |(lo, hi), v| (lo.min(v), hi.max(v)));
    if lo == hi {
        (lo, lo + 1.0)
    } else {
        (lo, hi)
    }
}

fn y_axis(canvas: &mut Canvas<'_>, plot: &Plot, ticks: &[f64], y_of: &dyn Fn(f64) -> f32, y_title: &str) {
    let step = if ticks.len() > 1 { ticks[1] - ticks[0] } else { 1.0 };
    for &tick in ticks {
        let y = y_of(tick);
        canvas.line(plot.left, y, plot.right, y, canvas.style.grid);
        canvas.text(plot.left - 8.0, y + 4.0, 11.0, canvas.style.muted, "end", "400", &format_tick(tick, step));
    }
    let _ = write!(
        canvas.svg,
        "<text transform=\"translate(16 {}) rotate(-90)\" font-family=\"{FONT}\" font-size=\"11\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
        n(plot.top + plot.height() / 2.0),
        hex(canvas.style.muted),
        escape(&short(y_title, 40))
    );
}

fn categories(canvas: &mut Canvas<'_>, model: &ChartModel, data: &Categories, bottom: f32) {
    let names: Vec<String> = data.series.iter().map(|s| s.name.to_string()).collect();
    let legend_height = legend(canvas, &names, bottom);
    let plot = Plot { left: MARGIN_LEFT, right: WIDTH - MARGIN_RIGHT, top: MARGIN_TOP, bottom: bottom - legend_height - 44.0 };
    let count = data.labels.len().max(1);
    // What is drawn: a missing sum is 0, any other missing value is a gap in the line.
    let drawn: Vec<Vec<Option<f64>>> = data.series.iter().map(|s| drawn_values(&s.values, model.aggregation)).collect();
    let (lo, hi) = value_extent(drawn.iter().flatten().flatten().copied());
    let ticks = nice_ticks(lo, hi, 6);
    let (axis_lo, axis_hi) = (ticks.first().copied().unwrap_or(lo), ticks.last().copied().unwrap_or(hi));
    let y_of = move |v: f64| plot.bottom - ((v - axis_lo) / (axis_hi - axis_lo)) as f32 * (plot.bottom - plot.top);
    let plot_ref = &plot;
    y_axis(canvas, plot_ref, &ticks, &y_of, &model.y_title);

    let bar = model.kind == ChartKind::Bar;
    // Bars sit in bands; lines and areas run through evenly spaced points.
    let x_of = |i: usize| {
        if bar {
            plot.left + (i as f32 + 0.5) * plot.width() / count as f32
        } else if count == 1 {
            plot.left + plot.width() / 2.0
        } else {
            plot.left + i as f32 * plot.width() / (count - 1) as f32
        }
    };
    let baseline = y_of(0.0f64.clamp(axis_lo, axis_hi));
    canvas.line(plot.left, baseline, plot.right, baseline, canvas.style.muted);

    // X labels: as many as fit.
    let slot = if bar { plot.width() / count as f32 } else { plot.width() / (count.max(2) - 1) as f32 };
    let every = (56.0 / slot.max(1.0)).ceil().max(1.0) as usize;
    for (i, label) in data.labels.iter().enumerate().filter(|(i, _)| i % every == 0) {
        canvas.text(x_of(i), plot.bottom + 16.0, 11.0, canvas.style.muted, "middle", "400", &short(label, 12));
    }
    canvas.text(plot.left + plot.width() / 2.0, plot.bottom + 36.0, 11.0, canvas.style.muted, "middle", "400", &short(&model.x_title, 40));

    match model.kind {
        ChartKind::Bar => {
            let series = data.series.len().max(1) as f32;
            let band = plot.width() / count as f32;
            let group = (band * 0.7).min(60.0 * series);
            let width = group / series;
            for (s, (series_data, values)) in data.series.iter().zip(&drawn).enumerate() {
                let color = hex(canvas.color(s));
                for (i, value) in values.iter().enumerate() {
                    let (Some(value), Some(_)) = (value, series_data.values[i]) else { continue };
                    let x = x_of(i) - group / 2.0 + width * s as f32;
                    let (top, base) = (y_of(*value), baseline);
                    let _ = write!(
                        canvas.svg,
                        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{color}\"/>",
                        n(x + 0.5),
                        n(top.min(base)),
                        n((width - 1.0).max(1.0)),
                        n((top - base).abs().max(0.5))
                    );
                }
            }
        }
        ChartKind::Line | ChartKind::Area => {
            for (s, values) in drawn.iter().enumerate() {
                let color = hex(canvas.color(s));
                // A run of defined values is one polyline; a gap breaks it, and has no marker.
                let stroke = if model.kind == ChartKind::Area { AREA_STROKE } else { LINE_STROKE };
                for range in runs(values) {
                    let run: Vec<(f32, f32)> = range.map(|i| (x_of(i), y_of(values[i].unwrap_or_default()))).collect();
                    let points: Vec<String> = run.iter().map(|(x, y)| format!("{},{}", n(*x), n(*y))).collect();
                    if model.kind == ChartKind::Area && run.len() > 1 {
                        // The screen's fill: a lone area is stronger than overlapping ones.
                        let fill = if drawn.len() == 1 { "0.3" } else { "0.18" };
                        let _ = write!(
                            canvas.svg,
                            "<polygon points=\"{},{} {} {},{}\" fill=\"{color}\" fill-opacity=\"{fill}\"/>",
                            n(run[0].0),
                            n(baseline),
                            points.join(" "),
                            n(run[run.len() - 1].0),
                            n(baseline)
                        );
                    }
                    if run.len() > 1 {
                        let _ = write!(canvas.svg, "<polyline points=\"{}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"{}\" stroke-linejoin=\"round\"/>", points.join(" "), n(stroke));
                    }
                    // Like the screen: no point markers. Only a value alone between gaps has no line to be seen by.
                    if run.len() == 1 {
                        let _ = write!(canvas.svg, "<circle cx=\"{}\" cy=\"{}\" r=\"1.5\" fill=\"{color}\"/>", n(run[0].0), n(run[0].1));
                    }
                }
            }
        }
        _ => {}
    }
}

fn scatter(canvas: &mut Canvas<'_>, model: &ChartModel, data: &Points, bottom: f32) {
    let names: Vec<String> = data.series.iter().map(|s| s.name.to_string()).collect();
    let legend_height = legend(canvas, &names, bottom);
    let plot = Plot { left: MARGIN_LEFT, right: WIDTH - MARGIN_RIGHT, top: MARGIN_TOP, bottom: bottom - legend_height - 44.0 };
    let all = data.series.iter().flat_map(|s| s.points.iter());
    let (x_lo, x_hi, y_lo, y_hi) = all.fold((f64::MAX, f64::MIN, f64::MAX, f64::MIN), |(a, b, c, d), p| (a.min(p[0]), b.max(p[0]), c.min(p[1]), d.max(p[1])));
    let widen = |lo: f64, hi: f64| if lo >= hi { (lo - 1.0, hi + 1.0) } else { (lo, hi) };
    let (x_lo, x_hi) = widen(x_lo, x_hi);
    let (y_lo, y_hi) = widen(y_lo, y_hi);
    let y_ticks = nice_ticks(y_lo, y_hi, 6);
    let x_ticks = nice_ticks(x_lo, x_hi, 7);
    let (ya, yb) = (y_ticks.first().copied().unwrap_or(y_lo), y_ticks.last().copied().unwrap_or(y_hi));
    let (xa, xb) = (x_ticks.first().copied().unwrap_or(x_lo), x_ticks.last().copied().unwrap_or(x_hi));
    let y_of = |v: f64| plot.bottom - ((v - ya) / (yb - ya)) as f32 * (plot.bottom - plot.top);
    let x_of = |v: f64| plot.left + ((v - xa) / (xb - xa)) as f32 * plot.width();
    y_axis(canvas, &plot, &y_ticks, &y_of, &model.y_title);
    let x_step = if x_ticks.len() > 1 { x_ticks[1] - x_ticks[0] } else { 1.0 };
    for &tick in &x_ticks {
        let x = x_of(tick);
        canvas.line(x, plot.top, x, plot.bottom, canvas.style.grid);
        let label = if data.x_temporal { format_time_axis(tick, x_step) } else { format_tick(tick, x_step) };
        canvas.text(x, plot.bottom + 16.0, 11.0, canvas.style.muted, "middle", "400", &short(&label, 19));
    }
    canvas.text(plot.left + plot.width() / 2.0, plot.bottom + 36.0, 11.0, canvas.style.muted, "middle", "400", &short(&model.x_title, 40));
    for (s, series) in data.series.iter().enumerate() {
        let color = hex(canvas.color(s));
        for p in &series.points {
            let _ = write!(canvas.svg, "<circle cx=\"{}\" cy=\"{}\" r=\"3\" fill=\"{color}\" fill-opacity=\"0.75\"/>", n(x_of(p[0])), n(y_of(p[1])));
        }
    }
}

fn pie(canvas: &mut Canvas<'_>, slices: &[Slice], bottom: f32) {
    let total: f64 = slices.iter().map(|s| s.value).sum();
    let radius = ((bottom - MARGIN_TOP) / 2.0 - 8.0).clamp(40.0, 190.0);
    let (cx, cy) = (MARGIN_LEFT + radius + 24.0, MARGIN_TOP + (bottom - MARGIN_TOP) / 2.0);
    let mut angle = -std::f64::consts::FRAC_PI_2;
    let point = |a: f64| (cx + radius * a.cos() as f32, cy + radius * a.sin() as f32);
    for (i, slice) in slices.iter().enumerate() {
        let color = if slice.other { canvas.style.muted } else { canvas.color(i) };
        let sweep = if total > 0.0 { slice.value / total * std::f64::consts::TAU } else { 0.0 };
        if sweep <= 0.0 {
            continue;
        }
        if sweep >= std::f64::consts::TAU - 1e-9 {
            let _ = write!(canvas.svg, "<circle cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"{}\"/>", n(cx), n(cy), n(radius), hex(color));
        } else {
            let ((x1, y1), (x2, y2)) = (point(angle), point(angle + sweep));
            let large = u8::from(sweep > std::f64::consts::PI);
            let _ = write!(
                canvas.svg,
                "<path d=\"M{} {} L{} {} A{r} {r} 0 {large} 1 {} {} Z\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.5\"/>",
                n(cx),
                n(cy),
                n(x1),
                n(y1),
                n(x2),
                n(y2),
                hex(color),
                hex(canvas.style.background),
                r = n(radius)
            );
        }
        angle += sweep;
    }
    // The legend lists every slice with its share.
    let x = cx + radius + 56.0;
    for (i, slice) in slices.iter().enumerate() {
        let y = MARGIN_TOP + 16.0 + i as f32 * 22.0;
        let color = if slice.other { canvas.style.muted } else { canvas.color(i) };
        let _ = write!(canvas.svg, "<rect x=\"{}\" y=\"{}\" width=\"12\" height=\"12\" rx=\"2\" fill=\"{}\"/>", n(x), n(y - 10.0), hex(color));
        let share = if total > 0.0 { slice.value / total * 100.0 } else { 0.0 };
        canvas.text(x + 20.0, y, 12.0, canvas.style.text, "start", "400", &format!("{}  {} ({share:.1}%)", short(&slice.label, 28), crate::chart_data::format_number((slice.value * 100.0).round() / 100.0)));
    }
}

#[cfg(test)]
mod tests;
