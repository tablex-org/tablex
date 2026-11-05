#[cfg_attr(not(feature = "abi-v2"), allow(dead_code))]
mod admin;
// Pure aggregation pipeline logic; also included by the GPUI app via #[path].
#[allow(dead_code)]
pub mod aggregate;
#[cfg_attr(not(feature = "abi-v2"), allow(dead_code))]
mod aggregate_export;
#[path = "../../dbengine/src/arrow.rs"]
mod arrow;
#[path = "../../dbengine/src/autocomplete.rs"]
mod autocomplete;
#[path = "../../dbengine/src/connection.rs"]
mod connection;
mod driver;
#[path = "../../dbengine/src/error.rs"]
mod error;
#[path = "../../dbengine/src/export.rs"]
mod export;
mod ffi;
#[path = "../../dbengine/src/field_parser.rs"]
mod field_parser;
mod models;
#[cfg_attr(not(feature = "abi-v2"), allow(dead_code))]
mod mongo_command;
#[path = "../../dbengine/src/mongo_script_parser.rs"]
mod mongo_script_parser;
mod pages;
#[path = "../../dbengine/src/pipeline.rs"]
mod pipeline;
#[path = "../../dbengine/src/query_parser.rs"]
mod query_parser;
#[path = "../../dbengine/src/render.rs"]
mod render;

use driver::MongoDriver;

use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use bson::Document;
use connection::ConnectionManager;
use engine_ffi_util::{error_json, success_json};
use futures::StreamExt;
use mongo_script_parser::ScriptPlan;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{error, info};

#[derive(serde::Deserialize)]
struct ExportTableRequest {
    format: String,
    output_path: String,
    database: String,
    collection: String,
    filter_json: Option<String>,
    project_json: Option<String>,
    sort_json: Option<String>,
    skip: Option<i64>,
    limit: Option<i64>,
    batch_size: Option<i64>,
}

pub const ABI_VERSION: u32 = 2;
const OPERATION_TIMEOUT_SECS: u64 = 30;
const MAX_COUNT_CACHE_SIZE: usize = 50;
const MAX_FIELD_CACHE_SIZE: usize = 100;
const MAX_DOCUMENT_CACHE_SIZE: usize = 8;
const MAX_DOCUMENT_CACHE_BYTES: usize = 256 * 1024 * 1024;

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

lazy_static::lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .thread_name("dbengine-mongo-worker")
        .build()
        .expect("Failed to create MongoDB driver tokio runtime");

    static ref ENGINE: RwLock<EngineState> = RwLock::new(EngineState::new());
}

struct CountCacheEntry {
    filter: Document,
    count: usize,
}

#[repr(C)]
pub struct CFieldInfo {
    pub key: *mut c_char,
    pub field_type: *mut c_char,
    pub display_value: *mut c_char,
    pub is_primary_key: bool,
}

#[repr(C)]
pub struct CFieldInfoArray {
    pub fields: *mut CFieldInfo,
    pub count: usize,
}

#[repr(C)]
pub struct CRenderedCell {
    pub row: usize,
    pub column: usize,
    pub offset: usize,
    pub length: usize,
    pub flags: u32,
}

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
    count_cache: HashMap<String, CountCacheEntry>,
    count_cache_order: VecDeque<String>,
    field_cache: HashMap<String, FieldCacheEntry>,
    field_cache_order: VecDeque<String>,
    document_cache: HashMap<u64, DocumentCacheEntry>,
    document_cache_order: VecDeque<u64>,
    document_cache_bytes: usize,
    next_document_session_id: u64,
    pub cancel_flag: Arc<AtomicBool>,
    pub pipeline_registry: HashMap<String, crate::pipeline::PipelineDefinition>,
}

impl EngineState {
    fn new() -> Self {
        Self {
            manager: ConnectionManager::new(),
            count_cache: HashMap::new(),
            count_cache_order: VecDeque::new(),
            field_cache: HashMap::new(),
            field_cache_order: VecDeque::new(),
            document_cache: HashMap::new(),
            document_cache_order: VecDeque::new(),
            document_cache_bytes: 0,
            next_document_session_id: 1,
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
        while self.count_cache.len() > MAX_COUNT_CACHE_SIZE {
            if let Some(oldest_key) = self.count_cache_order.pop_front() {
                self.count_cache.remove(&oldest_key);
            }
        }
    }

    fn insert_field_cache(&mut self, key: String, entry: FieldCacheEntry) {
        self.field_cache.insert(key.clone(), entry);
        self.field_cache_order.push_back(key.clone());
        while self.field_cache.len() > MAX_FIELD_CACHE_SIZE {
            if let Some(oldest_key) = self.field_cache_order.pop_front() {
                self.field_cache.remove(&oldest_key);
            }
        }
    }

    fn cache_documents(&mut self, docs: Vec<Document>, column_names: Vec<String>) -> u64 {
        let session_id = self.next_document_session_id;
        self.next_document_session_id += 1;
        let estimated_bytes: usize = docs
            .iter()
            .map(|doc| bson::to_vec(doc).map(|bytes| bytes.len()).unwrap_or(0))
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
        self.evict_document_sessions_until_within_budget();
        session_id
    }

    fn get_cached_document(&self, session_id: u64, row_index: usize) -> Option<&Document> {
        self.document_cache
            .get(&session_id)
            .and_then(|entry| entry.docs.get(row_index))
    }

    fn remove_document_session(&mut self, session_id: u64) {
        if let Some(entry) = self.document_cache.remove(&session_id) {
            self.document_cache_bytes = self
                .document_cache_bytes
                .saturating_sub(entry.estimated_bytes);
        }
        self.document_cache_order.retain(|id| *id != session_id);
        self.evict_field_cache_for_session(session_id);
    }

    fn evict_field_cache_for_session(&mut self, session_id: u64) {
        let prefix = format!("fields_row:{}:", session_id);
        let keys_to_remove: Vec<String> = self
            .field_cache
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys_to_remove {
            self.field_cache.remove(&key);
            self.field_cache_order.retain(|existing| existing != &key);
        }
    }

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
}

#[derive(serde::Serialize)]
pub struct ServerInfoResponse {
    version: String,
    git_version: String,
    engine: String,
    host: String,
}

pub fn validate_mongo_uri(uri: &str) -> Result<(), String> {
    let scheme = uri.split_once("://").map(|(scheme, _)| scheme);
    if scheme.is_some_and(|scheme| {
        scheme.eq_ignore_ascii_case("mongodb") || scheme.eq_ignore_ascii_case("mongodb+srv")
    }) {
        return Ok(());
    }
    Err("MongoDB driver requires a mongodb:// or mongodb+srv:// URI".to_string())
}

pub fn capabilities_json() -> String {
    serde_json::json!({
        "abi_version": ABI_VERSION,
        "driver": "mongodb",
        "display_name": "MongoDB",
        "uri_schemes": ["mongodb", "mongodb+srv"],
        "features": [
            "queries",
            "aggregates",
            "scripts",
            "bson_rendering",
            "document_cache",
            "autocomplete",
            "arrow_results"
        ],
        "dependencies": [
            {
                "id": "arrow_ffi",
                "display_name": "Arrow FFI",
                "kind": "bundled",
                "state": "installed",
                "required": true,
                "features": ["arrow_results"]
            },
            {
                "id": "completion",
                "display_name": "Completion Catalog",
                "kind": "bundled",
                "state": "installed",
                "required": false,
                "features": ["autocomplete"]
            },
            {
                "id": "tree_sitter_javascript",
                "display_name": "Tree-sitter JavaScript",
                "kind": "bundled",
                "state": "installed",
                "required": false,
                "features": ["script_highlighting"]
            }
        ]
    })
    .to_string()
}

fn run_with_timeout<F, T>(future: F) -> Option<T>
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    RUNTIME.block_on(async move {
        tokio::time::timeout(Duration::from_secs(OPERATION_TIMEOUT_SECS), future)
            .await
            .ok()
    })
}

fn c_string(ptr: *const c_char, name: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("null {name}"));
    }
    Ok(unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() })
}

fn simple_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn alloc_json(contents: &str) -> *mut c_char {
    engine_ffi_util::string_to_c_ptr(contents)
}

fn validate_error_envelope(message: &str) -> String {
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{{\"valid\":false,\"error\":\"{}\"}}", escaped)
}

fn parsed_fields_to_carray(fields: &[field_parser::ParsedField]) -> *mut CFieldInfoArray {
    let c_fields: Vec<CFieldInfo> = fields
        .iter()
        .map(|field| CFieldInfo {
            key: CString::new(field.key.as_str())
                .unwrap_or_default()
                .into_raw(),
            field_type: CString::new(field.field_type.as_str())
                .unwrap_or_default()
                .into_raw(),
            display_value: CString::new(field.display_value.as_str())
                .unwrap_or_default()
                .into_raw(),
            is_primary_key: field.is_primary_key,
        })
        .collect();
    let count = c_fields.len();
    let fields_ptr = c_fields.as_ptr() as *mut CFieldInfo;
    std::mem::forget(c_fields);
    Box::into_raw(Box::new(CFieldInfoArray {
        fields: fields_ptr,
        count,
    }))
}

#[cfg(not(feature = "abi-v2"))]
#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_abi_version() -> u32 {
    ABI_VERSION
}

#[cfg(not(feature = "abi-v2"))]
#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_capabilities() -> *mut c_char {
    engine_ffi_util::string_to_c_ptr(capabilities_json())
}

#[cfg(not(feature = "abi-v2"))]
#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_init() -> *mut c_char {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    success_json(&serde_json::json!({ "ok": true }))
}

#[cfg(not(feature = "abi-v2"))]
#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_shutdown() -> *mut c_char {
    if let Ok(mut engine) = ENGINE.write() {
        *engine = EngineState::new();
    }
    success_json(&serde_json::json!({ "ok": true }))
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_is_loaded() -> bool {
    true
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_connect(uri: *const c_char, name: *const c_char) -> *mut c_char {
    let uri_str = match c_string(uri, "uri") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let name_str = match c_string(name, "name") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    if let Err(error) = validate_mongo_uri(&uri_str) {
        return error_json(&error);
    }

    let client_result = run_with_timeout({
        let uri = uri_str.clone();
        async move {
            let options = mongodb::options::ClientOptions::parse(&uri)
                .await
                .map_err(|error| error::DbEngineError::Connection(error.to_string()))?;
            let client = Arc::new(
                mongodb::Client::with_options(options)
                    .map_err(|error| error::DbEngineError::Connection(error.to_string()))?,
            );
            client
                .list_database_names()
                .await
                .map_err(|error| error::DbEngineError::Connection(error.to_string()))?;
            Ok::<_, error::DbEngineError>(client)
        }
    });

    let client = match client_result {
        Some(Ok(client)) => client,
        Some(Err(error)) => return error_json(&error.to_string()),
        None => return error_json("MongoDB connection timed out"),
    };

    match ENGINE.write() {
        Ok(mut engine) => {
            let id = engine.manager.insert_client(uri_str, name_str, client);
            success_json(&serde_json::json!({ "id": id }))
        }
        Err(error) => error_json(&format!("Engine lock poisoned: {error}")),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_disconnect(conn_id: u64) -> *mut c_char {
    match ENGINE.write() {
        Ok(mut engine) => match engine.manager.remove_client(conn_id) {
            Ok(()) => success_json(&serde_json::json!({ "ok": true })),
            Err(error) => error_json(&error.to_string()),
        },
        Err(error) => error_json(&format!("Engine lock poisoned: {error}")),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_cancel_operation(_conn_id: u64) -> *mut c_char {
    match ENGINE.read() {
        Ok(engine) => {
            engine.cancel_flag.store(true, Ordering::Relaxed);
            success_json(&serde_json::json!({ "cancelled": true }))
        }
        Err(error) => error_json(&format!("Engine lock poisoned: {error}")),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_list_databases(conn_id: u64) -> *mut c_char {
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };
    match run_with_timeout(async move { client.list_database_names().await }) {
        Some(Ok(databases)) => success_json(&databases),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_list_collections(conn_id: u64, db: *const c_char) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };
    match run_with_timeout(async move { client.database(&db_str).list_collection_names().await }) {
        Some(Ok(collections)) => success_json(&collections),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_execute_query(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    query_json: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let query_str = match c_string(query_json, "query_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };

    let result = run_with_timeout(async move {
        let filter: Document = serde_json::from_str(&query_str)
            .map_err(|error| error::DbEngineError::Query(format!("invalid query JSON: {error}")))?;
        let cursor = client
            .database(&db_str)
            .collection::<Document>(&coll_str)
            .find(filter)
            .limit(200)
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        let docs: Vec<Document> = cursor
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(Result::ok)
            .collect();
        serde_json::to_string(&docs)
            .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
    });

    match result {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
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
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let query_str = match c_string(query_json, "query_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let cursor_str = optional_c_string(cursor_json);
    let project = optional_document(project_json, true);
    let sort = optional_document(sort_json, true);
    let collation_locale = optional_document(collation_json, true)
        .and_then(|doc| doc.get_str("locale").ok().map(str::to_string));
    let index_hint = optional_index_hint(index_hint_json);
    let effective_limit = if limit > 0 { limit } else { 200 };
    let effective_skip = (skip > 0).then_some(skip as u64);
    let effective_max_time = (max_time_ms > 0).then_some(Duration::from_millis(max_time_ms as u64));
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };

    let result = run_with_timeout(async move {
        let filter: Document =
            query_parser::parse_query(&query_str).map_err(error::DbEngineError::Query)?;
        let mut query_filter = filter.clone();
        if let Some(cursor_value_json) = &cursor_str {
            let cursor_value: bson::Bson =
                serde_json::from_str(cursor_value_json).map_err(|error| {
                    error::DbEngineError::Query(format!("invalid cursor JSON: {error}"))
                })?;
            query_filter.insert("_id", bson::doc! { "$gt": cursor_value });
        }

        let mut opts = mongodb::options::FindOptions::builder()
            .limit(effective_limit)
            .build();
        opts.sort = sort.or_else(|| Some(bson::doc! { "_id": 1 }));
        opts.projection = project;
        opts.skip = effective_skip;
        opts.max_time = effective_max_time;
        if let Some(locale) = collation_locale {
            opts.collation = Some(
                mongodb::options::Collation::builder()
                    .locale(locale)
                    .build(),
            );
        }
        opts.hint = index_hint;

        let collection_ref = client.database(&db_str).collection::<Document>(&coll_str);
        let cache_key = EngineState::cache_key(&db_str, &coll_str, &query_str);
        let cached_count = ENGINE
            .read()
            .ok()
            .and_then(|engine| {
                engine
                    .count_cache
                    .get(&cache_key)
                    .map(|entry| (entry.filter.clone(), entry.count))
            })
            .and_then(|(cached_filter, count)| (cached_filter == filter).then_some(count));

        let (docs_result, count_result) = tokio::join!(
            async {
                let cursor = collection_ref
                    .find(query_filter)
                    .with_options(opts)
                    .await
                    .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
                let docs: Vec<Document> = cursor
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .filter_map(Result::ok)
                    .collect();
                Ok::<_, error::DbEngineError>(docs)
            },
            async {
                if let Some(count) = cached_count {
                    Ok::<_, error::DbEngineError>((count, true))
                } else {
                    let count = collection_ref
                        .estimated_document_count()
                        .await
                        .map(|value| value as usize)
                        .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
                    if let Ok(mut engine) = ENGINE.write() {
                        engine.insert_count_cache(
                            cache_key,
                            CountCacheEntry {
                                filter: filter.clone(),
                                count,
                            },
                        );
                    }
                    Ok((count, true))
                }
            }
        );

        let docs = docs_result?;
        let (total_count, is_estimated) = count_result.unwrap_or((0, false));
        let last_id = docs
            .last()
            .and_then(|doc| doc.get("_id"))
            .and_then(|id| serde_json::to_string(id).ok());
        let mut response = arrow::documents_to_arrow_cdata(&docs)
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        let column_names = response.column_names.clone();
        let session_id = ENGINE
            .write()
            .map_err(|error| error::DbEngineError::Query(format!("Engine lock poisoned: {error}")))?
            .cache_documents(docs, column_names);
        response.total_count = Some(total_count);
        response.last_id = last_id;
        response.is_estimated_count = is_estimated;
        response.session_id = session_id;
        Ok::<_, error::DbEngineError>(response)
    });

    match result {
        Some(Ok(arrow_result)) => success_json(&arrow::ArrowCDataResponse::from(arrow_result)),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_export_table_data(
    conn_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    let request_str = match c_string(request_json, "request_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let request = match serde_json::from_str::<ExportTableRequest>(&request_str) {
        Ok(value) => value,
        Err(error) => return error_json(&format!("invalid export request: {error}")),
    };
    let format = match export::ExportFormat::parse(&request.format) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };

    let result = run_with_timeout(async move {
        let filter = query_parser::parse_query(request.filter_json.as_deref().unwrap_or("{}"))
            .map_err(error::DbEngineError::Query)?;
        let project = request
            .project_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<Document>(value).ok());
        let sort = request
            .sort_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<Document>(value).ok());
        let batch_size = request.batch_size.unwrap_or(10_000).clamp(1, 100_000);
        let start_skip = request.skip.unwrap_or(0).max(0) as u64;
        let total_limit = request.limit.unwrap_or(0).max(0);
        let collection_ref = client
            .database(&request.database)
            .collection::<Document>(&request.collection);

        let mut exported = 0usize;
        let mut offset = start_skip;
        let mut exporter: Option<export::TableExporter> = None;
        loop {
            let remaining_limit = if total_limit > 0 {
                let remaining = total_limit.saturating_sub(exported as i64);
                if remaining <= 0 {
                    break;
                }
                remaining.min(batch_size)
            } else {
                batch_size
            };
            let mut opts = mongodb::options::FindOptions::builder()
                .limit(remaining_limit)
                .skip(offset)
                .build();
            opts.sort = sort.clone().or_else(|| Some(bson::doc! { "_id": 1 }));
            opts.projection = project.clone();
            let cursor = collection_ref
                .find(filter.clone())
                .with_options(opts)
                .await
                .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
            let docs: Vec<Document> = cursor
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .filter_map(Result::ok)
                .collect();
            if exporter.is_none() {
                let columns = export_columns(&docs);
                exporter = Some(
                    export::TableExporter::create(
                        &request.output_path,
                        format,
                        export::ExportDialect::Mongo,
                        request.collection.clone(),
                        columns,
                    )
                    .map_err(error::DbEngineError::Query)?,
                );
            }
            let columns = exporter
                .as_ref()
                .map(|e| e.columns().to_vec())
                .unwrap_or_default();
            let rows = docs
                .iter()
                .map(|doc| mongo_export_row(doc, &columns))
                .collect::<Vec<_>>();
            if let Some(writer) = exporter.as_mut() {
                writer
                    .write_rows(&rows)
                    .map_err(error::DbEngineError::Query)?;
            }
            exported += docs.len();
            if docs.len() < remaining_limit as usize {
                break;
            }
            offset += docs.len() as u64;
        }
        let rows_exported = match exporter {
            Some(writer) => writer.finish().map_err(error::DbEngineError::Query)?,
            None => {
                let writer = export::TableExporter::create(
                    &request.output_path,
                    format,
                    export::ExportDialect::Mongo,
                    request.collection.clone(),
                    Vec::new(),
                )
                .map_err(error::DbEngineError::Query)?;
                writer.finish().map_err(error::DbEngineError::Query)?
            }
        };
        Ok::<_, error::DbEngineError>(export::ExportResult {
            ok: true,
            rows_exported,
            path: request.output_path,
        })
    });

    match result {
        Some(Ok(response)) => success_json(&response),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

fn export_columns(docs: &[Document]) -> Vec<String> {
    let mut columns = Vec::new();
    for doc in docs {
        for key in doc.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    columns
}

fn mongo_export_row(doc: &Document, columns: &[String]) -> Vec<String> {
    columns
        .iter()
        .map(|column| doc.get(column).map(bson_export_value).unwrap_or_default())
        .collect()
}

fn bson_export_value(value: &bson::Bson) -> String {
    match value {
        bson::Bson::Null => "null".to_string(),
        bson::Bson::String(value) => value.clone(),
        bson::Bson::Boolean(value) => value.to_string(),
        bson::Bson::Int32(value) => value.to_string(),
        bson::Bson::Int64(value) => value.to_string(),
        bson::Bson::Double(value) => value.to_string(),
        bson::Bson::ObjectId(value) => value.to_hex(),
        bson::Bson::DateTime(value) => value.to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_execute_aggregate_arrow(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    pipeline_json: *const c_char,
    limit: i64,
    max_time_ms: i64,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let pipeline_str = match c_string(pipeline_json, "pipeline_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let mut pipeline: Vec<Document> = match json5::from_str(&pipeline_str) {
        Ok(value) => value,
        Err(error) => return error_json(&format!("Invalid pipeline JSON: {error}")),
    };
    if !pipeline.iter().any(|stage| stage.contains_key("$limit")) {
        pipeline.push(bson::doc! { "$limit": if limit > 0 { limit } else { 200 } });
    }
    let effective_max_time = (max_time_ms > 0).then_some(Duration::from_millis(max_time_ms as u64));
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };

    let result = run_with_timeout(async move {
        let mut opts = mongodb::options::AggregateOptions::default();
        opts.max_time = effective_max_time;
        let cursor = client
            .database(&db_str)
            .collection::<Document>(&coll_str)
            .aggregate(pipeline)
            .with_options(opts)
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        let docs: Vec<Document> = cursor
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(Result::ok)
            .collect();
        let mut response = arrow::documents_to_arrow_cdata(&docs)
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        let column_names = response.column_names.clone();
        let session_id = ENGINE
            .write()
            .map_err(|error| error::DbEngineError::Query(format!("Engine lock poisoned: {error}")))?
            .cache_documents(docs, column_names);
        response.total_count = Some(response.row_count);
        response.is_estimated_count = false;
        response.session_id = session_id;
        Ok::<_, error::DbEngineError>(response)
    });

    match result {
        Some(Ok(arrow_result)) => success_json(&arrow::ArrowCDataResponse::from(arrow_result)),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_execute_mongo_script(
    conn_id: u64,
    db: *const c_char,
    script: *const c_char,
    max_time_ms: i64,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let script_str = match c_string(script, "script") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let plan = match mongo_script_parser::parse_script(&script_str) {
        Ok(ScriptPlan::Level1(level1)) => level1.to_pipeline(&db_str),
        Ok(ScriptPlan::Level2(pipeline)) => pipeline,
        Ok(ScriptPlan::Level3Define(definition)) => {
            let name = definition.name.clone();
            if let Ok(mut engine) = ENGINE.write() {
                engine.pipeline_registry.insert(name.clone(), definition);
                return success_json(
                    &serde_json::json!({ "ok": 1, "n": 0, "message": format!("Pipeline '{name}' defined") }),
                );
            }
            return error_json("Engine lock poisoned");
        }
        Ok(ScriptPlan::Level3Run { name, params }) => {
            let definition = match ENGINE
                .read()
                .ok()
                .and_then(|engine| engine.pipeline_registry.get(&name).cloned())
            {
                Some(definition) => definition,
                None => return error_json(&format!("Pipeline '{name}' not defined")),
            };
            let mut resolved_stages = definition.stages.clone();
            for stage in resolved_stages.iter_mut() {
                pipeline::apply_param_substitution(stage, &definition.default_params, &params);
            }
            pipeline::PipelinePlan {
                stages: resolved_stages,
            }
        }
        Err(error) => {
            let preview: String = script_str.chars().take(500).collect();
            return error_json(&format!(
                "Parse error: {error}. script_bytes={}, script_preview={:?}",
                script_str.len(),
                preview
            ));
        }
    };
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };
    match run_with_timeout(async move {
        pipeline::execute_pipeline(&client, &db_str, plan, max_time_ms).await
    }) {
        Some(Ok(arrow_result)) => success_json(&arrow::ArrowCDataResponse::from(arrow_result)),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_load_fixture_arrow(path: *const c_char, count: i64) -> *mut c_char {
    let path_str = match c_string(path, "path") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let docs = match arrow::load_documents_from_bson(&path_str, count as usize) {
        Ok(docs) => docs,
        Err(error) => return error_json(&error.to_string()),
    };
    if docs.is_empty() {
        return error_json("No documents loaded from fixture");
    }
    match arrow::documents_to_arrow_cdata(&docs) {
        Ok(result) => success_json(&arrow::ArrowCDataResponse::from(result)),
        Err(error) => error_json(&error.to_string()),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_explain_query(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    query_json: *const c_char,
    project_json: *const c_char,
    sort_json: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let query_str = match c_string(query_json, "query_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let project = optional_document(project_json, true);
    let sort = optional_document(sort_json, true);
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };
    match run_with_timeout(async move {
        let filter: Document =
            query_parser::parse_query(&query_str).map_err(error::DbEngineError::Query)?;
        let mut find_cmd = bson::doc! { "find": &coll_str, "filter": filter };
        if let Some(project) = project {
            find_cmd.insert("projection", project);
        }
        if let Some(sort) = sort {
            find_cmd.insert("sort", sort);
        }
        let explain_result = client
            .database(&db_str)
            .run_command(bson::doc! { "explain": find_cmd, "verbosity": "queryPlanner" })
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        serde_json::to_string(&explain_result)
            .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
    }) {
        Some(Ok(plan)) => engine_ffi_util::string_to_c_ptr(plan),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_explain_aggregate(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    pipeline_json: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let pipeline_str = match c_string(pipeline_json, "pipeline_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let pipeline: Vec<Document> = match json5::from_str(&pipeline_str) {
        Ok(value) => value,
        Err(error) => return error_json(&format!("Invalid pipeline JSON: {error}")),
    };
    let client = match ENGINE.read() {
        Ok(engine) => match engine.manager.get_client(conn_id) {
            Ok(client) => client,
            Err(error) => return error_json(&error.to_string()),
        },
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };
    match run_with_timeout(async move {
        let explain_result = client
            .database(&db_str)
            .run_command(bson::doc! { "explain": { "aggregate": coll_str, "pipeline": pipeline, "cursor": {} }, "verbosity": "queryPlanner" })
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        serde_json::to_string(&explain_result)
            .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
    }) {
        Some(Ok(plan)) => engine_ffi_util::string_to_c_ptr(plan),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_validate_aggregate_pipeline(
    pipeline_json: *const c_char,
) -> *mut c_char {
    let text = match c_string(pipeline_json, "pipeline_json") {
        Ok(value) => value,
        Err(error) => return alloc_json(&validate_error_envelope(&error)),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return alloc_json("{\"valid\":false,\"error\":\"Pipeline is empty. Start with [ and add stage objects.\"}");
    }
    let stages: Vec<Document> = match json5::from_str(trimmed) {
        Ok(stages) => stages,
        Err(error) => {
            return alloc_json(&validate_error_envelope(&format!("Invalid JSON: {error}")))
        }
    };
    for (index, stage) in stages.iter().enumerate() {
        if stage.is_empty() {
            return alloc_json(&validate_error_envelope(&format!(
                "Stage {} is empty.",
                index + 1
            )));
        }
        if stage.len() > 1 {
            return alloc_json(&validate_error_envelope(&format!(
                "Stage {} has {} keys; each stage must be a single-key object like {{ \"$match\": ... }}.",
                index + 1,
                stage.len()
            )));
        }
        if let Some(key) = stage.keys().next() {
            if !key.starts_with('$') {
                return alloc_json(&validate_error_envelope(&format!(
                    "Stage {} key '{}' must start with '$'.",
                    index + 1,
                    key
                )));
            }
        }
    }
    alloc_json("{\"valid\":true}")
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_normalize_aggregate_pipeline(
    pipeline_json: *const c_char,
) -> *mut c_char {
    let text = match c_string(pipeline_json, "pipeline_json") {
        Ok(value) => value,
        Err(_) => return std::ptr::null_mut(),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return engine_ffi_util::string_to_c_ptr(text);
    }
    let stages: Vec<Document> = match json5::from_str(trimmed) {
        Ok(stages) => stages,
        Err(_) => return engine_ffi_util::string_to_c_ptr(text),
    };
    let values: Vec<serde_json::Value> = stages
        .iter()
        .map(
            |stage| match bson::Bson::Document(stage.clone()).into_relaxed_extjson() {
                serde_json::Value::Object(map) => serde_json::Value::Object(map),
                other => other,
            },
        )
        .collect();
    match serde_json::to_string_pretty(&values) {
        Ok(pretty) => engine_ffi_util::string_to_c_ptr(pretty),
        Err(_) => engine_ffi_util::string_to_c_ptr(text),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_insert_one(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    document_json: *const c_char,
) -> *mut c_char {
    mutation_with_document(
        conn_id,
        db,
        collection,
        document_json,
        |coll, doc| async move {
            let result = coll
                .insert_one(doc)
                .await
                .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
            serde_json::to_string(&result.inserted_id)
                .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
        },
    )
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_update_one(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    filter_json: *const c_char,
    update_json: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let filter_str = match c_string(filter_json, "filter_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let update_str = match c_string(update_json, "update_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match mongo_client(conn_id) {
        Ok(client) => client,
        Err(error) => return error_json(&error),
    };
    let db_log = db_str.clone();
    let coll_log = coll_str.clone();
    match run_with_timeout(async move {
        let filter = query_parser::parse_query(&filter_str)
            .map_err(|error| error::DbEngineError::Query(format!("invalid filter: {error}")))?;
        let update: Document = serde_json::from_str(&update_str).map_err(|error| {
            error::DbEngineError::Query(format!("invalid update JSON: {error}"))
        })?;
        let result = client
            .database(&db_str)
            .collection::<Document>(&coll_str)
            .update_one(filter, update)
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        serde_json::to_string(&serde_json::json!({ "matched": result.matched_count, "modified": result.modified_count }))
            .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
    }) {
        Some(Ok(json)) => {
            invalidate_collection(&db_log, &coll_log);
            engine_ffi_util::string_to_c_ptr(json)
        }
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_batch_update(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    updates_json: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let updates_str = match c_string(updates_json, "updates_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match mongo_client(conn_id) {
        Ok(client) => client,
        Err(error) => return error_json(&error),
    };
    let db_log = db_str.clone();
    let coll_log = coll_str.clone();
    match run_with_timeout(async move {
        let updates: Vec<serde_json::Value> =
            serde_json::from_str(&updates_str).map_err(|error| {
                error::DbEngineError::Query(format!("invalid updates JSON: {error}"))
            })?;
        let coll = client.database(&db_str).collection::<Document>(&coll_str);
        let mut total_updated = 0;
        for update_entry in updates {
            let filter_str = update_entry
                .get("filter")
                .and_then(|value| value.as_str())
                .ok_or_else(|| error::DbEngineError::Query("missing filter".to_string()))?;
            let update_str = update_entry
                .get("update")
                .and_then(|value| value.as_str())
                .ok_or_else(|| error::DbEngineError::Query("missing update".to_string()))?;
            let filter = query_parser::parse_query(filter_str)
                .map_err(|error| error::DbEngineError::Query(format!("invalid filter: {error}")))?;
            let update = query_parser::parse_query(update_str)
                .map_err(|error| error::DbEngineError::Query(format!("invalid update: {error}")))?;
            let result = coll
                .update_one(filter, update)
                .await
                .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
            total_updated += result.modified_count;
        }
        serde_json::to_string(&serde_json::json!({ "success": true, "updated": total_updated }))
            .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
    }) {
        Some(Ok(json)) => {
            invalidate_collection(&db_log, &coll_log);
            engine_ffi_util::string_to_c_ptr(json)
        }
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_delete_one(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    filter_json: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let filter_str = match c_string(filter_json, "filter_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match mongo_client(conn_id) {
        Ok(client) => client,
        Err(error) => return error_json(&error),
    };
    let db_log = db_str.clone();
    let coll_log = coll_str.clone();
    match run_with_timeout(async move {
        let filter = query_parser::parse_query(&filter_str)
            .map_err(|error| error::DbEngineError::Query(format!("invalid filter: {error}")))?;
        let result = client
            .database(&db_str)
            .collection::<Document>(&coll_str)
            .delete_one(filter)
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        serde_json::to_string(&serde_json::json!({ "deleted": result.deleted_count }))
            .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
    }) {
        Some(Ok(json)) => {
            invalidate_collection(&db_log, &coll_log);
            engine_ffi_util::string_to_c_ptr(json)
        }
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_get_document_json(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    id_json: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let id_str = match c_string(id_json, "id_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match mongo_client(conn_id) {
        Ok(client) => client,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        let id_value: bson::Bson = serde_json::from_str(&id_str)
            .map_err(|error| error::DbEngineError::Query(format!("invalid _id JSON: {error}")))?;
        let doc = client
            .database(&db_str)
            .collection::<Document>(&coll_str)
            .find_one(bson::doc! { "_id": id_value })
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        Ok::<_, error::DbEngineError>(
            doc.and_then(|doc| serde_json::to_string(&doc).ok())
                .unwrap_or_else(|| "null".to_string()),
        )
    }) {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_get_collection_info(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
) -> *mut c_char {
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match mongo_client(conn_id) {
        Ok(client) => client,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        let coll = client.database(&db_str).collection::<Document>(&coll_str);
        let mut index_cursor = coll
            .list_indexes()
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        let mut indexes = Vec::new();
        while let Some(result) = index_cursor.next().await {
            if let Ok(index_model) = result {
                indexes.push(serde_json::json!({
                    "name": index_model.options.as_ref().and_then(|opts| opts.name.clone()).unwrap_or_default(),
                    "key": index_model.keys.to_string(),
                    "unique": index_model.options.as_ref().and_then(|opts| opts.unique).unwrap_or(false)
                }));
            }
        }
        let count = coll.estimated_document_count().await.unwrap_or(0);
        let mut sample_cursor = coll
            .find(Document::new())
            .limit(100)
            .await
            .map_err(|error| error::DbEngineError::Query(error.to_string()))?;
        let mut schema = HashMap::new();
        while let Some(result) = sample_cursor.next().await {
            if let Ok(doc) = result {
                for (key, value) in doc {
                    schema
                        .entry(key)
                        .or_insert_with(|| bson_type_name(&value).to_string());
                }
            }
        }
        serde_json::to_string(
            &serde_json::json!({ "indexes": indexes, "sample_schema": schema, "count": count }),
        )
        .map_err(|error| error::DbEngineError::Serialization(error.to_string()))
    }) {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_server_info(conn_id: u64) -> *mut c_char {
    let client = match mongo_client(conn_id) {
        Ok(client) => client,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        let result_doc = client
            .database("admin")
            .run_command(bson::doc! { "buildInfo": 1 })
            .await
            .map_err(|error| error::DbEngineError::MongoDB(error.to_string()))?;
        Ok::<_, error::DbEngineError>(ServerInfoResponse {
            version: result_doc
                .get_str("version")
                .unwrap_or("unknown")
                .to_string(),
            git_version: result_doc.get_str("gitVersion").unwrap_or("").to_string(),
            engine: result_doc.get_str("engine").unwrap_or("").to_string(),
            host: result_doc.get_str("host").unwrap_or("").to_string(),
        })
    }) {
        Some(Ok(info)) => success_json(&info),
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_get_document_fields(json: *const c_char) -> *mut CFieldInfoArray {
    let json_str = match c_string(json, "json") {
        Ok(value) => value,
        Err(_) => {
            return Box::into_raw(Box::new(CFieldInfoArray {
                fields: std::ptr::null_mut(),
                count: 0,
            }))
        }
    };
    let cache_key = format!("fields:{}", simple_hash(&json_str));
    if let Ok(state) = ENGINE.read() {
        if let Some(entry) = state.field_cache.get(&cache_key) {
            return parsed_fields_to_carray(&entry.fields);
        }
    }
    let parsed = match field_parser::parse_json_fields(&json_str) {
        Some(fields) => fields,
        None => {
            return Box::into_raw(Box::new(CFieldInfoArray {
                fields: std::ptr::null_mut(),
                count: 0,
            }))
        }
    };
    if let Ok(mut state) = ENGINE.write() {
        state.insert_field_cache(
            cache_key,
            FieldCacheEntry {
                fields: parsed.clone(),
            },
        );
    }
    parsed_fields_to_carray(&parsed)
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_get_document_fields_by_row(
    session_id: u64,
    row_index: usize,
) -> *mut CFieldInfoArray {
    let cache_key = format!("fields_row:{}:{}", session_id, row_index);
    if let Ok(state) = ENGINE.read() {
        if let Some(entry) = state.field_cache.get(&cache_key) {
            return parsed_fields_to_carray(&entry.fields);
        }
    }
    let parsed = match ENGINE
        .read()
        .ok()
        .and_then(|state| state.get_cached_document(session_id, row_index).cloned())
    {
        Some(doc) => field_parser::parse_document_fields(&doc),
        None => {
            return Box::into_raw(Box::new(CFieldInfoArray {
                fields: std::ptr::null_mut(),
                count: 0,
            }))
        }
    };
    if let Ok(mut state) = ENGINE.write() {
        state.insert_field_cache(
            cache_key,
            FieldCacheEntry {
                fields: parsed.clone(),
            },
        );
    }
    parsed_fields_to_carray(&parsed)
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_invalidate_document_session(session_id: u64) {
    if let Ok(mut state) = ENGINE.write() {
        state.remove_document_session(session_id);
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_render_cell_text_batch(
    session_id: u64,
    row_start: usize,
    row_end_exclusive: usize,
    col_start: usize,
    col_end_exclusive: usize,
    max_bytes_per_cell: usize,
) -> *mut CRenderedCellBatch {
    let Some((docs, column_names)) = ENGINE.read().ok().and_then(|state| {
        state
            .document_cache
            .get(&session_id)
            .map(|entry| (entry.docs.clone(), entry.column_names.clone()))
    }) else {
        return std::ptr::null_mut();
    };
    let batch = render::render_cell_text_batch(
        &docs,
        &column_names,
        row_start,
        row_end_exclusive,
        col_start,
        col_end_exclusive,
        max_bytes_per_cell,
    );
    let c_cells: Box<[CRenderedCell]> = batch
        .cells
        .into_iter()
        .map(|cell| CRenderedCell {
            row: cell.row,
            column: cell.column,
            offset: cell.offset,
            length: cell.length,
            flags: cell.flags,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let cell_count = c_cells.len();
    let cells_ptr = c_cells.as_ptr() as *mut CRenderedCell;
    let _ = Box::into_raw(c_cells);
    let text_buffer: Box<[u8]> = batch.text_buffer.into_boxed_slice();
    let text_buffer_len = text_buffer.len();
    let text_buffer_ptr = text_buffer.as_ptr() as *mut u8;
    let _ = Box::into_raw(text_buffer);
    Box::into_raw(Box::new(CRenderedCellBatch {
        cells: cells_ptr,
        cell_count,
        text_buffer: text_buffer_ptr,
        text_buffer_len,
    }))
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_rendered_cell_batch(batch: *mut CRenderedCellBatch) {
    if batch.is_null() {
        return;
    }
    let batch_ref = unsafe { &*batch };
    if !batch_ref.cells.is_null() && batch_ref.cell_count > 0 {
        let slice_ptr = std::ptr::slice_from_raw_parts_mut(batch_ref.cells, batch_ref.cell_count);
        unsafe {
            let _ = Box::from_raw(slice_ptr);
        }
    }
    if !batch_ref.text_buffer.is_null() && batch_ref.text_buffer_len > 0 {
        let slice_ptr =
            std::ptr::slice_from_raw_parts_mut(batch_ref.text_buffer, batch_ref.text_buffer_len);
        unsafe {
            let _ = Box::from_raw(slice_ptr);
        }
    }
    unsafe {
        let _ = Box::from_raw(batch);
    }
}

#[cfg(not(feature = "abi-v2"))]
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
                engine_ffi_util::free_string(field.key);
                engine_ffi_util::free_string(field.field_type);
                engine_ffi_util::free_string(field.display_value);
            }
            let _ = Vec::from_raw_parts(arr_ref.fields, arr_ref.count, arr_ref.count);
        }
        let _ = Box::from_raw(arr);
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_clear_field_cache(collection: *const c_char) {
    if let Ok(mut state) = ENGINE.write() {
        if collection.is_null() {
            state.field_cache.clear();
            state.field_cache_order.clear();
        } else {
            let collection_str =
                unsafe { CStr::from_ptr(collection).to_string_lossy().into_owned() };
            state
                .field_cache
                .retain(|key, _| !key.contains(&format!(":{}:", collection_str)));
            state
                .field_cache_order
                .retain(|key| !key.contains(&format!(":{}:", collection_str)));
        }
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_clear_all_caches() {
    if let Ok(mut state) = ENGINE.write() {
        state.document_cache.clear();
        state.document_cache_order.clear();
        state.document_cache_bytes = 0;
        state.field_cache.clear();
        state.field_cache_order.clear();
        state.count_cache.clear();
        state.count_cache_order.clear();
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_get_completions(
    db_type: *const c_char,
    query_text: *const c_char,
    cursor_pos: i32,
    schema_json: *const c_char,
) -> *mut c_char {
    let db_type_str = match c_string(db_type, "db_type") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let query_str = match c_string(query_text, "query_text") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema_str = optional_c_string(schema_json).unwrap_or_else(|| "{}".to_string());
    let response =
        autocomplete::get_completions(&db_type_str, &query_str, cursor_pos as usize, &schema_str);
    success_json(&response)
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_update_schema_cache(
    db_type: *const c_char,
    collection: *const c_char,
    fields_json: *const c_char,
) {
    let Some(db_type_str) = optional_c_string(db_type) else {
        return;
    };
    let Some(collection_str) = optional_c_string(collection) else {
        return;
    };
    let Some(fields_str) = optional_c_string(fields_json) else {
        return;
    };
    if db_type_str == "mongodb" {
        if let Ok(schema) =
            serde_json::from_str::<std::collections::HashMap<String, String>>(&fields_str)
        {
            let fields = schema
                .into_iter()
                .map(|(name, data_type)| autocomplete::MongoFieldInfo {
                    name,
                    data_type: Some(data_type),
                })
                .collect();
            autocomplete::update_mongo_schema(&collection_str, fields);
        }
    }
}

#[cfg(not(feature = "abi-v2"))]
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

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
}

fn mongo_client(conn_id: u64) -> Result<Arc<mongodb::Client>, String> {
    ENGINE
        .read()
        .map_err(|error| format!("Engine lock poisoned: {error}"))?
        .manager
        .get_client(conn_id)
        .map_err(|error| error.to_string())
}

fn optional_c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
    (!value.is_empty()).then_some(value)
}

fn optional_document(ptr: *const c_char, empty_object_is_none: bool) -> Option<Document> {
    let value = optional_c_string(ptr)?;
    if empty_object_is_none && value == "{}" {
        return None;
    }
    json5::from_str(&value).ok()
}

fn optional_index_hint(ptr: *const c_char) -> Option<mongodb::options::Hint> {
    let value = optional_c_string(ptr)?;
    if value == "{}" {
        return None;
    }
    if let Ok(doc) = serde_json::from_str::<Document>(&value) {
        return Some(mongodb::options::Hint::Keys(doc));
    }
    serde_json::from_str::<String>(&value)
        .ok()
        .map(mongodb::options::Hint::Name)
}

fn invalidate_collection(db: &str, collection: &str) {
    if let Ok(mut engine) = ENGINE.write() {
        engine.invalidate_collection(db, collection);
    }
}

fn bson_type_name(value: &bson::Bson) -> &'static str {
    match value {
        bson::Bson::Int32(_) => "int32",
        bson::Bson::Int64(_) => "int64",
        bson::Bson::Double(_) => "double",
        bson::Bson::Boolean(_) => "boolean",
        bson::Bson::String(_) => "string",
        bson::Bson::Array(_) => "array",
        bson::Bson::Document(doc) if arrow::is_extended_json_date(doc) => "datetime",
        bson::Bson::Document(_) => "object",
        bson::Bson::ObjectId(_) => "objectId",
        bson::Bson::DateTime(_) => "datetime",
        _ => "unknown",
    }
}

fn mutation_with_document<F, Fut>(
    conn_id: u64,
    db: *const c_char,
    collection: *const c_char,
    document_json: *const c_char,
    mutate: F,
) -> *mut c_char
where
    F: FnOnce(mongodb::Collection<Document>, Document) -> Fut + Send,
    Fut: std::future::Future<Output = Result<String, error::DbEngineError>> + Send,
{
    let db_str = match c_string(db, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let coll_str = match c_string(collection, "collection") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let doc_str = match c_string(document_json, "document_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let client = match mongo_client(conn_id) {
        Ok(client) => client,
        Err(error) => return error_json(&error),
    };
    let db_log = db_str.clone();
    let coll_log = coll_str.clone();
    match run_with_timeout(async move {
        let doc: Document = serde_json::from_str(&doc_str).map_err(|error| {
            error::DbEngineError::Query(format!("invalid document JSON: {error}"))
        })?;
        mutate(
            client.database(&db_str).collection::<Document>(&coll_str),
            doc,
        )
        .await
    }) {
        Some(Ok(json)) => {
            invalidate_collection(&db_log, &coll_log);
            engine_ffi_util::string_to_c_ptr(json)
        }
        Some(Err(error)) => error_json(&error.to_string()),
        None => error_json("Operation timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_json_describes_mongo_driver() {
        let value: serde_json::Value = serde_json::from_str(&capabilities_json()).unwrap();

        assert_eq!(value["abi_version"], 2);
        assert_eq!(value["driver"], "mongodb");
        assert_eq!(value["display_name"], "MongoDB");
        assert!(value["uri_schemes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("mongodb")));
        assert!(value["features"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("document_cache")));
        assert!(value["features"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("arrow_results")));
        let deps = value["dependencies"]
            .as_array()
            .expect("dependencies array");
        assert!(deps.iter().any(|dep| dep["id"] == "completion"));
        assert!(deps.iter().any(|dep| dep["id"] == "tree_sitter_javascript"));
        assert!(deps
            .iter()
            .any(|dep| dep["id"] == "arrow_ffi" && dep["required"] == true));
    }

    #[test]
    fn abi_version_is_stable() {
        assert_eq!(ABI_VERSION, 2);
    }

    #[test]
    fn accepts_mongodb_uri_schemes() {
        assert!(validate_mongo_uri("mongodb://localhost:27017").is_ok());
        assert!(validate_mongo_uri("mongodb+srv://cluster.example.com").is_ok());
        assert!(validate_mongo_uri("MONGODB://localhost:27017").is_ok());
        assert!(validate_mongo_uri("MONGODB+SRV://cluster.example.com").is_ok());
    }

    #[test]
    fn rejects_postgres_uri_without_opening_network_connection() {
        let result = validate_mongo_uri("postgresql://localhost/tableleaf");

        assert!(result.is_err());
    }

    #[test]
    fn plugin_parser_accepts_bracket_find_with_isodate_range() {
        let plan = mongo_script_parser::parse_script(
            r#"db["sales-orders"].find({
               state: "PAID",
               createdAt: {
                  $gte: ISODate("2024-09-01T00:00:00.000Z"),
                  $lt: ISODate("2024-10-01T00:00:00.000Z")
               }
            })"#,
        )
        .expect("plugin parser should accept ChatGPT Mongo shell query");

        let ScriptPlan::Level1(level1) = plan else {
            panic!("expected level 1 Mongo shell query");
        };
        let pipeline = level1.to_pipeline("billing-service");
        let Some(pipeline::PipelineStage::Match { filter }) = pipeline
            .stages
            .iter()
            .find(|stage| matches!(stage, pipeline::PipelineStage::Match { .. }))
        else {
            panic!("expected match stage");
        };

        assert_eq!(filter.get_str("state").unwrap(), "PAID");
        let created_at = filter.get_document("createdAt").unwrap();
        assert_eq!(
            created_at.get_datetime("$gte").unwrap().timestamp_millis(),
            bson::DateTime::parse_rfc3339_str("2024-09-01T00:00:00.000Z")
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(
            created_at.get_datetime("$lt").unwrap().timestamp_millis(),
            bson::DateTime::parse_rfc3339_str("2024-10-01T00:00:00.000Z")
                .unwrap()
                .timestamp_millis()
        );
    }
}
