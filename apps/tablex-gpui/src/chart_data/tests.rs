use super::*;
use arrow_array::{
    Date32Array, Decimal128Array, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{Field, Schema};

fn batch(columns: Vec<(&str, ArrayRef)>) -> Arc<RecordBatch> {
    let fields: Vec<Field> = columns.iter().map(|(name, array)| Field::new(*name, array.data_type().clone(), true)).collect();
    Arc::new(RecordBatch::try_new(Arc::new(Schema::new(fields)), columns.into_iter().map(|(_, a)| a).collect()).unwrap())
}

fn text(values: &[Option<&str>]) -> ArrayRef {
    Arc::new(StringArray::from(values.to_vec()))
}

fn ints(values: &[Option<i64>]) -> ArrayRef {
    Arc::new(Int64Array::from(values.to_vec()))
}

fn floats(values: &[Option<f64>]) -> ArrayRef {
    Arc::new(Float64Array::from(values.to_vec()))
}

fn spec(kind: ChartKind, x: usize, values: &[usize], aggregation: Aggregation, series_by: Option<usize>) -> ChartSpec {
    ChartSpec { kind, x, values: values.to_vec(), aggregation, series_by }
}

fn run(source: &ChartSource, spec: &ChartSpec) -> Result<ChartModel, ChartIssue> {
    prepare(source, spec, 2_000, &AtomicBool::new(false))
}

fn categories(model: &ChartModel) -> &Categories {
    match &model.data {
        ChartData::Categories(categories) => categories,
        other => panic!("expected categories, got {other:?}"),
    }
}

fn labels(model: &ChartModel) -> Vec<String> {
    categories(model).labels.iter().map(|l| l.to_string()).collect()
}

/// region, amount (with repeats and a NULL), qty.
fn sales() -> ChartSource {
    let region = text(&[Some("north"), Some("south"), Some("north"), Some("east"), Some("south"), Some("north")]);
    let amount = floats(&[Some(10.0), Some(4.0), Some(20.0), Some(7.0), None, Some(30.0)]);
    let qty = ints(&[Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]);
    ChartSource::new(&[batch(vec![("region", region), ("amount", amount), ("qty", qty)])])
}

#[test]
fn every_aggregation_combines_the_rows_of_a_category() {
    let source = sales();
    let expect = |aggregation, north: f64, south: f64, east: f64| {
        let model = run(&source, &spec(ChartKind::Bar, 0, &[1], aggregation, None)).unwrap();
        assert_eq!(labels(&model), ["north", "south", "east"], "{aggregation:?}: categories keep arrival order");
        let values = &categories(&model).series[0].values;
        assert_eq!(values, &vec![Some(north), Some(south), Some(east)], "{aggregation:?}");
    };
    expect(Aggregation::Sum, 60.0, 4.0, 7.0);
    expect(Aggregation::Avg, 20.0, 4.0, 7.0);
    expect(Aggregation::Min, 10.0, 4.0, 7.0);
    expect(Aggregation::Max, 30.0, 4.0, 7.0);
    // Count counts rows (the NULL amount of the second "south" row included), whatever the value column.
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Count, None)).unwrap();
    assert_eq!(categories(&model).series[0].values, vec![Some(3.0), Some(2.0), Some(1.0)]);
    assert_eq!(categories(&model).series[0].name.as_ref(), "count");
    assert_eq!(model.y_title, "COUNT(*)");
}

#[test]
fn nulls_are_skipped_and_counted() {
    let region = text(&[Some("a"), None, Some("a"), Some("b")]);
    let amount = floats(&[Some(1.0), Some(5.0), None, None]);
    let source = ChartSource::new(&[batch(vec![("region", region), ("amount", amount)])]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    // "b" has only a NULL value: no category. The NULL category row is skipped.
    assert_eq!(labels(&model), ["a"]);
    assert_eq!(categories(&model).series[0].values, vec![Some(1.0)]);
    assert!(model.notes.contains(&ChartNote::NullCategory(1)), "{:?}", model.notes);
    assert!(model.notes.contains(&ChartNote::NullValue(2)), "{:?}", model.notes);
    assert_eq!(ChartNote::NullCategory(1).to_string(), "1 row with a NULL category skipped");
    assert_eq!(ChartNote::NullValue(2).to_string(), "2 NULL values skipped");
    assert_eq!(ChartNote::NullValue(1).to_string(), "1 NULL value skipped");
}

#[test]
fn series_by_splits_the_value_per_group_and_leaves_missing_cells_empty() {
    let month = text(&[Some("jan"), Some("jan"), Some("feb"), Some("mar"), Some("mar")]);
    let team = text(&[Some("red"), Some("blue"), Some("red"), Some("blue"), Some("blue")]);
    let hours = ints(&[Some(5), Some(7), Some(11), Some(2), Some(3)]);
    let source = ChartSource::new(&[batch(vec![("month", month), ("team", team), ("hours", hours)])]);
    let model = run(&source, &spec(ChartKind::Line, 0, &[2], Aggregation::Sum, Some(1))).unwrap();
    assert_eq!(labels(&model), ["jan", "feb", "mar"]);
    let series = &categories(&model).series;
    assert_eq!(series.iter().map(|s| s.name.to_string()).collect::<Vec<_>>(), ["red", "blue"]);
    assert_eq!(series[0].values, vec![Some(5.0), Some(11.0), None]);
    assert_eq!(series[1].values, vec![Some(7.0), None, Some(5.0)]);
    assert_eq!(model.title, "SUM(hours) by month, per team");

    // Two value columns and groups: "group · column".
    let qty = ints(&[Some(1); 5]);
    let hours = ints(&[Some(1); 5]);
    let source = ChartSource::new(&[batch(vec![
        ("month", text(&[Some("jan"); 5])),
        ("team", text(&[Some("red"), Some("blue"), Some("red"), Some("blue"), Some("blue")])),
        ("hours", hours),
        ("qty", qty),
    ])]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[2, 3], Aggregation::Sum, Some(1))).unwrap();
    assert_eq!(
        categories(&model).series.iter().map(|s| s.name.to_string()).collect::<Vec<_>>(),
        ["red · hours", "red · qty", "blue · hours", "blue · qty"]
    );
}

#[test]
fn pie_keeps_the_top_slices_and_folds_the_rest_into_other() {
    let names: Vec<String> = (0..15).map(|i| format!("c{i}")).collect();
    let values: Vec<Option<f64>> = (0..15).map(|i| Some(f64::from(i + 1))).collect();
    let region: ArrayRef = Arc::new(StringArray::from(names.iter().map(|n| Some(n.as_str())).collect::<Vec<_>>()));
    let source = ChartSource::new(&[batch(vec![("name", region), ("v", floats(&values))])]);
    for aggregation in [Aggregation::None, Aggregation::Sum] {
        let model = run(&source, &spec(ChartKind::Pie, 0, &[1], aggregation, None)).unwrap();
        let ChartData::Slices(slices) = &model.data else { panic!("slices") };
        assert_eq!(slices.len(), MAX_PIE_SLICES + 1, "{aggregation:?}");
        assert_eq!(slices[0].label.as_ref(), "c14", "biggest first");
        assert_eq!(slices[0].value, 15.0);
        let other = slices.last().unwrap();
        assert!(other.other);
        assert_eq!(other.label.as_ref(), "Other (4)");
        // The smallest four: 1 + 2 + 3 + 4.
        assert_eq!(other.value, 10.0, "{aggregation:?}");
        assert_eq!(slices.iter().map(|s| s.value).sum::<f64>(), 120.0, "nothing is lost");
    }
    // Exactly as many as the cap: no Other.
    let few: ArrayRef = Arc::new(StringArray::from(vec![Some("a"), Some("b")]));
    let source = ChartSource::new(&[batch(vec![("name", few), ("v", floats(&[Some(1.0), Some(2.0)]))])]);
    let model = run(&source, &spec(ChartKind::Pie, 0, &[1], Aggregation::None, None)).unwrap();
    let ChartData::Slices(slices) = &model.data else { panic!() };
    assert!(slices.iter().all(|s| !s.other));
}

#[test]
fn a_pie_refuses_negative_values() {
    let source = ChartSource::new(&[batch(vec![("n", text(&[Some("a"), Some("b")])), ("v", floats(&[Some(1.0), Some(-2.0)]))])]);
    for aggregation in [Aggregation::None, Aggregation::Sum] {
        let error = run(&source, &spec(ChartKind::Pie, 0, &[1], aggregation, None)).unwrap_err();
        assert_eq!(error, ChartIssue::NegativePie { column: "v".into() }, "{aggregation:?}");
    }
}

#[test]
fn bars_stop_at_fifty_categories_and_say_how_many_there_are() {
    let names: Vec<String> = (0..80).map(|i| format!("k{i:02}")).collect();
    let cats: ArrayRef = Arc::new(StringArray::from(names.iter().map(|n| Some(n.as_str())).collect::<Vec<_>>()));
    let vals = ints(&(0..80).map(Some).collect::<Vec<_>>());
    let source = ChartSource::new(&[batch(vec![("k", cats), ("v", vals)])]);
    for aggregation in [Aggregation::None, Aggregation::Sum] {
        let model = run(&source, &spec(ChartKind::Bar, 0, &[1], aggregation, None)).unwrap();
        assert_eq!(categories(&model).labels.len(), MAX_BARS, "{aggregation:?}");
        assert_eq!(labels(&model)[0], "k00", "the first rows, in the order they came");
        assert_eq!(labels(&model)[49], "k49");
        let note = model.notes.iter().find(|n| matches!(n, ChartNote::Showing { .. })).expect("a note");
        let what = if aggregation == Aggregation::None { "rows" } else { "categories" };
        assert_eq!(note.to_string(), format!("showing 50 of 80 {what}"), "{aggregation:?}");
    }
    // Aggregated values past the cut are not summed into the shown bars.
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert_eq!(categories(&model).series[0].values[49], Some(49.0));
}

#[test]
fn line_and_area_points_are_bounded_and_keep_the_extremes() {
    let n = 100_000usize;
    let x = ints(&(0..n as i64).map(Some).collect::<Vec<_>>());
    // A slow wave with one spike and one dip that a stride sample would miss.
    let mut y: Vec<Option<f64>> = (0..n).map(|i| Some((i as f64 / 500.0).sin() * 10.0)).collect();
    y[31_337] = Some(500.0);
    y[77_777] = Some(-400.0);
    let source = ChartSource::new(&[batch(vec![("t", x), ("y", floats(&y))])]);
    for kind in [ChartKind::Line, ChartKind::Area] {
        for max_points in [64usize, 1_000, 3_201] {
            let model = prepare(&source, &spec(kind, 0, &[1], Aggregation::None, None), max_points, &AtomicBool::new(false)).unwrap();
            let c = categories(&model);
            assert!(c.labels.len() <= max_points, "{kind:?} {max_points}: {} points", c.labels.len());
            assert!(c.labels.len() >= max_points - 2, "close to the budget");
            assert_eq!(c.series[0].values.len(), c.labels.len());
            let seen: Vec<f64> = c.series[0].values.iter().flatten().copied().collect();
            assert_eq!(seen.iter().copied().fold(f64::MIN, f64::max), 500.0, "the spike survives");
            assert_eq!(seen.iter().copied().fold(f64::MAX, f64::min), -400.0, "the dip survives");
            // Labels keep the X order.
            let xs: Vec<i64> = c.labels.iter().map(|l| l.parse().unwrap()).collect();
            assert!(xs.windows(2).all(|w| w[0] <= w[1]), "{xs:?}");
            assert!(model.notes.iter().any(|n| n.to_string().starts_with("showing")), "{:?}", model.notes);
        }
    }
    // Few points are untouched.
    let model = prepare(&source, &spec(ChartKind::Line, 0, &[1], Aggregation::None, None), 200_000, &AtomicBool::new(false)).unwrap();
    assert_eq!(categories(&model).labels.len(), n);
    assert!(model.notes.is_empty());
}

#[test]
fn dates_and_numbers_sort_chronologically_for_line_and_area_but_bars_keep_arrival_order() {
    let day = |d: i32| d; // days since the epoch
    let dates: ArrayRef = Arc::new(Date32Array::from(vec![Some(day(19_000 + 2)), Some(day(19_000)), Some(day(19_000 + 1)), None]));
    let v = ints(&[Some(2), Some(0), Some(1), Some(9)]);
    let source = ChartSource::new(&[batch(vec![("d", dates), ("v", v)])]);
    for kind in [ChartKind::Line, ChartKind::Area] {
        let model = run(&source, &spec(kind, 0, &[1], Aggregation::None, None)).unwrap();
        assert_eq!(labels(&model), ["2022-01-08", "2022-01-09", "2022-01-10"], "{kind:?}");
        assert_eq!(categories(&model).series[0].values, vec![Some(0.0), Some(1.0), Some(2.0)]);
        assert!(model.notes.contains(&ChartNote::NullCategory(1)));
    }
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model), ["2022-01-10", "2022-01-08", "2022-01-09"]);

    // Timestamps keep the time of day; sorting also works after aggregation.
    let micros = |s: i64| s * 1_000_000;
    let ts: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![
        Some(micros(1_700_003_600)),
        Some(micros(1_700_000_000)),
        Some(micros(1_700_003_600)),
    ]));
    let source = ChartSource::new(&[batch(vec![("t", ts), ("v", ints(&[Some(1), Some(2), Some(3)]))])]);
    let model = run(&source, &spec(ChartKind::Line, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert_eq!(labels(&model), ["2023-11-14 22:13:20", "2023-11-14 23:13:20"]);
    assert_eq!(categories(&model).series[0].values, vec![Some(2.0), Some(4.0)]);

    // Numeric X sorts numerically (not as text).
    let x = ints(&[Some(10), Some(9), Some(100)]);
    let source = ChartSource::new(&[batch(vec![("x", x), ("v", ints(&[Some(1), Some(2), Some(3)]))])]);
    let model = run(&source, &spec(ChartKind::Line, 0, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model), ["9", "10", "100"]);
}

#[test]
fn text_that_parses_is_refused_not_converted() {
    let source = ChartSource::new(&[batch(vec![("k", text(&[Some("a"), Some("b")])), ("n", text(&[Some("1"), Some("2")]))])]);
    for aggregation in [Aggregation::None, Aggregation::Sum, Aggregation::Avg] {
        let error = run(&source, &spec(ChartKind::Bar, 0, &[1], aggregation, None)).unwrap_err();
        assert_eq!(error, ChartIssue::NotNumeric { column: "n".into() });
    }
    assert!(ChartIssue::NotNumeric { column: "n".into() }.to_string().contains("text is never converted"));
    // Count needs no value column, and no numeric column is the other message.
    let model = run(&source, &spec(ChartKind::Bar, 0, &[], Aggregation::Count, None)).unwrap();
    assert_eq!(categories(&model).series[0].values, vec![Some(1.0), Some(1.0)]);
    assert_eq!(run(&source, &spec(ChartKind::Bar, 0, &[], Aggregation::Sum, None)).unwrap_err(), ChartIssue::NoNumericColumn);
    let numeric = sales();
    assert_eq!(run(&numeric, &spec(ChartKind::Bar, 0, &[], Aggregation::Sum, None)).unwrap_err(), ChartIssue::NoValueColumn);
}

#[test]
fn every_numeric_arrow_type_is_a_value() {
    let decimals: ArrayRef = Arc::new(Decimal128Array::from(vec![Some(1250), Some(250), None]).with_precision_and_scale(10, 2).unwrap());
    let unsigned: ArrayRef = Arc::new(UInt64Array::from(vec![Some(u64::MAX / 2), Some(1), Some(2)]));
    let small: ArrayRef = Arc::new(arrow_array::Int8Array::from(vec![Some(-3), Some(4), Some(5)]));
    let single: ArrayRef = Arc::new(arrow_array::Float32Array::from(vec![Some(0.5), Some(1.5), Some(2.5)]));
    let source = ChartSource::new(&[batch(vec![
        ("k", text(&[Some("a"), Some("a"), Some("b")])),
        ("dec", decimals),
        ("u", unsigned),
        ("s", small),
        ("f", single),
    ])]);
    let sum = |column| {
        let model = run(&source, &spec(ChartKind::Bar, 0, &[column], Aggregation::Sum, None)).unwrap();
        categories(&model).series[0].values.clone()
    };
    assert_eq!(sum(1), vec![Some(15.0)], "12.50 + 2.50; \"b\" has only a NULL, so it is no category");
    assert_eq!(sum(3), vec![Some(1.0), Some(5.0)]);
    assert_eq!(sum(4), vec![Some(2.0), Some(2.5)]);
    assert!(sum(2)[1] == Some(2.0));
    assert_eq!(source.columns()[1].class, ColumnClass::Numeric);
}

#[test]
fn too_many_series_is_refused_with_a_count() {
    let n = 30usize;
    let names: Vec<String> = (0..n).map(|i| format!("s{i}")).collect();
    let by: ArrayRef = Arc::new(StringArray::from(names.iter().map(|n| Some(n.as_str())).collect::<Vec<_>>()));
    let source = ChartSource::new(&[batch(vec![
        ("x", ints(&(0..n as i64).map(Some).collect::<Vec<_>>())),
        ("by", by),
        ("v", ints(&vec![Some(1); n])),
    ])]);
    let error = run(&source, &spec(ChartKind::Line, 0, &[2], Aggregation::Sum, Some(1))).unwrap_err();
    assert_eq!(error, ChartIssue::TooManySeries { at_least: MAX_SERIES + 1, max: MAX_SERIES });
    assert!(error.to_string().starts_with("Too many series: at least 13"));
    // Exactly the cap is fine.
    let source = ChartSource::new(&[batch(vec![
        ("x", ints(&(0..12).map(Some).collect::<Vec<_>>())),
        ("by", Arc::new(StringArray::from((0..12).map(|i| Some(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"][i])).collect::<Vec<_>>())) as ArrayRef),
        ("v", ints(&[Some(1); 12])),
    ])]);
    assert!(run(&source, &spec(ChartKind::Line, 0, &[2], Aggregation::Sum, Some(1))).is_ok());
    // A pie and a scatter do not split by a series column, and say so.
    let model = run(&source, &spec(ChartKind::Pie, 0, &[2], Aggregation::Sum, Some(1))).unwrap();
    assert!(model.notes.iter().any(|n| matches!(n, ChartNote::Ignored(_))));
}

#[test]
fn scatter_needs_numbers_or_dates_along_x_and_caps_its_points() {
    let source = sales();
    assert_eq!(
        run(&source, &spec(ChartKind::Scatter, 0, &[1], Aggregation::None, None)).unwrap_err(),
        ChartIssue::ScatterNeedsNumericX { column: "region".into() }
    );
    let n = 40_000usize;
    let x = floats(&(0..n).map(|i| Some(i as f64)).collect::<Vec<_>>());
    let y = floats(&(0..n).map(|i| Some((i % 97) as f64)).collect::<Vec<_>>());
    let source = ChartSource::new(&[batch(vec![("x", x), ("y", y)])]);
    let model = run(&source, &spec(ChartKind::Scatter, 0, &[1], Aggregation::None, None)).unwrap();
    let ChartData::Points(points) = &model.data else { panic!() };
    let drawn = points.series[0].points.len();
    assert!(drawn <= MAX_SCATTER_POINTS && drawn > MAX_SCATTER_POINTS / 2, "{drawn} points");
    // Evenly spread over the whole range, in order.
    let xs: Vec<f64> = points.series[0].points.iter().map(|p| p[0]).collect();
    assert!(xs.windows(2).all(|w| w[0] < w[1]));
    assert!(xs[0] < 10.0 && *xs.last().unwrap() > n as f64 - 20_000.0, "{} .. {}", xs[0], xs.last().unwrap());
    let note = model.notes.iter().find(|n| matches!(n, ChartNote::Showing { .. })).expect("the cap is announced");
    assert!(note.to_string().contains("of 40000 points"), "{note}");
    // Small data is complete, with no note.
    let source = ChartSource::new(&[batch(vec![("x", floats(&[Some(1.0), Some(2.0)])), ("y", floats(&[Some(3.0), None]))])]);
    let model = run(&source, &spec(ChartKind::Scatter, 0, &[1], Aggregation::None, None)).unwrap();
    let ChartData::Points(points) = &model.data else { panic!() };
    assert_eq!(points.series[0].points, vec![[1.0, 3.0]]);
    assert!(model.notes.contains(&ChartNote::NullValue(1)));
}

#[test]
fn defaults_pick_the_first_text_or_date_and_the_first_number() {
    let columns = |classes: &[ColumnClass]| -> Vec<ChartColumn> {
        classes.iter().enumerate().map(|(i, class)| ChartColumn { name: format!("c{i}"), class: *class, from_text: false }).collect()
    };
    use ColumnClass::*;
    let spec = ChartSpec::defaults(&columns(&[Numeric, Text, Numeric]), |_| false);
    assert_eq!((spec.x, spec.values.clone(), spec.aggregation), (1, vec![0], Aggregation::None));
    let spec = ChartSpec::defaults(&columns(&[Numeric, Temporal, Numeric]), |x| x == 1);
    assert_eq!((spec.x, spec.values.clone(), spec.aggregation), (1, vec![0], Aggregation::Sum), "repeats -> Sum");
    let spec = ChartSpec::defaults(&columns(&[Numeric, Numeric]), |_| false);
    assert_eq!((spec.x, spec.values.clone()), (0, vec![1]));
    let spec = ChartSpec::defaults(&columns(&[Numeric]), |_| false);
    assert_eq!((spec.x, spec.values.clone()), (0, vec![0]));
    let spec = ChartSpec::defaults(&columns(&[Text, Text]), |_| true);
    assert!(spec.values.is_empty(), "no numeric column to chart");
    assert_eq!(spec.aggregation, Aggregation::None);
    assert_eq!(spec.kind, ChartKind::Bar);
    // The repeat probe reads the real column.
    let source = sales();
    assert!(source.repeats_within(0, 100));
    assert!(!source.repeats_within(2, 100));
    assert!(!source.repeats_within(0, 2), "only the sampled rows count");
}

#[test]
fn repeats_without_an_aggregation_show_the_last_row_and_say_so() {
    let source = sales();
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::None, Some(0))).unwrap();
    // 6 rows, 3 regions, one NULL amount: two rows land on an X / series pair that is already taken.
    assert!(model.notes.contains(&ChartNote::Overwritten(2)), "{:?}", model.notes);
    assert_eq!(categories(&model).series[0].values[0], Some(30.0), "the last north row wins");
}

#[test]
fn a_replaced_request_stops_early() {
    let n = CHUNK_ROWS * 3;
    let source = ChartSource::new(&[batch(vec![
        ("k", ints(&(0..n as i64).map(Some).collect::<Vec<_>>())),
        ("v", ints(&vec![Some(1); n])),
    ])]);
    let cancel = AtomicBool::new(true);
    let error = prepare(&source, &spec(ChartKind::Line, 0, &[1], Aggregation::None, None), 1000, &cancel).unwrap_err();
    assert_eq!(error, ChartIssue::Cancelled);
    assert!(run(&source, &spec(ChartKind::Line, 0, &[1], Aggregation::None, None)).is_ok());
}

#[test]
fn empty_inputs_have_their_own_messages() {
    let source = ChartSource::new(&[]);
    assert_eq!(run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::None, None)).unwrap_err(), ChartIssue::NoRows);
    let source = ChartSource::new(&[batch(vec![("k", text(&[None, None])), ("v", ints(&[Some(1), Some(2)]))])]);
    assert_eq!(run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::None, None)).unwrap_err(), ChartIssue::NothingToDraw);
    let source = sales();
    assert_eq!(run(&source, &spec(ChartKind::Bar, 9, &[1], Aggregation::None, None)).unwrap_err(), ChartIssue::UnknownColumn);
    assert_eq!(run(&source, &spec(ChartKind::Bar, 0, &[9], Aggregation::Sum, None)).unwrap_err(), ChartIssue::UnknownColumn);
}

#[test]
fn many_batches_are_read_as_one_result() {
    let first = batch(vec![("k", text(&[Some("a"), Some("b")])), ("v", ints(&[Some(1), Some(2)]))]);
    let second = batch(vec![("k", text(&[Some("b"), Some("c")])), ("v", ints(&[Some(10), Some(20)]))]);
    let source = ChartSource::new(&[first, second]);
    assert_eq!(source.rows(), 4);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert_eq!(labels(&model), ["a", "b", "c"]);
    assert_eq!(categories(&model).series[0].values, vec![Some(1.0), Some(12.0), Some(20.0)]);
}

#[test]
fn the_decimator_stays_bounded_and_spread() {
    for total in [10usize, 99, 100, 101, 1_000, 12_345] {
        let mut d = Decimator::new(100);
        for i in 0..total {
            d.push([i as f64, 0.0]);
        }
        assert_eq!(d.total, total);
        assert!(d.points.len() <= 100, "{total}: {}", d.points.len());
        if total <= 100 {
            assert_eq!(d.points.len(), total);
        } else {
            assert!(d.points.len() > 50, "{total}: {}", d.points.len());
            let gaps: Vec<f64> = d.points.windows(2).map(|w| w[1][0] - w[0][0]).collect();
            assert!(gaps.iter().all(|g| *g == gaps[0]), "{total}: even gaps {gaps:?}");
        }
    }
}

#[test]
fn ticks_and_labels() {
    assert_eq!(nice_ticks(0.0, 100.0, 5), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
    assert_eq!(nice_ticks(0.0, 7.0, 5), vec![0.0, 2.0, 4.0, 6.0, 8.0]);
    assert_eq!(nice_ticks(-3.0, 3.0, 5), vec![-4.0, -2.0, 0.0, 2.0, 4.0]);
    assert_eq!(nice_ticks(5.0, 5.0, 5), vec![5.0]);
    assert!(nice_ticks(0.0, f64::NAN, 5).is_empty());
    assert_eq!(format_tick(1_500_000.0, 500_000.0), "1.5M");
    assert_eq!(format_tick(2_000_000_000.0, 5e8), "2B");
    assert_eq!(format_tick(266_742.0, 66_000.0), "266.7K");
    assert_eq!(format_tick(-20_000.0, 10_000.0), "-20K");
    assert_eq!(format_tick(9_999.0, 2_000.0), "9999");
    assert_eq!(format_tick(0.25, 0.05), "0.25");
    assert_eq!(format_tick(-0.0, 1.0), "0");
    assert_eq!(format_tick(1234.0, 200.0), "1234");
    assert_eq!(format_number(3.0), "3");
    assert_eq!(format_number(0.125), "0.125");
    assert_eq!(format_number(1.0 / 3.0), "0.333333");
    assert_eq!(format_time(0), "1970-01-01");
    assert_eq!(format_time(86_400_000_000 + 5_000_000), "1970-01-02 00:00:05");
}

#[test]
fn long_text_labels_are_cut() {
    let long = "x".repeat(10_000);
    let source = ChartSource::new(&[batch(vec![("k", text(&[Some(long.as_str())])), ("v", ints(&[Some(1)]))])]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model)[0].chars().count(), MAX_LABEL_CHARS + 1);
}

#[test]
fn booleans_and_other_types_are_categories() {
    let flags: ArrayRef = Arc::new(arrow_array::BooleanArray::from(vec![Some(true), Some(false), Some(true)]));
    let source = ChartSource::new(&[batch(vec![("ok", flags), ("v", ints(&[Some(1), Some(2), Some(3)]))])]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert_eq!(labels(&model), ["true", "false"]);
    assert_eq!(categories(&model).series[0].values, vec![Some(4.0), Some(2.0)]);
}

// ---- text-encoded numbers and dates (how the SQL drivers deliver them) ----------------------------------------

fn typed(columns: Vec<(&str, ArrayRef)>, types: &[Option<&str>]) -> ChartSource {
    let types: Vec<Option<String>> = types.iter().map(|t| t.map(str::to_owned)).collect();
    ChartSource::with_types(&[batch(columns)], &types)
}

#[test]
fn type_names_say_which_text_is_a_number_or_a_date() {
    use ColumnClass::{Numeric, Temporal};
    for name in [
        "numeric", "NUMERIC(10,2)", "decimal", "decimal(10, 2)", "DECIMAL(10,2) UNSIGNED", "bigint", "bigint unsigned", "int", "integer",
        "smallint", "double", "double precision", "float", "real", "tinyint", "mediumint", "Nullable(Decimal(18, 4))", "Int64", "Float64",
        "LowCardinality(Nullable(UInt8))", "NUMBER", "serial",
    ] {
        assert_eq!(class_of_type_name(name), Some(Numeric), "{name}");
    }
    for name in [
        "date", "DATE", "datetime", "datetime(6)", "timestamp", "timestamp(3) with time zone", "timestamp without time zone",
        "timestamp with time zone", "timestamptz", "DateTime64(3, 'UTC')", "Nullable(Date32)", "datetime2", "smalldatetime",
    ] {
        assert_eq!(class_of_type_name(name), Some(Temporal), "{name}");
    }
    for name in [
        "text", "varchar", "character varying", "char", "uuid", "json", "jsonb", "money", "time", "time without time zone", "timetz",
        "interval", "year", "bytea", "boolean", "bit", "geometry", "enum", "int[]", "Array(Int32)", "numeric[]", "string", "", "mixed",
    ] {
        assert_eq!(class_of_type_name(name), None, "{name} stays text");
    }
}

#[test]
fn numbers_and_dates_are_parsed_strictly() {
    for (text, want) in [("12", 12.0), ("-3.50", -3.5), ("+7", 7.0), (" 1e3 ", 1000.0), (".5", 0.5), ("0012.10", 12.1)] {
        assert_eq!(parse_number(text), Some(want), "{text}");
    }
    for text in ["", "abc", "1,234", "$5", "1 2", "0x10", "12abc", "--1", "1_000"] {
        assert_eq!(parse_number(text), None, "{text:?}");
    }
    assert!(parse_number("NaN").is_some_and(f64::is_nan) && parse_number("Infinity").is_some_and(f64::is_infinite));

    let day = 86_400_000_000i64;
    assert_eq!(parse_time("1970-01-02"), Some(day));
    assert_eq!(parse_time("1970-01-01 00:00:01"), Some(1_000_000));
    assert_eq!(parse_time("1970-01-01T00:00:01"), Some(1_000_000));
    assert_eq!(parse_time("1970-01-01 00:00:01.25"), Some(1_250_000));
    assert_eq!(parse_time("1970-01-01 00:00:01.123456"), Some(1_123_456));
    // The wall clock as written, whatever the offset: the grid shows the same text.
    let hours = |h: i64| h * 3_600_000_000;
    assert_eq!(parse_time("1970-01-01 02:00:00+02"), Some(hours(2)), "a +02 offset is not applied");
    assert_eq!(parse_time("1970-01-01 02:00:00+02:00"), Some(hours(2)));
    assert_eq!(parse_time("1970-01-01 05:30:00+0530"), Some(hours(5) + 1_800_000_000));
    assert_eq!(parse_time("1969-12-31 19:00:00-05"), Some(-hours(5)));
    assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_time("2024-01-05 00:00:00+07"), parse_time("2024-01-05"), "a day-truncated value in a +07 session is that day");
    for text in ["", "0000-00-00", "0000-00-00 00:00:00", "infinity", "-infinity", "yesterday", "2024-13-01", "2024-02-30", "12:30:00", "2024/01/05"] {
        assert_eq!(parse_time(text), None, "{text:?}");
    }
}

#[test]
fn text_columns_typed_as_numeric_or_date_by_the_database_are_charted_like_native_ones() {
    // PostgreSQL: AVG(x) / SUM(bigint) are `numeric`, timestamps are text, all under `Utf8`.
    let region = text(&[Some("n"), Some("s"), Some("n"), Some("s")]);
    let avg = text(&[Some("1.50"), Some("2.25"), Some("3.50"), Some("10")]);
    let when = text(&[Some("2024-01-03 10:00:00+00"), Some("2024-01-01 10:00:00+00"), Some("2024-01-02 12:00:00+02"), Some("2024-01-04")]);
    let source = typed(vec![("region", region), ("avg", avg), ("ts", when)], &[Some("text"), Some("numeric"), Some("timestamp with time zone")]);
    let columns = source.columns();
    assert_eq!((columns[0].class, columns[1].class, columns[2].class), (ColumnClass::Text, ColumnClass::Numeric, ColumnClass::Temporal));
    assert!(!columns[0].from_text && columns[1].from_text && columns[2].from_text);

    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert_eq!(categories(&model).series[0].values, vec![Some(5.0), Some(12.25)]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Avg, None)).unwrap();
    assert_eq!(categories(&model).series[0].values, vec![Some(2.5), Some(6.125)]);
    // The timestamptz column orders by the wall clock as written, as a line's X.
    let model = run(&source, &spec(ChartKind::Line, 2, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model), ["2024-01-01 10:00:00", "2024-01-02 12:00:00", "2024-01-03 10:00:00", "2024-01-04"], "the wall clock as the grid shows it");
    assert_eq!(categories(&model).series[0].values, vec![Some(2.25), Some(3.5), Some(1.5), Some(10.0)]);
    // And as a scatter's X.
    let model = run(&source, &spec(ChartKind::Scatter, 2, &[1], Aggregation::None, None)).unwrap();
    let ChartData::Points(points) = &model.data else { panic!() };
    assert!(points.x_temporal && points.series[0].points.len() == 4);
}

#[test]
fn mysql_shaped_pages_without_native_types_are_charted_by_the_drivers_type_names() {
    // MySQL / MariaDB: DECIMAL (SUM(int)), DATE and DATETIME stay text; the driver's type names are `decimal`, `date`, `datetime`.
    let g = text(&[Some("a"), Some("b"), Some("a")]);
    let total = text(&[Some("3"), Some("5"), Some("4")]);
    let day = text(&[Some("2024-01-03"), Some("2024-01-01"), Some("2024-01-02")]);
    let at = text(&[Some("2024-01-03 10:00:00"), Some("2024-01-01 10:00:00.500000"), Some("0000-00-00 00:00:00")]);
    let source = typed(vec![("g", g), ("total", total), ("d", day), ("at", at)], &[Some("varchar"), Some("decimal"), Some("date"), Some("datetime")]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert_eq!(categories(&model).series[0].values, vec![Some(7.0), Some(5.0)]);
    let model = run(&source, &spec(ChartKind::Line, 2, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model), ["2024-01-01", "2024-01-02", "2024-01-03"]);
    // MySQL's zero date is not a date: skipped and counted, not a crash and not epoch 0.
    let model = run(&source, &spec(ChartKind::Line, 3, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model), ["2024-01-01 10:00:00", "2024-01-03 10:00:00"]);
    assert!(model.notes.contains(&ChartNote::Unparsed(1)), "{:?}", model.notes);
    assert_eq!(ChartNote::Unparsed(1).to_string(), "1 cell that is not a valid number or date skipped");
    assert_eq!(ChartNote::Unparsed(2).to_string(), "2 cells that are not a valid number or date skipped");
}

#[test]
fn real_text_stays_text_even_when_it_looks_like_numbers() {
    let k = text(&[Some("a"), Some("b")]);
    let n = text(&[Some("1"), Some("2")]);
    // Typed as text by the database (or untyped): refused, as before.
    for hint in [Some("varchar(10)"), Some("text"), Some("char"), Some("string"), None] {
        let source = typed(vec![("k", k.clone()), ("n", n.clone())], &[None, hint]);
        let error = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap_err();
        assert_eq!(error, ChartIssue::NotNumeric { column: "n".into() }, "{hint:?}");
    }
    // A typed column holding something that is not a number: the cell is skipped and counted, never coerced.
    let source = typed(vec![("k", text(&[Some("a"), Some("a"), Some("b")])), ("n", text(&[Some("1"), Some("1,5"), Some("abc")]))], &[None, Some("numeric")]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert_eq!(labels(&model), ["a"]);
    assert_eq!(categories(&model).series[0].values, vec![Some(1.0)]);
    assert!(model.notes.contains(&ChartNote::Unparsed(2)), "{:?}", model.notes);
}

#[test]
fn skipped_cells_are_named_for_what_they_are() {
    let k = text(&[Some("a"), None, Some("a"), Some("b")]);
    let s = text(&[Some("x"), Some("x"), None, Some("x")]);
    let v = floats(&[Some(1.0), Some(1.0), Some(1.0), Some(f64::NAN)]);
    let source = ChartSource::new(&[batch(vec![("k", k), ("s", s), ("v", v)])]);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[2], Aggregation::Sum, Some(1))).unwrap();
    assert!(model.notes.contains(&ChartNote::NullCategory(1)), "{:?}", model.notes);
    assert!(model.notes.contains(&ChartNote::NullSeries(1)), "{:?}", model.notes);
    assert!(model.notes.contains(&ChartNote::NonFinite(1)), "{:?}", model.notes);
    assert!(!model.notes.iter().any(|n| matches!(n, ChartNote::NullValue(_))));
    assert_eq!(ChartNote::NullSeries(1).to_string(), "1 row with a NULL series skipped");
    assert_eq!(ChartNote::NonFinite(3).to_string(), "3 non-finite values (NaN or infinity) skipped");
}

#[test]
fn a_chart_of_one_page_says_so() {
    let source = sales().partial(true);
    let model = run(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert!(model.notes.contains(&ChartNote::LoadedOnly { rows: 6 }));
    assert_eq!(ChartNote::LoadedOnly { rows: 200 }.to_string(), "charted from the 200 rows loaded; the result has more");
    let whole = run(&sales(), &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None)).unwrap();
    assert!(!whole.notes.iter().any(|n| matches!(n, ChartNote::LoadedOnly { .. })));
}

#[test]
fn more_value_columns_than_series_are_refused() {
    let columns: Vec<(String, ArrayRef)> = (0..14).map(|i| (format!("v{i}"), ints(&[Some(1), Some(2)]))).collect();
    let mut all: Vec<(&str, ArrayRef)> = vec![("k", text(&[Some("a"), Some("b")]))];
    all.extend(columns.iter().map(|(name, array)| (name.as_str(), array.clone())));
    let source = ChartSource::new(&[batch(all)]);
    let values: Vec<usize> = (1..=13).collect();
    let error = run(&source, &spec(ChartKind::Line, 0, &values, Aggregation::Sum, None)).unwrap_err();
    assert_eq!(error, ChartIssue::TooManySeries { at_least: 13, max: MAX_SERIES });
    assert!(run(&source, &spec(ChartKind::Line, 0, &values[..12], Aggregation::Sum, None)).is_ok());
}

#[test]
fn series_that_appear_late_count_against_the_cell_budget() {
    // 400,000 distinct X for one series, then eleven more series each of which gets a slot for all of them.
    let rows = 400_000usize;
    let mut x: Vec<Option<i64>> = (0..rows as i64).map(Some).collect();
    let mut by: Vec<Option<String>> = vec![Some("g0".into()); rows];
    for g in 1..12 {
        x.push(Some(0));
        by.push(Some(format!("g{g}")));
    }
    let by: ArrayRef = Arc::new(StringArray::from(by));
    let n = x.len();
    let source = ChartSource::new(&[batch(vec![("x", ints(&x)), ("by", by), ("v", ints(&vec![Some(1); n]))])]);
    let error = run(&source, &spec(ChartKind::Line, 0, &[2], Aggregation::Sum, Some(1))).unwrap_err();
    assert!(matches!(error, ChartIssue::TooManyPoints { cells } if cells > MAX_CELLS), "{error:?}");
}

#[test]
fn work_stops_between_batches_once_it_is_replaced() {
    let parts: Vec<Arc<RecordBatch>> = (0..3).map(|_| batch(vec![("k", text(&[Some("a")])), ("v", ints(&[Some(1)]))])).collect();
    let source = ChartSource::new(&parts);
    let cancel = AtomicBool::new(true);
    // Far fewer rows than the per-row poll interval: the batch boundary is what notices.
    let error = prepare(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None), 100, &cancel).unwrap_err();
    assert_eq!(error, ChartIssue::Cancelled);
}

#[test]
fn a_missing_mean_is_a_break_in_the_line_and_a_missing_sum_is_zero() {
    let values = [None, Some(2.0), None, None, Some(8.0), None];
    assert_eq!(drawn_values(&values, Aggregation::Avg), values.to_vec(), "a mean's gap stays a gap");
    assert_eq!(drawn_values(&values, Aggregation::None), values.to_vec());
    assert_eq!(drawn_values(&values, Aggregation::Sum), vec![Some(0.0), Some(2.0), Some(0.0), Some(0.0), Some(8.0), Some(0.0)]);
    assert_eq!(drawn_values(&values, Aggregation::Count), drawn_values(&values, Aggregation::Sum));
    // The pieces of the line: one for each stretch with a value.
    assert_eq!(runs(&values), vec![1..2, 4..5]);
    assert_eq!(runs(&drawn_values(&values, Aggregation::Sum)), vec![0..6], "a sum has no gap");
    assert_eq!(runs(&[Some(1.0), Some(2.0), None, Some(3.0), Some(4.0), Some(5.0)]), vec![0..2, 3..6]);
    assert_eq!(runs(&[None, None]), Vec::<std::ops::Range<usize>>::new());
    assert_eq!(runs(&[]), Vec::<std::ops::Range<usize>>::new());
}

#[test]
fn a_timestamptz_in_a_session_with_an_offset_is_labelled_as_the_grid_shows_it() {
    // `date_trunc('day', ts)` in a +07 session: midnight local, printed with its offset.
    let day = text(&[Some("2024-01-05 00:00:00+07"), Some("2024-01-03 00:00:00+07"), Some("2024-01-04 00:00:00+07")]);
    let v = ints(&[Some(5), Some(3), Some(4)]);
    let source = typed(vec![("d", day), ("v", v)], &[Some("timestamp with time zone"), None]);
    let model = run(&source, &spec(ChartKind::Line, 0, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model), ["2024-01-03", "2024-01-04", "2024-01-05"], "dates, not 17:00:00 of the day before");
    assert_eq!(categories(&model).series[0].values, vec![Some(3.0), Some(4.0), Some(5.0)]);
    // The same instants in a -05 session read as written too.
    let day = text(&[Some("2024-01-04 12:00:00-05"), Some("2024-01-03 12:00:00-05")]);
    let source = typed(vec![("d", day), ("v", ints(&[Some(2), Some(1)]))], &[Some("timestamptz"), None]);
    let model = run(&source, &spec(ChartKind::Line, 0, &[1], Aggregation::None, None)).unwrap();
    assert_eq!(labels(&model), ["2024-01-03 12:00:00", "2024-01-04 12:00:00"]);
}

#[test]
fn work_stops_within_one_huge_batch() {
    // One batch of text that is a number: parsed and read one slice at a time, so a cancel part-way stops it.
    let rows = CHUNK_ROWS * 40 + 17;
    let k: ArrayRef = Arc::new(StringArray::from_iter((0..rows).map(|i| Some(if i % 2 == 0 { "a" } else { "b" }))));
    let v: ArrayRef = Arc::new(StringArray::from_iter((0..rows).map(|i| Some(i.to_string()))));
    let source = typed(vec![("k", k), ("v", v)], &[None, Some("numeric")]);
    let cancel = AtomicBool::new(false);
    let mut calls = 0usize;
    let error = prepare_with(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None), 100, &cancel, &mut |chunk| {
        calls += 1;
        if chunk == 3 {
            cancel.store(true, Ordering::Relaxed);
        }
    })
    .unwrap_err();
    assert_eq!(error, ChartIssue::Cancelled);
    assert_eq!(calls, 4, "chunks 0..=3 were reached; the other 37 were never read");
    // Uncancelled, every row is read exactly once, chunk by chunk.
    let mut calls = 0usize;
    let model = prepare_with(&source, &spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None), 100, &AtomicBool::new(false), &mut |_| calls += 1).unwrap();
    assert_eq!(calls, 41);
    let total: f64 = categories(&model).series[0].values.iter().flatten().sum();
    assert_eq!(total, (0..rows).map(|i| i as f64).sum::<f64>(), "the slices lose nothing");
}

/// `columns` columns over `rows` rows: the category and the value, then the others all sharing one array (no memory spent on them).
fn wide_source(columns: usize, rows: usize) -> ChartSource {
    let region: ArrayRef = Arc::new(StringArray::from_iter_values((0..rows).map(|row| if row % 2 == 0 { "even" } else { "odd" })));
    let amount: ArrayRef = Arc::new(Int64Array::from_iter_values((0..rows as i64).map(|row| row % 7)));
    let filler: ArrayRef = Arc::new(Int64Array::from_iter_values(0..rows as i64));
    let names: Vec<String> = (0..columns).map(|index| format!("c{index}")).collect();
    let mut list: Vec<(&str, ArrayRef)> = vec![("region", region), ("amount", amount)];
    list.extend(names.iter().skip(2).map(|name| (name.as_str(), filler.clone())));
    ChartSource::new(&[batch(list)])
}

#[test]
fn only_the_columns_a_chart_reads_are_sliced_so_a_wide_result_costs_what_a_narrow_one_does() {
    let rows = 4 * CHUNK_ROWS + 5;
    let wide = wide_source(300, rows);
    // The category, the series and the value columns, once each, in column order.
    assert_eq!(used_columns(1, Some(0), &[1, 0]), [0, 1]);
    let used = used_columns(0, None, &[1]);
    let chunks = chunked(&wide.batches, &used);
    assert_eq!(chunks.len(), 5);
    assert!(chunks.iter().all(|chunk| chunk.arrays.len() == 2), "two arrays of the 300 columns per chunk");
    assert_eq!(chunks.iter().map(|chunk| chunk.rows).sum::<usize>(), rows);
    assert_eq!(chunks[4].rows, 5);
    assert!(chunks.iter().all(|chunk| chunk.arrays.iter().all(|array| array.len() == chunk.rows)));
    // The numbers are the same as a result of two columns gives.
    let narrow = wide_source(2, rows);
    let spec = spec(ChartKind::Bar, 0, &[1], Aggregation::Sum, None);
    assert_eq!(run(&wide, &spec).unwrap(), run(&narrow, &spec).unwrap());

    // Allocations: a wide result allocates as little as a narrow one (before, 298 more slices per chunk). Counted on this
    // thread only, where `run` does all its work, so the tests beside it do not count.
    let least = |source: &ChartSource| {
        (0..8)
            .map(|_| {
                let before = crate::grid_bench::thread_allocation_count();
                run(source, &spec).unwrap();
                crate::grid_bench::thread_allocation_count() - before
            })
            .min()
            .unwrap()
    };
    let (narrow_allocations, wide_allocations) = (least(&narrow), least(&wide));
    assert!(wide_allocations <= narrow_allocations + 64, "{wide_allocations} allocations for 300 columns, {narrow_allocations} for 2");
}

#[test]
fn the_repeat_probe_slices_only_the_column_it_reads_so_a_wide_result_costs_what_a_narrow_one_does() {
    let rows = 4 * CHUNK_ROWS + 5;
    let (wide, narrow) = (wide_source(300, rows), wide_source(2, rows));
    // The answer is the same whatever else the result holds.
    for source in [&wide, &narrow] {
        assert!(source.repeats_within(0, rows), "two regions over many rows repeat");
        assert!(source.repeats_within(1, rows) && !source.repeats_within(1, 7), "column 1 is 0..=6 over and over: no repeat in seven rows");
        assert!(!source.repeats_within(0, 2), "only the sampled rows count");
    }
    // Allocations of the probe on this thread only: before, a batch's every column was sliced (300 more for 300 columns).
    let least = |source: &ChartSource, sample: usize| {
        (0..8)
            .map(|_| {
                let before = crate::grid_bench::thread_allocation_count();
                std::hint::black_box(source.repeats_within(1, sample));
                crate::grid_bench::thread_allocation_count() - before
            })
            .min()
            .unwrap()
    };
    // Column 1 holds 0..=6 over and over: the first repeat is found after eight rows.
    let (narrow_allocations, wide_allocations) = (least(&narrow, rows), least(&wide, rows));
    assert!(wide_allocations <= narrow_allocations + 8, "{wide_allocations} allocations for 300 columns, {narrow_allocations} for 2");
}
