#[cfg(not(feature = "abi-v2"))]
use engine_ffi_util::{error_json, success_json};
#[cfg(not(feature = "abi-v2"))]
use std::collections::HashMap;
#[cfg(not(feature = "abi-v2"))]
use std::ffi::CStr;
#[cfg(not(feature = "abi-v2"))]
use std::os::raw::c_char;
#[cfg(not(feature = "abi-v2"))]
use std::sync::{Arc, RwLock};
#[cfg(not(feature = "abi-v2"))]
use std::time::Duration;
#[cfg(not(feature = "abi-v2"))]
use tracing::{error, info};

#[cfg(not(feature = "abi-v2"))]
use crate::db::DatabaseConnection;

#[cfg(not(feature = "abi-v2"))]
#[path = "../../dbengine/src/export.rs"]
mod export;

#[cfg(not(feature = "abi-v2"))]
pub const ABI_VERSION: u32 = 1;
#[cfg(not(feature = "abi-v2"))]
const OPERATION_TIMEOUT_SECS: u64 = 30;

#[cfg(not(feature = "abi-v2"))]
lazy_static::lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .thread_name("dbengine-postgres-worker")
        .build()
        .expect("Failed to create PostgreSQL driver tokio runtime");

    static ref ENGINE: RwLock<EngineState> = RwLock::new(EngineState::new());
}

#[cfg(not(feature = "abi-v2"))]
struct EngineState {
    connections: HashMap<u64, Arc<db::postgres::PostgresConn>>,
    next_conn_id: u64,
}

#[cfg(not(feature = "abi-v2"))]
#[derive(serde::Deserialize)]
#[cfg(not(feature = "abi-v2"))]
struct ExportTableRequest {
    format: String,
    output_path: String,
    database: String,
    schema: Option<String>,
    table: String,
    where_clause: Option<String>,
    order_by: Option<String>,
    limit: Option<i64>,
    batch_size: Option<i64>,
}

#[cfg(not(feature = "abi-v2"))]
impl EngineState {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
            next_conn_id: 1,
        }
    }
}

#[cfg(not(feature = "abi-v2"))]
pub mod arrow {
    use arrow::array::{RecordBatch, StructArray};
    use arrow::compute::concat_batches;
    use arrow::datatypes::DataType;
    use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
    use arrow_array::Array;
    use serde::Serialize;
    use std::ffi::CString;
    use std::os::raw::c_char;

    pub struct ArrowCDataResult {
        pub schema_ptr: *mut FFI_ArrowSchema,
        pub array_ptr: *mut FFI_ArrowArray,
        pub row_count: usize,
        pub col_count: usize,
        pub column_names: Vec<String>,
        pub column_types: Vec<String>,
        pub total_count: Option<usize>,
        pub cursor: *mut c_char,
    }

    unsafe impl Send for ArrowCDataResult {}

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

    pub fn record_batch_to_arrow_cdata(
        batches: Vec<RecordBatch>,
        total_count: Option<usize>,
        cursor_json: Option<String>,
    ) -> Result<ArrowCDataResult, String> {
        if batches.is_empty() {
            return Ok(ArrowCDataResult {
                schema_ptr: std::ptr::null_mut(),
                array_ptr: std::ptr::null_mut(),
                row_count: 0,
                col_count: 0,
                column_names: vec![],
                column_types: vec![],
                total_count,
                cursor: std::ptr::null_mut(),
            });
        }

        let single_batch = if batches.len() == 1 {
            batches.into_iter().next().expect("batch len checked")
        } else {
            let schema = batches[0].schema();
            concat_batches(&schema, &batches).map_err(|error| error.to_string())?
        };

        let schema = single_batch.schema();
        let row_count = single_batch.num_rows();
        let col_count = single_batch.num_columns();
        let column_names: Vec<String> = schema
            .fields()
            .iter()
            .map(|field| field.name().clone())
            .collect();
        let column_types: Vec<String> = schema
            .fields()
            .iter()
            .map(|field| normalize_arrow_type_str(field.data_type()))
            .collect();

        let struct_array = StructArray::from(single_batch);
        let ffi_schema =
            FFI_ArrowSchema::try_from(schema.as_ref()).map_err(|error| error.to_string())?;
        let schema_ptr = Box::into_raw(Box::new(ffi_schema));
        let ffi_array = FFI_ArrowArray::new(&struct_array.to_data());
        let array_ptr = Box::into_raw(Box::new(ffi_array));
        let cursor = match cursor_json {
            Some(cursor) => CString::new(cursor)
                .map_err(|error| error.to_string())?
                .into_raw(),
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
            cursor,
        })
    }

    fn normalize_arrow_type_str(data_type: &DataType) -> String {
        match data_type {
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
            DataType::Timestamp(_, _) | DataType::Date32 | DataType::Date64 => {
                "DateTime".to_string()
            }
            DataType::LargeBinary | DataType::Binary => "Utf8".to_string(),
            _ => "Utf8".to_string(),
        }
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
            Self {
                schema_ptr: result.schema_ptr as u64,
                array_ptr: result.array_ptr as u64,
                row_count: result.row_count,
                col_count: result.col_count,
                column_names: result.column_names,
                column_types: result.column_types,
                total_count: result.total_count,
                last_id: None,
                is_estimated_count: false,
                id_kind: "Unknown".to_string(),
                session_id: 0,
                large_field_report: LargeFieldReport::default(),
                cancelled: false,
                was_truncated: false,
                cursor: cursor_ptr_to_option(result.cursor),
            }
        }
    }
}

pub mod db {
    use arrow_array::RecordBatch;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CursorState {
        pub order_column: String,
        pub last_value: String,
        /// PostgreSQL type OID string, e.g. "int4", "int8", "text", "uuid".
        /// Used to emit the correct cast in SQL: `pk > $1::int8`.
        /// Empty string means no cast (string comparison).
        #[serde(default)]
        pub pk_type: String,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DbType {
        Postgres,
    }

    #[derive(Debug, Clone)]
    pub struct QueryResult {
        pub batches: Vec<RecordBatch>,
        pub total_count: i64,
        pub cursor: Option<CursorState>,
    }

    #[async_trait]
    pub trait DatabaseConnection: Send + Sync {
        fn db_type(&self) -> DbType;

        async fn test(&self) -> Result<(), String>;

        async fn list_databases(&self) -> Result<Vec<String>, String>;

        async fn list_collections(&self, db: &str) -> Result<Vec<String>, String>;

        async fn execute_query(
            &self,
            db: &str,
            table: &str,
            user_where: Option<&str>,
            user_order_by: Option<&str>,
            cursor: Option<&CursorState>,
            limit: i64,
        ) -> Result<QueryResult, String>;

        async fn cancel(&self);
    }

    // How `PostgresConn` reaches the server: the native engine's plain TCP, or the
    // v2 driver's TLS transport. Same API, so the shared source has one code path.
    #[cfg(not(feature = "abi-v2"))]
    pub mod pg_transport {
        include!("../../dbengine/src/db/pg_transport.rs");
    }
    #[cfg(feature = "abi-v2")]
    pub(crate) use crate::v2::tls as pg_transport;

    // The three modules below are verbatim shared sources (the native engine
    // compiles the same files); what one driver build does not call is not
    // dead code of this crate.
    #[allow(dead_code)]
    pub mod sql_builder {
        include!("../../dbengine/src/db/sql_builder.rs");
    }

    #[allow(dead_code)]
    pub mod postgres_arrow {
        include!("../../dbengine/src/db/postgres_arrow.rs");
    }

    #[allow(dead_code)]
    pub mod postgres {
        include!("../../dbengine/src/db/postgres.rs");
        // The v2 driver extends `PostgresConn` from inside its module because
        // the shared source keeps the per-database client pool private.
        #[cfg(feature = "abi-v2")]
        include!("v2/postgres_ext.rs");
    }
}

/// The error type the shared `PostgresConn` source expects at `crate::error`
/// (the native engine defines the full enum; only the variants that file
/// constructs are needed here).
pub mod error {
    #[derive(Debug)]
    pub enum DbEngineError {
        CreateRejected(String),
        Query(String),
    }

    impl std::fmt::Display for DbEngineError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::CreateRejected(message) => {
                    write!(f, "Create database rejected: {message}")
                }
                Self::Query(message) => write!(f, "Query error: {message}"),
            }
        }
    }

    impl std::error::Error for DbEngineError {}
}

#[cfg(feature = "abi-v2")]
mod v2;

#[cfg(feature = "abi-v2")]
engine_driver_kit::export_driver!(v2::PostgresBackend; cancel_operation, mutate, describe_object);

#[derive(serde::Serialize)]
pub struct ServerInfoResponse {
    version: String,
    git_version: String,
    engine: String,
    host: String,
}

pub mod sql_completion {
    include!("../../dbengine/src/sql_completion.rs");
}

#[cfg(not(feature = "abi-v2"))]
fn run_with_timeout<F, T>(future: F) -> Option<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    RUNTIME.block_on(async move {
        tokio::time::timeout(Duration::from_secs(OPERATION_TIMEOUT_SECS), future)
            .await
            .ok()
    })
}

#[cfg(not(feature = "abi-v2"))]
fn c_string(ptr: *const c_char, name: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("null {name} parameter"));
    }
    Ok(unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() })
}

#[cfg(not(feature = "abi-v2"))]
fn optional_c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(not(feature = "abi-v2"))]
fn postgres_conn(conn_id: u64) -> Result<Arc<db::postgres::PostgresConn>, String> {
    let engine = ENGINE
        .read()
        .map_err(|error| format!("Engine lock poisoned: {error}"))?;
    engine
        .connections
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| format!("No PostgreSQL connection for conn_id={conn_id}"))
}

pub fn validate_postgres_uri(uri: &str) -> Result<(), String> {
    if uri.starts_with("postgres://") || uri.starts_with("postgresql://") {
        return Ok(());
    }

    Err("PostgreSQL driver requires a postgres:// or postgresql:// URI".to_string())
}

#[cfg(not(feature = "abi-v2"))]
pub fn capabilities_json() -> String {
    serde_json::json!({
        "abi_version": ABI_VERSION,
        "driver": "postgresql",
        "display_name": "PostgreSQL",
        "uri_schemes": ["postgres", "postgresql"],
        "features": [
            "schemas",
            "sql_completion",
            "ddl_preview",
            "table_metadata",
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
                "display_name": "SQL Completion Catalog",
                "kind": "bundled",
                "state": "installed",
                "required": false,
                "features": ["sql_completion"]
            },
            {
                "id": "tree_sitter_sql",
                "display_name": "Tree-sitter SQL",
                "kind": "bundled",
                "state": "installed",
                "required": false,
                "features": ["sql_syntax"]
            }
        ]
    })
    .to_string()
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_abi_version() -> u32 {
    ABI_VERSION
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_capabilities() -> *mut c_char {
    engine_ffi_util::string_to_c_ptr(capabilities_json())
}

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
#[no_mangle]
pub extern "C" fn dbengine_shutdown() -> *mut c_char {
    if let Ok(mut engine) = ENGINE.write() {
        engine.connections.clear();
    }
    success_json(&serde_json::json!({ "ok": true }))
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_connect(uri: *const c_char, name: *const c_char) -> *mut c_char {
    let uri_str = match c_string(uri, "uri") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let _name_str = match c_string(name, "name") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };

    if let Err(error) = validate_postgres_uri(&uri_str) {
        return error_json(&error);
    }

    let result =
        run_with_timeout(async move { db::postgres::PostgresConn::connect(&uri_str).await });
    let conn = match result {
        Some(Ok(conn)) => conn,
        Some(Err(error)) => {
            error!(error = %error, "Postgres connection failed");
            return error_json(&error);
        }
        None => return error_json("Postgres connection timed out"),
    };

    let mut engine = match ENGINE.write() {
        Ok(engine) => engine,
        Err(error) => return error_json(&format!("Engine lock poisoned: {error}")),
    };
    let id = engine.next_conn_id;
    engine.next_conn_id += 1;
    engine.connections.insert(id, Arc::new(conn));
    info!(
        conn_id = id,
        "Postgres connection established via driver dylib"
    );
    success_json(&serde_json::json!({ "id": id }))
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_disconnect(conn_id: u64) -> *mut c_char {
    match ENGINE.write() {
        Ok(mut engine) => {
            if engine.connections.remove(&conn_id).is_some() {
                success_json(&serde_json::json!({ "ok": true }))
            } else {
                error_json(&format!("No PostgreSQL connection for conn_id={conn_id}"))
            }
        }
        Err(error) => error_json(&format!("Engine lock poisoned: {error}")),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_cancel_operation(conn_id: u64) -> *mut c_char {
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    let _ = run_with_timeout(async move { conn.cancel().await });
    success_json(&serde_json::json!({ "cancelled": true }))
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_server_info(conn_id: u64) -> *mut c_char {
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.server_info().await }) {
        Some(Ok(info)) => success_json(&info),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_list_databases(conn_id: u64) -> *mut c_char {
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_databases().await }) {
        Some(Ok(databases)) => success_json(&databases),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_list_collections(conn_id: u64, database: *const c_char) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_collections(&database).await }) {
        Some(Ok(tables)) => success_json(&tables),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_highlight_syntax(
    _language: *const c_char,
    _source: *const c_char,
) -> *mut c_char {
    error_json("PostgreSQL syntax highlighting is not wired yet")
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_get_sql_completions(request_json: *const c_char) -> *mut c_char {
    if request_json.is_null() {
        return error_json("null parameter");
    }

    let request_str = unsafe { CStr::from_ptr(request_json).to_string_lossy().into_owned() };
    let request = match serde_json::from_str::<sql_completion::CompletionRequest>(&request_str) {
        Ok(request) => request,
        Err(error) => return error_json(&format!("invalid SQL completion request: {error}")),
    };

    let response = sql_completion::complete_sql(&request);
    success_json(&response)
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_current_database(conn_id: u64) -> *mut c_char {
    match postgres_conn(conn_id) {
        Ok(conn) => success_json(&conn.default_database()),
        Err(error) => error_json(&error),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_list_postgres_schemas(
    conn_id: u64,
    database: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_schemas(&database).await }) {
        Some(Ok(schemas)) => success_json(&schemas),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_list_postgres_tables(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_tables(&database, &schema).await }) {
        Some(Ok(tables)) => success_json(&tables),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_list_sql_objects(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_sql_objects(&database, &schema).await }) {
        Some(Ok(objects)) => success_json(&objects),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_execute_postgres_query_arrow(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    user_where: *const c_char,
    user_order_by: *const c_char,
    cursor_json: *const c_char,
    limit: i64,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let table = match c_string(table, "table") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let user_where = optional_c_string(user_where);
    let user_order_by = optional_c_string(user_order_by);
    let cursor_state = optional_c_string(cursor_json)
        .and_then(|cursor| serde_json::from_str::<db::CursorState>(&cursor).ok());
    let effective_limit = if limit > 0 { limit } else { 200 };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };

    let result = run_with_timeout(async move {
        let query_result = conn
            .execute_query_in_schema(
                &database,
                &schema,
                &table,
                user_where.as_deref(),
                user_order_by.as_deref(),
                cursor_state.as_ref(),
                effective_limit,
            )
            .await?;
        let cursor_json = query_result
            .cursor
            .map(|cursor| serde_json::to_string(&cursor).unwrap_or_default());
        arrow::record_batch_to_arrow_cdata(
            query_result.batches,
            Some(query_result.total_count as usize),
            cursor_json,
        )
    });

    match result {
        Some(Ok(arrow_result)) => success_json(&arrow::ArrowCDataResponse::from(arrow_result)),
        Some(Err(error)) => error_json(&error),
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
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };

    let result = run_with_timeout(async move {
        let schema = request.schema.unwrap_or_else(|| "public".to_string());
        let batch_size = request.batch_size.unwrap_or(10_000).clamp(1, 100_000);
        let total_limit = request.limit.unwrap_or(0).max(0);
        let mut exported = 0usize;
        let mut offset = 0i64;
        let mut exporter: Option<export::TableExporter> = None;

        loop {
            let effective_limit = if total_limit > 0 {
                let remaining = total_limit.saturating_sub(exported as i64);
                if remaining <= 0 {
                    break;
                }
                remaining.min(batch_size)
            } else {
                batch_size
            };
            let (columns, rows) = conn
                .export_query_page(
                    &request.database,
                    &schema,
                    &request.table,
                    request.where_clause.as_deref(),
                    request.order_by.as_deref(),
                    effective_limit,
                    offset,
                )
                .await?;
            if exporter.is_none() {
                exporter = Some(export::TableExporter::create(
                    &request.output_path,
                    format,
                    export::ExportDialect::Postgres,
                    request.table.clone(),
                    columns,
                )?);
            }
            if let Some(writer) = exporter.as_mut() {
                writer.write_rows(&rows)?;
            }
            exported += rows.len();
            if rows.len() < effective_limit as usize {
                break;
            }
            offset += rows.len() as i64;
        }

        let rows_exported = match exporter {
            Some(writer) => writer.finish()?,
            None => {
                let writer = export::TableExporter::create(
                    &request.output_path,
                    format,
                    export::ExportDialect::Postgres,
                    request.table.clone(),
                    Vec::new(),
                )?;
                writer.finish()?
            }
        };
        Ok::<_, String>(export::ExportResult {
            ok: true,
            rows_exported,
            path: request.output_path,
        })
    });

    match result {
        Some(Ok(response)) => success_json(&response),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_execute_postgres_sql_arrow(
    conn_id: u64,
    database: *const c_char,
    sql: *const c_char,
    limit: i64,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let sql = match c_string(sql, "sql") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let effective_limit = if limit > 0 { limit } else { 200 };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };

    let result = run_with_timeout(async move {
        let query_result = conn.execute_sql(&database, &sql, effective_limit).await?;
        arrow::record_batch_to_arrow_cdata(
            query_result.batches,
            Some(query_result.total_count as usize),
            None,
        )
    });

    match result {
        Some(Ok(arrow_result)) => success_json(&arrow::ArrowCDataResponse::from(arrow_result)),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_catalog(conn_id: u64, database: *const c_char) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_catalog(&database).await }) {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_table_metadata(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let table = match c_string(table, "table") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.table_metadata(&database, &schema, &table).await }) {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_view_definition(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let name = match c_string(name, "name") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.view_definition(&database, &schema, &name).await }) {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
fn postgres_trigger_args(
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    trigger: *const c_char,
) -> Result<(String, String, String, String), String> {
    Ok((
        c_string(database, "database")?,
        c_string(schema, "schema")?,
        c_string(table, "table")?,
        c_string(trigger, "trigger")?,
    ))
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_enable_trigger(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    trigger: *const c_char,
) -> *mut c_char {
    let (database, schema, table, trigger) =
        match postgres_trigger_args(database, schema, table, trigger) {
            Ok(value) => value,
            Err(error) => return error_json(&error),
        };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.enable_trigger(&database, &schema, &table, &trigger)
            .await
    }) {
        Some(Ok(updated)) => success_json(&serde_json::json!({ "updated": updated })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_disable_trigger(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    trigger: *const c_char,
) -> *mut c_char {
    let (database, schema, table, trigger) =
        match postgres_trigger_args(database, schema, table, trigger) {
            Ok(value) => value,
            Err(error) => return error_json(&error),
        };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.disable_trigger(&database, &schema, &table, &trigger)
            .await
    }) {
        Some(Ok(updated)) => success_json(&serde_json::json!({ "updated": updated })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_drop_trigger(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    trigger: *const c_char,
) -> *mut c_char {
    let (database, schema, table, trigger) =
        match postgres_trigger_args(database, schema, table, trigger) {
            Ok(value) => value,
            Err(error) => return error_json(&error),
        };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.drop_trigger(&database, &schema, &table, &trigger)
            .await
    }) {
        Some(Ok(dropped)) => success_json(&serde_json::json!({ "dropped": dropped })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_rename_trigger(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    old_name: *const c_char,
    new_name: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let table = match c_string(table, "table") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let old_name = match c_string(old_name, "old_name") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let new_name = match c_string(new_name, "new_name") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.rename_trigger(&database, &schema, &table, &old_name, &new_name)
            .await
    }) {
        Some(Ok(renamed)) => success_json(&serde_json::json!({ "renamed": renamed })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_replace_trigger(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    old_name: *const c_char,
    ddl: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let table = match c_string(table, "table") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let old_name = match c_string(old_name, "old_name") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let ddl = match c_string(ddl, "ddl") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.replace_trigger(&database, &schema, &table, &old_name, &ddl)
            .await
    }) {
        Some(Ok(())) => success_json(&serde_json::json!({ "replaced": true })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_create_trigger(
    conn_id: u64,
    database: *const c_char,
    ddl: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let ddl = match c_string(ddl, "ddl") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.create_trigger(&database, &ddl).await }) {
        Some(Ok(())) => success_json(&serde_json::json!({ "created": true })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_execute_ddl(
    conn_id: u64,
    database: *const c_char,
    ddl: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let ddl = match c_string(ddl, "ddl") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.execute_ddl(&database, &ddl).await }) {
        Some(Ok(())) => success_json(&serde_json::json!({ "executed": true })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
fn postgres_schema_args(
    database: *const c_char,
    schema: *const c_char,
) -> Result<(String, String), String> {
    Ok((c_string(database, "database")?, c_string(schema, "schema")?))
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_list_functions(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
) -> *mut c_char {
    let (database, schema) = match postgres_schema_args(database, schema) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_functions(&database, &schema, false).await }) {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_list_trigger_functions(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
) -> *mut c_char {
    let (database, schema) = match postgres_schema_args(database, schema) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.list_functions(&database, &schema, true).await }) {
        Some(Ok(json)) => engine_ffi_util::string_to_c_ptr(json),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_save_function(
    conn_id: u64,
    database: *const c_char,
    ddl: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let ddl = match c_string(ddl, "ddl") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move { conn.create_or_replace_function(&database, &ddl).await }) {
        Some(Ok(())) => success_json(&serde_json::json!({ "saved": true })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_postgres_drop_function(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    name: *const c_char,
    arguments: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let name = match c_string(name, "name") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let arguments = match c_string(arguments, "arguments") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.drop_function(&database, &schema, &name, &arguments)
            .await
    }) {
        Some(Ok(dropped)) => success_json(&serde_json::json!({ "dropped": dropped })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_preview_postgres_sql(
    _database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    user_where: *const c_char,
    user_order_by: *const c_char,
    limit: i64,
) -> *mut c_char {
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let table = match c_string(table, "table") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let effective_limit = if limit > 0 { limit } else { 200 };
    match db::sql_builder::build_sql_preview_qualified(
        &schema,
        &table,
        optional_c_string(user_where).as_deref(),
        optional_c_string(user_order_by).as_deref(),
        effective_limit,
    ) {
        Ok(sql) => success_json(&serde_json::json!({ "sql": sql })),
        Err(error) => error_json(&error),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_postgres_sql_editability(sql: *const c_char) -> *mut c_char {
    let sql = match c_string(sql, "sql") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    match db::sql_builder::editable_single_table_select_target(&sql) {
        Ok(Some(target)) => success_json(&serde_json::json!({
            "editable": true,
            "schema": target.schema,
            "table": target.table,
        })),
        Ok(None) => success_json(&serde_json::json!({ "editable": false })),
        Err(error) => error_json(&error),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_insert_postgres_row(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    row_json: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let table = match c_string(table, "table") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let row_json = match c_string(row_json, "row_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.insert_row_from_json(&database, &schema, &table, &row_json)
            .await
    }) {
        Some(Ok(inserted)) => success_json(&serde_json::json!({ "inserted": inserted })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub extern "C" fn dbengine_batch_update_postgres_rows(
    conn_id: u64,
    database: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    key_column: *const c_char,
    updates_json: *const c_char,
) -> *mut c_char {
    let database = match c_string(database, "database") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let schema = match c_string(schema, "schema") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let table = match c_string(table, "table") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let key_column = match c_string(key_column, "key_column") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let updates_json = match c_string(updates_json, "updates_json") {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match postgres_conn(conn_id) {
        Ok(conn) => conn,
        Err(error) => return error_json(&error),
    };
    match run_with_timeout(async move {
        conn.batch_update_from_json(&database, &schema, &table, &key_column, &updates_json)
            .await
    }) {
        Some(Ok(updated)) => success_json(&serde_json::json!({ "updated": updated })),
        Some(Err(error)) => error_json(&error),
        None => error_json("Operation timed out"),
    }
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_cursor_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
}

#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_arrow_cdata(
    schema_ptr: *mut arrow_array::ffi::FFI_ArrowSchema,
    array_ptr: *mut arrow_array::ffi::FFI_ArrowArray,
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

/// Free a C string returned by this PostgreSQL driver dylib.
///
/// # Safety
/// `s` must be a pointer returned by this dylib, or null. It must not be
/// called more than once for the same pointer.
#[cfg(not(feature = "abi-v2"))]
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
}

#[cfg(not(feature = "abi-v2"))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mongodb_uri_without_opening_network_connection() {
        let result = validate_postgres_uri("mongodb://localhost:27017");

        assert!(result.is_err());
    }

    #[test]
    fn capabilities_json_describes_postgres_driver() {
        let value: serde_json::Value = serde_json::from_str(&capabilities_json()).unwrap();

        assert_eq!(value["abi_version"], 1);
        assert_eq!(value["driver"], "postgresql");
        assert_eq!(value["display_name"], "PostgreSQL");
        assert!(value["uri_schemes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("postgres")));
        assert!(value["features"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("sql_completion")));
        let deps = value["dependencies"]
            .as_array()
            .expect("dependencies array");
        assert!(deps.iter().any(|dep| dep["id"] == "completion"));
        assert!(deps.iter().any(|dep| dep["id"] == "tree_sitter_sql"));
        assert!(deps
            .iter()
            .any(|dep| dep["id"] == "arrow_ffi" && dep["required"] == true));
    }
}
