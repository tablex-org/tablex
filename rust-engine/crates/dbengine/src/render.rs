//! Backend cell value formatter for the main table view.
//!
//! Converts a single BSON value to a display string suitable for an `NSTableView`
//! cell, and renders a batch of cells for a visible (row, col) range into one
//! contiguous UTF-8 text buffer. Phase 1 of the backend-render perf plan: keep
//! `NSTableView`, but move per-cell text preparation off the main thread.
//!
//! Display format intentionally mirrors the Swift `ArrowTable` formatter in
//! `TableLeaf/Models/ArrowTable.swift`:
//!   - Double -> shortest round-trip digits with a decimal point or exponent (`5.0`, `1.5e300`)
//!   - String -> raw UTF-8
//!   - Int32/Int64 -> decimal
//!   - Bool -> "true" / "false"
//!   - DateTime -> RFC3339 with millis, UTC ('Z')
//!   - Null -> "null"
//!   - Binary -> "<binary N bytes>"
//!   - ObjectId -> hex

use bson::{Bson, Document};

pub const CELL_FLAG_NULL: u32 = 1 << 0;
pub const CELL_FLAG_TRUNCATED: u32 = 1 << 1;
pub const CELL_FLAG_MISSING: u32 = 1 << 2;
pub const CELL_FLAG_LARGE: u32 = 1 << 3;
pub const CELL_FLAG_BINARY: u32 = 1 << 4;

pub const DEFAULT_MAX_BYTES_PER_CELL: usize = 4096;
pub const LARGE_CELL_THRESHOLD: usize = 8192;

/// Per-cell metadata for a rendered batch. Offsets and lengths index into the
/// batch's `text_buffer` (a single UTF-8 buffer shared by all cells).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedCell {
    pub row: usize,
    pub column: usize,
    pub offset: usize,
    pub length: usize,
    pub flags: u32,
}

/// Owned batch returned to the FFI boundary and then leaked through raw
/// pointers; freed by `dbengine_free_rendered_cell_batch`.
#[derive(Debug)]
pub struct RenderedCellBatch {
    pub cells: Vec<RenderedCell>,
    pub text_buffer: Vec<u8>,
}

/// Format one BSON value for a table cell. Returns `(display_string, flags)`.
///
/// `None` means the field is missing from the document (key not present);
/// distinct from `Bson::Null` (key present, value is null).
pub fn format_cell_display_value(value: Option<&Bson>, max_bytes: usize) -> (String, u32) {
    let Some(val) = value else {
        return (String::new(), CELL_FLAG_MISSING);
    };
    let mut flags = 0u32;
    let raw = match val {
        Bson::Null => {
            flags |= CELL_FLAG_NULL;
            "null".to_string()
        }
        Bson::ObjectId(oid) => oid.to_hex(),
        Bson::String(s) => s.clone(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(n) => {
            let mut text = String::new();
            // Writing into a `String` cannot fail.
            let _ = engine_mongo_script::shell_value::write_double(&mut text, *n);
            text
        }
        Bson::Decimal128(d) => d.to_string(),
        Bson::Boolean(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Bson::DateTime(dt) => {
            let chrono_dt = chrono::DateTime::from_timestamp_millis(dt.timestamp_millis())
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
            chrono_dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }
        Bson::Array(arr) => serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string()),
        Bson::Document(doc) => serde_json::to_string(doc).unwrap_or_else(|_| "{}".to_string()),
        Bson::Binary(bin) => {
            flags |= CELL_FLAG_BINARY;
            format!("<binary {} bytes>", bin.bytes.len())
        }
        Bson::Timestamp(ts) => {
            let mut text = String::new();
            let _ =
                engine_mongo_script::shell_value::write_timestamp(&mut text, ts.time, ts.increment);
            text
        }
        Bson::RegularExpression(re) => format!("/{}/{}", re.pattern, re.options),
        Bson::Undefined => "undefined".to_string(),
        other => engine_mongo_script::shell_value::to_shell(other, false),
    };

    if raw.len() > LARGE_CELL_THRESHOLD {
        flags |= CELL_FLAG_LARGE;
    }

    let (final_s, was_truncated) = truncate_utf8(&raw, max_bytes);
    if was_truncated {
        flags |= CELL_FLAG_TRUNCATED;
    }
    (final_s, flags)
}

/// Truncate `s` so the result fits in `max_bytes`, appending "..." if cut.
/// Cuts on a UTF-8 char boundary to keep the output valid UTF-8.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    if max_bytes == 0 {
        return (String::new(), true);
    }
    if max_bytes < 3 {
        let mut out = String::new();
        for c in s.chars().take(max_bytes) {
            out.push(c);
        }
        return (out, true);
    }
    let mut cut = max_bytes - 3;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&s[..cut]);
    out.push_str("...");
    (out, true)
}

/// Render a batch of cells for the given (row, col) range.
///
/// `column_names[col]` is the BSON key used to read each cell. Rows with
/// out-of-range indices are skipped silently. Empty window returns an empty
/// batch.
pub fn render_cell_text_batch(
    docs: &[Document],
    column_names: &[String],
    row_start: usize,
    row_end_exclusive: usize,
    col_start: usize,
    col_end_exclusive: usize,
    max_bytes_per_cell: usize,
) -> RenderedCellBatch {
    let mut cells: Vec<RenderedCell> = Vec::new();
    let mut text_buffer: Vec<u8> = Vec::new();

    if row_start >= row_end_exclusive || col_start >= col_end_exclusive {
        return RenderedCellBatch { cells, text_buffer };
    }
    let row_end = row_end_exclusive.min(docs.len());
    if row_start >= row_end {
        return RenderedCellBatch { cells, text_buffer };
    }
    let col_end = col_end_exclusive.min(column_names.len());
    if col_start >= col_end {
        return RenderedCellBatch { cells, text_buffer };
    }

    for row in row_start..row_end {
        let doc = &docs[row];
        for col in col_start..col_end {
            let key = &column_names[col];
            let value = doc.get(key);
            let (s, flags) = format_cell_display_value(value, max_bytes_per_cell);
            let offset = text_buffer.len();
            text_buffer.extend_from_slice(s.as_bytes());
            let length = s.len();
            cells.push(RenderedCell {
                row,
                column: col,
                offset,
                length,
                flags,
            });
        }
    }

    RenderedCellBatch { cells, text_buffer }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn decode<'a>(text: &'a [u8], cell: &RenderedCell) -> &'a str {
        std::str::from_utf8(&text[cell.offset..cell.offset + cell.length]).unwrap()
    }

    #[test]
    fn formats_scalar_values_for_cells() {
        let d = doc! {
            "_id": "507f1f77bcf86cd799439011",
            "name": "alice",
            "age": 30i64,
            "score": 1.5f64,
            "active": true,
            "deleted": null,
        };
        assert_eq!(
            format_cell_display_value(d.get("name"), DEFAULT_MAX_BYTES_PER_CELL),
            ("alice".to_string(), 0)
        );
        assert_eq!(
            format_cell_display_value(d.get("age"), DEFAULT_MAX_BYTES_PER_CELL),
            ("30".to_string(), 0)
        );
        let (s, _) = format_cell_display_value(d.get("score"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "1.5");
        let whole = doc! { "x": 5.0f64, "y": 0.1f64 + 0.2f64, "z": 1.5e300f64 };
        assert_eq!(format_cell_display_value(whole.get("x"), 64).0, "5.0");
        assert_eq!(
            format_cell_display_value(whole.get("y"), 64).0,
            "0.30000000000000004"
        );
        assert_eq!(format_cell_display_value(whole.get("z"), 64).0, "1.5e300");
        assert_eq!(
            format_cell_display_value(d.get("active"), DEFAULT_MAX_BYTES_PER_CELL),
            ("true".to_string(), 0)
        );
        let (s, flags) = format_cell_display_value(d.get("deleted"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "null");
        assert_ne!(flags & CELL_FLAG_NULL, 0);
        let (s, flags) = format_cell_display_value(d.get("missing"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "");
        assert_ne!(flags & CELL_FLAG_MISSING, 0);
    }

    #[test]
    fn formats_objectid_as_hex() {
        let d = doc! { "_id": "507f1f77bcf86cd799439011" };
        let (s, _) = format_cell_display_value(d.get("_id"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "507f1f77bcf86cd799439011");
    }

    #[test]
    fn formats_double_nan_and_infinity() {
        let nan_d = doc! { "x": f64::NAN };
        let (s, _) = format_cell_display_value(nan_d.get("x"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "NaN");
        let pos_d = doc! { "x": f64::INFINITY };
        let (s, _) = format_cell_display_value(pos_d.get("x"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "Infinity");
        let neg_d = doc! { "x": f64::NEG_INFINITY };
        let (s, _) = format_cell_display_value(neg_d.get("x"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "-Infinity");
    }

    #[test]
    fn marks_binary_with_flag() {
        let bin = bson::Binary {
            subtype: bson::spec::BinarySubtype::Generic,
            bytes: vec![1, 2, 3, 4, 5],
        };
        let d = doc! { "blob": bin };
        let (s, flags) = format_cell_display_value(d.get("blob"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_eq!(s, "<binary 5 bytes>");
        assert_ne!(flags & CELL_FLAG_BINARY, 0);
    }

    #[test]
    fn truncates_at_utf8_boundary() {
        let s = "héllo world";
        assert_eq!(s.len(), 12); // h(1) é(2) l(1) l(1) o(1) ' '(1) w(1) o(1) r(1) l(1) d(1)
                                 // 8 - 3 = 5 -> "héll" (5 bytes) + "..." (3 bytes) = 8 bytes
        let (out, was) = truncate_utf8(s, 8);
        assert!(was);
        assert_eq!(out, "héll...");
        // 7 - 3 = 4 -> "hél" (4 bytes) + "..." (3 bytes) = 7 bytes
        let (out, was) = truncate_utf8(s, 7);
        assert!(was);
        assert_eq!(out, "hél...");
        // exact length: no truncation
        let (out, was) = truncate_utf8(s, 12);
        assert!(!was);
        assert_eq!(out, "héllo world");
        // 3 - 3 = 0 -> "" + "..." = 3 bytes
        let (out, was) = truncate_utf8(s, 3);
        assert!(was);
        assert_eq!(out, "...");
    }

    #[test]
    fn marks_truncated_cell() {
        let long_string: String = "x".repeat(5000);
        let d = doc! { "bio": long_string };
        let (s, flags) = format_cell_display_value(d.get("bio"), 128);
        assert_ne!(flags & CELL_FLAG_TRUNCATED, 0);
        assert!(s.ends_with("..."));
        assert!(s.len() <= 128);
    }

    #[test]
    fn marks_large_cell() {
        let long_string: String = "y".repeat(LARGE_CELL_THRESHOLD + 100);
        let d = doc! { "big": long_string };
        let (_s, flags) = format_cell_display_value(d.get("big"), DEFAULT_MAX_BYTES_PER_CELL);
        assert_ne!(flags & CELL_FLAG_LARGE, 0);
    }

    #[test]
    fn renders_batch_for_visible_window() {
        let docs = vec![
            doc! { "name": "alice", "age": 30i64 },
            doc! { "name": "bob", "age": 25i64 },
            doc! { "name": "carol", "age": 28i64 },
        ];
        let columns = vec!["name".to_string(), "age".to_string()];

        let batch = render_cell_text_batch(&docs, &columns, 0, 2, 0, 2, 64);
        assert_eq!(batch.cells.len(), 4);
        assert_eq!(batch.cells[0].row, 0);
        assert_eq!(batch.cells[0].column, 0);
        assert_eq!(batch.cells[0].flags, 0);
        assert_eq!(decode(&batch.text_buffer, &batch.cells[0]), "alice");
        assert_eq!(batch.cells[1].row, 0);
        assert_eq!(batch.cells[1].column, 1);
        assert_eq!(decode(&batch.text_buffer, &batch.cells[1]), "30");
        assert_eq!(batch.cells[2].row, 1);
        assert_eq!(batch.cells[2].column, 0);
        assert_eq!(decode(&batch.text_buffer, &batch.cells[2]), "bob");
        assert_eq!(batch.cells[3].row, 1);
        assert_eq!(batch.cells[3].column, 1);
        assert_eq!(decode(&batch.text_buffer, &batch.cells[3]), "25");
    }

    #[test]
    fn batch_marks_missing_field() {
        let docs = vec![doc! { "name": "alice" }];
        let columns = vec!["name".to_string(), "age".to_string()];
        let batch = render_cell_text_batch(&docs, &columns, 0, 1, 0, 2, 64);
        assert_eq!(batch.cells.len(), 2);
        assert_ne!(batch.cells[1].flags & CELL_FLAG_MISSING, 0);
        assert_eq!(decode(&batch.text_buffer, &batch.cells[1]), "");
    }

    #[test]
    fn batch_handles_out_of_range_window() {
        let docs = vec![doc! { "name": "alice" }];
        let columns = vec!["name".to_string()];
        // row end past doc count
        let batch = render_cell_text_batch(&docs, &columns, 0, 10, 0, 1, 64);
        assert_eq!(batch.cells.len(), 1);
        // col end past column count
        let batch = render_cell_text_batch(&docs, &columns, 0, 1, 0, 10, 64);
        assert_eq!(batch.cells.len(), 1);
    }

    #[test]
    fn empty_window_returns_empty_batch() {
        let docs = vec![doc! { "name": "alice" }];
        let columns = vec!["name".to_string()];
        let batch = render_cell_text_batch(&docs, &columns, 0, 0, 0, 1, 64);
        assert_eq!(batch.cells.len(), 0);
        assert_eq!(batch.text_buffer.len(), 0);
        let batch = render_cell_text_batch(&docs, &columns, 0, 1, 1, 1, 64);
        assert_eq!(batch.cells.len(), 0);
    }

    #[test]
    fn batch_text_buffer_is_valid_utf8_for_unicode_values() {
        let docs = vec![doc! { "name": "héllo wörld 你好" }];
        let columns = vec!["name".to_string()];
        let batch = render_cell_text_batch(&docs, &columns, 0, 1, 0, 1, 64);
        assert_eq!(batch.cells.len(), 1);
        let s = decode(&batch.text_buffer, &batch.cells[0]);
        assert!(std::str::from_utf8(batch.text_buffer.as_slice()).is_ok());
        assert!(s.contains("é") || s.contains("..."));
    }
}
