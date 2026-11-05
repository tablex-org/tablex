use engine_driver_host::{
    DriverArtifact, DriverManifestEntry, DriverTarget, DriverVerificationError, DriverVerifier,
    SignerConfig, TargetArch, TargetOs, TrustPolicy,
};
use std::fs;
use tempfile::TempDir;

const MAC_ARM64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::Aarch64);
const MAC_X86_64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::X86_64);
const LINUX_X86_64: DriverTarget = DriverTarget::new(TargetOs::Linux, TargetArch::X86_64);
const WINDOWS_ARM64: DriverTarget = DriverTarget::new(TargetOs::Windows, TargetArch::Aarch64);

const CPU_TYPE_ARM64: u32 = 0x0100_000C;
const MH_MAGIC_64: u32 = 0xFEED_FACF;

fn arm64_dylib_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
    bytes.extend_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
    bytes.extend(std::iter::repeat_n(0u8, 128));
    bytes
}

fn write_temp_dylib(temp: &TempDir, bytes: &[u8]) -> std::path::PathBuf {
    let path = temp.path().join("libdbengine_test.dylib");
    fs::write(&path, bytes).expect("write temp dylib");
    path
}

fn verifier_for_arm64() -> DriverVerifier {
    DriverVerifier::new(MAC_ARM64)
}

#[test]
fn verifies_file_size_exactly() {
    let temp = TempDir::new().expect("temp directory");
    let path = write_temp_dylib(&temp, &arm64_dylib_bytes());

    verifier_for_arm64()
        .verify_size(&path, arm64_dylib_bytes().len() as u64)
        .expect("size matches");

    let error = verifier_for_arm64()
        .verify_size(&path, 1)
        .expect_err("size mismatch");
    assert!(matches!(
        error,
        DriverVerificationError::SizeMismatch { expected: 1, actual }
        if actual == arm64_dylib_bytes().len() as u64
    ));
}

#[test]
fn verifies_sha256_lowercase_hex() {
    let temp = TempDir::new().expect("temp directory");
    let path = write_temp_dylib(&temp, &arm64_dylib_bytes());
    let digest = sha256_of(&path);

    verifier_for_arm64()
        .verify_checksum(&path, digest.to_lowercase().as_str())
        .expect("checksum matches");

    let error = verifier_for_arm64()
        .verify_checksum(&path, "deadbeef".repeat(8).as_str())
        .expect_err("checksum mismatch");
    assert!(matches!(
        error,
        DriverVerificationError::ChecksumMismatch { .. }
    ));
}

#[test]
fn verifies_mach_o_architecture() {
    let temp = TempDir::new().expect("temp directory");
    let arm64 = write_temp_dylib(&temp, &arm64_dylib_bytes());
    verifier_for_arm64()
        .verify_architecture(&arm64)
        .expect("arm64 accepted");

    let mut wrong_arch = Vec::new();
    wrong_arch.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
    wrong_arch.extend_from_slice(&0x0100_0007u32.to_le_bytes());
    wrong_arch.extend(std::iter::repeat_n(0u8, 128));
    let wrong = write_temp_dylib(&temp, &wrong_arch);

    let error = verifier_for_arm64()
        .verify_architecture(&wrong)
        .expect_err("x86_64 rejected on arm64 verifier");
    assert!(matches!(
        error,
        DriverVerificationError::ArchUnsupported { .. }
    ));
}

#[test]
fn accepts_universal_binary_for_any_architecture() {
    let temp = TempDir::new().expect("temp directory");
    let universal = write_temp_dylib(&temp, &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 1]);

    DriverVerifier::new(MAC_ARM64)
        .verify_architecture(&universal)
        .expect("universal accepted");
    DriverVerifier::new(MAC_X86_64)
        .verify_architecture(&universal)
        .expect("universal accepted");
}

#[test]
fn rejects_path_traversal_in_artifact_id() {
    let entry = DriverManifestEntry {
        id: "../../evil".into(),
        display_name: "Evil".into(),
        version: "1.0.0".into(),
        app_abi_version: 2,
        uri_schemes: vec!["evil".into()],
        features: vec![],
        license: "MIT".into(),
        source_repo: "https://example.com".into(),
        team_id: "TEAM".into(),
        artifacts: vec![DriverArtifact {
            target: Some("macos-aarch64".into()),
            url: "https://example.com/evil.zip".into(),
            sha256: "abc".into(),
            size_bytes: 1,
            ..Default::default()
        }],
        ..Default::default()
    };

    let temp = TempDir::new().expect("temp directory");
    let error = DriverVerifier::new(MAC_ARM64)
        .install_destination(&entry, temp.path())
        .expect_err("path traversal rejected");
    assert!(matches!(
        error,
        DriverVerificationError::InvalidDriverId { .. }
    ));
}

fn sha256_of(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let data = fs::read(path).expect("read file");
    let digest = Sha256::digest(&data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn artifact(arch: &str, target: Option<&str>) -> DriverArtifact {
    DriverArtifact {
        arch: arch.into(),
        target: target.map(Into::into),
        url: format!("https://example.com/{arch}-{target:?}"),
        sha256: "abc".into(),
        size_bytes: 1,
    }
}

fn entry_with(artifacts: Vec<DriverArtifact>) -> DriverManifestEntry {
    DriverManifestEntry {
        id: "mock".into(),
        version: "1.0.0".into(),
        artifacts,
        ..Default::default()
    }
}

fn picked_url(entry: &DriverManifestEntry, target: &DriverTarget) -> Option<String> {
    entry.artifact_for_target(target).map(|a| a.url.clone())
}

#[test]
fn old_arch_only_artifacts_are_macos_builds() {
    let entry = entry_with(vec![artifact("arm64", None), artifact("x86_64", None)]);
    assert!(picked_url(&entry, &MAC_ARM64)
        .unwrap()
        .starts_with("https://example.com/arm64"));
    assert!(picked_url(&entry, &MAC_X86_64)
        .unwrap()
        .starts_with("https://example.com/x86_64"));
    assert_eq!(picked_url(&entry, &LINUX_X86_64), None);
    assert_eq!(picked_url(&entry, &WINDOWS_ARM64), None);
}

#[test]
fn old_universal_artifact_serves_macos_only() {
    let entry = entry_with(vec![artifact("universal", None)]);
    assert!(picked_url(&entry, &MAC_ARM64).is_some());
    assert!(picked_url(&entry, &MAC_X86_64).is_some());
    assert_eq!(picked_url(&entry, &LINUX_X86_64), None);
}

#[test]
fn exact_target_wins_over_the_universal_build() {
    let entry = entry_with(vec![
        artifact("universal", Some("macos-universal")),
        artifact("arm64", Some("macos-aarch64")),
    ]);
    assert!(picked_url(&entry, &MAC_ARM64)
        .unwrap()
        .contains("macos-aarch64"));
    assert!(picked_url(&entry, &MAC_X86_64)
        .unwrap()
        .contains("macos-universal"));
}

#[test]
fn selects_the_artifact_for_each_os_and_arch() {
    let entry = entry_with(vec![
        artifact("arm64", Some("macos-aarch64")),
        artifact("linux-x86_64", Some("linux-x86_64")),
        artifact("windows-aarch64", Some("windows-aarch64")),
    ]);
    assert!(picked_url(&entry, &MAC_ARM64)
        .unwrap()
        .contains("macos-aarch64"));
    assert!(picked_url(&entry, &LINUX_X86_64)
        .unwrap()
        .contains("linux-x86_64"));
    assert!(picked_url(&entry, &WINDOWS_ARM64)
        .unwrap()
        .contains("windows-aarch64"));
    assert_eq!(picked_url(&entry, &MAC_X86_64), None);
    assert_eq!(
        picked_url(
            &entry,
            &DriverTarget::new(TargetOs::Linux, TargetArch::Aarch64)
        ),
        None
    );
}

#[test]
fn schema_v2_artifact_target_round_trips_and_legacy_omits_it() {
    let v2: DriverArtifact = serde_json::from_str(
        r#"{"arch":"linux-x86_64","target":"linux-x86_64","url":"u","sha256":"s","sizeBytes":1}"#,
    )
    .expect("v2 artifact");
    assert_eq!(v2.target_id(), "linux-x86_64");
    let legacy: DriverArtifact =
        serde_json::from_str(r#"{"arch":"arm64","url":"u","sha256":"s","sizeBytes":1}"#)
            .expect("legacy artifact");
    assert_eq!(legacy.target_id(), "macos-aarch64");
    assert!(!serde_json::to_string(&legacy).unwrap().contains("target"));
}

#[test]
fn install_destination_uses_the_library_name_of_the_target() {
    let temp = TempDir::new().expect("temp directory");
    let entry = entry_with(vec![]);
    let name = |target| {
        DriverVerifier::new(target)
            .install_destination(&entry, temp.path())
            .expect("destination")
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned()
    };
    assert_eq!(name(MAC_ARM64), "libdbengine_mock.dylib");
    assert_eq!(name(LINUX_X86_64), "libdbengine_mock.so");
    assert_eq!(name(WINDOWS_ARM64), "dbengine_mock.dll");
}

#[test]
fn verifies_elf_and_pe_through_the_target_verifier() {
    let temp = TempDir::new().expect("temp directory");
    let mut elf = vec![0x7F, b'E', b'L', b'F', 2, 1, 1];
    elf.resize(16, 0);
    elf.extend([3, 0, 62, 0]);
    elf.resize(64, 0);
    let path = temp.path().join("libdbengine_test.so");
    fs::write(&path, &elf).expect("write elf");
    DriverVerifier::new(LINUX_X86_64)
        .verify_architecture(&path)
        .expect("x86_64 ELF accepted on linux-x86_64");
    assert!(matches!(
        DriverVerifier::new(MAC_ARM64).verify_architecture(&path),
        Err(DriverVerificationError::ArchUnsupported(_))
    ));
    assert!(matches!(
        DriverVerifier::new(WINDOWS_ARM64).verify_architecture(&path),
        Err(DriverVerificationError::ArchUnsupported(_))
    ));
}

#[test]
fn native_signature_policy_is_per_os() {
    let temp = TempDir::new().expect("temp directory");
    let path = temp.path().join("lib.bin");
    fs::write(&path, b"x").expect("write");
    let none = SignerConfig::default();
    let verify = |target, signers: &SignerConfig, policy| {
        DriverVerifier::new(target).verify_native_signature(&path, signers, policy)
    };

    assert!(verify(LINUX_X86_64, &none, TrustPolicy::Strict).is_ok());
    assert!(verify(WINDOWS_ARM64, &none, TrustPolicy::Strict).is_ok());
    assert!(matches!(
        verify(MAC_ARM64, &none, TrustPolicy::Strict),
        Err(DriverVerificationError::InvalidSignature(_))
    ));
    let pinned = SignerConfig {
        windows_publisher: "TableX Ltd".into(),
        ..Default::default()
    };
    assert!(matches!(
        verify(WINDOWS_ARM64, &pinned, TrustPolicy::Strict),
        Err(DriverVerificationError::SignatureUnsupported(_))
    ));
    assert!(verify(LINUX_X86_64, &pinned, TrustPolicy::Strict).is_ok());
}
