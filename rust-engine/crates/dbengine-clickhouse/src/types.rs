//! ClickHouse column types and their Arrow representation.
//!
//! `FORMAT ArrowStream` carries no ClickHouse type names, and for several types
//! the Arrow it writes loses what the value *is*:
//!
//! | ClickHouse                       | arrives as            | converted to                   |
//! |----------------------------------|-----------------------|--------------------------------|
//! | `String`, `LowCardinality(String)` | Binary (see below)  | Utf8 (zero-copy when valid)    |
//! | `FixedString(N)`                 | FixedSizeBinary(N)    | Utf8, trailing NULs trimmed    |
//! | `Date` / `Date32`                | Date32 (UInt16 / Int32 when a setting says so) | Date32  |
//! | `DateTime`                       | UInt32                | Timestamp(s), see below        |
//! | `DateTime64`                     | Timestamp(unit, tz)   | Timestamp(unit), see below     |
//! | `IPv4`                           | UInt32                | Utf8 (`1.2.3.4`)               |
//! | `IPv6`                           | FixedSizeBinary(16)   | Utf8                           |
//! | `UUID`                           | FixedSizeBinary(16)   | Utf8 (`8-4-4-4-12`)            |
//! | `Int128`/`UInt128`/`Int256`/`UInt256` | FixedSizeBinary | Utf8 (decimal digits)          |
//! | `Enum8`/`Enum16`                 | Int8 / Int16          | Utf8 (the entry name)          |
//! | `Array`/`Map`/`Tuple`            | List / Map / Struct   | same, children converted       |
//!
//! Time zones: Arrow can only format a time-zone-aware timestamp with a tz
//! database this build does not carry, so the zone is dropped. A zone that is
//! UTC (or absent) keeps a native, zero-copy Timestamp holding the UTC wall
//! time; any other zone becomes Utf8 `YYYY-MM-DD HH:MM:SS[.fff] UTC` (the same
//! instant, written in UTC and labelled as such). A column *without* a zone
//! (`DateTime`, `DateTime64(3)`) has the server's default zone in ClickHouse;
//! the driver reads it as the UTC instant too, shows it without a suffix, and
//! says so in the column label (`datetime (utc)`). Writes follow the same
//! convention (see `datetime`, `value`): the text is an UTC instant.
//!
//! Everything else keeps the type ClickHouse wrote: `Int*`/`UInt*`/`Float*`,
//! `Bool`, `Decimal*` (Decimal128/256), `Date`/`Date32` (Date32),
//! `DateTime64` (Timestamp with the column's unit) and
//! `Nullable` (the Arrow validity bitmap).
//!
//! The driver asks for `String` as Binary (`output_format_arrow_string_as_string=0`)
//! because a ClickHouse `String` may hold arbitrary bytes, and Arrow rejects a
//! Utf8 column that is not valid UTF-8. The Binary buffers are reinterpreted as
//! Utf8 without a copy when every value is valid UTF-8. **Bytes are never
//! replaced**: a column (top level) whose page holds bytes that are not UTF-8
//! stays Arrow Binary with the label `<type> (invalid utf-8)`, exactly as
//! stored, and the app keeps it read-only. (`FixedString(N)`: trailing NULs are
//! padding and are trimmed from text; the Binary form carries all N bytes.)
//!
//! Types the Arrow format cannot express (`AggregateFunction`, `JSON`,
//! `Dynamic`, `Variant`, `Nothing`, ...) make the server refuse the query; the
//! driver then runs it again with exactly those columns wrapped in the server's
//! own text (`toString`), delivered as Utf8 with the label `<type> (read-only)`
//! (see `query`).
//!
//! The ClickHouse type names come from `DESCRIBE`, only when a column's Arrow
//! type is ambiguous (see [`needs_types`]); without them the lossless,
//! type-independent conversions (Binary to Utf8, UUID extension) still apply.

use arrow_array::builder::{BinaryBuilder, StringBuilder};
use arrow_array::cast::AsArray;
use arrow_array::temporal_conversions::as_datetime;
use arrow_array::types::{
    ArrowTimestampType, Int16Type, Int32Type, Int8Type, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt16Type, UInt32Type,
};
use arrow_array::{
    Array, ArrayRef, Date32Array, LargeStringArray, ListArray, MapArray, RecordBatch, StringArray,
    StructArray, TimestampSecondArray,
};
use arrow_buffer::ScalarBuffer;
use arrow_schema::{ArrowError, DataType, Field, Fields, Schema, TimeUnit};
use std::fmt::{self, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use crate::sql::{push_ident, split_expressions};

/// A ClickHouse type as `system.columns.type` / `DESCRIBE` print it:
/// `Nullable(Int32)`, `DateTime64(3, 'UTC')`, `Tuple(a Int32, b String)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChType {
    name: String,
    args: Vec<Arg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arg {
    Type(ChType),
    /// A tuple element `a Int32`.
    Named(String, ChType),
    /// A number or string: `3`, `'UTC'`, an enum entry `'a' = 1`.
    Literal(String),
}

fn unquote(identifier: &str) -> String {
    let bytes = identifier.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(b'`' | b'"'), Some(last)) if bytes.len() >= 2 && bytes[0] == *last => {
            let mut out = String::with_capacity(identifier.len());
            let mut chars = identifier[1..identifier.len() - 1].chars();
            while let Some(c) = chars.next() {
                match (c, c == '\\') {
                    (_, true) => out.extend(chars.next()),
                    _ => out.push(c),
                }
            }
            out
        }
        _ => identifier.to_owned(),
    }
}

impl Arg {
    fn parse(part: &str) -> Self {
        let part = part.trim();
        let first = part.chars().next().unwrap_or(' ');
        if first == '\'' || first == '-' || first.is_ascii_digit() {
            return Self::Literal(part.to_owned());
        }
        // `name Type`: whitespace before any parenthesis (or after a quoted name).
        let (name_end, quoted) = if matches!(first, '`' | '"') {
            let mut index = 1;
            let bytes = part.as_bytes();
            while index < bytes.len() && bytes[index] != bytes[0] {
                index += if bytes[index] == b'\\' { 2 } else { 1 };
            }
            ((index + 1).min(part.len()), true)
        } else {
            let space = part.find(char::is_whitespace);
            let paren = part.find('(');
            match (space, paren) {
                (Some(space), Some(paren)) if paren < space => (part.len(), false),
                (Some(space), _) => (space, false),
                (None, _) => (part.len(), false),
            }
        };
        let rest = part[name_end..].trim();
        if rest.is_empty() && !quoted {
            Self::Type(ChType::parse(part))
        } else if rest.is_empty() {
            Self::Type(ChType::parse(&unquote(part)))
        } else {
            Self::Named(unquote(&part[..name_end]), ChType::parse(rest))
        }
    }
}

impl ChType {
    /// Parses a type name. Never fails: text it does not understand becomes a
    /// type of that name, which no conversion matches.
    pub(crate) fn parse(text: &str) -> Self {
        let text = text.trim();
        let Some(open) = text.find('(') else {
            return Self {
                name: text.to_owned(),
                args: Vec::new(),
            };
        };
        let close = text.rfind(')').filter(|close| *close > open);
        let inner = &text[open + 1..close.unwrap_or(text.len())];
        Self {
            name: text[..open].trim().to_owned(),
            args: split_expressions(inner)
                .iter()
                .map(|part| Arg::parse(part))
                .collect(),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn type_arg(&self, index: usize) -> Option<&ChType> {
        match self.args.get(index)? {
            Arg::Type(inner) | Arg::Named(_, inner) => Some(inner),
            Arg::Literal(_) => None,
        }
    }

    /// The type without `Nullable`, `LowCardinality` and
    /// `SimpleAggregateFunction` wrappers: what the value looks like.
    pub(crate) fn peeled(&self) -> &ChType {
        match self.name.as_str() {
            "Nullable" | "LowCardinality" => self.type_arg(0).map_or(self, ChType::peeled),
            "SimpleAggregateFunction" => self.type_arg(1).map_or(self, ChType::peeled),
            _ => self,
        }
    }

    pub(crate) fn is_nullable(&self) -> bool {
        match self.name.as_str() {
            "Nullable" => true,
            "LowCardinality" => self.type_arg(0).is_some_and(ChType::is_nullable),
            _ => false,
        }
    }

    /// Short lower-case name for the grid header and its numeric sort
    /// (`int32`, `decimal`, `datetime64`, `array`), without parameters. A date
    /// time without a zone says `(utc)`: ClickHouse would read and print it in
    /// the server's zone, the driver shows and writes the UTC instant.
    pub(crate) fn label(&self) -> String {
        let peeled = self.peeled();
        let name = peeled.name.to_ascii_lowercase();
        let zone_less = match peeled.name.as_str() {
            "DateTime" => peeled.timezone(0).is_none(),
            "DateTime64" => peeled.timezone(1).is_none(),
            _ => false,
        };
        if zone_less {
            format!("{name} (utc)")
        } else {
            name
        }
    }

    /// Whether the Arrow output format carries this type and [`convert_batch`]
    /// gives it a display form. Everything else (`AggregateFunction`, `JSON`,
    /// `Dynamic`, `Variant`, `Nothing`, `BFloat16` ...) is fetched as the
    /// server's own text instead.
    pub(crate) fn is_decodable(&self) -> bool {
        match self.name.as_str() {
            "Nullable" | "LowCardinality" => self.type_arg(0).is_some_and(ChType::is_decodable),
            "SimpleAggregateFunction" => self.type_arg(1).is_some_and(ChType::is_decodable),
            "Array" => self.type_arg(0).is_some_and(ChType::is_decodable),
            "Map" => (0..2).all(|index| self.type_arg(index).is_some_and(ChType::is_decodable)),
            "Tuple" => {
                !self.args.is_empty()
                    && (0..self.args.len())
                        .all(|index| self.type_arg(index).is_some_and(ChType::is_decodable))
            }
            "Int8" | "Int16" | "Int32" | "Int64" | "Int128" | "Int256" | "UInt8" | "UInt16"
            | "UInt32" | "UInt64" | "UInt128" | "UInt256" | "Float32" | "Float64" | "Bool"
            | "String" | "FixedString" | "UUID" | "IPv4" | "IPv6" | "Enum8" | "Enum16" | "Date"
            | "Date32" | "DateTime" | "DateTime64" | "Decimal" | "Decimal32" | "Decimal64"
            | "Decimal128" | "Decimal256" | "Time" | "Time64" | "Point" | "Ring" | "LineString"
            | "MultiLineString" | "Polygon" | "MultiPolygon" => true,
            name => name.starts_with("Interval"),
        }
    }

    /// SQL that turns the column `column` (already quoted) of this type into the
    /// server's own text.
    fn server_text_of(&self, column: &str) -> String {
        match self.name.as_str() {
            // The state of an aggregate is bytes: its hex is its text.
            "AggregateFunction" => format!("concat('0x', hex({column}))"),
            // `toString(NULL)` is a NULL of no type, which Arrow cannot write either.
            "Nullable"
                if self
                    .type_arg(0)
                    .is_some_and(|inner| inner.name == "Nothing") =>
            {
                format!("CAST({column} AS Nullable(String))")
            }
            _ => format!("toString({column})"),
        }
    }

    pub(crate) fn literal(&self, index: usize) -> Option<&str> {
        match self.args.get(index)? {
            Arg::Literal(text) => Some(text.as_str()),
            _ => None,
        }
    }

    /// The time zone of `DateTime('UTC')` (argument 0) or
    /// `DateTime64(3, 'UTC')` (argument 1).
    pub(crate) fn timezone(&self, index: usize) -> Option<String> {
        let literal = self.literal(index)?;
        let inner = literal.strip_prefix('\'')?.strip_suffix('\'')?;
        Some(inner.to_owned())
    }

    /// `(value, name)` pairs of an `Enum8`/`Enum16`, sorted by value.
    pub(crate) fn enum_entries(&self) -> Vec<(i16, String)> {
        let mut entries: Vec<(i16, String)> = self
            .args
            .iter()
            .filter_map(|arg| {
                let Arg::Literal(text) = arg else { return None };
                let (name, value) = text.rsplit_once('=')?;
                let name = name.trim().strip_prefix('\'')?.strip_suffix('\'')?;
                Some((value.trim().parse().ok()?, unescape(name)))
            })
            .collect();
        entries.sort_by_key(|(value, _)| *value);
        entries
    }
}

/// Undoes the backslash and doubled-quote escapes of a quoted ClickHouse string.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('0') => out.push('\0'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            '\'' if chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            _ => out.push(c),
        }
    }
    out
}

impl fmt::Display for ChType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if self.args.is_empty() {
            return Ok(());
        }
        f.write_char('(')?;
        for (index, arg) in self.args.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            match arg {
                Arg::Type(inner) => write!(f, "{inner}")?,
                Arg::Named(name, inner) => write!(f, "{name} {inner}")?,
                Arg::Literal(text) => f.write_str(text)?,
            }
        }
        f.write_char(')')
    }
}

fn quoted(name: &str) -> String {
    let mut out = String::new();
    push_ident(&mut out, name);
    out
}

/// The column names and ClickHouse types of a statement, as `DESCRIBE` lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Described {
    pub names: Vec<String>,
    pub types: Vec<ChType>,
}

impl Described {
    /// The `(name, type, ...)` rows of `DESCRIBE TABLE`.
    pub(crate) fn from_rows(rows: &[Vec<serde_json::Value>]) -> Option<Self> {
        fn text(row: &[serde_json::Value], index: usize) -> Option<&str> {
            row.get(index).and_then(serde_json::Value::as_str)
        }
        Some(Self {
            names: rows
                .iter()
                .map(|row| text(row, 0).map(str::to_owned))
                .collect::<Option<_>>()?,
            types: rows
                .iter()
                .map(|row| text(row, 1).map(ChType::parse))
                .collect::<Option<_>>()?,
        })
    }

    /// `SELECT a, toString(b) AS b FROM (<statement>)` with every column whose
    /// type the Arrow output cannot carry (see [`ChType::is_decodable`])
    /// replaced by the server's own text, and the indexes of those columns.
    /// `None` when every column is fine, or when duplicate column names make
    /// the wrapping ambiguous.
    pub(crate) fn wrap_for_text(&self, statement: &str) -> Option<(String, Vec<usize>)> {
        let undecodable: Vec<usize> = self
            .types
            .iter()
            .enumerate()
            .filter(|(_, ty)| !ty.is_decodable())
            .map(|(index, _)| index)
            .collect();
        let mut seen = std::collections::HashSet::new();
        if undecodable.is_empty() || !self.names.iter().all(|name| seen.insert(name.as_str())) {
            return None;
        }
        let mut sql = String::from("SELECT ");
        for (index, (name, ty)) in self.names.iter().zip(&self.types).enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            if undecodable.contains(&index) {
                sql.push_str(&ty.server_text_of(&quoted(name)));
                sql.push_str(" AS ");
            }
            push_ident(&mut sql, name);
        }
        sql.push_str(" FROM (\n");
        sql.push_str(statement);
        sql.push_str("\n)");
        Some((sql, undecodable))
    }
}

// ----------------------------------------------------------------- Arrow

/// Whether `DESCRIBE` is needed to convert a result with this schema: some
/// column (at any depth) has an Arrow type that several ClickHouse types share
/// (`UInt32` is a `DateTime` or an `IPv4`, a timestamp is a `DateTime64` with or
/// without a zone of its own ...), or one the driver reads as text (`Union`).
/// `dates_ambiguous`: the server may write `Date` as `UInt16` and `Date32` as
/// `Int32` (an older server, or a statement `SETTINGS` clause that says so), so
/// those two are ambiguous as well; by default they are not, because every
/// `Int32` column would then cost a `DESCRIBE`.
pub(crate) fn needs_types(schema: &Schema, dates_ambiguous: bool) -> bool {
    fn ambiguous(data_type: &DataType, dates: bool) -> bool {
        match data_type {
            DataType::UInt32
            | DataType::Int8
            | DataType::Int16
            | DataType::FixedSizeBinary(_)
            | DataType::Timestamp(_, _)
            | DataType::Union(_, _) => true,
            DataType::UInt16 | DataType::Int32 => dates,
            DataType::List(field) | DataType::LargeList(field) | DataType::Map(field, _) => {
                ambiguous(field.data_type(), dates)
            }
            DataType::Struct(fields) => fields
                .iter()
                .any(|field| ambiguous(field.data_type(), dates)),
            _ => false,
        }
    }
    schema
        .fields()
        .iter()
        .any(|field| ambiguous(field.data_type(), dates_ambiguous))
}

/// Whether some column (at any depth) is an Arrow `Union`: ClickHouse's
/// `Variant`, which the driver shows as the server's own text instead.
pub(crate) fn has_union(schema: &Schema) -> bool {
    fn union(data_type: &DataType) -> bool {
        match data_type {
            DataType::Union(_, _) => true,
            DataType::List(field) | DataType::LargeList(field) | DataType::Map(field, _) => {
                union(field.data_type())
            }
            DataType::Struct(fields) => fields.iter().any(|field| union(field.data_type())),
            _ => false,
        }
    }
    schema.fields().iter().any(|field| union(field.data_type()))
}

/// The page-column label of an Arrow type when the ClickHouse type is unknown.
pub(crate) fn arrow_label(data_type: &DataType) -> String {
    match data_type {
        DataType::Boolean => "bool".into(),
        DataType::Int8 => "int8".into(),
        DataType::Int16 => "int16".into(),
        DataType::Int32 => "int32".into(),
        DataType::Int64 => "int64".into(),
        DataType::UInt8 => "uint8".into(),
        DataType::UInt16 => "uint16".into(),
        DataType::UInt32 => "uint32".into(),
        DataType::UInt64 => "uint64".into(),
        DataType::Float16 | DataType::Float32 => "float32".into(),
        DataType::Float64 => "float64".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary => {
            "string".into()
        }
        DataType::FixedSizeBinary(_) => "fixedstring".into(),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Timestamp(_, _) => "datetime".into(),
        DataType::Decimal128(_, _) | DataType::Decimal256(_, _) => "decimal".into(),
        DataType::List(_) | DataType::LargeList(_) => "array".into(),
        DataType::Map(_, _) => "map".into(),
        DataType::Struct(_) => "tuple".into(),
        other => other.to_string().to_ascii_lowercase(),
    }
}

/// How a converted column differs from what its type name says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Note {
    #[default]
    Plain,
    /// Arrow Binary: bytes that are not valid UTF-8, exactly as stored.
    InvalidUtf8,
    /// The server's own text of a type Arrow cannot express.
    ServerText,
}

impl Note {
    /// The grid label of a column of base label `label`.
    pub(crate) fn label(self, label: String) -> String {
        match self {
            Self::Plain => label,
            Self::InvalidUtf8 => format!("{label} (invalid utf-8)"),
            Self::ServerText => format!("{label} (read-only)"),
        }
    }
}

/// A batch in its display form and a [`Note`] per column.
pub(crate) struct Converted {
    pub batch: RecordBatch,
    pub notes: Vec<Note>,
}

/// Converts every column of `batch` to its display form (see the module docs).
/// `types` are the ClickHouse types in column order, when known; `server_text`
/// lists the columns that were fetched as the server's own text.
pub(crate) fn convert_batch(
    batch: &RecordBatch,
    types: Option<&[ChType]>,
    server_text: &[usize],
) -> Result<Converted, ArrowError> {
    let schema = batch.schema();
    let mut fields = Vec::with_capacity(batch.num_columns());
    let mut columns = Vec::with_capacity(batch.num_columns());
    let mut notes = Vec::with_capacity(batch.num_columns());
    let mut changed = false;
    for (index, (field, column)) in schema.fields().iter().zip(batch.columns()).enumerate() {
        let ty = types.and_then(|types| types.get(index));
        let converted = convert(column, ty)?.or_else(|| uuid_extension(field, column));
        let (field, column) = match converted {
            Some(array) => {
                changed = true;
                (
                    Field::new(field.name(), array.data_type().clone(), field.is_nullable()),
                    array,
                )
            }
            None => (field.as_ref().clone(), Arc::clone(column)),
        };
        notes.push(if server_text.contains(&index) {
            Note::ServerText
        } else if matches!(column.data_type(), DataType::Binary | DataType::LargeBinary) {
            Note::InvalidUtf8
        } else {
            Note::Plain
        });
        fields.push(field);
        columns.push(column);
    }
    let batch = if changed {
        RecordBatch::try_new_with_options(
            Arc::new(Schema::new(fields)),
            columns,
            &arrow_array::RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
        )?
    } else {
        batch.clone()
    };
    Ok(Converted { batch, notes })
}

/// ClickHouse tags a `UUID` column with the Arrow `arrow.uuid` extension; that
/// is enough to format it without `DESCRIBE`.
fn uuid_extension(field: &Field, column: &ArrayRef) -> Option<ArrayRef> {
    let is_uuid = field
        .metadata()
        .get("ARROW:extension:name")
        .is_some_and(|name| name == "arrow.uuid");
    let DataType::FixedSizeBinary(16) = column.data_type() else {
        return None;
    };
    is_uuid.then(|| fixed_to_text(column, write_uuid))
}

/// Converts one column; `None` when it already has its display form.
fn convert(array: &ArrayRef, ty: Option<&ChType>) -> Result<Option<ArrayRef>, ArrowError> {
    let peeled = ty.map(ChType::peeled);
    let name = peeled.map_or("", ChType::name);
    Ok(match array.data_type() {
        DataType::UInt32 if name == "DateTime" => {
            let values = array.as_primitive::<UInt32Type>();
            let seconds: Vec<i64> = values.values().iter().map(|v| i64::from(*v)).collect();
            let timestamps: ArrayRef = Arc::new(TimestampSecondArray::new(
                ScalarBuffer::from(seconds),
                values.nulls().cloned(),
            ));
            Some(match peeled.and_then(|ty| ty.timezone(0)) {
                Some(zone) if !is_utc(&zone) => {
                    timestamps_as_text::<TimestampSecondType>(&timestamps)
                }
                // UTC, or no zone at all (the server's default): the UTC instant.
                _ => timestamps,
            })
        }
        DataType::Timestamp(unit, Some(zone)) => {
            // `DateTime64(3)` without a zone arrives tagged with the server's
            // default zone: still the UTC instant, shown like a UTC column.
            let zone_less =
                peeled.is_some_and(|ty| ty.name() == "DateTime64" && ty.timezone(1).is_none());
            let utc = zone_less || is_utc(zone);
            Some(match (unit, utc) {
                (TimeUnit::Second, true) => naive::<TimestampSecondType>(array),
                (TimeUnit::Millisecond, true) => naive::<TimestampMillisecondType>(array),
                (TimeUnit::Microsecond, true) => naive::<TimestampMicrosecondType>(array),
                (TimeUnit::Nanosecond, true) => naive::<TimestampNanosecondType>(array),
                (TimeUnit::Second, false) => timestamps_as_text::<TimestampSecondType>(array),
                (TimeUnit::Millisecond, false) => {
                    timestamps_as_text::<TimestampMillisecondType>(array)
                }
                (TimeUnit::Microsecond, false) => {
                    timestamps_as_text::<TimestampMicrosecondType>(array)
                }
                (TimeUnit::Nanosecond, false) => {
                    timestamps_as_text::<TimestampNanosecondType>(array)
                }
            })
        }
        // A `Date` is Date32 unless `output_format_arrow_date_as_uint16` says
        // otherwise (an older server, a profile): the column type decides.
        DataType::UInt16 if name == "Date" => {
            let days: Vec<i32> = array
                .as_primitive::<UInt16Type>()
                .values()
                .iter()
                .map(|v| i32::from(*v))
                .collect();
            Some(Arc::new(Date32Array::new(
                ScalarBuffer::from(days),
                array.nulls().cloned(),
            )))
        }
        DataType::Int32 if name == "Date32" => Some(Arc::new(Date32Array::new(
            array.as_primitive::<Int32Type>().values().clone(),
            array.nulls().cloned(),
        ))),
        DataType::UInt32 if name == "IPv4" => {
            let values = array.as_primitive::<UInt32Type>();
            let mut out = StringBuilder::with_capacity(values.len(), values.len() * 12);
            for row in 0..values.len() {
                if values.is_null(row) {
                    out.append_null();
                } else {
                    let _ = write!(out, "{}", Ipv4Addr::from(values.value(row)));
                    out.append_value("");
                }
            }
            Some(Arc::new(out.finish()))
        }
        DataType::FixedSizeBinary(16) if name == "IPv6" => {
            Some(fixed_to_text(array, |bytes, out| {
                let octets: [u8; 16] = bytes.try_into().unwrap_or_default();
                let _ = write!(out, "{}", Ipv6Addr::from(octets));
            }))
        }
        DataType::FixedSizeBinary(16) if name == "UUID" => Some(fixed_to_text(array, write_uuid)),
        DataType::FixedSizeBinary(16 | 32)
            if matches!(name, "Int128" | "UInt128" | "Int256" | "UInt256") =>
        {
            let signed = name.starts_with('I');
            Some(fixed_to_text(array, |bytes, out| {
                write_wide_integer(bytes, signed, out)
            }))
        }
        DataType::FixedSizeBinary(_) if name == "FixedString" => Some(fixed_string(array)),
        DataType::Int8 if name == "Enum8" => {
            let values = array.as_primitive::<Int8Type>();
            let entries = peeled.map(ChType::enum_entries).unwrap_or_default();
            Some(enum_names(
                values.len(),
                |row| (!values.is_null(row)).then(|| i16::from(values.value(row))),
                &entries,
            ))
        }
        DataType::Int16 if name == "Enum16" => {
            let values = array.as_primitive::<Int16Type>();
            let entries = peeled.map(ChType::enum_entries).unwrap_or_default();
            Some(enum_names(
                values.len(),
                |row| (!values.is_null(row)).then(|| values.value(row)),
                &entries,
            ))
        }
        // Valid UTF-8 shares the buffers; anything else stays Binary, untouched.
        DataType::Binary => binary_to_text(array.as_binary::<i32>()),
        DataType::LargeBinary => {
            let binary = array.as_binary::<i64>();
            LargeStringArray::try_new(
                binary.offsets().clone(),
                binary.values().clone(),
                binary.nulls().cloned(),
            )
            .ok()
            .map(|text| Arc::new(text) as ArrayRef)
        }
        DataType::List(field) => {
            let list = array.as_list::<i32>();
            let element = peeled
                .filter(|ty| ty.name() == "Array")
                .and_then(|ty| ty.type_arg(0));
            convert(list.values(), element)?
                .map(|values| {
                    let field = Arc::new(Field::new(
                        field.name(),
                        values.data_type().clone(),
                        field.is_nullable(),
                    ));
                    ListArray::try_new(field, list.offsets().clone(), values, list.nulls().cloned())
                        .map(|list| Arc::new(list) as ArrayRef)
                })
                .transpose()?
        }
        DataType::Map(entries_field, sorted) => {
            let map = array.as_map();
            let (key_ty, value_ty) = match peeled.filter(|ty| ty.name() == "Map") {
                Some(ty) => (ty.type_arg(0), ty.type_arg(1)),
                None => (None, None),
            };
            let keys = convert(map.keys(), key_ty)?;
            let values = convert(map.values(), value_ty)?;
            if keys.is_none() && values.is_none() {
                None
            } else {
                let keys = keys.unwrap_or_else(|| Arc::clone(map.keys()));
                let values = values.unwrap_or_else(|| Arc::clone(map.values()));
                let key_name = map.entries().fields()[0].name().clone();
                let value_name = map.entries().fields()[1].name().clone();
                let fields = Fields::from(vec![
                    Field::new(key_name, keys.data_type().clone(), false),
                    Field::new(
                        value_name,
                        values.data_type().clone(),
                        map.entries().fields()[1].is_nullable(),
                    ),
                ]);
                let entries = StructArray::try_new(fields.clone(), vec![keys, values], None)?;
                let field = Arc::new(Field::new(
                    entries_field.name(),
                    DataType::Struct(fields),
                    entries_field.is_nullable(),
                ));
                Some(Arc::new(MapArray::try_new(
                    field,
                    map.offsets().clone(),
                    entries,
                    map.nulls().cloned(),
                    *sorted,
                )?) as ArrayRef)
            }
        }
        DataType::Struct(fields) => {
            let tuple = array.as_struct();
            let element = |index: usize| {
                peeled
                    .filter(|ty| ty.name() == "Tuple")
                    .and_then(|ty| ty.type_arg(index))
            };
            let mut changed = false;
            let mut new_fields = Vec::with_capacity(fields.len());
            let mut columns = Vec::with_capacity(fields.len());
            for (index, (field, column)) in fields.iter().zip(tuple.columns()).enumerate() {
                match convert(column, element(index))? {
                    Some(converted) => {
                        changed = true;
                        new_fields.push(Field::new(
                            field.name(),
                            converted.data_type().clone(),
                            field.is_nullable(),
                        ));
                        columns.push(converted);
                    }
                    None => {
                        new_fields.push(field.as_ref().clone());
                        columns.push(Arc::clone(column));
                    }
                }
            }
            if changed {
                Some(Arc::new(StructArray::try_new(
                    Fields::from(new_fields),
                    columns,
                    tuple.nulls().cloned(),
                )?) as ArrayRef)
            } else {
                None
            }
        }
        _ => None,
    })
}

fn is_utc(zone: &str) -> bool {
    matches!(
        zone,
        "UTC"
            | "Etc/UTC"
            | "Etc/UCT"
            | "UCT"
            | "GMT"
            | "Etc/GMT"
            | "Z"
            | "Zulu"
            | "Etc/Zulu"
            | "Universal"
            | "Etc/Universal"
    )
}

/// The same timestamps without a time zone (sharing the buffers).
fn naive<T: ArrowTimestampType>(array: &ArrayRef) -> ArrayRef {
    Arc::new(
        array
            .as_primitive::<T>()
            .clone()
            .with_timezone_opt(None::<Arc<str>>),
    )
}

/// Timestamps as `YYYY-MM-DD HH:MM:SS[.fff] UTC`.
fn timestamps_as_text<T: ArrowTimestampType>(array: &ArrayRef) -> ArrayRef {
    let values = array.as_primitive::<T>();
    let mut out = StringBuilder::with_capacity(values.len(), values.len() * 27);
    for row in 0..values.len() {
        if values.is_null(row) {
            out.append_null();
            continue;
        }
        match as_datetime::<T>(values.value(row)) {
            Some(moment) => {
                let _ = write!(out, "{moment} UTC");
                out.append_value("");
            }
            None => out.append_value(values.value(row).to_string()),
        }
    }
    Arc::new(out.finish())
}

/// Binary to Utf8 sharing the buffers when every value is valid UTF-8; `None`
/// (the Binary stays as it is) when some value is not.
fn binary_to_text(binary: &arrow_array::BinaryArray) -> Option<ArrayRef> {
    StringArray::try_new(
        binary.offsets().clone(),
        binary.values().clone(),
        binary.nulls().cloned(),
    )
    .ok()
    .map(|text| Arc::new(text) as ArrayRef)
}

/// A `FixedString(N)` column: Utf8 with the padding NULs trimmed when every
/// value is valid UTF-8, else Binary holding all N bytes of every value.
fn fixed_string(array: &ArrayRef) -> ArrayRef {
    let fixed = array.as_fixed_size_binary();
    fn trimmed(bytes: &[u8]) -> &[u8] {
        let end = bytes
            .iter()
            .rposition(|byte| *byte != 0)
            .map_or(0, |last| last + 1);
        &bytes[..end]
    }
    let mut out = StringBuilder::with_capacity(fixed.len(), fixed.len() * 16);
    for row in 0..fixed.len() {
        if fixed.is_null(row) {
            out.append_null();
        } else if let Ok(text) = std::str::from_utf8(trimmed(fixed.value(row))) {
            out.append_value(text);
        } else {
            let mut raw = BinaryBuilder::with_capacity(fixed.len(), fixed.len() * 16);
            for row in 0..fixed.len() {
                if fixed.is_null(row) {
                    raw.append_null();
                } else {
                    raw.append_value(fixed.value(row));
                }
            }
            return Arc::new(raw.finish());
        }
    }
    Arc::new(out.finish())
}

/// A FixedSizeBinary column as text, one `write` per non-null row.
fn fixed_to_text(array: &ArrayRef, write: impl Fn(&[u8], &mut String)) -> ArrayRef {
    let fixed = array.as_fixed_size_binary();
    let mut out = StringBuilder::with_capacity(fixed.len(), fixed.len() * 16);
    let mut scratch = String::new();
    for row in 0..fixed.len() {
        if fixed.is_null(row) {
            out.append_null();
        } else {
            scratch.clear();
            write(fixed.value(row), &mut scratch);
            out.append_value(&scratch);
        }
    }
    Arc::new(out.finish())
}

fn enum_names(
    rows: usize,
    value: impl Fn(usize) -> Option<i16>,
    entries: &[(i16, String)],
) -> ArrayRef {
    let mut out = StringBuilder::with_capacity(rows, rows * 8);
    for row in 0..rows {
        match value(row) {
            None => out.append_null(),
            Some(number) => match entries.binary_search_by_key(&number, |(value, _)| *value) {
                Ok(found) => out.append_value(&entries[found].1),
                Err(_) => out.append_value(number.to_string()),
            },
        }
    }
    Arc::new(out.finish())
}

/// A UUID's 16 big-endian bytes as `8-4-4-4-12` hex.
fn write_uuid(bytes: &[u8], out: &mut String) {
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
}

/// A little-endian 128- or 256-bit integer in decimal.
fn write_wide_integer(bytes: &[u8], signed: bool, out: &mut String) {
    let mut limbs: Vec<u64> = bytes
        .chunks(8)
        .map(|chunk| {
            let mut limb = [0_u8; 8];
            limb[..chunk.len()].copy_from_slice(chunk);
            u64::from_le_bytes(limb)
        })
        .collect();
    let negative = signed && limbs.last().is_some_and(|top| top >> 63 == 1);
    if negative {
        // Two's complement negation of the magnitude.
        let mut carry = true;
        for limb in &mut limbs {
            let (value, overflow) = (!*limb).overflowing_add(u64::from(carry));
            *limb = value;
            carry = overflow;
        }
    }
    // Repeated division by 10^19 (the largest power of ten in a u64).
    const CHUNK: u64 = 10_000_000_000_000_000_000;
    let mut chunks = Vec::new();
    while limbs.iter().any(|limb| *limb != 0) {
        let mut remainder = 0_u128;
        for limb in limbs.iter_mut().rev() {
            let current = (remainder << 64) | u128::from(*limb);
            *limb = (current / u128::from(CHUNK)) as u64;
            remainder = current % u128::from(CHUNK);
        }
        chunks.push(remainder as u64);
    }
    if negative {
        out.push('-');
    }
    match chunks.split_last() {
        None => out.push('0'),
        Some((top, rest)) => {
            let _ = write!(out, "{top}");
            for chunk in rest.iter().rev() {
                let _ = write!(out, "{chunk:019}");
            }
        }
    }
}

/// The schema of a converted batch, for tests and diagnostics.
#[cfg(test)]
pub(crate) fn data_types(batch: &RecordBatch) -> Vec<DataType> {
    batch
        .schema()
        .fields()
        .iter()
        .map(|field| field.data_type().clone())
        .collect()
}

/// The Arrow schema type a converted column has, by ClickHouse type name; the
/// table in the module docs as code.
#[cfg(test)]
pub(crate) fn expected_arrow(ty: &ChType) -> Option<DataType> {
    use arrow_schema::TimeUnit;
    let peeled = ty.peeled();
    Some(match peeled.name() {
        "Int8" => DataType::Int8,
        "Int16" => DataType::Int16,
        "Int32" => DataType::Int32,
        "Int64" => DataType::Int64,
        "UInt8" => DataType::UInt8,
        "UInt16" => DataType::UInt16,
        "UInt32" => DataType::UInt32,
        "UInt64" => DataType::UInt64,
        "Float32" => DataType::Float32,
        "Float64" => DataType::Float64,
        "Bool" => DataType::Boolean,
        "String" | "FixedString" | "UUID" | "IPv4" | "IPv6" | "Enum8" | "Enum16" | "Int128"
        | "UInt128" | "Int256" | "UInt256" => DataType::Utf8,
        "Date" | "Date32" => DataType::Date32,
        "DateTime" => DataType::Timestamp(TimeUnit::Second, peeled.timezone(0).map(Into::into)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{BinaryBuilder, FixedSizeBinaryBuilder, ListBuilder};
    use arrow_array::{
        BinaryArray, Date32Array, Decimal128Array, FixedSizeBinaryArray, Float32Array, Int16Array,
        Int32Array, Int64Array, Int8Array, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
    };
    use arrow_schema::TimeUnit;
    use std::collections::HashMap;

    fn one(array: ArrayRef) -> RecordBatch {
        RecordBatch::try_from_iter([("c", array)]).unwrap()
    }

    fn convert_one(array: ArrayRef, ty: &str) -> RecordBatch {
        convert_batch(&one(array), Some(&[ChType::parse(ty)]), &[])
            .unwrap()
            .batch
    }

    fn texts(batch: &RecordBatch) -> Vec<Option<String>> {
        let column = batch.column(0).as_string::<i32>();
        (0..column.len())
            .map(|row| (!column.is_null(row)).then(|| column.value(row).to_owned()))
            .collect()
    }

    #[test]
    fn type_names_parse_with_parameters_names_and_nesting() {
        let ty = ChType::parse("Nullable(Int32)");
        assert!(ty.is_nullable() && ty.peeled().name() == "Int32");
        let ty = ChType::parse("LowCardinality(Nullable(String))");
        assert!(ty.is_nullable() && ty.peeled().name() == "String");
        assert_eq!(
            ChType::parse("DateTime64(3, 'Europe/Berlin')")
                .timezone(1)
                .as_deref(),
            Some("Europe/Berlin")
        );
        assert_eq!(
            ChType::parse("DateTime('UTC')").timezone(0).as_deref(),
            Some("UTC")
        );
        assert_eq!(ChType::parse("DateTime").timezone(0), None);
        let tuple = ChType::parse("Tuple(a Int32, `b c` Array(Nullable(String)), UInt8)");
        assert_eq!(
            tuple.to_string(),
            "Tuple(a Int32, b c Array(Nullable(String)), UInt8)"
        );
        assert_eq!(
            tuple.type_arg(1).map(ChType::to_string).as_deref(),
            Some("Array(Nullable(String))")
        );
        let map = ChType::parse("Map(String, Array(Tuple(x Int8, y Decimal(9, 2))))");
        assert_eq!(map.type_arg(1).unwrap().name(), "Array");
        assert_eq!(
            ChType::parse("SimpleAggregateFunction(sum, UInt64)")
                .peeled()
                .name(),
            "UInt64"
        );
        assert_eq!(ChType::parse("Decimal(9, 2)").label(), "decimal");
        assert_eq!(ChType::parse("Array(Int32)").label(), "array");
        assert_eq!(ChType::parse("Nullable(UInt64)").label(), "uint64");
        assert!(ChType::parse("Array(Int8)").is_decodable());
        assert!(!ChType::parse("Array(Nullable(JSON))").is_decodable());
        assert!(!ChType::parse("AggregateFunction(sum, UInt64)").is_decodable());
        assert!(ChType::parse("SimpleAggregateFunction(sum, UInt64)").is_decodable());
        assert!(!ChType::parse("Tuple(a Int32, b Variant(UInt8, String))").is_decodable());
        assert!(ChType::parse("Map(String, Array(Tuple(x Int8, y Decimal(9, 2))))").is_decodable());
        assert!(!ChType::parse("Nullable(Nothing)").is_decodable());
        assert_eq!(ChType::parse("DateTime").label(), "datetime (utc)");
        assert_eq!(
            ChType::parse("Nullable(DateTime64(3))").label(),
            "datetime64 (utc)"
        );
        assert_eq!(ChType::parse("DateTime('Asia/Tokyo')").label(), "datetime");
        assert_eq!(ChType::parse("DateTime64(3, 'UTC')").label(), "datetime64");
        // Text it cannot read is kept, not rejected.
        assert_eq!(ChType::parse("Whatever(").name(), "Whatever");
    }

    #[test]
    fn enum_entries_survive_commas_quotes_and_negative_values() {
        let ty = ChType::parse("Enum8('a,b' = -1, 'it''s' = 2, 'q\\'x' = 3, '' = 0)");
        assert_eq!(
            ty.enum_entries(),
            [
                (-1, "a,b".to_owned()),
                (0, String::new()),
                (2, "it's".to_owned()),
                (3, "q'x".to_owned())
            ]
        );
    }

    #[test]
    fn plain_numbers_booleans_and_dates_keep_their_arrow_types() {
        // Nothing to convert: the batch comes back untouched (same buffers).
        let batch = RecordBatch::try_from_iter([
            ("i", Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef),
            ("u", Arc::new(UInt64Array::from(vec![u64::MAX, 0]))),
            ("f", Arc::new(Float32Array::from(vec![1.5, 2.5]))),
            ("d", Arc::new(Date32Array::from(vec![18262, 0]))),
            ("n", Arc::new(Int64Array::from(vec![Some(1), None]))),
            ("b", Arc::new(UInt8Array::from(vec![1, 0]))),
            (
                "dec",
                Arc::new(
                    Decimal128Array::from(vec![125, 0])
                        .with_precision_and_scale(9, 2)
                        .unwrap(),
                ),
            ),
        ])
        .unwrap();
        let types: Vec<ChType> = [
            "Int32",
            "UInt64",
            "Float32",
            "Date",
            "Nullable(Int64)",
            "UInt8",
            "Decimal(9, 2)",
        ]
        .map(ChType::parse)
        .into();
        let converted = convert_batch(&batch, Some(&types), &[]).unwrap().batch;
        assert_eq!(data_types(&converted), data_types(&batch));
        for (index, ty) in types.iter().enumerate() {
            let expected = expected_arrow(ty);
            if let Some(expected) = expected {
                assert_eq!(
                    converted.schema().field(index).data_type(),
                    &expected,
                    "{ty}"
                );
            }
        }
    }

    #[test]
    fn string_is_utf8_without_a_copy_and_bytes_are_never_replaced() {
        let mut builder = BinaryBuilder::new();
        builder.append_value("héllo");
        builder.append_null();
        builder.append_value("");
        let valid = builder.finish();
        let before = valid.values().as_ptr();
        let converted = convert_one(Arc::new(valid), "Nullable(String)");
        assert_eq!(
            texts(&converted),
            [Some("héllo".into()), None, Some(String::new())]
        );
        assert_eq!(
            converted.column(0).as_string::<i32>().values().as_ptr(),
            before,
            "zero-copy"
        );
        // Bytes that are not UTF-8 (a hash stored in a String) stay bytes: the
        // same Binary array, the label says so and the app keeps it read-only.
        let broken = BinaryArray::from(vec![&b"ok"[..], &[0xff, 0xfe, b'x'][..]]);
        let before = broken.values().as_ptr();
        let converted = convert_batch(
            &one(Arc::new(broken)),
            Some(&[ChType::parse("String")]),
            &[],
        )
        .unwrap();
        assert_eq!(converted.notes, [Note::InvalidUtf8]);
        let bytes = converted.batch.column(0).as_binary::<i32>();
        assert_eq!(bytes.value(0), b"ok");
        assert_eq!(bytes.value(1), [0xff, 0xfe, b'x']);
        assert_eq!(bytes.values().as_ptr(), before, "zero-copy");
        assert_eq!(
            Note::InvalidUtf8.label("string".into()),
            "string (invalid utf-8)"
        );
        // One bad row decides for the whole column of the page, nothing is lost.
        let mixed = BinaryArray::from(vec![Some(&b"fine"[..]), None, Some(&[0xc3, 0x28][..])]);
        let converted = convert_one(Arc::new(mixed), "Nullable(String)");
        let bytes = converted.column(0).as_binary::<i32>();
        assert_eq!(
            (bytes.value(0), bytes.is_null(1), bytes.value(2)),
            (&b"fine"[..], true, &[0xc3, 0x28][..])
        );
        // Without ClickHouse types Binary still becomes text.
        let untyped = convert_batch(
            &one(Arc::new(BinaryArray::from(vec![&b"a"[..]]))),
            None,
            &[],
        )
        .unwrap()
        .batch;
        assert_eq!(texts(&untyped), [Some("a".into())]);
        // Text fetched from the server is flagged whatever its bytes.
        let text = convert_batch(
            &one(Arc::new(BinaryArray::from(vec![&b"{}"[..]]))),
            Some(&[ChType::parse("JSON")]),
            &[0],
        )
        .unwrap();
        assert_eq!(text.notes, [Note::ServerText]);
        assert_eq!(texts(&text.batch), [Some("{}".into())]);
        assert_eq!(Note::ServerText.label("json".into()), "json (read-only)");
    }

    #[test]
    fn dates_arrive_as_dates_whatever_physical_type_the_server_used() {
        // `output_format_arrow_date_as_uint16`, or an older server: a Date is a
        // UInt16, a Date32 an Int32; the column type says what they are.
        let days = UInt16Array::from(vec![Some(18_263), None, Some(0)]);
        let converted = convert_one(Arc::new(days), "Nullable(Date)");
        let dates = converted
            .column(0)
            .as_primitive::<arrow_array::types::Date32Type>();
        assert_eq!(
            (dates.value(0), dates.is_null(1), dates.value(2)),
            (18_263, true, 0)
        );
        let days = Int32Array::from(vec![-25_567, 18_263]);
        let converted = convert_one(Arc::new(days), "Date32");
        let dates = converted
            .column(0)
            .as_primitive::<arrow_array::types::Date32Type>();
        assert_eq!((dates.value(0), dates.value(1)), (-25_567, 18_263));
        // A plain UInt16 / Int32 column is not touched.
        let plain = convert_one(Arc::new(UInt16Array::from(vec![7])), "UInt16");
        assert_eq!(plain.column(0).data_type(), &DataType::UInt16);
        let plain = convert_one(Arc::new(Int32Array::from(vec![7])), "Int32");
        assert_eq!(plain.column(0).data_type(), &DataType::Int32);
    }

    #[test]
    fn datetime_and_date_time_types_become_timestamps() {
        let seconds = UInt32Array::from(vec![Some(1_577_934_245), None]);
        let converted = convert_one(Arc::new(seconds.clone()), "Nullable(DateTime('UTC'))");
        assert_eq!(
            converted.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Second, None)
        );
        // Another zone: the UTC instant as text (Arrow cannot format the zone).
        let berlin = convert_one(
            Arc::new(seconds.clone()),
            "Nullable(DateTime('Europe/Berlin'))",
        );
        assert_eq!(
            texts(&berlin),
            [Some("2020-01-02 03:04:05 UTC".into()), None]
        );
        // DateTime64 arrives as Timestamp(unit, zone).
        let millis = arrow_array::TimestampMillisecondArray::from(vec![1_577_934_245_123])
            .with_timezone("UTC");
        let utc = convert_one(Arc::new(millis.clone()), "DateTime64(3, 'UTC')");
        assert_eq!(
            utc.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Millisecond, None)
        );
        assert_eq!(
            utc.column(0)
                .as_primitive::<TimestampMillisecondType>()
                .value(0),
            1_577_934_245_123
        );
        let tokyo = convert_one(
            Arc::new(millis.with_timezone("Asia/Tokyo")),
            "DateTime64(3, 'Asia/Tokyo')",
        );
        assert_eq!(texts(&tokyo), [Some("2020-01-02 03:04:05.123 UTC".into())]);
        let timestamps = converted
            .column(0)
            .as_primitive::<arrow_array::types::TimestampSecondType>();
        assert_eq!(timestamps.value(0), 1_577_934_245);
        assert!(timestamps.is_null(1));
        // No time zone in the type: a naive timestamp.
        let naive = convert_one(Arc::new(seconds), "DateTime");
        assert_eq!(
            naive.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Second, None)
        );
        // `DateTime64(3)` has no zone of its own; Arrow tags it with the
        // server's default. It is still the UTC instant, shown without a suffix,
        // whatever that default is.
        let tagged = arrow_array::TimestampMillisecondArray::from(vec![1_577_934_245_123])
            .with_timezone("America/New_York");
        let zone_less = convert_one(Arc::new(tagged.clone()), "DateTime64(3)");
        assert_eq!(
            zone_less.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Millisecond, None)
        );
        assert_eq!(
            zone_less
                .column(0)
                .as_primitive::<TimestampMillisecondType>()
                .value(0),
            1_577_934_245_123
        );
        // The same Arrow tag on a column that does name that zone: text.
        let named = convert_one(Arc::new(tagged), "DateTime64(3, 'America/New_York')");
        assert_eq!(texts(&named), [Some("2020-01-02 03:04:05.123 UTC".into())]);
    }

    #[test]
    fn ip_addresses_are_text() {
        let v4 = convert_one(Arc::new(UInt32Array::from(vec![16_909_060, 0])), "IPv4");
        assert_eq!(texts(&v4), [Some("1.2.3.4".into()), Some("0.0.0.0".into())]);
        let mut builder = FixedSizeBinaryBuilder::new(16);
        builder.append_value(Ipv6Addr::LOCALHOST.octets()).unwrap();
        builder
            .append_value(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets())
            .unwrap();
        builder.append_null();
        let v6 = convert_one(Arc::new(builder.finish()), "Nullable(IPv6)");
        assert_eq!(
            texts(&v6),
            [Some("::1".into()), Some("2001:db8::1".into()), None]
        );
    }

    #[test]
    fn uuids_format_from_the_type_or_from_the_arrow_extension() {
        let bytes = [
            0xc9, 0x1b, 0xf7, 0xe0, 0x92, 0xee, 0x41, 0x69, 0xac, 0x36, 0x4e, 0xc4, 0x1d, 0x2d,
            0x4d, 0x54,
        ];
        let array = FixedSizeBinaryArray::try_from_iter([bytes.as_slice()].into_iter()).unwrap();
        let typed = convert_one(Arc::new(array.clone()), "UUID");
        assert_eq!(
            texts(&typed),
            [Some("c91bf7e0-92ee-4169-ac36-4ec41d2d4d54".into())]
        );
        // Without DESCRIBE the extension metadata on the field is enough.
        let field = Field::new("c", DataType::FixedSizeBinary(16), false).with_metadata(
            HashMap::from([("ARROW:extension:name".to_owned(), "arrow.uuid".to_owned())]),
        );
        let batch = RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![Arc::new(array)])
            .unwrap();
        assert_eq!(
            texts(&convert_batch(&batch, None, &[]).unwrap().batch),
            [Some("c91bf7e0-92ee-4169-ac36-4ec41d2d4d54".into())]
        );
    }

    #[test]
    fn wide_integers_fixed_strings_and_enums_are_text() {
        let le = |value: i128| value.to_le_bytes();
        let ints = FixedSizeBinaryArray::try_from_iter(
            [le(5), le(-5), le(i128::MIN), le(i128::MAX)]
                .iter()
                .map(|b| b.as_slice()),
        )
        .unwrap();
        assert_eq!(
            texts(&convert_one(Arc::new(ints), "Int128")),
            [
                Some("5".into()),
                Some("-5".into()),
                Some("-170141183460469231731687303715884105728".into()),
                Some("170141183460469231731687303715884105727".into())
            ]
        );
        let unsigned = FixedSizeBinaryArray::try_from_iter(
            [u128::MAX.to_le_bytes()].iter().map(|b| b.as_slice()),
        )
        .unwrap();
        assert_eq!(
            texts(&convert_one(Arc::new(unsigned), "UInt128")),
            [Some("340282366920938463463374607431768211455".into())]
        );
        let max256 = [0xff_u8; 32];
        let huge = FixedSizeBinaryArray::try_from_iter([max256.as_slice()].into_iter()).unwrap();
        assert_eq!(
            texts(&convert_one(Arc::new(huge), "UInt256")),
            [Some(
                "115792089237316195423570985008687907853269984665640564039457584007913129639935"
                    .into()
            )]
        );
        let signed =
            FixedSizeBinaryArray::try_from_iter([[0xff_u8; 32].as_slice()].into_iter()).unwrap();
        assert_eq!(
            texts(&convert_one(Arc::new(signed), "Int256")),
            [Some("-1".into())]
        );
        let zero =
            FixedSizeBinaryArray::try_from_iter([[0_u8; 16].as_slice()].into_iter()).unwrap();
        assert_eq!(
            texts(&convert_one(Arc::new(zero), "UInt128")),
            [Some("0".into())]
        );

        let fixed = FixedSizeBinaryArray::try_from_iter(
            [b"ab\0".as_slice(), b"\0\0\0".as_slice()].into_iter(),
        )
        .unwrap();
        assert_eq!(
            texts(&convert_one(Arc::new(fixed), "FixedString(3)")),
            [Some("ab".into()), Some(String::new())]
        );
        // Padding is trimmed from text; a value that is not UTF-8 keeps all its
        // N bytes (the whole column of the page then is Binary).
        let hashes = FixedSizeBinaryArray::try_from_iter(
            [[0xff, 0, 0, 1].as_slice(), b"ab\0\0".as_slice()].into_iter(),
        )
        .unwrap();
        let converted = convert_batch(
            &one(Arc::new(hashes)),
            Some(&[ChType::parse("FixedString(4)")]),
            &[],
        )
        .unwrap();
        assert_eq!(converted.notes, [Note::InvalidUtf8]);
        let bytes = converted.batch.column(0).as_binary::<i32>();
        assert_eq!(bytes.value(0), [0xff, 0, 0, 1]);
        assert_eq!(bytes.value(1), b"ab\0\0");

        let enum8 = Int8Array::from(vec![Some(1), Some(2), None, Some(9)]);
        assert_eq!(
            texts(&convert_one(
                Arc::new(enum8),
                "Nullable(Enum8('a' = 1, 'b' = 2))"
            )),
            [Some("a".into()), Some("b".into()), None, Some("9".into())]
        );
        let enum16 = Int16Array::from(vec![1000, -3]);
        assert_eq!(
            texts(&convert_one(
                Arc::new(enum16),
                "Enum16('big' = 1000, 'neg' = -3)"
            )),
            [Some("big".into()), Some("neg".into())]
        );
    }

    #[test]
    fn nested_types_convert_their_children() {
        // Array(DateTime): UInt32 elements become timestamps, offsets are shared.
        let mut builder = ListBuilder::new(arrow_array::builder::UInt32Builder::new());
        builder.values().append_value(10);
        builder.values().append_value(20);
        builder.append(true);
        builder.append(true);
        let list = builder.finish();
        let converted = convert_one(Arc::new(list), "Array(DateTime)");
        let list = converted.column(0).as_list::<i32>();
        assert_eq!(
            list.values().data_type(),
            &DataType::Timestamp(TimeUnit::Second, None)
        );
        assert_eq!(list.value_length(0), 2);
        assert_eq!(list.value_length(1), 0);

        // Array(String): Binary children become text without any ClickHouse type.
        let mut builder = ListBuilder::new(BinaryBuilder::new());
        builder.values().append_value("x");
        builder.append(true);
        let untyped = convert_batch(&one(Arc::new(builder.finish())), None, &[])
            .unwrap()
            .batch;
        assert_eq!(
            untyped.column(0).as_list::<i32>().values().data_type(),
            &DataType::Utf8
        );

        // Tuple(a UInt32, b String) with IPv4 first.
        let fields = Fields::from(vec![
            Field::new("a", DataType::UInt32, false),
            Field::new("b", DataType::Binary, false),
        ]);
        let tuple = StructArray::try_new(
            fields,
            vec![
                Arc::new(UInt32Array::from(vec![16_909_060])),
                Arc::new(BinaryArray::from(vec![&b"s"[..]])),
            ],
            None,
        )
        .unwrap();
        let converted = convert_one(Arc::new(tuple), "Tuple(a IPv4, b String)");
        let tuple = converted.column(0).as_struct();
        assert_eq!(tuple.column(0).as_string::<i32>().value(0), "1.2.3.4");
        assert_eq!(tuple.column(1).as_string::<i32>().value(0), "s");
        assert_eq!(tuple.fields()[0].name(), "a");
    }

    #[test]
    fn maps_convert_keys_and_values() {
        use arrow_array::builder::{MapBuilder, StringBuilder as Utf8Builder, UInt32Builder};
        let mut builder = MapBuilder::new(None, Utf8Builder::new(), UInt32Builder::new());
        builder.keys().append_value("k");
        builder.values().append_value(16_909_060);
        builder.append(true).unwrap();
        let converted = convert_one(Arc::new(builder.finish()), "Map(String, IPv4)");
        let map = converted.column(0).as_map();
        assert_eq!(map.values().as_string::<i32>().value(0), "1.2.3.4");
        assert_eq!(map.keys().as_string::<i32>().value(0), "k");
    }

    #[test]
    fn only_ambiguous_arrow_types_ask_for_describe() {
        let schema = |fields: Vec<Field>| Schema::new(fields);
        let plain = schema(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Utf8, true),
            Field::new("c", DataType::Binary, true),
            Field::new("d", DataType::Date32, true),
            Field::new("e", DataType::UInt16, true),
        ]);
        assert!(!needs_types(&plain, false));
        // A server that may write dates as UInt16 / Int32 has to say which is which.
        assert!(needs_types(&plain, true));
        for ambiguous in [
            DataType::UInt32,
            DataType::Int8,
            DataType::Int16,
            DataType::FixedSizeBinary(16),
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            DataType::Timestamp(TimeUnit::Second, None),
        ] {
            assert!(needs_types(
                &schema(vec![Field::new("a", ambiguous, false)]),
                false
            ));
        }
        let nested = DataType::List(Arc::new(Field::new("item", DataType::UInt32, false)));
        assert!(needs_types(
            &schema(vec![Field::new("a", nested, false)]),
            false
        ));
        let variant = DataType::Union(
            arrow_schema::UnionFields::new([0], [Field::new("UInt8", DataType::UInt8, false)]),
            arrow_schema::UnionMode::Dense,
        );
        let variants = schema(vec![Field::new("v", variant, false)]);
        assert!(needs_types(&variants, false) && has_union(&variants));
        assert!(!has_union(&plain));
    }

    #[test]
    fn types_arrow_cannot_carry_are_wrapped_in_the_servers_text() {
        let rows: Vec<Vec<serde_json::Value>> = [
            ("id", "Int32"),
            ("j", "JSON"),
            ("a b", "AggregateFunction(sum, UInt64)"),
            ("n", "Nullable(Nothing)"),
            ("t", "Array(Tuple(x Int8, y Dynamic))"),
            ("ok", "Map(String, Array(Nullable(Decimal(9, 2))))"),
        ]
        .iter()
        .map(|(name, ty)| vec![(*name).into(), (*ty).into(), "".into()])
        .collect();
        let described = Described::from_rows(&rows).unwrap();
        let (sql, wrapped) = described.wrap_for_text("SELECT 1 -- c").unwrap();
        assert_eq!(wrapped, [1, 2, 3, 4]);
        assert_eq!(
            sql,
            "SELECT `id`, toString(`j`) AS `j`, concat('0x', hex(`a b`)) AS `a b`, \
             CAST(`n` AS Nullable(String)) AS `n`, toString(`t`) AS `t`, `ok` FROM (\n\
             SELECT 1 -- c\n)"
        );
        // Nothing to wrap, or names that cannot be told apart: no second statement.
        let plain = Described::from_rows(&[vec!["a".into(), "Int32".into()]]).unwrap();
        assert_eq!(plain.wrap_for_text("SELECT 1"), None);
        let twins = Described::from_rows(&[
            vec!["a".into(), "JSON".into()],
            vec!["a".into(), "Int32".into()],
        ])
        .unwrap();
        assert_eq!(twins.wrap_for_text("SELECT 1"), None);
        assert_eq!(Described::from_rows(&[vec!["a".into(), 5.into()]]), None);
    }

    #[test]
    fn labels_for_unknown_types_come_from_arrow() {
        assert_eq!(arrow_label(&DataType::UInt64), "uint64");
        assert_eq!(arrow_label(&DataType::Binary), "string");
        assert_eq!(
            arrow_label(&DataType::Timestamp(TimeUnit::Millisecond, None)),
            "datetime"
        );
    }
}
