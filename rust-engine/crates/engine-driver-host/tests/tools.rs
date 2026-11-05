//! Tools (`tablex-mcp`): the manifest `tools` entry, the catalog and the install at the stable
//! path. No network: the byte source is an in-memory reader, the URLs are never opened.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use engine_driver_host::{
    DownloadError, DriverManifest, DriverTarget, InstallProgress, InstallStep,
    InstalledDriverStore, ManifestFetcher, ManifestTool, ManifestTrust, ManifestTrustError,
    NoProgress, RegistryError, RegistryService, TargetArch, TargetOs, ToolCatalogState, ToolError,
    ToolInstaller, ToolKind, ToolManifestEntry, ToolManifestError, ToolStore, TrustPolicy,
    MCP_TOOL_ID,
};
use futures::executor::block_on;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tempfile::TempDir;

const MAC_ARM64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::Aarch64);
const MAC_X86: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::X86_64);
const LINUX_X86: DriverTarget = DriverTarget::new(TargetOs::Linux, TargetArch::X86_64);
const WINDOWS_X86: DriverTarget = DriverTarget::new(TargetOs::Windows, TargetArch::X86_64);
const HTTPS: &str = "https://example.com/drivers-manifest.json";

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn artifact_json(target: &str, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "target": target,
        "arch": target,
        "url": format!("https://example.com/tablex-mcp-{target}"),
        "sha256": hex_sha256(bytes),
        "sizeBytes": bytes.len(),
    })
}

fn tool_json(version: &str, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "id": "tablex-mcp",
        "kind": "mcpServer",
        "displayName": "TableX MCP server",
        "version": version,
        "description": "Lets MCP clients use your saved connections.",
        "minAppVersion": "0.1.0",
        "artifacts": [
            artifact_json("macos-aarch64", bytes),
            artifact_json("linux-x86_64", bytes),
            artifact_json("windows-x86_64", bytes),
        ],
    })
}

fn manifest_with(tools: Vec<serde_json::Value>) -> Vec<u8> {
    serde_json::json!({
        "schemaVersion": 2,
        "generatedAt": "2026-10-14T12:00:00Z",
        "drivers": [{
            "id": "sqlite", "displayName": "SQLite", "version": "1.0.0", "appAbiVersion": 2,
            "uriSchemes": ["sqlite"], "features": [], "license": "MIT",
            "sourceRepo": "https://example.com", "teamId": "T",
            "artifacts": [{"target": "macos-aarch64", "url": "https://example.com/s.zip",
                           "sha256": "ab", "sizeBytes": 3}],
        }],
        "tools": tools,
    })
    .to_string()
    .into_bytes()
}

fn parse(tools: Vec<serde_json::Value>) -> DriverManifest {
    serde_json::from_slice(&manifest_with(tools)).expect("a manifest always parses")
}

fn known(tool: &ManifestTool) -> &ToolManifestEntry {
    match tool {
        ManifestTool::Known(entry) => entry,
        other => panic!("expected a known tool, got {other:?}"),
    }
}

// --- manifest ---------------------------------------------------------------------------

#[test]
fn a_manifest_with_a_tool_parses_into_a_typed_entry() {
    let bytes = b"mcp server v1";
    let manifest = parse(vec![tool_json("1.0.0", bytes)]);
    assert_eq!(manifest.drivers.len(), 1);
    let entry = known(&manifest.tools[0]);
    assert_eq!(entry.id, MCP_TOOL_ID);
    assert_eq!(entry.kind, ToolKind::McpServer);
    assert_eq!(entry.display_name, "TableX MCP server");
    assert_eq!(entry.version, "1.0.0");
    assert_eq!(entry.min_app_version.as_deref(), Some("0.1.0"));
    assert_eq!(entry.artifacts.len(), 3);
    assert_eq!(entry.artifacts[0].sha256, hex_sha256(bytes));
    assert_eq!(entry.artifacts[0].size_bytes, bytes.len() as u64);
    assert_eq!(entry.artifacts[0].target_id(), "macos-aarch64");
}

#[test]
fn manifests_without_tools_still_work() {
    for text in [
        r#"{"generatedAt":"2026-08-31T00:00:00Z","drivers":[]}"#,
        r#"{"schemaVersion":2,"generatedAt":"2026-08-31T00:00:00Z","drivers":[],"tools":[]}"#,
        r#"{"tools":[]}"#,
    ] {
        let manifest: DriverManifest = serde_json::from_str(text).expect(text);
        assert!(manifest.tools.is_empty(), "{text}");
    }
}

#[test]
fn unknown_kinds_ids_and_shapes_are_ignored_without_failing_the_drivers() {
    let bytes = b"x";
    let mut other_kind = tool_json("1.0.0", bytes);
    other_kind["kind"] = "cliTool".into();
    let mut other_id = tool_json("1.0.0", bytes);
    other_id["id"] = "future-tool".into();
    let manifest = parse(vec![
        other_kind,
        other_id,
        serde_json::json!("not an object"),
        serde_json::json!({"kind": "mcpServer"}),
        serde_json::json!({"id": "tablex-mcp"}),
        tool_json("2.0.0", bytes),
    ]);
    assert_eq!(manifest.drivers.len(), 1, "the drivers are untouched");
    let ignored = manifest
        .tools
        .iter()
        .filter(|tool| matches!(tool, ManifestTool::Ignored(_)))
        .count();
    assert_eq!(ignored, 5);
    assert_eq!(known(&manifest.tools[5]).version, "2.0.0");
}

#[test]
fn a_malformed_known_entry_fails_that_tool_only_with_a_typed_error() {
    let bytes = b"mcp";
    let broken = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut tool = tool_json("1.0.0", bytes);
        edit(&mut tool);
        let manifest = parse(vec![tool]);
        assert_eq!(manifest.drivers.len(), 1, "the drivers are untouched");
        match manifest.tools.into_iter().next() {
            Some(ManifestTool::Invalid { id, error, .. }) => {
                assert_eq!(id, MCP_TOOL_ID);
                error
            }
            other => panic!("expected an invalid tool, got {other:?}"),
        }
    };
    assert!(matches!(
        broken(&|t| {
            t.as_object_mut().unwrap().remove("version");
        }),
        ToolManifestError::Malformed(_)
    ));
    assert!(matches!(
        broken(&|t| t["artifacts"] = "none".into()),
        ToolManifestError::Malformed(_)
    ));
    assert!(matches!(
        broken(&|t| t["version"] = "../1".into()),
        ToolManifestError::InvalidVersion(_)
    ));
    assert_eq!(
        broken(&|t| t["artifacts"] = serde_json::json!([])),
        ToolManifestError::NoBuilds
    );
    assert!(matches!(
        broken(&|t| t["artifacts"][0]["sha256"] = "xyz".into()),
        ToolManifestError::InvalidChecksum { .. }
    ));
    assert!(matches!(
        broken(&|t| t["artifacts"][0]["sizeBytes"] = 0.into()),
        ToolManifestError::InvalidSize { .. }
    ));
    assert!(matches!(
        broken(&|t| t["artifacts"][0]["sizeBytes"] = (u64::MAX / 2).into()),
        ToolManifestError::InvalidSize { .. }
    ));
    assert!(matches!(
        broken(&|t| t["artifacts"][0]["url"] = "".into()),
        ToolManifestError::EmptyUrl { .. }
    ));
    assert!(matches!(
        broken(&|t| t["artifacts"][1]["target"] = "macos-aarch64".into()),
        ToolManifestError::DuplicateTarget { .. }
    ));
}

#[test]
fn the_tool_build_is_selected_by_os_and_arch_like_a_driver() {
    let manifest = parse(vec![tool_json("1.0.0", b"x")]);
    let entry = known(&manifest.tools[0]);
    let id = |target: &DriverTarget| {
        entry
            .artifact_for_target(target)
            .map(|artifact| artifact.target_id())
    };
    assert_eq!(id(&MAC_ARM64).as_deref(), Some("macos-aarch64"));
    assert_eq!(id(&LINUX_X86).as_deref(), Some("linux-x86_64"));
    assert_eq!(id(&WINDOWS_X86).as_deref(), Some("windows-x86_64"));
    assert_eq!(id(&MAC_X86), None, "no Intel Mac build listed");
    assert_eq!(
        id(&DriverTarget::new(TargetOs::Linux, TargetArch::Aarch64)),
        None
    );

    let mut universal = entry.clone();
    universal.artifacts[0].target = Some("macos-universal".into());
    assert_eq!(
        universal
            .artifact_for_target(&MAC_X86)
            .map(|artifact| artifact.target_id())
            .as_deref(),
        Some("macos-universal"),
        "the macOS universal build serves either Mac"
    );
    assert!(universal.artifact_for_target(&LINUX_X86).is_some());
}

// --- trust ------------------------------------------------------------------------------

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn sign(bytes: &[u8]) -> Vec<u8> {
    format!(
        "{}\n",
        STANDARD.encode(signing_key().sign(bytes).to_bytes())
    )
    .into_bytes()
}

#[test]
fn a_tool_entry_is_covered_by_the_manifest_signature() {
    let trust = ManifestTrust::new(
        &[signing_key().verifying_key().to_bytes()],
        TrustPolicy::Strict,
    )
    .expect("trust");
    let temp = TempDir::new().unwrap();
    let store = InstalledDriverStore::new(temp.path());
    let body = manifest_with(vec![tool_json("1.0.0", b"honest build")]);
    let signature = sign(&body);

    let verified = trust
        .verify(&store, HTTPS, &body, Some(&signature))
        .expect("the signed manifest verifies");
    assert_eq!(known(&verified.tools[0]).version, "1.0.0");

    // Swap the tool's hash for another build's: the signature no longer matches, and the whole
    // manifest (the drivers too) is rejected.
    let honest = hex_sha256(b"honest build");
    let evil = hex_sha256(b"evil build");
    let tampered = String::from_utf8(body.clone())
        .unwrap()
        .replace(&honest, &evil)
        .into_bytes();
    assert_ne!(tampered, body);
    assert!(matches!(
        trust.verify(&store, HTTPS, &tampered, Some(&signature)),
        Err(ManifestTrustError::SignatureInvalid)
    ));
    // A tool appended after signing is rejected the same way.
    let appended = manifest_with(vec![
        tool_json("1.0.0", b"honest build"),
        serde_json::json!({"id": "tablex-mcp", "kind": "mcpServer"}),
    ]);
    assert!(matches!(
        trust.verify(&store, HTTPS, &appended, Some(&signature)),
        Err(ManifestTrustError::SignatureInvalid)
    ));
}

// --- catalog ----------------------------------------------------------------------------

struct OneShot(Mutex<Vec<Vec<u8>>>);

impl ManifestFetcher for OneShot {
    fn fetch<'a>(
        &'a self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, RegistryError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().remove(0)) })
    }
}

struct Rig {
    _temp: TempDir,
    drivers: InstalledDriverStore,
    tools: ToolStore,
}

impl Rig {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        Self {
            drivers: InstalledDriverStore::new(temp.path().join("drivers")),
            tools: ToolStore::new(temp.path().join("tools")),
            _temp: temp,
        }
    }

    fn catalog(
        &self,
        manifest: Vec<u8>,
        app_version: &str,
        target: &DriverTarget,
    ) -> engine_driver_host::FullCatalog {
        let fetcher = OneShot(Mutex::new(vec![manifest]));
        block_on(
            RegistryService::new("file:///m.json", &fetcher, &self.drivers)
                .with_app_version(app_version)
                .catalog_with_tools(target, &self.tools),
        )
        .expect("catalog")
    }

    fn install(&self, entry: &ToolManifestEntry, bytes: &[u8]) -> Result<(), ToolError> {
        ToolInstaller::new(MAC_ARM64, &self.tools)
            .install(entry, reader(bytes), &AtomicBool::new(false), &NoProgress)
            .map(|_| ())
    }
}

fn reader(
    bytes: &[u8],
) -> impl FnOnce(
    &engine_driver_host::DriverArtifact,
    engine_driver_host::DownloadTransport,
) -> Result<Box<dyn Read>, DownloadError> {
    let bytes = bytes.to_vec();
    move |_, _| Ok(Box::new(Cursor::new(bytes)))
}

fn entry(version: &str, bytes: &[u8]) -> ToolManifestEntry {
    let manifest = parse(vec![tool_json(version, bytes)]);
    known(&manifest.tools[0]).clone()
}

#[test]
fn the_catalog_lists_the_tool_in_each_state_and_never_fails_the_drivers() {
    let rig = Rig::new();
    let v1 = b"build one";
    let state = |manifest: Vec<u8>, app: &str, target: &DriverTarget| {
        let catalog = rig.catalog(manifest, app, target);
        assert_eq!(catalog.drivers.len(), 1, "the drivers are always there");
        catalog.tools.into_iter().next().map(|item| item.state)
    };

    assert_eq!(state(manifest_with(vec![]), "1.0.0", &MAC_ARM64), None);
    assert_eq!(
        state(
            manifest_with(vec![tool_json("1.0.0", v1)]),
            "1.0.0",
            &MAC_ARM64
        ),
        Some(ToolCatalogState::Available)
    );
    assert!(matches!(
        state(manifest_with(vec![tool_json("1.0.0", v1)]), "1.0.0", &MAC_X86),
        Some(ToolCatalogState::Unavailable { reason }) if reason.starts_with("No build for")
    ));
    let mut gated = tool_json("1.0.0", v1);
    gated["minAppVersion"] = "9.0".into();
    assert!(matches!(
        state(manifest_with(vec![gated]), "1.0.0", &MAC_ARM64),
        Some(ToolCatalogState::Unavailable { reason }) if reason.contains("Needs TableX")
    ));
    let mut broken = tool_json("1.0.0", v1);
    broken["artifacts"][0]["sha256"] = "zz".into();
    assert!(matches!(
        state(manifest_with(vec![broken.clone()]), "1.0.0", &MAC_ARM64),
        Some(ToolCatalogState::Unavailable { reason }) if reason.contains("SHA-256")
    ));

    rig.install(&entry("1.0.0", v1), v1).expect("install");
    assert_eq!(
        state(
            manifest_with(vec![tool_json("1.0.0", v1)]),
            "1.0.0",
            &MAC_ARM64
        ),
        Some(ToolCatalogState::Installed {
            version: "1.0.0".into()
        })
    );
    assert_eq!(
        state(
            manifest_with(vec![tool_json("1.1.0", v1)]),
            "1.0.0",
            &MAC_ARM64
        ),
        Some(ToolCatalogState::UpdateAvailable {
            installed: "1.0.0".into(),
            latest: "1.1.0".into()
        })
    );
    // Installed, but the manifest no longer lists it (offline, older catalog): still shown, so
    // it can be uninstalled and its configuration copied.
    assert_eq!(
        state(manifest_with(vec![]), "1.0.0", &MAC_ARM64),
        Some(ToolCatalogState::Installed {
            version: "1.0.0".into()
        })
    );
    // Installed and the entry is broken: stays installed, the problem is reported.
    let catalog = rig.catalog(manifest_with(vec![broken]), "1.0.0", &MAC_ARM64);
    assert!(catalog.tools[0].problem.is_some());
    assert!(matches!(
        catalog.tools[0].state,
        ToolCatalogState::Installed { .. }
    ));
    assert_eq!(
        catalog.tools[0].path,
        rig.tools.executable_path(MCP_TOOL_ID).unwrap()
    );
}

// --- install ----------------------------------------------------------------------------

fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A folder the way the installer makes one (0755 whatever the umask), for tests that lay out the
/// tools folder by hand.
fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn tool_dir(rig: &Rig) -> PathBuf {
    rig.tools.root().join("tablex-mcp")
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "tablex-mcp.exe"
    } else {
        "tablex-mcp"
    }
}

#[test]
fn install_puts_the_executable_at_the_stable_path_with_its_record() {
    let rig = Rig::new();
    let bytes = b"\x7fELF pretend this is a server".to_vec();
    let seen = Mutex::new(Vec::<InstallProgress>::new());
    let sink = |progress: InstallProgress| seen.lock().unwrap().push(progress);
    let record = ToolInstaller::new(MAC_ARM64, &rig.tools)
        .install(
            &entry("1.2.3", &bytes),
            reader(&bytes),
            &AtomicBool::new(false),
            &sink,
        )
        .expect("install");

    let path = tool_dir(&rig).join(exe_name());
    assert_eq!(rig.tools.executable_path(MCP_TOOL_ID).unwrap(), path);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "executable bit set: {mode:o}");
    }
    assert_eq!(record.version, "1.2.3");
    assert_eq!(record.sha256, hex_sha256(&bytes));
    assert_eq!(record.size_bytes, bytes.len() as u64);
    assert_eq!(rig.tools.installed(MCP_TOOL_ID).unwrap(), Some(record));
    let on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tool_dir(&rig).join("installed.json")).unwrap())
            .unwrap();
    assert_eq!(on_disk["version"], "1.2.3");
    assert_eq!(on_disk["sha256"], hex_sha256(&bytes));
    assert_eq!(
        names(&tool_dir(&rig)),
        ["installed.json", exe_name()],
        "no temp file is left"
    );
    let steps: Vec<InstallStep> = seen.lock().unwrap().iter().map(|p| p.step).collect();
    assert_eq!(steps.first(), Some(&InstallStep::Downloading));
    assert_eq!(steps.last(), Some(&InstallStep::VerifyingChecksum));
}

#[test]
fn an_update_replaces_the_file_at_the_same_path() {
    let rig = Rig::new();
    let (v1, v2) = (b"first build".to_vec(), b"second build, longer".to_vec());
    rig.install(&entry("1.0.0", &v1), &v1).unwrap();
    let path = rig.tools.executable_path(MCP_TOOL_ID).unwrap();
    rig.install(&entry("2.0.0", &v2), &v2).unwrap();
    assert_eq!(
        rig.tools.executable_path(MCP_TOOL_ID).unwrap(),
        path,
        "the path survives"
    );
    assert_eq!(std::fs::read(&path).unwrap(), v2);
    assert_eq!(
        rig.tools.installed(MCP_TOOL_ID).unwrap().unwrap().version,
        "2.0.0"
    );
    assert_eq!(names(&tool_dir(&rig)).len(), 2);
}

/// Yields `bytes` and fails (or does something) after `after` bytes.
struct Flaky {
    data: Cursor<Vec<u8>>,
    after: usize,
    served: usize,
    on_stall: Box<dyn FnMut() -> std::io::Result<()> + Send>,
}

impl Read for Flaky {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.served >= self.after {
            (self.on_stall)()?;
        }
        let limit = buffer
            .len()
            .min(self.after.saturating_sub(self.served).max(1));
        let read = self.data.read(&mut buffer[..limit])?;
        self.served += read;
        Ok(read)
    }
}

fn flaky(
    bytes: &[u8],
    after: usize,
    on_stall: impl FnMut() -> std::io::Result<()> + Send + 'static,
) -> impl FnOnce(
    &engine_driver_host::DriverArtifact,
    engine_driver_host::DownloadTransport,
) -> Result<Box<dyn Read>, DownloadError> {
    let source = Flaky {
        data: Cursor::new(bytes.to_vec()),
        after,
        served: 0,
        on_stall: Box::new(on_stall),
    };
    move |_, _| Ok(Box::new(source))
}

#[test]
fn a_failed_download_leaves_the_old_binary_and_nothing_else() {
    let rig = Rig::new();
    let (v1, v2) = (b"first build".to_vec(), vec![9_u8; 300_000]);
    rig.install(&entry("1.0.0", &v1), &v1).unwrap();
    let before = names(&tool_dir(&rig));

    let result = ToolInstaller::new(MAC_ARM64, &rig.tools).install(
        &entry("2.0.0", &v2),
        flaky(&v2, 100_000, || {
            Err(std::io::Error::other("connection reset"))
        }),
        &AtomicBool::new(false),
        &NoProgress,
    );
    assert!(
        matches!(result, Err(ToolError::Download(DownloadError::Io(_)))),
        "{result:?}"
    );
    assert_eq!(names(&tool_dir(&rig)), before);
    assert_eq!(
        std::fs::read(rig.tools.executable_path(MCP_TOOL_ID).unwrap()).unwrap(),
        v1
    );
    assert_eq!(
        rig.tools.installed(MCP_TOOL_ID).unwrap().unwrap().version,
        "1.0.0"
    );
}

#[test]
fn a_hash_or_size_mismatch_leaves_nothing_behind() {
    let rig = Rig::new();
    let good = vec![5_u8; 200_000];
    let entry_for = |bytes: &[u8]| entry("1.0.0", bytes);

    // Same size, other bytes: the hash differs.
    let mut other = good.clone();
    other[100] ^= 1;
    let result = rig.install(&entry_for(&good), &other);
    assert!(
        matches!(result, Err(ToolError::ChecksumMismatch { .. })),
        "{result:?}"
    );
    assert!(
        names(&tool_dir(&rig)).is_empty(),
        "{:?}",
        names(&tool_dir(&rig))
    );

    // Short and long bodies are caught by the size cap while streaming.
    let result = rig.install(&entry_for(&good), &good[..good.len() - 1]);
    assert!(
        matches!(
            result,
            Err(ToolError::Download(DownloadError::SizeMismatch { .. }))
        ),
        "{result:?}"
    );
    let mut long = good.clone();
    long.push(0);
    let result = rig.install(&entry_for(&good), &long);
    assert!(
        matches!(
            result,
            Err(ToolError::Download(DownloadError::TooLarge { .. }))
        ),
        "{result:?}"
    );
    assert!(
        names(&tool_dir(&rig)).is_empty(),
        "{:?}",
        names(&tool_dir(&rig))
    );
    assert!(
        !tool_dir(&rig).exists(),
        "the folder this install created is gone too"
    );
    assert_eq!(rig.tools.installed(MCP_TOOL_ID).unwrap(), None);
}

#[test]
fn cancel_mid_download_or_before_the_commit_leaves_nothing_behind() {
    let rig = Rig::new();
    let (v1, v2) = (b"first build".to_vec(), vec![3_u8; 400_000]);
    rig.install(&entry("1.0.0", &v1), &v1).unwrap();
    let before = names(&tool_dir(&rig));

    // Mid download: the flag goes up while bytes are still arriving.
    let flag = std::sync::Arc::new(AtomicBool::new(false));
    let raise = flag.clone();
    let result = ToolInstaller::new(MAC_ARM64, &rig.tools).install(
        &entry("2.0.0", &v2),
        flaky(&v2, 128 * 1024, move || {
            raise.store(true, Ordering::Release);
            Ok(())
        }),
        &flag,
        &NoProgress,
    );
    assert!(
        result.as_ref().is_err_and(ToolError::is_cancelled),
        "{result:?}"
    );
    assert_eq!(names(&tool_dir(&rig)), before);

    // After the last byte, during verification: still nothing is committed.
    let flag = AtomicBool::new(false);
    let sink = |progress: InstallProgress| {
        if progress.step == InstallStep::VerifyingChecksum {
            flag.store(true, Ordering::Release);
        }
    };
    let result = ToolInstaller::new(MAC_ARM64, &rig.tools).install(
        &entry("2.0.0", &v2),
        reader(&v2),
        &flag,
        &sink,
    );
    assert!(
        result.as_ref().is_err_and(ToolError::is_cancelled),
        "{result:?}"
    );
    assert_eq!(names(&tool_dir(&rig)), before);
    assert_eq!(
        std::fs::read(rig.tools.executable_path(MCP_TOOL_ID).unwrap()).unwrap(),
        v1
    );
    assert_eq!(
        rig.tools.installed(MCP_TOOL_ID).unwrap().unwrap().version,
        "1.0.0"
    );
}

/// The child half of [`a_crash_between_download_and_rename_keeps_the_old_binary`]: does nothing
/// unless the parent asks, and then dies (SIGABRT, no destructors) the moment the download is
/// complete and verification starts, before the rename.
#[test]
fn crash_child_installs_then_aborts() {
    let Ok(root) = std::env::var("TOOL_CRASH_ROOT") else {
        return;
    };
    let v2 = vec![4_u8; 400_000];
    let sink = |progress: InstallProgress| {
        if progress.step == InstallStep::VerifyingChecksum {
            std::process::abort();
        }
    };
    let _ = ToolInstaller::new(MAC_ARM64, &ToolStore::new(root)).install(
        &entry("2.0.0", &v2),
        reader(&v2),
        &AtomicBool::new(false),
        &sink,
    );
}

#[test]
fn a_crash_between_download_and_rename_keeps_the_old_binary() {
    let rig = Rig::new();
    let (v1, v2) = (b"first build".to_vec(), vec![4_u8; 400_000]);
    rig.install(&entry("1.0.0", &v1), &v1).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child_installs_then_aborts", "--nocapture"])
        .env("TOOL_CRASH_ROOT", rig.tools.root())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "the child must have died: {status}");

    // The complete download is still sitting in the temp file; the stable path is untouched.
    let left: Vec<String> = names(&tool_dir(&rig))
        .into_iter()
        .filter(|name| name.ends_with(".staging"))
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(
        std::fs::metadata(tool_dir(&rig).join(&left[0]))
            .unwrap()
            .len(),
        v2.len() as u64
    );
    assert_eq!(
        std::fs::read(rig.tools.executable_path(MCP_TOOL_ID).unwrap()).unwrap(),
        v1
    );
    let record = rig
        .tools
        .installed(MCP_TOOL_ID)
        .unwrap()
        .expect("still installed");
    assert_eq!(
        (record.version.as_str(), record.sha256),
        ("1.0.0", hex_sha256(&v1))
    );

    // The next install goes through, and uninstall clears the crash leftover too.
    rig.install(&entry("2.0.0", &v2), &v2).unwrap();
    assert_eq!(
        rig.tools.installed(MCP_TOOL_ID).unwrap().unwrap().version,
        "2.0.0"
    );
    assert!(ToolInstaller::new(MAC_ARM64, &rig.tools)
        .uninstall(MCP_TOOL_ID)
        .unwrap());
    assert!(!tool_dir(&rig).exists(), "{:?}", names(&tool_dir(&rig)));
}

#[test]
fn a_half_replaced_install_reads_as_not_installed_until_reinstalled() {
    // Crash between the executable rename and the record rename: new file, old record.
    let rig = Rig::new();
    let (v1, v2) = (b"first build".to_vec(), b"second build, longer".to_vec());
    rig.install(&entry("1.0.0", &v1), &v1).unwrap();
    std::fs::write(rig.tools.executable_path(MCP_TOOL_ID).unwrap(), &v2).unwrap();
    assert_eq!(rig.tools.installed(MCP_TOOL_ID).unwrap(), None);
    rig.install(&entry("2.0.0", &v2), &v2).unwrap();
    assert_eq!(
        rig.tools.installed(MCP_TOOL_ID).unwrap().unwrap().version,
        "2.0.0"
    );
}

#[test]
fn the_url_policy_decides_before_anything_is_downloaded() {
    let rig = Rig::new();
    let bytes = b"build".to_vec();
    let with_url = |url: &str| {
        let mut entry = entry("1.0.0", &bytes);
        entry.artifacts[0].url = url.to_string();
        entry
    };
    let attempt = |policy: TrustPolicy, url: &str| {
        let called = AtomicBool::new(false);
        let result = ToolInstaller::new(MAC_ARM64, &rig.tools)
            .with_trust_policy(policy)
            .install(
                &with_url(url),
                |_, _| {
                    called.store(true, Ordering::Release);
                    Ok(Box::new(Cursor::new(bytes.clone())) as Box<dyn Read>)
                },
                &AtomicBool::new(false),
                &NoProgress,
            );
        (result, called.load(Ordering::Acquire))
    };

    for url in [
        "http://example.com/t",
        "file:///tmp/t",
        "ftp://h/t",
        "no-scheme",
    ] {
        let (result, called) = attempt(TrustPolicy::Strict, url);
        assert!(
            matches!(result, Err(ToolError::Download(_))),
            "{url}: {result:?}"
        );
        assert!(!called, "{url}: nothing was opened");
    }
    let (result, called) = attempt(TrustPolicy::Developer, "http://example.com/t");
    assert!(
        result.is_err() && !called,
        "plain http to a remote host is never allowed"
    );
    assert!(attempt(TrustPolicy::Developer, "file:///tmp/t").0.is_ok());
    assert!(attempt(TrustPolicy::Strict, "http://127.0.0.1:8080/t")
        .0
        .is_ok());
    assert!(names(&tool_dir(&rig)).contains(&"installed.json".to_string()));
}

#[test]
fn a_target_without_a_build_and_a_bad_id_are_typed_errors() {
    let rig = Rig::new();
    let bytes = b"build".to_vec();
    let result = ToolInstaller::new(MAC_X86, &rig.tools).install(
        &entry("1.0.0", &bytes),
        reader(&bytes),
        &AtomicBool::new(false),
        &NoProgress,
    );
    assert!(
        matches!(result, Err(ToolError::NoBuild { .. })),
        "{result:?}"
    );
    let mut evil = entry("1.0.0", &bytes);
    evil.id = "../escape".into();
    let result = rig.install(&evil, &bytes);
    assert!(matches!(result, Err(ToolError::InvalidId(_))), "{result:?}");
    assert!(!rig.tools.root().exists() || names(rig.tools.root()).is_empty());
}

#[test]
fn uninstall_removes_exactly_what_was_installed() {
    let rig = Rig::new();
    let bytes = b"a build".to_vec();
    let installer = ToolInstaller::new(MAC_ARM64, &rig.tools);
    assert!(
        !installer.uninstall(MCP_TOOL_ID).unwrap(),
        "nothing to remove"
    );

    rig.install(&entry("1.0.0", &bytes), &bytes).unwrap();
    // Not ours: a user file in the tool's folder, a sibling tool, an unrelated file in the root.
    std::fs::write(tool_dir(&rig).join("notes.txt"), "mine").unwrap();
    std::fs::create_dir_all(rig.tools.root().join("other-tool")).unwrap();
    std::fs::write(rig.tools.root().join("other-tool").join("other-tool"), "x").unwrap();
    std::fs::write(rig.tools.root().join("README"), "x").unwrap();
    // Ours: a leftover temp file of a crashed install.
    std::fs::write(tool_dir(&rig).join(".download.1-1.staging"), "partial").unwrap();

    assert!(installer.uninstall(MCP_TOOL_ID).unwrap());
    assert_eq!(
        names(&tool_dir(&rig)),
        ["notes.txt"],
        "only the user's file stays"
    );
    assert_eq!(rig.tools.installed(MCP_TOOL_ID).unwrap(), None);
    assert_eq!(
        names(rig.tools.root()),
        ["README", "other-tool", "tablex-mcp"]
    );
    assert!(rig
        .tools
        .root()
        .join("other-tool")
        .join("other-tool")
        .exists());

    // With nothing else in the folder the folder goes too.
    std::fs::remove_file(tool_dir(&rig).join("notes.txt")).unwrap();
    rig.install(&entry("1.0.0", &bytes), &bytes).unwrap();
    assert!(installer.uninstall(MCP_TOOL_ID).unwrap());
    assert!(!tool_dir(&rig).exists());
    assert!(!installer.uninstall(MCP_TOOL_ID).unwrap());
}

#[test]
fn a_damaged_record_is_reported_and_a_reinstall_repairs_it() {
    let rig = Rig::new();
    let bytes = b"a build".to_vec();
    rig.install(&entry("1.0.0", &bytes), &bytes).unwrap();
    std::fs::write(tool_dir(&rig).join("installed.json"), "{ not json").unwrap();
    assert!(matches!(
        rig.tools.installed(MCP_TOOL_ID),
        Err(ToolError::Record(_))
    ));
    let catalog = rig.catalog(
        manifest_with(vec![tool_json("1.0.0", &bytes)]),
        "1.0.0",
        &MAC_ARM64,
    );
    assert_eq!(catalog.tools[0].state, ToolCatalogState::Available);
    assert!(catalog.tools[0]
        .problem
        .as_deref()
        .is_some_and(|p| p.contains("installed.json")));
    rig.install(&entry("1.0.0", &bytes), &bytes).unwrap();
    assert!(rig.tools.installed(MCP_TOOL_ID).unwrap().is_some());
}

#[cfg(unix)]
mod links {
    use super::*;
    use std::os::unix::fs::symlink;

    fn victim(rig: &Rig) -> PathBuf {
        let victim = rig._temp.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("precious"), "keep me").unwrap();
        victim
    }

    fn untouched(victim: &Path) {
        assert_eq!(names(victim), ["precious"]);
        assert_eq!(
            std::fs::read_to_string(victim.join("precious")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn a_link_at_the_tools_directory_or_the_tool_folder_is_not_followed() {
        for at_root in [true, false] {
            let rig = Rig::new();
            let victim = victim(&rig);
            let bytes = b"build".to_vec();
            if at_root {
                symlink(&victim, rig.tools.root()).unwrap();
            } else {
                private_dir(rig.tools.root());
                symlink(&victim, tool_dir(&rig)).unwrap();
            }
            let result = rig.install(&entry("1.0.0", &bytes), &bytes);
            assert!(
                matches!(result, Err(ToolError::LinkRefused { .. })),
                "{at_root}: {result:?}"
            );
            assert!(matches!(
                rig.tools.installed(MCP_TOOL_ID),
                Err(ToolError::LinkRefused { .. })
            ));
            let result = ToolInstaller::new(MAC_ARM64, &rig.tools).uninstall(MCP_TOOL_ID);
            assert!(
                matches!(result, Err(ToolError::LinkRefused { .. })),
                "{at_root}: {result:?}"
            );
            untouched(&victim);
        }
    }

    #[test]
    fn a_link_planted_at_the_executable_or_the_record_is_not_followed() {
        for name in ["tablex-mcp", "installed.json"] {
            let rig = Rig::new();
            let victim = victim(&rig);
            let bytes = b"build".to_vec();
            private_dir(&tool_dir(&rig));
            let target = victim.join("precious");
            symlink(&target, tool_dir(&rig).join(name)).unwrap();

            let result = rig.install(&entry("1.0.0", &bytes), &bytes);
            assert!(
                matches!(result, Err(ToolError::LinkRefused { .. })),
                "{name}: {result:?}"
            );
            let result = ToolInstaller::new(MAC_ARM64, &rig.tools).uninstall(MCP_TOOL_ID);
            assert!(
                matches!(result, Err(ToolError::LinkRefused { .. })),
                "{name}: {result:?}"
            );
            untouched(&victim);
            assert!(std::fs::symlink_metadata(tool_dir(&rig).join(name))
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }

    #[test]
    fn a_file_where_the_folder_should_be_is_refused() {
        let rig = Rig::new();
        private_dir(rig.tools.root());
        std::fs::write(tool_dir(&rig), "i am a file").unwrap();
        let bytes = b"build".to_vec();
        assert!(matches!(
            rig.install(&entry("1.0.0", &bytes), &bytes),
            Err(ToolError::UnexpectedEntry { .. })
        ));
    }
}

/// The release tooling and the host agree: what `driver_manifest.py tool-entry` + `merge` write
/// parses into a known, usable tool. Skipped where python3 is not installed.
#[test]
fn the_manifest_script_writes_what_the_host_reads() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/driver_manifest.py");
    let python = |args: &[&str]| {
        std::process::Command::new("python3")
            .arg(&script)
            .args(args)
            .output()
    };
    if python(&["--help"]).is_err() {
        eprintln!("python3 not found; skipped");
        return;
    }
    let temp = TempDir::new().unwrap();
    let binary = temp.path().join("tablex-mcp");
    std::fs::write(&binary, b"pretend this is the server").unwrap();
    let manifest = temp.path().join("manifest.json");
    for target in ["macos-aarch64", "linux-x86_64", "windows-x86_64"] {
        let out = temp.path().join(format!("{target}.json"));
        let result = python(&[
            "tool-entry",
            "--binary",
            binary.to_str().unwrap(),
            "--version",
            "0.1.0-dev.abcd1234",
            "--target",
            target,
            "--url",
            &format!("https://example.com/tablex-mcp-{target}"),
            "--out",
            out.to_str().unwrap(),
        ])
        .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let merged = python(&[
        "merge",
        "--out",
        manifest.to_str().unwrap(),
        temp.path().join("macos-aarch64.json").to_str().unwrap(),
        temp.path().join("linux-x86_64.json").to_str().unwrap(),
        temp.path().join("windows-x86_64.json").to_str().unwrap(),
    ])
    .unwrap();
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );

    let parsed: DriverManifest =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    let tool = known(&parsed.tools[0]);
    assert_eq!(
        (tool.version.as_str(), tool.artifacts.len()),
        ("0.1.0-dev.abcd1234", 3)
    );
    assert_eq!(
        tool.artifacts[0].sha256,
        hex_sha256(b"pretend this is the server")
    );
    assert_eq!(tool.artifact_for_target(&LINUX_X86).unwrap().size_bytes, 26);
    assert!(tool.artifact_for_target(&WINDOWS_X86).is_some());
}
