//! Building a driver dylib for a live test.

use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds `crate_manifest` in release mode with `features` (for example
/// `["abi-v2"]`) and returns the path of the library: `lib<lib_name>.dylib`, `.so`
/// or `<lib_name>.dll`. Symbols are kept (`CARGO_PROFILE_RELEASE_STRIP=none`) so a
/// failing run has useful backtraces.
///
/// Call it with `Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")` from a
/// driver crate's test.
pub fn build_release_dylib(
    crate_manifest: &Path,
    lib_name: &str,
    features: &[&str],
) -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut build = Command::new(&cargo);
    build
        .args(["build", "--release", "--manifest-path"])
        .arg(crate_manifest)
        .env("CARGO_PROFILE_RELEASE_STRIP", "none");
    if !features.is_empty() {
        build.arg("--features").arg(features.join(","));
    }
    let output = build
        .output()
        .map_err(|error| format!("cargo build: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "driver build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let metadata = Command::new(&cargo)
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(crate_manifest)
        .output()
        .map_err(|error| format!("cargo metadata: {error}"))?;
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)
        .map_err(|error| format!("cargo metadata output: {error}"))?;
    let target = metadata["target_directory"]
        .as_str()
        .ok_or("cargo metadata has no target_directory")?;
    let path = Path::new(target)
        .join("release")
        .join(format!("{DLL_PREFIX}{lib_name}{DLL_SUFFIX}"));
    if path.exists() {
        Ok(path)
    } else {
        Err(format!("{} was not produced", path.display()))
    }
}
