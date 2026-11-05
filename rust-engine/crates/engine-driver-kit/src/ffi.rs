//! Raw FFI helpers: envelopes, request decoding, panic guards, Arrow leases
//! and cell batches. Everything `unsafe` in the kit lives here or in
//! `driver.rs` / `macros.rs`, each block with a `SAFETY` comment.

use crate::cell_text::CellFormatter;
use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::{Array, RecordBatch, StructArray};
use engine_driver_api::{
    CArrowPageLease, CCellText, CCellTextBatch, DriverError, DriverErrorEnvelope, SuccessEnvelope,
    CELL_FLAG_NULL,
};
use engine_ffi_util::string_to_c_ptr;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ffi::{c_char, c_void, CStr};
use std::ops::Range;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub(crate) fn failure(code: impl Into<String>, message: impl Into<String>) -> DriverError {
    DriverError {
        code: code.into(),
        message: message.into(),
        retryable: false,
    }
}

fn error_ptr(error: DriverError) -> *mut c_char {
    // The envelope is plain strings and a bool: serialization cannot fail, and
    // JSON escapes NUL so the C string conversion cannot either.
    string_to_c_ptr(
        serde_json::to_string(&DriverErrorEnvelope { ok: false, error })
            .unwrap_or_else(|_| String::from(r#"{"ok":false,"error":{"code":"driver.serialize","message":"error envelope failed to serialize","retryable":false}}"#)),
    )
}

/// Serializes `result` as the v2 envelope: `{"ok":true,"value":..}` or
/// `{"ok":false,"error":{code,message,retryable}}`. The caller frees the string
/// with `dbengine_free_string`.
pub(crate) fn reply<T: Serialize>(result: Result<T, DriverError>) -> *mut c_char {
    match result {
        Ok(value) => match serde_json::to_string(&SuccessEnvelope { ok: true, value }) {
            Ok(json) => string_to_c_ptr(json),
            Err(error) => error_ptr(failure("driver.serialize", error.to_string())),
        },
        Err(error) => error_ptr(error),
    }
}

/// Runs an envelope-returning entry point so that a panic becomes an error
/// envelope with `internal_code` instead of unwinding into the host (which
/// would abort the process).
pub(crate) fn guarded<T: Serialize>(
    internal_code: &'static str,
    body: impl FnOnce() -> Result<T, DriverError>,
) -> *mut c_char {
    // Serialization runs inside the guard too: a panicking `Serialize` impl in a
    // backend-supplied value must not unwind into the host either.
    match catch_unwind(AssertUnwindSafe(|| reply(body()))) {
        Ok(envelope) => envelope,
        Err(_) => reply::<()>(Err(failure(
            internal_code,
            "internal driver error: backend panicked",
        ))),
    }
}

/// Runs an entry point that has no error channel; a panic yields `fallback`.
pub(crate) fn guarded_or<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(fallback)
}

/// Decodes a NUL-terminated JSON request into `T`; failures carry `code`.
///
/// # Safety
/// `ptr` must be null or point to a valid NUL-terminated string that stays
/// alive for the duration of the call.
pub(crate) unsafe fn parse_request<T: DeserializeOwned>(
    ptr: *const c_char,
    code: &'static str,
) -> Result<T, DriverError> {
    if ptr.is_null() {
        return Err(failure(code, "null request pointer"));
    }
    // SAFETY: non-null, and the caller guarantees a live NUL-terminated string.
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|error| failure(code, error.to_string()))?;
    serde_json::from_str(text).map_err(|error| failure(code, error.to_string()))
}

/// A lease with no data: the host treats it as "page not available".
pub(crate) fn empty_lease() -> CArrowPageLease {
    CArrowPageLease {
        schema: std::ptr::null_mut(),
        array: std::ptr::null_mut(),
        row_count: 0,
        column_count: 0,
        generation: 0,
        release: None,
        release_context: std::ptr::null_mut(),
    }
}

/// Exports `batch` (already sliced, zero-copy) through the Arrow C data
/// interface. The returned lease owns boxed `FFI_ArrowSchema`/`FFI_ArrowArray`
/// values that keep the batch's buffers alive until `release_arrow_page`.
pub(crate) fn export_arrow_lease(batch: RecordBatch) -> Option<CArrowPageLease> {
    let row_count = batch.num_rows() as u64;
    let column_count = batch.num_columns() as u64;
    let struct_array = StructArray::from(batch);
    let array = Box::new(FFI_ArrowArray::new(&struct_array.to_data()));
    let schema = Box::new(FFI_ArrowSchema::try_from(struct_array.data_type()).ok()?);
    Some(CArrowPageLease {
        schema: Box::into_raw(schema).cast::<c_void>(),
        array: Box::into_raw(array).cast::<c_void>(),
        row_count,
        column_count,
        generation: 1,
        release: Some(release_arrow_page),
        release_context: std::ptr::null_mut(),
    })
}

/// `CArrowPageLease::release` for leases built by [`export_arrow_lease`].
unsafe extern "C" fn release_arrow_page(
    schema: *mut c_void,
    array: *mut c_void,
    _context: *mut c_void,
) {
    // A panic in an Arrow release callback must not unwind into the host.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !schema.is_null() {
            // SAFETY: `schema` was produced by `Box::into_raw` of an
            // `FFI_ArrowSchema` in `export_arrow_lease`; the host calls
            // `release` exactly once per lease.
            unsafe { drop(Box::from_raw(schema.cast::<FFI_ArrowSchema>())) };
        }
        if !array.is_null() {
            // SAFETY: as above, for the boxed `FFI_ArrowArray`.
            unsafe { drop(Box::from_raw(array.cast::<FFI_ArrowArray>())) };
        }
    }));
}

/// Renders `rows` x `columns` as row-major text cells in one shared buffer.
/// `formatter` builds one [`CellFormatter`] per column (once); cells are written
/// into the shared buffer, so the kit adds no per-cell allocation.
pub(crate) fn build_cell_batch<'a>(
    rows: Range<usize>,
    columns: Range<usize>,
    formatter: impl FnMut(usize) -> CellFormatter<'a>,
) -> Box<CCellTextBatch> {
    let formatters: Vec<CellFormatter<'a>> = columns.clone().map(formatter).collect();
    let mut cells = Vec::with_capacity(rows.len().saturating_mul(columns.len()));
    let mut text = String::new();
    for row in rows {
        for (column, formatter) in columns.clone().zip(&formatters) {
            let offset = text.len();
            formatter.write(row, &mut text);
            cells.push(CCellText {
                row: row as u64,
                column: column as u64,
                offset: offset as u64,
                length: (text.len() - offset) as u64,
                flags: if formatter.is_null(row) {
                    CELL_FLAG_NULL
                } else {
                    0
                },
            });
        }
    }
    let cell_count = cells.len() as u64;
    let cells = cells.into_boxed_slice();
    let text = text.into_bytes().into_boxed_slice();
    let text_buffer_len = text.len() as u64;
    Box::new(CCellTextBatch {
        cells: if cells.is_empty() {
            std::ptr::null_mut()
        } else {
            Box::into_raw(cells).cast::<CCellText>()
        },
        cell_count,
        text_buffer: if text.is_empty() {
            std::ptr::null_mut()
        } else {
            Box::into_raw(text).cast::<u8>()
        },
        text_buffer_len,
    })
}

/// Frees a batch built by [`build_cell_batch`].
///
/// # Safety
/// `batch` must be null or a pointer returned through `render_cell_batch` that
/// has not been freed yet.
pub(crate) unsafe fn free_cell_batch(batch: *mut CCellTextBatch) {
    if batch.is_null() {
        return;
    }
    // SAFETY: non-null and, per the contract, a live `Box<CCellTextBatch>`
    // created by `Box::into_raw` in `render_cell_batch`.
    let batch = unsafe { Box::from_raw(batch) };
    if !batch.cells.is_null() {
        // SAFETY: `cells` is the data pointer of a `Box<[CCellText]>` of exactly
        // `cell_count` elements (see `build_cell_batch`), freed once here.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                batch.cells,
                batch.cell_count as usize,
            )));
        }
    }
    if !batch.text_buffer.is_null() {
        // SAFETY: `text_buffer` is the data pointer of a `Box<[u8]>` of exactly
        // `text_buffer_len` bytes, freed once here.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                batch.text_buffer,
                batch.text_buffer_len as usize,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanicsWhileSerializing;

    impl Serialize for PanicsWhileSerializing {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            panic!("serialize boom");
        }
    }

    #[test]
    fn a_panic_while_serializing_the_reply_becomes_an_error_envelope() {
        let ptr = guarded("test.internal", || Ok(PanicsWhileSerializing));
        // SAFETY: `ptr` is a NUL-terminated string just returned by `guarded`.
        let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
        // SAFETY: freed once.
        unsafe { engine_ffi_util::free_string(ptr) };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "test.internal");
    }
}
