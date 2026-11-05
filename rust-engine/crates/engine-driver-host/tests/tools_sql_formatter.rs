//! The SQL formatter tool (`tablex-sqruff`) in the manifest and the installer: a free-standing
//! tool (no driver requirement) that carries its licence and source repository, installs at the
//! stable path, shows in the catalog next to the others and uninstalls. No network.

use engine_driver_host::{
    DriverManifest, DriverTarget, InstalledDriverStore, ManifestFetcher, ManifestTool, NoProgress,
    RegistryError, RegistryService, TargetArch, TargetOs, ToolCatalogState, ToolInstaller,
    ToolKind, ToolManifestEntry, ToolStore, MCP_TOOL_ID, SQRUFF_TOOL_ID,
};
use futures::executor::block_on;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;

const MAC_ARM64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::Aarch64);

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn formatter_json(version: &str, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "id": SQRUFF_TOOL_ID,
        "kind": "sqlFormatter",
        "displayName": "SQL Formatter (sqruff)",
        "version": version,
        "description": "Formats SQL with sqruff in a separate process.",
        "license": "Apache-2.0",
        "sourceRepo": "https://github.com/quarylabs/sqruff",
        "artifacts": [{
            "target": "macos-aarch64",
            "arch": "arm64",
            "url": "https://example.com/tablex-sqruff-macos-aarch64",
            "sha256": hex_sha256(bytes),
            "sizeBytes": bytes.len(),
        }],
    })
}

fn manifest_with(tools: Vec<serde_json::Value>) -> Vec<u8> {
    serde_json::json!({"schemaVersion": 2, "drivers": [], "tools": tools}).to_string().into_bytes()
}

fn entry(version: &str, bytes: &[u8]) -> ToolManifestEntry {
    let manifest: DriverManifest = serde_json::from_slice(&manifest_with(vec![formatter_json(version, bytes)])).unwrap();
    match manifest.tools.into_iter().next() {
        Some(ManifestTool::Known(entry)) => entry,
        other => panic!("expected a known tool, got {other:?}"),
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

    fn install(&self, version: &str, bytes: &[u8]) {
        let bytes_owned = bytes.to_vec();
        ToolInstaller::new(MAC_ARM64, &self.tools)
            .with_drivers(&self.drivers)
            .install(
                &entry(version, bytes),
                move |_, _| Ok(Box::new(Cursor::new(bytes_owned))),
                &AtomicBool::new(false),
                &NoProgress,
            )
            .expect("installs");
    }
}

struct OneShot(Vec<u8>);

impl ManifestFetcher for OneShot {
    fn fetch<'a>(
        &'a self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, RegistryError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

fn catalog(rig: &Rig, manifest: Vec<u8>) -> engine_driver_host::FullCatalog {
    let fetcher = OneShot(manifest);
    block_on(
        RegistryService::new("file:///m.json", &fetcher, &rig.drivers)
            .with_app_version("1.0.0")
            .catalog_with_tools(&MAC_ARM64, &rig.tools),
    )
    .expect("catalog")
}

#[test]
fn the_entry_carries_its_licence_and_source_and_needs_no_driver() {
    let tool = entry("0.1.0", b"sqruff");
    assert_eq!(tool.kind, ToolKind::SqlFormatter);
    assert_eq!(tool.id, SQRUFF_TOOL_ID);
    assert!(tool.requires.is_empty());
    assert_eq!(tool.license.as_deref(), Some("Apache-2.0"));
    assert_eq!(tool.source_repo.as_deref(), Some("https://github.com/quarylabs/sqruff"));
    assert!(!ToolKind::SqlFormatter.is_addon() && ToolKind::MongoShell.is_addon());
    // Back to the wire with the same field names.
    let json = serde_json::to_value(&tool).unwrap();
    assert_eq!(json["license"], "Apache-2.0");
    assert_eq!(json["sourceRepo"], "https://github.com/quarylabs/sqruff");
    assert_eq!(json["kind"], "sqlFormatter");
}

#[test]
fn entries_without_the_new_fields_still_parse_and_stay_without_them() {
    // A manifest of an older release: the MCP server has neither field.
    let mcp = serde_json::json!({
        "id": "tablex-mcp", "kind": "mcpServer", "displayName": "TableX MCP server", "version": "1.0.0",
        "artifacts": [{"target": "macos-aarch64", "url": "https://example.com/m", "sha256": hex_sha256(b"m"), "sizeBytes": 1}],
    });
    let manifest: DriverManifest = serde_json::from_slice(&manifest_with(vec![mcp])).unwrap();
    let [ManifestTool::Known(mcp)] = &manifest.tools[..] else { panic!("known") };
    assert_eq!((mcp.license.as_deref(), mcp.source_repo.as_deref()), (None, None));
    let json = serde_json::to_value(mcp).unwrap();
    assert!(json.get("license").is_none() && json.get("sourceRepo").is_none(), "{json}");
}

#[test]
fn the_kind_and_the_id_must_belong_together() {
    let mut wrong = formatter_json("0.1.0", b"x");
    wrong["id"] = MCP_TOOL_ID.into();
    let manifest: DriverManifest = serde_json::from_slice(&manifest_with(vec![wrong])).unwrap();
    assert!(matches!(&manifest.tools[..], [ManifestTool::Ignored(_)]));
}

#[test]
fn install_update_and_uninstall_at_the_stable_path() {
    let rig = Rig::new();
    let first = b"sqruff build 1";
    rig.install("0.1.0", first);
    let path = rig.tools.executable_path(SQRUFF_TOOL_ID).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), first);
    assert_eq!(rig.tools.installed(SQRUFF_TOOL_ID).unwrap().expect("installed").version, "0.1.0");
    assert!(rig.tools.installed(SQRUFF_TOOL_ID).unwrap().is_some_and(|record| record.requires.is_empty()));

    let second = b"sqruff build 2, a little longer";
    rig.install("0.2.0", second);
    assert_eq!(rig.tools.executable_path(SQRUFF_TOOL_ID).unwrap(), path, "the path is stable");
    assert_eq!(std::fs::read(&path).unwrap(), second);

    let installer = ToolInstaller::new(MAC_ARM64, &rig.tools);
    assert!(installer.uninstall(SQRUFF_TOOL_ID).unwrap());
    assert_eq!(rig.tools.installed(SQRUFF_TOOL_ID).unwrap(), None);
    assert!(!rig.tools.root().join(SQRUFF_TOOL_ID).exists());
}

#[test]
fn the_catalog_lists_the_formatter_with_its_entry_and_an_update() {
    let rig = Rig::new();
    let build = b"sqruff build";
    let listing = catalog(&rig, manifest_with(vec![formatter_json("0.1.0", build)]));
    let [item] = &listing.tools[..] else { panic!("{:?}", listing.tools) };
    assert_eq!((item.kind, item.id.as_str()), (ToolKind::SqlFormatter, SQRUFF_TOOL_ID));
    assert!(matches!(item.state, ToolCatalogState::Available));
    assert!(item.requires().is_empty());
    assert_eq!(item.entry.as_ref().and_then(|entry| entry.license.as_deref()), Some("Apache-2.0"));

    rig.install("0.1.0", build);
    let listing = catalog(&rig, manifest_with(vec![formatter_json("0.2.0", b"newer")]));
    assert!(matches!(
        &listing.tools[0].state,
        ToolCatalogState::UpdateAvailable { installed, latest } if installed == "0.1.0" && latest == "0.2.0"
    ));
}
