//! Charts from a result: the prepare step. Pure (no windows, no GPUI entities), so it runs on the
//! background executor and is tested without a UI.
//!
//! Input: the Arrow batches the result already holds (`Arc` clones, never a copy of the rows) and a
//! [`ChartSpec`] (type, category / X column, value columns, aggregation, "series by" column).
//! Output: a bounded [`ChartModel`] that both the on-screen charts and the SVG / PNG export draw:
//!
//! * bars: at most [`MAX_BARS`] categories in the order the data comes, with a "showing 50 of N" note;
//! * pie: the top [`MAX_PIE_SLICES`] slices and one "Other";
//! * line and area: at most `max_points` points (min / max per bucket when there are more), sorted
//!   by X when X is a number or a date;
//! * scatter: at most [`MAX_SCATTER_POINTS`] points per series (every k-th), with a note.
//!
//! Typing: value columns must be numeric (integers, floats, decimals). Text that happens to parse is
//! refused ([`ChartIssue::NotNumeric`]), never converted. The category can be any column. NULLs are
//! skipped and counted in a note. One pass over the rows, no per-row allocation for text keys.

use arrow_array::{
    Array, ArrayRef, Float64Array, LargeStringArray, RecordBatch, StringArray, StringViewArray,
    TimestampMicrosecondArray,
};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, TimeUnit};
use gpui::SharedString;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Most bars one chart draws.
pub const MAX_BARS: usize = 50;
/// Slices of a pie before the rest is folded into "Other".
pub const MAX_PIE_SLICES: usize = 11;
/// Most series (value columns x "series by" values) one chart draws.
pub const MAX_SERIES: usize = 12;
/// Most scatter points per series.
pub const MAX_SCATTER_POINTS: usize = 5_000;
/// Most (X value, series) cells a line / area chart holds before the chart asks for an aggregation.
pub const MAX_CELLS: usize = 4_000_000;
/// Rows read between two looks at the cancel flag: the result is processed in zero-copy slices of
/// this many rows, so even one huge imported batch is stopped within one slice (and a column of
/// text is parsed or cast one slice at a time, never whole).
const CHUNK_ROWS: usize = 8_192;
/// Longest label kept (a text cell can be megabytes; the axis never shows more than a few dozen).
const MAX_LABEL_CHARS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChartKind {
    Bar,
    Line,
    Area,
    Scatter,
    Pie,
}

impl ChartKind {
    pub const ALL: [Self; 5] = [Self::Bar, Self::Line, Self::Area, Self::Scatter, Self::Pie];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bar => "Bar",
            Self::Line => "Line",
            Self::Area => "Area",
            Self::Scatter => "Scatter",
            Self::Pie => "Pie",
        }
    }

    /// Bar, line and area can be split into one series per value of the "series by" column.
    pub fn groups_series(self) -> bool {
        matches!(self, Self::Bar | Self::Line | Self::Area)
    }

    /// Line, area and scatter put numbers and dates in order along X.
    fn sorts_x(self) -> bool {
        matches!(self, Self::Line | Self::Area | Self::Scatter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Aggregation {
    None,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl Aggregation {
    pub const ALL: [Self; 6] = [Self::None, Self::Count, Self::Sum, Self::Avg, Self::Min, Self::Max];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Count => "Count",
            Self::Sum => "Sum",
            Self::Avg => "Average",
            Self::Min => "Min",
            Self::Max => "Max",
        }
    }

    fn sql_name(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Avg => "AVG",
            Self::Min => "MIN",
            Self::Max => "MAX",
        }
    }
}

/// How a column takes part in a chart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnClass {
    /// Integers, floats, decimals: usable as values and as a numeric X.
    Numeric,
    /// Dates and timestamps: ordered chronologically along X.
    Temporal,
    Text,
    /// Booleans, binary, times of day, dictionaries...: a category by its display text.
    Other,
}

impl ColumnClass {
    fn of(data_type: &DataType) -> Self {
        match data_type {
            DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float16
            | DataType::Float32
            | DataType::Float64
            | DataType::Decimal128(..)
            | DataType::Decimal256(..) => Self::Numeric,
            DataType::Date32 | DataType::Date64 | DataType::Timestamp(..) => Self::Temporal,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Self::Text,
            _ => Self::Other,
        }
    }
}

/// What a column of text stands for, going by the database's own type name (`numeric`,
/// `decimal(10,2)`, `timestamp with time zone`, `Nullable(Date)`...): the drivers deliver most
/// types other than integers and floats as text, and the type name is the only thing that says
/// the text is a number or a date. `None` for every other type, which stays text.
pub fn class_of_type_name(type_name: &str) -> Option<ColumnClass> {
    let mut name = type_name.trim().to_ascii_lowercase();
    // ClickHouse wrappers.
    while let Some(inner) = ["nullable(", "lowcardinality("]
        .iter()
        .find_map(|wrapper| name.strip_prefix(wrapper))
        .and_then(|rest| rest.trim_end().strip_suffix(')'))
    {
        name = inner.trim().to_owned();
    }
    if name.ends_with(']') || name.starts_with("array(") {
        return None;
    }
    // Parameters: `numeric(10,2)`, `timestamp(3) with time zone`, `DateTime64(3, 'UTC')`.
    let mut bare = String::with_capacity(name.len());
    let mut depth = 0u32;
    for c in name.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => bare.push(c),
            _ => {}
        }
    }
    let words: Vec<&str> = bare.split_whitespace().filter(|w| !matches!(*w, "unsigned" | "signed" | "zerofill")).collect();
    let head = words.join(" ");
    match head.as_str() {
        "smallint" | "integer" | "int" | "bigint" | "tinyint" | "mediumint" | "int1" | "int2" | "int3" | "int4" | "int8"
        | "int16" | "int32" | "int64" | "int128" | "int256" | "uint8" | "uint16" | "uint32" | "uint64" | "uint128"
        | "uint256" | "real" | "float" | "float4" | "float8" | "float32" | "float64" | "double" | "double precision"
        | "numeric" | "decimal" | "dec" | "fixed" | "number" | "smallserial" | "serial" | "bigserial" | "serial2"
        | "serial4" | "serial8" | "decimal32" | "decimal64" | "decimal128" | "decimal256" => Some(ColumnClass::Numeric),
        "date" | "date32" | "datetime" | "datetime2" | "datetime64" | "smalldatetime" | "timestamp" | "timestamptz"
        | "timestamp with time zone" | "timestamp without time zone" | "timestamp with local time zone" => {
            Some(ColumnClass::Temporal)
        }
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartColumn {
    pub name: String,
    pub class: ColumnClass,
    /// The values are text (Arrow `Utf8`) that stands for numbers or dates, by the column's type name:
    /// they are parsed strictly when read.
    pub from_text: bool,
}

/// What the user picked. Columns are indexes into the result's (source) columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub x: usize,
    pub values: Vec<usize>,
    pub aggregation: Aggregation,
    pub series_by: Option<usize>,
}

impl ChartSpec {
    /// The first text or date column is the category, the first numeric column the value;
    /// `repeats` (the category repeats within the sampled rows) picks Sum over None.
    pub fn defaults(columns: &[ChartColumn], repeats: impl FnOnce(usize) -> bool) -> Self {
        let position = |wanted: &dyn Fn(&ChartColumn) -> bool| columns.iter().position(wanted);
        let x = position(&|c| matches!(c.class, ColumnClass::Text | ColumnClass::Temporal))
            .or_else(|| position(&|c| c.class == ColumnClass::Other))
            .unwrap_or(0);
        let value = columns
            .iter()
            .enumerate()
            .find(|(i, c)| c.class == ColumnClass::Numeric && *i != x)
            .or_else(|| columns.iter().enumerate().find(|(_, c)| c.class == ColumnClass::Numeric))
            .map(|(i, _)| i);
        let aggregation = if value.is_some() && repeats(x) { Aggregation::Sum } else { Aggregation::None };
        Self { kind: ChartKind::Bar, x, values: value.into_iter().collect(), aggregation, series_by: None }
    }
}

/// Why no chart can be drawn from the result and spec, as the pane says it.
#[derive(Clone, Debug, PartialEq)]
pub enum ChartIssue {
    /// The result has no rows.
    NoRows,
    /// Nothing is left to draw once NULLs are skipped.
    NothingToDraw,
    /// The result has no numeric column (and Count is not chosen).
    NoNumericColumn,
    /// No value column is chosen.
    NoValueColumn,
    /// A value column holds text, dates or booleans. Never converted.
    NotNumeric { column: String },
    /// Scatter needs numbers or dates along X.
    ScatterNeedsNumericX { column: String },
    /// A pie slice cannot be negative.
    NegativePie { column: String },
    /// More series than a chart can tell apart.
    TooManySeries { at_least: usize, max: usize },
    /// More (X, series) cells than a chart holds.
    TooManyPoints { cells: usize },
    /// The spec names a column the result does not have.
    UnknownColumn,
    /// A column could not be read (an unsupported Arrow conversion).
    Unreadable { column: String, reason: String },
    /// A newer request replaced this one.
    Cancelled,
}

impl fmt::Display for ChartIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRows => f.write_str("The result has no rows to chart."),
            Self::NothingToDraw => f.write_str("Every row has a NULL in the chosen columns, so there is nothing to draw."),
            Self::NoNumericColumn => f.write_str("This result has no numeric column to chart. Choose Count to count rows per category."),
            Self::NoValueColumn => f.write_str("Choose a numeric value column, or Count to count rows per category."),
            Self::NotNumeric { column } => write!(f, "Column \"{column}\" is not numeric. Values must be integers, floats or decimals; text is never converted."),
            Self::ScatterNeedsNumericX { column } => write!(f, "A scatter chart needs numbers or dates along X; \"{column}\" is neither."),
            Self::NegativePie { column } => write!(f, "A pie cannot show negative values (\"{column}\" has some)."),
            Self::TooManySeries { at_least, max } => write!(f, "Too many series: at least {at_least} (a chart shows at most {max}). Pick a column with fewer distinct values, or fewer value columns."),
            Self::TooManyPoints { cells } => write!(f, "Too many points to chart ({cells}). Choose an aggregation to group them."),
            Self::UnknownColumn => f.write_str("The chart refers to a column this result does not have."),
            Self::Unreadable { column, reason } => write!(f, "Column \"{column}\" cannot be charted: {reason}"),
            Self::Cancelled => f.write_str("Chart preparation was replaced by a newer request."),
        }
    }
}

impl std::error::Error for ChartIssue {}

/// A line under the chart saying what was left out.
#[derive(Clone, Debug, PartialEq)]
pub enum ChartNote {
    Showing { shown: usize, total: usize, what: &'static str },
    NullCategory(usize),
    /// A NULL in the "series by" column.
    NullSeries(usize),
    NullValue(usize),
    /// NaN and infinity: they have no place on an axis.
    NonFinite(usize),
    /// Text in a number or date column that is not a valid number or date.
    Unparsed(usize),
    Overwritten(usize),
    /// Only the rows loaded so far are charted; the result has more.
    LoadedOnly { rows: usize },
    Ignored(&'static str),
}

impl fmt::Display for ChartNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Showing { shown, total, what } => write!(f, "showing {shown} of {total} {what}"),
            Self::NullCategory(n) => write!(f, "{n} row{} with a NULL category skipped", plural(*n)),
            Self::NullSeries(n) => write!(f, "{n} row{} with a NULL series skipped", plural(*n)),
            Self::NullValue(n) => write!(f, "{n} NULL value{} skipped", plural(*n)),
            Self::NonFinite(n) => write!(f, "{n} non-finite value{} (NaN or infinity) skipped", plural(*n)),
            Self::Unparsed(n) => write!(f, "{n} cell{} that {} not a valid number or date skipped", plural(*n), if *n == 1 { "is" } else { "are" }),
            Self::LoadedOnly { rows } => write!(f, "charted from the {rows} rows loaded; the result has more"),
            Self::Overwritten(n) => write!(f, "{n} repeated X / series pair{} show the last row (choose an aggregation to combine them)", plural(*n)),
            Self::Ignored(what) => write!(f, "{what}"),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub name: SharedString,
    /// One value per label; `None` where the series has no row for that label.
    pub values: Vec<Option<f64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Categories {
    pub labels: Vec<SharedString>,
    pub series: Vec<Series>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PointSeries {
    pub name: SharedString,
    pub points: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Points {
    /// X is a date: ticks are drawn as dates.
    pub x_temporal: bool,
    pub series: Vec<PointSeries>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Slice {
    pub label: SharedString,
    pub value: f64,
    pub other: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChartData {
    Categories(Categories),
    Points(Points),
    Slices(Vec<Slice>),
}

/// What gets drawn: the aggregated, bounded series plus the names the axes and the export need.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartModel {
    pub kind: ChartKind,
    pub aggregation: Aggregation,
    pub title: String,
    pub x_title: String,
    pub y_title: String,
    pub notes: Vec<ChartNote>,
    pub data: ChartData,
}

impl ChartModel {
    pub fn series_names(&self) -> Vec<SharedString> {
        match &self.data {
            ChartData::Categories(c) => c.series.iter().map(|s| s.name.clone()).collect(),
            ChartData::Points(p) => p.series.iter().map(|s| s.name.clone()).collect(),
            ChartData::Slices(s) => s.iter().map(|s| s.label.clone()).collect(),
        }
    }

    /// Points the chart draws.
    #[cfg(test)]
    pub fn drawn_points(&self) -> usize {
        match &self.data {
            ChartData::Categories(c) => c.labels.len() * c.series.len(),
            ChartData::Points(p) => p.series.iter().map(|s| s.points.len()).sum(),
            ChartData::Slices(s) => s.len(),
        }
    }
}

/// The result's rows as the chart reads them: the batches (shared, not copied) and the column list.
#[derive(Clone)]
pub struct ChartSource {
    batches: Vec<Arc<RecordBatch>>,
    columns: Vec<ChartColumn>,
    rows: usize,
    /// The result has more rows than these (another page exists): the chart says so.
    partial: bool,
}

impl ChartSource {
    /// A source with no type names: Arrow's own types decide.
    #[cfg(test)]
    pub fn new(batches: &[Arc<RecordBatch>]) -> Self {
        Self::with_types(batches, &[])
    }

    /// `type_names[i]` is the database's type name of column `i`, when known. It decides what a text
    /// column is (see [`class_of_type_name`]); Arrow's own numeric and date types need no name.
    pub fn with_types(batches: &[Arc<RecordBatch>], type_names: &[Option<String>]) -> Self {
        let columns = batches
            .first()
            .map(|batch| {
                batch
                    .schema_ref()
                    .fields()
                    .iter()
                    .enumerate()
                    .map(|(index, field)| {
                        let class = ColumnClass::of(field.data_type());
                        let named = (class == ColumnClass::Text)
                            .then(|| type_names.get(index).and_then(|name| name.as_deref()).and_then(class_of_type_name))
                            .flatten();
                        ChartColumn {
                            name: field.name().clone(),
                            class: named.unwrap_or(class),
                            from_text: named.is_some(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { batches: batches.to_vec(), columns, rows: batches.iter().map(|batch| batch.num_rows()).sum(), partial: false }
    }

    /// Marks the rows as one page of a bigger result.
    pub fn partial(mut self, partial: bool) -> Self {
        self.partial = partial;
        self
    }

    pub fn columns(&self) -> &[ChartColumn] {
        &self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// True when the category column repeats within the first `sample` rows (the default
    /// aggregation is Sum then). Bounded: it runs on the UI thread when a chart is first shown.
    pub fn repeats_within(&self, column: usize, sample: usize) -> bool {
        if self.columns.get(column).is_none() {
            return false;
        }
        // Zero-copy slices of this column's first `sample` rows: only those are decoded, and no other column is touched.
        let mut left = sample;
        let mut slices = Vec::new();
        for batch in &self.batches {
            if left == 0 {
                break;
            }
            let take = batch.num_rows().min(left);
            slices.push(batch.column(column).slice(0, take));
            left -= take;
        }
        let mut seen: std::collections::HashSet<XKey<'_>> = std::collections::HashSet::new();
        for array in &slices {
            let Ok(cells) = Cells::open(array, &self.columns[column], false) else { return false };
            for row in 0..array.len() {
                if cells.is_null(row) {
                    continue;
                }
                if !seen.insert(cells.key(row)) {
                    return true;
                }
            }
        }
        false
    }
}

// ---- cells ---------------------------------------------------------------------------------

enum Strings<'a> {
    Utf8(&'a StringArray),
    Large(&'a LargeStringArray),
    View(&'a StringViewArray),
}

impl<'a> Strings<'a> {
    fn open(array: &'a ArrayRef) -> Result<Self, arrow_schema::ArrowError> {
        if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
            Ok(Self::Utf8(a))
        } else if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
            Ok(Self::Large(a))
        } else if let Some(a) = array.as_any().downcast_ref::<StringViewArray>() {
            Ok(Self::View(a))
        } else {
            Err(arrow_schema::ArrowError::CastError("unexpected text array".into()))
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Utf8(a) => a.len(),
            Self::Large(a) => a.len(),
            Self::View(a) => a.len(),
        }
    }

    fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Utf8(a) => a.is_null(row),
            Self::Large(a) => a.is_null(row),
            Self::View(a) => a.is_null(row),
        }
    }

    fn value(&self, row: usize) -> &'a str {
        match self {
            Self::Utf8(a) => a.value(row),
            Self::Large(a) => a.value(row),
            Self::View(a) => a.value(row),
        }
    }
}

/// A number written as text, strictly: optional sign, digits, a fraction, an exponent. Anything
/// else (`1,234`, `$5`, `abc`) is `None`; `NaN` and `inf` parse and are skipped later as non-finite.
fn parse_number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok()
}

/// A date or timestamp written as text (`2024-01-05`, `2024-01-05 10:20:30.123456`, with `T`, with a
/// `Z` / `+05` / `+05:30` offset) as microseconds of its *wall clock* since the epoch: the digits as
/// they are written, whatever the offset says. PostgreSQL prints a `timestamptz` in the session's time
/// zone and the grid shows that text, so the chart labels the same wall clock (a `date_trunc('day', ..)`
/// in a +07 session is `2024-01-05 00:00:00+07`, and the axis says `2024-01-05`). Within one session
/// offset the order of wall clocks is the order of instants (except the repeated hour when daylight
/// saving ends). `None` for anything else, MySQL's `0000-00-00` and PostgreSQL's `infinity` included.
fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() < 10 {
        return None;
    }
    let mut owned;
    let mut text = text;
    if text.contains(['T', 'Z']) {
        owned = text.replacen('T', " ", 1);
        if let Some(stripped) = owned.strip_suffix('Z') {
            owned = format!("{stripped}+00:00");
        }
        text = &owned;
    }
    if text.len() == 10 {
        let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros());
    }
    if let Ok(at) = chrono::DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%#z") {
        return Some(at.naive_local().and_utc().timestamp_micros());
    }
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").ok().map(|at| at.and_utc().timestamp_micros())
}

/// One column of one batch, opened once so the row loop never downcasts. A number or date that came
/// as text keeps its text, to tell a cell that does not parse from a NULL.
enum Cells<'a> {
    Text(Strings<'a>),
    Number(Float64Array, Option<Strings<'a>>),
    Time(TimestampMicrosecondArray, Option<Strings<'a>>),
    Display { formatter: ArrayFormatter<'a>, array: &'a ArrayRef },
}

/// Hash / equality key of a category. Text borrows from the batch: no allocation per row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum XKey<'a> {
    Text(&'a str),
    /// `f64::to_bits` of a number (`-0.0` folded into `0.0`).
    Number(u64),
    /// Microseconds since the epoch.
    Time(i64),
    Owned(String),
}

fn number_key(value: f64) -> XKey<'static> {
    XKey::Number(if value == 0.0 { 0.0f64 } else { value }.to_bits())
}

impl<'a> Cells<'a> {
    fn open(array: &'a ArrayRef, column: &ChartColumn, numeric_only: bool) -> Result<Self, arrow_schema::ArrowError> {
        if column.from_text {
            let strings = Strings::open(array)?;
            return Ok(match column.class {
                ColumnClass::Numeric => {
                    let numbers = Float64Array::from_iter((0..strings.len()).map(|row| {
                        if strings.is_null(row) {
                            None
                        } else {
                            parse_number(strings.value(row))
                        }
                    }));
                    Self::Number(numbers, Some(strings))
                }
                _ if numeric_only => return Err(arrow_schema::ArrowError::CastError("not numeric".into())),
                _ => {
                    let times = TimestampMicrosecondArray::from_iter((0..strings.len()).map(|row| {
                        if strings.is_null(row) {
                            None
                        } else {
                            parse_time(strings.value(row))
                        }
                    }));
                    Self::Time(times, Some(strings))
                }
            });
        }
        match column.class {
            ColumnClass::Numeric => {
                let cast = arrow_cast::cast::cast(array, &DataType::Float64)?;
                let numbers = cast
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or_else(|| arrow_schema::ArrowError::CastError("not a Float64 array".into()))?
                    .clone();
                Ok(Self::Number(numbers, None))
            }
            _ if numeric_only => Err(arrow_schema::ArrowError::CastError("not numeric".into())),
            ColumnClass::Temporal => {
                let cast = arrow_cast::cast::cast(array, &DataType::Timestamp(TimeUnit::Microsecond, None))?;
                let times = cast
                    .as_any()
                    .downcast_ref::<TimestampMicrosecondArray>()
                    .ok_or_else(|| arrow_schema::ArrowError::CastError("not a timestamp array".into()))?
                    .clone();
                Ok(Self::Time(times, None))
            }
            ColumnClass::Text => Ok(Self::Text(Strings::open(array)?)),
            ColumnClass::Other => Ok(Self::Display {
                formatter: ArrayFormatter::try_new(array.as_ref(), &FormatOptions::default())?,
                array,
            }),
        }
    }

    /// NULL, or a cell the chart cannot place: text that is not a number or date, NaN, infinity.
    fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Text(strings) => strings.is_null(row),
            Self::Number(a, _) => a.is_null(row) || !a.value(row).is_finite(),
            Self::Time(a, _) => a.is_null(row),
            Self::Display { array, .. } => array.is_null(row),
        }
    }

    /// A cell of a number / date column that has text which is not a valid number / date.
    fn unparsed(&self, row: usize) -> bool {
        match self {
            Self::Number(a, Some(text)) => a.is_null(row) && !text.is_null(row),
            Self::Time(a, Some(text)) => a.is_null(row) && !text.is_null(row),
            _ => false,
        }
    }

    /// A number that is NaN or infinite.
    fn non_finite(&self, row: usize) -> bool {
        matches!(self, Self::Number(a, _) if !a.is_null(row) && !a.value(row).is_finite())
    }

    /// The category key of a non-NULL cell.
    fn key(&self, row: usize) -> XKey<'a> {
        match self {
            Self::Text(strings) => XKey::Text(strings.value(row)),
            Self::Number(a, _) => number_key(a.value(row)),
            Self::Time(a, _) => XKey::Time(a.value(row)),
            Self::Display { formatter, .. } => XKey::Owned(formatter.value(row).to_string()),
        }
    }

    /// A non-NULL numeric cell.
    fn number(&self, row: usize) -> f64 {
        match self {
            Self::Number(a, _) => a.value(row),
            _ => f64::NAN,
        }
    }
}

impl XKey<'_> {
    fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(bits) => Some(f64::from_bits(*bits)),
            Self::Time(micros) => Some(*micros as f64),
            _ => None,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Text(text) => truncate_label(text),
            Self::Owned(text) => truncate_label(text),
            Self::Number(bits) => format_number(f64::from_bits(*bits)),
            Self::Time(micros) => format_time(*micros),
        }
    }

    /// Order along X for numbers and dates; the rest keep arrival order.
    fn order(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Time(a), Self::Time(b)) => a.cmp(b),
            (a, b) => match (a.as_number(), b.as_number()) {
                (Some(a), Some(b)) => a.total_cmp(&b),
                _ => std::cmp::Ordering::Equal,
            },
        }
    }
}

fn truncate_label(text: &str) -> String {
    match text.char_indices().nth(MAX_LABEL_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// A number as a label: integers bare, the rest to at most six decimals.
pub fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{value:.0}");
    }
    let text = format!("{value:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A date or timestamp: the date alone at midnight, else to the second.
pub fn format_time(micros: i64) -> String {
    let Some(at) = chrono::DateTime::from_timestamp_micros(micros) else {
        return micros.to_string();
    };
    if micros.rem_euclid(86_400_000_000) == 0 {
        at.format("%Y-%m-%d").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// A tick on a date axis (`micros` since the epoch, ticks `step` apart): dates for day-sized steps
/// and up, minutes below that, seconds below a minute.
pub fn format_time_axis(micros: f64, step: f64) -> String {
    let Some(at) = chrono::DateTime::from_timestamp_micros(micros as i64) else {
        return format_number(micros);
    };
    let pattern = if step >= 86_400_000_000.0 {
        "%Y-%m-%d"
    } else if step >= 60_000_000.0 {
        "%Y-%m-%d %H:%M"
    } else {
        "%H:%M:%S"
    };
    at.format(pattern).to_string()
}

/// Round tick values covering `lo..=hi`, about `target` of them.
pub fn nice_ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    if !(lo.is_finite() && hi.is_finite()) || target < 2 {
        return vec![];
    }
    if lo >= hi {
        return vec![lo];
    }
    fn nice(x: f64, round: bool) -> f64 {
        let exponent = x.log10().floor();
        let fraction = x / 10f64.powf(exponent);
        let nice = if round {
            match fraction {
                f if f < 1.5 => 1.0,
                f if f < 3.0 => 2.0,
                f if f < 7.0 => 5.0,
                _ => 10.0,
            }
        } else {
            match fraction {
                f if f <= 1.0 => 1.0,
                f if f <= 2.0 => 2.0,
                f if f <= 5.0 => 5.0,
                _ => 10.0,
            }
        };
        nice * 10f64.powf(exponent)
    }
    let step = nice(nice(hi - lo, false) / (target - 1) as f64, true);
    let start = (lo / step).floor() * step;
    let end = (hi / step).ceil() * step;
    let count = ((end - start) / step).round() as usize + 1;
    (0..count.min(1_000)).map(|i| start + step * i as f64).collect()
}

/// A tick or value on an axis: `step` decides the decimals, big values are abbreviated.
pub fn format_tick(value: f64, step: f64) -> String {
    let abs = value.abs();
    if abs >= 1e9 {
        return format!("{}B", format_number((value / 1e8).round() / 10.0));
    }
    if abs >= 1e6 {
        return format!("{}M", format_number((value / 1e5).round() / 10.0));
    }
    if abs >= 1e4 {
        return format!("{}K", format_number((value / 1e2).round() / 10.0));
    }
    let decimals = if step > 0.0 && step < 1.0 { (-step.log10().floor()) as usize } else { 0 };
    let text = format!("{value:.*}", decimals.min(6));
    if text == "-0" {
        "0".to_owned()
    } else {
        text
    }
}

/// Rows and cells left out of the chart, by why.
#[derive(Default)]
struct Skips {
    category: usize,
    series: usize,
    value: usize,
    non_finite: usize,
    unparsed: usize,
}

impl Skips {
    /// Counts why a missing cell is missing: NULL (in `role`), a cell that does not parse, or NaN / infinity.
    fn why(&mut self, cells: &Cells<'_>, row: usize, role: fn(&mut Self) -> &mut usize) {
        let slot = if cells.unparsed(row) {
            &mut self.unparsed
        } else if cells.non_finite(row) {
            &mut self.non_finite
        } else {
            role(self)
        };
        *slot += 1;
    }

    fn category(&mut self, cells: &Cells<'_>, row: usize) {
        self.why(cells, row, |s| &mut s.category);
    }

    fn series(&mut self, cells: &Cells<'_>, row: usize) {
        self.why(cells, row, |s| &mut s.series);
    }

    fn value(&mut self, cells: &Cells<'_>, row: usize) {
        self.why(cells, row, |s| &mut s.value);
    }

    fn notes(&self, notes: &mut Vec<ChartNote>) {
        for (count, note) in [
            (self.category, ChartNote::NullCategory as fn(usize) -> ChartNote),
            (self.series, ChartNote::NullSeries),
            (self.value, ChartNote::NullValue),
            (self.non_finite, ChartNote::NonFinite),
            (self.unparsed, ChartNote::Unparsed),
        ] {
            if count > 0 {
                notes.push(note(count));
            }
        }
    }
}

// ---- accumulators --------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Acc {
    value: f64,
    count: u32,
}

impl Acc {
    fn add(&mut self, aggregation: Aggregation, v: f64) {
        match aggregation {
            Aggregation::Sum | Aggregation::Avg => self.value += v,
            Aggregation::Min if self.count == 0 || v < self.value => self.value = v,
            Aggregation::Max if self.count == 0 || v > self.value => self.value = v,
            Aggregation::None => self.value = v,
            _ => {}
        }
        self.count = self.count.saturating_add(1);
    }

    fn result(&self, aggregation: Aggregation) -> Option<f64> {
        (self.count > 0).then(|| match aggregation {
            Aggregation::Count => f64::from(self.count),
            Aggregation::Avg => self.value / f64::from(self.count),
            _ => self.value,
        })
    }
}

// ---- the prepare step ----------------------------------------------------------------------

struct Resolved {
    aggregation: Aggregation,
    /// Value columns in play (empty for Count).
    values: Vec<usize>,
    /// Values per group: the value columns, or 1 for Count.
    per_group: usize,
    series_by: Option<usize>,
}

fn resolve(source: &ChartSource, spec: &ChartSpec) -> Result<(Resolved, Vec<ChartNote>), ChartIssue> {
    let columns = source.columns();
    let column = |i: usize| columns.get(i).ok_or(ChartIssue::UnknownColumn);
    let x = column(spec.x)?;
    let mut notes = Vec::new();
    let aggregation = spec.aggregation;
    let mut values = if aggregation == Aggregation::Count { Vec::new() } else { spec.values.clone() };
    if aggregation != Aggregation::Count {
        if values.is_empty() {
            return Err(if columns.iter().any(|c| c.class == ColumnClass::Numeric) {
                ChartIssue::NoValueColumn
            } else {
                ChartIssue::NoNumericColumn
            });
        }
        for &value in &values {
            let info = column(value)?;
            if info.class != ColumnClass::Numeric {
                return Err(ChartIssue::NotNumeric { column: info.name.clone() });
            }
        }
    }
    if spec.kind == ChartKind::Scatter && !matches!(x.class, ColumnClass::Numeric | ColumnClass::Temporal) {
        return Err(ChartIssue::ScatterNeedsNumericX { column: x.name.clone() });
    }
    if values.len() > MAX_SERIES {
        return Err(ChartIssue::TooManySeries { at_least: values.len(), max: MAX_SERIES });
    }
    if spec.kind == ChartKind::Pie && values.len() > 1 {
        values.truncate(1);
        notes.push(ChartNote::Ignored("a pie shows the first value column only"));
    }
    let series_by = match spec.series_by {
        Some(by) if spec.kind.groups_series() => {
            column(by)?;
            Some(by)
        }
        Some(_) => {
            notes.push(ChartNote::Ignored("this chart type is not split by a series column"));
            None
        }
        None => None,
    };
    let per_group = if aggregation == Aggregation::Count { 1 } else { values.len() };
    Ok((Resolved { aggregation, values, per_group, series_by }, notes))
}

/// The columns a chart reads, each once, in column order.
fn used_columns(x: usize, series_by: Option<usize>, values: &[usize]) -> Vec<usize> {
    let mut used: Vec<usize> = std::iter::once(x).chain(series_by).chain(values.iter().copied()).collect();
    used.sort_unstable();
    used.dedup();
    used
}

/// `CHUNK_ROWS` rows of the columns a chart reads (`used`, in that order), sliced without copying.
struct Chunk {
    rows: usize,
    arrays: Vec<ArrayRef>,
}

impl Chunk {
    /// The slice of the column `column` of the result (one of the `used` columns).
    fn column(&self, used: &[usize], column: usize) -> &ArrayRef {
        // `used` is sorted and holds every column asked for.
        &self.arrays[used.binary_search(&column).expect("a column the chart reads is one of the used columns")]
    }
}

fn chunked(batches: &[Arc<RecordBatch>], used: &[usize]) -> Vec<Chunk> {
    batches
        .iter()
        .flat_map(|batch| {
            (0..batch.num_rows()).step_by(CHUNK_ROWS).map(move |start| {
                let rows = CHUNK_ROWS.min(batch.num_rows() - start);
                Chunk { rows, arrays: used.iter().map(|&column| batch.column(column).slice(start, rows)).collect() }
            })
        })
        .collect()
}

/// What the row loop hands over: X keys with one column of values per series.
struct Collected<'a> {
    xs: Vec<XKey<'a>>,
    /// Series-major.
    values: Vec<Vec<Option<f64>>>,
    /// Distinct X values in the data (more than `xs.len()` when bars were cut).
    total_x: usize,
}

/// Prepares the chart: one pass over `source`, bounded output. `max_points` is about twice the plot
/// width in pixels; `cancel` is polled so a replaced request stops early.
pub fn prepare(
    source: &ChartSource,
    spec: &ChartSpec,
    max_points: usize,
    cancel: &AtomicBool,
) -> Result<ChartModel, ChartIssue> {
    prepare_with(source, spec, max_points, cancel, &mut |_| {})
}

/// [`prepare`], calling `at_chunk(index)` as each slice of the rows is about to be read (before the
/// cancel flag is looked at): the hook tests use to cancel part-way through one batch.
fn prepare_with(
    source: &ChartSource,
    spec: &ChartSpec,
    max_points: usize,
    cancel: &AtomicBool,
    at_chunk: &mut dyn FnMut(usize),
) -> Result<ChartModel, ChartIssue> {
    if source.rows() == 0 {
        return Err(ChartIssue::NoRows);
    }
    let (plan, mut notes) = resolve(source, spec)?;
    let columns = source.columns();
    let x_info = &columns[spec.x];
    let max_points = max_points.max(8);

    // Groups ("series by" values) in arrival order.
    let mut groups: Vec<XKey<'_>> = Vec::new();
    let mut group_index: HashMap<XKey<'_>, usize> = HashMap::new();
    let cap_series = MAX_SERIES / plan.per_group.max(1);

    let grouped = plan.aggregation != Aggregation::None || plan.series_by.is_some();
    let bar_limit = if spec.kind == ChartKind::Bar { MAX_BARS } else { usize::MAX };

    // Grouped: X -> slot, accumulators per series.
    let mut x_index: HashMap<XKey<'_>, usize> = HashMap::new();
    let mut xs: Vec<XKey<'_>> = Vec::new();
    let mut acc: Vec<Vec<Acc>> = Vec::new();
    // Raw: one row per point.
    let mut raw: Vec<Vec<Option<f64>>> = (0..plan.per_group).map(|_| Vec::new()).collect();
    let mut raw_total = 0usize;
    // Raw scatter: decimated points per series.
    let mut decimator: Vec<Decimator> = (0..plan.per_group).map(|_| Decimator::new(MAX_SCATTER_POINTS)).collect();
    // Raw pie: the best slices so far.
    let mut pie_best: Vec<(f64, XKey<'_>)> = Vec::new();
    let (mut pie_total, mut pie_count) = (0.0f64, 0usize);

    let mut skips = Skips::default();
    let mut overwritten = 0usize;
    let mut cells_so_far = 0usize;
    let mut scratch: Vec<Option<f64>> = vec![None; plan.per_group];

    // Zero-copy slices of the columns the chart reads (the category, the series, the values) in every batch, `CHUNK_ROWS` rows a
    // slice: the keys below borrow from them. A column the chart does not use is never sliced, so a wide result costs what a
    // narrow one does.
    let used = used_columns(spec.x, plan.series_by, &plan.values);
    let chunks = chunked(&source.batches, &used);
    for (index, chunk) in chunks.iter().enumerate() {
        at_chunk(index);
        if cancel.load(Ordering::Relaxed) {
            return Err(ChartIssue::Cancelled);
        }
        let open = |column: usize, numeric_only: bool| {
            Cells::open(chunk.column(&used, column), &columns[column], numeric_only)
                .map_err(|e| ChartIssue::Unreadable { column: columns[column].name.clone(), reason: e.to_string() })
        };
        let x_cells = open(spec.x, false)?;
        let by_cells = plan.series_by.map(|by| open(by, false)).transpose()?;
        let value_cells = plan.values.iter().map(|&v| open(v, true)).collect::<Result<Vec<_>, _>>()?;
        for row in 0..chunk.rows {
            if x_cells.is_null(row) {
                skips.category(&x_cells, row);
                continue;
            }
            let mut any_value = plan.aggregation == Aggregation::Count;
            for (slot, cells) in scratch.iter_mut().zip(&value_cells) {
                if cells.is_null(row) {
                    *slot = None;
                    skips.value(cells, row);
                } else {
                    *slot = Some(cells.number(row));
                    any_value = true;
                }
            }
            if plan.aggregation == Aggregation::Count {
                scratch[0] = Some(1.0);
            }
            if !any_value {
                continue;
            }
            let key = x_cells.key(row);
            if !grouped {
                match spec.kind {
                    ChartKind::Pie => {
                        let Some(v) = scratch[0] else { continue };
                        if v < 0.0 {
                            return Err(ChartIssue::NegativePie { column: columns[plan.values[0]].name.clone() });
                        }
                        pie_total += v;
                        pie_count += 1;
                        pie_best.push((v, key));
                        if pie_best.len() > 8 * MAX_PIE_SLICES {
                            pie_best.sort_by(|a, b| b.0.total_cmp(&a.0));
                            pie_best.truncate(MAX_PIE_SLICES);
                        }
                    }
                    ChartKind::Scatter => {
                        let Some(x) = key.as_number() else { continue };
                        for (series, v) in decimator.iter_mut().zip(&scratch) {
                            if let Some(y) = v {
                                series.push([x, *y]);
                            }
                        }
                    }
                    _ => {
                        raw_total += 1;
                        if raw_total <= bar_limit {
                            xs.push(key);
                            for (column, v) in raw.iter_mut().zip(&scratch) {
                                column.push(*v);
                            }
                            cells_so_far += plan.per_group;
                            if cells_so_far > MAX_CELLS {
                                return Err(ChartIssue::TooManyPoints { cells: cells_so_far });
                            }
                        }
                    }
                }
                continue;
            }

            // Grouped: find the group, then the X slot.
            let group = match &by_cells {
                None => 0,
                Some(cells) => {
                    if cells.is_null(row) {
                        skips.series(cells, row);
                        continue;
                    }
                    let by_key = cells.key(row);
                    match group_index.get(&by_key) {
                        Some(&g) => g,
                        None => {
                            if groups.len() >= cap_series {
                                return Err(ChartIssue::TooManySeries {
                                    at_least: (groups.len() + 1) * plan.per_group,
                                    max: MAX_SERIES,
                                });
                            }
                            groups.push(by_key.clone());
                            group_index.insert(by_key, groups.len() - 1);
                            // The slots the series gets for the X values seen so far count too.
                            cells_so_far += xs.len().min(bar_limit) * plan.per_group;
                            if cells_so_far > MAX_CELLS {
                                return Err(ChartIssue::TooManyPoints { cells: cells_so_far });
                            }
                            for _ in 0..plan.per_group {
                                acc.push(vec![Acc::default(); xs.len().min(bar_limit)]);
                            }
                            groups.len() - 1
                        }
                    }
                }
            };
            if by_cells.is_none() && acc.is_empty() {
                for _ in 0..plan.per_group {
                    acc.push(Vec::new());
                }
            }
            let slot = match x_index.get(&key) {
                Some(&s) => s,
                None => {
                    let s = xs.len();
                    xs.push(key.clone());
                    x_index.insert(key, s);
                    if s < bar_limit {
                        cells_so_far += acc.len().max(plan.per_group);
                        if cells_so_far > MAX_CELLS {
                            return Err(ChartIssue::TooManyPoints { cells: cells_so_far });
                        }
                        for column in acc.iter_mut() {
                            column.push(Acc::default());
                        }
                    }
                    s
                }
            };
            if slot >= bar_limit {
                continue;
            }
            for (v, value) in scratch.iter().enumerate() {
                let Some(value) = value else { continue };
                let cell = &mut acc[group * plan.per_group + v][slot];
                if plan.aggregation == Aggregation::None && cell.count > 0 {
                    overwritten += 1;
                }
                cell.add(plan.aggregation, *value);
            }
        }
    }

    skips.notes(&mut notes);
    if overwritten > 0 {
        notes.push(ChartNote::Overwritten(overwritten));
    }

    // ---- names -------------------------------------------------------------------------------
    let value_names: Vec<String> = if plan.aggregation == Aggregation::Count {
        vec!["count".to_owned()]
    } else {
        plan.values.iter().map(|&v| columns[v].name.clone()).collect()
    };
    let group_labels: Vec<String> = if plan.series_by.is_some() { groups.iter().map(XKey::label).collect() } else { vec![String::new()] };
    let many_values = value_names.len() > 1;
    let series_names: Vec<SharedString> = group_labels
        .iter()
        .flat_map(|group| {
            value_names.iter().map(move |value| {
                if group.is_empty() {
                    value.clone()
                } else if many_values {
                    format!("{group} · {value}")
                } else {
                    group.clone()
                }
                .into()
            })
        })
        .collect();

    let y_title = {
        let inner = if plan.aggregation == Aggregation::Count { "*".to_owned() } else { value_names.join(", ") };
        match plan.aggregation {
            Aggregation::None => inner,
            agg => format!("{}({inner})", agg.sql_name()),
        }
    };
    let mut title = format!("{y_title} by {}", x_info.name);
    if let Some(by) = plan.series_by {
        title.push_str(&format!(", per {}", columns[by].name));
    }

    // ---- shape the output ------------------------------------------------------------------------
    let (series_count, collected) = if grouped {
        let values: Vec<Vec<Option<f64>>> =
            acc.iter().map(|column| column.iter().map(|a| a.result(plan.aggregation)).collect()).collect();
        let total_x = xs.len();
        xs.truncate(bar_limit);
        (values.len(), Collected { xs, values, total_x })
    } else {
        (raw.len(), Collected { xs, values: raw, total_x: raw_total })
    };

    let data = match spec.kind {
        ChartKind::Pie => {
            let slices = if grouped {
                let column = &collected.values[0];
                let mut all: Vec<(f64, &XKey<'_>)> = Vec::new();
                for (v, key) in column.iter().zip(&collected.xs) {
                    if let Some(v) = v {
                        if *v < 0.0 {
                            return Err(ChartIssue::NegativePie { column: columns[plan.values.first().copied().unwrap_or(spec.x)].name.clone() });
                        }
                        all.push((*v, key));
                    }
                }
                let total: f64 = all.iter().map(|(v, _)| v).sum();
                all.sort_by(|a, b| b.0.total_cmp(&a.0));
                let rest = all.len().saturating_sub(MAX_PIE_SLICES);
                let mut slices: Vec<Slice> = all
                    .iter()
                    .take(MAX_PIE_SLICES)
                    .map(|(v, key)| Slice { label: key.label().into(), value: *v, other: false })
                    .collect();
                let shown: f64 = slices.iter().map(|s| s.value).sum();
                if rest > 0 {
                    slices.push(Slice { label: format!("Other ({rest})").into(), value: total - shown, other: true });
                }
                slices
            } else {
                pie_best.sort_by(|a, b| b.0.total_cmp(&a.0));
                pie_best.truncate(MAX_PIE_SLICES);
                let shown: f64 = pie_best.iter().map(|(v, _)| v).sum();
                let mut slices: Vec<Slice> =
                    pie_best.iter().map(|(v, key)| Slice { label: key.label().into(), value: *v, other: false }).collect();
                let rest = pie_count.saturating_sub(slices.len());
                if rest > 0 {
                    slices.push(Slice { label: format!("Other ({rest})").into(), value: (pie_total - shown).max(0.0), other: true });
                }
                slices
            };
            let slices: Vec<Slice> = slices.into_iter().filter(|s| s.value > 0.0 || !s.other).collect();
            if slices.iter().all(|s| s.value <= 0.0) {
                return Err(ChartIssue::NothingToDraw);
            }
            ChartData::Slices(slices)
        }
        ChartKind::Scatter => {
            let x_temporal = x_info.class == ColumnClass::Temporal;
            let mut series: Vec<PointSeries> = Vec::new();
            let mut sampled: Option<(usize, usize)> = None;
            let mut keep = |d: Decimator, name: &SharedString| {
                if d.total > d.points.len() {
                    let (shown, total) = sampled.unwrap_or((0, 0));
                    sampled = Some((shown.max(d.points.len()), total.max(d.total)));
                }
                series.push(PointSeries { name: name.clone(), points: d.points });
            };
            if grouped {
                for (name, column) in series_names.iter().zip(&collected.values) {
                    let mut points: Vec<[f64; 2]> = collected
                        .xs
                        .iter()
                        .zip(column)
                        .filter_map(|(key, v)| Some([key.as_number()?, (*v)?]))
                        .collect();
                    points.sort_by(|a, b| a[0].total_cmp(&b[0]));
                    let mut d = Decimator::new(MAX_SCATTER_POINTS);
                    for p in points {
                        d.push(p);
                    }
                    keep(d, name);
                }
            } else {
                for (name, d) in series_names.iter().zip(decimator) {
                    keep(d, name);
                }
            }
            if let Some((shown, total)) = sampled {
                notes.push(ChartNote::Showing { shown, total, what: "points per series (every few rows)" });
            }
            if series.iter().all(|s| s.points.is_empty()) {
                return Err(ChartIssue::NothingToDraw);
            }
            ChartData::Points(Points { x_temporal, series })
        }
        ChartKind::Bar | ChartKind::Line | ChartKind::Area => {
            let Collected { mut xs, mut values, total_x } = collected;
            if xs.is_empty() || values.iter().all(|column| column.iter().all(Option::is_none)) {
                return Err(ChartIssue::NothingToDraw);
            }
            if spec.kind == ChartKind::Bar {
                if total_x > xs.len() {
                    let what = if grouped { "categories" } else { "rows" };
                    notes.push(ChartNote::Showing { shown: xs.len(), total: total_x, what });
                }
            } else if spec.kind.sorts_x() && matches!(x_info.class, ColumnClass::Numeric | ColumnClass::Temporal) {
                // Chronological (or numeric) order; stable, so equal X keep the order they came in.
                let mut order: Vec<usize> = (0..xs.len()).collect();
                order.sort_by(|&a, &b| xs[a].order(&xs[b]));
                if order.iter().enumerate().any(|(i, &o)| i != o) {
                    xs = order.iter().map(|&i| xs[i].clone()).collect();
                    for column in &mut values {
                        *column = order.iter().map(|&i| column[i]).collect();
                    }
                }
            }
            let mut labels: Vec<SharedString> = Vec::new();
            let mut series_values = values;
            if spec.kind != ChartKind::Bar && xs.len() > max_points {
                let from = xs.len();
                let label_at = downsample(&mut series_values, max_points);
                labels = label_at.iter().map(|&i| SharedString::from(xs[i].label())).collect();
                notes.push(ChartNote::Showing { shown: labels.len(), total: from, what: "points (min / max per pixel)" });
            } else {
                labels.extend(xs.iter().map(|key| SharedString::from(key.label())));
            }
            ChartData::Categories(Categories {
                labels,
                series: series_names
                    .into_iter()
                    .zip(series_values)
                    .take(series_count)
                    .map(|(name, values)| Series { name, values })
                    .collect(),
            })
        }
    };

    if source.partial {
        notes.push(ChartNote::LoadedOnly { rows: source.rows });
    }
    Ok(ChartModel { kind: spec.kind, aggregation: plan.aggregation, title, x_title: x_info.name.clone(), y_title, notes, data })
}

/// The series as it is drawn: a missing sum or count is 0 (no rows is none of it); any other missing
/// value stays missing, and the line breaks there ([`runs`]).
pub fn drawn_values(values: &[Option<f64>], aggregation: Aggregation) -> Vec<Option<f64>> {
    if matches!(aggregation, Aggregation::Sum | Aggregation::Count) {
        values.iter().map(|v| Some(v.unwrap_or(0.0))).collect()
    } else {
        values.to_vec()
    }
}

/// The stretches of a drawn series that have a value, as index ranges: a line is one piece for each, and a missing value
/// between two of them is a break, on screen and in the export alike.
pub fn runs(values: &[Option<f64>]) -> Vec<std::ops::Range<usize>> {
    let mut runs = Vec::new();
    let mut start = None;
    for (index, value) in values.iter().enumerate() {
        match (value, start) {
            (Some(_), None) => start = Some(index),
            (None, Some(from)) => {
                runs.push(from..index);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        runs.push(from..values.len());
    }
    runs
}

/// How wide a line series is stroked, in pixels: what the official `LineChart` draws.
pub const LINE_STROKE: f32 = 2.0;
/// How wide the outline of an area is stroked: what the official `AreaChart` draws.
pub const AREA_STROKE: f32 = 1.0;

/// Keeps at most `limit` points by dropping every other kept point whenever the buffer fills:
/// deterministic, bounded, evenly spread.
struct Decimator {
    limit: usize,
    step: usize,
    seen: usize,
    total: usize,
    points: Vec<[f64; 2]>,
}

impl Decimator {
    fn new(limit: usize) -> Self {
        Self { limit, step: 1, seen: 0, total: 0, points: Vec::new() }
    }

    fn push(&mut self, point: [f64; 2]) {
        self.total += 1;
        if self.seen.is_multiple_of(self.step) {
            if self.points.len() == self.limit {
                let mut keep = 0;
                self.points.retain(|_| {
                    keep += 1;
                    keep % 2 == 1
                });
                self.step *= 2;
            }
            if self.seen.is_multiple_of(self.step) {
                self.points.push(point);
            }
        }
        self.seen += 1;
    }
}

/// Min / max per bucket. Replaces every series in `series` (all of equal length `L > max_points`)
/// with at most `max_points` values and returns, for each kept slot, the source index whose label
/// the slot carries (the bucket's first index for its first slot, its last for its second).
/// The global minimum and maximum of every series survive.
pub fn downsample(series: &mut [Vec<Option<f64>>], max_points: usize) -> Vec<usize> {
    let len = series.first().map_or(0, Vec::len);
    let buckets = (max_points / 2).max(1);
    if len <= buckets * 2 {
        return (0..len).collect();
    }
    let bounds = |b: usize| (b * len / buckets, ((b + 1) * len / buckets).max(b * len / buckets + 1));
    let mut labels = Vec::with_capacity(buckets * 2);
    for b in 0..buckets {
        let (start, end) = bounds(b);
        labels.push(start);
        labels.push(end - 1);
    }
    for column in series.iter_mut() {
        let mut out = Vec::with_capacity(buckets * 2);
        for b in 0..buckets {
            let (start, end) = bounds(b);
            let mut lo: Option<(usize, f64)> = None;
            let mut hi: Option<(usize, f64)> = None;
            for (i, v) in column[start..end].iter().enumerate() {
                let Some(v) = *v else { continue };
                if lo.is_none_or(|(_, l)| v < l) {
                    lo = Some((i, v));
                }
                if hi.is_none_or(|(_, h)| v > h) {
                    hi = Some((i, v));
                }
            }
            match (lo, hi) {
                (Some((li, lv)), Some((hi_i, hv))) => {
                    // The extreme that comes first in time goes first.
                    if li <= hi_i {
                        out.push(Some(lv));
                        out.push(Some(hv));
                    } else {
                        out.push(Some(hv));
                        out.push(Some(lv));
                    }
                }
                _ => {
                    out.push(None);
                    out.push(None);
                }
            }
        }
        *column = out;
    }
    labels
}

#[cfg(test)]
mod tests;
