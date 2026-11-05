//! Optional native OS signature layer. The signed manifest's SHA-256 already
//! authenticates an artifact on every OS; this adds the platform's own
//! publisher check where one exists.

use crate::{DriverVerificationError, TargetOs, TrustPolicy};
use std::path::Path;

/// The publisher identity pinned for each OS. An OS whose value is empty has no
/// pinned publisher.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignerConfig {
    /// Apple Team ID that must have signed macOS libraries.
    pub macos_team_id: String,
    /// Authenticode publisher (certificate subject) for Windows libraries.
    pub windows_publisher: String,
    /// The values come from the signed manifest of a catalog install, so an
    /// empty Team ID is the publisher's own statement that its macOS builds
    /// carry an ad-hoc signature: the manifest's signature and SHA-256 are then
    /// the check, as on Linux. Pinned values of a local import leave it false,
    /// where an empty Team ID still means "nothing is trusted".
    pub manifest_attested: bool,
}

impl SignerConfig {
    pub fn pinned(&self, os: TargetOs) -> &str {
        match os {
            TargetOs::Macos => &self.macos_team_id,
            TargetOs::Windows => &self.windows_publisher,
            TargetOs::Linux | TargetOs::Other(_) => "",
        }
    }
}

/// How an OS signs native libraries and what that means for trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSignature {
    /// `codesign` plus a pinned Team ID; required under [`TrustPolicy::Strict`].
    Codesign,
    /// Authenticode; only checked when a publisher is pinned.
    Authenticode,
    /// No native signature: the manifest's SHA-256 is the check.
    None,
}

impl NativeSignature {
    pub(crate) fn for_os(os: TargetOs) -> Self {
        match os {
            TargetOs::Macos => Self::Codesign,
            TargetOs::Windows => Self::Authenticode,
            TargetOs::Linux | TargetOs::Other(_) => Self::None,
        }
    }

    /// Verify `path` against the `pinned` publisher of its OS. Under
    /// [`TrustPolicy::Developer`] an ad-hoc macOS signature (valid, no Team
    /// ID) is accepted; one that carries a Team ID must still match. A signed
    /// manifest that pins no Team ID (`attested` and `pinned` empty) accepts a
    /// valid ad-hoc signature under either policy.
    pub(crate) fn verify(
        self,
        path: &Path,
        pinned: &str,
        attested: bool,
        policy: TrustPolicy,
    ) -> Result<(), DriverVerificationError> {
        match self {
            Self::Codesign => verify_codesign(path, pinned, attested, policy),
            Self::Authenticode if pinned.is_empty() => Ok(()),
            // TODO(windows): WinVerifyTrust + publisher match (`windows-sys` is
            // already in the lock file). Until then a pinned publisher cannot
            // be honoured, so refuse instead of silently skipping it.
            Self::Authenticode => Err(DriverVerificationError::SignatureUnsupported(
                std::env::consts::OS,
            )),
            Self::None => Ok(()),
        }
    }
}

fn verify_codesign(
    path: &Path,
    expected_team_id: &str,
    attested: bool,
    policy: TrustPolicy,
) -> Result<(), DriverVerificationError> {
    let developer = policy == TrustPolicy::Developer;
    // The signed manifest says this build has no Team ID: a valid signature of
    // any kind will do, the manifest's SHA-256 pins the exact file.
    let unpinned_by_manifest = attested && expected_team_id.is_empty();
    if expected_team_id.is_empty() && !developer && !unpinned_by_manifest {
        return Err(DriverVerificationError::InvalidSignature(
            "no pinned Team ID configured".into(),
        ));
    }

    let status = std::process::Command::new("codesign")
        .arg("--verify")
        .arg("--strict")
        .arg(path)
        .status()?;
    if !status.success() {
        return Err(DriverVerificationError::InvalidSignature(
            "codesign --verify failed".into(),
        ));
    }

    let output = std::process::Command::new("codesign")
        .arg("-dv")
        .arg("--verbose=4")
        .arg(path)
        .output()?;
    let text = String::from_utf8_lossy(&output.stderr);
    let team_line = text
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="));
    match team_line {
        Some("not set") | None if developer || unpinned_by_manifest => Ok(()),
        Some(_) if unpinned_by_manifest => Ok(()),
        Some(team) if team == expected_team_id => Ok(()),
        Some(team) => Err(DriverVerificationError::TeamIdMismatch {
            expected: expected_team_id.into(),
            actual: team.into(),
        }),
        None => Err(DriverVerificationError::InvalidSignature(
            "no Team ID in signing info".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "/nonexistent/driver";

    fn verify(
        os: TargetOs,
        pinned: &str,
        policy: TrustPolicy,
    ) -> Result<(), DriverVerificationError> {
        NativeSignature::for_os(os).verify(Path::new(FILE), pinned, false, policy)
    }

    #[test]
    fn pinned_signer_is_per_os() {
        let config = SignerConfig {
            macos_team_id: "TEAM".into(),
            windows_publisher: "Publisher".into(),
            manifest_attested: false,
        };
        assert_eq!(config.pinned(TargetOs::Macos), "TEAM");
        assert_eq!(config.pinned(TargetOs::Windows), "Publisher");
        assert_eq!(config.pinned(TargetOs::Linux), "");
    }

    #[test]
    fn macos_strict_requires_a_pinned_team_id() {
        let error = verify(TargetOs::Macos, "", TrustPolicy::Strict).unwrap_err();
        assert!(matches!(
            error,
            DriverVerificationError::InvalidSignature(_)
        ));
    }

    #[test]
    fn a_signed_manifest_without_a_team_id_goes_on_to_the_signature_check() {
        // No longer refused for want of a pinned Team ID; the (missing) file
        // then fails `codesign --verify`.
        let error = NativeSignature::Codesign
            .verify(Path::new(FILE), "", true, TrustPolicy::Strict)
            .unwrap_err();
        assert!(!error.to_string().contains("no pinned Team ID"), "{error}");
    }

    /// An ad-hoc signed Mach-O: what a release without a Developer ID ships.
    #[cfg(target_os = "macos")]
    fn ad_hoc_binary() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("driver");
        std::fs::copy("/usr/bin/true", &path).unwrap();
        let signed = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(signed.success());
        (dir, path)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_ad_hoc_signature_passes_only_where_a_signed_manifest_pins_no_team_id() {
        let (_dir, path) = ad_hoc_binary();
        let codesign = NativeSignature::Codesign;
        assert!(codesign
            .verify(&path, "", true, TrustPolicy::Strict)
            .is_ok());
        // A local import (nothing attested) with nothing pinned is still refused,
        // and a manifest that pins a Team ID still requires it.
        assert!(codesign
            .verify(&path, "", false, TrustPolicy::Strict)
            .is_err());
        assert!(matches!(
            codesign.verify(&path, "TEAM123456", true, TrustPolicy::Strict),
            Err(DriverVerificationError::TeamIdMismatch { .. })
                | Err(DriverVerificationError::InvalidSignature(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_attested_unpinned_manifest_still_needs_a_valid_signature() {
        let (_dir, path) = ad_hoc_binary();
        // Tamper with a code page after signing (the first page past the
        // Mach-O header): its hash no longer matches the signature.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0x4100] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
        assert!(NativeSignature::Codesign
            .verify(&path, "", true, TrustPolicy::Strict)
            .is_err());
    }

    #[test]
    fn linux_strict_passes_on_the_manifest_checksum_alone() {
        assert!(verify(TargetOs::Linux, "", TrustPolicy::Strict).is_ok());
        assert!(verify(TargetOs::Other("freebsd"), "", TrustPolicy::Strict).is_ok());
    }

    #[test]
    fn windows_without_a_pinned_publisher_passes() {
        assert!(verify(TargetOs::Windows, "", TrustPolicy::Strict).is_ok());
    }

    #[test]
    fn windows_with_a_pinned_publisher_is_unsupported_until_authenticode_lands() {
        let error = verify(TargetOs::Windows, "Publisher", TrustPolicy::Strict).unwrap_err();
        assert!(matches!(
            error,
            DriverVerificationError::SignatureUnsupported(_)
        ));
    }
}
