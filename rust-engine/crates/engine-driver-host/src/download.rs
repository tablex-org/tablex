use crate::os_fs;
use crate::policy::TrustPolicy;
use crate::progress::{InstallStep, ProgressSink, ThrottledProgress};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use thiserror::Error;

const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("download cancelled")]
    Cancelled,
    #[error("download I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("response exceeds expected size {limit}")]
    TooLarge { limit: u64 },
    #[error("download size mismatch: expected {expected}, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("downloads require HTTPS: {0}")]
    InsecureUrl(String),
    #[error("local URLs are only allowed in developer mode: {0}")]
    LocalUrlNotAllowed(String),
    #[error("unsupported URL scheme in '{0}'")]
    UnsupportedScheme(String),
}

/// How the caller must open an artifact URL after policy validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadTransport {
    /// HTTPS, or plain HTTP to localhost.
    Http,
    /// `file://`; only returned under [`TrustPolicy::Developer`].
    LocalFile,
}

/// Apply the download URL policy: HTTPS only (plain HTTP to `localhost` /
/// `127.0.0.1` is allowed for tests), and `file://` only in developer mode.
pub fn classify_download_url(
    url: &str,
    policy: TrustPolicy,
) -> Result<DownloadTransport, DownloadError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| DownloadError::UnsupportedScheme(url.to_string()))?;
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(DownloadTransport::Http),
        "http" if is_loopback_authority(rest) => Ok(DownloadTransport::Http),
        "http" => Err(DownloadError::InsecureUrl(url.to_string())),
        "file" if policy == TrustPolicy::Developer => Ok(DownloadTransport::LocalFile),
        "file" => Err(DownloadError::LocalUrlNotAllowed(url.to_string())),
        _ => Err(DownloadError::UnsupportedScheme(url.to_string())),
    }
}

fn is_loopback_authority(after_scheme: &str) -> bool {
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or_default();
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1"
}

/// A fully received artifact with the digest computed while it streamed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedArtifact {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Stream `source` into a new file at `destination`, hashing incrementally so
/// the artifact is read exactly once, enforcing `expected_size` as a hard cap,
/// honouring `cancelled`, and reporting throttled `Downloading` progress.
///
/// `destination` must not exist (it is created exclusively, owner-only, and never
/// through a symlink or reparse point). On
/// any failure or cancellation the partial file is removed.
pub fn stream_to_file(
    source: &mut dyn Read,
    destination: &Path,
    expected_size: u64,
    cancelled: &AtomicBool,
    sink: &dyn ProgressSink,
) -> Result<DownloadedArtifact, DownloadError> {
    let mut output =
        os_fs::open_private(OpenOptions::new().create_new(true).write(true), destination)?;
    let mut partial = PartialFile::new(destination);
    let mut progress = ThrottledProgress::new(sink);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK_SIZE];
    let mut copied = 0_u64;

    progress.report(InstallStep::Downloading, 0, expected_size, Instant::now());
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(DownloadError::Cancelled);
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        if copied > expected_size {
            return Err(DownloadError::TooLarge {
                limit: expected_size,
            });
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
        progress.report(
            InstallStep::Downloading,
            copied,
            expected_size,
            Instant::now(),
        );
    }
    if copied != expected_size {
        return Err(DownloadError::SizeMismatch {
            expected: expected_size,
            actual: copied,
        });
    }
    output.sync_all()?;
    progress.finish(
        InstallStep::Downloading,
        copied,
        expected_size,
        Instant::now(),
    );
    partial.keep();
    Ok(DownloadedArtifact {
        path: destination.to_path_buf(),
        size_bytes: copied,
        sha256: hex(&hasher.finalize()),
    })
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Removes the file on drop unless [`PartialFile::keep`] ran.
struct PartialFile<'a> {
    path: &'a Path,
    keep: bool,
}

impl<'a> PartialFile<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, keep: false }
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for PartialFile<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(self.path);
        }
    }
}
