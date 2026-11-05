use engine_ffi_util::{error_json, success_json};
use engine_storage::{aggregate, ai_chat, connection, history, view_state};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

pub const ABI_VERSION: u32 = 1;

fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "abi_version": ABI_VERSION,
        "driver": "core",
        "display_name": "Core Engine",
        "uri_schemes": [],
        "features": [
            "history",
            "view_state",
                "saved_aggregates",
                "ai_chat",
            "json_syntax",
        ],
        "dependencies": [
            {
                "id": "storage",
                "display_name": "SQLite Storage",
                "kind": "bundled",
                "state": "installed",
                "required": true,
                "features": ["history", "view_state", "saved_aggregates", "ai_chat"]
            },
            {
                "id": "tree_sitter_json",
                "display_name": "Tree-sitter JSON",
                "kind": "bundled",
                "state": "installed",
                "required": false,
                "features": ["json_syntax"]
            }
        ]
    })
}

fn write_json_out<T: serde::Serialize>(value: &T, out_json: *mut *mut c_char) -> i32 {
    let json = match serde_json::to_string(value) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("[core] JSON encode failed: {error}");
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

fn c_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

#[no_mangle]
pub extern "C" fn dbengine_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
pub extern "C" fn dbengine_capabilities() -> *mut c_char {
    success_json(&capabilities())
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
pub extern "C" fn dbengine_highlight_syntax(
    text: *const c_char,
    language: *const c_char,
) -> *mut c_char {
    if text.is_null() || language.is_null() {
        return error_json("null parameter");
    }

    let text = c_string(text);
    let language = c_string(language);
    if language != "json" {
        return error_json("unsupported core syntax language");
    }

    let tokens = engine_syntax::highlight_syntax(
        &text,
        tree_sitter_json::LANGUAGE.into(),
        engine_syntax::SyntaxProfile::Json,
    );
    success_json(&tokens)
}

/// Free a C string returned by a core `dbengine_*` function.
///
/// # Safety
/// `s` must be a pointer returned by this dylib, or null. It must not be
/// called more than once for the same pointer.
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(s: *mut c_char) {
    unsafe { engine_ffi_util::free_string(s) };
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
    match history::save_entry(
        &c_string(conn_uuid),
        &c_string(database),
        &c_string(collection),
        &c_string(filter),
        &c_string(project),
        &c_string(sort),
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
    let result = history::fetch_entries(
        &c_string(conn_uuid),
        &c_string(database),
        &c_string(collection),
        limit,
    );
    match result {
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
            let c_str = match CString::new(json_str) {
                Ok(c_str) => c_str,
                Err(_) => return -1,
            };
            unsafe { *out_json = c_str.into_raw() };
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
    match history::clear_entries(
        &c_string(conn_uuid),
        &c_string(database),
        &c_string(collection),
    ) {
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
    match view_state::save_state(&c_string(conn_uuid), &c_string(state_json)) {
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
    match view_state::load_state(&c_string(conn_uuid)) {
        Ok(Some(json)) => {
            let c_str = match CString::new(json) {
                Ok(c_str) => c_str,
                Err(_) => return -1,
            };
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
    match view_state::delete_state(&c_string(conn_uuid)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[view_state] delete failed: {}", e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_saved_connections(out_json: *mut *mut c_char) -> i32 {
    match connection::list_connections() {
        Ok(items) => match serde_json::to_string(&items) {
            Ok(json) => match CString::new(json) {
                Ok(c_str) => {
                    unsafe { *out_json = c_str.into_raw() };
                    0
                }
                Err(_) => -1,
            },
            Err(error) => {
                eprintln!("[connection] list encode failed: {error}");
                -1
            }
        },
        Err(error) => {
            eprintln!("[connection] list failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_upsert_saved_connection(connection_json: *const c_char) -> i32 {
    let record =
        match serde_json::from_str::<connection::SavedConnectionRecord>(&c_string(connection_json))
        {
            Ok(record) => record,
            Err(error) => {
                eprintln!("[connection] decode failed: {error}");
                return -1;
            }
        };
    match connection::upsert_connection(&record) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] upsert failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_delete_saved_connection(id: *const c_char) -> i32 {
    match connection::delete_connection(&c_string(id)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] delete failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_touch_saved_connection(id: *const c_char, timestamp: f64) -> i32 {
    match connection::touch_connection(&c_string(id), timestamp) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] touch failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_set_connection_pinned(id: *const c_char, is_pinned: bool) -> i32 {
    match connection::set_connection_pinned(&c_string(id), is_pinned) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] set pinned failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_project_connection_ids(
    project_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    match connection::list_project_connection_ids(&c_string(project_id)) {
        Ok(items) => write_json_out(&items, out_json),
        Err(error) => {
            eprintln!("[connection] list project memberships failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_add_project_connection(
    project_id: *const c_char,
    connection_id: *const c_char,
) -> i32 {
    match connection::add_project_connection(&c_string(project_id), &c_string(connection_id)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] add project membership failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_remove_project_connection(
    project_id: *const c_char,
    connection_id: *const c_char,
) -> i32 {
    match connection::remove_project_connection(&c_string(project_id), &c_string(connection_id)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] remove project membership failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_list_projects(out_json: *mut *mut c_char) -> i32 {
    match connection::list_projects() {
        Ok(items) => write_json_out(&items, out_json),
        Err(error) => {
            eprintln!("[connection] list projects failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_upsert_project(project_json: *const c_char) -> i32 {
    let record = match serde_json::from_str::<connection::ProjectRecord>(&c_string(project_json)) {
        Ok(record) => record,
        Err(error) => {
            eprintln!("[connection] decode project failed: {error}");
            return -1;
        }
    };
    match connection::upsert_project(&record) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] upsert project failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_delete_project(id: *const c_char) -> i32 {
    match connection::delete_project(&c_string(id)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] delete project failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_rename_project(
    old_id: *const c_char,
    project_json: *const c_char,
) -> i32 {
    let record = match serde_json::from_str::<connection::ProjectRecord>(&c_string(project_json)) {
        Ok(record) => record,
        Err(error) => {
            eprintln!("[connection] decode renamed project failed: {error}");
            return -1;
        }
    };
    match connection::rename_project(&c_string(old_id), &record) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[connection] rename project failed: {error}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_ai_upsert_conversation(conversation_json: *const c_char) -> i32 {
    let conversation =
        match serde_json::from_str::<ai_chat::AIConversationRecord>(&c_string(conversation_json)) {
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
    let scope = match serde_json::from_str::<ai_chat::AIConversationScope>(&c_string(scope_json)) {
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
    match ai_chat::get_conversation(&c_string(id)) {
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
    match ai_chat::delete_conversation(&c_string(id)) {
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
    match aggregate::list_aggregates(&c_string(conn_uuid), &c_string(database)) {
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
            let c_str = match CString::new(json_str) {
                Ok(c_str) => c_str,
                Err(_) => return -1,
            };
            unsafe { *out_json = c_str.into_raw() };
            0
        }
        Err(e) => {
            eprintln!("[aggregate] list failed: {}", e);
            let empty = CString::new("[]").expect("static JSON contains no nul");
            unsafe { *out_json = empty.into_raw() };
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dbengine_get_saved_aggregate(
    id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    match aggregate::get_aggregate(&c_string(id)) {
        Ok(Some(item)) => {
            let json = serde_json::json!({
                "id": item.id,
                "conn_uuid": item.conn_uuid,
                "database": item.database,
                "collection": item.collection,
                "name": item.name,
                "kind": item.kind,
                "pipeline_json": item.pipeline_json,
                "created_at": item.created_at,
                "updated_at": item.updated_at,
            })
            .to_string();
            let c_str = match CString::new(json) {
                Ok(c_str) => c_str,
                Err(_) => return -1,
            };
            unsafe { *out_json = c_str.into_raw() };
            0
        }
        Ok(None) => {
            unsafe { *out_json = std::ptr::null_mut() };
            0
        }
        Err(e) => {
            eprintln!("[aggregate] get failed: {}", e);
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
    match aggregate::upsert_aggregate(
        &c_string(id),
        &c_string(conn_uuid),
        &c_string(database),
        &c_string(collection),
        &c_string(name),
        &c_string(kind),
        &c_string(pipeline_json),
        created_at,
        updated_at,
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
    match aggregate::delete_aggregate(&c_string(id)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[aggregate] delete failed: {}", e);
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_json_includes_core_features() {
        let value = serde_json::to_value(capabilities()).expect("serialize capabilities");
        assert_eq!(value["abi_version"], ABI_VERSION);
        assert_eq!(value["driver"], "core");
        let features = value["features"].as_array().expect("features array");
        for feature in ["history", "view_state", "saved_aggregates", "json_syntax"] {
            assert!(
                features.iter().any(|item| item == feature),
                "missing feature {feature}: {features:?}"
            );
        }
        let deps = value["dependencies"]
            .as_array()
            .expect("dependencies array");
        assert!(deps
            .iter()
            .any(|dep| dep["id"] == "storage" && dep["required"] == true));
        assert!(deps.iter().any(|dep| dep["id"] == "tree_sitter_json"));
    }

    #[test]
    fn abi_version_is_stable() {
        assert_eq!(dbengine_abi_version(), 1);
    }
}
