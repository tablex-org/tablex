use crate::dynlib::{NativeLibrary, NativeLibraryError};
use engine_driver_api::{
    validate_arrow_function_table, validate_page_function_table, ArrowFunctionTableFn,
    CArrowFunctionTable, CArrowPageLease, CCellText, CCellTextBatch, CPageFunctionTable,
    CPageLease, CancelRequest, Capability, DriverCapabilities, DriverErrorEnvelope, ExportRequest,
    ExportResult, MutationRequest, MutationResult, NavigatorPage, OperationId, PageFunctionTableFn,
    QueryPage, RowFields, SqlDialectInfo,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, CStr, CString};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("dylib open failed: {message}")]
    DlOpenFailed { message: String },
    #[error("loading driver libraries is not implemented on {0} yet")]
    PlatformUnsupported(&'static str),
    #[error("driver checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("driver integrity check failed: {message}")]
    IntegrityCheckFailed { message: String },
    #[error("driver is missing required symbol: {symbol}")]
    MissingSymbol { symbol: String },
    #[error("driver capabilities are invalid: {0}")]
    InvalidCapabilities(String),
    /// The driver answered `{"ok":false,"error":{...}}`. `retryable` is the
    /// driver's own flag (`query.interrupted`: an interrupt nobody requested).
    #[error("driver rejected the request: {code}: {message}")]
    DriverRejected {
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("driver returned malformed JSON: {0}")]
    MalformedResponse(String),
    #[error("driver is shut down")]
    Shutdown,
    #[error("driver registry lock poisoned")]
    RegistryPoisoned,
    #[error("driver artifact changed at {path}: loaded checksum {loaded}, requested {requested}")]
    ArtifactChanged {
        path: PathBuf,
        loaded: String,
        requested: String,
    },
    #[error("driver artifact at {path} has no recorded checksum")]
    UnverifiedArtifact { path: PathBuf },
    #[error("driver does not support feature: {feature}")]
    UnsupportedFeature { feature: String },
    #[error(
        "driver has active owners: {sessions} sessions, {pending_connects} pending connects, {pages} pages, {batches} batches, {leases} page leases"
    )]
    ActiveOwners {
        sessions: usize,
        pending_connects: usize,
        pages: usize,
        batches: usize,
        operations: usize,
        leases: usize,
    },
}

/// What still keeps a driver generation alive: open sessions and in-flight
/// work. Empty means the generation can be shut down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriverOwners {
    pub sessions: usize,
    pub pending_connects: usize,
    pub pages: usize,
    pub batches: usize,
    pub operations: usize,
    pub leases: usize,
}

impl DriverOwners {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn total(&self) -> usize {
        self.sessions
            + self.pending_connects
            + self.pages
            + self.batches
            + self.operations
            + self.leases
    }

    pub(crate) fn combine(self, other: Self) -> Self {
        Self {
            sessions: self.sessions + other.sessions,
            pending_connects: self.pending_connects + other.pending_connects,
            pages: self.pages + other.pages,
            batches: self.batches + other.batches,
            operations: self.operations + other.operations,
            leases: self.leases + other.leases,
        }
    }
}

impl From<DriverOwners> for LoaderError {
    fn from(owners: DriverOwners) -> Self {
        LoaderError::ActiveOwners {
            sessions: owners.sessions,
            pending_connects: owners.pending_connects,
            pages: owners.pages,
            batches: owners.batches,
            operations: owners.operations,
            leases: owners.leases,
        }
    }
}

struct DriverSymbols {
    init: unsafe extern "C" fn() -> *mut c_char,
    shutdown: unsafe extern "C" fn() -> *mut c_char,
    connect: unsafe extern "C" fn(*const c_char) -> *mut c_char,
    disconnect: unsafe extern "C" fn(u64) -> *mut c_char,
    list_nav_nodes: unsafe extern "C" fn(u64, *const c_char) -> *mut c_char,
    open_query: unsafe extern "C" fn(u64, *const c_char) -> *mut c_char,
    render_cell_batch: unsafe extern "C" fn(
        u64,
        u64,
        u64,
        u64,
        u64,
        *mut *mut CCellTextBatch,
        *mut *mut c_char,
    ) -> c_int,
    inspect_row: unsafe extern "C" fn(u64, u64) -> *mut c_char,
    release_page: unsafe extern "C" fn(u64) -> *mut c_char,
    mutate: Option<unsafe extern "C" fn(u64, *const c_char) -> *mut c_char>,
    cancel_operation: Option<unsafe extern "C" fn(u64, *const c_char) -> *mut c_char>,
    export: Option<unsafe extern "C" fn(u64, *const c_char) -> *mut c_char>,
    /// `dbengine_describe_object`, resolved when `metadata.table_properties`
    /// is advertised.
    describe_object: Option<unsafe extern "C" fn(u64, *const c_char) -> *mut c_char>,
    /// `dbengine_mongo_command`, resolved when `mongo.command` is advertised.
    mongo_command: Option<unsafe extern "C" fn(u64, *const c_char) -> *mut c_char>,
    free_string: unsafe extern "C" fn(*mut c_char),
    free_cell_text_batch: unsafe extern "C" fn(*mut CCellTextBatch),
    page_function_table: Option<PageFunctionTableFn>,
    arrow_function_table: Option<ArrowFunctionTableFn>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FunctionTableHeader {
    struct_size: u32,
    abi_version: u32,
}

enum DriverEntryState {
    Empty,
    Loading,
    Ready {
        handle: Weak<DriverHandle>,
        generation: Arc<()>,
    },
    Retiring {
        generation: Arc<()>,
    },
}

struct DriverEntry {
    state: Mutex<DriverEntryState>,
    wake: Condvar,
}

impl DriverEntry {
    fn new() -> Self {
        Self {
            state: Mutex::new(DriverEntryState::Empty),
            wake: Condvar::new(),
        }
    }
}

struct DriverRegistry {
    entries: Mutex<HashMap<PathBuf, Arc<DriverEntry>>>,
}

impl DriverRegistry {
    fn entry(&self, key: &Path) -> Result<Arc<DriverEntry>, LoaderError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| LoaderError::RegistryPoisoned)?;
        Ok(entries
            .entry(key.to_path_buf())
            .or_insert_with(|| Arc::new(DriverEntry::new()))
            .clone())
    }
}

fn driver_registry() -> &'static DriverRegistry {
    static REGISTRY: OnceLock<DriverRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| DriverRegistry {
        entries: Mutex::new(HashMap::new()),
    })
}

fn driver_cache_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// A loaded driver dylib generation shared by handle and sessions.
/// dlclose happens exactly once, when the last owner drops.
pub struct DriverHandle {
    path: PathBuf,
    entry: Arc<DriverEntry>,
    generation: Arc<()>,
    /// Closed in `Drop` after the driver shut down and before the retirement is
    /// published, so a reload of the same path never maps the old image.
    library: Option<NativeLibrary>,
    symbols: DriverSymbols,
    capabilities: DriverCapabilities,
    page_function_table: Option<CPageFunctionTable>,
    arrow_function_table: Option<CArrowFunctionTable>,
    closed: Mutex<bool>,
    lifecycle: Mutex<()>,
    retiring: std::sync::atomic::AtomicBool,
    artifact_sha256: Option<String>,
    active_sessions: AtomicUsize,
    pending_connects: AtomicUsize,
    active_pages: AtomicUsize,
    active_batches: AtomicUsize,
    active_leases: AtomicUsize,
    next_operation_id: AtomicU64,
    active_operations: Mutex<HashMap<OperationId, u64>>,
}

unsafe impl Send for DriverHandle {}
unsafe impl Sync for DriverHandle {}

pub struct DriverLoader;

impl From<NativeLibraryError> for LoaderError {
    fn from(error: NativeLibraryError) -> Self {
        match error {
            NativeLibraryError::Open(message) => Self::DlOpenFailed { message },
            NativeLibraryError::Unsupported(os) => Self::PlatformUnsupported(os),
        }
    }
}

impl DriverLoader {
    /// The live generation loaded from `path`, if any `Arc<DriverHandle>`
    /// (handle, session, lease) still exists for it.
    pub fn loaded_handle(&self, path: &Path) -> Option<Arc<DriverHandle>> {
        let entry = driver_registry()
            .entries
            .lock()
            .ok()?
            .get(&driver_cache_key(path))
            .cloned()?;
        let state = entry.state.lock().ok()?;
        match &*state {
            DriverEntryState::Ready { handle, .. } => handle.upgrade(),
            _ => None,
        }
    }

    /// Whether a generation loaded from `path` is still alive or being
    /// retired, i.e. its file must not be deleted yet.
    pub fn is_loaded(&self, path: &Path) -> bool {
        let Some(entry) = driver_registry()
            .entries
            .lock()
            .ok()
            .and_then(|entries| entries.get(&driver_cache_key(path)).cloned())
        else {
            return false;
        };
        let Ok(state) = entry.state.lock() else {
            return true;
        };
        match &*state {
            DriverEntryState::Empty => false,
            DriverEntryState::Ready { handle, .. } => handle.strong_count() > 0,
            DriverEntryState::Loading | DriverEntryState::Retiring { .. } => true,
        }
    }

    pub fn load_verified(
        &self,
        path: &Path,
        expected_sha256: &str,
    ) -> Result<Arc<DriverHandle>, LoaderError> {
        let expected = expected_sha256.to_ascii_lowercase();
        let actual = crate::verifier::sha256_file(path).map_err(|error| {
            LoaderError::IntegrityCheckFailed {
                message: error.to_string(),
            }
        })?;
        if actual != expected {
            return Err(LoaderError::ChecksumMismatch { expected, actual });
        }
        self.load_cached(path, Some(&actual))
    }

    pub fn load(&self, path: &Path) -> Result<Arc<DriverHandle>, LoaderError> {
        self.load_cached(path, None)
    }

    fn load_cached(
        &self,
        path: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<Arc<DriverHandle>, LoaderError> {
        let key = driver_cache_key(path);
        let entry = driver_registry().entry(&key)?;
        let load = loop {
            let mut state = entry
                .state
                .lock()
                .map_err(|_| LoaderError::RegistryPoisoned)?;
            match &*state {
                DriverEntryState::Empty => {
                    *state = DriverEntryState::Loading;
                    break true;
                }
                DriverEntryState::Loading | DriverEntryState::Retiring { .. } => {
                    state = entry
                        .wake
                        .wait(state)
                        .map_err(|_| LoaderError::RegistryPoisoned)?;
                    drop(state);
                }
                DriverEntryState::Ready {
                    handle: weak_handle,
                    generation,
                } => {
                    let Some(handle) = weak_handle.upgrade() else {
                        *state = DriverEntryState::Retiring {
                            generation: Arc::clone(generation),
                        };
                        entry.wake.notify_all();
                        state = entry
                            .wake
                            .wait(state)
                            .map_err(|_| LoaderError::RegistryPoisoned)?;
                        drop(state);
                        continue;
                    };
                    if handle.retiring.load(Ordering::Acquire) {
                        state = entry
                            .wake
                            .wait(state)
                            .map_err(|_| LoaderError::RegistryPoisoned)?;
                        drop(state);
                        continue;
                    }
                    if let Some(expected) = expected_sha256 {
                        let loaded = match handle.artifact_sha256.as_deref() {
                            Some(loaded) => loaded,
                            None => {
                                drop(state);
                                drop(handle);
                                return Err(LoaderError::UnverifiedArtifact { path: key });
                            }
                        };
                        if loaded != expected {
                            let error = LoaderError::ArtifactChanged {
                                path: key,
                                loaded: loaded.to_string(),
                                requested: expected.to_string(),
                            };
                            drop(state);
                            drop(handle);
                            return Err(error);
                        }
                    }
                    if handle.ensure_open().is_ok() {
                        let result = Arc::clone(&handle);
                        drop(state);
                        drop(handle);
                        return Ok(result);
                    }
                    *state = DriverEntryState::Empty;
                    entry.wake.notify_all();
                    drop(state);
                    drop(handle);
                    continue;
                }
            }
        };
        debug_assert!(load);

        let result =
            self.load_uncached(path, expected_sha256.map(str::to_owned), Arc::clone(&entry));
        let mut state = entry
            .state
            .lock()
            .map_err(|_| LoaderError::RegistryPoisoned)?;
        match result {
            Ok(handle) => {
                *state = DriverEntryState::Ready {
                    handle: Arc::downgrade(&handle),
                    generation: Arc::clone(&handle.generation),
                };
                entry.wake.notify_all();
                Ok(handle)
            }
            Err(error) => {
                *state = DriverEntryState::Empty;
                entry.wake.notify_all();
                Err(error)
            }
        }
    }

    fn load_uncached(
        &self,
        path: &Path,
        artifact_sha256: Option<String>,
        entry: Arc<DriverEntry>,
    ) -> Result<Arc<DriverHandle>, LoaderError> {
        let library = NativeLibrary::open(path)?;

        let bootstrap = resolve_bootstrap_symbols(&library)?;

        // SAFETY: abi_version is resolved and callable for this handle.
        let abi = unsafe { (bootstrap.abi_version)() };
        if abi != engine_driver_api::ABI_VERSION {
            return Err(LoaderError::InvalidCapabilities(format!(
                "ABI version {abi} != {}",
                engine_driver_api::ABI_VERSION
            )));
        }

        // SAFETY: capabilities is resolved; its string is valid until free.
        let capabilities_json = take_driver_string(bootstrap.capabilities, bootstrap.free_string)?;
        let capabilities: DriverCapabilities =
            parse_envelope(&capabilities_json).map_err(|error| match error {
                LoaderError::MalformedResponse(message) => {
                    LoaderError::InvalidCapabilities(message)
                }
                other => other,
            })?;
        if capabilities.abi_version != engine_driver_api::ABI_VERSION {
            return Err(LoaderError::InvalidCapabilities(format!(
                "capabilities report ABI {} != {}",
                capabilities.abi_version,
                engine_driver_api::ABI_VERSION
            )));
        }

        let symbols = resolve_symbols(&library, bootstrap, &capabilities)?;
        let page_function_table = load_page_function_table(&symbols, &capabilities)?;
        let arrow_function_table = load_arrow_function_table(&symbols, &capabilities)?;
        // SAFETY: init is a required symbol and the handle is live.
        let init_response = unsafe { (symbols.init)() };
        let init_response = take_raw_driver_string(init_response, symbols.free_string)?;
        unwrap_envelope_value(&init_response)?;

        let handle = Arc::new(DriverHandle {
            path: path.to_path_buf(),
            entry,
            generation: Arc::new(()),
            library: Some(library),
            symbols,
            capabilities,
            page_function_table,
            arrow_function_table,
            closed: Mutex::new(false),
            lifecycle: Mutex::new(()),
            retiring: std::sync::atomic::AtomicBool::new(false),
            artifact_sha256,
            active_sessions: AtomicUsize::new(0),
            pending_connects: AtomicUsize::new(0),
            active_pages: AtomicUsize::new(0),
            active_batches: AtomicUsize::new(0),
            active_leases: AtomicUsize::new(0),
            next_operation_id: AtomicU64::new(1),
            active_operations: Mutex::new(HashMap::new()),
        });
        Ok(handle)
    }
}

impl DriverHandle {
    pub fn capabilities(&self) -> &DriverCapabilities {
        &self.capabilities
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn page_function_table(&self) -> Option<&CPageFunctionTable> {
        self.page_function_table.as_ref()
    }

    pub fn arrow_function_table(&self) -> Option<&CArrowFunctionTable> {
        self.arrow_function_table.as_ref()
    }

    pub fn connect(self: &Arc<Self>, request_json: &str) -> Result<DriverSession, LoaderError> {
        {
            let _lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| LoaderError::RegistryPoisoned)?;
            if self.retiring.load(Ordering::Acquire) {
                return Err(LoaderError::Shutdown);
            }
            self.ensure_open()?;
            self.pending_connects.fetch_add(1, Ordering::AcqRel);
        }
        let result = self.connect_inner(request_json);
        self.pending_connects.fetch_sub(1, Ordering::AcqRel);
        result
    }

    fn connect_inner(self: &Arc<Self>, request_json: &str) -> Result<DriverSession, LoaderError> {
        let request = CString::new(request_json).map_err(|error| LoaderError::DlOpenFailed {
            message: error.to_string(),
        })?;
        // SAFETY: connect is resolved; request outlives the call.
        let response = unsafe { (self.symbols.connect)(request.as_ptr()) };
        let response = self.take_string(response)?;
        let value = unwrap_envelope_value(&response)?;
        let connection_id = value
            .get("connection_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| LoaderError::MalformedResponse(response.clone()))?;

        self.active_sessions.fetch_add(1, Ordering::Relaxed);
        Ok(DriverSession {
            handle: Arc::clone(self),
            connection_id,
            dialect: connection_dialect(&value),
            server_version: connection_server_version(&value),
            pages: HashSet::new(),
            connected: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Mark this generation closed. Subsequent calls fail with `Shutdown`;
    /// dlclose happens when the last Arc owner drops.
    pub fn shutdown(&self) -> Result<(), LoaderError> {
        let entry = Arc::clone(&self.entry);
        let cached_handle = {
            self.retiring.store(true, Ordering::Release);
            let mut state = entry
                .state
                .lock()
                .map_err(|_| LoaderError::RegistryPoisoned)?;
            let candidate = match &*state {
                DriverEntryState::Ready {
                    handle: weak_handle,
                    generation,
                } if Arc::ptr_eq(generation, &self.generation) => weak_handle.upgrade(),
                _ => None,
            };
            let accepted = candidate
                .as_ref()
                .is_some_and(|handle| std::ptr::eq(handle.as_ref(), self));
            if accepted {
                *state = DriverEntryState::Retiring {
                    generation: Arc::clone(&self.generation),
                };
            } else {
                self.retiring.store(false, Ordering::Release);
            }
            drop(state);
            if accepted {
                candidate
            } else {
                drop(candidate);
                None
            }
        };

        let result = self.shutdown_inner();
        let mut state = entry
            .state
            .lock()
            .map_err(|_| LoaderError::RegistryPoisoned)?;
        if let Some(handle) = cached_handle.as_ref() {
            if matches!(&*state, DriverEntryState::Retiring { generation } if Arc::ptr_eq(generation, &self.generation))
            {
                match &result {
                    Ok(()) => *state = DriverEntryState::Empty,
                    Err(_) => {
                        *state = DriverEntryState::Ready {
                            handle: Arc::downgrade(handle),
                            generation: Arc::clone(&self.generation),
                        };
                    }
                }
            }
        }
        self.retiring.store(false, Ordering::Release);
        entry.wake.notify_all();
        drop(state);
        drop(cached_handle);
        result
    }

    fn shutdown_inner(&self) -> Result<(), LoaderError> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| LoaderError::RegistryPoisoned)?;
        self.ensure_open()?;
        let owners = self.owners();
        if !owners.is_empty() {
            return Err(owners.into());
        }
        // SAFETY: shutdown is resolved and no driver-owned work remains.
        let response = unsafe { (self.symbols.shutdown)() };
        let response = self.take_string(response)?;
        unwrap_envelope_value(&response)?;
        *self.closed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        Ok(())
    }

    /// Snapshot of the sessions and in-flight work pinning this generation.
    pub fn owners(&self) -> DriverOwners {
        DriverOwners {
            sessions: self.active_sessions.load(Ordering::Acquire),
            pending_connects: self.pending_connects.load(Ordering::Acquire),
            pages: self.active_pages.load(Ordering::Acquire),
            batches: self.active_batches.load(Ordering::Acquire),
            operations: self.active_operation_count(),
            leases: self.active_leases.load(Ordering::Acquire),
        }
    }

    fn ensure_open(&self) -> Result<(), LoaderError> {
        if *self.closed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
            Err(LoaderError::Shutdown)
        } else {
            Ok(())
        }
    }

    pub fn begin_operation(
        self: &Arc<Self>,
        connection_id: u64,
    ) -> Result<DriverOperation, LoaderError> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| LoaderError::RegistryPoisoned)?;
        if self.retiring.load(Ordering::Acquire) {
            return Err(LoaderError::Shutdown);
        }
        self.ensure_open()?;
        let id = OperationId(self.next_operation_id.fetch_add(1, Ordering::Relaxed));
        self.active_operations
            .lock()
            .map_err(|_| LoaderError::Shutdown)?
            .insert(id, connection_id);
        Ok(DriverOperation {
            handle: Arc::clone(self),
            connection_id,
            id,
        })
    }

    /// Sessions of this driver generation that are connected and not yet disconnected (tests count
    /// them to see that a session was closed, not just dropped).
    pub fn active_session_count(&self) -> usize {
        self.active_sessions.load(Ordering::Acquire)
    }

    pub fn active_operation_count(&self) -> usize {
        self.active_operations
            .lock()
            .map(|operations| operations.len())
            .unwrap_or(0)
    }

    pub fn adopt_page(
        self: &Arc<Self>,
        raw: CPageLease,
    ) -> Result<crate::page::PageLease, crate::page::PageError> {
        if let Err(error) = self.ensure_open() {
            let _ = crate::page::PageLease::from_raw(raw);
            return Err(crate::page::PageError::Driver(error));
        }
        crate::page::PageLease::from_raw_with_generation(raw, Arc::clone(self))
    }

    pub fn adopt_arrow_page(
        self: &Arc<Self>,
        raw: CArrowPageLease,
    ) -> Result<crate::arrow::ArrowPageLease, crate::arrow::ArrowPageError> {
        if let Err(error) = self.ensure_open() {
            let _ = crate::arrow::ArrowPageLease::from_raw(raw);
            return Err(crate::arrow::ArrowPageError::Driver(error));
        }
        crate::arrow::ArrowPageLease::from_raw_with_generation(raw, Arc::clone(self))
    }

    pub(crate) fn retain_page_lease(&self) {
        self.active_leases.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn release_page_lease(&self) {
        self.active_leases.fetch_sub(1, Ordering::Release);
    }

    pub fn cancel_operation(
        self: &Arc<Self>,
        connection_id: u64,
        request: &CancelRequest,
    ) -> Result<(), LoaderError> {
        let _owner = self.begin_operation(connection_id)?;
        self.ensure_open()?;
        let symbol =
            self.symbols
                .cancel_operation
                .ok_or_else(|| LoaderError::UnsupportedFeature {
                    feature: "operation.cancel".into(),
                })?;
        let request = serde_json::to_string(request)
            .map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
        let request = CString::new(request)
            .map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
        // SAFETY: the optional symbol was capability-validated; the request
        // pointer remains valid for the duration of the call.
        let response = unsafe { symbol(connection_id, request.as_ptr()) };
        let response = self.take_string(response)?;
        unwrap_envelope_value(&response)?;
        Ok(())
    }

    /// The `(connection id, request JSON)` call every optional JSON entry
    /// point (`mutate`, `export`, `describe_object`, ...) shares: resolves the
    /// capability-gated `symbol`, calls it and decodes the envelope. Needs
    /// neither a session nor its lock; the caller has checked liveness.
    fn call_optional<T: Serialize, R: DeserializeOwned>(
        &self,
        connection_id: u64,
        feature: &str,
        symbol: Option<unsafe extern "C" fn(u64, *const c_char) -> *mut c_char>,
        request: &T,
    ) -> Result<R, LoaderError> {
        self.ensure_open()?;
        let symbol = symbol.ok_or_else(|| LoaderError::UnsupportedFeature {
            feature: feature.into(),
        })?;
        let request = serde_json::to_string(request)
            .map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
        let request = CString::new(request)
            .map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
        // SAFETY: optional symbol was capability-validated; request outlives call.
        let response = unsafe { symbol(connection_id, request.as_ptr()) };
        let response = self.take_string(response)?;
        parse_value_envelope(&response)
    }

    /// `dbengine_mutate` for `request`, refused up front when the driver did
    /// not advertise the mutation kind.
    fn mutate_raw(
        &self,
        connection_id: u64,
        request: &MutationRequest,
    ) -> Result<MutationResult, LoaderError> {
        if !self.capabilities.supports_mutation_kind(&request.kind) {
            let feature = match Capability::for_mutation_kind(&request.kind) {
                Some(capability) => capability.as_str().to_string(),
                None => format!("mutation.{}", request.kind),
            };
            return Err(LoaderError::UnsupportedFeature { feature });
        }
        self.call_optional(connection_id, "mutation.*", self.symbols.mutate, request)
    }

    fn describe_object_raw(
        &self,
        connection_id: u64,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, LoaderError> {
        self.call_optional(
            connection_id,
            Capability::MetadataTableProperties.as_str(),
            self.symbols.describe_object,
            request,
        )
    }

    /// `dbengine_mongo_command`: returns the driver's raw envelope text (the
    /// reply document's key order must reach the caller untouched, so it is not
    /// parsed into a `Value` here).
    fn mongo_command_raw(
        &self,
        connection_id: u64,
        request_json: &str,
    ) -> Result<String, LoaderError> {
        self.ensure_open()?;
        let symbol = self
            .symbols
            .mongo_command
            .ok_or_else(|| LoaderError::UnsupportedFeature {
                feature: Capability::MongoCommand.as_str().into(),
            })?;
        let request = CString::new(request_json)
            .map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
        // SAFETY: the optional symbol was capability-validated; the request
        // pointer remains valid for the duration of the call.
        let response = unsafe { symbol(connection_id, request.as_ptr()) };
        self.take_string(response)
    }

    fn inspect_row_raw(&self, page_id: u64, row_index: u64) -> Result<RowFields, LoaderError> {
        self.ensure_open()?;
        // SAFETY: inspect_row is required and page_id is a driver-local id.
        let response = unsafe { (self.symbols.inspect_row)(page_id, row_index) };
        let response = self.take_string(response)?;
        parse_value_envelope(&response)
    }

    fn take_string(&self, raw: *mut c_char) -> Result<String, LoaderError> {
        if raw.is_null() {
            return Ok(String::new());
        }
        // SAFETY: raw is a NUL-terminated string owned by this driver.
        let c_str = unsafe { CStr::from_ptr(raw) };
        let text = c_str.to_string_lossy().into_owned();
        // SAFETY: raw was allocated by this driver; free_string is its pair.
        unsafe { (self.symbols.free_string)(raw) };
        Ok(text)
    }
}

impl Drop for DriverHandle {
    fn drop(&mut self) {
        let publish_retirement = {
            let mut state = self
                .entry
                .state
                .lock()
                // A panic must not turn into a second one here: `drop` can run while unwinding.
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let matches_generation = match &*state {
                DriverEntryState::Ready { generation, .. }
                | DriverEntryState::Retiring { generation } => {
                    Arc::ptr_eq(generation, &self.generation)
                }
                _ => false,
            };
            if matches_generation {
                *state = DriverEntryState::Retiring {
                    generation: Arc::clone(&self.generation),
                };
            }
            matches_generation
        };
        if !*self.closed.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner) {
            // SAFETY: this is the last Arc owner, so no session or batch can call
            // into the driver while its runtime is being stopped.
            let response = unsafe { (self.symbols.shutdown)() };
            if !response.is_null() {
                // SAFETY: response came from this driver generation.
                unsafe { (self.symbols.free_string)(response) };
            }
        }
        // Runs exactly once, when the last Arc owner (handle or session) is gone.
        drop(self.library.take());
        if publish_retirement {
            let mut state = self
                .entry
                .state
                .lock()
                // A panic must not turn into a second one here: `drop` can run while unwinding.
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(&*state, DriverEntryState::Retiring { generation } if Arc::ptr_eq(generation, &self.generation))
            {
                *state = DriverEntryState::Empty;
                self.entry.wake.notify_all();
            }
        }
    }
}

pub struct DriverSession {
    handle: Arc<DriverHandle>,
    connection_id: u64,
    /// The SQL syntax this connection's server speaks right now, when the
    /// driver reported it in its `dbengine_connect` reply (the MySQL family
    /// reports the session's real `NO_BACKSLASH_ESCAPES` state there). It
    /// overrides the driver's static capabilities for this connection.
    dialect: Option<SqlDialectInfo>,
    /// The server's version in MySQL's numeric form (MySQL 8.4.6 = `80406`, MariaDB
    /// 11.8.9 = `110809`), when the driver reported `server_version` in its connect reply.
    server_version: Option<u32>,
    pages: HashSet<u64>,
    /// Shared with [`SessionAnchor`] so lock-free callers (see
    /// `SharedDriverSession`) observe a disconnect without the session lock.
    connected: Arc<AtomicBool>,
}

/// The immutable identity of a [`DriverSession`] (driver generation,
/// connection id, liveness flag). It lets `SharedDriverSession` register
/// operations and hand out [`QueryRunner`]s **without** locking the session,
/// so a UI thread never queues behind a session held for network I/O.
#[derive(Clone)]
pub(crate) struct SessionAnchor {
    handle: Arc<DriverHandle>,
    connection_id: u64,
    connected: Arc<AtomicBool>,
}

impl SessionAnchor {
    fn ensure_connected(&self) -> Result<(), LoaderError> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(not_connected());
        }
        self.handle.ensure_open()
    }

    pub(crate) fn begin_operation(&self) -> Result<DriverOperation, LoaderError> {
        self.ensure_connected()?;
        self.handle.begin_operation(self.connection_id)
    }

    /// Reads one row of a page the driver holds. The registered operation
    /// pins the driver generation for the duration of the call.
    pub(crate) fn inspect_row(
        &self,
        page_id: u64,
        row_index: u64,
    ) -> Result<RowFields, LoaderError> {
        self.ensure_connected()?;
        let _operation = self.handle.begin_operation(self.connection_id)?;
        self.handle.inspect_row_raw(page_id, row_index)
    }

    /// `dbengine_mutate` under `operation`, which must be one of this
    /// session's (it pins the driver generation for the call and its id is the
    /// one to cancel); the request's own id is replaced by it.
    pub(crate) fn mutate(
        &self,
        operation: &DriverOperation,
        mut request: MutationRequest,
    ) -> Result<MutationResult, LoaderError> {
        self.ensure_connected()?;
        if operation.connection_id != self.connection_id
            || !Arc::ptr_eq(&operation.handle, &self.handle)
        {
            return Err(LoaderError::DriverRejected {
                code: "session.operation_mismatch".into(),
                message: "the operation belongs to a different session".into(),
                retryable: false,
            });
        }
        request.operation_id = operation.id();
        self.handle.mutate_raw(self.connection_id, &request)
    }

    /// `dbengine_describe_object` under a freshly registered operation that
    /// pins the driver generation for the call.
    pub(crate) fn describe_object(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, LoaderError> {
        self.ensure_connected()?;
        let _operation = self.handle.begin_operation(self.connection_id)?;
        self.handle.describe_object_raw(self.connection_id, request)
    }

    pub(crate) fn query_runner(&self) -> Result<QueryRunner, LoaderError> {
        self.ensure_connected()?;
        Ok(QueryRunner {
            handle: Arc::clone(&self.handle),
            connection_id: self.connection_id,
            operation: Arc::new(self.handle.begin_operation(self.connection_id)?),
        })
    }
}

fn not_connected() -> LoaderError {
    LoaderError::DriverRejected {
        code: "session.not_connected".into(),
        message: "driver session is disconnected".into(),
        retryable: false,
    }
}

/// See [`DriverSession::query_runner`].
#[derive(Clone)]
pub struct QueryRunner {
    handle: Arc<DriverHandle>,
    connection_id: u64,
    /// Registers a driver operation for as long as any clone of the runner
    /// lives: the driver cannot be retired or unloaded under a query that is
    /// still running through it (the same guarantee a [`DriverOperation`]
    /// gives), so a runner cannot be misused past the driver's lifetime.
    operation: Arc<DriverOperation>,
}

impl QueryRunner {
    /// The host-allocated id of the operation this runner registered; put it
    /// in the request and pass it to `cancel_operation` to cancel the query.
    pub fn operation_id(&self) -> OperationId {
        self.operation.id()
    }

    /// The operation the runner keeps registered. Clone it to keep the
    /// driver pinned (and the id cancellable) beyond the runner's own life.
    pub fn operation(&self) -> &Arc<DriverOperation> {
        &self.operation
    }

    /// `dbengine_inspect_row` without borrowing the session: a read of a page
    /// the driver already holds, so it never queues behind a session that is
    /// busy with network I/O. The page may be released concurrently; the
    /// driver then answers with an error, never undefined behaviour.
    pub fn inspect_row(&self, page_id: u64, row_index: u64) -> Result<RowFields, LoaderError> {
        self.handle.inspect_row_raw(page_id, row_index)
    }

    pub fn open_query(&self, request_json: &str) -> Result<QueryPage, LoaderError> {
        self.handle.ensure_open()?;
        let request = CString::new(request_json).map_err(|error| LoaderError::DlOpenFailed {
            message: error.to_string(),
        })?;
        // SAFETY: symbols are resolved; request outlives the call.
        let response =
            unsafe { (self.handle.symbols.open_query)(self.connection_id, request.as_ptr()) };
        let response = self.handle.take_string(response)?;
        parse_value_envelope(&response)
    }

    /// `dbengine_export` without borrowing the session: an export streams a
    /// whole result and may run for minutes, so it must not hold the session
    /// lock (cancel it through the operation id).
    pub fn export(&self, request: &ExportRequest) -> Result<ExportResult, LoaderError> {
        self.handle.ensure_open()?;
        let symbol = self
            .handle
            .symbols
            .export
            .ok_or_else(|| LoaderError::UnsupportedFeature {
                feature: "export.*".into(),
            })?;
        let request = serde_json::to_string(request)
            .map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
        let request = CString::new(request)
            .map_err(|error| LoaderError::MalformedResponse(error.to_string()))?;
        // SAFETY: the optional symbol was capability-validated; the request
        // pointer remains valid for the duration of the call.
        let response = unsafe { symbol(self.connection_id, request.as_ptr()) };
        let response = self.handle.take_string(response)?;
        parse_value_envelope(&response)
    }

    /// `dbengine_mongo_command` (capability `mongo.command`): one raw
    /// `runCommand`. `request_json` is `{operation_id, database, command,
    /// read_only?, max_time_ms?}` (see the driver's `mongo_command` module);
    /// returns the driver's response envelope as text, unparsed. Without the
    /// capability: [`LoaderError::UnsupportedFeature`].
    pub fn mongo_command(&self, request_json: &str) -> Result<String, LoaderError> {
        self.handle
            .mongo_command_raw(self.connection_id, request_json)
    }

    /// Cancels the operation this runner registered (the one its requests
    /// carry): a command in flight stops and the next one starts cancelled.
    pub fn cancel_operation(&self) -> Result<(), LoaderError> {
        self.handle.cancel_operation(
            self.connection_id,
            &CancelRequest {
                operation_id: self.operation.id(),
            },
        )
    }

    /// Best-effort release of a page nobody owns (see
    /// [`DriverSession::adopt_page`]).
    pub fn release_page(&self, page_id: u64) {
        // SAFETY: release_page is required and page_id came from this driver.
        let response = unsafe { (self.handle.symbols.release_page)(page_id) };
        if !response.is_null() {
            // SAFETY: response belongs to this handle generation.
            unsafe { (self.handle.symbols.free_string)(response) };
        }
    }
}

pub struct DriverOperation {
    handle: Arc<DriverHandle>,
    connection_id: u64,
    id: OperationId,
}

impl DriverOperation {
    pub fn id(&self) -> OperationId {
        self.id
    }

    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }
}

impl Drop for DriverOperation {
    fn drop(&mut self) {
        if let Ok(mut operations) = self.handle.active_operations.lock() {
            operations.remove(&self.id);
        }
    }
}

impl DriverSession {
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub fn capabilities(&self) -> &DriverCapabilities {
        &self.handle.capabilities
    }

    /// The server version from the connect reply, in MySQL's numeric form (MySQL
    /// 8.4.6 = `80406`, MariaDB 11.8.9 = `110809`): what
    /// `SqlLexOptions::with_server_version` takes. `None` when the driver did not report it.
    pub fn server_version(&self) -> Option<u32> {
        self.server_version
    }

    /// The per-connection SQL dialect from the connect reply; `None` when the
    /// driver reported none (the static `capabilities().sql_dialect` applies).
    pub fn connection_dialect(&self) -> Option<&SqlDialectInfo> {
        self.dialect.as_ref()
    }

    fn ensure_connected(&self) -> Result<(), LoaderError> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(not_connected());
        }
        self.handle.ensure_open()
    }

    pub(crate) fn anchor(&self) -> SessionAnchor {
        SessionAnchor {
            handle: Arc::clone(&self.handle),
            connection_id: self.connection_id,
            connected: Arc::clone(&self.connected),
        }
    }

    pub fn begin_operation(&self) -> Result<DriverOperation, LoaderError> {
        self.ensure_connected()?;
        self.handle.begin_operation(self.connection_id)
    }

    pub fn acquire_page(
        &self,
        page_id: u64,
        row_start: u64,
        row_count: u64,
    ) -> Result<crate::page::PageLease, crate::page::PageError> {
        self.ensure_connected()?;
        let table =
            self.handle
                .page_function_table()
                .ok_or_else(|| LoaderError::UnsupportedFeature {
                    feature: "query.page_lease".into(),
                })?;
        let acquire = table
            .acquire_page
            .ok_or_else(|| LoaderError::UnsupportedFeature {
                feature: "query.page_lease".into(),
            })?;
        // SAFETY: the function table was validated during driver load and the
        // page arguments are driver-local handles from this session.
        let raw = unsafe { acquire(self.connection_id, page_id, row_start, row_count) };
        self.handle.adopt_page(raw)
    }

    pub fn acquire_arrow_page(
        &self,
        page_id: u64,
        row_start: u64,
        row_count: u64,
    ) -> Result<crate::arrow::ArrowPageLease, crate::arrow::ArrowPageError> {
        self.ensure_connected()?;
        let table =
            self.handle
                .arrow_function_table()
                .ok_or_else(|| LoaderError::UnsupportedFeature {
                    feature: "query.arrow_c_data".into(),
                })?;
        let acquire = table
            .acquire_page
            .ok_or_else(|| LoaderError::UnsupportedFeature {
                feature: "query.arrow_c_data".into(),
            })?;
        // SAFETY: the table was validated at load time and all ids are owned
        // by this session's driver generation.
        let raw = unsafe { acquire(self.connection_id, page_id, row_start, row_count) };
        self.handle.adopt_arrow_page(raw)
    }

    pub fn list_nav_nodes(&mut self, request_json: &str) -> Result<NavigatorPage, LoaderError> {
        self.ensure_connected()?;
        let request = CString::new(request_json).map_err(|error| LoaderError::DlOpenFailed {
            message: error.to_string(),
        })?;
        // SAFETY: symbols are resolved; request outlives the call.
        let response =
            unsafe { (self.handle.symbols.list_nav_nodes)(self.connection_id, request.as_ptr()) };
        let response = self.handle.take_string(response)?;
        parse_value_envelope(&response)
    }

    pub fn open_query(&mut self, request_json: &str) -> Result<QueryPage, LoaderError> {
        let page = self.query_runner()?.open_query(request_json)?;
        self.track_page(page.page_id);
        Ok(page)
    }

    fn track_page(&mut self, page_id: u64) {
        if self.pages.insert(page_id) {
            self.handle.active_pages.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A detached handle that runs `open_query` **without borrowing the
    /// session**, so a slow query (a 30 s aggregate) does not keep the
    /// session lock and queue every other tab of the connection behind it.
    /// Pair it with [`Self::adopt_page`] once the query returned.
    pub fn query_runner(&self) -> Result<QueryRunner, LoaderError> {
        self.anchor().query_runner()
    }

    /// Registers a page opened through a [`QueryRunner`] with this session
    /// (release / disconnect bookkeeping). If the session was disconnected
    /// while the query ran, the page is released straight away and an error
    /// is returned, so nothing leaks.
    pub fn adopt_page(&mut self, runner: &QueryRunner, page_id: u64) -> Result<(), LoaderError> {
        // A page belongs to the connection that opened it: a runner of another
        // session (or driver) must not register pages here.
        if runner.connection_id != self.connection_id || !Arc::ptr_eq(&runner.handle, &self.handle)
        {
            runner.release_page(page_id);
            return Err(LoaderError::DriverRejected {
                code: "session.runner_mismatch".into(),
                message: "the query runner belongs to a different session".into(),
                retryable: false,
            });
        }
        if let Err(error) = self.ensure_connected() {
            runner.release_page(page_id);
            return Err(error);
        }
        self.track_page(page_id);
        Ok(())
    }

    pub fn render_cell_batch(
        &mut self,
        page_id: u64,
        row_start: u64,
        row_count: u64,
        column_start: u64,
        column_count: u64,
    ) -> Result<HostCellBatch, LoaderError> {
        self.ensure_connected()?;
        let mut batch: *mut CCellTextBatch = std::ptr::null_mut();
        let mut error_json: *mut c_char = std::ptr::null_mut();
        // SAFETY: symbols are resolved; out pointers are valid locals.
        let status = unsafe {
            (self.handle.symbols.render_cell_batch)(
                page_id,
                row_start,
                row_count,
                column_start,
                column_count,
                &mut batch,
                &mut error_json,
            )
        };

        if status != 0 {
            let message = self.handle.take_string(error_json)?;
            let envelope: DriverErrorEnvelope = serde_json::from_str(&message)
                .map_err(|_| LoaderError::MalformedResponse(message.clone()))?;
            return Err(LoaderError::DriverRejected {
                code: envelope.error.code,
                message: envelope.error.message,
                retryable: envelope.error.retryable,
            });
        }
        if batch.is_null() {
            return Err(LoaderError::MalformedResponse(
                "render_cell_batch returned null batch".into(),
            ));
        }
        if batch.align_offset(std::mem::align_of::<CCellTextBatch>()) != 0 {
            // SAFETY: the driver returned this non-null batch pointer and owns
            // its deallocator according to the negotiated ABI.
            unsafe { (self.handle.symbols.free_cell_text_batch)(batch) };
            return Err(LoaderError::MalformedResponse(
                "render_cell_batch returned an unaligned batch".into(),
            ));
        }

        self.handle.active_batches.fetch_add(1, Ordering::Relaxed);
        Ok(HostCellBatch {
            handle: Arc::clone(&self.handle),
            raw: batch,
        })
    }

    pub fn disconnect(&mut self) -> Result<(), LoaderError> {
        if !self.connected.load(Ordering::Acquire) {
            return Ok(());
        }
        self.release_all_pages();
        // SAFETY: symbols are resolved; connection_id is driver-local.
        let response = unsafe { (self.handle.symbols.disconnect)(self.connection_id) };
        let response = self.handle.take_string(response)?;
        let envelope: serde_json::Value = parse_envelope(&response)?;
        if envelope.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            self.connected.store(false, Ordering::Release);
            self.handle.active_sessions.fetch_sub(1, Ordering::Release);
            Ok(())
        } else {
            Err(LoaderError::DriverRejected {
                code: "disconnect.failed".into(),
                message: response,
                retryable: false,
            })
        }
    }

    pub fn inspect_row(&mut self, page_id: u64, row_index: u64) -> Result<RowFields, LoaderError> {
        self.ensure_connected()?;
        // SAFETY: inspect_row is required and page_id belongs to this driver.
        let response = unsafe { (self.handle.symbols.inspect_row)(page_id, row_index) };
        let response = self.handle.take_string(response)?;
        parse_value_envelope(&response)
    }

    pub fn release_page(&mut self, page_id: u64) -> Result<(), LoaderError> {
        self.ensure_connected()?;
        // SAFETY: release_page is required and page_id belongs to this driver.
        let response = unsafe { (self.handle.symbols.release_page)(page_id) };
        let response = self.handle.take_string(response)?;
        unwrap_envelope_value(&response)?;
        if self.pages.remove(&page_id) {
            self.handle.active_pages.fetch_sub(1, Ordering::Release);
        }
        Ok(())
    }

    pub fn mutate(&mut self, request: &MutationRequest) -> Result<MutationResult, LoaderError> {
        self.ensure_connected()?;
        let result = self.handle.mutate_raw(self.connection_id, request)?;
        self.forget_pages(&result.invalidated_page_ids);
        Ok(result)
    }

    /// Drops the bookkeeping of pages a mutation invalidated (the driver
    /// already freed them).
    pub(crate) fn forget_pages(&mut self, page_ids: &[u64]) {
        for page_id in page_ids {
            if self.pages.remove(page_id) {
                self.handle.active_pages.fetch_sub(1, Ordering::Release);
            }
        }
    }

    pub fn cancel_operation(&mut self, request: &CancelRequest) -> Result<(), LoaderError> {
        self.call_optional::<_, serde_json::Value>(
            "operation.cancel",
            self.handle.symbols.cancel_operation,
            request,
        )?;
        Ok(())
    }

    pub fn export(&mut self, request: &ExportRequest) -> Result<ExportResult, LoaderError> {
        self.call_optional("export.*", self.handle.symbols.export, request)
    }

    /// `dbengine_describe_object` (capability `metadata.table_properties`):
    /// caller-defined JSON in, caller-defined JSON out (PostgreSQL returns the
    /// table-properties document). Without the capability this fails with
    /// [`LoaderError::UnsupportedFeature`] and never calls into the driver.
    pub fn describe_object(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, LoaderError> {
        self.ensure_connected()?;
        self.handle.describe_object_raw(self.connection_id, request)
    }

    fn call_optional<T: Serialize, R: DeserializeOwned>(
        &self,
        feature: &str,
        symbol: Option<unsafe extern "C" fn(u64, *const c_char) -> *mut c_char>,
        request: &T,
    ) -> Result<R, LoaderError> {
        self.ensure_connected()?;
        self.handle
            .call_optional(self.connection_id, feature, symbol, request)
    }

    fn release_all_pages(&mut self) {
        for page_id in self.pages.drain() {
            // SAFETY: release_page is required; best-effort cleanup during disconnect.
            let response = unsafe { (self.handle.symbols.release_page)(page_id) };
            if !response.is_null() {
                // SAFETY: response belongs to this handle generation.
                unsafe { (self.handle.symbols.free_string)(response) };
            }
            self.handle.active_pages.fetch_sub(1, Ordering::Release);
        }
    }
}

impl Drop for DriverSession {
    fn drop(&mut self) {
        if self.connected.load(Ordering::Acquire) {
            let _ = self.disconnect();
        }
    }
}

/// A cell batch owned by the host, freed through the originating driver.
pub struct HostCellBatch {
    handle: Arc<DriverHandle>,
    raw: *mut CCellTextBatch,
}

impl HostCellBatch {
    pub fn cell_count(&self) -> u64 {
        // SAFETY: raw is a valid batch from the driver until drop.
        unsafe { (*self.raw).cell_count }
    }

    pub fn cell(&self, index: u64) -> Option<HostCell> {
        // SAFETY: raw is a valid batch; cells points to cell_count entries.
        let batch = unsafe { &*self.raw };
        if index >= batch.cell_count {
            return None;
        }
        let index = usize::try_from(index).ok()?;
        if index > isize::MAX as usize
            || batch.cells.is_null()
            || batch.cells.align_offset(std::mem::align_of::<CCellText>()) != 0
        {
            return None;
        }
        // SAFETY: index is bounds-checked above.
        let cell = unsafe { *batch.cells.add(index) };
        let text_buffer_len = usize::try_from(batch.text_buffer_len).ok()?;
        let offset = usize::try_from(cell.offset).ok()?;
        let length = usize::try_from(cell.length).ok()?;
        if offset > text_buffer_len || length > text_buffer_len - offset {
            return None;
        }
        if length != 0 && batch.text_buffer.is_null() {
            return None;
        }
        let text = if length == 0 {
            String::new()
        } else {
            // SAFETY: null and offset+length were validated above.
            let bytes =
                unsafe { std::slice::from_raw_parts(batch.text_buffer.add(offset), length) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        Some(HostCell {
            row: cell.row,
            column: cell.column,
            length: cell.length,
            flags: cell.flags,
            text,
        })
    }
}

impl Drop for HostCellBatch {
    fn drop(&mut self) {
        // SAFETY: raw was allocated by this handle's render_cell_batch.
        unsafe { (self.handle.symbols.free_cell_text_batch)(self.raw) };
        self.handle.active_batches.fetch_sub(1, Ordering::Release);
    }
}

pub struct HostCell {
    pub row: u64,
    pub column: u64,
    pub length: u64,
    pub flags: u32,
    pub text: String,
}

struct BootstrapSymbols {
    abi_version: unsafe extern "C" fn() -> u32,
    capabilities: unsafe extern "C" fn() -> *mut c_char,
    free_string: unsafe extern "C" fn(*mut c_char),
}

fn resolve_bootstrap_symbols(library: &NativeLibrary) -> Result<BootstrapSymbols, LoaderError> {
    Ok(BootstrapSymbols {
        // SAFETY: library is open.
        abi_version: unsafe { load_symbol(library, "dbengine_abi_version")? },
        // SAFETY: library is open.
        capabilities: unsafe { load_symbol(library, "dbengine_capabilities")? },
        // SAFETY: library is open.
        free_string: unsafe { load_symbol(library, "dbengine_free_string")? },
    })
}

fn resolve_symbols(
    library: &NativeLibrary,
    bootstrap: BootstrapSymbols,
    capabilities: &DriverCapabilities,
) -> Result<DriverSymbols, LoaderError> {
    Ok(DriverSymbols {
        // SAFETY: library is open; each dlsym either resolves
        // the named export or we fail with MissingSymbol.
        init: unsafe { load_symbol(library, "dbengine_init")? },
        shutdown: unsafe { load_symbol(library, "dbengine_shutdown")? },
        connect: unsafe { load_symbol(library, "dbengine_connect")? },
        disconnect: unsafe { load_symbol(library, "dbengine_disconnect")? },
        list_nav_nodes: unsafe { load_symbol(library, "dbengine_list_nav_nodes")? },
        open_query: unsafe { load_symbol(library, "dbengine_open_query")? },
        render_cell_batch: unsafe { load_symbol(library, "dbengine_render_cell_batch")? },
        inspect_row: unsafe { load_symbol(library, "dbengine_inspect_row")? },
        release_page: unsafe { load_symbol(library, "dbengine_release_page")? },
        mutate: load_optional_symbol(library, capabilities.has_any_mutation(), "dbengine_mutate")?,
        cancel_operation: load_optional_symbol(
            library,
            capabilities.has(Capability::OperationCancel),
            "dbengine_cancel_operation",
        )?,
        export: load_optional_symbol(library, capabilities.has_any_export(), "dbengine_export")?,
        describe_object: load_optional_symbol(
            library,
            capabilities.has(Capability::MetadataTableProperties),
            "dbengine_describe_object",
        )?,
        mongo_command: load_optional_symbol(
            library,
            capabilities.has(Capability::MongoCommand),
            "dbengine_mongo_command",
        )?,
        free_string: bootstrap.free_string,
        free_cell_text_batch: unsafe { load_symbol(library, "dbengine_free_cell_text_batch")? },
        page_function_table: if capabilities.has(Capability::QueryPageLease) {
            // SAFETY: capability negotiation requires this ABI-defined symbol.
            Some(unsafe { load_symbol(library, "dbengine_page_function_table")? })
        } else {
            None
        },
        arrow_function_table: if capabilities.has(Capability::QueryArrowCData) {
            // SAFETY: capability negotiation requires this ABI-defined symbol.
            Some(unsafe { load_symbol(library, "dbengine_arrow_function_table")? })
        } else {
            None
        },
    })
}

fn load_page_function_table(
    symbols: &DriverSymbols,
    capabilities: &DriverCapabilities,
) -> Result<Option<CPageFunctionTable>, LoaderError> {
    let Some(table_fn) = symbols.page_function_table else {
        return Ok(None);
    };
    // SAFETY: the symbol is capability-gated and returns a table owned by the
    // live driver generation.
    let table_ptr = unsafe { table_fn() };
    if table_ptr.is_null() {
        return Err(LoaderError::InvalidCapabilities(
            "query.page_lease returned a null function table".into(),
        ));
    }
    // SAFETY: every versioned table begins with this fixed-size header.
    let header = unsafe { std::ptr::read(table_ptr.cast::<FunctionTableHeader>()) };
    let is_header_only = (header.struct_size as usize) < std::mem::size_of::<CPageFunctionTable>();
    let table = if is_header_only {
        CPageFunctionTable {
            struct_size: header.struct_size,
            abi_version: header.abi_version,
            acquire_page: None,
        }
    } else {
        // SAFETY: the driver reports a table at least as large as the host
        // version before the host reads the optional function pointer.
        unsafe { std::ptr::read(table_ptr) }
    };
    validate_page_function_table(&table).map_err(|error| {
        LoaderError::InvalidCapabilities(format!("page function table: {error:?}"))
    })?;
    if !capabilities.has(Capability::QueryPageLease) {
        return Err(LoaderError::InvalidCapabilities(
            "page function table requires query.page_lease".into(),
        ));
    }
    Ok((!is_header_only).then_some(table))
}

fn load_arrow_function_table(
    symbols: &DriverSymbols,
    capabilities: &DriverCapabilities,
) -> Result<Option<CArrowFunctionTable>, LoaderError> {
    let Some(table_fn) = symbols.arrow_function_table else {
        return Ok(None);
    };
    // SAFETY: the symbol is capability-gated and returns a table owned by the
    // live driver generation.
    let table_ptr = unsafe { table_fn() };
    if table_ptr.is_null() {
        return Err(LoaderError::InvalidCapabilities(
            "query.arrow_c_data returned a null function table".into(),
        ));
    }
    // SAFETY: every versioned table begins with this fixed-size header.
    let header = unsafe { std::ptr::read(table_ptr.cast::<FunctionTableHeader>()) };
    let is_header_only = (header.struct_size as usize) < std::mem::size_of::<CArrowFunctionTable>();
    let table = if is_header_only {
        CArrowFunctionTable {
            struct_size: header.struct_size,
            abi_version: header.abi_version,
            acquire_page: None,
        }
    } else {
        // SAFETY: the driver reports a table at least as large as the host
        // version before the host reads the optional function pointer.
        unsafe { std::ptr::read(table_ptr) }
    };
    validate_arrow_function_table(&table).map_err(|error| {
        LoaderError::InvalidCapabilities(format!("Arrow function table: {error}"))
    })?;
    if !capabilities.has(Capability::QueryArrowCData) {
        return Err(LoaderError::InvalidCapabilities(
            "Arrow function table requires query.arrow_c_data".into(),
        ));
    }
    Ok((!is_header_only).then_some(table))
}

fn load_optional_symbol<T>(
    library: &NativeLibrary,
    advertised: bool,
    symbol: &str,
) -> Result<Option<T>, LoaderError> {
    if advertised {
        // SAFETY: capability requires symbol with ABI-defined type T.
        unsafe { load_symbol(library, symbol).map(Some) }
    } else {
        Ok(None)
    }
}

/// SAFETY: `T` must be the ABI-defined pointer-sized function type of the export.
unsafe fn load_symbol<T>(library: &NativeLibrary, name: &str) -> Result<T, LoaderError> {
    let missing = || LoaderError::MissingSymbol {
        symbol: name.into(),
    };
    let c_name = CString::new(name).map_err(|_| missing())?;
    let ptr = library.symbol(&c_name).ok_or_else(missing)?;
    // SAFETY: ptr points to the driver's exported function of type T.
    Ok(unsafe { std::mem::transmute_copy(&ptr.as_ptr()) })
}

fn take_driver_string(
    produce: unsafe extern "C" fn() -> *mut c_char,
    free: unsafe extern "C" fn(*mut c_char),
) -> Result<String, LoaderError> {
    // SAFETY: produce is the driver's allocator for this string.
    let raw = unsafe { produce() };
    if raw.is_null() {
        return Ok(String::new());
    }
    // SAFETY: raw is a NUL-terminated driver string.
    let text = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: free is the matching deallocator for produce.
    unsafe { free(raw) };
    Ok(text)
}

fn take_raw_driver_string(
    raw: *mut c_char,
    free: unsafe extern "C" fn(*mut c_char),
) -> Result<String, LoaderError> {
    if raw.is_null() {
        return Ok(String::new());
    }
    // SAFETY: raw is a NUL-terminated driver string.
    let text = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: free is the matching driver deallocator.
    unsafe { free(raw) };
    Ok(text)
}

fn parse_envelope<T: DeserializeOwned>(json: &str) -> Result<T, LoaderError> {
    serde_json::from_str(json)
        .map_err(|error| LoaderError::MalformedResponse(format!("{error}: {json}")))
}

/// Parse `{"ok":true,"value":V}` and return V; map ok:false to DriverRejected.
fn parse_value_envelope<T: DeserializeOwned>(json: &str) -> Result<T, LoaderError> {
    let envelope: serde_json::Value = parse_envelope(json)?;
    match envelope.get("ok").and_then(serde_json::Value::as_bool) {
        Some(true) => {
            let value = envelope
                .get("value")
                .cloned()
                .ok_or_else(|| LoaderError::MalformedResponse(json.into()))?;
            serde_json::from_value(value)
                .map_err(|error| LoaderError::MalformedResponse(format!("{error}: {json}")))
        }
        _ => Err(driver_rejected_from_envelope(&envelope, json)),
    }
}

/// The `sql_dialect` object of a `dbengine_connect` reply. A reply without it,
/// or with one that is not a dialect, reports none: the host keeps the static
/// capabilities rather than guessing.
fn connection_dialect(connect_value: &serde_json::Value) -> Option<SqlDialectInfo> {
    serde_json::from_value(connect_value.get("sql_dialect")?.clone()).ok()
}

/// The `server_version` of a `dbengine_connect` reply; absent or not a number: `None`.
fn connection_server_version(connect_value: &serde_json::Value) -> Option<u32> {
    u32::try_from(connect_value.get("server_version")?.as_u64()?).ok()
}

/// Parse `{"ok":true,"value":{...}}` returning the raw value; errors on ok:false.
fn unwrap_envelope_value(json: &str) -> Result<serde_json::Value, LoaderError> {
    let envelope: serde_json::Value = parse_envelope(json)?;
    match envelope.get("ok").and_then(serde_json::Value::as_bool) {
        Some(true) => Ok(envelope
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null)),
        _ => Err(driver_rejected_from_envelope(&envelope, json)),
    }
}

fn driver_rejected_from_envelope(envelope: &serde_json::Value, fallback: &str) -> LoaderError {
    let code = envelope
        .pointer("/error/code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("driver.error");
    let message = envelope
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback);
    let retryable = envelope
        .pointer("/error/retryable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    LoaderError::DriverRejected {
        code: code.into(),
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod connection_dialect_tests {
    use super::connection_dialect;
    use serde_json::json;

    #[test]
    fn the_connect_reply_dialect_is_read() {
        let reply = json!({
            "connection_id": 7,
            "sql_dialect": {
                "identifier_quote": "`",
                "string_literal_quote": "'",
                "supports_schemas": false,
                "database_qualified_names": true,
                "string_backslash_escapes": false,
                "double_quote_identifiers": true,
                "bracket_identifiers": true,
            },
        });
        let dialect = connection_dialect(&reply).expect("a dialect");
        assert_eq!(dialect.identifier_quote, "`");
        assert!(!dialect.string_backslash_escapes);
        assert!(dialect.double_quote_identifiers && dialect.bracket_identifiers);
        // The same reply with the session in the default mode.
        let mut backslash = reply.clone();
        backslash["sql_dialect"]["string_backslash_escapes"] = json!(true);
        assert!(
            connection_dialect(&backslash)
                .unwrap()
                .string_backslash_escapes
        );
    }

    #[test]
    fn the_connect_reply_server_version_is_read() {
        use super::connection_server_version;
        assert_eq!(
            connection_server_version(&json!({"connection_id": 7, "server_version": 80411})),
            Some(80411)
        );
        for reply in [
            json!({"connection_id": 7}),
            json!({"server_version": null}),
            json!({"server_version": "8.4.11"}),
            json!({"server_version": -1}),
            json!({"server_version": 99_999_999_999_u64}),
        ] {
            assert_eq!(connection_server_version(&reply), None, "{reply}");
        }
    }

    #[test]
    fn a_reply_without_a_usable_dialect_reports_none() {
        assert_eq!(connection_dialect(&json!({"connection_id": 7})), None);
        assert_eq!(connection_dialect(&json!({"sql_dialect": null})), None);
        assert_eq!(connection_dialect(&json!({"sql_dialect": "mysql"})), None);
        // Missing the quotes: not a dialect, not a guess.
        assert_eq!(
            connection_dialect(&json!({"sql_dialect": {"string_backslash_escapes": true}})),
            None
        );
    }
}
