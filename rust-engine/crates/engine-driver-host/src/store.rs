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
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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
}

/// A flat install that [`InstalledDriverStore::migrate_flat_installs`] moved
/// into its versioned directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedInstall {
    pub id: String,
    pub from: PathBuf,
    pub to: PathBuf,
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
            } else {
                record.previous_version = Some(existing.version);
                record.previous_path = Some(existing.path);
                record.previous_sha256 = Some(existing.sha256);
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

    fn driver_directory(&self) -> PathBuf {
        self.base_directory.join("Drivers").join("v2")
    }

    fn acquire_lock(&self, mode: LockMode) -> Result<StoreLock, StoreError> {
        let directory = self.driver_directory();
        fs::create_dir_all(&directory)?;
        let path = self.lock_path();
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

#[derive(Debug, Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

struct StoreLock {
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
