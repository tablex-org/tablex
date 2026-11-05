//! Pure Driver Manager model: install jobs and their FIFO queue, the catalog
//! rows with search / filter / counts. No GPUI, no I/O.

use engine_driver_host::{
    DownloadError, DriverCatalogItem, DriverCatalogState, DriverCategory, DriverManifestEntry,
    DriverTarget, DriverVerificationError, InstallError as HostInstallError, InstallProgress, InstallStep,
    InstalledDriverRecord, TrustPolicy,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// --- install jobs --------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct InstallFailure {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl InstallFailure {
    pub(in crate::app) fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }

    pub(in crate::app) fn no_build(id: &str) -> Self {
        Self::new("driver.no_build", format!("No driver build for {}: {id}", crate::drivers::current_target()), false)
    }
}

/// `None` for a cancel, which is not a failure.
impl From<&DownloadError> for InstallFailure {
    fn from(error: &DownloadError) -> Self {
        let (code, retryable) = match error {
            DownloadError::Cancelled => ("driver.cancelled", false),
            DownloadError::Io(_) => ("driver.download_failed", true),
            DownloadError::TooLarge { .. } => ("driver.size_mismatch", false),
            DownloadError::SizeMismatch { .. } => ("driver.size_mismatch", true),
            DownloadError::InsecureUrl(_) => ("driver.insecure_url", false),
            DownloadError::LocalUrlNotAllowed(_) => ("driver.local_url_not_allowed", false),
            DownloadError::UnsupportedScheme(_) => ("driver.unsupported_url", false),
        };
        Self::new(code, error.to_string(), retryable)
    }
}

impl From<&HostInstallError> for InstallFailure {
    fn from(error: &HostInstallError) -> Self {
        let (code, retryable) = match error {
            HostInstallError::Verification(verification) => match verification {
                DriverVerificationError::Io(_) => ("driver.io", true),
                DriverVerificationError::SizeMismatch { .. } => ("driver.size_mismatch", false),
                DriverVerificationError::ChecksumMismatch { .. } => {
                    ("driver.checksum_mismatch", false)
                }
                DriverVerificationError::ArchUnsupported(_) => ("driver.arch_unsupported", false),
                DriverVerificationError::InvalidDriverId(_) => ("driver.invalid_id", false),
                DriverVerificationError::InvalidSignature(_) => ("driver.signature_invalid", false),
                DriverVerificationError::SignatureUnsupported(_) => {
                    ("driver.signature_unsupported", false)
                }
                DriverVerificationError::TeamIdMismatch { .. } => ("driver.team_id_mismatch", false),
                DriverVerificationError::AbiSymbolMissing
                | DriverVerificationError::AbiMismatch { .. } => ("driver.abi_mismatch", false),
            },
            HostInstallError::Io(_) => ("driver.io", true),
            HostInstallError::Loader(_) => ("driver.load_failed", false),
            HostInstallError::Store(engine_driver_host::StoreError::LockTimeout { .. }) => {
                ("driver.store_busy", true)
            }
            HostInstallError::Store(_) => ("driver.store_failed", false),
        };
        Self::new(code, error.to_string(), retryable)
    }
}

/// Pipeline position of one install: `Queued` → `Downloading` → `Verifying`
/// → `Loading`, then it leaves the queue as done / cancelled, or stays `Failed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum InstallState {
    Queued,
    Downloading { bytes: u64, total: u64 },
    Verifying(InstallStep),
    Loading,
    /// Kept for Retry. A done or cancelled job leaves the queue instead
    /// ([`JobOutcome`]), so those two have no state of their own.
    Failed(InstallFailure),
}

impl InstallState {
    /// Queued or running: the row shows progress / a badge and ✕.
    pub(in crate::app) fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Downloading { .. } | Self::Verifying(_) | Self::Loading
        )
    }

    pub(in crate::app) fn is_running(&self) -> bool {
        self.is_active() && *self != Self::Queued
    }

    /// Step text of a running install.
    pub(in crate::app) fn step_label(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Downloading { .. } => "Downloading",
            Self::Verifying(InstallStep::VerifyingSignature) => "Verifying signature",
            Self::Verifying(InstallStep::VerifyingAbi) => "Checking driver ABI",
            Self::Verifying(_) => "Verifying checksum",
            Self::Loading => "Loading driver",
            Self::Failed(_) => "Failed",
        }
    }

    /// Applies a progress report; ignored unless running (a late report after
    /// the install ended must not resurrect it).
    pub(in crate::app) fn apply(&mut self, progress: InstallProgress) {
        if !self.is_running() {
            return;
        }
        *self = match progress.step {
            InstallStep::Downloading => Self::Downloading {
                bytes: progress.bytes_done,
                total: progress.bytes_total,
            },
            InstallStep::Loading => Self::Loading,
            step => Self::Verifying(step),
        };
    }

    /// Download progress as a fraction in `0.0..=1.0`; later steps are full.
    pub(in crate::app) fn fraction(&self) -> f32 {
        match self {
            Self::Queued => 0.0,
            Self::Downloading { bytes, total } if *total > 0 => {
                (*bytes as f64 / *total as f64).clamp(0.0, 1.0) as f32
            }
            Self::Downloading { .. } => 0.0,
            _ => 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum JobKind {
    Install,
    Update,
    Reinstall,
}

/// What a job was created under: the trust policy and the "trust epoch" (bumped whenever what was
/// read from a manifest stops being trusted). A job only runs, and its result is only adopted,
/// while both are still the current ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::app) struct Origin {
    pub policy: TrustPolicy,
    pub epoch: u64,
}

#[derive(Debug)]
pub(in crate::app) struct InstallJob {
    pub id: String,
    pub kind: JobKind,
    pub entry: DriverManifestEntry,
    pub state: InstallState,
    pub cancel: Arc<AtomicBool>,
    pub origin: Origin,
    in_batch: bool,
}

impl InstallJob {
    fn new(kind: JobKind, entry: DriverManifestEntry, origin: Origin) -> Self {
        Self {
            origin,
            id: entry.id.clone(),
            kind,
            entry,
            state: InstallState::Queued,
            cancel: Arc::new(AtomicBool::new(false)),
            in_batch: false,
        }
    }

    /// A cancel was requested but the worker has not stopped yet.
    pub(in crate::app) fn cancelling(&self) -> bool {
        self.state.is_running() && self.cancel.load(Ordering::Acquire)
    }

    /// Applies a progress report; ignored unless the job is running (a late
    /// report after the job ended must not resurrect it).
    pub(in crate::app) fn apply(&mut self, progress: InstallProgress) {
        self.state.apply(progress);
    }
}

/// How a running job ended.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::app) enum JobOutcome {
    Done,
    Cancelled,
    Failed(InstallFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Enqueued {
    /// Added to the queue (it runs when everything before it is done).
    Queued,
    /// The same driver already has a queued or running job: nothing added.
    AlreadyActive,
    /// The catalog was read under another trust policy than the current one: nothing is installed
    /// from it.
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum CancelEffect {
    /// A queued job left the queue.
    Dequeued,
    /// The running job was told to stop; it ends through [`InstallQueue::finish`].
    Signalled,
    Nothing,
}

/// A job the queue just started.
#[derive(Debug)]
pub(in crate::app) struct StartedJob {
    pub id: String,
    pub entry: DriverManifestEntry,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Batch {
    total: usize,
    finished: usize,
}

/// FIFO of installs: one runs at a time, so two jobs of the same driver can
/// never run together, and a second request for an active driver is refused.
/// Failed jobs stay (the row shows Retry) until retried or dismissed; done
/// and cancelled jobs leave.
#[derive(Default)]
pub(in crate::app) struct InstallQueue {
    jobs: Vec<InstallJob>,
    batch: Option<Batch>,
    /// Stamped on the jobs enqueued from now on.
    origin: Origin,
}

impl InstallQueue {
    pub(in crate::app) fn set_origin(&mut self, origin: Origin) {
        self.origin = origin;
    }

    /// Whether the job `id` was created under `origin` (false for no such job).
    pub(in crate::app) fn job_is_from(&self, id: &str, origin: Origin) -> bool {
        self.job(id).is_some_and(|job| job.origin == origin)
    }

    /// What the jobs were created from is no longer trusted: queued jobs are dropped, failed ones
    /// dismissed, the running one is told to stop (it leaves through [`Self::finish`]).
    pub(in crate::app) fn retire_all(&mut self) {
        self.jobs.retain(|job| job.state.is_running());
        for job in &self.jobs {
            job.cancel.store(true, Ordering::Release);
        }
        self.batch = None;
    }

    /// Ids of the failed jobs (their rows offer Retry).
    pub(in crate::app) fn failed_ids(&self) -> Vec<String> {
        self.jobs
            .iter()
            .filter(|job| matches!(job.state, InstallState::Failed(_)))
            .map(|job| job.id.clone())
            .collect()
    }

    /// [`Self::start_next`] for jobs created under `origin`: a queued job from another origin is
    /// dropped instead of started.
    pub(in crate::app) fn start_next_from(&mut self, origin: Origin) -> Option<StartedJob> {
        self.jobs
            .retain(|job| job.state != InstallState::Queued || job.origin == origin);
        self.start_next()
    }

    pub(in crate::app) fn enqueue(&mut self, kind: JobKind, entry: DriverManifestEntry) -> Enqueued {
        if self.job(&entry.id).is_some_and(|job| job.state.is_active()) {
            return Enqueued::AlreadyActive;
        }
        self.jobs.retain(|job| job.id != entry.id);
        self.jobs.push(InstallJob::new(kind, entry, self.origin));
        Enqueued::Queued
    }

    /// Enqueues `entries` as one "Update all" batch and returns how many were
    /// added (already active drivers are skipped).
    pub(in crate::app) fn enqueue_batch(
        &mut self,
        entries: impl IntoIterator<Item = DriverManifestEntry>,
    ) -> usize {
        let mut added = 0;
        for entry in entries {
            let id = entry.id.clone();
            if self.enqueue(JobKind::Update, entry) == Enqueued::Queued {
                if let Some(job) = self.job_mut(&id) {
                    job.in_batch = true;
                }
                added += 1;
            }
        }
        if added > 0 {
            let batch = self.batch.get_or_insert_with(Batch::default);
            batch.total += added;
        }
        added
    }

    /// Starts the oldest queued job when nothing is running.
    pub(in crate::app) fn start_next(&mut self) -> Option<StartedJob> {
        if self.running().is_some() {
            return None;
        }
        let job = self
            .jobs
            .iter_mut()
            .find(|job| job.state == InstallState::Queued)?;
        let total = job
            .entry
            .artifact_for_target(&crate::drivers::current_target())
            .map_or(0, |artifact| artifact.size_bytes);
        job.state = InstallState::Downloading { bytes: 0, total };
        Some(StartedJob {
            id: job.id.clone(),
            entry: job.entry.clone(),
            cancel: Arc::clone(&job.cancel),
        })
    }

    pub(in crate::app) fn progress(&mut self, id: &str, progress: InstallProgress) {
        if let Some(job) = self.job_mut(id) {
            job.apply(progress);
        }
    }

    /// Ends the running job `id`. Done and cancelled jobs leave the queue; a
    /// failed one stays for Retry. Returns whether `id` was running.
    pub(in crate::app) fn finish(&mut self, id: &str, outcome: JobOutcome) -> bool {
        let Some(position) = self
            .jobs
            .iter()
            .position(|job| job.id == id && job.state.is_running())
        else {
            return false;
        };
        let in_batch = self.jobs[position].in_batch;
        match outcome {
            JobOutcome::Done | JobOutcome::Cancelled => {
                self.jobs.remove(position);
            }
            JobOutcome::Failed(failure) => {
                self.jobs[position].state = InstallState::Failed(failure);
            }
        }
        if in_batch {
            self.batch_job_finished();
        }
        true
    }

    pub(in crate::app) fn cancel(&mut self, id: &str) -> CancelEffect {
        let Some(position) = self
            .jobs
            .iter()
            .position(|job| job.id == id && job.state.is_active())
        else {
            return CancelEffect::Nothing;
        };
        if self.jobs[position].state == InstallState::Queued {
            let job = self.jobs.remove(position);
            if job.in_batch {
                self.batch_job_finished();
            }
            return CancelEffect::Dequeued;
        }
        self.jobs[position].cancel.store(true, Ordering::Release);
        CancelEffect::Signalled
    }

    /// Cancels every queued job and signals the running one.
    pub(in crate::app) fn cancel_all(&mut self) {
        let ids: Vec<String> = self
            .jobs
            .iter()
            .filter(|job| job.state.is_active())
            .map(|job| job.id.clone())
            .collect();
        for id in ids {
            self.cancel(&id);
        }
    }

    /// Forgets a failed job (the row returns to its catalog state).
    pub(in crate::app) fn dismiss(&mut self, id: &str) {
        self.jobs
            .retain(|job| !(job.id == id && matches!(job.state, InstallState::Failed(_))));
    }

    pub(in crate::app) fn job(&self, id: &str) -> Option<&InstallJob> {
        self.jobs.iter().find(|job| job.id == id)
    }

    fn job_mut(&mut self, id: &str) -> Option<&mut InstallJob> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    pub(in crate::app) fn running(&self) -> Option<&InstallJob> {
        self.jobs.iter().find(|job| job.state.is_running())
    }

    /// Queued or running jobs.
    pub(in crate::app) fn active_count(&self) -> usize {
        self.jobs.iter().filter(|job| job.state.is_active()).count()
    }

    /// `(finished, total)` of the current "Update all" batch.
    pub(in crate::app) fn batch_counter(&self) -> Option<(usize, usize)> {
        self.batch.map(|batch| (batch.finished, batch.total))
    }

    fn batch_job_finished(&mut self) {
        if let Some(batch) = &mut self.batch {
            batch.finished += 1;
            if batch.finished >= batch.total {
                self.batch = None;
            }
        }
    }
}

// --- catalog rows ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum RowStatus {
    Available,
    Installed,
    UpdateAvailable,
    Unavailable(String),
}

#[derive(Clone, Debug)]
pub(in crate::app) struct DriverRow {
    pub id: String,
    pub name: String,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub size_bytes: Option<u64>,
    pub category: Option<DriverCategory>,
    /// Short capability chips for the row (`sql`, `arrow`, `cancel`, …).
    pub chips: Vec<&'static str>,
    pub features: Vec<String>,
    pub schemes: Vec<String>,
    pub status: RowStatus,
    pub disabled: bool,
    /// Installed without a pinned Team ID (a developer / ad-hoc build).
    pub developer: bool,
    pub entry: Option<DriverManifestEntry>,
    pub record: Option<InstalledDriverRecord>,
    /// Lower-cased name, id, schemes and capabilities for search.
    search: String,
}

/// Row-sized chip for a manifest feature id (several features share a chip:
/// every `mutation.*` is "edit"); minor ones are detail-only.
fn chip_for_feature(feature: &str) -> Option<&'static str> {
    Some(match feature {
        "navigator" => "navigator",
        "query.sql" => "sql",
        "query.arrow_c_data" => "arrow",
        "operation.cancel" => "cancel",
        feature if feature.starts_with("query.mongo") => "mongo",
        feature if feature.starts_with("query.redis") => "redis",
        feature if feature.starts_with("mutation.") => "edit",
        feature if feature.starts_with("export.") => "export",
        feature if feature.starts_with("admin.") => "admin",
        _ => return None,
    })
}

/// The manifest's category, else a guess from the URI schemes.
pub(in crate::app) fn category_for(
    declared: Option<DriverCategory>,
    schemes: &[String],
) -> Option<DriverCategory> {
    if let Some(category) = declared.filter(|category| *category != DriverCategory::Other) {
        return Some(category);
    }
    schemes.iter().find_map(|scheme| {
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "mysql" | "mariadb" | "sqlite" | "sqlserver" => {
                Some(DriverCategory::Sql)
            }
            "mongodb" | "mongodb+srv" => Some(DriverCategory::Nosql),
            "redis" | "rediss" => Some(DriverCategory::KeyValue),
            "clickhouse" => Some(DriverCategory::Analytics),
            _ => None,
        }
    })
}

impl DriverRow {
    pub(in crate::app) fn build(
        item: &DriverCatalogItem,
        record: Option<&InstalledDriverRecord>,
        target: &DriverTarget,
    ) -> Self {
        let entry = item.manifest.as_ref();
        let schemes = entry
            .map(|entry| entry.uri_schemes.clone())
            .or_else(|| record.map(|record| record.uri_schemes.clone()))
            .unwrap_or_default();
        let features = entry.map(|entry| entry.features.clone()).unwrap_or_default();
        let mut chips: Vec<&'static str> = Vec::new();
        for chip in features.iter().filter_map(|feature| chip_for_feature(feature)) {
            if !chips.contains(&chip) {
                chips.push(chip);
            }
        }
        let status = match &item.state {
            DriverCatalogState::Available => RowStatus::Available,
            DriverCatalogState::Installed { .. } => RowStatus::Installed,
            DriverCatalogState::UpdateAvailable { .. } => RowStatus::UpdateAvailable,
            DriverCatalogState::Unavailable { reason } => RowStatus::Unavailable(reason.clone()),
        };
        let mut row = Self {
            id: item.id.clone(),
            name: item.display_name.clone(),
            installed_version: record.map(|record| record.version.clone()),
            latest_version: entry.map(|entry| entry.version.clone()),
            size_bytes: entry
                .and_then(|entry| entry.artifact_for_target(target))
                .map(|artifact| artifact.size_bytes),
            category: category_for(entry.and_then(|entry| entry.category), &schemes),
            chips,
            features,
            schemes,
            status,
            disabled: item.disabled,
            developer: record.is_some_and(|record| record.team_id.is_empty()),
            entry: item.manifest.clone(),
            record: record.cloned(),
            search: String::new(),
        };
        row.search = [
            row.name.as_str(),
            row.id.as_str(),
            &row.schemes.join(" "),
            &row.features.join(" "),
            &row.chips.join(" "),
        ]
        .join(" ")
        .to_lowercase();
        row
    }

    pub(in crate::app) fn has_update(&self) -> bool {
        self.status == RowStatus::UpdateAvailable
    }

    pub(in crate::app) fn is_installed(&self) -> bool {
        self.installed_version.is_some()
    }

    pub(in crate::app) fn matches(&self, query: &str) -> bool {
        query.is_empty() || self.search.contains(query)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(in crate::app) enum DriverFilter {
    #[default]
    All,
    Installed,
    Updates,
    Category(DriverCategory),
}

impl DriverFilter {
    pub(in crate::app) const CHIPS: [Self; 7] = [
        Self::All,
        Self::Installed,
        Self::Updates,
        Self::Category(DriverCategory::Sql),
        Self::Category(DriverCategory::Nosql),
        Self::Category(DriverCategory::Analytics),
        Self::Category(DriverCategory::KeyValue),
    ];

    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Installed => "Installed",
            Self::Updates => "Updates",
            Self::Category(DriverCategory::Sql) => "SQL",
            Self::Category(DriverCategory::Nosql) => "NoSQL",
            Self::Category(DriverCategory::Analytics) => "Analytics",
            Self::Category(DriverCategory::KeyValue) => "Key-Value",
            Self::Category(DriverCategory::Other) => "Other",
        }
    }

    pub(in crate::app) fn selector(self) -> &'static str {
        match self {
            Self::All => "driver-filter-all",
            Self::Installed => "driver-filter-installed",
            Self::Updates => "driver-filter-updates",
            Self::Category(DriverCategory::Sql) => "driver-filter-sql",
            Self::Category(DriverCategory::Nosql) => "driver-filter-nosql",
            Self::Category(DriverCategory::Analytics) => "driver-filter-analytics",
            Self::Category(DriverCategory::KeyValue) => "driver-filter-key-value",
            Self::Category(DriverCategory::Other) => "driver-filter-other",
        }
    }

    fn admits(self, row: &DriverRow) -> bool {
        match self {
            Self::All => true,
            Self::Installed => row.is_installed(),
            Self::Updates => row.has_update(),
            Self::Category(category) => row.category == Some(category),
        }
    }
}

/// The merged catalog: rows built once per catalog change, the filtered view
/// rebuilt once per filter / search change. Rendering only indexes into it.
#[derive(Default)]
pub(in crate::app) struct CatalogModel {
    rows: Vec<DriverRow>,
    visible: Vec<usize>,
    filter: DriverFilter,
    query: String,
}

impl CatalogModel {
    pub(in crate::app) fn rebuild(
        &mut self,
        catalog: &[DriverCatalogItem],
        records: &BTreeMap<String, InstalledDriverRecord>,
        target: &DriverTarget,
    ) {
        self.rows = catalog
            .iter()
            .map(|item| DriverRow::build(item, records.get(&item.id), target))
            .collect();
        self.refilter();
    }

    pub(in crate::app) fn set_filter(&mut self, filter: DriverFilter) {
        if self.filter != filter {
            self.filter = filter;
            self.refilter();
        }
    }

    /// `query` is trimmed and lower-cased here.
    pub(in crate::app) fn set_query(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        if self.query != query {
            self.query = query;
            self.refilter();
        }
    }

    fn refilter(&mut self) {
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| self.filter.admits(row) && row.matches(&self.query))
            .map(|(index, _)| index)
            .collect();
    }

    pub(in crate::app) fn filter(&self) -> DriverFilter {
        self.filter
    }

    pub(in crate::app) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(in crate::app) fn visible_row(&self, position: usize) -> Option<&DriverRow> {
        self.rows.get(*self.visible.get(position)?)
    }

    pub(in crate::app) fn rows(&self) -> &[DriverRow] {
        &self.rows
    }

    pub(in crate::app) fn row(&self, id: &str) -> Option<&DriverRow> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// Row count behind a chip (independent of the search text).
    pub(in crate::app) fn count(&self, filter: DriverFilter) -> usize {
        self.rows.iter().filter(|row| filter.admits(row)).count()
    }

    /// Rows "Update all" would update: an update with a manifest entry that is
    /// not disabled.
    pub(in crate::app) fn updatable(&self) -> impl Iterator<Item = &DriverRow> {
        self.rows
            .iter()
            .filter(|row| row.has_update() && !row.disabled && row.entry.is_some())
    }
}

// --- what a row offers ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum Badge {
    Queued,
    Disabled,
    Developer,
    Installed,
    Update,
    Unavailable(String),
}

pub(in crate::app) fn badge_for(row: &DriverRow, job: Option<&InstallJob>) -> Option<Badge> {
    if job.is_some_and(|job| job.state == InstallState::Queued) {
        return Some(Badge::Queued);
    }
    if row.disabled {
        return Some(Badge::Disabled);
    }
    if row.developer && row.is_installed() {
        return Some(Badge::Developer);
    }
    match &row.status {
        RowStatus::Unavailable(reason) => Some(Badge::Unavailable(reason.clone())),
        RowStatus::UpdateAvailable => Some(Badge::Update),
        RowStatus::Installed => Some(Badge::Installed),
        RowStatus::Available => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum PrimaryAction {
    Install,
    Update,
    Enable,
    Retry,
    /// The row is installing or queued: ✕ cancels it.
    Cancel,
    /// Sessions still run on an older generation: move them to the newest.
    Reconnect,
}

pub(in crate::app) fn primary_action(row: &DriverRow, job: Option<&InstallJob>) -> Option<PrimaryAction> {
    match job.map(|job| &job.state) {
        Some(state) if state.is_active() => return Some(PrimaryAction::Cancel),
        Some(InstallState::Failed(_)) => return Some(PrimaryAction::Retry),
        _ => {}
    }
    if row.disabled && row.is_installed() {
        return Some(PrimaryAction::Enable);
    }
    match (&row.status, &row.entry) {
        (RowStatus::Available, Some(_)) => Some(PrimaryAction::Install),
        (RowStatus::UpdateAvailable, Some(_)) => Some(PrimaryAction::Update),
        _ => None,
    }
}

/// [`primary_action`], or Reconnect when nothing else is on offer and
/// `stale_sessions` still run on an older generation.
pub(in crate::app) fn row_action(
    row: &DriverRow,
    job: Option<&InstallJob>,
    stale_sessions: bool,
) -> Option<PrimaryAction> {
    primary_action(row, job).or_else(|| stale_sessions.then_some(PrimaryAction::Reconnect))
}

/// `"1.3.0 → 1.4.0"`, `"— 1.0.0"`, `"1.2.0"` — `(current, arrow target)`.
pub(in crate::app) fn version_text(row: &DriverRow) -> (String, Option<String>) {
    match (&row.installed_version, &row.latest_version) {
        (Some(installed), Some(latest)) if installed != latest => {
            (installed.clone(), Some(latest.clone()))
        }
        (Some(installed), _) => (installed.clone(), None),
        (None, Some(latest)) => ("\u{2014}".into(), Some(latest.clone())),
        (None, None) => ("\u{2014}".into(), None),
    }
}

/// `1.2 / 2.0 MB`: both in the unit of the total.
pub(in crate::app) fn format_size_pair(done: u64, total: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if total >= 1024 * 1024 {
        format!("{:.1} / {:.1} MB", done as f64 / MB, total as f64 / MB)
    } else {
        format!("{} / {} KB", done.div_ceil(1024), total.div_ceil(1024))
    }
}

/// `2.1 MB` / `640 KB`.
pub(in crate::app) fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_host::DriverArtifact;

    fn entry(id: &str, version: &str) -> DriverManifestEntry {
        DriverManifestEntry {
            id: id.into(),
            display_name: id.to_uppercase(),
            version: version.into(),
            app_abi_version: 2,
            uri_schemes: vec![id.into()],
            features: vec!["query.sql".into(), "query.arrow_c_data".into(), "mutation.insert".into(), "mutation.update".into()],
            artifacts: vec![DriverArtifact {
                target: Some(crate::drivers::current_target().id()),
                url: format!("file:///{id}.dylib"),
                sha256: "00".into(),
                size_bytes: 2_000_000,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn progress(step: InstallStep, done: u64) -> InstallProgress {
        InstallProgress {
            step,
            bytes_done: done,
            bytes_total: 2_000_000,
        }
    }

    fn failure() -> InstallFailure {
        InstallFailure::new("driver.download_failed", "offline", true)
    }

    #[test]
    fn a_job_walks_queued_downloading_verifying_loading_done() {
        let mut queue = InstallQueue::default();
        assert_eq!(queue.enqueue(JobKind::Install, entry("pg", "1.0.0")), Enqueued::Queued);
        assert_eq!(queue.job("pg").unwrap().state, InstallState::Queued);
        let started = queue.start_next().expect("starts");
        assert_eq!(started.id, "pg");
        assert_eq!(queue.job("pg").unwrap().state, InstallState::Downloading { bytes: 0, total: 2_000_000 });

        queue.progress("pg", progress(InstallStep::Downloading, 1_000_000));
        assert_eq!(queue.job("pg").unwrap().state.fraction(), 0.5);
        for (step, label) in [
            (InstallStep::VerifyingChecksum, "Verifying checksum"),
            (InstallStep::VerifyingSignature, "Verifying signature"),
            (InstallStep::VerifyingAbi, "Checking driver ABI"),
        ] {
            queue.progress("pg", progress(step, 2_000_000));
            let state = &queue.job("pg").unwrap().state;
            assert_eq!(*state, InstallState::Verifying(step));
            assert_eq!(state.step_label(), label);
            assert_eq!(state.fraction(), 1.0);
        }
        queue.progress("pg", progress(InstallStep::Loading, 2_000_000));
        assert_eq!(queue.job("pg").unwrap().state, InstallState::Loading);

        assert!(queue.finish("pg", JobOutcome::Done));
        assert!(queue.job("pg").is_none(), "a done job leaves the queue");
        assert_eq!(queue.active_count(), 0);
    }

    #[test]
    fn late_progress_never_resurrects_a_finished_or_queued_job() {
        let mut queue = InstallQueue::default();
        queue.enqueue(JobKind::Install, entry("pg", "1.0.0"));
        queue.progress("pg", progress(InstallStep::Downloading, 5));
        assert_eq!(queue.job("pg").unwrap().state, InstallState::Queued, "not started yet");
        queue.start_next();
        queue.finish("pg", JobOutcome::Failed(failure()));
        queue.progress("pg", progress(InstallStep::Loading, 5));
        assert_eq!(queue.job("pg").unwrap().state, InstallState::Failed(failure()));
        assert!(!queue.finish("pg", JobOutcome::Done), "only a running job can finish");
    }

    #[test]
    fn installs_run_one_at_a_time_in_fifo_order() {
        let mut queue = InstallQueue::default();
        for id in ["a", "b", "c"] {
            queue.enqueue(JobKind::Install, entry(id, "1.0.0"));
        }
        assert_eq!(queue.start_next().unwrap().id, "a");
        assert!(queue.start_next().is_none(), "one is running");
        assert_eq!(queue.job("b").unwrap().state, InstallState::Queued);
        queue.finish("a", JobOutcome::Done);
        assert_eq!(queue.start_next().unwrap().id, "b");
        queue.finish("b", JobOutcome::Failed(failure()));
        assert_eq!(queue.start_next().unwrap().id, "c", "a failure does not stop the queue");
        queue.finish("c", JobOutcome::Done);
        assert!(queue.start_next().is_none());
    }

    #[test]
    fn the_same_driver_is_serialized_not_run_twice() {
        let mut queue = InstallQueue::default();
        assert_eq!(queue.enqueue(JobKind::Install, entry("pg", "1.0.0")), Enqueued::Queued);
        assert_eq!(queue.enqueue(JobKind::Update, entry("pg", "1.1.0")), Enqueued::AlreadyActive);
        queue.start_next();
        assert_eq!(
            queue.enqueue(JobKind::Reinstall, entry("pg", "1.0.0")),
            Enqueued::AlreadyActive,
            "also while it is running"
        );
        assert_eq!(queue.active_count(), 1);
        queue.finish("pg", JobOutcome::Done);
        assert_eq!(queue.enqueue(JobKind::Update, entry("pg", "1.1.0")), Enqueued::Queued, "free again afterwards");
    }

    #[test]
    fn a_failed_job_stays_for_retry_and_can_be_requeued_or_dismissed() {
        let mut queue = InstallQueue::default();
        queue.enqueue(JobKind::Install, entry("pg", "1.0.0"));
        queue.start_next();
        queue.finish("pg", JobOutcome::Failed(failure()));
        assert_eq!(queue.job("pg").unwrap().state, InstallState::Failed(failure()));
        assert_eq!(queue.active_count(), 0, "a failure is not active work");
        assert_eq!(queue.enqueue(JobKind::Install, entry("pg", "1.0.0")), Enqueued::Queued, "Retry");
        assert_eq!(queue.job("pg").unwrap().state, InstallState::Queued);
        queue.start_next();
        queue.finish("pg", JobOutcome::Failed(failure()));
        queue.dismiss("pg");
        assert!(queue.job("pg").is_none());
    }

    #[test]
    fn cancelling_a_queued_job_removes_it_without_touching_the_running_one() {
        let mut queue = InstallQueue::default();
        queue.enqueue(JobKind::Install, entry("a", "1.0.0"));
        queue.enqueue(JobKind::Install, entry("b", "1.0.0"));
        queue.start_next();
        assert_eq!(queue.cancel("b"), CancelEffect::Dequeued);
        assert!(queue.job("b").is_none());
        assert!(!queue.job("a").unwrap().cancelling());
        assert_eq!(queue.cancel("nope"), CancelEffect::Nothing);
    }

    #[test]
    fn cancelling_the_running_job_signals_the_worker_and_ends_through_finish() {
        let mut queue = InstallQueue::default();
        queue.enqueue(JobKind::Install, entry("a", "1.0.0"));
        let started = queue.start_next().unwrap();
        assert!(!started.cancel.load(Ordering::Acquire));
        assert_eq!(queue.cancel("a"), CancelEffect::Signalled);
        assert!(started.cancel.load(Ordering::Acquire), "the worker's flag is set");
        assert!(queue.job("a").unwrap().cancelling());
        assert!(queue.job("a").unwrap().state.is_running(), "still running until the worker stops");
        assert!(queue.finish("a", JobOutcome::Cancelled));
        assert!(queue.job("a").is_none(), "the row returns to its catalog state");
    }

    #[test]
    fn update_all_counts_finished_jobs_and_cancel_all_drains_it() {
        let mut queue = InstallQueue::default();
        let added = queue.enqueue_batch(["a", "b", "c"].map(|id| entry(id, "2.0.0")));
        assert_eq!(added, 3);
        assert_eq!(queue.batch_counter(), Some((0, 3)));
        queue.start_next();
        queue.finish("a", JobOutcome::Done);
        assert_eq!(queue.batch_counter(), Some((1, 3)));
        queue.start_next();
        queue.finish("b", JobOutcome::Failed(failure()));
        assert_eq!(queue.batch_counter(), Some((2, 3)), "a failure still counts as handled");
        queue.finish("b", JobOutcome::Done);
        assert_eq!(queue.batch_counter(), Some((2, 3)), "a non-running job cannot finish twice");
        queue.start_next();
        queue.finish("c", JobOutcome::Done);
        assert_eq!(queue.batch_counter(), None, "the batch ends with its last job");

        queue.enqueue_batch(["x", "y", "z"].map(|id| entry(id, "2.0.0")));
        queue.start_next();
        queue.cancel_all();
        assert_eq!(queue.batch_counter(), Some((2, 3)), "queued ones left, the running one is signalled");
        assert!(queue.job("x").unwrap().cancelling());
        queue.finish("x", JobOutcome::Cancelled);
        assert_eq!(queue.batch_counter(), None);
        assert_eq!(queue.active_count(), 0);
    }

    #[test]
    fn a_batch_skips_drivers_that_are_already_active() {
        let mut queue = InstallQueue::default();
        queue.enqueue(JobKind::Install, entry("a", "1.0.0"));
        assert_eq!(queue.enqueue_batch(["a", "b"].map(|id| entry(id, "2.0.0"))), 1);
        assert_eq!(queue.batch_counter(), Some((0, 1)));
        assert_eq!(queue.enqueue_batch(["a"].map(|id| entry(id, "2.0.0"))), 0);
        assert_eq!(queue.batch_counter(), Some((0, 1)));
    }

    #[test]
    fn failures_carry_stable_codes_and_retryability() {
        let io = DownloadError::Io(std::io::Error::other("reset"));
        let failure = InstallFailure::from(&io);
        assert_eq!((failure.code, failure.retryable), ("driver.download_failed", true));
        let insecure = InstallFailure::from(&DownloadError::InsecureUrl("http://x".into()));
        assert_eq!((insecure.code, insecure.retryable), ("driver.insecure_url", false));
        let checksum = InstallFailure::from(&HostInstallError::Verification(
            DriverVerificationError::ChecksumMismatch { expected: "a".into(), actual: "b".into() },
        ));
        assert_eq!((checksum.code, checksum.retryable), ("driver.checksum_mismatch", false));
        assert!(checksum.message.contains("SHA-256"), "{}", checksum.message);
    }

    fn item(id: &str, state: DriverCatalogState) -> DriverCatalogItem {
        let manifest = entry(id, "2.0.0");
        DriverCatalogItem {
            id: id.into(),
            display_name: manifest.display_name.clone(),
            version: manifest.version.clone(),
            state,
            manifest: Some(manifest),
            disabled: false,
        }
    }

    fn record(id: &str, version: &str, team: &str) -> InstalledDriverRecord {
        InstalledDriverRecord {
            id: id.into(),
            version: version.into(),
            team_id: team.into(),
            uri_schemes: vec![id.into()],
            ..Default::default()
        }
    }

    fn catalog() -> (Vec<DriverCatalogItem>, BTreeMap<String, InstalledDriverRecord>) {
        let mut mongo = item("mongodb", DriverCatalogState::Available);
        mongo.manifest.as_mut().unwrap().category = Some(DriverCategory::Nosql);
        let mut ch = item("clickhouse", DriverCatalogState::Available);
        ch.manifest.as_mut().unwrap().uri_schemes = vec!["clickhouse".into(), "http".into()];
        let mut redis = item("redis", DriverCatalogState::Unavailable { reason: "Needs TableX \u{2265} 9".into() });
        redis.manifest.as_mut().unwrap().category = Some(DriverCategory::KeyValue);
        let mut sqlite = item("sqlite", DriverCatalogState::Installed { version: "2.0.0".into() });
        sqlite.disabled = true;
        let catalog = vec![
            item("postgresql", DriverCatalogState::UpdateAvailable { installed: "1.0.0".into(), latest: "2.0.0".into() }),
            item("mysql", DriverCatalogState::Installed { version: "2.0.0".into() }),
            mongo,
            ch,
            redis,
            sqlite,
        ];
        let records = BTreeMap::from([
            ("postgresql".to_string(), record("postgresql", "1.0.0", "TEAM")),
            ("mysql".to_string(), record("mysql", "2.0.0", "")),
            ("sqlite".to_string(), record("sqlite", "2.0.0", "TEAM")),
        ]);
        (catalog, records)
    }

    fn model() -> CatalogModel {
        let (catalog, records) = catalog();
        let mut model = CatalogModel::default();
        model.rebuild(&catalog, &records, &crate::drivers::current_target());
        model
    }

    #[test]
    fn filter_chips_count_installed_updates_and_manifest_categories() {
        let model = model();
        assert_eq!(model.count(DriverFilter::All), 6);
        assert_eq!(model.count(DriverFilter::Installed), 3);
        assert_eq!(model.count(DriverFilter::Updates), 1);
        // postgresql / mysql / sqlite fall back to their scheme; mongodb and
        // redis declare theirs; clickhouse is recognised by its scheme.
        assert_eq!(model.count(DriverFilter::Category(DriverCategory::Sql)), 3);
        assert_eq!(model.count(DriverFilter::Category(DriverCategory::Nosql)), 1);
        assert_eq!(model.count(DriverFilter::Category(DriverCategory::Analytics)), 1);
        assert_eq!(model.count(DriverFilter::Category(DriverCategory::KeyValue)), 1);
    }

    #[test]
    fn selecting_a_chip_narrows_the_visible_rows() {
        let mut model = model();
        model.set_filter(DriverFilter::Updates);
        assert_eq!(model.visible_len(), 1);
        assert_eq!(model.visible_row(0).unwrap().id, "postgresql");
        model.set_filter(DriverFilter::Category(DriverCategory::Sql));
        let ids: Vec<_> = (0..model.visible_len()).map(|i| model.visible_row(i).unwrap().id.clone()).collect();
        assert_eq!(ids, ["postgresql", "mysql", "sqlite"]);
        model.set_filter(DriverFilter::All);
        assert_eq!(model.visible_len(), 6);
        assert!(model.visible_row(6).is_none());
    }

    #[test]
    fn search_matches_name_id_scheme_and_capability_and_combines_with_the_chip() {
        let mut model = model();
        for (query, expected) in [
            ("MONGO", vec!["mongodb"]),
            ("  mysql ", vec!["mysql"]),
            ("clickhouse", vec!["clickhouse"]),
            ("http", vec!["clickhouse"]),
            ("arrow", vec!["postgresql", "mysql", "mongodb", "clickhouse", "redis", "sqlite"]),
            ("edit", vec!["postgresql", "mysql", "mongodb", "clickhouse", "redis", "sqlite"]),
            ("nothing-matches", vec![]),
            ("", vec!["postgresql", "mysql", "mongodb", "clickhouse", "redis", "sqlite"]),
        ] {
            model.set_query(query);
            let ids: Vec<_> = (0..model.visible_len()).map(|i| model.visible_row(i).unwrap().id.clone()).collect();
            assert_eq!(ids, expected, "query {query:?}");
        }
        model.set_query("arrow");
        model.set_filter(DriverFilter::Installed);
        assert_eq!(model.visible_len(), 3);
        assert_eq!(model.count(DriverFilter::All), 6, "counts ignore the search text");
    }

    #[test]
    fn badges_and_primary_actions_follow_state_job_and_flags() {
        let model = model();
        let pg = model.row("postgresql").unwrap();
        assert_eq!(badge_for(pg, None), Some(Badge::Update));
        assert_eq!(primary_action(pg, None), Some(PrimaryAction::Update));
        let mysql = model.row("mysql").unwrap();
        assert_eq!(badge_for(mysql, None), Some(Badge::Developer), "no Team ID: a developer driver");
        assert_eq!(primary_action(mysql, None), None);
        let ch = model.row("clickhouse").unwrap();
        assert_eq!(badge_for(ch, None), None);
        assert_eq!(primary_action(ch, None), Some(PrimaryAction::Install));
        let redis = model.row("redis").unwrap();
        assert_eq!(badge_for(redis, None), Some(Badge::Unavailable("Needs TableX \u{2265} 9".into())));
        assert_eq!(primary_action(redis, None), None, "an unavailable driver cannot be installed");
        let sqlite = model.row("sqlite").unwrap();
        assert_eq!(badge_for(sqlite, None), Some(Badge::Disabled));
        assert_eq!(primary_action(sqlite, None), Some(PrimaryAction::Enable));

        let mut queue = InstallQueue::default();
        queue.enqueue(JobKind::Install, entry("clickhouse", "2.0.0"));
        queue.enqueue(JobKind::Install, entry("mongodb", "2.0.0"));
        queue.start_next();
        let queued = queue.job("mongodb");
        assert_eq!(badge_for(model.row("mongodb").unwrap(), queued), Some(Badge::Queued));
        assert_eq!(primary_action(model.row("mongodb").unwrap(), queued), Some(PrimaryAction::Cancel));
        assert_eq!(primary_action(ch, queue.job("clickhouse")), Some(PrimaryAction::Cancel));
        queue.finish("clickhouse", JobOutcome::Failed(failure()));
        assert_eq!(primary_action(ch, queue.job("clickhouse")), Some(PrimaryAction::Retry));
    }

    #[test]
    fn stale_sessions_offer_reconnect_only_when_nothing_else_is_primary() {
        let model = model();
        let mysql = model.row("mysql").unwrap();
        assert_eq!(row_action(mysql, None, false), None);
        assert_eq!(row_action(mysql, None, true), Some(PrimaryAction::Reconnect));
        let pg = model.row("postgresql").unwrap();
        assert_eq!(row_action(pg, None, true), Some(PrimaryAction::Update), "an update comes first");
        assert_eq!(format_size_pair(1_300_000, 2_000_000), "1.2 / 1.9 MB");
    }

    #[test]
    fn rows_show_capability_chips_versions_and_sizes() {
        let model = model();
        let pg = model.row("postgresql").unwrap();
        assert_eq!(pg.chips, ["sql", "arrow", "edit"]);
        let mut real = entry("mongodb", "1.0.0");
        real.features = ["navigator", "query.mongo_find", "query.mongo_script", "operation.cancel", "export.mongo_aggregate", "mutation.insert", "admin.indexes", "bson_rendering"].map(String::from).to_vec();
        let item = DriverCatalogItem { id: "mongodb".into(), display_name: "MongoDB".into(), version: "1.0.0".into(), state: DriverCatalogState::Available, manifest: Some(real), disabled: false };
        assert_eq!(DriverRow::build(&item, None, &crate::drivers::current_target()).chips, ["navigator", "mongo", "cancel", "export", "edit", "admin"], "real manifests render their own chips, one per family");
        assert_eq!(version_text(pg), ("1.0.0".to_string(), Some("2.0.0".to_string())));
        assert_eq!(version_text(model.row("mysql").unwrap()), ("2.0.0".to_string(), None));
        assert_eq!(version_text(model.row("clickhouse").unwrap()), ("\u{2014}".to_string(), Some("2.0.0".to_string())));
        assert_eq!(pg.size_bytes, Some(2_000_000));
        assert_eq!(format_size(2_000_000), "1.9 MB");
        assert_eq!(format_size(1_800), "2 KB");
    }

    #[test]
    fn update_all_covers_only_enabled_updates_with_a_manifest_entry() {
        let mut model = model();
        let ids: Vec<_> = model.updatable().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["postgresql"]);
        let (mut catalog, records) = catalog();
        catalog[0].disabled = true;
        model.rebuild(&catalog, &records, &crate::drivers::current_target());
        assert_eq!(model.updatable().count(), 0, "a disabled driver is not updated");
    }

    #[test]
    fn an_orphaned_install_without_a_manifest_is_still_listed() {
        let orphan = DriverCatalogItem {
            id: "legacy".into(),
            display_name: "legacy".into(),
            version: "0.9.0".into(),
            state: DriverCatalogState::Installed { version: "0.9.0".into() },
            manifest: None,
            disabled: false,
        };
        let records = BTreeMap::from([("legacy".to_string(), record("legacy", "0.9.0", "TEAM"))]);
        let mut model = CatalogModel::default();
        model.rebuild(&[orphan], &records, &crate::drivers::current_target());
        let row = model.row("legacy").unwrap();
        assert!(row.is_installed());
        assert_eq!(primary_action(row, None), None);
        assert_eq!(row.size_bytes, None);
    }
}
