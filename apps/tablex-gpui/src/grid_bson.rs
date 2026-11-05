use crate::grid::{
    CellPosition, CellState, GridSearchScan, GridSearchStatus, GridSource, PreviewCache,
};
use engine_mongo_script::shell_value;
use bson::raw::{RawBsonRef, RawDocument};
use engine_driver_api::PageColumn;
use engine_driver_host::PageLease;
use gpui::SharedString;
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

const MAX_PREVIEW_BYTES: usize = 2048;

/// What a document without the column's field shows. mongosh leaves such a
/// field out, so the cell is a dim dash: unlike `NULL` (a present null, drawn
/// with the configured NULL text) and unlike the text `NULL` (a string).
pub const MISSING_TEXT: &str = "\u{2014}";

/// Binary payloads up to this size are written out in a cell; larger ones show
/// their size only (the detail pane has the whole value).
const MAX_INLINE_BINARY_BYTES: usize = 64;

#[derive(Clone)]
pub struct BsonGridSource {
    page: PageLease,
    columns: Vec<PageColumn>,
    row_count: usize,
    previews: Arc<Mutex<PreviewCache>>,
}

impl fmt::Debug for BsonGridSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BsonGridSource")
            .field("generation", &self.page.generation())
            .field("row_count", &self.row_count)
            .field("column_count", &self.columns.len())
            .finish()
    }
}

impl BsonGridSource {
    pub fn new(page: PageLease, columns: Vec<PageColumn>, row_count: u64) -> Self {
        Self {
            page,
            columns,
            row_count: row_count as usize,
            previews: Arc::new(Mutex::new(PreviewCache::default())),
        }
    }

    #[cfg(test)]
    pub fn page(&self) -> &PageLease {
        &self.page
    }

    fn raw_slice(&self, row: usize) -> Option<engine_driver_host::PageSlice> {
        self.page.indexed_slice(row as u64).ok()
    }

    /// `None`: the document has no such field.
    fn render_cell(&self, position: CellPosition) -> Option<String> {
        let column = self.columns.get(position.column)?;
        let slice = self.raw_slice(position.row)?;
        let document = RawDocument::from_bytes(slice.as_bytes()).ok()?;
        let value = document.get(&column.name).ok().flatten()?;
        // A column of one BSON type needs no marker on each value (the header
        // names the type); in a mixed one an Int64 must not read as an Int32,
        // an ObjectId or a Date as a string.
        Some(raw_value_preview(
            value,
            matches!(column.data_type.as_str(), "mixed" | "number"),
        ))
    }
}

impl GridSource for BsonGridSource {
    fn row_count(&self) -> usize {
        self.row_count
    }

    fn column_count(&self) -> usize {
        self.columns.len()
    }

    fn column_name(&self, column: usize) -> Option<&str> {
        self.columns.get(column).map(|column| column.name.as_str())
    }

    fn column_type(&self, column: usize) -> Option<&str> {
        self.columns
            .get(column)
            .map(|column| column.data_type.as_str())
    }

    fn cell_text(&self, position: CellPosition) -> Option<SharedString> {
        if position.row >= self.row_count || position.column >= self.columns.len() {
            return None;
        }
        if let Ok(previews) = self.previews.lock() {
            if let Some(value) = previews.get(position) {
                return Some(value);
            }
        }
        let value = self
            .render_cell(position)
            .map_or_else(|| SharedString::from(MISSING_TEXT), SharedString::from);
        if let Ok(mut previews) = self.previews.lock() {
            previews.insert(position, value.clone());
        }
        Some(value)
    }

    fn full_cell_text(&self, position: CellPosition) -> Option<String> {
        let column = self.columns.get(position.column)?;
        let slice = self.raw_slice(position.row)?;
        let document = RawDocument::from_bytes(slice.as_bytes()).ok()?;
        let value = document.get(&column.name).ok().flatten()?;
        Some(value.display_text())
    }

    fn cell_is_null(&self, position: CellPosition) -> bool {
        self.cell_state(position) == CellState::Null
    }

    fn cell_state(&self, position: CellPosition) -> CellState {
        let Some(column) = self.columns.get(position.column) else {
            return CellState::Value;
        };
        let Some(slice) = self.raw_slice(position.row) else {
            return CellState::Value;
        };
        let Ok(document) = RawDocument::from_bytes(slice.as_bytes()) else {
            return CellState::Value;
        };
        match document.get(&column.name) {
            Ok(Some(RawBsonRef::Null)) => CellState::Null,
            Ok(None) => CellState::Missing,
            _ => CellState::Value,
        }
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

    /// Row-major scan over the raw BSON documents: each document is walked
    /// once (not once per column), strings are compared as borrowed `&str`,
    /// other values are formatted with the grid's display formatting into one
    /// reused buffer. Null and absent fields never match; the preview cache is
    /// not touched.
    fn search_cells(&self, scan: &GridSearchScan<'_>, out: &mut Vec<CellPosition>) -> GridSearchStatus {
        let mut display_by_field: HashMap<&str, usize> = HashMap::with_capacity(scan.columns.len());
        for (display, &source) in scan.columns.iter().enumerate() {
            if let Some(column) = self.columns.get(source) {
                display_by_field.entry(column.name.as_str()).or_insert(display);
            }
        }
        let mut buffer = String::new();
        let mut scratch = String::new();
        let mut hits: Vec<usize> = Vec::new();
        for row in scan.rows(self.row_count) {
            let Some(slice) = self.raw_slice(row.source) else {
                continue;
            };
            let Ok(document) = RawDocument::from_bytes(slice.as_bytes()) else {
                continue;
            };
            hits.clear();
            for (key, value) in document.iter().map_while(Result::ok) {
                let Some(&display) = display_by_field.get(key) else {
                    continue;
                };
                let text: &str = match value {
                    RawBsonRef::Null => continue,
                    RawBsonRef::String(text)
                    | RawBsonRef::Symbol(text)
                    | RawBsonRef::JavaScriptCode(text) => text,
                    other => {
                        buffer.clear();
                        let _ = other.write_display(&mut buffer);
                        buffer.as_str()
                    }
                };
                if scan.query.matches(text, &mut scratch) {
                    hits.push(display);
                }
            }
            // Documents list fields in their own order; emit by column.
            hits.sort_unstable();
            hits.dedup();
            for &column in &hits {
                if scan.push(out, CellPosition { row: row.display, column }) {
                    return GridSearchStatus::Capped;
                }
            }
        }
        scan.finish()
    }
}

/// Display text of BSON values: the single formatting definition shared by
/// the grid cell text, the copy / sort / export text and the find scan. Types
/// read the way mongosh prints them, so none can be taken for another: a
/// Double always has a decimal point or an exponent (`5.0`), a Binary is
/// `BinData(0, '…')` / `UUID('…')`, a Regex `/pattern/flags`, a Timestamp
/// `Timestamp({ t: 1, i: 2 })`, Symbol / Code / MinKey / MaxKey /
/// undefined by name. Where a column mixes types (`marked`), an Int64 is
/// `Long('5')`, a Decimal128 `Decimal128('5')`, a Date
/// `ISODate('2020-01-02T03:04:05.123Z')` and an ObjectId `ObjectId('…')`; in a
/// column of one type they are plain digits, ISO text and hex (the text copied,
/// sorted and summed).
trait RawBsonRefExt {
    /// Appends the display text to `out` (no intermediate allocation except
    /// for the odd `format!`-style type).
    fn write_display_in(self, out: &mut String, marked: bool) -> fmt::Result;

    fn write_display(self, out: &mut String) -> fmt::Result
    where
        Self: Sized,
    {
        self.write_display_in(out, false)
    }

    fn display_text(self) -> String
    where
        Self: Sized,
    {
        let mut text = String::new();
        // Writing into a `String` cannot fail.
        let _ = self.write_display(&mut text);
        text
    }
}

impl RawBsonRefExt for RawBsonRef<'_> {
    fn write_display_in(self, out: &mut String, marked: bool) -> fmt::Result {
        match self {
            RawBsonRef::String(value) => out.write_str(value),
            RawBsonRef::Symbol(value) => write!(out, "Symbol({})", quoted(value)),
            RawBsonRef::JavaScriptCode(value) => write!(out, "Code({})", quoted(value)),
            RawBsonRef::Boolean(value) => write!(out, "{value}"),
            RawBsonRef::Int32(value) => write!(out, "{value}"),
            RawBsonRef::Int64(value) if marked => write!(out, "Long('{value}')"),
            RawBsonRef::Int64(value) => write!(out, "{value}"),
            RawBsonRef::Double(value) => shell_value::write_double(out, value),
            // `ObjectId`'s Display is its lowercase hex.
            RawBsonRef::ObjectId(value) if marked => write!(out, "ObjectId('{value}')"),
            RawBsonRef::ObjectId(value) => write!(out, "{value}"),
            RawBsonRef::DateTime(value) if marked => {
                shell_value::write_date(out, value.timestamp_millis())
            }
            RawBsonRef::DateTime(value) => {
                match shell_value::iso_date_text(value.timestamp_millis()) {
                    Some(text) => out.write_str(&text),
                    None => write!(out, "Date({})", value.timestamp_millis()),
                }
            }
            RawBsonRef::Decimal128(value) if marked => write!(out, "Decimal128('{value}')"),
            RawBsonRef::Decimal128(value) => write!(out, "{value}"),
            RawBsonRef::Null => out.write_str("NULL"),
            RawBsonRef::Binary(value) => shell_value::write_binary(
                out,
                u8::from(value.subtype),
                value.bytes,
                Some(MAX_INLINE_BINARY_BYTES),
            ),
            RawBsonRef::Array(_) => out.write_str("<array>"),
            RawBsonRef::Document(_) => out.write_str("<object>"),
            RawBsonRef::Timestamp(value) => {
                shell_value::write_timestamp(out, value.time, value.increment)
            }
            RawBsonRef::RegularExpression(value) => {
                shell_value::write_regex(out, value.pattern, value.options)
            }
            RawBsonRef::JavaScriptCodeWithScope(value) => {
                write!(out, "Code({}, {{ … }})", quoted(value.code))
            }
            RawBsonRef::Undefined => out.write_str("undefined"),
            RawBsonRef::MaxKey => out.write_str("MaxKey()"),
            RawBsonRef::MinKey => out.write_str("MinKey()"),
            RawBsonRef::DbPointer(_) => out.write_str("<db-pointer>"),
        }
    }
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn raw_value_preview(value: RawBsonRef<'_>, marked: bool) -> String {
    match value {
        RawBsonRef::String(value) => cap_preview(value),
        value => {
            let mut text = String::new();
            // Writing into a `String` cannot fail.
            let _ = value.write_display_in(&mut text, marked);
            cap_preview(&text)
        }
    }
}

fn cap_preview(value: &str) -> String {
    if value.len() <= MAX_PREVIEW_BYTES {
        return value.to_string();
    }
    let mut end = MAX_PREVIEW_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// A grid source over `documents` for tests (any module of the crate).
#[cfg(test)]
pub(crate) fn test_source(
    documents: Vec<bson::Document>,
    columns: &[(&str, &str)],
) -> BsonGridSource {
    use engine_driver_api::{CPageLease, CPageSlice};
    use std::ffi::c_void;

    struct TestBacking {
        data: Vec<u8>,
        slices: Vec<CPageSlice>,
    }

    unsafe extern "C" fn release_page(context: *mut c_void) {
        if !context.is_null() {
            unsafe { drop(Box::from_raw(context.cast::<TestBacking>())) };
        }
    }

    let row_count = documents.len() as u64;
    let mut data = Vec::new();
    let mut slices = Vec::new();
    for document in documents {
        let bytes = bson::to_vec(&document).expect("BSON");
        let offset = data.len() as u64;
        data.extend_from_slice(&bytes);
        slices.push(CPageSlice {
            offset,
            length: bytes.len() as u64,
        });
    }
    let backing = Box::new(TestBacking { data, slices });
    let data_ptr = backing.data.as_ptr();
    let data_len = backing.data.len() as u64;
    let slice_ptr = backing.slices.as_ptr();
    let slice_count = backing.slices.len() as u64;
    let context = Box::into_raw(backing).cast::<c_void>();
    let page = PageLease::from_raw(CPageLease {
        data: data_ptr,
        data_len,
        generation: 1,
        release: Some(release_page),
        release_context: context,
        slices: slice_ptr,
        slice_count,
    })
    .expect("page");
    BsonGridSource::new(
        page,
        columns
            .iter()
            .map(|(name, data_type)| PageColumn {
                id: (*name).into(),
                name: (*name).into(),
                data_type: (*data_type).into(),
                nullable: true,
            })
            .collect(),
        row_count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> BsonGridSource {
        test_source(
            vec![
                bson::doc! { "_id": 1, "name": "Ada", "active": true, "empty": bson::Bson::Null },
                bson::doc! { "_id": 2, "name": "Lin", "active": false },
            ],
            &[
                ("_id", "int32"),
                ("name", "string"),
                ("empty", "null"),
                ("missing", "string"),
            ],
        )
    }

    #[test]
    fn reads_cells_from_raw_bson_without_document_materialization() {
        let source = source();
        assert_eq!(
            source.cell_text(CellPosition { row: 0, column: 1 }),
            Some("Ada".into())
        );
        assert_eq!(
            source.cell_text(CellPosition { row: 1, column: 0 }),
            Some("2".into())
        );
        assert_eq!(
            source.full_cell_text(CellPosition { row: 0, column: 1 }),
            Some("Ada".into())
        );
    }

    #[test]
    fn caches_only_display_previews_and_keeps_raw_page_shared() {
        let source = source();
        let clone = source.clone();
        let _ = source.cell_text(CellPosition { row: 0, column: 1 });
        assert_eq!(clone.page().slice_count(), 2);
    }

    fn search(
        source: &BsonGridSource,
        text: &str,
        columns: &[usize],
        row_order: Option<&[usize]>,
    ) -> Vec<CellPosition> {
        let query = crate::grid::GridSearchQuery::new(text, Default::default()).unwrap();
        let never = std::sync::atomic::AtomicBool::new(false);
        let scan = GridSearchScan {
            query: &query,
            columns,
            row_order,
            max_matches: 100,
            cancel: &never,
        };
        let mut out = Vec::new();
        source.search_cells(&scan, &mut out);
        out
    }

    #[test]
    fn search_walks_raw_documents_and_skips_null_and_missing_fields() {
        let source = source();
        let cell = |row, column| CellPosition { row, column };
        let all = [0, 1, 2, 3];
        assert_eq!(search(&source, "2", &all, None), [cell(1, 0)]);
        assert_eq!(search(&source, "ADA", &all, None), [cell(0, 1)]);
        // A BSON null and an absent field render as "NULL" but never match.
        assert!(search(&source, "null", &all, None).is_empty());
        // Display columns pick and reorder source columns.
        assert_eq!(search(&source, "ada", &[1, 0], None), [cell(0, 0)]);
        assert_eq!(search(&source, "2", &[1, 0], None), [cell(1, 1)]);
        assert!(search(&source, "ada", &[0, 2], None).is_empty());
        // A locally sorted page maps display rows to source rows.
        assert_eq!(search(&source, "lin", &all, Some(&[1, 0])), [cell(0, 1)]);
        assert_eq!(
            source.previews.lock().expect("preview cache").len(),
            0,
            "searching must not churn the bounded display cache"
        );
    }

    #[test]
    fn distinguishes_bson_null_from_a_missing_field() {
        let source = source();

        assert!(source.cell_is_null(CellPosition { row: 0, column: 2 }));
        assert!(!source.cell_is_null(CellPosition { row: 0, column: 3 }));
    }

    fn texts(source: &BsonGridSource, row: usize, columns: usize) -> Vec<String> {
        (0..columns)
            .map(|column| {
                source
                    .cell_text(CellPosition { row, column })
                    .expect("cell")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn every_bson_type_reads_the_way_mongosh_prints_it() {
        use bson::spec::BinarySubtype;
        use bson::{Binary, Bson, DateTime, Regex, Timestamp};
        let uuid = |subtype| Bson::Binary(Binary { subtype, bytes: (0..16).collect() });
        let document = bson::doc! {
            "i32": 5, "i64": 5_i64, "dbl": 5.0, "dbl_f": 1.5, "dbl_e": 1.5e300, "nan": f64::NAN,
            "ninf": f64::NEG_INFINITY, "dec": "5".parse::<bson::Decimal128>().unwrap(),
            "str": "5", "null_text": "NULL", "oid": bson::oid::ObjectId::parse_str("66d4f84d0000000000000003").unwrap(),
            "date": DateTime::from_millis(1_577_934_245_123),
            "bin": Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: vec![0, 1, 255] }),
            "big_bin": Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: vec![7; 100] }),
            "uuid": uuid(BinarySubtype::Uuid), "uuid_old": uuid(BinarySubtype::UuidOld),
            "re": Bson::RegularExpression(Regex { pattern: "^a".into(), options: "i".into() }),
            "ts": Bson::Timestamp(Timestamp { time: 1_700_000_000, increment: 7 }),
            "min": Bson::MinKey, "max": Bson::MaxKey, "undef": Bson::Undefined,
            "sym": Bson::Symbol("s".into()), "js": Bson::JavaScriptCode("x".into()),
            "bool": true,
        };
        let names: Vec<_> = document.keys().cloned().collect();
        let columns: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "mixed")).collect();
        let source = test_source(vec![document], &columns);
        let shown: std::collections::HashMap<_, _> = names
            .iter()
            .cloned()
            .zip(texts(&source, 0, names.len()))
            .collect();
        let expect = [
            ("i32", "5"), ("i64", "Long('5')"), ("dbl", "5.0"), ("dbl_f", "1.5"),
            ("dbl_e", "1.5e300"), ("nan", "NaN"), ("ninf", "-Infinity"),
            ("dec", "Decimal128('5')"), ("str", "5"), ("null_text", "NULL"),
            ("oid", "ObjectId('66d4f84d0000000000000003')"),
            ("date", "ISODate('2020-01-02T03:04:05.123Z')"),
            ("bin", "BinData(0, 'AAH/')"), ("big_bin", "BinData(0, <100 bytes>)"),
            ("uuid", "UUID('00010203-0405-0607-0809-0a0b0c0d0e0f')"),
            ("uuid_old", "UUID('00010203-0405-0607-0809-0a0b0c0d0e0f', 3)"),
            ("re", "/^a/i"), ("ts", "Timestamp({ t: 1700000000, i: 7 })"),
            ("min", "MinKey()"), ("max", "MaxKey()"), ("undef", "undefined"),
            ("sym", "Symbol(\"s\")"), ("js", "Code(\"x\")"), ("bool", "true"),
        ];
        for (name, text) in expect {
            assert_eq!(shown[name], text, "{name}");
        }
        // The types that read the same as a number or string are told apart.
        assert_ne!(shown["i32"], shown["i64"]);
        assert_ne!(shown["i32"], shown["dbl"]);
        assert_ne!(shown["i32"], shown["dec"]);
        assert_ne!(shown["str"], shown["i32"].replace('5', "\"5\""), "strings are raw text");
    }

    #[test]
    fn an_object_id_column_shows_bare_hex_and_a_mixed_one_marks_it() {
        let id = bson::oid::ObjectId::parse_str("66d4f84d0000000000000003").unwrap();
        let documents = vec![bson::doc! { "a": id, "b": id }];
        let source = test_source(documents, &[("a", "object_id"), ("b", "mixed")]);
        assert_eq!(texts(&source, 0, 2), ["66d4f84d0000000000000003", "ObjectId('66d4f84d0000000000000003')"]);
    }

    #[test]
    fn a_column_of_one_type_shows_plain_digits_dates_and_the_copy_text_matches() {
        use bson::{Bson, DateTime};
        let documents = vec![bson::doc! {
            "long": 5_i64, "dec": "5".parse::<bson::Decimal128>().unwrap(),
            "date": DateTime::from_millis(1_577_934_245_123), "dbl": 5.0,
        }];
        let source = test_source(
            documents,
            &[("long", "int64"), ("dec", "decimal128"), ("date", "date_time"), ("dbl", "double")],
        );
        assert_eq!(texts(&source, 0, 4), ["5", "5", "2020-01-02T03:04:05.123Z", "5.0"]);
        // What is copied, sorted and summed is the same plain text everywhere.
        let full: Vec<_> = (0..4)
            .map(|column| source.full_cell_text(CellPosition { row: 0, column }).unwrap())
            .collect();
        assert_eq!(full, ["5", "5", "2020-01-02T03:04:05.123Z", "5.0"]);
        let mixed = test_source(vec![bson::doc! { "v": Bson::Int64(5) }], &[("v", "number")]);
        assert_eq!(texts(&mixed, 0, 1), ["Long('5')"]);
        assert_eq!(mixed.full_cell_text(CellPosition { row: 0, column: 0 }).unwrap(), "5");
    }

    #[test]
    fn a_missing_field_a_null_and_the_text_null_are_three_different_cells() {
        let source = test_source(
            vec![
                bson::doc! { "f": bson::Bson::Null },
                bson::doc! { "f": "NULL" },
                bson::doc! { "other": 1 },
            ],
            &[("f", "string")],
        );
        let at = |row| CellPosition { row, column: 0 };
        assert_eq!(source.cell_state(at(0)), CellState::Null);
        assert_eq!(source.cell_state(at(1)), CellState::Value);
        assert_eq!(source.cell_state(at(2)), CellState::Missing);
        assert!(source.cell_is_null(at(0)) && !source.cell_is_null(at(1)) && !source.cell_is_null(at(2)));
        assert_eq!(source.cell_text(at(1)).unwrap(), "NULL");
        assert_eq!(source.cell_text(at(2)).unwrap(), MISSING_TEXT);
        assert_ne!(MISSING_TEXT, "NULL");
        assert!(source.full_cell_text(at(2)).is_none(), "nothing to copy from a missing field");
    }
}
