//! Tools, second round: what the catalog trusts (cache, signature), what a damaged tools folder
//! can do (FIFO, size, link, permissions), and what a sweep may delete. No network.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use engine_driver_host::{
    DriverManifest, DriverTarget, InstalledDriverStore, ManifestFetcher, ManifestTool,
    ManifestTrust, NoProgress, RegistryError, RegistryService, TargetArch, TargetOs,
    ToolCatalogState, ToolError, ToolInstaller, ToolManifestEntry, ToolStore, TrustPolicy,
    MCP_TOOL_ID,
};
use futures::executor::block_on;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::future::Future;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use tempfile::TempDir;

const MAC: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::Aarch64);
const HTTPS: &str = "https://example.com/drivers-manifest.json";
const LOCAL: &str = "file:///tmp/dist/drivers-manifest.json";

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn tool_json(version: &str, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "id": "tablex-mcp", "kind": "mcpServer", "displayName": "TableX MCP server",
        "version": version,
        "artifacts": [{
            "target": "macos-aarch64", "url": "https://example.com/tablex-mcp",
            "sha256": hex(bytes), "sizeBytes": bytes.len(),
        }],
    })
}

/// A manifest with one driver (`sqlite`) and `tools`; `generated_at` makes it replay-safe.
fn manifest(generated_at: &str, tools: serde_json::Value) -> Vec<u8> {
    serde_json::json!({
        "schemaVersion": 2, "generatedAt": generated_at,
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

fn entry(version: &str, bytes: &[u8]) -> ToolManifestEntry {
    let parsed: DriverManifest = serde_json::from_slice(&manifest(
        "2026-10-14T00:00:00Z",
        serde_json::json!([tool_json(version, bytes)]),
    ))
    .unwrap();
    match parsed.tools.into_iter().next() {
        Some(ManifestTool::Known(entry)) => entry,
        other => panic!("{other:?}"),
    }
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[9; 32])
}

fn sign(bytes: &[u8]) -> Vec<u8> {
    format!("{}\n", STANDARD.encode(key().sign(bytes).to_bytes())).into_bytes()
}

fn trust(policy: TrustPolicy) -> ManifestTrust {
    ManifestTrust::new(&[key().verifying_key().to_bytes()], policy).unwrap()
}

/// Answers fetches from a queue, in order; an empty queue is a failed fetch.
struct Queue(Mutex<VecDeque<Result<Vec<u8>, RegistryError>>>);

impl Queue {
    fn of(items: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self(Mutex::new(items.into_iter().map(Ok).collect()))
    }

    fn offline() -> Self {
        Self(Mutex::default())
    }
}

impl ManifestFetcher for Queue {
    fn fetch<'a>(
        &'a self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, RegistryError>> + Send + 'a>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(RegistryError::Fetch("offline".into())))
        })
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

    /// `(drivers listed, tool state if listed)` after one catalog request.
    fn catalog(
        &self,
        source: &str,
        trust: Option<ManifestTrust>,
        fetcher: &Queue,
    ) -> Result<(Vec<String>, Option<ToolCatalogState>), RegistryError> {
        let mut registry = RegistryService::new(source, fetcher, &self.drivers);
        if let Some(trust) = trust {
            registry = registry.with_manifest_trust(trust);
        }
        let full = block_on(registry.catalog_with_tools(&MAC, &self.tools))?;
        Ok((
            full.drivers.into_iter().map(|item| item.id).collect(),
            full.tools.into_iter().next().map(|item| item.state),
        ))
    }

    fn install(&self, bytes: &[u8]) {
        let owned = bytes.to_vec();
        ToolInstaller::new(MAC, &self.tools)
            .install(
                &entry("1.0.0", bytes),
                move |_, _| Ok(Box::new(Cursor::new(owned)) as Box<dyn Read>),
                &AtomicBool::new(false),
                &NoProgress,
            )
            .unwrap();
    }

    fn tool_dir(&self) -> PathBuf {
        self.tools.root().join("tablex-mcp")
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.tool_dir())
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
}

// --- the cache is only as trusted as the manifest it came from -----------------------------------

#[test]
fn a_manifest_accepted_unsigned_in_developer_mode_is_never_served_from_the_cache_in_strict_mode() {
    let rig = Rig::new();
    let tools = serde_json::json!([tool_json("1.0.0", b"unsigned build")]);
    // Developer mode, a local file: accepted without a signature, and cached.
    let fetcher = Queue::of([manifest("2026-10-14T00:00:00Z", tools)]);
    let (drivers, tool) = rig
        .catalog(LOCAL, Some(trust(TrustPolicy::Developer)), &fetcher)
        .unwrap();
    assert_eq!(drivers, ["sqlite"]);
    assert_eq!(tool, Some(ToolCatalogState::Available));

    // Developer mode is turned off and the official fetch fails: the unsigned cache is not served,
    // for the drivers or for the tool, whichever source is asked for.
    for source in [HTTPS, LOCAL] {
        let (drivers, tool) = rig
            .catalog(source, Some(trust(TrustPolicy::Strict)), &Queue::offline())
            .unwrap();
        assert!(drivers.is_empty(), "{source}: {drivers:?}");
        assert_eq!(tool, None, "{source}");
    }
    // And Developer mode with a remote source needs a signature too.
    let (drivers, tool) = rig
        .catalog(
            HTTPS,
            Some(trust(TrustPolicy::Developer)),
            &Queue::offline(),
        )
        .unwrap();
    assert!(drivers.is_empty() && tool.is_none());
    // The same developer source still reads its own cache offline.
    let (drivers, _) = rig
        .catalog(
            LOCAL,
            Some(trust(TrustPolicy::Developer)),
            &Queue::offline(),
        )
        .unwrap();
    assert_eq!(drivers, ["sqlite"]);
}

#[test]
fn a_signed_cache_is_served_again_only_while_it_still_verifies() {
    let rig = Rig::new();
    let body = manifest(
        "2026-10-14T00:00:00Z",
        serde_json::json!([tool_json("1.0.0", b"signed build")]),
    );
    let fetcher = Queue::of([body.clone(), sign(&body)]);
    let (drivers, tool) = rig
        .catalog(HTTPS, Some(trust(TrustPolicy::Strict)), &fetcher)
        .unwrap();
    assert_eq!(drivers, ["sqlite"]);
    assert_eq!(tool, Some(ToolCatalogState::Available));

    let offline = |rig: &Rig| {
        rig.catalog(HTTPS, Some(trust(TrustPolicy::Strict)), &Queue::offline())
            .unwrap()
    };
    let (drivers, tool) = offline(&rig);
    assert_eq!(
        drivers,
        ["sqlite"],
        "the verified cache serves while offline"
    );
    assert_eq!(tool, Some(ToolCatalogState::Available));

    // Another key (rotated away) no longer vouches for it.
    let other = ManifestTrust::new(
        &[SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes()],
        TrustPolicy::Strict,
    )
    .unwrap();
    let (drivers, tool) = rig.catalog(HTTPS, Some(other), &Queue::offline()).unwrap();
    assert!(drivers.is_empty() && tool.is_none());

    // Tampered bytes, or a signature that is gone: not served.
    let cache = rig.drivers.registry_cache_path();
    let signature = PathBuf::from(format!("{}.sig", cache.display()));
    let original = std::fs::read(&cache).unwrap();
    let mut flipped = original.clone();
    flipped[10] ^= 1;
    std::fs::write(&cache, &flipped).unwrap();
    let (drivers, tool) = offline(&rig);
    assert!(drivers.is_empty() && tool.is_none(), "tampered cache");
    std::fs::write(&cache, &original).unwrap();
    assert_eq!(offline(&rig).0, ["sqlite"]);
    std::fs::remove_file(&signature).unwrap();
    let (drivers, tool) = offline(&rig);
    assert!(
        drivers.is_empty() && tool.is_none(),
        "cache without its signature"
    );
}

#[test]
fn a_catalog_built_with_a_manifest_trust_lists_the_tool_only_for_a_valid_signature() {
    let rig = Rig::new();
    let body = manifest(
        "2026-10-14T00:00:00Z",
        serde_json::json!([tool_json("1.0.0", b"build")]),
    );
    let (drivers, tool) = rig
        .catalog(
            HTTPS,
            Some(trust(TrustPolicy::Strict)),
            &Queue::of([body.clone(), sign(&body)]),
        )
        .unwrap();
    assert_eq!(
        (drivers.len(), tool),
        (1, Some(ToolCatalogState::Available))
    );

    // A tool swapped in after signing: the whole catalog is refused, not just the tool.
    let tampered = manifest(
        "2026-10-15T00:00:00Z",
        serde_json::json!([tool_json("1.0.0", b"evil")]),
    );
    let result = rig.catalog(
        HTTPS,
        Some(trust(TrustPolicy::Strict)),
        &Queue::of([tampered, sign(&body)]),
    );
    assert!(
        matches!(result, Err(RegistryError::Manifest(_))),
        "{result:?}"
    );
    // No signature at all.
    let result = rig.catalog(
        HTTPS,
        Some(trust(TrustPolicy::Strict)),
        &Queue::of([body.clone()]),
    );
    assert!(
        matches!(result, Err(RegistryError::Manifest(_))),
        "{result:?}"
    );
}

#[test]
fn a_tools_value_that_is_not_a_list_never_fails_the_drivers() {
    let rig = Rig::new();
    for tools in [
        serde_json::json!({}),
        serde_json::json!("none"),
        serde_json::json!(7),
    ] {
        let body = manifest("2026-10-14T00:00:00Z", tools.clone());
        let parsed: DriverManifest = serde_json::from_slice(&body).expect("the manifest parses");
        assert_eq!(parsed.drivers.len(), 1, "{tools}");
        assert!(
            matches!(parsed.tools.as_slice(), [ManifestTool::Invalid { .. }]),
            "{tools}"
        );
        let (drivers, tool) = rig.catalog(LOCAL, None, &Queue::of([body])).unwrap();
        assert_eq!(drivers, ["sqlite"]);
        assert!(
            matches!(tool, Some(ToolCatalogState::Unavailable { .. })),
            "{tools}: {tool:?}"
        );
    }
    // `null` is "no tools".
    let body = manifest("2026-10-14T00:00:00Z", serde_json::Value::Null);
    let parsed: DriverManifest = serde_json::from_slice(&body).unwrap();
    assert!(parsed.tools.is_empty() && parsed.drivers.len() == 1);
    // Signed, so it also goes through the trust path unharmed.
    let body = manifest("2026-10-14T00:00:00Z", serde_json::json!({}));
    let (drivers, _) = rig
        .catalog(
            HTTPS,
            Some(trust(TrustPolicy::Strict)),
            &Queue::of([body.clone(), sign(&body)]),
        )
        .unwrap();
    assert_eq!(drivers, ["sqlite"]);
}

// --- the record and the executable ----------------------------------------------------------

fn write_record(rig: &Rig, text: &[u8]) {
    std::fs::write(rig.tool_dir().join("installed.json"), text).unwrap();
}

#[test]
fn an_unusable_record_is_a_typed_error_not_a_guess() {
    let rig = Rig::new();
    rig.install(b"a build");
    let good = std::fs::read(rig.tool_dir().join("installed.json")).unwrap();

    write_record(&rig, &vec![b' '; 64 * 1024 + 1]);
    assert!(
        matches!(rig.tools.installed(MCP_TOOL_ID), Err(ToolError::Record(m)) if m.contains("too large"))
    );

    let other = String::from_utf8(good.clone())
        .unwrap()
        .replace("\"tablex-mcp\"", "\"other-tool\"");
    write_record(&rig, other.as_bytes());
    assert!(
        matches!(rig.tools.installed(MCP_TOOL_ID), Err(ToolError::Record(m)) if m.contains("other-tool"))
    );

    write_record(&rig, b"{ not json");
    assert!(matches!(
        rig.tools.installed(MCP_TOOL_ID),
        Err(ToolError::Record(_))
    ));

    // A folder where the record belongs.
    std::fs::remove_file(rig.tool_dir().join("installed.json")).unwrap();
    std::fs::create_dir(rig.tool_dir().join("installed.json")).unwrap();
    assert!(matches!(
        rig.tools.installed(MCP_TOOL_ID),
        Err(ToolError::UnexpectedEntry { .. })
    ));

    std::fs::remove_dir(rig.tool_dir().join("installed.json")).unwrap();
    write_record(&rig, &good);
    assert!(rig.tools.installed(MCP_TOOL_ID).unwrap().is_some());
}

/// A FIFO at the record or the executable must not hang the open (the catalog reads on a worker
/// that the Driver Manager waits for): the call returns, with an error.
#[cfg(unix)]
#[test]
fn a_fifo_where_the_record_or_the_executable_belongs_does_not_block() {
    for name in ["installed.json", "tablex-mcp"] {
        let rig = Rig::new();
        rig.install(b"a build");
        std::fs::remove_file(rig.tool_dir().join(name)).unwrap();
        let fifo = rig.tool_dir().join(name);
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        // Make the other file the record would need so the FIFO is what is reached.
        let (sender, receiver) = std::sync::mpsc::channel();
        let tools = rig.tools.clone();
        std::thread::spawn(move || {
            let _ = sender.send(tools.installed(MCP_TOOL_ID).map(|_| ()));
        });
        let answer = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("a FIFO at {name} blocked the read"));
        assert!(
            matches!(answer, Err(ToolError::UnexpectedEntry { .. }))
                || (name == "tablex-mcp" && answer.is_ok()),
            "{name}: {answer:?}"
        );
    }
}

#[test]
fn a_new_build_of_the_same_size_under_an_old_record_reads_as_not_installed() {
    let rig = Rig::new();
    rig.install(b"first build!");
    assert!(rig.tools.installed(MCP_TOOL_ID).unwrap().is_some());
    // The crash window between the two renames, with an equal-size build: new bytes, old record.
    let executable = rig.tools.executable_path(MCP_TOOL_ID).unwrap();
    std::fs::write(&executable, b"other build!").unwrap();
    assert_eq!(rig.tools.installed(MCP_TOOL_ID).unwrap(), None);
    // Repaired by installing again.
    rig.install(b"first build!");
    assert!(rig.tools.installed(MCP_TOOL_ID).unwrap().is_some());
}

// --- folders --------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn folders_are_made_with_a_fixed_mode_and_a_shared_writable_one_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    rig.install(b"a build");
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(rig.tools.root()), 0o755);
    assert_eq!(mode(&rig.tool_dir()), 0o755);

    for (label, path) in [
        ("tool folder", rig.tool_dir()),
        ("tools folder", rig.tools.root().to_path_buf()),
    ] {
        let original = mode(&path);
        for open in [0o775, 0o757, 0o777] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(open)).unwrap();
            let owned = b"a build".to_vec();
            let result = ToolInstaller::new(MAC, &rig.tools).install(
                &entry("1.0.0", b"a build"),
                move |_, _| Ok(Box::new(Cursor::new(owned)) as Box<dyn Read>),
                &AtomicBool::new(false),
                &NoProgress,
            );
            assert!(
                matches!(result, Err(ToolError::SharedWritable { .. })),
                "{label} {open:o}: {result:?}"
            );
            assert!(matches!(
                rig.tools.installed(MCP_TOOL_ID),
                Err(ToolError::SharedWritable { .. })
            ));
            assert!(matches!(
                ToolInstaller::new(MAC, &rig.tools).uninstall(MCP_TOOL_ID),
                Err(ToolError::SharedWritable { .. })
            ));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(original)).unwrap();
    }
    assert!(rig.tools.installed(MCP_TOOL_ID).unwrap().is_some());
}

#[test]
fn missing_levels_above_the_tools_folder_are_created_one_by_one() {
    let temp = TempDir::new().unwrap();
    let store = ToolStore::new(temp.path().join("a").join("b").join("tools"));
    let owned = b"a build".to_vec();
    ToolInstaller::new(MAC, &store)
        .install(
            &entry("1.0.0", b"a build"),
            move |_, _| Ok(Box::new(Cursor::new(owned)) as Box<dyn Read>),
            &AtomicBool::new(false),
            &NoProgress,
        )
        .unwrap();
    assert!(store.installed(MCP_TOOL_ID).unwrap().is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for level in ["a", "a/b", "a/b/tools", "a/b/tools/tablex-mcp"] {
            let mode = std::fs::metadata(temp.path().join(level))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o022, 0, "{level}: {mode:o}");
        }
    }
}

// --- what a sweep may delete ----------------------------------------------------------------

/// A reader that stalls after `after` bytes until `go` is signalled.
struct Gate {
    data: Cursor<Vec<u8>>,
    after: usize,
    served: usize,
    reached: std::sync::mpsc::Sender<()>,
    go: std::sync::mpsc::Receiver<()>,
    paused: bool,
}

impl Read for Gate {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.served >= self.after && !self.paused {
            self.paused = true;
            let _ = self.reached.send(());
            let _ = self.go.recv();
        }
        let limit = buffer.len().min(if self.paused {
            buffer.len()
        } else {
            self.after.saturating_sub(self.served).max(1)
        });
        let read = self.data.read(&mut buffer[..limit])?;
        self.served += read;
        Ok(read)
    }
}

#[test]
fn an_uninstall_never_deletes_the_temp_file_of_an_install_in_flight() {
    let rig = std::sync::Arc::new(Rig::new());
    rig.install(b"first build");
    let update = vec![7_u8; 300_000];
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let worker = {
        let (rig, update) = (rig.clone(), update.clone());
        std::thread::spawn(move || {
            let mut gate = Some(Gate {
                data: Cursor::new(update.clone()),
                after: 100_000,
                served: 0,
                reached: reached_tx,
                go: go_rx,
                paused: false,
            });
            ToolInstaller::new(MAC, &rig.tools).install(
                &entry("2.0.0", &update),
                move |_, _| Ok(Box::new(gate.take().unwrap()) as Box<dyn Read>),
                &AtomicBool::new(false),
                &NoProgress,
            )
        })
    };
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the download is under way");
    let staging = |rig: &Rig| {
        rig.names()
            .into_iter()
            .filter(|n| n.ends_with(".staging"))
            .count()
    };
    assert_eq!(
        staging(&rig),
        1,
        "only the download exists yet: {:?}",
        rig.names()
    );

    // An uninstall now removes the old files and leaves the running download's temp file.
    assert!(ToolInstaller::new(MAC, &rig.tools)
        .uninstall(MCP_TOOL_ID)
        .unwrap());
    assert_eq!(staging(&rig), 1, "{:?}", rig.names());

    go_tx.send(()).unwrap();
    let record = worker.join().unwrap().expect("the install still completes");
    assert_eq!(record.version, "2.0.0");
    assert_eq!(
        rig.tools.installed(MCP_TOOL_ID).unwrap().unwrap().version,
        "2.0.0"
    );
    assert_eq!(rig.names().len(), 2, "{:?}", rig.names());
}

#[test]
fn an_install_sweeps_old_leftovers_but_not_fresh_ones() {
    let rig = Rig::new();
    rig.install(b"first build");
    let old = rig.tool_dir().join(".download.1-1.staging");
    let fresh = rig.tool_dir().join(".download.2-2.staging");
    let user = rig.tool_dir().join(".hidden-note");
    for path in [&old, &fresh, &user] {
        std::fs::write(path, b"x").unwrap();
    }
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 60 * 60);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    rig.install(b"second build");
    assert!(
        !old.exists(),
        "a leftover older than an hour is a crash leftover"
    );
    assert!(
        fresh.exists(),
        "another process may be writing it right now"
    );
    assert!(user.exists(), "not a temp file of ours");
}

// --- correction 1 -----------------------------------------------------------------------------

/// The digest memory is keyed on the opened file's identity: a same-size swap whose mtime is put
/// back (a restored timestamp cannot restore the inode change time) is not "installed".
#[cfg(unix)]
#[test]
fn a_same_size_swap_with_the_mtime_put_back_reads_as_not_installed() {
    let rig = Rig::new();
    rig.install(b"first build!");
    assert!(
        rig.tools.installed(MCP_TOOL_ID).unwrap().is_some(),
        "primed and warm"
    );
    let executable = rig.tools.executable_path(MCP_TOOL_ID).unwrap();
    let modified = std::fs::metadata(&executable).unwrap().modified().unwrap();
    // In place (same inode) ...
    std::fs::write(&executable, b"other build!").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&executable)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(rig.tools.installed(MCP_TOOL_ID).unwrap(), None, "in place");
    rig.install(b"first build!");
    assert!(rig.tools.installed(MCP_TOOL_ID).unwrap().is_some());
    // ... and as a new file (new inode).
    let replacement = rig.tool_dir().join("swap");
    std::fs::write(&replacement, b"other build!").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    std::fs::rename(&replacement, &executable).unwrap();
    assert_eq!(rig.tools.installed(MCP_TOOL_ID).unwrap(), None, "new inode");
}

/// A tools folder that is not ours (root's `/usr`, not group- or world-writable) is refused.
#[cfg(unix)]
#[test]
fn a_tools_folder_owned_by_someone_else_is_refused() {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } == 0 || !Path::new("/usr").is_dir() {
        eprintln!("skipped: running as root or no /usr");
        return;
    }
    let store = ToolStore::new("/usr");
    assert!(matches!(
        store.installed(MCP_TOOL_ID),
        Err(ToolError::ForeignOwner { .. })
    ));
    let owned = b"a build".to_vec();
    let result = ToolInstaller::new(MAC, &store).install(
        &entry("1.0.0", b"a build"),
        move |_, _| Ok(Box::new(Cursor::new(owned)) as Box<dyn Read>),
        &AtomicBool::new(false),
        &NoProgress,
    );
    assert!(
        matches!(result, Err(ToolError::ForeignOwner { .. })),
        "{result:?}"
    );
    assert!(!Path::new("/usr/tablex-mcp").exists());
}

#[test]
fn a_failing_anti_replay_store_is_reported_not_read_as_no_cache() {
    let rig = Rig::new();
    let body = manifest(
        "2026-10-14T00:00:00Z",
        serde_json::json!([tool_json("1.0.0", b"build")]),
    );
    rig.catalog(
        HTTPS,
        Some(trust(TrustPolicy::Strict)),
        &Queue::of([body.clone(), sign(&body)]),
    )
    .unwrap();
    // The watermark file becomes unreadable (a folder in its place).
    let watermark = rig.drivers.manifest_watermark_path();
    std::fs::remove_file(&watermark).unwrap();
    std::fs::create_dir(&watermark).unwrap();
    let result = rig.catalog(HTTPS, Some(trust(TrustPolicy::Strict)), &Queue::offline());
    assert!(
        matches!(
            result,
            Err(RegistryError::Manifest(
                engine_driver_host::ManifestTrustError::Store(_)
            ))
        ),
        "{result:?}"
    );
}
