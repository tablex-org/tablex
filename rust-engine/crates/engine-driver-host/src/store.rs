use crate::os_fs;
use crate::verifier::{validate_driver_id, versioned_artifact_path};
use crate::DriverTarget;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const PENDING_DELETE: &str = ".pending-delete";
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Why an install was accepted, recorded with it: the Driver Manager marks a developer build by
/// this, not by the Team ID (an official release driver has none either).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallSource {
    /// Installed under the strict trust policy: the manifest was signed, every artifact pinned.
    Catalog,
    /// Installed under the developer policy (an unsigned or local manifest, an ad-hoc signed
    /// library, an imported file).
    Developer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledDriverRecord {
    pub id: String,
    pub version: String,
    pub path: PathBuf,
    pub sha256: String,
    pub team_id: String,
    pub installed_at: String,
    pub app_abi_version: u32,
    pub uri_schemes: Vec<String>,
    pub license: String,
    pub source_repo: String,
    #[serde(default)]
    pub disabled: bool,
    /// The version that was current before the latest install, kept on disk
    /// (one back) for rollback. Older versions are garbage collected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_sha256: Option<String>,
    /// How the current version came to be installed. Absent in records written before it was kept
    /// (and by an older build that rewrote the store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<InstallSource>,
    /// [`Self::source`] of the previous version, so a rollback restores it with the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_source: Option<InstallSource>,
}

/// A flat install that [`InstalledDriverStore::migrate_flat_installs`] moved
/// into its versioned directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedInstall {
    pub id: String,
    pub from: PathBuf,
    pub to: PathBuf,
}

/// What [`InstalledDriverStore::migrate_legacy_location`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyMigration {
    /// No store of the current format at the legacy location: nothing was touched.
    NotNeeded,
    /// The legacy `Drivers` directory now lives at `to`. `set_aside` is where the directory that
    /// was in the way went (renamed, never deleted or merged); `paths_rewritten` counts the
    /// absolute record paths that were rebased onto the new root.
    Moved {
        from: PathBuf,
        to: PathBuf,
        set_aside: Option<PathBuf>,
        paths_rewritten: usize,
    },
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("timed out waiting for driver store lock: {path}")]
    LockTimeout { path: PathBuf },
    #[error("driver store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("driver store JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("driver '{id}' has no previous version to roll back to")]
    NoPreviousVersion { id: String },
}

#[derive(Debug, Clone)]
pub struct InstalledDriverStore {
    base_directory: PathBuf,
    lock_timeout: Duration,
}

impl InstalledDriverStore {
    pub fn new(base_directory: impl Into<PathBuf>) -> Self {
        Self::with_lock_timeout(base_directory, DEFAULT_LOCK_TIMEOUT)
    }

    pub fn with_lock_timeout(base_directory: impl Into<PathBuf>, lock_timeout: Duration) -> Self {
        Self {
            base_directory: base_directory.into(),
            lock_timeout,
        }
    }

    pub fn store_path(&self) -> PathBuf {
        self.driver_directory().join("installed-drivers.json")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.driver_directory().join("installed-drivers.lock")
    }

    pub fn manifest_watermark_path(&self) -> PathBuf {
        self.driver_directory().join("manifest-generated-at")
    }

    pub fn base_directory(&self) -> &Path {
        &self.base_directory
    }

    pub fn registry_cache_path(&self) -> PathBuf {
        self.driver_directory().join("registry-cache.json")
    }

    pub fn load_all(&self) -> Result<BTreeMap<String, InstalledDriverRecord>, StoreError> {
        let _lock = self.acquire_lock(LockMode::Shared)?;
        self.load_unlocked()
    }

    pub fn record(&self, id: &str) -> Result<Option<InstalledDriverRecord>, StoreError> {
        Ok(self.load_all()?.remove(id))
    }

    pub fn save(&self, record: InstalledDriverRecord) -> Result<(), StoreError> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        let mut records = self.load_unlocked()?;
        records.insert(record.id.clone(), record);
        self.write_unlocked(records.into_values().collect())
    }

    /// Record a freshly installed version. The record it replaces (if it lives
    /// at a different path) becomes `previous_*`, so one version stays on disk
    /// for [`Self::rollback`]; reinstalling the same path keeps the existing
    /// previous version.
    pub fn save_installed(&self, mut record: InstalledDriverRecord) -> Result<(), StoreError> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        let mut records = self.load_unlocked()?;
        if let Some(existing) = records.remove(&record.id) {
            if existing.path == record.path {
                record.previous_version = existing.previous_version;
                record.previous_path = existing.previous_path;
                record.previous_sha256 = existing.previous_sha256;
                record.previous_source = existing.previous_source;
            } else {
                record.previous_version = Some(existing.version);
                record.previous_path = Some(existing.path);
                record.previous_sha256 = Some(existing.sha256);
                record.previous_source = existing.source;
            }
        }
        records.insert(record.id.clone(), record);
        self.write_unlocked(records.into_values().collect())
    }

    /// Make the previous version current again and clear `previous_*` (the
    /// abandoned version is left for garbage collection). Fails without
    /// touching the record when there is no previous version or its file is
    /// gone.
    pub fn rollback(&self, id: &str) -> Result<InstalledDriverRecord, StoreError> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        let mut records = self.load_unlocked()?;
        let no_previous = || StoreError::NoPreviousVersion { id: id.into() };
        let record = records.get_mut(id).ok_or_else(no_previous)?;
        let (Some(version), Some(path), Some(sha256)) = (
            record.previous_version.take(),
            record.previous_path.take(),
            record.previous_sha256.take(),
        ) else {
            return Err(no_previous());
        };
        if !path.is_file() {
            return Err(no_previous());
        }
        record.version = version;
        record.path = path;
        record.sha256 = sha256;
        record.source = record.previous_source.take();
        let rolled_back = record.clone();
        self.write_unlocked(records.into_values().collect())?;
        Ok(rolled_back)
    }

    /// Move installs left in the legacy flat layout (`Drivers/v2/lib….dylib` or
    /// `Drivers/v2/<id>/lib….dylib`) into `Drivers/v2/<id>/<version>/` and
    /// update their records. Records pointing elsewhere, missing files and
    /// invalid ids/versions are left untouched. Run once at startup, before any
    /// driver is loaded.
    pub fn migrate_flat_installs(&self) -> Result<Vec<MigratedInstall>, StoreError> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        let mut records = self.load_unlocked()?;
        let root = self.driver_directory();
        let mut migrated = Vec::new();
        for record in records.values_mut() {
            let is_flat = record
                .path
                .parent()
                .is_some_and(|parent| parent == root || parent == root.join(&record.id));
            if !is_flat || !record.path.is_file() || validate_driver_id(&record.id).is_err() {
                continue;
            }
            let Ok(target) = versioned_artifact_path(
                &self.base_directory,
                &DriverTarget::current(),
                &record.id,
                &record.version,
            ) else {
                continue;
            };
            if let Some(directory) = target.parent() {
                fs::create_dir_all(directory)?;
            }
            if target.exists() {
                // A versioned copy already exists; keep it, drop the stale flat file.
                fs::remove_file(&record.path)?;
            } else {
                fs::rename(&record.path, &target)?;
            }
            migrated.push(MigratedInstall {
                id: record.id.clone(),
                from: std::mem::replace(&mut record.path, target.clone()),
                to: target,
            });
        }
        if !migrated.is_empty() {
            self.write_unlocked(records.into_values().collect())?;
        }
        Ok(migrated)
    }

    /// One-time move of a store kept at `<legacy_base>/Drivers` to this store's own
    /// `<base>/Drivers`. Only a store in the current (camelCase) record format counts: anything
    /// else at the legacy location (the generic `Drivers` name may belong to another program, or
    /// to the old Swift app) is left alone and reported as [`LegacyMigration::NotNeeded`].
    ///
    /// Under the legacy store's exclusive lock: (1) rewrite the absolute `path` / `previousPath`
    /// of the records that start with the legacy root onto the new root (atomic write, in place);
    /// (2) rename a `<base>/Drivers` in the way to `Drivers.legacy-<UTC yyyymmddHHMMSS>`; (3)
    /// rename the legacy `Drivers` directory to `<base>/Drivers` (same volume, one atomic step).
    ///
    /// A crash after (1) leaves the legacy store in place with records already naming the new
    /// root: the next run redoes (2) and (3), the rewrite is a no-op by then. A crash after (2)
    /// leaves no target and the intact legacy store: the next run needs no aside. After (3) the
    /// store is complete at the new location and the legacy one is gone, so a second run is
    /// `NotNeeded`. When (2) or (3) fails (for instance across volumes) the records and the
    /// aside directory are put back and the error returned: the legacy store is still whole and
    /// usable in place. Nothing is ever copied.
    pub fn migrate_legacy_location(
        &self,
        legacy_base: &Path,
    ) -> Result<LegacyMigration, StoreError> {
        let legacy = Self::with_lock_timeout(legacy_base, self.lock_timeout);
        let legacy_root = legacy_base.join("Drivers");
        let new_root = self.base_directory.join("Drivers");
        if legacy_root == new_root || !legacy.store_path().is_file() {
            return Ok(LegacyMigration::NotNeeded);
        }
        // Checked before the lock too, so a foreign `Drivers` directory never gets a lock file.
        if legacy.read_current_format()?.is_none() {
            return Ok(LegacyMigration::NotNeeded);
        }
        // Windows cannot rename a directory that holds an open file, and a process waiting for the
        // lock below holds its lock file open while it waits: so the movers queue on a lock file
        // beside the new store, in the app's own folder (kept after the move: a delete-pending file
        // refuses its next opener).
        #[cfg(windows)]
        let _queue = {
            fs::create_dir_all(&self.base_directory)?;
            self.lock_at(self.base_directory.join("Drivers.migrate.lock"), LockMode::Exclusive)?
        };
        // A process that lost the race finds the directory gone (the lock file moved with it).
        let Some(lock) = legacy.acquire_lock_if_present(LockMode::Exclusive)? else {
            return Ok(LegacyMigration::NotNeeded);
        };
        let Some((mut records, original)) = legacy.read_current_format()? else {
            return Ok(LegacyMigration::NotNeeded);
        };
        let mut paths_rewritten = 0;
        let mut rebase = |path: &mut PathBuf| {
            if let Ok(rest) = path.strip_prefix(&legacy_root) {
                *path = new_root.join(rest);
                paths_rewritten += 1;
            }
        };
        for record in &mut records {
            rebase(&mut record.path);
            if let Some(previous) = record.previous_path.as_mut() {
                rebase(previous);
            }
        }
        if paths_rewritten > 0 {
            legacy.write_unlocked(records)?;
        }
        // Windows cannot rename a directory that holds an open file (the lock file).
        let mut lock = Some(lock);
        if cfg!(windows) {
            lock = None;
        }
        match move_into_place(&legacy_root, &new_root) {
            Ok(set_aside) => {
                drop(lock);
                Ok(LegacyMigration::Moved {
                    from: legacy_root,
                    to: new_root,
                    set_aside,
                    paths_rewritten,
                })
            }
            Err(error) => {
                if paths_rewritten > 0 {
                    let _ = legacy.write_file_atomic(&legacy.store_path(), &original);
                }
                drop(lock);
                Err(StoreError::Io(error))
            }
        }
    }

    /// Anti-replay watermark: record `generated_at_unix` as the newest accepted
    /// manifest time unless it is older than the stored one. Returns whether
    /// the manifest is acceptable.
    pub fn accept_manifest_time(&self, generated_at_unix: i64) -> Result<bool, StoreError> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        let path = self.manifest_watermark_path();
        let newest = match fs::read_to_string(&path) {
            Ok(text) => text.trim().parse::<i64>().ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(StoreError::Io(error)),
        };
        if newest.is_some_and(|newest| generated_at_unix < newest) {
            return Ok(false);
        }
        if newest != Some(generated_at_unix) {
            self.write_file_atomic(&path, format!("{generated_at_unix}\n").as_bytes())?;
        }
        Ok(true)
    }

    pub fn set_disabled(&self, id: &str, disabled: bool) -> Result<bool, StoreError> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        let mut records = self.load_unlocked()?;
        let Some(record) = records.get_mut(id) else {
            return Ok(false);
        };
        record.disabled = disabled;
        self.write_unlocked(records.into_values().collect())?;
        Ok(true)
    }

    pub fn remove_record(&self, id: &str) -> Result<Option<InstalledDriverRecord>, StoreError> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        let mut records = self.load_unlocked()?;
        let removed = records.remove(id);
        if removed.is_some() {
            self.write_unlocked(records.into_values().collect())?;
        }
        Ok(removed)
    }

    pub fn remove(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.remove_record(id)?.is_some())
    }

    /// Where files that could not be deleted are moved to wait for [`Self::purge_pending_deletes`].
    /// It is not an `<id>` directory (ids start with a letter, digit, `-` or `_`), so no scan of
    /// the installed drivers sees it.
    pub fn pending_delete_directory(&self) -> PathBuf {
        self.driver_directory().join(PENDING_DELETE)
    }

    /// Delete a file or directory tree of the store. Windows cannot delete a library a loaded
    /// driver still maps (the app keeps its `Arc<DriverHandle>` until the driver is dropped, and
    /// a driver's own threads outlive its shutdown, so the library is never unloaded early) but it
    /// can rename one: what cannot be deleted is moved into the pending-delete directory, which
    /// frees its name, and the next [`Self::purge_pending_deletes`] removes it. Elsewhere this is a
    /// plain delete.
    pub(crate) fn discard(&self, path: &Path) -> std::io::Result<()> {
        let result = if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        match result {
            Err(_) if cfg!(windows) && path.exists() => {
                self.park_files(path)?;
                if path.is_dir() {
                    fs::remove_dir_all(path)?;
                }
                Ok(())
            }
            other => other,
        }
    }

    /// Move `path` (a file) into the pending-delete directory; returns where it went.
    pub(crate) fn park(&self, path: &Path) -> std::io::Result<PathBuf> {
        let directory = self.pending_delete_directory();
        fs::create_dir_all(&directory)?;
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
        let unique = format!(
            "{}-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.as_nanos()),
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        );
        let target = directory.join(unique);
        fs::rename(path, &target)?;
        Ok(target)
    }

    /// Park every file left under `path`; the empty directories stay for the caller to delete.
    fn park_files(&self, path: &Path) -> std::io::Result<()> {
        if !path.is_dir() {
            return self.park(path).map(drop);
        }
        for entry in fs::read_dir(path)? {
            self.park_files(&entry?.path())?;
        }
        Ok(())
    }

    /// The lock that keeps a purge and an install over a loaded library (park, then take the name)
    /// from interleaving, across processes (`tablex-mcp` shares the store). Separate from the
    /// store lock: [`Self::purge_pending_deletes`] reads the records while it holds this one.
    pub(crate) fn park_lock(&self) -> Result<StoreLock, StoreError> {
        fs::create_dir_all(self.driver_directory())?;
        self.lock_at(self.driver_directory().join("installed-drivers.park.lock"), LockMode::Exclusive)
    }

    /// Puts `staging` at `destination`, moving what is there out of the way first (Windows cannot
    /// replace a library a loaded driver maps, but can rename it). The old file is put back when
    /// the rename fails. Under [`Self::park_lock`] from the park to the end, so a purge never sees
    /// the name free, and never deletes the parked file, half-way.
    pub(crate) fn replace_by_parking(&self, staging: &Path, destination: &Path) -> Result<(), StoreError> {
        let _lock = self.park_lock()?;
        let parked = self.park(destination)?;
        if let Err(error) = fs::rename(staging, destination) {
            let _ = fs::rename(&parked, destination);
            return Err(error.into());
        }
        Ok(())
    }

    /// Delete what earlier uninstalls and updates moved aside ([`Self::discard`]). Best effort:
    /// an entry still locked (its driver is loaded by another process) stays for the next time.
    /// An entry that an installed record still points to, by its file name, with nothing at that
    /// path (an install that moved the library aside and could not put it back) is put back, not
    /// deleted, when its SHA-256 is the record's (`sha256` for `path`, `previous_sha256` for
    /// `previous_path`): a parked copy of another version is deleted. Under [`Self::park_lock`]
    /// (skipped, everything remaining, when an install holds it past the timeout). Run it at
    /// startup, before any driver is loaded. Returns how many entries remain.
    pub fn purge_pending_deletes(&self) -> usize {
        let directory = self.pending_delete_directory();
        if !directory.is_dir() {
            return 0;
        }
        let Ok(_lock) = self.park_lock() else {
            return fs::read_dir(&directory).map_or(0, |entries| entries.flatten().count());
        };
        let Ok(entries) = fs::read_dir(&directory) else {
            return 0;
        };
        let entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        // Without the records nothing can be told from a library that is still in use: keep all.
        let Ok(records) = self.load_all() else {
            return entries.len();
        };
        // Where a record points, with nothing there, and the digest of what belongs there.
        let mut missing: Vec<(PathBuf, String)> = records
            .into_values()
            .flat_map(|record| [(Some(record.path), Some(record.sha256)), (record.previous_path, record.previous_sha256)])
            .filter_map(|(path, sha256)| Some((path?, sha256?)))
            .filter(|(path, _)| !path.exists())
            .collect();
        let mut remaining = 0;
        for path in entries {
            if let Some(original) = Self::parked_name(&path) {
                let named = |wanted: &Path| wanted.file_name().is_some_and(|name| name == original);
                // Hashed only when a record could want it.
                let digest = missing
                    .iter()
                    .any(|(wanted, _)| named(wanted))
                    .then(|| crate::verifier::sha256_file(&path).ok())
                    .flatten();
                let at = digest.and_then(|digest| {
                    missing
                        .iter()
                        .position(|(wanted, sha256)| named(wanted) && sha256.eq_ignore_ascii_case(&digest))
                });
                if let Some(at) = at {
                    let (wanted, _) = missing.remove(at);
                    let restored = wanted
                        .parent()
                        .map_or(Ok(()), fs::create_dir_all)
                        .and_then(|()| fs::rename(&path, &wanted));
                    if restored.is_err() {
                        remaining += 1;
                    }
                    continue;
                }
            }
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if removed.is_err() {
                remaining += 1;
            }
        }
        if remaining == 0 {
            let _ = fs::remove_dir(&directory);
        }
        remaining
    }

    /// The name a parked entry had before [`Self::park`] (`<pid>-<nanos>-<sequence>-<name>`).
    fn parked_name(entry: &Path) -> Option<&str> {
        entry.file_name()?.to_str()?.splitn(4, '-').nth(3)
    }

    fn driver_directory(&self) -> PathBuf {
        self.base_directory.join("Drivers").join("v2")
    }

    fn acquire_lock(&self, mode: LockMode) -> Result<StoreLock, StoreError> {
        fs::create_dir_all(self.driver_directory())?;
        self.lock_file(mode)
    }

    /// [`Self::acquire_lock`] for a store that may have been moved away meanwhile: `None` when its
    /// directory is gone, instead of creating it again.
    fn acquire_lock_if_present(&self, mode: LockMode) -> Result<Option<StoreLock>, StoreError> {
        match self.lock_file(mode) {
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            other => other.map(Some),
        }
    }

    fn lock_file(&self, mode: LockMode) -> Result<StoreLock, StoreError> {
        self.lock_at(self.lock_path(), mode)
    }

    fn lock_at(&self, path: PathBuf, mode: LockMode) -> Result<StoreLock, StoreError> {
        let file = os_fs::open_private(
            OpenOptions::new().create(true).read(true).write(true),
            &path,
        )?;
        let started = Instant::now();

        loop {
            let attempt = match mode {
                LockMode::Shared => file.try_lock_shared(),
                LockMode::Exclusive => file.try_lock(),
            };
            match attempt {
                Ok(()) => return Ok(StoreLock { file }),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(StoreError::Io(error)),
            }
            if started.elapsed() >= self.lock_timeout {
                return Err(StoreError::LockTimeout { path });
            }
            thread::sleep(LOCK_RETRY_DELAY.min(self.lock_timeout));
        }
    }

    /// The records and the raw bytes of the store file, or `None` when there is no file or it is not
    /// a list of current-format records.
    #[allow(clippy::type_complexity)]
    fn read_current_format(
        &self,
    ) -> Result<Option<(Vec<InstalledDriverRecord>, Vec<u8>)>, StoreError> {
        match fs::read(self.store_path()) {
            Ok(data) => Ok(serde_json::from_slice(&data).ok().map(|records| (records, data))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    fn load_unlocked(&self) -> Result<BTreeMap<String, InstalledDriverRecord>, StoreError> {
        let path = self.store_path();
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new())
            }
            Err(error) => return Err(StoreError::Io(error)),
        };
        let records: Vec<InstalledDriverRecord> = serde_json::from_slice(&data)?;
        Ok(records
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect())
    }

    fn write_unlocked(&self, records: Vec<InstalledDriverRecord>) -> Result<(), StoreError> {
        let mut data = serde_json::to_vec_pretty(&records)?;
        data.push(b'\n');
        self.write_file_atomic(&self.store_path(), &data)
    }

    fn write_file_atomic(&self, path: &Path, data: &[u8]) -> Result<(), StoreError> {
        let directory = self.driver_directory();
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("store");
        let temp_path = directory.join(format!("{name}-{}-{sequence}.tmp", std::process::id()));
        let result = self.write_bytes_and_rename(&directory, &temp_path, path, data);
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }

    fn write_bytes_and_rename(
        &self,
        directory: &Path,
        temp_path: &Path,
        destination: &Path,
        data: &[u8],
    ) -> Result<(), StoreError> {
        let mut temp =
            os_fs::open_private(OpenOptions::new().create_new(true).write(true), temp_path)?;
        temp.write_all(data)?;
        temp.sync_all()?;
        fs::rename(temp_path, destination)?;
        os_fs::sync_directory(directory)?;
        Ok(())
    }
}

/// Renames `from` to `to`, first renaming a `to` that exists to `Drivers.legacy-<UTC stamp>` next
/// to it; returns where that went. If the final rename fails the aside rename is undone.
fn move_into_place(from: &Path, to: &Path) -> std::io::Result<Option<PathBuf>> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let set_aside = match fs::symlink_metadata(to) {
        Ok(_) => {
            let aside = free_aside_path(to);
            fs::rename(to, &aside)?;
            Some(aside)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Err(error) = fs::rename(from, to) {
        if let Some(aside) = &set_aside {
            let _ = fs::rename(aside, to);
        }
        return Err(error);
    }
    Ok(set_aside)
}

fn free_aside_path(target: &Path) -> PathBuf {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let (year, month, day, hour, minute, second) = crate::installer::civil_from_unix(seconds);
    let name = format!("Drivers.legacy-{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}");
    let mut candidate = target.with_file_name(&name);
    let mut attempt = 1;
    while fs::symlink_metadata(&candidate).is_ok() {
        candidate = target.with_file_name(format!("{name}-{attempt}"));
        attempt += 1;
    }
    candidate
}

#[derive(Debug, Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

pub(crate) struct StoreLock {
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_at(path: PathBuf) -> InstalledDriverRecord {
        let sha256 = crate::verifier::sha256_file(&path).unwrap_or_default();
        InstalledDriverRecord { id: "mock".into(), version: "1".into(), path, sha256, ..Default::default() }
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        crate::download::hex(&Sha256::digest(bytes))
    }

    #[test]
    fn a_library_that_could_not_be_put_back_is_not_purged_while_a_record_points_to_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = InstalledDriverStore::new(temp.path());
        let library = temp.path().join("Drivers").join("v2").join("mock").join("1").join("libmock.so");
        std::fs::create_dir_all(library.parent().unwrap()).unwrap();
        std::fs::write(&library, b"driver").unwrap();
        store.save(record_at(library.clone())).unwrap();
        // The install moved the library aside and its restore failed: the file sits in the pending
        // directory and the record still names its old place.
        store.park(&library).unwrap();
        let stray = temp.path().join("libother.so");
        std::fs::write(&stray, b"old").unwrap();
        store.park(&stray).unwrap();
        assert!(!library.exists());

        assert_eq!(store.purge_pending_deletes(), 0);
        assert_eq!(std::fs::read(&library).unwrap(), b"driver", "put back where the record points");
        assert!(!store.pending_delete_directory().exists(), "the entry nobody points to is deleted");
    }

    #[test]
    fn an_entry_is_deleted_when_the_record_finds_its_library() {
        let temp = tempfile::tempdir().unwrap();
        let store = InstalledDriverStore::new(temp.path());
        let library = temp.path().join("libmock.so");
        std::fs::write(&library, b"new").unwrap();
        store.save(record_at(library.clone())).unwrap();
        let old = temp.path().join("old");
        std::fs::create_dir_all(&old).unwrap();
        let moved_aside = old.join("libmock.so");
        std::fs::write(&moved_aside, b"old").unwrap();
        store.park(&moved_aside).unwrap();

        assert_eq!(store.purge_pending_deletes(), 0);
        assert_eq!(std::fs::read(&library).unwrap(), b"new", "the installed file is not replaced");
        assert!(!store.pending_delete_directory().exists());
    }

    /// Two versions of `libmock.so` parked under one name: each goes back to its own folder, the
    /// copy a record does not describe is deleted.
    #[test]
    fn the_parked_copy_with_the_records_digest_is_the_one_put_back() {
        let temp = tempfile::tempdir().unwrap();
        let store = InstalledDriverStore::new(temp.path());
        let current = temp.path().join("2").join("libmock.so");
        let previous = temp.path().join("1").join("libmock.so");
        let mut record = record_at(current.clone());
        record.sha256 = sha_of(b"version 2");
        record.previous_path = Some(previous.clone());
        record.previous_sha256 = Some(sha_of(b"version 1"));
        store.save(record).unwrap();
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        // Parked in an order that does not match the records, plus a third unrelated version.
        for content in [&b"version 1"[..], b"version 3", b"version 2"] {
            let file = elsewhere.join("libmock.so");
            std::fs::write(&file, content).unwrap();
            store.park(&file).unwrap();
        }

        assert_eq!(store.purge_pending_deletes(), 0);
        assert_eq!(std::fs::read(&current).unwrap(), b"version 2");
        assert_eq!(std::fs::read(&previous).unwrap(), b"version 1");
        assert!(!store.pending_delete_directory().exists(), "version 3 was deleted");
    }

    #[test]
    fn a_parked_copy_that_is_not_the_previous_version_is_not_put_back_as_one() {
        let temp = tempfile::tempdir().unwrap();
        let store = InstalledDriverStore::new(temp.path());
        let current = temp.path().join("2").join("libmock.so");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, b"version 2").unwrap();
        let previous = temp.path().join("1").join("libmock.so");
        let mut record = record_at(current.clone());
        record.previous_path = Some(previous.clone());
        record.previous_sha256 = Some(sha_of(b"version 1"));
        store.save(record).unwrap();
        let stray = temp.path().join("libmock.so");
        std::fs::write(&stray, b"some other build").unwrap();
        store.park(&stray).unwrap();

        assert_eq!(store.purge_pending_deletes(), 0);
        assert!(!previous.exists(), "a file of another digest never stands in for the previous version");
        assert!(!store.pending_delete_directory().exists());
    }

    #[test]
    fn a_purge_skips_while_an_install_holds_the_park_lock_and_runs_after() {
        let temp = tempfile::tempdir().unwrap();
        let store = InstalledDriverStore::with_lock_timeout(temp.path(), Duration::from_millis(60));
        let stray = temp.path().join("libold.so");
        std::fs::write(&stray, b"old").unwrap();
        store.park(&stray).unwrap();
        let install = store.park_lock().unwrap();
        assert_eq!(store.purge_pending_deletes(), 1, "nothing is touched while the install is half-way");
        assert_eq!(std::fs::read_dir(store.pending_delete_directory()).unwrap().count(), 1);
        drop(install);
        assert_eq!(store.purge_pending_deletes(), 0);
        assert!(!store.pending_delete_directory().exists());
    }

    #[test]
    fn replacing_by_parking_holds_the_park_lock_and_puts_the_old_file_back_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        let store = InstalledDriverStore::with_lock_timeout(temp.path(), Duration::from_millis(60));
        let destination = temp.path().join("libmock.so");
        let staging = temp.path().join(".libmock.so.staging");
        std::fs::write(&destination, b"old").unwrap();
        std::fs::write(&staging, b"new").unwrap();

        // A purge of another process holds the lock: the install waits, then gives up untouched.
        let purge = store.park_lock().unwrap();
        assert!(matches!(
            store.replace_by_parking(&staging, &destination),
            Err(StoreError::LockTimeout { .. })
        ));
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        drop(purge);

        // The rename cannot happen (no staging file): the old file is back in its place.
        let missing = temp.path().join("no-such-staging");
        assert!(store.replace_by_parking(&missing, &destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");

        store.replace_by_parking(&staging, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(store.pending_delete_directory()).unwrap().count(), 1, "the old file waits for the purge");
    }
}
