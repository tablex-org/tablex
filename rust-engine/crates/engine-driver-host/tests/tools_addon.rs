//! The MongoDB Shell plugin as an add-on of the MongoDB driver: the manifest entry (`kind:
//! "mongoShell"`, `requires`), the install refused while the driver is missing or too old, the
//! catalog and the uninstall. No network: the byte source is an in-memory reader.

use engine_driver_host::{
    DownloadError, DriverManifest, DriverRequirement, DriverTarget, InstalledDriverRecord,
    InstalledDriverStore, ManifestFetcher, ManifestTool, NoProgress, RegistryError,
    RegistryService, RequirementStatus, TargetArch, TargetOs, ToolCatalogState, ToolError,
    ToolInstaller, ToolKind, ToolManifestEntry, ToolManifestError, ToolStore,
    MCP_TOOL_ID, MONGODB_DRIVER_ID, MONGO_SHELL_TOOL_ID,
};
use futures::executor::block_on;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::{Cursor, Read};
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use tempfile::TempDir;

const MAC_ARM64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::Aarch64);

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn shell_json(version: &str, minimum: &str, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "id": MONGO_SHELL_TOOL_ID,
        "kind": "mongoShell",
        "displayName": "MongoDB Shell",
        "version": version,
        "description": "Runs mongosh-style scripts in a MongoDB tab.",
        "requires": [{"driver": "mongodb", "minVersion": minimum}],
        "artifacts": [{
            "target": "macos-aarch64",
            "arch": "macos-aarch64",
            "url": "https://example.com/tablex-mongo-shell-macos-aarch64",
            "sha256": hex_sha256(bytes),
            "sizeBytes": bytes.len(),
        }],
    })
}

fn mcp_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "id": "tablex-mcp",
        "kind": "mcpServer",
        "displayName": "TableX MCP server",
        "version": "1.0.0",
        "artifacts": [{
            "target": "macos-aarch64",
            "url": "https://example.com/tablex-mcp-macos-aarch64",
            "sha256": hex_sha256(bytes),
            "sizeBytes": bytes.len(),
        }],
    })
}

fn manifest_with(tools: Vec<serde_json::Value>) -> Vec<u8> {
    serde_json::json!({"schemaVersion": 2, "drivers": [], "tools": tools})
        .to_string()
        .into_bytes()
}

fn parse(tools: Vec<serde_json::Value>) -> DriverManifest {
    serde_json::from_slice(&manifest_with(tools)).expect("a manifest always parses")
}

fn shell_entry(minimum: &str, bytes: &[u8]) -> ToolManifestEntry {
    match parse(vec![shell_json("0.1.0", minimum, bytes)]).tools.remove(0) {
        ManifestTool::Known(entry) => entry,
        other => panic!("expected a known tool, got {other:?}"),
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

    fn install_driver(&self, version: &str, disabled: bool) {
        self.drivers
            .save(InstalledDriverRecord {
                id: MONGODB_DRIVER_ID.into(),
                version: version.into(),
                disabled,
                ..Default::default()
            })
            .expect("driver record");
    }

    fn install(&self, entry: &ToolManifestEntry, bytes: &[u8]) -> Result<(), ToolError> {
        ToolInstaller::new(MAC_ARM64, &self.tools)
            .with_drivers(&self.drivers)
            .install(entry, reader(bytes), &AtomicBool::new(false), &NoProgress)
            .map(|_| ())
    }
}

#[test]
fn the_plugin_entry_parses_with_its_requirement_and_round_trips() {
    let bytes = b"plugin";
    let manifest = parse(vec![shell_json("0.1.0", "0.2.0", bytes), mcp_json(bytes)]);
    let [ManifestTool::Known(shell), ManifestTool::Known(mcp)] = &manifest.tools[..] else {
        panic!("both are known: {:?}", manifest.tools)
    };
    assert_eq!(shell.kind, ToolKind::MongoShell);
    assert_eq!(shell.id, MONGO_SHELL_TOOL_ID);
    assert_eq!(
        shell.requires,
        [DriverRequirement {
            driver: "mongodb".into(),
            min_version: "0.2.0".into()
        }]
    );
    assert!(mcp.requires.is_empty(), "a tool without `requires` still parses");

    // The signed text of a manifest is what is read; writing it back keeps the field.
    let written = serde_json::to_value(shell).unwrap();
    assert_eq!(written["kind"], "mongoShell");
    assert_eq!(written["requires"][0]["minVersion"], "0.2.0");
    assert!(
        serde_json::to_value(mcp).unwrap().get("requires").is_none(),
        "no empty `requires` is written for a free-standing tool"
    );
}

#[test]
fn old_manifests_and_unknown_kinds_still_parse() {
    // A manifest of an app before the plugin: only the MCP server, no `requires` anywhere.
    let old = parse(vec![mcp_json(b"x")]);
    assert!(matches!(&old.tools[..], [ManifestTool::Known(entry)] if entry.id == MCP_TOOL_ID));

    // A newer manifest: a kind and an id this build does not know are ignored, not failed.
    let mut future_kind = shell_json("0.1.0", "0.1.0", b"x");
    future_kind["kind"] = "wasmPlugin".into();
    let mut future_id = shell_json("0.1.0", "0.1.0", b"x");
    future_id["id"] = "tablex-other-plugin".into();
    // The right kind under the other tool's id is not the plugin either.
    let mut mixed_up = shell_json("0.1.0", "0.1.0", b"x");
    mixed_up["id"] = MCP_TOOL_ID.into();
    let manifest = parse(vec![future_kind, future_id, mixed_up, shell_json("0.1.0", "0.1.0", b"x")]);
    let ignored = manifest
        .tools
        .iter()
        .filter(|tool| matches!(tool, ManifestTool::Ignored(_)))
        .count();
    assert_eq!(ignored, 3);
    assert!(matches!(&manifest.tools[3], ManifestTool::Known(entry) if entry.kind == ToolKind::MongoShell));

    // The plugin without a requirement on the MongoDB driver (none, or another driver) is unusable.
    for requires in [serde_json::json!([]), serde_json::json!([{"driver": "postgresql", "minVersion": "1.0"}])] {
        let mut lone = shell_json("0.1.0", "0.1.0", b"x");
        lone["requires"] = requires;
        let manifest = parse(vec![lone]);
        assert!(
            matches!(
                &manifest.tools[0],
                ManifestTool::Invalid { id, error: ToolManifestError::MissingRequirement(driver), .. }
                    if id == MONGO_SHELL_TOOL_ID && driver == "mongodb"
            ),
            "{:?}",
            manifest.tools[0]
        );
    }
    let mut absent = shell_json("0.1.0", "0.1.0", b"x");
    absent.as_object_mut().unwrap().remove("requires");
    assert!(matches!(&parse(vec![absent]).tools[0], ManifestTool::Invalid { .. }), "no `requires` at all");

    // The kind this build does know, with a requirement it cannot use, fails that tool only.
    for bad in [
        serde_json::json!([{"driver": "mongodb"}]),
        serde_json::json!([{"driver": "../x", "minVersion": "1.0"}]),
        serde_json::json!([{"driver": "mongodb", "minVersion": "soon"}]),
    ] {
        let mut broken = shell_json("0.1.0", "0.1.0", b"x");
        broken["requires"] = bad;
        let manifest = parse(vec![broken]);
        assert!(
            matches!(
                &manifest.tools[0],
                ManifestTool::Invalid { id, error, .. }
                    if id == MONGO_SHELL_TOOL_ID
                        && matches!(error, ToolManifestError::Malformed(_) | ToolManifestError::InvalidRequirement(_))
            ),
            "{:?}",
            manifest.tools[0]
        );
    }
}

#[test]
fn requirements_compare_the_leading_numbers_of_the_versions() {
    let requirement = DriverRequirement {
        driver: "mongodb".into(),
        min_version: "1.2.0".into(),
    };
    let check = |installed: Option<&str>| requirement.check(installed);
    assert_eq!(check(None), RequirementStatus::Missing);
    assert_eq!(
        check(Some("1.1.9")),
        RequirementStatus::TooOld {
            installed: "1.1.9".into()
        }
    );
    for met in ["1.2.0", "1.2", "1.10.0", "2.0.0", "v1.3", "1.2.0-dev.ab12cd34", "1.2.0+build"] {
        assert_eq!(check(Some(met)), RequirementStatus::Met, "{met}");
    }
    assert!(
        matches!(check(Some("1.1.0-dev.ffffffff")), RequirementStatus::TooOld { .. }),
        "a development build of an older release is older"
    );
    // A driver imported from a local file has no version to compare: not refused for that.
    assert_eq!(check(Some("local-1a2b3c4d")), RequirementStatus::Met);
}

#[test]
fn the_install_is_refused_while_the_driver_is_missing_disabled_or_too_old() {
    let rig = Rig::new();
    let bytes = b"plugin build";
    let entry = shell_entry("0.2.0", bytes);

    let refusal = |rig: &Rig| match rig.install(&entry, bytes) {
        Err(ToolError::RequiresDriver {
            requirement, status, ..
        }) => (requirement.driver, requirement.min_version, status),
        other => panic!("expected the typed refusal, got {other:?}"),
    };
    // No driver at all.
    assert_eq!(
        refusal(&rig),
        ("mongodb".into(), "0.2.0".into(), RequirementStatus::Missing)
    );
    // Older than the minimum.
    rig.install_driver("0.1.9", false);
    assert_eq!(
        refusal(&rig),
        (
            "mongodb".into(),
            "0.2.0".into(),
            RequirementStatus::TooOld {
                installed: "0.1.9".into()
            }
        )
    );
    // Recent enough but disabled: it cannot serve the plugin.
    rig.install_driver("0.2.0", true);
    assert_eq!(refusal(&rig).2, RequirementStatus::Missing);
    // The refusal is worded for the dialog.
    let message = rig.install(&entry, bytes).unwrap_err().to_string();
    assert!(message.contains("MongoDB Shell") && message.contains("mongodb"), "{message}");
    // Nothing was written for any of the refusals.
    assert!(
        !rig.tools.root().exists() || std::fs::read_dir(rig.tools.root()).unwrap().next().is_none(),
        "a refused install leaves no folder"
    );
    // An installer that was never told about the drivers cannot show the requirement holds.
    assert!(matches!(
        ToolInstaller::new(MAC_ARM64, &rig.tools).install(
            &entry,
            reader(bytes),
            &AtomicBool::new(false),
            &NoProgress
        ),
        Err(ToolError::RequiresDriver { .. })
    ));

    // Enabled and recent enough: installs.
    rig.install_driver("0.2.0", false);
    rig.install(&entry, bytes).expect("install");
    assert!(rig.tools.installed(MONGO_SHELL_TOOL_ID).unwrap().is_some());
}

#[test]
fn an_installed_plugin_remembers_its_requirement_updates_and_uninstalls() {
    let rig = Rig::new();
    rig.install_driver("0.3.0", false);
    let (v1, v2) = (b"plugin one".to_vec(), b"plugin two, longer".to_vec());
    let entry = shell_entry("0.2.0", &v1);
    rig.install(&entry, &v1).expect("install");
    let record = rig.tools.installed(MONGO_SHELL_TOOL_ID).unwrap().expect("installed");
    assert_eq!(record.requires, entry.requires);
    let executable = rig.tools.executable_path(MONGO_SHELL_TOOL_ID).unwrap();
    assert_eq!(std::fs::read(&executable).unwrap(), v1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&executable).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "executable bit set: {mode:o}");
    }

    // An update replaces the file at the same stable path; a record without `requires` (an
    // install of an app before the field) still reads.
    let mut next = shell_entry("0.2.0", &v2);
    next.version = "0.2.0".into();
    rig.install(&next, &v2).expect("update");
    assert_eq!(std::fs::read(&executable).unwrap(), v2);
    let text = std::fs::read_to_string(executable.with_file_name("installed.json")).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value.as_object_mut().unwrap().remove("requires");
    std::fs::write(executable.with_file_name("installed.json"), value.to_string()).unwrap();
    let old = rig.tools.installed(MONGO_SHELL_TOOL_ID).unwrap().expect("still installed");
    assert!(old.requires.is_empty());

    let installer = ToolInstaller::new(MAC_ARM64, &rig.tools);
    assert!(installer.uninstall(MONGO_SHELL_TOOL_ID).unwrap());
    assert_eq!(rig.tools.installed(MONGO_SHELL_TOOL_ID).unwrap(), None);
    assert!(!executable.exists());
    assert!(!rig.tools.root().join(MONGO_SHELL_TOOL_ID).exists(), "the folder goes with it");
    assert!(!installer.uninstall(MONGO_SHELL_TOOL_ID).unwrap());
}

struct OneShot(Mutex<Vec<Vec<u8>>>);

impl ManifestFetcher for OneShot {
    fn fetch<'a>(
        &'a self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, RegistryError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().remove(0)) })
    }
}

fn catalog(rig: &Rig, manifest: Vec<u8>) -> engine_driver_host::FullCatalog {
    let fetcher = OneShot(Mutex::new(vec![manifest]));
    block_on(
        RegistryService::new("file:///m.json", &fetcher, &rig.drivers)
            .with_app_version("1.0.0")
            .catalog_with_tools(&MAC_ARM64, &rig.tools),
    )
    .expect("catalog")
}

#[test]
fn the_catalog_lists_the_plugin_next_to_the_mcp_server_and_keeps_an_orphan() {
    let rig = Rig::new();
    let bytes = b"plugin build";

    let listing = catalog(&rig, manifest_with(vec![mcp_json(bytes), shell_json("0.1.0", "0.2.0", bytes)]));
    let kinds: Vec<(ToolKind, bool)> = listing
        .tools
        .iter()
        .map(|item| (item.kind, matches!(item.state, ToolCatalogState::Available)))
        .collect();
    assert_eq!(kinds, [(ToolKind::McpServer, true), (ToolKind::MongoShell, true)]);
    let shell = &listing.tools[1];
    assert_eq!(shell.id, MONGO_SHELL_TOOL_ID);
    assert_eq!(shell.requires().len(), 1);
    assert_eq!(shell.path, rig.tools.executable_path(MONGO_SHELL_TOOL_ID).unwrap());

    // Installed, then the manifest stops listing it (offline, older catalog, withdrawn): the row
    // stays with the requirement the install recorded, so it can say what it needs.
    rig.install_driver("0.2.0", false);
    rig.install(&shell_entry("0.2.0", bytes), bytes).unwrap();
    let orphan = catalog(&rig, manifest_with(vec![]));
    let item = &orphan.tools[0];
    assert!(item.entry.is_none());
    assert_eq!(item.kind, ToolKind::MongoShell);
    assert!(matches!(item.state, ToolCatalogState::Installed { .. }));
    assert_eq!(item.requires()[0].min_version, "0.2.0");
    assert_eq!(
        item.requires()[0].check(Some("0.1.0")),
        RequirementStatus::TooOld {
            installed: "0.1.0".into()
        }
    );

    // The plugin's entry being broken never fails the drivers or the other tool.
    let mut broken = shell_json("0.1.0", "0.2.0", bytes);
    broken["artifacts"][0]["sha256"] = "zz".into();
    let listing = catalog(&rig, manifest_with(vec![mcp_json(bytes), broken]));
    assert_eq!(listing.tools.len(), 2);
    assert!(listing.tools.iter().any(|item| item.id == MCP_TOOL_ID));
}
