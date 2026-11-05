mod common;

use engine_driver_host::{
    DriverArtifact, DriverInstaller, DriverManifestEntry, DriverTarget, DriverVerificationError,
    InstallError, InstalledDriverStore,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-driver")
}

/// Build the mock-driver cdylib (incremental after the first build; see
/// `common::helper_target_dir`).
fn build_mock_driver() -> PathBuf {
    common::built_once(&common::mock_key(""), cargo_build_mock)
}

fn cargo_build_mock(target_dir: &Path) -> PathBuf {
    let manifest = workspace_root().join("Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--release")
        .arg("--target-dir")
        .arg(target_dir)
        .output()
        .expect("cargo build mock-driver");
    assert!(
        output.status.success(),
        "mock-driver build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dylib = target_dir
        .join("release")
        .join(common::cdylib_file_name("mock_driver"));
    assert!(dylib.exists(), "mock dylib missing at {}", dylib.display());
    dylib
}

fn mock_entry(sha256: String, size: u64) -> DriverManifestEntry {
    DriverManifestEntry {
        id: "mock".into(),
        display_name: "Mock".into(),
        version: "1.0.0".into(),
        app_abi_version: 2,
        uri_schemes: vec!["mock".into()],
        features: vec!["navigator".into()],
        license: "MIT".into(),
        source_repo: "https://example.com".into(),
        team_id: "TEAMID123".into(),
        artifacts: vec![DriverArtifact {
            target: Some(DriverTarget::current().id()),
            url: "file://unused".into(),
            sha256,
            size_bytes: size,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn sha256_of(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let data = fs::read(path).expect("read dylib");
    let digest = Sha256::digest(&data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Copies the source dylib into the test temp directory so the installer
/// can consume it like a downloaded artifact.
fn stage_source(temp: &TempDir, source: &Path) -> PathBuf {
    let staged = temp.path().join("downloaded.dylib");
    fs::copy(source, &staged).expect("stage source");
    staged
}

#[cfg(target_os = "macos")]
fn zip_source(temp: &TempDir, source: &Path) -> PathBuf {
    let archive = temp.path().join("downloaded.zip");
    let status = Command::new("/usr/bin/ditto")
        .args([
            "-c",
            "-k",
            source.to_str().expect("source path"),
            archive.to_str().expect("archive path"),
        ])
        .status()
        .expect("create driver archive");
    assert!(status.success(), "ditto failed with {status}");
    archive
}

#[test]
fn installs_verified_artifact_and_records_it() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let size = fs::metadata(&staged).expect("metadata").len();
    let sha = sha256_of(&staged);
    let entry = mock_entry(sha.clone(), size);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let record = installer
        .install_from_file(&entry, &staged)
        .expect("install succeeds");

    assert_eq!(record.id, "mock");
    assert_eq!(record.version, "1.0.0");
    assert_eq!(record.sha256, sha);
    assert_eq!(record.app_abi_version, 2);

    let expected_path = temp
        .path()
        .join("Drivers")
        .join("v2")
        .join("mock")
        .join("1.0.0")
        .join(common::driver_file_name("mock"));
    assert!(expected_path.exists(), "artifact installed");
    assert_eq!(record.path, expected_path);

    let saved = store.load_all().expect("load store");
    assert_eq!(saved.get("mock"), Some(&record));
}

#[test]
#[cfg(target_os = "macos")]
fn installs_universal_artifact_for_host_architecture() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let size = fs::metadata(&staged).expect("metadata").len();
    let mut entry = mock_entry(sha256_of(&staged), size);
    entry.artifacts[0].target = Some("macos-universal".into());
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    installer
        .install_from_file(&entry, &staged)
        .expect("universal artifact installs");
}

#[test]
fn uninstalls_artifact_and_record() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let size = fs::metadata(&staged).expect("metadata").len();
    let entry = mock_entry(sha256_of(&staged), size);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);
    let record = installer
        .install_from_file(&entry, &staged)
        .expect("install succeeds");

    installer
        .uninstall("mock", None)
        .expect("uninstall succeeds");

    assert!(!record.path.exists());
    assert!(store.record("mock").expect("read store").is_none());
}

#[cfg(target_os = "macos")]
#[test]
fn installs_verified_dylib_from_zip_artifact() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = zip_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let size = fs::metadata(&staged).expect("metadata").len();
    let sha = sha256_of(&staged);
    let entry = mock_entry(sha, size);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let record = installer
        .install_from_file(&entry, &staged)
        .expect("zip install succeeds");

    assert!(record.path.exists());
    assert_eq!(record.sha256, sha256_of(&record.path));
    assert_eq!(
        store.record("mock").expect("load record").unwrap().version,
        "1.0.0"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a locally packaged MongoDB artifact"]
fn installs_and_loads_local_mongodb_package() {
    let artifact = std::env::var_os("TABLEX_MONGO_ARTIFACT")
        .map(PathBuf::from)
        .expect("TABLEX_MONGO_ARTIFACT");
    let manifest = std::env::var_os("TABLEX_MONGO_MANIFEST")
        .map(PathBuf::from)
        .expect("TABLEX_MONGO_MANIFEST");
    let entry: DriverManifestEntry =
        serde_json::from_slice(&fs::read(manifest).expect("read manifest entry"))
            .expect("decode manifest entry");
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let record = installer
        .install_from_file(&entry, &artifact)
        .expect("install MongoDB package");
    let handle = engine_driver_host::DriverLoader
        .load_verified(&record.path, &record.sha256)
        .expect("load installed MongoDB driver");

    assert_eq!(handle.capabilities().driver, "mongodb");
    handle.shutdown().expect("shutdown MongoDB driver");
}

// Inode numbers exist on Unix only.
#[cfg(unix)]
#[test]
fn reinstall_replaces_the_dylib_inode_atomically() {
    use std::os::unix::fs::MetadataExt;
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let size = fs::metadata(&staged).expect("metadata").len();
    let entry = mock_entry(sha256_of(&staged), size);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let first = installer
        .install_from_file(&entry, &staged)
        .expect("first install succeeds");
    let first_inode = fs::metadata(&first.path).expect("first metadata").ino();

    let second = installer
        .install_from_file(&entry, &staged)
        .expect("second install succeeds");
    let second_inode = fs::metadata(&second.path).expect("second metadata").ino();

    assert_ne!(
        first_inode, second_inode,
        "reinstall must not overwrite a live inode"
    );
}

#[test]
fn rejects_checksum_mismatch_before_install() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let size = fs::metadata(&staged).expect("metadata").len();
    let entry = mock_entry("0".repeat(64), size);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let error = installer
        .install_from_file(&entry, &staged)
        .expect_err("checksum mismatch");

    assert!(matches!(
        error,
        InstallError::Verification(DriverVerificationError::ChecksumMismatch { .. })
    ));
    let saved = store.load_all().expect("load store");
    assert!(saved.is_empty(), "no record written");
}

#[test]
fn rejects_size_mismatch_before_install() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let sha = sha256_of(&staged);
    let entry = mock_entry(sha, 1);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let error = installer
        .install_from_file(&entry, &staged)
        .expect_err("size mismatch");

    assert!(matches!(
        error,
        InstallError::Verification(DriverVerificationError::SizeMismatch { .. })
    ));
    assert!(store.load_all().expect("load store").is_empty());
}

#[test]
fn rejects_wrong_architecture() {
    let temp = TempDir::new().expect("temp directory");
    let staged = temp.path().join("not-macho.dylib");
    fs::write(&staged, vec![0u8; 256]).expect("write fake dylib");
    let store = InstalledDriverStore::new(temp.path());
    let sha = sha256_of(&staged);
    let size = fs::metadata(&staged).expect("metadata").len();
    let entry = mock_entry(sha, size);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let error = installer
        .install_from_file(&entry, &staged)
        .expect_err("not a mach-o");

    assert!(matches!(
        error,
        InstallError::Verification(DriverVerificationError::ArchUnsupported { .. })
    ));
    assert!(store.load_all().expect("load store").is_empty());
}

#[test]
fn rejects_abi_mismatch() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let sha = sha256_of(&staged);
    let size = fs::metadata(&staged).expect("metadata").len();
    let mut entry = mock_entry(sha, size);
    entry.app_abi_version = 99;
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let error = installer
        .install_from_file(&entry, &staged)
        .expect_err("abi mismatch");

    assert!(matches!(
        error,
        InstallError::Verification(DriverVerificationError::AbiMismatch { expected: 99, .. })
    ));
    assert!(store.load_all().expect("load store").is_empty());
}

#[test]
fn interrupted_install_leaves_no_partial_artifact() {
    let source = build_mock_driver();
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_source(&temp, &source);
    let store = InstalledDriverStore::new(temp.path());
    let sha = sha256_of(&staged);
    let size = fs::metadata(&staged).expect("metadata").len();
    let entry = mock_entry(sha, size);
    let installer =
        DriverInstaller::new(DriverTarget::current(), &store).with_signature_verification(false);

    let error = installer
        .install_from_file(&entry, &staged)
        .expect("install succeeds once");
    assert!(error.path.exists());

    // Corrupt a second artifact download to simulate interruption:
    // same entry but corrupted source fails checksum, no partial files remain.
    let corrupt = temp.path().join("corrupt.dylib");
    fs::write(&corrupt, vec![1u8; 100]).expect("write corrupt");
    let bad_sha = sha256_of(&corrupt);
    let bad_size = fs::metadata(&corrupt).expect("metadata").len();
    let bad_entry = mock_entry(bad_sha, bad_size);
    let _ = installer
        .install_from_file(&bad_entry, &corrupt)
        .expect_err("corrupt rejected");

    let version_dir = temp
        .path()
        .join("Drivers")
        .join("v2")
        .join("mock")
        .join("1.0.0");
    let files: Vec<_> = fs::read_dir(&version_dir)
        .expect("read version dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 1, "no staging leftovers: {files:?}");
    assert!(files[0].ends_with(&common::driver_file_name("mock")));
}
