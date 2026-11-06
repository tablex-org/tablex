use crate::driver::MongoDriver;
use crate::pages::BsonPageCache;
use engine_driver_api::{
    CCellText, CCellTextBatch, CPageFunctionTable, CPageLease, MutationRequest, NavigatorRequest,
    QueryRequest, SuccessEnvelope,
};
use engine_ffi_util::string_to_c_ptr;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::{Arc, Mutex, OnceLock};

struct Session {
    drivers: HashMap<u64, Arc<MongoDriver>>,
    revisions: HashMap<u64, u64>,
    pages: BsonPageCache,
}

static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();

#[cfg(feature = "abi-v2")]
static PAGE_FUNCTIONS: CPageFunctionTable = CPageFunctionTable {
    struct_size: std::mem::size_of::<CPageFunctionTable>() as u32,
    abi_version: engine_driver_api::PAGE_ABI_VERSION,
    acquire_page: Some(dbengine_acquire_page),
};

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_abi_version() -> u32 {
    engine_driver_api::ABI_VERSION
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_capabilities() -> *mut c_char {
    guarded("capabilities.internal", || {
        string_to_c_ptr(
            r#"{
        "abi_version": 2,
        "driver": "mongodb",
        "display_name": "MongoDB",
         "uri_schemes": ["mongodb", "mongodb+srv"],
         "features": [
             "navigator", "query.mongo_find", "query.mongo_script", "query.mongo_aggregate",
             "query.mongo_aggregate_explain", "mongo.command", "operation.cancel", "export.mongo_aggregate",
             "mutation.insert", "mutation.update",
             "mutation.delete", "database.create", "bson_rendering", "document_cache", "query.page_lease",
             "admin.collection", "admin.indexes", "admin.database",
             "mutation.drop_collection", "mutation.rename_collection", "mutation.clone_collection",
             "mutation.create_index", "mutation.drop_index", "mutation.drop_database"
         ],
         "editors": ["mongo_find"],
         "navigator_kinds": ["database", "collection"],
        "single_connection_pool": true
    }"#,
        )
    })
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_page_function_table() -> *const CPageFunctionTable {
    &PAGE_FUNCTIONS
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_acquire_page(
    connection_id: u64,
    page_id: u64,
    _row_start: u64,
    _row_count: u64,
) -> CPageLease {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        acquire_page_unguarded(connection_id, page_id, _row_start, _row_count)
    }))
    .unwrap_or_else(|_| empty_page_lease())
}

#[cfg(feature = "abi-v2")]
unsafe fn acquire_page_unguarded(
    connection_id: u64,
    page_id: u64,
    _row_start: u64,
    _row_count: u64,
) -> CPageLease {
    let Some(mtx) = SESSION.get() else {
        return empty_page_lease();
    };
    let mut session = lock_session(mtx);
    if connection_id == 0 || !session.drivers.contains_key(&connection_id) {
        return empty_page_lease();
    }
    session
        .pages
        .acquire_raw_page_for_connection(connection_id, page_id)
        .unwrap_or_else(empty_page_lease)
}

#[cfg(feature = "abi-v2")]
fn empty_page_lease() -> CPageLease {
    CPageLease {
        data: std::ptr::null(),
        data_len: 0,
        generation: 0,
        release: None,
        release_context: std::ptr::null_mut(),
        slices: std::ptr::null(),
        slice_count: 0,
    }
}

/// What a find reads its filter, sort and projection from.
#[cfg(feature = "abi-v2")]
enum FindInput {
    /// Text the user typed (the find bar).
    Text {
        filter: String,
        sort: String,
        project: String,
    },
    /// Documents a parsed script already produced.
    Parsed {
        filter: bson::Document,
        sort: Option<bson::Document>,
        projection: Option<bson::Document>,
    },
}

/// The session even after a panic poisoned its lock: the maps it guards are only ever
/// mutated by whole inserts/removes, so a panicking request leaves them consistent, and
/// refusing every later call on every connection would be worse than going on.
fn lock_session(mtx: &Mutex<Session>) -> std::sync::MutexGuard<'_, Session> {
    mtx.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn driver_snapshot(connection_id: u64) -> Result<(Arc<MongoDriver>, u64), &'static str> {
    let mtx = SESSION.get().ok_or("session not initialized")?;
    let session = lock_session(mtx);
    let driver = session
        .drivers
        .get(&connection_id)
        .cloned()
        .ok_or("session not connected")?;
    let revision = session
        .revisions
        .get(&connection_id)
        .copied()
        .ok_or("session revision missing")?;
    Ok((driver, revision))
}

fn revision_is_current(connection_id: u64, revision: u64) -> Result<bool, &'static str> {
    let mtx = SESSION.get().ok_or("session not initialized")?;
    let session = lock_session(mtx);
    Ok(session.revisions.get(&connection_id).copied() == Some(revision))
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_init() -> *mut c_char {
    guarded("init.internal", || init_unguarded())
}

#[cfg(feature = "abi-v2")]
fn init_unguarded() -> *mut c_char {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    SESSION.get_or_init(|| {
        Mutex::new(Session {
            drivers: HashMap::new(),
            revisions: HashMap::new(),
            pages: BsonPageCache::default(),
        })
    });
    string_to_c_ptr(r#"{"ok":true,"value":{}}"#)
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_shutdown() -> *mut c_char {
    guarded("shutdown.internal", || shutdown_unguarded())
}

#[cfg(feature = "abi-v2")]
fn shutdown_unguarded() -> *mut c_char {
    cancel_connection_operations(None);
    if let Some(mtx) = SESSION.get() {
        let mut session = lock_session(mtx);
        session.drivers.clear();
        session.revisions.clear();
        session.pages.clear();
    }
    string_to_c_ptr(r#"{"ok":true,"value":{}}"#)
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_connect(request_json: *const c_char) -> *mut c_char {
    guarded("connect.internal", || unsafe { connect_unguarded(request_json) })
}

#[cfg(feature = "abi-v2")]
unsafe fn connect_unguarded(request_json: *const c_char) -> *mut c_char {
    let request = match request_string(request_json, "request_json") {
        Ok(value) => value,
        Err(error) => return error_envelope("connect.bad_request", &error),
    };
    let payload: serde_json::Value = match serde_json::from_str(&request) {
        Ok(value) => value,
        Err(error) => return error_envelope("connect.bad_request", &error.to_string()),
    };
    let uri = match payload.get("uri").and_then(serde_json::Value::as_str) {
        Some(uri) => uri,
        None => return error_envelope("connect.bad_request", "missing uri"),
    };
    let (id, driver) = match MongoDriver::connect(uri) {
        Ok(value) => value,
        Err(error) => return error_envelope("connect.failed", &error),
    };
    let Some(mtx) = SESSION.get() else {
        return error_envelope("connect.not_initialized", "session not initialized");
    };
    let mut session = lock_session(mtx);
    session.drivers.insert(id, Arc::new(driver));
    session.revisions.insert(id, 0);
    success_json(serde_json::json!({ "connection_id": id }))
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_disconnect(connection_id: u64) -> *mut c_char {
    guarded("disconnect.internal", || disconnect_unguarded(connection_id))
}

#[cfg(feature = "abi-v2")]
fn disconnect_unguarded(connection_id: u64) -> *mut c_char {
    cancel_connection_operations(Some(connection_id));
    if let Some(mtx) = SESSION.get() {
        let mut session = lock_session(mtx);
        session.drivers.remove(&connection_id);
        session.revisions.remove(&connection_id);
        session.pages.clear_connection(connection_id);
    }
    string_to_c_ptr(r#"{"ok":true,"value":{}}"#)
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_list_nav_nodes(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    guarded("navigator.internal", || unsafe { list_nav_nodes_unguarded(connection_id, request_json) })
}

#[cfg(feature = "abi-v2")]
unsafe fn list_nav_nodes_unguarded(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    let request = match request_string(request_json, "request_json") {
        Ok(value) => value,
        Err(error) => return error_envelope("navigator.bad_request", &error),
    };
    let request: NavigatorRequest = match serde_json::from_str(&request) {
        Ok(value) => value,
        Err(error) => return error_envelope("navigator.bad_request", &error.to_string()),
    };
    let (driver, revision) = match driver_snapshot(connection_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return error_envelope("navigator.not_connected", error),
    };
    let page = match driver.list_nav_nodes(&request) {
        Ok(page) => page,
        Err(error) => return error_envelope("navigator.failed", &error),
    };
    match revision_is_current(connection_id, revision) {
        Ok(true) => success_json(serde_json::to_value(&page).expect("navigator page serializes")),
        Ok(false) => error_envelope("navigator.stale", "connection changed during request"),
        Err(error) => error_envelope("navigator.not_connected", error),
    }
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_open_query(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    guarded("query.internal", || unsafe { open_query_unguarded(connection_id, request_json) })
}

#[cfg(feature = "abi-v2")]
unsafe fn open_query_unguarded(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    let request_json = match request_string(request_json, "request_json") {
        Ok(value) => value,
        Err(error) => return error_envelope("query.bad_request", &error),
    };
    let request: QueryRequest = match serde_json::from_str(&request_json) {
        Ok(value) => value,
        Err(error) => return error_envelope("query.bad_request", &error.to_string()),
    };
    if request.kind == crate::admin::QUERY_LIST_INDEXES {
        return list_indexes_page(connection_id, &request);
    }
    if request.kind == QUERY_AGGREGATE || request.kind == QUERY_AGGREGATE_EXPLAIN {
        return guarded("query.internal", || aggregate_page(connection_id, &request));
    }
    if request.kind != "mongo_find" && request.kind != "mongo_script" {
        return error_envelope("query.unsupported", "unsupported MongoDB query kind");
    }
    let replacement_page_id =
        if request.payload.get("filter").is_some() || request.payload.get("script").is_some() {
            match request.payload.get("replacement_page_id") {
                None => None,
                Some(value) => match value.as_u64() {
                    Some(page_id) => Some(page_id),
                    None => {
                        return error_envelope(
                            "query.bad_request",
                            "replacement_page_id must be an unsigned integer",
                        )
                    }
                },
            }
        } else {
            None
        };
    let Some(database) = request.database.as_deref() else {
        return error_envelope("query.bad_request", "missing database");
    };
    if database.trim().is_empty() {
        return error_envelope(
            "query.bad_request",
            "select a database before running a MongoDB query",
        );
    }
    let (collection, input, offset, limit, script_window) = if request.kind == "mongo_script" {
        let Some(script) = request
            .payload
            .get("script")
            .and_then(serde_json::Value::as_str)
        else {
            return error_envelope("query.bad_request", "MongoDB script text is required");
        };
        let plan = match crate::mongo_script_parser::inspect_read_script(script) {
            Ok(plan) => plan,
            Err(error) => return error_envelope("query.unsupported", &error.to_string()),
        };
        let Some(offset) = plan.skip.checked_add(request.offset) else {
            return error_envelope("query.bad_request", "MongoDB script offset overflow");
        };
        let remaining = plan
            .limit
            .map(|limit| limit.saturating_sub(request.offset))
            .unwrap_or(u64::MAX);
        if remaining == 0 {
            return error_envelope(
                "query.bad_request",
                "requested page exceeds the script limit",
            );
        }
        (
            plan.collection,
            // The parsed documents go to the server as they are: typed values
            // (dates, ObjectIds, NumberLong ...) are never printed and re-read.
            FindInput::Parsed {
                filter: plan.filter,
                sort: plan.sort,
                projection: plan.projection,
            },
            offset,
            request.limit.min(remaining),
            Some((plan.skip, plan.limit)),
        )
    } else {
        let Some(collection) = request.object.clone() else {
            return error_envelope("query.bad_request", "missing collection");
        };
        let filter = request.payload.get("filter").unwrap_or(&request.payload);
        let text = |value: Option<&serde_json::Value>| match value {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(value)) => value.clone(),
            Some(value) => value.to_string(),
        };
        (
            collection,
            FindInput::Text {
                filter: text(Some(filter)),
                sort: text(request.payload.get("sort")),
                project: text(request.payload.get("project")),
            },
            request.offset,
            request.limit,
            None,
        )
    };
    let (driver, revision) = match driver_snapshot(connection_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return error_envelope("query.not_connected", error),
    };
    let found = match input {
        FindInput::Text {
            filter,
            sort,
            project,
        } => driver.mongo_find(database, &collection, &filter, &sort, &project, offset, limit),
        FindInput::Parsed {
            filter,
            sort,
            projection,
        } => driver.mongo_find_documents(
            database,
            &collection,
            filter,
            sort,
            projection,
            offset,
            limit,
        ),
    };
    let mut result = match found {
        Ok(result) => result,
        Err(error) => return error_envelope("query.failed", &error),
    };
    if let Some((skip, limit)) = script_window {
        result.total_count = limit.map_or(result.total_count.saturating_sub(skip), |limit| {
            result.total_count.saturating_sub(skip).min(limit)
        });
    }
    let Some(mtx) = SESSION.get() else {
        return error_envelope("query.not_connected", "session not initialized");
    };
    let mut session = lock_session(mtx);
    if session.revisions.get(&connection_id).copied() != Some(revision) {
        return error_envelope("query.stale", "connection changed during query");
    }
    let page = match replacement_page_id {
        Some(replacement_page_id) => session.pages.insert_replacement_for_connection(
            connection_id,
            replacement_page_id,
            request.operation_id,
            result.documents,
            Some(result.total_count),
            result.total_is_exact,
        ),
        None => session.pages.insert_for_connection(
            connection_id,
            request.operation_id,
            result.documents,
            Some(result.total_count),
            result.total_is_exact,
        ),
    };
    let Some(page) = page else {
        return error_envelope("query.page_cache_full", "all cached query pages are in use");
    };
    session.pages.set_namespace(page.page_id, database, &collection);
    success_json(serde_json::to_value(page).expect("query page serializes"))
}

/// `mongo_list_indexes`: raw `listIndexes` documents as a cached query page.
#[cfg(feature = "abi-v2")]
fn list_indexes_page(connection_id: u64, request: &QueryRequest) -> *mut c_char {
    let (database, collection) = match crate::admin::parse_list_indexes(request) {
        Ok(target) => target,
        Err(error) => return error_envelope("query.bad_request", &error),
    };
    let (driver, revision) = match driver_snapshot(connection_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return error_envelope("query.not_connected", error),
    };
    let documents = match driver.list_indexes(&database, &collection) {
        Ok(documents) => documents,
        Err(error) => return error_envelope("query.failed", &crate::admin::redact(&error)),
    };
    let total = documents.len() as u64;
    let Some(mtx) = SESSION.get() else {
        return error_envelope("query.not_connected", "session not initialized");
    };
    let mut session = lock_session(mtx);
    if session.revisions.get(&connection_id).copied() != Some(revision) {
        return error_envelope("query.stale", "connection changed during query");
    }
    let Some(page) = session.pages.insert_for_connection(
        connection_id,
        request.operation_id,
        documents,
        Some(total),
        true,
    ) else {
        return error_envelope("query.page_cache_full", "all cached query pages are in use");
    };
    session.pages.set_namespace(page.page_id, &database, &collection);
    success_json(serde_json::to_value(page).expect("query page serializes"))
}

#[cfg(feature = "abi-v2")]
const QUERY_AGGREGATE: &str = "mongo_aggregate";
#[cfg(feature = "abi-v2")]
const QUERY_AGGREGATE_EXPLAIN: &str = "mongo_aggregate_explain";

/// In-flight cancellable operations, keyed by (connection id, operation id).
#[cfg(feature = "abi-v2")]
struct ActiveOperation {
    cancel: crate::driver::CancelToken,
    comment: String,
    driver: Arc<MongoDriver>,
}

#[cfg(feature = "abi-v2")]
type OperationMap = HashMap<(u64, u64), ActiveOperation>;

#[cfg(feature = "abi-v2")]
static OPERATIONS: OnceLock<Mutex<OperationMap>> = OnceLock::new();

#[cfg(feature = "abi-v2")]
fn operations() -> std::sync::MutexGuard<'static, OperationMap> {
    OPERATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cancels that arrived before their operation registered (the host may send
/// `operation.cancel` the instant it learns the id, racing `open_query`). The
/// operation starts already cancelled. Bounded: ids are unique per driver
/// handle, so an entry for an operation that never comes only ages out.
#[cfg(feature = "abi-v2")]
const MAX_EARLY_CANCELS: usize = 256;

#[cfg(feature = "abi-v2")]
static EARLY_CANCELS: OnceLock<Mutex<std::collections::VecDeque<(u64, u64)>>> = OnceLock::new();

#[cfg(feature = "abi-v2")]
fn early_cancels() -> std::sync::MutexGuard<'static, std::collections::VecDeque<(u64, u64)>> {
    EARLY_CANCELS
        .get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Remembers a cancel for an operation that is not registered (yet).
#[cfg(feature = "abi-v2")]
fn remember_early_cancel(key: (u64, u64)) {
    let mut early = early_cancels();
    if early.contains(&key) {
        return;
    }
    if early.len() >= MAX_EARLY_CANCELS {
        early.pop_front();
    }
    early.push_back(key);
}

/// Runs an export's body so a panic becomes an error envelope: unwinding out
/// of an `extern "C"` function aborts the host process.
#[cfg(feature = "abi-v2")]
fn guarded(code: &str, body: impl FnOnce() -> *mut c_char) -> *mut c_char {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(response) => response,
        Err(_) => error_envelope(code, "internal driver error"),
    }
}

/// Removes the registry entry when the operation ends (any exit path).
#[cfg(feature = "abi-v2")]
struct OperationGuard((u64, u64));

#[cfg(feature = "abi-v2")]
impl Drop for OperationGuard {
    fn drop(&mut self) {
        operations().remove(&self.0);
    }
}

#[cfg(feature = "abi-v2")]
/// A random value per process: two TableX instances (or two driver loads)
/// sharing a server never produce the same operation tag.
#[cfg(feature = "abi-v2")]
fn process_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
        format!("{:08x}{:016x}", std::process::id(), random)
    })
}

/// The `comment` that tags an operation for `killOp`: `prefix` is
/// `tablex` for queries and [`crate::mongo_command::COMMAND_TAG_PREFIX`] for raw commands.
#[cfg(feature = "abi-v2")]
fn operation_tag(prefix: &str, connection_id: u64, operation_id: u64) -> String {
    format!("{prefix}{}:{connection_id}:{operation_id}", process_nonce())
}

#[cfg(feature = "abi-v2")]
fn begin_operation(
    connection_id: u64,
    operation_id: u64,
    driver: &Arc<MongoDriver>,
) -> (OperationGuard, crate::driver::AggControl) {
    begin_tagged_operation(connection_id, operation_id, driver, "tablex:")
}

#[cfg(feature = "abi-v2")]
fn begin_tagged_operation(
    connection_id: u64,
    operation_id: u64,
    driver: &Arc<MongoDriver>,
    tag_prefix: &str,
) -> (OperationGuard, crate::driver::AggControl) {
    let cancel = crate::driver::CancelToken::default();
    let comment = operation_tag(tag_prefix, connection_id, operation_id);
    // One lock order (operations, then the early-cancel list) and the
    // operations lock held across "consume a tombstone, register": a cancel
    // either finds the operation registered or leaves a tombstone this sees.
    let mut active = operations();
    {
        let mut early = early_cancels();
        if let Some(position) = early.iter().position(|key| *key == (connection_id, operation_id)) {
            early.remove(position);
            cancel.cancel();
        }
    }
    active.insert(
        (connection_id, operation_id),
        ActiveOperation {
            cancel: cancel.clone(),
            comment: comment.clone(),
            driver: Arc::clone(driver),
        },
    );
    drop(active);
    (
        OperationGuard((connection_id, operation_id)),
        crate::driver::AggControl {
            comment: Some(comment),
            cancel: Some(cancel),
        },
    )
}

/// Cancels every in-flight operation of a connection (disconnect / shutdown).
#[cfg(feature = "abi-v2")]
fn cancel_connection_operations(connection_id: Option<u64>) {
    let active = operations();
    early_cancels().retain(|(connection, _)| connection_id.is_some_and(|id| id != *connection));
    let targets: Vec<_> = active
        .iter()
        .filter(|((connection, _), _)| connection_id.is_none_or(|id| id == *connection))
        .map(|(_, op)| (op.cancel.clone(), op.comment.clone(), Arc::clone(&op.driver)))
        .collect();
    drop(active);
    for (cancel, comment, driver) in targets {
        cancel.cancel();
        driver.kill_operation(&comment);
    }
}

/// `operation.cancel`: stops the query started with `operation_id` on this
/// connection. Idempotent: an unknown / finished operation is not an error
/// (the host cancels after the fact all the time). The running query's
/// future is dropped client-side and a best-effort `killOp` is sent.
#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_cancel_operation(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    guarded("cancel.internal", || {
        let request_json = match request_string(request_json, "request_json") {
            Ok(value) => value,
            Err(error) => return error_envelope("cancel.bad_request", &error),
        };
        let request: engine_driver_api::CancelRequest = match serde_json::from_str(&request_json) {
            Ok(value) => value,
            Err(error) => return error_envelope("cancel.bad_request", &error.to_string()),
        };
        let key = (connection_id, request.operation_id.0);
        let active = operations();
        let target = active
            .get(&key)
            .map(|op| (op.cancel.clone(), op.comment.clone(), Arc::clone(&op.driver)));
        let found = target.is_some();
        if target.is_none() {
            // Not running (yet): if it starts later it must start cancelled.
            // Done under the registry lock, so it cannot slip between a
            // registration and this lookup.
            remember_early_cancel(key);
        }
        drop(active);
        if let Some((cancel, comment, driver)) = target {
            cancel.cancel();
            driver.kill_operation(&comment);
        }
        success_json(serde_json::json!({ "cancelled": found }))
    })
}

/// `mongo.command`: one raw `runCommand`; request and reply shapes are in
/// [`crate::mongo_command`]. Cancel with `dbengine_cancel_operation` and the
/// request's `operation_id`; the same id may be reused by consecutive commands
/// (a cancel that lands between two of them starts the next one cancelled).
#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_mongo_command(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    guarded("query.internal", || {
        use crate::mongo_command::{CommandError, CommandRequest};
        let request_json = match request_string(request_json, "request_json") {
            Ok(value) => value,
            Err(error) => return error_envelope("query.bad_request", &error),
        };
        let request: CommandRequest = match serde_json::from_str(&request_json) {
            Ok(value) => value,
            Err(error) => return error_envelope("query.bad_request", &error.to_string()),
        };
        let (driver, revision) = match driver_snapshot(connection_id) {
            Ok(snapshot) => snapshot,
            Err(error) => return error_envelope("query.not_connected", error),
        };
        let (guard, control) = begin_tagged_operation(
            connection_id,
            request.operation_id.0,
            &driver,
            crate::mongo_command::COMMAND_TAG_PREFIX,
        );
        let outcome = driver.mongo_command(request, &control);
        drop(guard);
        match revision_is_current(connection_id, revision) {
            Ok(true) => {}
            Ok(false) => return error_envelope("query.stale", "connection changed during query"),
            Err(error) => return error_envelope("query.not_connected", error),
        }
        match outcome {
            Ok(reply) => success_json(serde_json::json!({
                "reply": crate::models::bson_canonical(&bson::Bson::Document(reply)),
            })),
            Err(CommandError::BadRequest(message)) => error_envelope("query.bad_request", &message),
            Err(CommandError::ReadOnlyViolation(message)) => {
                error_envelope(engine_driver_api::sql::codes::QUERY_READ_ONLY_VIOLATION, &message)
            }
            Err(CommandError::Cancelled) => error_envelope("query.cancelled", "operation cancelled"),
            Err(CommandError::Unsupported) => {
                error_envelope("query.unsupported", "this connection does not run raw commands")
            }
            Err(error @ (CommandError::Failed(_) | CommandError::Server { .. })) => {
                error_envelope("query.failed", &crate::admin::redact(&error.to_string()))
            }
        }
    })
}

/// `export.mongo_aggregate`: streams an aggregation (`query.kind =
/// "mongo_aggregate"`, `database`, `object`, `payload` as for the query kind)
/// through one cursor into `destination` (an absolute path) as `format`
/// (`csv`, `json` or `xlsx`). Returns `{ rows_written, bytes_written }`.
/// A pipeline ending in `$out` / `$merge` is refused. Cancel with
/// `dbengine_cancel_operation` and the request's `operation_id`.
#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_export(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    guarded("export.internal", || export_impl(connection_id, request_json))
}

#[cfg(feature = "abi-v2")]
fn export_impl(connection_id: u64, request_json: *const c_char) -> *mut c_char {
    use crate::driver::AggregateError;
    let request_json = match request_string(request_json, "request_json") {
        Ok(value) => value,
        Err(error) => return error_envelope("export.bad_request", &error),
    };
    let request: engine_driver_api::ExportRequest = match serde_json::from_str(&request_json) {
        Ok(value) => value,
        Err(error) => return error_envelope("export.bad_request", &error.to_string()),
    };
    let Some(query) = request.query.as_ref().filter(|query| query.kind == QUERY_AGGREGATE) else {
        return error_envelope(
            "export.unsupported",
            "this driver exports only mongo_aggregate queries",
        );
    };
    let (Some(database), Some(collection)) = (
        query.database.as_deref().filter(|name| !name.trim().is_empty()),
        query.object.as_deref().filter(|name| !name.is_empty()),
    ) else {
        return error_envelope("export.bad_request", "export needs a database and a collection");
    };
    let format = match crate::aggregate_export::parse_format(&request.format) {
        Ok(format) => format,
        Err(error) => return error_envelope("export.bad_request", &error),
    };
    let destination = std::path::PathBuf::from(&request.destination);
    if !destination.is_absolute() {
        return error_envelope("export.bad_request", "destination must be an absolute path");
    }
    let parsed = match crate::aggregate::parse_request_payload(&query.payload) {
        Ok(parsed) => parsed,
        Err(error) => return invalid_pipeline_envelope(&error),
    };
    let (driver, _revision) = match driver_snapshot(connection_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return error_envelope("export.not_connected", error),
    };
    let (guard, control) = begin_operation(connection_id, request.operation_id.0, &driver);
    let outcome =
        driver.export_aggregate(database, collection, &parsed, format, &destination, &control);
    drop(guard);
    match outcome {
        Ok(summary) => success_json(serde_json::json!({
            "rows_written": summary.rows,
            "bytes_written": summary.bytes,
        })),
        Err(AggregateError::Cancelled) => error_envelope("export.cancelled", "export cancelled"),
        Err(AggregateError::NeedsConfirmation(_)) => {
            error_envelope("export.failed", "export cannot run a pipeline that writes")
        }
        Err(AggregateError::Failed(message)) => {
            error_envelope("export.failed", &crate::admin::redact(&message))
        }
    }
}

/// Error envelope carrying structured `details` next to the message. The
/// host's `DriverError` ignores unknown fields, so this stays additive.
#[cfg(feature = "abi-v2")]
fn error_envelope_with_details(code: &str, message: &str, details: serde_json::Value) -> *mut c_char {
    let body = serde_json::json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "retryable": false,
            "details": details,
        }
    });
    string_to_c_ptr(&body.to_string())
}

#[cfg(feature = "abi-v2")]
fn invalid_pipeline_envelope(error: &crate::aggregate::PipelineError) -> *mut c_char {
    error_envelope_with_details(
        "query.invalid_pipeline",
        &error.to_string(),
        serde_json::json!({
            "stage": error.stage,
            "line": error.line,
            "col": error.col,
            "message": error.message,
        }),
    )
}

/// `mongo_aggregate` / `mongo_aggregate_explain`; payload contract in
/// [`crate::aggregate::AggregateRequest`] (+ `verbosity` for explain and the
/// usual `replacement_page_id`). Rows are cached like `mongo_find` pages.
#[cfg(feature = "abi-v2")]
fn aggregate_page(connection_id: u64, request: &QueryRequest) -> *mut c_char {
    use crate::driver::{AggregateError, AggregateOutput};
    let explain = request.kind == QUERY_AGGREGATE_EXPLAIN;
    let Some(database) = request
        .database
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    else {
        return error_envelope(
            "query.bad_request",
            "select a database before running an aggregation",
        );
    };
    let Some(collection) = request.object.as_deref().filter(|name| !name.is_empty()) else {
        return error_envelope("query.bad_request", "missing collection");
    };
    let replacement_page_id = match request.payload.get("replacement_page_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(page_id) => Some(page_id),
            None => {
                return error_envelope(
                    "query.bad_request",
                    "replacement_page_id must be an unsigned integer",
                )
            }
        },
    };
    let parsed = match crate::aggregate::parse_request_payload(&request.payload) {
        Ok(parsed) => parsed,
        Err(error) => return invalid_pipeline_envelope(&error),
    };
    let verbosity = match request
        .payload
        .get("verbosity")
        .and_then(serde_json::Value::as_str)
    {
        None => crate::aggregate::ExplainVerbosity::QueryPlanner,
        Some(text) => match crate::aggregate::ExplainVerbosity::parse(text) {
            Some(verbosity) => verbosity,
            None => {
                return error_envelope(
                    "query.bad_request",
                    &format!("unknown explain verbosity '{text}'"),
                )
            }
        },
    };
    let (driver, revision) = match driver_snapshot(connection_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return error_envelope("query.not_connected", error),
    };
    let (guard, control) = begin_operation(connection_id, request.operation_id.0, &driver);
    let outcome = if explain {
        driver
            .mongo_aggregate_explain(database, collection, &parsed, verbosity, &control)
            .map(|document| AggregateOutput {
                documents: vec![document],
                total_count: 1,
                total_is_exact: true,
            })
    } else {
        driver.mongo_aggregate(
            database,
            collection,
            &parsed,
            request.offset,
            request.limit,
            &control,
        )
    };
    drop(guard);
    let output = match outcome {
        Ok(output) => output,
        Err(AggregateError::NeedsConfirmation(write)) => {
            return error_envelope_with_details(
                "query.write_requires_confirmation",
                &format!(
                    "This pipeline writes to {} with {}; confirm to run it.",
                    write.target, write.operator
                ),
                serde_json::json!({
                    "stage": write.index + 1,
                    "operator": write.operator,
                    "target": write.target,
                }),
            )
        }
        Err(AggregateError::Cancelled) => {
            return error_envelope("query.cancelled", "operation cancelled")
        }
        Err(AggregateError::Failed(message)) => {
            return error_envelope("query.failed", &crate::admin::redact(&message))
        }
    };
    let Some(mtx) = SESSION.get() else {
        return error_envelope("query.not_connected", "session not initialized");
    };
    let mut session = lock_session(mtx);
    if session.revisions.get(&connection_id).copied() != Some(revision) {
        return error_envelope("query.stale", "connection changed during query");
    }
    let page = match replacement_page_id {
        Some(replacement_page_id) => session.pages.insert_replacement_for_connection(
            connection_id,
            replacement_page_id,
            request.operation_id,
            output.documents,
            Some(output.total_count),
            output.total_is_exact,
        ),
        None => session.pages.insert_for_connection(
            connection_id,
            request.operation_id,
            output.documents,
            Some(output.total_count),
            output.total_is_exact,
        ),
    };
    let Some(page) = page else {
        return error_envelope("query.page_cache_full", "all cached query pages are in use");
    };
    success_json(serde_json::to_value(page).expect("query page serializes"))
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_render_cell_batch(
    page_id: u64,
    row_start: u64,
    row_count: u64,
    column_start: u64,
    column_count: u64,
    out_batch: *mut *mut CCellTextBatch,
    out_error: *mut *mut c_char,
) -> c_int {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        render_cell_batch_unguarded(
            page_id,
            row_start,
            row_count,
            column_start,
            column_count,
            out_batch,
            out_error,
        )
    }));
    outcome.unwrap_or_else(|_| {
        if !out_error.is_null() {
            unsafe { *out_error = error_envelope("render.internal", "internal driver error") };
        }
        1
    })
}

#[cfg(feature = "abi-v2")]
unsafe fn render_cell_batch_unguarded(
    page_id: u64,
    row_start: u64,
    row_count: u64,
    column_start: u64,
    column_count: u64,
    out_batch: *mut *mut CCellTextBatch,
    out_error: *mut *mut c_char,
) -> c_int {
    if out_batch.is_null() {
        if !out_error.is_null() {
            unsafe { *out_error = error_envelope("render.bad_request", "null output batch") };
        }
        return 1;
    }
    let Some(mtx) = SESSION.get() else {
        if !out_error.is_null() {
            unsafe {
                *out_error = error_envelope("render.not_connected", "session not initialized")
            };
        }
        return 1;
    };
    let session = lock_session(mtx);
    let Some(rendered) = session.pages.render_cells(
        page_id,
        row_start as usize,
        row_count as usize,
        column_start as usize,
        column_count as usize,
    ) else {
        if !out_error.is_null() {
            unsafe { *out_error = error_envelope("render.page_not_found", "unknown page") };
        }
        return 1;
    };
    let cells = rendered.cells.into_boxed_slice();
    let cell_count = cells.len() as u64;
    let cells_ptr = if cells.is_empty() {
        std::ptr::null_mut()
    } else {
        Box::into_raw(cells) as *mut CCellText
    };
    let text = rendered.text.into_boxed_slice();
    let text_buffer_len = text.len() as u64;
    let text_buffer = if text.is_empty() {
        std::ptr::null_mut()
    } else {
        Box::into_raw(text) as *mut u8
    };
    let batch = Box::new(CCellTextBatch {
        cells: cells_ptr,
        cell_count,
        text_buffer,
        text_buffer_len,
    });
    unsafe { *out_batch = Box::into_raw(batch) };
    0
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_inspect_row(page_id: u64, row_index: u64) -> *mut c_char {
    guarded("inspect.internal", || inspect_row_unguarded(page_id, row_index))
}

#[cfg(feature = "abi-v2")]
fn inspect_row_unguarded(page_id: u64, row_index: u64) -> *mut c_char {
    let Some(mtx) = SESSION.get() else {
        return error_envelope("inspect.not_connected", "session not initialized");
    };
    let session = lock_session(mtx);
    let Some(row) = session.pages.inspect_row(page_id, row_index as usize) else {
        return error_envelope("inspect.row_not_found", "unknown page or row");
    };
    success_json(serde_json::to_value(row).expect("row fields serialize"))
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_mutate(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    guarded("mutation.internal", || unsafe { mutate_unguarded(connection_id, request_json) })
}

#[cfg(feature = "abi-v2")]
unsafe fn mutate_unguarded(
    connection_id: u64,
    request_json: *const c_char,
) -> *mut c_char {
    let request_json = match request_string(request_json, "request_json") {
        Ok(value) => value,
        Err(error) => return error_envelope("mutation.bad_request", &error),
    };
    let request: MutationRequest = match serde_json::from_str(&request_json) {
        Ok(value) => value,
        Err(error) => return error_envelope("mutation.bad_request", &error.to_string()),
    };
    let (driver, _revision) = match driver_snapshot(connection_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return error_envelope("mutation.not_connected", error),
    };
    // Registered for the whole mutation so `dbengine_cancel_operation` finds it.
    // A write is never interrupted mid-flight (its outcome would be unknown); a
    // cancel that landed before it starts stops it without writing.
    let (_operation, control) = begin_operation(connection_id, request.operation_id.0, &driver);
    if control.cancel.as_ref().is_some_and(crate::driver::CancelToken::is_cancelled) {
        return error_envelope("mutation.cancelled", "operation cancelled");
    }
    let affected_rows = if crate::admin::is_admin_mutation_kind(&request.kind) {
        let operation = match crate::admin::parse_admin_mutation(&request) {
            Ok(operation) => operation,
            Err(error) => return error_envelope("mutation.bad_request", &error),
        };
        match driver.admin_mutate(&operation) {
            Ok(value) => value,
            Err(error) => return error_envelope("mutation.failed", &crate::admin::redact(&error)),
        }
    } else if request.kind == "create_database" {
        match driver.create_database(&request) {
            Ok(()) => 0,
            Err(crate::driver::CreateDatabaseError::Rejected(error)) => {
                return error_envelope("mutation.create_database.rejected", &error)
            }
            Err(crate::driver::CreateDatabaseError::Unknown(error)) => {
                return error_envelope("mutation.create_database.unknown", &error)
            }
        }
    } else {
        match driver.mutate(&request) {
            Ok(value) => value,
            Err(error) => return error_envelope(error.code(), error.message()),
        }
    };
    let Some(mtx) = SESSION.get() else {
        return success_json(serde_json::json!({
            "affected_rows": affected_rows,
            "invalidated_page_ids": [],
        }));
    };
    let mut session = lock_session(mtx);
    if let Some(revision) = session.revisions.get_mut(&connection_id) {
        *revision = revision.wrapping_add(1);
    }
    // A write stales the pages read from its collection; an administrative
    // change (drop, rename, index) may reach anything on the connection, and a
    // new database stales nothing.
    let invalidated_page_ids = match (request.kind.as_str(), &request.database, &request.object) {
        ("create_database", ..) => Vec::new(),
        ("insert" | "update" | "delete", Some(database), Some(collection)) => {
            session.pages.clear_namespace(connection_id, database, collection)
        }
        _ => session.pages.clear_connection(connection_id),
    };
    success_json(serde_json::json!({
        "affected_rows": affected_rows,
        "invalidated_page_ids": invalidated_page_ids,
    }))
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub extern "C" fn dbengine_release_page(page_id: u64) -> *mut c_char {
    guarded("release.internal", || release_page_unguarded(page_id))
}

#[cfg(feature = "abi-v2")]
fn release_page_unguarded(page_id: u64) -> *mut c_char {
    let Some(mtx) = SESSION.get() else {
        return error_envelope("release.not_connected", "session not initialized");
    };
    let mut session = lock_session(mtx);
    if !session.pages.release(page_id) {
        return error_envelope("release.page_not_found", "unknown page");
    }
    success_json(serde_json::json!({}))
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(s));
    }
}

#[cfg(feature = "abi-v2")]
#[no_mangle]
pub unsafe extern "C" fn dbengine_free_cell_text_batch(batch: *mut CCellTextBatch) {
    if batch.is_null() {
        return;
    }
    unsafe {
        let batch = Box::from_raw(batch);
        if !batch.cells.is_null() {
            let _ = Vec::from_raw_parts(
                batch.cells as *mut CCellText,
                batch.cell_count as usize,
                batch.cell_count as usize,
            );
        }
        if !batch.text_buffer.is_null() {
            let _ = Vec::from_raw_parts(
                batch.text_buffer,
                batch.text_buffer_len as usize,
                batch.text_buffer_len as usize,
            );
        }
    }
}

fn success_json(value: serde_json::Value) -> *mut c_char {
    let envelope = SuccessEnvelope { ok: true, value };
    string_to_c_ptr(&serde_json::to_string(&envelope).expect("envelope serializes"))
}

fn request_string(ptr: *const c_char, name: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("null {name}"));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(str::to_owned)
        .map_err(|error| error.to_string())
}

fn error_envelope(code: &str, message: &str) -> *mut c_char {
    let body = serde_json::json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "retryable": false,
        }
    });
    string_to_c_ptr(&body.to_string())
}

#[cfg(all(test, feature = "abi-v2"))]
mod tests {
    use super::*;
    use crate::driver::{FindDocuments, MongoBackend};
    use bson::{doc, Document};
    use engine_driver_api::OperationId;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SERIAL: Mutex<()> = Mutex::new(());
    static NEXT_TEST_CONNECTION_ID: AtomicU64 = AtomicU64::new(50_000);

    fn test_serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().expect("test serial lock")
    }

    #[test]
    fn v2_capabilities_use_only_known_typed_tokens() {
        use engine_driver_api::{Capability, DriverCapabilities};
        let raw = dbengine_capabilities();
        // SAFETY: the driver returns a NUL-terminated string it owns until freed.
        let json = unsafe { std::ffi::CStr::from_ptr(raw) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: raw came from dbengine_capabilities and is freed once.
        unsafe { dbengine_free_string(raw) };
        let capabilities: DriverCapabilities = serde_json::from_str(&json).unwrap();
        for token in capabilities.features() {
            assert!(Capability::parse(token).is_some(), "untyped token {token}");
        }
        assert!(capabilities.has(Capability::QueryMongoScript));
        assert!(capabilities.has(Capability::QueryPageLease));
        assert!(capabilities.has(Capability::DatabaseCreate));
        assert!(capabilities.has_any_mutation() && capabilities.has_any_export());
        assert!(capabilities.sql_dialect.is_none());
    }

    struct TestConnectionCleanup([u64; 2]);

    impl Drop for TestConnectionCleanup {
        fn drop(&mut self) {
            for connection_id in self.0 {
                let response = dbengine_disconnect(connection_id);
                unsafe { dbengine_free_string(response) };
            }
        }
    }

    #[test]
    fn malformed_query_returns_structured_error() {
        let _serial = test_serial();
        let request = CString::new("{").expect("request");
        let response = unsafe { dbengine_open_query(1, request.as_ptr()) };
        let body = unsafe { CStr::from_ptr(response) }
            .to_str()
            .expect("response utf8")
            .to_string();
        unsafe { dbengine_free_string(response) };

        let value: serde_json::Value = serde_json::from_str(&body).expect("error json");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "query.bad_request");
    }

    #[test]
    fn malformed_replacement_page_id_returns_structured_query_error() {
        let _serial = test_serial();
        let request = CString::new(
            r#"{"kind":"mongo_find","operation_id":1,"database":"db","object":"items","offset":0,"limit":1,"payload":{"filter":{},"replacement_page_id":"7"}}"#,
        )
        .expect("request");
        let response = unsafe { dbengine_open_query(1, request.as_ptr()) };
        let body = unsafe { CStr::from_ptr(response) }
            .to_str()
            .expect("response utf8")
            .to_string();
        unsafe { dbengine_free_string(response) };

        let value: serde_json::Value = serde_json::from_str(&body).expect("error json");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "query.bad_request");
    }

    #[test]
    fn capabilities_advertise_mongo_mutations() {
        let _serial = test_serial();
        let response = dbengine_capabilities();
        let body = unsafe { CStr::from_ptr(response) }
            .to_str()
            .expect("capabilities utf8")
            .to_string();
        unsafe { dbengine_free_string(response) };

        let value: serde_json::Value = serde_json::from_str(&body).expect("capabilities json");
        assert!(value["features"]
            .as_array()
            .expect("features array")
            .iter()
            .any(|feature| feature == "mutation.update"));
        assert!(value["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "database.create"));
        assert!(value["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "query.mongo_script"));
    }

    struct TestBackend {
        filters: Option<Arc<Mutex<Vec<Document>>>>,
        created: Option<Arc<Mutex<Vec<String>>>>,
        windows: Option<Arc<Mutex<Vec<(u64, u64)>>>>,
        total_count: u64,
    }

    impl MongoBackend for TestBackend {
        fn list_database_names(&self) -> Result<Vec<String>, String> {
            Err("unused test backend operation".into())
        }

        fn create_database(&self, _name: &str) -> Result<(), crate::driver::CreateDatabaseError> {
            if let Some(created) = &self.created {
                created.lock().unwrap().push(_name.into());
                return Err(crate::driver::CreateDatabaseError::Unknown(
                    "response lost after accept".into(),
                ));
            }
            Err(crate::driver::CreateDatabaseError::Rejected(
                "unused test backend operation".into(),
            ))
        }

        fn list_collection_names(&self, _database: &str) -> Result<Vec<String>, String> {
            Err("unused test backend operation".into())
        }

        fn find_documents(
            &self,
            _database: &str,
            _collection: &str,
            filter: bson::Document,
            _sort: bson::Document,
            _projection: Option<bson::Document>,
            offset: u64,
            limit: u64,
        ) -> Result<FindDocuments, String> {
            let Some(filters) = &self.filters else {
                return Err("unused test backend operation".into());
            };
            filters.lock().expect("filter capture lock").push(filter);
            if let Some(windows) = &self.windows {
                windows.lock().unwrap().push((offset, limit));
            }
            Ok(FindDocuments {
                documents: vec![doc! { "value": "seed" }],
                total_count: self.total_count,
                total_is_exact: true,
            })
        }

        fn insert_document(
            &self,
            _database: &str,
            _collection: &str,
            _document: bson::Document,
        ) -> Result<u64, String> {
            Err("unused test backend operation".into())
        }

        fn update_document(
            &self,
            _database: &str,
            _collection: &str,
            _filter: bson::Document,
            _update: bson::Document,
        ) -> Result<u64, String> {
            Err("unused test backend operation".into())
        }

        fn delete_document(
            &self,
            _database: &str,
            _collection: &str,
            _filter: bson::Document,
        ) -> Result<u64, String> {
            Err("unused test backend operation".into())
        }
    }

    #[test]
    fn session_keeps_multiple_connections_independent() {
        let _serial = test_serial();
        let mut session = Session {
            drivers: HashMap::new(),
            revisions: HashMap::new(),
            pages: BsonPageCache::default(),
        };

        session.drivers.insert(
            7,
            Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                filters: None,
                created: None,
                windows: None,
                total_count: 1,
            }))),
        );
        session.drivers.insert(
            8,
            Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                filters: None,
                created: None,
                windows: None,
                total_count: 1,
            }))),
        );
        assert_eq!(session.drivers.len(), 2);

        session.drivers.remove(&7);
        assert!(session.drivers.contains_key(&8));
    }

    #[test]
    fn malformed_mutation_returns_structured_error() {
        let _serial = test_serial();
        let request = CString::new("{").expect("request");
        let response = unsafe { dbengine_mutate(1, request.as_ptr()) };
        let body = unsafe { CStr::from_ptr(response) }
            .to_str()
            .expect("response utf8")
            .to_string();
        unsafe { dbengine_free_string(response) };

        let value: serde_json::Value = serde_json::from_str(&body).expect("error json");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "mutation.bad_request");
    }

    #[test]
    fn mongo_script_query_parses_before_driver_dispatch_and_rejects_unsupported_statements() {
        let _serial = test_serial();
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let filters = Arc::new(Mutex::new(Vec::new()));
        {
            let mut session = SESSION.get().unwrap().lock().unwrap();
            session.drivers.insert(
                id,
                Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                    filters: Some(filters.clone()),
                    created: None,
                    windows: None,
                    total_count: 1,
                }))),
            );
            session.revisions.insert(id, 0);
        }
        let call = |script: &str| {
            let request = CString::new(serde_json::json!({
                "kind": "mongo_script", "operation_id": 41, "database": "fixture_db", "object": null,
                "offset": 0, "limit": 20, "payload": {"script":script}
            }).to_string()).unwrap();
            let response = unsafe { dbengine_open_query(id, request.as_ptr()) };
            let json: serde_json::Value =
                serde_json::from_str(unsafe { CStr::from_ptr(response) }.to_str().unwrap())
                    .unwrap();
            unsafe { dbengine_free_string(response) };
            json
        };
        let result = call("db.users.find({ active: true }).limit(5)");
        assert_eq!(result["ok"], true);
        assert_eq!(result["value"]["row_count"], 1);
        assert_eq!(filters.lock().unwrap()[0].get_bool("active").unwrap(), true);
        for unsupported in [
            "SELECT * FROM users",
            "db.users.deleteMany({})",
            "db.users.find({}); db.users.drop()",
            "db.users.findOne({}).sort({age:1})",
            "db.users.find({tenantId})",
            "db.users.find({...restriction})",
            "db.users.find({age: 1 + 2})",
        ] {
            let response = call(unsupported);
            assert_eq!(response["error"]["code"], "query.unsupported");
        }
        assert_eq!(filters.lock().unwrap().len(), 1);
    }

    #[test]
    fn mongo_script_paging_preserves_skip_limit_and_total_window() {
        let _serial = test_serial();
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let windows = Arc::new(Mutex::new(Vec::new()));
        let filters = Arc::new(Mutex::new(Vec::new()));
        {
            let mut session = SESSION.get().unwrap().lock().unwrap();
            session.drivers.insert(
                id,
                Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                    filters: Some(filters),
                    created: None,
                    windows: Some(windows.clone()),
                    total_count: 750,
                }))),
            );
            session.revisions.insert(id, 0);
        }
        for (offset, expected) in [(0, (2, 200)), (200, (202, 100))] {
            let request = CString::new(serde_json::json!({
                "kind":"mongo_script", "operation_id":offset + 10, "database":"fixture_db", "object":null,
                "offset":offset, "limit":200,
                "payload":{"script":"db.users.find({}).skip(2).limit(300)"}
            }).to_string()).unwrap();
            let response = unsafe { dbengine_open_query(id, request.as_ptr()) };
            let value: serde_json::Value =
                serde_json::from_str(unsafe { CStr::from_ptr(response) }.to_str().unwrap())
                    .unwrap();
            unsafe { dbengine_free_string(response) };
            assert_eq!(value["ok"], true);
            assert_eq!(value["value"]["total_count"], 300);
            assert_eq!(windows.lock().unwrap().last().copied(), Some(expected));
        }
    }

    #[test]
    fn create_database_ffi_propagates_backend_error_without_claiming_success() {
        let _serial = test_serial();
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let session = SESSION.get().unwrap();
        {
            let mut session = session.lock().unwrap();
            session.drivers.insert(
                id,
                Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                    filters: None,
                    created: None,
                    windows: None,
                    total_count: 1,
                }))),
            );
            session.revisions.insert(id, 0);
        }
        let request = CString::new(
            serde_json::json!({
                "operation_id": 1, "kind": "create_database", "database": "fixture", "object": null,
                "identity": null, "payload": null
            })
            .to_string(),
        )
        .unwrap();
        let response = unsafe { dbengine_mutate(id, request.as_ptr()) };
        let value: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(response) }.to_str().unwrap()).unwrap();
        unsafe { dbengine_free_string(response) };
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "mutation.create_database.rejected");
        assert_eq!(session.lock().unwrap().revisions[&id], 0);
    }

    #[test]
    fn mutate_registers_operation_and_requested_cancel_skips_the_write() {
        let _serial = test_serial();
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let created = Arc::new(Mutex::new(Vec::new()));
        {
            let mut session = SESSION.get().unwrap().lock().unwrap();
            session.drivers.insert(
                id,
                Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                    filters: None,
                    created: Some(created.clone()),
                    windows: None,
                    total_count: 1,
                }))),
            );
            session.revisions.insert(id, 0);
        }
        let call = |function: unsafe extern "C" fn(u64, *const c_char) -> *mut c_char,
                    request: serde_json::Value| {
            let request = CString::new(request.to_string()).unwrap();
            let response = unsafe { function(id, request.as_ptr()) };
            let value: serde_json::Value =
                serde_json::from_str(unsafe { CStr::from_ptr(response) }.to_str().unwrap())
                    .unwrap();
            unsafe { dbengine_free_string(response) };
            value
        };
        let mutation = |operation_id: u64| {
            serde_json::json!({
                "operation_id": operation_id, "kind": "create_database", "database": "fixture",
                "object": null, "identity": null, "payload": null
            })
        };
        // Cancel before the mutation starts: it never reaches the backend.
        let cancel = call(dbengine_cancel_operation, serde_json::json!({"operation_id": 5}));
        assert_eq!(cancel["value"]["cancelled"], false);
        let cancelled = call(dbengine_mutate, mutation(5));
        assert_eq!(cancelled["error"]["code"], "mutation.cancelled");
        assert!(created.lock().unwrap().is_empty());
        assert!(!operations().contains_key(&(id, 5)));
        // A later operation on the same connection still runs, and unregisters.
        let next = call(dbengine_mutate, mutation(6));
        assert_eq!(next["error"]["code"], "mutation.create_database.unknown");
        assert_eq!(created.lock().unwrap().as_slice(), ["fixture"]);
        assert!(!operations().contains_key(&(id, 6)));
    }

    #[test]
    fn create_database_ffi_reports_accepted_write_with_lost_response_as_unknown() {
        let _serial = test_serial();
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let created = Arc::new(Mutex::new(Vec::new()));
        let session = SESSION.get().unwrap();
        {
            let mut session = session.lock().unwrap();
            session.drivers.insert(
                id,
                Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                    filters: None,
                    created: Some(created.clone()),
                    windows: None,
                    total_count: 1,
                }))),
            );
            session.revisions.insert(id, 0);
        }
        let request = CString::new(
            serde_json::json!({
                "operation_id": 1, "kind": "create_database", "database": "fixture", "object": null,
                "identity": null, "payload": null
            })
            .to_string(),
        )
        .unwrap();
        let response = unsafe { dbengine_mutate(id, request.as_ptr()) };
        let value: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(response) }.to_str().unwrap()).unwrap();
        unsafe { dbengine_free_string(response) };
        assert_eq!(created.lock().unwrap().as_slice(), ["fixture"]);
        assert_eq!(value["error"]["code"], "mutation.create_database.unknown");
        assert_eq!(session.lock().unwrap().revisions[&id], 0);
    }

    #[test]
    fn unknown_page_render_and_inspect_return_errors() {
        let _serial = test_serial();
        unsafe { dbengine_init() };
        let mut batch = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        let status = unsafe { dbengine_render_cell_batch(999, 0, 1, 0, 1, &mut batch, &mut error) };
        assert_eq!(status, 1);
        let render_error = unsafe { CStr::from_ptr(error) }
            .to_str()
            .expect("render error utf8")
            .to_string();
        unsafe { dbengine_free_string(error) };
        assert!(render_error.contains("render.page_not_found"));

        let inspect = dbengine_inspect_row(999, 0);
        let inspect_error = unsafe { CStr::from_ptr(inspect) }
            .to_str()
            .expect("inspect error utf8")
            .to_string();
        unsafe { dbengine_free_string(inspect) };
        assert!(inspect_error.contains("inspect.row_not_found"));
    }

    #[test]
    fn seeded_ffi_query_preserves_raw_filter_and_replaces_at_full_cache() {
        let _serial = test_serial();
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };

        let connection_a = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let connection_b = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let _cleanup = TestConnectionCleanup([connection_a, connection_b]);
        let filters = Arc::new(Mutex::new(Vec::new()));
        let (old_page_id, page_b) = {
            let Some(mtx) = SESSION.get() else {
                panic!("session initialized");
            };
            let mut session = mtx.lock().expect("session lock");
            session.drivers.insert(
                connection_a,
                Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                    filters: Some(filters.clone()),
                    created: None,
                    windows: None,
                    total_count: 1,
                }))),
            );
            session.revisions.insert(connection_a, 0);
            session.drivers.insert(
                connection_b,
                Arc::new(MongoDriver::from_backend(Box::new(TestBackend {
                    filters: None,
                    created: None,
                    windows: None,
                    total_count: 1,
                }))),
            );
            session.revisions.insert(connection_b, 0);
            assert_eq!(session.pages.len(), 0, "seed cache must be empty");
            let mut old_page_id = None;
            for operation_id in 0..6 {
                let page = session
                    .pages
                    .insert_for_connection(
                        connection_a,
                        OperationId(operation_id),
                        vec![doc! { "seed": operation_id as i32 }],
                        None,
                        true,
                    )
                    .expect("A seed page fits");
                old_page_id.get_or_insert(page.page_id);
            }
            let page_b = session
                .pages
                .insert_for_connection(
                    connection_b,
                    OperationId(7),
                    vec![doc! { "seed": 7_i32 }],
                    None,
                    true,
                )
                .expect("B seed page fits");
            let old_page_id = old_page_id.expect("A seed page");
            (old_page_id, page_b)
        };

        let raw_request = serde_json::json!({
            "kind": "mongo_find",
            "operation_id": 100,
            "database": "db",
            "object": "items",
            "offset": 0,
            "limit": 1,
            "payload": {"replacement_page_id": 7}
        });
        let raw_request = CString::new(raw_request.to_string()).expect("raw request");
        let raw_response = unsafe { dbengine_open_query(connection_a, raw_request.as_ptr()) };
        let raw_body = unsafe { CStr::from_ptr(raw_response) }
            .to_str()
            .expect("raw response utf8")
            .to_string();
        unsafe { dbengine_free_string(raw_response) };
        let raw_value: serde_json::Value = serde_json::from_str(&raw_body).expect("raw json");
        assert_eq!(raw_value["ok"], true);
        assert_eq!(
            filters.lock().expect("filter capture lock").last(),
            Some(&doc! { "replacement_page_id": 7 })
        );
        let raw_page_id = raw_value["value"]["page_id"].as_u64().expect("raw page id");
        let released = dbengine_release_page(raw_page_id);
        unsafe { dbengine_free_string(released) };

        let raw_gt_request = serde_json::json!({
            "kind": "mongo_find",
            "operation_id": 102,
            "database": "db",
            "object": "items",
            "offset": 0,
            "limit": 1,
            "payload": {"replacement_page_id": {"$gt": 7}}
        });
        let raw_gt_request = CString::new(raw_gt_request.to_string()).expect("raw $gt request");
        let raw_gt_response = unsafe { dbengine_open_query(connection_a, raw_gt_request.as_ptr()) };
        let raw_gt_body = unsafe { CStr::from_ptr(raw_gt_response) }
            .to_str()
            .expect("raw $gt response utf8")
            .to_string();
        unsafe { dbengine_free_string(raw_gt_response) };
        let raw_gt_value: serde_json::Value =
            serde_json::from_str(&raw_gt_body).expect("raw $gt json");
        assert_eq!(raw_gt_value["ok"], true);
        assert_eq!(
            filters.lock().expect("filter capture lock").last(),
            Some(&doc! { "replacement_page_id": { "$gt": 7 } })
        );

        let replacement_request = serde_json::json!({
            "kind": "mongo_find",
            "operation_id": 101,
            "database": "db",
            "object": "items",
            "offset": 0,
            "limit": 1,
            "payload": {
                "filter": {"status": "active"},
                "replacement_page_id": old_page_id
            }
        });
        let replacement_request =
            CString::new(replacement_request.to_string()).expect("replacement request");
        let replacement_response =
            unsafe { dbengine_open_query(connection_a, replacement_request.as_ptr()) };
        let replacement_body = unsafe { CStr::from_ptr(replacement_response) }
            .to_str()
            .expect("replacement response utf8")
            .to_string();
        unsafe { dbengine_free_string(replacement_response) };
        let replacement_value: serde_json::Value =
            serde_json::from_str(&replacement_body).expect("replacement json");
        assert_eq!(replacement_value["ok"], true);

        let disconnected = dbengine_disconnect(connection_a);
        unsafe { dbengine_free_string(disconnected) };
        let inspect = dbengine_inspect_row(page_b.page_id, 0);
        let inspect_body = unsafe { CStr::from_ptr(inspect) }
            .to_str()
            .expect("B inspect response utf8")
            .to_string();
        unsafe { dbengine_free_string(inspect) };
        let inspect_value: serde_json::Value =
            serde_json::from_str(&inspect_body).expect("B inspect json");
        assert_eq!(inspect_value["ok"], true);
        assert_eq!(inspect_value["value"]["page_id"], page_b.page_id);
    }

    /// Records admin calls; answers `listIndexes` with fixed documents.
    struct AdminBackend {
        calls: Arc<Mutex<Vec<crate::admin::AdminMutation>>>,
    }

    impl MongoBackend for AdminBackend {
        fn list_database_names(&self) -> Result<Vec<String>, String> {
            Err("unused".into())
        }
        fn create_database(&self, _: &str) -> Result<(), crate::driver::CreateDatabaseError> {
            Err(crate::driver::CreateDatabaseError::Rejected(
                "unused".into(),
            ))
        }
        fn list_collection_names(&self, _: &str) -> Result<Vec<String>, String> {
            Err("unused".into())
        }
        fn find_documents(
            &self,
            _: &str,
            _: &str,
            _: Document,
            _: Document,
            _: Option<Document>,
            _: u64,
            _: u64,
        ) -> Result<FindDocuments, String> {
            Err("unused".into())
        }
        fn insert_document(&self, _: &str, _: &str, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn update_document(
            &self,
            _: &str,
            _: &str,
            _: Document,
            _: Document,
        ) -> Result<u64, String> {
            Err("unused".into())
        }
        fn delete_document(&self, _: &str, _: &str, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn admin_mutate(&self, operation: &crate::admin::AdminMutation) -> Result<u64, String> {
            self.calls.lock().unwrap().push(operation.clone());
            match operation {
                crate::admin::AdminMutation::CloneCollection { .. } => Ok(42),
                crate::admin::AdminMutation::DropDatabase { .. } => {
                    Err("boom mongodb://user:secret@host/db".into())
                }
                _ => Ok(1),
            }
        }
        fn list_indexes(&self, _: &str, collection: &str) -> Result<Vec<Document>, String> {
            assert_eq!(collection, "orders");
            Ok(vec![
                doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
                doc! { "v": 2, "key": { "email": 1 }, "name": "email_1", "unique": true },
            ])
        }
    }

    fn admin_connection(calls: Arc<Mutex<Vec<crate::admin::AdminMutation>>>) -> u64 {
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let mut session = SESSION.get().unwrap().lock().unwrap();
        session.drivers.insert(
            id,
            Arc::new(MongoDriver::from_backend(Box::new(AdminBackend { calls }))),
        );
        session.revisions.insert(id, 0);
        id
    }

    fn call_json(
        function: unsafe extern "C" fn(u64, *const c_char) -> *mut c_char,
        id: u64,
        request: serde_json::Value,
    ) -> serde_json::Value {
        let request = CString::new(request.to_string()).unwrap();
        let response = unsafe { function(id, request.as_ptr()) };
        let value = serde_json::from_str(unsafe { CStr::from_ptr(response) }.to_str().unwrap())
            .expect("response json");
        unsafe { dbengine_free_string(response) };
        value
    }

    #[test]
    fn capabilities_advertise_admin_features() {
        let _serial = test_serial();
        let response = dbengine_capabilities();
        let body = unsafe { CStr::from_ptr(response) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { dbengine_free_string(response) };
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let features = value["features"].as_array().unwrap();
        // `admin.*` gates the UI; the host additionally requires
        // `mutation.<kind>` for every mutate call, so each kind is advertised.
        for feature in [
            "admin.collection",
            "admin.indexes",
            "admin.database",
            "mutation.drop_collection",
            "mutation.rename_collection",
            "mutation.clone_collection",
            "mutation.create_index",
            "mutation.drop_index",
            "mutation.drop_database",
        ] {
            assert!(
                features.iter().any(|candidate| candidate == feature),
                "{feature}"
            );
        }
    }

    #[test]
    fn list_indexes_query_returns_inspectable_page() {
        let _serial = test_serial();
        let id = admin_connection(Arc::default());
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let value = call_json(
            dbengine_open_query,
            id,
            serde_json::json!({
                "kind": "mongo_list_indexes", "operation_id": 9, "database": "app",
                "object": "orders", "offset": 0, "limit": 100, "payload": null
            }),
        );
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["value"]["row_count"], 2);
        let page_id = value["value"]["page_id"].as_u64().unwrap();
        let row = dbengine_inspect_row(page_id, 1);
        let row: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(row) }.to_str().unwrap()).unwrap();
        let fields = row["value"]["fields"].as_array().unwrap();
        let name = fields.iter().find(|field| field["name"] == "name").unwrap();
        assert_eq!(name["value"], "email_1");
        let released = dbengine_release_page(page_id);
        unsafe { dbengine_free_string(released) };

        let missing = call_json(
            dbengine_open_query,
            id,
            serde_json::json!({
                "kind": "mongo_list_indexes", "operation_id": 10, "database": "app",
                "object": null, "offset": 0, "limit": 100, "payload": null
            }),
        );
        assert_eq!(missing["error"]["code"], "query.bad_request");
    }

    #[test]
    fn admin_mutations_validate_before_dispatch_and_bump_revision_on_success() {
        let _serial = test_serial();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let id = admin_connection(calls.clone());
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let mutate = |kind: &str,
                      database: Option<&str>,
                      object: Option<&str>,
                      payload: serde_json::Value| {
            call_json(
                dbengine_mutate,
                id,
                serde_json::json!({
                    "operation_id": 1, "kind": kind, "database": database, "object": object,
                    "identity": null, "payload": payload
                }),
            )
        };
        let refused = mutate(
            "drop_index",
            Some("app"),
            Some("orders"),
            serde_json::json!({"name": "_id_"}),
        );
        assert_eq!(refused["error"]["code"], "mutation.bad_request");
        let refused = mutate(
            "rename_collection",
            Some("app"),
            Some("orders"),
            serde_json::json!({"to": "bad$name"}),
        );
        assert_eq!(refused["error"]["code"], "mutation.bad_request");
        assert!(
            calls.lock().unwrap().is_empty(),
            "invalid requests never reach the backend"
        );
        let session = SESSION.get().unwrap();
        assert_eq!(session.lock().unwrap().revisions[&id], 0);

        let cloned = mutate(
            "clone_collection",
            Some("app"),
            Some("orders"),
            serde_json::json!({"to": "orders_copy", "include_indexes": true}),
        );
        assert_eq!(cloned["ok"], true, "{cloned}");
        assert_eq!(cloned["value"]["affected_rows"], 42);
        assert_eq!(session.lock().unwrap().revisions[&id], 1);
        assert!(matches!(
            calls.lock().unwrap()[0],
            crate::admin::AdminMutation::CloneCollection {
                include_indexes: true,
                ..
            }
        ));

        let failed = mutate(
            "drop_database",
            Some("sales"),
            None,
            serde_json::Value::Null,
        );
        assert_eq!(failed["error"]["code"], "mutation.failed");
        let message = failed["error"]["message"].as_str().unwrap();
        assert!(
            !message.contains("secret"),
            "secrets must be redacted: {message}"
        );
    }

    // ---- mongo_aggregate / explain / cancel ------------------------------

    struct AggregateBackend {
        calls: Arc<Mutex<Vec<Vec<Document>>>>,
        killed: Arc<Mutex<Vec<String>>>,
        result: Result<Vec<Document>, String>,
        block_until_cancelled: bool,
        panic_on_call: bool,
    }

    impl AggregateBackend {
        fn new(result: Result<Vec<Document>, String>) -> Self {
            Self {
                calls: Arc::default(),
                killed: Arc::default(),
                result,
                block_until_cancelled: false,
                panic_on_call: false,
            }
        }
    }

    impl MongoBackend for AggregateBackend {
        fn list_database_names(&self) -> Result<Vec<String>, String> {
            Err("unused".into())
        }
        fn create_database(&self, _: &str) -> Result<(), crate::driver::CreateDatabaseError> {
            Err(crate::driver::CreateDatabaseError::Rejected("unused".into()))
        }
        fn list_collection_names(&self, _: &str) -> Result<Vec<String>, String> {
            Err("unused".into())
        }
        fn find_documents(
            &self,
            _: &str,
            _: &str,
            _: Document,
            _: Document,
            _: Option<Document>,
            _: u64,
            _: u64,
        ) -> Result<FindDocuments, String> {
            Err("unused".into())
        }
        fn insert_document(&self, _: &str, _: &str, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn update_document(&self, _: &str, _: &str, _: Document, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn delete_document(&self, _: &str, _: &str, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn aggregate_documents(
            &self,
            _: &str,
            _: &str,
            pipeline: Vec<Document>,
            options: &crate::driver::AggOpts,
        ) -> Result<Vec<Document>, String> {
            self.calls.lock().unwrap().push(pipeline);
            if self.panic_on_call {
                panic!("backend bug");
            }
            if self.block_until_cancelled {
                let token = options.cancel.clone().expect("cancel token supplied");
                let started = std::time::Instant::now();
                while !token.is_cancelled() {
                    assert!(started.elapsed().as_secs() < 10, "never cancelled");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                return Err(crate::driver::CANCELLED_MESSAGE.into());
            }
            self.result.clone()
        }
        fn aggregate_stream(
            &self,
            _: &str,
            _: &str,
            pipeline: Vec<Document>,
            _: &crate::driver::AggOpts,
            sink: &mut (dyn FnMut(Document) -> Result<(), String> + Send),
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push(pipeline);
            if self.panic_on_call {
                panic!("backend bug");
            }
            for document in self.result.clone()? {
                sink(document)?;
            }
            Ok(())
        }
        fn explain_aggregate(
            &self,
            _: &str,
            _: &str,
            pipeline: Vec<Document>,
            verbosity: crate::aggregate::ExplainVerbosity,
            _: &crate::driver::AggOpts,
        ) -> Result<Document, String> {
            self.calls.lock().unwrap().push(pipeline);
            Ok(doc! { "verbosity": verbosity.as_str(), "queryPlanner": { "winningPlan": { "stage": "COLLSCAN" } } })
        }
        fn kill_operation(&self, comment: &str) {
            self.killed.lock().unwrap().push(comment.into());
        }
    }

    fn aggregate_connection(backend: AggregateBackend) -> u64 {
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let mut session = SESSION.get().unwrap().lock().unwrap();
        session
            .drivers
            .insert(id, Arc::new(MongoDriver::from_backend(Box::new(backend))));
        session.revisions.insert(id, 0);
        id
    }

    fn aggregate_query(
        id: u64,
        operation: u64,
        kind: &str,
        payload: serde_json::Value,
    ) -> serde_json::Value {
        call_json(
            dbengine_open_query,
            id,
            serde_json::json!({
                "kind": kind, "operation_id": operation, "database": "app",
                "object": "orders", "offset": 0, "limit": 50, "payload": payload
            }),
        )
    }

    #[test]
    fn capabilities_advertise_aggregate_explain_and_cancel() {
        let _serial = test_serial();
        let response = dbengine_capabilities();
        let body = unsafe { CStr::from_ptr(response) }.to_str().unwrap().to_string();
        unsafe { dbengine_free_string(response) };
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let features = value["features"].as_array().unwrap();
        for feature in ["query.mongo_aggregate", "query.mongo_aggregate_explain", "operation.cancel"] {
            assert!(features.iter().any(|candidate| candidate == feature), "{feature}");
        }
        assert_eq!(value["abi_version"], 2, "additive: no ABI bump");
    }

    #[test]
    fn aggregate_preview_returns_an_inspectable_inexact_page() {
        let _serial = test_serial();
        let backend = AggregateBackend::new(Ok(vec![doc! { "_id": 1, "n": "a" }, doc! { "_id": 2, "n": "b" }]));
        let calls = backend.calls.clone();
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let value = aggregate_query(
            id,
            31,
            "mongo_aggregate",
            serde_json::json!({ "pipeline": "[{ $match: { a: 1 } }, { $limit: 99 }]", "mode": "preview", "sample_size": 10 }),
        );
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["value"]["row_count"], 2);
        assert_eq!(value["value"]["total_is_exact"], false);
        assert_eq!(calls.lock().unwrap()[0].last().unwrap(), &doc! { "$limit": 10_i64 });
        let page_id = value["value"]["page_id"].as_u64().unwrap();
        let row = dbengine_inspect_row(page_id, 1);
        let row: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(row) }.to_str().unwrap()).unwrap();
        assert_eq!(row["value"]["fields"][1]["value"], "b");
        let released = dbengine_release_page(page_id);
        unsafe { dbengine_free_string(released) };
        assert!(operations().get(&(id, 31)).is_none(), "operation registry entry is removed");
    }

    #[test]
    fn aggregate_replacement_page_id_swaps_the_cached_page() {
        let _serial = test_serial();
        let id = aggregate_connection(AggregateBackend::new(Ok(vec![doc! { "_id": 1 }])));
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let payload = serde_json::json!({ "pipeline": "[]", "mode": "preview" });
        let first = aggregate_query(id, 41, "mongo_aggregate", payload.clone());
        let first_page = first["value"]["page_id"].as_u64().unwrap();
        let mut again = payload;
        again["replacement_page_id"] = first_page.into();
        let second = aggregate_query(id, 42, "mongo_aggregate", again);
        assert_eq!(second["ok"], true, "{second}");
        assert_ne!(second["value"]["page_id"].as_u64().unwrap(), first_page);
        let bad = aggregate_query(
            id,
            43,
            "mongo_aggregate",
            serde_json::json!({ "pipeline": "[]", "replacement_page_id": "x" }),
        );
        assert_eq!(bad["error"]["code"], "query.bad_request");
    }

    #[test]
    fn invalid_pipeline_returns_structured_stage_line_col_without_reaching_the_backend() {
        let _serial = test_serial();
        let backend = AggregateBackend::new(Ok(vec![]));
        let calls = backend.calls.clone();
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let value = aggregate_query(
            id,
            51,
            "mongo_aggregate",
            serde_json::json!({ "pipeline": "[\n  { $match: { a: 1 } },\n  { $project: { b: } }\n]" }),
        );
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "query.invalid_pipeline");
        let details = &value["error"]["details"];
        assert_eq!((details["stage"].as_u64(), details["line"].as_u64()), (Some(2), Some(3)));
        assert!(details["col"].as_u64().unwrap() > 1);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn write_pipeline_needs_explicit_confirmation() {
        let _serial = test_serial();
        let backend = AggregateBackend::new(Ok(vec![]));
        let calls = backend.calls.clone();
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let payload = serde_json::json!({ "pipeline": "[{ $match: {} }, { $out: 'copy' }]", "mode": "full" });
        let refused = aggregate_query(id, 61, "mongo_aggregate", payload.clone());
        assert_eq!(refused["error"]["code"], "query.write_requires_confirmation", "{refused}");
        assert_eq!(refused["error"]["details"]["target"], "copy");
        assert_eq!(refused["error"]["details"]["operator"], "$out");
        assert!(calls.lock().unwrap().is_empty());
        let mut confirmed = payload;
        confirmed["confirm_write"] = true.into();
        let ran = aggregate_query(id, 62, "mongo_aggregate", confirmed);
        assert_eq!(ran["ok"], true, "{ran}");
        assert_eq!(calls.lock().unwrap()[0].len(), 2, "no window appended after $out");
        // Previews of the same pipeline never need (or perform) the write.
        let preview = aggregate_query(
            id, 63, "mongo_aggregate",
            serde_json::json!({ "pipeline": "[{ $match: {} }, { $out: 'copy' }]", "mode": "preview" }),
        );
        assert_eq!(preview["ok"], true, "{preview}");
        let sent = calls.lock().unwrap()[1].clone();
        assert!(sent.iter().all(|stage| stage.keys().next().unwrap() != "$out"));
    }

    #[test]
    fn backend_errors_fail_the_query_and_special_characters_survive_the_envelope() {
        let _serial = test_serial();
        let nasty = "cursor error: bad \"quote\" back\\slash\nnewline\ttab";
        let id = aggregate_connection(AggregateBackend::new(Err(nasty.into())));
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let value = aggregate_query(id, 71, "mongo_aggregate", serde_json::json!({ "pipeline": "[]" }));
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "query.failed");
        assert_eq!(value["error"]["message"], nasty, "built with serde_json, not string escaping");
        let secret = aggregate_connection(AggregateBackend::new(Err(
            "auth failed for mongodb://user:pw@host/db".into(),
        )));
        let _cleanup2 = TestConnectionCleanup([secret, u64::MAX]);
        let value = aggregate_query(secret, 72, "mongo_aggregate", serde_json::json!({ "pipeline": "[]" }));
        assert!(!value["error"]["message"].as_str().unwrap().contains("pw@"));
    }

    #[test]
    fn explain_returns_the_plan_as_a_one_row_page() {
        let _serial = test_serial();
        let backend = AggregateBackend::new(Ok(vec![]));
        let calls = backend.calls.clone();
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let value = aggregate_query(
            id,
            81,
            "mongo_aggregate_explain",
            serde_json::json!({ "pipeline": "[{ $match: {} }, { $out: 'x' }]", "verbosity": "executionStats" }),
        );
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["value"]["row_count"], 1);
        assert_eq!(value["value"]["total_is_exact"], true);
        assert_eq!(calls.lock().unwrap()[0].len(), 1, "executionStats strips $out");
        let page_id = value["value"]["page_id"].as_u64().unwrap();
        let row = dbengine_inspect_row(page_id, 0);
        let row: serde_json::Value =
            serde_json::from_str(unsafe { CStr::from_ptr(row) }.to_str().unwrap()).unwrap();
        let names: Vec<_> = row["value"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["verbosity", "queryPlanner"]);
        let bad = aggregate_query(
            id, 82, "mongo_aggregate_explain",
            serde_json::json!({ "pipeline": "[]", "verbosity": "loud" }),
        );
        assert_eq!(bad["error"]["code"], "query.bad_request");
    }

    #[test]
    fn cancel_operation_stops_a_running_aggregate_and_tags_killop() {
        let _serial = test_serial();
        let mut backend = AggregateBackend::new(Ok(vec![]));
        backend.block_until_cancelled = true;
        let killed = backend.killed.clone();
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let runner = std::thread::spawn(move || {
            aggregate_query(id, 91, "mongo_aggregate", serde_json::json!({ "pipeline": "[]" }))
        });
        let started = std::time::Instant::now();
        while operations().get(&(id, 91)).is_none() {
            assert!(started.elapsed().as_secs() < 10, "operation never registered");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ack = call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": 91 }));
        assert_eq!(ack["ok"], true);
        assert_eq!(ack["value"]["cancelled"], true);
        let result = runner.join().unwrap();
        assert_eq!(result["error"]["code"], "query.cancelled", "{result}");
        assert_eq!(*killed.lock().unwrap(), vec![operation_tag("tablex:", id, 91)]);
        // Cancelling something that already finished is not an error.
        let again = call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": 91 }));
        assert_eq!(again["ok"], true);
        assert_eq!(again["value"]["cancelled"], false);
        let malformed = call_json(dbengine_cancel_operation, id, serde_json::json!({ "nope": 1 }));
        assert_eq!(malformed["error"]["code"], "cancel.bad_request");
    }

    #[test]
    fn disconnect_cancels_in_flight_operations() {
        let _serial = test_serial();
        let mut backend = AggregateBackend::new(Ok(vec![]));
        backend.block_until_cancelled = true;
        let id = aggregate_connection(backend);
        let runner = std::thread::spawn(move || {
            aggregate_query(id, 95, "mongo_aggregate", serde_json::json!({ "pipeline": "[]" }))
        });
        let started = std::time::Instant::now();
        while operations().get(&(id, 95)).is_none() {
            assert!(started.elapsed().as_secs() < 10);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let response = dbengine_disconnect(id);
        unsafe { dbengine_free_string(response) };
        assert_eq!(runner.join().unwrap()["error"]["code"], "query.cancelled");
    }

    fn export_request(operation: u64, destination: &str, format: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "operation_id": operation, "format": format, "destination": destination,
            "page_id": null, "options": null,
            "query": {
                "kind": "mongo_aggregate", "operation_id": operation, "database": "app",
                "object": "orders", "offset": 0, "limit": 1, "payload": payload
            }
        })
    }

    #[test]
    fn export_streams_one_cursor_into_a_file_and_reports_rows() {
        let _serial = test_serial();
        let backend = AggregateBackend::new(Ok(vec![
            doc! { "_id": 1, "name": "a" },
            doc! { "_id": 2, "name": "b", "extra": true },
        ]));
        let calls = backend.calls.clone();
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let dir = std::env::temp_dir().join(format!("tablex-ffi-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("out.csv");
        let value = call_json(
            dbengine_export,
            id,
            export_request(101, path.to_str().unwrap(), "csv", serde_json::json!({ "pipeline": "[{ $match: {} }]" })),
        );
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["value"]["rows_written"], 2);
        assert!(value["value"]["bytes_written"].as_u64().unwrap() > 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().next().unwrap(), "_id,name,extra");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one cursor");
        assert_eq!(calls[0], vec![doc! { "$match": {} }], "no $limit / $skip window on an export");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_refuses_write_pipelines_bad_paths_and_surfaces_errors() {
        let _serial = test_serial();
        let id = aggregate_connection(AggregateBackend::new(Ok(vec![doc! { "_id": 1 }])));
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let dir = std::env::temp_dir().join(format!("tablex-ffi-export2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("out.csv");
        let refused = call_json(
            dbengine_export,
            id,
            export_request(111, path.to_str().unwrap(), "csv", serde_json::json!({ "pipeline": "[{ $out: 'copy' }]" })),
        );
        assert_eq!(refused["error"]["code"], "export.failed", "{refused}");
        assert!(refused["error"]["message"].as_str().unwrap().contains("writes"));
        assert!(!path.exists());
        let relative = call_json(
            dbengine_export, id,
            export_request(112, "relative.csv", "csv", serde_json::json!({ "pipeline": "[]" })),
        );
        assert_eq!(relative["error"]["code"], "export.bad_request");
        let format = call_json(
            dbengine_export, id,
            export_request(113, path.to_str().unwrap(), "sql_insert", serde_json::json!({ "pipeline": "[]" })),
        );
        assert_eq!(format["error"]["code"], "export.bad_request");
        let invalid = call_json(
            dbengine_export, id,
            export_request(114, path.to_str().unwrap(), "csv", serde_json::json!({ "pipeline": "[{ $match: { a: } }]" })),
        );
        assert_eq!(invalid["error"]["code"], "query.invalid_pipeline");
        let other_kind = call_json(
            dbengine_export, id,
            serde_json::json!({
                "operation_id": 115, "format": "csv", "destination": path.to_str().unwrap(),
                "page_id": 1, "query": null, "options": null
            }),
        );
        assert_eq!(other_kind["error"]["code"], "export.unsupported");

        let failing = aggregate_connection(AggregateBackend::new(Err("cursor error: server went away".into())));
        let _cleanup2 = TestConnectionCleanup([failing, u64::MAX]);
        let error = call_json(
            dbengine_export, failing,
            export_request(116, path.to_str().unwrap(), "csv", serde_json::json!({ "pipeline": "[]" })),
        );
        assert_eq!(error["error"]["code"], "export.failed");
        assert!(error["error"]["message"].as_str().unwrap().contains("server went away"));
        assert!(!path.exists(), "a failed export leaves no file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cancel_that_arrives_before_the_operation_registers_is_not_lost() {
        let _serial = test_serial();
        let backend = AggregateBackend::new(Ok(vec![doc! { "_id": 1 }]));
        let calls = backend.calls.clone();
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        // The host cancels operation 700 before open_query for it got going.
        let ack = call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": 700 }));
        assert_eq!((ack["ok"].as_bool(), ack["value"]["cancelled"].as_bool()), (Some(true), Some(false)));
        let result = aggregate_query(id, 700, "mongo_aggregate", serde_json::json!({ "pipeline": "[]" }));
        assert_eq!(result["error"]["code"], "query.cancelled", "{result}");
        assert!(calls.lock().unwrap().is_empty(), "the cancelled operation never reached the server");
        assert!(!early_cancels().contains(&(id, 700)), "the tombstone is consumed");
        // An unrelated operation id is unaffected.
        let other = aggregate_query(id, 701, "mongo_aggregate", serde_json::json!({ "pipeline": "[]" }));
        assert_eq!(other["ok"], true, "{other}");
        // Export honours it too.
        let dir = std::env::temp_dir().join(format!("tablex-early-{}", std::process::id()));
        call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": 702 }));
        let exported = call_json(
            dbengine_export,
            id,
            export_request(702, dir.join("x.csv").to_str().unwrap(), "csv", serde_json::json!({ "pipeline": "[]" })),
        );
        assert_eq!(exported["error"]["code"], "export.cancelled", "{exported}");
        assert!(!dir.join("x.csv").exists());
    }

    #[test]
    fn early_cancel_memory_is_bounded_and_dropped_with_the_connection() {
        let _serial = test_serial();
        let id = aggregate_connection(AggregateBackend::new(Ok(vec![])));
        for operation in 0..(MAX_EARLY_CANCELS as u64 + 50) {
            call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": 10_000 + operation }));
        }
        let held = early_cancels().iter().filter(|(connection, _)| *connection == id).count();
        assert!(held <= MAX_EARLY_CANCELS, "{held}");
        assert!(!early_cancels().contains(&(id, 10_000)), "the oldest entries aged out");
        let response = dbengine_disconnect(id);
        unsafe { dbengine_free_string(response) };
        assert_eq!(early_cancels().iter().filter(|(connection, _)| *connection == id).count(), 0);
    }

    #[test]
    fn a_panic_in_a_query_or_export_becomes_an_error_envelope_not_an_abort() {
        let _serial = test_serial();
        let mut backend = AggregateBackend::new(Ok(vec![]));
        backend.panic_on_call = true;
        let id = aggregate_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let query = aggregate_query(id, 800, "mongo_aggregate", serde_json::json!({ "pipeline": "[]" }));
        assert_eq!(query["error"]["code"], "query.internal", "{query}");
        assert!(operations().get(&(id, 800)).is_none(), "the registry entry is cleaned up on unwind");
        let dir = std::env::temp_dir().join(format!("tablex-panic-{}", std::process::id()));
        let export = call_json(
            dbengine_export,
            id,
            export_request(801, dir.join("p.csv").to_str().unwrap(), "csv", serde_json::json!({ "pipeline": "[]" })),
        );
        assert_eq!(export["error"]["code"], "export.internal", "{export}");
        assert!(operations().get(&(id, 801)).is_none());
        // The connection is still usable afterwards.
        let session = SESSION.get().unwrap().lock().unwrap();
        assert!(session.drivers.contains_key(&id));
    }

    // ---- mongo_command ----------------------------------------------------

    struct CommandBackend {
        seen: Arc<Mutex<Vec<(String, Document)>>>,
        reply: Result<Document, crate::mongo_command::CommandError>,
        block_until_cancelled: bool,
        wire: Option<i32>,
    }

    impl CommandBackend {
        fn replying(reply: Document) -> Self {
            Self {
                seen: Arc::default(),
                reply: Ok(reply),
                block_until_cancelled: false,
                wire: None,
            }
        }
    }

    impl MongoBackend for CommandBackend {
        fn list_database_names(&self) -> Result<Vec<String>, String> {
            Err("unused".into())
        }
        fn create_database(&self, _: &str) -> Result<(), crate::driver::CreateDatabaseError> {
            Err(crate::driver::CreateDatabaseError::Rejected("unused".into()))
        }
        fn list_collection_names(&self, _: &str) -> Result<Vec<String>, String> {
            Err("unused".into())
        }
        fn find_documents(
            &self,
            _: &str,
            _: &str,
            _: Document,
            _: Document,
            _: Option<Document>,
            _: u64,
            _: u64,
        ) -> Result<FindDocuments, String> {
            Err("unused".into())
        }
        fn insert_document(&self, _: &str, _: &str, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn update_document(&self, _: &str, _: &str, _: Document, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn delete_document(&self, _: &str, _: &str, _: Document) -> Result<u64, String> {
            Err("unused".into())
        }
        fn max_wire_version(&self, _: &crate::driver::AggControl) -> Option<i32> {
            self.wire
        }
        fn run_command(
            &self,
            database: &str,
            command: Document,
            control: &crate::driver::AggControl,
            _: Option<std::time::Duration>,
        ) -> Result<Document, crate::mongo_command::CommandError> {
            self.seen.lock().unwrap().push((database.into(), command));
            if self.block_until_cancelled {
                let token = control.cancel.clone().expect("cancel token supplied");
                let started = std::time::Instant::now();
                while !token.is_cancelled() {
                    assert!(started.elapsed().as_secs() < 10, "never cancelled");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                return Err(crate::mongo_command::CommandError::Cancelled);
            }
            match &self.reply {
                Ok(reply) => Ok(reply.clone()),
                Err(crate::mongo_command::CommandError::Server { code, code_name, message }) => {
                    Err(crate::mongo_command::CommandError::Server {
                        code: *code,
                        code_name: code_name.clone(),
                        message: message.clone(),
                    })
                }
                Err(other) => Err(crate::mongo_command::CommandError::Failed(other.to_string())),
            }
        }
    }

    fn command_connection(backend: CommandBackend) -> u64 {
        aggregate_connection_with(Box::new(backend))
    }

    fn aggregate_connection_with(backend: Box<dyn MongoBackend>) -> u64 {
        let init = dbengine_init();
        unsafe { dbengine_free_string(init) };
        let id = NEXT_TEST_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        let mut session = SESSION.get().unwrap().lock().unwrap();
        session.drivers.insert(id, Arc::new(MongoDriver::from_backend(backend)));
        session.revisions.insert(id, 0);
        id
    }

    fn command_call(id: u64, operation: u64, request: serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({ "operation_id": operation, "database": "app" });
        body.as_object_mut().unwrap().extend(request.as_object().unwrap().clone());
        call_json(dbengine_mongo_command, id, body)
    }

    #[test]
    fn capabilities_advertise_mongo_command() {
        let _serial = test_serial();
        let response = dbengine_capabilities();
        let body = unsafe { CStr::from_ptr(response) }.to_str().unwrap().to_string();
        unsafe { dbengine_free_string(response) };
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(value["features"].as_array().unwrap().iter().any(|f| f == "mongo.command"));
        assert_eq!(value["abi_version"], 2, "additive: no ABI bump");
    }

    #[test]
    fn mongo_command_round_trips_canonical_values_in_the_servers_key_order() {
        let _serial = test_serial();
        let backend = CommandBackend::replying(doc! {
            "zeta": 1_i64, "alpha": "text",
            "cursor": { "id": 77_i64, "ns": "app.c", "firstBatch": [{ "_id": 1, "b": 2, "a": 3 }] },
            "ok": 1.0,
        });
        let seen = backend.seen.clone();
        let id = command_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let reply = command_call(
            id,
            900,
            serde_json::json!({
                "command": { "find": "c", "filter": { "n": { "$numberLong": "9007199254740993" } },
                              "when": { "$date": { "$numberLong": "-1" } } },
                "max_time_ms": 1500,
            }),
        );
        assert_eq!(reply["ok"], true, "{reply}");
        let text = reply["value"]["reply"].to_string();
        assert!(text.find("zeta").unwrap() < text.find("alpha").unwrap(), "key order kept: {text}");
        assert_eq!(reply["value"]["reply"]["zeta"]["$numberLong"], "1");
        assert_eq!(reply["value"]["reply"]["cursor"]["id"]["$numberLong"], "77");
        let (database, command) = seen.lock().unwrap().remove(0);
        assert_eq!(database, "app");
        assert_eq!(command.keys().next().map(String::as_str), Some("find"));
        assert_eq!(command.get_document("filter").unwrap().get_i64("n"), Ok(9_007_199_254_740_993));
        assert_eq!(command.get_datetime("when").unwrap().timestamp_millis(), -1);
        assert_eq!(
            command.get_str("comment"),
            Ok(operation_tag(crate::mongo_command::COMMAND_TAG_PREFIX, id, 900).as_str())
        );
        assert_eq!(command.get_i64("maxTimeMS"), Ok(1500));
    }

    #[test]
    fn mongo_command_read_only_refuses_before_the_backend_is_reached() {
        let _serial = test_serial();
        let backend = CommandBackend::replying(doc! { "ok": 1.0 });
        let seen = backend.seen.clone();
        let id = command_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        for command in [
            serde_json::json!({ "insert": "c", "documents": [{ "a": 1 }] }),
            serde_json::json!({ "drop": "c" }),
            serde_json::json!({ "aggregate": "c", "pipeline": [{ "$out": "x" }], "cursor": {} }),
        ] {
            let reply = command_call(id, 901, serde_json::json!({ "command": command, "read_only": true }));
            assert_eq!(reply["error"]["code"], "query.read_only_violation", "{reply}");
        }
        assert!(seen.lock().unwrap().is_empty(), "nothing was sent");
        let allowed = command_call(
            id,
            902,
            serde_json::json!({ "command": { "find": "c" }, "read_only": true }),
        );
        assert_eq!(allowed["ok"], true, "{allowed}");
        let writable = command_call(id, 903, serde_json::json!({ "command": { "drop": "c" } }));
        assert_eq!(writable["ok"], true, "{writable}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn mongo_command_server_refusals_are_replies_and_bad_requests_are_errors() {
        let _serial = test_serial();
        let backend = CommandBackend {
            seen: Arc::default(),
            reply: Err(crate::mongo_command::CommandError::Server {
                code: 26,
                code_name: "NamespaceNotFound".into(),
                message: "ns not found".into(),
            }),
            block_until_cancelled: false,
            wire: None,
        };
        let id = command_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let reply = command_call(id, 904, serde_json::json!({ "command": { "drop": "gone" } }));
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["value"]["reply"]["ok"]["$numberDouble"], "0.0");
        assert_eq!(reply["value"]["reply"]["code"]["$numberInt"], "26");
        assert_eq!(reply["value"]["reply"]["codeName"], "NamespaceNotFound");
        assert_eq!(reply["value"]["reply"]["errmsg"], "ns not found");
        for (request, code) in [
            (serde_json::json!({ "command": {} }), "query.bad_request"),
            (serde_json::json!({ "command": [1] }), "query.bad_request"),
            (serde_json::json!({ "database": " ", "command": { "ping": 1 } }), "query.bad_request"),
            (serde_json::json!({ "nope": 1 }), "query.bad_request"),
        ] {
            let reply = command_call(id, 905, request.clone());
            assert_eq!(reply["error"]["code"], code, "{request}: {reply}");
        }
    }

    #[test]
    fn mongo_command_cancel_stops_it_and_an_early_cancel_starts_it_cancelled() {
        let _serial = test_serial();
        let mut backend = CommandBackend::replying(doc! { "ok": 1.0 });
        backend.block_until_cancelled = true;
        let seen = backend.seen.clone();
        let id = command_connection(backend);
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let runner = std::thread::spawn(move || {
            command_call(id, 910, serde_json::json!({ "command": { "find": "c" } }))
        });
        let started = std::time::Instant::now();
        while operations().get(&(id, 910)).is_none() {
            assert!(started.elapsed().as_secs() < 10, "operation never registered");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ack = call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": 910 }));
        assert_eq!(ack["value"]["cancelled"], true);
        let result = runner.join().unwrap();
        assert_eq!(result["error"]["code"], "query.cancelled", "{result}");
        // A cancel that landed before the command started: nothing is sent.
        let _ = call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": 911 }));
        let early = command_call(id, 911, serde_json::json!({ "command": { "find": "c" } }));
        assert_eq!(early["error"]["code"], "query.cancelled", "{early}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_connection_without_raw_commands_reports_it() {
        let _serial = test_serial();
        let id = aggregate_connection(AggregateBackend::new(Ok(vec![])));
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let reply = command_call(id, 912, serde_json::json!({ "command": { "ping": 1 } }));
        assert_eq!(reply["error"]["code"], "query.unsupported", "{reply}");
    }

    #[test]
    fn the_cancel_tag_on_writes_and_counts_waits_for_a_server_that_takes_it() {
        let _serial = test_serial();
        for (wire, tagged) in [(None, false), (Some(8), false), (Some(9), true), (Some(25), true)] {
            let mut backend = CommandBackend::replying(doc! { "ok": 1.0 });
            backend.wire = wire;
            let seen = backend.seen.clone();
            let id = command_connection(backend);
            let _cleanup = TestConnectionCleanup([id, u64::MAX]);
            for (operation, command) in [
                (920, serde_json::json!({ "update": "c", "updates": [] })),
                (921, serde_json::json!({ "count": "c" })),
                (922, serde_json::json!({ "find": "c" })),
            ] {
                let reply = command_call(id, operation, serde_json::json!({ "command": command, "max_time_ms": 100 }));
                assert_eq!(reply["ok"], true, "{reply}");
            }
            let commands: Vec<Document> = seen.lock().unwrap().iter().map(|(_, c)| c.clone()).collect();
            assert_eq!(commands[0].contains_key("comment"), tagged, "update on wire {wire:?}");
            assert_eq!(commands[1].contains_key("comment"), tagged, "count on wire {wire:?}");
            assert!(commands[2].contains_key("comment"), "find is tagged on every server");
            assert!(commands[1].contains_key("maxTimeMS") && commands[2].contains_key("maxTimeMS"));
            assert_eq!(commands[0].contains_key("maxTimeMS"), tagged, "update's maxTimeMS also waits for 4.4");
        }
    }

    #[test]
    fn a_cancel_racing_the_start_of_an_operation_is_never_lost() {
        let _serial = test_serial();
        let id = aggregate_connection(AggregateBackend::new(Ok(vec![])));
        let _cleanup = TestConnectionCleanup([id, u64::MAX]);
        let driver = SESSION.get().unwrap().lock().unwrap().drivers.get(&id).cloned().unwrap();
        for round in 0..300_u64 {
            let operation = 50_000 + round;
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let canceller = {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    call_json(dbengine_cancel_operation, id, serde_json::json!({ "operation_id": operation }))
                })
            };
            barrier.wait();
            let (guard, control) = begin_operation(id, operation, &driver);
            canceller.join().unwrap();
            // The guard is still alive: whichever of the two ran first, the
            // operation has been told to stop.
            assert!(control.cancel.as_ref().unwrap().is_cancelled(), "round {round}: the cancel was lost");
            drop(guard);
            early_cancels().retain(|(_, op)| *op != operation);
        }
    }
}
