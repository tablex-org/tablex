use arrow::compute::concat_batches;
use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::{Array, RecordBatch, StructArray};
use engine_ffi_util::{error_json, string_to_c_ptr, success_json};
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::sync::{Arc, RwLock};

pub const ABI_VERSION: u32 = 1;

lazy_static::lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .thread_name("dbengine-mysql-worker")
        .build()
        .expect("Failed to create MySQL driver tokio runtime");
    static ref ENGINE: RwLock<EngineState> = RwLock::new(EngineState::new());
}

struct EngineState {
    connections: HashMap<u64, Arc<db::mysql::MySqlConn>>,
    next_conn_id: u64,
}

impl EngineState {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
            next_conn_id: 1,
        }
    }
}

pub mod db {
    pub mod mysql_arrow {
        include!("../../dbengine/src/db/mysql_arrow.rs");
    }
    pub mod mysql {
        include!("../../dbengine/src/db/mysql.rs");
    }
}

#[derive(serde::Serialize)]
struct ArrowCDataResponse {
    schema_ptr: u64,
    array_ptr: u64,
    row_count: usize,
    col_count: usize,
    column_names: Vec<String>,
    column_types: Vec<String>,
    total_count: usize,
    last_id: Option<String>,
    is_estimated_count: bool,
    id_kind: String,
    session_id: u64,
    large_field_report: serde_json::Value,
    cancelled: bool,
    was_truncated: bool,
}

pub fn capabilities_json() -> String {
    serde_json::json!({
        "abi_version": ABI_VERSION,
        "driver": "mysql",
        "display_name": "MySQL",
        "uri_schemes": ["mysql"],
        "features": ["sql_queries", "catalog_metadata", "arrow_results"],
        "dependencies": [
            {"id":"arrow_ffi","display_name":"Arrow FFI","kind":"bundled","state":"installed","required":true,"features":["arrow_results"]},
            {"id":"tree_sitter_sql","display_name":"Tree-sitter SQL","kind":"bundled","state":"installed","required":false,"features":["sql_syntax"]}
        ]
    }).to_string()
}

#[no_mangle]
pub extern "C" fn dbengine_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
pub extern "C" fn dbengine_capabilities() -> *mut c_char {
    string_to_c_ptr(capabilities_json())
}

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

#[no_mangle]
pub extern "C" fn dbengine_shutdown() -> *mut c_char {
    success_json(&serde_json::json!({ "ok": true }))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_connect(uri: *const c_char, _name: *const c_char) -> *mut c_char {
    let uri = match cstr(uri) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    match RUNTIME.block_on(db::mysql::MySqlConn::connect(&uri)) {
        Ok(conn) => {
            let mut engine = match ENGINE.write() {
                Ok(engine) => engine,
                Err(_) => return error_json("engine lock poisoned"),
            };
            let id = engine.next_conn_id;
            engine.next_conn_id += 1;
            engine.connections.insert(id, Arc::new(conn));
            success_json(&serde_json::json!({ "id": id }))
        }
        Err(error) => error_json(&error),
    }
}

#[no_mangle]
pub extern "C" fn dbengine_disconnect(conn_id: u64) -> *mut c_char {
    let conn = match ENGINE.write() {
        Ok(mut engine) => engine.connections.remove(&conn_id),
        Err(_) => return error_json("engine lock poisoned"),
    };
    match conn {
        Some(conn) => {
            if let Ok(conn) = Arc::try_unwrap(conn) {
                RUNTIME.block_on(conn.disconnect());
            }
            success_json(&serde_json::json!({ "ok": true }))
        }
        None => error_json(&format!("Connection {conn_id} not found")),
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_databases(conn_id: u64) -> *mut c_char {
    with_conn(conn_id, |conn| RUNTIME.block_on(conn.list_databases()))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_list_collections(conn_id: u64, db: *const c_char) -> *mut c_char {
    let db = match cstr(db) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    with_conn(conn_id, |conn| RUNTIME.block_on(conn.list_tables(&db)))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_server_info(conn_id: u64) -> *mut c_char {
    with_conn(conn_id, |conn| RUNTIME.block_on(conn.server_info()))
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_execute_mysql_sql_arrow(
    conn_id: u64,
    db: *const c_char,
    sql: *const c_char,
    limit: i64,
) -> *mut c_char {
    let db = match cstr(db) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let sql = match cstr(sql) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    let conn = match get_conn(conn_id) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    match RUNTIME
        .block_on(conn.execute_sql(&db, &sql, limit))
        .and_then(|batch| record_batch_to_response(vec![batch]))
    {
        Ok(response) => success_json(&response),
        Err(error) => error_json(&error),
    }
}

#[no_mangle]
pub unsafe extern "C" fn dbengine_free_arrow_cdata(
    schema: *mut FFI_ArrowSchema,
    array: *mut FFI_ArrowArray,
) {
    if !schema.is_null() {
        unsafe {
            drop(Box::from_raw(schema));
        }
    }
    if !array.is_null() {
        unsafe {
            drop(Box::from_raw(array));
        }
    }
}

fn with_conn<T: serde::Serialize>(
    conn_id: u64,
    f: impl FnOnce(Arc<db::mysql::MySqlConn>) -> Result<T, String>,
) -> *mut c_char {
    let conn = match get_conn(conn_id) {
        Ok(value) => value,
        Err(error) => return error_json(&error),
    };
    match f(conn) {
        Ok(value) => success_json(&value),
        Err(error) => error_json(&error),
    }
}

fn get_conn(conn_id: u64) -> Result<Arc<db::mysql::MySqlConn>, String> {
    ENGINE
        .read()
        .map_err(|_| "engine lock poisoned".to_string())?
        .connections
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| format!("Connection {conn_id} not found"))
}

unsafe fn cstr(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("null string pointer".to_string());
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|value| value.to_string())
        .map_err(|e| e.to_string())
}

fn record_batch_to_response(batches: Vec<RecordBatch>) -> Result<ArrowCDataResponse, String> {
    let batch = if batches.len() == 1 {
        batches.into_iter().next().expect("batch len checked")
    } else {
        let schema = batches[0].schema();
        concat_batches(&schema, &batches).map_err(|error| error.to_string())?
    };
    let schema = batch.schema();
    let row_count = batch.num_rows();
    let col_count = batch.num_columns();
    let column_names = schema
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect::<Vec<_>>();
    let column_types = schema
        .fields()
        .iter()
        .map(|field| format!("{:?}", field.data_type()))
        .collect::<Vec<_>>();
    let struct_array = StructArray::from(batch);
    let schema_ptr = Box::into_raw(Box::new(
        FFI_ArrowSchema::try_from(schema.as_ref()).map_err(|error| error.to_string())?,
    ));
    let array_ptr = Box::into_raw(Box::new(FFI_ArrowArray::new(&struct_array.to_data())));
    Ok(ArrowCDataResponse {
        schema_ptr: schema_ptr as u64,
        array_ptr: array_ptr as u64,
        row_count,
        col_count,
        column_names,
        column_types,
        total_count: row_count,
        last_id: None,
        is_estimated_count: false,
        id_kind: "MySQL".to_string(),
        session_id: 0,
        large_field_report: serde_json::json!({ "fields": [] }),
        cancelled: false,
        was_truncated: false,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn capabilities_json_describes_mysql_driver() {
        let value: serde_json::Value = serde_json::from_str(&super::capabilities_json()).unwrap();
        assert_eq!(value["driver"], "mysql");
        assert_eq!(value["display_name"], "MySQL");
        assert!(value["uri_schemes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("mysql")));
    }
}
