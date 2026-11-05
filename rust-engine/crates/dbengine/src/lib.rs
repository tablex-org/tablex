mod arrow;
mod connection;
mod driver;
mod engine;
mod error;
pub mod export;
pub use arrow::documents_to_arrow_cdata;
pub use arrow::NATIVE_TYPE_METADATA_KEY;
pub use engine::{
    ConnectionId, DatabaseKind, DeleteRowRequest, Engine, InsertRowRequest, MutationValue,
    QueryRequest, QueryResultBatch, SqlMutation, UpdateCellRequest,
};
pub use error::{DbEngineError, Result};
pub use export::{ExportDialect, ExportFormat, TableExporter};
mod autocomplete;
mod query_parser;

pub mod catalog;
pub mod name_filter;
pub use catalog::{CatalogColumn, CatalogObject, CatalogObjectKind};
mod ai_schema;
pub mod api;
mod db;
mod field_parser;
mod mongo_script_parser;
pub use mongo_script_parser::{inspect_read_script, MongoReadScript};
mod pipeline;
mod render;
mod sql_completion;
mod sql_risk;
mod syntax;

use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use bson::Document;
use connection::ConnectionManager;
use db::DatabaseConnection;
use driver::DatabaseDriver;
use engine_ffi_util::{error_json, success_json};
use engine_storage::{aggregate, ai_chat, connection as saved_connection, history, view_state};
use futures::StreamExt;
use mongo_script_parser::ScriptPlan;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tracing::{error, info};

const OPERATION_TIMEOUT_SECS: u64 = 30;

fn detail_perf_logs_enabled() -> bool {
    std::env::var("TABLELEAF_DETAIL_PERF")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn query_perf_logs_enabled() -> bool {
    std::env::var("TABLELEAF_QUERY_PERF")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn ffi_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

fn write_json_out<T: serde::Serialize>(value: &T, out_json: *mut *mut c_char) -> i32 {
    let json = match serde_json::to_string(value) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("[dbengine] JSON encode failed: {error}");
            return -1;
        }
    };
    let c_str = match CString::new(json) {
        Ok(c_str) => c_str,
        Err(_) => return -1,
    };
    unsafe { *out_json = c_str.into_raw() };
    0
}

lazy_static::lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .thread_name("dbengine-worker")
            .build()
            .expect("Failed to create tokio runtime")
    };

    static ref ENGINE: RwLock<EngineState> = RwLock::new(EngineState::new());
}

struct CountCacheEntry {
    filter: Document,
    count: usize,
}

/// Represents a single field in a document for display in the detail pane.
#[repr(C)]
pub struct CFieldInfo {
    pub key: *mut c_char,
    pub field_type: *mut c_char,
    pub display_value: *mut c_char,
    pub is_primary_key: bool,
}

/// Array of CFieldInfo returned from FFI.
#[repr(C)]
pub struct CFieldInfoArray {
    pub fields: *mut CFieldInfo,
    pub count: usize,
}

/// One cell in a rendered batch. Offsets/lengths index into the batch's
/// `text_buffer` (a single UTF-8 buffer shared by all cells).
#[repr(C)]
pub struct CRenderedCell {
    pub row: usize,
    pub column: usize,
    pub offset: usize,
    pub length: usize,
    pub flags: u32,
}

/// All display strings for one visible (row, col) range. Caller must free
/// with `dbengine_free_rendered_cell_batch`.
#[repr(C)]
pub struct CRenderedCellBatch {
    pub cells: *mut CRenderedCell,
    pub cell_count: usize,
    pub text_buffer: *mut u8,
    pub text_buffer_len: usize,
}

struct FieldCacheEntry {
    fields: Vec<field_parser::ParsedField>,
}

struct DocumentCacheEntry {
    docs: Vec<Document>,
    column_names: Vec<String>,
    estimated_bytes: usize,
}

struct EngineState {
    manager: ConnectionManager,
    postgres_connections:
        std::collections::HashMap<u64, std::sync::Arc<crate::db::postgres::PostgresConn>>,
    sqlite_connections:
        std::collections::HashMap<u64, std::sync::Arc<crate::db::sqlite::SQLiteConn>>,
    count_cache: HashMap<String, CountCacheEntry>,
    count_cache_order: VecDeque<String>,
    field_cache: HashMap<String, FieldCacheEntry>,
    field_cache_order: VecDeque<String>,
    document_cache: HashMap<u64, DocumentCacheEntry>,
    document_cache_order: VecDeque<u64>,
    document_cache_bytes: usize,
    next_session_id: u64,
    pub cancel_flag: Arc<AtomicBool>,
    pub pipeline_registry: HashMap<String, crate::pipeline::PipelineDefinition>,
}

const MAX_COUNT_CACHE_SIZE: usize = 50;
const MAX_FIELD_CACHE_SIZE: usize = 100;
const MAX_DOCUMENT_CACHE_SIZE: usize = 8;
const MAX_DOCUMENT_CACHE_BYTES: usize = 256 * 1024 * 1024;

impl EngineState {
    fn new() -> Self {
        Self {
            manager: ConnectionManager::new(),
            postgres_connections: std::collections::HashMap::new(),
            sqlite_connections: std::collections::HashMap::new(),
            count_cache: HashMap::new(),
            count_cache_order: VecDeque::new(),
            field_cache: HashMap::new(),
            field_cache_order: VecDeque::new(),
            document_cache: HashMap::new(),
            document_cache_order: VecDeque::new(),
            document_cache_bytes: 0,
            next_session_id: 1,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            pipeline_registry: HashMap::new(),
        }
    }

    fn cache_key(db: &str, collection: &str, query_json: &str) -> String {
        format!("{}:{}:{}", db, collection, query_json)
    }

    fn invalidate_collection(&mut self, db: &str, collection: &str) {
        let prefix = format!("{}:{}:", db, collection);
        self.count_cache.retain(|key, _| !key.starts_with(&prefix));
        self.count_cache_order
            .retain(|key| !key.starts_with(&prefix));
    }

    fn insert_count_cache(&mut self, key: String, entry: CountCacheEntry) {
        self.count_cache.insert(key.clone(), entry);
        self.count_cache_order.push_back(key.clone());

        // Evict oldest if over limit (LRU)
        while self.count_cache.len() > MAX_COUNT_CACHE_SIZE {
            if let Some(oldest_key) = self.count_cache_order.pop_front() {
                self.count_cache.remove(&oldest_key);
            }
        }
    }

    fn insert_field_cache(&mut self, key: String, entry: FieldCacheEntry) {
        self.field_cache.insert(key.clone(), entry);
        self.field_cache_order.push_back(key.clone());

        // Evict oldest if over limit (LRU)
        while self.field_cache.len() > MAX_FIELD_CACHE_SIZE {
            if let Some(oldest_key) = self.field_cache_order.pop_front() {
                self.field_cache.remove(&oldest_key);
            }
        }
    }

    fn cache_documents(&mut self, docs: Vec<Document>, column_names: Vec<String>) -> u64 {
        let session_id = self.next_session_id;
        self.next_session_id += 1;

        // Estimate BSON memory footprint for this page.
        // bson::to_vec gives the exact BSON-encoded size, used as a memory budget hint.
        let estimated_bytes: usize = docs
            .iter()
            .map(|d| bson::to_vec(d).map(|v| v.len()).unwrap_or(0))
            .sum();

        self.document_cache.insert(
            session_id,
            DocumentCacheEntry {
                docs,
                column_names,
                estimated_bytes,
            },
        );
        self.document_cache_order.push_back(session_id);
        self.document_cache_bytes += estimated_bytes;

        // Evict oldest sessions while exceeding count or byte budget (LRU).
        self.evict_document_sessions_until_within_budget();

        session_id
    }

    fn get_cached_document(&self, session_id: u64, row_index: usize) -> Option<&Document> {
        self.document_cache
            .get(&session_id)
            .and_then(|entry| entry.docs.get(row_index))
    }

    /// Remove a single document session and its related field cache entries.
    /// Updates the byte counter and order queue.
    fn remove_document_session(&mut self, session_id: u64) {
        if let Some(entry) = self.document_cache.remove(&session_id) {
            self.document_cache_bytes = self
                .document_cache_bytes
                .saturating_sub(entry.estimated_bytes);
        }
        self.document_cache_order.retain(|id| *id != session_id);
        self.evict_field_cache_for_session(session_id);
    }

    /// Evict field cache entries belonging to a specific document session.
    fn evict_field_cache_for_session(&mut self, session_id: u64) {
        let prefix = format!("fields_row:{}:", session_id);
        let keys_to_remove: Vec<String> = self
            .field_cache
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys_to_remove {
            self.field_cache.remove(&key);
            self.field_cache_order.retain(|k| k != &key);
        }
    }

    /// Evict oldest sessions until both count and byte budgets are satisfied.
    fn evict_document_sessions_until_within_budget(&mut self) {
        while self.document_cache.len() > MAX_DOCUMENT_CACHE_SIZE
            || self.document_cache_bytes > MAX_DOCUMENT_CACHE_BYTES
        {
            if let Some(oldest_id) = self.document_cache_order.pop_front() {
                self.remove_document_session(oldest_id);
            } else {
                break;
            }
        }
    }

    fn invalidate_session(&mut self, session_id: u64) {
        self.remove_document_session(session_id);
    }
}

/// Confirm the dylib is loaded. Returns true.
#[no_mangle]
pub extern "C" fn dbengine_is_loaded() -> bool {
    true
}

/// Free a CFieldInfoArray returned from FFI.
#[no_mangle]
pub extern "C" fn dbengine_free_field_info_array(arr: *mut CFieldInfoArray) {
    if arr.is_null() {
        return;
    }
    unsafe {
        let arr_ref = &*arr;
        if !arr_ref.fields.is_null() && arr_ref.count > 0 {
            let fields = std::slice::from_raw_parts_mut(arr_ref.fields, arr_ref.count);
            for field in fields.iter_mut() {
                if !field.key.is_null() {
                    let _ = CString::from_raw(field.key);
                }
                if !field.field_type.is_null() {
                    let _ = CString::from_raw(field.field_type);
                }
                if !field.display_value.is_null() {
                    let _ = CString::from_raw(field.display_value);
                }
            }
            let _ = Vec::from_raw_parts(arr_ref.fields, arr_ref.count, arr_ref.count);
        }
    }
    unsafe {
        let _ = Box::from_raw(arr);
    }
}

/// Parse a document's fields for the detail pane.
///
/// Accepts a JSON string, parses it into typed fields, caches the result,
/// and returns a CFieldInfoArray. The caller must free the result with
/// `dbengine_free_field_info_array`.
#[no_mangle]
pub extern "C" fn dbengine_get_document_fields(json: *const c_char) -> *mut CFieldInfoArray {
    let json_str = unsafe { CStr::from_ptr(json).to_string_lossy().into_owned() };

    // Compute cache key from JSON content hash
    let cache_key = format!("fields:{}", simple_hash(&json_str));

    let mut state = ENGINE.write().unwrap();

    // Check cache first
    if let Some(entry) = state.field_cache.get(&cache_key) {
        // Cache hit — convert cached ParsedField to CFieldInfo
        return parsed_fields_to_carray(&entry.fields);
    }

    // Cache miss — parse the JSON
    let parsed = match field_parser::parse_json_fields(&json_str) {
        Some(f) => f,
        None => {
            return Box::into_raw(Box::new(CFieldInfoArray {
                fields: std::ptr::null_mut(),
                count: 0,
            }));
        }
    };

    // Cache the parsed fields (store ParsedField, not CFieldInfo, for Send/Sync)
    state.insert_field_cache(cache_key.clone(), FieldCacheEntry { fields: parsed });

    // Convert to C array
    let entry = state.field_cache.get(&cache_key).unwrap();
    parsed_fields_to_carray(&entry.fields)
}

/// Parse a document's fields directly from cached BSON data (no JSON round-trip).
///
/// Accepts a session ID (from Arrow query response) and row index,
/// reads the BSON document directly from the cache, parses it into typed fields,
/// and returns a CFieldInfoArray. The caller must free the result with
/// `dbengine_free_field_info_array`.
#[no_mangle]
pub extern "C" fn dbengine_get_document_fields_by_row(
    session_id: u64,
    row_index: usize,
) -> *mut CFieldInfoArray {
    let cache_key = format!("fields_row:{}:{}", session_id, row_index);
    let t0 = std::time::Instant::now();

    // Check field cache first (avoid re-parsing BSON on every row click)
    {
        let state = ENGINE.read().unwrap();
        if let Some(entry) = state.field_cache.get(&cache_key) {
            let display_bytes: usize = entry.fields.iter().map(|f| f.display_value.len()).sum();
            let fields_count = entry.fields.len();
            let elapsed_us = t0.elapsed().as_micros();
            let sessions = state.document_cache.len();
            let bytes_total = state.document_cache_bytes;
            if detail_perf_logs_enabled() {
                println!(
                    "[rust-detail-row] session={} row={} cache=hit fields={} display_bytes={} sessions={} doc_bytes={} elapsed_us={}",
                    session_id, row_index, fields_count, display_bytes, sessions, bytes_total, elapsed_us
                );
            }
            return parsed_fields_to_carray(&entry.fields);
        }
    }

    // Cache miss — parse from BSON
    let parsed = {
        let state = ENGINE.read().unwrap();
        match state.get_cached_document(session_id, row_index) {
            Some(doc) => field_parser::parse_document_fields(doc),
            None => {
                let sessions = state.document_cache.len();
                let bytes_total = state.document_cache_bytes;
                if detail_perf_logs_enabled() {
                    println!(
                        "[rust-detail-row] session={} row={} cache=miss reason=no-cached-doc sessions={} doc_bytes={}",
                        session_id, row_index, sessions, bytes_total
                    );
                }
                return Box::into_raw(Box::new(CFieldInfoArray {
                    fields: std::ptr::null_mut(),
                    count: 0,
                }));
            }
        }
    };

    let display_bytes: usize = parsed.iter().map(|f| f.display_value.len()).sum();
    let fields_count = parsed.len();
    let parse_us = t0.elapsed().as_micros();

    // Cache the parsed fields for future hits
    {
        let mut state = ENGINE.write().unwrap();
        state.insert_field_cache(
            cache_key,
            FieldCacheEntry {
                fields: parsed.clone(),
            },
        );
        if detail_perf_logs_enabled() {
            println!(
                "[rust-detail-row] session={} row={} cache=miss fields={} display_bytes={} sessions={} doc_bytes={} parse_us={}",
                session_id, row_index, fields_count, display_bytes,
                state.document_cache.len(), state.document_cache_bytes, parse_us
            );
        }
    }

    parsed_fields_to_carray(&parsed)
}

/// Invalidate a document cache session (e.g., when collection changes).
#[no_mangle]
pub extern "C" fn dbengine_invalidate_document_session(session_id: u64) {
    let mut state = ENGINE.write().unwrap();
    state.invalidate_session(session_id);
}

/// Render display strings for a visible (row, col) range from a document
/// session. Returns null if `session_id` is unknown.
///
/// All display formatting happens off the main thread (we only hold a read
/// lock on the engine, not the executor runtime). The returned batch must be
/// freed with `dbengine_free_rendered_cell_batch`.
#[no_mangle]
pub extern "C" fn dbengine_render_cell_text_batch(
    session_id: u64,
    row_start: usize,
    row_end_exclusive: usize,
    col_start: usize,
    col_end_exclusive: usize,
    max_bytes_per_cell: usize,
) -> *mut CRenderedCellBatch {
    let engine = ENGINE.read().unwrap();
    let entry = match engine.document_cache.get(&session_id) {
        Some(e) => e,
        None => return std::ptr::null_mut(),
    };
    let batch = render::render_cell_text_batch(
        &entry.docs,
        &entry.column_names,
        row_start,
        row_end_exclusive,
        col_start,
        col_end_exclusive,
        max_bytes_per_cell,
    );
    drop(engine);

    // Build the FFI batch. Convert each owned Vec to a Box<[T]> so the
    // capacity equals the length, then leak the Box.
    let c_cells: Box<[CRenderedCell]> = batch
        .cells
        .into_iter()
        .map(|c| CRenderedCell {
            row: c.row,
            column: c.column,
            offset: c.offset,
            length: c.length,
            flags: c.flags,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let cell_count = c_cells.len();
    let cells_ptr = c_cells.as_ptr() as *mut CRenderedCell;
    let _cells_box = Box::into_raw(c_cells);

    let text_buffer: Box<[u8]> = batch.text_buffer.into_boxed_slice();
    let text_buffer_len = text_buffer.len();
    let text_buffer_ptr = text_buffer.as_ptr() as *mut u8;
    let _text_buffer_box = Box::into_raw(text_buffer);

    // Both leaked slice boxes are reconstructed in `dbengine_free_rendered_cell_batch`
    // by re-boxing the raw slice pointers.

    Box::into_raw(Box::new(CRenderedCellBatch {
        cells: cells_ptr,
        cell_count,
        text_buffer: text_buffer_ptr,
        text_buffer_len,
    }))
}

/// Free a `CRenderedCellBatch` returned by `dbengine_render_cell_text_batch`.
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_rendered_cell_batch(batch: *mut CRenderedCellBatch) {
    if batch.is_null() {
        return;
    }
    let batch_ref = &*batch;
    if !batch_ref.cells.is_null() && batch_ref.cell_count > 0 {
        let slice_ptr = std::ptr::slice_from_raw_parts_mut(batch_ref.cells, batch_ref.cell_count);
        let _ = Box::from_raw(slice_ptr);
    }
    if !batch_ref.text_buffer.is_null() && batch_ref.text_buffer_len > 0 {
        let slice_ptr =
            std::ptr::slice_from_raw_parts_mut(batch_ref.text_buffer, batch_ref.text_buffer_len);
        let _ = Box::from_raw(slice_ptr);
    }
    let _ = Box::from_raw(batch);
}

/// Convert a slice of ParsedField to a heap-allocated CFieldInfoArray.
fn parsed_fields_to_carray(fields: &[field_parser::ParsedField]) -> *mut CFieldInfoArray {
    let c_fields: Vec<CFieldInfo> = fields
        .iter()
        .map(|f| CFieldInfo {
            key: CString::new(f.key.as_str()).unwrap().into_raw(),
            field_type: CString::new(f.field_type.as_str()).unwrap().into_raw(),
            display_value: CString::new(f.display_value.as_str()).unwrap().into_raw(),
            is_primary_key: f.is_primary_key,
        })
        .collect();

    let count = c_fields.len();
    let fields_ptr = c_fields.as_ptr() as *mut CFieldInfo;

    // Leak the Vec so Swift owns it
    std::mem::forget(c_fields);

    Box::into_raw(Box::new(CFieldInfoArray {
        fields: fields_ptr,
        count,
    }))
}

/// Clear the field cache. If collection is non-empty, clear only entries for that collection.
#[no_mangle]
pub extern "C" fn dbengine_clear_field_cache(collection: *const c_char) {
    let mut state = ENGINE.write().unwrap();
    if collection.is_null() {
        state.field_cache.clear();
        state.field_cache_order.clear();
        return;
    }
    let collection_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    if collection_str.is_empty() {
        state.field_cache.clear();
        state.field_cache_order.clear();
    } else {
        state
            .field_cache
            .retain(|k, _| !k.contains(&format!(":{}:", collection_str)));
        state
            .field_cache_order
            .retain(|k| !k.contains(&format!(":{}:", collection_str)));
    }
}

/// Clear all caches: document_cache, field_cache, and count_cache.
/// Call this when switching collections or connections to free memory.
#[no_mangle]
pub extern "C" fn dbengine_clear_all_caches() {
    let mut state = ENGINE.write().unwrap();
    state.document_cache.clear();
    state.document_cache_order.clear();
    state.document_cache_bytes = 0;
    state.field_cache.clear();
    state.field_cache_order.clear();
    state.count_cache.clear();
    state.count_cache_order.clear();
    info!("All caches cleared");
}

fn simple_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Run a Send future inside the shared runtime with a timeout.
/// The timeout is created INSIDE block_on so the Sleep handle
/// has access to the runtime context.
fn run_with_timeout<F, T>(future: F) -> Option<T>
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    RUNTIME.block_on(async move {
        match tokio::time::timeout(Duration::from_secs(OPERATION_TIMEOUT_SECS), future).await {
            Ok(result) => Some(result),
            Err(_) => {
                error!("Operation timed out after {}s", OPERATION_TIMEOUT_SECS);
                None
            }
        }
    })
}

/// Initialize the engine. Sets up tracing subscriber.
///
/// # Safety
/// This function is safe to call. It must be called before any other
/// `dbengine_*` functions to ensure tracing is initialized.
#[no_mangle]
pub extern "C" fn dbengine_init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    info!("dbengine initialized");
}

/// Connect to a database instance (MongoDB or PostgreSQL).
///
/// # Safety
/// `uri` and `name` must be valid, null-terminated C strings.
/// They are read once and not stored beyond this call.
///
/// Returns a JSON string on both success and error. On success, the JSON is:
///   `{"id": <connection_id>}`
/// On error, the JSON is:
///   `{"error": "<description>"}`
/// The caller must free the returned pointer with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub unsafe extern "C" fn dbengine_connect(uri: *const c_char, name: *const c_char) -> *mut c_char {
    fn json_response(obj: serde_json::Value) -> *mut c_char {
        match serde_json::to_string(&obj) {
            Ok(s) => match CString::new(s) {
                Ok(cs) => cs.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Err(_) => std::ptr::null_mut(),
        }
    }

    if uri.is_null() || name.is_null() {
        error!("dbengine_connect: null pointer argument");
        return json_response(serde_json::json!({"error": "Null pointer argument"}));
    }

    let uri_str = unsafe { CStr::from_ptr(uri).to_string_lossy().into_owned() };
    let name_str = unsafe { CStr::from_ptr(name).to_string_lossy().into_owned() };

    // Detect database type by URI prefix
    let is_postgres = uri_str.starts_with("postgres://") || uri_str.starts_with("postgresql://");
    let is_sqlite = uri_str.starts_with("sqlite://");

    if is_sqlite {
        let conn = match crate::db::sqlite::SQLiteConn::connect(&uri_str) {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "SQLite connection failed");
                return json_response(serde_json::json!({"error": e}));
            }
        };

        let mut engine = match ENGINE.write() {
            Ok(guard) => guard,
            Err(e) => {
                error!("Engine lock poisoned: {}", e);
                return json_response(
                    serde_json::json!({"error": "Internal engine lock poisoned"}),
                );
            }
        };

        let id = engine.next_session_id;
        engine.next_session_id += 1;
        engine.sqlite_connections.insert(id, Arc::new(conn));
        info!(conn_id = id, "SQLite connection established via FFI");
        return json_response(serde_json::json!({"id": id}));
    } else if is_postgres {
        // PostgreSQL connection
        let conn_result = run_with_timeout({
            let uri = uri_str.clone();
            async move { db::postgres::PostgresConn::connect(&uri).await }
        });

        let conn = match conn_result {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                error!(error = %e, "Postgres connection failed");
                return json_response(serde_json::json!({"error": e}));
            }
            None => {
                error!("Postgres connection timed out");
                return json_response(
                    serde_json::json!({"error": "Postgres connection timed out"}),
                );
            }
        };

        let mut engine = match ENGINE.write() {
            Ok(guard) => guard,
            Err(e) => {
                error!("Engine lock poisoned: {}", e);
                return json_response(
                    serde_json::json!({"error": "Internal engine lock poisoned"}),
                );
            }
        };

        let id = engine.next_session_id;
        engine.next_session_id += 1;
        engine.postgres_connections.insert(id, Arc::new(conn));
        info!(conn_id = id, "Postgres connection established via FFI");
        return json_response(serde_json::json!({"id": id}));
    } else {
        // MongoDB connection (existing logic)
        let client_result = run_with_timeout({
            let uri = uri_str.clone();
            async move {
                let options = mongodb::options::ClientOptions::parse(&uri)
                    .await
                    .map_err(|e| error::DbEngineError::Connection(e.to_string()))?;

                let client = std::sync::Arc::new(
                    mongodb::Client::with_options(options)
                        .map_err(|e| error::DbEngineError::Connection(e.to_string()))?,
                );

                client
                    .list_database_names()
                    .await
                    .map_err(|e| error::DbEngineError::Connection(e.to_string()))?;

                Ok::<_, error::DbEngineError>(client)
            }
        });

        let client = match client_result {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                error!(error = %e, "MongoDB connection failed during setup");
                return json_response(serde_json::json!({"error": e.to_string()}));
            }
            None => {
                error!("MongoDB connection timed out during setup");
                return json_response(serde_json::json!({"error": "MongoDB connection timed out"}));
            }
        };

        let mut engine = match ENGINE.write() {
            Ok(guard) => guard,
            Err(e) => {
                error!("Engine lock poisoned: {}", e);
                return json_response(
                    serde_json::json!({"error": "Internal engine lock poisoned"}),
                );
            }
        };

        let id = engine.next_session_id;
        engine.next_session_id += 1;
        engine
            .manager
            .insert_client_with_id(id, uri_str, name_str, client);
        info!(conn_id = id, "MongoDB connection established via FFI");
        return json_response(serde_json::json!({"id": id}));
    }
}

/// Disconnect from a database instance.
///
/// # Safety
/// `conn_id` must be a valid connection ID returned by `dbengine_connect`.
///
/// Returns 0 on success, -1 on failure.
#[no_mangle]
pub extern "C" fn dbengine_disconnect(conn_id: u64) -> i32 {
    match ENGINE.write() {
        Ok(mut engine) => {
            // Try MongoDB first
            if engine.manager.remove_client(conn_id).is_ok() {
                info!(conn_id = conn_id, "MongoDB disconnected via FFI");
                return 0;
            }
            // Try PostgreSQL
            if engine.postgres_connections.remove(&conn_id).is_some() {
                info!(conn_id = conn_id, "Postgres disconnected via FFI");
                return 0;
            }
            if engine.sqlite_connections.remove(&conn_id).is_some() {
                info!(conn_id = conn_id, "SQLite disconnected via FFI");
                return 0;
            }
            error!(conn_id = conn_id, "Connection not found for disconnect");
            -1
        }
        Err(e) => {
            error!("Engine lock poisoned during disconnect: {}", e);
            -1
        }
    }
}

/// List all databases for a connection.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
///
/// Returns a JSON array of database names as a C string.
/// Caller must free with `dbengine_free_string`.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_list_databases(conn_id: u64) -> *mut c_char {
    let engine = match ENGINE.read() {
        Ok(e) => e,
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    // Try MongoDB first
    if let Ok(client) = engine.manager.get_client(conn_id) {
        drop(engine);
        let result = run_with_timeout(async move {
            let driver = driver::MongoDBDriver::new(client);
            driver.list_databases().await
        });

        return match result {
            Some(Ok(dbs)) => {
                info!(count = dbs.len(), "Listed MongoDB databases via FFI");
                success_json(&dbs)
            }
            Some(Err(e)) => {
                error!(error = %e, "List MongoDB databases failed via FFI");
                error_json(&e.to_string())
            }
            None => error_json("Operation timed out"),
        };
    }

    // Try PostgreSQL
    if let Some(conn) = engine.postgres_connections.get(&conn_id).cloned() {
        drop(engine);
        let result = run_with_timeout(async move { conn.list_databases().await });

        return match result {
            Some(Ok(dbs)) => {
                info!(count = dbs.len(), "Listed Postgres databases via FFI");
                success_json(&dbs)
            }
            Some(Err(e)) => {
                error!(error = %e, "List Postgres databases failed via FFI");
                error_json(&e.to_string())
            }
            None => error_json("Operation timed out"),
        };
    }

    error_json(&format!("Connection {} not found", conn_id))
}

/// List all collections/tables in a database.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db` must be a valid, null-terminated C string.
///
/// Returns a JSON array of collection/table names as a C string.
/// Caller must free with `dbengine_free_string`.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_list_collections(conn_id: u64, db: *const c_char) -> *mut c_char {
    if db.is_null() {
        return error_json("null database name");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };

    let engine = match ENGINE.read() {
        Ok(e) => e,
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    // Try MongoDB first
    if let Ok(client) = engine.manager.get_client(conn_id) {
        drop(engine);
        let result = run_with_timeout({
            let db = db_str.clone();
            async move {
                let driver = driver::MongoDBDriver::new(client);
                driver.list_collections(&db).await
            }
        });

        return match result {
            Some(Ok(colls)) => {
                info!(db = %db_str, count = colls.len(), "Listed MongoDB collections via FFI");
                success_json(&colls)
            }
            Some(Err(e)) => {
                error!(db = %db_str, error = %e, "List MongoDB collections failed via FFI");
                error_json(&e.to_string())
            }
            None => error_json("Operation timed out"),
        };
    }

    // Try PostgreSQL
    if let Some(conn) = engine.postgres_connections.get(&conn_id).cloned() {
        drop(engine);
        let result = run_with_timeout({
            let db = db_str.clone();
            async move { conn.list_collections(&db).await }
        });

        return match result {
            Some(Ok(tables)) => {
                info!(db = %db_str, count = tables.len(), "Listed Postgres tables via FFI");
                success_json(&tables)
            }
            Some(Err(e)) => {
                error!(db = %db_str, error = %e, "List Postgres tables failed via FFI");
                error_json(&e.to_string())
            }
            None => error_json("Operation timed out"),
        };
    }

    error_json(&format!("Connection {} not found", conn_id))
}

/// Return the current/default PostgreSQL database for a connection.
///
/// Returns a JSON string. Caller must free with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_current_database(conn_id: u64) -> *mut c_char {
    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    match conn {
        Some(conn) => success_json(&conn.default_database()),
        None => error_json(&format!("No PostgreSQL connection for conn_id={}", conn_id)),
    }
}

#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_list_postgres_schemas(conn_id: u64, db: *const c_char) -> *mut c_char {
    if db.is_null() {
        return error_json("null database name");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={}", conn_id)),
    };

    let db_for_log = db_str.clone();
    let result = run_with_timeout(async move { conn.list_schemas(&db_str).await });

    match result {
        Some(Ok(schemas)) => {
            info!(db = %db_for_log, count = schemas.len(), "Listed Postgres schemas via FFI");
            success_json(&schemas)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, error = %e, "List Postgres schemas failed via FFI");
            error_json(&e)
        }
        None => error_json("Operation timed out"),
    }
}

#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_list_postgres_tables(
    conn_id: u64,
    db: *const c_char,
    schema: *const c_char,
) -> *mut c_char {
    if db.is_null() || schema.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={}", conn_id)),
    };

    let db_for_log = db_str.clone();
    let schema_for_log = schema_str.clone();
    let result = run_with_timeout(async move { conn.list_tables(&db_str, &schema_str).await });

    match result {
        Some(Ok(tables)) => {
            info!(db = %db_for_log, schema = %schema_for_log, count = tables.len(), "Listed Postgres tables via FFI");
            success_json(&tables)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, schema = %schema_for_log, error = %e, "List Postgres tables failed via FFI");
            error_json(&e)
        }
        None => error_json("Operation timed out"),
    }
}

#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_list_sql_objects(
    conn_id: u64,
    db: *const c_char,
    schema: *const c_char,
) -> *mut c_char {
    if db.is_null() || schema.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No SQL connection for conn_id={}", conn_id)),
    };

    let db_for_log = db_str.clone();
    let schema_for_log = schema_str.clone();
    let result = run_with_timeout(async move { conn.list_sql_objects(&db_str, &schema_str).await });

    match result {
        Some(Ok(objects)) => {
            info!(db = %db_for_log, schema = %schema_for_log, count = objects.len(), "Listed SQL objects via FFI");
            success_json(&objects)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, schema = %schema_for_log, error = %e, "List SQL objects failed via FFI");
            error_json(&e)
        }
        None => error_json("Operation timed out"),
    }
}

/// Execute a query against a collection.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, and `query_json` must be valid, null-terminated C strings.
///
/// Returns a JSON array of documents as a C string.
/// Caller must free with `dbengine_free_string`.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_execute_query(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    query_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || query_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let query_str = unsafe { CStr::from_ptr(query_json).to_string_lossy().into_owned() };

    let db_for_log = db_str.clone();
    let coll_for_log = coll_str.clone();

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;
        use futures::stream::StreamExt;
        use serde_json::json;

        let filter: Document = serde_json::from_str(&query_str)
            .map_err(|e| error::DbEngineError::Query(format!("invalid query JSON: {}", e)))?;

        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let cursor = coll
            .find(filter)
            .limit(200)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        let docs: Vec<Document> = cursor
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(|r| r.ok())
            .collect();

        let json_docs: Vec<serde_json::Value> = docs
            .iter()
            .map(|doc| {
                bson::to_document(doc)
                    .ok()
                    .and_then(|b| serde_json::to_value(b).ok())
                    .unwrap_or(json!({}))
            })
            .collect();

        serde_json::to_string(&json_docs)
            .map_err(|e| error::DbEngineError::Serialization(e.to_string()))
    });

    match result {
        Some(Ok(json)) => {
            info!(db = %db_for_log, collection = %coll_for_log, "Query executed via FFI");
            match CString::new(json) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => error_json("Failed to create C string from JSON"),
            }
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, collection = %coll_for_log, error = %e, "Query failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Execute a query against a collection, returning Arrow C Data Interface format.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, and `query_json` must be valid, null-terminated C strings.
/// `skip` and `limit` control pagination (default: skip=0, limit=1000).
///
/// Returns a JSON string containing Arrow C Data Interface pointers:
/// `{"schema_ptr": <u64>, "array_ptr": <u64>, "row_count": <usize>, "col_count": <usize>, "column_names": [...], "column_types": [...], "raw_documents": [...], "total_count": <usize>, "last_id": "<BSON>"}`
/// Caller must free the metadata string with `dbengine_free_string`
/// AND free the Arrow C Data structs with `dbengine_free_arrow_cdata(schema_ptr, array_ptr)`.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_execute_query_arrow(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    query_json: *const c_char,
    cursor_json: *const c_char,
    limit: i64,
    project_json: *const c_char,
    sort_json: *const c_char,
    collation_json: *const c_char,
    index_hint_json: *const c_char,
    skip: i64,
    max_time_ms: i64,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || query_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let query_str = unsafe { CStr::from_ptr(query_json).to_string_lossy().into_owned() };
    let cursor_str = if cursor_json.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(cursor_json).to_string_lossy().into_owned() };
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };

    let project: Option<bson::Document> = if project_json.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(project_json).to_string_lossy().into_owned() };
        if s.is_empty() || s == "{}" {
            None
        } else {
            json5::from_str(&s).ok()
        }
    };

    let sort: Option<bson::Document> = if sort_json.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(sort_json).to_string_lossy().into_owned() };
        if s.is_empty() || s == "{}" {
            None
        } else {
            json5::from_str(&s).ok()
        }
    };

    let collation_locale: Option<String> = if collation_json.is_null() {
        None
    } else {
        let s = unsafe {
            CStr::from_ptr(collation_json)
                .to_string_lossy()
                .into_owned()
        };
        if s.is_empty() || s == "{}" {
            None
        } else {
            serde_json::from_str::<bson::Document>(&s)
                .ok()
                .and_then(|doc| doc.get_str("locale").ok().map(|l| l.to_string()))
        }
    };

    let index_hint: Option<mongodb::options::Hint> = if index_hint_json.is_null() {
        None
    } else {
        let s = unsafe {
            CStr::from_ptr(index_hint_json)
                .to_string_lossy()
                .into_owned()
        };
        if s.is_empty() || s == "{}" {
            None
        } else if let Ok(doc) = serde_json::from_str::<bson::Document>(&s) {
            Some(mongodb::options::Hint::Keys(doc))
        } else if let Ok(name) = serde_json::from_str::<String>(&s) {
            Some(mongodb::options::Hint::Name(name))
        } else {
            None
        }
    };

    let effective_skip = if skip > 0 { Some(skip as u64) } else { None };
    let effective_max_time = if max_time_ms > 0 {
        Some(std::time::Duration::from_millis(max_time_ms as u64))
    } else {
        None
    };

    let db_for_log = db_str.clone();
    let coll_for_log = coll_str.clone();

    let effective_limit = if limit > 0 { limit } else { 200 };

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;
        use futures::stream::StreamExt;
        use mongodb::options::FindOptions;

        let filter: Document =
            query_parser::parse_query(&query_str).map_err(|e| error::DbEngineError::Query(e))?;

        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let query_filter = if let Some(cursor_value_json) = &cursor_str {
            if query_perf_logs_enabled() {
                println!(
                    "[Rust] Cursor received from Swift: cursor_raw={}",
                    cursor_value_json
                );
            }
            let cursor_value: bson::Bson = serde_json::from_str(cursor_value_json)
                .map_err(|e| error::DbEngineError::Query(format!("invalid cursor JSON: {}", e)))?;
            let cursor_type_name = match &cursor_value {
                bson::Bson::Double(_) => "Double",
                bson::Bson::String(_) => "String",
                bson::Bson::Boolean(_) => "Boolean",
                bson::Bson::Int32(_) => "Int32",
                bson::Bson::Int64(_) => "Int64",
                bson::Bson::ObjectId(_) => "ObjectId",
                _ => "Other",
            };
            if query_perf_logs_enabled() {
                println!(
                    "[Rust] Cursor deserialized: cursor_type={}, cursor_value={:?}",
                    cursor_type_name, cursor_value
                );
            }
            let mut combined = filter.clone();
            combined.insert("_id", bson::doc! { "$gt": cursor_value });
            combined
        } else {
            filter.clone()
        };

        if query_perf_logs_enabled() {
            println!(
                "[Rust] Final query filter sent to MongoDB: query_filter={:?}",
                query_filter
            );
        }

        let mut opts = FindOptions::builder().limit(effective_limit).build();

        // Default sort by _id for cursor pagination consistency
        let effective_sort = if sort.is_none() {
            Some(bson::doc! { "_id": 1 })
        } else {
            sort.clone()
        };

        if query_perf_logs_enabled() {
            println!("[Rust] Effective sort: {:?}", effective_sort);
        }

        if let Some(proj) = project {
            opts.projection = Some(proj);
        }
        if let Some(s) = effective_sort {
            opts.sort = Some(s);
        }
        if let Some(sk) = effective_skip {
            opts.skip = Some(sk);
        }
        if let Some(mt) = effective_max_time {
            opts.max_time = Some(mt);
        }
        if let Some(locale) = collation_locale {
            opts.collation = Some(
                mongodb::options::Collation::builder()
                    .locale(locale)
                    .build(),
            );
        }
        if let Some(hint) = index_hint {
            opts.hint = Some(hint);
        }

        let cache_key = EngineState::cache_key(&db_str, &coll_str, &query_str);
        if query_perf_logs_enabled() {
            println!("[Rust] Cache key: {}", cache_key);
        }
        let cached_count = {
            match ENGINE.read() {
                Ok(engine) => {
                    if let Some(entry) = engine.count_cache.get(&cache_key) {
                        if query_perf_logs_enabled() {
                            println!("[Rust] Cache hit: cached_count={}", entry.count);
                        }
                        if entry.filter == filter {
                            Some(entry.count)
                        } else {
                            if query_perf_logs_enabled() {
                                println!("[Rust] Cache filter mismatch, invalidating");
                            }
                            None
                        }
                    } else {
                        if query_perf_logs_enabled() {
                            println!("[Rust] Cache miss");
                        }
                        None
                    }
                }
                Err(_) => None,
            }
        };

        let (docs_result, count_result) = tokio::join!(
            async {
                if query_perf_logs_enabled() {
                    println!("[Rust] Executing find query with limit={}", effective_limit);
                }
                let t_fetch = std::time::Instant::now();
                let cursor = coll
                    .find(query_filter.clone())
                    .with_options(opts)
                    .await
                    .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

                let docs: Vec<Document> = cursor
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .collect();

                let fetch_us = t_fetch.elapsed().as_micros();
                info!(
                    docs_returned = docs.len(),
                    "Documents returned from MongoDB"
                );
                if query_perf_logs_enabled() {
                    println!(
                        "[rust-perf] MongoDB fetch: {} docs, {} us ({} ms)",
                        docs.len(),
                        fetch_us,
                        fetch_us / 1000
                    );
                }

                Ok::<_, error::DbEngineError>(docs)
            },
            async {
                if let Some(count) = cached_count {
                    if query_perf_logs_enabled() {
                        println!("[Rust] Using cached count: {}", count);
                    }
                    Ok::<_, error::DbEngineError>((count, false))
                } else {
                    // Try estimatedDocumentCount first (O(1) from metadata)
                    if query_perf_logs_enabled() {
                        println!("[Rust] Trying estimatedDocumentCount (O(1))");
                    }
                    let estimated = coll.estimated_document_count().await;
                    match estimated {
                        Ok(count) => {
                            let count_usize = count as usize;
                            if query_perf_logs_enabled() {
                                println!("[Rust] estimatedDocumentCount: {}", count_usize);
                            }
                            if let Ok(mut engine) = ENGINE.write() {
                                engine.insert_count_cache(
                                    cache_key,
                                    CountCacheEntry {
                                        filter: filter.clone(),
                                        count: count_usize,
                                    },
                                );
                            }
                            Ok((count_usize, true))
                        }
                        Err(e) => {
                            eprintln!("[Rust] estimatedDocumentCount failed: {}, falling back to count_documents", e);
                            let count = coll
                                .count_documents(filter.clone())
                                .await
                                .map_err(|e| error::DbEngineError::Query(e.to_string()))?;
                            let count_usize = count as usize;
                            if query_perf_logs_enabled() {
                                println!("[Rust] count_documents result: {}", count_usize);
                            }
                            if let Ok(mut engine) = ENGINE.write() {
                                engine.insert_count_cache(
                                    cache_key,
                                    CountCacheEntry {
                                        filter: filter.clone(),
                                        count: count_usize,
                                    },
                                );
                            }
                            Ok((count_usize, false))
                        }
                    }
                }
            }
        );

        let docs = docs_result?;
        let (total_count, is_estimated) = match count_result {
            Ok((count, estimated)) => (count, estimated),
            Err(e) => {
                eprintln!("[Rust] Count query failed: {}", e);
                (0, false)
            }
        };
        if query_perf_logs_enabled() {
            println!(
                "[Rust] Count result: total_count={}, estimated={}",
                total_count, is_estimated
            );
        }

        let last_id = docs.last().and_then(|d| {
            d.get("_id")
                .map(|v| serde_json::to_string(&v).unwrap_or_default())
        });

        let t_arrow = std::time::Instant::now();
        let mut response = arrow::documents_to_arrow_cdata(&docs)
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;
        let arrow_us = t_arrow.elapsed().as_micros();
        if query_perf_logs_enabled() {
            println!(
                "[rust-perf] Arrow conversion: {} us ({} ms)",
                arrow_us,
                arrow_us / 1000
            );
        }

        // Use the exact column order produced by `documents_to_arrow_cdata`
        // so the cached session's column order matches what Swift sees in
        // `ArrowTable`. This matters for the backend cell-batch renderer.
        let column_names: Vec<String> = response.column_names.clone();

        let t_cache = std::time::Instant::now();
        let session_id = {
            let mut engine = ENGINE.write().unwrap();
            engine.cache_documents(docs, column_names)
        };
        let cache_us = t_cache.elapsed().as_micros();
        if query_perf_logs_enabled() {
            println!(
                "[rust-perf] cache_documents: {} us ({} ms)",
                cache_us,
                cache_us / 1000
            );
        }

        response.total_count = Some(total_count);
        response.last_id = last_id;
        response.is_estimated_count = is_estimated;
        response.session_id = session_id;
        Ok::<_, error::DbEngineError>(response)
    });

    match result {
        Some(Ok(arrow_result)) => {
            info!(db = %db_for_log, collection = %coll_for_log, rows = arrow_result.row_count, limit = effective_limit, total = ?arrow_result.total_count, "Arrow query executed via FFI");
            let response = arrow::ArrowCDataResponse::from(arrow_result);
            success_json(&response)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, collection = %coll_for_log, error = %e, "Arrow query failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Execute a PostgreSQL query and return Arrow C Data.
///
/// `table` is the table name to query.
/// `user_where` is a raw WHERE clause (e.g. `"status = 'active'"`), can be null/empty.
/// `user_order_by` is a raw ORDER BY clause (e.g. `"created_at DESC"`), can be null/empty.
/// `cursor_json` is a JSON-encoded `CursorState` for keyset pagination, or null.
/// `limit` is the page size (defaults to 200 if <= 0).
///
/// Returns an `ArrowCDataResponse` JSON envelope. On error returns a JSON `{"error": "..."}` object.
/// Caller must free the metadata string with `dbengine_free_string`
/// AND free the Arrow C Data structs with `dbengine_free_arrow_cdata(schema_ptr, array_ptr)`.
/// If `cursor` is present in the response, free it with `dbengine_free_cursor_string(cursor_ptr)`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_execute_postgres_query_arrow(
    conn_id: u64,
    db: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    user_where: *const c_char,
    user_order_by: *const c_char,
    cursor_json: *const c_char,
    limit: i64,
) -> *mut c_char {
    if db.is_null() || schema.is_null() || table.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };
    let table_str = unsafe { CStr::from_ptr(table).to_string_lossy().into_owned() };
    let user_where_str = if user_where.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(user_where).to_string_lossy().into_owned() };
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    let user_order_by_str = if user_order_by.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(user_order_by).to_string_lossy().into_owned() };
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    let cursor_state: Option<crate::db::CursorState> = if cursor_json.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(cursor_json).to_string_lossy().into_owned() };
        if s.is_empty() {
            None
        } else {
            serde_json::from_str(&s).ok()
        }
    };

    let effective_limit = if limit > 0 { limit } else { 200 };

    let db_for_log = db_str.clone();
    let schema_for_log = schema_str.clone();
    let table_for_log = table_str.clone();

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={}", conn_id)),
    };

    let result = run_with_timeout(async move {
        let query_result = conn
            .execute_query_in_schema(
                &db_str,
                &schema_str,
                &table_str,
                user_where_str.as_deref(),
                user_order_by_str.as_deref(),
                cursor_state.as_ref(),
                effective_limit,
            )
            .await
            .map_err(|e| error::DbEngineError::Query(e))?;

        let cursor_json_out = query_result
            .cursor
            .map(|c| serde_json::to_string(&c).unwrap_or_default());

        let arrow_result = crate::arrow::record_batch_to_arrow_cdata(
            query_result.batches,
            Some(query_result.total_count as usize),
            cursor_json_out,
        )
        .map_err(|e| error::DbEngineError::Serialization(e.to_string()))?;

        Ok::<_, error::DbEngineError>(arrow_result)
    });

    match result {
        Some(Ok(arrow_result)) => {
            let response = arrow::ArrowCDataResponse::from(arrow_result);
            success_json(&response)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, schema = %schema_for_log, table = %table_for_log, error = %e, "PostgreSQL query failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Execute a read-only PostgreSQL SQL query and return Arrow C Data.
///
/// `sql` must start with SELECT or WITH. `limit` is enforced by wrapping the query.
///
/// Returns an `ArrowCDataResponse` JSON envelope. On error returns a JSON `{"error": "..."}` object.
/// Caller must free the metadata string with `dbengine_free_string`
/// AND free the Arrow C Data structs with `dbengine_free_arrow_cdata(schema_ptr, array_ptr)`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_execute_postgres_sql_arrow(
    conn_id: u64,
    db: *const c_char,
    sql: *const c_char,
    limit: i64,
) -> *mut c_char {
    if db.is_null() || sql.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let sql_str = unsafe { CStr::from_ptr(sql).to_string_lossy().into_owned() };
    let effective_limit = if limit > 0 { limit } else { 200 };

    let db_for_log = db_str.clone();

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={}", conn_id)),
    };

    let result = run_with_timeout(async move {
        let query_result = conn
            .execute_sql(&db_str, &sql_str, effective_limit)
            .await
            .map_err(|e| error::DbEngineError::Query(e))?;

        let arrow_result = crate::arrow::record_batch_to_arrow_cdata(
            query_result.batches,
            Some(query_result.total_count as usize),
            None,
        )
        .map_err(|e| error::DbEngineError::Serialization(e.to_string()))?;

        Ok::<_, error::DbEngineError>(arrow_result)
    });

    match result {
        Some(Ok(arrow_result)) => {
            let response = arrow::ArrowCDataResponse::from(arrow_result);
            success_json(&response)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, error = %e, "PostgreSQL raw SQL query failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_sqlite_catalog(conn_id: u64) -> *mut c_char {
    let conn = match ENGINE.read() {
        Ok(engine) => engine.sqlite_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No SQLite connection for conn_id={conn_id}")),
    };

    match run_with_timeout(async move { conn.catalog().await }) {
        Some(Ok(catalog)) => success_json(&catalog),
        Some(Err(e)) => error_json(&e),
        None => error_json("Operation timed out"),
    }
}

#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_execute_sqlite_sql_arrow(
    conn_id: u64,
    sql: *const c_char,
    limit: i64,
) -> *mut c_char {
    if sql.is_null() {
        return error_json("null parameter");
    }
    let sql_str = unsafe { CStr::from_ptr(sql).to_string_lossy().into_owned() };
    let effective_limit = if limit > 0 { limit } else { 200 };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.sqlite_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No SQLite connection for conn_id={conn_id}")),
    };

    let result = run_with_timeout(async move {
        let batch = conn
            .execute_sql(&sql_str, effective_limit)
            .await
            .map_err(error::DbEngineError::Query)?;
        let row_count = batch.num_rows();
        crate::arrow::record_batch_to_arrow_cdata(vec![batch], Some(row_count), None)
            .map_err(|e| error::DbEngineError::Serialization(e.to_string()))
    });

    match result {
        Some(Ok(arrow_result)) => {
            let response = arrow::ArrowCDataResponse::from(arrow_result);
            success_json(&response)
        }
        Some(Err(e)) => error_json(&e.to_string()),
        None => error_json("Operation timed out"),
    }
}

/// Fetch the full schema/table/column catalog for a PostgreSQL database in one round-trip.
///
/// # Safety
/// `conn_id` must be a valid connection ID; `db` must be a valid null-terminated C string.
///
/// Returns a JSON string with shape `{"schemas":[...]}`. Caller must free with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_postgres_catalog(conn_id: u64, db: *const c_char) -> *mut c_char {
    if db.is_null() {
        return error_json("null db parameter");
    }
    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };
    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={conn_id}")),
    };

    let db_for_log = db_str.clone();
    let result = run_with_timeout(async move { conn.list_catalog(&db_str).await });

    match result {
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => error_json("Failed to create C string"),
        },
        Some(Err(e)) => {
            error!(db = %db_for_log, error = %e, "Postgres catalog failed via FFI");
            error_json(&e)
        }
        None => error_json("Operation timed out"),
    }
}

/// Fetch table metadata for a PostgreSQL table in one FFI call.
///
/// # Safety
/// `conn_id` must be valid; `db`, `schema`, and `table` must be valid null-terminated C strings.
///
/// Returns JSON with `columns`, `constraints`, `foreign_keys`, `indexes`, and `triggers` arrays.
/// Caller must free with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_postgres_table_metadata(
    conn_id: u64,
    db: *const c_char,
    schema: *const c_char,
    table: *const c_char,
) -> *mut c_char {
    if db.is_null() || schema.is_null() || table.is_null() {
        return error_json("null parameter");
    }
    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };
    let table_str = unsafe { CStr::from_ptr(table).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };
    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={conn_id}")),
    };

    let db_for_log = db_str.clone();
    let schema_for_log = schema_str.clone();
    let table_for_log = table_str.clone();
    let result =
        run_with_timeout(
            async move { conn.table_metadata(&db_str, &schema_str, &table_str).await },
        );

    match result {
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => error_json("Failed to create C string"),
        },
        Some(Err(e)) => {
            error!(db = %db_for_log, schema = %schema_for_log, table = %table_for_log, error = %e, "Postgres table metadata failed via FFI");
            error_json(&e)
        }
        None => error_json("Operation timed out"),
    }
}

/// Fetch a PostgreSQL view/materialized-view definition.
///
/// # Safety
/// `conn_id` must be valid; `db`, `schema`, and `name` must be valid null-terminated C strings.
///
/// Returns JSON with `relkind` and `definition`. Caller must free with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_postgres_view_definition(
    conn_id: u64,
    db: *const c_char,
    schema: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    if db.is_null() || schema.is_null() || name.is_null() {
        return error_json("null parameter");
    }
    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };
    let name_str = unsafe { CStr::from_ptr(name).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };
    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={conn_id}")),
    };

    let result =
        run_with_timeout(
            async move { conn.view_definition(&db_str, &schema_str, &name_str).await },
        );

    match result {
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => error_json("Failed to create C string"),
        },
        Some(Err(e)) => error_json(&e),
        None => error_json("Operation timed out"),
    }
}

/// Preview the generated SQL for a PostgreSQL query without executing it.
///
/// # Safety
/// `db`, `table`, `user_where`, and `user_order_by` must be valid, null-terminated C strings or null.
///
/// Returns a JSON string. On success: `{"sql": "SELECT * FROM ..."}`.
/// On error: `{"error": "..."}`.
/// The caller must free the returned pointer with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub unsafe extern "C" fn dbengine_preview_postgres_sql(
    db: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    user_where: *const c_char,
    user_order_by: *const c_char,
    limit: i64,
) -> *mut c_char {
    if db.is_null() || schema.is_null() || table.is_null() {
        return error_json("null parameter");
    }

    let _db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };
    let table_str = unsafe { CStr::from_ptr(table).to_string_lossy().into_owned() };
    let user_where_str = if user_where.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(user_where).to_string_lossy().into_owned() };
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    let user_order_by_str = if user_order_by.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(user_order_by).to_string_lossy().into_owned() };
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };

    let effective_limit = if limit > 0 { limit } else { 200 };

    match db::sql_builder::build_sql_preview_qualified(
        &schema_str,
        &table_str,
        user_where_str.as_deref(),
        user_order_by_str.as_deref(),
        effective_limit,
    ) {
        Ok(sql) => success_json(&serde_json::json!({"sql": sql})),
        Err(e) => error_json(&e),
    }
}

/// Check whether a PostgreSQL raw SQL statement is safe to edit as one table.
///
/// # Safety
/// `sql` must be a valid, null-terminated C string.
///
/// Returns a JSON string. On success: `{"editable": true}`.
/// On error: `{"error": "..."}`.
/// The caller must free the returned pointer with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub unsafe extern "C" fn dbengine_postgres_sql_editability(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        return error_json("null parameter");
    }

    let sql_str = unsafe { CStr::from_ptr(sql).to_string_lossy().into_owned() };
    match db::sql_builder::editable_single_table_select_target(&sql_str) {
        Ok(Some(target)) => success_json(&serde_json::json!({
            "editable": true,
            "schema": target.schema,
            "table": target.table,
        })),
        Ok(None) => success_json(&serde_json::json!({"editable": false})),
        Err(e) => error_json(&e),
    }
}

/// Insert one row into a PostgreSQL table from a JSON object.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_insert_postgres_row(
    conn_id: u64,
    db: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    row_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || schema.is_null() || table.is_null() || row_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };
    let table_str = unsafe { CStr::from_ptr(table).to_string_lossy().into_owned() };
    let row_json_str = unsafe { CStr::from_ptr(row_json).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };
    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={}", conn_id)),
    };

    let db_for_log = db_str.clone();
    let result = run_with_timeout(async move {
        let inserted = conn
            .insert_row_from_json(&db_str, &schema_str, &table_str, &row_json_str)
            .await
            .map_err(error::DbEngineError::Query)?;
        Ok::<_, error::DbEngineError>(serde_json::json!({"inserted": inserted}))
    });

    match result {
        Some(Ok(response)) => success_json(&response),
        Some(Err(e)) => {
            error!(db = %db_for_log, error = %e, "PostgreSQL insert failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Apply batch updates to a PostgreSQL table using a text-comparable key column.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_batch_update_postgres_rows(
    conn_id: u64,
    db: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    key_column: *const c_char,
    updates_json: *const c_char,
) -> *mut c_char {
    if db.is_null()
        || schema.is_null()
        || table.is_null()
        || key_column.is_null()
        || updates_json.is_null()
    {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let schema_str = unsafe { CStr::from_ptr(schema).to_string_lossy().into_owned() };
    let table_str = unsafe { CStr::from_ptr(table).to_string_lossy().into_owned() };
    let key_column_str = unsafe { CStr::from_ptr(key_column).to_string_lossy().into_owned() };
    let updates_json_str = unsafe { CStr::from_ptr(updates_json).to_string_lossy().into_owned() };

    let conn = match ENGINE.read() {
        Ok(engine) => engine.postgres_connections.get(&conn_id).cloned(),
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };
    let conn = match conn {
        Some(c) => c,
        None => return error_json(&format!("No PostgreSQL connection for conn_id={}", conn_id)),
    };

    let db_for_log = db_str.clone();
    let result = run_with_timeout(async move {
        let updated = conn
            .batch_update_from_json(
                &db_str,
                &schema_str,
                &table_str,
                &key_column_str,
                &updates_json_str,
            )
            .await
            .map_err(error::DbEngineError::Query)?;
        Ok::<_, error::DbEngineError>(serde_json::json!({"updated": updated}))
    });

    match result {
        Some(Ok(response)) => success_json(&response),
        Some(Err(e)) => {
            error!(db = %db_for_log, error = %e, "PostgreSQL batch update failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Execute a MongoDB aggregation pipeline and return Arrow C Data.
///
/// `pipeline_json` is a JSON array of stage documents (e.g. `[{"$match": {...}}, {"$limit": 10}]`).
/// If no `$limit` stage is present in the pipeline, `limit` is appended automatically.
/// `max_time_ms` of 0 disables the server-side timeout.
///
/// Returns an `ArrowCDataResponse` JSON envelope. On error returns a JSON `{"error": "..."}` object.
/// Caller must free the metadata string with `dbengine_free_string`
/// AND free the Arrow C Data structs with `dbengine_free_arrow_cdata(schema_ptr, array_ptr)`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_execute_aggregate_arrow(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    pipeline_json: *const c_char,
    limit: i64,
    max_time_ms: i64,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || pipeline_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let pipeline_str = unsafe { CStr::from_ptr(pipeline_json).to_string_lossy().into_owned() };

    let mut pipeline: Vec<bson::Document> = match json5::from_str(&pipeline_str) {
        Ok(p) => p,
        Err(e) => return error_json(&format!("Invalid pipeline JSON: {}", e)),
    };

    // Append preview $limit if user didn't specify one
    let has_limit = pipeline.iter().any(|s| s.keys().any(|k| k == "$limit"));
    let effective_limit = if limit > 0 { limit } else { 200 };
    if !has_limit {
        pipeline.push(bson::doc! { "$limit": effective_limit });
    }

    let effective_max_time = if max_time_ms > 0 {
        Some(std::time::Duration::from_millis(max_time_ms as u64))
    } else {
        None
    };

    let db_for_log = db_str.clone();
    let coll_for_log = coll_str.clone();

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;
        use futures::stream::StreamExt;
        use mongodb::options::AggregateOptions;

        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let opts = if let Some(mt) = effective_max_time {
            AggregateOptions::builder().max_time(mt).build()
        } else {
            AggregateOptions::builder().build()
        };

        let cursor = coll
            .aggregate(pipeline)
            .with_options(opts)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        let docs: Vec<Document> = cursor
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(|r| r.ok())
            .collect();

        info!(
            docs_returned = docs.len(),
            "Aggregate returned from MongoDB"
        );
        if query_perf_logs_enabled() {
            println!(
                "[Rust] Aggregate returned from MongoDB: docs_returned={}",
                docs.len()
            );
        }

        let mut response = arrow::documents_to_arrow_cdata(&docs)
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        // Use the exact column order produced by `documents_to_arrow_cdata`
        // so the cached session's column order matches what Swift sees in
        // `ArrowTable`. This matters for the backend cell-batch renderer.
        let column_names: Vec<String> = response.column_names.clone();

        let session_id = {
            let mut engine = ENGINE.write().unwrap();
            engine.cache_documents(docs, column_names)
        };

        response.total_count = Some(response.row_count as usize);
        response.is_estimated_count = false;
        response.last_id = None;
        response.session_id = session_id;
        Ok::<_, error::DbEngineError>(response)
    });

    match result {
        Some(Ok(arrow_result)) => {
            info!(db = %db_for_log, collection = %coll_for_log, rows = arrow_result.row_count, "Arrow aggregate executed via FFI");
            let response = arrow::ArrowCDataResponse::from(arrow_result);
            success_json(&response)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, collection = %coll_for_log, error = %e, "Arrow aggregate failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Execute a MongoDB shell script (e.g. db.collection.find().sort().limit())
/// and return Arrow C Data.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db` and `script` must be valid, null-terminated C strings.
///
/// Returns a JSON string with Arrow C Data pointers (same format as `dbengine_execute_query_arrow`).
/// Caller must free with `dbengine_free_string` AND `dbengine_free_arrow_cdata`.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_cancel_operation(conn_id: u64) -> bool {
    if let Ok(engine) = ENGINE.read() {
        engine.cancel_flag.store(true, Ordering::Relaxed);
        true
    } else {
        false
    }
}

#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_execute_mongo_script(
    conn_id: u64,
    db: *const c_char,
    script: *const c_char,
    max_time_ms: i64,
) -> *mut c_char {
    if db.is_null() || script.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let script_str = unsafe { CStr::from_ptr(script).to_string_lossy().into_owned() };

    let plan = match mongo_script_parser::parse_script(&script_str) {
        Ok(ScriptPlan::Level1(l1)) => l1.to_pipeline(&db_str),
        Ok(ScriptPlan::Level2(pipeline)) => pipeline,
        Ok(ScriptPlan::Level3Define(def)) => {
            // Register the pipeline definition and return success
            let name = def.name.clone();
            if let Ok(mut engine) = ENGINE.write() {
                engine.pipeline_registry.insert(name.clone(), def);
                let json = format!(
                    "{{\"ok\":1,\"n\":0,\"message\":\"Pipeline '{}' defined\"}}",
                    name
                );
                return match CString::new(json) {
                    Ok(cstr) => cstr.into_raw(),
                    Err(_) => error_json("Failed to create response"),
                };
            } else {
                return error_json("Engine lock poisoned");
            }
        }
        Ok(ScriptPlan::Level3Run { name, params }) => {
            let def = {
                let engine = ENGINE.read().map_err(|e| e.to_string()).unwrap();
                engine.pipeline_registry.get(&name).cloned()
            };
            let def = match def {
                Some(d) => d,
                None => return error_json(&format!("Pipeline '{}' not defined", name)),
            };
            // Apply parameter substitution: merge defaults with provided params
            let mut resolved_stages = def.stages.clone();
            for stage in resolved_stages.iter_mut() {
                pipeline::apply_param_substitution(stage, &def.default_params, &params);
            }
            pipeline::PipelinePlan {
                stages: resolved_stages,
            }
        }
        Err(e) => return error_json(&format!("Parse error: {}", e)),
    };

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let db_for_log = db_str.clone();

    let result = run_with_timeout(async move {
        pipeline::execute_pipeline(&client, &db_str, plan, max_time_ms).await
    });

    match result {
        Some(Ok(arrow_result)) => {
            info!(db = %db_for_log, rows = arrow_result.row_count, "Mongo script executed via FFI");
            let response = arrow::ArrowCDataResponse::from(arrow_result);
            success_json(&response)
        }
        Some(Err(e)) => {
            error!(db = %db_for_log, error = %e, "Mongo script failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Load documents from a BSON dump file and return Arrow C Data.
///
/// # Safety
/// `path` must be a valid, null-terminated C string pointing to a BSON dump file.
///
/// Returns a JSON string with Arrow C Data pointers:
/// `{"schema_ptr": <u64>, "array_ptr": <u64>, "row_count": <usize>, "col_count": <usize>, "column_names": [...], "column_types": [...]}`
/// Caller must free with `dbengine_free_string` AND `dbengine_free_arrow_cdata`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_load_fixture_arrow(path: *const c_char, count: i64) -> *mut c_char {
    if path.is_null() {
        return error_json("null path");
    }

    let path_str = unsafe { CStr::from_ptr(path).to_string_lossy().into_owned() };
    let count = count as usize;

    // Load BSON documents from file
    let docs = match arrow::load_documents_from_bson(&path_str, count) {
        Ok(docs) => docs,
        Err(e) => return error_json(&e.to_string()),
    };

    if docs.is_empty() {
        return error_json("No documents loaded from fixture");
    }

    // Convert to Arrow C Data
    match arrow::documents_to_arrow_cdata(&docs) {
        Ok(result) => {
            let response = arrow::ArrowCDataResponse::from(result);
            success_json(&response)
        }
        Err(e) => error_json(&e.to_string()),
    }
}

/// Explain a query and return the execution plan.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, `query_json` must be valid, null-terminated C strings.
/// Optional params (`project_json`, `sort_json`) may be null or empty.
///
/// Returns the explain plan as a JSON string.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_explain_query(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    query_json: *const c_char,
    project_json: *const c_char,
    sort_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || query_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let query_str = unsafe { CStr::from_ptr(query_json).to_string_lossy().into_owned() };

    let project: Option<bson::Document> = if project_json.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(project_json).to_string_lossy().into_owned() };
        if s.is_empty() || s == "{}" {
            None
        } else {
            json5::from_str(&s).ok()
        }
    };

    let sort: Option<bson::Document> = if sort_json.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(sort_json).to_string_lossy().into_owned() };
        if s.is_empty() || s == "{}" {
            None
        } else {
            json5::from_str(&s).ok()
        }
    };

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::{doc, Document};

        let filter: Document =
            query_parser::parse_query(&query_str).map_err(|e| error::DbEngineError::Query(e))?;

        let db = client.database(&db_str);

        let mut find_cmd = doc! {
            "find": &coll_str,
            "filter": filter,
        };

        if let Some(proj) = project {
            find_cmd.insert("projection", proj);
        }
        if let Some(s) = sort {
            find_cmd.insert("sort", s);
        }

        let explain_cmd = doc! {
            "explain": find_cmd,
            "verbosity": "queryPlanner"
        };

        let explain_result = db
            .run_command(explain_cmd)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        let explain_json = serde_json::to_string(&explain_result)
            .map_err(|e| error::DbEngineError::Query(format!("explain serialization: {}", e)))?;

        Ok::<_, error::DbEngineError>(explain_json)
    });

    match result {
        Some(Ok(plan)) => {
            let c_str = std::ffi::CString::new(plan).unwrap_or_default();
            c_str.into_raw()
        }
        Some(Err(e)) => error_json(&e.to_string()),
        None => error_json("Operation timed out"),
    }
}

/// Explain an aggregate pipeline.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, and `pipeline_json` must be valid, null-terminated C strings.
///
/// Returns the explain plan as a JSON string.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_explain_aggregate(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    pipeline_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || pipeline_json.is_null() {
        return error_json("null parameter");
    }
    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let pipeline_str = unsafe { CStr::from_ptr(pipeline_json).to_string_lossy().into_owned() };

    let pipeline: Vec<bson::Document> = match json5::from_str(&pipeline_str) {
        Ok(p) => p,
        Err(e) => return error_json(&format!("Invalid pipeline JSON: {}", e)),
    };

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::doc;
        let database = client.database(&db_str);
        let explain_cmd = doc! {
            "explain": {
                "aggregate": coll_str.clone(),
                "pipeline": pipeline.clone(),
                "cursor": {}
            },
            "verbosity": "queryPlanner"
        };
        let resp = database
            .run_command(explain_cmd)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;
        let json = serde_json::to_string(&resp)
            .map_err(|e| error::DbEngineError::Query(format!("explain serialization: {}", e)))?;
        Ok::<_, error::DbEngineError>(json)
    });

    match result {
        Some(Ok(plan)) => {
            let c_str = std::ffi::CString::new(plan).unwrap_or_default();
            c_str.into_raw()
        }
        Some(Err(e)) => error_json(&e.to_string()),
        None => error_json("Operation timed out"),
    }
}

/// Validate a raw aggregate pipeline string without executing it.
///
/// Parses the input with the same Mongo shell / JSON5 dialect used by
/// `dbengine_execute_aggregate_arrow`, so the editor can use the result
/// as a single source of truth. Accepts strict JSON, unquoted keys, trailing
/// commas, and `//` / `/* */` comments. Rejects pipelines that are not a
/// non-empty JSON array of single-key stage objects starting with `$`.
///
/// # Safety
/// `pipeline_json` must be a valid, null-terminated C string.
///
/// Returns a JSON object:
/// - `{"valid": true}` on success (caller must free with `dbengine_free_string`).
/// - `{"valid": false, "error": "..."}` on failure (caller must free with
///   `dbengine_free_string`).
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_validate_aggregate_pipeline(
    pipeline_json: *const c_char,
) -> *mut c_char {
    if pipeline_json.is_null() {
        return alloc_json("{\"valid\":false,\"error\":\"null parameter\"}");
    }
    let text = unsafe { CStr::from_ptr(pipeline_json).to_string_lossy().into_owned() };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return alloc_json("{\"valid\":false,\"error\":\"Pipeline is empty. Start with [ and add stage objects.\"}");
    }
    let stages: Vec<bson::Document> = match json5::from_str(trimmed) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("Invalid JSON: {}", e);
            return alloc_json(&validate_error_envelope(&msg));
        }
    };
    for (i, stage) in stages.iter().enumerate() {
        if stage.is_empty() {
            let msg = format!("Stage {} is empty.", i + 1);
            return alloc_json(&validate_error_envelope(&msg));
        }
        if stage.len() > 1 {
            let msg = format!(
                "Stage {} has {} keys; each stage must be a single-key object like {{ \"$match\": ... }}.",
                i + 1,
                stage.len()
            );
            return alloc_json(&validate_error_envelope(&msg));
        }
        for key in stage.keys() {
            if key.is_empty() {
                let msg = format!("Stage {} has an empty key.", i + 1);
                return alloc_json(&validate_error_envelope(&msg));
            }
            if !key.starts_with('$') {
                let msg = format!("Stage {} key '{}' must start with '$'.", i + 1, key);
                return alloc_json(&validate_error_envelope(&msg));
            }
        }
    }
    alloc_json("{\"valid\":true}")
}

/// Normalize a raw aggregate pipeline string to pretty-printed strict JSON.
///
/// Accepts the same Mongo shell / JSON5 dialect as
/// `dbengine_validate_aggregate_pipeline`. On parse error the original
/// input is returned unchanged so callers can preserve the user's draft
/// while still showing them a validation error elsewhere.
///
/// # Safety
/// `pipeline_json` must be a valid, null-terminated C string.
///
/// Returns a string the caller must free with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_normalize_aggregate_pipeline(
    pipeline_json: *const c_char,
) -> *mut c_char {
    if pipeline_json.is_null() {
        return std::ptr::null_mut();
    }
    let text = unsafe { CStr::from_ptr(pipeline_json).to_string_lossy().into_owned() };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return CString::new(text).unwrap_or_default().into_raw();
    }
    let stages: Vec<bson::Document> = match json5::from_str(trimmed) {
        Ok(p) => p,
        Err(_) => return CString::new(text).unwrap_or_default().into_raw(),
    };
    let mut values: Vec<serde_json::Value> = Vec::with_capacity(stages.len());
    for stage in stages.iter() {
        match bson::Bson::Document(stage.clone()).into_relaxed_extjson() {
            serde_json::Value::Object(map) => values.push(serde_json::Value::Object(map)),
            other => values.push(other),
        }
    }
    match serde_json::to_string_pretty(&values) {
        Ok(pretty) => CString::new(pretty).unwrap_or_default().into_raw(),
        Err(_) => CString::new(text).unwrap_or_default().into_raw(),
    }
}

fn alloc_json(contents: &str) -> *mut c_char {
    CString::new(contents)
        .map(|c| c.into_raw())
        .unwrap_or_else(|_| std::ptr::null_mut())
}

fn validate_error_envelope(message: &str) -> String {
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{{\"valid\":false,\"error\":\"{}\"}}", escaped)
}

/// Insert a single document into a collection.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, and `document_json` must be valid, null-terminated C strings.
///
/// Returns the inserted document's _id as a JSON string.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_insert_one(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    document_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || document_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let doc_str = unsafe { CStr::from_ptr(document_json).to_string_lossy().into_owned() };

    let db_log = db_str.clone();
    let coll_log = coll_str.clone();

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;

        let doc: Document = serde_json::from_str(&doc_str)
            .map_err(|e| error::DbEngineError::Query(format!("invalid document JSON: {}", e)))?;

        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let result = coll
            .insert_one(doc)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        let id_json = serde_json::to_string(&result.inserted_id)
            .map_err(|e| error::DbEngineError::Serialization(e.to_string()))?;

        Ok::<_, error::DbEngineError>(id_json)
    });

    match result {
        Some(Ok(json)) => {
            info!(db = %db_log, collection = %coll_log, "Document inserted via FFI");
            if let Ok(mut engine) = ENGINE.write() {
                engine.invalidate_collection(&db_log, &coll_log);
            }
            match CString::new(json) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => error_json("Failed to create C string from JSON"),
            }
        }
        Some(Err(e)) => {
            error!(db = %db_log, collection = %coll_log, error = %e, "Insert failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Update a single document in a collection.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, `filter_json`, and `update_json` must be valid, null-terminated C strings.
///
/// Returns `{"matched": <n>, "modified": <n>}` as a JSON string.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_update_one(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    filter_json: *const c_char,
    update_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || filter_json.is_null() || update_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let filter_str = unsafe { CStr::from_ptr(filter_json).to_string_lossy().into_owned() };
    let update_str = unsafe { CStr::from_ptr(update_json).to_string_lossy().into_owned() };

    let db_log = db_str.clone();
    let coll_log = coll_str.clone();

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;

        let filter = query_parser::parse_query(&filter_str)
            .map_err(|e| error::DbEngineError::Query(format!("invalid filter: {}", e)))?;

        let update: Document = serde_json::from_str(&update_str)
            .map_err(|e| error::DbEngineError::Query(format!("invalid update JSON: {}", e)))?;

        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let result = coll
            .update_one(filter, update)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        let response = serde_json::json!({
            "matched": result.matched_count,
            "modified": result.modified_count
        });

        serde_json::to_string(&response)
            .map_err(|e| error::DbEngineError::Serialization(e.to_string()))
    });

    match result {
        Some(Ok(json)) => {
            info!(db = %db_log, collection = %coll_log, "Document updated via FFI");
            if let Ok(mut engine) = ENGINE.write() {
                engine.invalidate_collection(&db_log, &coll_log);
            }
            match CString::new(json) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => error_json("Failed to create C string from JSON"),
            }
        }
        Some(Err(e)) => {
            error!(db = %db_log, collection = %coll_log, error = %e, "Update failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Batch update multiple documents in a collection.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, and `updates_json` must be valid, null-terminated C strings.
/// `updates_json` should be a JSON array of objects:
/// `[{"filter": {...}, "update": {...}}, ...]`
///
/// Returns `{"success": true, "updated": <n>}` as a JSON string.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_batch_update(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    updates_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || updates_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let updates_str = unsafe { CStr::from_ptr(updates_json).to_string_lossy().into_owned() };

    let db_log = db_str.clone();
    let coll_log = coll_str.clone();

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;

        if query_perf_logs_enabled() {
            println!("[Rust] Batch update received: updates_str={}", updates_str);
        }

        let updates: Vec<serde_json::Value> = serde_json::from_str(&updates_str)
            .map_err(|e| error::DbEngineError::Query(format!("invalid updates JSON: {}", e)))?;

        if query_perf_logs_enabled() {
            println!("[Rust] Parsed {} update entries", updates.len());
        }

        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let mut total_updated = 0;

        for (i, update_entry) in updates.iter().enumerate() {
            let filter_str = update_entry
                .get("filter")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .ok_or_else(|| error::DbEngineError::Query("missing filter".to_string()))?;

            if query_perf_logs_enabled() {
                println!("[Rust] Update #{} filter_str={}", i, filter_str);
            }

            let filter_obj = query_parser::parse_query(&filter_str)
                .map_err(|e| error::DbEngineError::Query(format!("invalid filter: {}", e)))?;

            if query_perf_logs_enabled() {
                println!("[Rust] Update #{} filter_obj={:?}", i, filter_obj);
            }

            let update_str = update_entry
                .get("update")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .ok_or_else(|| error::DbEngineError::Query("missing update".to_string()))?;

            if query_perf_logs_enabled() {
                println!("[Rust] Update #{} update_str={}", i, update_str);
            }

            let update_obj = query_parser::parse_query(&update_str)
                .map_err(|e| error::DbEngineError::Query(format!("invalid update: {}", e)))?;

            if query_perf_logs_enabled() {
                println!("[Rust] Update #{} update_obj={:?}", i, update_obj);
            }

            let result = coll
                .update_one(filter_obj, update_obj)
                .await
                .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

            if query_perf_logs_enabled() {
                println!(
                    "[Rust] Update #{} matched={}, modified={}",
                    i, result.matched_count, result.modified_count
                );
            }

            total_updated += result.modified_count;
        }

        if query_perf_logs_enabled() {
            println!(
                "[Rust] Batch update complete: total_updated={}",
                total_updated
            );
        }

        let response = serde_json::json!({
            "success": true,
            "updated": total_updated
        });

        serde_json::to_string(&response)
            .map_err(|e| error::DbEngineError::Serialization(e.to_string()))
    });

    match result {
        Some(Ok(json)) => {
            info!(db = %db_log, collection = %coll_log, "Batch update via FFI");
            if let Ok(mut engine) = ENGINE.write() {
                engine.invalidate_collection(&db_log, &coll_log);
            }
            match CString::new(json) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => error_json("Failed to create C string from JSON"),
            }
        }
        Some(Err(e)) => {
            error!(db = %db_log, collection = %coll_log, error = %e, "Batch update failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Delete a single document from a collection.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, and `filter_json` must be valid, null-terminated C strings.
///
/// Returns `{"deleted": <n>}` as a JSON string.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_delete_one(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    filter_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || filter_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let filter_str = unsafe { CStr::from_ptr(filter_json).to_string_lossy().into_owned() };

    let db_log = db_str.clone();
    let coll_log = coll_str.clone();

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;

        let filter = query_parser::parse_query(&filter_str)
            .map_err(|e| error::DbEngineError::Query(format!("invalid filter: {}", e)))?;

        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let result = coll
            .delete_one(filter)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        let response = serde_json::json!({
            "deleted": result.deleted_count
        });

        serde_json::to_string(&response)
            .map_err(|e| error::DbEngineError::Serialization(e.to_string()))
    });

    match result {
        Some(Ok(json)) => {
            info!(db = %db_log, collection = %coll_log, "Document deleted via FFI");
            if let Ok(mut engine) = ENGINE.write() {
                engine.invalidate_collection(&db_log, &coll_log);
            }
            match CString::new(json) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => error_json("Failed to create C string from JSON"),
            }
        }
        Some(Err(e)) => {
            error!(db = %db_log, collection = %coll_log, error = %e, "Delete failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Get a single document by its _id.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db`, `collection`, and `id_json` must be valid, null-terminated C strings.
/// `id_json` should be the BSON JSON representation of the _id value.
///
/// Returns the document as a JSON string.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_get_document_json(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    id_json: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() || id_json.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let id_str = unsafe { CStr::from_ptr(id_json).to_string_lossy().into_owned() };

    let db_log = db_str.clone();
    let coll_log = coll_str.clone();

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout(async move {
        use bson::Document;

        let id_value: bson::Bson = serde_json::from_str(&id_str)
            .map_err(|e| error::DbEngineError::Query(format!("invalid _id JSON: {}", e)))?;

        let filter = bson::doc! { "_id": id_value };
        let coll = client.database(&db_str).collection::<Document>(&coll_str);

        let doc = coll
            .find_one(filter)
            .await
            .map_err(|e| error::DbEngineError::Query(e.to_string()))?;

        match doc {
            Some(d) => {
                let json_doc = bson::to_document(&d)
                    .ok()
                    .and_then(|b| serde_json::to_string(&b).ok())
                    .unwrap_or_else(|| "{}".to_string());
                Ok::<_, error::DbEngineError>(json_doc)
            }
            None => Ok::<_, error::DbEngineError>("null".to_string()),
        }
    });

    match result {
        Some(Ok(json)) => {
            info!(db = %db_log, collection = %coll_log, "Document fetched via FFI");
            match CString::new(json) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => error_json("Failed to create C string from JSON"),
            }
        }
        Some(Err(e)) => {
            error!(db = %db_log, collection = %coll_log, error = %e, "Fetch document failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Get collection info: indexes and sample schema.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
/// `db` and `collection` must be valid, null-terminated C strings.
///
/// Returns JSON: `{"indexes": [...], "sample_schema": {...}, "count": <n>}`
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_get_collection_info(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
) -> *mut c_char {
    if db.is_null() || collection.is_null() {
        return error_json("null parameter");
    }

    let db_str = unsafe { CStr::from_ptr(db).to_string_lossy().into_owned() };
    let coll_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };

    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(c) => c,
            Err(e) => return error_json(&e.to_string()),
        },
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    let result = run_with_timeout({
        let db = db_str.clone();
        let coll = coll_str.clone();
        async move {
            use bson::Document;
            use serde_json::json;

            let coll_obj = client.database(&db).collection::<Document>(&coll);

            // Get indexes
            let mut cursor = coll_obj
                .list_indexes()
                .await
                .map_err(|e| error::DbEngineError::Query(e.to_string()))?;
            let mut indexes: Vec<Document> = Vec::new();
            while let Some(result) = cursor.next().await {
                if let Ok(idx_model) = result {
                    let mut doc = Document::new();
                    let (name, unique) = match idx_model.options {
                        Some(opts) => (opts.name.unwrap_or_default(), opts.unique.unwrap_or(false)),
                        None => (String::new(), false),
                    };
                    doc.insert("name", name);
                    let key_doc: Document = idx_model.keys.clone();
                    doc.insert("key", bson::Bson::Document(key_doc));
                    doc.insert("unique", bson::Bson::Boolean(unique));
                    indexes.push(doc);
                }
            }

            // Get document count (estimated, O(1))
            let count = coll_obj.estimated_document_count().await.unwrap_or(0);

            // Get sample schema from first 100 docs
            let mut find_cursor = coll_obj
                .find(Document::new())
                .limit(100)
                .await
                .map_err(|e| error::DbEngineError::Query(e.to_string()))?;
            let mut sample_docs: Vec<Document> = Vec::new();
            while let Some(result) = find_cursor.next().await {
                if let Ok(doc) = result {
                    sample_docs.push(doc);
                }
            }

            // Infer schema: collect all keys + types
            let mut schema: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for doc in &sample_docs {
                for (key, value) in doc {
                    if !schema.contains_key::<str>(key) {
                        let type_name = match value {
                            bson::Bson::Int32(_) => "int32",
                            bson::Bson::Int64(_) => "int64",
                            bson::Bson::Double(_) => "double",
                            bson::Bson::Boolean(_) => "boolean",
                            bson::Bson::String(_) => "string",
                            bson::Bson::Array(_) => "array",
                            bson::Bson::Document(d) if arrow::is_extended_json_date(d) => {
                                eprintln!("[schema] Column '{}': detected extended JSON date", key);
                                "datetime"
                            }
                            bson::Bson::Document(_) => "object",
                            bson::Bson::ObjectId(_) => "objectId",
                            bson::Bson::DateTime(_) => "datetime",
                            _ => "unknown",
                        };
                        eprintln!("[schema] Column '{}': type={}", key, type_name);
                        schema.insert(key.clone(), type_name.to_string());
                    }
                }
            }

            let index_list: Vec<serde_json::Value> = indexes.iter().map(|idx| {
                json!({
                    "name": idx.get("name").map(|v| v.to_string()).unwrap_or_default(),
                    "key": idx.get("key").map(|v| v.to_string()).unwrap_or_default(),
                    "unique": idx.get("unique").map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false)
                })
            }).collect();

            let response = json!({
                "indexes": index_list,
                "sample_schema": schema,
                "count": count
            });

            serde_json::to_string(&response)
                .map_err(|e| error::DbEngineError::Serialization(e.to_string()))
        }
    });

    match result {
        Some(Ok(json)) => {
            info!(db = %db_str, collection = %coll_str, "Collection info retrieved via FFI");
            match CString::new(json) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => error_json("Failed to create C string from JSON"),
            }
        }
        Some(Err(e)) => {
            error!(db = %db_str, collection = %coll_str, error = %e, "Collection info failed via FFI");
            error_json(&e.to_string())
        }
        None => error_json("Operation timed out"),
    }
}

/// Get server info (version, host, etc.) for a connection.
///
/// # Safety
/// `conn_id` must be a valid connection ID.
///
/// Returns a JSON object with server info as a C string.
/// Caller must free with `dbengine_free_string`.
/// Returns a JSON error object on failure.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_server_info(conn_id: u64) -> *mut c_char {
    info!(conn_id = conn_id, "dbengine_server_info called");
    let engine = match ENGINE.read() {
        Ok(e) => e,
        Err(e) => return error_json(&format!("Engine lock poisoned: {}", e)),
    };

    // Try MongoDB first
    if let Ok(client) = engine.manager.get_client(conn_id) {
        drop(engine);
        let result = run_with_timeout(async move {
            let db = client.database("admin");
            let cmd = bson::doc! { "buildInfo": 1 };
            let result_doc = db
                .run_command(cmd)
                .await
                .map_err(|e| crate::error::DbEngineError::MongoDB(e.to_string()))?;
            Ok::<_, crate::error::DbEngineError>(result_doc)
        });

        return match result {
            Some(Ok(doc)) => {
                let info = ServerInfoResponse {
                    version: doc.get_str("version").unwrap_or("unknown").to_string(),
                    git_version: doc.get_str("gitVersion").unwrap_or("").to_string(),
                    engine: doc.get_str("engine").unwrap_or("").to_string(),
                    host: doc.get_str("host").unwrap_or("").to_string(),
                };
                success_json(&info)
            }
            Some(Err(e)) => {
                error!(error = %e, "Server info failed via FFI");
                error_json(&e.to_string())
            }
            None => error_json("Operation timed out"),
        };
    }

    // Try PostgreSQL
    if let Some(conn) = engine.postgres_connections.get(&conn_id).cloned() {
        drop(engine);
        let result = run_with_timeout(async move { conn.server_info().await });

        return match result {
            Some(Ok(info)) => success_json(&info),
            Some(Err(e)) => {
                error!(conn_id = conn_id, error = %e, "Postgres server info failed via FFI");
                error_json(&e.to_string())
            }
            None => error_json("Operation timed out"),
        };
    }

    error_json(&format!("Connection {} not found", conn_id))
}

#[derive(serde::Serialize)]
pub struct ServerInfoResponse {
    version: String,
    git_version: String,
    engine: String,
    host: String,
}

/// Free Arrow C Data Interface structs returned by `dbengine_execute_query_arrow`.
///
/// # Safety
/// `schema_ptr` and `array_ptr` must be pointers returned by `dbengine_execute_query_arrow`.
/// Must not be called twice on the same pointers.
/// After calling, the pointers are invalid and must not be used.
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_arrow_cdata(
    schema_ptr: *mut FFI_ArrowSchema,
    array_ptr: *mut FFI_ArrowArray,
) {
    if !schema_ptr.is_null() {
        unsafe {
            let _ = Box::from_raw(schema_ptr);
        }
    }
    if !array_ptr.is_null() {
        unsafe {
            let _ = Box::from_raw(array_ptr);
        }
    }
}

/// Free a cursor string returned by `dbengine_execute_postgres_query_arrow`.
///
/// # Safety
/// `s` must be a pointer returned by a `dbengine_*` function,
/// or null. Must not be called twice on the same pointer.
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_cursor_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
}

/// Free a C string returned by any `dbengine_*` function.
///
/// # Safety
/// `s` must be a pointer returned by a `dbengine_*` function,
/// or null. Must not be called twice on the same pointer.
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
}

/// Get autocomplete suggestions for a query.
///
/// # Safety
/// `db_type`, `query_text`, and `schema_json` must be valid, null-terminated C strings.
///
/// Returns JSON: `{"items": [{"label": "...", "kind": "...", "detail": "..."}], "trigger_range": [start, end]}`
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_get_completions(
    db_type: *const c_char,
    query_text: *const c_char,
    cursor_pos: i32,
    schema_json: *const c_char,
) -> *mut c_char {
    if db_type.is_null() || query_text.is_null() {
        return error_json("null parameter");
    }

    let db_type_str = unsafe { CStr::from_ptr(db_type).to_string_lossy().into_owned() };
    let query_str = unsafe { CStr::from_ptr(query_text).to_string_lossy().into_owned() };
    let schema_str = if schema_json.is_null() {
        "{}".to_string()
    } else {
        unsafe { CStr::from_ptr(schema_json).to_string_lossy().into_owned() }
    };

    let response =
        autocomplete::get_completions(&db_type_str, &query_str, cursor_pos as usize, &schema_str);

    match serde_json::to_string(&response) {
        Ok(json) => match CString::new(json) {
            Ok(cstr) => cstr.into_raw(),
            Err(_) => error_json("Failed to create C string from JSON"),
        },
        Err(e) => error_json(&e.to_string()),
    }
}

/// Get PostgreSQL SQL completion suggestions from a JSON request.
///
/// # Safety
/// `request_json` must be a valid, null-terminated C string containing a
/// serialized `sql_completion::CompletionRequest`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_get_sql_completions(request_json: *const c_char) -> *mut c_char {
    if request_json.is_null() {
        return error_json("null parameter");
    }

    let request_str = unsafe { CStr::from_ptr(request_json).to_string_lossy().into_owned() };
    let request = match serde_json::from_str::<sql_completion::CompletionRequest>(&request_str) {
        Ok(request) => request,
        Err(e) => return error_json(&format!("invalid SQL completion request: {e}")),
    };

    let response = sql_completion::complete_sql(&request);
    match serde_json::to_string(&response) {
        Ok(json) => match CString::new(json) {
            Ok(cstr) => cstr.into_raw(),
            Err(_) => error_json("Failed to create C string from JSON"),
        },
        Err(e) => error_json(&e.to_string()),
    }
}

/// Build a relevance-ranked schema digest for AI context.
///
/// # Safety
/// `request_json` must be a valid, null-terminated C string.
/// It should be a serialized `DigestRequest` with `catalog`, `query`, and optional `caps` fields.
/// Returns a JSON string. Caller must free with `dbengine_free_string`.
#[no_mangle]
#[must_use]
pub extern "C" fn dbengine_build_ai_schema_digest(request_json: *const c_char) -> *mut c_char {
    let request_str = unsafe { CStr::from_ptr(request_json) }
        .to_str()
        .unwrap_or("");

    let request: ai_schema::DigestRequest = match serde_json::from_str(request_str) {
        Ok(r) => r,
        Err(_) => {
            let empty = ai_schema::SchemaDigest {
                tables: vec![],
                truncated: false,
            };
            let json = serde_json::to_string(&empty).unwrap_or_else(|_| "{}".to_string());
            return CString::new(json).unwrap_or_default().into_raw();
        }
    };

    let digest = ai_schema::build_schema_digest(
        &request.catalog,
        &request.query,
        &request.caps,
        &request.mentioned_tables,
    );
    let json = serde_json::to_string(&digest).unwrap_or_else(|_| "{}".to_string());
    CString::new(json).unwrap_or_default().into_raw()
}

/// Update schema cache for autocomplete.
///
/// # Safety
/// `db_type`, `collection`, and `fields_json` must be valid, null-terminated C strings.
/// `fields_json` should be a JSON object of field names to types,
/// or legacy JSON array of field names: `["field1", "field2", ...]`
#[no_mangle]
pub extern "C" fn dbengine_update_schema_cache(
    db_type: *const c_char,
    collection: *const c_char,
    fields_json: *const c_char,
) {
    if db_type.is_null() || collection.is_null() || fields_json.is_null() {
        return;
    }

    let db_type_str = unsafe { CStr::from_ptr(db_type).to_string_lossy().into_owned() };
    let collection_str = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let fields_str = unsafe { CStr::from_ptr(fields_json).to_string_lossy().into_owned() };

    if db_type_str == "mongodb" {
        if let Ok(fields) =
            serde_json::from_str::<std::collections::HashMap<String, String>>(&fields_str)
        {
            autocomplete::update_mongo_schema(
                &collection_str,
                autocomplete::mongo_fields_from_type_map(fields),
            );
        } else if let Ok(fields) = serde_json::from_str::<Vec<String>>(&fields_str) {
            autocomplete::update_mongo_schema(
                &collection_str,
                autocomplete::mongo_fields_from_names(fields),
            );
        }
    }
}

/// Gracefully shut down the engine.
///
/// # Safety
/// This function is safe to call. After calling, no other
/// `dbengine_*` functions should be used.
///
/// Note: The shared tokio runtime cannot be shut down from
/// outside its own context. This function clears engine state
/// and logs the shutdown. The runtime will be cleaned up on
/// process exit.
#[no_mangle]
pub extern "C" fn dbengine_shutdown() {
    info!("Shutting down dbengine");

    match ENGINE.write() {
        Ok(mut engine) => {
            engine.manager = ConnectionManager::new();
            info!("Engine state cleared");
        }
        Err(e) => {
            error!("Failed to acquire engine lock during shutdown: {}", e);
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_save_history(
    conn_uuid: *const c_char,
    database: *const c_char,
    collection: *const c_char,
    filter: *const c_char,
    project: *const c_char,
    sort: *const c_char,
    skip: i64,
    limit: i64,
    max_time_ms: i64,
    result_count: i64,
) -> i32 {
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };
    let db = unsafe { CStr::from_ptr(database).to_string_lossy().into_owned() };
    let coll = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let filter = unsafe { CStr::from_ptr(filter).to_string_lossy().into_owned() };
    let project = unsafe { CStr::from_ptr(project).to_string_lossy().into_owned() };
    let sort = unsafe { CStr::from_ptr(sort).to_string_lossy().into_owned() };

    match history::save_entry(
        &uuid,
        &db,
        &coll,
        &filter,
        &project,
        &sort,
        skip,
        limit,
        max_time_ms,
        result_count,
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[history] save failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_fetch_history(
    conn_uuid: *const c_char,
    database: *const c_char,
    collection: *const c_char,
    limit: u32,
    out_json: *mut *mut c_char,
) -> i32 {
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };
    let db = unsafe { CStr::from_ptr(database).to_string_lossy().into_owned() };
    let coll = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };

    match history::fetch_entries(&uuid, &db, &coll, limit) {
        Ok(entries) => {
            let json_entries: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "id": e.id,
                        "conn_uuid": e.conn_uuid,
                        "database": e.database,
                        "collection": e.collection,
                        "filter": e.filter,
                        "project": e.project,
                        "sort": e.sort,
                        "skip": e.skip,
                        "limit": e.limit,
                        "max_time_ms": e.max_time_ms,
                        "result_count": e.result_count,
                        "timestamp": e.timestamp,
                    })
                })
                .collect();

            let json_str = serde_json::to_string(&json_entries).unwrap_or_default();
            let c_str = CString::new(json_str).unwrap();
            unsafe {
                *out_json = c_str.into_raw();
            }
            0
        }
        Err(e) => {
            eprintln!("[history] fetch failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_delete_history_entry(id: i64) -> i32 {
    match history::delete_entry(id) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[history] delete failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_clear_history(
    conn_uuid: *const c_char,
    database: *const c_char,
    collection: *const c_char,
) -> i32 {
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };
    let db = unsafe { CStr::from_ptr(database).to_string_lossy().into_owned() };
    let coll = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };

    match history::clear_entries(&uuid, &db, &coll) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[history] clear failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_save_connection_state(
    conn_uuid: *const c_char,
    state_json: *const c_char,
) -> i32 {
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };
    let json = unsafe { CStr::from_ptr(state_json).to_string_lossy().into_owned() };

    match view_state::save_state(&uuid, &json) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[view_state] save failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_load_connection_state(
    conn_uuid: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };

    match view_state::load_state(&uuid) {
        Ok(Some(json)) => {
            let c_str = CString::new(json).unwrap();
            unsafe { *out_json = c_str.into_raw() };
            0
        }
        Ok(None) => {
            unsafe { *out_json = std::ptr::null_mut() };
            0
        }
        Err(e) => {
            eprintln!("[view_state] load failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_delete_connection_state(conn_uuid: *const c_char) -> i32 {
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };

    match view_state::delete_state(&uuid) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[view_state] delete failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_saved_connections(out_json: *mut *mut c_char) -> i32 {
    match saved_connection::list_connections() {
        Ok(items) => match serde_json::to_string(&items) {
            Ok(json) => {
                let c_str = CString::new(json).unwrap();
                unsafe { *out_json = c_str.into_raw() };
                0
            }
            Err(e) => {
                eprintln!("[connection] list encode failed: {}", e);
                -1
            }
        },
        Err(e) => {
            eprintln!("[connection] list failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_upsert_saved_connection(connection_json: *const c_char) -> i32 {
    let json = unsafe {
        CStr::from_ptr(connection_json)
            .to_string_lossy()
            .into_owned()
    };
    let record = match serde_json::from_str::<saved_connection::SavedConnectionRecord>(&json) {
        Ok(record) => record,
        Err(e) => {
            eprintln!("[connection] decode failed: {}", e);
            return -1;
        }
    };

    match saved_connection::upsert_connection(&record) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] upsert failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_delete_saved_connection(id: *const c_char) -> i32 {
    let id_str = unsafe { CStr::from_ptr(id).to_string_lossy().into_owned() };
    match saved_connection::delete_connection(&id_str) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] delete failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_touch_saved_connection(id: *const c_char, timestamp: f64) -> i32 {
    let id_str = unsafe { CStr::from_ptr(id).to_string_lossy().into_owned() };
    match saved_connection::touch_connection(&id_str, timestamp) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] touch failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_set_connection_pinned(id: *const c_char, is_pinned: bool) -> i32 {
    let id_str = unsafe { CStr::from_ptr(id).to_string_lossy().into_owned() };
    match saved_connection::set_connection_pinned(&id_str, is_pinned) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] set pinned failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_project_connection_ids(
    project_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    match saved_connection::list_project_connection_ids(&ffi_string(project_id)) {
        Ok(items) => write_json_out(&items, out_json),
        Err(e) => {
            eprintln!("[connection] list project memberships failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_add_project_connection(
    project_id: *const c_char,
    connection_id: *const c_char,
) -> i32 {
    match saved_connection::add_project_connection(
        &ffi_string(project_id),
        &ffi_string(connection_id),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] add project membership failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_remove_project_connection(
    project_id: *const c_char,
    connection_id: *const c_char,
) -> i32 {
    match saved_connection::remove_project_connection(
        &ffi_string(project_id),
        &ffi_string(connection_id),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] remove project membership failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_projects(out_json: *mut *mut c_char) -> i32 {
    match saved_connection::list_projects() {
        Ok(items) => write_json_out(&items, out_json),
        Err(e) => {
            eprintln!("[connection] list projects failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_upsert_project(project_json: *const c_char) -> i32 {
    let record =
        match serde_json::from_str::<saved_connection::ProjectRecord>(&ffi_string(project_json)) {
            Ok(record) => record,
            Err(e) => {
                eprintln!("[connection] decode project failed: {}", e);
                return -1;
            }
        };
    match saved_connection::upsert_project(&record) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] upsert project failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_delete_project(id: *const c_char) -> i32 {
    match saved_connection::delete_project(&ffi_string(id)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] delete project failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_rename_project(
    old_id: *const c_char,
    project_json: *const c_char,
) -> i32 {
    let record =
        match serde_json::from_str::<saved_connection::ProjectRecord>(&ffi_string(project_json)) {
            Ok(record) => record,
            Err(e) => {
                eprintln!("[connection] decode renamed project failed: {}", e);
                return -1;
            }
        };
    match saved_connection::rename_project(&ffi_string(old_id), &record) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[connection] rename project failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_ai_upsert_conversation(conversation_json: *const c_char) -> i32 {
    let conversation =
        match serde_json::from_str::<ai_chat::AIConversationRecord>(&ffi_string(conversation_json))
        {
            Ok(value) => value,
            Err(error) => {
                eprintln!("[ai_chat] decode upsert failed: {error}");
                return -1;
            }
        };
    match ai_chat::upsert_conversation(&conversation) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[ai_chat] upsert failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_ai_list_conversations(
    scope_json: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let scope = match serde_json::from_str::<ai_chat::AIConversationScope>(&ffi_string(scope_json))
    {
        Ok(value) => value,
        Err(error) => {
            eprintln!("[ai_chat] decode scope failed: {error}");
            return -1;
        }
    };
    match ai_chat::list_conversations(&scope, limit) {
        Ok(items) => write_json_out(&items, out_json),
        Err(error) => {
            eprintln!("[ai_chat] list failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_ai_get_conversation(
    id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    match ai_chat::get_conversation(&ffi_string(id)) {
        Ok(Some(conversation)) => write_json_out(&conversation, out_json),
        Ok(None) => {
            unsafe { *out_json = std::ptr::null_mut() };
            0
        }
        Err(error) => {
            eprintln!("[ai_chat] get failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_ai_delete_conversation(id: *const c_char) -> i32 {
    match ai_chat::delete_conversation(&ffi_string(id)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[ai_chat] delete failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_saved_aggregates(
    conn_uuid: *const c_char,
    database: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };
    let db = unsafe { CStr::from_ptr(database).to_string_lossy().into_owned() };

    match aggregate::list_aggregates(&uuid, &db) {
        Ok(items) => {
            let json_items: Vec<serde_json::Value> = items
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "id": a.id,
                        "conn_uuid": a.conn_uuid,
                        "database": a.database,
                        "collection": a.collection,
                        "name": a.name,
                        "kind": a.kind,
                        "pipeline_json": a.pipeline_json,
                        "created_at": a.created_at,
                        "updated_at": a.updated_at,
                    })
                })
                .collect();
            let json_str = serde_json::to_string(&json_items).unwrap_or_else(|_| "[]".to_string());
            let c_str = CString::new(json_str).unwrap();
            unsafe {
                *out_json = c_str.into_raw();
            }
            0
        }
        Err(e) => {
            eprintln!("[aggregate] list failed: {}", e);
            let empty = CString::new("[]").unwrap();
            unsafe {
                *out_json = empty.into_raw();
            }
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_get_saved_aggregate(
    id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let id_str = unsafe { CStr::from_ptr(id).to_string_lossy().into_owned() };

    match aggregate::get_aggregate(&id_str) {
        Ok(Some(a)) => {
            let json = serde_json::json!({
                "id": a.id,
                "conn_uuid": a.conn_uuid,
                "database": a.database,
                "collection": a.collection,
                "name": a.name,
                "kind": a.kind,
                "pipeline_json": a.pipeline_json,
                "created_at": a.created_at,
                "updated_at": a.updated_at,
            });
            let json_str = serde_json::to_string(&json).unwrap_or_else(|_| "null".to_string());
            let c_str = CString::new(json_str).unwrap();
            unsafe {
                *out_json = c_str.into_raw();
            }
            0
        }
        Ok(None) => {
            let empty = CString::new("null").unwrap();
            unsafe {
                *out_json = empty.into_raw();
            }
            1
        }
        Err(e) => {
            eprintln!("[aggregate] get failed: {}", e);
            let empty = CString::new("null").unwrap();
            unsafe {
                *out_json = empty.into_raw();
            }
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_upsert_saved_aggregate(
    id: *const c_char,
    conn_uuid: *const c_char,
    database: *const c_char,
    collection: *const c_char,
    name: *const c_char,
    kind: *const c_char,
    pipeline_json: *const c_char,
    created_at: f64,
    updated_at: f64,
) -> i32 {
    let id_str = unsafe { CStr::from_ptr(id).to_string_lossy().into_owned() };
    let uuid = unsafe { CStr::from_ptr(conn_uuid).to_string_lossy().into_owned() };
    let db = unsafe { CStr::from_ptr(database).to_string_lossy().into_owned() };
    let coll = unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
    let name = unsafe { CStr::from_ptr(name).to_string_lossy().into_owned() };
    let kind_str = unsafe { CStr::from_ptr(kind).to_string_lossy().into_owned() };
    let pipeline = unsafe { CStr::from_ptr(pipeline_json).to_string_lossy().into_owned() };

    match aggregate::upsert_aggregate(
        &id_str, &uuid, &db, &coll, &name, &kind_str, &pipeline, created_at, updated_at,
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[aggregate] upsert failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_delete_saved_aggregate(id: *const c_char) -> i32 {
    let id_str = unsafe { CStr::from_ptr(id).to_string_lossy().into_owned() };

    match aggregate::delete_aggregate(&id_str) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[aggregate] delete failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_highlight_syntax(
    text: *const c_char,
    language: *const c_char,
) -> *mut c_char {
    let text_str = if text.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(text).to_string_lossy().into_owned() }
    };
    let lang_str = if language.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(language).to_string_lossy().into_owned() }
    };

    let lang = match lang_str.as_str() {
        "json" => syntax::SyntaxLanguage::Json,
        "mongoExpression" => syntax::SyntaxLanguage::MongoExpression,
        "javascript" => syntax::SyntaxLanguage::JavaScript,
        "sql" => syntax::SyntaxLanguage::Sql,
        _ => {
            let empty = CString::new("{\"tokens\":[]}").unwrap();
            return empty.into_raw();
        }
    };

    let tokens = syntax::highlight_syntax(&text_str, lang);
    let payload = serde_json::json!({ "tokens": tokens });
    let json = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(_) => "{\"tokens\":[]}".to_string(),
    };
    let c_str = CString::new(json).unwrap_or_else(|_| CString::new("{\"tokens\":[]}").unwrap());
    c_str.into_raw()
}

#[no_mangle]
pub extern "C" fn dbengine_classify_sql_risk(sql_json: *const c_char) -> *mut c_char {
    let sql_json = unsafe { CStr::from_ptr(sql_json) }.to_str().unwrap_or("");
    let sql: String = match serde_json::from_str::<serde_json::Value>(sql_json) {
        Ok(v) => v
            .get("sql")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        Err(_) => sql_json.to_string(),
    };
    let report = crate::sql_risk::classify_sql_risk(&sql);
    let json = serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string());
    CString::new(json).unwrap_or_default().into_raw()
}

#[cfg(test)]
mod sql_completion_ffi_tests {
    use super::*;
    use serde_json::json;
    use std::ffi::CString;

    fn read(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { dbengine_free_string(ptr) };
        s
    }

    #[test]
    fn sql_completion_ffi_returns_json_response() {
        let request = CString::new(
            r#"{"dialect":"postgresql","query":"SEL","cursor_pos":3,"catalog":{"schemas":[]}}"#,
        )
        .unwrap();

        let output = read(dbengine_get_sql_completions(request.as_ptr()));
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(value["replacement_range"], json!([0, 3]));
        assert!(value["items"].as_array().unwrap().iter().any(|item| {
            item["label"] == "SELECT"
                && item["kind"] == "keyword"
                && item["insert_text"] == "SELECT"
        }));
    }
}

#[cfg(test)]
mod aggregate_pipeline_validation_tests {
    use super::*;
    use std::ffi::CString;

    fn ptr(s: &str) -> *const c_char {
        CString::new(s).unwrap().into_raw() as *const c_char
    }

    fn read(ptr: *mut c_char) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let s = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { dbengine_free_string(ptr) };
        s
    }

    fn assert_valid(input: &str) {
        let p = dbengine_validate_aggregate_pipeline(ptr(input));
        let out = read(p);
        assert!(
            out.contains("\"valid\":true"),
            "expected valid, got: {}",
            out
        );
    }

    fn assert_invalid(input: &str, contains: &str) {
        let p = dbengine_validate_aggregate_pipeline(ptr(input));
        let out = read(p);
        assert!(
            out.contains("\"valid\":false") && out.contains(contains),
            "expected invalid containing '{}', got: {}",
            contains,
            out
        );
    }

    #[test]
    fn validates_empty_array() {
        assert_valid("[]");
    }

    #[test]
    fn validates_strict_json_pipeline() {
        assert_valid(r#"[{"$match": {"x": 1}}]"#);
    }

    #[test]
    fn validates_unquoted_dollar_keys() {
        assert_valid(r#"[{ $match: { storeCode: "STOREA" } }]"#);
    }

    #[test]
    fn validates_line_comment() {
        let with_comment = r#"[
            // Pull every field from storeProfile
            { $match: { storeCode: "STOREA" } }
        ]"#;
        assert_valid(with_comment);
    }

    #[test]
    fn validates_block_comment() {
        let with_block = r#"[
            /* full lookup */
            { $match: { storeCode: "STOREA" } }
        ]"#;
        assert_valid(with_block);
    }

    #[test]
    fn validates_trailing_comma_in_object() {
        assert_valid(r#"[{ $match: { a: 1, } }]"#);
    }

    #[test]
    fn validates_trailing_comma_in_array() {
        assert_valid(r#"[{ $match: {} },]"#);
    }

    #[test]
    fn validates_full_compass_pipeline() {
        let compass = r#"[
            { $match: { storeCode: "STOREA" } },
            { $addFields: { storeIdString: { $toString: "$_id" } } },
            { $lookup: { from: "store-api-keys", as: "apiKeyInfo", pipeline: [ { $match: { $expr: { $eq: ["$storeId", "$$storeId"] } } } ] } },
            { $project: { _id: 1, storeCode: 1, name: 1, apiKeyInfo: 1 } }
        ]"#;
        assert_valid(compass);
    }

    #[test]
    fn rejects_empty_input() {
        assert_invalid("", "Pipeline is empty");
    }

    #[test]
    fn rejects_top_level_object() {
        assert_invalid("{}", "Invalid JSON");
    }

    #[test]
    fn rejects_garbage() {
        assert_invalid("not json", "Invalid JSON");
    }

    #[test]
    fn rejects_stage_with_multiple_keys() {
        assert_invalid(r#"[{"$match": {}, "$sort": {}}]"#, "single-key object");
    }

    #[test]
    fn rejects_stage_with_non_dollar_key() {
        assert_invalid(r#"[{"match": {}}]"#, "must start with '$'");
    }

    #[test]
    fn rejects_unquoted_value() {
        assert_invalid(r#"[{ $match: { storeCode: STOREA } }]"#, "Invalid JSON");
    }

    #[test]
    fn normalize_returns_input_on_empty() {
        let p = dbengine_normalize_aggregate_pipeline(ptr(""));
        let out = read(p);
        assert_eq!(out, "");
    }

    #[test]
    fn normalize_returns_input_on_invalid() {
        let bad = "not json";
        let p = dbengine_normalize_aggregate_pipeline(ptr(bad));
        let out = read(p);
        assert_eq!(out, bad);
    }

    #[test]
    fn normalize_quotes_unquoted_keys() {
        let relaxed = r#"[{ $match: { storeCode: "STOREA" } }]"#;
        let p = dbengine_normalize_aggregate_pipeline(ptr(relaxed));
        let out = read(p);
        assert!(out.contains("\"$match\""), "missing quoted $match: {}", out);
        assert!(
            out.contains("\"storeCode\""),
            "missing quoted storeCode: {}",
            out
        );
    }

    #[test]
    fn normalize_strips_trailing_commas() {
        let relaxed = r#"[{ $match: { a: 1, } }]"#;
        let p = dbengine_normalize_aggregate_pipeline(ptr(relaxed));
        let out = read(p);
        assert!(!out.contains(", }"), "trailing comma not stripped: {}", out);
    }

    #[test]
    fn normalize_handles_line_comment() {
        let with_comment = r#"[
            // Pull every field from storeProfile
            { $match: { storeCode: "STOREA" } }
        ]"#;
        let p = dbengine_normalize_aggregate_pipeline(ptr(with_comment));
        let out = read(p);
        assert!(!out.contains("//"), "comment not stripped: {}", out);
        assert!(out.contains("\"$match\""), "missing $match: {}", out);
    }
}

#[cfg(test)]
mod cell_batch_ffi_tests {
    use super::*;
    use bson::doc;
    use std::sync::Mutex;

    // Serialize tests that touch the global ENGINE.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn decode_cell<'a>(batch: &'a CRenderedCellBatch, cell: &CRenderedCell) -> &'a str {
        unsafe {
            let slice = std::slice::from_raw_parts(batch.text_buffer.add(cell.offset), cell.length);
            std::str::from_utf8(slice).unwrap()
        }
    }

    #[test]
    fn render_cell_batch_ffi_returns_decodable_batch() {
        let _guard = TEST_LOCK.lock().unwrap();

        // Populate a session.
        let session_id = {
            let mut engine = ENGINE.write().unwrap();
            let docs = vec![
                doc! { "name": "alice", "age": 30i64, "active": true },
                doc! { "name": "bob", "age": 25i64, "active": false },
                doc! { "name": "carol", "age": 28i64, "active": true },
            ];
            let columns = vec!["name".to_string(), "age".to_string(), "active".to_string()];
            engine.cache_documents(docs, columns)
        };

        // Ask for rows 0..2, cols 0..2.
        let batch_ptr = dbengine_render_cell_text_batch(session_id, 0, 2, 0, 2, 128);
        assert!(!batch_ptr.is_null());
        let batch = unsafe { &*batch_ptr };
        assert_eq!(batch.cell_count, 4);
        assert!(batch.text_buffer_len > 0);
        assert!(!batch.cells.is_null());
        assert!(!batch.text_buffer.is_null());

        // Decode each cell.
        let cells = unsafe { std::slice::from_raw_parts(batch.cells, batch.cell_count) };
        assert_eq!(cells[0].row, 0);
        assert_eq!(cells[0].column, 0);
        assert_eq!(decode_cell(batch, &cells[0]), "alice");
        assert_eq!(cells[1].column, 1);
        assert_eq!(decode_cell(batch, &cells[1]), "30");
        assert_eq!(cells[2].row, 1);
        assert_eq!(decode_cell(batch, &cells[2]), "bob");
        assert_eq!(cells[3].row, 1);
        assert_eq!(cells[3].column, 1);
        assert_eq!(decode_cell(batch, &cells[3]), "25");

        // Offsets/lengths must be in range.
        for c in cells {
            assert!(c.offset + c.length <= batch.text_buffer_len);
        }

        unsafe { dbengine_free_rendered_cell_batch(batch_ptr) };

        // Clean up the test session.
        let mut engine = ENGINE.write().unwrap();
        engine.invalidate_session(session_id);
    }

    #[test]
    fn render_cell_batch_ffi_returns_null_for_unknown_session() {
        let _guard = TEST_LOCK.lock().unwrap();
        let batch_ptr = dbengine_render_cell_text_batch(999_999_999, 0, 1, 0, 1, 64);
        assert!(batch_ptr.is_null());
    }

    #[test]
    fn render_cell_batch_ffi_handles_empty_window() {
        let _guard = TEST_LOCK.lock().unwrap();

        let session_id = {
            let mut engine = ENGINE.write().unwrap();
            let docs = vec![doc! { "name": "alice" }];
            let columns = vec!["name".to_string()];
            engine.cache_documents(docs, columns)
        };

        let batch_ptr = dbengine_render_cell_text_batch(session_id, 0, 0, 0, 1, 64);
        assert!(!batch_ptr.is_null());
        let batch = unsafe { &*batch_ptr };
        assert_eq!(batch.cell_count, 0);
        assert_eq!(batch.text_buffer_len, 0);
        unsafe { dbengine_free_rendered_cell_batch(batch_ptr) };

        let mut engine = ENGINE.write().unwrap();
        engine.invalidate_session(session_id);
    }

    #[test]
    fn render_cell_batch_ffi_marks_truncated_and_missing() {
        let _guard = TEST_LOCK.lock().unwrap();

        let session_id = {
            let mut engine = ENGINE.write().unwrap();
            let long_bio = "x".repeat(5000);
            let docs = vec![doc! { "name": "alice", "bio": long_bio }];
            let columns = vec!["name".to_string(), "bio".to_string(), "missing".to_string()];
            engine.cache_documents(docs, columns)
        };

        let batch_ptr = dbengine_render_cell_text_batch(session_id, 0, 1, 0, 3, 128);
        assert!(!batch_ptr.is_null());
        let batch = unsafe { &*batch_ptr };
        let cells = unsafe { std::slice::from_raw_parts(batch.cells, batch.cell_count) };
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].flags, 0);
        assert_ne!(cells[1].flags & render::CELL_FLAG_TRUNCATED, 0);
        assert_ne!(cells[2].flags & render::CELL_FLAG_MISSING, 0);
        let bio = decode_cell(batch, &cells[1]);
        assert!(bio.ends_with("..."));
        assert!(bio.len() <= 128);
        unsafe { dbengine_free_rendered_cell_batch(batch_ptr) };

        let mut engine = ENGINE.write().unwrap();
        engine.invalidate_session(session_id);
    }

    #[test]
    fn free_rendered_cell_batch_handles_null() {
        unsafe { dbengine_free_rendered_cell_batch(std::ptr::null_mut()) };
        // No panic = success
    }

    #[test]
    fn cached_session_preserves_arrow_column_order_for_render() {
        let _guard = TEST_LOCK.lock().unwrap();

        // Build the same docs that `documents_to_arrow_cdata` would see, run
        // the conversion, and use the produced `column_names` to populate the
        // cache. This verifies the cache order matches the Arrow response
        // order, which is what Swift sees in `ArrowTable`.
        let docs = vec![
            doc! { "a": 1i64, "c": 3i64, "e": 5i64 },
            doc! { "a": 2i64, "b": 20i64, "c": 30i64 },
        ];
        let response = arrow::documents_to_arrow_cdata(&docs).unwrap();
        let expected_columns = response.column_names.clone();
        let column_names = expected_columns.clone();

        let session_id = {
            let mut engine = ENGINE.write().unwrap();
            engine.cache_documents(docs, column_names)
        };

        // Verify cache stored the same column order.
        let stored = {
            let engine = ENGINE.read().unwrap();
            engine
                .document_cache
                .get(&session_id)
                .unwrap()
                .column_names
                .clone()
        };
        assert_eq!(stored, expected_columns);

        // Render col 0..N for row 0..1.
        let col_count = stored.len();
        let batch_ptr = dbengine_render_cell_text_batch(session_id, 0, 1, 0, col_count, 64);
        assert!(!batch_ptr.is_null());
        let batch = unsafe { &*batch_ptr };
        let cells = unsafe { std::slice::from_raw_parts(batch.cells, batch.cell_count) };
        assert_eq!(cells.len(), col_count);

        // Each cell at column `i` should be the BSON value of the key
        // `stored[i]` in row 0. The BSON document for row 0 is
        // { a: 1, c: 3, e: 5 }.
        let row0: std::collections::HashMap<String, i64> = [
            ("a".to_string(), 1),
            ("c".to_string(), 3),
            ("e".to_string(), 5),
        ]
        .iter()
        .cloned()
        .collect();
        for (i, cell) in cells.iter().enumerate() {
            let key = &stored[i];
            let expected = row0.get(key).map(|v| v.to_string()).unwrap_or_default();
            let actual = decode_cell(batch, cell);
            assert_eq!(actual, expected, "col {} (key={})", i, key);
            if row0.get(key).is_none() {
                assert_ne!(cell.flags & render::CELL_FLAG_MISSING, 0);
            }
        }

        unsafe { dbengine_free_rendered_cell_batch(batch_ptr) };

        let mut engine = ENGINE.write().unwrap();
        engine.invalidate_session(session_id);
    }
}
