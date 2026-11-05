//! Cell text rendering shared by `render_cell_batch` and `inspect_row`.
//!
//! A [`CellFormatter`] is built once per rendered column (downcast and, for
//! uncommon types, `ArrayFormatter` construction happen there) and then writes
//! each cell into a reused `String`, so the per-cell path allocates nothing for
//! the common types.

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type, Int64Type};
use arrow_array::Int64Array;
use arrow_array::{Array, ArrayRef, BinaryArray, BooleanArray, Float32Array, Float64Array};
use arrow_array::{LargeBinaryArray, StringArray, UnionArray};
use arrow_buffer::NullBuffer;
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::DataType;
use std::fmt::Write;

type WriteCell<'a> = Box<dyn Fn(usize, &mut String) + 'a>;

static FORMAT_OPTIONS: FormatOptions<'static> = FormatOptions::new();

enum Kind<'a> {
    /// `DataType::Null`: every row is null, with no physical null buffer.
    Null,
    Int64(&'a Int64Array),
    /// Printed as an `f32`: the shortest text that reads back to the same
    /// single-precision value (`0.1`, not the widened `0.10000000149011612`).
    Float32(&'a Float32Array),
    Float64(&'a Float64Array),
    Boolean(&'a BooleanArray),
    Utf8(&'a StringArray),
    Binary(&'a BinaryArray),
    LargeBinary(&'a LargeBinaryArray),
    /// Everything else Arrow can format (Int32, dates, timestamps, decimals,
    /// large/view strings, lists, ...).
    Display(ArrayFormatter<'a>),
    /// Arrow cannot format this type.
    Unsupported,
    Custom(WriteCell<'a>),
    /// A union (SQLite's mixed storage classes): every cell is formatted by the
    /// formatter of the child it belongs to, so a blob in a mixed column reads
    /// like a blob in a plain one. Nulls are the children's.
    Union {
        array: &'a UnionArray,
        /// Indexed by type id.
        children: Vec<Option<CellFormatter<'a>>>,
    },
}

/// Writes the display text of one column's cells.
///
/// Nulls always render as `NULL`. Int64, Float32, Float64, Boolean and Utf8 print as
/// their natural text; Binary/LargeBinary as a hex literal (`X'ABCD'`); other
/// types through Arrow's display formatter (`<value>` if Arrow has none).
pub struct CellFormatter<'a> {
    /// Logical nulls, computed once per column (covers dictionary, union and
    /// run-end arrays whose nulls are not in the physical buffer). `None` when
    /// the column has no nulls.
    nulls: Option<NullBuffer>,
    kind: Kind<'a>,
}

impl<'a> CellFormatter<'a> {
    /// The default formatter for `array`.
    pub fn new(array: &'a ArrayRef) -> Self {
        let kind = match array.data_type() {
            DataType::Null => Kind::Null,
            DataType::Int64 => Kind::Int64(array.as_primitive::<Int64Type>()),
            DataType::Float32 => Kind::Float32(array.as_primitive::<Float32Type>()),
            DataType::Float64 => Kind::Float64(array.as_primitive::<Float64Type>()),
            DataType::Boolean => Kind::Boolean(array.as_boolean()),
            DataType::Utf8 => Kind::Utf8(array.as_string::<i32>()),
            DataType::Binary => Kind::Binary(array.as_binary::<i32>()),
            DataType::LargeBinary => Kind::LargeBinary(array.as_binary::<i64>()),
            DataType::Union(fields, _) if array.as_any().is::<UnionArray>() => {
                let union = array
                    .as_any()
                    .downcast_ref::<UnionArray>()
                    .expect("checked above");
                let slots = fields
                    .iter()
                    .map(|(id, _)| id)
                    .max()
                    .map_or(0, |id| id as usize + 1);
                let mut children: Vec<Option<CellFormatter<'a>>> =
                    (0..slots).map(|_| None).collect();
                for (id, _) in fields.iter() {
                    if let Ok(slot) = usize::try_from(id) {
                        children[slot] = Some(CellFormatter::new(union.child(id)));
                    }
                }
                return Self {
                    nulls: None,
                    kind: Kind::Union {
                        array: union,
                        children,
                    },
                };
            }
            _ => match ArrayFormatter::try_new(array.as_ref(), &FORMAT_OPTIONS) {
                Ok(formatter) => Kind::Display(formatter),
                Err(_) => Kind::Unsupported,
            },
        };
        Self {
            nulls: logical_nulls(array),
            kind,
        }
    }

    /// A formatter that calls `write` for every non-null cell of `array`
    /// (nulls still render as `NULL`). `write` should not allocate.
    pub fn custom(array: &'a ArrayRef, write: impl Fn(usize, &mut String) + 'a) -> Self {
        Self {
            nulls: logical_nulls(array),
            kind: Kind::Custom(Box::new(write)),
        }
    }

    /// Whether the cell at `row` is SQL NULL. The text of a null cell is the
    /// word `NULL`, which a real text value can equal: consumers that must tell
    /// them apart ask this (`render_cell_batch` sets `CELL_FLAG_NULL` from it).
    pub fn is_null(&self, row: usize) -> bool {
        match &self.kind {
            Kind::Null => true,
            Kind::Union { array, children } => children
                .get(array.type_id(row) as usize)
                .and_then(Option::as_ref)
                .is_none_or(|child| child.is_null(array.value_offset(row))),
            _ => self.nulls.as_ref().is_some_and(|n| n.is_null(row)),
        }
    }

    /// Appends the text of `row` to `out`.
    pub fn write(&self, row: usize, out: &mut String) {
        if matches!(self.kind, Kind::Null) || self.nulls.as_ref().is_some_and(|n| n.is_null(row)) {
            out.push_str("NULL");
            return;
        }
        // `write!` into a `String` cannot fail for primitives.
        match &self.kind {
            Kind::Int64(array) => {
                let _ = write!(out, "{}", array.value(row));
            }
            Kind::Float32(array) => {
                let _ = write!(out, "{}", array.value(row));
            }
            Kind::Float64(array) => {
                let _ = write!(out, "{}", array.value(row));
            }
            Kind::Boolean(array) => {
                let _ = write!(out, "{}", array.value(row));
            }
            Kind::Null => unreachable!("null columns return above"),
            Kind::Utf8(array) => out.push_str(array.value(row)),
            Kind::Binary(array) => write_hex_blob_literal(out, array.value(row)),
            Kind::LargeBinary(array) => write_hex_blob_literal(out, array.value(row)),
            Kind::Display(formatter) => {
                let start = out.len();
                if write!(out, "{}", formatter.value(row)).is_err() {
                    out.truncate(start);
                    out.push_str("<value>");
                }
            }
            Kind::Unsupported => out.push_str("<value>"),
            Kind::Custom(write) => write(row, out),
            Kind::Union { array, children } => {
                match children
                    .get(array.type_id(row) as usize)
                    .and_then(Option::as_ref)
                {
                    Some(child) => child.write(array.value_offset(row), out),
                    None => out.push_str("<value>"),
                }
            }
        }
    }
}

fn logical_nulls(array: &ArrayRef) -> Option<NullBuffer> {
    match array.data_type() {
        // All rows are null; avoid materializing a bitmap as long as the column.
        DataType::Null => None,
        _ => array.logical_nulls(),
    }
}

/// Writes `X'0A1B'` (upper-case hex) for `bytes`.
pub fn write_hex_blob_literal(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.reserve(bytes.len().saturating_mul(2).saturating_add(3));
    out.push_str("X'");
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out.push('\'');
}

/// Convenience for tests and cold paths: the default text of one cell.
/// Builds a formatter per call; use [`CellFormatter`] for more than one cell.
pub fn default_cell_text(array: &ArrayRef, row: usize) -> String {
    let mut text = String::new();
    CellFormatter::new(array).write(row, &mut text);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{
        Date32Array, Decimal128Array, Int32Array, LargeStringArray, ListArray,
        TimestampMillisecondArray,
    };
    use std::sync::Arc;

    #[test]
    fn default_text_covers_the_native_sqlite_type_set() {
        let ints: ArrayRef = Arc::new(Int64Array::from(vec![Some(7), None]));
        assert_eq!(default_cell_text(&ints, 0), "7");
        assert_eq!(default_cell_text(&ints, 1), "NULL");
        let reals: ArrayRef = Arc::new(Float64Array::from(vec![3.5]));
        assert_eq!(default_cell_text(&reals, 0), "3.5");
        // A single-precision column prints its own shortest form, not the f64
        // widening of it.
        let singles: ArrayRef = Arc::new(Float32Array::from(vec![
            Some(0.1f32),
            Some(2.71),
            Some(1.5),
            None,
        ]));
        let cells: Vec<String> = (0..4).map(|row| default_cell_text(&singles, row)).collect();
        assert_eq!(cells, ["0.1", "2.71", "1.5", "NULL"]);
        let bools: ArrayRef = Arc::new(BooleanArray::from(vec![true]));
        assert_eq!(default_cell_text(&bools, 0), "true");
        let text: ArrayRef = Arc::new(StringArray::from(vec!["hi"]));
        assert_eq!(default_cell_text(&text, 0), "hi");
        let blobs: ArrayRef = Arc::new(BinaryArray::from_vec(vec![&[0xab, 0x01][..]]));
        assert_eq!(default_cell_text(&blobs, 0), "X'AB01'");
    }

    #[test]
    fn other_arrow_types_use_the_display_formatter() {
        let cases: Vec<(ArrayRef, &str)> = vec![
            (Arc::new(Int32Array::from(vec![-5])), "-5"),
            (Arc::new(LargeStringArray::from(vec!["big"])), "big"),
            (Arc::new(Date32Array::from(vec![19_000])), "2022-01-08"),
            (
                Arc::new(TimestampMillisecondArray::from(vec![1_000])),
                "1970-01-01T00:00:01",
            ),
            (
                Arc::new(
                    Decimal128Array::from(vec![12345])
                        .with_precision_and_scale(10, 2)
                        .unwrap(),
                ),
                "123.45",
            ),
            (
                Arc::new(ListArray::from_iter_primitive::<
                    arrow_array::types::Int32Type,
                    _,
                    _,
                >(vec![Some(vec![Some(1), Some(2)])])),
                "[1, 2]",
            ),
        ];
        for (array, expected) in cases {
            assert_eq!(
                default_cell_text(&array, 0),
                expected,
                "{:?}",
                array.data_type()
            );
        }
        let nulls: ArrayRef = Arc::new(Int32Array::from(vec![None::<i32>]));
        assert_eq!(default_cell_text(&nulls, 0), "NULL");
    }

    #[test]
    fn null_type_and_logical_nulls_render_as_null() {
        use arrow_array::{DictionaryArray, NullArray};
        let nulls: ArrayRef = Arc::new(NullArray::new(3));
        for row in 0..3 {
            assert_eq!(default_cell_text(&nulls, row), "NULL");
        }
        // A dictionary whose null lives in the *values*, not the key buffer.
        let keys = arrow_array::Int8Array::from(vec![0, 1]);
        let values: ArrayRef = Arc::new(StringArray::from(vec![Some("a"), None]));
        let dictionary: ArrayRef = Arc::new(
            DictionaryArray::<arrow_array::types::Int8Type>::try_new(keys, values).unwrap(),
        );
        assert_eq!(default_cell_text(&dictionary, 0), "a");
        assert_eq!(default_cell_text(&dictionary, 1), "NULL");
    }

    #[test]
    fn sql_null_is_told_apart_from_the_text_null() {
        let text: ArrayRef = Arc::new(StringArray::from(vec![Some("NULL"), None, Some("x")]));
        let formatter = CellFormatter::new(&text);
        let rendered: Vec<(String, bool)> = (0..3)
            .map(|row| {
                let mut out = String::new();
                formatter.write(row, &mut out);
                (out, formatter.is_null(row))
            })
            .collect();
        // Same text, different null bit.
        assert_eq!(rendered[0], ("NULL".into(), false));
        assert_eq!(rendered[1], ("NULL".into(), true));
        assert_eq!(rendered[2], ("x".into(), false));
        let nulls: ArrayRef = Arc::new(arrow_array::NullArray::new(1));
        assert!(CellFormatter::new(&nulls).is_null(0));
        let ints: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None]));
        let custom = CellFormatter::custom(&ints, |_, out| out.push('#'));
        assert!(!custom.is_null(0) && custom.is_null(1));
    }

    #[test]
    fn a_union_formats_each_cell_by_its_own_class() {
        use arrow_array::{BinaryArray, Float64Array};
        use arrow_buffer::ScalarBuffer;
        use arrow_schema::{Field, UnionFields};
        let fields = UnionFields::new(
            [0_i8, 1, 2, 3],
            [
                Field::new("integer", DataType::Int64, true),
                Field::new("real", DataType::Float64, true),
                Field::new("text", DataType::Utf8, true),
                Field::new("blob", DataType::Binary, true),
            ],
        );
        let children: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![Some(1), None])),
            Arc::new(Float64Array::from(vec![1.5])),
            Arc::new(StringArray::from(vec!["1", "NULL"])),
            Arc::new(BinaryArray::from_vec(vec![&[0xab][..]])),
        ];
        // rows: integer 1, text '1', real 1.5, blob, SQL NULL, text 'NULL'
        let type_ids = ScalarBuffer::from(vec![0_i8, 2, 1, 3, 0, 2]);
        let offsets = ScalarBuffer::from(vec![0_i32, 0, 0, 0, 1, 1]);
        let union: ArrayRef = Arc::new(
            UnionArray::try_new(fields, type_ids, Some(offsets), children).expect("union"),
        );
        let formatter = CellFormatter::new(&union);
        let rows: Vec<(String, bool)> = (0..6)
            .map(|row| {
                let mut out = String::new();
                formatter.write(row, &mut out);
                (out, formatter.is_null(row))
            })
            .collect();
        assert_eq!(rows[0], ("1".into(), false));
        assert_eq!(rows[1], ("1".into(), false));
        assert_eq!(rows[2], ("1.5".into(), false));
        assert_eq!(rows[3], ("X'AB'".into(), false));
        assert_eq!(rows[4], ("NULL".into(), true));
        assert_eq!(rows[5], ("NULL".into(), false));
    }

    #[test]
    fn custom_formatter_is_used_for_non_null_cells_only() {
        let ints: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None]));
        let formatter = CellFormatter::custom(&ints, |row, out| out.push_str(&format!("#{row}")));
        let mut out = String::new();
        formatter.write(0, &mut out);
        formatter.write(1, &mut out);
        assert_eq!(out, "#0NULL");
    }
}
