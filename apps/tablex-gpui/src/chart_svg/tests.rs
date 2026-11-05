use super::*;
use crate::chart_data::{prepare, Aggregation, ChartKind, ChartSource, ChartSpec};
use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const DARK: SvgStyle = SvgStyle {
    background: [0x13, 0x14, 0x16],
    text: [0xd7, 0xd7, 0xdb],
    muted: [0x9c, 0x9d, 0xa2],
    grid: [0x2e, 0x2f, 0x32],
    palette: [[0x89, 0xad, 0xdf], [0x74, 0xc6, 0x9d], [0xe0, 0xa4, 0x58], [0xc7, 0x92, 0xea], [0xf0, 0x70, 0x7a]],
};

fn source() -> ChartSource {
    let region: ArrayRef = Arc::new(StringArray::from(vec![Some("<north> & \"co\""), Some("south"), Some("north"), Some("east"), None]));
    let amount: ArrayRef = Arc::new(Float64Array::from(vec![Some(10.0), Some(-4.0), Some(20.5), None, Some(7.0)]));
    let qty: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3), Some(4), Some(5)]));
    let when: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3), Some(4), Some(5)]));
    let batch = RecordBatch::try_from_iter(vec![("region", region), ("amount", amount), ("qty", qty), ("n", when)]).unwrap();
    ChartSource::new(&[Arc::new(batch)])
}

fn model(kind: ChartKind, x: usize, values: &[usize], aggregation: Aggregation, by: Option<usize>) -> crate::chart_data::ChartModel {
    let spec = ChartSpec { kind, x, values: values.to_vec(), aggregation, series_by: by };
    prepare(&source(), &spec, 1_000, &AtomicBool::new(false)).expect("a model")
}

fn parsed(svg: &str) -> resvg::usvg::Tree {
    resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()).expect("the SVG parses")
}

#[test]
fn every_chart_type_serialises_to_an_svg_that_parses_at_the_export_size() {
    let cases = [
        model(ChartKind::Bar, 0, &[1], Aggregation::None, None),
        model(ChartKind::Bar, 0, &[1, 2], Aggregation::Sum, None),
        model(ChartKind::Line, 3, &[1], Aggregation::None, None),
        model(ChartKind::Line, 3, &[1, 2], Aggregation::None, None),
        model(ChartKind::Area, 3, &[1, 2], Aggregation::None, None),
        model(ChartKind::Scatter, 3, &[1], Aggregation::None, None),
        model(ChartKind::Pie, 0, &[2], Aggregation::None, None),
    ];
    for model in cases {
        let svg = render(&model, &DARK);
        let tree = parsed(&svg);
        assert_eq!((tree.size().width(), tree.size().height()), (WIDTH, HEIGHT), "{:?}", model.kind);
        assert!(!svg.contains("NaN") && !svg.contains("inf"), "{:?}: a number is not finite", model.kind);
        assert!(svg.contains(&format!("fill=\"{}\"", hex(DARK.background))), "the theme background");
        assert!(svg.contains(&hex(DARK.palette[0])), "{:?}: the first series color", model.kind);
        assert!(svg.contains("<text"), "{:?}: labels", model.kind);
    }
}

#[test]
fn text_is_escaped_and_notes_and_legend_are_written() {
    let model = model(ChartKind::Bar, 0, &[1, 2], Aggregation::Sum, None);
    let svg = render(&model, &DARK);
    assert!(svg.contains("&lt;north&gt; &amp; &quot;co&quot;") || svg.contains("&lt;north"), "labels are escaped");
    assert!(!svg.contains("<north>"));
    assert!(svg.contains("amount") && svg.contains("qty"), "the legend names both series");
    assert!(svg.contains("NULL"), "the notes about skipped NULLs are exported: {svg}");
    parsed(&svg);
}

#[test]
fn bars_are_drawn_down_for_negative_values_and_gaps_are_not_drawn() {
    let model = model(ChartKind::Bar, 0, &[1], Aggregation::None, None);
    let svg = render(&model, &DARK);
    // Three categories ("east" has only a NULL amount, the NULL-region row is skipped): the background
    // and three bars, one of them hanging below the zero line.
    assert_eq!(svg.matches("<rect").count(), 1 + 3, "{svg}");
    assert!(svg.contains("y=\"363.5\" width=\"59\" height=\"41\""), "the -4 bar hangs from the zero line: {svg}");
}

#[test]
fn the_pie_lists_every_slice_with_its_share() {
    let model = model(ChartKind::Pie, 0, &[2], Aggregation::None, None);
    let svg = render(&model, &DARK);
    assert!(svg.contains("%)"), "shares: {svg}");
    assert_eq!(svg.matches("<path").count() + svg.matches("<circle").count(), 4, "one wedge per slice");
}

#[test]
fn series_colors_follow_the_theme_then_tint() {
    for i in 0..5 {
        assert_eq!(series_color(&DARK.palette, DARK.background, i), DARK.palette[i]);
    }
    let sixth = series_color(&DARK.palette, DARK.background, 5);
    assert_ne!(sixth, DARK.palette[0], "past five the palette is tinted");
    assert_ne!(series_color(&DARK.palette, DARK.background, 10), sixth);
    let distinct: std::collections::HashSet<Rgb> = (0..12).map(|i| series_color(&DARK.palette, DARK.background, i)).collect();
    assert_eq!(distinct.len(), 12, "twelve series, twelve colors");
}

#[test]
fn a_single_point_and_a_single_value_still_draw() {
    let region: ArrayRef = Arc::new(StringArray::from(vec![Some("only")]));
    let v: ArrayRef = Arc::new(Float64Array::from(vec![Some(5.0)]));
    let batch = RecordBatch::try_from_iter(vec![("k", region), ("v", v)]).unwrap();
    let source = ChartSource::new(&[Arc::new(batch)]);
    for kind in [ChartKind::Bar, ChartKind::Line, ChartKind::Area, ChartKind::Pie] {
        let spec = ChartSpec { kind, x: 0, values: vec![1], aggregation: Aggregation::None, series_by: None };
        let model = prepare(&source, &spec, 100, &AtomicBool::new(false)).unwrap();
        let svg = render(&model, &DARK);
        parsed(&svg);
        assert!(!svg.contains("NaN") && !svg.contains("inf"), "{kind:?}");
    }
}

#[test]
fn a_gap_breaks_the_line_and_has_no_marker() {
    // A mean over rows: "b" has no row for the middle X, so its line is two pieces, and the gap is not drawn as a value.
    let x: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3), Some(1), Some(3)]));
    let by: ArrayRef = Arc::new(StringArray::from(vec![Some("a"), Some("a"), Some("a"), Some("b"), Some("b")]));
    let v: ArrayRef = Arc::new(Float64Array::from(vec![Some(1.0), Some(2.0), Some(3.0), Some(5.0), Some(7.0)]));
    let batch = RecordBatch::try_from_iter(vec![("x", x), ("by", by), ("v", v)]).unwrap();
    let source = ChartSource::new(&[Arc::new(batch)]);
    let spec = ChartSpec { kind: ChartKind::Line, x: 0, values: vec![2], aggregation: Aggregation::Avg, series_by: Some(1) };
    let model = prepare(&source, &spec, 100, &AtomicBool::new(false)).unwrap();
    let svg = render(&model, &DARK);
    parsed(&svg);
    // Series "a" is one polyline of three points; "b" is two lone points (a dot each, no line between them),
    // and no marker sits on the points of the line, as on screen.
    assert_eq!(svg.matches("<polyline").count(), 1, "{svg}");
    assert_eq!(svg.matches("<circle").count(), 2, "{svg}");
    assert!(svg.contains("stroke-width=\"2\" stroke-linejoin"), "the 2px line of the official LineChart, as on screen");
    // With a sum a missing cell is 0 and the line is whole.
    let spec = ChartSpec { aggregation: Aggregation::Sum, ..spec };
    let model = prepare(&source, &spec, 100, &AtomicBool::new(false)).unwrap();
    let svg = render(&model, &DARK);
    assert_eq!(svg.matches("<polyline").count(), 2);
}

#[test]
fn characters_xml_cannot_hold_are_dropped_and_the_document_still_parses() {
    let region: ArrayRef = Arc::new(StringArray::from(vec![Some("a\u{FFFE}b\u{1}c\u{FFFF}d\u{8}e\u{B}f"), Some("tab\there\nline")]));
    let v: ArrayRef = Arc::new(Float64Array::from(vec![Some(1.0), Some(2.0)]));
    let batch = RecordBatch::try_from_iter(vec![("a\u{FFFE}xis", region), ("v", v)]).unwrap();
    let source = ChartSource::new(&[Arc::new(batch)]);
    let spec = ChartSpec { kind: ChartKind::Bar, x: 0, values: vec![1], aggregation: Aggregation::None, series_by: None };
    let model = prepare(&source, &spec, 100, &AtomicBool::new(false)).unwrap();
    let svg = render(&model, &DARK);
    assert!(svg.contains("abcdef") && svg.contains("axis"), "{svg}");
    for bad in ['\u{FFFE}', '\u{FFFF}', '\u{1}', '\u{8}', '\u{B}'] {
        assert!(!svg.contains(bad), "{bad:?} is in the document");
    }
    parsed(&svg);
    assert_eq!(escape("a&b<c>\"d\""), "a&amp;b&lt;c&gt;&quot;d&quot;");
}

#[test]
fn the_export_draws_lines_and_areas_as_the_screen_does() {
    let n = 30usize;
    let k: ArrayRef = Arc::new(Int64Array::from_iter_values(0..n as i64));
    let a: ArrayRef = Arc::new(Float64Array::from_iter_values((0..n).map(|i| (i % 7) as f64)));
    let b: ArrayRef = Arc::new(Float64Array::from_iter_values((0..n).map(|i| (i % 5) as f64 + 2.0)));
    let batch = RecordBatch::try_from_iter(vec![("k", k), ("a", a), ("b", b)]).unwrap();
    let source = ChartSource::new(&[Arc::new(batch)]);
    let render_of = |kind, values: &[usize]| {
        let spec = ChartSpec { kind, x: 0, values: values.to_vec(), aggregation: Aggregation::None, series_by: None };
        render(&prepare(&source, &spec, 100, &AtomicBool::new(false)).unwrap(), &DARK)
    };
    let line = render_of(ChartKind::Line, &[1, 2]);
    assert_eq!(line.matches("<polyline").count(), 2);
    assert_eq!(line.matches("<circle").count(), 0, "no markers on a line, however few points");
    assert_eq!(line.matches("stroke-width=\"2\" stroke-linejoin").count(), 2, "2px, as on screen");
    assert!(!line.contains("<polygon"));
    // Area fills: stronger alone than overlapping, the same as on screen.
    assert!(render_of(ChartKind::Area, &[1]).contains("fill-opacity=\"0.3\""));
    let two = render_of(ChartKind::Area, &[1, 2]);
    assert_eq!(two.matches("fill-opacity=\"0.18\"").count(), 2);
    assert_eq!(two.matches("stroke-width=\"1\" stroke-linejoin").count(), 2, "the outline of an area is 1px, as on screen");
}
