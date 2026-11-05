//! PostgreSQL rows -> Arrow columns, with no per-cell allocation.
//!
//! `tokio-postgres` fetches results in the binary format. Each cell is read as
//! a borrowed byte slice ([`Raw`]) and decoded straight into an Arrow builder:
//! integers/floats/booleans into native columns, everything else as UTF-8 text
//! written in place (`StringBuilder` implements `fmt::Write`). `numeric` never
//! goes through `Decimal128` (Arrow's `Display` allocates twice per cell): its
//! wire digits are rendered directly, preserving the column's scale and any
//! precision.
//!
//! Column types are the engine's widened set: every integer is `Int64`, `real`
//! is `Float32`, `double precision` `Float64`, booleans `Boolean`, the rest
//! `Utf8` (a page whose text in one column outgrows `i32` offsets becomes
//! `LargeUtf8` for that column, see [`TextColumn`]). The PostgreSQL type name is
//! kept in the Arrow field metadata ([`NATIVE_TYPE_KEY`]) like the native engine.
//!
//! # One text form per type
//!
//! Whatever the path, a value reads the way `psql` prints it:
//!
//! * binary decoders here (`date`, `time`, `timestamp`, `numeric`, `uuid`,
//!   `json(b)`, `bytea` as `\x` + lowercase hex) reproduce the server's ISO
//!   output byte for byte (BC after the clock, `.25` not `.250`, `infinity`);
//! * text, `json` and `jsonb` are never repaired: a column whose page holds a value
//!   that is not valid UTF-8 (a SQL_ASCII database, a session that changed its
//!   `client_encoding`) is delivered as Arrow `Binary` with the bytes exactly as
//!   sent and the native type `<type> (invalid utf-8)`, which the app keeps
//!   read-only;
//! * every other type (`timestamptz`, `timetz`, `interval`, arrays, ranges, ...)
//!   is printed by the server: the query layer runs the statement over the
//!   simple query protocol, which returns the server's own text for every column
//!   (`timestamptz` in the session's TimeZone with a numeric offset), and
//!   [`retype`] turns the integer/float/boolean columns back into native ones
//!   (`t`/`f` -> `true`/`false`).

use crate::db::postgres_arrow::{pg_native_type_name, PG_NATIVE_TYPE_METADATA_KEY};
use arrow::compute::{cast, concat};
use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Float32Builder, Float64Builder, Int64Builder, StringBuilder,
};
use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use engine_driver_kit::{batch_with_row_count, BackendError};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use tokio_postgres::types::{FromSql, Kind as PgKind, Type};

/// Arrow field metadata key carrying the PostgreSQL type name.
pub(crate) const NATIVE_TYPE_KEY: &str = PG_NATIVE_TYPE_METADATA_KEY;

/// Days from 1970-01-01 to the PostgreSQL epoch 2000-01-01.
const PG_EPOCH_UNIX_DAYS: i64 = 10_957;
const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// A text column is closed and a new chunk started once it holds this many
/// bytes (see [`TextColumn`]): well below the 2 GiB that `i32` offsets address,
/// so the cell that crosses it still fits (PostgreSQL caps a value at 1 GiB; its
/// widest text form, `bytea` as hex, is 2 GiB - 8 bytes).
const TEXT_CHUNK_LIMIT: usize = 1 << 30;

/// Appended to the native type of a text column delivered as bytes.
const INVALID_UTF8_SUFFIX: &str = " (invalid utf-8)";

/// Why a value could not be turned into a cell.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConvertError {
    /// A binary value whose layout is not the type's (`what` names it).
    Malformed(&'static str),
    /// A fixed-width binary value of the wrong width.
    BadWidth {
        type_name: &'static str,
        len: usize,
    },
    UnsupportedJsonb(u8),
    /// Server text that does not read as the column's type.
    BadText {
        type_name: &'static str,
        text: String,
    },
    /// The statement described one number of columns and returned another.
    ColumnCount {
        expected: usize,
        found: usize,
    },
    /// The wire layer failed to hand over a cell.
    Source(String),
    /// Writing into the Arrow buffer failed.
    Write,
    /// The failure of one column.
    Column {
        name: String,
        cause: Box<ConvertError>,
    },
    /// Joining the chunks of an oversized text column failed.
    Arrow(String),
    /// A column of non-UTF-8 text that holds more than a `Binary` array can address.
    TooLarge,
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed {what}"),
            Self::BadWidth { type_name, len } => {
                write!(f, "malformed {type_name} value ({len} bytes)")
            }
            Self::UnsupportedJsonb(version) => write!(f, "unsupported jsonb version {version}"),
            Self::BadText { type_name, text } => {
                write!(f, "the server sent {text:?} for a {type_name} column")
            }
            Self::ColumnCount { expected, found } => {
                write!(f, "expected {expected} columns, the server sent {found}")
            }
            Self::Source(message) => f.write_str(message),
            Self::Write => f.write_str("text buffer write failed"),
            Self::Column { name, cause } => write!(f, "column \"{name}\": {cause}"),
            Self::Arrow(message) => write!(f, "cannot join the text chunks: {message}"),
            Self::TooLarge => f.write_str("a column of non-UTF-8 text is larger than 2 GiB"),
        }
    }
}

impl std::error::Error for ConvertError {}

impl From<std::fmt::Error> for ConvertError {
    fn from(_: std::fmt::Error) -> Self {
        Self::Write
    }
}

/// A borrowed binary cell (`None` = SQL NULL). Accepts every type: decoding is
/// by [`Kind`], chosen once per column.
pub(crate) struct Raw<'a>(pub(crate) Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(None))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

/// How one column is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Int2,
    Int4,
    Int8,
    Oid,
    Float4,
    Float8,
    Bool,
    Text,
    Uuid,
    Numeric,
    Date,
    Time,
    Timestamp,
    Json,
    Jsonb,
    Bytea,
}

impl Kind {
    /// The binary decoder for `ty`, or `None` when the server must print the
    /// column (`timestamptz`, `timetz`, `interval`, arrays, ranges, composites,
    /// `money`, `inet`, extension types, ...): see the module docs.
    pub(crate) fn classify(ty: &Type) -> Option<Self> {
        Some(match *ty {
            Type::INT2 => Self::Int2,
            Type::INT4 => Self::Int4,
            Type::INT8 => Self::Int8,
            Type::OID => Self::Oid,
            Type::FLOAT4 => Self::Float4,
            Type::FLOAT8 => Self::Float8,
            Type::BOOL => Self::Bool,
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => Self::Text,
            Type::UUID => Self::Uuid,
            Type::NUMERIC => Self::Numeric,
            Type::DATE => Self::Date,
            Type::TIME => Self::Time,
            Type::TIMESTAMP => Self::Timestamp,
            Type::JSON => Self::Json,
            Type::JSONB => Self::Jsonb,
            Type::BYTEA => Self::Bytea,
            // Enum labels travel as their UTF-8 text.
            _ if matches!(ty.kind(), PgKind::Enum(_)) => Self::Text,
            _ => return None,
        })
    }

    fn data_type(self) -> DataType {
        match self {
            Self::Int2 | Self::Int4 | Self::Int8 | Self::Oid => DataType::Int64,
            Self::Float4 => DataType::Float32,
            Self::Float8 => DataType::Float64,
            Self::Bool => DataType::Boolean,
            _ => DataType::Utf8,
        }
    }
}

/// What one result column is: how to decode it and what to call it.
#[derive(Debug, Clone)]
pub(crate) struct ColumnPlan {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    /// PostgreSQL type name (`bigint`, `timestamp with time zone`, `integer[]`).
    pub(crate) native_type: String,
}

impl ColumnPlan {
    pub(crate) fn new(name: &str, kind: Kind, ty: &Type) -> Self {
        Self {
            name: name.to_owned(),
            kind,
            native_type: pg_native_type_name(ty),
        }
    }

    fn field(&self, data_type: DataType) -> Field {
        // Text delivered as bytes says so in its type, so nobody mistakes it for text.
        let native_type = if data_type == DataType::Binary {
            format!("{}{INVALID_UTF8_SUFFIX}", self.native_type)
        } else {
            self.native_type.clone()
        };
        Field::new(&self.name, data_type, true)
            .with_metadata(HashMap::from([(NATIVE_TYPE_KEY.to_owned(), native_type)]))
    }
}

/// One text column. Arrow's `Utf8` addresses its bytes with `i32` offsets, so a
/// page whose text in a single column passes 2 GiB would overflow them (the
/// builder panics). The column therefore closes its builder every
/// [`TEXT_CHUNK_LIMIT`] bytes and, only when that happened, joins the chunks as
/// one `LargeUtf8` array at the end: an ordinary page stays `Utf8` and pays one
/// comparison per cell.
struct TextColumn {
    open: StringBuilder,
    /// Closed chunks of an oversized column (empty for a normal one).
    sealed: Vec<StringArray>,
    chunk_limit: usize,
    /// Set once a value was not valid UTF-8: the whole column is bytes from then on
    /// (everything read so far is copied in, this and later values as sent).
    bytes: Option<BinaryBuilder>,
}

impl TextColumn {
    fn new(chunk_limit: usize) -> Self {
        Self {
            open: StringBuilder::new(),
            sealed: Vec::new(),
            chunk_limit,
            bytes: None,
        }
    }

    fn push_null(&mut self) {
        match &mut self.bytes {
            Some(bytes) => bytes.append_null(),
            None => self.open.append_null(),
        }
    }

    /// One cell of a text-like type (`text`, `json`, `jsonb`): valid UTF-8 is text,
    /// anything else turns the column into bytes, kept exactly as sent.
    fn push_text_bytes(&mut self, raw: &[u8]) -> Result<(), ConvertError> {
        if self.bytes.is_none() {
            if let Ok(text) = std::str::from_utf8(raw) {
                self.push_str(Some(text));
                return Ok(());
            }
            self.turn_into_bytes();
        }
        let bytes = self.bytes.as_mut().expect("just set");
        // `Binary` addresses its bytes with `i32` offsets.
        if bytes.values_slice().len().saturating_add(raw.len()) > i32::MAX as usize {
            return Err(ConvertError::TooLarge);
        }
        bytes.append_value(raw);
        Ok(())
    }

    /// Copies what was read so far into a byte column.
    fn turn_into_bytes(&mut self) {
        let mut chunks = std::mem::take(&mut self.sealed);
        chunks.push(self.open.finish());
        let total: usize = chunks.iter().map(|chunk| chunk.value_data().len()).sum();
        let rows: usize = chunks.iter().map(Array::len).sum();
        let mut bytes = BinaryBuilder::with_capacity(rows + 1, total);
        for chunk in &chunks {
            for value in chunk.iter() {
                bytes.append_option(value.map(str::as_bytes));
            }
        }
        self.bytes = Some(bytes);
    }

    /// The builder to write the next cell into; `extra` is an upper bound of
    /// that cell's bytes (0 for types whose text is a few hundred bytes at most).
    fn begin(&mut self, extra: usize) -> &mut StringBuilder {
        let used = self.open.values_slice().len();
        if used > 0 && used.saturating_add(extra) > self.chunk_limit {
            self.sealed.push(self.open.finish());
        }
        &mut self.open
    }

    fn push_str(&mut self, cell: Option<&str>) {
        match (cell, &mut self.bytes) {
            (None, _) => self.push_null(),
            (Some(text), Some(bytes)) => bytes.append_value(text.as_bytes()),
            (Some(text), None) => {
                self.begin(text.len());
                self.open.append_value(text);
            }
        }
    }

    fn finish(mut self) -> Result<ArrayRef, ConvertError> {
        if let Some(mut bytes) = self.bytes.take() {
            return Ok(Arc::new(bytes.finish()));
        }
        if self.sealed.is_empty() {
            return Ok(Arc::new(self.open.finish()));
        }
        self.sealed.push(self.open.finish());
        let wide = self
            .sealed
            .iter()
            .map(|chunk| cast(chunk, &DataType::LargeUtf8))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ConvertError::Arrow(error.to_string()))?;
        let parts: Vec<&dyn Array> = wide.iter().map(|chunk| chunk.as_ref()).collect();
        concat(&parts).map_err(|error| ConvertError::Arrow(error.to_string()))
    }
}

enum ColumnBuilder {
    Int(Int64Builder),
    Float32(Float32Builder),
    Float(Float64Builder),
    Bool(BooleanBuilder),
    Text(TextColumn),
}

/// Accumulates rows column-wise into one [`RecordBatch`].
pub(crate) struct BatchBuilder {
    plans: Vec<ColumnPlan>,
    builders: Vec<ColumnBuilder>,
    rows: usize,
}

impl BatchBuilder {
    pub(crate) fn new(plans: Vec<ColumnPlan>) -> Self {
        Self::with_chunk_limit(plans, TEXT_CHUNK_LIMIT)
    }

    fn with_chunk_limit(plans: Vec<ColumnPlan>, chunk_limit: usize) -> Self {
        let builders = plans
            .iter()
            .map(|plan| match plan.kind.data_type() {
                DataType::Int64 => ColumnBuilder::Int(Int64Builder::new()),
                DataType::Float32 => ColumnBuilder::Float32(Float32Builder::new()),
                DataType::Float64 => ColumnBuilder::Float(Float64Builder::new()),
                DataType::Boolean => ColumnBuilder::Bool(BooleanBuilder::new()),
                _ => ColumnBuilder::Text(TextColumn::new(chunk_limit)),
            })
            .collect();
        Self {
            plans,
            builders,
            rows: 0,
        }
    }

    /// A builder of plain text columns named `names` (what the simple query
    /// protocol returns; see [`retype`]).
    pub(crate) fn text_columns(names: &[String]) -> Self {
        Self::new(
            names
                .iter()
                .map(|name| ColumnPlan::new(name, Kind::Text, &Type::TEXT))
                .collect(),
        )
    }

    pub(crate) fn row_count(&self) -> usize {
        self.rows
    }

    /// Appends one row (`cell(i)` is column `i`'s raw bytes or `None` for NULL).
    pub(crate) fn push_row<'a>(
        &mut self,
        mut cell: impl FnMut(usize) -> Result<Option<&'a [u8]>, ConvertError>,
    ) -> Result<(), ConvertError> {
        for (index, (plan, builder)) in self.plans.iter().zip(&mut self.builders).enumerate() {
            append(builder, plan.kind, cell(index)?).map_err(|cause| ConvertError::Column {
                name: plan.name.clone(),
                cause: Box::new(cause),
            })?;
        }
        self.rows += 1;
        Ok(())
    }

    /// Appends one row of already-textual cells (the simple query protocol).
    pub(crate) fn push_text_row<'a>(&mut self, cells: impl Iterator<Item = Option<&'a str>>) {
        for (builder, cell) in self.builders.iter_mut().zip(cells) {
            if let ColumnBuilder::Text(column) = builder {
                column.push_str(cell);
            }
        }
        self.rows += 1;
    }

    pub(crate) fn finish(self) -> Result<RecordBatch, BackendError> {
        let arrays = self
            .builders
            .into_iter()
            .map(|builder| match builder {
                ColumnBuilder::Int(mut builder) => Ok(Arc::new(builder.finish()) as ArrayRef),
                ColumnBuilder::Float32(mut builder) => Ok(Arc::new(builder.finish()) as ArrayRef),
                ColumnBuilder::Float(mut builder) => Ok(Arc::new(builder.finish()) as ArrayRef),
                ColumnBuilder::Bool(mut builder) => Ok(Arc::new(builder.finish()) as ArrayRef),
                ColumnBuilder::Text(column) => column.finish(),
            })
            .collect::<Result<Vec<ArrayRef>, ConvertError>>()
            .map_err(|error| BackendError::new(error.to_string()))?;
        let fields: Vec<Field> = self
            .plans
            .iter()
            .zip(&arrays)
            .map(|(plan, array)| plan.field(array.data_type().clone()))
            .collect();
        batch_with_row_count(Arc::new(Schema::new(fields)), arrays, self.rows)
    }
}

/// Turns a batch of plain text columns (the simple query protocol prints every
/// value as the server does) into the columns `plans` describe: integer, float
/// and boolean columns become native, everything else stays the server's text.
/// The field metadata takes the plans' PostgreSQL type names.
pub(crate) fn retype(
    batch: &RecordBatch,
    plans: &[ColumnPlan],
) -> Result<RecordBatch, BackendError> {
    let convert = || -> Result<(Vec<Field>, Vec<ArrayRef>), ConvertError> {
        if batch.num_columns() != plans.len() {
            return Err(ConvertError::ColumnCount {
                expected: plans.len(),
                found: batch.num_columns(),
            });
        }
        let mut fields = Vec::with_capacity(plans.len());
        let mut arrays = Vec::with_capacity(plans.len());
        for (column, plan) in batch.columns().iter().zip(plans) {
            let array = match plan.kind.data_type() {
                DataType::Utf8 => Arc::clone(column),
                other => parse_column(column, plan.kind, &other).map_err(|cause| {
                    ConvertError::Column {
                        name: plan.name.clone(),
                        cause: Box::new(cause),
                    }
                })?,
            };
            fields.push(plan.field(array.data_type().clone()));
            arrays.push(array);
        }
        Ok((fields, arrays))
    };
    let (fields, arrays) = convert().map_err(|error| BackendError::new(error.to_string()))?;
    batch_with_row_count(Arc::new(Schema::new(fields)), arrays, batch.num_rows())
}

/// Parses a text column the server printed into the native column of `kind`.
fn parse_column(text: &ArrayRef, kind: Kind, target: &DataType) -> Result<ArrayRef, ConvertError> {
    let strings = text
        .as_string_opt::<i32>()
        .ok_or(ConvertError::Malformed("text column"))?;
    let values = strings.iter();
    let bad = |type_name: &'static str, text: &str| ConvertError::BadText {
        type_name,
        text: text.chars().take(40).collect(),
    };
    Ok(match target {
        DataType::Int64 => {
            let mut builder = Int64Builder::with_capacity(strings.len());
            for value in values {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(if kind == Kind::Oid {
                        text.parse::<u32>()
                            .map(i64::from)
                            .map_err(|_| bad("oid", text))?
                    } else {
                        text.parse::<i64>().map_err(|_| bad("integer", text))?
                    }),
                }
            }
            Arc::new(builder.finish())
        }
        DataType::Float32 => {
            let mut builder = Float32Builder::with_capacity(strings.len());
            for value in values {
                match value {
                    None => builder.append_null(),
                    // "NaN", "Infinity" and "-Infinity" are what the server prints
                    // and what Rust reads.
                    Some(text) => {
                        builder.append_value(text.parse::<f32>().map_err(|_| bad("real", text))?)
                    }
                }
            }
            Arc::new(builder.finish())
        }
        DataType::Float64 => {
            let mut builder = Float64Builder::with_capacity(strings.len());
            for value in values {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        text.parse::<f64>()
                            .map_err(|_| bad("double precision", text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        _ => {
            let mut builder = BooleanBuilder::with_capacity(strings.len());
            for value in values {
                match value {
                    None => builder.append_null(),
                    Some("t") => builder.append_value(true),
                    Some("f") => builder.append_value(false),
                    Some(text) => return Err(bad("boolean", text)),
                }
            }
            Arc::new(builder.finish())
        }
    })
}

fn append(
    builder: &mut ColumnBuilder,
    kind: Kind,
    cell: Option<&[u8]>,
) -> Result<(), ConvertError> {
    match builder {
        ColumnBuilder::Int(builder) => match cell {
            None => builder.append_null(),
            Some(raw) => builder.append_value(match kind {
                Kind::Int2 => i64::from(i16::from_be_bytes(fixed(raw, "smallint")?)),
                Kind::Int4 => i64::from(i32::from_be_bytes(fixed(raw, "integer")?)),
                Kind::Int8 => i64::from_be_bytes(fixed(raw, "bigint")?),
                _ => i64::from(u32::from_be_bytes(fixed(raw, "oid")?)),
            }),
        },
        ColumnBuilder::Float32(builder) => match cell {
            None => builder.append_null(),
            Some(raw) => builder.append_value(f32::from_be_bytes(fixed(raw, "real")?)),
        },
        ColumnBuilder::Float(builder) => match cell {
            None => builder.append_null(),
            Some(raw) => builder.append_value(f64::from_be_bytes(fixed(raw, "double precision")?)),
        },
        ColumnBuilder::Bool(builder) => match cell {
            None => builder.append_null(),
            Some(raw) => builder.append_value(
                *raw.first()
                    .ok_or(ConvertError::Malformed("boolean value (empty)"))?
                    != 0,
            ),
        },
        ColumnBuilder::Text(column) => match (cell, kind) {
            (None, _) => column.push_null(),
            (Some(raw), Kind::Text | Kind::Json) => column.push_text_bytes(raw)?,
            (Some(raw), Kind::Jsonb) => {
                // jsonb binary = a version byte (1) followed by the JSON text.
                let (version, text) = raw
                    .split_first()
                    .ok_or(ConvertError::Malformed("jsonb value (empty)"))?;
                if *version != 1 {
                    return Err(ConvertError::UnsupportedJsonb(*version));
                }
                column.push_text_bytes(text)?;
            }
            (Some(raw), _) => {
                write_text(column, kind, raw)?;
                // Finishes the row started by `write!` (an empty append).
                column.open.append_value("");
            }
        },
    }
    Ok(())
}

fn fixed<const N: usize>(raw: &[u8], type_name: &'static str) -> Result<[u8; N], ConvertError> {
    raw.try_into().map_err(|_| ConvertError::BadWidth {
        type_name,
        len: raw.len(),
    })
}

/// Writes the text of `raw` (non-null) into the current row of `column`.
fn write_text(column: &mut TextColumn, kind: Kind, raw: &[u8]) -> Result<(), ConvertError> {
    match kind {
        // Text-like kinds are pushed whole by `append` (they may turn into bytes).
        Kind::Text | Kind::Json | Kind::Jsonb => unreachable!("text kinds are pushed whole"),
        Kind::Bytea => write_bytea(column.begin(raw.len().saturating_mul(2) + 2), raw)?,
        Kind::Uuid => write_uuid(column.begin(0), raw)?,
        Kind::Numeric => write_numeric(column.begin(0), raw)?,
        Kind::Date => write_date(column.begin(0), raw)?,
        Kind::Time => write_time(column.begin(0), raw)?,
        Kind::Timestamp => write_timestamp(column.begin(0), raw)?,
        // Handled by the native builders.
        Kind::Int2
        | Kind::Int4
        | Kind::Int8
        | Kind::Oid
        | Kind::Float4
        | Kind::Float8
        | Kind::Bool => unreachable!("numeric kinds use native columns"),
    }
    Ok(())
}

fn write_uuid(out: &mut StringBuilder, raw: &[u8]) -> Result<(), ConvertError> {
    let bytes: [u8; 16] = fixed(raw, "uuid")?;
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.write_char('-')?;
        }
        write!(out, "{byte:02x}")?;
    }
    Ok(())
}

/// `\x` + lowercase hex: PostgreSQL's own `bytea_output = hex` form, which its
/// input function reads back to the same bytes. Never cut: an edit or a clone
/// writes this text back.
fn write_bytea(out: &mut StringBuilder, raw: &[u8]) -> Result<(), ConvertError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    const CHUNK: usize = 512;
    out.write_str("\\x")?;
    let mut hex = [0u8; CHUNK * 2];
    for bytes in raw.chunks(CHUNK) {
        for (pair, byte) in hex.chunks_exact_mut(2).zip(bytes) {
            pair[0] = HEX[usize::from(byte >> 4)];
            pair[1] = HEX[usize::from(byte & 0x0f)];
        }
        out.write_str(
            std::str::from_utf8(&hex[..bytes.len() * 2]).map_err(|_| ConvertError::Write)?,
        )?;
    }
    Ok(())
}

/// A civil date as PostgreSQL prints it: the year is the displayed one (1 BC is
/// year 1 with `bc` set).
struct Civil {
    year: i64,
    month: i64,
    day: i64,
    bc: bool,
}

/// Civil date for `days` since 1970-01-01 (proleptic Gregorian; Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> Civil {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    // Astronomical year: 0 is 1 BC.
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Civil {
        year: if year >= 1 { year } else { 1 - year },
        month,
        day,
        bc: year < 1,
    }
}

impl Civil {
    fn of_pg_days(pg_days: i64) -> Self {
        civil_from_days(pg_days + PG_EPOCH_UNIX_DAYS)
    }

    /// `YYYY-MM-DD` (more year digits above 9999, like the server).
    fn write_ymd(&self, out: &mut StringBuilder) -> std::fmt::Result {
        write!(out, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// The ` BC` the server prints after the date (`date`) or after the clock
    /// (`timestamp`).
    fn write_era(&self, out: &mut StringBuilder) -> std::fmt::Result {
        if self.bc {
            out.write_str(" BC")
        } else {
            Ok(())
        }
    }
}

/// `HH:MM:SS` plus the fraction the way the server prints it: up to six digits
/// with the trailing zeros dropped (`.25`, never `.250`).
fn write_clock(out: &mut StringBuilder, micros_of_day: i64) -> std::fmt::Result {
    let seconds = micros_of_day / MICROS_PER_SECOND;
    let mut micros = micros_of_day % MICROS_PER_SECOND;
    write!(
        out,
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )?;
    if micros == 0 {
        return Ok(());
    }
    let mut digits = [b'0'; 6];
    for digit in digits.iter_mut().rev() {
        *digit = b'0' + (micros % 10) as u8;
        micros /= 10;
    }
    let kept = digits
        .iter()
        .rposition(|digit| *digit != b'0')
        .map_or(0, |at| at + 1);
    out.write_char('.')?;
    out.write_str(std::str::from_utf8(&digits[..kept]).map_err(|_| std::fmt::Error)?)
}

fn write_date(out: &mut StringBuilder, raw: &[u8]) -> Result<(), ConvertError> {
    let days = i32::from_be_bytes(fixed(raw, "date")?);
    match days {
        i32::MAX => out.write_str("infinity")?,
        i32::MIN => out.write_str("-infinity")?,
        _ => {
            let date = Civil::of_pg_days(i64::from(days));
            date.write_ymd(out)?;
            date.write_era(out)?;
        }
    }
    Ok(())
}

fn write_time(out: &mut StringBuilder, raw: &[u8]) -> Result<(), ConvertError> {
    let micros = i64::from_be_bytes(fixed(raw, "time")?);
    Ok(write_clock(out, micros.clamp(0, MICROS_PER_DAY))?)
}

/// `timestamp` (without time zone). `timestamptz` is printed by the server, in
/// the session's TimeZone, so it never reaches this decoder.
fn write_timestamp(out: &mut StringBuilder, raw: &[u8]) -> Result<(), ConvertError> {
    let micros = i64::from_be_bytes(fixed(raw, "timestamp")?);
    match micros {
        i64::MAX => out.write_str("infinity")?,
        i64::MIN => out.write_str("-infinity")?,
        _ => {
            let date = Civil::of_pg_days(micros.div_euclid(MICROS_PER_DAY));
            date.write_ymd(out)?;
            out.write_char(' ')?;
            write_clock(out, micros.rem_euclid(MICROS_PER_DAY))?;
            date.write_era(out)?;
        }
    }
    Ok(())
}

const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;

/// Renders a binary `numeric` exactly as the server's text output does
/// (port of `get_str_from_var`): arbitrary precision, scale preserved.
fn write_numeric(out: &mut StringBuilder, raw: &[u8]) -> Result<(), ConvertError> {
    let header = raw
        .get(..8)
        .ok_or(ConvertError::Malformed("numeric value"))?;
    let word = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    let (ndigits, weight, sign, dscale) = (
        usize::from(word(0)),
        i32::from(word(2) as i16),
        word(4),
        usize::from(word(6)),
    );
    let digit_bytes = raw
        .get(8..8 + ndigits * 2)
        .ok_or(ConvertError::Malformed("numeric digits"))?;
    let digit = |index: i32| -> u16 {
        usize::try_from(index)
            .ok()
            .filter(|index| *index < ndigits)
            .map_or(0, |index| {
                u16::from_be_bytes([digit_bytes[index * 2], digit_bytes[index * 2 + 1]])
            })
    };

    match sign {
        NUMERIC_NAN => return Ok(out.write_str("NaN")?),
        NUMERIC_PINF => return Ok(out.write_str("Infinity")?),
        NUMERIC_NINF => return Ok(out.write_str("-Infinity")?),
        NUMERIC_NEG => out.write_char('-')?,
        _ => {}
    }
    if weight < 0 {
        out.write_char('0')?;
    } else {
        for group in 0..=weight {
            if group == 0 {
                write!(out, "{}", digit(group))?;
            } else {
                write!(out, "{:04}", digit(group))?;
            }
        }
    }
    if dscale > 0 {
        out.write_char('.')?;
        for position in 0..dscale {
            let group = digit(weight + 1 + (position / 4) as i32);
            let divisor = [1000, 100, 10, 1][position % 4];
            out.write_char(char::from(b'0' + ((group / divisor) % 10) as u8))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Float32Type, Float64Type, Int64Type};
    use arrow_array::Array;

    fn numeric(ndigits: u16, weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
        let mut raw = Vec::new();
        for word in [ndigits, weight as u16, sign, dscale] {
            raw.extend_from_slice(&word.to_be_bytes());
        }
        for digit in digits {
            raw.extend_from_slice(&digit.to_be_bytes());
        }
        raw
    }

    /// The text one binary cell decodes to.
    fn decode(kind: Kind, ty: &Type, raw: &[u8]) -> String {
        let mut batch = BatchBuilder::new(vec![ColumnPlan::new("x", kind, ty)]);
        batch.push_row(|_| Ok(Some(raw))).unwrap();
        text_of(batch.finish().unwrap())
    }

    fn render(raw: &[u8]) -> String {
        decode(Kind::Numeric, &Type::NUMERIC, raw)
    }

    fn text_of(batch: RecordBatch) -> String {
        batch.column(0).as_string::<i32>().value(0).to_owned()
    }

    #[test]
    fn numeric_text_matches_the_server_format() {
        // 12345.67 (numeric(10,2)): groups 1 | 2345 | 6700, weight 1.
        assert_eq!(render(&numeric(3, 1, 0, 2, &[1, 2345, 6700])), "12345.67");
        // 42.50 keeps its scale (the native Decimal path printed "42.5").
        assert_eq!(render(&numeric(2, 0, 0, 2, &[42, 5000])), "42.50");
        // 0.000123: fraction groups [0001][2300], weight -1; and 0.00001230 with
        // leading empty groups (weight -2).
        assert_eq!(render(&numeric(2, -1, 0, 6, &[1, 2300])), "0.000123");
        assert_eq!(render(&numeric(1, -2, 0, 8, &[1230])), "0.00001230");
        // -1000000 (weight 1, trailing zero group omitted) and zero.
        assert_eq!(render(&numeric(1, 1, NUMERIC_NEG, 0, &[100])), "-1000000");
        assert_eq!(render(&numeric(0, 0, 0, 0, &[])), "0");
        assert_eq!(render(&numeric(0, 0, 0, 3, &[])), "0.000");
        // 30 digits: beyond rust_decimal's 28-digit limit.
        assert_eq!(
            render(&numeric(
                8,
                7,
                0,
                0,
                &[123, 4567, 8901, 2345, 6789, 123, 4567, 8901]
            )),
            "1234567890123456789012345678901"
        );
        assert_eq!(render(&numeric(0, 0, NUMERIC_NAN, 0, &[])), "NaN");
        assert_eq!(render(&numeric(0, 0, NUMERIC_PINF, 0, &[])), "Infinity");
        assert_eq!(render(&numeric(0, 0, NUMERIC_NINF, 0, &[])), "-Infinity");
    }

    #[test]
    fn malformed_numeric_is_an_error_not_a_panic() {
        let plan = ColumnPlan::new("n", Kind::Numeric, &Type::NUMERIC);
        let mut batch = BatchBuilder::new(vec![plan]);
        assert!(batch.push_row(|_| Ok(Some(&[0u8; 3][..]))).is_err());
        // Header claims two digits, none present.
        let truncated = numeric(2, 0, 0, 0, &[]);
        assert!(batch.push_row(|_| Ok(Some(&truncated[..]))).is_err());
    }

    #[test]
    fn widened_native_columns_keep_nulls_and_values() {
        let plans = vec![
            ColumnPlan::new("a", Kind::Int4, &Type::INT4),
            ColumnPlan::new("b", Kind::Float4, &Type::FLOAT4),
            ColumnPlan::new("c", Kind::Bool, &Type::BOOL),
            ColumnPlan::new("d", Kind::Text, &Type::TEXT),
            ColumnPlan::new("e", Kind::Float8, &Type::FLOAT8),
        ];
        let mut batch = BatchBuilder::new(plans);
        let four = 7i32.to_be_bytes();
        let float = 0.1f32.to_be_bytes();
        let double = 0.1f64.to_be_bytes();
        batch
            .push_row(|index| {
                Ok(match index {
                    0 => Some(&four[..]),
                    1 => Some(&float[..]),
                    2 => Some(&[1u8][..]),
                    3 => Some(&b"hi"[..]),
                    _ => Some(&double[..]),
                })
            })
            .unwrap();
        batch.push_row(|_| Ok(None)).unwrap();
        let batch = batch.finish().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.schema().field(0).data_type(), &DataType::Int64);
        assert_eq!(batch.schema().field(1).data_type(), &DataType::Float32);
        assert_eq!(batch.schema().field(4).data_type(), &DataType::Float64);
        assert_eq!(
            batch
                .schema()
                .field(0)
                .metadata()
                .get(NATIVE_TYPE_KEY)
                .map(String::as_str),
            Some("integer")
        );
        assert_eq!(batch.column(0).as_primitive::<Int64Type>().value(0), 7);
        // `real` keeps its own width: 0.1 is 0.1, not 0.10000000149011612.
        assert_eq!(
            batch.column(1).as_primitive::<Float32Type>().value(0),
            0.1f32
        );
        assert_eq!(
            batch.column(4).as_primitive::<Float64Type>().value(0),
            0.1f64
        );
        assert!(batch.column(2).as_boolean().value(0));
        assert_eq!(batch.column(3).as_string::<i32>().value(0), "hi");
        for column in 0..5 {
            assert!(batch.column(column).is_null(1), "column {column}");
        }
    }

    #[test]
    fn temporal_and_scalar_text_formats() {
        let text = decode;
        // 2024-01-02 is 8767 days after 2000-01-01.
        assert_eq!(
            text(Kind::Date, &Type::DATE, &8767i32.to_be_bytes()),
            "2024-01-02"
        );
        assert_eq!(
            text(Kind::Date, &Type::DATE, &i32::MAX.to_be_bytes()),
            "infinity"
        );
        assert_eq!(
            text(Kind::Date, &Type::DATE, &i32::MIN.to_be_bytes()),
            "-infinity"
        );
        // The server prints `.25`, not `.250`.
        let micros = (8767i64 * 86_400 + 3 * 3600 + 4 * 60 + 5) * 1_000_000 + 250_000;
        assert_eq!(
            text(Kind::Timestamp, &Type::TIMESTAMP, &micros.to_be_bytes()),
            "2024-01-02 03:04:05.25"
        );
        assert_eq!(
            text(Kind::Timestamp, &Type::TIMESTAMP, &i64::MAX.to_be_bytes()),
            "infinity"
        );
        assert_eq!(
            text(Kind::Timestamp, &Type::TIMESTAMP, &i64::MIN.to_be_bytes()),
            "-infinity"
        );
        assert_eq!(
            text(
                Kind::Time,
                &Type::TIME,
                &((3 * 3600 + 4 * 60 + 5) * 1_000_000i64).to_be_bytes()
            ),
            "03:04:05"
        );
        assert_eq!(
            text(Kind::Time, &Type::TIME, &(86_400_000_000i64).to_be_bytes()),
            "24:00:00"
        );
        assert_eq!(
            text(Kind::Time, &Type::TIME, &(45_296_100_000i64).to_be_bytes()),
            "12:34:56.1"
        );
        let uuid = [
            0x12, 0x3e, 0x45, 0x67, 0xe8, 0x9b, 0x12, 0xd3, 0xa4, 0x56, 0x42, 0x66, 0x14, 0x17,
            0x40, 0x00,
        ];
        assert_eq!(
            text(Kind::Uuid, &Type::UUID, &uuid),
            "123e4567-e89b-12d3-a456-426614174000"
        );
        assert_eq!(
            text(Kind::Jsonb, &Type::JSONB, b"\x01{\"a\": 1}"),
            "{\"a\": 1}"
        );
        assert_eq!(text(Kind::Json, &Type::JSON, b"[1, 2]"), "[1, 2]");
    }

    #[test]
    fn bytea_is_the_full_value_in_the_servers_hex_input_form() {
        let bytea = |raw: &[u8]| decode(Kind::Bytea, &Type::BYTEA, raw);
        assert_eq!(bytea(&[]), "\\x");
        assert_eq!(bytea(&[0x00, 0xff]), "\\x00ff");
        // NUL, backslash and bytes that are not UTF-8 all survive.
        assert_eq!(bytea(&[0x00, 0x5c, 0xff, 0xfe, 0x80]), "\\x005cfffe80");
        // Past the old 64-byte preview, and across the hex staging chunk.
        for len in [70usize, 511, 512, 513, 5000] {
            let raw: Vec<u8> = (0..len).map(|at| (at % 251) as u8).collect();
            let text = bytea(&raw);
            assert_eq!(text.len(), 2 + 2 * len, "{len} bytes");
            let back: Vec<u8> = (0..len)
                .map(|at| u8::from_str_radix(&text[2 + 2 * at..4 + 2 * at], 16).unwrap())
                .collect();
            assert_eq!(back, raw, "{len} bytes");
        }
        assert!(!bytea(&[0xab; 70]).contains('.'));
    }

    #[test]
    fn civil_dates_cover_epochs_leap_days_and_bc() {
        let date = |days: i32| decode(Kind::Date, &Type::DATE, &days.to_be_bytes());
        assert_eq!(date(0), "2000-01-01");
        assert_eq!(date(-1), "1999-12-31");
        assert_eq!(date(59), "2000-02-29");
        assert_eq!(date(60), "2000-03-01");
        assert_eq!(date(8825), "2024-02-29");
        assert_eq!(date(-10_957), "1970-01-01");
        // 0001-01-01 is 730119 days before 2000-01-01; the day before is 1 BC.
        assert_eq!(date(-730_119), "0001-01-01");
        assert_eq!(date(-730_120), "0001-12-31 BC");
        assert_eq!(date(2_921_939), "9999-12-31");
        assert_eq!(date(2_921_940), "10000-01-01");
    }

    #[test]
    fn timestamps_before_the_pg_epoch_and_with_odd_fractions() {
        let stamp = |micros: i64| decode(Kind::Timestamp, &Type::TIMESTAMP, &micros.to_be_bytes());
        assert_eq!(stamp(0), "2000-01-01 00:00:00");
        assert_eq!(stamp(-1), "1999-12-31 23:59:59.999999");
        assert_eq!(stamp(1), "2000-01-01 00:00:00.000001");
        // Trailing zeros of the fraction are dropped, interior ones are kept.
        assert_eq!(stamp(1_500_000), "2000-01-01 00:00:01.5");
        assert_eq!(stamp(1_050_000), "2000-01-01 00:00:01.05");
        assert_eq!(stamp(1_000_100), "2000-01-01 00:00:01.0001");
        assert_eq!(stamp(86_400_000_000 - 1), "2000-01-01 23:59:59.999999");
    }

    #[test]
    fn bc_timestamps_put_the_era_after_the_clock_and_big_years_widen() {
        let stamp = |micros: i64| decode(Kind::Timestamp, &Type::TIMESTAMP, &micros.to_be_bytes());
        // One microsecond before 0001-01-01 00:00:00 is 0001-12-31 23:59:59.999999 BC.
        let first = -730_119i64 * MICROS_PER_DAY;
        assert_eq!(stamp(first - 1), "0001-12-31 23:59:59.999999 BC");
        assert_eq!(stamp(first), "0001-01-01 00:00:00");
        assert_eq!(
            stamp(first - MICROS_PER_DAY + 12 * 3_600 * MICROS_PER_SECOND + 250_000),
            "0001-12-31 12:00:00.25 BC"
        );
        // 10000-01-01 00:00:00.5
        assert_eq!(
            stamp(2_921_940 * MICROS_PER_DAY + 500_000),
            "10000-01-01 00:00:00.5"
        );
    }

    /// Server text -> native column, the way the simple query protocol's rows are
    /// read: the same value gives the same cell as the binary decoder.
    #[test]
    fn server_text_retypes_to_the_same_cells_as_the_binary_decoders() {
        let plans = vec![
            ColumnPlan::new("i", Kind::Int8, &Type::INT8),
            ColumnPlan::new("o", Kind::Oid, &Type::OID),
            ColumnPlan::new("r", Kind::Float4, &Type::FLOAT4),
            ColumnPlan::new("d", Kind::Float8, &Type::FLOAT8),
            ColumnPlan::new("b", Kind::Bool, &Type::BOOL),
            ColumnPlan::new("t", Kind::Text, &Type::TEXT),
            ColumnPlan::new("y", Kind::Bytea, &Type::BYTEA),
        ];
        let names: Vec<String> = plans.iter().map(|plan| plan.name.clone()).collect();
        let mut text = BatchBuilder::text_columns(&names);
        for row in [
            [
                "-9223372036854775808",
                "4294967295",
                "0.1",
                "0.1",
                "t",
                "x",
                "\\x00ff",
            ],
            ["7", "1", "NaN", "-Infinity", "f", "", "\\x"],
            ["7", "1", "Infinity", "NaN", "f", "t", "\\x"],
            ["7", "1", "1e-45", "1e+300", "f", "t", "\\x"],
        ] {
            text.push_text_row(row.into_iter().map(Some));
        }
        text.push_text_row(std::iter::repeat_n(None, 7));
        let typed = retype(&text.finish().unwrap(), &plans).unwrap();

        let mut binary = BatchBuilder::new(plans.clone());
        let (i, o, r, d) = (
            i64::MIN.to_be_bytes(),
            u32::MAX.to_be_bytes(),
            0.1f32.to_be_bytes(),
            0.1f64.to_be_bytes(),
        );
        binary
            .push_row(|at| {
                Ok(Some(match at {
                    0 => &i[..],
                    1 => &o[..],
                    2 => &r[..],
                    3 => &d[..],
                    4 => &[1u8][..],
                    5 => b"x",
                    _ => &[0x00, 0xff][..],
                }))
            })
            .unwrap();
        let binary = binary.finish().unwrap();

        for column in 0..plans.len() {
            assert_eq!(typed.schema().field(column), binary.schema().field(column));
            assert_eq!(
                typed.column(column).slice(0, 1).to_data(),
                binary.column(column).to_data(),
                "column {column}"
            );
            assert!(typed.column(column).is_null(4), "column {column}");
        }
        let reals = typed.column(2).as_primitive::<Float32Type>();
        assert!(reals.value(1).is_nan() && reals.value(2).is_infinite());
        assert_eq!(reals.value(3), 1e-45f32);
        let doubles = typed.column(3).as_primitive::<Float64Type>();
        assert_eq!(doubles.value(1), f64::NEG_INFINITY);
        assert_eq!(doubles.value(3), 1e300);
        assert!(!typed.column(4).as_boolean().value(1));
    }

    #[test]
    fn retype_rejects_text_the_type_cannot_hold() {
        let plan = |kind, ty: &Type| vec![ColumnPlan::new("x", kind, ty)];
        for (kind, ty, bad) in [
            (Kind::Int4, Type::INT4, "1.5"),
            (Kind::Bool, Type::BOOL, "true"),
            (Kind::Float8, Type::FLOAT8, "ten"),
        ] {
            let mut text = BatchBuilder::text_columns(&["x".to_owned()]);
            text.push_text_row([Some(bad)].into_iter());
            let error = retype(&text.finish().unwrap(), &plan(kind, &ty)).unwrap_err();
            assert!(error.message.contains(bad), "{}", error.message);
        }
        let text = BatchBuilder::text_columns(&["x".to_owned()])
            .finish()
            .unwrap();
        assert!(retype(&text, &[]).is_err(), "column count mismatch");
    }

    /// A column that outgrows the chunk limit (stand-in for 2 GiB) comes out as one
    /// `LargeUtf8` array with every value and null in place; a smaller one stays
    /// `Utf8`.
    #[test]
    fn a_text_column_past_the_chunk_limit_becomes_large_utf8() {
        let plans = vec![
            ColumnPlan::new("t", Kind::Text, &Type::TEXT),
            ColumnPlan::new("y", Kind::Bytea, &Type::BYTEA),
            ColumnPlan::new("n", Kind::Int4, &Type::INT4),
        ];
        let values = [
            "alpha",
            "",
            "bravo-bravo",
            "charlie",
            "d",
            "echo-echo-echo",
            "f",
        ];
        let build = |limit: usize| {
            let mut batch = BatchBuilder::with_chunk_limit(plans.clone(), limit);
            for (at, value) in values.iter().enumerate() {
                let number = (at as i32).to_be_bytes();
                let raw = [at as u8, 0x5c];
                batch
                    .push_row(|column| {
                        Ok(match column {
                            0 if at == 3 => None,
                            0 => Some(value.as_bytes()),
                            1 => Some(&raw[..]),
                            _ => Some(&number[..]),
                        })
                    })
                    .unwrap();
            }
            batch.finish().unwrap()
        };
        let small = build(16);
        let whole = build(usize::MAX);
        assert_eq!(whole.schema().field(0).data_type(), &DataType::Utf8);
        assert_eq!(small.schema().field(0).data_type(), &DataType::LargeUtf8);
        assert_eq!(small.schema().field(1).data_type(), &DataType::LargeUtf8);
        assert_eq!(small.schema().field(2).data_type(), &DataType::Int64);
        assert_eq!(small.num_rows(), values.len());
        for column in [0, 1] {
            let wide = small.column(column).as_string::<i64>();
            let narrow = whole.column(column).as_string::<i32>();
            assert_eq!(wide.len(), narrow.len());
            for row in 0..narrow.len() {
                assert_eq!(wide.is_null(row), narrow.is_null(row), "{column}/{row}");
                if !narrow.is_null(row) {
                    assert_eq!(wide.value(row), narrow.value(row), "{column}/{row}");
                }
            }
        }
        // The metadata survives the join.
        assert_eq!(
            small
                .schema()
                .field(0)
                .metadata()
                .get(NATIVE_TYPE_KEY)
                .map(String::as_str),
            Some("text")
        );
        // A single cell larger than the limit still fits in its own chunk.
        let mut batch = BatchBuilder::with_chunk_limit(plans[..1].to_vec(), 4);
        batch
            .push_row(|_| Ok(Some(&b"much longer than four"[..])))
            .unwrap();
        batch.push_row(|_| Ok(Some(&b"x"[..]))).unwrap();
        let batch = batch.finish().unwrap();
        assert_eq!(
            batch.column(0).as_string::<i64>().value(0),
            "much longer than four"
        );
        assert_eq!(batch.column(0).as_string::<i64>().value(1), "x");
    }

    /// Text that is not UTF-8 (a SQL_ASCII database) is never repaired or lost: the
    /// column becomes `Binary`, every value exactly as sent, typed
    /// `<type> (invalid utf-8)`; a page without such a value stays text.
    #[test]
    fn a_page_with_non_utf8_text_becomes_a_binary_column_with_the_bytes_as_sent() {
        let plans = vec![
            ColumnPlan::new("t", Kind::Text, &Type::TEXT),
            ColumnPlan::new("j", Kind::Jsonb, &Type::JSONB),
            ColumnPlan::new("n", Kind::Int4, &Type::INT4),
        ];
        let mut batch = BatchBuilder::new(plans.clone());
        let rows: [(Option<&[u8]>, &[u8]); 4] = [
            (Some(b"plain"), b"\x01{}"),
            (None, b"\x01[1]"),
            (Some(b"caf\xe9 \xff\x00"), b"\x01[2]"),
            (Some(b"after"), b"\x01[3]"),
        ];
        for (at, (text, json)) in rows.iter().enumerate() {
            let number = (at as i32).to_be_bytes();
            batch
                .push_row(|column| {
                    Ok(match column {
                        0 => *text,
                        1 => Some(*json),
                        _ => Some(&number[..]),
                    })
                })
                .unwrap();
        }
        let batch = batch.finish().unwrap();
        let schema = batch.schema();
        assert_eq!(schema.field(0).data_type(), &DataType::Binary);
        assert_eq!(
            schema
                .field(0)
                .metadata()
                .get(NATIVE_TYPE_KEY)
                .map(String::as_str),
            Some("text (invalid utf-8)")
        );
        let bytes = batch.column(0).as_binary::<i32>();
        assert_eq!(bytes.value(0), b"plain");
        assert!(bytes.is_null(1));
        assert_eq!(bytes.value(2), b"caf\xe9 \xff\x00");
        assert_eq!(bytes.value(3), b"after");
        // The other columns of the page are untouched.
        assert_eq!(schema.field(1).data_type(), &DataType::Utf8);
        assert_eq!(batch.column(1).as_string::<i32>().value(2), "[2]");
        assert_eq!(schema.field(2).data_type(), &DataType::Int64);

        // Invalid bytes in a jsonb/json column: the same.
        let mut batch = BatchBuilder::new(vec![ColumnPlan::new("j", Kind::Json, &Type::JSON)]);
        batch.push_row(|_| Ok(Some(&b"\"\xff\""[..]))).unwrap();
        let batch = batch.finish().unwrap();
        assert_eq!(batch.column(0).as_binary::<i32>().value(0), b"\"\xff\"");
        assert_eq!(
            batch
                .schema()
                .field(0)
                .metadata()
                .get(NATIVE_TYPE_KEY)
                .map(String::as_str),
            Some("json (invalid utf-8)")
        );
    }

    #[test]
    fn classification_covers_the_fast_set_and_defers_the_rest() {
        assert_eq!(Kind::classify(&Type::INT8), Some(Kind::Int8));
        assert_eq!(Kind::classify(&Type::NUMERIC), Some(Kind::Numeric));
        assert_eq!(Kind::classify(&Type::INTERVAL), None);
        assert_eq!(Kind::classify(&Type::INT4_ARRAY), None);
        assert_eq!(Kind::classify(&Type::INET), None);
        assert_eq!(Kind::classify(&Type::TIMETZ), None);
        // Printed by the server in the session's TimeZone.
        assert_eq!(Kind::classify(&Type::TIMESTAMPTZ), None);
    }
}
