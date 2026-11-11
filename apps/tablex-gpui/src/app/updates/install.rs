//! Installing an update without any UI: the signed update manifest, the bundle the running app
//! lives in, the download, the checks of the unpacked bundle, the swap with its rollback and the
//! clean-up. The network sits behind [`UpdateSource`], so tests feed canned answers.
//!
//! Trust: `app-update.json` is signed (a detached base64 ed25519 signature in `app-update.json.sig`,
//! over the exact bytes) with the key that signs the driver catalog; the app verifies it with the
//! keys already pinned for that catalog (`crate::drivers::manifest_trust`) before it reads a field.
//! The manifest names the zip of the bundle by size and SHA-256. Nothing here removes or adds a
//! quarantine attribute, and nothing calls `spctl` or `xattr`: the zip never goes through a browser
//! and the bundle is checked by `codesign` and by its own Info.plist.

use super::check::Version;
use engine_driver_host::{DownloadError, DownloadedArtifact, ManifestTrust, ProgressSink};
use serde::Deserialize;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Where release assets are downloaded from (the release of tag `v<version>`).
const DOWNLOAD_BASE: &str = "https://github.com/tablex-org/tablex/releases/download";
pub(in crate::app) const MANIFEST_NAME: &str = "app-update.json";
/// The manifest is a few hundred bytes of JSON; the signature is 88 characters of base64.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 1024;
/// A zipped app is tens of MiB; the manifest cannot ask for more than this.
const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const SCHEMA: u64 = 1;
/// The `kind` every update manifest carries (a driver catalog never does), so that a signed document of
/// another kind cannot be taken for one.
pub(in crate::app) const KIND: &str = "tablex-app-update";
const DITTO: &str = "/usr/bin/ditto";
const CODESIGN: &str = "/usr/bin/codesign";
const PLUTIL: &str = "/usr/bin/plutil";
const OPEN: &str = "/usr/bin/open";
/// The suffix of the previous bundle, kept beside the new one until the new version has launched.
const BACKUP_INFIX: &str = ".old-";
/// Prefix of the directory next to the bundle the new bundle is unpacked into.
const STAGING_PREFIX: &str = ".TableX-update-";

/// The target triple of this build, as the manifest names it; empty where there is no macOS build.
pub(in crate::app) const fn current_target() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        ""
    }
}

// --- the network ----------------------------------------------------------------------------

/// What an update needs from the network.
pub(in crate::app) trait UpdateSource: Send + Sync {
    /// The body at `url`, at most `max_bytes` long.
    fn fetch(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String>;
    /// Streams `url` into the new file `destination` (size capped at `size`, hashed on the way).
    fn download(
        &self,
        url: &str,
        destination: &Path,
        size: u64,
        cancel: &Arc<AtomicBool>,
        progress: &dyn ProgressSink,
    ) -> Result<DownloadedArtifact, DownloadError>;
}

/// The production source: the driver pipeline's HTTPS client (https only, every redirect hop checked,
/// idle timeout, cancellable).
pub(in crate::app) struct HttpSource;

impl UpdateSource for HttpSource {
    fn fetch(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        crate::drivers::fetch_bytes(url, max_bytes)
    }

    fn download(
        &self,
        url: &str,
        destination: &Path,
        size: u64,
        cancel: &Arc<AtomicBool>,
        progress: &dyn ProgressSink,
    ) -> Result<DownloadedArtifact, DownloadError> {
        crate::drivers::download_artifact(
            url,
            destination,
            size,
            engine_driver_host::TrustPolicy::Strict,
            cancel,
            progress,
        )
    }
}

/// Scenarios and tests never reach the network.
pub(in crate::app) struct OfflineSource;

impl UpdateSource for OfflineSource {
    fn fetch(&self, _: &str, _: u64) -> Result<Vec<u8>, String> {
        Err("offline".into())
    }

    fn download(
        &self,
        _: &str,
        _: &Path,
        _: u64,
        _: &Arc<AtomicBool>,
        _: &dyn ProgressSink,
    ) -> Result<DownloadedArtifact, DownloadError> {
        Err(DownloadError::Io(io::Error::other("offline")))
    }
}

// --- the manifest ---------------------------------------------------------------------------

/// The zip of a bundle, as the signed manifest describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct Artifact {
    pub(in crate::app) url: String,
    pub(in crate::app) size: u64,
    /// Lower-case hex.
    pub(in crate::app) sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::app) enum ManifestError {
    /// The manifest or its signature could not be fetched.
    Fetch(String),
    /// The signature is missing, malformed, or not by a pinned key.
    Signature(String),
    Malformed(String),
    Schema(u64),
    /// The manifest is signed, but for another version than the release it came with.
    VersionMismatch { manifest: String, release: String },
    /// Not newer than this build: never a downgrade, never a reinstall.
    NotNewer { manifest: String, current: String },
    NoArtifact(String),
    BadArtifact(&'static str),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch(error) => write!(f, "update manifest not available ({error})"),
            Self::Signature(error) => write!(f, "update manifest signature rejected ({error})"),
            Self::Malformed(error) => write!(f, "update manifest unreadable ({error})"),
            Self::Schema(schema) => write!(f, "update manifest schema {schema} is not supported"),
            Self::VersionMismatch { manifest, release } => {
                write!(f, "update manifest is for {manifest}, the release is {release}")
            }
            Self::NotNewer { manifest, current } => {
                write!(f, "update manifest {manifest} is not newer than {current}")
            }
            Self::NoArtifact(target) => write!(f, "update manifest has no build for {target}"),
            Self::BadArtifact(why) => write!(f, "update manifest artifact is invalid ({why})"),
        }
    }
}

/// `https://github.com/<repo>/releases/download/v<version>/app-update.json`.
pub(in crate::app) fn manifest_url(version: &Version) -> String {
    format!("{DOWNLOAD_BASE}/v{version}/{MANIFEST_NAME}")
}

/// Checks the signature first, then reads `body`: the schema, the version (the release's, and newer
/// than `current`) and the artifact of `target`.
pub(in crate::app) fn verify_manifest(
    trust: &ManifestTrust,
    body: &[u8],
    signature: &[u8],
    release: &Version,
    current: &Version,
    target: &str,
) -> Result<Artifact, ManifestError> {
    #[derive(Deserialize)]
    struct Wire {
        kind: String,
        schema: u64,
        version: String,
        #[serde(default)]
        artifacts: Vec<WireArtifact>,
    }
    #[derive(Deserialize)]
    struct WireArtifact {
        target: String,
        url: String,
        size: u64,
        sha256: String,
    }
    trust
        .verify_signature(body, Some(signature))
        .map_err(|error| ManifestError::Signature(error.to_string()))?;
    let wire: Wire =
        serde_json::from_slice(body).map_err(|error| ManifestError::Malformed(error.to_string()))?;
    if wire.kind != KIND {
        return Err(ManifestError::Malformed(format!("kind {:?}", wire.kind)));
    }
    if wire.schema != SCHEMA {
        return Err(ManifestError::Schema(wire.schema));
    }
    let version = Version::parse(&wire.version)
        .ok_or_else(|| ManifestError::Malformed(format!("version {:?}", wire.version)))?;
    if version != *release {
        return Err(ManifestError::VersionMismatch {
            manifest: version.to_string(),
            release: release.to_string(),
        });
    }
    if version <= *current {
        return Err(ManifestError::NotNewer {
            manifest: version.to_string(),
            current: current.to_string(),
        });
    }
    let artifact = wire
        .artifacts
        .into_iter()
        .find(|artifact| artifact.target == target)
        .ok_or_else(|| ManifestError::NoArtifact(target.to_owned()))?;
    let https = url::Url::parse(&artifact.url).is_ok_and(|url| url.scheme() == "https");
    if !https {
        return Err(ManifestError::BadArtifact("the address is not https"));
    }
    if artifact.size == 0 || artifact.size > MAX_ARTIFACT_BYTES {
        return Err(ManifestError::BadArtifact("the size"));
    }
    let hex = artifact.sha256.len() == 64 && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex {
        return Err(ManifestError::BadArtifact("the SHA-256"));
    }
    Ok(Artifact {
        url: artifact.url,
        size: artifact.size,
        sha256: artifact.sha256.to_ascii_lowercase(),
    })
}

/// Fetches `app-update.json` and its signature for `release` and verifies them.
pub(in crate::app) fn fetch_artifact(
    source: &dyn UpdateSource,
    trust: &ManifestTrust,
    release: &Version,
    current: &Version,
    target: &str,
) -> Result<Artifact, ManifestError> {
    let url = manifest_url(release);
    let body = source.fetch(&url, MAX_MANIFEST_BYTES).map_err(ManifestError::Fetch)?;
    let signature = source
        .fetch(&format!("{url}.sig"), MAX_SIGNATURE_BYTES)
        .map_err(ManifestError::Fetch)?;
    verify_manifest(trust, &body, &signature, release, current, target)
}

// --- the running bundle ---------------------------------------------------------------------

/// The `.app` the running executable lives in, in a folder this user can write to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct Bundle {
    pub(in crate::app) path: PathBuf,
}

impl Bundle {
    fn name(&self) -> &std::ffi::OsStr {
        self.path.file_name().unwrap_or_default()
    }

    fn parent(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("/"))
    }

    fn info_plist(&self) -> PathBuf {
        self.path.join("Contents/Info.plist")
    }
}

/// Why this run cannot replace itself: the notice offers Download instead.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::app) enum Unavailable {
    DebugBuild,
    /// A development run: the executable is not inside an `.app`.
    NotBundle,
    /// The folder holding the app (`/Applications` for a standard user, a read-only disk image,
    /// Gatekeeper's translocation) cannot be written.
    NotWritable(PathBuf),
}

impl Unavailable {
    /// What the notice says; none for a development run, which nobody needs told.
    pub(in crate::app) fn note(&self) -> Option<String> {
        match self {
            Self::DebugBuild | Self::NotBundle => None,
            // A disk image or a translocated launch (Gatekeeper's read-only copy) is not where the app lives.
            Self::NotWritable(folder)
                if folder.starts_with("/Volumes") || folder.components().any(|part| part.as_os_str() == "AppTranslocation") =>
            {
                Some("Move TableX to Applications to update it.".to_owned())
            }
            Self::NotWritable(folder) => Some(format!(
                "TableX can't update itself: {} is not writable.",
                folder.display()
            )),
        }
    }
}

/// The bundle of the executable `exe` (`<name>.app/Contents/MacOS/<exe>`), when an update may replace it.
pub(in crate::app) fn locate_bundle(exe: Option<&Path>, debug_build: bool) -> Result<Bundle, Unavailable> {
    if debug_build {
        return Err(Unavailable::DebugBuild);
    }
    let bundle = exe
        .filter(|exe| exe.parent().and_then(Path::file_name).is_some_and(|dir| dir == "MacOS"))
        .and_then(|exe| exe.parent()?.parent())
        .filter(|contents| contents.file_name().is_some_and(|dir| dir == "Contents"))
        .and_then(Path::parent)
        .filter(|bundle| bundle.extension().is_some_and(|extension| extension == "app"))
        .ok_or(Unavailable::NotBundle)?;
    let bundle = Bundle { path: bundle.to_path_buf() };
    if !directory_writable(bundle.parent()) {
        return Err(Unavailable::NotWritable(bundle.parent().to_path_buf()));
    }
    Ok(bundle)
}

#[cfg(unix)]
fn directory_writable(directory: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(directory.as_os_str().as_bytes())
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } == 0)
}

#[cfg(not(unix))]
fn directory_writable(_: &Path) -> bool {
    false
}

// --- the plan the notice acts on --------------------------------------------------------------

/// What an available release lets the notice offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum Plan {
    /// A verified artifact and a bundle that can be replaced: Install & Restart.
    Install { artifact: Artifact, bundle: Bundle },
    /// Download (the release page). `note` is shown to the user; `problem` only goes to the log.
    Download { note: Option<String>, problem: Option<String> },
}

/// Decides what to offer for `release` (blocking: fetches the manifest). The bundle is looked at
/// first, so a development run never touches the network.
pub(in crate::app) fn plan(
    source: &dyn UpdateSource,
    trust: Option<&ManifestTrust>,
    exe: Option<&Path>,
    debug_build: bool,
    release: &Version,
    current: &str,
    target: &str,
) -> Plan {
    let bundle = match locate_bundle(exe, debug_build) {
        Ok(bundle) => bundle,
        Err(unavailable) => return Plan::Download { note: unavailable.note(), problem: None },
    };
    let (Some(trust), Some(current)) = (trust, Version::parse(current)) else {
        return Plan::Download { note: None, problem: Some("this build cannot verify an update".into()) };
    };
    match fetch_artifact(source, trust, release, &current, target) {
        Ok(artifact) => Plan::Install { artifact, bundle },
        Err(error) => Plan::Download { note: None, problem: Some(error.to_string()) },
    }
}

// --- download and unpack --------------------------------------------------------------------

#[derive(Debug)]
pub(in crate::app) enum InstallError {
    Cancelled,
    Download(String),
    /// The download is not the file the signed manifest describes.
    Checksum(String),
    Unpack(String),
    /// The unpacked bundle is not this app's next version.
    Bundle(String),
    Swap(String),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Download(error) => write!(f, "download failed ({error})"),
            Self::Checksum(error) => write!(f, "the download is not the signed file ({error})"),
            Self::Unpack(error) => write!(f, "could not unpack the update ({error})"),
            Self::Bundle(error) => write!(f, "the update is not valid ({error})"),
            Self::Swap(error) => write!(f, "could not replace the app ({error})"),
        }
    }
}

/// Where an install is, for the notice. Written by the installing thread, read by the UI.
#[derive(Default)]
pub(in crate::app) struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    stage: AtomicU8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Stage {
    Downloading,
    Verifying,
    Installing,
}

impl Progress {
    fn enter(&self, stage: Stage) {
        self.stage.store(stage as u8, Ordering::Release);
    }

    pub(in crate::app) fn stage(&self) -> Stage {
        match self.stage.load(Ordering::Acquire) {
            0 => Stage::Downloading,
            1 => Stage::Verifying,
            _ => Stage::Installing,
        }
    }

    /// 0.0 to 1.0 of the download.
    pub(in crate::app) fn fraction(&self) -> f32 {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        (self.done.load(Ordering::Relaxed) as f32 / total as f32).clamp(0.0, 1.0)
    }
}

impl ProgressSink for Progress {
    fn report(&self, progress: engine_driver_host::InstallProgress) {
        self.total.store(progress.bytes_total, Ordering::Relaxed);
        self.done.store(progress.bytes_done, Ordering::Relaxed);
    }
}

/// An unpacked and checked bundle, waiting to be swapped in.
#[derive(Debug)]
pub(in crate::app) struct Staged {
    /// The new bundle, inside `staging_dir`.
    pub(in crate::app) bundle: PathBuf,
    /// Removed once the bundle has been moved out of it.
    pub(in crate::app) staging_dir: PathBuf,
}

/// Downloads `artifact` into `downloads`, checks it against the manifest, unpacks it next to
/// `bundle` and verifies the new bundle. The zip is gone when this returns.
#[allow(clippy::too_many_arguments)]
pub(in crate::app) fn download_and_stage(
    source: &dyn UpdateSource,
    artifact: &Artifact,
    bundle: &Bundle,
    version: &Version,
    target: &str,
    downloads: &Path,
    cancel: &Arc<AtomicBool>,
    progress: &Progress,
) -> Result<Staged, InstallError> {
    std::fs::create_dir_all(downloads).map_err(|error| InstallError::Download(error.to_string()))?;
    let zip = downloads.join(format!("TableX-{version}-{}.zip", uuid::Uuid::new_v4()));
    progress.enter(Stage::Downloading);
    let result = (|| {
        let downloaded = source
            .download(&artifact.url, &zip, artifact.size, cancel, progress)
            .map_err(|error| match error {
                DownloadError::Cancelled => InstallError::Cancelled,
                _ if cancel.load(Ordering::Acquire) => InstallError::Cancelled,
                error => InstallError::Download(error.to_string()),
            })?;
        progress.enter(Stage::Verifying);
        check_download(&downloaded, artifact)?;
        if cancel.load(Ordering::Acquire) {
            return Err(InstallError::Cancelled);
        }
        progress.enter(Stage::Installing);
        stage(&zip, bundle, version, target)
    })();
    let _ = std::fs::remove_file(&zip);
    result
}

fn check_download(downloaded: &DownloadedArtifact, artifact: &Artifact) -> Result<(), InstallError> {
    if downloaded.size_bytes != artifact.size {
        return Err(InstallError::Checksum(format!(
            "size {} instead of {}",
            downloaded.size_bytes, artifact.size
        )));
    }
    if !downloaded.sha256.eq_ignore_ascii_case(&artifact.sha256) {
        return Err(InstallError::Checksum("SHA-256 differs".into()));
    }
    Ok(())
}

/// Unpacks `zip` with `ditto` into a new directory beside `bundle` (the same folder, so the swap is a
/// rename on one volume) and verifies what came out.
pub(in crate::app) fn stage(
    zip: &Path,
    bundle: &Bundle,
    version: &Version,
    target: &str,
) -> Result<Staged, InstallError> {
    let staging_dir = bundle.parent().join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4()));
    let unpack = |error: String| InstallError::Unpack(error);
    std::fs::create_dir(&staging_dir).map_err(|error| unpack(error.to_string()))?;
    let result = (|| {
        let output = Command::new(DITTO)
            .args(["-x", "-k"])
            .arg(zip)
            .arg(&staging_dir)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| unpack(error.to_string()))?;
        if !output.status.success() {
            return Err(unpack(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
        }
        let mut apps = std::fs::read_dir(&staging_dir)
            .map_err(|error| unpack(error.to_string()))?
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "app"))
            .map(|entry| entry.path());
        let (Some(new), None) = (apps.next(), apps.next()) else {
            return Err(InstallError::Bundle("the archive does not hold exactly one app".into()));
        };
        let is_directory = std::fs::symlink_metadata(&new).is_ok_and(|metadata| metadata.is_dir());
        if !is_directory {
            return Err(InstallError::Bundle("the app in the archive is not a bundle".into()));
        }
        verify_bundle(&new, bundle, version, target)?;
        Ok(new)
    })();
    match result {
        Ok(bundle) => Ok(Staged { bundle, staging_dir }),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            Err(error)
        }
    }
}

/// `key` of an Info.plist in any format (XML or binary).
fn plist_string(plist: &Path, key: &str) -> Result<String, String> {
    let output = Command::new(PLUTIL)
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(plist)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("no {key} in the Info.plist"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The new bundle `new` is this app (same identifier as `running`), at the manifest's version, runs
/// on this CPU and carries a valid code signature.
pub(in crate::app) fn verify_bundle(
    new: &Path,
    running: &Bundle,
    version: &Version,
    target: &str,
) -> Result<(), InstallError> {
    let invalid = |why: String| InstallError::Bundle(why);
    let plist = new.join("Contents/Info.plist");
    let identifier = plist_string(&plist, "CFBundleIdentifier").map_err(invalid)?;
    let expected = plist_string(&running.info_plist(), "CFBundleIdentifier").map_err(invalid)?;
    if identifier != expected {
        return Err(invalid(format!("bundle identifier {identifier}, expected {expected}")));
    }
    let shown = plist_string(&plist, "CFBundleShortVersionString").map_err(invalid)?;
    if Version::parse(&shown).as_ref() != Some(version) {
        return Err(invalid(format!("version {shown}, expected {version}")));
    }
    let executable = plist_string(&plist, "CFBundleExecutable").map_err(invalid)?;
    if executable.contains('/') || executable.is_empty() {
        return Err(invalid("executable name".into()));
    }
    let executable = new.join("Contents/MacOS").join(executable);
    match executable_runs_on(&executable, target) {
        Ok(true) => {}
        Ok(false) => return Err(invalid(format!("the executable is not built for {target}"))),
        Err(error) => return Err(invalid(format!("the executable: {error}"))),
    }
    let output = Command::new(CODESIGN)
        .args(["--verify", "--deep", "--strict"])
        .arg(new)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| invalid(error.to_string()))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(invalid(format!("code signature: {reason}")));
    }
    Ok(())
}

/// Whether the Mach-O at `path` (thin or universal) has a slice for `target`.
fn executable_runs_on(path: &Path, target: &str) -> io::Result<bool> {
    use std::io::Read as _;
    const ARM64: u32 = 0x0100_000C;
    const X86_64: u32 = 0x0100_0007;
    let cpu = match target {
        "aarch64-apple-darwin" => ARM64,
        "x86_64-apple-darwin" => X86_64,
        _ => return Ok(false),
    };
    let mut head = [0_u8; 8 + 20 * 16];
    let read = std::fs::File::open(path)?.read(&mut head)?;
    let head = &head[..read];
    let word = |at: usize, big: bool| {
        head.get(at..at + 4).map(|bytes| {
            let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
            if big { u32::from_be_bytes(bytes) } else { u32::from_le_bytes(bytes) }
        })
    };
    match word(0, true) {
        // A thin 64-bit Mach-O (stored little-endian): the CPU follows the magic.
        Some(0xCFFA_EDFE) => Ok(word(4, false) == Some(cpu)),
        // A universal file: a count, then one record (CPU first) per slice.
        Some(magic @ (0xCAFE_BABE | 0xCAFE_BABF)) => {
            let record = if magic == 0xCAFE_BABE { 20 } else { 32 };
            let count = word(4, true).unwrap_or(0).min(16) as usize;
            Ok((0..count).any(|index| word(8 + index * record, true) == Some(cpu)))
        }
        _ => Err(io::Error::other("not a 64-bit Mach-O file")),
    }
}

// --- the swap -----------------------------------------------------------------------------

/// `<folder>/<name>.app.old-<version>`: where the running bundle waits until the new one has launched.
pub(in crate::app) fn backup_path(bundle: &Bundle, current_version: &str) -> PathBuf {
    let mut name = bundle.name().to_owned();
    name.push(format!("{BACKUP_INFIX}{current_version}"));
    bundle.parent().join(name)
}

/// Written inside a backup when it is made: the pid of the process that runs from it.
const BACKUP_MARKER: &str = ".tablex-update-pid";
/// A backup younger than this is kept, whatever its marker says.
pub(in crate::app) const BACKUP_MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// Whether the process that wrote `backup`'s marker is alive (a backup without a readable marker is
/// not "in use": only its age protects it).
fn backup_in_use(backup: &Path) -> bool {
    std::fs::read_to_string(backup.join(BACKUP_MARKER))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .is_some_and(process_alive)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(not(unix))]
fn process_alive(_: u32) -> bool {
    true
}

/// Moves the running bundle aside (with a marker naming this process, which runs from it) and the
/// staged one into its place, then removes the (now empty) staging directory. A failure of the second
/// step puts the first back. `rename` is `fs::rename`; tests make it fail. A backup of this version that
/// a live process still runs from is never replaced.
pub(in crate::app) fn swap(
    bundle: &Bundle,
    staged: &Staged,
    current_version: &str,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<PathBuf, InstallError> {
    let backup = backup_path(bundle, current_version);
    let swap = |error: String| InstallError::Swap(error);
    if backup.exists() {
        if backup_in_use(&backup) {
            return Err(swap("another TableX is still running from the previous update".into()));
        }
        // A backup of this very version left by an earlier try is stale.
        std::fs::remove_dir_all(&backup).map_err(|error| swap(error.to_string()))?;
    }
    rename(&bundle.path, &backup).map_err(|error| swap(error.to_string()))?;
    if let Err(error) = rename(&staged.bundle, &bundle.path) {
        return Err(match rename(&backup, &bundle.path) {
            Ok(()) => swap(error.to_string()),
            Err(restore) => swap(format!(
                "{error}; the old app could not be put back ({restore}), it is at {}",
                backup.display()
            )),
        });
    }
    // Best effort: without it only the age protects the backup.
    let _ = std::fs::write(backup.join(BACKUP_MARKER), std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&staged.staging_dir);
    Ok(backup)
}

/// The last step of an install, off the UI thread: the swap, unless the install was cancelled meanwhile.
/// The staging directory does not outlive a failure.
pub(in crate::app) fn install_staged(
    bundle: &Bundle,
    staged: Staged,
    current_version: &str,
    cancel: &AtomicBool,
) -> Result<(), InstallError> {
    let result = if cancel.load(Ordering::Acquire) {
        Err(InstallError::Cancelled)
    } else {
        swap(bundle, &staged, current_version, &|from, to| std::fs::rename(from, to)).map(drop)
    };
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staged.staging_dir);
    }
    result
}

/// Removes what an update leaves behind, on a launch: a `<name>.app.old-*` beside the running bundle
/// that is at least `BACKUP_MIN_AGE` old and that no live process runs from (a second instance may
/// still run from one), and staging directories and downloads older than `min_age` (an install in
/// another instance is younger). Best effort.
pub(in crate::app) fn clean_up(bundle: &Bundle, downloads: Option<&Path>, min_age: Duration, now: SystemTime) {
    let age_at_least = |path: &Path, age: Duration| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() >= age)
    };
    let backup_prefix = format!("{}{BACKUP_INFIX}", bundle.name().to_string_lossy());
    for entry in std::fs::read_dir(bundle.parent()).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let removable = if name.starts_with(&backup_prefix) {
            age_at_least(&path, BACKUP_MIN_AGE) && !backup_in_use(&path)
        } else {
            name.starts_with(STAGING_PREFIX) && age_at_least(&path, min_age)
        };
        if removable {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    for entry in downloads.and_then(|dir| std::fs::read_dir(dir).ok()).into_iter().flatten().flatten() {
        let path = entry.path();
        if age_at_least(&path, min_age) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Starts the new app once this process has exited (so its settings and session are on disk by
/// then): `open -n <bundle>` from a detached shell that waits for `pid` (at most 120 seconds).
pub(in crate::app) fn spawn_relaunch(bundle: &Path, pid: u32) -> io::Result<()> {
    relaunch_with(bundle, pid, OPEN)
}

#[cfg(not(unix))]
fn relaunch_with(_: &Path, _: u32, _: &str) -> io::Result<()> {
    Err(io::Error::other("restarting is only supported on macOS"))
}

/// [`spawn_relaunch`] with the program that opens the bundle named (a test stands in for `open`).
#[cfg(unix)]
fn relaunch_with(bundle: &Path, pid: u32, open: &str) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    const SCRIPT: &str = r#"n=0; while kill -0 "$0" 2>/dev/null; do n=$((n+1)); [ "$n" -gt 1200 ] && exit 1; sleep 0.1; done; exec "$2" -n "$1""#;
    Command::new("/bin/sh")
        .args(["-c", SCRIPT])
        .arg(pid.to_string())
        .arg(bundle)
        .arg(open)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

#[cfg(test)]
#[path = "install_tests.rs"]
pub(in crate::app) mod tests;
