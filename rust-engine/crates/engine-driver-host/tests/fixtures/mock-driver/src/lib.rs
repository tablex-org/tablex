use std::ffi::{c_char, CStr, CString};
#[cfg(any(feature = "drop-barrier", feature = "init-marker"))]
use std::{env, fs};
#[cfg(feature = "drop-barrier")]
use std::{path::Path, thread, time::Duration};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn dbengine_abi_version() -> u32 {
    2
}

#[no_mangle]
pub extern "C" fn dbengine_capabilities() -> *mut c_char {
    #[cfg(feature = "readonly")]
    let features = r#"["navigator", "query.find", "query.page_lease", "query.arrow_c_data"]"#;
    #[cfg(feature = "missing-mutate")]
    let features =
        r#"["navigator", "query.find", "mutation.update", "query.page_lease", "query.arrow_c_data"]"#;
    #[cfg(feature = "short-tables")]
    let features = r#"["navigator", "query.find", "query.page_lease", "query.arrow_c_data"]"#;
    #[cfg(feature = "legacy-cells")]
    let features = r#"["navigator", "query.find"]"#;
    #[cfg(feature = "malformed-cells")]
    let features = r#"["navigator", "query.find"]"#;
    #[cfg(feature = "drop-barrier")]
    let features =
        r#"["navigator", "query.find", "mutation.update", "operation.cancel", "export.json", "query.page_lease", "query.arrow_c_data"]"#;
    #[cfg(feature = "init-marker")]
    let features =
        r#"["navigator", "query.find", "mutation.update", "operation.cancel", "export.json", "query.page_lease", "query.arrow_c_data"]"#;
    #[cfg(feature = "full")]
    let features =
        r#"["navigator", "query.find", "mutation.update", "operation.cancel", "export.json", "query.page_lease", "query.arrow_c_data"]"#;
    // Pair with `full` (`full,describe`): the `full` list plus the capability.
    #[cfg(any(feature = "describe", feature = "describe-missing-symbol"))]
    let features = r#"["navigator", "query.find", "mutation.update", "operation.cancel", "export.json", "metadata.table_properties", "query.page_lease", "query.arrow_c_data"]"#;
    alloc_string(&format!(
        r#"{{
        "abi_version": 2,
        "driver": "mock",
        "display_name": "Mock",
        "uri_schemes": ["mock"],
        "features": {features},
        "editors": [],
        "navigator_kinds": ["database", "collection"],
        "single_connection_pool": true
    }}"#
    ))
}

#[repr(C)]
pub struct MockPageLease {
    data: *const u8,
    data_len: u64,
    generation: u64,
    release: Option<unsafe extern "C" fn(*mut libc::c_void)>,
    release_context: *mut libc::c_void,
    slices: *const MockPageSlice,
    slice_count: u64,
}

#[repr(C)]
pub struct MockPageSlice {
    offset: u64,
    length: u64,
}

#[repr(C)]
pub struct MockPageFunctionTable {
    struct_size: u32,
    abi_version: u32,
    acquire_page: Option<unsafe extern "C" fn(u64, u64, u64, u64) -> MockPageLease>,
}

#[repr(C)]
struct MockFunctionTableHeader {
    struct_size: u32,
    abi_version: u32,
}

struct MockPageBacking {
    data: Box<[u8]>,
    slices: Box<[MockPageSlice]>,
}

unsafe extern "C" fn release_mock_page(context: *mut libc::c_void) {
    if !context.is_null() {
        unsafe { drop(Box::from_raw(context.cast::<MockPageBacking>())) };
    }
}

unsafe extern "C" fn acquire_mock_page(
    _connection_id: u64,
    _page_id: u64,
    _row_start: u64,
    _row_count: u64,
) -> MockPageLease {
    let data: Box<[u8]> = Box::from(*b"hello");
    let slices = Box::new([MockPageSlice {
        offset: 0,
        length: 5,
    }]);
    let backing = Box::new(MockPageBacking { data, slices });
    let data_ptr = backing.data.as_ptr();
    let data_len = backing.data.len() as u64;
    let slice_ptr = backing.slices.as_ptr();
    let slice_count = backing.slices.len() as u64;
    let context = Box::into_raw(backing).cast::<libc::c_void>();
    MockPageLease {
        data: data_ptr,
        data_len,
        generation: 1,
        release: Some(release_mock_page),
        release_context: context,
        slices: slice_ptr,
        slice_count,
    }
}

static MOCK_PAGE_FUNCTIONS: MockPageFunctionTable = MockPageFunctionTable {
    struct_size: std::mem::size_of::<MockPageFunctionTable>() as u32,
    abi_version: 1,
    acquire_page: Some(acquire_mock_page),
};

static MOCK_PAGE_FUNCTION_HEADER: MockFunctionTableHeader = MockFunctionTableHeader {
    struct_size: std::mem::size_of::<MockFunctionTableHeader>() as u32,
    abi_version: 1,
};

#[no_mangle]
#[cfg(not(any(feature = "legacy-cells", feature = "malformed-cells")))]
pub extern "C" fn dbengine_page_function_table() -> *const MockPageFunctionTable {
    #[cfg(feature = "short-tables")]
    return (&MOCK_PAGE_FUNCTION_HEADER as *const MockFunctionTableHeader)
        .cast::<MockPageFunctionTable>();
    #[cfg(not(feature = "short-tables"))]
    &MOCK_PAGE_FUNCTIONS
}

#[repr(C)]
pub struct MockArrowPageLease {
    schema: *mut libc::c_void,
    array: *mut libc::c_void,
    row_count: u64,
    column_count: u64,
    generation: u64,
    release: Option<unsafe extern "C" fn(
        *mut libc::c_void,
        *mut libc::c_void,
        *mut libc::c_void,
    )>,
    release_context: *mut libc::c_void,
}

#[repr(C)]
pub struct MockArrowFunctionTable {
    struct_size: u32,
    abi_version: u32,
    acquire_page: Option<unsafe extern "C" fn(u64, u64, u64, u64) -> MockArrowPageLease>,
}

struct MockArrowBacking {
    schema: Box<u8>,
    array: Box<u8>,
}

unsafe extern "C" fn release_mock_arrow_page(
    _schema: *mut libc::c_void,
    _array: *mut libc::c_void,
    context: *mut libc::c_void,
) {
    if !context.is_null() {
        unsafe { drop(Box::from_raw(context.cast::<MockArrowBacking>())) };
    }
}

unsafe extern "C" fn acquire_mock_arrow_page(
    _connection_id: u64,
    _page_id: u64,
    _row_start: u64,
    _row_count: u64,
) -> MockArrowPageLease {
    let backing = Box::new(MockArrowBacking {
        schema: Box::new(1),
        array: Box::new(2),
    });
    let schema = (&*backing.schema as *const u8).cast_mut().cast::<libc::c_void>();
    let array = (&*backing.array as *const u8).cast_mut().cast::<libc::c_void>();
    let context = Box::into_raw(backing).cast::<libc::c_void>();
    MockArrowPageLease {
        schema,
        array,
        row_count: 1,
        column_count: 1,
        generation: 1,
        release: Some(release_mock_arrow_page),
        release_context: context,
    }
}

static MOCK_ARROW_FUNCTIONS: MockArrowFunctionTable = MockArrowFunctionTable {
    struct_size: std::mem::size_of::<MockArrowFunctionTable>() as u32,
    abi_version: 1,
    acquire_page: Some(acquire_mock_arrow_page),
};

static MOCK_ARROW_FUNCTION_HEADER: MockFunctionTableHeader = MockFunctionTableHeader {
    struct_size: std::mem::size_of::<MockFunctionTableHeader>() as u32,
    abi_version: 1,
};

#[no_mangle]
pub extern "C" fn dbengine_arrow_function_table() -> *const MockArrowFunctionTable {
    #[cfg(feature = "short-tables")]
    return (&MOCK_ARROW_FUNCTION_HEADER as *const MockFunctionTableHeader)
        .cast::<MockArrowFunctionTable>();
    #[cfg(not(feature = "short-tables"))]
    &MOCK_ARROW_FUNCTIONS
}

#[no_mangle]
pub extern "C" fn dbengine_init() -> *mut c_char {
    #[cfg(feature = "init-marker")]
    if let Some(marker) = env::var_os("TABLEX_MOCK_INIT_MARKER") {
        fs::write(marker, b"initialized").expect("write init marker");
    }
    INITIALIZED.store(true, Ordering::Release);
    alloc_string(r#"{"ok":true,"value":{}}"#)
}

#[no_mangle]
pub extern "C" fn dbengine_shutdown() -> *mut c_char {
    #[cfg(feature = "drop-barrier")]
    wait_for_shutdown_release();
    INITIALIZED.store(false, Ordering::Release);
    alloc_string(r#"{"ok":true,"value":{}}"#)
}

#[cfg(feature = "drop-barrier")]
fn wait_for_shutdown_release() {
    let Some(entered) = env::var_os("TABLEX_MOCK_SHUTDOWN_ENTERED") else {
        return;
    };
    let Some(release) = env::var_os("TABLEX_MOCK_SHUTDOWN_RELEASE") else {
        return;
    };
    fs::write(entered, b"entered").expect("write shutdown barrier");
    while !Path::new(&release).exists() {
        thread::sleep(Duration::from_millis(1));
    }
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_connect(request_json: *const c_char) -> *mut c_char {
    // SAFETY: caller passes a valid NUL-terminated JSON string.
    let request = unsafe { CStr::from_ptr(request_json) }.to_string_lossy();
    if request.contains("mock://retryable") {
        return alloc_string(
            r#"{"ok":false,"error":{"code":"connect.interrupted","message":"mock connect interrupted","retryable":true}}"#,
        );
    }
    if request.contains("mock://fail") {
        return alloc_string(
            r#"{"ok":false,"error":{"code":"connect.failed","message":"mock connect refused","retryable":false}}"#,
        );
    }
    let id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    // A SQL driver reports the session's real dialect in its reply.
    let dialect = if request.contains("mock://no-backslash-escapes") {
        r#","sql_dialect":{"identifier_quote":"`","string_literal_quote":"'","supports_schemas":false,"database_qualified_names":true,"string_backslash_escapes":false}"#
    } else {
        ""
    };
    alloc_string(&format!(
        "{{\"ok\":true,\"value\":{{\"connection_id\":{id}{dialect}}}}}"
    ))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_disconnect(_connection_id: u64) -> *mut c_char {
    alloc_string(r#"{"ok":true,"value":{}}"#)
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_list_nav_nodes(
    _connection_id: u64,
    _request_json: *const c_char,
) -> *mut c_char {
    alloc_string(
        r#"{"ok":true,"value":{"nodes":[{"id":"database:app","kind":"database","label":"app","subtitle":null,"icon":"database","has_children":false,"payload":{"database":"app"},"actions":["refresh"]}],"next_cursor":null}}"#,
    )
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_open_query(
    _connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    let page_id = NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the host passes a valid NUL-terminated JSON string.
    let request = unsafe { CStr::from_ptr(request_json) }.to_string_lossy();
    PAGE_OBJECTS
        .lock()
        .unwrap()
        .push((page_id, json_str_after(&request, r#""object":""#)));
    let body = format!(
        r#"{{"operation_id":1,"page_id":{page_id},"columns":[{{"id":"_id","name":"_id","type":"object_id","nullable":false}}],"row_count":1,"total_count":1,"total_is_exact":true,"flags":{{"truncated":false,"cancelled":false,"read_only":false}}}}"#
    );
    alloc_string(&format!("{{\"ok\":true,\"value\":{body}}}"))
}

/// The object (collection) each open page was read from.
static PAGE_OBJECTS: std::sync::Mutex<Vec<(u64, String)>> = std::sync::Mutex::new(Vec::new());

fn json_str_after(json: &str, key: &str) -> String {
    json.split_once(key)
        .map(|(_, rest)| rest.chars().take_while(|c| *c != '"').collect())
        .unwrap_or_default()
}

/// Pages the host released: like a real driver, inspecting one is an error.
static RELEASED_PAGES: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

#[no_mangle]
pub unsafe extern "C" fn dbengine_inspect_row(page_id: u64, row_index: u64) -> *mut c_char {
    if RELEASED_PAGES.lock().unwrap().contains(&page_id) {
        return alloc_string(
            r#"{"ok":false,"error":{"code":"inspect.row_not_found","message":"unknown page or row","retryable":false}}"#,
        );
    }
    alloc_string(&format!(
        r#"{{"ok":true,"value":{{"page_id":{page_id},"row_index":{row_index},"fields":[{{"name":"_id","native_type":"object_id","display_text":"1","value":{{"$oid":"1"}},"editable":false}}],"mutation_identity":{{"_id":{{"$oid":"1"}}}}}}}}"#
    ))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_release_page(page_id: u64) -> *mut c_char {
    RELEASED_PAGES.lock().unwrap().push(page_id);
    alloc_string(r#"{"ok":true,"value":{}}"#)
}

#[no_mangle]
#[cfg(feature = "full")]
pub unsafe extern "C" fn dbengine_mutate(
    _connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    // SAFETY: the host passes a valid NUL-terminated JSON string.
    let request = unsafe { CStr::from_ptr(request_json) }.to_string_lossy();
    // Fixture: a request with `"block_until_cancelled":true` stays in flight
    // until `dbengine_cancel_operation` is called with its `operation_id`.
    if request.contains(r#""block_until_cancelled":true"#) {
        let operation_id = json_u64_after(&request, r#""operation_id":"#);
        for _ in 0..10_000 {
            if CANCELLED_OPERATIONS.lock().unwrap().contains(&operation_id) {
                return alloc_string(
                    r#"{"ok":false,"error":{"code":"operation.cancelled","message":"cancelled","retryable":false}}"#,
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    // Fixture: a mutation on database `scoped` drops, like the real MongoDB
    // driver, only the open pages read from the object it wrote.
    if request.contains(r#""database":"scoped""#) {
        let object = json_str_after(&request, r#""object":""#);
        let mut released = RELEASED_PAGES.lock().unwrap();
        let dropped: Vec<u64> = PAGE_OBJECTS
            .lock()
            .unwrap()
            .iter()
            .filter(|(page_id, page_object)| *page_object == object && !released.contains(page_id))
            .map(|(page_id, _)| *page_id)
            .collect();
        released.extend(&dropped);
        let ids: Vec<String> = dropped.iter().map(u64::to_string).collect();
        return alloc_string(&format!(
            r#"{{"ok":true,"value":{{"affected_rows":1,"invalidated_page_ids":[{}]}}}}"#,
            ids.join(",")
        ));
    }
    alloc_string(r#"{"ok":true,"value":{"affected_rows":1,"invalidated_page_ids":[1]}}"#)
}

#[cfg(feature = "full")]
static CANCELLED_OPERATIONS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

#[cfg(feature = "full")]
fn json_u64_after(json: &str, key: &str) -> u64 {
    json.split_once(key)
        .map(|(_, rest)| rest.chars().take_while(char::is_ascii_digit).collect::<String>())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// Echoes the request so tests can prove the JSON reached the driver.
#[no_mangle]
#[cfg(feature = "describe")]
pub unsafe extern "C" fn dbengine_describe_object(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    // SAFETY: the host passes a valid NUL-terminated JSON string.
    let request = unsafe { CStr::from_ptr(request_json) }.to_string_lossy();
    alloc_string(&format!(
        r#"{{"ok":true,"value":{{"connection_id":{connection_id},"echo":{request}}}}}"#
    ))
}

#[no_mangle]
#[cfg(feature = "full")]
pub unsafe extern "C" fn dbengine_cancel_operation(
    _connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    // SAFETY: the host passes a valid NUL-terminated JSON string.
    let request = unsafe { CStr::from_ptr(request_json) }.to_string_lossy();
    CANCELLED_OPERATIONS
        .lock()
        .unwrap()
        .push(json_u64_after(&request, r#""operation_id":"#));
    alloc_string(r#"{"ok":true,"value":{}}"#)
}

#[no_mangle]
#[cfg(feature = "full")]
pub unsafe extern "C" fn dbengine_export(
    _connection_id: u64,
    _request_json: *const c_char,
) -> *mut c_char {
    alloc_string(r#"{"ok":true,"value":{"rows_written":1,"bytes_written":5}}"#)
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_render_cell_batch(
    _page_id: u64,
    _row_start: u64,
    _row_count: u64,
    _column_start: u64,
    _column_count: u64,
    _out_batch: *mut *mut MockCellBatch,
    _out_error: *mut *mut c_char,
) -> i32 {
    // SAFETY: out pointers come from the trusted host in the same process.
    unsafe {
        // Fixture: leaks batch + cells + text per call; freed only via
        // dbengine_free_cell_text_batch below, matching the real ABI contract.
        let (offset, length) = if cfg!(feature = "malformed-cells") {
            (5, 1)
        } else {
            (0, 5)
        };
        let cells = Box::new(MockCellText {
            row: 0,
            column: 0,
            offset,
            length,
            flags: 16,
        });
        let text = Box::new(*b"hello");
        *_out_batch = Box::into_raw(Box::new(MockCellBatch {
            cells: Box::into_raw(cells),
            cell_count: 1,
            text: Box::into_raw(text).cast::<u8>() as *const c_char,
            text_len: 5,
        }));
    }
    0
}

#[repr(C)]
pub struct MockCellBatch {
    pub cells: *const MockCellText,
    pub cell_count: u64,
    pub text: *const c_char,
    pub text_len: u64,
}

#[repr(C)]
pub struct MockCellText {
    pub row: u64,
    pub column: u64,
    pub offset: u64,
    pub length: u64,
    pub flags: u32,
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_free_cell_text_batch(batch: *mut MockCellBatch) {
    if !batch.is_null() {
        // SAFETY: all pointers were allocated by render_cell_batch below.
        unsafe {
            let batch = Box::from_raw(batch);
            if !batch.cells.is_null() {
                drop(Box::from_raw(batch.cells as *mut MockCellText));
            }
            if !batch.text.is_null() {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    batch.text as *mut u8,
                    batch.text_len as usize,
                )));
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(value: *mut c_char) {
    if !value.is_null() {
        // SAFETY: value was allocated by alloc_string in this fixture.
        unsafe { drop(CString::from_raw(value)) };
    }
}

fn alloc_string(value: &str) -> *mut c_char {
    CString::new(value)
        .expect("fixture JSON has no NUL")
        .into_raw()
}
