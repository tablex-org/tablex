//! Driver Manager: the catalog, the install queue, the operations on installed
//! drivers (uninstall, disable, roll back, import, reconnect) and the
//! missing-driver prompt. `model` is the pure part, `view` the tab.

mod model;
mod prompt;
mod tools;
mod tools_view;
mod view;

use super::*;
use engine_driver_host::{
    DriverCatalogItem, DriverInstaller, DriverLoader, DriverManifestEntry, DriverOwners,
    DriverVerifier, InstallError as HostInstallError, InstallProgress, InstalledDriverRecord,
    InstalledDriverStore, NoProgress, ProgressSink, TrustPolicy, UninstallOutcome,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(super) use model::{
    badge_for, format_size, format_size_pair, row_action, version_text, Badge, CancelEffect, CatalogModel,
    DriverFilter, DriverRow, Enqueued, InstallFailure, InstallJob, InstallQueue, InstallState,
    JobKind, JobOutcome, Origin, PrimaryAction, RowStatus,
};
#[cfg(test)]
pub(super) use model::primary_action;
pub(super) use prompt::MissingDriverPrompt;
pub(super) use tools::{addon_actions, installable_entry, AddonNeed, FormatterSeed, ShellAddonSeed, ToolAction};
use tools::ToolsUi;
#[cfg(test)]
pub(super) use tools::{
    client_config_json, code_span, tool_actions, tool_version_lines, ClientTextError, ShellQuoting,
    MCP_CAPABILITY_NOTE,
};

/// How long an uninstall waits for sessions that were just disconnected to
/// let go of the driver (their disconnect runs on a blocking thread).
const OWNER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StorageStats {
    pub(super) installed_bytes: u64,
    pub(super) cache_bytes: u64,
}

/// Everything the tab shows, read off the UI thread in one go.
pub(super) struct Snapshot {
    catalog: Vec<DriverCatalogItem>,
    records: BTreeMap<String, InstalledDriverRecord>,
    installed: HashSet<String>,
    storage: StorageStats,
    /// The manifest could not be fetched (the catalog is the cached one).
    fetch_error: Option<String>,
    /// Tools of the manifest merged with what is installed (empty without a tools directory).
    tools: Vec<engine_driver_host::ToolCatalogItem>,
    /// The trust policy this snapshot was verified under.
    policy: TrustPolicy,
}

/// Why a snapshot could not be made. `untrusted`: the manifest failed verification (or could not
/// be verified at all), so nothing read earlier under a weaker policy may stay on offer.
#[derive(Debug)]
pub(super) struct SnapshotError {
    pub(super) message: String,
    pub(super) untrusted: bool,
}

impl SnapshotError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            untrusted: false,
        }
    }

    fn untrusted(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            untrusted: true,
        }
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Fetch (or re-read the cached) manifest, merge it with the store.
async fn load_snapshot(
    store: Arc<InstalledDriverStore>,
    source: String,
    tools: Option<Arc<engine_driver_host::ToolStore>>,
    trust: Result<engine_driver_host::ManifestTrust, engine_driver_host::ManifestTrustError>,
    offline: bool,
) -> Result<Snapshot, SnapshotError> {
    let trust = trust.map_err(|error| SnapshotError::untrusted(error.to_string()))?;
    load_snapshot_with(trust, store, tools, source, offline).await
}

/// [`load_snapshot`] under an explicit `trust` (tests inject a keyless one).
async fn load_snapshot_with(
    trust: engine_driver_host::ManifestTrust,
    store: Arc<InstalledDriverStore>,
    tools: Option<Arc<engine_driver_host::ToolStore>>,
    source: String,
    offline: bool,
) -> Result<Snapshot, SnapshotError> {
    let policy = trust.policy();
    if !offline {
        // Never fetch a manifest this build could not verify anyway.
        crate::drivers::ensure_catalog_verifiable(&trust, &source).map_err(SnapshotError::untrusted)?;
    }
    let fetcher = if offline {
        crate::drivers::HttpManifestFetcher::offline()
    } else {
        crate::drivers::HttpManifestFetcher::default()
    };
    let registry = engine_driver_host::RegistryService::new(source.clone(), &fetcher, &store)
        .with_app_version(env!("CARGO_PKG_VERSION"))
        .with_manifest_trust(trust);
    let target = crate::drivers::current_target();
    // Reads the tools' install records, like the driver store: off the UI thread.
    let (catalog, tool_items) = match &tools {
        Some(tools) => registry
            .catalog_with_tools(&target, tools)
            .await
            .map(|full| (full.drivers, full.tools)),
        None => registry.catalog(&target).await.map(|drivers| (drivers, Vec::new())),
    }
    .map_err(|error| SnapshotError {
        message: crate::drivers::explain_registry_error(&error, &source, policy),
        untrusted: matches!(error, engine_driver_host::RegistryError::Manifest(_)),
    })?;
    let fetch_error = fetcher.take_failure();
    smol::unblock(move || {
        let records = store.load_all().map_err(|error| SnapshotError::other(error.to_string()))?;
        let installed = records
            .iter()
            .filter(|(_, record)| !record.disabled)
            .map(|(id, _)| id.clone())
            .collect();
        let installed_bytes = records
            .keys()
            .map(|id| {
                crate::drivers::tree_bytes(
                    &store.base_directory().join("Drivers").join("v2").join(id),
                )
            })
            .sum();
        let cache_bytes = crate::drivers::directory_bytes(&crate::drivers::downloads_dir(&store));
        Ok(Snapshot {
            catalog,
            records,
            installed,
            storage: StorageStats {
                installed_bytes,
                cache_bytes,
            },
            fetch_error,
            tools: tool_items,
            policy,
        })
    })
    .await
}

pub(super) struct DriverManagerUi {
    /// Newest loaded generation per driver id.
    pub(super) handles: HashMap<String, Arc<engine_driver_host::DriverHandle>>,
    /// Ids of the installed, enabled drivers: the route decision reads this
    /// instead of the store, which is a locked file on disk.
    pub(super) installed: HashSet<String>,
    /// False until the startup read of the store finished (off the UI thread).
    pub(super) installed_loaded: bool,
    /// A real catalog + store snapshot was applied (not a fixture): the
    /// picker may then hide kinds that have no driver.
    pub(super) snapshot_applied: bool,
    pub(super) store: Arc<InstalledDriverStore>,
    /// The effective policy: Developer when the user turned Developer mode on
    /// (or, in debug builds, `TABLEX_DRIVER_DEV=1`).
    pub(super) policy: TrustPolicy,
    /// Tests: verify manifests against this trust instead of the keys compiled into the build
    /// (the private halves of those are not in the repository).
    #[cfg(test)]
    pub(super) trust_override: Option<engine_driver_host::ManifestTrust>,
    /// The user's persisted Developer mode choice (the toggle in the ⋯ menu).
    pub(super) developer_mode: bool,
    /// Manifest URL override (tests); else `crate::drivers::manifest_source`.
    pub(super) manifest_source: Option<String>,
    pub(super) catalog: Vec<DriverCatalogItem>,
    /// The policy `catalog` and the tools list were read under; an install is refused while it is
    /// not the current one (a catalog read in Developer mode must not install in Strict mode).
    pub(super) catalog_policy: Option<TrustPolicy>,
    /// Bumped whenever what was read from a manifest stops being trusted ([`Self::distrust_catalog`]);
    /// with the policy it is the [`Origin`] of the install jobs.
    pub(super) trust_epoch: u64,
    pub(super) records: BTreeMap<String, InstalledDriverRecord>,
    pub(super) model: CatalogModel,
    pub(super) storage: StorageStats,
    pub(super) catalog_error: Option<String>,
    /// The Tools section (`tablex-mcp`).
    pub(super) tools: ToolsUi,
    pub(super) catalog_task: Option<Task<()>>,
    pub(super) catalog_generation: u64,
    pub(super) queue: InstallQueue,
    pub(super) install_task: Option<Task<()>>,
    /// One non-install operation at a time (uninstall, disable, roll back, …).
    pub(super) op_task: Option<Task<()>>,
    pub(super) op_status: Option<String>,
    pub(super) selected: Option<String>,
    pub(super) search: Option<gpui::Entity<InputState>>,
    pub(super) list_scroll: UniformListScrollHandle,
    pub(super) prompt: Option<MissingDriverPrompt>,
    /// Drivers with an update, from the last catalog (drives the banner / badge).
    pub(super) updates_available: usize,
    pub(super) updates_announced: bool,
    /// UI scenario: open the missing-driver dialog on the first render.
    pub(super) scenario_prompt_pending: bool,
    /// Width of the list at the last render; rows pick their columns from it.
    pub(super) list_width: f32,
}

impl DriverManagerUi {
    pub(super) fn new(store: Arc<InstalledDriverStore>) -> Self {
        let developer_mode = crate::drivers::read_developer_mode(&store);
        Self {
            handles: HashMap::new(),
            installed: HashSet::new(),
            installed_loaded: false,
            snapshot_applied: false,
            store,
            policy: crate::drivers::effective_trust_policy(developer_mode),
            #[cfg(test)]
            trust_override: None,
            developer_mode,
            manifest_source: None,
            catalog: Vec::new(),
            catalog_policy: None,
            trust_epoch: 0,
            records: BTreeMap::new(),
            model: CatalogModel::default(),
            storage: StorageStats::default(),
            catalog_error: None,
            tools: ToolsUi::new(),
            catalog_task: None,
            catalog_generation: 0,
            queue: InstallQueue::default(),
            install_task: None,
            op_task: None,
            op_status: None,
            selected: None,
            search: None,
            list_scroll: UniformListScrollHandle::new(),
            prompt: None,
            updates_available: 0,
            updates_announced: false,
            scenario_prompt_pending: false,
            list_width: 0.0,
        }
    }

    /// Whether a driver with `id` is loaded or installed and enabled.
    pub(super) fn has_driver(&self, id: &str) -> bool {
        self.handles.contains_key(id) || self.installed.contains(id)
    }

    /// Whether the driver can be downloaded: the catalog lists an installable build.
    pub(super) fn is_downloadable(&self, id: &str) -> bool {
        self.model
            .row(id)
            .is_some_and(|row| row.status == RowStatus::Available && row.entry.is_some())
    }

    /// Which drivers the New Connection picker may offer.
    pub(super) fn availability(&self) -> crate::connection_dialog::DriverAvailability {
        // Nothing installed and no catalog (offline): which kinds have a driver
        // is unknown, so every kind is offered and a connect names the missing
        // driver and opens the Driver Manager.
        if !self.snapshot_applied || (self.installed.is_empty() && self.catalog.is_empty()) {
            return Default::default();
        }
        let installed = self
            .installed
            .iter()
            .chain(self.handles.keys())
            .cloned()
            .collect();
        let downloadable = self
            .model
            .rows()
            .iter()
            .filter(|row| self.is_downloadable(&row.id))
            .map(|row| row.id.clone())
            .collect();
        crate::connection_dialog::DriverAvailability::known(installed, downloadable)
    }

    /// Ids of the enabled drivers in `store`; empty when it cannot be read.
    #[cfg(test)]
    pub(super) fn installed_ids(store: &InstalledDriverStore) -> HashSet<String> {
        store
            .load_all()
            .map(|records| {
                records
                    .into_iter()
                    .filter(|(_, record)| !record.disabled)
                    .map(|(id, _)| id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What manifests are verified against: the keys compiled into this build under the current
    /// policy.
    fn manifest_trust(
        &self,
    ) -> Result<engine_driver_host::ManifestTrust, engine_driver_host::ManifestTrustError> {
        #[cfg(test)]
        if let Some(trust) = &self.trust_override {
            return Ok(trust.clone());
        }
        crate::drivers::manifest_trust(self.policy)
    }

    /// What an install job created now is stamped with.
    pub(super) fn current_origin(&self) -> Origin {
        Origin {
            policy: self.policy,
            epoch: self.trust_epoch,
        }
    }

    /// Whether the lists an install would start from were read under the policy in force now.
    pub(super) fn catalog_is_current(&self) -> bool {
        self.catalog_policy == Some(self.policy)
    }

    /// Forgets what was read from a manifest that is no longer trusted: nothing stays that could
    /// be installed from it (entries are dropped, rows that are not installed go). What is
    /// installed stays listed, without Install or Update, so it can still be used, copied and
    /// removed; the next good snapshot brings the catalog back.
    pub(super) fn distrust_catalog(&mut self) {
        use engine_driver_host::{DriverCatalogState, ToolCatalogState};
        self.catalog_policy = None;
        // Jobs made from the old lists neither start nor are adopted; a running one is told to stop.
        self.trust_epoch += 1;
        self.queue.retire_all();
        if let Some(job) = self.tools.job.as_ref().filter(|job| job.state.is_active()) {
            job.cancel.store(true, Ordering::Release);
        }
        // An installed driver stays, whatever its manifest entry said (even "needs a newer app").
        let records = &self.records;
        self.catalog = std::mem::take(&mut self.catalog)
            .into_iter()
            .filter_map(|mut item| {
                let installed = records.get(&item.id)?.version.clone();
                item.manifest = None;
                item.version = installed.clone();
                item.state = DriverCatalogState::Installed { version: installed };
                Some(item)
            })
            .collect();
        self.model.rebuild(&self.catalog, &self.records, &crate::drivers::current_target());
        self.updates_available = 0;
        self.tools.items.retain_mut(|item| {
            item.entry = None;
            let Some(record) = &item.installed else {
                return false;
            };
            item.version = record.version.clone();
            item.state = ToolCatalogState::Installed {
                version: record.version.clone(),
            };
            true
        });
        if self
            .tools
            .job
            .as_ref()
            .is_some_and(|job| matches!(job.state, InstallState::Failed(_)))
        {
            self.tools.job = None;
        }
    }

    fn manifest_source(&self) -> String {
        self.manifest_source
            .clone()
            .unwrap_or_else(|| crate::drivers::manifest_source(self.policy))
    }
}

/// One open connection on a driver.
pub(super) struct DriverSessionInfo {
    pub(super) key: String,
    pub(super) name: String,
    /// Connected through an older generation than the newest loaded one.
    pub(super) stale: bool,
    /// Still connecting (counts as in use: the finishing connect would load
    /// the driver).
    pub(super) connecting: bool,
}

/// Version directory a driver generation was loaded from
/// (`…/<id>/<version>/lib….dylib`).
fn generation_version(handle: &engine_driver_host::DriverHandle) -> String {
    handle
        .path()
        .parent()
        .and_then(|directory| directory.file_name())
        .map_or_else(|| "an older version".into(), |name| name.to_string_lossy().into_owned())
}

// --- install work (runs on a blocking thread) -------------------------------------

pub(super) struct InstalledOutcome {
    record: InstalledDriverRecord,
    handle: Arc<engine_driver_host::DriverHandle>,
    gc_problem: Option<String>,
}

#[derive(Debug)]
pub(super) enum JobError {
    Cancelled,
    Failed(InstallFailure),
}

impl From<InstallFailure> for JobError {
    fn from(failure: InstallFailure) -> Self {
        Self::Failed(failure)
    }
}

/// Download → verify → stage → load `entry`. A cancel at any point (or any
/// error after the cancel flag was raised) is a [`JobError::Cancelled`] and
/// leaves no partial file behind.
fn run_install_job(
    store: &InstalledDriverStore,
    policy: TrustPolicy,
    entry: &DriverManifestEntry,
    cancel: &Arc<AtomicBool>,
    sink: &dyn ProgressSink,
) -> Result<InstalledOutcome, JobError> {
    let artifact = entry
        .artifact_for_target(&crate::drivers::current_target())
        .ok_or_else(|| InstallFailure::no_build(&entry.id))?;
    let path = crate::drivers::temporary_download_path(store, &entry.id, &artifact.url);
    let downloaded = crate::drivers::download_artifact(
        &artifact.url,
        &path,
        artifact.size_bytes,
        policy,
        cancel,
        sink,
    )
    .map_err(|error| {
        if cancel.load(Ordering::Acquire) {
            JobError::Cancelled
        } else {
            JobError::Failed(InstallFailure::from(&error))
        }
    })?;
    let result = install_downloaded(store, policy, entry, &downloaded, cancel, sink);
    let _ = std::fs::remove_file(&downloaded.path);
    result
}

fn install_downloaded(
    store: &InstalledDriverStore,
    policy: TrustPolicy,
    entry: &DriverManifestEntry,
    downloaded: &engine_driver_host::DownloadedArtifact,
    cancel: &AtomicBool,
    sink: &dyn ProgressSink,
) -> Result<InstalledOutcome, JobError> {
    if cancel.load(Ordering::Acquire) {
        return Err(JobError::Cancelled);
    }
    let installer =
        DriverInstaller::new(crate::drivers::current_target(), store).with_trust_policy(policy);
    let failed = |error: HostInstallError| JobError::Failed(InstallFailure::from(&error));
    let record = installer
        .install_downloaded(entry, downloaded, sink)
        .map_err(failed)?;
    let activated = installer.activate(&record, sink).map_err(failed)?;
    Ok(InstalledOutcome {
        record,
        handle: activated.handle,
        gc_problem: activated.collected.err().map(|error| error.to_string()),
    })
}

/// Import a local dylib: copy it into the download area (hashing as it goes),
/// verify architecture and signature *before* loading it, read its identity,
/// then run the same install as a downloaded driver.
fn run_import(
    store: &InstalledDriverStore,
    policy: TrustPolicy,
    source: &Path,
    cancel: &AtomicBool,
) -> Result<InstalledOutcome, JobError> {
    let target = crate::drivers::current_target();
    let io_failure = |error: std::io::Error| {
        JobError::Failed(InstallFailure::new("driver.io", error.to_string(), true))
    };
    let size = std::fs::metadata(source).map_err(io_failure)?.len();
    let destination = crate::drivers::temporary_download_path(store, "import", "local.dylib");
    if let Some(directory) = destination.parent() {
        std::fs::create_dir_all(directory).map_err(io_failure)?;
    }
    let mut input = std::fs::File::open(source).map_err(io_failure)?;
    let downloaded =
        engine_driver_host::stream_to_file(&mut input, &destination, size, cancel, &NoProgress)
            .map_err(|error| JobError::Failed(InstallFailure::from(&error)))?;
    let result = (|| {
        let signers = match policy {
            TrustPolicy::Developer => engine_driver_host::SignerConfig::default(),
            TrustPolicy::Strict => crate::drivers::pinned_signers(),
        };
        let verifier = DriverVerifier::new(target);
        let verification = |error: engine_driver_host::DriverVerificationError| {
            JobError::Failed(InstallFailure::from(&HostInstallError::from(error)))
        };
        verifier
            .verify_architecture(&downloaded.path)
            .map_err(verification)?;
        verifier
            .verify_native_signature(&downloaded.path, &signers, policy)
            .map_err(verification)?;
        // Only now is it safe to load (and run its initializers).
        let capabilities = DriverLoader
            .load(&downloaded.path)
            .map_err(|error| {
                JobError::Failed(InstallFailure::from(&HostInstallError::from(error)))
            })?
            .capabilities()
            .clone();
        let entry = DriverManifestEntry {
            id: capabilities.driver.clone(),
            display_name: capabilities.display_name.clone(),
            version: format!("local-{}", &downloaded.sha256[..8]),
            app_abi_version: capabilities.abi_version,
            uri_schemes: capabilities.uri_schemes.clone(),
            features: capabilities.features().to_vec(),
            team_id: signers.macos_team_id.clone(),
            artifacts: vec![engine_driver_host::DriverArtifact {
                target: Some(target.id()),
                url: format!("file://{}", source.display()),
                sha256: downloaded.sha256.clone(),
                size_bytes: downloaded.size_bytes,
                ..Default::default()
            }],
            ..Default::default()
        };
        install_downloaded(store, policy, &entry, &downloaded, cancel, &NoProgress)
    })();
    let _ = std::fs::remove_file(&downloaded.path);
    result
}

/// Remove every version of `id`, waiting up to `OWNER_DRAIN_TIMEOUT` for
/// sessions that were just disconnected.
fn run_uninstall(
    store: &InstalledDriverStore,
    id: &str,
    handle: Option<&engine_driver_host::DriverHandle>,
) -> Result<Result<(), DriverOwners>, HostInstallError> {
    let installer = DriverInstaller::new(crate::drivers::current_target(), store);
    let deadline = Instant::now() + OWNER_DRAIN_TIMEOUT;
    loop {
        match installer.try_uninstall(id, handle)? {
            UninstallOutcome::NotInstalled | UninstallOutcome::Removed(_) => return Ok(Ok(())),
            UninstallOutcome::InUse(owners) if Instant::now() >= deadline => {
                return Ok(Err(owners))
            }
            UninstallOutcome::InUse(_) => std::thread::sleep(Duration::from_millis(40)),
        }
    }
}

fn in_use_message(id: &str, owners: &DriverOwners) -> String {
    format!(
        "Driver '{id}' is still in use ({} sessions, {} open results or operations); close them and try again",
        owners.sessions,
        owners.total() - owners.sessions
    )
}

/// What a finished operation on an installed driver did.
enum OpResult {
    Installed(Box<InstalledOutcome>),
    Removed,
    ToolRemoved,
    /// The uninstall of tool `.0` ran and failed; the typed failure is shown on the tool's row.
    ToolUninstallFailed(String, InstallFailure),
    Toggled,
    Cleared,
}

impl AppRoot {
    /// Runs `f` on the window once the current update is over.
    pub(in crate::app) fn with_driver_window(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Window, &mut gpui::App) + 'static,
    ) {
        let Some(window) = self.driver_runtime.window else {
            return;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| f(window, cx));
        });
    }

    pub(super) fn driver_availability(&self) -> crate::connection_dialog::DriverAvailability {
        self.driver_ui.availability()
    }

    fn driver_info(&mut self, message: impl AsRef<str>) {
        let report = crate::error_log::Report::new(
            crate::error_log::Severity::Info,
            Area::Driver,
            message.as_ref(),
        );
        self.report_event(None, report);
    }

    /// The Driver Manager tab is the active tab, or its dialog is open:
    /// progress repaints are only worth it then.
    fn driver_manager_visible(&self) -> bool {
        self.driver_ui.prompt.is_some()
            || self
                .workspace
                .active_tab()
                .is_some_and(|tab| tab.kind == TabKind::DriverManager)
    }

    // --- catalog -----------------------------------------------------------------

    pub(super) fn open_driver_manager(&mut self, cx: &mut Context<Self>) {
        self.workspace.open_driver_manager_tab();
        self.refresh_driver_catalog(cx);
        cx.notify();
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        let ui = &mut self.driver_ui;
        ui.model.rebuild(
            &snapshot.catalog,
            &snapshot.records,
            &crate::drivers::current_target(),
        );
        ui.updates_available = ui.model.updatable().count();
        ui.catalog = snapshot.catalog;
        ui.records = snapshot.records;
        ui.installed = snapshot.installed;
        ui.storage = snapshot.storage;
        ui.tools.items = snapshot.tools;
        ui.catalog_policy = Some(snapshot.policy);
        // A failed install of a driver the catalog no longer offers has nothing to retry.
        for id in ui.queue.failed_ids() {
            let offered = ui.model.row(&id).is_some_and(|row| {
                row.entry.is_some() && !matches!(row.status, RowStatus::Unavailable(_))
            });
            if !offered {
                ui.queue.dismiss(&id);
            }
        }
        // A failed install of a build the catalog no longer offers has nothing to retry.
        if let Some(job) = ui.tools.job.as_ref().filter(|job| matches!(job.state, InstallState::Failed(_))) {
            if ui.tools.item_of(&job.tool_id).and_then(tools::installable_entry).is_none() {
                ui.tools.job = None;
            }
        }
        ui.installed_loaded = true;
        ui.snapshot_applied = true;
        ui.catalog_error = None;
        if ui.selected.as_ref().is_some_and(|id| ui.model.row(id).is_none()) {
            ui.selected = None;
        }
    }

    /// Fetches the manifest over the network (the Check for updates button,
    /// opening the tab) and merges it with the store.
    pub(super) fn refresh_driver_catalog(&mut self, cx: &mut Context<Self>) {
        self.spawn_snapshot(false, cx);
    }

    /// Re-reads the store and the cached manifest (after an install,
    /// uninstall, enable / disable): no network.
    pub(super) fn reload_driver_catalog(&mut self, cx: &mut Context<Self>) {
        self.spawn_snapshot(true, cx);
    }

    fn spawn_snapshot(&mut self, offline: bool, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        self.driver_ui.catalog_generation = self.driver_ui.catalog_generation.wrapping_add(1);
        let generation = self.driver_ui.catalog_generation;
        let (store, tools, source, trust) = (
            self.driver_ui.store.clone(),
            self.driver_ui.tools.store.clone(),
            self.driver_ui.manifest_source(),
            self.driver_ui.manifest_trust(),
        );
        let manifest_url = source.clone();
        let task = cx
            .background_executor()
            .spawn(async move {
                let snapshot = load_snapshot(store.clone(), source, tools, trust, offline).await;
                if snapshot.as_ref().is_ok_and(|snapshot| snapshot.fetch_error.is_none()) && !offline {
                    let _ = crate::drivers::write_last_update_check(&store, SystemTime::now());
                }
                snapshot
            });
        self.driver_ui.catalog_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if !crate::drivers::catalog_generation_is_current(
                    this.driver_ui.catalog_generation,
                    generation,
                ) {
                    return;
                }
                this.driver_ui.catalog_task = None;
                match result {
                    Ok(snapshot) => {
                        let fetch_error = snapshot.fetch_error.clone();
                        this.apply_snapshot(snapshot);
                        if let Some(error) = fetch_error.filter(|_| !offline) {
                            let message = format!("Could not reach {manifest_url}: {error}");
                            this.report_warning(Area::Driver, None, &message);
                            this.driver_ui.catalog_error = Some(message);
                        }
                    }
                    Err(error) => {
                        this.driver_ui.installed_loaded = true;
                        this.report_error(
                            Area::Driver,
                            None,
                            format!("Driver catalog ({manifest_url}): {error}"),
                        );
                        if error.untrusted {
                            this.driver_ui.distrust_catalog();
                            this.drop_addon_wait_for_missing_driver(cx);
                        }
                        this.driver_ui.catalog_error = Some(error.message);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Startup: migrate flat installs, read the store and the cached manifest
    /// (all off the UI thread), then check for updates at most once a day.
    pub(super) fn start_driver_startup(&mut self, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            self.driver_ui.installed_loaded = true;
            return;
        }
        let (store, tools, source) = (
            self.driver_ui.store.clone(),
            self.driver_ui.tools.store.clone(),
            self.driver_ui.manifest_source(),
        );
        let (first_trust, check_trust) = (self.driver_ui.manifest_trust(), self.driver_ui.manifest_trust());
        let first = {
            let (store, tools, source) = (store.clone(), tools.clone(), source.clone());
            cx.background_executor().spawn(async move {
                let migrating = store.clone();
                let migration = smol::unblock(move || migrating.migrate_flat_installs()).await;
                let due = crate::drivers::update_check_due(
                    crate::drivers::read_last_update_check(&store),
                    SystemTime::now(),
                );
                (migration, load_snapshot(store, source, tools, first_trust, true).await, due)
            })
        };
        self.driver_ui.catalog_task = Some(cx.spawn(async move |this, cx| {
            let (migration, cached, due) = first.await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = migration {
                    this.report_error(Area::Driver, None, format!("Driver store migration: {error}"));
                }
                match cached {
                    Ok(snapshot) => this.apply_snapshot(snapshot),
                    Err(error) => {
                        this.driver_ui.installed_loaded = true;
                        this.report_error(Area::Driver, None, format!("Driver store: {error}"));
                        if error.untrusted {
                            this.driver_ui.distrust_catalog();
                            this.drop_addon_wait_for_missing_driver(cx);
                        }
                    }
                }
                if !due {
                    this.driver_ui.catalog_task = None;
                }
                cx.notify();
            });
            if !due {
                return;
            }
            let check = cx.background_executor().spawn({
                let store = store.clone();
                async move {
                    let snapshot = load_snapshot(store.clone(), source, tools, check_trust, false).await;
                    if snapshot.as_ref().is_ok_and(|snapshot| snapshot.fetch_error.is_none()) {
                        let _ = crate::drivers::write_last_update_check(&store, SystemTime::now());
                    }
                    snapshot
                }
            });
            let result = check.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(snapshot) => {
                        let fetch_error = snapshot.fetch_error.clone();
                        this.apply_snapshot(snapshot);
                        match fetch_error {
                            // Silent for the user; the Error Log keeps an Info entry.
                            Some(error) => {
                                this.driver_info(format!("Driver update check skipped: {error}"))
                            }
                            None => this.announce_driver_updates(cx),
                        }
                    }
                    // Silent for the user; the Error Log keeps an Info entry.
                    Err(error) => this.driver_info(format!("Driver update check skipped: {error}")),
                }
                this.driver_ui.catalog_task = None;
                cx.notify();
            });
        }));
    }

    /// One info toast per run when the catalog has updates.
    fn announce_driver_updates(&mut self, cx: &mut Context<Self>) {
        let count = self.driver_ui.updates_available;
        if count == 0 || self.driver_ui.updates_announced {
            return;
        }
        self.driver_ui.updates_announced = true;
        let message = if count == 1 {
            "1 driver update is available (Driver Manager)".to_string()
        } else {
            format!("{count} driver updates are available (Driver Manager)")
        };
        self.with_driver_window(cx, move |window, cx| window.push_notification(message, cx));
    }

    // --- installs ------------------------------------------------------------------

    /// Queues an install of `entry` and starts the queue.
    pub(super) fn enqueue_driver_install(
        &mut self,
        entry: DriverManifestEntry,
        kind: JobKind,
        cx: &mut Context<Self>,
    ) -> Enqueued {
        if self.scenario_ui.service.is_some() {
            return Enqueued::AlreadyActive;
        }
        if !self.driver_ui.catalog_is_current() {
            return Enqueued::Stale;
        }
        let origin = self.driver_ui.current_origin();
        self.driver_ui.queue.set_origin(origin);
        let enqueued = self.driver_ui.queue.enqueue(kind, entry);
        self.pump_install_queue(cx);
        cx.notify();
        enqueued
    }

    pub(super) fn update_all_drivers(&mut self, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() || !self.driver_ui.catalog_is_current() {
            return;
        }
        let entries: Vec<DriverManifestEntry> = self
            .driver_ui
            .model
            .updatable()
            .filter_map(|row| row.entry.clone())
            .collect();
        let origin = self.driver_ui.current_origin();
        self.driver_ui.queue.set_origin(origin);
        self.driver_ui.queue.enqueue_batch(entries);
        self.pump_install_queue(cx);
        cx.notify();
    }

    pub(super) fn cancel_driver_install(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.driver_ui.queue.cancel(id) != CancelEffect::Nothing {
            // A queued job that leaves the queue never ends through `on_install_finished`.
            self.drop_addon_wait_for_missing_driver(cx);
            cx.notify();
        }
    }

    pub(super) fn cancel_all_driver_installs(&mut self, cx: &mut Context<Self>) {
        self.driver_ui.queue.cancel_all();
        self.drop_addon_wait_for_missing_driver(cx);
        cx.notify();
    }

    /// Starts the oldest queued install when none is running.
    fn pump_install_queue(&mut self, cx: &mut Context<Self>) {
        if self.driver_ui.install_task.is_some() {
            return;
        }
        // Nothing starts from a catalog that is not the one read under the policy in force now.
        if !self.driver_ui.catalog_is_current() {
            self.driver_ui.queue.retire_all();
            self.drop_addon_wait_for_missing_driver(cx);
            return;
        }
        let origin = self.driver_ui.current_origin();
        let Some(job) = self.driver_ui.queue.start_next_from(origin) else {
            return;
        };
        let (tx, rx) = smol::channel::unbounded::<InstallProgress>();
        let (store, policy) = (self.driver_ui.store.clone(), self.driver_ui.policy);
        let (entry, cancel, id) = (job.entry, job.cancel, job.id);
        let worker = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                // Throttled by the host (>= 64 KiB or 50 ms): a bounded trickle.
                let sink = move |progress: InstallProgress| {
                    let _ = tx.try_send(progress);
                };
                run_install_job(&store, policy, &entry, &cancel, &sink)
            })
            .await
        });
        self.driver_ui.install_task = Some(cx.spawn(async move |this, cx| {
            // The sender lives in the worker's sink: the loop ends with the job.
            while let Ok(mut latest) = rx.recv().await {
                while let Ok(next) = rx.try_recv() {
                    latest = next;
                }
                if this
                    .update(cx, |this, cx| this.on_install_progress(&id, latest, cx))
                    .is_err()
                {
                    return;
                }
            }
            let result = worker.await;
            let _ = this.update(cx, |this, cx| this.on_install_finished(&id, result, cx));
        }));
    }

    fn on_install_progress(&mut self, id: &str, progress: InstallProgress, cx: &mut Context<Self>) {
        self.driver_ui.queue.progress(id, progress);
        if self.driver_manager_visible() {
            cx.notify();
        }
    }

    fn on_install_finished(
        &mut self,
        id: &str,
        result: Result<InstalledOutcome, JobError>,
        cx: &mut Context<Self>,
    ) {
        self.driver_ui.install_task = None;
        let name = self
            .driver_ui
            .model
            .row(id)
            .map_or_else(|| id.to_string(), |row| row.name.clone());
        // A job made before the catalog stopped being trusted: its result is not adopted (the
        // driver is not routed to, nothing is loaded for connections) and a failure is not kept
        // for Retry.
        let from_trusted = self
            .driver_ui
            .queue
            .job_is_from(id, self.driver_ui.current_origin());
        let result = match result {
            Ok(installed) if !from_trusted => {
                self.report_warning(
                    Area::Driver,
                    None,
                    format!("{name} finished installing from a catalog that is no longer trusted; it is not used until you check it in the Driver Manager"),
                );
                drop(installed);
                // It is on disk now: show it as installed rather than pretend it is not.
                self.reload_driver_catalog(cx);
                Err(JobError::Cancelled)
            }
            Err(JobError::Failed(failure)) if !from_trusted => {
                self.report_warning(Area::Driver, None, format!("{name}: {}", failure.message));
                Err(JobError::Cancelled)
            }
            other => other,
        };
        let outcome = match result {
            Ok(installed) => {
                self.adopt_installed(id, installed);
                self.driver_ui.queue.finish(id, JobOutcome::Done);
                self.reload_driver_catalog(cx);
                PromptInstall::Installed
            }
            Err(JobError::Cancelled) => {
                self.driver_ui.queue.finish(id, JobOutcome::Cancelled);
                PromptInstall::Cancelled
            }
            Err(JobError::Failed(failure)) => {
                let mut report = crate::error_log::Report::new(
                    crate::error_log::Severity::Error,
                    Area::Driver,
                    format!("{name}: {}", failure.message),
                );
                report.code = Some(failure.code.to_string());
                report.retryable = failure.retryable;
                self.report_event(None, report);
                let message = failure.message.clone();
                self.driver_ui.queue.finish(id, JobOutcome::Failed(failure));
                PromptInstall::Failed(message)
            }
        };
        self.on_driver_install_for_addon(id, &outcome, cx);
        self.on_prompt_install_finished(id, outcome, cx);
        self.pump_install_queue(cx);
        cx.notify();
    }

    /// Tests: the result of the running install of `id` arrives as a success (a result that the
    /// cancel flag could not stop).
    #[cfg(test)]
    pub(super) fn finish_driver_install_for_test(
        &mut self,
        id: &str,
        record: InstalledDriverRecord,
        handle: Arc<engine_driver_host::DriverHandle>,
        cx: &mut Context<Self>,
    ) {
        let installed = InstalledOutcome {
            record,
            handle,
            gc_problem: None,
        };
        self.on_install_finished(id, Ok(installed), cx);
    }

    /// A freshly activated generation becomes the one new connections use.
    fn adopt_installed(&mut self, id: &str, installed: InstalledOutcome) {
        if let Some(problem) = installed.gc_problem {
            self.report_warning(Area::Driver, None, format!("Old versions of {id} were not removed: {problem}"));
        }
        let ui = &mut self.driver_ui;
        ui.installed.insert(id.to_string());
        ui.handles.insert(id.to_string(), installed.handle);
        ui.records.insert(id.to_string(), installed.record);
    }

    // --- sessions ------------------------------------------------------------------

    /// Open (or connecting) connections on driver `id`.
    pub(super) fn driver_sessions(&self, id: &str) -> Vec<DriverSessionInfo> {
        let current = self.driver_ui.handles.get(id);
        self.saved_connections
            .iter()
            .filter(|saved| saved.kind.driver_id() == id)
            .filter_map(|saved| {
                let key = saved.id.to_string();
                let live = self.driver_runtime.sessions.contains_key(&key);
                let connecting = matches!(self.sessions.status(&key), ConnectionStatus::Connecting);
                if !live && !connecting {
                    return None;
                }
                let stale = live
                    && match (self.driver_runtime.session_handles.get(&key), current) {
                        (Some(session), Some(newest)) => !Arc::ptr_eq(session, newest),
                        _ => false,
                    };
                Some(DriverSessionInfo {
                    key,
                    name: saved.name.clone(),
                    stale,
                    connecting: connecting && !live,
                })
            })
            .collect()
    }

    /// Version label of the older generation(s) `id`'s stale sessions run on.
    pub(super) fn stale_session_version(&self, id: &str) -> Option<String> {
        self.driver_sessions(id)
            .into_iter()
            .filter(|session| session.stale)
            .find_map(|session| {
                self.driver_runtime
                    .session_handles
                    .get(&session.key)
                    .map(|handle| generation_version(handle))
            })
    }

    /// Disconnect, then connect again through the newest generation.
    pub(super) fn reconnect_stale_sessions(&mut self, id: &str, cx: &mut Context<Self>) {
        let stale: Vec<String> = self
            .driver_sessions(id)
            .into_iter()
            .filter(|session| session.stale)
            .map(|session| session.key)
            .collect();
        // Reconnecting closes the tabs' own sessions too: an open transaction is asked about first.
        let again = id.to_owned();
        if self.guard_connections_deferred(&stale, "Reconnecting ends their sessions.", cx, move |this, _, cx| {
            this.reconnect_stale_sessions(&again, cx)
        }) {
            return;
        }
        for key in stale {
            self.disconnect_connection(&key, cx);
            self.ensure_connected(key, None, cx);
        }
        cx.notify();
    }

    fn disconnect_driver_sessions(&mut self, id: &str, cx: &mut Context<Self>) {
        for session in self.driver_sessions(id) {
            self.disconnect_connection(&session.key, cx);
        }
    }

    // --- operations on installed drivers ---------------------------------------------

    /// Starts a blocking `work` on the one operation slot.
    fn run_driver_operation(
        &mut self,
        status: String,
        work: impl FnOnce() -> Result<OpResult, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.scenario_ui.service.is_some() || self.driver_ui.op_task.is_some() {
            return false;
        }
        self.driver_ui.op_status = Some(status);
        let task = cx
            .background_executor()
            .spawn(async move { smol::unblock(work).await });
        self.driver_ui.op_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.driver_ui.op_task = None;
                this.driver_ui.op_status = None;
                match result {
                    Ok(outcome) => this.apply_operation(outcome, cx),
                    Err(error) => {
                        this.report_error(Area::Driver, None, &error);
                        this.driver_ui.catalog_error = Some(error);
                    }
                }
                cx.notify();
            });
        }));
        true
    }

    fn apply_operation(&mut self, outcome: OpResult, cx: &mut Context<Self>) {
        match outcome {
            OpResult::Installed(installed) => {
                let id = installed.record.id.clone();
                self.adopt_installed(&id, *installed);
                self.driver_ui.catalog_error = None;
            }
            OpResult::Removed | OpResult::ToolRemoved | OpResult::Toggled | OpResult::Cleared => {
                self.driver_ui.catalog_error = None;
                // Settings › Editor shows whether SQL Formatter (sqruff) is there.
                self.rescan_format_tools_if_seen(cx);
            }
            OpResult::ToolUninstallFailed(tool, failure) => {
                let mut report = crate::error_log::Report::new(
                    crate::error_log::Severity::Error,
                    Area::Driver,
                    format!("{tool}: {}", failure.message),
                );
                report.code = Some(failure.code.to_string());
                report.retryable = failure.retryable;
                self.report_event(None, report);
                self.driver_ui.tools.failure = Some((tool, failure));
            }
        }
        self.reload_driver_catalog(cx);
    }

    /// Uninstall (`remove == true`) or disable `id`. With open connections, or add-ons that
    /// need the driver, a dialog lists them first: connections are disconnected, add-ons can be
    /// removed with the driver.
    pub(super) fn request_driver_removal(
        &mut self,
        id: String,
        remove: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let sessions = self.driver_sessions(&id);
        let addons = self.addons_broken_by(&id, None);
        if sessions.is_empty() && addons.is_empty() {
            self.run_driver_removal(id, remove, Vec::new(), cx);
            return;
        }
        let name = self
            .driver_ui
            .model
            .row(&id)
            .map_or_else(|| id.clone(), |row| row.name.clone());
        let verb = if remove { "uninstall" } else { "disable" };
        let mut lines = Vec::new();
        if !sessions.is_empty() {
            lines.push(format!(
                "These connections use the {name} driver and will be disconnected before it is {}:",
                if remove { "removed" } else { "disabled" }
            ));
            lines.extend(sessions.into_iter().map(|session| {
                if session.connecting {
                    format!("\u{2022} {} (connecting)", session.name)
                } else {
                    format!("\u{2022} {}", session.name)
                }
            }));
        }
        let also = (!addons.is_empty()).then(|| {
            let names: Vec<String> = addons.iter().map(|(_, name)| name.clone()).collect();
            lines.push(format!(
                "{} needs the {name} driver and will stop working without it.",
                names.join(", ")
            ));
            format!("{} and remove {}", capitalized(verb), names.join(", "))
        });
        let has_sessions = !self.driver_sessions(&id).is_empty();
        self.open_driver_step_dialog(
            DriverStep::Remove { id, remove, disconnect: has_sessions },
            format!("{} {name}?", if remove { "Uninstall" } else { "Disable" }),
            lines,
            if has_sessions {
                format!("Disconnect and {verb}")
            } else {
                capitalized(verb)
            },
            also.map(|label| (label, addons.into_iter().map(|(id, _)| id).collect())),
            window,
            cx,
        );
    }

    /// The installed add-ons that stop working when driver `id` goes away (`resulting` is
    /// `None`) or becomes version `resulting`: `(tool id, display name)`.
    fn addons_broken_by(&self, id: &str, resulting: Option<&str>) -> Vec<(String, String)> {
        self.driver_ui
            .tools
            .items
            .iter()
            .filter(|item| item.installed.is_some())
            .filter(|item| {
                item.requires().iter().any(|requirement| {
                    requirement.driver == id
                        && requirement.check(resulting) != engine_driver_host::RequirementStatus::Met
                })
            })
            .map(|item| (item.id.clone(), item.display_name.clone()))
            .collect()
    }

    /// The confirmation of an operation on a driver that other things depend on: `confirm` does
    /// the step as asked; `also` (a label and the add-ons to remove with it) does it and removes
    /// those add-ons first.
    #[allow(clippy::too_many_arguments)]
    fn open_driver_step_dialog(
        &mut self,
        step: DriverStep,
        title: String,
        lines: Vec<String>,
        confirm: String,
        also: Option<(String, Vec<String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (root_ok, step_ok) = (root.clone(), step.clone());
            let also_button = also.clone().map(|(label, addons)| {
                let (root, step) = (root.clone(), step.clone());
                div().debug_selector(|| "driver-confirm-also".into()).child(
                    Button::new("driver-confirm-also").label(label).danger().on_click(
                        move |_, window, cx| {
                            let (step, addons) = (step.clone(), addons.clone());
                            let _ = root.update(cx, |this, cx| this.perform_driver_step(step, addons, cx));
                            window.close_dialog(cx);
                        },
                    ),
                )
            });
            dialog
                .w(px(500.0))
                .title(title.clone())
                .child(
                    div()
                        .debug_selector(|| "driver-confirm-body".into())
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .text_size(px(13.0))
                        .children(lines.iter().enumerate().map(|(index, line)| {
                            div()
                                .when(line.starts_with('\u{2022}'), |this| {
                                    this.text_color(theme::text_muted())
                                })
                                .when(index > 0 && !line.starts_with('\u{2022}'), |this| this.mt(px(4.0)))
                                .child(line.clone())
                        })),
                )
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div().debug_selector(|| "driver-confirm-cancel".into()).child(
                                gpui_component::dialog::DialogClose::new()
                                    .trigger(|button| button.label("Cancel")),
                            ),
                        )
                        .children(also_button)
                        .child(
                            div().debug_selector(|| "driver-confirm-ok".into()).child(
                                gpui_component::dialog::DialogAction::new().child(
                                    Button::new("driver-confirm-ok").label(confirm.clone()).danger(),
                                ),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let step = step_ok.clone();
                    let _ = root_ok.update(cx, |this, cx| this.perform_driver_step(step, Vec::new(), cx));
                    true
                })
        });
        cx.notify();
    }

    /// Does `step`, first removing the add-ons `addons`.
    pub(in crate::app) fn perform_driver_step(&mut self, step: DriverStep, addons: Vec<String>, cx: &mut Context<Self>) {
        match step {
            DriverStep::Remove { id, remove, disconnect } => {
                if disconnect {
                    // Removing the driver closes every session on it, the tabs' own included: those of
                    // every saved connection of the driver, also one whose shared session is gone.
                    let keys: Vec<String> = self
                        .saved_connections
                        .iter()
                        .filter(|saved| saved.kind.driver_id() == id)
                        .map(|saved| saved.id.to_string())
                        .collect();
                    let (again, addons_again) = (DriverStep::Remove { id: id.clone(), remove, disconnect }, addons.clone());
                    if self.guard_connections_deferred(&keys, "Removing the driver ends their sessions.", cx, move |this, _, cx| {
                        this.perform_driver_step(again, addons_again, cx)
                    }) {
                        return;
                    }
                    for key in &keys {
                        self.connection_transactions_closed(key);
                    }
                    self.disconnect_driver_sessions(&id, cx);
                }
                self.run_driver_removal(id, remove, addons, cx);
            }
            DriverStep::Rollback { id } => self.run_driver_rollback(id, addons, cx),
        }
    }

    /// Drops the result sets tabs of driver `id` still show: each holds Arrow
    /// buffers owned by the driver, which cannot be unloaded while they live.
    /// (A disconnected tab keeps its last result on screen otherwise.)
    fn release_driver_results(&mut self, id: &str) {
        let connections: HashSet<String> = self
            .saved_connections
            .iter()
            .filter(|saved| saved.kind.driver_id() == id)
            .map(|saved| saved.id.to_string())
            .collect();
        let tabs: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| connections.contains(&tab.connection_id))
            .map(|tab| tab.id)
            .collect();
        for tab in tabs {
            self.release_driver_page(tab);
            if let Some(runtime) = self.workspace.runtime_mut(tab) {
                runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(
                    Vec::new(),
                )));
            }
            // Caches that keep a clone of the shown page alive.
            let grid_ui = &mut self.view_mut(tab).grid_ui;
            grid_ui.column_types = None;
            grid_ui.find.forget_scanned_grid();
            self.reset_tab_view(tab);
        }
    }

    fn run_driver_removal(&mut self, id: String, remove: bool, addons: Vec<String>, cx: &mut Context<Self>) {
        let store = self.driver_ui.store.clone();
        if remove {
            self.release_driver_results(&id);
            let handle = self.driver_ui.handles.get(&id).cloned();
            let status = format!("Uninstalling {id}...");
            let removed = id.clone();
            let tools = self.driver_ui.tools.store.clone();
            let started = self.run_driver_operation(
                status,
                move || {
                    remove_addons(tools.as_deref(), &addons).map_err(|error| error.to_string())?;
                    match run_uninstall(&store, &removed, handle.as_deref()) {
                        Ok(Ok(())) => Ok(OpResult::Removed),
                        Ok(Err(owners)) => Err(in_use_message(&removed, &owners)),
                        Err(error) => Err(error.to_string()),
                    }
                },
                cx,
            );
            // The route flips now: nothing may keep serving a removed driver.
            if started {
                self.driver_ui.handles.remove(&id);
                self.driver_ui.installed.remove(&id);
            }
        } else {
            self.set_driver_disabled_with(id, true, addons, cx);
        }
    }

    pub(super) fn set_driver_disabled(&mut self, id: String, disabled: bool, cx: &mut Context<Self>) {
        self.set_driver_disabled_with(id, disabled, Vec::new(), cx);
    }

    fn set_driver_disabled_with(
        &mut self,
        id: String,
        disabled: bool,
        addons: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let store = self.driver_ui.store.clone();
        let tools = self.driver_ui.tools.store.clone();
        let status = format!("{} {id}...", if disabled { "Disabling" } else { "Enabling" });
        let toggled = id.clone();
        let started = self.run_driver_operation(
            status,
            move || {
                remove_addons(tools.as_deref(), &addons).map_err(|error| error.to_string())?;
                store
                    .set_disabled(&toggled, disabled)
                    .map(|_| OpResult::Toggled)
                    .map_err(|error| error.to_string())
            },
            cx,
        );
        if started {
            if disabled {
                // The route flips now: neither the installed set nor a loaded
                // handle may keep serving a disabled driver.
                self.driver_ui.installed.remove(&id);
                self.driver_ui.handles.remove(&id);
            } else {
                self.driver_ui.installed.insert(id);
            }
        }
    }

    /// Rolling `id` back. When the previous version is older than an installed add-on needs, a
    /// dialog says the add-on will stop working and offers to remove it too.
    pub(super) fn request_driver_rollback(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self
            .driver_ui
            .records
            .get(&id)
            .and_then(|record| record.previous_version.clone());
        let addons = self.addons_broken_by(&id, previous.as_deref());
        if addons.is_empty() {
            self.run_driver_rollback(id, Vec::new(), cx);
            return;
        }
        let name = self
            .driver_ui
            .model
            .row(&id)
            .map_or_else(|| id.clone(), |row| row.name.clone());
        let names: Vec<String> = addons.iter().map(|(_, name)| name.clone()).collect();
        let lines = vec![format!(
            "{} needs a newer {name} driver than {} and will stop working after the rollback.",
            names.join(", "),
            previous.as_deref().unwrap_or("the previous version")
        )];
        self.open_driver_step_dialog(
            DriverStep::Rollback { id },
            format!("Roll back {name}?"),
            lines,
            "Roll back".into(),
            Some((
                format!("Roll back and remove {}", names.join(", ")),
                addons.into_iter().map(|(id, _)| id).collect(),
            )),
            window,
            cx,
        );
    }

    /// Make the previous version current again and load it.
    fn run_driver_rollback(&mut self, id: String, addons: Vec<String>, cx: &mut Context<Self>) {
        let store = self.driver_ui.store.clone();
        let tools = self.driver_ui.tools.store.clone();
        let status = format!("Rolling back {id}...");
        self.run_driver_operation(
            status,
            move || {
                remove_addons(tools.as_deref(), &addons).map_err(|error| error.to_string())?;
                let installer = DriverInstaller::new(crate::drivers::current_target(), &store);
                let record = installer.rollback(&id).map_err(|error| error.to_string())?;
                let activated = installer
                    .activate(&record, &NoProgress)
                    .map_err(|error| error.to_string())?;
                Ok(OpResult::Installed(Box::new(InstalledOutcome {
                    record,
                    handle: activated.handle,
                    gc_problem: activated.collected.err().map(|error| error.to_string()),
                })))
            },
            cx,
        );
    }

    /// Empties the download area (leftovers of interrupted installs).
    pub(super) fn clear_driver_download_cache(&mut self, cx: &mut Context<Self>) {
        if self.driver_ui.queue.active_count() > 0 {
            return;
        }
        let directory = crate::drivers::downloads_dir(&self.driver_ui.store);
        self.run_driver_operation(
            "Clearing the download cache...".into(),
            move || match std::fs::remove_dir_all(&directory) {
                Ok(()) => Ok(OpResult::Cleared),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(OpResult::Cleared),
                Err(error) => Err(format!("Could not clear the download cache: {error}")),
            },
            cx,
        );
    }

    /// The ⋯ menu's Developer mode item: turning it off applies at once;
    /// turning it on first asks, because it lowers the trust checks.
    pub(super) fn request_developer_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.driver_ui.developer_mode {
            self.set_developer_mode(false, cx);
            return;
        }
        let root = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let root_ok = root.clone();
            dialog
                .w(px(480.0))
                .title("Turn on Developer mode?")
                .child(
                    div()
                        .debug_selector(|| "driver-developer-body".into())
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .text_size(px(13.0))
                        .child(
                            "Developer mode turns off the checks that keep untrusted code out of TableX:",
                        )
                        .child(div().text_color(theme::text_muted()).child(
                            "\u{2022} driver manifests from local files are accepted without a signature",
                        ))
                        .child(div().text_color(theme::text_muted()).child(
                            "\u{2022} drivers from local files and ad-hoc signed drivers can be installed",
                        ))
                        .child(
                            "A driver runs inside TableX with full access to your files and connections. Only turn this on if you build drivers yourself.",
                        ),
                )
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div().debug_selector(|| "driver-developer-cancel".into()).child(
                                gpui_component::dialog::DialogClose::new()
                                    .trigger(|button| button.label("Cancel")),
                            ),
                        )
                        .child(
                            div().debug_selector(|| "driver-developer-ok".into()).child(
                                gpui_component::dialog::DialogAction::new().child(
                                    Button::new("driver-developer-ok")
                                        .label("Turn on Developer mode")
                                        .danger(),
                                ),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = root_ok.update(cx, |this, cx| this.set_developer_mode(true, cx));
                    true
                })
        });
        cx.notify();
    }

    /// Saves the choice, re-resolves the trust policy and reloads the catalog
    /// under it. Nothing changes if the choice cannot be saved.
    fn set_developer_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if let Err(error) = crate::drivers::write_developer_mode(&self.driver_ui.store, enabled) {
            self.report_error(Area::Driver, None, format!("Could not save Developer mode: {error}"));
            return;
        }
        let ui = &mut self.driver_ui;
        ui.developer_mode = enabled;
        ui.policy = crate::drivers::effective_trust_policy(enabled);
        ui.catalog_error = None;
        // What was read under the other policy is not offered while the refresh is out.
        ui.distrust_catalog();
        self.drop_addon_wait_for_missing_driver(cx);
        self.driver_info(if enabled { "Developer mode turned on" } else { "Developer mode turned off" });
        self.refresh_driver_catalog(cx);
        cx.notify();
    }

    /// "Import local driver…": asks for a dylib, then verifies and installs it.
    pub(super) fn import_local_driver(&mut self, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() || self.driver_ui.op_task.is_some() {
            return;
        }
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import driver".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.import_driver_file(path, cx));
        })
        .detach();
    }

    pub(super) fn import_driver_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (store, policy) = (self.driver_ui.store.clone(), self.driver_ui.policy);
        self.run_driver_operation(
            format!("Importing {}...", path.display()),
            move || {
                run_import(&store, policy, &path, &AtomicBool::new(false))
                    .map(|installed| OpResult::Installed(Box::new(installed)))
                    .map_err(|error| match error {
                        JobError::Cancelled => "Import cancelled".to_string(),
                        JobError::Failed(failure) => {
                            format!("Import failed ({}): {}", failure.code, failure.message)
                        }
                    })
            },
            cx,
        );
    }

    pub(super) fn reveal_driver(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(record) = self.driver_ui.records.get(id) {
            cx.reveal_path(&record.path);
        }
    }

    pub(super) fn copy_driver_details(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(row) = self.driver_ui.model.row(id) else {
            return;
        };
        let mut text = format!("{} ({})\n", row.name, row.id);
        if let Some(record) = &row.record {
            text.push_str(&format!(
                "version {}\nsha256 {}\nteam {}\nabi {}\npath {}\ninstalled {}\n",
                record.version,
                record.sha256,
                if record.team_id.is_empty() { "(none)" } else { &record.team_id },
                record.app_abi_version,
                record.path.display(),
                record.installed_at,
            ));
        }
        if let Some(latest) = &row.latest_version {
            text.push_str(&format!("latest {latest}\n"));
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    // --- UI scenario fixture -----------------------------------------------------------

    /// `--ui-scenario driver-manager`: a catalog with a downloading row, a
    /// queued one, an update, a developer driver, a disabled one and an
    /// unavailable one. Nothing touches the disk or the network.
    ///
    /// `missing_driver` (`--ui-scenario missing-driver`) instead leaves one
    /// driver uninstalled behind the "driver is not installed" dialog of a
    /// connect. Capture-only `TABLEX_SEED_MISSING_DRIVER=<postgresql|sqlite>[,<phase>]`
    /// picks the driver (default PostgreSQL) and the dialog's phase:
    /// `asking` (the first thing a fresh install shows), `installing`
    /// (default, mid-download), `connecting` (installed, opening the session)
    /// or `installed` (no dialog: the driver is installed and the connection
    /// is open).
    pub(super) fn seed_driver_manager_scenario(&mut self, missing_driver: bool) {
        let seed = std::env::var("TABLEX_SEED_MISSING_DRIVER").unwrap_or_default();
        let (missing_id, phase) = seed.split_once(',').unwrap_or((seed.as_str(), ""));
        let missing_id = if missing_id == "sqlite" { "sqlite" } else { "postgresql" };
        let installed_after = phase == "installed";
        let phase = match phase {
            "asking" => prompt::PromptPhase::Asking,
            "connecting" => prompt::PromptPhase::Connecting,
            _ => prompt::PromptPhase::Installing,
        };
        let missing_driver_dialog = missing_driver && !installed_after;
        use engine_driver_host::{DriverArtifact, DriverCatalogState, DriverCategory};
        let target = crate::drivers::current_target();
        let entry = |id: &str, name: &str, version: &str, category: DriverCategory, megabytes: u64| {
            DriverManifestEntry {
                id: id.into(),
                display_name: name.into(),
                version: version.into(),
                app_abi_version: engine_driver_api::ABI_VERSION,
                uri_schemes: vec![id.into()],
                features: scenario_features(id),
                license: "MIT".into(),
                source_repo: "https://example.com/tablex".into(),
                team_id: "ABCDE12345".into(),
                category: Some(category),
                description: Some(format!("{name} driver for TableX.")),
                artifacts: vec![DriverArtifact {
                    target: Some(target.id()),
                    url: format!("https://example.com/{id}.dylib"),
                    sha256: "00".into(),
                    size_bytes: megabytes * 1024 * 1024,
                    ..Default::default()
                }],
                ..Default::default()
            }
        };
        let record = |id: &str, version: &str, team: &str| InstalledDriverRecord {
            id: id.into(),
            version: version.into(),
            path: PathBuf::from(format!("/Users/you/Library/Application Support/Drivers/v2/{id}/{version}/libdbengine_{id}.dylib")),
            sha256: "ab".repeat(32),
            team_id: team.into(),
            installed_at: "2026-10-08T15:42:00Z".into(),
            app_abi_version: engine_driver_api::ABI_VERSION,
            uri_schemes: vec![id.into()],
            license: "MIT".into(),
            source_repo: "https://example.com/tablex".into(),
            previous_version: (id == "postgresql").then(|| "1.2.0".to_string()),
            ..Default::default()
        };
        let rows = [
            ("postgresql", "PostgreSQL", "1.4.0", DriverCategory::Sql, 2, (!(missing_driver_dialog && missing_id == "postgresql")).then_some(("1.3.0", "ABCDE12345")), false),
            ("mysql", "MySQL", "1.2.0", DriverCategory::Sql, 3, Some(("1.2.0", "ABCDE12345")), false),
            ("clickhouse", "ClickHouse", "1.0.0", DriverCategory::Analytics, 2, None, false),
            ("redis", "Redis", "1.1.0", DriverCategory::KeyValue, 2, None, false),
            ("mongodb", "MongoDB", "2.1.0", DriverCategory::Nosql, 4, Some(("2.0.3", "ABCDE12345")), false),
            ("sqlite", "SQLite", "1.1.0", DriverCategory::Sql, 1, (!(missing_driver_dialog && missing_id == "sqlite")).then_some(("1.1.0", "ABCDE12345")), !(missing_driver && missing_id == "sqlite")),
            ("mariadb", "MariaDB", "1.2.0-dev", DriverCategory::Sql, 3, Some(("1.2.0-dev", "")), false),
        ];
        let mut catalog = Vec::new();
        let mut records = BTreeMap::new();
        let catalog_disabled: HashSet<&str> =
            rows.iter().filter(|row| row.6).map(|row| row.0).collect();
        for (id, name, latest, category, megabytes, installed, disabled) in rows {
            let manifest = entry(id, name, latest, category, megabytes);
            let state = match installed {
                Some((version, _)) if version == latest => DriverCatalogState::Installed { version: version.into() },
                Some((version, _)) => DriverCatalogState::UpdateAvailable { installed: version.into(), latest: latest.into() },
                None => DriverCatalogState::Available,
            };
            if let Some((version, team)) = installed {
                records.insert(id.to_string(), record(id, version, team));
            }
            catalog.push(DriverCatalogItem {
                id: id.into(),
                display_name: name.into(),
                version: latest.into(),
                state,
                manifest: Some(manifest),
                disabled,
            });
        }
        let ui = &mut self.driver_ui;
        ui.installed_loaded = true;
        ui.storage = StorageStats { installed_bytes: 15_900_000, cache_bytes: 4_200_000 };
        let missing = catalog
            .iter()
            .find(|item| item.id == missing_id)
            .and_then(|item| item.manifest.clone())
            .expect("the missing driver's entry");
        if missing_driver_dialog {
            if phase != prompt::PromptPhase::Asking {
                ui.queue.enqueue(JobKind::Install, missing.clone());
                ui.queue.start_next();
                ui.queue.progress(
                    missing_id,
                    InstallProgress {
                        step: engine_driver_host::InstallStep::VerifyingSignature,
                        bytes_done: 2 * 1024 * 1024,
                        bytes_total: 2 * 1024 * 1024,
                    },
                );
            }
        } else {
            let (redis, mongo) = (
                catalog[3].manifest.clone().expect("redis entry"),
                catalog[4].manifest.clone().expect("mongodb entry"),
            );
            // Redis is mid-download (62%), MongoDB waits behind it.
            ui.queue.enqueue(JobKind::Install, redis);
            ui.queue.enqueue(JobKind::Update, mongo);
            ui.queue.start_next();
            ui.queue.progress(
                "redis",
                InstallProgress {
                    step: engine_driver_host::InstallStep::Downloading,
                    bytes_done: 1_300_000,
                    bytes_total: 2 * 1024 * 1024,
                },
            );
        }
        ui.model.rebuild(&catalog, &records, &target);
        ui.updates_available = ui.model.updatable().count();
        ui.catalog = catalog;
        // What is installed and enabled, as a snapshot of the store would say.
        ui.installed = records
            .keys()
            .filter(|id| !catalog_disabled.contains(id.as_str()))
            .cloned()
            .collect();
        ui.records = records;
        ui.selected = Some(missing_id.into());
        let missing_kind =
            if missing_id == "sqlite" { ConnectionKind::Sqlite } else { ConnectionKind::Postgres };
        // The fixture connection of the missing driver's kind.
        let saved_of_kind = self
            .saved_connections
            .iter()
            .find(|saved| saved.kind == missing_kind)
            .or(self.saved_connections.first())
            .cloned();
        if installed_after {
            if let Some(saved) = saved_of_kind.as_ref() {
                let key = saved.id.to_string();
                if let Some(attempt) = self.sessions.begin_connect(&key) {
                    self.sessions.finish_connect(&key, attempt, Ok(()));
                }
                self.sync_connected_ids();
            }
        }
        if missing_driver_dialog {
            if let Some(mut saved) = saved_of_kind {
                let (name, label) = if missing_id == "sqlite" {
                    ("local-sqlite", "SQLite")
                } else {
                    ("dev-postgres", "PostgreSQL")
                };
                saved.name = name.into();
                saved.kind = missing_kind;
                self.driver_ui.prompt = Some(prompt::MissingDriverPrompt::new(
                    label.into(),
                    missing,
                    saved,
                    None,
                    1,
                    phase,
                ));
                self.driver_ui.scenario_prompt_pending = true;
            }
        }
        self.seed_driver_tools_fixture(false);
        self.seed_shell_addon_fixture(ShellAddonSeed::from_environment());
        self.workspace.open_driver_manager_tab();
    }
}

/// The features each real driver declares in its manifest.
fn scenario_features(id: &str) -> Vec<String> {
    let features: &[&str] = match id {
        "redis" => &[
            "navigator",
            "query.arrow_c_data",
            "query.redis_key",
            "query.redis_command",
            "mutation.set_string",
            "mutation.set_ttl",
            "mutation.delete_key",
            "mutation.hset",
            "mutation.zadd",
        ],
        "mongodb" => &[
            "navigator",
            "query.mongo_find",
            "query.mongo_script",
            "query.mongo_aggregate",
            "operation.cancel",
            "export.mongo_aggregate",
            "mutation.insert",
            "mutation.update",
            "mutation.delete",
            "admin.collection",
            "admin.indexes",
            "bson_rendering",
        ],
        "sqlite" => &[
            "navigator",
            "query.sql",
            "query.arrow_c_data",
            "operation.cancel",
            "mutation.insert",
            "mutation.update",
            "mutation.delete",
            "mutation.batch",
            "metadata.table_properties",
        ],
        _ => &[
            "navigator",
            "query.sql",
            "query.arrow_c_data",
            "operation.cancel",
            "mutation.insert",
            "mutation.update",
            "mutation.delete",
            "mutation.batch",
            "database.create",
            "metadata.table_properties",
        ],
    };
    features.iter().map(|feature| feature.to_string()).collect()
}

/// An operation on a driver that other things depend on, waiting for the user's answer.
#[derive(Clone, Debug)]
pub(in crate::app) enum DriverStep {
    Remove { id: String, remove: bool, disconnect: bool },
    Rollback { id: String },
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// An add-on that could not be removed with its driver.
#[derive(Debug)]
struct RemoveAddonError {
    tool: String,
    source: engine_driver_host::ToolError,
}

impl std::fmt::Display for RemoveAddonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Could not remove {}: {}", self.tool, self.source)
    }
}

/// Uninstalls add-ons `ids` (blocking): the first failure stops the operation they were part of.
fn remove_addons(
    store: Option<&engine_driver_host::ToolStore>,
    ids: &[String],
) -> Result<(), RemoveAddonError> {
    let Some(store) = store else {
        return Ok(());
    };
    ids.iter().try_for_each(|id| {
        engine_driver_host::ToolInstaller::new(crate::drivers::current_target(), store)
            .uninstall(id)
            .map(|_| ())
            .map_err(|source| RemoveAddonError { tool: id.clone(), source })
    })
}

/// How an install that a prompt waits on ended.
pub(super) enum PromptInstall {
    Installed,
    Cancelled,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(label: &str) -> (Arc<InstalledDriverStore>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tablex-dm-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        (Arc::new(InstalledDriverStore::new(&dir)), dir)
    }

    /// A release build without `TABLEX_MANIFEST_PUBLIC_KEYS` trusts no key.
    #[test]
    fn a_keyless_build_refuses_the_remote_catalog_with_an_actionable_message() {
        let (store, dir) = temp_store("keyless");
        for policy in [TrustPolicy::Strict, TrustPolicy::Developer] {
            let trust = engine_driver_host::ManifestTrust::new(&[], policy).expect("trust");
            let result = smol::block_on(load_snapshot_with(
                trust,
                store.clone(),
                None,
                "https://github.com/x/y/releases/latest/download/drivers-manifest.json".into(),
                false,
            ));
            let message = result.err().expect("refused, not fetched").message;
            assert!(message.starts_with("This build has no driver signing key"), "{message}");
            assert!(message.contains("TABLEX_MANIFEST_PUBLIC_KEYS") && message.contains("Developer mode"), "{message}");
        }
        // Offline (cached) reads and Developer-mode local manifests still work.
        let trust = engine_driver_host::ManifestTrust::new(&[], TrustPolicy::Strict).expect("trust");
        let cached = smol::block_on(load_snapshot_with(trust, store.clone(), None, "https://h/m.json".into(), true));
        assert!(cached.is_ok(), "offline startup read needs no key");

        let manifest = dir.join("manifest.json");
        std::fs::write(&manifest, r#"{"schemaVersion":2,"drivers":[]}"#).expect("manifest");
        let local = url::Url::from_file_path(&manifest).expect("url").to_string();
        let developer = engine_driver_host::ManifestTrust::new(&[], TrustPolicy::Developer).expect("trust");
        let loaded = smol::block_on(load_snapshot_with(developer, store.clone(), None, local.clone(), false));
        assert!(loaded.is_ok(), "{:?}", loaded.err());
        let strict = engine_driver_host::ManifestTrust::new(&[], TrustPolicy::Strict).expect("trust");
        assert!(smol::block_on(load_snapshot_with(strict, store, None, local, false)).is_err(), "strict never accepts a local manifest");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn in_use_message_names_sessions_and_other_owners() {
        let owners = DriverOwners {
            sessions: 2,
            pages: 1,
            operations: 1,
            ..Default::default()
        };
        let message = in_use_message("sqlite", &owners);
        assert!(message.contains("'sqlite'") && message.contains("2 sessions") && message.contains("2 open"), "{message}");
    }
}
