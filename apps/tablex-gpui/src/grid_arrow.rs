use crate::grid::{CellPosition, GridSearchScan, GridSearchStatus, GridSource, PreviewCache};
use arrow_array::{
    Array, BinaryArray, BinaryViewArray, BooleanArray, Date32Array, Date64Array, Decimal128Array,
    Decimal256Array, FixedSizeBinaryArray, Float16Array, Float32Array, Float64Array, Int16Array,
    Int32Array, Int64Array, Int8Array, LargeBinaryArray, LargeStringArray, RecordBatch,
    StringArray, StringViewArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array, UnionArray,
};
use arrow_cast::display::{ArrayFormatter, DurationFormat, FormatOptions};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use engine_driver_host::ImportedArrowBatch;
use gpui::SharedString;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

/// Arrow field metadata key under which a driver reports a column's native
/// database type (e.g. PostgreSQL `uuid`), written by the SQL drivers.
pub const NATIVE_TYPE_METADATA_KEY: &str = "tablex.native_type";

/// Maximum bytes of text to store in a cell preview.
/// Cells exceeding this are truncated with "…" so a 16 MB blob never
/// reaches the text layout path.
const MAX_CELL_DISPLAY_BYTES: usize = 2048;

/// Bytes of a binary value a cell shows in hex before it reads `… (size)`: a
/// cell is a few dozen characters wide, so the grid never formats more.
const HEX_PREVIEW_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLocation {
    pub batch: usize,
    pub row: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMap {
    batch_starts: Vec<usize>,
    total_rows: usize,
}

impl RowMap {
    pub fn from_batches(batches: &[Arc<RecordBatch>]) -> Self {
        let mut batch_starts = Vec::with_capacity(batches.len());
        let mut total_rows = 0;
        for batch in batches {
            batch_starts.push(total_rows);
            total_rows += batch.num_rows();
        }
        Self {
            batch_starts,
            total_rows,
        }
    }

    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    pub fn locate(&self, row: usize) -> Option<RowLocation> {
        if row >= self.total_rows {
            return None;
        }
        let batch = self
            .batch_starts
            .partition_point(|&start| start <= row)
            .checked_sub(1)?;
        Some(RowLocation {
            batch,
            row: row - self.batch_starts[batch],
        })
    }
}

/// Arrow-backed page source for SQL results. Cell previews are formatted on
/// demand and retained in a bounded cache; the typed record batches stay
/// available for full-value paths.
#[derive(Debug, Clone)]
pub struct ArrowGridSource {
    batches: Vec<Arc<RecordBatch>>,
    rows: RowMap,
    column_names: Vec<String>,
    column_types: Vec<String>,
    /// Native database type per column, from the engine's Arrow field
    /// metadata (`NATIVE_TYPE_METADATA_KEY`); read once at build.
    native_types: Vec<Option<String>>,
    /// The type name the driver reported per column (its `PageColumn::data_type`), when the page
    /// carries no native type of its own (MySQL and MariaDB). Charts read it to tell a DECIMAL or a
    /// DATE, which arrive as text, from text.
    driver_types: Vec<String>,
    binary: BinaryDisplay,
    previews: Arc<Mutex<PreviewCache>>,
    _imported_page: Option<Arc<ImportedArrowBatch>>,
}

impl ArrowGridSource {
    pub fn new(batches: Vec<Arc<RecordBatch>>) -> Self {
        let rows = RowMap::from_batches(&batches);
        let column_names: Vec<String> = batches
            .first()
            .map(|batch| {
                batch
                    .schema_ref()
                    .fields()
                    .iter()
                    .map(|field| field.name().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let column_types: Vec<String> = batches
            .first()
            .map(|batch| {
                batch
                    .schema_ref()
                    .fields()
                    .iter()
                    .map(|field| arrow_data_type_label(field.data_type()).to_string())
                    .collect()
            })
            .unwrap_or_default();
        let native_types: Vec<Option<String>> = batches
            .first()
            .map(|batch| {
                batch
                    .schema_ref()
                    .fields()
                    .iter()
                    .map(|field| {
                        field
                            .metadata()
                            .get(NATIVE_TYPE_METADATA_KEY)
                            .filter(|native| !native.is_empty())
                            .cloned()
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            batches,
            rows,
            column_names,
            column_types,
            native_types,
            driver_types: Vec::new(),
            binary: BinaryDisplay::default(),
            previews: Arc::new(Mutex::new(PreviewCache::default())),
            _imported_page: None,
        }
    }

    /// The typed record batches the page holds (shared, not copied): charts read the numbers from them.
    pub fn batches(&self) -> &[Arc<RecordBatch>] {
        &self.batches
    }

    /// Records the type names the driver reported for the page's columns.
    pub fn with_driver_types(mut self, types: impl IntoIterator<Item = String>) -> Self {
        self.driver_types = types.into_iter().collect();
        self
    }

    /// The type name the driver reported for `column` (`decimal`, `datetime`...), if it did.
    pub fn driver_type(&self, column: usize) -> Option<&str> {
        self.driver_types.get(column).map(String::as_str).filter(|name| !name.is_empty())
    }

    /// How `Binary` cells are shown (set before the first cell is read).
    pub fn with_binary_display(mut self, binary: BinaryDisplay) -> Self {
        self.binary = binary;
        self
    }

    /// Bytes the Arrow arrays of the page hold (what keeping this page costs).
    #[cfg(test)]
    pub fn memory_size(&self) -> usize {
        self.batches.iter().map(|batch| batch.get_array_memory_size()).sum()
    }

    pub fn from_imported_page(page: ImportedArrowBatch) -> Self {
        let page = Arc::new(page);
        let mut source = Self::new(vec![Arc::new(page.batch.clone())]);
        source._imported_page = Some(page);
        source
    }

    pub fn with_column_types(
        batches: Vec<Arc<RecordBatch>>,
        column_types: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut source = Self::new(batches);
        source.column_types = column_types.into_iter().collect();
        source
    }
}

/// Truncate `s` to [`MAX_CELL_DISPLAY_BYTES`] bytes on a UTF-8 boundary,
/// appending "…" when cut.
fn truncate_cell(s: &str) -> std::borrow::Cow<'_, str> {
    if s.len() <= MAX_CELL_DISPLAY_BYTES {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut cut = MAX_CELL_DISPLAY_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&s[..cut]);
    out.push('…');
    std::borrow::Cow::Owned(out)
}

pub fn host_cells_to_record_batch(
    columns: &[engine_driver_api::PageColumn],
    row_count: u64,
    cells: impl IntoIterator<Item = engine_driver_host::HostCell>,
) -> Result<RecordBatch, String> {
    let row_count = usize::try_from(row_count).map_err(|_| "row count is too large".to_string())?;
    let mut values = columns
        .iter()
        .map(|_| vec![None::<String>; row_count])
        .collect::<Vec<_>>();
    for cell in cells {
        let row = usize::try_from(cell.row).ok();
        let column = usize::try_from(cell.column).ok();
        if let (Some(row), Some(column)) = (row, column) {
            if let Some(column_values) = values.get_mut(column) {
                if let Some(value) = column_values.get_mut(row) {
                    *value = Some(cell.text);
                }
            }
        }
    }

    let schema = Arc::new(Schema::new(
        columns
            .iter()
            .map(|column| Field::new(column.name.clone(), DataType::Utf8, true))
            .collect::<Vec<_>>(),
    ));
    let arrays = values
        .into_iter()
        .map(|values| Arc::new(StringArray::from(values)) as arrow_array::ArrayRef)
        .collect();
    // A page without columns (a statement's reply, an empty Mongo result) is
    // valid; Arrow only accepts it when the row count is stated.
    engine_driver_kit::batch_with_row_count(schema, arrays, row_count)
        .map_err(|error| error.message)
}

impl GridSource for ArrowGridSource {
    fn row_count(&self) -> usize {
        self.rows.total_rows()
    }

    fn column_count(&self) -> usize {
        self.column_names.len()
    }

    fn column_name(&self, column: usize) -> Option<&str> {
        self.column_names.get(column).map(String::as_str)
    }

    fn column_type(&self, column: usize) -> Option<&str> {
        self.column_types.get(column).map(String::as_str)
    }

    fn native_type(&self, column: usize) -> Option<&str> {
        self.native_types.get(column)?.as_deref()
    }

    fn cell_text(&self, position: CellPosition) -> Option<SharedString> {
        if position.row >= self.row_count() || position.column >= self.column_count() {
            return None;
        }
        if let Ok(previews) = self.previews.lock() {
            if let Some(value) = previews.get(position) {
                return Some(value);
            }
        }

        let location = self.rows.locate(position.row)?;
        let array = self.batches[location.batch].column(position.column);
        let value = if array_cell_is_null(array.as_ref(), location.row) {
            SharedString::new_static("NULL")
        } else {
            let preview = array_value_preview(array, location.row, self.binary);
            match self.native_types.get(position.column) {
                Some(Some(native)) if native == "bytea" => {
                    compact_bytea(array.as_ref(), location.row).unwrap_or(preview)
                }
                _ => preview,
            }
        };
        if let Ok(mut previews) = self.previews.lock() {
            previews.insert(position, value.clone());
        }
        Some(value)
    }

    fn full_cell_text(&self, position: CellPosition) -> Option<String> {
        if position.row >= self.row_count() || position.column >= self.column_count() {
            return None;
        }
        let location = self.rows.locate(position.row)?;
        let array = self.batches[location.batch].column(position.column);
        if array_cell_is_null(array.as_ref(), location.row) {
            Some("NULL".into())
        } else {
            let mut out = String::new();
            ArrowCellReader::new(array.as_ref(), self.binary).write_value(location.row, &mut out);
            Some(out)
        }
    }

    fn cell_is_null(&self, position: CellPosition) -> bool {
        let Some(location) = self.rows.locate(position.row) else {
            return false;
        };
        self.batches
            .get(location.batch)
            .and_then(|batch| batch.columns().get(position.column))
            .is_some_and(|array| array_cell_is_null(array.as_ref(), location.row))
    }

    fn cell_bytes(&self, position: CellPosition) -> Option<&[u8]> {
        self.binary_cell(position.row, position.column)
    }

    fn cell_str(&self, position: CellPosition) -> Option<&str> {
        let location = self.rows.locate(position.row)?;
        let array = self.batches.get(location.batch)?.columns().get(position.column)?;
        let strings = array.as_any().downcast_ref::<StringArray>()?;
        (!strings.is_null(location.row)).then(|| strings.value(location.row))
    }

    #[cfg(test)]
    fn row_values(&self, row: usize) -> Vec<(String, String)> {
        (0..self.column_count())
            .filter_map(|column| {
                Some((
                    self.column_name(column)?.to_string(),
                    self.cell_text(CellPosition { row, column })?.to_string(),
                ))
            })
            .collect()
    }

    /// Row-major scan straight over the Arrow arrays: string columns are
    /// compared as borrowed `&str`, other types are formatted (with the grid's
    /// own display formatting) into one reused buffer. Typed column readers
    /// are resolved once per batch, never per cell; NULLs never match; the
    /// preview cache is not touched. The whole value is searched, not the
    /// 2 KiB display preview.
    fn search_cells(
        &self,
        scan: &GridSearchScan<'_>,
        out: &mut Vec<CellPosition>,
    ) -> GridSearchStatus {
        let readers: Vec<Vec<Option<ArrowCellReader<'_>>>> = self
            .batches
            .iter()
            .map(|batch| {
                scan.columns
                    .iter()
                    .map(|&column| {
                        batch
                            .columns()
                            .get(column)
                            .map(|a| ArrowCellReader::new(a.as_ref(), self.binary))
                    })
                    .collect()
            })
            .collect();
        let mut buffer = String::new();
        let mut scratch = String::new();
        for row in scan.rows(self.row_count()) {
            let Some(location) = self.rows.locate(row.source) else {
                continue;
            };
            for (column, reader) in readers[location.batch].iter().enumerate() {
                let Some(reader) = reader.as_ref().filter(|r| !r.is_null(location.row)) else {
                    continue;
                };
                let text = reader.text(location.row, &mut buffer);
                if scan.query.matches(text, &mut scratch)
                    && scan.push(
                        out,
                        CellPosition {
                            row: row.display,
                            column,
                        },
                    )
                {
                    return GridSearchStatus::Capped;
                }
            }
        }
        scan.finish()
    }
}

fn arrow_data_type_label(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::Boolean => "boolean",
        DataType::Int8 | DataType::Int16 | DataType::Int32 => "int32",
        DataType::Int64 => "int64",
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => "number",
        DataType::Float16 | DataType::Float32 | DataType::Float64 => "number",
        DataType::Decimal128(_, _) | DataType::Decimal256(_, _) => "number",
        DataType::Date32 | DataType::Date64 => "date",
        DataType::Timestamp(_, _) => "datetime",
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => "binary",
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _) => "array",
        DataType::Struct(_) | DataType::Map(_, _) => "object",
        // A SQLite column holding several storage classes.
        DataType::Union(_, _) => "mixed",
        _ => "string",
    }
}

/// The storage class of one cell of a SQLite column that holds several
/// (an Arrow dense union, label `mixed`; the type ids are the driver's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellClass {
    Integer,
    Real,
    Text,
    Blob,
    /// Text that is not valid UTF-8: shown as hex, read-only.
    InvalidUtf8Text,
}

impl CellClass {
    fn of_type_id(type_id: i8) -> Option<Self> {
        Some(match type_id {
            0 => Self::Integer,
            1 => Self::Real,
            2 => Self::Text,
            3 => Self::Blob,
            4 => Self::InvalidUtf8Text,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
            Self::InvalidUtf8Text => "TEXT (invalid UTF-8)",
        }
    }
}

/// `(type id, offset in that child)` of a union cell that is not SQL NULL (a
/// null of child 0).
fn union_cell(union: &UnionArray, row: usize) -> Option<(i8, usize)> {
    let type_id = union.type_id(row);
    let offset = union.value_offset(row);
    (type_id != 0 || !union.child(0).is_null(offset)).then_some((type_id, offset))
}

/// How `Binary` cells are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BinaryDisplay {
    /// `0x` + lower-case hex: one text per byte string, the form the SQL
    /// drivers accept back for binary columns.
    #[default]
    Hex,
    /// The way the Redis driver shows a byte string: text as it is, bytes that
    /// are not text escaped as `\xNN`.
    ByteString,
}

#[cfg(test)]
pub(crate) fn array_value_to_string(array: &dyn Array, row: usize) -> String {
    let mut out = String::new();
    ArrowCellReader::new(array, BinaryDisplay::Hex).write_value(row, &mut out);
    out
}

/// Whether the cell is NULL as a user sees it: the validity bit, a `Null`-typed
/// column (every cell), a dictionary entry whose value is null.
fn array_cell_is_null(array: &dyn Array, row: usize) -> bool {
    use arrow_array::downcast_dictionary_array;
    match array.data_type() {
        DataType::Null => true,
        DataType::Union(_, _) => array
            .as_any()
            .downcast_ref::<UnionArray>()
            .is_some_and(|union| union_cell(union, row).is_none()),
        DataType::Dictionary(_, _) => downcast_dictionary_array!(
            array => array.key(row).is_none_or(|key| array.values().is_null(key)),
            _ => array.is_null(row)
        ),
        _ => array.is_null(row),
    }
}

/// Options of the generic formatter used for the types without an exact arm:
/// nested nulls read `NULL`, durations are spelled out. Errors surface as a
/// `fmt::Error` so the caller can fall back to the type name.
static FORMAT_OPTIONS: FormatOptions<'static> = FormatOptions::new()
    .with_display_error(false)
    .with_null("NULL")
    .with_duration_format(DurationFormat::Pretty);

/// The bytes of a binary column, whichever Arrow layout carries them.
enum ByteValues<'a> {
    Binary(&'a BinaryArray),
    Large(&'a LargeBinaryArray),
    View(&'a BinaryViewArray),
    Fixed(&'a FixedSizeBinaryArray),
}

impl<'a> ByteValues<'a> {
    /// The byte reader of a binary column of any layout.
    fn of(array: &'a dyn Array) -> Option<Self> {
        let any = array.as_any();
        match array.data_type() {
            DataType::Binary => any.downcast_ref().map(Self::Binary),
            DataType::LargeBinary => any.downcast_ref().map(Self::Large),
            DataType::BinaryView => any.downcast_ref().map(Self::View),
            DataType::FixedSizeBinary(_) => any.downcast_ref().map(Self::Fixed),
            _ => None,
        }
    }

    fn value(&self, row: usize) -> &'a [u8] {
        match self {
            Self::Binary(values) => (*values).value(row),
            Self::Large(values) => (*values).value(row),
            Self::View(values) => (*values).value(row),
            Self::Fixed(values) => (*values).value(row),
        }
    }
}

/// How the timestamps of a column are placed on the clock.
enum Zone {
    /// No zone: the stored wall-clock value as it is.
    Naive,
    /// A fixed offset or UTC (`Tz` knows fixed offsets only: this build has no
    /// zone database).
    Offset(arrow_array::timezone::Tz),
    /// A zone name this build cannot resolve: the instant in UTC, labelled.
    Utc,
}

struct TimestampColumn<'a> {
    values: &'a [i64],
    unit: TimeUnit,
    zone: Zone,
}

/// A column resolved to its concrete Arrow type once, so reading many cells
/// does not repeat the downcast chain per cell. Single source of truth for the
/// grid's display formatting (`array_value_to_string`) and the find scan.
pub(crate) struct ArrowCellReader<'a> {
    array: &'a dyn Array,
    kind: ReaderKind<'a>,
}

enum ReaderKind<'a> {
    Utf8(&'a StringArray),
    LargeUtf8(&'a LargeStringArray),
    Utf8View(&'a StringViewArray),
    Date32(&'a Date32Array),
    Date64(&'a Date64Array),
    Int8(&'a Int8Array),
    Int16(&'a Int16Array),
    Int32(&'a Int32Array),
    Int64(&'a Int64Array),
    UInt8(&'a UInt8Array),
    UInt16(&'a UInt16Array),
    UInt32(&'a UInt32Array),
    UInt64(&'a UInt64Array),
    Float16(&'a Float16Array),
    Float32(&'a Float32Array),
    Float64(&'a Float64Array),
    Boolean(&'a BooleanArray),
    Decimal128(&'a Decimal128Array, i8),
    Decimal256(&'a Decimal256Array, i8),
    Timestamp(TimestampColumn<'a>),
    Bytes(ByteValues<'a>, BinaryDisplay),
    /// A SQLite column of several storage classes; each cell reads by its own.
    Union(&'a UnionArray),
    /// Every cell is NULL.
    Null,
    /// Everything else Arrow can format (times, durations, intervals, lists,
    /// structs, maps, dictionaries, unions, ...). The formatter is built once,
    /// here; a column of cells shares it.
    Display(ArrayFormatter<'a>),
    /// Arrow cannot format this type.
    Unsupported,
}

impl<'a> ArrowCellReader<'a> {
    pub(crate) fn new(array: &'a dyn Array, binary: BinaryDisplay) -> Self {
        let any = array.as_any();
        macro_rules! downcast {
            ($variant:ident, $array:ty) => {
                any.downcast_ref::<$array>().map(ReaderKind::$variant)
            };
        }
        let exact = match array.data_type() {
            DataType::Utf8 => downcast!(Utf8, StringArray),
            DataType::LargeUtf8 => downcast!(LargeUtf8, LargeStringArray),
            DataType::Utf8View => downcast!(Utf8View, StringViewArray),
            DataType::Date32 => downcast!(Date32, Date32Array),
            DataType::Date64 => downcast!(Date64, Date64Array),
            DataType::Int8 => downcast!(Int8, Int8Array),
            DataType::Int16 => downcast!(Int16, Int16Array),
            DataType::Int32 => downcast!(Int32, Int32Array),
            DataType::Int64 => downcast!(Int64, Int64Array),
            DataType::UInt8 => downcast!(UInt8, UInt8Array),
            DataType::UInt16 => downcast!(UInt16, UInt16Array),
            DataType::UInt32 => downcast!(UInt32, UInt32Array),
            DataType::UInt64 => downcast!(UInt64, UInt64Array),
            DataType::Float16 => downcast!(Float16, Float16Array),
            DataType::Float32 => downcast!(Float32, Float32Array),
            DataType::Float64 => downcast!(Float64, Float64Array),
            DataType::Boolean => downcast!(Boolean, BooleanArray),
            DataType::Decimal128(_, scale) => any
                .downcast_ref::<Decimal128Array>()
                .map(|values| ReaderKind::Decimal128(values, *scale)),
            DataType::Decimal256(_, scale) => any
                .downcast_ref::<Decimal256Array>()
                .map(|values| ReaderKind::Decimal256(values, *scale)),
            DataType::Timestamp(unit, zone) => {
                timestamp_column(array, *unit, zone.as_deref()).map(ReaderKind::Timestamp)
            }
            DataType::Binary => any
                .downcast_ref::<BinaryArray>()
                .map(|values| ReaderKind::Bytes(ByteValues::Binary(values), binary)),
            DataType::LargeBinary => any
                .downcast_ref::<LargeBinaryArray>()
                .map(|values| ReaderKind::Bytes(ByteValues::Large(values), binary)),
            DataType::BinaryView => any
                .downcast_ref::<BinaryViewArray>()
                .map(|values| ReaderKind::Bytes(ByteValues::View(values), binary)),
            DataType::FixedSizeBinary(_) => any
                .downcast_ref::<FixedSizeBinaryArray>()
                .map(|values| ReaderKind::Bytes(ByteValues::Fixed(values), binary)),
            DataType::Null => Some(ReaderKind::Null),
            DataType::Union(_, _) => any.downcast_ref::<UnionArray>().map(ReaderKind::Union),
            _ => None,
        };
        let kind = exact.unwrap_or_else(|| match ArrayFormatter::try_new(array, &FORMAT_OPTIONS) {
            Ok(formatter) => ReaderKind::Display(formatter),
            Err(_) => ReaderKind::Unsupported,
        });
        Self { array, kind }
    }

    pub(crate) fn is_null(&self, row: usize) -> bool {
        match &self.kind {
            ReaderKind::Null => true,
            ReaderKind::Display(_) | ReaderKind::Unsupported | ReaderKind::Union(_) => {
                array_cell_is_null(self.array, row)
            }
            _ => self.array.is_null(row),
        }
    }

    /// Display text of a non-null cell appended to `out`.
    pub(crate) fn write_value(&self, row: usize, out: &mut String) {
        // Writing into a `String` cannot fail.
        let _ = match &self.kind {
            ReaderKind::Utf8(values) => out.write_str(values.value(row)),
            ReaderKind::LargeUtf8(values) => out.write_str(values.value(row)),
            ReaderKind::Utf8View(values) => out.write_str(values.value(row)),
            ReaderKind::Date32(values) => match values.value_as_date(row) {
                Some(date) => write!(out, "{date}"),
                None => write!(out, "<Date32 raw value: {}>", values.value(row)),
            },
            ReaderKind::Date64(values) => match values.value_as_datetime(row) {
                Some(datetime) => write!(out, "{}", datetime.date()),
                None => write!(out, "<Date64 raw value: {}>", values.value(row)),
            },
            ReaderKind::Int8(values) => write!(out, "{}", values.value(row)),
            ReaderKind::Int16(values) => write!(out, "{}", values.value(row)),
            ReaderKind::Int32(values) => write!(out, "{}", values.value(row)),
            ReaderKind::Int64(values) => write!(out, "{}", values.value(row)),
            ReaderKind::UInt8(values) => write!(out, "{}", values.value(row)),
            ReaderKind::UInt16(values) => write!(out, "{}", values.value(row)),
            ReaderKind::UInt32(values) => write!(out, "{}", values.value(row)),
            ReaderKind::UInt64(values) => write!(out, "{}", values.value(row)),
            ReaderKind::Float16(values) => {
                let value = shortest_f16(values.value(row));
                write_float(
                    out,
                    value,
                    |out| write!(out, "{value}"),
                    |out| write!(out, "{value:e}"),
                )
            }
            ReaderKind::Float32(values) => {
                let value = values.value(row);
                write_float(
                    out,
                    f64::from(value),
                    |out| write!(out, "{value}"),
                    |out| write!(out, "{value:e}"),
                )
            }
            ReaderKind::Float64(values) => {
                let value = values.value(row);
                write_float(
                    out,
                    value,
                    |out| write!(out, "{value}"),
                    |out| write!(out, "{value:e}"),
                )
            }
            ReaderKind::Boolean(values) => write!(out, "{}", values.value(row)),
            ReaderKind::Decimal128(values, scale) => write_decimal(out, values.value(row), *scale),
            ReaderKind::Decimal256(values, scale) => write_decimal(out, values.value(row), *scale),
            ReaderKind::Timestamp(column) => write_timestamp(out, column, row),
            ReaderKind::Bytes(values, BinaryDisplay::Hex) => {
                write_hex(values.value(row), usize::MAX, out);
                Ok(())
            }
            ReaderKind::Bytes(values, BinaryDisplay::ByteString) => {
                write_byte_string(values.value(row), out);
                Ok(())
            }
            ReaderKind::Null => out.write_str("NULL"),
            ReaderKind::Union(union) => write_union_cell(out, union, row),
            ReaderKind::Display(formatter) => {
                let start = out.len();
                let written = write!(out, "{}", formatter.value(row));
                if written.is_err() {
                    out.truncate(start);
                    write!(out, "<{:?}>", self.array.data_type())
                } else {
                    Ok(())
                }
            }
            ReaderKind::Unsupported => write!(out, "<{:?}>", self.array.data_type()),
        };
    }

    /// Display text of a non-null cell: borrowed from the array for strings,
    /// otherwise formatted into `buffer` (cleared first, capacity reused).
    pub(crate) fn text<'b>(&'b self, row: usize, buffer: &'b mut String) -> &'b str {
        match &self.kind {
            ReaderKind::Utf8(values) => values.value(row),
            ReaderKind::LargeUtf8(values) => values.value(row),
            ReaderKind::Utf8View(values) => values.value(row),
            _ => {
                buffer.clear();
                self.write_value(row, buffer);
                buffer.as_str()
            }
        }
    }

    /// The display text of a non-null cell, cut for the grid: a long hex string
    /// is never built in full. A binary value longer than
    /// [`HEX_PREVIEW_BYTES`] reads `0x` + its first bytes + `… (12.3 KB)`: the
    /// cell shows a handful of characters, so the rest is never formatted.
    fn write_preview(&self, row: usize, out: &mut String) {
        let bytes = match &self.kind {
            ReaderKind::Bytes(values, BinaryDisplay::Hex) => Some(values.value(row)),
            ReaderKind::Union(union) => cell_bytes(*union, row),
            _ => None,
        };
        if let Some(bytes) = bytes.filter(|bytes| bytes.len() > HEX_PREVIEW_BYTES) {
            write_hex(bytes, HEX_PREVIEW_BYTES, out);
            let _ = write!(out, "… ({})", crate::binary_value::format_size(bytes.len()));
            return;
        }
        self.write_value(row, out);
    }
}

/// A union cell by its own class: integer digits, a float, text as it is, and
/// `0x` + hex for a BLOB (and for text that is not UTF-8).
fn write_union_cell(out: &mut String, union: &UnionArray, row: usize) -> std::fmt::Result {
    let Some((type_id, offset)) = union_cell(union, row) else {
        return out.write_str("NULL");
    };
    let child = union.child(type_id);
    let any = child.as_any();
    match (CellClass::of_type_id(type_id), type_id) {
        (Some(CellClass::Integer), _) => match any.downcast_ref::<Int64Array>() {
            Some(values) => write!(out, "{}", values.value(offset)),
            None => write!(out, "<{:?}>", child.data_type()),
        },
        (Some(CellClass::Real), _) => match any.downcast_ref::<Float64Array>() {
            Some(values) => {
                let value = values.value(offset);
                write_float(
                    out,
                    value,
                    |out| write!(out, "{value}"),
                    |out| write!(out, "{value:e}"),
                )
            }
            None => write!(out, "<{:?}>", child.data_type()),
        },
        (Some(CellClass::Text), _) => match any.downcast_ref::<StringArray>() {
            Some(values) => out.write_str(values.value(offset)),
            None => write!(out, "<{:?}>", child.data_type()),
        },
        (Some(CellClass::Blob | CellClass::InvalidUtf8Text), _) => {
            match any.downcast_ref::<BinaryArray>() {
                Some(values) => {
                    write_hex(values.value(offset), usize::MAX, out);
                    Ok(())
                }
                None => write!(out, "<{:?}>", child.data_type()),
            }
        }
        (None, id) => write!(out, "<union type {id}>"),
    }
}

/// Shortest text that reads back to the same float: plain digits for a readable
/// magnitude, exponent form outside it (never hundreds of digits), `NaN`,
/// `Infinity` and `-Infinity` spelled that way. `plain` and
/// `exponent` write the value in the float's own precision.
/// Arrow's half-precision float (`half::f16`, named through arrow so the app
/// takes no dependency on the crate).
type Half = <arrow_array::types::Float16Type as arrow_array::ArrowPrimitiveType>::Native;

/// The shortest decimal (at most 5 significant digits, as many as a half
/// needs) that reads back as the same half, as an `f64` whose own shortest
/// text is those digits. Not-finite and zero values are returned as they are.
fn shortest_f16(value: Half) -> f64 {
    let wide = f64::from(value);
    if !wide.is_finite() || wide == 0.0 {
        return wide;
    }
    let mut text = String::with_capacity(16);
    (0..=4)
        .find_map(|decimals| {
            text.clear();
            write!(text, "{wide:.decimals$e}").ok()?;
            let candidate: f64 = text.parse().ok()?;
            (Half::from_f64(candidate) == value).then_some(candidate)
        })
        .unwrap_or(wide)
}

fn write_float(
    out: &mut String,
    value: f64,
    plain: impl FnOnce(&mut String) -> std::fmt::Result,
    exponent: impl FnOnce(&mut String) -> std::fmt::Result,
) -> std::fmt::Result {
    if value.is_nan() {
        out.write_str("NaN")
    } else if value.is_infinite() {
        out.write_str(if value > 0.0 { "Infinity" } else { "-Infinity" })
    } else if value == 0.0 || (1e-7..1e21).contains(&value.abs()) {
        plain(out)
    } else {
        exponent(out)
    }
}

/// `value` with `scale` digits after the decimal point, every digit kept
/// (`12.50`, `-0.05`): exact for any width, never through a float.
fn write_decimal(out: &mut String, value: impl std::fmt::Display, scale: i8) -> std::fmt::Result {
    const ZEROS: &str =
        "0000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    let start = out.len();
    write!(out, "{value}")?;
    let digits_start = start + usize::from(out[start..].starts_with('-'));
    let digits = out.len() - digits_start;
    if scale > 0 {
        let scale = scale as usize;
        if digits <= scale {
            out.insert_str(
                digits_start,
                &ZEROS[..(scale - digits + 1).min(ZEROS.len())],
            );
        }
        out.insert(out.len() - scale, '.');
    } else if scale < 0 && &out[digits_start..] != "0" {
        out.push_str(&ZEROS[..usize::from(scale.unsigned_abs()).min(ZEROS.len())]);
    }
    Ok(())
}

fn timestamp_column<'a>(
    array: &'a dyn Array,
    unit: TimeUnit,
    zone: Option<&str>,
) -> Option<TimestampColumn<'a>> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
        TimestampSecondType,
    };
    let values: &[i64] = match unit {
        TimeUnit::Second => array.as_primitive_opt::<TimestampSecondType>()?.values(),
        TimeUnit::Millisecond => array
            .as_primitive_opt::<TimestampMillisecondType>()?
            .values(),
        TimeUnit::Microsecond => array
            .as_primitive_opt::<TimestampMicrosecondType>()?
            .values(),
        TimeUnit::Nanosecond => array
            .as_primitive_opt::<TimestampNanosecondType>()?
            .values(),
    };
    let zone = match zone {
        None => Zone::Naive,
        Some(name) => match (if matches!(name, "UTC" | "Z" | "GMT" | "Etc/UTC") {
            "+00:00"
        } else {
            name
        })
        .parse::<arrow_array::timezone::Tz>()
        {
            Ok(tz) => Zone::Offset(tz),
            Err(_) => Zone::Utc,
        },
    };
    Some(TimestampColumn { values, unit, zone })
}

/// `YYYY-MM-DD HH:MM:SS[.fraction][offset]`: the fraction keeps the unit's own
/// digits (3, 6 or 9) and is left out when zero; a zone adds its offset; a
/// value outside the calendar reads as its raw number instead of panicking.
fn write_timestamp(out: &mut String, column: &TimestampColumn<'_>, row: usize) -> std::fmt::Result {
    use chrono::{Offset as _, TimeZone as _, Timelike as _};
    let raw = column.values[row];
    let (per_second, digits) = match column.unit {
        TimeUnit::Second => (1, 0),
        TimeUnit::Millisecond => (1_000, 3),
        TimeUnit::Microsecond => (1_000_000, 6),
        TimeUnit::Nanosecond => (1_000_000_000, 9),
    };
    let seconds = raw.div_euclid(per_second);
    let fraction = raw.rem_euclid(per_second);
    // `per_second` divides 1e9, so the nanoseconds are exact.
    let nanos = (fraction * (1_000_000_000 / per_second)) as u32;
    let Some(utc) = chrono::DateTime::from_timestamp(seconds, nanos) else {
        return write!(out, "<Timestamp raw value: {raw}>");
    };
    let naive = utc.naive_utc();
    // The wall clock and, with a zone, its offset from UTC in seconds.
    let (clock, offset) = match &column.zone {
        Zone::Naive | Zone::Utc => (naive, None),
        Zone::Offset(tz) => {
            let offset = tz.offset_from_utc_datetime(&naive).fix().local_minus_utc();
            (
                naive
                    .checked_add_signed(chrono::Duration::seconds(i64::from(offset)))
                    .unwrap_or(naive),
                Some(offset),
            )
        }
    };
    write!(
        out,
        "{} {:02}:{:02}:{:02}",
        clock.date(),
        clock.hour(),
        clock.minute(),
        clock.second()
    )?;
    if digits > 0 && fraction != 0 {
        write!(out, ".{:0width$}", fraction, width = digits)?;
    }
    match (offset, &column.zone) {
        (Some(offset), _) => {
            let magnitude = offset.unsigned_abs();
            write!(
                out,
                "{}{:02}:{:02}",
                if offset < 0 { '-' } else { '+' },
                magnitude / 3600,
                magnitude % 3600 / 60
            )
        }
        (None, Zone::Utc) => out.write_str(" UTC"),
        (None, _) => Ok(()),
    }
}

/// `0x` + lower-case hex of at most `limit` bytes.
fn write_hex(bytes: &[u8], limit: usize, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = &bytes[..bytes.len().min(limit)];
    out.reserve(bytes.len() * 2 + 2);
    out.push_str("0x");
    for &byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
}

/// Appends a byte string the way the Redis driver shows it: text as it is,
/// control characters and bytes that are not UTF-8 as `\xNN` (a backslash as
/// `\\` in the latter case only). Tab, newline and carriage return stay.
pub(crate) fn write_byte_string(bytes: &[u8], out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let escape = |byte: u8, out: &mut String| {
        out.push_str("\\x");
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            for ch in text.chars() {
                if ch.is_ascii_control() && !matches!(ch, '\t' | '\n' | '\r') {
                    escape(ch as u8, out);
                } else {
                    out.push(ch);
                }
            }
        }
        Err(_) => {
            for &byte in bytes {
                match byte {
                    b'\\' => out.push_str("\\\\"),
                    0x20..=0x7e => out.push(byte as char),
                    _ => escape(byte, out),
                }
            }
        }
    }
}

impl ArrowGridSource {
    /// The storage class of a cell of a `mixed` column (`None` for any other
    /// column, and for a NULL).
    pub fn cell_class(&self, row: usize, column: usize) -> Option<CellClass> {
        let location = self.rows.locate(row)?;
        let array = self.batches.get(location.batch)?.columns().get(column)?;
        let union = array.as_any().downcast_ref::<UnionArray>()?;
        let (type_id, _) = union_cell(union, location.row)?;
        CellClass::of_type_id(type_id)
    }

    /// The bytes of a binary cell, whatever the Arrow layout (`Binary`,
    /// `LargeBinary`, `BinaryView`, `FixedSizeBinary`) and including a BLOB of a
    /// SQLite `mixed` column (`None` for NULL, out of range, or another type).
    pub fn binary_cell(&self, row: usize, column: usize) -> Option<&[u8]> {
        let location = self.rows.locate(row)?;
        let array = self.batches.get(location.batch)?.columns().get(column)?;
        cell_bytes(array.as_ref(), location.row)
    }
}

/// The bytes of the binary cell `row` of `array` (a BLOB or invalid UTF-8 text
/// cell of a SQLite union included).
fn cell_bytes(array: &dyn Array, row: usize) -> Option<&[u8]> {
    if let Some(union) = array.as_any().downcast_ref::<UnionArray>() {
        let (type_id, offset) = union_cell(union, row)?;
        return matches!(
            CellClass::of_type_id(type_id),
            Some(CellClass::Blob | CellClass::InvalidUtf8Text)
        )
        .then(|| union.child(type_id).as_any().downcast_ref::<BinaryArray>())
        .flatten()
        .map(|values| values.value(offset));
    }
    let values = ByteValues::of(array)?;
    (!array.is_null(row)).then(|| values.value(row))
}

/// A PostgreSQL `bytea` cell (the server's `\x` + hex text) as a binary cell
/// reads: the first bytes in hex and the size, `\x…… (12.3 KB)`. `None` for a
/// short value (shown whole), or text that is not `\x` + hex.
fn compact_bytea(array: &dyn Array, row: usize) -> Option<SharedString> {
    let text = array.as_any().downcast_ref::<StringArray>()?.value(row);
    let digits = text.strip_prefix("\\x")?;
    let shown = 2 * HEX_PREVIEW_BYTES;
    if digits.len() <= shown || digits.len() % 2 != 0 {
        return None;
    }
    Some(SharedString::from(format!(
        "\\x{}… ({})",
        &digits[..shown],
        crate::binary_value::format_size(digits.len() / 2)
    )))
}

fn array_value_preview(array: &dyn Array, row: usize, binary: BinaryDisplay) -> SharedString {
    if let Some(values) = array.as_any().downcast_ref::<StringArray>() {
        return SharedString::from(truncate_cell(values.value(row)).into_owned());
    }
    let mut value = String::new();
    ArrowCellReader::new(array, binary).write_preview(row, &mut value);
    SharedString::from(truncate_cell(&value).into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use arrow_array::{Date32Array, Date64Array, Int64Array};
    use arrow_schema::{DataType, Field, Schema};

    fn batch(rows: usize) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let values: arrow_array::ArrayRef = Arc::new(Int64Array::from_iter_values(0..rows as i64));
        RecordBatch::try_new(schema, vec![values]).expect("valid batch")
    }

    #[test]
    fn redis_byte_strings_show_as_text_with_escapes_and_stay_readable_by_cell() {
        let show = |bytes: &[u8]| {
            let mut out = String::new();
            write_byte_string(bytes, &mut out);
            out
        };
        assert_eq!(show(b"plain \xc3\xa9\ttab"), "plain \u{e9}\ttab");
        assert_eq!(show(b"nul\x00 bell\x07"), "nul\\x00 bell\\x07");
        assert_eq!(show(b"\xac\xed\x00\x05t\\"), "\\xac\\xed\\x00\\x05t\\\\");
        let schema = Arc::new(Schema::new(vec![Field::new(
            "value",
            DataType::Binary,
            true,
        )]));
        let values: arrow_array::ArrayRef = Arc::new(BinaryArray::from(vec![
            Some(&b"hello"[..]),
            None,
            Some(&b"\xff\xfe"[..]),
        ]));
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(schema, vec![values]).unwrap(),
        )])
        .with_binary_display(BinaryDisplay::ByteString);
        let at = |row| CellPosition { row, column: 0 };
        assert_eq!(source.cell_text(at(0)).as_deref(), Some("hello"));
        assert!(source.cell_is_null(at(1)));
        assert_eq!(source.cell_text(at(2)).as_deref(), Some("\\xff\\xfe"));
        assert_eq!(source.binary_cell(2, 0), Some(&b"\xff\xfe"[..]));
        assert_eq!(source.binary_cell(1, 0), None);
        assert_eq!(source.binary_cell(0, 3), None);
        assert_eq!(source.column_type(0), Some("binary"));
        // The find scan sees the displayed text.
        let (hits, _) = search(&source, "xff", &[0], 10);
        assert_eq!(hits, vec![at(2)]);
    }

    fn search_query(text: &str) -> crate::grid::GridSearchQuery {
        crate::grid::GridSearchQuery::new(text, crate::grid::GridSearchOptions::default())
            .expect("non-empty query")
    }

    fn search(
        source: &ArrowGridSource,
        text: &str,
        columns: &[usize],
        max: usize,
    ) -> (Vec<CellPosition>, GridSearchStatus) {
        let grid = crate::grid::GridState::new(source.clone());
        let never = std::sync::atomic::AtomicBool::new(false);
        grid.search_cells(&search_query(text), columns, max, &never)
    }

    fn mixed_source() -> ArrowGridSource {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("score", DataType::Float64, true),
            Field::new("born", DataType::Date32, true),
            Field::new("active", DataType::Boolean, true),
            Field::new("big", DataType::LargeUtf8, true),
        ]));
        let first = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![7, 8])) as arrow_array::ArrayRef,
                Arc::new(StringArray::from(vec![Some("Ada"), None])) as arrow_array::ArrayRef,
                Arc::new(Float64Array::from(vec![Some(2.5), None])) as arrow_array::ArrayRef,
                Arc::new(Date32Array::from(vec![Some(0), Some(1)])) as arrow_array::ArrayRef,
                Arc::new(BooleanArray::from(vec![Some(true), Some(false)]))
                    as arrow_array::ArrayRef,
                Arc::new(LargeStringArray::from(vec![Some("wide ADA"), None]))
                    as arrow_array::ArrayRef,
            ],
        )
        .expect("batch");
        let second = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![9])) as arrow_array::ArrayRef,
                Arc::new(StringArray::from(vec![Some("Lin")])) as arrow_array::ArrayRef,
                Arc::new(Float64Array::from(vec![Some(0.25)])) as arrow_array::ArrayRef,
                Arc::new(Date32Array::from(vec![None])) as arrow_array::ArrayRef,
                Arc::new(BooleanArray::from(vec![None])) as arrow_array::ArrayRef,
                Arc::new(LargeStringArray::from(vec![Some("x")])) as arrow_array::ArrayRef,
            ],
        )
        .expect("batch");
        ArrowGridSource::new(vec![Arc::new(first), Arc::new(second)])
    }

    #[test]
    fn search_formats_non_string_columns_like_the_grid_across_batches() {
        let source = mixed_source();
        let all = [0, 1, 2, 3, 4, 5];
        // Int64 / Float64 / Date32 / Boolean use the grid's display text.
        let cell = |row, column| CellPosition { row, column };
        assert_eq!(search(&source, "9", &[0, 1, 2], 100).0, [cell(2, 0)]);
        assert_eq!(search(&source, "2.5", &all, 100).0, [cell(0, 2)]);
        assert_eq!(search(&source, "1970-01-02", &all, 100).0, [cell(1, 3)]);
        assert_eq!(search(&source, "true", &all, 100).0, [cell(0, 4)]);
        // LargeUtf8 is searched as text; the match is case-insensitive.
        assert_eq!(
            search(&source, "ada", &all, 100).0,
            [cell(0, 1), cell(0, 5)],
            "row-major: Utf8 then LargeUtf8 of row 0"
        );
        // NULLs (including a NULL boolean/date) never match the "NULL" text.
        assert!(search(&source, "null", &all, 100).0.is_empty());
        // Display columns select and reorder source columns.
        assert_eq!(
            search(&source, "ada", &[5, 1], 100).0,
            [cell(0, 0), cell(0, 1)]
        );
        assert!(search(&source, "ada", &[0, 2], 100).0.is_empty());
        // The grid's own formatting is the single definition.
        assert_eq!(
            array_value_to_string(source.batches[0].column(5), 0),
            "wide ADA"
        );
    }

    #[test]
    fn search_stops_at_the_cap_and_never_touches_the_preview_cache() {
        let source = mixed_source();
        let (cells, status) = search(&source, "a", &[0, 1, 2, 3, 4, 5], 1);
        assert_eq!((cells.len(), status), (1, GridSearchStatus::Capped));
        let (_, status) = search(&source, "a", &[0, 1, 2, 3, 4, 5], 100);
        assert_eq!(status, GridSearchStatus::Complete);
        assert_eq!(
            source.previews.lock().expect("preview cache").len(),
            0,
            "searching must not churn the bounded display cache"
        );
    }

    #[test]
    fn search_reports_display_rows_for_a_sorted_page() {
        let source = mixed_source();
        let mut grid = crate::grid::GridState::new(source);
        grid.sort_page(0, crate::grid::SortKind::Integer, true).unwrap(); // ids 9, 8, 7 -> source 2, 1, 0
        let never = std::sync::atomic::AtomicBool::new(false);
        let (cells, _) = grid.search_cells(&search_query("ada"), &[1], 100, &never);
        assert_eq!(cells, [CellPosition { row: 2, column: 0 }]);
    }

    #[test]
    fn maps_global_rows_across_record_batches() {
        let map =
            RowMap::from_batches(&[Arc::new(batch(2)), Arc::new(batch(3)), Arc::new(batch(1))]);

        assert_eq!(map.total_rows(), 6);
        assert_eq!(map.locate(0), Some(RowLocation { batch: 0, row: 0 }));
        assert_eq!(map.locate(2), Some(RowLocation { batch: 1, row: 0 }));
        assert_eq!(map.locate(4), Some(RowLocation { batch: 1, row: 2 }));
        assert_eq!(map.locate(5), Some(RowLocation { batch: 2, row: 0 }));
        assert_eq!(map.locate(6), None);
    }

    #[test]
    fn reads_visible_arrow_cells_without_copying_record_batches() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let record = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![7, 8])) as arrow_array::ArrayRef,
                Arc::new(StringArray::from(vec![Some("Ada"), None])) as arrow_array::ArrayRef,
            ],
        )
        .expect("valid batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        assert_eq!(grid.row_count(), 2);
        assert_eq!(grid.column_count(), 2);
        assert_eq!(grid.visible_rows(1, 10), 1..2);
        assert_eq!(
            grid.cell_text(CellPosition { row: 0, column: 0 }),
            Some("7".into())
        );
        assert_eq!(
            grid.cell_text(CellPosition { row: 1, column: 1 }),
            Some("NULL".into())
        );
        assert_eq!(grid.cell_text(CellPosition { row: 2, column: 0 }), None);
    }

    #[test]
    fn creates_arrow_previews_only_when_cells_are_read() {
        let source = ArrowGridSource::new(vec![Arc::new(batch(2))]);
        assert_eq!(source.previews.lock().expect("preview cache").len(), 0);

        assert_eq!(
            source.cell_text(CellPosition { row: 1, column: 0 }),
            Some("1".into())
        );
        assert_eq!(source.previews.lock().expect("preview cache").len(), 1);
    }

    #[test]
    fn exposes_arrow_column_names() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let record = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1])) as arrow_array::ArrayRef,
                Arc::new(StringArray::from(vec![Some("Ada")])) as arrow_array::ArrayRef,
            ],
        )
        .expect("valid batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        assert_eq!(grid.column_name(0), Some("id"));
        assert_eq!(grid.column_name(1), Some("name"));
        assert_eq!(grid.column_name(2), None);
    }

    #[test]
    fn exposes_engine_native_types_without_changing_logical_types() {
        let native = std::collections::HashMap::from([(
            NATIVE_TYPE_METADATA_KEY.to_string(),
            "bigint".to_string(),
        )]);
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false).with_metadata(native),
            Field::new("n", DataType::Int64, true),
        ]));
        let record = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1])) as arrow_array::ArrayRef,
                Arc::new(Int64Array::from(vec![2])) as arrow_array::ArrayRef,
            ],
        )
        .expect("valid batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        assert_eq!(grid.native_type(0), Some("bigint"));
        assert_eq!(grid.native_type(1), None);
        assert_eq!(grid.native_type(2), None);
        // Behavior (sort/copy/alignment) keeps keying on the logical type.
        assert_eq!(grid.column_type(0), Some("int64"));
    }

    #[test]
    fn exposes_arrow_column_types() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let record = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1])) as arrow_array::ArrayRef,
                Arc::new(StringArray::from(vec![Some("Ada")])) as arrow_array::ArrayRef,
            ],
        )
        .expect("valid batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        assert_eq!(grid.column_type(0), Some("int64"));
        assert_eq!(grid.column_type(1), Some("string"));
        assert_eq!(grid.column_type(2), None);
    }

    #[test]
    fn row_values_returns_named_cells_for_detail_views() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let record = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![7])) as arrow_array::ArrayRef,
                Arc::new(StringArray::from(vec![Some("Ada")])) as arrow_array::ArrayRef,
            ],
        )
        .expect("valid detail batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        assert_eq!(
            grid.row_values(0),
            vec![("id".into(), "7".into()), ("name".into(), "Ada".into())]
        );
        assert!(grid.row_values(1).is_empty());
    }

    #[test]
    fn tsv_copy_joins_selected_cells_with_tabs_and_newlines() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let record = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1, 2])) as arrow_array::ArrayRef,
                Arc::new(StringArray::from(vec![Some("Ada"), None])) as arrow_array::ArrayRef,
            ],
        )
        .expect("valid batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        let mut selection = crate::grid::GridSelection::default();
        selection.select(CellPosition { row: 0, column: 0 });
        selection.extend_to(CellPosition { row: 1, column: 1 });

        assert_eq!(
            crate::grid::selection_as_tsv(&grid, &selection),
            "1\tAda\n2\tNULL"
        );
    }

    #[test]
    fn a_driver_page_without_columns_is_an_empty_grid_not_an_arrow_error() {
        // An empty Mongo result and a statement without a result set report no columns.
        let batch = host_cells_to_record_batch(&[], 0, Vec::new()).expect("zero-column batch");
        assert_eq!((batch.num_columns(), batch.num_rows()), (0, 0));
        let source = ArrowGridSource::new(vec![Arc::new(batch)]);
        assert_eq!((source.column_count(), source.row_count()), (0, 0));
        // The stated row count survives without columns.
        let batch = host_cells_to_record_batch(&[], 3, Vec::new()).expect("zero-column rows");
        assert_eq!((batch.num_columns(), batch.num_rows()), (0, 3));
    }

    #[test]
    fn converts_driver_cells_to_utf8_columns_and_ignores_invalid_coordinates() {
        let columns = vec![
            engine_driver_api::PageColumn {
                id: "_id".into(),
                name: "_id".into(),
                data_type: "object_id".into(),
                nullable: false,
            },
            engine_driver_api::PageColumn {
                id: "name".into(),
                name: "name".into(),
                data_type: "string".into(),
                nullable: true,
            },
        ];
        let cells = vec![
            engine_driver_host::HostCell {
                row: 0,
                column: 0,
                length: 1,
                flags: 0,
                text: "1".into(),
            },
            engine_driver_host::HostCell {
                row: 0,
                column: 1,
                length: 6,
                flags: 0,
                text: "東京".into(),
            },
            engine_driver_host::HostCell {
                row: 4,
                column: 8,
                length: 0,
                flags: 0,
                text: "ignored".into(),
            },
        ];

        let batch = host_cells_to_record_batch(&columns, 2, cells).expect("driver batch");
        let source = ArrowGridSource::new(vec![Arc::new(batch)]);

        assert_eq!(source.column_count(), 2);
        assert_eq!(source.row_count(), 2);
        assert_eq!(
            source.cell_text(CellPosition { row: 0, column: 0 }),
            Some("1".into())
        );
        assert_eq!(
            source.cell_text(CellPosition { row: 0, column: 1 }),
            Some("東京".into())
        );
        assert_eq!(
            source.cell_text(CellPosition { row: 1, column: 1 }),
            Some("NULL".into())
        );
    }

    #[test]
    fn pre_render_truncates_oversized_cells() {
        let huge = "x".repeat(MAX_CELL_DISPLAY_BYTES * 2);
        let schema = Arc::new(Schema::new(vec![Field::new("blob", DataType::Utf8, false)]));
        let record = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec![huge.as_str()])) as arrow_array::ArrayRef],
        )
        .expect("valid batch");
        let source = ArrowGridSource::new(vec![Arc::new(record)]);

        let cell = source
            .cell_text(CellPosition { row: 0, column: 0 })
            .expect("cell exists");
        let cell_str: &str = cell.as_ref();
        assert!(
            cell_str.len() <= MAX_CELL_DISPLAY_BYTES + 3,
            "truncated cell must not exceed cap + ellipsis, got {} bytes",
            cell_str.len()
        );
        assert!(
            cell_str.ends_with('…'),
            "truncated cell must end with ellipsis"
        );
    }

    #[test]
    fn full_cell_text_returns_untruncated_original_for_oversized_cells() {
        let huge = "x".repeat(MAX_CELL_DISPLAY_BYTES * 2);
        let schema = Arc::new(Schema::new(vec![Field::new("blob", DataType::Utf8, false)]));
        let record = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec![huge.as_str()])) as arrow_array::ArrayRef],
        )
        .expect("valid batch");
        let source = ArrowGridSource::new(vec![Arc::new(record)]);

        let display = source
            .cell_text(CellPosition { row: 0, column: 0 })
            .expect("display cell exists");
        assert!(display.as_ref().ends_with('…'), "display is truncated");

        let full = source
            .full_cell_text(CellPosition { row: 0, column: 0 })
            .expect("full cell exists");
        assert_eq!(
            full.len(),
            MAX_CELL_DISPLAY_BYTES * 2,
            "full_cell_text returns the original untruncated value"
        );
        assert!(!full.ends_with('…'), "full text has no ellipsis");
    }

    #[test]
    fn full_cell_text_matches_display_for_normal_cells() {
        let schema = Arc::new(Schema::new(vec![Field::new("name", DataType::Utf8, false)]));
        let record = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec!["Ada"])) as arrow_array::ArrayRef],
        )
        .expect("valid batch");
        let source = ArrowGridSource::new(vec![Arc::new(record)]);

        let display: String = source
            .cell_text(CellPosition { row: 0, column: 0 })
            .map(|s| s.to_string())
            .unwrap_or_default();
        let full = source
            .full_cell_text(CellPosition { row: 0, column: 0 })
            .unwrap_or_default();
        assert_eq!(display, full, "non-truncated cell: display == full");
    }

    #[test]
    fn formats_date32_values_in_display_and_full_paths() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "created",
            DataType::Date32,
            false,
        )]));
        let record = RecordBatch::try_new(
            schema,
            vec![Arc::new(Date32Array::from(vec![0, 20_000, -1])) as arrow_array::ArrayRef],
        )
        .expect("valid date batch");
        let source = ArrowGridSource::new(vec![Arc::new(record)]);

        for (row, expected) in [(0, "1970-01-01"), (1, "2024-10-04"), (2, "1969-12-31")] {
            let display = source
                .cell_text(CellPosition { row, column: 0 })
                .expect("display cell")
                .to_string();
            let full = source
                .full_cell_text(CellPosition { row, column: 0 })
                .expect("full cell");
            assert_eq!(display, expected);
            assert_eq!(full, expected);
            assert_eq!(display, full, "display and full paths must agree");
        }
    }

    #[test]
    fn formats_date64_values_with_arrow_temporal_conversion() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "created",
            DataType::Date64,
            false,
        )]));
        let record = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Date64Array::from(vec![0, 20_000_i64 * 86_400_000]))
                    as arrow_array::ArrayRef,
            ],
        )
        .expect("valid date batch");
        let source = ArrowGridSource::new(vec![Arc::new(record)]);

        assert_eq!(
            source.full_cell_text(CellPosition { row: 0, column: 0 }),
            Some("1970-01-01".into())
        );
        assert_eq!(
            source.full_cell_text(CellPosition { row: 1, column: 0 }),
            Some("2024-10-04".into())
        );
    }

    #[test]
    fn preserves_null_date_values_through_grid_display() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "created",
            DataType::Date32,
            true,
        )]));
        let record = RecordBatch::try_new(
            schema,
            vec![Arc::new(Date32Array::from(vec![Some(0), None])) as arrow_array::ArrayRef],
        )
        .expect("valid nullable date batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        assert_eq!(
            grid.cell_text(CellPosition { row: 0, column: 0 }),
            Some("1970-01-01".into())
        );
        assert_eq!(
            grid.full_cell_text(CellPosition { row: 0, column: 0 }),
            Some("1970-01-01".into())
        );
        assert_eq!(
            grid.cell_text(CellPosition { row: 1, column: 0 }),
            Some("NULL".into())
        );
        assert_eq!(
            grid.full_cell_text(CellPosition { row: 1, column: 0 }),
            Some("NULL".into())
        );
        assert!(grid.cell_is_null(CellPosition { row: 1, column: 0 }));
    }

    #[test]
    fn uses_readable_raw_fallback_for_invalid_date_values() {
        let date32 = Date32Array::from(vec![i32::MAX, i32::MIN]);
        assert_eq!(
            array_value_to_string(&date32, 0),
            "<Date32 raw value: 2147483647>"
        );
        assert_eq!(
            array_value_to_string(&date32, 1),
            "<Date32 raw value: -2147483648>"
        );

        let date64 = Date64Array::from(vec![i64::MAX, i64::MIN]);
        assert_eq!(
            array_value_to_string(&date64, 0),
            "<Date64 raw value: 9223372036854775807>"
        );
        assert_eq!(
            array_value_to_string(&date64, 1),
            "<Date64 raw value: -9223372036854775808>"
        );
    }

    #[test]
    fn cell_is_null_distinguishes_null_from_the_text_null() {
        let schema = Arc::new(Schema::new(vec![Field::new("value", DataType::Utf8, true)]));
        let record = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec![Some("NULL"), None])) as arrow_array::ArrayRef],
        )
        .expect("valid batch");
        let source = ArrowGridSource::new(vec![Arc::new(record)]);

        assert!(!source.cell_is_null(CellPosition { row: 0, column: 0 }));
        assert!(source.cell_is_null(CellPosition { row: 1, column: 0 }));
    }

    #[test]
    fn tsv_copy_returns_full_text_of_truncated_cells() {
        let huge = "y".repeat(MAX_CELL_DISPLAY_BYTES * 2);
        let schema = Arc::new(Schema::new(vec![Field::new("blob", DataType::Utf8, false)]));
        let record = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec![huge.as_str()])) as arrow_array::ArrayRef],
        )
        .expect("valid batch");
        let grid = crate::grid::GridState::new(ArrowGridSource::new(vec![Arc::new(record)]));

        let mut selection = crate::grid::GridSelection::default();
        selection.select(CellPosition { row: 0, column: 0 });

        let tsv = crate::grid::selection_as_tsv(&grid, &selection);
        assert_eq!(
            tsv.len(),
            MAX_CELL_DISPLAY_BYTES * 2,
            "TSV copy must contain full untruncated text, not the display version"
        );
        assert!(!tsv.ends_with('…'), "TSV copy has no ellipsis");
    }

    fn cast(array: arrow_array::ArrayRef, to: DataType) -> arrow_array::ArrayRef {
        arrow_cast::cast::cast(&array, &to).expect("cast")
    }

    fn text_of(array: &arrow_array::ArrayRef, row: usize) -> String {
        array_value_to_string(array.as_ref(), row)
    }

    #[test]
    fn every_arrow_integer_renders_its_exact_value() {
        let cases: Vec<(arrow_array::ArrayRef, Vec<&str>)> = vec![
            (
                Arc::new(Int8Array::from(vec![i8::MIN, 0, i8::MAX])),
                vec!["-128", "0", "127"],
            ),
            (
                Arc::new(Int16Array::from(vec![i16::MIN, i16::MAX])),
                vec!["-32768", "32767"],
            ),
            (
                Arc::new(Int32Array::from(vec![i32::MIN, i32::MAX])),
                vec!["-2147483648", "2147483647"],
            ),
            (
                Arc::new(Int64Array::from(vec![i64::MIN, i64::MAX])),
                vec!["-9223372036854775808", "9223372036854775807"],
            ),
            (
                Arc::new(UInt8Array::from(vec![0, u8::MAX])),
                vec!["0", "255"],
            ),
            (Arc::new(UInt16Array::from(vec![u16::MAX])), vec!["65535"]),
            (
                Arc::new(UInt32Array::from(vec![u32::MAX])),
                vec!["4294967295"],
            ),
            (
                Arc::new(UInt64Array::from(vec![u64::MAX])),
                vec!["18446744073709551615"],
            ),
            (
                Arc::new(BooleanArray::from(vec![true, false])),
                vec!["true", "false"],
            ),
        ];
        for (array, expected) in cases {
            for (row, expected) in expected.into_iter().enumerate() {
                assert_eq!(text_of(&array, row), expected, "{:?}", array.data_type());
            }
        }
    }

    #[test]
    fn floats_print_the_shortest_text_that_reads_back_in_a_readable_range() {
        let f32s: arrow_array::ArrayRef = Arc::new(arrow_array::Float32Array::from(vec![
            0.1,
            -0.0,
            16_777_216.0,
            1e-10,
            1e30,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ]));
        let expected32 = [
            "0.1",
            "-0",
            "16777216",
            "1e-10",
            "1e30",
            "NaN",
            "Infinity",
            "-Infinity",
        ];
        for (row, expected) in expected32.into_iter().enumerate() {
            assert_eq!(text_of(&f32s, row), expected, "f32 row {row}");
        }
        // Float16 renders the shortest text that reads back as the same half.
        let halves = cast(f32s.clone(), DataType::Float16);
        assert_eq!(text_of(&halves, 0), "0.1");
        assert_eq!(text_of(&halves, 2), "Infinity", "out of the half range");
        let values = [
            1.5_f32,
            65504.0,
            1.0 / 3.0,
            6.0e-8,
            -2.5e-5,
            1024.5,
            0.0,
            -0.0,
        ];
        let halves: arrow_array::ArrayRef = Arc::new(
            values
                .into_iter()
                .map(Half::from_f32)
                .collect::<Float16Array>(),
        );
        let shown: Vec<_> = (0..values.len()).map(|row| text_of(&halves, row)).collect();
        // 65500 is the shortest text in the interval that rounds to the maximum, 65504.
        assert_eq!(shown.join(" "), "1.5 65500 0.3333 6e-8 -0.000025 1024 0 -0");
        // Every finite half reads back as itself from the text shown.
        for bits in 0..=u16::MAX {
            let value = Half::from_bits(bits);
            if value.is_finite() {
                let one: arrow_array::ArrayRef = Arc::new(Float16Array::from(vec![value]));
                let text = text_of(&one, 0);
                let back: f64 = text.parse().expect("a plain or exponent number");
                assert_eq!(Half::from_f64(back).to_bits(), bits, "{text}");
            }
        }
        let doubles = [
            (0.1, "0.1"),
            (-0.0, "-0"),
            (0.0, "0"),
            (1e300, "1e300"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e308"),
            (1e21, "1e21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (1e-7, "0.0000001"),
            (1e-8, "1e-8"),
            (0.30000000000000004, "0.30000000000000004"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        let array: arrow_array::ArrayRef = Arc::new(Float64Array::from(
            doubles.iter().map(|(value, _)| *value).collect::<Vec<_>>(),
        ));
        for (row, (value, expected)) in doubles.iter().enumerate() {
            let text = text_of(&array, row);
            assert_eq!(&text, expected);
            if value.is_finite() {
                let read: f64 = text.parse().expect("text reads back");
                assert_eq!(
                    read.to_bits(),
                    value.to_bits(),
                    "{text} must read back to the same f64"
                );
            }
        }
        // f32 text reads back to the same f32 too.
        for row in 0..5 {
            let value = f32s
                .as_any()
                .downcast_ref::<arrow_array::Float32Array>()
                .unwrap()
                .value(row);
            assert_eq!(
                text_of(&f32s, row).parse::<f32>().unwrap().to_bits(),
                value.to_bits()
            );
        }
    }

    #[test]
    fn decimals_keep_every_digit_scale_and_sign_without_a_float() {
        let d128 = |values: Vec<i128>, precision, scale| -> arrow_array::ArrayRef {
            Arc::new(
                arrow_array::Decimal128Array::from(values)
                    .with_precision_and_scale(precision, scale)
                    .unwrap(),
            )
        };
        let array = d128(vec![12_350, -5, 0, 100, i128::MAX, i128::MIN + 1], 38, 2);
        let expected = [
            "123.50",
            "-0.05",
            "0.00",
            "1.00",
            "1701411834604692317316873037158841057.27",
            "-1701411834604692317316873037158841057.27",
        ];
        for (row, expected) in expected.into_iter().enumerate() {
            assert_eq!(text_of(&array, row), expected);
        }
        assert_eq!(text_of(&d128(vec![42, -42], 10, 0), 1), "-42");
        // A negative scale appends the zeros the stored value stands for.
        assert_eq!(text_of(&d128(vec![12, 0], 10, -3), 0), "12000");
        assert_eq!(text_of(&d128(vec![12, 0], 10, -3), 1), "0");
        // Decimal256 beyond i128.
        let wide = cast(
            Arc::new(StringArray::from(vec![
                "12345678901234567890123456789012345678901234567890.1234500",
                "-0.0000001",
            ])),
            DataType::Decimal256(76, 7),
        );
        assert_eq!(
            text_of(&wide, 0),
            "12345678901234567890123456789012345678901234567890.1234500"
        );
        assert_eq!(text_of(&wide, 1), "-0.0000001");
    }

    #[test]
    fn timestamps_of_every_unit_keep_their_fraction_and_never_panic() {
        use arrow_array::{
            TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
            TimestampSecondArray,
        };
        let s: arrow_array::ArrayRef =
            Arc::new(TimestampSecondArray::from(vec![0, 1_700_000_000, -1]));
        assert_eq!(text_of(&s, 0), "1970-01-01 00:00:00");
        assert_eq!(text_of(&s, 1), "2023-11-14 22:13:20");
        assert_eq!(text_of(&s, 2), "1969-12-31 23:59:59");
        let ms: arrow_array::ArrayRef =
            Arc::new(TimestampMillisecondArray::from(vec![1_250, -1, 1_000]));
        assert_eq!(text_of(&ms, 0), "1970-01-01 00:00:01.250");
        assert_eq!(text_of(&ms, 1), "1969-12-31 23:59:59.999");
        assert_eq!(
            text_of(&ms, 2),
            "1970-01-01 00:00:01",
            "no fraction when zero"
        );
        let us: arrow_array::ArrayRef =
            Arc::new(TimestampMicrosecondArray::from(vec![1_000_001, -1]));
        assert_eq!(text_of(&us, 0), "1970-01-01 00:00:01.000001");
        assert_eq!(text_of(&us, 1), "1969-12-31 23:59:59.999999");
        // Nanoseconds before 1970, with a zone: the offset is part of the text.
        let ns = TimestampNanosecondArray::from(vec![-1, -86_400_000_000_001, i64::MIN, i64::MAX])
            .with_timezone("+02:00");
        let ns: arrow_array::ArrayRef = Arc::new(ns);
        assert_eq!(text_of(&ns, 0), "1970-01-01 01:59:59.999999999+02:00");
        assert_eq!(text_of(&ns, 1), "1969-12-31 01:59:59.999999999+02:00");
        assert_eq!(text_of(&ns, 2), "1677-09-21 02:12:43.145224192+02:00");
        assert_eq!(text_of(&ns, 3), "2262-04-12 01:47:16.854775807+02:00");
        // UTC, and a zone name this build has no database for (read as UTC).
        let utc: arrow_array::ArrayRef =
            Arc::new(TimestampSecondArray::from(vec![86_400]).with_timezone("UTC"));
        assert_eq!(text_of(&utc, 0), "1970-01-02 00:00:00+00:00");
        let named: arrow_array::ArrayRef =
            Arc::new(TimestampSecondArray::from(vec![86_400]).with_timezone("America/Montreal"));
        assert_eq!(text_of(&named, 0), "1970-01-02 00:00:00 UTC");
        // Out of the calendar's range: the raw number, no panic.
        let far: arrow_array::ArrayRef =
            Arc::new(TimestampSecondArray::from(vec![i64::MAX, i64::MIN]).with_timezone("-05:00"));
        assert_eq!(
            text_of(&far, 0),
            format!("<Timestamp raw value: {}>", i64::MAX)
        );
        assert_eq!(
            text_of(&far, 1),
            format!("<Timestamp raw value: {}>", i64::MIN)
        );
    }

    #[test]
    fn times_durations_intervals_and_nested_types_use_the_arrow_formatter() {
        use arrow_array::types::{Int32Type, IntervalMonthDayNanoType, IntervalYearMonthType};
        use arrow_array::{
            DictionaryArray, DurationSecondArray, FixedSizeListArray, IntervalDayTimeArray,
            IntervalMonthDayNanoArray, IntervalYearMonthArray, ListArray, StructArray,
            Time32SecondArray, Time64NanosecondArray,
        };
        let time32: arrow_array::ArrayRef = Arc::new(Time32SecondArray::from(vec![3_661]));
        assert_eq!(text_of(&time32, 0), "01:01:01");
        let time64: arrow_array::ArrayRef =
            Arc::new(Time64NanosecondArray::from(vec![3_661_000_000_001]));
        assert_eq!(text_of(&time64, 0), "01:01:01.000000001");
        let duration: arrow_array::ArrayRef = Arc::new(DurationSecondArray::from(vec![90_061]));
        assert!(
            text_of(&duration, 0).starts_with("1 days 1 hours 1 mins 1"),
            "{}",
            text_of(&duration, 0)
        );
        let year_month: arrow_array::ArrayRef = Arc::new(IntervalYearMonthArray::from(vec![
            IntervalYearMonthType::make_value(1, 2),
        ]));
        assert!(
            text_of(&year_month, 0).contains("mons"),
            "{}",
            text_of(&year_month, 0)
        );
        let day_time: arrow_array::ArrayRef = Arc::new(IntervalDayTimeArray::from(vec![
            arrow_array::types::IntervalDayTimeType::make_value(3, 4_000),
        ]));
        assert!(
            text_of(&day_time, 0).contains("3 days"),
            "{}",
            text_of(&day_time, 0)
        );
        let month_day_nano: arrow_array::ArrayRef =
            Arc::new(IntervalMonthDayNanoArray::from(vec![
                IntervalMonthDayNanoType::make_value(14, 3, 1_000_000_001),
            ]));
        assert!(
            text_of(&month_day_nano, 0).contains("3 days"),
            "{}",
            text_of(&month_day_nano, 0)
        );

        let list: arrow_array::ArrayRef =
            Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
                Some(vec![Some(1), None, Some(3)]),
                Some(vec![]),
            ]));
        assert_eq!(text_of(&list, 0), "[1, NULL, 3]");
        assert_eq!(text_of(&list, 1), "[]");
        let large = cast(
            list.clone(),
            DataType::LargeList(Arc::new(Field::new("item", DataType::Int32, true))),
        );
        assert!(matches!(large.data_type(), DataType::LargeList(_)));
        assert_eq!(text_of(&large, 0), "[1, NULL, 3]");
        let fixed: arrow_array::ArrayRef =
            Arc::new(FixedSizeListArray::from_iter_primitive::<Int32Type, _, _>(
                vec![Some(vec![Some(7), Some(8)])],
                2,
            ));
        assert_eq!(text_of(&fixed, 0), "[7, 8]");

        let strukt: arrow_array::ArrayRef = Arc::new(StructArray::from(vec![
            (
                Arc::new(Field::new("a", DataType::Int32, true)),
                Arc::new(Int32Array::from(vec![1])) as arrow_array::ArrayRef,
            ),
            (
                Arc::new(Field::new("b", DataType::Utf8, true)),
                Arc::new(StringArray::from(vec!["x"])) as arrow_array::ArrayRef,
            ),
        ]));
        assert_eq!(text_of(&strukt, 0), "{a: 1, b: x}");

        let mut builder = arrow_array::builder::MapBuilder::new(
            None,
            arrow_array::builder::StringBuilder::new(),
            arrow_array::builder::Int32Builder::new(),
        );
        builder.keys().append_value("k1");
        builder.values().append_value(10);
        builder.keys().append_value("k2");
        builder.values().append_value(20);
        builder.append(true).unwrap();
        let map: arrow_array::ArrayRef = Arc::new(builder.finish());
        assert_eq!(text_of(&map, 0), "{k1: 10, k2: 20}");

        let dictionary: arrow_array::ArrayRef = Arc::new(
            DictionaryArray::<arrow_array::types::Int8Type>::try_new(
                arrow_array::Int8Array::from(vec![Some(1), None, Some(0), Some(2)]),
                Arc::new(StringArray::from(vec![Some("a"), Some("b"), None])),
            )
            .unwrap(),
        );
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(
                Arc::new(Schema::new(vec![Field::new(
                    "d",
                    dictionary.data_type().clone(),
                    true,
                )])),
                vec![dictionary],
            )
            .unwrap(),
        )]);
        let shown: Vec<String> = (0..4)
            .map(|row| {
                source
                    .cell_text(CellPosition { row, column: 0 })
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            shown,
            ["b", "NULL", "a", "NULL"],
            "key null and value null are both NULL"
        );
        assert!(source.cell_is_null(CellPosition { row: 3, column: 0 }));
    }

    #[test]
    fn a_null_element_inside_a_nested_value_renders_as_null() {
        use arrow_array::builder::{ListBuilder, MapBuilder, StringBuilder};
        use arrow_array::StructArray;
        // Array(Nullable(String)): the NULL element is not an empty slot.
        let mut strings = ListBuilder::new(StringBuilder::new());
        strings.values().append_value("a");
        strings.values().append_null();
        strings.values().append_value("c");
        strings.append(true);
        strings.values().append_null();
        strings.append(true);
        strings.append_null();
        let strings: arrow_array::ArrayRef = Arc::new(strings.finish());
        assert_eq!(text_of(&strings, 0), "[a, NULL, c]");
        assert_eq!(text_of(&strings, 1), "[NULL]");
        // A NULL list element of a list of lists.
        let mut outer = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
        outer.values().values().append_value("a");
        outer.values().values().append_null();
        outer.values().append(true);
        outer.values().append_null();
        outer.append(true);
        let nested: arrow_array::ArrayRef = Arc::new(outer.finish());
        assert_eq!(text_of(&nested, 0), "[[a, NULL], NULL]");
        // Struct fields and Map values.
        let strukt: arrow_array::ArrayRef = Arc::new(StructArray::from(vec![
            (
                Arc::new(Field::new("a", DataType::Utf8, true)),
                Arc::new(StringArray::from(vec![None::<&str>])) as arrow_array::ArrayRef,
            ),
            (
                Arc::new(Field::new("b", DataType::Int32, true)),
                Arc::new(Int32Array::from(vec![None])) as arrow_array::ArrayRef,
            ),
        ]));
        assert_eq!(text_of(&strukt, 0), "{a: NULL, b: NULL}");
        let mut builder = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        builder.keys().append_value("k1");
        builder.values().append_null();
        builder.keys().append_value("k2");
        builder.values().append_value("v");
        builder.append(true).unwrap();
        let map: arrow_array::ArrayRef = Arc::new(builder.finish());
        assert_eq!(text_of(&map, 0), "{k1: NULL, k2: v}");
        // A dictionary-encoded element whose key is NULL.
        let dictionary = cast(
            strings,
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Utf8)),
                true,
            ))),
        );
        assert_eq!(text_of(&dictionary, 0), "[a, NULL, c]");
    }

    #[test]
    fn null_typed_columns_and_other_binary_layouts_render() {
        use arrow_array::{FixedSizeBinaryArray, NullArray};
        let schema = Arc::new(Schema::new(vec![
            Field::new("n", DataType::Null, true),
            Field::new("v", DataType::Utf8View, true),
            Field::new("lb", DataType::LargeBinary, true),
            Field::new("fb", DataType::FixedSizeBinary(2), true),
            Field::new("b", DataType::Binary, true),
        ]));
        let view = cast(
            Arc::new(StringArray::from(vec![Some("a view"), None])),
            DataType::Utf8View,
        );
        let lb: arrow_array::ArrayRef =
            Arc::new(LargeBinaryArray::from(vec![Some(&b"\x00\xff"[..]), None]));
        let fb: arrow_array::ArrayRef = Arc::new(
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                vec![Some(vec![0xde_u8, 0xad]), None].into_iter(),
                2,
            )
            .unwrap(),
        );
        let b: arrow_array::ArrayRef =
            Arc::new(BinaryArray::from(vec![Some(&b"hello"[..]), Some(&b""[..])]));
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(schema, vec![Arc::new(NullArray::new(2)), view, lb, fb, b])
                .unwrap(),
        )]);
        let cell = |row, column| CellPosition { row, column };
        // Every cell of a Null column is NULL, for the grid, the pane and the find scan.
        for row in 0..2 {
            assert!(source.cell_is_null(cell(row, 0)), "row {row}");
            assert_eq!(source.cell_text(cell(row, 0)).as_deref(), Some("NULL"));
            assert_eq!(source.full_cell_text(cell(row, 0)).as_deref(), Some("NULL"));
        }
        assert_eq!(source.cell_text(cell(0, 1)).as_deref(), Some("a view"));
        assert!(source.cell_is_null(cell(1, 1)));
        // A SQL byte string is `0x` + hex, whatever its layout; empty is `0x`.
        assert_eq!(source.cell_text(cell(0, 2)).as_deref(), Some("0x00ff"));
        assert_eq!(source.cell_text(cell(0, 3)).as_deref(), Some("0xdead"));
        assert_eq!(
            source.cell_text(cell(0, 4)).as_deref(),
            Some("0x68656c6c6f")
        );
        assert_eq!(source.cell_text(cell(1, 4)).as_deref(), Some("0x"));
        assert!(source.cell_is_null(cell(1, 3)));
        assert_eq!(source.column_type(2), Some("binary"));
        assert_eq!(source.column_type(3), Some("binary"));
        // Different byte strings never share a text, `\x00` the text included.
        let text_cell: arrow_array::ArrayRef = Arc::new(StringArray::from(vec!["\\x00"]));
        let byte_cell: arrow_array::ArrayRef = Arc::new(BinaryArray::from(vec![&b"\x00"[..]]));
        assert_ne!(text_of(&text_cell, 0), text_of(&byte_cell, 0));
        // Find scans see the hex text of a binary column and skip NULL columns.
        let (hits, _) = search(&source, "dead", &[0, 1, 2, 3, 4], 10);
        assert_eq!(hits, vec![cell(0, 3)]);
        let (hits, _) = search(&source, "null", &[0, 1], 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn a_long_binary_value_previews_its_first_bytes_and_size_and_copies_whole() {
        let bytes: Vec<u8> = (0..5_000u32).map(|n| (n % 251) as u8).collect();
        let schema = Arc::new(Schema::new(vec![Field::new("b", DataType::Binary, true)]));
        let values: arrow_array::ArrayRef =
            Arc::new(BinaryArray::from(vec![Some(bytes.as_slice())]));
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(schema, vec![values]).unwrap(),
        )]);
        let at = CellPosition { row: 0, column: 0 };
        let preview = source.cell_text(at).unwrap();
        assert!(preview.starts_with("0x000102"), "{}", &preview[..12]);
        assert!(preview.ends_with("… (4.9 KB)"), "{preview}");
        assert!(preview.len() < 100, "a cell never formats the whole value");
        let full = source.full_cell_text(at).unwrap();
        assert_eq!(
            full.len(),
            2 + 2 * bytes.len(),
            "the full text is the full hex"
        );
        let shown_hex = &preview[..2 + 2 * HEX_PREVIEW_BYTES];
        assert!(full.starts_with(shown_hex));
        // A short value is shown whole.
        let short: arrow_array::ArrayRef =
            Arc::new(BinaryArray::from(vec![Some(&bytes[..HEX_PREVIEW_BYTES])]));
        let schema = Arc::new(Schema::new(vec![Field::new("b", DataType::Binary, true)]));
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(schema, vec![short]).unwrap(),
        )]);
        let whole = source.cell_text(at).unwrap();
        assert_eq!(whole.len(), 2 + 2 * HEX_PREVIEW_BYTES);
        assert!(!whole.contains('…'));
    }

    #[test]
    fn a_postgres_bytea_text_previews_as_its_first_bytes_and_size() {
        let long = format!("\\x{}", "ab".repeat(5_000));
        let metadata = std::collections::HashMap::from([(NATIVE_TYPE_METADATA_KEY.to_string(), "bytea".to_string())]);
        let schema = Arc::new(Schema::new(vec![
            Field::new("b", DataType::Utf8, true).with_metadata(metadata),
            Field::new("t", DataType::Utf8, true),
        ]));
        let values: Vec<arrow_array::ArrayRef> = vec![
            Arc::new(StringArray::from(vec![long.as_str(), "\\xdead"])),
            Arc::new(StringArray::from(vec![long.as_str(), "x"])),
        ];
        let source = ArrowGridSource::new(vec![Arc::new(RecordBatch::try_new(schema, values).unwrap())]);
        let cell = |row, column| CellPosition { row, column };
        let preview = source.cell_text(cell(0, 0)).unwrap();
        assert_eq!(preview.as_ref(), format!("\\x{}… (4.9 KB)", "ab".repeat(HEX_PREVIEW_BYTES)));
        assert_eq!(source.cell_text(cell(1, 0)).unwrap().as_ref(), "\\xdead", "a short value is whole");
        assert!(source.cell_text(cell(0, 1)).unwrap().len() > 1000, "only bytea columns are compacted");
        assert_eq!(source.full_cell_text(cell(0, 0)).unwrap(), long, "copy has every byte");
    }

    #[test]
    fn binary_cells_of_every_layout_and_a_sqlite_blob_expose_their_bytes() {
        let cell = |row, column| CellPosition { row, column };
        let blobs: Vec<arrow_array::ArrayRef> = vec![
            Arc::new(BinaryArray::from(vec![Some(&b"\x01\x02"[..]), None])),
            Arc::new(LargeBinaryArray::from(vec![Some(&b"\x01\x02"[..]), None])),
            Arc::new(BinaryViewArray::from(vec![Some(&b"\x01\x02"[..]), None])),
            Arc::new(FixedSizeBinaryArray::try_from_iter(vec![&b"\x01\x02"[..], &b"\x03\x04"[..]].into_iter()).unwrap()),
        ];
        let fields: Vec<Field> = blobs
            .iter()
            .enumerate()
            .map(|(index, array)| Field::new(format!("b{index}"), array.data_type().clone(), true))
            .collect();
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(Arc::new(Schema::new(fields)), blobs).unwrap(),
        )]);
        for column in 0..3 {
            assert_eq!(source.cell_bytes(cell(0, column)), Some(&b"\x01\x02"[..]), "column {column}");
            assert_eq!(source.cell_bytes(cell(1, column)), None, "NULL has no bytes");
        }
        assert_eq!(source.cell_bytes(cell(1, 3)), Some(&b"\x03\x04"[..]));
        assert_eq!(source.cell_bytes(cell(9, 0)), None);

        // SQLite `mixed`: the BLOB and the invalid UTF-8 text have bytes, text does not.
        let mixed = mixed_column();
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(
                Arc::new(Schema::new(vec![Field::new("m", mixed.data_type().clone(), true)])),
                vec![mixed],
            )
            .unwrap(),
        )]);
        let by_class: Vec<(Option<CellClass>, Option<&[u8]>)> = (0..source.row_count())
            .map(|row| (source.cell_class(row, 0), source.cell_bytes(cell(row, 0))))
            .collect();
        assert!(by_class.contains(&(Some(CellClass::Blob), Some(&b"\x01\xff"[..]))), "{by_class:?}");
        assert!(by_class.contains(&(Some(CellClass::InvalidUtf8Text), Some(&b"\xff\xfe"[..]))));
        assert!(by_class.iter().filter(|(class, _)| matches!(class, Some(CellClass::Text | CellClass::Integer | CellClass::Real))).all(|(_, bytes)| bytes.is_none()));
    }

    pub(crate) fn mixed_column() -> arrow_array::ArrayRef {
        use arrow_schema::UnionFields;
        let children: Vec<arrow_array::ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![Some(1), None])),
            Arc::new(Float64Array::from(vec![2.5])),
            Arc::new(StringArray::from(vec!["1"])),
            Arc::new(BinaryArray::from(vec![&b"\x01\xff"[..]])),
            Arc::new(BinaryArray::from(vec![&b"\xff\xfe"[..]])),
        ];
        let fields = UnionFields::new(
            vec![0, 1, 2, 3, 4],
            vec![
                Field::new("integer", DataType::Int64, true),
                Field::new("real", DataType::Float64, true),
                Field::new("text", DataType::Utf8, true),
                Field::new("blob", DataType::Binary, true),
                Field::new("text_bytes", DataType::Binary, true),
            ],
        );
        Arc::new(
            UnionArray::try_new(
                fields,
                vec![0_i8, 2, 1, 3, 0, 4].into(),
                Some(vec![0_i32, 0, 0, 0, 1, 0].into()),
                children,
            )
            .expect("a dense union"),
        )
    }

    #[test]
    fn a_mixed_sqlite_column_renders_each_cell_by_its_own_class() {
        let column = mixed_column();
        let source = ArrowGridSource::new(vec![Arc::new(
            RecordBatch::try_new(
                Arc::new(Schema::new(vec![Field::new(
                    "v",
                    column.data_type().clone(),
                    true,
                )])),
                vec![column],
            )
            .unwrap(),
        )]);
        let cell = |row| CellPosition { row, column: 0 };
        let shown: Vec<String> = (0..6)
            .map(|row| source.cell_text(cell(row)).unwrap().to_string())
            .collect();
        assert_eq!(shown, ["1", "1", "2.5", "0x01ff", "NULL", "0xfffe"]);
        // The INTEGER 1 and the TEXT '1' read alike in the grid; the class tells them apart.
        let classes: Vec<_> = (0..6).map(|row| source.cell_class(row, 0)).collect();
        assert_eq!(
            classes,
            [
                Some(CellClass::Integer),
                Some(CellClass::Text),
                Some(CellClass::Real),
                Some(CellClass::Blob),
                None,
                Some(CellClass::InvalidUtf8Text),
            ]
        );
        // NULL is the null of child 0, for the grid, the pane and the find scan.
        assert!(source.cell_is_null(cell(4)));
        assert!(!source.cell_is_null(cell(0)));
        assert_eq!(source.full_cell_text(cell(4)).as_deref(), Some("NULL"));
        assert_eq!(source.column_type(0), Some("mixed"));
        let (hits, _) = search(&source, "01ff", &[0], 10);
        assert_eq!(hits, vec![cell(3)]);
        let (hits, _) = search(&source, "null", &[0], 10);
        assert!(hits.is_empty());
    }
}
