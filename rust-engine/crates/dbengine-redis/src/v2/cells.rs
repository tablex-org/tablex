//! Cell text and row inspection for Redis pages.

use super::key_page::{KeyContext, KeyType};
use super::RedisPage;
use crate::redis_key_bytes_to_raw;
use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use engine_driver_kit::api::{RowField, RowFields};
use engine_driver_kit::{CellFormatter, Page};
use std::borrow::Cow;

/// Display text of a byte string: valid UTF-8 without control characters as is;
/// otherwise control characters and non-UTF-8 bytes become `\xNN` (and, for
/// non-UTF-8 input only, a backslash becomes `\\`). Tab, newline and carriage
/// return are kept.
pub(super) fn display(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) if !has_control(text) => Cow::Borrowed(text),
        _ => {
            let mut text = String::new();
            write_display(bytes, &mut text);
            Cow::Owned(text)
        }
    }
}

fn has_control(text: &str) -> bool {
    text.bytes()
        .any(|byte| (byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r')) || byte == 0x7f)
}

/// Appends [`display`] of `bytes` to `out` (no allocation of its own).
pub(super) fn write_display(bytes: &[u8], out: &mut String) {
    match std::str::from_utf8(bytes) {
        Ok(text) if !has_control(text) => out.push_str(text),
        Ok(text) => {
            for ch in text.chars() {
                if ch.is_control() && !matches!(ch, '\t' | '\n' | '\r') && ch.is_ascii() {
                    push_hex_escape(ch as u8, out);
                } else {
                    out.push(ch);
                }
            }
        }
        Err(_) => write_escaped(bytes, out),
    }
}

fn push_hex_escape(byte: u8, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push_str("\\x");
    out.push(HEX[usize::from(byte >> 4)] as char);
    out.push(HEX[usize::from(byte & 0x0f)] as char);
}

fn write_escaped(bytes: &[u8], out: &mut String) {
    out.reserve(bytes.len());
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(byte as char),
            _ => push_hex_escape(byte, out),
        }
    }
}

/// Binary columns render through [`write_display`]; every other type uses the
/// kit default.
pub(super) fn formatter(batch: &RecordBatch, column: usize) -> CellFormatter<'_> {
    let array = batch.column(column);
    if array.data_type() == &DataType::Binary {
        let binary = array.as_binary::<i32>();
        CellFormatter::custom(array, move |row, out| write_display(binary.value(row), out))
    } else {
        CellFormatter::new(array)
    }
}

/// JSON value of one cell: text, number, `null`, or `{"base64": ..}` for bytes
/// that are not UTF-8.
fn cell_value(batch: &RecordBatch, column: usize, row: usize) -> serde_json::Value {
    let array = batch.column(column);
    if array.is_null(row) {
        return serde_json::Value::Null;
    }
    match array.data_type() {
        DataType::Binary => {
            let bytes = array.as_binary::<i32>().value(row);
            match std::str::from_utf8(bytes) {
                Ok(text) => serde_json::Value::String(text.to_owned()),
                Err(_) => serde_json::json!({ "base64": redis_key_bytes_to_raw(bytes) }),
            }
        }
        DataType::Int64 => serde_json::json!(array.as_primitive::<Int64Type>().value(row)),
        DataType::Float64 => {
            let score = array.as_primitive::<Float64Type>().value(row);
            serde_json::Number::from_f64(score)
                .map_or_else(|| serde_json::Value::String(score.to_string()), Into::into)
        }
        DataType::Utf8 => serde_json::Value::String(array.as_string::<i32>().value(row).to_owned()),
        _ => serde_json::Value::Null,
    }
}

fn bytes_at(batch: &RecordBatch, column: usize, row: usize) -> Option<&[u8]> {
    let array = batch.column(column);
    (array.data_type() == &DataType::Binary && !array.is_null(row))
        .then(|| array.as_binary::<i32>().value(row))
}

/// Whether the cell of `column` can be edited with a mutation of the key.
fn editable(key_type: KeyType, column: &str) -> bool {
    matches!(
        (key_type, column),
        (KeyType::String, "value")
            | (KeyType::Hash, "value")
            | (KeyType::List, "value")
            | (KeyType::ZSet, "score")
    )
}

/// The part of a mutation identity that names `row` inside the key.
fn row_identity(
    context: &KeyContext,
    batch: &RecordBatch,
    row: usize,
) -> serde_json::Map<String, serde_json::Value> {
    let mut identity = serde_json::Map::new();
    let raw = |column: usize| bytes_at(batch, column, row).map(redis_key_bytes_to_raw);
    match context.key_type {
        KeyType::String => {}
        KeyType::Hash => {
            identity.insert("field_raw".into(), raw(0).into());
        }
        KeyType::List => {
            identity.insert("index".into(), cell_value(batch, 0, row));
        }
        KeyType::Set | KeyType::ZSet => {
            identity.insert("member_raw".into(), raw(0).into());
        }
        KeyType::Stream => {
            identity.insert("entry_id".into(), cell_value(batch, 0, row));
        }
    }
    identity
}

pub(super) fn inspect_row(page: &Page<RedisPage>, row: usize) -> RowFields {
    let key = page.context.key.as_ref();
    let mut fields = Vec::with_capacity(page.columns.len());
    for (column, metadata) in page.columns.iter().enumerate() {
        let mut text = String::new();
        formatter(&page.batch, column).write(row, &mut text);
        fields.push(RowField {
            name: metadata.name.clone(),
            native_type: metadata.data_type.clone(),
            display_text: text,
            value: cell_value(&page.batch, column, row),
            editable: key.is_some_and(|key| editable(key.key_type, &metadata.name)),
        });
    }
    let mutation_identity = match key {
        Some(key) => {
            let mut identity = row_identity(key, &page.batch, row);
            identity.insert("db".into(), page.context.db.into());
            identity.insert("key_raw".into(), redis_key_bytes_to_raw(&key.key).into());
            if let Ok(text) = std::str::from_utf8(&key.key) {
                identity.insert("key".into(), text.into());
            }
            identity.insert("type".into(), key.key_type.as_str().into());
            identity.insert("ttl".into(), key.ttl.into());
            serde_json::Value::Object(identity)
        }
        None => serde_json::json!({}),
    };
    RowFields {
        page_id: page.page_id,
        row_index: row as u64,
        fields,
        mutation_identity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::BinaryBuilder;
    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{Field, Schema};
    use std::sync::Arc;

    #[test]
    fn display_keeps_utf8_and_escapes_other_bytes() {
        assert_eq!(display(b"C:\\path \xc3\xa9"), "C:\\path \u{e9}");
        assert!(matches!(display(b"a\tb\n"), Cow::Borrowed(_)));
        assert_eq!(display(b"f\x00\x7f"), "f\\x00\\x7f");
        assert_eq!(display(b"\xac\xed\x00\x05t\\"), "\\xac\\xed\\x00\\x05t\\\\");
        let mut out = String::from(">");
        write_display(b"a\xffb", &mut out);
        assert_eq!(out, ">a\\xffb");
    }

    fn page(key_type: KeyType, batch: RecordBatch) -> Page<RedisPage> {
        Page {
            page_id: 7,
            connection_id: 1,
            columns: engine_driver_kit::page_columns(&batch, super::super::type_label),
            batch,
            context: RedisPage {
                db: 9,
                key: Some(KeyContext {
                    key: b"h\xff".to_vec(),
                    key_type,
                    ttl: Some(30),
                }),
            },
        }
    }

    #[test]
    fn hash_row_identity_and_binary_values() {
        let mut builder = BinaryBuilder::new();
        builder.append_value(b"f\xff");
        let field: ArrayRef = Arc::new(builder.finish());
        let mut builder = BinaryBuilder::new();
        builder.append_value("ok \u{e9}".as_bytes());
        let value: ArrayRef = Arc::new(builder.finish());
        let schema = Arc::new(Schema::new(vec![
            Field::new("field", DataType::Binary, false),
            Field::new("value", DataType::Binary, false),
        ]));
        let batch = RecordBatch::try_new(schema, vec![field, value]).unwrap();
        let page = page(KeyType::Hash, batch);
        let row = inspect_row(&page, 0);
        assert_eq!(row.fields[0].display_text, "f\\xff");
        assert_eq!(row.fields[0].value, serde_json::json!({"base64": "Zv8="}));
        assert!(!row.fields[0].editable);
        assert_eq!(row.fields[1].display_text, "ok \u{e9}");
        assert!(row.fields[1].editable);
        assert_eq!(row.fields[1].native_type, "bytes");
        assert_eq!(row.mutation_identity["db"], 9);
        assert_eq!(row.mutation_identity["type"], "hash");
        assert_eq!(row.mutation_identity["field_raw"], "Zv8=");
        assert_eq!(
            row.mutation_identity["key_raw"],
            redis_key_bytes_to_raw(b"h\xff")
        );
        assert!(row.mutation_identity.get("key").is_none());
        assert_eq!(row.mutation_identity["ttl"], 30);
    }

    #[test]
    fn list_row_identity_is_the_absolute_index() {
        let index: ArrayRef = Arc::new(Int64Array::from(vec![41]));
        let mut builder = BinaryBuilder::new();
        builder.append_value(b"x");
        let value: ArrayRef = Arc::new(builder.finish());
        let schema = Arc::new(Schema::new(vec![
            Field::new("index", DataType::Int64, false),
            Field::new("value", DataType::Binary, false),
        ]));
        let page = page(
            KeyType::List,
            RecordBatch::try_new(schema, vec![index, value]).unwrap(),
        );
        let row = inspect_row(&page, 0);
        assert_eq!(row.mutation_identity["index"], 41);
        assert_eq!(row.fields[0].value, serde_json::json!(41));
        assert!(!row.fields[0].editable && row.fields[1].editable);
    }
}
