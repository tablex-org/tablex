// Driver Manager proofs against the REAL, PUBLISHED driver catalog (no fixtures): every test is
// `#[ignore]` because it needs the network (github.com releases) and, for the connection proofs,
// the local test containers (Postgres 55432, MariaDB 53307, MongoDB 57017, ClickHouse 58123).
//
// Included after `driver_manager.rs` so it reuses `DmRig`. Run with the data directories pointed at
// a scratch location (the rig builds its own temp stores, but `AppRoot::new` reads
// `TABLEX_DRIVER_DATA_DIR` / `TABLEX_DATA_DIR`):
//
//   TABLEX_DATA_DIR=<tmp> TABLEX_DRIVER_DATA_DIR=<tmp> TMPDIR=<tmp> \
//     cargo test -p tablex-gpui -j 4 live_ -- --ignored --test-threads=1 --nocapture
//
// What makes these "release-like" although the test binary is a debug build:
//  * the manifest source is ALWAYS `DEFAULT_MANIFEST_SOURCE` or an explicit URL below. A debug build's
//    `manifest_source()` would silently pick a local `dist/drivers/drivers-manifest.json` instead;
//  * the trust is built from the OFFICIAL public key alone (no development key), policy Strict;
//  * Strict installs go through the real installer (`manifest_attested`, empty teamId from the
//    signed manifest).

const LIVE_OFFICIAL_KEY: &str = "d3AP0ATwJT0nMlUz5e84cAjxGPzvEYazlqhH7E+TmZI=";
const LIVE_DEV_KEY: &str = "PcjR2auT+V+YvquR+phRcdjpAX33Rvh+dyHjSg6QCqM=";
const LIVE_DRIVERS: [&str; 7] = ["clickhouse", "mariadb", "mongodb", "mysql", "postgresql", "redis", "sqlite"];
const LIVE_TOOLS: [&str; 3] = ["tablex-mcp", "tablex-mongo-shell", "tablex-sqruff"];

fn live_trust(key: &str, policy: TrustPolicy) -> engine_driver_host::ManifestTrust {
    let keys = engine_connect::drivers::trusted_manifest_keys(Some(key), None).expect("a valid key");
    engine_driver_host::ManifestTrust::new(&keys, policy).expect("trust")
}

fn live_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tablex-live-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// The catalog (drivers and tools) from `source` through the app's own HTTP fetcher.
fn live_catalog(
    store: &engine_driver_host::InstalledDriverStore,
    tools: &engine_driver_host::ToolStore,
    source: &str,
    trust: engine_driver_host::ManifestTrust,
) -> (Result<engine_driver_host::FullCatalog, engine_driver_host::RegistryError>, Option<String>) {
    let fetcher = crate::drivers::HttpManifestFetcher::default();
    let registry = engine_driver_host::RegistryService::new(source, &fetcher, store)
        .with_app_version(env!("CARGO_PKG_VERSION"))
        .with_manifest_trust(trust);
    let result = smol::block_on(registry.catalog_with_tools(&crate::drivers::current_target(), tools));
    (result, fetcher.take_failure())
}

fn live_fetch(source: &str) -> Vec<u8> {
    let fetcher = crate::drivers::HttpManifestFetcher::default();
    smol::block_on(engine_driver_host::ManifestFetcher::fetch(&fetcher, source)).expect("fetch")
}

#[test]
#[ignore = "network: fetches the published driver catalog"]
fn live_catalog_is_fetched_and_verified_with_the_production_key() {
    use engine_driver_host::{InstalledDriverStore, ManifestTrustError, RegistryError, ToolStore};
    let root = live_root("catalog");
    let target = crate::drivers::current_target();
    eprintln!("LIVE source            = {}", crate::drivers::DEFAULT_MANIFEST_SOURCE);
    eprintln!(
        "LIVE manifest_source() = {} (what a debug build would resolve by itself; NOT used here)",
        crate::drivers::manifest_source(TrustPolicy::Strict)
    );
    eprintln!("LIVE target            = {target}");

    let store = InstalledDriverStore::new(root.join("store"));
    let tools = ToolStore::new(root.join("tools"));
    let (result, failure) = live_catalog(
        &store,
        &tools,
        crate::drivers::DEFAULT_MANIFEST_SOURCE,
        live_trust(LIVE_OFFICIAL_KEY, TrustPolicy::Strict),
    );
    let full = result.expect("the published catalog verifies under the official key, Strict");
    assert_eq!(failure, None, "no fetch failure");
    for item in &full.drivers {
        let artifact = item.manifest.as_ref().and_then(|entry| entry.artifact_for_target(&target));
        eprintln!(
            "LIVE driver {:<11} version {:<6} state {:?} team_id {:?}\n     artifact {:?}",
            item.id,
            item.version,
            item.state,
            item.manifest.as_ref().map(|entry| entry.team_id.clone()),
            artifact.map(|a| (a.url.rsplit('/').next().unwrap_or("").to_string(), a.size_bytes, a.sha256.clone())),
        );
    }
    for item in &full.tools {
        let artifact = item.entry.as_ref().and_then(|entry| entry.artifact_for_target(&target));
        eprintln!(
            "LIVE tool   {:<19} version {:<6} state {:?} requires {:?}\n     artifact {:?}",
            item.id,
            item.version,
            item.state,
            item.requires(),
            artifact.map(|a| (a.url.rsplit('/').next().unwrap_or("").to_string(), a.size_bytes, a.sha256.clone())),
        );
    }
    let mut ids: Vec<&str> = full.drivers.iter().map(|item| item.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, LIVE_DRIVERS);
    for item in &full.drivers {
        assert_eq!(item.state, engine_driver_host::DriverCatalogState::Available, "{}", item.id);
        assert!(item.manifest.as_ref().is_some_and(|entry| entry.artifact_for_target(&target).is_some()));
    }
    let mongodb = full.drivers.iter().find(|item| item.id == "mongodb").unwrap();
    let mut tool_ids: Vec<&str> = full.tools.iter().map(|item| item.id.as_str()).collect();
    tool_ids.sort();
    assert_eq!(tool_ids, LIVE_TOOLS);
    let shell = full.tools.iter().find(|item| item.id == "tablex-mongo-shell").unwrap();
    assert_eq!(shell.requires().len(), 1);
    // The release pins the shell to the mongodb driver of the same checkout.
    assert_eq!((shell.requires()[0].driver.as_str(), shell.requires()[0].min_version.as_str()), ("mongodb", mongodb.version.as_str()));

    // The replay watermark of the store moved to the manifest's generatedAt.
    eprintln!("LIVE watermark file    = {:?}", std::fs::read_to_string(store.manifest_watermark_path()).ok());

    // Negative control: the DEVELOPMENT key alone does not verify the published signature, so
    // acceptance above rests on the official key.
    let control = InstalledDriverStore::new(root.join("control-store"));
    let (denied, _) = live_catalog(
        &control,
        &tools,
        crate::drivers::DEFAULT_MANIFEST_SOURCE,
        live_trust(LIVE_DEV_KEY, TrustPolicy::Strict),
    );
    assert!(
        matches!(denied, Err(RegistryError::Manifest(ManifestTrustError::SignatureInvalid))),
        "{denied:?}"
    );

    // The keys compiled into this build (official + dev in a debug build) accept it as well.
    let compiled = InstalledDriverStore::new(root.join("compiled-store"));
    let trust = crate::drivers::manifest_trust(TrustPolicy::Strict).expect("compiled keys");
    let (accepted, _) = live_catalog(&compiled, &tools, crate::drivers::DEFAULT_MANIFEST_SOURCE, trust);
    assert_eq!(accepted.expect("compiled trust").drivers.len(), 7);
    let _ = std::fs::remove_dir_all(root);
}

// --- the rig --------------------------------------------------------------------------------

/// A `DmRig` on an empty temp store, Strict, the published catalog and the official key only.
fn live_rig(cx: &mut TestAppContext) -> DmRig<'_> {
    cx.dispatcher.allow_parking();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = sidebar_test_app(cx);
    // Tall: the list is virtualized, and every row's button must be rendered to be clicked.
    visual.simulate_resize(gpui::size(gpui::px(1500.0), gpui::px(3200.0)));
    let root = live_root("rig");
    let store_dir = root.join("store");
    let tools_dir = root.join("tools");
    std::fs::create_dir_all(&store_dir).unwrap();
    let (rig_store, rig_tools) = (store_dir.clone(), tools_dir.clone());
    app.update(visual, |this, cx| {
        this.driver_ui.tools.store = Some(std::sync::Arc::new(engine_driver_host::ToolStore::new(&rig_tools)));
        this.scenario_ui.service = None;
        this.driver_ui.store = std::sync::Arc::new(engine_driver_host::InstalledDriverStore::new(&rig_store));
        this.driver_ui.policy = TrustPolicy::Strict;
        this.driver_ui.manifest_source = Some(crate::drivers::DEFAULT_MANIFEST_SOURCE.to_string());
        this.driver_ui.trust_override = Some(live_trust(LIVE_OFFICIAL_KEY, TrustPolicy::Strict));
        this.driver_ui.installed_loaded = true;
        cx.notify();
    });
    settle(visual);
    DmRig { app, visual, root, store_dir, tools_dir, dylib: std::path::PathBuf::new(), sha: String::new(), size: 0 }
}

impl DmRig<'_> {
    fn live_store(&self) -> engine_driver_host::InstalledDriverStore {
        engine_driver_host::InstalledDriverStore::new(&self.store_dir)
    }

    fn live_record(&self, id: &str) -> Option<engine_driver_host::InstalledDriverRecord> {
        self.live_store().record(id).unwrap()
    }

    fn live_driver_dir(&self, id: &str) -> std::path::PathBuf {
        self.store_dir.join("Drivers/v2").join(id)
    }

    fn live_versions(&self, id: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.live_driver_dir(id))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Clicks the row's primary button and waits for the job to leave the queue; a failure panics
    /// with the typed failure.
    fn live_install(&mut self, id: &'static str) -> engine_driver_host::InstalledDriverRecord {
        self.click(&format!("driver-action-{id}"));
        self.wait("the install to end", move |this| {
            this.driver_ui
                .queue
                .job(id)
                .is_none_or(|job| matches!(job.state, InstallState::Failed(_)))
        });
        if let Some(state) = self.read(|this| this.driver_ui.queue.job(id).map(|job| format!("{:?}", job.state))) {
            panic!("install of {id} failed: {state}");
        }
        self.wait("the handle to be adopted", move |this| this.driver_ui.handles.contains_key(id));
        let record = self.live_record(id).unwrap_or_else(|| panic!("{id} has no installed record"));
        eprintln!(
            "LIVE installed {id} {} -> {} sha256 {}",
            record.version,
            record.path.display(),
            record.sha256
        );
        record
    }

    fn live_uninstall(&mut self, id: &'static str) {
        self.pick_menu(id, 1);
        self.wait("the uninstall", move |this| {
            this.driver_ui.op_task.is_none() && this.driver_ui.model.row(id).is_some_and(|row| !row.is_installed())
        });
        assert!(self.live_record(id).is_none(), "{id}: the record is gone");
        assert!(!self.live_driver_dir(id).exists(), "{id}: its files are gone");
    }

    fn live_tool_action(&mut self, id: &'static str, action: driver_manager::ToolAction) {
        self.update(|this, cx| this.run_tool_action_of(id, action, cx));
        self.wait("the tool operation", move |this| {
            this.driver_ui.op_task.is_none()
                && this.driver_ui.tools.job_of(id).is_none_or(|job| !job.state.is_active())
        });
    }

    /// The manager with its tools loaded, on the All tab: the driver rows (and
    /// their action buttons) are not rendered on the Tools tab.
    fn open_live_manager(&mut self) {
        self.open_manager_with_tool();
        self.click(DriverFilter::All.selector());
    }

    fn live_tool_installed(&self, id: &str) -> bool {
        self.read(|this| this.driver_ui.tools.item_of(id).is_some_and(|item| item.installed.is_some()))
    }

    fn live_tool_exe(&self, id: &str) -> std::path::PathBuf {
        self.tools_dir.join(id).join(id)
    }

    fn live_save_connection(&mut self, kind: ConnectionKind, name: &str, uri: &str) -> String {
        let (name, uri) = (name.to_string(), uri.to_string());
        self.update(|this, cx| {
            let id = uuid::Uuid::new_v4();
            this.saved_connections.push(SavedConnection {
                id,
                name,
                uri,
                kind,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            cx.notify();
            id.to_string()
        })
    }
}

// --- helpers on a loaded driver ---------------------------------------------------------------

fn live_query_request(sql: &str) -> String {
    serde_json::json!({
        "kind": "sql", "operation_id": 1, "database": null, "object": null,
        "offset": 0, "limit": 10, "payload": { "sql": sql }
    })
    .to_string()
}

/// Loads the installed driver `id` from `store` (the production `load_driver`: record, SHA-256,
/// identity and scheme), connects with `uri`, runs `sql`, returns the first cell of the first row.
fn live_sql(store: &engine_driver_host::InstalledDriverStore, id: &str, uri: &str, sql: &str) -> String {
    let handle = engine_connect::drivers::load_driver(store, id, uri).unwrap_or_else(|e| panic!("{id}: load: {e}"));
    let mut session = handle
        .connect(&serde_json::json!({ "uri": uri, "name": "live" }).to_string())
        .unwrap_or_else(|e| panic!("{id}: connect: {e}"));
    let page = session.open_query(&live_query_request(sql)).unwrap_or_else(|e| panic!("{id}: query: {e}"));
    assert_eq!(page.row_count, 1, "{id}: one row");
    let row = session.inspect_row(page.page_id, 0).unwrap_or_else(|e| panic!("{id}: row: {e}"));
    let value = row.fields[0].display_text.clone();
    session.release_page(page.page_id).unwrap();
    session.disconnect().unwrap();
    value
}

fn live_mongo_databases(session: &mut engine_driver_host::DriverSession) -> Vec<String> {
    let request = serde_json::json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 50});
    session.list_nav_nodes(&request.to_string()).expect("list databases").nodes.into_iter().map(|n| n.label).collect()
}

fn live_codesign(path: &std::path::Path) -> String {
    let verify = std::process::Command::new("codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(path)
        .output()
        .unwrap();
    let detail = std::process::Command::new("codesign").arg("-dv").arg(path).output().unwrap();
    let text = String::from_utf8_lossy(&detail.stderr);
    let interesting: Vec<&str> =
        text.lines().filter(|l| l.starts_with("Signature") || l.starts_with("TeamIdentifier") || l.starts_with("CodeDirectory")).collect();
    format!("verify_ok={} [{}] {:?}", verify.status.success(), String::from_utf8_lossy(&verify.stderr).trim(), interesting)
}

// --- cases 1, 2, 3, 6, 7 ----------------------------------------------------------------------

#[gpui::test]
#[ignore = "network + local containers: installs every published driver and tool"]
fn live_every_catalog_driver_installs_connects_uninstalls_and_reinstalls(cx: &mut TestAppContext) {
    let mut rig = live_rig(cx);
    rig.open_live_manager();
    assert_eq!(rig.read(|this| this.driver_ui.catalog_error.clone()), None);
    assert_eq!(rig.read(|this| this.driver_ui.policy), TrustPolicy::Strict);
    for id in LIVE_DRIVERS {
        let (installed, latest, action) = rig.read(|this| {
            let row = this.driver_ui.model.row(id).unwrap_or_else(|| panic!("{id} is listed"));
            (row.installed_version.clone(), row.latest_version.clone(), driver_manager::primary_action(row, None))
        });
        eprintln!("LIVE row {id}: installed {installed:?} latest {latest:?} action {action:?}");
        assert_eq!((installed, action), (None, Some(PrimaryAction::Install)), "{id}");
    }

    // Case 2: install each of the 7 through the Install button, into the empty temp store.
    let target = crate::drivers::current_target();
    for id in LIVE_DRIVERS {
        let record = rig.live_install(id);
        assert!(record.path.starts_with(&rig.store_dir), "{id} installed inside the temp store");
        assert_eq!(record.sha256, driver_sha256(&record.path), "{id}: record sha256 is the installed file's");
        assert_eq!(record.path.file_name().unwrap().to_string_lossy(), target.library_file_name(id));
        eprintln!("LIVE codesign {id}: {}", live_codesign(&record.path));
        let handle = rig.read(|this| this.driver_ui.handles.get(id).cloned()).expect("adopted handle");
        let caps = handle.capabilities();
        eprintln!(
            "LIVE caps {id}: driver={} display={:?} abi={} schemes={:?} features={}",
            caps.driver,
            caps.display_name,
            caps.abi_version,
            caps.uri_schemes,
            caps.features().len()
        );
        assert_eq!(caps.driver, id);
        assert!(!caps.features().is_empty());
        drop(handle);
    }
    assert_eq!(rig.read(|this| this.error_log.log.len()), 0, "no error was logged");
    for id in LIVE_DRIVERS {
        let version = rig.read(|this| driver_manager::version_text(this.driver_ui.model.row(id).unwrap()));
        assert!(version.1.is_none(), "{id}: installed == latest, no update offered: {version:?}");
    }

    // Case 3: connect through each installed catalog driver and run one trivial statement.
    let store = rig.live_store();
    let sqlite_file = rig.root.join("live.sqlite");
    let sqlite_uri = format!("sqlite://{}", sqlite_file.display());
    for (id, uri, sql) in [
        ("postgresql", "postgres://tablex:tablex@127.0.0.1:55432/tablex", "SELECT 1 AS one"),
        ("mariadb", "mariadb://tablex:tablex@127.0.0.1:53307/tablex", "SELECT 1 AS one"),
        ("mysql", "mysql://tablex:tablex@127.0.0.1:53307/tablex", "SELECT 1 AS one"),
        ("clickhouse", "clickhouse://tablex:tablex@127.0.0.1:58123/tablex", "SELECT 1 AS one"),
        ("sqlite", sqlite_uri.as_str(), "SELECT 1 AS one"),
    ] {
        let value = live_sql(&store, id, uri, sql);
        eprintln!("LIVE query {id}: {sql} -> {value}");
        assert_eq!(value, "1", "{id}");
    }
    {
        let uri = "mongodb://127.0.0.1:57017";
        let handle = engine_connect::drivers::load_driver(&store, "mongodb", uri).unwrap();
        let mut session = handle.connect(&serde_json::json!({ "uri": uri, "name": "live" }).to_string()).unwrap();
        let databases = live_mongo_databases(&mut session);
        eprintln!("LIVE query mongodb: list databases -> {databases:?}");
        assert!(databases.iter().any(|name| name == "admin"), "{databases:?}");
        session.disconnect().unwrap();
    }
    // Redis: no container; load + capabilities only (done above), plus the identity/scheme check.
    let redis = engine_connect::drivers::load_driver(&store, "redis", "redis://127.0.0.1:6379").unwrap();
    eprintln!("LIVE redis: loaded, schemes {:?} (NOT connected: no container)", redis.capabilities().uri_schemes);
    drop(redis);

    // Case 7: the three tools from the catalog.
    for id in LIVE_TOOLS {
        rig.live_tool_action(id, driver_manager::ToolAction::Install);
        assert!(rig.live_tool_installed(id), "{id} installed; job: {:?}", rig.read(|this| this.driver_ui.tools.job_of(id).map(|j| format!("{:?}", j.state))));
        let exe = rig.live_tool_exe(id);
        let size = std::fs::metadata(&exe).unwrap().len();
        eprintln!("LIVE tool {id}: {} ({size} bytes) codesign {}", exe.display(), live_codesign(&exe));
    }
    let mcp = std::process::Command::new(rig.live_tool_exe("tablex-mcp")).arg("--version").output().unwrap();
    eprintln!("LIVE tablex-mcp --version -> {:?} {:?}", String::from_utf8_lossy(&mcp.stdout).trim(), mcp.status);
    assert!(mcp.status.success() && !mcp.stdout.is_empty());

    let format_request = engine_driver_api::sql_format::FormatRequest {
        protocol: engine_driver_api::sql_format::PROTOCOL_VERSION,
        sql: "select a,b from t where x=1 and y=2".into(),
        dialect: engine_driver_api::sql_format::Dialect::Postgres,
        keyword_case: engine_driver_api::sql_format::KeywordCase::Upper,
        indent: 4,
    };
    let formatted = engine_driver_host::SqlFormatTool::new(rig.live_tool_exe("tablex-sqruff"))
        .format(format_request, Duration::from_secs(30), &engine_driver_host::FormatCancel::new())
        .expect("sqruff formats");
    eprintln!("LIVE tablex-sqruff format -> {formatted:?}");
    assert!(formatted.contains("SELECT") && formatted.contains("WHERE"), "{formatted}");

    {
        let uri = "mongodb://127.0.0.1:57017";
        let handle = engine_connect::drivers::load_driver(&store, "mongodb", uri).unwrap();
        let session = handle.connect(&serde_json::json!({ "uri": uri, "name": "live" }).to_string()).unwrap();
        let shell = engine_driver_host::MongoShell::new(rig.live_tool_exe("tablex-mongo-shell"));
        let request = |script: &str| engine_driver_api::mongo_shell::ShellRequest {
            script: script.into(),
            database: "admin".into(),
            allow_writes: false,
            page_size: 50,
            timeout_ms: Some(60_000),
            cursor: None,
        };
        let reply = shell.run(&session, request("print(1 + 1)")).expect("start").wait().expect("run");
        eprintln!("LIVE tablex-mongo-shell print(1 + 1) -> output {:?} error {:?}", reply.output, reply.error);
        assert!(reply.error.is_none() && reply.output == vec!["2".to_string()], "{reply:?}");
        let reply = shell.run(&session, request("db.runCommand({ ping: 1 })")).expect("start").wait().expect("run");
        eprintln!("LIVE tablex-mongo-shell ping -> error {:?} result {:?}", reply.error, reply.result);
        assert!(reply.error.is_none(), "{reply:?}");
        drop(session);
    }

    // Case 6: uninstall everything (tools first: the shell depends on mongodb), then reinstall.
    for id in LIVE_TOOLS {
        rig.live_tool_action(id, driver_manager::ToolAction::Uninstall);
        assert!(!rig.live_tool_installed(id), "{id} uninstalled");
        assert!(!rig.tools_dir.join(id).exists(), "{id}: its folder is gone");
    }
    for id in LIVE_DRIVERS {
        rig.live_uninstall(id);
        eprintln!("LIVE uninstalled {id}: record gone, {} removed", rig.live_driver_dir(id).display());
    }
    assert!(rig.read(|this| this.driver_ui.handles.is_empty() && this.driver_ui.installed.is_empty()));
    for id in LIVE_DRIVERS {
        let record = rig.live_install(id);
        assert_eq!(record.sha256, driver_sha256(&record.path));
    }
    let store = rig.live_store();
    assert_eq!(live_sql(&store, "postgresql", "postgres://tablex:tablex@127.0.0.1:55432/tablex", "SELECT 1"), "1");
    assert_eq!(live_sql(&store, "sqlite", &sqlite_uri, "SELECT 1"), "1");
    eprintln!("LIVE reinstall of all 7 drivers works; postgresql + sqlite query again OK");
}

// --- cases 4, 5 -------------------------------------------------------------------------------

#[gpui::test]
#[ignore = "network + local MongoDB container: update and uninstall with an open session"]
fn live_mongodb_update_offer_stale_session_reconnect_and_uninstall(cx: &mut TestAppContext) {
    let mut rig = live_rig(cx);
    rig.open_live_manager();

    // The catalog's real build first (to get the real bytes), then turn the install into a 0.2.0 one.
    let real = rig.live_install("mongodb");
    let latest = real.version.clone();
    assert_ne!(latest, "0.2.0");
    // The directory of the catalog build, for the `move` wait closures below.
    let latest_dir: &'static str = Box::leak(format!("/mongodb/{latest}/").into_boxed_str());
    let saved = rig.root.join("mongodb-latest.dylib");
    std::fs::copy(&real.path, &saved).unwrap();
    rig.live_uninstall("mongodb");
    let old_dir = rig.live_driver_dir("mongodb").join("0.2.0");
    std::fs::create_dir_all(&old_dir).unwrap();
    let old_path = old_dir.join(real.path.file_name().unwrap());
    std::fs::copy(&saved, &old_path).unwrap();
    rig.live_store()
        .save_installed(engine_driver_host::InstalledDriverRecord {
            version: "0.2.0".into(),
            path: old_path.clone(),
            ..real.clone()
        })
        .unwrap();
    eprintln!("LIVE rewrote the installed mongodb record to 0.2.0 at {}", old_path.display());
    rig.update(|this, cx| this.reload_driver_catalog(cx));
    rig.wait("the reload", |this| {
        this.driver_ui.catalog_task.is_none() && this.driver_ui.model.row("mongodb").is_some_and(|row| row.is_installed())
    });

    // The model offers 0.2.0 -> the catalog version and the add-on says "update driver first".
    let (text, has_update, action) = rig.read(|this| {
        let row = this.driver_ui.model.row("mongodb").unwrap();
        (driver_manager::version_text(row), row.has_update(), driver_manager::primary_action(row, None))
    });
    eprintln!("LIVE mongodb row: version_text {text:?} has_update {has_update} action {action:?}");
    assert_eq!(text, ("0.2.0".to_string(), Some(latest.clone())));
    assert!(has_update);
    assert_eq!(action, Some(PrimaryAction::Update));
    assert!(rig.read(|this| this.driver_ui.updates_available) >= 1);
    let (reason, actions) = rig.read(|this| {
        let item = this.driver_ui.tools.item_of("tablex-mongo-shell").expect("the shell row");
        let need = this.addon_need(item).expect("the shell needs a newer driver");
        (need.reason(), driver_manager::addon_actions(item, None, Some(&need)))
    });
    eprintln!("LIVE mongo-shell: reason {reason:?} actions {actions:?}");
    assert!(reason.contains(latest.as_str()) && reason.contains("0.2.0 installed"), "{reason}");
    assert_eq!(actions, [driver_manager::ToolAction::UpdateDriverFirst]);
    // And installing it directly is refused by the host (the requirement is enforced there too).
    rig.update(|this, cx| this.run_tool_action_of("tablex-mongo-shell", driver_manager::ToolAction::Install, cx));
    settle(rig.visual);
    assert!(!rig.live_tool_installed("tablex-mongo-shell"), "not installed while the driver is old");

    // Case 5: an open session on 0.2.0.
    let key = rig.live_save_connection(ConnectionKind::Mongodb, "live mongo", "mongodb://127.0.0.1:57017");
    rig.connect(&key);
    assert!(rig.session_version(&key).contains("/mongodb/0.2.0/"), "{}", rig.session_version(&key));

    // Run the update with the session open.
    rig.click("driver-action-mongodb");
    rig.wait("the update", |this| {
        this.driver_ui.queue.job("mongodb").is_none()
            && this.driver_ui.handles.get("mongodb").is_some_and(|handle| handle.path().to_string_lossy().contains(latest_dir))
    });
    let record = rig.live_record("mongodb").unwrap();
    eprintln!("LIVE after update: record {} previous {:?} dirs {:?}", record.version, record.previous_version, rig.live_versions("mongodb"));
    assert_eq!(record.version, latest);
    assert_eq!(record.sha256, driver_sha256(&record.path));
    assert_eq!(rig.live_versions("mongodb"), ["0.2.0", latest.as_str()]);
    let text = rig.read(|this| driver_manager::version_text(this.driver_ui.model.row("mongodb").unwrap()));
    assert_eq!(text, (latest.clone(), None));
    assert_eq!(rig.read(|this| this.driver_ui.updates_available), 0);

    // The open session was not disturbed: still on 0.2.0, still answers, flagged stale.
    assert!(rig.session_version(&key).contains("/mongodb/0.2.0/"));
    let session = rig.read(|this| this.driver_runtime.sessions.get(&key).cloned()).expect("the session is open");
    let databases = live_mongo_databases(&mut session.lock().unwrap_or_else(|e| e.into_inner()));
    drop(session);
    eprintln!("LIVE stale session still answers on 0.2.0: {databases:?}");
    assert!(databases.iter().any(|name| name == "admin"));
    assert_eq!(rig.read(|this| this.driver_sessions("mongodb").iter().filter(|s| s.stale).count()), 1);
    assert_eq!(rig.read(|this| this.stale_session_version("mongodb")).as_deref(), Some("0.2.0"));
    assert!(rig.rendered("driver-reconnect-stale"), "the detail pane offers Reconnect");
    assert_eq!(
        rig.read(|this| driver_manager::row_action(this.driver_ui.model.row("mongodb").unwrap(), None, true)),
        Some(PrimaryAction::Reconnect)
    );

    // The shell add-on can now be installed.
    assert!(rig.read(|this| this.addon_need(this.driver_ui.tools.item_of("tablex-mongo-shell").unwrap()).is_none()));
    rig.live_tool_action("tablex-mongo-shell", driver_manager::ToolAction::Install);
    assert!(rig.live_tool_installed("tablex-mongo-shell"));

    // Reconnect moves the session to the catalog version.
    rig.click("driver-action-mongodb");
    let k = key.clone();
    rig.wait("the reconnect", move |this| {
        matches!(this.sessions.status(&k), ConnectionStatus::Connected)
            && this
                .driver_runtime
                .session_handles
                .get(&k)
                .is_some_and(|handle| handle.path().to_string_lossy().contains(latest_dir))
    });
    assert_eq!(rig.read(|this| this.driver_sessions("mongodb").iter().filter(|s| s.stale).count()), 0);
    eprintln!("LIVE reconnected on {}", rig.session_version(&key));

    // Uninstall with the session open: the dialog lists it, Disconnect and uninstall removes all.
    rig.pick_menu("mongodb", 1);
    assert!(rig.rendered("driver-confirm-body"), "the confirm dialog lists the open connection");
    rig.click("driver-confirm-ok");
    rig.wait("the uninstall", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.model.row("mongodb").is_some_and(|row| !row.is_installed())
    });
    assert!(rig.live_record("mongodb").is_none());
    assert!(!rig.live_driver_dir("mongodb").exists(), "every version directory is deleted");
    assert!(rig.read(|this| this.driver_runtime.sessions.is_empty()), "the session was disconnected");
    eprintln!("LIVE uninstall with an open session: disconnected, record and files removed, app alive");

    // Reinstall works afterwards.
    let again = rig.live_install("mongodb");
    assert_eq!(again.version, latest);
}

// --- case 8 -----------------------------------------------------------------------------------

#[test]
#[ignore = "network: tampered manifest and artifact are refused"]
fn live_tampered_manifest_and_artifact_are_rejected() {
    use engine_driver_host::{
        DriverInstaller, DriverVerificationError, InstallError, InstalledDriverStore, ManifestTrustError, NoProgress,
        RegistryError, ToolStore,
    };
    let root = live_root("tamper");
    let target = crate::drivers::current_target();
    let tools = ToolStore::new(root.join("tools"));
    let trust = || live_trust(LIVE_OFFICIAL_KEY, TrustPolicy::Strict);

    // 8a: a copy of the manifest with ONE byte changed, served from file:// as an explicit source.
    let manifest = live_fetch(crate::drivers::DEFAULT_MANIFEST_SOURCE);
    let signature = live_fetch(&format!("{}.sig", crate::drivers::DEFAULT_MANIFEST_SOURCE));
    let copy = root.join("copy");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("drivers-manifest.json"), &manifest).unwrap();
    std::fs::write(copy.join("drivers-manifest.json.sig"), &signature).unwrap();
    let control_source = file_url(&copy.join("drivers-manifest.json"));
    let control_store = InstalledDriverStore::new(root.join("control-store"));
    let (control, _) = live_catalog(&control_store, &tools, &control_source, trust());
    eprintln!("LIVE untouched copy via file:// (Strict): {:?}", control.as_ref().map(|c| c.drivers.len()).map_err(|e| e.to_string()));
    assert!(control.is_ok(), "an untouched copy with its original signature still verifies");

    let position = manifest.windows(8).position(|w| w == b"sqlite\",").expect("an id to flip") + 1;
    let mut tampered = manifest.clone();
    tampered[position] = b'S';
    assert_eq!(tampered.len(), manifest.len());
    assert_eq!(tampered.iter().zip(&manifest).filter(|(a, b)| a != b).count(), 1);
    let bad = root.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("drivers-manifest.json"), &tampered).unwrap();
    std::fs::write(bad.join("drivers-manifest.json.sig"), &signature).unwrap();
    let bad_store = InstalledDriverStore::new(root.join("bad-store"));
    let (result, _) = live_catalog(&bad_store, &tools, &file_url(&bad.join("drivers-manifest.json")), trust());
    eprintln!("LIVE one-byte-changed copy via file:// (Strict): {:?}", result.as_ref().map(|c| c.drivers.len()).map_err(|e| e.to_string()));
    assert!(
        matches!(result, Err(RegistryError::Manifest(ManifestTrustError::SignatureInvalid))),
        "{:?}",
        result.map(|c| c.drivers.len())
    );
    assert!(!bad_store.registry_cache_path().exists(), "nothing of it was cached");

    // 8b: a real artifact against a manifest entry whose sha256 differs, under Strict.
    let store = InstalledDriverStore::new(root.join("store"));
    let (catalog, _) = live_catalog(&store, &tools, crate::drivers::DEFAULT_MANIFEST_SOURCE, trust());
    let entry = catalog.unwrap().drivers.into_iter().find(|item| item.id == "sqlite").unwrap().manifest.unwrap();
    let artifact = entry.artifact_for_target(&target).unwrap().clone();
    let download_path = crate::drivers::temporary_download_path(&store, "sqlite", &artifact.url);
    let downloaded = crate::drivers::download_artifact(
        &artifact.url,
        &download_path,
        artifact.size_bytes,
        TrustPolicy::Strict,
        &std::sync::Arc::new(AtomicBool::new(false)),
        &NoProgress,
    )
    .expect("download the real artifact");
    assert_eq!(downloaded.sha256, artifact.sha256, "the published bytes match the published sha256");
    let installer = DriverInstaller::new(target, &store).with_trust_policy(TrustPolicy::Strict);
    let nothing_installed = |label: &str| {
        assert!(store.record("sqlite").unwrap().is_none(), "{label}: no record");
        let dir = store.base_directory().join("Drivers/v2/sqlite");
        let leftovers: Vec<_> = walk(&dir);
        assert!(leftovers.is_empty(), "{label}: no partial install: {leftovers:?}");
        let unpack: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("tablex-driver-unpack-{}-", std::process::id())))
            .collect();
        assert!(unpack.is_empty(), "{label}: no unpack directory left");
    };
    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            found.push(path.clone());
            if path.is_dir() {
                found.extend(walk(&path));
            }
        }
        found
    }

    let mut wrong_sha = entry.clone();
    let flipped = if artifact.sha256.starts_with('0') { '1' } else { '0' };
    wrong_sha.artifacts.iter_mut().for_each(|a| a.sha256.replace_range(0..1, &flipped.to_string()));
    let error = installer.install_downloaded(&wrong_sha, &downloaded, &NoProgress).expect_err("wrong sha is refused");
    eprintln!("LIVE wrong manifest sha256 -> {error}");
    assert!(matches!(error, InstallError::Verification(DriverVerificationError::ChecksumMismatch { .. })), "{error:?}");
    nothing_installed("wrong sha");

    let corrupted = download_path.with_file_name("corrupted.zip");
    let mut bytes = std::fs::read(&download_path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&corrupted, &bytes).unwrap();
    let error = installer.install_from_file(&entry, &corrupted).expect_err("a changed byte is refused");
    eprintln!("LIVE artifact with one byte changed -> {error}");
    assert!(matches!(error, InstallError::Verification(DriverVerificationError::ChecksumMismatch { .. })), "{error:?}");
    nothing_installed("one byte changed");

    let mut wrong_size = entry.clone();
    wrong_size.artifacts.iter_mut().for_each(|a| a.size_bytes += 1);
    let error = installer.install_from_file(&wrong_size, &download_path).expect_err("wrong size is refused");
    eprintln!("LIVE wrong manifest size -> {error}");
    assert!(matches!(error, InstallError::Verification(DriverVerificationError::SizeMismatch { .. })), "{error:?}");
    nothing_installed("wrong size");

    // Control: the genuine bytes against the genuine entry install under Strict (ad-hoc signed).
    let record = installer.install_downloaded(&entry, &downloaded, &NoProgress).expect("the genuine artifact installs");
    eprintln!("LIVE genuine artifact under Strict -> installed {} at {}", record.version, record.path.display());
    assert!(store.record("sqlite").unwrap().is_some());
    let _ = std::fs::remove_dir_all(root);
}

// --- case 9 -----------------------------------------------------------------------------------

#[gpui::test]
#[ignore = "network: an unreachable catalog URL keeps installed drivers working"]
fn live_an_unreachable_catalog_reports_clearly_and_installed_drivers_still_load(cx: &mut TestAppContext) {
    let mut rig = live_rig(cx);
    rig.open_live_manager();
    let sqlite = rig.live_install("sqlite");
    let sqlite_uri = format!("sqlite://{}", rig.root.join("offline.sqlite").display());

    for unreachable in ["https://catalog.invalid/drivers-manifest.json", "http://127.0.0.1:1/drivers-manifest.json"] {
        rig.update(|this, _| this.driver_ui.manifest_source = Some(unreachable.to_string()));
        rig.click("driver-check-updates");
        rig.wait("the failed check", |this| this.driver_ui.catalog_task.is_none());
        let error = rig.read(|this| this.driver_ui.catalog_error.clone()).expect("the failure is reported");
        eprintln!("LIVE unreachable {unreachable} -> catalog_error: {error}");
        assert!(error.contains("Could not reach") && error.contains(unreachable), "{error}");
        let (installed, rows) = rig.read(|this| {
            (this.driver_ui.model.row("sqlite").map(|row| row.installed_version.clone()), this.driver_ui.model.rows().len())
        });
        eprintln!("LIVE   still listed: sqlite {installed:?}, {rows} rows (cached catalog)");
        assert_eq!(installed, Some(Some("0.1.0".to_string())));
        // The installed driver keeps loading and working.
        assert_eq!(live_sql(&rig.live_store(), "sqlite", &sqlite_uri, "SELECT 1"), "1");
        assert_eq!(rig.live_record("sqlite").unwrap().sha256, sqlite.sha256);
    }

    // No cache at all (a fresh store) + unreachable: an empty catalog and the failure, no panic.
    let fresh = engine_driver_host::InstalledDriverStore::new(rig.root.join("fresh-store"));
    let tools = engine_driver_host::ToolStore::new(rig.root.join("fresh-tools"));
    let (result, failure) = live_catalog(
        &fresh,
        &tools,
        "https://catalog.invalid/drivers-manifest.json",
        live_trust(LIVE_OFFICIAL_KEY, TrustPolicy::Strict),
    );
    eprintln!("LIVE fresh store + unreachable: drivers {:?} failure {failure:?}", result.as_ref().map(|c| c.drivers.len()));
    assert_eq!(result.unwrap().drivers.len(), 0);
    assert!(failure.is_some());

    // Offline reload (what startup does) keeps the rows too.
    rig.update(|this, cx| this.reload_driver_catalog(cx));
    rig.wait("the offline reload", |this| this.driver_ui.catalog_task.is_none());
    assert!(rig.read(|this| this.driver_ui.model.row("sqlite").is_some_and(|row| row.is_installed())));
}

// --- case 10 ----------------------------------------------------------------------------------

#[test]
#[ignore = "network: cancels a real download mid-stream"]
fn live_a_cancel_in_the_middle_of_a_real_download_leaves_nothing() {
    use engine_driver_host::{DownloadError, InstalledDriverStore, NoProgress, ToolStore};
    struct CancelAfter {
        inner: Box<dyn std::io::Read>,
        seen: u64,
        limit: u64,
        flag: std::sync::Arc<AtomicBool>,
    }
    impl std::io::Read for CancelAfter {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.seen += read as u64;
            if self.seen >= self.limit {
                self.flag.store(true, Ordering::Release);
            }
            Ok(read)
        }
    }
    let root = live_root("cancel");
    let store = InstalledDriverStore::new(root.join("store"));
    let tools = ToolStore::new(root.join("tools"));
    let (catalog, _) = live_catalog(
        &store,
        &tools,
        crate::drivers::DEFAULT_MANIFEST_SOURCE,
        live_trust(LIVE_OFFICIAL_KEY, TrustPolicy::Strict),
    );
    let entry = catalog.unwrap().drivers.into_iter().find(|item| item.id == "mongodb").unwrap().manifest.unwrap();
    let artifact = entry.artifact_for_target(&crate::drivers::current_target()).unwrap().clone();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let transport = engine_driver_host::classify_download_url(&artifact.url, TrustPolicy::Strict).unwrap();
    let inner = crate::drivers::open_artifact(&artifact.url, artifact.size_bytes, transport, TrustPolicy::Strict, &cancel)
        .expect("open the real artifact");
    let mut source = CancelAfter { inner, seen: 0, limit: 1024 * 1024, flag: cancel.clone() };
    let destination = crate::drivers::temporary_download_path(&store, "mongodb", &artifact.url);
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let result = engine_driver_host::stream_to_file(&mut source, &destination, artifact.size_bytes, &cancel, &NoProgress);
    eprintln!(
        "LIVE cancel after {} of {} bytes -> {:?}; partial file exists: {}",
        source.seen,
        artifact.size_bytes,
        result.as_ref().map(|d| d.size_bytes).map_err(|e| e.to_string()),
        destination.exists()
    );
    assert!(matches!(result, Err(DownloadError::Cancelled)));
    assert!(source.seen < artifact.size_bytes, "cancelled mid-stream, not after the end");
    assert!(!destination.exists(), "the partial file is removed");
    assert!(crate::drivers::directory_bytes(&crate::drivers::downloads_dir(&store)) == 0);
    assert!(store.record("mongodb").unwrap().is_none());
    assert!(!store.base_directory().join("Drivers/v2/mongodb").exists());
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
#[ignore = "network: the Cancel button during a real driver download"]
fn live_the_cancel_button_during_a_real_download_leaves_no_files_or_record(cx: &mut TestAppContext) {
    let mut rig = live_rig(cx);
    rig.open_live_manager();
    let mut cancelled = 0;
    for attempt in 1..=4 {
        rig.click("driver-action-mongodb");
        rig.wait("the job to start", |this| this.driver_ui.queue.job("mongodb").is_some());
        // Cancel as soon as the job shows (the same button slot becomes Cancel).
        let state = rig.read(|this| format!("{:?}", this.driver_ui.queue.job("mongodb").map(|job| job.state.clone())));
        rig.click("driver-action-mongodb");
        rig.wait("the cancel or the finish", |this| this.driver_ui.queue.job("mongodb").is_none());
        let installed = rig.live_record("mongodb").is_some();
        eprintln!("LIVE attempt {attempt}: state when cancelling {state}; installed anyway: {installed}");
        if installed {
            // The download won the race: remove and try again.
            rig.live_uninstall("mongodb");
            continue;
        }
        cancelled += 1;
        assert!(rig.downloads_empty(), "no partial file");
        assert!(!rig.live_driver_dir("mongodb").exists(), "nothing installed on disk");
        assert!(rig.read(|this| !this.driver_ui.handles.contains_key("mongodb")));
        assert_eq!(rig.read(|this| this.error_log.log.len()), 0, "a cancel is not an error");
        assert_eq!(
            rig.read(|this| driver_manager::primary_action(this.driver_ui.model.row("mongodb").unwrap(), None)),
            Some(PrimaryAction::Install),
            "the row offers Install again"
        );
        break;
    }
    assert!(cancelled > 0, "the Cancel button won at least once in 4 attempts");
}

// --- extra: the same proofs through the Driver Manager UI ---------------------------------------

/// Installs the catalog's real mongodb, then makes the store hold it as a 0.2.0
/// install. Returns the catalog version.
fn live_downgrade_mongodb(rig: &mut DmRig<'_>) -> String {
    let real = rig.live_install("mongodb");
    let latest = real.version.clone();
    let saved = rig.root.join("mongodb-latest.dylib");
    std::fs::copy(&real.path, &saved).unwrap();
    rig.live_uninstall("mongodb");
    let old_dir = rig.live_driver_dir("mongodb").join("0.2.0");
    std::fs::create_dir_all(&old_dir).unwrap();
    let old_path = old_dir.join(real.path.file_name().unwrap());
    std::fs::copy(&saved, &old_path).unwrap();
    rig.live_store()
        .save_installed(engine_driver_host::InstalledDriverRecord { version: "0.2.0".into(), path: old_path, ..real })
        .unwrap();
    rig.update(|this, cx| this.reload_driver_catalog(cx));
    rig.wait("the reload", |this| {
        this.driver_ui.catalog_task.is_none() && this.driver_ui.model.row("mongodb").is_some_and(|row| row.is_installed())
    });
    latest
}

#[gpui::test]
#[ignore = "network: Update driver first updates mongodb to the catalog version, then installs the shell"]
fn live_update_driver_first_updates_the_driver_then_installs_the_shell(cx: &mut TestAppContext) {
    let mut rig = live_rig(cx);
    rig.open_live_manager();
    let latest = live_downgrade_mongodb(&mut rig);
    rig.update(|this, cx| this.run_tool_action_of("tablex-mongo-shell", driver_manager::ToolAction::UpdateDriverFirst, cx));
    rig.wait("the driver update and the add-on install", |this| {
        this.driver_ui.queue.job("mongodb").is_none()
            && this.driver_ui.tools.job_of("tablex-mongo-shell").is_none_or(|job| !job.state.is_active())
            && this.driver_ui.tools.after_driver.is_none()
            && this.driver_ui.tools.item_of("tablex-mongo-shell").is_some_and(|item| item.installed.is_some())
    });
    let record = rig.live_record("mongodb").unwrap();
    eprintln!("LIVE Update driver first -> mongodb {} (previous {:?}), shell installed", record.version, record.previous_version);
    assert_eq!(record.version, latest);
    assert!(rig.live_tool_installed("tablex-mongo-shell"));
    assert!(rig.read(|this| this.addon_need(this.driver_ui.tools.item_of("tablex-mongo-shell").unwrap()).is_none()));
}

#[gpui::test]
#[ignore = "network: a one-byte-changed manifest as the Driver Manager's explicit file:// source"]
fn live_driver_manager_refuses_a_tampered_manifest_and_offers_nothing_from_it(cx: &mut TestAppContext) {
    let mut rig = live_rig(cx);
    rig.open_live_manager();
    rig.live_install("sqlite");
    let manifest = live_fetch(crate::drivers::DEFAULT_MANIFEST_SOURCE);
    let signature = live_fetch(&format!("{}.sig", crate::drivers::DEFAULT_MANIFEST_SOURCE));
    let dir = rig.root.join("tampered");
    std::fs::create_dir_all(&dir).unwrap();
    let position = manifest.windows(8).position(|w| w == b"sqlite\",").unwrap() + 1;
    let mut tampered = manifest.clone();
    tampered[position] = b'S';
    std::fs::write(dir.join("drivers-manifest.json"), &tampered).unwrap();
    std::fs::write(dir.join("drivers-manifest.json.sig"), &signature).unwrap();
    let source = file_url(&dir.join("drivers-manifest.json"));
    rig.update(|this, _| this.driver_ui.manifest_source = Some(source));
    rig.click("driver-check-updates");
    rig.wait("the refusal", |this| this.driver_ui.catalog_task.is_none());
    let error = rig.read(|this| this.driver_ui.catalog_error.clone()).expect("reported");
    eprintln!("LIVE tampered manifest in the Driver Manager -> catalog_error: {error}");
    assert!(error.contains("signature"), "{error}");
    let (rows, installable) = rig.read(|this| {
        (
            this.driver_ui.model.rows().len(),
            this.driver_ui.model.rows().iter().filter(|row| row.entry.is_some()).count(),
        )
    });
    eprintln!("LIVE   rows left {rows}, rows with an installable entry {installable}");
    assert_eq!(installable, 0, "nothing can be installed or updated from the refused manifest");
    assert!(rig.read(|this| this.driver_ui.model.row("sqlite").is_some_and(|row| row.is_installed())));
    assert!(!rig.rendered("driver-action-postgresql"));
}
