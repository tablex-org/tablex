//! Load driver dylibs through the real host loader and report what the host sees.
//!
//! ```text
//! cargo run --release -p engine-driver-host --example inspect-driver -- <dylib>
//! cargo run --release -p engine-driver-host --example inspect-driver -- --manifest <manifest.json>
//! ```
//!
//! * `<dylib>`: `dlopen` + ABI check + capability parse via [`DriverLoader`].
//!   Prints one JSON object on stdout (`abi_version`, `driver`, `features`,
//!   the full `capabilities`, plus file `sha256` and `size_bytes`).
//!   Exits non-zero (message on stderr) if any step fails.
//! * `--manifest <file>`: parse the manifest with the host's `DriverManifest`,
//!   then for every driver whose artifact for this platform is a `file://` URL run
//!   the real install path (`DriverInstaller`: size, sha256, architecture,
//!   ABI probe) into a throwaway store, load the installed dylib with
//!   `DriverLoader::load_verified`, and require that the manifest `id`,
//!   `uriSchemes` and `features` match the dylib's own capabilities. Prints a
//!   JSON report and exits non-zero if any driver fails.
//!
//! Local helper for driver packaging (`rust-engine/scripts`); not shipped.

use engine_driver_host::{
    DriverInstaller, DriverLoader, DriverManifest, DriverManifestEntry, DriverTarget,
    InstalledDriverStore,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, manifest] if flag == "--manifest" => check_manifest(Path::new(manifest)),
        [path] if !path.starts_with("--") => inspect_dylib(Path::new(path)),
        _ => {
            Err("usage: inspect-driver <dylib> | inspect-driver --manifest <manifest.json>".into())
        }
    };
    match result {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("report JSON")
            );
            if report.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(message) => {
            eprintln!("inspect-driver: {message}");
            ExitCode::FAILURE
        }
    }
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn inspect_dylib(path: &Path) -> Result<serde_json::Value, String> {
    let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size_bytes = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    let sha256 = sha256_hex(&path)?;
    let handle = DriverLoader
        .load_verified(&path, &sha256)
        .map_err(|e| format!("load failed for {}: {e}", path.display()))?;
    let capabilities = handle.capabilities();
    let report = serde_json::json!({
        "ok": true,
        "path": path,
        "sha256": sha256,
        "size_bytes": size_bytes,
        "abi_version": capabilities.abi_version,
        "driver": capabilities.driver,
        "display_name": capabilities.display_name,
        "uri_schemes": capabilities.uri_schemes,
        "features": capabilities.features(),
        "feature_count": capabilities.features().len(),
        "capabilities": capabilities,
    });
    handle
        .shutdown()
        .map_err(|e| format!("shutdown failed for {}: {e}", path.display()))?;
    Ok(report)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Some(value) = input
                .get(index + 1..index + 3)
                .map(|hex| u8::from_str_radix(hex, 16))
                .and_then(Result::ok)
            {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // `file:///abs/path` (empty authority) or `file://localhost/abs/path`.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    rest.starts_with('/')
        .then(|| PathBuf::from(percent_decode(rest)))
}

fn check_entry(
    entry: &DriverManifestEntry,
    store: &InstalledDriverStore,
) -> Result<serde_json::Value, String> {
    let target = DriverTarget::current();
    let artifact = entry
        .artifact_for_target(&target)
        .ok_or_else(|| format!("no artifact for {target}"))?;
    let path = file_url_path(&artifact.url)
        .ok_or_else(|| format!("artifact URL is not a local file:// URL: {}", artifact.url))?;
    let record = DriverInstaller::new(target, store)
        .with_signature_verification(false)
        .install_from_file(entry, &path)
        .map_err(|e| format!("install failed: {e}"))?;
    let handle = DriverLoader
        .load_verified(&record.path, &record.sha256)
        .map_err(|e| format!("load failed: {e}"))?;
    let capabilities = handle.capabilities();

    let mut problems = Vec::new();
    if capabilities.driver != entry.id {
        problems.push(format!(
            "id: manifest '{}' != driver '{}'",
            entry.id, capabilities.driver
        ));
    }
    if capabilities.abi_version != entry.app_abi_version {
        problems.push(format!(
            "abi: manifest {} != driver {}",
            entry.app_abi_version, capabilities.abi_version
        ));
    }
    let as_set = |items: &[String]| items.iter().cloned().collect::<BTreeSet<_>>();
    if as_set(&capabilities.uri_schemes) != as_set(&entry.uri_schemes) {
        problems.push(format!(
            "uriSchemes: manifest {:?} != driver {:?}",
            entry.uri_schemes, capabilities.uri_schemes
        ));
    }
    let manifest_features = as_set(&entry.features);
    let driver_features = as_set(capabilities.features());
    if manifest_features != driver_features {
        problems.push(format!(
            "features differ: only in manifest {:?}, only in driver {:?}",
            manifest_features
                .difference(&driver_features)
                .collect::<Vec<_>>(),
            driver_features
                .difference(&manifest_features)
                .collect::<Vec<_>>(),
        ));
    }
    let report = serde_json::json!({
        "id": entry.id,
        "version": entry.version,
        "abi_version": capabilities.abi_version,
        "feature_count": capabilities.features().len(),
        "installed_to": record.path,
        "ok": problems.is_empty(),
        "problems": problems,
    });
    handle
        .shutdown()
        .map_err(|e| format!("shutdown failed: {e}"))?;
    Ok(report)
}

fn check_manifest(path: &Path) -> Result<serde_json::Value, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let manifest: DriverManifest =
        serde_json::from_slice(&data).map_err(|e| format!("manifest parse failed: {e}"))?;
    if manifest.drivers.is_empty() {
        return Err("manifest has no drivers".into());
    }
    let store_root =
        std::env::temp_dir().join(format!("tablex-inspect-driver-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_root);
    let store = InstalledDriverStore::new(&store_root);

    let mut all_ok = true;
    let mut drivers = Vec::new();
    for entry in &manifest.drivers {
        let report = match check_entry(entry, &store) {
            Ok(report) => report,
            Err(message) => serde_json::json!({
                "id": entry.id,
                "version": entry.version,
                "ok": false,
                "problems": [message],
            }),
        };
        all_ok &= report["ok"].as_bool().unwrap_or(false);
        drivers.push(report);
    }
    let _ = std::fs::remove_dir_all(&store_root);
    Ok(serde_json::json!({
        "ok": all_ok,
        "manifest": path,
        "generated_at": manifest.generated_at,
        "drivers": drivers,
    }))
}
