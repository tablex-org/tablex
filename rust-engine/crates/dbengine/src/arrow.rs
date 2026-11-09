use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray, StructArray,
};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema};
use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use base64::Engine;
use bson::Document;
use chrono::Datelike;
use serde::Serialize;
use std::ffi::CString;
use std::fs::File;
use std::io::{Cursor, Read};
use std::os::raw::c_char;
use std::sync::Arc;

use crate::error::{DbEngineError, Result};

/// Arrow `Field` metadata key under which SQL backends publish the column's
/// native database type (`bigint`, `text`, ...) for the UI header label.
pub const NATIVE_TYPE_METADATA_KEY: &str = "tablex.native_type";

fn arrow_perf_logs_enabled() -> bool {
    std::env::var("TABLELEAF_ARROW_PERF")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Load documents from a BSON dump file (mongodump format).
/// Each document is prefixed with a 4-byte little-endian length.
/// The BSON document itself starts with its own 4-byte length.
pub fn load_documents_from_bson(path: &str, count: usize) -> Result<Vec<Document>> {
    let mut file = File::open(path)
        .map_err(|e| DbEngineError::Query(format!("Failed to open fixture: {}", e)))?;
    let mut docs = Vec::with_capacity(count);

    while docs.len() < count {
        let mut len_bytes = [0u8; 4];
        if file.read_exact(&mut len_bytes).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_bytes) as usize;

        if len == 0 || len > 16_777_216 {
            break;
        }

        // Read the full BSON document (includes its own 4-byte length prefix)
        let mut doc_bytes = vec![0u8; len];
        if file.read_exact(&mut doc_bytes).is_err() {
            break;
        }

        let cursor = Cursor::new(doc_bytes);
        if let Ok(doc) = bson::from_reader(cursor) {
            docs.push(doc);
        }
    }

    Ok(docs)
}

/// Arrow C Data Interface result.
/// The FFI structs are heap-allocated and must be freed via `dbengine_free_arrow_cdata`.
pub struct ArrowCDataResult {
    pub schema_ptr: *mut FFI_ArrowSchema,
    pub array_ptr: *mut FFI_ArrowArray,
    pub row_count: usize,
    pub col_count: usize,
    pub column_names: Vec<String>,
    pub column_types: Vec<String>,
    pub total_count: Option<usize>,
    pub last_id: Option<String>,
    pub is_estimated_count: bool,
    pub id_kind: String,
    pub session_id: u64,
    pub large_field_report: LargeFieldReport,
    pub cancelled: bool,
    pub was_truncated: bool,
    /// Cursor state for keyset pagination (JSON string or null).
    /// Caller must free with `dbengine_free_string`.
    pub cursor: *mut c_char,
}

// SAFETY: These pointers are to heap-allocated FFI structs that are safe
// to transfer across threads. The underlying Arrow data is immutable after creation.
unsafe impl Send for ArrowCDataResult {}

/// Convert BSON documents to Arrow C Data Interface format.
///
/// Returns heap-allocated FFI structs. The caller must free them with
/// `dbengine_free_arrow_cdata(schema_ptr, array_ptr)`.
pub fn documents_to_arrow_cdata(docs: &[Document]) -> Result<ArrowCDataResult> {
    let t_total = std::time::Instant::now();
    if arrow_perf_logs_enabled() {
        eprintln!("[arrow-perf] documents_to_arrow_cdata: docs={}", docs.len());
    }
    if docs.is_empty() {
        return Ok(ArrowCDataResult {
            schema_ptr: std::ptr::null_mut(),
            array_ptr: std::ptr::null_mut(),
            row_count: 0,
            col_count: 0,
            column_names: vec![],
            column_types: vec![],
            total_count: None,
            last_id: None,
            is_estimated_count: false,
            id_kind: "Unknown".to_string(),
            session_id: 0,
            large_field_report: LargeFieldReport::default(),
            cancelled: false,
            was_truncated: false,
            cursor: std::ptr::null_mut(),
        });
    }

    // Phase 1: Discover column names (preserve insertion order)
    let t_phase1 = std::time::Instant::now();
    let mut keys: Vec<String> = Vec::new();
    for doc in docs {
        for key in doc.keys() {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
    }
    let phase1_us = t_phase1.elapsed().as_micros();
    if arrow_perf_logs_enabled() {
        eprintln!(
            "[arrow-perf] Phase1 key_discovery: {} keys, {} us",
            keys.len(),
            phase1_us
        );
    }

    // Phase 2: Infer types by scanning all documents (majority vote)
    let t_phase2 = std::time::Instant::now();
    let mut col_types: Vec<DataType> = Vec::with_capacity(keys.len());
    let mut is_datetime: Vec<bool> = Vec::with_capacity(keys.len());
    for key in &keys {
        let mut type_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for doc in docs {
            if let Some(value) = doc.get(key) {
                if matches!(value, bson::Bson::Null) {
                    continue;
                }
                let type_name = match value {
                    bson::Bson::Int32(_) | bson::Bson::Int64(_) => "Int64",
                    bson::Bson::Double(_) => "Float64",
                    bson::Bson::Boolean(_) => "Boolean",
                    bson::Bson::DateTime(_) => "DateTime",
                    bson::Bson::Document(d) if is_extended_json_date(d) => "DateTime",
                    bson::Bson::Document(_) => "Document",
                    bson::Bson::Array(_) => "Array",
                    _ => "Utf8",
                };
                *type_counts.entry(type_name).or_insert(0) += 1;
            }
        }
        let (inferred, is_dt) = match type_counts.iter().max_by_key(|(_, count)| **count) {
            Some((type_name, _)) => match *type_name {
                "Int64" => (DataType::Int64, false),
                "Float64" => (DataType::Float64, false),
                "Boolean" => (DataType::Boolean, false),
                "DateTime" => (DataType::Utf8, true),
                _ => (DataType::Utf8, false),
            },
            None => (DataType::Utf8, false),
        };
        col_types.push(inferred);
        is_datetime.push(is_dt);
    }
    let phase2_us = t_phase2.elapsed().as_micros();
    if arrow_perf_logs_enabled() {
        eprintln!("[arrow-perf] Phase2 type_inference: {} us", phase2_us);
    }

    // Phase 3: Build Arrow arrays
    let t_phase3 = std::time::Instant::now();
    let mut fields: Vec<Field> = Vec::with_capacity(keys.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(keys.len());
    let mut report = LargeFieldReport::default();

    for (i, (key, col_type)) in keys.iter().zip(&col_types).enumerate() {
        fields.push(Field::new(key, col_type.clone(), true));

        let t_col = std::time::Instant::now();
        let array: ArrayRef = match col_type {
            DataType::Int64 => Arc::new(build_int64_array(docs, key)),
            DataType::Float64 => Arc::new(build_float64_array(docs, key)),
            DataType::Boolean => Arc::new(build_bool_array(docs, key)),
            _ if is_datetime[i] => Arc::new(build_datetime_string_array(docs, key)),
            _ => Arc::new(build_string_array(docs, key, &mut report)),
        };
        let col_us = t_col.elapsed().as_micros();
        if arrow_perf_logs_enabled() {
            eprintln!(
                "[arrow-perf] Phase3 col[{}]='{}' {:?}: {} us",
                i, key, col_type, col_us
            );
        }

        arrays.push(array);
    }
    let phase3_us = t_phase3.elapsed().as_micros();
    if arrow_perf_logs_enabled() {
        eprintln!("[arrow-perf] Phase3 total: {} us", phase3_us);
    }

    // Phase 4: Create RecordBatch + FFI export
    let t_phase4 = std::time::Instant::now();
    let schema = Arc::new(Schema::new(fields.clone()));
    let batch = RecordBatch::try_new(schema.clone(), arrays)
        .map_err(|e| DbEngineError::Serialization(e.to_string()))?;

    let row_count = batch.num_rows();
    let col_count = batch.num_columns();

    // Phase 4b: Extract id_kind for _id type detection
    let id_kind = docs
        .first()
        .and_then(|d| d.get("_id"))
        .map(|v| id_type_name(v))
        .unwrap_or("Unknown")
        .to_string();

    // Phase 5: Export to C Data Interface
    let struct_array = StructArray::from(batch);

    // Export schema
    let ffi_schema = FFI_ArrowSchema::try_from(schema.as_ref())
        .map_err(|e| DbEngineError::Serialization(e.to_string()))?;
    let schema_ptr = Box::into_raw(Box::new(ffi_schema));

    // Export array
    let ffi_array = FFI_ArrowArray::new(&struct_array.to_data());
    let array_ptr = Box::into_raw(Box::new(ffi_array));

    // Build column metadata for Swift
    let column_types: Vec<String> = is_datetime
        .iter()
        .zip(&col_types)
        .map(|(is_dt, t)| {
            if *is_dt {
                "DateTime".to_string()
            } else {
                format!("{:?}", t)
            }
        })
        .collect();

    let phase4_us = t_phase4.elapsed().as_micros();
    let total_us = t_total.elapsed().as_micros();
    if arrow_perf_logs_enabled() {
        eprintln!(
            "[arrow-perf] Phase4 recordbatch+ffi: {} us, TOTAL: {} us ({} ms)",
            phase4_us,
            total_us,
            total_us / 1000
        );
    }

    Ok(ArrowCDataResult {
        schema_ptr,
        array_ptr,
        row_count,
        col_count,
        column_names: keys,
        column_types,
        total_count: None,
        last_id: None,
        is_estimated_count: false,
        id_kind,
        session_id: 0,
        large_field_report: report,
        cancelled: false,
        was_truncated: false,
        cursor: std::ptr::null_mut(),
    })
}

/// Convert Arrow RecordBatch(es) to ArrowCDataResult for FFI.
/// Used by PostgreSQL and other non-MongoDB database backends.
pub fn record_batch_to_arrow_cdata(
    batches: Vec<RecordBatch>,
    total_count: Option<usize>,
    cursor_json: Option<String>,
) -> Result<ArrowCDataResult> {
    if batches.is_empty() {
        return Ok(ArrowCDataResult {
            schema_ptr: std::ptr::null_mut(),
            array_ptr: std::ptr::null_mut(),
            row_count: 0,
            col_count: 0,
            column_names: vec![],
            column_types: vec![],
            total_count,
            last_id: None,
            is_estimated_count: false,
            id_kind: "Unknown".to_string(),
            session_id: 0,
            large_field_report: LargeFieldReport::default(),
            cancelled: false,
            was_truncated: false,
            cursor: std::ptr::null_mut(),
        });
    }

    let single_batch = if batches.len() == 1 {
        batches.into_iter().next().unwrap()
    } else {
        let schema = batches[0].schema();
        concat_batches(&schema, &batches)
            .map_err(|e| DbEngineError::Serialization(e.to_string()))?
    };

    let schema = single_batch.schema();
    let row_count = single_batch.num_rows();
    let col_count = single_batch.num_columns();

    let column_names: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
    let column_types: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| normalize_arrow_type_str(f.data_type()))
        .collect();

    let struct_array = StructArray::from(single_batch);
    let ffi_schema = FFI_ArrowSchema::try_from(schema.as_ref())
        .map_err(|e| DbEngineError::Serialization(e.to_string()))?;
    let schema_ptr = Box::into_raw(Box::new(ffi_schema));

    let ffi_array = FFI_ArrowArray::new(&struct_array.to_data());
    let array_ptr = Box::into_raw(Box::new(ffi_array));

    let cursor_ptr = match cursor_json {
        Some(s) => CString::new(s).unwrap().into_raw(),
        None => std::ptr::null_mut(),
    };

    Ok(ArrowCDataResult {
        schema_ptr,
        array_ptr,
        row_count,
        col_count,
        column_names,
        column_types,
        total_count,
        last_id: None,
        is_estimated_count: false, // COUNT(*) is exact for Postgres
        id_kind: "Unknown".to_string(),
        session_id: 0,
        large_field_report: LargeFieldReport::default(),
        cancelled: false,
        was_truncated: false,
        cursor: cursor_ptr,
    })
}

/// Normalize Arrow DataType Debug strings to Swift-compatible typeStr values.
/// Swift's ArrowTable.parseColumn only handles: Int64, Float64, Boolean, Utf8, LargeUtf8, DateTime.
fn normalize_arrow_type_str(dt: &DataType) -> String {
    match dt {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64 => "Int64".to_string(),
        DataType::Float16 | DataType::Float32 | DataType::Float64 => "Float64".to_string(),
        DataType::Boolean => "Boolean".to_string(),
        DataType::Utf8 | DataType::LargeUtf8 => "Utf8".to_string(),
        DataType::Timestamp(_, _) | DataType::Date32 | DataType::Date64 => "DateTime".to_string(),
        DataType::LargeBinary | DataType::Binary => "Utf8".to_string(),
        _ => "Utf8".to_string(),
    }
}

fn build_int64_array(docs: &[Document], key: &str) -> Int64Array {
    Int64Array::from(
        docs.iter()
            .map(|doc| {
                doc.get(key).and_then(|v| match v {
                    bson::Bson::Int32(n) => Some(*n as i64),
                    bson::Bson::Int64(n) => Some(*n),
                    _ => None,
                })
            })
            .collect::<Vec<Option<i64>>>(),
    )
}

fn build_float64_array(docs: &[Document], key: &str) -> Float64Array {
    Float64Array::from(
        docs.iter()
            .map(|doc| {
                doc.get(key).and_then(|v| match v {
                    bson::Bson::Double(n) => Some(*n),
                    bson::Bson::Int32(n) => Some(*n as f64),
                    bson::Bson::Int64(n) => Some(*n as f64),
                    _ => None,
                })
            })
            .collect::<Vec<Option<f64>>>(),
    )
}

fn build_bool_array(docs: &[Document], key: &str) -> BooleanArray {
    BooleanArray::from(
        docs.iter()
            .map(|doc| {
                doc.get(key).and_then(|v| match v {
                    bson::Bson::Boolean(b) => Some(*b),
                    _ => None,
                })
            })
            .collect::<Vec<Option<bool>>>(),
    )
}

const LARGE_STRING_BYTES: usize = 16 * 1024;
const PREVIEW_CHARS: usize = 200;
const BINARY_PREVIEW_BYTES: usize = 150; // base64(150 bytes) ≈ 200 chars

fn format_human_bytes(len: usize) -> String {
    let mb = len as f64 / (1024.0 * 1024.0);
    if mb >= 1.0 {
        format!("{:.1} MB", mb)
    } else {
        format!("{:.1} KB", len as f64 / 1024.0)
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct LargeFieldReport {
    pub fields: Vec<LargeField>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LargeField {
    pub field: String,
    pub sample_bytes: usize,
    pub kind: String,
}

fn looks_like_base64(s: &str) -> bool {
    if s.len() < 32 {
        return false;
    }
    let sample_end = s.floor_char_boundary(256);
    let sample = &s[..sample_end];
    let base64_chars = sample
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        .count();
    base64_chars * 10 >= sample.len() * 9
}

fn cap_string_ref_with_kind(s: &str) -> (Option<String>, &'static str) {
    if s.len() <= LARGE_STRING_BYTES {
        return (None, "");
    }
    let kind = if looks_like_base64(s) {
        "Base64"
    } else {
        "LargeString"
    };
    let human_size = format_human_bytes(s.len());
    let trunc_end = s.floor_char_boundary(PREVIEW_CHARS);
    let preview = format!("{}... ({})", &s[..trunc_end], human_size);
    (Some(preview), kind)
}

fn build_string_array(docs: &[Document], key: &str, report: &mut LargeFieldReport) -> StringArray {
    let t0 = std::time::Instant::now();
    let mut saw_large = false;
    let mut large_count = 0usize;
    let mut sample_bytes: usize = 0;
    let mut sample_kind = String::new();

    let arr = StringArray::from(
        docs.iter()
            .enumerate()
            .map(|(row, doc)| {
                doc.get(key).map(|v| match v {
                    bson::Bson::String(s) => {
                        let (capped, kind) = cap_string_ref_with_kind(s);
                        if let Some(c) = capped {
                            if !saw_large {
                                saw_large = true;
                                sample_bytes = s.len();
                                sample_kind = kind.to_string();
                            }
                            large_count += 1;
                            c
                        } else {
                            s.clone()
                        }
                    }
                    bson::Bson::ObjectId(oid) => oid.to_string(),
                    bson::Bson::Document(d) if is_extended_json_date(d) => {
                        match extended_json_date_to_iso8601(d, key, row, "<id>") {
                            Some(iso) => iso,
                            None => {
                                let id = doc.get("_id").map(bson_value_to_log_string).unwrap_or_else(|| "<none>".to_string());
                                eprintln!("[arrow] build_string_array: extended JSON date conversion FAILED for key='{}', row={}, _id={}, doc={:?}", key, row, id, d);
                                let ser = serde_json::to_string(v).unwrap_or_else(|_| "null".to_string());
                                let (capped, kind) = cap_string_ref_with_kind(&ser);
                                if let Some(c) = capped {
                                    if !saw_large {
                                        saw_large = true;
                                        sample_bytes = ser.len();
                                        sample_kind = kind.to_string();
                                    }
                                    large_count += 1;
                                    c
                                } else {
                                    ser
                                }
                            }
                        }
                    }
                    bson::Bson::Binary(bin) => {
                        let byte_count = bin.bytes.len();
                        if byte_count > LARGE_STRING_BYTES {
                            if !saw_large {
                                saw_large = true;
                                sample_bytes = byte_count;
                                sample_kind = "Binary".to_string();
                            }
                            large_count += 1;
                            let preview_byte_len = BINARY_PREVIEW_BYTES.min(bin.bytes.len());
                            let preview_bytes = &bin.bytes[..preview_byte_len];
                            let preview_b64 = base64::engine::general_purpose::STANDARD.encode(preview_bytes);
                            let trunc_end = preview_b64.floor_char_boundary(PREVIEW_CHARS);
                            let human_size = format_human_bytes(byte_count);
                            format!("[binary] {}... ({})", &preview_b64[..trunc_end], human_size)
                        } else {
                            let ser = serde_json::to_string(v).unwrap_or_else(|_| "null".to_string());
                            let (capped, kind) = cap_string_ref_with_kind(&ser);
                            if let Some(c) = capped {
                                if !saw_large {
                                    saw_large = true;
                                    sample_bytes = ser.len();
                                    sample_kind = kind.to_string();
                                }
                                large_count += 1;
                                c
                            } else {
                                ser
                            }
                        }
                    }
                    other => {
                        let ser = serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string());
                        let (capped, kind) = cap_string_ref_with_kind(&ser);
                        if let Some(c) = capped {
                            if !saw_large {
                                saw_large = true;
                                sample_bytes = ser.len();
                                sample_kind = kind.to_string();
                            }
                            large_count += 1;
                            c
                        } else {
                            ser
                        }
                    }
                })
            })
            .collect::<Vec<Option<String>>>(),
    );

    let elapsed_us = t0.elapsed().as_micros();
    if arrow_perf_logs_enabled() {
        eprintln!(
            "[arrow-perf] build_string_array('{}'): {} docs, {} large, {} us",
            key,
            docs.len(),
            large_count,
            elapsed_us
        );
    }

    if saw_large {
        report.fields.push(LargeField {
            field: key.to_string(),
            sample_bytes,
            kind: sample_kind,
        });
    }
    arr
}

fn build_datetime_string_array(docs: &[Document], key: &str) -> StringArray {
    eprintln!("[arrow] build_datetime_string_array: key={}", key);
    StringArray::from(
        docs.iter()
            .enumerate()
            .map(|(row, doc)| {
                let id = doc.get("_id").map(bson_value_to_log_string).unwrap_or_else(|| "<none>".to_string());
                doc.get(key).map(|v| match v {
                    bson::Bson::DateTime(dt) => safe_bson_datetime_to_string(dt, key, row, &id),
                    bson::Bson::Document(d) if is_extended_json_date(d) => {
                        match extended_json_date_to_iso8601(d, key, row, &id) {
                            Some(iso) => iso,
                            None => {
                                eprintln!("[arrow] build_datetime_string_array: conversion FAILED for key='{}', row={}, _id={}, doc={:?}", key, row, id, d);
                                "null".to_string()
                            }
                        }
                    }
                    _ => "null".to_string(),
                })
            })
            .collect::<Vec<Option<String>>>(),
    )
}

/// Safely format a BSON DateTime as an ISO/RFC3339 string.
///
/// `bson::DateTime::to_rfc3339_string` can panic via an internal `unwrap` for
/// dates whose year cannot be represented in the requested RFC3339 format
/// (year >= 10000 or pre-0001). Use `chrono` to convert the millisecond
/// timestamp with a year check, and fall back to the Extended JSON
/// `{"$date":{"$numberLong":<ms>}}` representation for out-of-range values.
///
/// `key` is the column/field name, `row` is the row index in the result
/// batch and `id` is the document `_id` (any BSON value rendered as a
/// string), used to locate the offending row quickly. All are included in
/// the diagnostic log when the value falls back. Pass `""` / `0` when there
/// is no context.
fn safe_bson_datetime_to_string(
    dt: &bson::DateTime,
    key: &str,
    row: usize,
    id: &str,
) -> String {
    let ms = dt.timestamp_millis();
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(c) => {
            let year = c.year();
            if (1..=9999).contains(&year) {
                c.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            } else {
                eprintln!(
                    "[arrow] datetime fallback: key={} row={} _id={} ms={} year={} reason=outside_rfc3339_year_range value={{\"$date\":{{\"$numberLong\":\"{}\"}}}}",
                    key, row, id, ms, year, ms
                );
                format!("{{\"$date\":{{\"$numberLong\":\"{}\"}}}}", ms)
            }
        }
        None => {
            eprintln!(
                "[arrow] datetime fallback: key={} row={} _id={} ms={} year=<chrono_none> reason=chrono_from_timestamp_millis_failed value={{\"$date\":{{\"$numberLong\":\"{}\"}}}}",
                key, row, id, ms, ms
            );
            format!("{{\"$date\":{{\"$numberLong\":\"{}\"}}}}", ms)
        }
    }
}

/// Metadata returned to Swift via FFI.
#[derive(Serialize)]
pub struct ArrowCDataResponse {
    pub schema_ptr: u64,
    pub array_ptr: u64,
    pub row_count: usize,
    pub col_count: usize,
    pub column_names: Vec<String>,
    pub column_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    pub is_estimated_count: bool,
    pub id_kind: String,
    pub session_id: u64,
    pub large_field_report: LargeFieldReport,
    pub cancelled: bool,
    pub was_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

fn cursor_ptr_to_option(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        unsafe { CString::from_raw(ptr).into_string().ok() }
    }
}

impl From<ArrowCDataResult> for ArrowCDataResponse {
    fn from(result: ArrowCDataResult) -> Self {
        let cursor = cursor_ptr_to_option(result.cursor);
        Self {
            schema_ptr: result.schema_ptr as u64,
            array_ptr: result.array_ptr as u64,
            row_count: result.row_count,
            col_count: result.col_count,
            column_names: result.column_names,
            column_types: result.column_types,
            total_count: result.total_count,
            last_id: result.last_id,
            is_estimated_count: result.is_estimated_count,
            id_kind: result.id_kind,
            session_id: result.session_id,
            large_field_report: result.large_field_report,
            cancelled: result.cancelled,
            was_truncated: result.was_truncated,
            cursor,
        }
    }
}

/// Check if a BSON document is an extended JSON date representation.
/// Matches: {"$date": {"$numberLong": "..."}} or {"$date": "ISO8601 string"}
pub fn is_extended_json_date(doc: &bson::Document) -> bool {
    if doc.len() != 1 {
        return false;
    }
    doc.contains_key("$date")
}

/// Convert an extended JSON date document to an ISO8601/RFC3339 string.
/// Handles: {"$date": {"$numberLong": "ms"}}, {"$date": {"$numberLong": ms}},
/// {"$date": ms}, and {"$date": "ISO8601"}.
fn extended_json_date_to_iso8601(
    doc: &bson::Document,
    key: &str,
    row: usize,
    id: &str,
) -> Option<String> {
    doc.get("$date").and_then(|date_value| {
        match date_value {
            // {"$date": {"$numberLong": "1727239981386"}} or {"$numberLong": 1727...}
            bson::Bson::Document(inner) => {
                match extract_millis(inner.get("$numberLong")) {
                    Some(ms) => {
                        Some(safe_bson_datetime_to_string(&bson::DateTime::from_millis(ms), key, row, id))
                    }
                    None => {
                        eprintln!(
                            "[arrow] extended_json_date: key={} row={} _id={} reason=invalid_numberLong value={:?}",
                            key, row, id, inner
                        );
                        None
                    }
                }
            }
            // {"$date": 1727239981386} (direct Int64)
            bson::Bson::Int64(ms) => {
                Some(safe_bson_datetime_to_string(&bson::DateTime::from_millis(*ms), key, row, id))
            }
            // {"$date": 1727239981} (direct Int32 - seconds, unlikely but handle)
            bson::Bson::Int32(ms) => {
                Some(safe_bson_datetime_to_string(&bson::DateTime::from_millis(*ms as i64), key, row, id))
            }
            // {"$date": "2024-09-25T10:13:01.386Z"} (ISO string)
            bson::Bson::String(s) => Some(s.clone()),
            _ => {
                eprintln!(
                    "[arrow] extended_json_date: key={} row={} _id={} reason=unsupported_inner_type value={:?}",
                    key, row, id, date_value
                );
                None
            }
        }
    })
}

/// Render any BSON value as a short string suitable for log lines.
/// Objects/arrays are JSON-serialized, others use their Display impl.
fn bson_value_to_log_string(v: &bson::Bson) -> String {
    match v {
        bson::Bson::ObjectId(oid) => oid.to_hex(),
        bson::Bson::String(s) => s.clone(),
        bson::Bson::Int64(i) => i.to_string(),
        bson::Bson::Int32(i) => i.to_string(),
        bson::Bson::Double(f) => f.to_string(),
        bson::Bson::Boolean(b) => b.to_string(),
        bson::Bson::Null => "null".to_string(),
        bson::Bson::DateTime(dt) => format!("DateTime({})", dt.timestamp_millis()),
        _ => serde_json::to_string(v).unwrap_or_else(|_| "<unprintable>".to_string()),
    }
}

fn extract_millis(v: Option<&bson::Bson>) -> Option<i64> {
    match v {
        Some(bson::Bson::String(s)) => s.parse::<i64>().ok(),
        Some(bson::Bson::Int64(n)) => Some(*n),
        Some(bson::Bson::Int32(n)) => Some(*n as i64),
        _ => None,
    }
}

/// Get the type name of an _id value for Swift-side filter construction.
fn id_type_name(v: &bson::Bson) -> &'static str {
    match v {
        bson::Bson::ObjectId(_) => "ObjectId",
        bson::Bson::Int64(_) => "Int64",
        bson::Bson::Int32(_) => "Int32",
        bson::Bson::String(_) => "String",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::{doc, DateTime};

    // MARK: - is_extended_json_date

    #[test]
    fn test_is_extended_json_date_with_numberlong_string() {
        let d = doc! { "$date": { "$numberLong": "1727239981386" } };
        assert!(is_extended_json_date(&d));
    }

    #[test]
    fn test_is_extended_json_date_with_iso_string() {
        let d = doc! { "$date": "2024-09-25T10:13:01.386Z" };
        assert!(is_extended_json_date(&d));
    }

    #[test]
    fn test_is_extended_json_date_with_direct_int64() {
        let d = doc! { "$date": 1727239981386_i64 };
        assert!(is_extended_json_date(&d));
    }

    #[test]
    fn test_is_extended_json_date_returns_false_for_regular_doc() {
        let d = doc! { "foo": "bar", "$date": "extra" };
        assert!(!is_extended_json_date(&d)); // len != 1
    }

    #[test]
    fn test_is_extended_json_date_returns_false_for_nested_doc() {
        let d = doc! { "nested": { "$date": "2024-01-01" } };
        assert!(!is_extended_json_date(&d));
    }

    #[test]
    fn test_is_extended_json_date_returns_false_for_empty_doc() {
        let d = doc! {};
        assert!(!is_extended_json_date(&d)); // len == 0, not 1
    }

    // MARK: - extended_json_date_to_iso8601

    #[test]
    fn test_extended_json_date_numberlong_string() {
        let d = doc! { "$date": { "$numberLong": "1727239981386" } };
        let result = extended_json_date_to_iso8601(&d, "ts", 0, "id1").unwrap();
        assert!(
            result.contains("2024-09-25"),
            "Expected 2024-09-25 in: {}",
            result
        );
    }

    #[test]
    fn test_extended_json_date_direct_int64() {
        let d = doc! { "$date": 1727239981386_i64 };
        let result = extended_json_date_to_iso8601(&d, "ts", 0, "id1").unwrap();
        assert!(
            result.contains("2024-09-25"),
            "Expected 2024-09-25 in: {}",
            result
        );
    }

    #[test]
    fn test_extended_json_date_direct_int32() {
        // Int32 max is ~2.1B, so max ms is ~24 days from epoch (Jan 1970)
        // Use a realistic Int32 millisecond value
        let d = doc! { "$date": 86400000_i32 }; // 1 day after epoch
        let result = extended_json_date_to_iso8601(&d, "ts", 0, "id1").unwrap();
        assert!(
            result.contains("1970-01-02"),
            "Expected 1970-01-02 in: {}",
            result
        );
    }

    #[test]
    fn test_extended_json_date_iso_string_passthrough() {
        let iso = "2024-09-25T10:13:01.386Z";
        let d = doc! { "$date": iso };
        let result = extended_json_date_to_iso8601(&d, "ts", 0, "id1").unwrap();
        assert_eq!(result, iso);
    }

    #[test]
    fn test_extended_json_date_invalid_returns_none() {
        let d = doc! { "$date": { "$invalid": "key" } };
        assert!(extended_json_date_to_iso8601(&d, "ts", 0, "id1").is_none());
    }

    #[test]
    fn test_extended_json_date_missing_key_returns_none() {
        let d = doc! { "$date": { "$numberLong": 123 } };
        // This should work since $numberLong is present
        assert!(extended_json_date_to_iso8601(&d, "ts", 0, "id1").is_some());
    }

    // MARK: - extract_millis

    #[test]
    fn test_extract_millis_from_string() {
        let v = bson::Bson::String("1727239981386".to_string());
        assert_eq!(extract_millis(Some(&v)), Some(1727239981386));
    }

    #[test]
    fn test_extract_millis_from_int64() {
        let v = bson::Bson::Int64(1727239981386);
        assert_eq!(extract_millis(Some(&v)), Some(1727239981386));
    }

    #[test]
    fn test_extract_millis_from_int32() {
        let v = bson::Bson::Int32(1727239981);
        assert_eq!(extract_millis(Some(&v)), Some(1727239981));
    }

    #[test]
    fn test_extract_millis_from_invalid_type() {
        let v = bson::Bson::String("not-a-number".to_string());
        assert_eq!(extract_millis(Some(&v)), None);
    }

    #[test]
    fn test_extract_millis_from_none() {
        assert_eq!(extract_millis(None), None);
    }

    // MARK: - Type inference with null handling (integration)

    #[test]
    fn test_type_inference_skips_null_and_finds_datetime() {
        // Simulates completedAt bug: first non-null is DateTime
        let docs = vec![
            doc! { "completedAt": bson::Bson::Null },
            doc! { "completedAt": bson::Bson::Null },
            doc! { "completedAt": DateTime::from_millis(1727239981386) },
            doc! { "completedAt": bson::Bson::Null },
        ];
        let result = documents_to_arrow_cdata(&docs).unwrap();
        let completed_at_type = result
            .column_types
            .iter()
            .find(|t| t == &&"DateTime".to_string());
        assert!(
            completed_at_type.is_some(),
            "completedAt should be detected as DateTime"
        );
    }

    #[test]
    fn test_type_inference_detects_extended_json_date() {
        let docs = vec![
            doc! { "completedAt": { "$date": { "$numberLong": "1727239981386" } } },
            doc! { "completedAt": { "$date": { "$numberLong": "1727239981386" } } },
        ];
        let result = documents_to_arrow_cdata(&docs).unwrap();
        let completed_at_type = result
            .column_types
            .iter()
            .find(|t| t == &&"DateTime".to_string());
        assert!(
            completed_at_type.is_some(),
            "Extended JSON date should be detected as DateTime"
        );
    }

    #[test]
    fn test_type_inference_majority_vote() {
        let docs = vec![
            doc! { "status": "active" },
            doc! { "status": "active" },
            doc! { "status": "active" },
            doc! { "status": 42 },
        ];
        let result = documents_to_arrow_cdata(&docs).unwrap();
        let status_type = result
            .column_types
            .iter()
            .find(|t| t == &&"Utf8".to_string());
        assert!(
            status_type.is_some(),
            "String should win majority vote over Int64"
        );
    }

    #[test]
    fn test_type_inference_all_null_falls_back_to_utf8() {
        let docs = vec![
            doc! { "field": bson::Bson::Null },
            doc! { "field": bson::Bson::Null },
        ];
        let result = documents_to_arrow_cdata(&docs).unwrap();
        let field_type = result
            .column_types
            .iter()
            .find(|t| t == &&"Utf8".to_string());
        assert!(
            field_type.is_some(),
            "All null column should fallback to Utf8"
        );
    }

    // MARK: - build_string_array with extended JSON dates

    #[test]
    fn test_build_string_array_converts_extended_json_date() {
        let docs = vec![doc! { "date": { "$date": { "$numberLong": "1727239981386" } } }];
        let mut report = LargeFieldReport::default();
        let arr = build_string_array(&docs, "date", &mut report);
        let val = arr.value(0);
        assert!(
            val.contains("2024-09-25"),
            "Expected ISO date, got: {}",
            val
        );
        assert!(!val.contains("$date"), "Should not contain extended JSON");
    }

    #[test]
    fn test_build_string_array_handles_regular_documents() {
        let docs = vec![doc! { "data": { "foo": "bar" } }];
        let mut report = LargeFieldReport::default();
        let arr = build_string_array(&docs, "data", &mut report);
        let val = arr.value(0);
        assert!(val.contains("foo"), "Regular doc should be JSON serialized");
    }

    // MARK: - safe_bson_datetime_to_string (regression: bson to_rfc3339_string panics on extreme years)

    #[test]
    fn test_safe_bson_datetime_normal_value() {
        let dt = DateTime::from_millis(1727239981386);
        let s = safe_bson_datetime_to_string(&dt, "ts", 0, "id1");
        assert!(s.contains("2024-09-25"), "Expected 2024-09-25 in: {}", s);
        assert!(
            !s.contains("$date"),
            "Should not fall back to Extended JSON"
        );
    }

    #[test]
    fn test_safe_bson_datetime_extreme_positive_does_not_panic() {
        // i64::MAX ms is far outside the RFC3339 year range and historically
        // caused bson::DateTime::to_rfc3339_string to panic via internal unwrap.
        let dt = DateTime::from_millis(i64::MAX);
        let s = safe_bson_datetime_to_string(&dt, "birthDate", 0, "id1");
        assert!(
            s.contains("$date"),
            "Extreme value should fall back to Extended JSON, got: {}",
            s
        );
    }

    #[test]
    fn test_safe_bson_datetime_extreme_negative_does_not_panic() {
        let dt = DateTime::from_millis(i64::MIN);
        let s = safe_bson_datetime_to_string(&dt, "birthDate", 0, "id1");
        assert!(
            s.contains("$date"),
            "Extreme value should fall back to Extended JSON, got: {}",
            s
        );
    }

    #[test]
    fn test_build_datetime_string_array_with_extreme_value_does_not_panic() {
        // Direct repro of the production crash: documents_to_arrow_cdata on
        // a BSON datetime with an out-of-range year must not abort the dylib.
        let docs = vec![
            doc! { "_id": bson::oid::ObjectId::new(), "ts": DateTime::from_millis(i64::MAX) },
            doc! { "_id": bson::oid::ObjectId::new(), "ts": DateTime::from_millis(i64::MIN) },
            doc! { "_id": bson::oid::ObjectId::new(), "ts": DateTime::from_millis(0) },
        ];
        let result =
            documents_to_arrow_cdata(&docs).expect("must not panic/err on extreme datetimes");
        let ts_col = result
            .column_names
            .iter()
            .position(|n| n == "ts")
            .expect("ts column");
        let _ = result.column_types[ts_col].clone();
    }

    #[test]
    fn test_extended_json_date_with_extreme_value_does_not_panic() {
        let d = doc! { "$date": { "$numberLong": "9223372036854775807" } };
        let result = extended_json_date_to_iso8601(&d, "ts", 0, "id1");
        assert!(
            result.is_some(),
            "Extreme extended JSON date should not return None"
        );
        let s = result.unwrap();
        assert!(
            s.contains("$date"),
            "Extreme value should fall back to Extended JSON, got: {}",
            s
        );
    }

    // MARK: - cap_string_ref_with_kind + looks_like_base64

    #[test]
    fn cap_ref_short_unchanged() {
        let (capped, _) = cap_string_ref_with_kind("hello world");
        assert!(capped.is_none());
    }

    #[test]
    fn cap_ref_long_string() {
        let s = "hello world! ".repeat(LARGE_STRING_BYTES / 13 + 1);
        let (capped, kind) = cap_string_ref_with_kind(&s);
        assert!(capped.is_some());
        let val = capped.unwrap();
        assert!(
            val.contains("... ("),
            "expected truncated preview, got: {}",
            val
        );
        assert!(
            val.contains("KB") || val.contains("MB"),
            "expected size suffix, got: {}",
            val
        );
        assert!(
            val.starts_with("hello world!"),
            "expected original data preview, got: {}",
            val
        );
        assert_eq!(kind, "LargeString");
    }

    #[test]
    fn cap_ref_base64_kind() {
        let s: String = (0..32_000)
            .map(|i| {
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"[i % 64] as char
            })
            .collect();
        let (capped, kind) = cap_string_ref_with_kind(&s);
        assert!(capped.is_some());
        let val = capped.unwrap();
        assert!(
            val.contains("... ("),
            "expected truncated preview, got: {}",
            val
        );
        assert!(
            val.starts_with("ABCDEF"),
            "expected base64 data preview, got: {}",
            val
        );
        assert_eq!(kind, "Base64");
    }

    #[test]
    fn base64_short_negative() {
        assert!(!looks_like_base64("abcd"));
    }

    #[test]
    fn floor_char_boundary_safe() {
        // Non-base64 string with non-ASCII chars, > LARGE_STRING_BYTES
        let s = "!@#$%^&*()".repeat(LARGE_STRING_BYTES / 10 + 1) + "ééé";
        let (capped, kind) = cap_string_ref_with_kind(&s);
        assert!(capped.is_some());
        let val = capped.unwrap();
        assert_eq!(kind, "LargeString");
        assert!(
            val.contains("... ("),
            "expected truncated preview, got: {}",
            val
        );
        assert!(
            val.starts_with("!@#"),
            "expected original data preview, got: {}",
            val
        );
    }

    #[test]
    fn build_string_array_binary_large() {
        // Verify Bson::Binary > LARGE_STRING_BYTES shows base64 preview
        let big_bytes = vec![0xFFu8; LARGE_STRING_BYTES + 1024];
        let bin = bson::Binary {
            subtype: bson::spec::BinarySubtype::Generic,
            bytes: big_bytes.clone(),
        };
        let docs = vec![doc! { "img": bson::Bson::Binary(bin) }];
        let mut report = LargeFieldReport::default();
        let arr = build_string_array(&docs, "img", &mut report);
        let val = arr.value(0);
        assert!(
            val.starts_with("[binary] "),
            "expected [binary] prefix, got: {}",
            val
        );
        assert!(
            val.contains("... ("),
            "expected truncated preview, got: {}",
            val
        );
        assert!(
            val.contains("KB") || val.contains("MB"),
            "expected size suffix, got: {}",
            val
        );
        assert!(report.fields.len() == 1, "expected 1 large field report");
        assert_eq!(report.fields[0].field, "img");
        assert_eq!(report.fields[0].kind, "Binary");
    }
}
