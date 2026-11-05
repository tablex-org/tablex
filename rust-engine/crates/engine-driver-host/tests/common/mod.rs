#![allow(dead_code)]
//! Shared helpers: one mock-driver build per test binary and manifest entries
//! that point at it.

use engine_driver_host::{DriverArtifact, DriverManifestEntry, DriverTarget};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};

/// The cargo target directory of a helper build (a mock or real driver a test
/// loads), under this crate's `CARGO_TARGET_TMPDIR` inside `target/`, one per
/// `key` (what is built, with which features). It is kept between runs, so a
/// rebuild is incremental and nothing collects in the OS temp directory;
/// cargo's lock on the directory serialises concurrent builds, and a
/// directory per key keeps a build with other features from replacing the
/// library another test is reading.
pub fn helper_target_dir(key: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("helper-builds")
        .join(key)
}

/// Builds helper `key` into its [`helper_target_dir`] once per test process
/// and hands every later caller the same file. Cargo replaces the library in
/// `release/` on every build, even one with nothing to do, so a second build
/// while another test copies the file would race; building inside one lock
/// (all keys share it, which only slows the first test of each kind) rules
/// that out.
pub fn built_once(key: &str, build: impl FnOnce(&Path) -> PathBuf) -> PathBuf {
    static BUILT: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let mut built = BUILT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    built
        .entry(key.to_owned())
        .or_insert_with(|| build(&helper_target_dir(key)))
        .clone()
}

/// The key of a mock-driver build with `features` (none: the default set).
pub fn mock_key(features: &str) -> String {
    if features.is_empty() {
        "mock-driver".into()
    } else {
        format!("mock-driver-{}", features.replace(',', "+"))
    }
}

pub fn mock_dylib() -> &'static Path {
    static DYLIB: OnceLock<PathBuf> = OnceLock::new();
    DYLIB.get_or_init(|| built_once(&mock_key(""), cargo_build_mock_default))
}

fn cargo_build_mock_default(target_dir: &Path) -> PathBuf {
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-driver/Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args(["build", "--release", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target_dir)
        .output()
        .expect("cargo build mock-driver");
    assert!(
        output.status.success(),
        "mock-driver build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    target_dir
        .join("release")
        .join(cdylib_file_name("mock_driver"))
}

/// File name `cargo build` gives the cdylib of crate `crate_name` on this OS
/// (`lib<name>.dylib`, `lib<name>.so`, `<name>.dll`).
pub fn cdylib_file_name(crate_name: &str) -> String {
    format!(
        "{}{crate_name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// Name an installed / staged `libdbengine_<id>` driver has on this OS.
pub fn driver_file_name(id: &str) -> String {
    DriverTarget::current().library_file_name(id)
}

pub fn sha256_of(path: &Path) -> String {
    let digest = Sha256::digest(std::fs::read(path).expect("read file"));
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn mock_entry(version: &str, team_id: &str) -> DriverManifestEntry {
    let dylib = mock_dylib();
    DriverManifestEntry {
        id: "mock".into(),
        display_name: "Mock".into(),
        version: version.into(),
        app_abi_version: 2,
        uri_schemes: vec!["mock".into()],
        features: vec!["navigator".into()],
        license: "MIT".into(),
        source_repo: "https://example.com".into(),
        team_id: team_id.into(),
        artifacts: vec![DriverArtifact {
            target: Some(DriverTarget::current().id()),
            url: "file://unused".into(),
            sha256: sha256_of(dylib),
            size_bytes: std::fs::metadata(dylib).expect("metadata").len(),
            ..Default::default()
        }],
        ..Default::default()
    }
}
