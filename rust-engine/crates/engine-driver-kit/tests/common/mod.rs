//! A mock backend plus helpers shared by the integration tests. The mock is
//! exported through `export_driver!` (so the macro-emitted symbols are
//! exercised) and also usable as a plain `Driver<Mock>` for isolated tests.
#![allow(dead_code)]

use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use engine_driver_kit::api::{
    CCellTextBatch, Capability, DriverCapabilities, ExportRequest, ExportResult, MutationRequest,
    NavigatorPage, NavigatorRequest, OperationId, PageFlags, QueryRequest,
};
use engine_driver_kit::{
    page_columns, BackendError, CellFormatter, ConnectRequest, Driver, DriverBackend,
    ExportedSymbols, MutationOutcome, OperationGuard, Page, QueryOutput,
};
use serde_json::json;
use std::ffi::{c_char, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub struct Mock;

/// Operation ids whose `spin` query has started (never removed; ids are unique
/// per test).
static SPINNING: Mutex<Vec<u64>> = Mutex::new(Vec::new());
/// While false, the `blocking-cancel` canceller blocks (after interrupting).
pub static CANCEL_GATE: AtomicBool = AtomicBool::new(false);

pub fn wait_until_spinning(operation_id: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !SPINNING.lock().unwrap().contains(&operation_id) {
        assert!(
            Instant::now() < deadline,
            "spin {operation_id} never started"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[derive(Default)]
pub struct MockState {
    interrupted: AtomicBool,
    interrupt_panics: bool,
    blocking_cancel: bool,
}

pub struct MockConnection {
    state: Arc<MockState>,
    /// The connection lock a real driver would hold while a statement runs.
    lock: Mutex<()>,
}

/// Operation ids whose `gate` query reached its gate / were released.
static GATED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static RELEASED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
/// Executions that began in a call issued after the test saw `disconnect` return.
pub static LATE_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn wait_until_gated(operation_id: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !GATED.lock().unwrap().contains(&operation_id) {
        assert!(
            Instant::now() < deadline,
            "gate {operation_id} never reached"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub fn release_gate(operation_id: u64) {
    RELEASED.lock().unwrap().push(operation_id);
}

impl DriverBackend for Mock {
    type Connection = MockConnection;
    type PageContext = String;
    type Canceller = Arc<MockState>;

    fn capabilities() -> DriverCapabilities {
        DriverCapabilities::new(
            "mock",
            "Mock",
            vec![
                Capability::Navigator.as_str().into(),
                Capability::QueryArrowCData.as_str().into(),
                Capability::OperationCancel.as_str().into(),
                Capability::MutationInsert.as_str().into(),
                "export.json".into(),
                Capability::MetadataTableProperties.as_str().into(),
            ],
        )
    }

    fn connect(request: &ConnectRequest) -> Result<MockConnection, BackendError> {
        match request.uri.as_str() {
            "mock://panic" => panic!("connect boom"),
            "mock://fail" => Err(BackendError::with_code("connect.bad_request", "bad uri")),
            uri => Ok(MockConnection {
                state: Arc::new(MockState {
                    interrupted: AtomicBool::new(false),
                    interrupt_panics: uri == "mock://interrupt-panics",
                    blocking_cancel: uri == "mock://blocking-cancel",
                }),
                lock: Mutex::new(()),
            }),
        }
    }

    fn list_nav_nodes(
        _connection: &MockConnection,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, BackendError> {
        if request.parent.as_deref() == Some("panic") {
            panic!("navigator boom");
        }
        Ok(NavigatorPage {
            nodes: Vec::new(),
            next_cursor: Some("next".into()),
        })
    }

    fn open_query(
        connection: &MockConnection,
        request: &QueryRequest,
        operation: &mut OperationGuard<'_>,
    ) -> Result<QueryOutput<String>, BackendError> {
        let mode = request.payload["mode"].as_str().unwrap_or("rows");
        if mode == "panic_before_activate" {
            panic!("query boom");
        }
        if mode == "gate" {
            GATED.lock().unwrap().push(request.operation_id.0);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !RELEASED.lock().unwrap().contains(&request.operation_id.0) {
                assert!(Instant::now() < deadline, "gate never released");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let active = operation.activate(
            connection
                .lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )?;
        if request.payload["late"] == true {
            LATE_RUNS.fetch_add(1, Ordering::SeqCst);
        }
        match mode {
            "server_kill" => {
                // A server-side interrupt nobody asked for is retryable.
                return Err(if active.cancel_requested() {
                    BackendError::with_code("query.cancelled", "interrupted")
                } else {
                    BackendError::with_code("query.interrupted", "interrupted by server")
                        .retryable()
                });
            }
            "panic_active" => panic!("query boom while active"),
            "ddl" => return Ok(QueryOutput::command_complete(3, String::new())),
            "spin" => {
                SPINNING.lock().unwrap().push(request.operation_id.0);
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if connection.state.interrupted.swap(false, Ordering::SeqCst) {
                        assert!(active.cancel_requested());
                        return Err(BackendError::with_code("query.cancelled", "interrupted"));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                return Err(BackendError::new("spin was never interrupted"));
            }
            _ => {}
        }
        let rows = if request.limit == 0 {
            5
        } else {
            request.limit as i64
        };
        let names: Vec<Option<String>> = (0..rows)
            .map(|row| match (mode, row) {
                ("boom", 1) => Some("boom".to_owned()),
                (_, row) => (row % 3 != 2).then(|| format!("row{row}")),
            })
            .collect();
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("name", DataType::Utf8, true),
            ])),
            vec![
                Arc::new(Int64Array::from_iter_values(0..rows)),
                Arc::new(StringArray::from(names)),
            ],
        )
        .map_err(|error| BackendError::new(error.to_string()))?;
        let columns = page_columns(&batch, |data_type| data_type.to_string().to_lowercase());
        Ok(QueryOutput {
            batch,
            columns,
            total_count: Some(100),
            total_is_exact: true,
            flags: PageFlags {
                truncated: false,
                cancelled: false,
                read_only: false,
            },
            context: request.object.clone().unwrap_or_default(),
        })
    }

    fn canceller(connection: &MockConnection) -> Result<Arc<MockState>, BackendError> {
        Ok(Arc::clone(&connection.state))
    }

    fn cancel(state: &Arc<MockState>, _operation_id: OperationId) -> Result<(), BackendError> {
        // Set the flag first so the interrupted statement ends even when this
        // method then blocks or panics.
        state.interrupted.store(true, Ordering::SeqCst);
        if state.interrupt_panics {
            panic!("interrupt boom");
        }
        if state.blocking_cancel {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !CANCEL_GATE.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    fn cell_formatter(page: &Page<String>, column: usize) -> CellFormatter<'_> {
        let array = page.batch.column(column);
        match array.as_any().downcast_ref::<StringArray>() {
            Some(strings) => CellFormatter::custom(array, move |row, out| {
                if strings.value(row) == "boom" {
                    panic!("render boom");
                }
                out.push_str(strings.value(row));
            }),
            None => CellFormatter::new(array),
        }
    }

    fn mutate(
        _connection: &MockConnection,
        request: &MutationRequest,
        _operation: &mut OperationGuard<'_>,
    ) -> Result<MutationOutcome, BackendError> {
        match request.kind.as_str() {
            "panic" => panic!("mutate boom"),
            "keep" => Ok(MutationOutcome::new(1).keep_pages()),
            _ => Ok(MutationOutcome::new(2)),
        }
    }

    fn export(
        _connection: &MockConnection,
        _request: &ExportRequest,
        page: Option<&Page<String>>,
        _operation: &mut OperationGuard<'_>,
    ) -> Result<ExportResult, BackendError> {
        let page = page.ok_or_else(|| BackendError::new("export needs a page"))?;
        Ok(ExportResult {
            rows_written: page.batch.num_rows() as u64,
            bytes_written: page.context.len() as u64,
        })
    }

    fn describe_object(
        _connection: &MockConnection,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        Ok(serde_json::json!({ "described": request["object"] }))
    }
}

engine_driver_kit::export_driver!(Mock; cancel_operation, mutate, export, describe_object);

pub const ALL_SYMBOLS: ExportedSymbols = ExportedSymbols::CANCEL_OPERATION
    .union(ExportedSymbols::MUTATE)
    .union(ExportedSymbols::EXPORT)
    .union(ExportedSymbols::DESCRIBE_OBJECT);

/// An isolated registry (separate from the macro's static).
pub fn driver() -> Driver<Mock> {
    Driver::new(ALL_SYMBOLS)
}

pub fn cstring(json: &serde_json::Value) -> CString {
    CString::new(json.to_string()).unwrap()
}

/// Reads and frees an envelope string through the exported `free_string`.
pub fn take(ptr: *mut c_char) -> serde_json::Value {
    assert!(!ptr.is_null(), "entry point returned a null envelope");
    // SAFETY: `ptr` is a NUL-terminated string the driver just returned.
    let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
    // SAFETY: `ptr` came from the driver's string allocator and is freed once.
    unsafe { driver().free_string(ptr) };
    serde_json::from_str(&text).unwrap()
}

pub fn ok_value(envelope: serde_json::Value) -> serde_json::Value {
    assert_eq!(envelope["ok"], true, "expected success, got {envelope}");
    envelope["value"].clone()
}

pub fn error_code(envelope: &serde_json::Value) -> String {
    assert_eq!(envelope["ok"], false, "expected error, got {envelope}");
    envelope["error"]["code"].as_str().unwrap().to_owned()
}

pub fn connect(driver: &Driver<Mock>, uri: &str) -> serde_json::Value {
    let request = cstring(&serde_json::json!({ "uri": uri }));
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { driver.connect(request.as_ptr()) })
}

pub fn connect_ok(driver: &Driver<Mock>) -> u64 {
    ok_value(connect(driver, "mock://ok"))["connection_id"]
        .as_u64()
        .unwrap()
}

pub fn query(
    driver: &Driver<Mock>,
    connection_id: u64,
    operation_id: u64,
    mode: &str,
    limit: u64,
) -> serde_json::Value {
    query_with(
        driver,
        connection_id,
        operation_id,
        limit,
        json!({ "mode": mode }),
    )
}

pub fn query_with(
    driver: &Driver<Mock>,
    connection_id: u64,
    operation_id: u64,
    limit: u64,
    payload: serde_json::Value,
) -> serde_json::Value {
    let request = cstring(&json!({
        "kind": "sql", "operation_id": operation_id, "database": null,
        "object": "orders", "offset": 0, "limit": limit,
        "payload": payload,
    }));
    // SAFETY: `request` is a live NUL-terminated string.
    take(unsafe { driver.open_query(connection_id, request.as_ptr()) })
}

pub fn render(
    driver: &Driver<Mock>,
    page_id: u64,
    rows: (u64, u64),
    columns: (u64, u64),
) -> (i32, *mut CCellTextBatch, serde_json::Value) {
    let mut batch: *mut CCellTextBatch = std::ptr::null_mut();
    let mut error: *mut c_char = std::ptr::null_mut();
    // SAFETY: both out pointers are writable locals.
    let status = unsafe {
        driver.render_cell_batch(
            page_id, rows.0, rows.1, columns.0, columns.1, &mut batch, &mut error,
        )
    };
    let error = if error.is_null() {
        serde_json::Value::Null
    } else {
        take(error)
    };
    (status, batch, error)
}
