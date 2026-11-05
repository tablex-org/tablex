//! `Driver<B>`: the generic engine behind the exported `dbengine_*` symbols.
//!
//! Each method here is one C entry point (the [`export_driver!`] macro forwards
//! to it). Every body runs under `catch_unwind`; the fallback for a panic is
//! documented per method.
//!
//! [`export_driver!`]: crate::export_driver

use crate::backend::{ConnectRequest, DriverBackend, Page, QueryOutput};
use crate::ffi::{
    build_cell_batch, empty_lease, export_arrow_lease, failure, free_cell_batch, guarded,
    guarded_or, parse_request, reply,
};
use crate::session::{ConnectionEntry, Session};
use engine_driver_api::{
    CArrowPageLease, CCellTextBatch, CancelRequest, Capability, DriverCapabilities, DriverError,
    ExportRequest, MutationRequest, MutationResult, NavigatorRequest, QueryPage, QueryRequest,
};
use engine_ffi_util::string_to_c_ptr;
use serde::Serialize;
use std::ffi::c_char;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

/// Which optional symbols the driver's `export_driver!` invocation emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportedSymbols(u8);

impl ExportedSymbols {
    pub const NONE: Self = Self(0);
    pub const CANCEL_OPERATION: Self = Self(1);
    pub const MUTATE: Self = Self(2);
    pub const EXPORT: Self = Self(4);
    pub const DESCRIBE_OBJECT: Self = Self(8);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Checks that the optional symbols match the advertised capabilities: the
    /// host resolves `dbengine_mutate`, `dbengine_export`,
    /// `dbengine_cancel_operation` and `dbengine_describe_object` from the
    /// capabilities, and the kit never exports `dbengine_page_function_table`.
    pub fn validate(self, capabilities: &DriverCapabilities) -> Result<(), String> {
        let mut problems: Vec<&str> = Vec::new();
        if capabilities.has(Capability::OperationCancel) != self.contains(Self::CANCEL_OPERATION) {
            problems.push("operation.cancel does not match the cancel_operation export");
        }
        // The host resolves `dbengine_mutate` only when a `mutation.*` token is
        // advertised (`database.create` alone does not load it).
        if capabilities.has_any_mutation() != self.contains(Self::MUTATE) {
            problems.push("mutation.* capabilities do not match the mutate export");
        }
        if capabilities.has_any_export() != self.contains(Self::EXPORT) {
            problems.push("export.* capabilities do not match the export export");
        }
        if capabilities.has(Capability::MetadataTableProperties)
            != self.contains(Self::DESCRIBE_OBJECT)
        {
            problems.push("metadata.table_properties does not match the describe_object export");
        }
        if capabilities.has(Capability::QueryPageLease) {
            problems.push(
                "query.page_lease is not served by the kit (no dbengine_page_function_table)",
            );
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

#[derive(Serialize)]
struct Empty {}

#[derive(Serialize)]
struct Connected {
    connection_id: u64,
    /// This connection's SQL dialect when it differs from the static
    /// capabilities (see [`DriverBackend::connection_dialect`]); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    sql_dialect: Option<engine_driver_api::SqlDialectInfo>,
    /// The server version in MySQL's numeric form (see
    /// [`DriverBackend::connection_server_version`]); absent when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    server_version: Option<u32>,
}

#[derive(Serialize)]
struct Cancelled {
    cancelled: bool,
}

/// Registries and entry points for one backend. Create it once in a `static`
/// (the macro does).
pub struct Driver<B: DriverBackend> {
    exports: ExportedSymbols,
    session: OnceLock<Mutex<Session<B>>>,
    _backend: PhantomData<fn() -> B>,
}

impl<B: DriverBackend> Driver<B> {
    pub const fn new(exports: ExportedSymbols) -> Self {
        Self {
            exports,
            session: OnceLock::new(),
            _backend: PhantomData,
        }
    }

    fn session(&self) -> &Mutex<Session<B>> {
        self.session.get_or_init(|| Mutex::new(Session::new()))
    }

    /// The registries hold only maps and counters that kit code updates in
    /// trivial, non-panicking steps (backend code never runs under this lock),
    /// so a poisoned lock is recovered instead of disabling the driver.
    fn lock(&self) -> MutexGuard<'_, Session<B>> {
        self.session()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn connection(
        &self,
        connection_id: u64,
        code: &'static str,
    ) -> Result<Arc<ConnectionEntry<B>>, DriverError> {
        self.lock()
            .connections
            .get(&connection_id)
            .cloned()
            .ok_or_else(|| failure(code, format!("unknown connection {connection_id}")))
    }

    fn page(
        &self,
        page_id: u64,
        missing_code: &'static str,
    ) -> Result<Arc<Page<B::PageContext>>, DriverError> {
        self.lock()
            .pages
            .get(&page_id)
            .cloned()
            .ok_or_else(|| failure(missing_code, "unknown page"))
    }

    /// `dbengine_abi_version`. A panic returns 0 (no ABI is version 0).
    pub fn abi_version(&self) -> u32 {
        guarded_or(0, || engine_driver_api::ABI_VERSION)
    }

    /// `dbengine_capabilities`. A panic returns null, which the host reads as
    /// an empty (invalid) capabilities document and rejects the driver.
    pub fn capabilities(&self) -> *mut c_char {
        guarded_or(std::ptr::null_mut(), || {
            serde_json::to_string(&B::capabilities()).map_or(std::ptr::null_mut(), string_to_c_ptr)
        })
    }

    /// `dbengine_init`.
    pub fn init(&self) -> *mut c_char {
        guarded("init.internal", || {
            self.exports
                .validate(&B::capabilities())
                .map_err(|message| failure("init.capability_mismatch", message))?;
            B::init().map_err(|error| error.into_failure("init.failed"))?;
            let _ = self.session();
            Ok(Empty {})
        })
    }

    /// `dbengine_shutdown`: drops every page, disconnects every connection.
    pub fn shutdown(&self) -> *mut c_char {
        guarded("shutdown.internal", || {
            // Take everything out under the lock; stop operations, call the
            // backend and free the page buffers after it is released.
            let (pages, entries): (Vec<_>, Vec<_>) = {
                let mut session = self.lock();
                let pages = std::mem::take(&mut session.pages);
                let entries = session
                    .connections
                    .drain()
                    .map(|(_, entry)| entry)
                    .collect();
                (pages.into_values().collect(), entries)
            };
            for entry in &entries {
                entry.cancel_all();
                B::disconnect(&entry.connection);
            }
            drop(pages);
            drop(entries);
            B::shutdown();
            Ok(Empty {})
        })
    }

    /// `dbengine_connect`.
    ///
    /// # Safety
    /// `request_json` must be null or point to a valid NUL-terminated string.
    pub unsafe fn connect(&self, request_json: *const c_char) -> *mut c_char {
        guarded("connect.internal", || {
            // SAFETY: forwarded caller contract (null or valid NUL-terminated).
            let request: ConnectRequest =
                unsafe { parse_request(request_json, "connect.bad_request")? };
            let connection =
                B::connect(&request).map_err(|error| error.into_failure("connect.failed"))?;
            let entry = ConnectionEntry::<B>::new(connection)
                .map_err(|error| error.into_failure("connect.failed"))?;
            let entry = Arc::new(entry);
            let sql_dialect = B::connection_dialect(&entry.connection);
            let server_version = B::connection_server_version(&entry.connection);
            let connection_id = self.lock().insert_connection(entry);
            Ok(Connected {
                connection_id,
                sql_dialect,
                server_version,
            })
        })
    }

    /// `dbengine_disconnect`: releases the connection's pages first.
    pub fn disconnect(&self, connection_id: u64) -> *mut c_char {
        guarded("disconnect.internal", || {
            // Stop a running query first: it holds the connection and would
            // otherwise run to completion with nobody able to cancel it.
            let entry = self.connection(connection_id, "disconnect.not_found")?;
            entry.cancel_all();
            let (removed, pages) = {
                let mut session = self.lock();
                let pages = session.remove_connection_pages(connection_id);
                (session.connections.remove(&connection_id), pages)
            };
            drop(pages); // buffers are freed outside the session lock
            if removed.is_none() {
                return Err(failure(
                    "disconnect.not_found",
                    format!("unknown connection {connection_id}"),
                ));
            }
            B::disconnect(&entry.connection);
            Ok(Empty {})
        })
    }

    /// `dbengine_list_nav_nodes`.
    ///
    /// # Safety
    /// `request_json` must be null or point to a valid NUL-terminated string.
    pub unsafe fn list_nav_nodes(
        &self,
        connection_id: u64,
        request_json: *const c_char,
    ) -> *mut c_char {
        guarded("navigator.internal", || {
            // SAFETY: forwarded caller contract.
            let request: NavigatorRequest =
                unsafe { parse_request(request_json, "navigator.bad_request")? };
            let entry = self.connection(connection_id, "navigator.not_connected")?;
            B::list_nav_nodes(&entry.connection, &request)
                .map_err(|error| error.into_failure("navigator.failed"))
        })
    }

    /// `dbengine_open_query`: runs the query, registers the result as a page.
    ///
    /// # Safety
    /// `request_json` must be null or point to a valid NUL-terminated string.
    pub unsafe fn open_query(
        &self,
        connection_id: u64,
        request_json: *const c_char,
    ) -> *mut c_char {
        guarded("query.internal", || {
            // SAFETY: forwarded caller contract.
            let request: QueryRequest =
                unsafe { parse_request(request_json, "query.bad_request")? };
            let entry = self.connection(connection_id, "query.not_connected")?;
            let output = {
                let mut operation = entry.register_operation(
                    request.operation_id,
                    "query.failed",
                    "query.not_connected",
                    "query.cancelled",
                )?;
                B::open_query(&entry.connection, &request, &mut operation)
                    .map_err(|error| error.into_failure("query.failed"))?
            };
            self.register_page(connection_id, request.operation_id, output)
        })
    }

    fn register_page(
        &self,
        connection_id: u64,
        operation_id: engine_driver_api::OperationId,
        output: QueryOutput<B::PageContext>,
    ) -> Result<QueryPage, DriverError> {
        let QueryOutput {
            batch,
            columns,
            total_count,
            total_is_exact,
            flags,
            context,
        } = output;
        if columns.len() != batch.num_columns() {
            return Err(failure(
                "query.failed",
                format!(
                    "driver returned {} column descriptors for {} columns",
                    columns.len(),
                    batch.num_columns()
                ),
            ));
        }
        let mut session = self.lock();
        // The connection may have been disconnected while the query ran.
        if !session.connections.contains_key(&connection_id) {
            return Err(failure(
                "query.not_connected",
                format!("unknown connection {connection_id}"),
            ));
        }
        let page_id = session.next_page_id();
        let page = QueryPage {
            operation_id,
            page_id,
            columns: columns.clone(),
            row_count: batch.num_rows() as u64,
            total_count,
            total_is_exact,
            flags,
        };
        session.pages.insert(
            page_id,
            Arc::new(Page {
                page_id,
                connection_id,
                batch,
                columns,
                context,
            }),
        );
        Ok(page)
    }

    /// `dbengine_acquire_arrow_page`: leases `row_count` rows from `row_start`
    /// as an Arrow C-data struct array. The slice is zero-copy; the lease owns
    /// refcounts on the page's buffers until released. Unknown page, wrong
    /// connection, lock failure, export failure and panics all return an empty
    /// lease (null pointers, no `release`), the ABI's only error channel.
    pub fn acquire_arrow_page(
        &self,
        connection_id: u64,
        page_id: u64,
        row_start: u64,
        row_count: u64,
    ) -> CArrowPageLease {
        guarded_or(empty_lease(), || {
            let Some(page) = self.lock().pages.get(&page_id).cloned() else {
                return empty_lease();
            };
            if page.connection_id != connection_id {
                return empty_lease();
            }
            let Ok(requested) = usize::try_from(row_count) else {
                return empty_lease();
            };
            let rows = page.batch.num_rows();
            let start = row_start.min(rows as u64) as usize;
            let end = start.saturating_add(requested).min(rows);
            let slice = page.batch.slice(start, end - start);
            export_arrow_lease(slice).unwrap_or_else(empty_lease)
        })
    }

    /// `dbengine_render_cell_batch`. Returns 0 and stores the batch in
    /// `out_batch`, or returns 1 and (if `out_error` is non-null) stores an
    /// error envelope there. A panic takes the error path with code
    /// `render.internal`. A null `out_batch` returns 1 with no envelope.
    ///
    /// # Safety
    /// `out_batch` must be null or writable for one pointer; `out_error` must
    /// be null or writable for one pointer.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn render_cell_batch(
        &self,
        page_id: u64,
        row_start: u64,
        row_count: u64,
        column_start: u64,
        column_count: u64,
        out_batch: *mut *mut CCellTextBatch,
        out_error: *mut *mut c_char,
    ) -> i32 {
        if out_batch.is_null() {
            return 1;
        }
        // SAFETY: non-null and writable per the caller contract.
        unsafe { *out_batch = std::ptr::null_mut() };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let page = self.page(page_id, "render.page_not_found")?;
            let (rows, columns) = (page.batch.num_rows(), page.batch.num_columns());
            let row_start = row_start.min(rows as u64) as usize;
            let row_end = row_start
                .saturating_add(usize::try_from(row_count).unwrap_or(usize::MAX))
                .min(rows);
            let column_start = column_start.min(columns as u64) as usize;
            let column_end = column_start
                .saturating_add(usize::try_from(column_count).unwrap_or(usize::MAX))
                .min(columns);
            Ok::<_, DriverError>(build_cell_batch(
                row_start..row_end,
                column_start..column_end,
                |column| B::cell_formatter(&page, column),
            ))
        }));
        let error = match outcome {
            Ok(Ok(batch)) => {
                // SAFETY: as above; ownership of the box passes to the host,
                // which frees it with `free_cell_text_batch`.
                unsafe { *out_batch = Box::into_raw(batch) };
                return 0;
            }
            Ok(Err(error)) => error,
            Err(_) => failure("render.internal", "internal driver error: backend panicked"),
        };
        if !out_error.is_null() {
            // SAFETY: non-null and writable per the caller contract.
            unsafe { *out_error = reply::<()>(Err(error)) };
        }
        1
    }

    /// `dbengine_inspect_row`.
    pub fn inspect_row(&self, page_id: u64, row_index: u64) -> *mut c_char {
        guarded("inspect.internal", || {
            let page = self.page(page_id, "inspect.page_not_found")?;
            let row = usize::try_from(row_index)
                .ok()
                .filter(|row| *row < page.batch.num_rows())
                .ok_or_else(|| failure("inspect.row_not_found", "unknown row"))?;
            B::inspect_row(&page, row).map_err(|error| error.into_failure("inspect.failed"))
        })
    }

    /// `dbengine_release_page`.
    pub fn release_page(&self, page_id: u64) -> *mut c_char {
        guarded("release.internal", || {
            // Drop the page's buffers after the registry lock is released.
            let removed = self.lock().pages.remove(&page_id);
            removed
                .map(|_| Empty {})
                .ok_or_else(|| failure("release.page_not_found", "unknown page"))
        })
    }

    /// `dbengine_cancel_operation`.
    ///
    /// # Safety
    /// `request_json` must be null or point to a valid NUL-terminated string.
    pub unsafe fn cancel_operation(
        &self,
        connection_id: u64,
        request_json: *const c_char,
    ) -> *mut c_char {
        guarded("operation.internal", || {
            // SAFETY: forwarded caller contract.
            let request: CancelRequest =
                unsafe { parse_request(request_json, "operation.bad_request")? };
            let entry = self.connection(connection_id, "operation.not_connected")?;
            let cancelled = entry
                .cancel_operation(request.operation_id)
                .map_err(|error| error.into_failure("operation.failed"))?;
            Ok(Cancelled { cancelled })
        })
    }

    /// `dbengine_mutate`: runs the mutation, then (unless the backend keeps
    /// them) drops the connection's pages and reports their ids.
    ///
    /// # Safety
    /// `request_json` must be null or point to a valid NUL-terminated string.
    pub unsafe fn mutate(&self, connection_id: u64, request_json: *const c_char) -> *mut c_char {
        guarded("mutation.internal", || {
            // SAFETY: forwarded caller contract.
            let request: MutationRequest =
                unsafe { parse_request(request_json, "mutation.bad_request")? };
            let entry = self.connection(connection_id, "mutation.not_connected")?;
            let outcome = {
                let mut operation = entry.register_operation(
                    request.operation_id,
                    "mutation.failed",
                    "mutation.not_connected",
                    "mutation.cancelled",
                )?;
                B::mutate(&entry.connection, &request, &mut operation)
                    .map_err(|error| error.into_failure("mutation.failed"))?
            };
            // The removed pages are dropped after the session lock is released.
            let removed = if outcome.invalidate_pages {
                self.lock().remove_connection_pages(connection_id)
            } else {
                Vec::new()
            };
            let invalidated_page_ids = removed.iter().map(|page| page.page_id).collect();
            Ok(MutationResult {
                affected_rows: outcome.affected_rows,
                invalidated_page_ids,
            })
        })
    }

    /// `dbengine_export`.
    ///
    /// # Safety
    /// `request_json` must be null or point to a valid NUL-terminated string.
    pub unsafe fn export(&self, connection_id: u64, request_json: *const c_char) -> *mut c_char {
        guarded("export.internal", || {
            // SAFETY: forwarded caller contract.
            let request: ExportRequest =
                unsafe { parse_request(request_json, "export.bad_request")? };
            let entry = self.connection(connection_id, "export.not_connected")?;
            let page = match request.page_id {
                Some(page_id) => Some(self.page(page_id, "export.page_not_found").and_then(
                    |page| {
                        if page.connection_id == connection_id {
                            Ok(page)
                        } else {
                            Err(failure("export.page_not_found", "unknown page"))
                        }
                    },
                )?),
                None => None,
            };
            let mut operation = entry.register_operation(
                request.operation_id,
                "export.failed",
                "export.not_connected",
                "export.cancelled",
            )?;
            B::export(&entry.connection, &request, page.as_deref(), &mut operation)
                .map_err(|error| error.into_failure("export.failed"))
        })
    }

    /// `dbengine_describe_object`.
    ///
    /// # Safety
    /// `request_json` must be null or point to a valid NUL-terminated string.
    pub unsafe fn describe_object(
        &self,
        connection_id: u64,
        request_json: *const c_char,
    ) -> *mut c_char {
        guarded("describe.internal", || {
            // SAFETY: forwarded caller contract.
            let request: serde_json::Value =
                unsafe { parse_request(request_json, "describe.bad_request")? };
            let entry = self.connection(connection_id, "describe.not_connected")?;
            B::describe_object(&entry.connection, &request)
                .map_err(|error| error.into_failure("describe.failed"))
        })
    }

    /// `dbengine_free_cell_text_batch`.
    ///
    /// # Safety
    /// `batch` must be null or a batch returned by `render_cell_batch` that has
    /// not been freed.
    pub unsafe fn free_cell_text_batch(&self, batch: *mut CCellTextBatch) {
        // SAFETY: forwarded caller contract.
        guarded_or((), || unsafe { free_cell_batch(batch) });
    }

    /// `dbengine_free_string`.
    ///
    /// # Safety
    /// `value` must be null or a string returned by this driver and not yet freed.
    pub unsafe fn free_string(&self, value: *mut c_char) {
        // SAFETY: forwarded caller contract (`CString::into_raw` origin).
        guarded_or((), || unsafe { engine_ffi_util::free_string(value) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendError, QueryOutput};
    use crate::operation::OperationGuard;
    use engine_driver_api::{NavigatorPage, NavigatorRequest};

    struct Tiny;

    impl DriverBackend for Tiny {
        type Connection = ();
        type PageContext = ();
        type Canceller = ();

        fn capabilities() -> DriverCapabilities {
            DriverCapabilities::new("tiny", "Tiny", Vec::new())
        }
        fn connect(_: &ConnectRequest) -> Result<(), BackendError> {
            Ok(())
        }
        fn canceller(_: &()) -> Result<(), BackendError> {
            Ok(())
        }
        fn list_nav_nodes(_: &(), _: &NavigatorRequest) -> Result<NavigatorPage, BackendError> {
            Ok(NavigatorPage {
                nodes: Vec::new(),
                next_cursor: None,
            })
        }
        fn open_query(
            _: &(),
            _: &QueryRequest,
            _: &mut OperationGuard<'_>,
        ) -> Result<QueryOutput, BackendError> {
            Ok(QueryOutput::command_complete(0, ()))
        }
    }

    fn envelope(ptr: *mut c_char) -> serde_json::Value {
        // SAFETY: `ptr` is a NUL-terminated string the driver just returned.
        let text = unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: freed once, by the driver that allocated it.
        unsafe { engine_ffi_util::free_string(ptr) };
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn poisoned_session_lock_is_recovered() {
        let driver = Driver::<Tiny>::new(ExportedSymbols::NONE);
        std::thread::scope(|scope| {
            let poisoner = scope.spawn(|| {
                let _session = driver.session().lock().unwrap();
                panic!("poison the session lock");
            });
            assert!(poisoner.join().is_err());
        });
        assert!(driver.session().is_poisoned());
        let request = std::ffi::CString::new(r#"{"uri":"tiny://"}"#).unwrap();
        // SAFETY: `request` is a live NUL-terminated string.
        let connected = envelope(unsafe { driver.connect(request.as_ptr()) });
        assert_eq!(connected["ok"], true, "{connected}");
        assert_eq!(envelope(driver.shutdown())["ok"], true);
        assert!(driver.acquire_arrow_page(1, 1, 0, 1).array.is_null());
    }

    #[test]
    fn validate_follows_the_hosts_symbol_gating() {
        let caps = |tokens: &[&str]| {
            DriverCapabilities::new("t", "T", tokens.iter().map(|t| (*t).to_owned()).collect())
        };
        // `database.create` alone does not make the host load dbengine_mutate.
        let create_only = caps(&["database.create"]);
        assert!(ExportedSymbols::NONE.validate(&create_only).is_ok());
        assert!(ExportedSymbols::MUTATE.validate(&create_only).is_err());
        let mutating = caps(&["mutation.insert"]);
        assert!(ExportedSymbols::MUTATE.validate(&mutating).is_ok());
        assert!(ExportedSymbols::NONE.validate(&mutating).is_err());
        assert!(ExportedSymbols::NONE
            .validate(&caps(&["query.page_lease"]))
            .is_err());
    }
}
