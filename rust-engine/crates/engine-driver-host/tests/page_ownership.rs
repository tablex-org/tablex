mod common;

use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::{Array, Int64Array, RecordBatch, StructArray};
use arrow_schema::{DataType, Field, Schema};
use engine_driver_api::{CArrowPageLease, CPageLease, CPageSlice};
use engine_driver_host::{DriverLoader, LoaderError, PageError};
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

struct Backing {
    bytes: [u8; 4],
    releases: Arc<AtomicUsize>,
}

struct ArrowBacking {
    schema: u8,
    array: u8,
    releases: Arc<AtomicUsize>,
}

unsafe extern "C" fn release_boxed_bytes(context: *mut c_void) {
    let backing = Box::from_raw(context.cast::<Backing>());
    backing.releases.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn release_arrow(
    _schema: *mut c_void,
    _array: *mut c_void,
    context: *mut c_void,
) {
    let backing = Box::from_raw(context.cast::<ArrowBacking>());
    backing.releases.fetch_add(1, Ordering::SeqCst);
}

fn boxed_arrow(releases: Arc<AtomicUsize>) -> CArrowPageLease {
    let backing = Box::new(ArrowBacking {
        schema: 1,
        array: 2,
        releases,
    });
    let schema = (&backing.schema as *const u8).cast_mut().cast::<c_void>();
    let array = (&backing.array as *const u8).cast_mut().cast::<c_void>();
    let context = Box::into_raw(backing).cast::<c_void>();
    CArrowPageLease {
        schema,
        array,
        row_count: 1,
        column_count: 1,
        generation: 3,
        release: Some(release_arrow),
        release_context: context,
    }
}

fn boxed_page(bytes: [u8; 4], releases: Arc<AtomicUsize>) -> CPageLease {
    let backing = Box::new(Backing { bytes, releases });
    let data = backing.bytes.as_ptr();
    let context = Box::into_raw(backing).cast::<c_void>();
    CPageLease {
        data,
        data_len: 4,
        generation: 7,
        release: Some(release_boxed_bytes),
        release_context: context,
        slices: std::ptr::null(),
        slice_count: 0,
    }
}

#[test]
fn clone_and_drop_release_foreign_page_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let page = engine_driver_host::PageLease::from_raw(boxed_page(*b"abcd", Arc::clone(&releases)))
        .expect("page");
    let clone = page.clone();
    drop(page);
    assert_eq!(clone.as_bytes(), b"abcd");
    drop(clone);
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn slice_outlives_page_wrapper_without_copying() {
    let releases = Arc::new(AtomicUsize::new(0));
    let page = engine_driver_host::PageLease::from_raw(boxed_page(*b"abcd", Arc::clone(&releases)))
        .expect("page");
    let slice = page.slice(1, 2).expect("slice");
    drop(page);
    assert_eq!(slice.as_bytes(), b"bc");
    assert_eq!(releases.load(Ordering::SeqCst), 0);
    drop(slice);
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn indexed_slice_borrows_the_same_backing() {
    let releases = Arc::new(AtomicUsize::new(0));
    let mut page = boxed_page(*b"abcd", Arc::clone(&releases));
    let ranges = Box::new([CPageSlice {
        offset: 1,
        length: 2,
    }]);
    page.slices = ranges.as_ptr();
    page.slice_count = 1;
    let page = engine_driver_host::PageLease::from_raw(page).expect("page");
    assert_eq!(page.indexed_slice(0).expect("slice").as_bytes(), b"bc");
    drop(page);
    drop(ranges);
}

#[test]
fn malformed_page_is_rejected_and_released_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let mut page = boxed_page(*b"abcd", Arc::clone(&releases));
    page.data = std::ptr::null();
    let error = match engine_driver_host::PageLease::from_raw(page) {
        Ok(_) => panic!("null data accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, PageError::InvalidMetadata(_)));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn oversized_page_is_rejected_and_released_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let mut page = boxed_page(*b"abcd", Arc::clone(&releases));
    page.data_len = isize::MAX as u64 + 1;
    let error = match engine_driver_host::PageLease::from_raw(page) {
        Ok(_) => panic!("oversized page accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, PageError::InvalidMetadata(_)));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn empty_page_with_null_data_is_safe() {
    let releases = Arc::new(AtomicUsize::new(0));
    let mut page = boxed_page(*b"abcd", Arc::clone(&releases));
    page.data = std::ptr::null();
    page.data_len = 0;
    let page = engine_driver_host::PageLease::from_raw(page).expect("empty page");
    assert!(page.as_bytes().is_empty());
    assert!(page.slice(0, 0).expect("empty slice").as_bytes().is_empty());
    drop(page);
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn misaligned_slice_index_is_rejected_and_released_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let mut page = boxed_page(*b"abcd", Arc::clone(&releases));
    let storage = [0_u64; 3];
    page.slices = unsafe { storage.as_ptr().cast::<u8>().add(1).cast::<CPageSlice>() };
    page.slice_count = 1;

    let error = match engine_driver_host::PageLease::from_raw(page) {
        Ok(_) => panic!("misaligned slice index accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, PageError::InvalidMetadata(_)));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn malformed_arrow_page_is_rejected_and_released_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let mut page = boxed_arrow(Arc::clone(&releases));
    page.array = std::ptr::null_mut();
    let error = match engine_driver_host::ArrowPageLease::from_raw(page) {
        Ok(_) => panic!("null Arrow array accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        engine_driver_host::ArrowPageError::InvalidMetadata(_)
    ));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn misaligned_arrow_container_is_rejected_and_released_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let mut page = boxed_arrow(Arc::clone(&releases));
    let storage = [0_u64; 2];
    page.schema = unsafe {
        storage
            .as_ptr()
            .cast::<u8>()
            .add(1)
            .cast_mut()
            .cast::<c_void>()
    };

    let error = match engine_driver_host::ArrowPageLease::from_raw(page) {
        Ok(_) => panic!("misaligned Arrow schema accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        engine_driver_host::ArrowPageError::InvalidMetadata(_)
    ));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

unsafe extern "C" fn release_ffi_pair(
    schema: *mut c_void,
    array: *mut c_void,
    _context: *mut c_void,
) {
    unsafe {
        drop(Box::from_raw(schema.cast::<FFI_ArrowSchema>()));
        drop(Box::from_raw(array.cast::<FFI_ArrowArray>()));
    }
}

unsafe extern "C" fn release_ffi_pair_with_counter(
    schema: *mut c_void,
    array: *mut c_void,
    context: *mut c_void,
) {
    unsafe {
        drop(Box::from_raw(schema.cast::<FFI_ArrowSchema>()));
        drop(Box::from_raw(array.cast::<FFI_ArrowArray>()));
        let releases = Arc::from_raw(context.cast::<AtomicUsize>());
        releases.fetch_add(1, Ordering::SeqCst);
    }
}

fn boxed_arrow_record_batch() -> (CArrowPageLease, *const i64) {
    let schema = std::sync::Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
    let values = Int64Array::from(vec![7, 8]);
    let value_ptr = values.values().as_ptr();
    let batch = RecordBatch::try_new(schema, vec![std::sync::Arc::new(values)]).expect("batch");
    let struct_array = StructArray::from(batch);
    let array = Box::new(FFI_ArrowArray::new(&struct_array.to_data()));
    let schema = Box::new(FFI_ArrowSchema::try_from(struct_array.data_type()).expect("schema"));
    let array_ptr = Box::into_raw(array).cast::<c_void>();
    let schema_ptr = Box::into_raw(schema).cast::<c_void>();
    (
        CArrowPageLease {
            schema: schema_ptr,
            array: array_ptr,
            row_count: 2,
            column_count: 1,
            generation: 9,
            release: Some(release_ffi_pair),
            release_context: std::ptr::null_mut(),
        },
        value_ptr,
    )
}

fn boxed_arrow_record_batch_with_counter(
    releases: Arc<AtomicUsize>,
) -> (CArrowPageLease, *const i64) {
    let schema = std::sync::Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
    let values = Int64Array::from(vec![7, 8]);
    let value_ptr = values.values().as_ptr();
    let batch = RecordBatch::try_new(schema, vec![std::sync::Arc::new(values)]).expect("batch");
    let struct_array = StructArray::from(batch);
    let array = Box::new(FFI_ArrowArray::new(&struct_array.to_data()));
    let schema = Box::new(FFI_ArrowSchema::try_from(struct_array.data_type()).expect("schema"));
    let array_ptr = Box::into_raw(array).cast::<c_void>();
    let schema_ptr = Box::into_raw(schema).cast::<c_void>();
    (
        CArrowPageLease {
            schema: schema_ptr,
            array: array_ptr,
            row_count: 2,
            column_count: 1,
            generation: 9,
            release: Some(release_ffi_pair_with_counter),
            release_context: Arc::into_raw(releases).cast_mut().cast::<c_void>(),
        },
        value_ptr,
    )
}

fn boxed_arrow_non_struct_with_counter(releases: Arc<AtomicUsize>) -> CArrowPageLease {
    let values = Int64Array::from(vec![7, 8]);
    let array = Box::new(FFI_ArrowArray::new(&values.to_data()));
    let schema = Box::new(FFI_ArrowSchema::try_from(values.data_type()).expect("schema"));
    CArrowPageLease {
        schema: Box::into_raw(schema).cast::<c_void>(),
        array: Box::into_raw(array).cast::<c_void>(),
        row_count: 2,
        column_count: 1,
        generation: 9,
        release: Some(release_ffi_pair_with_counter),
        release_context: Arc::into_raw(releases).cast_mut().cast::<c_void>(),
    }
}

#[test]
fn arrow_import_moves_c_data_without_copying_value_buffer() {
    let (raw, value_ptr) = boxed_arrow_record_batch();
    let lease = engine_driver_host::ArrowPageLease::from_raw(raw).expect("Arrow page");
    let imported = unsafe { lease.into_record_batch() }.expect("import Arrow page");
    let values = imported
        .batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("int64 column");
    assert_eq!(values.values().as_ptr(), value_ptr);
    assert_eq!(values.values().as_ref(), &[7, 8]);
}

#[test]
fn arrow_import_error_releases_export_containers_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let raw = boxed_arrow_non_struct_with_counter(Arc::clone(&releases));
    let lease = engine_driver_host::ArrowPageLease::from_raw(raw).expect("Arrow page");
    let error = unsafe { lease.into_record_batch() }.expect_err("non-struct Arrow root");

    assert!(matches!(
        error,
        engine_driver_host::ArrowPageError::InvalidMetadata(_)
    ));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn arrow_import_releases_export_containers_once() {
    let releases = Arc::new(AtomicUsize::new(0));
    let (raw, _) = boxed_arrow_record_batch_with_counter(Arc::clone(&releases));
    let lease = engine_driver_host::ArrowPageLease::from_raw(raw).expect("Arrow page");
    let imported = unsafe { lease.into_record_batch() }.expect("import Arrow page");
    let column = imported.batch.column(0).clone();

    assert_eq!(releases.load(Ordering::SeqCst), 1);
    drop(imported);
    drop(column);
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn imported_arrow_children_keep_generation_live_until_drop() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = DriverLoader.load(&staged).expect("load mock driver");
    let (raw, _) = boxed_arrow_record_batch();
    let lease = handle.adopt_arrow_page(raw).expect("adopt Arrow page");
    let imported = unsafe { lease.into_record_batch() }.expect("import Arrow page");
    let column = imported.batch.column(0).clone();

    drop(imported);
    assert!(matches!(
        handle.shutdown(),
        Err(LoaderError::ActiveOwners { leases: 1, .. })
    ));

    drop(column);
    handle
        .shutdown()
        .expect("generation stays live until the last child is dropped");
}

#[test]
fn generation_stays_live_until_last_page_lease() {
    let releases = Arc::new(AtomicUsize::new(0));
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = DriverLoader.load(&staged).expect("load mock driver");
    let page = handle
        .adopt_page(boxed_page(*b"abcd", Arc::clone(&releases)))
        .expect("adopt page");

    let error = handle.shutdown().expect_err("lease blocks shutdown");
    assert!(matches!(error, LoaderError::ActiveOwners { leases: 1, .. }));
    drop(page);
    handle.shutdown().expect("shutdown after lease release");
    assert_eq!(releases.load(Ordering::SeqCst), 1);
}

#[test]
fn loader_negotiates_page_table_and_acquires_without_copying() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = DriverLoader.load(&staged).expect("load mock driver");
    assert!(handle.page_function_table().is_some());
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");
    let page = session.acquire_page(1, 0, 1).expect("acquire page");
    assert_eq!(page.as_bytes(), b"hello");
    assert_eq!(
        page.indexed_slice(0).expect("indexed slice").as_bytes(),
        b"hello"
    );
    drop(page);
    session.disconnect().expect("disconnect");
}

#[test]
fn loader_negotiates_arrow_table_and_releases_opaque_export_once() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = DriverLoader.load(&staged).expect("load mock driver");
    assert!(handle.arrow_function_table().is_some());
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");
    let arrow = session
        .acquire_arrow_page(1, 0, 1)
        .expect("acquire Arrow page");
    assert!(!arrow.schema().is_null());
    assert!(!arrow.array().is_null());
    assert_eq!(arrow.row_count(), 1);
    assert_eq!(arrow.column_count(), 1);
    drop(arrow);
    session.disconnect().expect("disconnect");
}

#[test]
fn loader_does_not_read_past_header_only_function_tables() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock_with_features(&temp, &["short-tables"]);
    let handle = DriverLoader.load(&staged).expect("load short-table driver");
    assert!(handle.page_function_table().is_none());
    assert!(handle.arrow_function_table().is_none());
}

#[test]
fn sqlite_driver_exports_and_imports_arrow_without_value_copy() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_sqlite(&temp);
    let handle = DriverLoader.load(&staged).expect("load SQLite driver");
    assert!(handle
        .capabilities()
        .has(engine_driver_api::Capability::QueryArrowCData));
    let mut session = handle
        .connect(r#"{"uri":"sqlite://:memory:"}"#)
        .expect("connect SQLite driver");
    let page = session
        .open_query(
            r#"{"kind":"sql","operation_id":1,"database":null,"object":null,"offset":0,"limit":10,"payload":{"sql":"SELECT 7 AS id, 'Ada' AS name, X'00ff' AS payload"}}"#,
        )
        .expect("open SQLite query");
    let arrow = session
        .acquire_arrow_page(page.page_id, 0, page.row_count)
        .expect("acquire SQLite Arrow page");
    let imported = unsafe { arrow.into_record_batch() }.expect("import SQLite Arrow page");
    assert_eq!(imported.batch.num_rows(), 1);
    assert_eq!(imported.batch.num_columns(), 3);
    assert_eq!(
        imported
            .batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64 column")
            .value(0),
        7
    );
    assert_eq!(
        imported
            .batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .expect("string column")
            .value(0),
        "Ada"
    );
    assert_eq!(
        imported
            .batch
            .column(2)
            .as_any()
            .downcast_ref::<arrow_array::BinaryArray>()
            .expect("binary column")
            .value(0),
        &[0, 255]
    );
    session.release_page(page.page_id).expect("release page");
    session.disconnect().expect("disconnect SQLite driver");
}

fn stage_mock(temp: &TempDir) -> PathBuf {
    stage_mock_with_features(temp, &[])
}

fn stage_mock_with_features(temp: &TempDir, features: &[&str]) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-driver");
    // On top of the default features (no `--no-default-features` here).
    let key = if features.is_empty() {
        common::mock_key("")
    } else {
        format!("{}-with-defaults", common::mock_key(&features.join(",")))
    };
    let source = common::built_once(&key, |target_dir| {
        let output = Command::new(env!("CARGO"))
            .arg("build")
            .arg("--manifest-path")
            .arg(manifest.join("Cargo.toml"))
            .args(features.iter().flat_map(|feature| ["--features", *feature]))
            .arg("--release")
            .arg("--target-dir")
            .arg(target_dir)
            .output()
            .expect("build mock driver");
        assert!(
            output.status.success(),
            "mock build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        target_dir
            .join("release")
            .join(common::cdylib_file_name("mock_driver"))
    });
    let staged = temp.path().join(common::driver_file_name("mock"));
    fs::copy(source, &staged).expect("stage mock driver");
    staged
}

fn stage_sqlite(temp: &TempDir) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dbengine-sqlite");
    let source = common::built_once("dbengine-sqlite", |target_dir| {
        let output = Command::new(env!("CARGO"))
            .arg("build")
            .arg("--manifest-path")
            .arg(manifest.join("Cargo.toml"))
            .arg("--release")
            .arg("--target-dir")
            .arg(target_dir)
            .output()
            .expect("build SQLite driver");
        assert!(
            output.status.success(),
            "SQLite driver build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        target_dir
            .join("release")
            .join(common::cdylib_file_name("dbengine_sqlite"))
    });
    let staged = temp
        .path()
        .join(common::cdylib_file_name("dbengine_sqlite"));
    fs::copy(source, &staged).expect("stage SQLite driver");
    staged
}
