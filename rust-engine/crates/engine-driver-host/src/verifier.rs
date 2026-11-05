use crate::dynlib::{NativeLibrary, NativeLibraryError};
use crate::image::ImageFormat;
use crate::signer::NativeSignature;
use crate::{DriverManifestEntry, DriverTarget, SignerConfig, TrustPolicy};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use thiserror::Error;

const CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum DriverVerificationError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("size mismatch: expected {expected}, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("SHA-256 mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("architecture not supported: {0}")]
    ArchUnsupported(String),
    #[error("invalid driver id: {0}")]
    InvalidDriverId(String),
    #[error("code signature invalid: {0}")]
    InvalidSignature(String),
    #[error("native signature checks are not implemented on {0} yet")]
    SignatureUnsupported(&'static str),
    #[error("Team ID mismatch: expected {expected}, got {actual}")]
    TeamIdMismatch { expected: String, actual: String },
    #[error("driver dylib is missing the ABI version symbol")]
    AbiSymbolMissing,
    #[error("ABI version mismatch: expected {expected}, got {actual}")]
    AbiMismatch { expected: u32, actual: u32 },
}

#[derive(Debug, Clone)]
pub struct DriverVerifier {
    target: DriverTarget,
}

impl DriverVerifier {
    pub fn new(target: DriverTarget) -> Self {
        Self { target }
    }

    pub fn target(&self) -> &DriverTarget {
        &self.target
    }

    pub fn verify_size(&self, path: &Path, expected: u64) -> Result<(), DriverVerificationError> {
        let actual = std::fs::metadata(path)?.len();
        if actual == expected {
            Ok(())
        } else {
            Err(DriverVerificationError::SizeMismatch { expected, actual })
        }
    }

    pub fn verify_checksum(
        &self,
        path: &Path,
        expected: &str,
    ) -> Result<(), DriverVerificationError> {
        self.verify_digest(&sha256_file(path)?, expected)
    }

    /// Compare a digest computed while streaming against the manifest value.
    pub fn verify_digest(
        &self,
        actual: &str,
        expected: &str,
    ) -> Result<(), DriverVerificationError> {
        let expected = expected.to_lowercase();
        if actual == expected {
            Ok(())
        } else {
            Err(DriverVerificationError::ChecksumMismatch {
                expected,
                actual: actual.to_string(),
            })
        }
    }

    /// The file is a loadable image for this target: Mach-O on macOS, ELF on
    /// Linux, PE on Windows, built for the target's CPU.
    pub fn verify_architecture(&self, path: &Path) -> Result<(), DriverVerificationError> {
        let format = ImageFormat::for_os(self.target.os)?;
        format.verify(&mut File::open(path)?, self.target.arch)
    }

    pub fn install_destination(
        &self,
        entry: &DriverManifestEntry,
        base_directory: &Path,
    ) -> Result<PathBuf, DriverVerificationError> {
        versioned_artifact_path(base_directory, &self.target, &entry.id, &entry.version)
    }

    /// Check the native OS signature against the publisher pinned for this
    /// OS in `signers` (see [`SignerConfig`]). macOS: `codesign` plus the
    /// pinned Team ID, required under [`TrustPolicy::Strict`] and accepting an
    /// ad-hoc signature under [`TrustPolicy::Developer`]. Windows:
    /// Authenticode when a publisher is pinned. Linux: nothing; the manifest's
    /// SHA-256 is the check.
    pub fn verify_native_signature(
        &self,
        path: &Path,
        signers: &SignerConfig,
        policy: TrustPolicy,
    ) -> Result<(), DriverVerificationError> {
        NativeSignature::for_os(self.target.os).verify(
            path,
            signers.pinned(self.target.os),
            signers.manifest_attested,
            policy,
        )
    }

    /// Probe `dbengine_abi_version` with a scoped open/close of the library.
    pub fn probe_abi(&self, path: &Path, expected: u32) -> Result<(), DriverVerificationError> {
        let library = NativeLibrary::open(path).map_err(|error| match error {
            unsupported @ NativeLibraryError::Unsupported(_) => {
                DriverVerificationError::ArchUnsupported(unsupported.to_string())
            }
            open => DriverVerificationError::Io(std::io::Error::other(open.to_string())),
        })?;
        let symbol = library
            .symbol(c"dbengine_abi_version")
            .ok_or(DriverVerificationError::AbiSymbolMissing)?;
        // SAFETY: symbol points to the driver's exported `dbengine_abi_version` function.
        let abi_fn: extern "C" fn() -> u32 = unsafe { std::mem::transmute(symbol.as_ptr()) };
        let actual = abi_fn();
        if actual == expected {
            Ok(())
        } else {
            Err(DriverVerificationError::AbiMismatch { expected, actual })
        }
    }
}

/// `<base>/Drivers/v2/<id>/<version>/<library file name of the target>`
pub(crate) fn versioned_artifact_path(
    base_directory: &Path,
    target: &DriverTarget,
    id: &str,
    version: &str,
) -> Result<PathBuf, DriverVerificationError> {
    validate_driver_id(id)?;
    validate_version(version)?;
    Ok(base_directory
        .join("Drivers")
        .join("v2")
        .join(id)
        .join(version)
        .join(target.library_file_name(id)))
}

pub(crate) fn validate_driver_id(id: &str) -> Result<(), DriverVerificationError> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(DriverVerificationError::InvalidDriverId(id.into()))
    }
}

fn validate_version(version: &str) -> Result<(), DriverVerificationError> {
    let valid = !version.is_empty()
        && version.len() <= 32
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(DriverVerificationError::InvalidDriverId(version.into()))
    }
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, DriverVerificationError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::download::hex(&hasher.finalize()))
}
