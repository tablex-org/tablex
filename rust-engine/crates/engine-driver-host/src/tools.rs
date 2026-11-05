//! Standalone tools (today: the `tablex-mcp` server) that the Driver Manager installs next to the
//! drivers. A tool is one executable, installed at a STABLE path
//! `<root>/<id>/<id>[.exe]` (an MCP client keeps that path in its configuration, so it survives
//! updates), with `installed.json` beside it (version, SHA-256, size, time).
//!
//! * Download with the drivers' machinery: [`stream_to_file`] (progress, cancel, exact size cap,
//!   SHA-256 while streaming) and the drivers' URL policy ([`classify_download_url`]: HTTPS only,
//!   `file://` only under [`TrustPolicy::Developer`]). The caller supplies only the byte source,
//!   because the host has no HTTP client.
//! * The download is written to a temp file INSIDE the tool directory (same volume), checked
//!   against the signed manifest's hash and size, and then renamed over the old executable. A
//!   crash at any point leaves the old or the new executable, never a partial one: the rename is
//!   the only step that touches the stable path. The downloaded file is never executed.
//! * Nothing here follows a link out of the tools directory: a link at the tools root, at the
//!   tool's directory or at the executable / record is refused, and the temp files are created
//!   exclusively without following links (see `os_fs`).
//!
//! Relied on: POSIX `rename(2)` replaces atomically (a running copy keeps its old inode). On
//! Windows `std::fs::rename` is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which fails while the
//! executable is running; that is [`ToolError::InUse`], never a half-installed state (the old file
//! is untouched, the temp files are removed).

use crate::download::{classify_download_url, stream_to_file, DownloadError, DownloadTransport};
use crate::manifest::{
    DriverRequirement, ManifestTool, RequirementStatus, ToolManifestEntry, ToolKind,
};
use crate::store::{InstalledDriverStore, StoreError};
use crate::os_fs;
use crate::policy::TrustPolicy;
use crate::progress::{InstallStep, ProgressSink, ThrottledProgress};
use crate::{DriverArtifact, DriverTarget, TargetOs};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

const RECORD_FILE: &str = "installed.json";
const STAGING_SUFFIX: &str = ".staging";
/// `installed.json` is a few hundred bytes; anything bigger is not ours.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// A temp file older than this that no install of this process is writing is a crash leftover.
const STALE_STAGING_AGE: Duration = Duration::from_secs(60 * 60);
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(1);
/// Temp files the installs of this process are writing right now: no sweep touches them.
static ACTIVE_STAGING: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
/// What identifies a file's content for the digest memory: the metadata of the OPENED handle (so
/// a path swapped after the check cannot borrow another file's digest), and on Unix the inode and
/// the change time too, which a restored mtime cannot fake.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, i64, i64, i64),
}

impl FileStamp {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            inode: (
                meta.ino(),
                meta.ctime(),
                meta.ctime_nsec(),
                meta.mtime_nsec(),
            ),
        }
    }
}

/// `(stamp)` of an executable that was hashed, and the digest: a catalog refresh does not
/// read a 7 MB file again while it is unchanged.
type HashMemory = HashMap<PathBuf, (FileStamp, String)>;
static HASHED: Mutex<Option<HashMemory>> = Mutex::new(None);

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("'{0}' is not a valid tool id")]
    InvalidId(String),
    #[error("no build of {tool} for {target}")]
    NoBuild { tool: String, target: String },
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error("SHA-256 mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("{} is a link; refusing to follow it", .path.display())]
    LinkRefused { path: PathBuf },
    #[error("{} is not the kind of file or folder it should be", .path.display())]
    UnexpectedEntry { path: PathBuf },
    /// Windows cannot replace or delete a running executable (sharing / lock violation).
    #[error("{} is in use. Close the MCP client that is using it, then try again", .path.display())]
    InUse { path: PathBuf },
    /// Windows "access denied": a read-only file, its permissions, or a program holding it.
    #[error("access to {} was denied. Check that it is not read-only and that you may change it, then try again", .path.display())]
    AccessDenied { path: PathBuf },
    /// A tools folder owned by someone else could be changed under the install.
    #[error("{} belongs to another user; refusing to use it", .path.display())]
    ForeignOwner { path: PathBuf },
    /// A tool folder other users can write to could be changed under the install.
    #[error("{} can be written by other users; refusing to use it", .path.display())]
    SharedWritable { path: PathBuf },
    /// An add-on whose driver is missing, disabled or too old.
    #[error("{}", requirement_message(.tool, .requirement, .status))]
    RequiresDriver {
        tool: String,
        requirement: DriverRequirement,
        status: RequirementStatus,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("installed.json is unusable: {0}")]
    Record(String),
    #[error("tool I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// "MongoDB Shell needs the mongodb driver (not installed)" / "(2.0.3 is installed, needs 2.1.0 or newer)".
pub fn requirement_message(
    tool: &str,
    requirement: &DriverRequirement,
    status: &RequirementStatus,
) -> String {
    match status {
        RequirementStatus::Met => format!("{tool} needs the {} driver", requirement.driver),
        RequirementStatus::Missing => format!(
            "{tool} needs the {} driver ({} or newer), which is not installed",
            requirement.driver, requirement.min_version
        ),
        RequirementStatus::TooOld { installed } => format!(
            "{tool} needs the {} driver {} or newer ({installed} is installed)",
            requirement.driver, requirement.min_version
        ),
    }
}

impl ToolError {
    /// The install was stopped by the user (the cancel flag), not by a failure.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Download(DownloadError::Cancelled))
    }
}

/// What `installed.json` says about an installed tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledToolRecord {
    pub id: String,
    pub version: String,
    /// SHA-256 of the installed executable (the artifact's hash).
    pub sha256: String,
    pub size_bytes: u64,
    pub installed_at: String,
    /// What the add-on needed when it was installed, so an add-on whose manifest entry is gone
    /// still says which driver it needs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<DriverRequirement>,
}

/// The tools directory (`<app data dir>/tools`) and the paths inside it.
#[derive(Debug, Clone)]
pub struct ToolStore {
    root: PathBuf,
}

impl ToolStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<id>`.
    pub fn tool_directory(&self, id: &str) -> Result<PathBuf, ToolError> {
        crate::verifier::validate_driver_id(id)
            .map_err(|_| ToolError::InvalidId(id.to_string()))?;
        Ok(self.root.join(id))
    }

    /// The stable path of the executable: `<root>/<id>/<id>` (`.exe` on Windows).
    pub fn executable_path(&self, id: &str) -> Result<PathBuf, ToolError> {
        Ok(self
            .tool_directory(id)?
            .join(executable_name(id, DriverTarget::current().os)))
    }

    fn record_path(&self, id: &str) -> Result<PathBuf, ToolError> {
        Ok(self.tool_directory(id)?.join(RECORD_FILE))
    }

    /// The installed tool, if `installed.json` and the executable agree: same size and the same
    /// SHA-256 (the digest is remembered for an unchanged file, so a refresh does not read it
    /// again). A missing record or executable, or a half-finished replacement, reads as "not
    /// installed", so a reinstall repairs it. A link anywhere on the way, a record or executable
    /// that is not a regular file, or an unreadable record, is an error.
    pub fn installed(&self, id: &str) -> Result<Option<InstalledToolRecord>, ToolError> {
        if self.existing_directory(id)?.is_none() {
            return Ok(None);
        }
        let Some(record) = self.read_record(id)? else {
            return Ok(None);
        };
        let executable = self.executable_path(id)?;
        match inspect(&executable)? {
            Some(meta) if meta.is_file() && meta.len() == record.size_bytes => {
                Ok(digest_matches(&executable, &record.sha256)?.then_some(record))
            }
            Some(meta) if !meta.is_file() => Err(ToolError::UnexpectedEntry { path: executable }),
            _ => Ok(None),
        }
    }

    fn read_record(&self, id: &str) -> Result<Option<InstalledToolRecord>, ToolError> {
        let path = self.record_path(id)?;
        match inspect(&path)? {
            None => return Ok(None),
            Some(meta) if !meta.is_file() => return Err(ToolError::UnexpectedEntry { path }),
            Some(_) => {}
        }
        let mut bytes = Vec::new();
        match os_fs::open_regular_read(&path) {
            Ok(file) => file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
                return Err(ToolError::UnexpectedEntry { path })
            }
            Err(error) => return Err(error.into()),
        };
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(ToolError::Record("file is too large".into()));
        }
        let record: InstalledToolRecord =
            serde_json::from_slice(&bytes).map_err(|error| ToolError::Record(error.to_string()))?;
        if record.id != id {
            return Err(ToolError::Record(format!("it names '{}'", record.id)));
        }
        Ok(Some(record))
    }

    /// `<root>/<id>` when it exists as a real folder below a real tools folder; `None` when either
    /// is missing; an error for a link, a file or a folder other users can write to.
    fn existing_directory(&self, id: &str) -> Result<Option<PathBuf>, ToolError> {
        let directory = self.tool_directory(id)?;
        for path in [self.root.as_path(), directory.as_path()] {
            match inspect(path)? {
                None => return Ok(None),
                Some(meta) => check_folder(path, &meta)?,
            }
        }
        Ok(Some(directory))
    }

    /// Makes `<root>` and `<root>/<id>` exist, each level created by itself with mode 0755 on
    /// Unix and looked at again afterwards (a link planted between the check and the create is
    /// refused); links, non-directories and folders other users can write to are refused.
    /// Returns the folder and whether this call created it.
    fn prepare_directory(&self, id: &str) -> Result<(PathBuf, bool), ToolError> {
        let directory = self.tool_directory(id)?;
        let mut created = false;
        for path in [self.root.as_path(), directory.as_path()] {
            match inspect(path)? {
                Some(meta) => {
                    check_folder(path, &meta)?;
                    created = false;
                }
                None => {
                    create_missing_ancestors(path)?;
                    make_folder(path)?;
                    created = true;
                }
            }
        }
        Ok((directory, created))
    }
}

/// A real folder, not writable by group or others.
fn check_folder(path: &Path, meta: &fs::Metadata) -> Result<(), ToolError> {
    if !meta.is_dir() {
        return Err(ToolError::UnexpectedEntry {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.permissions().mode() & 0o022 != 0 {
            return Err(ToolError::SharedWritable {
                path: path.to_path_buf(),
            });
        }
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(ToolError::ForeignOwner {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn folder_builder() -> fs::DirBuilder {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o755);
    }
    builder
}

/// The folders above `path` that do not exist yet (the user's own data directory on a fresh
/// machine), top down, one `mkdir` each.
fn create_missing_ancestors(path: &Path) -> Result<(), ToolError> {
    let mut missing: Vec<&Path> = path
        .ancestors()
        .skip(1)
        .take_while(|ancestor| !ancestor.as_os_str().is_empty() && !ancestor.exists())
        .collect();
    missing.reverse();
    for ancestor in missing {
        match folder_builder().create(ancestor) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// `mkdir path` (0755), then look at what is there: a link that won the race is refused.
fn make_folder(path: &Path) -> Result<(), ToolError> {
    match folder_builder().create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    match inspect(path)? {
        Some(meta) => check_folder(path, &meta),
        None => Err(ToolError::UnexpectedEntry {
            path: path.to_path_buf(),
        }),
    }
}

/// Whether `executable` hashes to `expected`. The file is opened once; its handle's stamp is the
/// key of the digest memory, and the bytes hashed are the ones of that handle.
fn digest_matches(executable: &Path, expected: &str) -> Result<bool, ToolError> {
    let mut file = match os_fs::open_regular_read(executable) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            return Err(ToolError::UnexpectedEntry {
                path: executable.to_path_buf(),
            })
        }
        Err(error) => return Err(error.into()),
    };
    let stamp = FileStamp::of(&file.metadata()?);
    if let Some(memory) = HASHED.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if let Some((remembered, digest)) = memory.get(executable) {
            if *remembered == stamp {
                return Ok(digest.eq_ignore_ascii_case(expected));
            }
        }
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = crate::download::hex(&hasher.finalize());
    let matches = digest.eq_ignore_ascii_case(expected);
    remember_digest(executable, stamp, digest);
    Ok(matches)
}

fn remember_digest(executable: &Path, stamp: FileStamp, digest: String) {
    HASHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(executable.to_path_buf(), (stamp, digest));
}

/// `tablex-mcp` or `tablex-mcp.exe`.
fn executable_name(id: &str, os: TargetOs) -> String {
    match os {
        TargetOs::Windows => format!("{id}.exe"),
        _ => id.to_string(),
    }
}

/// `symlink_metadata` that refuses links: `None` when absent.
fn inspect(path: &Path) -> Result<Option<fs::Metadata>, ToolError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if os_fs::is_link(&meta) => Err(ToolError::LinkRefused {
            path: path.to_path_buf(),
        }),
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Windows error codes of "the file is in use": sharing and lock violations.
fn in_use_code(code: Option<i32>) -> bool {
    matches!(code, Some(32 | 33))
}

/// Windows "access denied": not necessarily a running process (read-only attribute, ACL).
fn access_denied_code(code: Option<i32>) -> bool {
    code == Some(5)
}

/// Removes `.*.staging` files in `directory` that no install of this process is writing: all of
/// them when `older_than` is `None`, else only those not modified for that long (another process
/// may be downloading right now). Returns whether any was removed.
fn sweep_staging(directory: &Path, older_than: Option<Duration>) -> Result<bool, ToolError> {
    let active = ACTIVE_STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    let mut removed = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with('.') && name.ends_with(STAGING_SUFFIX)) {
            continue;
        }
        let path = entry.path();
        let meta = entry.metadata()?;
        if active.contains(&path) || !meta.is_file() || os_fs::is_link(&meta) {
            continue;
        }
        if let Some(age) = older_than {
            let stale = meta
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|elapsed| elapsed >= age);
            if !stale {
                continue;
            }
        }
        fs::remove_file(&path)?;
        removed = true;
    }
    Ok(removed)
}

/// A replace / delete that failed (Windows only; on Unix replacing or unlinking a running
/// executable works): "in use" for a sharing or lock violation, a neutral "access denied" for
/// error 5, which a read-only file or an ACL causes as well.
fn replace_failure(error: std::io::Error, path: &Path) -> ToolError {
    let path = path.to_path_buf();
    match error.raw_os_error() {
        code if cfg!(windows) && in_use_code(code) => ToolError::InUse { path },
        code if cfg!(windows) && access_denied_code(code) => ToolError::AccessDenied { path },
        _ => error.into(),
    }
}

/// Removes the temp files of an install that did not finish, and the tool's folder when this
/// install created it and nothing else is in it.
struct Cleanup {
    files: Vec<PathBuf>,
    created_folder: Option<PathBuf>,
}

impl Cleanup {
    /// Registers the temp files as being written by this process, so no sweep removes them.
    fn new(files: Vec<PathBuf>, created_folder: Option<PathBuf>) -> Self {
        ACTIVE_STAGING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashSet::new)
            .extend(files.iter().cloned());
        Self {
            files,
            created_folder,
        }
    }

    /// The install is done: the folder stays.
    fn committed(&mut self) {
        self.created_folder = None;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(active) = ACTIVE_STAGING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            for path in &self.files {
                active.remove(path);
            }
        }
        for path in &self.files {
            let _ = fs::remove_file(path);
        }
        if let Some(folder) = &self.created_folder {
            let _ = fs::remove_dir(folder);
        }
    }
}

pub struct ToolInstaller<'a> {
    store: &'a ToolStore,
    target: DriverTarget,
    policy: TrustPolicy,
    drivers: Option<&'a InstalledDriverStore>,
}

impl<'a> ToolInstaller<'a> {
    pub fn new(target: DriverTarget, store: &'a ToolStore) -> Self {
        Self {
            store,
            target,
            policy: TrustPolicy::Strict,
            drivers: None,
        }
    }

    /// The installed drivers an add-on's `requires` is checked against. Without it an add-on
    /// cannot be installed (its requirement can not be shown to hold).
    pub fn with_drivers(mut self, drivers: &'a InstalledDriverStore) -> Self {
        self.drivers = Some(drivers);
        self
    }

    /// `Ok` when every driver `entry` requires is installed, enabled and recent enough.
    pub fn check_requirements(&self, entry: &ToolManifestEntry) -> Result<(), ToolError> {
        if entry.requires.is_empty() {
            return Ok(());
        }
        let records = match self.drivers {
            Some(drivers) => drivers.load_all()?,
            None => Default::default(),
        };
        for requirement in &entry.requires {
            let installed = records
                .get(&requirement.driver)
                .filter(|record| !record.disabled)
                .map(|record| record.version.as_str());
            match requirement.check(installed) {
                RequirementStatus::Met => {}
                status => {
                    return Err(ToolError::RequiresDriver {
                        tool: entry.display_name.clone(),
                        requirement: requirement.clone(),
                        status,
                    })
                }
            }
        }
        Ok(())
    }

    /// Under [`TrustPolicy::Developer`] a `file://` artifact URL is accepted.
    pub fn with_trust_policy(mut self, policy: TrustPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Download, verify and install `entry`. `open` returns the byte source of the build selected
    /// for this target, after the URL passed the policy (the caller owns the HTTP client).
    /// Reports `Downloading`, then `VerifyingChecksum`. Nothing is changed on failure or cancel;
    /// the previous executable (if any) keeps working until the final rename.
    pub fn install<O>(
        &self,
        entry: &ToolManifestEntry,
        open: O,
        cancelled: &AtomicBool,
        sink: &dyn ProgressSink,
    ) -> Result<InstalledToolRecord, ToolError>
    where
        O: FnOnce(&DriverArtifact, DownloadTransport) -> Result<Box<dyn Read>, DownloadError>,
    {
        self.check_requirements(entry)?;
        let artifact =
            entry
                .artifact_for_target(&self.target)
                .ok_or_else(|| ToolError::NoBuild {
                    tool: entry.id.clone(),
                    target: self.target.to_string(),
                })?;
        let transport = classify_download_url(&artifact.url, self.policy)?;
        let (directory, created) = self.store.prepare_directory(&entry.id)?;
        let executable = self.store.executable_path(&entry.id)?;
        let record_path = self.store.record_path(&entry.id)?;
        for existing in [&executable, &record_path] {
            if inspect(existing)?.is_some_and(|meta| !meta.is_file()) {
                return Err(ToolError::UnexpectedEntry {
                    path: existing.clone(),
                });
            }
        }

        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let staging_path = |name: &str| {
            directory.join(format!(
                ".{name}.{}-{sequence}{STAGING_SUFFIX}",
                std::process::id()
            ))
        };
        let staged_executable = staging_path("download");
        let staged_record = staging_path(RECORD_FILE);
        let mut cleanup = Cleanup::new(
            vec![staged_executable.clone(), staged_record.clone()],
            created.then(|| directory.clone()),
        );
        // Leftovers of a crashed install (up to one binary each), if no one is writing them.
        let _ = sweep_staging(&directory, Some(STALE_STAGING_AGE));

        let mut source = open(artifact, transport)?;
        let downloaded = stream_to_file(
            &mut *source,
            &staged_executable,
            artifact.size_bytes,
            cancelled,
            sink,
        )?;
        drop(source);

        ThrottledProgress::new(sink).report(
            InstallStep::VerifyingChecksum,
            downloaded.size_bytes,
            downloaded.size_bytes,
            Instant::now(),
        );
        let expected = artifact.sha256.to_ascii_lowercase();
        if downloaded.sha256 != expected {
            return Err(ToolError::ChecksumMismatch {
                expected,
                actual: downloaded.sha256,
            });
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(DownloadError::Cancelled.into());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged_executable, fs::Permissions::from_mode(0o755))?;
        }
        let record = InstalledToolRecord {
            id: entry.id.clone(),
            version: entry.version.clone(),
            sha256: downloaded.sha256,
            size_bytes: downloaded.size_bytes,
            installed_at: crate::installer::iso8601_now(),
            requires: entry.requires.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&record)
            .map_err(|error| ToolError::Record(error.to_string()))?;
        bytes.push(b'\n');
        let mut file = os_fs::open_private(
            OpenOptions::new().create_new(true).write(true),
            &staged_record,
        )?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);

        // The only steps that touch the stable path: the executable first, then its record. A
        // crash between the two leaves a record whose size no longer matches the file, which
        // `installed` reads as "not installed" (a reinstall repairs it).
        // Held across the rename: the handle follows the inode, so its stamp afterwards (the rename
        // changes the change time) is the stamp of the file that was hashed and installed.
        let staged_handle = os_fs::open_regular_read(&staged_executable).ok();
        fs::rename(&staged_executable, &executable)
            .map_err(|error| replace_failure(error, &executable))?;
        fs::rename(&staged_record, &record_path)
            .map_err(|error| replace_failure(error, &record_path))?;
        cleanup.committed();
        // The digest of the file just installed is known: `installed` need not read it again.
        if let Some(meta) = staged_handle.and_then(|handle| handle.metadata().ok()) {
            remember_digest(&executable, FileStamp::of(&meta), record.sha256.clone());
        }
        // Durability only: both renames already took effect.
        let _ = os_fs::sync_directory(&directory);
        Ok(record)
    }

    /// Removes exactly what an install writes: the executable, `installed.json` and leftover temp
    /// files, then the tool's folder if that leaves it empty. Returns whether anything was
    /// removed. A link at the folder, the executable or the record is refused before anything is
    /// deleted.
    pub fn uninstall(&self, id: &str) -> Result<bool, ToolError> {
        let Some(directory) = self.store.existing_directory(id)? else {
            return Ok(false);
        };
        let executable = self.store.executable_path(id)?;
        let record = self.store.record_path(id)?;
        // Check both before deleting either, so a refusal changes nothing.
        let mut present = Vec::with_capacity(2);
        for path in [&executable, &record] {
            match inspect(path)? {
                Some(meta) if meta.is_file() => present.push(path),
                Some(_) => return Err(ToolError::UnexpectedEntry { path: path.clone() }),
                None => {}
            }
        }
        let mut removed = !present.is_empty();
        for path in present {
            fs::remove_file(path).map_err(|error| replace_failure(error, path))?;
        }
        removed |= sweep_staging(&directory, None)?;
        // Only an empty folder goes; anything else in it is not ours.
        let _ = fs::remove_dir(&directory);
        Ok(removed)
    }
}

// --- catalog ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCatalogState {
    Available,
    Installed {
        version: String,
    },
    UpdateAvailable {
        installed: String,
        latest: String,
    },
    /// Cannot be installed (no build for this platform, needs a newer app, bad entry).
    Unavailable {
        reason: String,
    },
}

/// One tool as the Driver Manager shows it: the manifest entry merged with what is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCatalogItem {
    pub id: String,
    pub kind: ToolKind,
    pub display_name: String,
    /// The version the manifest offers (empty when the manifest does not list the tool).
    pub version: String,
    pub description: Option<String>,
    pub state: ToolCatalogState,
    /// Installable entry; `None` when the manifest lacks the tool or its entry is invalid.
    pub entry: Option<ToolManifestEntry>,
    pub installed: Option<InstalledToolRecord>,
    /// The stable executable path (also when not installed yet).
    pub path: PathBuf,
    /// Something the user should know: an unusable manifest entry or an unreadable record.
    pub problem: Option<String>,
}

impl ToolCatalogItem {
    /// The drivers this tool is an add-on of: what the manifest says now, else what the install
    /// recorded (an add-on the manifest no longer lists).
    pub fn requires(&self) -> &[DriverRequirement] {
        match (&self.entry, &self.installed) {
            (Some(entry), _) if !entry.requires.is_empty() => &entry.requires,
            (_, Some(record)) => &record.requires,
            _ => &[],
        }
    }

    /// Merge `tool` (from the manifest, `None` when the manifest does not list it) with the
    /// installed state in `store`; `None` when there is nothing to show (not listed, not
    /// installed). `unmet_app` is the minimum app version the running app lacks.
    pub(crate) fn resolve(
        kind: ToolKind,
        tool: Option<&ManifestTool>,
        store: &ToolStore,
        target: &DriverTarget,
        unmet_app: impl Fn(&ToolManifestEntry) -> Option<String>,
    ) -> Result<Option<Self>, ToolError> {
        let id = kind.tool_id();
        let path = store.executable_path(id)?;
        let (installed, mut problem) = match store.installed(id) {
            Ok(installed) => (installed, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let entry = match tool {
            Some(ManifestTool::Known(entry)) => Some(entry.clone()),
            Some(ManifestTool::Invalid { error, .. }) => {
                problem = Some(error.to_string());
                None
            }
            Some(ManifestTool::Ignored(_)) | None => None,
        };
        if entry.is_none()
            && installed.is_none()
            && !matches!(tool, Some(ManifestTool::Invalid { .. }))
        {
            return Ok(None);
        }
        let installed_version = installed.as_ref().map(|record| record.version.clone());
        let unavailable = match (tool, &entry) {
            (Some(ManifestTool::Invalid { error, .. }), _) => Some(error.to_string()),
            (_, Some(entry)) => unmet_app(entry)
                .map(|minimum| format!("Needs TableX \u{2265} {minimum}"))
                .or_else(|| {
                    entry
                        .artifact_for_target(target)
                        .is_none()
                        .then(|| format!("No build for {target}"))
                }),
            _ => None,
        };
        let state = match (&installed_version, &entry, unavailable) {
            (Some(installed), Some(entry), None) if *installed != entry.version => {
                ToolCatalogState::UpdateAvailable {
                    installed: installed.clone(),
                    latest: entry.version.clone(),
                }
            }
            (Some(installed), _, _) => ToolCatalogState::Installed {
                version: installed.clone(),
            },
            (None, _, Some(reason)) => ToolCatalogState::Unavailable { reason },
            (None, _, None) => ToolCatalogState::Available,
        };
        Ok(Some(Self {
            id: id.to_string(),
            kind,
            display_name: entry
                .as_ref()
                .map_or_else(|| id.to_string(), |entry| entry.display_name.clone()),
            version: entry
                .as_ref()
                .map_or_else(String::new, |entry| entry.version.clone()),
            description: entry.as_ref().and_then(|entry| entry.description.clone()),
            state,
            entry,
            installed,
            path,
            problem,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_names_follow_the_target_os() {
        assert_eq!(executable_name("tablex-mcp", TargetOs::Macos), "tablex-mcp");
        assert_eq!(executable_name("tablex-mcp", TargetOs::Linux), "tablex-mcp");
        assert_eq!(
            executable_name("tablex-mcp", TargetOs::Windows),
            "tablex-mcp.exe"
        );
    }

    #[test]
    fn windows_sharing_violations_are_in_use_and_access_denied_is_worded_neutrally() {
        for code in [32, 33] {
            assert!(
                in_use_code(Some(code)) && !access_denied_code(Some(code)),
                "{code}"
            );
        }
        assert!(!in_use_code(Some(5)) && access_denied_code(Some(5)));
        assert!(!in_use_code(Some(2)) && !in_use_code(None) && !access_denied_code(None));
        let path = PathBuf::from("C:\\tools\\tablex-mcp.exe");
        let in_use = ToolError::InUse { path: path.clone() }.to_string();
        assert!(
            in_use.contains("Close the MCP client that is using it, then try again"),
            "{in_use}"
        );
        let denied = ToolError::AccessDenied { path }.to_string();
        assert!(
            denied.contains("access") && denied.contains("denied"),
            "{denied}"
        );
        assert!(
            !denied.contains("MCP client"),
            "error 5 does not mean a client holds it: {denied}"
        );
    }

    #[test]
    fn ids_cannot_leave_the_tools_directory() {
        let store = ToolStore::new("/nonexistent/tools");
        for id in ["", "..", "a/b", "a\\b", "../x", "with space"] {
            assert!(
                matches!(store.executable_path(id), Err(ToolError::InvalidId(_))),
                "{id:?}"
            );
        }
        assert!(store.executable_path("tablex-mcp").is_ok());
    }
}
