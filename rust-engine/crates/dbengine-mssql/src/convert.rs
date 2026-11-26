//! TDS values -> Arrow columns, with no per-cell allocation of our own.
//!
//! One Arrow builder per result column. The column set is the widened one the other SQL drivers
//! use: every integer is `Int64`, `real` is `Float32`, `float` `Float64`, `bit` `Boolean`,
//! `binary`/`varbinary`/`image`/`rowversion` and CLR types (`geometry`, `hierarchyid`, ...)
//! `Binary`, and everything else `Utf8` in the text form SQL Server itself prints, so the grid
//! shows and an edit writes back the same digits:
//!
//! * `decimal`/`numeric` and `money`/`smallmoney` are exact text (`-12.50`, never a float;
//!   money has four decimals; tiberius is patched to decode money exactly);
//! * `date`, `time(n)`, `datetime2(n)`, `datetimeoffset(n)`, `datetime`, `smalldatetime` are
//!   `YYYY-MM-DD hh:mm:ss[.fff]`, a `datetimeoffset` in its own offset (`+hh:mm`), with as many
//!   fraction digits as the column's scale;
//! * `uniqueidentifier` is upper-case and hyphenated, as SQL Server prints it;
//! * `xml`, `text`, `ntext` and the character types are the text as sent (never repaired).
//!
//! The SQL Server type name rides along as the column's native type (page column `type` and the
//! Arrow field metadata [`NATIVE_TYPE_KEY`]).

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Float32Builder, Float64Builder, Int64Builder, StringBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use engine_driver_kit::api::PageColumn;
use engine_driver_kit::{batch_with_row_count, BackendError};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use tiberius::numeric::Numeric;
use tiberius::time::{DateTime, DateTime2, DateTimeOffset, SmallDateTime};
use tiberius::{Column, ColumnData, ColumnType};

/// Arrow field metadata key carrying the SQL Server type name.
pub(crate) const NATIVE_TYPE_KEY: &str = "tablex.native_type";
/// No text or binary column of a page may hold more than this; `i32` offsets address 2 GiB.
const COLUMN_BYTES_LIMIT: usize = 1 << 30;
const INITIAL_ROWS: usize = 256;

/// The type name SQL Server shows for a column of this wire type.
pub(crate) fn type_label(column_type: ColumnType) -> &'static str {
    match column_type {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 | ColumnType::Intn => "bigint",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

enum Sink {
    Int(Int64Builder),
    F32(Float32Builder),
    F64(Float64Builder),
    Bool(BooleanBuilder),
    Text(StringBuilder),
    Bytes(BinaryBuilder),
}

impl Sink {
    fn new(column_type: ColumnType, capacity: usize) -> Self {
        match column_type {
            ColumnType::Bit | ColumnType::Bitn => Self::Bool(BooleanBuilder::with_capacity(capacity)),
            ColumnType::Int1
            | ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Intn => Self::Int(Int64Builder::with_capacity(capacity)),
            ColumnType::Float4 => Self::F32(Float32Builder::with_capacity(capacity)),
            ColumnType::Float8 | ColumnType::Floatn => {
                Self::F64(Float64Builder::with_capacity(capacity))
            }
            ColumnType::BigVarBin
            | ColumnType::BigBinary
            | ColumnType::Image
            | ColumnType::Udt => Self::Bytes(BinaryBuilder::with_capacity(capacity, capacity * 16)),
            _ => Self::Text(StringBuilder::with_capacity(capacity, capacity * 16)),
        }
    }

    fn data_type(&self) -> DataType {
        match self {
            Self::Int(_) => DataType::Int64,
            Self::F32(_) => DataType::Float32,
            Self::F64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Boolean,
            Self::Text(_) => DataType::Utf8,
            Self::Bytes(_) => DataType::Binary,
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Int(b) => Arc::new(b.finish()),
            Self::F32(b) => Arc::new(b.finish()),
            Self::F64(b) => Arc::new(b.finish()),
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Text(b) => Arc::new(b.finish()),
            Self::Bytes(b) => Arc::new(b.finish()),
        }
    }

    fn data_bytes(&self) -> usize {
        match self {
            Self::Text(b) => b.values_slice().len(),
            Self::Bytes(b) => b.values_slice().len(),
            _ => 0,
        }
    }

    /// Appends one cell. A value of another shape than the column's wire type is a protocol
    /// surprise and fails the page instead of being guessed at.
    fn push(&mut self, cell: &ColumnData<'_>) -> Result<(), ()> {
        use ColumnData as C;
        match (&mut *self, cell) {
            (Self::Int(b), C::U8(v)) => b.append_option(v.map(i64::from)),
            (Self::Int(b), C::I16(v)) => b.append_option(v.map(i64::from)),
            (Self::Int(b), C::I32(v)) => b.append_option(v.map(i64::from)),
            (Self::Int(b), C::I64(v)) => b.append_option(*v),
            (Self::F32(b), C::F32(v)) => b.append_option(*v),
            (Self::F64(b), C::F64(v)) => b.append_option(*v),
            (Self::F64(b), C::F32(v)) => b.append_option(v.map(f64::from)),
            (Self::Bool(b), C::Bit(v)) => b.append_option(*v),
            (Self::Bytes(b), C::Binary(v)) => match v {
                Some(bytes) => b.append_value(bytes),
                None => b.append_null(),
            },
            (Self::Text(b), cell) => {
                if !write_text(b, cell)? {
                    b.append_null();
                }
            }
            _ => return Err(()),
        }
        Ok(())
    }
}

/// Writes `cell` as text; `false` (nothing written) for a NULL.
fn write_text(out: &mut StringBuilder, cell: &ColumnData<'_>) -> Result<bool, ()> {
    use ColumnData as C;
    match cell {
        C::String(Some(text)) => out.append_value(text),
        C::Xml(Some(xml)) => out.append_value(xml.as_ref().as_ref()),
        C::Guid(Some(guid)) => {
            let mut buffer = [0u8; uuid::fmt::Hyphenated::LENGTH];
            out.append_value(guid.hyphenated().encode_upper(&mut buffer));
        }
        C::Numeric(Some(number)) => {
            write_numeric(out, *number);
            out.append_value("");
        }
        C::DateTime(Some(value)) => {
            write_datetime(out, *value);
            out.append_value("");
        }
        C::SmallDateTime(Some(value)) => {
            write_smalldatetime(out, *value);
            out.append_value("");
        }
        C::Date(Some(value)) => {
            write_date(out, i64::from(value.days()));
            out.append_value("");
        }
        C::Time(Some(value)) => {
            write_time(out, value.increments(), value.scale(), 0);
            out.append_value("");
        }
        C::DateTime2(Some(value)) => {
            write_datetime2(out, *value, 0);
            out.append_value("");
        }
        C::DateTimeOffset(Some(value)) => {
            write_datetimeoffset(out, *value);
            out.append_value("");
        }
        C::U8(Some(v)) => out.append_value(v.to_string()),
        C::I16(Some(v)) => out.append_value(v.to_string()),
        C::I32(Some(v)) => out.append_value(v.to_string()),
        C::I64(Some(v)) => out.append_value(v.to_string()),
        C::Bit(Some(v)) => out.append_value(if *v { "1" } else { "0" }),
        C::F32(Some(v)) => out.append_value(v.to_string()),
        C::F64(Some(v)) => out.append_value(v.to_string()),
        C::String(None)
        | C::Xml(None)
        | C::Guid(None)
        | C::Numeric(None)
        | C::F64(None)
        | C::DateTime(None)
        | C::SmallDateTime(None)
        | C::Date(None)
        | C::Time(None)
        | C::DateTime2(None)
        | C::DateTimeOffset(None)
        | C::U8(None)
        | C::I16(None)
        | C::I32(None)
        | C::I64(None)
        | C::Bit(None)
        | C::F32(None)
        | C::Binary(None) => return Ok(false),
        C::Binary(Some(_)) => return Err(()),
    }
    Ok(true)
}

/// `decimal` text with the value's own scale (`1.50`, `-0.05`, `7`).
pub(crate) fn write_numeric(out: &mut impl Write, number: Numeric) {
    let (value, scale) = (number.value(), u32::from(number.scale()));
    let magnitude = value.unsigned_abs();
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        let _ = write!(out, "{sign}{magnitude}");
    } else {
        let divisor = 10u128.pow(scale);
        let _ = write!(
            out,
            "{sign}{}.{:0>width$}",
            magnitude / divisor,
            magnitude % divisor,
            width = scale as usize
        );
    }
}

/// Days since 1970-01-01 -> `(year, month, day)` (proleptic Gregorian; Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Days from 0001-01-01 to 1970-01-01.
const DAYS_0001_TO_1970: i64 = 719_162;
/// Days from 1900-01-01 to 1970-01-01.
const DAYS_1900_TO_1970: i64 = 25_567;

/// `date`: days since 0001-01-01.
fn write_date(out: &mut impl Write, days_since_year_one: i64) {
    let (year, month, day) = civil_from_days(days_since_year_one - DAYS_0001_TO_1970);
    let _ = write!(out, "{year:04}-{month:02}-{day:02}");
}

fn write_clock(out: &mut impl Write, seconds_of_day: u64) {
    let _ = write!(
        out,
        "{:02}:{:02}:{:02}",
        seconds_of_day / 3600,
        seconds_of_day / 60 % 60,
        seconds_of_day % 60
    );
}

/// `time(scale)`: `increments` of 10^-scale seconds since midnight, shifted by `shift_minutes`
/// (a `datetimeoffset` shows its local time). Returns the day carry (-1, 0 or 1).
fn write_time(out: &mut impl Write, increments: u64, scale: u8, shift_minutes: i64) -> i64 {
    let scale = u32::from(scale).min(9);
    let per_second = 10u64.pow(scale);
    let fraction = increments % per_second;
    let mut seconds = (increments / per_second) as i64 + shift_minutes * 60;
    let carry = seconds.div_euclid(86_400);
    seconds = seconds.rem_euclid(86_400);
    write_clock(out, seconds as u64);
    if scale > 0 {
        let _ = write!(out, ".{fraction:0>width$}", width = scale as usize);
    }
    carry
}

fn write_datetime2(out: &mut impl Write, value: DateTime2, shift_minutes: i64) -> i64 {
    // The clock is written first into a scratch string only when a day carry is possible.
    if shift_minutes == 0 {
        write_date(out, i64::from(value.date().days()));
        let _ = out.write_char(' ');
        let time = value.time();
        return write_time(out, time.increments(), time.scale(), 0);
    }
    let time = value.time();
    let mut clock = String::with_capacity(16);
    let carry = write_time(&mut clock, time.increments(), time.scale(), shift_minutes);
    write_date(out, i64::from(value.date().days()) + carry);
    let _ = out.write_char(' ');
    let _ = out.write_str(&clock);
    carry
}

fn write_datetimeoffset(out: &mut impl Write, value: DateTimeOffset) {
    let offset = i64::from(value.offset());
    write_datetime2(out, value.datetime2(), offset);
    let sign = if offset < 0 { '-' } else { '+' };
    let _ = write!(out, " {sign}{:02}:{:02}", offset.abs() / 60, offset.abs() % 60);
}

fn write_datetime(out: &mut impl Write, value: DateTime) {
    let fragments = u64::from(value.seconds_fragments());
    write_date(out, i64::from(value.days()) + DAYS_0001_TO_1970 - DAYS_1900_TO_1970);
    let _ = out.write_char(' ');
    write_clock(out, fragments / 300);
    // One fragment is 1/300 s: SQL Server prints .000, .003, .007, .010 ...
    let _ = write!(out, ".{:03}", (fragments % 300 * 10 + 1) / 3);
}

fn write_smalldatetime(out: &mut impl Write, value: SmallDateTime) {
    write_date(
        out,
        i64::from(value.days()) + DAYS_0001_TO_1970 - DAYS_1900_TO_1970,
    );
    let minutes = u64::from(value.seconds_fragments());
    let _ = write!(out, " {:02}:{:02}:00", minutes / 60, minutes % 60);
}

/// The text form of one value, for the places that need a `String` (row inspection tests).
#[cfg(test)]
fn text_of(cell: &ColumnData<'_>) -> Option<String> {
    let mut builder = StringBuilder::new();
    if write_text(&mut builder, cell).unwrap() {
        let array = builder.finish();
        Some(array.value(0).to_owned())
    } else {
        None
    }
}

/// One result set being collected into Arrow.
pub(crate) struct Collector {
    names: Vec<String>,
    labels: Vec<&'static str>,
    sinks: Vec<Sink>,
    rows: usize,
}

impl Collector {
    pub(crate) fn new(columns: &[Column], expected_rows: u64) -> Self {
        let capacity = usize::try_from(expected_rows).map_or(INITIAL_ROWS, |rows| rows.min(4096));
        Self {
            names: columns
                .iter()
                .map(|column| {
                    if column.name().is_empty() {
                        "(No column name)".to_owned()
                    } else {
                        column.name().to_owned()
                    }
                })
                .collect(),
            labels: columns.iter().map(|column| type_label(column.column_type())).collect(),
            sinks: columns
                .iter()
                .map(|column| Sink::new(column.column_type(), capacity))
                .collect(),
            rows: 0,
        }
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    /// Appends one row (a cell per column).
    pub(crate) fn push_row<'a>(
        &mut self,
        cells: impl Iterator<Item = &'a ColumnData<'static>>,
    ) -> Result<(), BackendError> {
        let mut seen = 0;
        for (index, cell) in cells.enumerate() {
            let sink = self.sinks.get_mut(index).ok_or_else(|| {
                BackendError::new("the server sent more cells than the result set has columns")
            })?;
            sink.push(cell).map_err(|()| {
                BackendError::new(format!(
                    "column \"{}\" ({}): the server sent a value this driver cannot read as that type",
                    self.names[index], self.labels[index]
                ))
            })?;
            if sink.data_bytes() > COLUMN_BYTES_LIMIT {
                return Err(BackendError::with_code(
                    "query.too_large",
                    format!(
                        "column \"{}\" holds more than 1 GiB in this page; lower the row limit",
                        self.names[index]
                    ),
                ));
            }
            seen += 1;
        }
        if seen != self.sinks.len() {
            return Err(BackendError::new("the server sent fewer cells than the result set has columns"));
        }
        self.rows += 1;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(RecordBatch, Vec<PageColumn>), BackendError> {
        let fields: Vec<Field> = self
            .sinks
            .iter()
            .zip(&self.names)
            .zip(&self.labels)
            .map(|((sink, name), label)| {
                Field::new(name, sink.data_type(), true)
                    .with_metadata(HashMap::from([(NATIVE_TYPE_KEY.to_owned(), (*label).to_owned())]))
            })
            .collect();
        let columns: Vec<PageColumn> = self
            .names
            .iter()
            .zip(&self.labels)
            .enumerate()
            .map(|(index, (name, label))| PageColumn {
                id: index.to_string(),
                name: name.clone(),
                data_type: (*label).to_owned(),
                nullable: true,
            })
            .collect();
        let arrays: Vec<ArrayRef> = self.sinks.iter_mut().map(Sink::finish).collect();
        let batch = batch_with_row_count(Arc::new(Schema::new(fields)), arrays, self.rows)?;
        Ok((batch, columns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiberius::time::{Date, Time};

    #[test]
    fn dates_print_as_the_server_prints_them() {
        // 2020-01-02 is 737,426 days after 0001-01-01.
        assert_eq!(
            text_of(&ColumnData::Date(Some(Date::new(737_425)))).as_deref(),
            Some("2020-01-02")
        );
        assert_eq!(
            text_of(&ColumnData::Date(Some(Date::new(0)))).as_deref(),
            Some("0001-01-01")
        );
        assert_eq!(
            text_of(&ColumnData::Date(Some(Date::new(3_652_058)))).as_deref(),
            Some("9999-12-31")
        );
        // The Gregorian leap day of 2000.
        assert_eq!(
            text_of(&ColumnData::Date(Some(Date::new(730_178)))).as_deref(),
            Some("2000-02-29")
        );
    }

    #[test]
    fn time_keeps_the_scale_of_its_column() {
        // 03:04:05.1234567 at scale 7.
        let increments = (3 * 3600 + 4 * 60 + 5) * 10_000_000 + 1_234_567;
        assert_eq!(
            text_of(&ColumnData::Time(Some(Time::new(increments, 7)))).as_deref(),
            Some("03:04:05.1234567")
        );
        assert_eq!(
            text_of(&ColumnData::Time(Some(Time::new(3 * 3600, 0)))).as_deref(),
            Some("03:00:00")
        );
        assert_eq!(
            text_of(&ColumnData::Time(Some(Time::new(5, 3)))).as_deref(),
            Some("00:00:00.005")
        );
    }

    #[test]
    fn datetime2_and_datetimeoffset_show_their_own_clock() {
        let date = Date::new(737_425);
        let utc = DateTime2::new(date, Time::new(23 * 3600 * 10 + 5, 1));
        assert_eq!(
            text_of(&ColumnData::DateTime2(Some(utc))).as_deref(),
            Some("2020-01-02 23:00:00.5")
        );
        // 23:00:00.5 UTC at +07:00 is 06:00:00.5 on the next day.
        assert_eq!(
            text_of(&ColumnData::DateTimeOffset(Some(DateTimeOffset::new(utc, 420)))).as_deref(),
            Some("2020-01-03 06:00:00.5 +07:00")
        );
        // ... and at -05:30 it is 17:30:00.5 the same day.
        assert_eq!(
            text_of(&ColumnData::DateTimeOffset(Some(DateTimeOffset::new(utc, -330)))).as_deref(),
            Some("2020-01-02 17:30:00.5 -05:30")
        );
    }

    #[test]
    fn legacy_datetimes_use_their_fixed_precision() {
        // 1900-01-01 plus 3 fragments (3/300 s = .010).
        assert_eq!(
            text_of(&ColumnData::DateTime(Some(DateTime::new(0, 3)))).as_deref(),
            Some("1900-01-01 00:00:00.010")
        );
        assert_eq!(
            text_of(&ColumnData::DateTime(Some(DateTime::new(43_829, 300 * 3661 + 1)))).as_deref(),
            Some("2020-01-01 01:01:01.003")
        );
        assert_eq!(
            text_of(&ColumnData::SmallDateTime(Some(SmallDateTime::new(43_829, 61)))).as_deref(),
            Some("2020-01-01 01:01:00")
        );
    }

    #[test]
    fn decimals_money_and_guids_print_exactly() {
        let numeric = |value, scale| text_of(&ColumnData::Numeric(Some(Numeric::new_with_scale(value, scale))));
        assert_eq!(numeric(150, 2).as_deref(), Some("1.50"));
        assert_eq!(numeric(-5, 2).as_deref(), Some("-0.05"));
        assert_eq!(numeric(7, 0).as_deref(), Some("7"));
        assert_eq!(
            numeric(i128::from(i64::MAX), 4).as_deref(),
            Some("922337203685477.5807")
        );
        // money and smallmoney are numerics of scale 4.
        assert_eq!(numeric(125_000, 4).as_deref(), Some("12.5000"));
        let guid = uuid::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff").unwrap();
        assert_eq!(
            text_of(&ColumnData::Guid(Some(guid))).as_deref(),
            Some("6F9619FF-8B86-D011-B42D-00C04FC964FF")
        );
        assert_eq!(text_of(&ColumnData::String(None)), None);
    }

    #[test]
    fn a_page_keeps_nulls_native_types_and_names() {
        let columns = vec![
            Column::new("id".into(), ColumnType::Int4),
            Column::new("".into(), ColumnType::NVarchar),
            Column::new("blob".into(), ColumnType::BigVarBin),
        ];
        let mut collector = Collector::new(&columns, 10);
        let rows = [
            [
                ColumnData::I32(Some(1)),
                ColumnData::String(Some("a".into())),
                ColumnData::Binary(Some(vec![1u8, 2].into())),
            ],
            [ColumnData::I32(None), ColumnData::String(None), ColumnData::Binary(None)],
        ];
        for row in &rows {
            collector.push_row(row.iter()).unwrap();
        }
        let (batch, page) = collector.finish().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(page[1].name, "(No column name)");
        assert_eq!(page[0].data_type, "int");
        assert_eq!(
            batch.schema().field(2).metadata().get(NATIVE_TYPE_KEY).map(String::as_str),
            Some("varbinary")
        );
        assert_eq!(batch.column(0).null_count(), 1);
        assert_eq!(batch.column(2).data_type(), &DataType::Binary);
    }

    #[test]
    fn a_cell_of_the_wrong_shape_fails_the_page() {
        let columns = vec![Column::new("n".into(), ColumnType::Int4)];
        let mut collector = Collector::new(&columns, 1);
        let error = collector
            .push_row([ColumnData::String(Some("x".into()))].iter())
            .unwrap_err();
        assert!(error.message.contains("cannot read"), "{}", error.message);
    }
}
