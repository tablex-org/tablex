use crate::progress::{InstallStep, ProgressSink, ThrottledProgress};
use crate::{
    DownloadedArtifact, DriverArtifact, DriverHandle, DriverLoader, DriverManifestEntry,
    DriverOwners, DriverTarget, DriverVerificationError, DriverVerifier, InstalledDriverRecord,
    InstalledDriverStore, LoaderError, NoProgress, SignerConfig, TargetOs, TrustPolicy,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

static STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(transparent)]
    Verification(#[from] DriverVerificationError),
    #[error("install I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Loader(#[from] crate::LoaderError),
    #[error(transparent)]
    Store(crate::StoreError),
}

pub struct DriverInstaller<'a> {
    verifier: DriverVerifier,
    store: &'a InstalledDriverStore,
    verify_signature: bool,
    policy: TrustPolicy,
    windows_publisher: String,
}

/// Result of [`DriverInstaller::try_uninstall`].
// One value per uninstall click; boxing the record would only hurt the API.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    NotInstalled,
    Removed(InstalledDriverRecord),
    /// Sessions or in-flight work still use the driver. Nothing was changed;
    /// the app lists its connections, asks "Disconnect and uninstall", drops
    /// them, and calls again.
    InUse(DriverOwners),
}

/// A freshly loaded driver generation.
pub struct ActivatedDriver {
    pub handle: Arc<DriverHandle>,
    /// Version directories garbage collected after the load (older than the
    /// previous version and not held by any owner), or why that failed.
    pub collected: Result<Vec<PathBuf>, InstallError>,
}

impl<'a> DriverInstaller<'a> {
    pub fn new(target: DriverTarget, store: &'a InstalledDriverStore) -> Self {
        Self {
            verifier: DriverVerifier::new(target),
            store,
            verify_signature: true,
            policy: TrustPolicy::Strict,
            windows_publisher: String::new(),
        }
    }

    /// Pin the Authenticode publisher Windows libraries must carry. The macOS
    /// Team ID comes from each manifest entry.
    pub fn with_windows_publisher(mut self, publisher: impl Into<String>) -> Self {
        self.windows_publisher = publisher.into();
        self
    }

    /// Skip the native OS signature check (codesign/Team ID, Authenticode).
    /// Only for tests using locally built, ad-hoc signed fixture libraries.
    pub fn with_signature_verification(mut self, enabled: bool) -> Self {
        self.verify_signature = enabled;
        self
    }

    /// Under [`TrustPolicy::Developer`] ad-hoc signed dylibs (no Team ID) pass
    /// the signature step; they must still be validly signed.
    pub fn with_trust_policy(mut self, policy: TrustPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Verify and atomically install a downloaded artifact file.
    ///
    /// Verification order: size, checksum, architecture, code signature,
    /// ABI version. Only after all checks pass is the file staged and
    /// renamed into the versioned destination, and the installed record
    /// written under the store lock.
    pub fn install_from_file(
        &self,
        entry: &DriverManifestEntry,
        artifact: &Path,
    ) -> Result<InstalledDriverRecord, InstallError> {
        self.install_from_file_with_progress(entry, artifact, &NoProgress)
    }

    /// [`Self::install_from_file`] reporting `VerifyingChecksum`,
    /// `VerifyingSignature` and `VerifyingAbi` to `sink`.
    pub fn install_from_file_with_progress(
        &self,
        entry: &DriverManifestEntry,
        artifact: &Path,
        sink: &dyn ProgressSink,
    ) -> Result<InstalledDriverRecord, InstallError> {
        let meta = self.artifact_meta(entry)?;
        let mut progress = ThrottledProgress::new(sink);
        progress.report(
            InstallStep::VerifyingChecksum,
            0,
            meta.size_bytes,
            Instant::now(),
        );
        self.verifier.verify_size(artifact, meta.size_bytes)?;
        let digest = crate::verifier::sha256_file(artifact)?;
        self.verifier.verify_digest(&digest, &meta.sha256)?;
        self.install_checked(entry, artifact, digest, meta.size_bytes, progress)
    }

    /// Install an artifact received through [`crate::stream_to_file`]. The
    /// bytes were hashed while streaming, so the file is not read again for
    /// the checksum. The caller still owns (and deletes) `downloaded.path`.
    pub fn install_downloaded(
        &self,
        entry: &DriverManifestEntry,
        downloaded: &DownloadedArtifact,
        sink: &dyn ProgressSink,
    ) -> Result<InstalledDriverRecord, InstallError> {
        let meta = self.artifact_meta(entry)?;
        let mut progress = ThrottledProgress::new(sink);
        progress.report(
            InstallStep::VerifyingChecksum,
            meta.size_bytes,
            meta.size_bytes,
            Instant::now(),
        );
        if downloaded.size_bytes != meta.size_bytes {
            return Err(DriverVerificationError::SizeMismatch {
                expected: meta.size_bytes,
                actual: downloaded.size_bytes,
            }
            .into());
        }
        self.verifier
            .verify_digest(&downloaded.sha256, &meta.sha256)?;
        self.install_checked(
            entry,
            &downloaded.path,
            downloaded.sha256.clone(),
            meta.size_bytes,
            progress,
        )
    }

    /// Load the installed version (new connections use it from now on) and
    /// garbage collect version directories older than the previous one that no
    /// owner holds. Older generations with open sessions stay loaded and their
    /// files stay on disk until the last owner drops.
    pub fn activate(
        &self,
        record: &InstalledDriverRecord,
        sink: &dyn ProgressSink,
    ) -> Result<ActivatedDriver, InstallError> {
        let size = std::fs::metadata(&record.path).map_or(0, |meta| meta.len());
        ThrottledProgress::new(sink).report(InstallStep::Loading, size, size, Instant::now());
        let handle = DriverLoader.load_verified(&record.path, &record.sha256)?;
        Ok(ActivatedDriver {
            handle,
            collected: self.gc_versions(&record.id),
        })
    }

    /// Make the previous version current again. Returns the new current
    /// record; call [`Self::activate`] with it. Fails with
    /// `StoreError::NoPreviousVersion` when there is nothing to go back to.
    pub fn rollback(&self, id: &str) -> Result<InstalledDriverRecord, InstallError> {
        Ok(self.store.rollback(id)?)
    }

    /// Delete version directories of `id` that are neither the current nor the
    /// previous version and are not held by a live generation. Returns the
    /// removed directories.
    pub fn gc_versions(&self, id: &str) -> Result<Vec<PathBuf>, InstallError> {
        let Some(record) = self.store.record(id)? else {
            return Ok(Vec::new());
        };
        let mut removed = Vec::new();
        for directory in self.version_directories(id)? {
            let kept = [Some(&record.path), record.previous_path.as_ref()]
                .into_iter()
                .flatten()
                .any(|path| path.starts_with(&directory));
            let artifact = directory.join(self.verifier.target().library_file_name(id));
            if kept || DriverLoader.is_loaded(&artifact) {
                continue;
            }
            std::fs::remove_dir_all(&directory)?;
            removed.push(directory);
        }
        Ok(removed)
    }

    /// Sessions and in-flight work across every loaded version of `id`. Use it
    /// to gate disable; empty when the driver is unknown or not loaded.
    pub fn driver_owners(&self, id: &str) -> Result<DriverOwners, InstallError> {
        let Some(record) = self.store.record(id)? else {
            return Ok(DriverOwners::default());
        };
        let handles = self.loaded_handles(&record)?;
        Ok(handles.iter().fold(DriverOwners::default(), |sum, handle| {
            sum.combine(handle.owners())
        }))
    }

    /// Remove an installed driver (every version) after shutting down its
    /// generations. Refuses with an error while owners remain; use
    /// [`Self::try_uninstall`] to get the owners as a typed result.
    pub fn uninstall(
        &self,
        id: &str,
        active_handle: Option<&crate::DriverHandle>,
    ) -> Result<Option<InstalledDriverRecord>, InstallError> {
        match self.try_uninstall(id, active_handle)? {
            UninstallOutcome::NotInstalled => Ok(None),
            UninstallOutcome::Removed(record) => Ok(Some(record)),
            UninstallOutcome::InUse(owners) => Err(LoaderError::from(owners).into()),
        }
    }

    pub fn try_uninstall(
        &self,
        id: &str,
        active_handle: Option<&crate::DriverHandle>,
    ) -> Result<UninstallOutcome, InstallError> {
        let Some(record) = self.store.record(id)? else {
            return Ok(UninstallOutcome::NotInstalled);
        };
        let loaded = self.loaded_handles(&record)?;
        let mut handles: Vec<&DriverHandle> = loaded.iter().map(Arc::as_ref).collect();
        if let Some(active) = active_handle {
            if !handles.iter().any(|handle| std::ptr::eq(*handle, active)) {
                handles.push(active);
            }
        }
        let owners = handles.iter().fold(DriverOwners::default(), |sum, handle| {
            sum.combine(handle.owners())
        });
        if !owners.is_empty() {
            return Ok(UninstallOutcome::InUse(owners));
        }
        for handle in handles {
            match handle.shutdown() {
                Ok(()) | Err(LoaderError::Shutdown) => {}
                Err(LoaderError::ActiveOwners { .. }) => {
                    // An owner appeared between the check and the shutdown.
                    return Ok(UninstallOutcome::InUse(handle.owners()));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let root = self.driver_root(id)?;
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        if !record.path.starts_with(&root) && record.path.exists() {
            std::fs::remove_file(&record.path)?;
        }
        self.store.remove_record(id)?;
        Ok(UninstallOutcome::Removed(record))
    }

    fn artifact_meta<'e>(
        &self,
        entry: &'e DriverManifestEntry,
    ) -> Result<&'e DriverArtifact, InstallError> {
        let target = self.verifier.target();
        entry.artifact_for_target(target).ok_or_else(|| {
            DriverVerificationError::ArchUnsupported(format!("no build for {target}")).into()
        })
    }

    fn driver_root(&self, id: &str) -> Result<PathBuf, InstallError> {
        crate::verifier::validate_driver_id(id)?;
        Ok(self
            .store
            .base_directory()
            .join("Drivers")
            .join("v2")
            .join(id))
    }

    fn version_directories(&self, id: &str) -> Result<Vec<PathBuf>, InstallError> {
        let entries = match std::fs::read_dir(self.driver_root(id)?) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut directories = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
            }
        }
        Ok(directories)
    }

    /// Live generations of every version of the record (current, previous and
    /// any other version directory).
    fn loaded_handles(
        &self,
        record: &InstalledDriverRecord,
    ) -> Result<Vec<Arc<DriverHandle>>, InstallError> {
        let mut paths = vec![record.path.clone()];
        paths.extend(record.previous_path.clone());
        paths.extend(
            self.version_directories(&record.id)?
                .into_iter()
                .map(|directory| {
                    directory.join(self.verifier.target().library_file_name(&record.id))
                }),
        );
        paths.sort();
        paths.dedup();
        Ok(paths
            .iter()
            .filter_map(|path| DriverLoader.loaded_handle(path))
            .collect())
    }

    fn install_checked(
        &self,
        entry: &DriverManifestEntry,
        artifact: &Path,
        artifact_sha256: String,
        size_bytes: u64,
        progress: ThrottledProgress<'_>,
    ) -> Result<InstalledDriverRecord, InstallError> {
        let (prepared, cleanup) = self.prepare_artifact(artifact)?;
        let sha256 = if prepared == artifact {
            Ok(artifact_sha256)
        } else {
            crate::verifier::sha256_file(&prepared)
        };
        let result = sha256.map_err(InstallError::from).and_then(|sha256| {
            self.install_prepared(entry, &prepared, sha256, size_bytes, progress)
        });
        if let Some(directory) = cleanup {
            let _ = std::fs::remove_dir_all(directory);
        }
        result
    }

    fn install_prepared(
        &self,
        entry: &DriverManifestEntry,
        artifact: &Path,
        installed_sha256: String,
        size_bytes: u64,
        mut progress: ThrottledProgress<'_>,
    ) -> Result<InstalledDriverRecord, InstallError> {
        progress.report(
            InstallStep::VerifyingSignature,
            size_bytes,
            size_bytes,
            Instant::now(),
        );
        self.verifier.verify_architecture(artifact)?;
        if self.verify_signature {
            let signers = SignerConfig {
                macos_team_id: entry.team_id.clone(),
                windows_publisher: self.windows_publisher.clone(),
                manifest_attested: true,
            };
            self.verifier
                .verify_native_signature(artifact, &signers, self.policy)?;
        }
        progress.report(
            InstallStep::VerifyingAbi,
            size_bytes,
            size_bytes,
            Instant::now(),
        );
        self.verifier.probe_abi(artifact, entry.app_abi_version)?;

        let destination = self.destination_for(entry, &installed_sha256)?;
        self.stage_and_rename(artifact, &destination)?;

        let record = InstalledDriverRecord {
            id: entry.id.clone(),
            version: entry.version.clone(),
            path: destination,
            sha256: installed_sha256,
            team_id: entry.team_id.clone(),
            installed_at: iso8601_now(),
            app_abi_version: entry.app_abi_version,
            uri_schemes: entry.uri_schemes.clone(),
            license: entry.license.clone(),
            source_repo: entry.source_repo.clone(),
            ..Default::default()
        };
        self.store.save_installed(record.clone())?;
        // The stored record carries previous_* computed under the store lock.
        Ok(self.store.record(&record.id)?.unwrap_or(record))
    }

    /// `Drivers/v2/<id>/<version>/…`. A version directory is never overwritten
    /// with different bytes (a live generation may map it): when the same
    /// version is reinstalled with other content the directory gets a short
    /// digest suffix instead.
    fn destination_for(
        &self,
        entry: &DriverManifestEntry,
        sha256: &str,
    ) -> Result<PathBuf, InstallError> {
        let base = self.store.base_directory();
        let preferred = self.verifier.install_destination(entry, base)?;
        let differs = match crate::verifier::sha256_file(&preferred) {
            Ok(existing) => existing != sha256,
            Err(DriverVerificationError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                false
            }
            Err(error) => return Err(error.into()),
        };
        if !differs {
            return Ok(preferred);
        }
        let suffixed = format!("{}-{}", entry.version, &sha256[..8]);
        Ok(crate::verifier::versioned_artifact_path(
            base,
            self.verifier.target(),
            &entry.id,
            &suffixed,
        )?)
    }

    fn prepare_artifact(
        &self,
        artifact: &Path,
    ) -> Result<(PathBuf, Option<PathBuf>), InstallError> {
        if artifact
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("zip")
        {
            return Ok((artifact.to_path_buf(), None));
        }

        // TODO(cross-platform): unpack zips with a Rust zip reader off macOS;
        // until then Linux and Windows artifacts are published as bare libraries.
        if self.verifier.target().os != TargetOs::Macos {
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "zip artifacts are only unpacked on macOS; {} artifacts must be bare libraries",
                    self.verifier.target()
                ),
            )));
        }
        let directory = std::env::temp_dir().join(format!(
            "tablex-driver-unpack-{}-{}",
            std::process::id(),
            STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory)?;
        let status = std::process::Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(artifact)
            .arg(&directory)
            .status();
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(InstallError::Io(error));
            }
        };
        if !status.success() {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("ditto exited with status {status}"),
            )));
        }
        let dylib = find_library(&directory, self.verifier.target().os.library_extension())
            .ok_or_else(|| {
                let _ = std::fs::remove_dir_all(&directory);
                InstallError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "driver archive did not contain a driver library",
                ))
            })?;
        Ok((dylib, Some(directory)))
    }

    fn stage_and_rename(&self, artifact: &Path, destination: &Path) -> Result<(), InstallError> {
        let directory = destination.parent().ok_or_else(|| {
            DriverVerificationError::InvalidDriverId(destination.display().to_string())
        })?;
        std::fs::create_dir_all(directory)?;
        let file_name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                DriverVerificationError::InvalidDriverId(destination.display().to_string())
            })?;
        let sequence = STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let staging = directory.join(format!(
            ".{file_name}.{}-{sequence}.staging",
            std::process::id()
        ));
        let result = (|| {
            std::fs::copy(artifact, &staging)?;
            crate::os_fs::sync_file(&staging)?;
            std::fs::rename(&staging, destination)?;
            crate::os_fs::sync_directory(directory)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        result
    }
}

fn find_library(directory: &Path, extension: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(directory).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(library) = find_library(&path, extension) {
                return Some(library);
            }
        } else if path.extension().and_then(|found| found.to_str()) == Some(extension) {
            return Some(path);
        }
    }
    None
}

impl From<crate::StoreError> for InstallError {
    fn from(error: crate::StoreError) -> Self {
        match error {
            crate::StoreError::Io(io) => InstallError::Io(io),
            other => InstallError::Store(other),
        }
    }
}

pub(crate) fn iso8601_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // Package metadata only needs whole-second UTC timestamps after 1970.
    let seconds = now.as_secs() as i64;
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

pub(crate) fn civil_from_unix(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;
    (year, month, day, hour, minute, second)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's civil_from_days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 - doe / 36524 + doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

use thiserror::Error;

// Used by tests to locate fixture artifacts.
#[allow(dead_code)]
pub(crate) fn debug_paths(base: &Path) -> PathBuf {
    base.to_path_buf()
}
