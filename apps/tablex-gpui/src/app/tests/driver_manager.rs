// Driver Manager proofs with real clicks. A local `file://` manifest (developer
// mode) lists the REAL SQLite driver dylib built by `built_driver`; installs,
// updates, rollbacks and removals go through the real installer / loader. Every
// scratch store lives in a temp dir, never in the user's driver data dir.

use driver_manager::{DriverFilter, InstallState, PrimaryAction};
use engine_driver_host::TrustPolicy;

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn file_url(path: &std::path::Path) -> String {
    url::Url::from_file_path(path).expect("absolute path").to_string()
}

/// A `file://` artifact that streams like a slow download: a FIFO whose
/// writer sends the first half, then waits for `release` (dropping it also
/// releases).
struct SlowArtifact {
    path: std::path::PathBuf,
    release: Option<std::sync::mpsc::Sender<()>>,
}

impl SlowArtifact {
    fn new(dir: &std::path::Path, bytes: Vec<u8>) -> Self {
        use std::io::Write;
        let path = dir.join("slow-artifact.fifo");
        assert!(std::process::Command::new("mkfifo").arg(&path).status().unwrap().success());
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let fifo = path.clone();
        std::thread::spawn(move || {
            // Blocks until the installer opens the read end.
            let Ok(mut out) = std::fs::OpenOptions::new().write(true).open(&fifo) else {
                return;
            };
            let half = bytes.len() / 2;
            let _ = out.write_all(&bytes[..half]);
            let _ = out.flush();
            let _ = wait.recv();
            let _ = out.write_all(&bytes[half..]);
        });
        Self { path, release: Some(release) }
    }

    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl Drop for SlowArtifact {
    fn drop(&mut self) {
        self.release();
    }
}

struct DmRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    root: std::path::PathBuf,
    store_dir: std::path::PathBuf,
    /// Where the Tools section installs `tablex-mcp` (`<data dir>/tools`).
    tools_dir: std::path::PathBuf,
    dylib: std::path::PathBuf,
    sha: String,
    size: u64,
}

impl<'a> DmRig<'a> {
    fn new(cx: &'a mut TestAppContext, policy: TrustPolicy) -> Self {
        let dylib = built_driver("dbengine-sqlite", "");
        cx.dispatcher.allow_parking();
        cx.update(|cx| cx.set_reduce_motion(true));
        let (app, visual) = sidebar_test_app(cx);
        visual.simulate_resize(gpui::size(gpui::px(1500.0), gpui::px(850.0)));
        let root = std::env::temp_dir().join(format!("tablex-dm-{}", uuid::Uuid::new_v4()));
        let store_dir = root.join("store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let manifest = root.join("manifest.json");
        let rig_store = store_dir.clone();
        let tools_dir = root.join("tools");
        let rig_tools = tools_dir.clone();
        app.update(visual, |this, cx| {
            this.driver_ui.tools.store = Some(std::sync::Arc::new(engine_driver_host::ToolStore::new(&rig_tools)));
            this.scenario_ui.service = None;
            this.driver_ui.store = std::sync::Arc::new(engine_driver_host::InstalledDriverStore::new(&rig_store));
            this.driver_ui.policy = policy;
            this.driver_ui.manifest_source = Some(file_url(&manifest));
            this.driver_ui.installed_loaded = true;
            cx.notify();
        });
        settle(visual);
        let size = std::fs::metadata(&dylib).unwrap().len();
        Self { app, visual, root, store_dir, tools_dir, sha: driver_sha256(&dylib), dylib, size }
    }

    /// Publishes `version` of the SQLite driver at `url` (default: a copy of
    /// the real dylib under this version).
    fn publish_at(&self, version: &str, url: &str) {
        let entry = |id: &str, name: &str, version: &str, url: &str| serde_json::json!({
                "id": id, "displayName": name, "version": version,
                "appAbiVersion": 2, "uriSchemes": [id],
                "features": ["navigator", "query.sql", "query.arrow_c_data", "operation.cancel"],
                "license": "MIT", "sourceRepo": "https://example.com/tablex", "teamId": "",
                "artifacts": [{
                    "target": crate::drivers::current_target().id(),
                    "url": url, "sha256": self.sha, "sizeBytes": self.size,
                }],
                "category": "sql", "description": "test driver",
        });
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "generatedAt": "2026-10-08T00:00:00Z",
            // MySQL is only ever offered, never installed, by these tests.
            "drivers": [entry("mysql", "MySQL", "1.0.0", "file:///nonexistent-mysql.dylib"), {
                "id": "sqlite", "displayName": "SQLite", "version": version,
                "appAbiVersion": 2, "uriSchemes": ["sqlite"],
                "features": ["navigator", "query.sql", "query.arrow_c_data", "operation.cancel"],
                "license": "MIT", "sourceRepo": "https://example.com/tablex", "teamId": "",
                "artifacts": [{
                    "target": crate::drivers::current_target().id(),
                    "url": url, "sha256": self.sha, "sizeBytes": self.size,
                }],
                "category": "sql", "description": "SQLite test driver",
            }],
        });
        std::fs::write(self.root.join("manifest.json"), manifest.to_string()).unwrap();
    }

    fn publish(&self, version: &str) {
        let artifact = self.root.join(format!("artifact-{version}.dylib"));
        std::fs::copy(&self.dylib, &artifact).unwrap();
        self.publish_at(version, &file_url(&artifact));
    }

    fn read<R>(&self, f: impl FnOnce(&AppRoot) -> R) -> R {
        self.app.read_with(self.visual, |this, _| f(this))
    }

    fn update<R>(&mut self, f: impl FnOnce(&mut AppRoot, &mut Context<AppRoot>) -> R) -> R {
        self.app.update(self.visual, f)
    }

    /// Polls until `ready`. Installs shell out to `codesign` and `dlopen` real
    /// dylibs on blocking threads, so under a loaded machine (the full suite
    /// next to other builds) they take far longer than `wait_until`'s 15 s.
    fn wait(&mut self, label: &'static str, ready: impl Fn(&AppRoot) -> bool) {
        self.visual.dispatcher.allow_parking();
        let started = std::time::Instant::now();
        loop {
            self.visual.run_until_parked();
            if self.read(&ready) {
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(180), "timed out waiting for {label}");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        settle(self.visual);
    }

    fn click(&mut self, selector: &str) {
        click_debug_button(self.visual, leak(selector.to_string()));
        settle(self.visual);
    }

    fn rendered(&mut self, selector: &str) -> bool {
        self.visual.debug_bounds(leak(selector.to_string())).is_some()
    }

    fn open_manager(&mut self) {
        self.update(|this, cx| this.open_driver_manager(cx));
        self.wait("the catalog", |this| {
            this.driver_ui.catalog_task.is_none() && !this.driver_ui.model.rows().is_empty()
        });
    }

    /// Opens the `⋯` menu of `id` and picks the `index`-th enabled entry.
    fn pick_menu(&mut self, id: &str, index: usize) {
        self.click(&format!("driver-more-{id}"));
        for _ in 0..=index {
            self.visual.simulate_keystrokes("down");
        }
        self.visual.simulate_keystrokes("enter");
        settle(self.visual);
    }

    /// Opens the toolbar `⋯` menu and picks the `index`-th entry.
    fn pick_toolbar_menu(&mut self, index: usize) {
        self.click("driver-more-actions");
        for _ in 0..=index {
            self.visual.simulate_keystrokes("down");
        }
        self.visual.simulate_keystrokes("enter");
        settle(self.visual);
    }

    fn install_current(&mut self) {
        self.click("driver-action-sqlite");
        self.wait("the install", |this| {
            this.driver_ui.queue.job("sqlite").is_none() && this.driver_ui.handles.contains_key("sqlite")
        });
    }

    fn save_sqlite(&mut self, db: &SqliteDb, name: &str) -> (String, u64) {
        let uri = db.uri();
        let name = name.to_string();
        self.update(|this, cx| {
            let id = uuid::Uuid::new_v4();
            this.saved_connections.push(SavedConnection {
                id,
                name,
                uri,
                kind: ConnectionKind::Sqlite,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            let key = id.to_string();
            let tab = this.workspace.open_query_tab_with_metadata(
                "SQL",
                TabMetadata { connection_id: key.clone(), ..Default::default() },
            );
            this.reset_active_view();
            cx.notify();
            (key, tab)
        })
    }

    fn connect(&mut self, key: &str) {
        let key = key.to_string();
        let uri = self.read(|this| {
            this.saved_connections.iter().find(|saved| saved.matches_id(&key)).unwrap().uri.clone()
        });
        let k = key.clone();
        self.update(|this, cx| this.ensure_connected_with_uri(k, Some(uri.into()), None, cx));
        let k = key.clone();
        self.wait("the connection", move |this| {
            matches!(this.sessions.status(&k), ConnectionStatus::Connected)
                && this.driver_runtime.sessions.contains_key(&k)
        });
    }

    /// Brings the Driver Manager tab back to the front.
    fn show_manager(&mut self) {
        self.update(|this, cx| {
            this.workspace.open_driver_manager_tab();
            cx.notify();
        });
        settle(self.visual);
    }

    /// Runs a SELECT in `tab` (activating it) and returns its row count; the
    /// Driver Manager tab is shown again afterwards.
    fn select_rows(&mut self, tab: u64) -> usize {
        self.update(|this, cx| {
            this.workspace.activate_tab(tab);
            this.workspace.tab_mut(tab).unwrap().sql = "SELECT name FROM items ORDER BY id".into();
            this.reset_active_view();
            this.run_active_tab_query(cx);
        });
        self.wait("the SELECT", move |this| {
            matches!(this.workspace.runtime(tab).unwrap().query.status(), crate::query::QueryStatus::Ready { .. })
        });
        let rows = self.read(|this| this.workspace.runtime(tab).unwrap().grid.row_count());
        self.show_manager();
        rows
    }

    fn session_version(&self, key: &str) -> String {
        self.read(|this| {
            this.driver_runtime
                .session_handles
                .get(key)
                .map(|handle| handle.path().to_string_lossy().into_owned())
                .unwrap_or_default()
        })
    }

    fn downloads_empty(&self) -> bool {
        let downloads = self.store_dir.join("Drivers/v2/downloads");
        std::fs::read_dir(downloads).map_or(true, |mut entries| entries.next().is_none())
    }

    fn version_dirs(&self) -> Vec<String> {
        let mut dirs: Vec<String> = std::fs::read_dir(self.store_dir.join("Drivers/v2/sqlite"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        dirs.sort();
        dirs
    }

    fn stored_version(&self) -> Option<String> {
        engine_driver_host::InstalledDriverStore::new(&self.store_dir)
            .record("sqlite")
            .unwrap()
            .map(|record| record.version)
    }
}

impl Drop for DmRig<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[gpui::test]
fn install_shows_real_progress_then_the_driver_connects_without_a_restart(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let mut slow = SlowArtifact::new(&rig.root, std::fs::read(&rig.dylib).unwrap());
    rig.publish_at("1.0.0", &file_url(&slow.path));
    rig.open_manager();
    assert!(rig.rendered("driver-row-sqlite") && rig.rendered("driver-action-sqlite"));
    assert_eq!(
        rig.read(|this| driver_manager::primary_action(this.driver_ui.model.row("sqlite").unwrap(), None)),
        Some(PrimaryAction::Install)
    );

    rig.click("driver-action-sqlite");
    rig.wait("download progress", |this| {
        matches!(
            this.driver_ui.queue.job("sqlite").map(|job| &job.state),
            Some(InstallState::Downloading { bytes, .. }) if *bytes > 0
        )
    });
    assert!(rig.rendered("driver-progress-sqlite"), "the row shows the inline progress bar");
    let fraction = rig.read(|this| this.driver_ui.queue.job("sqlite").unwrap().state.fraction());
    assert!(fraction > 0.0 && fraction < 1.0, "mid-download: {fraction}");
    assert!(!rig.read(|this| this.driver_ui.handles.contains_key("sqlite")), "not usable yet");

    slow.release();
    rig.wait("the install", |this| {
        this.driver_ui.queue.job("sqlite").is_none() && this.driver_ui.handles.contains_key("sqlite")
    });
    assert!(!rig.rendered("driver-progress-sqlite"));
    assert!(rig.downloads_empty(), "the downloaded file is removed after the install");
    assert_eq!(rig.stored_version().as_deref(), Some("1.0.0"));

    // Usable at once: connect and SELECT with no restart.
    let (key, tab) = rig.save_sqlite(&db, "after install");
    rig.connect(&key);
    assert_eq!(rig.select_rows(tab), 5);
    assert!(rig.session_version(&key).contains("/sqlite/1.0.0/"), "{}", rig.session_version(&key));
}

#[gpui::test]
fn cancelling_mid_download_leaves_no_partial_file_and_the_row_returns_to_install(
    cx: &mut TestAppContext,
) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let mut slow = SlowArtifact::new(&rig.root, std::fs::read(&rig.dylib).unwrap());
    rig.publish_at("1.0.0", &file_url(&slow.path));
    rig.open_manager();
    rig.click("driver-action-sqlite");
    rig.wait("download progress", |this| {
        matches!(
            this.driver_ui.queue.job("sqlite").map(|job| &job.state),
            Some(InstallState::Downloading { bytes, .. }) if *bytes > 0
        )
    });
    assert!(!rig.downloads_empty(), "a partial file exists while downloading");

    // The same slot now holds the cancel button.
    assert_eq!(
        rig.read(|this| {
            let row = this.driver_ui.model.row("sqlite").unwrap();
            driver_manager::primary_action(row, this.driver_ui.queue.job("sqlite"))
        }),
        Some(PrimaryAction::Cancel)
    );
    rig.click("driver-action-sqlite");
    assert!(rig.read(|this| this.driver_ui.queue.job("sqlite").unwrap().cancelling()));
    slow.release();
    rig.wait("the cancel", |this| this.driver_ui.queue.job("sqlite").is_none());

    assert!(rig.downloads_empty(), "no partial file");
    assert!(rig.version_dirs().is_empty(), "nothing was installed");
    assert_eq!(rig.stored_version(), None);
    assert!(!rig.rendered("driver-progress-sqlite"));
    assert_eq!(
        rig.read(|this| driver_manager::primary_action(this.driver_ui.model.row("sqlite").unwrap(), None)),
        Some(PrimaryAction::Install),
        "the row offers Install again"
    );
    assert!(rig.read(|this| this.error_log.log.len()) == 0, "a cancel is not an error");
}

#[gpui::test]
fn update_keeps_an_open_session_on_the_old_version_until_reconnect_them(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.install_current();
    let (old_key, old_tab) = rig.save_sqlite(&db, "old session");
    rig.connect(&old_key);
    assert_eq!(rig.select_rows(old_tab), 5);

    rig.publish("2.0.0");
    rig.click("driver-check-updates");
    rig.wait("the update to be listed", |this| {
        this.driver_ui.catalog_task.is_none()
            && this.driver_ui.model.row("sqlite").is_some_and(|row| row.has_update())
    });
    assert!(rig.rendered("driver-banner"), "the updates banner");
    assert!(rig.rendered("driver-update-badge"), "the dot on the Driver Manager tab");
    assert!(rig.rendered("driver-update-all"));
    rig.click("driver-action-sqlite");
    rig.wait("the update", |this| {
        this.driver_ui.queue.job("sqlite").is_none()
            && this.driver_ui.handles.get("sqlite").is_some_and(|handle| {
                handle.path().to_string_lossy().contains("/sqlite/2.0.0/")
            })
    });
    assert_eq!(rig.version_dirs(), ["1.0.0", "2.0.0"], "installed next to the old one");

    // The old session still works, on the old generation.
    assert!(rig.session_version(&old_key).contains("/sqlite/1.0.0/"));
    assert_eq!(rig.select_rows(old_tab), 5, "the open session keeps running queries");
    assert_eq!(rig.read(|this| this.driver_sessions("sqlite").iter().filter(|s| s.stale).count()), 1);
    assert_eq!(rig.read(|this| this.stale_session_version("sqlite")).as_deref(), Some("1.0.0"));
    assert!(rig.rendered("driver-detail") && rig.rendered("driver-reconnect-stale"));

    // A new connection uses the new version.
    let (new_key, new_tab) = rig.save_sqlite(&db, "new session");
    rig.connect(&new_key);
    assert!(rig.session_version(&new_key).contains("/sqlite/2.0.0/"), "{}", rig.session_version(&new_key));
    assert_eq!(rig.select_rows(new_tab), 5);

    // Narrow window: the detail pane gives way, and the row itself offers
    // Reconnect (the same action as the pane's "Reconnect them").
    rig.visual.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(850.0)));
    settle(rig.visual);
    assert!(!rig.rendered("driver-reconnect-stale"), "no detail pane when narrow");
    assert_eq!(
        rig.read(|this| driver_manager::row_action(
            this.driver_ui.model.row("sqlite").unwrap(),
            None,
            this.driver_sessions("sqlite").iter().any(|session| session.stale),
        )),
        Some(PrimaryAction::Reconnect)
    );
    rig.click("driver-action-sqlite");
    let key = old_key.clone();
    rig.wait("the reconnect", move |this| {
        matches!(this.sessions.status(&key), ConnectionStatus::Connected)
            && this
                .driver_runtime
                .session_handles
                .get(&key)
                .is_some_and(|handle| handle.path().to_string_lossy().contains("/sqlite/2.0.0/"))
    });
    assert_eq!(rig.select_rows(old_tab), 5);
    assert_eq!(rig.read(|this| this.driver_sessions("sqlite").iter().filter(|s| s.stale).count()), 0);
    assert!(!rig.rendered("driver-reconnect-stale"));
}

#[gpui::test]
fn uninstall_with_an_open_session_confirms_then_disconnects_and_removes(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.install_current();
    let (key, tab) = rig.save_sqlite(&db, "open connection");
    rig.connect(&key);
    assert_eq!(rig.select_rows(tab), 5);

    // Menu: Disable(0), Uninstall(1).
    rig.pick_menu("sqlite", 1);
    assert!(rig.rendered("driver-confirm-body"), "the confirm dialog lists the open connections");
    assert_eq!(rig.read(|this| this.driver_sessions("sqlite").len()), 1);
    rig.click("driver-confirm-cancel");
    assert!(!rig.rendered("driver-confirm-body"));
    assert_eq!(rig.stored_version().as_deref(), Some("1.0.0"), "cancel changes nothing");
    assert!(rig.read(|this| this.driver_runtime.sessions.contains_key(&key)));

    rig.pick_menu("sqlite", 1);
    rig.click("driver-confirm-ok");
    rig.wait("the uninstall", |this| this.driver_ui.op_task.is_none() && this.driver_ui.model.row("sqlite").is_some_and(|row| !row.is_installed()));
    assert_eq!(rig.stored_version(), None);
    assert!(rig.version_dirs().is_empty(), "every version is deleted");
    assert!(rig.read(|this| this.driver_runtime.sessions.is_empty()), "the session was disconnected");
    assert!(rig.read(|this| !this.driver_ui.handles.contains_key("sqlite") && !this.driver_ui.installed.contains("sqlite")));
    assert_eq!(
        rig.read(|this| driver_manager::primary_action(this.driver_ui.model.row("sqlite").unwrap(), None)),
        Some(PrimaryAction::Install)
    );
}

#[gpui::test]
fn roll_back_makes_the_previous_version_current_and_loads_it(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.install_current();
    rig.publish("2.0.0");
    rig.click("driver-check-updates");
    rig.wait("the update to be listed", |this| {
        this.driver_ui.catalog_task.is_none() && this.driver_ui.model.row("sqlite").is_some_and(|row| row.has_update())
    });
    rig.click("driver-action-sqlite");
    rig.wait("the update", |this| {
        this.driver_ui.queue.job("sqlite").is_none()
            && this.driver_ui.records.get("sqlite").is_some_and(|record| record.version == "2.0.0")
    });

    // Menu: Disable(0), Uninstall(1), Reinstall(2), Roll back(3).
    rig.pick_menu("sqlite", 3);
    rig.wait("the rollback", |this| {
        this.driver_ui.op_task.is_none()
            && this.driver_ui.records.get("sqlite").is_some_and(|record| record.version == "1.0.0")
    });
    assert_eq!(rig.stored_version().as_deref(), Some("1.0.0"));
    assert!(rig.read(|this| {
        this.driver_ui.handles.get("sqlite").is_some_and(|handle| handle.path().to_string_lossy().contains("/sqlite/1.0.0/"))
    }));
    assert!(rig.read(|this| this.driver_ui.model.row("sqlite").unwrap().has_update()), "2.0.0 is offered again");
}

#[gpui::test]
fn disable_and_enable_flip_the_row_and_the_route(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.install_current();

    rig.pick_menu("sqlite", 0); // Disable (no sessions: no confirm).
    rig.wait("the disable", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.model.row("sqlite").is_some_and(|row| row.disabled)
    });
    assert!(rig.read(|this| !this.driver_ui.installed.contains("sqlite") && !this.driver_ui.handles.contains_key("sqlite")));
    assert_eq!(
        rig.read(|this| {
            let row = this.driver_ui.model.row("sqlite").unwrap();
            (driver_manager::badge_for(row, None), driver_manager::primary_action(row, None))
        }),
        (Some(driver_manager::Badge::Disabled), Some(PrimaryAction::Enable))
    );
    assert!(rig.rendered("driver-badge-sqlite"));

    rig.click("driver-action-sqlite"); // Enable.
    rig.wait("the enable", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.model.row("sqlite").is_some_and(|row| !row.disabled)
    });
    assert!(rig.read(|this| this.driver_ui.installed.contains("sqlite")));
    assert_eq!(
        rig.read(|this| driver_manager::badge_for(this.driver_ui.model.row("sqlite").unwrap(), None)),
        Some(driver_manager::Badge::Developer),
        "enabled again (an unsigned local build keeps its Developer badge)"
    );
}

#[gpui::test]
fn a_driver_without_a_pinned_team_is_badged_developer(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.install_current();
    assert_eq!(
        rig.read(|this| driver_manager::badge_for(this.driver_ui.model.row("sqlite").unwrap(), None)),
        Some(driver_manager::Badge::Developer)
    );
    assert!(rig.rendered("driver-badge-sqlite"));
}

#[gpui::test]
fn strict_mode_rejects_a_local_manifest_with_a_clear_error(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Strict);
    rig.publish("1.0.0");
    rig.update(|this, cx| this.open_driver_manager(cx));
    rig.wait("the catalog result", |this| this.driver_ui.catalog_task.is_none());
    let error = rig.read(|this| this.driver_ui.catalog_error.clone()).expect("an error");
    assert!(error.contains("Developer mode"), "{error}");
    assert!(rig.rendered("driver-catalog-error"), "the tab shows it");
    assert!(rig.read(|this| this.driver_ui.model.rows().is_empty()), "nothing is offered from an untrusted manifest");
    assert_eq!(rig.read(|this| this.error_log.log.len()), 1, "and the Error Log has it");
}

#[gpui::test]
fn developer_mode_needs_a_confirmation_then_shows_a_warning_and_survives_a_restart(
    cx: &mut TestAppContext,
) {
    let mut rig = DmRig::new(cx, TrustPolicy::Strict);
    rig.publish("1.0.0");
    rig.update(|this, cx| this.open_driver_manager(cx));
    rig.wait("the strict refusal", |this| this.driver_ui.catalog_task.is_none());
    assert!(rig.read(|this| this.driver_ui.catalog_error.is_some()), "a local manifest is refused");
    assert!(!rig.rendered("driver-developer-banner"), "no warning while strict");

    // Menu: Import local driver(0), Developer mode(1). Cancel changes nothing.
    rig.pick_toolbar_menu(1);
    assert!(rig.rendered("driver-developer-body"), "the risk is explained first");
    assert_eq!(rig.read(|this| this.driver_ui.policy), TrustPolicy::Strict, "not on before the confirm");
    rig.click("driver-developer-cancel");
    assert!(!rig.rendered("driver-developer-body"));
    assert_eq!(rig.read(|this| (this.driver_ui.policy, this.driver_ui.developer_mode)), (TrustPolicy::Strict, false));
    assert!(!crate::drivers::read_developer_mode(&engine_driver_host::InstalledDriverStore::new(&rig.store_dir)));

    rig.pick_toolbar_menu(1);
    rig.click("driver-developer-ok");
    rig.wait("the developer catalog", |this| {
        this.driver_ui.catalog_task.is_none() && !this.driver_ui.model.rows().is_empty()
    });
    assert_eq!(rig.read(|this| this.driver_ui.policy), TrustPolicy::Developer);
    assert!(rig.read(|this| this.driver_ui.catalog_error.is_none()), "the local manifest is now accepted");
    assert!(rig.rendered("driver-developer-banner"), "a persistent warning is shown");
    assert!(
        crate::drivers::read_developer_mode(&engine_driver_host::InstalledDriverStore::new(&rig.store_dir)),
        "the choice is saved"
    );
    // A restart reads the saved choice.
    let restarted = driver_manager::DriverManagerUi::new(std::sync::Arc::new(
        engine_driver_host::InstalledDriverStore::new(&rig.store_dir),
    ));
    assert!(restarted.developer_mode && restarted.policy == TrustPolicy::Developer);

    // Turning it off needs no confirmation and puts the checks back.
    rig.pick_toolbar_menu(1);
    assert!(!rig.rendered("driver-developer-body"));
    rig.wait("the strict catalog", |this| this.driver_ui.catalog_task.is_none());
    assert_eq!(rig.read(|this| (this.driver_ui.policy, this.driver_ui.developer_mode)), (TrustPolicy::Strict, false));
    assert!(!rig.rendered("driver-developer-banner"));
    assert!(rig.read(|this| this.driver_ui.catalog_error.is_some()), "the local manifest is refused again");
    assert!(!crate::drivers::read_developer_mode(&engine_driver_host::InstalledDriverStore::new(&rig.store_dir)));
}

#[gpui::test]
fn a_missing_driver_dialog_installs_and_connects_and_runs_the_pending_statement(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.update(|this, cx| {
        this.reload_driver_catalog(cx);
        this.refresh_driver_catalog(cx);
    });
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.is_downloadable("sqlite"));
    let (key, tab) = rig.save_sqlite(&db, "needs driver");
    rig.update(|this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT name FROM items ORDER BY id".into();
        this.run_active_tab_query(cx);
    });
    rig.wait("the dialog", |this| this.driver_ui.prompt.is_some());
    settle(rig.visual);
    assert!(rig.rendered("driver-prompt-body"), "the missing-driver dialog is open");
    assert!(rig.rendered("driver-prompt-install") && rig.rendered("driver-prompt-cancel") && rig.rendered("driver-prompt-open-manager"));
    assert!(rig.read(|this| this.query_tasks.pending_runs.contains_key(&tab)), "the run waits on the connect");
    assert!(!rig.rendered("driver-prompt-progress"), "no progress before the user decides");

    rig.click("driver-prompt-install");
    let tab_for_wait = tab;
    rig.wait("connected and the run finished", move |this| {
        this.driver_ui.prompt.is_none()
            && matches!(
                this.workspace.runtime(tab_for_wait).unwrap().query.status(),
                crate::query::QueryStatus::Ready { .. }
            )
    });
    assert!(!rig.rendered("driver-prompt-body"), "the dialog closed itself");
    assert_eq!(rig.read(|this| this.workspace.runtime(tab).unwrap().grid.row_count()), 5, "the pending run executed");
    assert!(rig.read(|this| matches!(this.sessions.status(&key), ConnectionStatus::Connected)));
    assert!(rig.session_version(&key).contains("/sqlite/1.0.0/"));
}

#[gpui::test]
fn cancelling_the_missing_driver_dialog_fails_the_connect_and_its_pending_run(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.update(|this, cx| {
        this.refresh_driver_catalog(cx);
    });
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.is_downloadable("sqlite"));
    let (key, tab) = rig.save_sqlite(&db, "needs driver");
    rig.update(|this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT 1".into();
        this.run_active_tab_query(cx);
    });
    rig.wait("the dialog", |this| this.driver_ui.prompt.is_some());
    settle(rig.visual);
    rig.click("driver-prompt-cancel");
    let k = key.clone();
    rig.wait("the failed connect", move |this| matches!(this.sessions.status(&k), ConnectionStatus::Failed(_)));
    assert!(rig.read(|this| this.driver_ui.prompt.is_none()));
    assert!(!rig.rendered("driver-prompt-body"));
    let status = rig.read(|this| this.workspace.runtime(tab).unwrap().query.status().clone());
    assert!(matches!(status, crate::query::QueryStatus::Failed(_)), "{status:?}");
    assert_eq!(rig.stored_version(), None, "nothing was installed");
}

#[gpui::test]
fn open_driver_manager_from_the_dialog_abandons_the_connect_and_shows_the_tab(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.update(|this, cx| {
        this.refresh_driver_catalog(cx);
    });
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.is_downloadable("sqlite"));
    let (_, tab) = rig.save_sqlite(&db, "needs driver");
    rig.update(|this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT 1".into();
        this.run_active_tab_query(cx);
    });
    rig.wait("the dialog", |this| this.driver_ui.prompt.is_some());
    settle(rig.visual);
    rig.click("driver-prompt-open-manager");
    settle(rig.visual);
    assert!(rig.read(|this| this.driver_ui.prompt.is_none()));
    assert!(rig.read(|this| this.workspace.active_tab().is_some_and(|tab| tab.kind == TabKind::DriverManager)));
    assert!(rig.rendered("driver-manager"));
}

#[gpui::test]
fn filter_chips_and_search_narrow_the_rendered_list(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    assert!(rig.rendered("driver-row-sqlite"));
    rig.click(DriverFilter::Updates.selector());
    assert!(!rig.rendered("driver-row-sqlite"), "no updates: the Updates chip is empty");
    assert!(rig.rendered("driver-empty"));
    rig.click(DriverFilter::Category(engine_driver_host::DriverCategory::Sql).selector());
    assert!(rig.rendered("driver-row-sqlite"), "SQLite is a SQL driver");
    rig.click(DriverFilter::Category(engine_driver_host::DriverCategory::Nosql).selector());
    assert!(!rig.rendered("driver-row-sqlite"));
    rig.click(DriverFilter::All.selector());
    assert!(rig.rendered("driver-row-sqlite"));
    rig.update(|this, cx| {
        this.driver_ui.model.set_query("zzz");
        cx.notify();
    });
    settle(rig.visual);
    assert!(!rig.rendered("driver-row-sqlite"));
}

// --- P5b review findings --------------------------------------------------------

#[gpui::test]
fn disabling_a_driver_while_a_connect_is_in_flight_asks_and_discards_that_connect(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.install_current();
    // Forget the loaded handle: a fresh connect has to load the driver again.
    rig.update(|this, _| {
        this.driver_ui.handles.clear();
    });
    let (key, _) = rig.save_sqlite(&db, "connecting");
    let k = key.clone();
    let uri = db.uri();
    rig.update(move |this, cx| {
        this.ensure_connected_with_uri(k.clone(), Some(uri.into()), None, cx);
        assert!(matches!(this.sessions.status(&k), ConnectionStatus::Connecting));
        let sessions = this.driver_sessions("sqlite");
        assert_eq!(sessions.len(), 1, "a connect in flight counts as using the driver");
        assert!(sessions[0].connecting);
    });
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| this.request_driver_removal("sqlite".into(), false, window, cx));
    });
    settle(rig.visual);
    assert!(rig.rendered("driver-confirm-body"), "disabling asks instead of racing the connect");
    rig.click("driver-confirm-ok");
    rig.wait("the disable", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.model.row("sqlite").is_some_and(|row| row.disabled)
    });
    // Give the abandoned connect time to finish; it must not re-insert the handle.
    for _ in 0..40 {
        settle(rig.visual);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(rig.read(|this| !this.driver_ui.handles.contains_key("sqlite")), "the finishing connect did not re-enable the route");
    assert!(rig.read(|this| this.driver_runtime.sessions.is_empty()));
    assert!(rig.read(|this| !this.driver_ui.has_driver("sqlite")), "disabled stays disabled");
}

#[gpui::test]
fn startup_reads_the_driver_store_off_the_ui_thread(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("tablex-startup-{}", uuid::Uuid::new_v4()));
    let store = engine_driver_host::InstalledDriverStore::new(&dir);
    store
        .save(engine_driver_host::InstalledDriverRecord {
            id: "sqlite".into(),
            version: "1.0.0".into(),
            path: dir.join("missing.dylib"),
            team_id: "T".into(),
            ..Default::default()
        })
        .unwrap();
    // Another process holds the exclusive store lock.
    let lock = std::fs::OpenOptions::new().read(true).write(true).open(store.lock_path()).unwrap();
    lock.lock().unwrap();

    let (app, visual) = sidebar_test_app(cx);
    visual.dispatcher.allow_parking();
    let started = std::time::Instant::now();
    app.update(visual, |this, cx| {
        this.scenario_ui.service = None;
        this.driver_ui.store = std::sync::Arc::new(store.clone());
        this.driver_ui.installed_loaded = false;
        this.driver_ui.installed.clear();
        this.driver_ui.manifest_source = Some("file:///tablex-no-such-manifest.json".into());
        this.start_driver_startup(cx);
    });
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "startup returned without waiting for the lock");
    assert!(app.read_with(visual, |this, _| !this.driver_ui.installed_loaded), "drivers are loading");

    lock.unlock().unwrap();
    wait_until(visual, "the store read", |visual| app.read_with(visual, |this, _| this.driver_ui.installed_loaded));
    assert!(app.read_with(visual, |this, _| this.driver_ui.installed.contains("sqlite")));
    let _ = std::fs::remove_dir_all(dir);
}

#[gpui::test]
fn the_startup_check_runs_once_a_day_flags_updates_and_stays_silent_offline(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.install_current();
    rig.publish("2.0.0");
    let marker = rig.store_dir.join("Drivers/v2/last-update-check");
    let _ = std::fs::remove_file(&marker);

    // First start: due. The update is found and remembered.
    rig.update(|this, cx| {
        this.driver_ui.model = Default::default();
        this.driver_ui.updates_available = 0;
        this.start_driver_startup(cx);
    });
    rig.wait("the startup check", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.updates_available == 1);
    assert!(marker.exists(), "the check time is persisted");
    assert!(rig.read(|this| this.driver_ui.updates_announced), "the one-time info toast was issued");

    // Second start the same day: not due. The manifest is gone, yet nothing is logged.
    std::fs::remove_file(rig.root.join("manifest.json")).unwrap();
    rig.update(|this, cx| this.start_driver_startup(cx));
    rig.wait("the second start", |this| this.driver_ui.catalog_task.is_none());
    assert_eq!(rig.read(|this| this.error_log.log.len()), 0, "no network attempt, no log entry");

    // A due check with no reachable manifest: silent, one Info entry.
    let _ = std::fs::remove_file(&marker);
    rig.update(|this, cx| this.start_driver_startup(cx));
    rig.wait("the failed check", |this| this.driver_ui.catalog_task.is_none());
    settle(rig.visual);
    let infos = rig.read(|this| {
        (0..this.error_log.log.len())
            .filter_map(|index| this.error_log.log.get(index))
            .filter(|entry| entry.severity == crate::error_log::Severity::Info && entry.area == Area::Driver)
            .count()
    });
    assert_eq!(infos, 1, "an Info entry, not an error");
}

#[test]
fn a_not_connected_rejection_is_a_lost_connection_without_parsing_text() {
    let rejected = |code: &str| {
        DriverQueryError::Driver(engine_driver_host::LoaderError::DriverRejected {
            code: code.into(),
            message: "gone".into(),
            retryable: false,
        })
    };
    assert!(rejected("session.not_connected").is_connection_lost());
    assert!(rejected("query.not_connected").is_connection_lost());
    assert!(!rejected("query.failed").is_connection_lost());
    assert!(!DriverQueryError::DriverNotLoaded.is_connection_lost());
}

#[gpui::test]
fn the_new_connection_picker_badges_downloadable_kinds_and_hides_driverless_ones(
    cx: &mut TestAppContext,
) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| this.open_connection_dialog(None, window, cx));
    });
    settle(rig.visual);
    // Relational (the default category) lists MySQL (in the catalog, not
    // installed: badged) and omits PostgreSQL and MariaDB (no driver at all).
    assert!(rig.rendered("connection-picker-mysql"), "a downloadable kind is listed");
    assert!(rig.rendered("connection-picker-download-mysql"), "with the download badge");
    assert!(!rig.rendered("connection-picker-postgres"), "a kind with no driver stays hidden");
    assert!(!rig.rendered("connection-picker-mariadb"), "a kind with no driver stays hidden");
}

// --- Tools: tablex-mcp -------------------------------------------------------------------------
//
// The same rig: a `file://` manifest (developer mode) now also lists the `tablex-mcp` tool, whose
// "build" is a plain byte buffer (never executed). Installs go through the real host installer into
// a temp tools directory; clicks are real.

/// A stand-in for the server binary: big enough for several progress reports.
fn tool_bytes(seed: u8) -> Vec<u8> {
    (0..300_000_u32).map(|n| (n as u8).wrapping_add(seed)).collect()
}

fn tool_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

impl DmRig<'_> {
    /// Lists `tablex-mcp` `version` in the manifest (next to whatever drivers it has), built from
    /// `bytes`, downloadable at `url` for this platform.
    fn publish_tool_at(&self, version: &str, url: &str, bytes: &[u8], sha256: &str) {
        let path = self.root.join("manifest.json");
        let mut manifest: serde_json::Value = std::fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_else(|| {
                serde_json::json!({"schemaVersion": 2, "generatedAt": "2026-10-08T00:00:00Z", "drivers": []})
            });
        manifest["tools"] = serde_json::json!([{
            "id": "tablex-mcp", "kind": "mcpServer", "displayName": "TableX MCP server",
            "version": version, "description": "Lets MCP clients use your saved connections.",
            "artifacts": [{
                "target": crate::drivers::current_target().id(),
                "url": url, "sha256": sha256, "sizeBytes": bytes.len(),
            }],
        }]);
        std::fs::write(path, manifest.to_string()).unwrap();
    }

    /// Publishes a tool build as a file next to the manifest.
    fn publish_tool(&self, version: &str, bytes: &[u8]) {
        let artifact = self.root.join(format!("tablex-mcp-{version}.bin"));
        std::fs::write(&artifact, bytes).unwrap();
        self.publish_tool_at(version, &file_url(&artifact), bytes, &tool_sha256(bytes));
    }

    fn tool_item<R>(&self, f: impl FnOnce(&engine_driver_host::ToolCatalogItem) -> R) -> R {
        self.read(|this| f(this.driver_ui.tools.item().expect("the tool row")))
    }

    fn tool_state(&self) -> engine_driver_host::ToolCatalogState {
        self.tool_item(|item| item.state.clone())
    }

    fn tool_file(&self) -> std::path::PathBuf {
        self.tools_dir
            .join("tablex-mcp")
            .join(if cfg!(windows) { "tablex-mcp.exe" } else { "tablex-mcp" })
    }

    fn tool_dir_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.tools_dir.join("tablex-mcp"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Opens the manager on its Tools tab (the tools are not listed under the drivers).
    fn open_manager_with_tool(&mut self) {
        self.open_manager();
        self.wait("the tool row", |this| this.driver_ui.tools.item().is_some());
        self.click(DriverFilter::Tools.selector());
    }

    fn install_tool(&mut self) {
        self.click("tool-action-install:tablex-mcp");
        self.wait("the tool install", |this| {
            this.driver_ui.tools.job.is_none()
                && this.driver_ui.tools.item().is_some_and(|item| item.installed.is_some())
        });
    }

    /// Re-reads the manifest (the "Check for updates" button).
    fn check_for_updates(&mut self) {
        self.click("driver-check-updates");
        self.wait("the catalog", |this| this.driver_ui.catalog_task.is_none());
    }

    fn clipboard(&mut self) -> String {
        self.visual.cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default()
    }

    /// The row's text on screen is what the model says (no clipping off the window).
    fn assert_on_screen(&mut self, selector: &'static str) {
        let bounds = self.visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
        let window = self.visual.update(|window, _| window.viewport_size());
        assert!(
            bounds.left() >= gpui::px(0.0) && bounds.right() <= window.width
                && bounds.top() >= gpui::px(0.0) && bounds.bottom() <= window.height,
            "{selector} {bounds:?} is inside the window {window:?}"
        );
    }
}

#[gpui::test]
fn the_tools_row_installs_with_progress_then_shows_the_path_and_the_copy_buttons(
    cx: &mut TestAppContext,
) {
    use engine_driver_host::ToolCatalogState;
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let bytes = tool_bytes(1);
    let mut slow = SlowArtifact::new(&rig.root, bytes.clone());
    rig.publish("1.0.0");
    rig.publish_tool_at("0.1.0", &file_url(&slow.path), &bytes, &tool_sha256(&bytes));
    rig.open_manager_with_tool();

    // Not installed: name, what it is, the versions, Install; no path, no copy buttons.
    assert_eq!(rig.tool_state(), ToolCatalogState::Available);
    for selector in ["driver-tools", "tool-row:tablex-mcp", "tool-description:tablex-mcp", "tool-versions:tablex-mcp", "tool-action-install:tablex-mcp", "tool-note:tablex-mcp"] {
        rig.assert_on_screen(selector);
    }
    let (installed, available) = rig.read(|this| driver_manager::tool_version_lines(this.driver_ui.tools.item().unwrap()));
    assert_eq!((installed.as_str(), available.as_deref()), ("Not installed", Some("Available 0.1.0")));
    for selector in ["tool-path:tablex-mcp", "tool-copy-config:tablex-mcp", "tool-copy-command:tablex-mcp", "tool-action-uninstall:tablex-mcp", "tool-progress:tablex-mcp", "tool-error:tablex-mcp"] {
        assert!(!rig.rendered(selector), "{selector} must not show before the install");
    }
    let note = rig.read(|_| driver_manager::MCP_CAPABILITY_NOTE);
    assert!(note.contains("assistant access") && note.contains("\"Let MCP clients run changes\"") && note.contains("Settings \u{2192} AI"), "{note}");
    assert!(
        note.contains("names of your connections are listed") && note.contains("even where access is off")
            && note.contains("\"Read and write\"") && note.contains("production"),
        "{note}"
    );

    // Installing: the same progress and cancel presentation as a driver row.
    rig.click("tool-action-install:tablex-mcp");
    rig.wait("download progress", |this| {
        matches!(
            this.driver_ui.tools.job.as_ref().map(|job| &job.state),
            Some(InstallState::Downloading { bytes, .. }) if *bytes > 0
        )
    });
    assert!(rig.rendered("tool-progress:tablex-mcp") && rig.rendered("tool-action-cancel:tablex-mcp"));
    assert!(!rig.rendered("tool-action-install:tablex-mcp") && !rig.rendered("tool-versions:tablex-mcp"));
    let fraction = rig.read(|this| this.driver_ui.tools.job.as_ref().unwrap().state.fraction());
    assert!(fraction > 0.0 && fraction < 1.0, "mid-download: {fraction}");
    assert!(!rig.tool_file().exists(), "nothing at the stable path yet");

    slow.release();
    rig.wait("the install", |this| {
        this.driver_ui.tools.job.is_none() && this.driver_ui.tools.item().is_some_and(|item| item.installed.is_some())
    });
    assert!(!rig.rendered("tool-progress:tablex-mcp"));
    assert_eq!(std::fs::read(rig.tool_file()).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(rig.tool_file()).unwrap().permissions().mode() & 0o777, 0o755);
    }
    assert_eq!(rig.tool_dir_files().len(), 2, "the file and its record: {:?}", rig.tool_dir_files());
    assert_eq!(rig.tool_state(), ToolCatalogState::Installed { version: "0.1.0".into() });
    for selector in ["tool-path:tablex-mcp", "tool-copy-config:tablex-mcp", "tool-copy-command:tablex-mcp", "tool-action-uninstall:tablex-mcp", "tool-badge:tablex-mcp"] {
        rig.assert_on_screen(selector);
    }
    assert!(!rig.rendered("tool-action-install:tablex-mcp"), "an installed tool is not offered again");
    assert_eq!(rig.tool_item(|item| item.path.clone()), rig.tool_file());
    assert_eq!(rig.read(|this| this.error_log.log.len()), 0);
}

#[gpui::test]
fn cancelling_the_tool_download_leaves_nothing_and_the_row_returns_to_install(
    cx: &mut TestAppContext,
) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let bytes = tool_bytes(2);
    let mut slow = SlowArtifact::new(&rig.root, bytes.clone());
    rig.publish("1.0.0");
    rig.publish_tool_at("0.1.0", &file_url(&slow.path), &bytes, &tool_sha256(&bytes));
    rig.open_manager_with_tool();
    rig.click("tool-action-install:tablex-mcp");
    rig.wait("download progress", |this| {
        matches!(
            this.driver_ui.tools.job.as_ref().map(|job| &job.state),
            Some(InstallState::Downloading { bytes, .. }) if *bytes > 0
        )
    });
    assert!(!rig.tool_dir_files().is_empty(), "a partial file exists while downloading");

    rig.click("tool-action-cancel:tablex-mcp");
    assert!(rig.read(|this| this.driver_ui.tools.job.as_ref().unwrap().cancelling()));
    slow.release();
    rig.wait("the cancel", |this| this.driver_ui.tools.job.is_none());

    assert!(rig.tool_dir_files().is_empty(), "no partial file: {:?}", rig.tool_dir_files());
    assert!(!rig.tool_file().exists());
    assert!(rig.rendered("tool-action-install:tablex-mcp") && !rig.rendered("tool-progress:tablex-mcp") && !rig.rendered("tool-error:tablex-mcp"));
    assert_eq!(rig.read(|this| this.error_log.log.len()), 0, "a cancel is not an error");
}

#[gpui::test]
fn an_update_keeps_the_stable_path_and_uninstall_removes_exactly_the_installed_files(
    cx: &mut TestAppContext,
) {
    use engine_driver_host::ToolCatalogState;
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let (v1, v2) = (tool_bytes(3), tool_bytes(4));
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &v1);
    rig.open_manager_with_tool();
    rig.install_tool();
    let path = rig.tool_item(|item| item.path.clone());
    assert!(!rig.rendered("tool-action-update:tablex-mcp"));

    rig.publish_tool("0.2.0", &v2);
    rig.check_for_updates();
    assert_eq!(
        rig.tool_state(),
        ToolCatalogState::UpdateAvailable { installed: "0.1.0".into(), latest: "0.2.0".into() }
    );
    let (installed, available) = rig.read(|this| driver_manager::tool_version_lines(this.driver_ui.tools.item().unwrap()));
    assert_eq!((installed.as_str(), available.as_deref()), ("Installed 0.1.0", Some("Available 0.2.0")));
    assert!(rig.rendered("tool-action-update:tablex-mcp") && rig.rendered("tool-action-uninstall:tablex-mcp") && rig.rendered("tool-path:tablex-mcp"));
    assert!(!rig.rendered("tool-action-install:tablex-mcp"));
    // The user's own file in the folder is not ours to delete.
    std::fs::write(rig.tools_dir.join("tablex-mcp").join("notes.txt"), "mine").unwrap();

    rig.click("tool-action-update:tablex-mcp");
    rig.wait("the update", |this| {
        this.driver_ui.tools.job.is_none()
            && this.driver_ui.tools.item().is_some_and(|item| matches!(&item.state, ToolCatalogState::Installed { version } if version == "0.2.0"))
    });
    assert_eq!(rig.tool_item(|item| item.path.clone()), path, "the path survives the update");
    assert_eq!(std::fs::read(rig.tool_file()).unwrap(), v2);
    assert_eq!(rig.tool_item(|item| item.installed.clone().unwrap().version), "0.2.0");
    assert!(!rig.rendered("tool-action-update:tablex-mcp"));

    rig.click("tool-action-uninstall:tablex-mcp");
    rig.wait("the uninstall", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.tools.item().is_some_and(|item| item.installed.is_none())
    });
    assert_eq!(rig.tool_dir_files(), ["notes.txt"], "only the installed files went");
    assert!(!rig.rendered("tool-path:tablex-mcp") && !rig.rendered("tool-copy-config:tablex-mcp") && !rig.rendered("tool-action-uninstall:tablex-mcp"));
    assert!(rig.rendered("tool-action-install:tablex-mcp"), "available again");
    assert_eq!(rig.tool_state(), ToolCatalogState::Available);
}

impl DmRig<'_> {
    /// Lists SQL Formatter (sqruff) `version` in the manifest, after whatever tools it has, built
    /// from `bytes` (never executed here).
    fn publish_formatter(&self, version: &str, bytes: &[u8]) {
        let artifact = self.root.join(format!("tablex-sqruff-{version}.bin"));
        std::fs::write(&artifact, bytes).unwrap();
        let path = self.root.join("manifest.json");
        let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entry = serde_json::json!({
            "id": "tablex-sqruff", "kind": "sqlFormatter", "displayName": "SQL Formatter (sqruff)",
            "version": version, "description": "Formats SQL with sqruff, in a separate process.",
            "license": "Apache-2.0", "sourceRepo": "https://github.com/quarylabs/sqruff",
            "artifacts": [{
                "target": crate::drivers::current_target().id(),
                "url": file_url(&artifact), "sha256": tool_sha256(bytes), "sizeBytes": bytes.len(),
            }],
        });
        match manifest["tools"].as_array_mut() {
            Some(tools) => {
                tools.retain(|tool| tool["id"] != "tablex-sqruff");
                tools.push(entry);
            }
            None => manifest["tools"] = serde_json::json!([entry]),
        }
        std::fs::write(path, manifest.to_string()).unwrap();
    }

    fn formatter_file(&self) -> std::path::PathBuf {
        self.tools_dir
            .join("tablex-sqruff")
            .join(if cfg!(windows) { "tablex-sqruff.exe" } else { "tablex-sqruff" })
    }

    fn formatter_installed(&self) -> bool {
        self.read(|this| this.driver_ui.tools.item_of("tablex-sqruff").is_some_and(|item| item.installed.is_some()))
    }
}

#[gpui::test]
fn the_formatter_has_its_own_row_beside_the_mcp_server_and_installs_updates_and_uninstalls(
    cx: &mut TestAppContext,
) {
    use engine_driver_host::ToolCatalogState;
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &tool_bytes(1));
    let first = tool_bytes(7);
    rig.publish_formatter("0.1.0", &first);
    rig.open_manager();
    rig.wait("both tool rows", |this| this.driver_ui.tools.item().is_some() && this.driver_ui.tools.item_of("tablex-sqruff").is_some());
    rig.click(DriverFilter::Tools.selector());

    // One row per catalog tool, each with its own controls; the add-on shell is not in this list.
    for selector in ["tool-row:tablex-mcp", "tool-row:tablex-sqruff", "tool-action-install:tablex-mcp", "tool-action-install:tablex-sqruff", "tool-note:tablex-sqruff"] {
        rig.assert_on_screen(selector);
    }
    let names = rig.read(|this| this.driver_ui.tools.standalone().map(|item| item.display_name.clone()).collect::<Vec<_>>());
    assert_eq!(names, ["TableX MCP server", "SQL Formatter (sqruff)"]);
    // The licence and the source are shown for the tool that names them (and only for it).
    rig.assert_on_screen("tool-origin:tablex-sqruff");
    assert!(!rig.rendered("tool-origin:tablex-mcp"));
    let entry = rig.read(|this| this.driver_ui.tools.item_of("tablex-sqruff").unwrap().entry.clone().unwrap());
    assert_eq!((entry.license.as_deref(), entry.source_repo.as_deref()), (Some("Apache-2.0"), Some("https://github.com/quarylabs/sqruff")));

    // Install: the formatter only; the other row is untouched. No MCP copy buttons for it.
    rig.click("tool-action-install:tablex-sqruff");
    rig.wait("the formatter install", |this| {
        this.driver_ui.tools.job.is_none() && this.driver_ui.tools.item_of("tablex-sqruff").is_some_and(|item| item.installed.is_some())
    });
    assert_eq!(std::fs::read(rig.formatter_file()).unwrap(), first);
    assert!(rig.formatter_installed());
    assert!(rig.read(|this| this.driver_ui.tools.item().is_some_and(|item| item.installed.is_none())), "the MCP server is not installed");
    assert!(rig.rendered("tool-action-uninstall:tablex-sqruff") && rig.rendered("tool-badge:tablex-sqruff"));
    assert!(rig.rendered("tool-action-install:tablex-mcp") && !rig.rendered("tool-action-uninstall:tablex-mcp"));
    for selector in ["tool-path:tablex-sqruff", "tool-copy-config:tablex-sqruff", "tool-copy-command:tablex-sqruff"] {
        assert!(!rig.rendered(selector), "{selector} belongs to the MCP server");
    }
    // The settings page finds it installed.
    let found = rig.read(|this| super::settings_format::scan_formatters(this.driver_ui.tools.store.as_deref()).tool);
    assert!(matches!(found, super::settings_format::FormatterToolState::Installed { version } if version == "0.1.0"));

    // A newer build is an update at the same path.
    let second = tool_bytes(9);
    rig.publish_formatter("0.2.0", &second);
    rig.check_for_updates();
    rig.wait("the update to be offered", |this| {
        this.driver_ui.tools.item_of("tablex-sqruff").is_some_and(|item| matches!(item.state, ToolCatalogState::UpdateAvailable { .. }))
    });
    assert!(rig.rendered("tool-action-update:tablex-sqruff"));
    rig.click("tool-action-update:tablex-sqruff");
    rig.wait("the update", |this| {
        this.driver_ui.tools.job.is_none()
            && this.driver_ui.tools.item_of("tablex-sqruff").is_some_and(|item| matches!(&item.state, ToolCatalogState::Installed { version } if version == "0.2.0"))
    });
    assert_eq!(std::fs::read(rig.formatter_file()).unwrap(), second);

    // Uninstall removes what the install wrote, and the row offers the install again.
    rig.click("tool-action-uninstall:tablex-sqruff");
    rig.wait("the uninstall", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.tools.item_of("tablex-sqruff").is_some_and(|item| item.installed.is_none())
    });
    assert!(!rig.formatter_file().exists() && !rig.tools_dir.join("tablex-sqruff").exists());
    assert!(rig.rendered("tool-action-install:tablex-sqruff") && !rig.rendered("tool-action-uninstall:tablex-sqruff"));
    assert!(!rig.formatter_installed());
}

#[gpui::test]
fn a_failed_tool_install_shows_the_error_keeps_nothing_and_retry_recovers(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let bytes = tool_bytes(5);
    rig.publish("1.0.0");
    let artifact = rig.root.join("tablex-mcp-bad.bin");
    std::fs::write(&artifact, &bytes).unwrap();
    // The signed manifest names other bytes than the download carries.
    rig.publish_tool_at("0.1.0", &file_url(&artifact), &bytes, &tool_sha256(b"something else"));
    rig.open_manager_with_tool();
    rig.click("tool-action-install:tablex-mcp");
    rig.wait("the failure", |this| {
        matches!(this.driver_ui.tools.job.as_ref().map(|job| &job.state), Some(InstallState::Failed(_)))
    });
    assert!(rig.rendered("tool-error:tablex-mcp") && rig.rendered("tool-action-retry:tablex-mcp"));
    assert!(!rig.rendered("tool-action-install:tablex-mcp") && !rig.rendered("tool-progress:tablex-mcp"));
    let failure = rig.read(|this| match &this.driver_ui.tools.job.as_ref().unwrap().state {
        InstallState::Failed(failure) => failure.clone(),
        other => panic!("{other:?}"),
    });
    assert_eq!(failure.code, "tool.checksum_mismatch");
    assert!(failure.message.contains("SHA-256"), "{}", failure.message);
    assert!(!failure.retryable);
    assert!(rig.tool_dir_files().is_empty(), "nothing left: {:?}", rig.tool_dir_files());
    assert!(!rig.tool_file().exists());
    assert_eq!(rig.read(|this| this.error_log.log.len()), 1, "the Error Log has it");

    // The maintainer fixes the manifest; Retry installs.
    rig.publish_tool("0.1.0", &bytes);
    rig.check_for_updates();
    rig.click("tool-action-retry:tablex-mcp");
    rig.wait("the retry", |this| {
        this.driver_ui.tools.job.is_none() && this.driver_ui.tools.item().is_some_and(|item| item.installed.is_some())
    });
    assert!(!rig.rendered("tool-error:tablex-mcp") && rig.rendered("tool-path:tablex-mcp"));
    assert_eq!(std::fs::read(rig.tool_file()).unwrap(), bytes);
}

#[gpui::test]
fn a_tool_without_a_build_for_this_platform_is_unavailable_and_a_bad_entry_spares_the_drivers(
    cx: &mut TestAppContext,
) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    let bytes = tool_bytes(6);
    rig.publish_tool("0.1.0", &bytes);
    // Other platform's build only.
    let path = rig.root.join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["tools"][0]["artifacts"][0]["target"] = "plan9-mips".into();
    manifest["tools"][0]["artifacts"][0]["arch"] = "plan9-mips".into();
    std::fs::write(&path, manifest.to_string()).unwrap();
    rig.open_manager_with_tool();
    assert!(rig.rendered("tool-badge:tablex-mcp") && !rig.rendered("tool-action-install:tablex-mcp"), "nothing to install");
    rig.click(DriverFilter::All.selector());
    assert!(rig.rendered("driver-row-sqlite"), "the drivers are listed");
    rig.click(DriverFilter::Tools.selector());
    assert!(matches!(rig.tool_state(), engine_driver_host::ToolCatalogState::Unavailable { reason } if reason.starts_with("No build for")));

    // A malformed entry fails that tool only.
    manifest["tools"][0]["artifacts"][0]["sha256"] = "not-a-hash".into();
    std::fs::write(&path, manifest.to_string()).unwrap();
    rig.check_for_updates();
    assert!(rig.rendered("tool-badge:tablex-mcp"));
    rig.click(DriverFilter::All.selector());
    assert!(rig.rendered("driver-row-sqlite"));
    assert!(matches!(rig.tool_state(), engine_driver_host::ToolCatalogState::Unavailable { reason } if reason.contains("SHA-256")));
    assert!(rig.read(|this| this.driver_ui.catalog_error.is_none()), "no catalog error for a bad tool");
}

#[gpui::test]
fn the_copy_buttons_put_valid_text_with_the_absolute_path_on_the_clipboard(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    // A folder name with spaces, an apostrophe and double quotes (legal on Unix and macOS).
    let folder = if cfg!(windows) { "My Tools" } else { "My \"Tools\" it's" };
    let tools_dir = rig.root.join(folder).join("tools");
    rig.tools_dir = tools_dir.clone();
    rig.update(|this, _| {
        this.driver_ui.tools.store = Some(std::sync::Arc::new(engine_driver_host::ToolStore::new(&tools_dir)));
    });
    let bytes = tool_bytes(7);
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &bytes);
    rig.open_manager_with_tool();
    rig.install_tool();
    let path = rig.tool_file();
    assert!(path.is_absolute());
    for selector in ["tool-copy-config:tablex-mcp", "tool-copy-command:tablex-mcp"] {
        rig.assert_on_screen(selector);
    }

    rig.click("tool-copy-config:tablex-mcp");
    let config: serde_json::Value = serde_json::from_str(&rig.clipboard()).expect("the copied block is valid JSON");
    assert_eq!(config["mcpServers"]["tablex"]["command"], path.to_str().unwrap());
    assert_eq!(config["mcpServers"]["tablex"]["args"], serde_json::json!([]));
    assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);

    rig.click("tool-copy-command:tablex-mcp");
    let command = rig.clipboard();
    let expected = driver_manager::ShellQuoting::native().command(&path).unwrap();
    assert_eq!(command, expected);
    assert!(command.starts_with("claude mcp add tablex -- "), "{command}");
    #[cfg(unix)]
    {
        // Run by a real shell, the quoted path comes back as ONE argument equal to the path.
        let quoted = command.strip_prefix("claude mcp add tablex -- ").unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(format!("printf %s {quoted}")).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), path.to_str().unwrap());
    }
    assert_eq!(rig.read(|this| this.driver_ui.catalog_error.clone()), None);
}

#[gpui::test]
fn the_installed_path_is_selectable_text(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let bytes = tool_bytes(8);
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &bytes);
    rig.open_manager_with_tool();
    rig.install_tool();
    let path = rig.tool_file().display().to_string();
    let bounds = rig.visual.debug_bounds("tool-path:tablex-mcp").expect("the path is rendered");
    assert!(bounds.size.width > gpui::px(40.0) && bounds.size.height > gpui::px(8.0), "{bounds:?}");

    // Drag across the first line of the text, then the platform Copy shortcut.
    let y = bounds.top() + gpui::px(9.0);
    let (from, to) = (gpui::point(bounds.left() + gpui::px(2.0), y), gpui::point(bounds.left() + gpui::px(300.0), y));
    rig.visual.simulate_mouse_move(from, None, Default::default());
    settle(rig.visual);
    rig.visual.simulate_mouse_down(from, gpui::MouseButton::Left, Default::default());
    for step in 1..=10 {
        let x = from.x + (to.x - from.x) * (step as f32 / 10.0);
        rig.visual.simulate_mouse_move(gpui::point(x, y), gpui::MouseButton::Left, Default::default());
    }
    rig.visual.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
    settle(rig.visual);
    rig.visual.simulate_keystrokes(if cfg!(target_os = "macos") { "secondary-c" } else { "ctrl-c" });
    settle(rig.visual);
    let copied = rig.clipboard();
    assert!(
        copied.len() > 20 && path.contains(copied.as_str()),
        "a drag over the path then Copy puts that part of the path on the clipboard, got {copied:?} for {path:?}"
    );
}

#[gpui::test]
fn the_tool_installs_over_loopback_http_with_the_size_and_hash_checked(cx: &mut TestAppContext) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bytes = tool_bytes(9);
    let body = bytes.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 2 {
            line.clear();
        }
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
    });
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.publish_tool_at("0.1.0", &format!("http://127.0.0.1:{port}/tablex-mcp"), &bytes, &tool_sha256(&bytes));
    rig.open_manager_with_tool();
    rig.install_tool();
    server.join().unwrap();
    assert_eq!(std::fs::read(rig.tool_file()).unwrap(), bytes);
    assert_eq!(rig.tool_dir_files().len(), 2);
}

#[gpui::test]
fn the_driver_manager_tools_scenario_shows_the_installed_tool(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1500.0), gpui::px(850.0)));
    app.update(visual, |this, cx| {
        this.seed_driver_manager_scenario(false);
        this.seed_driver_tools_fixture(true);
        this.driver_ui.selected = None;
        this.driver_ui.model.set_filter(DriverFilter::Tools);
        cx.notify();
    });
    settle(visual);
    for selector in ["driver-tools", "tool-row:tablex-mcp", "tool-path:tablex-mcp", "tool-copy-config:tablex-mcp", "tool-copy-command:tablex-mcp", "tool-note:tablex-mcp", "tool-action-uninstall:tablex-mcp"] {
        let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
        assert!(bounds.bottom() <= gpui::px(850.0) && bounds.right() <= gpui::px(1500.0), "{selector} {bounds:?}");
    }
    assert!(visual.debug_bounds("driver-footer").is_some(), "the footer is still shown under the section");
}

#[gpui::test]
fn the_tools_chip_swaps_the_driver_list_for_the_tool_cards_search_filters_them_and_install_works(
    cx: &mut TestAppContext,
) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let bytes = tool_bytes(3);
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &bytes);
    rig.publish_formatter("0.1.0", &tool_bytes(4));
    rig.open_manager();
    rig.wait("both tools", |this| this.driver_ui.tools.item().is_some() && this.driver_ui.tools.item_of("tablex-sqruff").is_some());

    // The drivers' tab: the list only, the tools are not under it.
    assert!(rig.rendered("driver-list") && rig.rendered("driver-row-sqlite") && rig.rendered("driver-footer"));
    assert!(!rig.rendered("driver-tools") && !rig.rendered("tool-row:tablex-mcp"));
    assert_eq!(DriverFilter::CHIPS.last(), Some(&DriverFilter::Tools));
    assert!(rig.rendered(DriverFilter::Tools.selector()), "the chip is the last of the row");
    let all_before = rig.read(|this| this.driver_ui.model.count(DriverFilter::All));

    // A real click on the chip: the cards, not the drivers or their detail pane; the footer stays.
    rig.update(|this, cx| {
        this.driver_ui.selected = Some("sqlite".into());
        cx.notify();
    });
    settle(rig.visual);
    assert!(rig.rendered("driver-detail"), "a selected driver has its detail pane on the drivers' tabs");
    rig.click(DriverFilter::Tools.selector());
    assert_eq!(rig.read(|this| this.driver_ui.model.filter()), DriverFilter::Tools);
    for selector in ["driver-tools", "tool-row:tablex-mcp", "tool-row:tablex-sqruff", "driver-footer"] {
        rig.assert_on_screen(selector);
    }
    assert!(!rig.rendered("driver-list") && !rig.rendered("driver-row-sqlite") && !rig.rendered("driver-detail"));
    assert!(!rig.rendered("driver-empty"), "the tools tab is not an empty driver list");

    // The search box filters the tools by name and by description.
    let search = |rig: &mut DmRig, text: &str| {
        let text = text.to_string();
        rig.update(|this, cx| {
            this.driver_ui.model.set_query(&text);
            cx.notify();
        });
        settle(rig.visual);
    };
    search(&mut rig, "SQRUFF");
    assert!(rig.rendered("tool-row:tablex-sqruff") && !rig.rendered("tool-row:tablex-mcp"));
    search(&mut rig, "mcp clients");
    assert!(rig.rendered("tool-row:tablex-mcp") && !rig.rendered("tool-row:tablex-sqruff"), "the description counts");
    search(&mut rig, "nothing-matches");
    assert!(rig.rendered("driver-tools-empty") && !rig.rendered("tool-row:tablex-mcp") && !rig.rendered("tool-row:tablex-sqruff"));
    search(&mut rig, "");

    // Install from the Tools tab.
    rig.install_tool();
    assert!(rig.rendered("tool-action-uninstall:tablex-mcp") && rig.rendered("tool-path:tablex-mcp"));
    assert_eq!(std::fs::read(rig.tool_file()).unwrap(), bytes);

    // Back to All: the drivers again, no tools, and the drivers' count did not move.
    rig.click(DriverFilter::All.selector());
    assert!(rig.rendered("driver-row-sqlite") && rig.rendered("driver-footer"));
    assert!(!rig.rendered("driver-tools") && !rig.rendered("tool-row:tablex-mcp") && !rig.rendered("tool-row:tablex-sqruff"));
    assert_eq!(rig.read(|this| this.driver_ui.model.count(DriverFilter::All)), all_before);
}

#[gpui::test]
fn the_tools_tab_hides_the_driver_update_banner_and_the_drivers_tabs_keep_it(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.open_manager();
    rig.update(|this, cx| {
        this.driver_ui.updates_available = 1;
        cx.notify();
    });
    settle(rig.visual);
    assert!(rig.rendered("driver-banner"));
    rig.click(DriverFilter::Tools.selector());
    assert!(!rig.rendered("driver-banner"), "Update all is about drivers");
    rig.click(DriverFilter::Installed.selector());
    assert!(rig.rendered("driver-banner"));
}

// Pure texts of the two copy buttons.

#[test]
fn the_client_configuration_is_valid_json_for_any_path() {
    for path in [
        "/Users/you/Library/Application Support/tablex/tools/tablex-mcp/tablex-mcp",
        "/tmp/with \"quotes\" and 'apostrophes'/tablex-mcp",
        r"C:\Users\A B\AppData\Roaming\tablex\tools\tablex-mcp\tablex-mcp.exe",
        "C:\\Users\\say \"hi\"\\x.exe",
        "/tmp/ünïcode/日本語/tablex-mcp",
        "/tmp/new\nline\ttab/tablex-mcp",
    ] {
        let text = driver_manager::client_config_json(std::path::Path::new(path)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}\n{text}"));
        assert_eq!(parsed, serde_json::json!({"mcpServers": {"tablex": {"command": path, "args": []}}}), "{path}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let not_utf8 = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff/tablex-mcp"));
        assert_eq!(driver_manager::client_config_json(not_utf8), Err(driver_manager::ClientTextError::NotUtf8));
        assert_eq!(
            driver_manager::ShellQuoting::Posix.command(not_utf8),
            Err(driver_manager::ClientTextError::NotUtf8)
        );
    }
}

#[test]
fn the_client_configuration_text_is_exactly_this() {
    let mac = driver_manager::client_config_json(std::path::Path::new(
        "/Users/me/Library/Application Support/tablex/tools/tablex-mcp/tablex-mcp",
    ))
    .unwrap();
    assert_eq!(
        mac,
        "{\n  \"mcpServers\": {\n    \"tablex\": {\n      \"command\": \"/Users/me/Library/Application Support/tablex/tools/tablex-mcp/tablex-mcp\",\n      \"args\": []\n    }\n  }\n}"
    );
    let windows = driver_manager::client_config_json(std::path::Path::new(
        r"C:\Users\me\AppData\Roaming\tablex\tools\tablex-mcp\tablex-mcp.exe",
    ))
    .unwrap();
    assert!(
        windows.contains(r#""command": "C:\\Users\\me\\AppData\\Roaming\\tablex\\tools\\tablex-mcp\\tablex-mcp.exe""#),
        "{windows}"
    );
}

#[test]
fn the_claude_code_command_quotes_for_the_shell_family_of_the_os() {
    use driver_manager::ShellQuoting;
    let command = |path: &str, quoting: ShellQuoting| quoting.command(std::path::Path::new(path));
    assert_eq!(
        command("/Users/me/Library/Application Support/tablex/tools/tablex-mcp/tablex-mcp", ShellQuoting::Posix).unwrap(),
        "claude mcp add tablex -- '/Users/me/Library/Application Support/tablex/tools/tablex-mcp/tablex-mcp'"
    );
    assert_eq!(command("/a b/it's", ShellQuoting::Posix).unwrap(), r"claude mcp add tablex -- '/a b/it'\''s'");
    assert_eq!(command(r"/a\b", ShellQuoting::Posix).unwrap(), r"claude mcp add tablex -- '/a'\\'b'");
    assert_eq!(
        command(r"C:\Users\A B\AppData\Roaming\tablex\tools\tablex-mcp\tablex-mcp.exe", ShellQuoting::Windows).unwrap(),
        r#"claude mcp add tablex -- "C:\Users\A B\AppData\Roaming\tablex\tools\tablex-mcp\tablex-mcp.exe""#
    );
    // What cmd / PowerShell would still interpret inside double quotes is refused, not guessed at.
    for (path, c) in [
        (r"C:\100%\x.exe", '%'),
        (r"C:\a$b\x.exe", '$'),
        ("C:\\a`b\\x.exe", '`'),
        (r"C:\hi!\x.exe", '!'),
        ("C:\\\u{201C}q\u{201D}\\x.exe", '\u{201C}'),
        ("C:\\q\u{201D}\\x.exe", '\u{201D}'),
        ("C:\\q\u{201E}\\x.exe", '\u{201E}'),
        ("C:\\a\u{1b}[31m\\x.exe", '\u{1b}'),
        ("C:\\a\u{85}b\\x.exe", '\u{85}'),
    ] {
        assert_eq!(command(path, ShellQuoting::Windows), Err(driver_manager::ClientTextError::UnsafeForShell(c)));
    }
    assert_eq!(command(r"\\server\share\x.exe", ShellQuoting::Windows), Err(driver_manager::ClientTextError::UncPath));
    // A control character (ESC, newline, DEL, a C1 code) never goes on a command line, on any OS.
    for c in ['\u{1b}', '\n', '\r', '\t', '\u{7f}', '\u{85}', '\u{9b}'] {
        let path = format!("/a{c}b/tablex-mcp");
        assert_eq!(command(&path, ShellQuoting::Posix), Err(driver_manager::ClientTextError::UnsafeForShell(c)), "{c:?}");
    }
    assert!(driver_manager::ClientTextError::UnsafeForShell('%').to_string().contains("Copy client configuration"));
    assert!(driver_manager::ClientTextError::UncPath.to_string().contains("Copy client configuration"));
}

/// The shells the quoting claims to be right for. A developer machine that normally has them all
/// must not pass with fewer: a missing one fails the test. Where a shell is really absent (a
/// minimal CI image), say so with `TABLEX_ALLOW_MISSING_SHELLS=1`; the list that ran is printed.
#[cfg(unix)]
#[test]
fn the_posix_quoting_survives_every_installed_shell() {
    use driver_manager::ShellQuoting;
    const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "fish", "dash"];
    let paths = [
        "/plain/path/tablex-mcp",
        "/with space/and  two/tablex-mcp",
        "/it's/a \"quote\"/tablex-mcp",
        "/dollar $HOME and `backtick` and $(cmd)/tablex-mcp",
        r"/back\slash\\double\/tablex-mcp",
        r"/ends\",
        "/glob * ? [a-z] {a,b} ~/tablex-mcp",
        "/semi; amp& pipe| bang! hash#/tablex-mcp",
        "/'",
        "/ünïcode/日本語",
    ];
    let (mut ran, mut missing) = (Vec::new(), Vec::new());
    for shell in SHELLS {
        if std::process::Command::new(shell).arg("-c").arg("true").output().is_err() {
            missing.push(shell);
            continue;
        }
        ran.push(shell);
        for path in paths {
            let command = ShellQuoting::Posix.command(std::path::Path::new(path)).unwrap();
            let quoted = command.strip_prefix("claude mcp add tablex -- ").unwrap();
            // `printf %s <arg>` prints its single argument; two arguments would print both.
            let out = std::process::Command::new(shell).arg("-c").arg(format!("printf %s {quoted}")).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), path, "{shell}: {command}");
        }
    }
    eprintln!("shells exercised: {ran:?}; missing: {missing:?}");
    assert!(
        missing.is_empty() || std::env::var_os("TABLEX_ALLOW_MISSING_SHELLS").is_some(),
        "shells not installed: {missing:?} (ran {ran:?}); set TABLEX_ALLOW_MISSING_SHELLS=1 where that is expected"
    );
    assert!(ran.contains(&"sh"));
}

#[test]
fn a_code_span_renders_exactly_the_text_whatever_it_holds() {
    for (text, expected) in [
        ("/a/b", "` /a/b `"),
        ("/a_b*c\\d", "` /a_b*c\\d `"),
        ("/a`b", "`` /a`b ``"),
        ("`x`", "`` `x` ``"),
        ("/a``b`c", "``` /a``b`c ```"),
    ] {
        assert_eq!(driver_manager::code_span(text), expected);
    }
}

#[test]
fn the_tool_buttons_follow_the_state() {
    use driver_manager::{tool_actions, ToolAction};
    use engine_driver_host::{ToolCatalogState as State, ToolManifestEntry};
    let entry: ToolManifestEntry = match serde_json::from_value::<engine_driver_host::ManifestTool>(serde_json::json!({
        "id": "tablex-mcp", "kind": "mcpServer", "displayName": "T", "version": "1.0.0",
        "artifacts": [{"target": "macos-aarch64", "url": "https://x/y", "sha256": "0".repeat(64), "sizeBytes": 1}],
    }))
    .unwrap()
    {
        engine_driver_host::ManifestTool::Known(entry) => entry,
        other => panic!("{other:?}"),
    };
    let record = engine_driver_host::InstalledToolRecord {
        id: "tablex-mcp".into(),
        version: "0.9.0".into(),
        sha256: "0".repeat(64),
        size_bytes: 1,
        installed_at: String::new(),
        requires: Vec::new(),
    };
    let item = |state: State, installed: bool, listed: bool| engine_driver_host::ToolCatalogItem {
        id: "tablex-mcp".into(),
        kind: engine_driver_host::ToolKind::McpServer,
        display_name: "T".into(),
        version: "1.0.0".into(),
        description: None,
        state,
        entry: listed.then(|| entry.clone()),
        installed: installed.then(|| record.clone()),
        path: "/x/tablex-mcp".into(),
        problem: None,
    };
    assert_eq!(tool_actions(&item(State::Available, false, true), None), [ToolAction::Install]);
    assert_eq!(
        tool_actions(&item(State::UpdateAvailable { installed: "0.9.0".into(), latest: "1.0.0".into() }, true, true), None),
        [ToolAction::Update, ToolAction::Uninstall]
    );
    assert_eq!(tool_actions(&item(State::Installed { version: "0.9.0".into() }, true, true), None), [ToolAction::Uninstall]);
    // Installed, but the manifest no longer lists it: only Uninstall.
    assert_eq!(tool_actions(&item(State::Installed { version: "0.9.0".into() }, true, false), None), [ToolAction::Uninstall]);
    assert!(tool_actions(&item(State::Unavailable { reason: "No build".into() }, false, true), None).is_empty());
}

/// The child half of [`the_tools_directory_follows_tablex_data_dir`]: a no-op unless the parent
/// set `TOOLS_ROOT_EXPECTED` (a process-wide environment change must not leak into other tests).
#[test]
fn tools_directory_child() {
    let Ok(expected) = std::env::var("TOOLS_ROOT_EXPECTED") else {
        return;
    };
    let store = driver_manager::DriverManagerUi::new(std::sync::Arc::new(
        engine_driver_host::InstalledDriverStore::new(std::env::temp_dir()),
    ))
    .tools
    .store
    .expect("a data directory exists");
    assert_eq!(store.root(), std::path::Path::new(&expected));
    assert_eq!(
        store.executable_path("tablex-mcp").unwrap(),
        std::path::Path::new(&expected)
            .join("tablex-mcp")
            .join(if cfg!(windows) { "tablex-mcp.exe" } else { "tablex-mcp" })
    );
}

#[test]
fn the_tools_directory_follows_tablex_data_dir() {
    let data = std::env::temp_dir().join(format!("tablex-data-{}", uuid::Uuid::new_v4()));
    let expected = data.join("tablex").join("tools");
    let run = |data_dir: &std::ffi::OsStr, expected: &std::path::Path| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "app::tests::tools_directory_child", "--nocapture"])
            .env("TABLEX_DATA_DIR", data_dir)
            .env("TOOLS_ROOT_EXPECTED", expected)
            .output()
            .unwrap()
    };
    let output = run(data.as_os_str(), &expected);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "the child test ran");
    // A relative value is ignored, like everywhere else: the platform data dir is used.
    let output = run(std::ffi::OsStr::new("relative/dir"), &expected);
    assert!(!output.status.success(), "a relative TABLEX_DATA_DIR must not move the tools");
}

// --- Tools: polish round ---------------------------------------------------------------------

/// A loopback HTTP server that answers its first request with `bytes`; returns the port.
fn serve_bytes_once(bytes: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 2 {
            line.clear();
        }
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
        let _ = stream.write_all(&bytes);
    });
    (port, server)
}

#[gpui::test]
fn retry_installs_only_the_catalogs_current_installable_entry(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let bytes = tool_bytes(11);
    rig.publish("1.0.0");
    let artifact = rig.root.join("tablex-mcp-bad.bin");
    std::fs::write(&artifact, &bytes).unwrap();
    rig.publish_tool_at("0.1.0", &file_url(&artifact), &bytes, &tool_sha256(b"other bytes"));
    rig.open_manager_with_tool();
    rig.click("tool-action-install:tablex-mcp");
    rig.wait("the failure", |this| {
        matches!(this.driver_ui.tools.job.as_ref().map(|job| &job.state), Some(InstallState::Failed(_)))
    });
    assert!(rig.rendered("tool-action-retry:tablex-mcp"));

    // The maintainer withdraws the tool (the manifest lists no build for this platform): the old
    // entry in the failed job must not be installable any more.
    let path = rig.root.join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["tools"][0]["artifacts"][0]["target"] = "plan9-mips".into();
    manifest["tools"][0]["artifacts"][0]["arch"] = "plan9-mips".into();
    std::fs::write(&path, manifest.to_string()).unwrap();
    // Retry on the stale row (before the catalog is refreshed) installs what the catalog has now.
    rig.check_for_updates();
    assert!(rig.read(|this| this.driver_ui.tools.job.is_none()), "a failure of a build that is gone is dropped");
    assert!(!rig.rendered("tool-action-retry:tablex-mcp") && !rig.rendered("tool-action-install:tablex-mcp"));
    // Even a Retry that reaches the handler installs nothing.
    rig.update(|this, cx| this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Retry, cx));
    rig.update(|this, cx| this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Install, cx));
    settle(rig.visual);
    assert!(rig.read(|this| this.driver_ui.tools.job.is_none()));
    assert!(!rig.tool_file().exists() && rig.tool_dir_files().is_empty());
}

#[gpui::test]
fn the_tool_takes_one_operation_at_a_time(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let (v1, v2) = (tool_bytes(12), tool_bytes(13));
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &v1);
    rig.open_manager_with_tool();
    rig.install_tool();
    rig.publish_tool("0.2.0", &v2);
    rig.check_for_updates();
    // Uninstall, then an Update in the same breath: the update is refused while the uninstall's
    // operation holds the slot (its sweep could otherwise delete the update's temp file).
    rig.update(|this, cx| {
        this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Uninstall, cx);
        assert!(this.driver_ui.op_task.is_some());
        this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Update, cx);
        assert!(this.driver_ui.tools.job.is_none(), "no install while an operation runs");
    });
    rig.wait("the uninstall", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.tools.item().is_some_and(|item| item.installed.is_none())
    });
    assert!(rig.tool_dir_files().is_empty());
    // And not the other way round either.
    rig.install_tool();
    rig.update(|this, cx| {
        this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Update, cx);
        this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Uninstall, cx);
        assert!(this.driver_ui.op_task.is_none(), "no uninstall while the install runs");
    });
    rig.wait("the update", |this| this.driver_ui.tools.job.is_none());
}

#[cfg(unix)]
#[gpui::test]
fn a_failed_uninstall_keeps_its_typed_error_on_the_row(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &tool_bytes(14));
    rig.open_manager_with_tool();
    rig.install_tool();
    // Someone replaced the executable with a link: uninstall refuses and says why, by code.
    let victim = rig.root.join("victim");
    std::fs::write(&victim, "keep").unwrap();
    std::fs::remove_file(rig.tool_file()).unwrap();
    std::os::unix::fs::symlink(&victim, rig.tool_file()).unwrap();
    rig.update(|this, cx| this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Uninstall, cx));
    rig.wait("the failed uninstall", |this| this.driver_ui.op_task.is_none() && this.driver_ui.tools.failure.is_some());
    settle(rig.visual);
    let failure = rig.read(|this| this.driver_ui.tools.failure.clone().unwrap().1);
    assert_eq!(failure.code, "tool.link_refused");
    assert!(!failure.retryable);
    assert!(rig.rendered("tool-error:tablex-mcp"), "the row shows code and message");
    assert!(rig.read(|this| this.driver_ui.catalog_error.is_none()), "not folded into a string");
    assert_eq!(rig.read(|this| this.error_log.log.len()), 1);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep", "the link target is untouched");
}

#[test]
fn tool_failures_keep_their_code_and_whether_to_try_again() {
    use engine_driver_host::ToolError;
    let path = std::path::PathBuf::from("/x/tablex-mcp");
    let code = |error: ToolError| {
        let failure = driver_manager::InstallFailure::from(&error);
        (failure.code, failure.retryable)
    };
    assert_eq!(code(ToolError::InUse { path: path.clone() }), ("tool.in_use", true));
    assert_eq!(code(ToolError::AccessDenied { path: path.clone() }), ("tool.access_denied", true));
    assert_eq!(code(ToolError::SharedWritable { path: path.clone() }), ("tool.folder_writable", false));
    assert_eq!(code(ToolError::LinkRefused { path }), ("tool.link_refused", false));
}

/// A manifest signed by a test key the app is told to trust (the keys compiled into the build have
/// private halves outside the repository), under the STRICT policy: nothing here is developer-only.
fn strict_rig(cx: &mut TestAppContext) -> (DmRig<'_>, ed25519_dalek::SigningKey) {
    use ed25519_dalek::SigningKey;
    let rig = DmRig::new(cx, TrustPolicy::Strict);
    let key = SigningKey::from_bytes(&[5; 32]);
    let trust = engine_driver_host::ManifestTrust::new(&[key.verifying_key().to_bytes()], TrustPolicy::Strict).unwrap();
    let mut rig = rig;
    rig.update(|this, _| this.driver_ui.trust_override = Some(trust));
    (rig, key)
}

fn publish_signed(rig: &DmRig<'_>, key: &ed25519_dalek::SigningKey, generated: &str, url: &str, bytes: &[u8]) {
    use base64::Engine;
    use ed25519_dalek::Signer;
    let manifest = serde_json::json!({
        "schemaVersion": 2, "generatedAt": generated, "drivers": [],
        "tools": [{
            "id": "tablex-mcp", "kind": "mcpServer", "displayName": "TableX MCP server", "version": "0.1.0",
            "artifacts": [{
                "target": crate::drivers::current_target().id(),
                "url": url, "sha256": tool_sha256(bytes), "sizeBytes": bytes.len(),
            }],
        }],
    })
    .to_string();
    std::fs::write(rig.root.join("manifest.json"), &manifest).unwrap();
    let signature = base64::engine::general_purpose::STANDARD.encode(key.sign(manifest.as_bytes()).to_bytes());
    std::fs::write(rig.root.join("manifest.json.sig"), format!("{signature}\n")).unwrap();
}

fn open_strict_manager(rig: &mut DmRig<'_>) {
    rig.update(|this, cx| this.open_driver_manager(cx));
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.tools.item().is_some());
    rig.click(DriverFilter::Tools.selector());
}

#[gpui::test]
fn under_the_strict_policy_a_signed_manifest_installs_over_loopback_and_a_tampered_one_is_refused(
    cx: &mut TestAppContext,
) {
    let (mut rig, key) = strict_rig(cx);
    let bytes = tool_bytes(15);
    let (port, server) = serve_bytes_once(bytes.clone());
    publish_signed(&rig, &key, "2026-10-14T00:00:01Z", &format!("http://127.0.0.1:{port}/tablex-mcp"), &bytes);
    open_strict_manager(&mut rig);
    assert_eq!(rig.read(|this| this.driver_ui.policy), TrustPolicy::Strict);
    assert!(rig.read(|this| this.driver_ui.catalog_error.is_none()), "{:?}", rig.read(|this| this.driver_ui.catalog_error.clone()));
    rig.install_tool();
    server.join().unwrap();
    assert_eq!(std::fs::read(rig.tool_file()).unwrap(), bytes);

    // The manifest is changed after signing (a different hash): refused as a whole, the row stays.
    let manifest = std::fs::read_to_string(rig.root.join("manifest.json")).unwrap();
    std::fs::write(rig.root.join("manifest.json"), manifest.replace("0.1.0", "6.6.6")).unwrap();
    rig.click("driver-check-updates");
    rig.wait("the refusal", |this| this.driver_ui.catalog_task.is_none());
    let error = rig.read(|this| this.driver_ui.catalog_error.clone()).expect("the tampered manifest is reported");
    assert!(error.contains("signature"), "{error}");
    assert_eq!(rig.tool_item(|item| item.installed.clone().unwrap().version), "0.1.0");
    // What is installed stays listed (path, copy buttons, uninstall); nothing can be installed or
    // updated from the refused manifest.
    assert!(rig.rendered("tool-path:tablex-mcp") && rig.rendered("tool-action-uninstall:tablex-mcp"));
    assert!(!rig.rendered("tool-action-update:tablex-mcp") && !rig.rendered("tool-action-install:tablex-mcp"));
    assert!(rig.tool_item(|item| item.entry.is_none()));
}

#[gpui::test]
fn turning_developer_mode_off_withdraws_what_an_unsigned_manifest_offered(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.publish_tool("0.1.0", &tool_bytes(17));
    rig.update(|this, _| this.driver_ui.developer_mode = true);
    rig.open_manager_with_tool();
    assert!(rig.rendered("tool-action-install:tablex-mcp"));
    rig.click(DriverFilter::All.selector());
    assert!(rig.rendered("driver-action-sqlite"));
    rig.click(DriverFilter::Tools.selector());
    let sqlite = rig.read(|this| this.driver_ui.model.row("sqlite").and_then(|row| row.entry.clone())).expect("sqlite is offered");
    assert!(rig.read(|this| this.driver_ui.catalog_is_current()));

    // Developer mode off (the toolbar's menu): the refresh is Strict and the unsigned local
    // manifest fails verification.
    rig.pick_toolbar_menu(1);
    rig.wait("the strict refresh", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.catalog_error.is_some());
    assert_eq!(rig.read(|this| this.driver_ui.policy), TrustPolicy::Strict);
    assert!(!rig.rendered("tool-action-install:tablex-mcp"), "nothing is offered");
    rig.click(DriverFilter::All.selector());
    assert!(!rig.rendered("driver-action-sqlite"), "nothing is offered");
    assert!(rig.read(|this| this.driver_ui.tools.item().is_none() && this.driver_ui.model.rows().is_empty()));
    assert!(!rig.read(|this| this.driver_ui.catalog_is_current()));
    // And a direct call is refused too, with the entries a stale row could still hold.
    rig.update(|this, cx| {
        this.run_tool_action_of(engine_driver_host::MCP_TOOL_ID, driver_manager::ToolAction::Install, cx);
        assert!(this.driver_ui.tools.job.is_none());
        assert_eq!(this.enqueue_driver_install(sqlite.clone(), driver_manager::JobKind::Install, cx), driver_manager::Enqueued::Stale);
        assert!(this.driver_ui.queue.job("sqlite").is_none());
        this.update_all_drivers(cx);
    });
    settle(rig.visual);
    assert!(rig.tool_dir_files().is_empty() && rig.version_dirs().is_empty());
}

#[gpui::test]
fn under_the_strict_policy_a_file_url_in_a_signed_manifest_is_refused_at_install(cx: &mut TestAppContext) {
    let (mut rig, key) = strict_rig(cx);
    let bytes = tool_bytes(16);
    let artifact = rig.root.join("tablex-mcp.bin");
    std::fs::write(&artifact, &bytes).unwrap();
    publish_signed(&rig, &key, "2026-10-14T00:00:02Z", &file_url(&artifact), &bytes);
    open_strict_manager(&mut rig);
    rig.click("tool-action-install:tablex-mcp");
    rig.wait("the refusal", |this| {
        matches!(this.driver_ui.tools.job.as_ref().map(|job| &job.state), Some(InstallState::Failed(_)))
    });
    let failure = rig.read(|this| match &this.driver_ui.tools.job.as_ref().unwrap().state {
        InstallState::Failed(failure) => failure.clone(),
        other => panic!("{other:?}"),
    });
    assert_eq!(failure.code, "tool.local_url_not_allowed");
    assert!(!rig.tool_file().exists() && rig.tool_dir_files().is_empty());
}

// --- Tools / drivers: correction 2 (jobs that outlive a policy change) -------------------------------

/// Developer mode off, through the toolbar's menu (the rig has set the flag, so it applies at once).
fn turn_developer_mode_off(rig: &mut DmRig<'_>) {
    rig.pick_toolbar_menu(1);
    rig.wait("the strict refresh", |this| {
        this.driver_ui.catalog_task.is_none() && this.driver_ui.catalog_error.is_some()
    });
    assert_eq!(rig.read(|this| this.driver_ui.policy), TrustPolicy::Strict);
}

#[gpui::test]
fn a_policy_change_drops_queued_installs_and_cancels_running_ones_and_adopts_nothing(
    cx: &mut TestAppContext,
) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    // A running driver install (A), one queued behind it (B = mysql) and a running tool install.
    let mut slow_driver = SlowArtifact::new(&rig.root, std::fs::read(&rig.dylib).unwrap());
    rig.publish_at("1.0.0", &file_url(&slow_driver.path));
    let tool_dir = rig.root.join("tool-source");
    std::fs::create_dir_all(&tool_dir).unwrap();
    let bytes = tool_bytes(21);
    let mut slow_tool = SlowArtifact::new(&tool_dir, bytes.clone());
    rig.publish_tool_at("0.1.0", &file_url(&slow_tool.path), &bytes, &tool_sha256(&bytes));
    rig.update(|this, _| this.driver_ui.developer_mode = true);
    rig.open_manager();
    rig.wait("the tool row", |this| this.driver_ui.tools.item().is_some());
    rig.click("driver-action-sqlite");
    rig.click(DriverFilter::Tools.selector());
    rig.click("tool-action-install:tablex-mcp");
    rig.wait("both downloads under way", |this| {
        let running = |state: Option<&InstallState>| matches!(state, Some(InstallState::Downloading { bytes, .. }) if *bytes > 0);
        running(this.driver_ui.queue.job("sqlite").map(|job| &job.state)) && running(this.driver_ui.tools.job.as_ref().map(|job| &job.state))
    });
    let mysql = rig.read(|this| this.driver_ui.model.row("mysql").and_then(|row| row.entry.clone())).unwrap();
    rig.update(|this, cx| {
        assert_eq!(this.enqueue_driver_install(mysql, driver_manager::JobKind::Install, cx), driver_manager::Enqueued::Queued);
    });
    assert_eq!(rig.read(|this| this.driver_ui.queue.job("mysql").map(|job| job.state.clone())), Some(InstallState::Queued));

    turn_developer_mode_off(&mut rig);
    // (a) the queued one is gone, (b) the running ones were told to stop.
    assert!(rig.read(|this| this.driver_ui.queue.job("mysql").is_none()), "queued at the flip: dropped");
    assert!(rig.read(|this| this.driver_ui.queue.job("sqlite").unwrap().cancelling()), "running driver job: cancelled");
    assert!(rig.read(|this| this.driver_ui.tools.job.as_ref().unwrap().cancelling()), "running tool job: cancelled");

    slow_driver.release();
    slow_tool.release();
    rig.wait("both jobs end", |this| {
        this.driver_ui.queue.job("sqlite").is_none() && this.driver_ui.tools.job.is_none() && this.driver_ui.install_task.is_none()
    });
    settle(rig.visual);
    // Nothing was adopted or loaded, nothing was started in their place.
    assert!(rig.read(|this| !this.driver_ui.handles.contains_key("sqlite") && !this.driver_ui.installed.contains("sqlite")));
    assert_eq!(rig.stored_version(), None);
    assert!(rig.version_dirs().is_empty() && rig.downloads_empty());
    assert!(rig.read(|this| this.driver_ui.queue.job("mysql").is_none() && this.driver_ui.queue.running().is_none()));
    assert!(!rig.tool_file().exists() && rig.tool_dir_files().is_empty());
    assert!(rig.read(|this| this.driver_ui.tools.item().is_none()));
}

#[gpui::test]
fn retry_of_a_driver_installs_the_catalogs_current_entry_not_the_failed_ones(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    // The first manifest points at a file that does not exist: the install fails.
    rig.publish_at("1.0.0", "file:///nonexistent-sqlite-artifact.dylib");
    rig.open_manager();
    rig.click("driver-action-sqlite");
    rig.wait("the failure", |this| {
        matches!(this.driver_ui.queue.job("sqlite").map(|job| &job.state), Some(InstallState::Failed(_)))
    });
    assert_eq!(
        rig.read(|this| driver_manager::primary_action(this.driver_ui.model.row("sqlite").unwrap(), this.driver_ui.queue.job("sqlite"))),
        Some(PrimaryAction::Retry)
    );
    // The maintainer fixes the manifest. Retry must use the new entry (the old one fails again).
    rig.publish("1.0.0");
    rig.click("driver-check-updates");
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none());
    rig.click("driver-action-sqlite");
    rig.wait("the retry", |this| {
        this.driver_ui.queue.job("sqlite").is_none() && this.driver_ui.handles.contains_key("sqlite")
    });
    assert_eq!(rig.stored_version().as_deref(), Some("1.0.0"));

}

#[gpui::test]
fn a_failed_driver_install_is_dismissed_when_the_policy_changes(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish_at("1.0.0", "file:///nonexistent-sqlite-artifact.dylib");
    rig.update(|this, _| this.driver_ui.developer_mode = true);
    rig.open_manager();
    rig.click("driver-action-sqlite");
    rig.wait("the failure", |this| {
        matches!(this.driver_ui.queue.job("sqlite").map(|job| &job.state), Some(InstallState::Failed(_)))
    });
    turn_developer_mode_off(&mut rig);
    assert!(rig.read(|this| this.driver_ui.queue.failed_ids().is_empty()), "no Retry survives the flip");
}

#[test]
fn distrusting_the_catalog_keeps_every_installed_driver_even_an_unavailable_one() {
    use engine_driver_host::{DriverCatalogItem, DriverCatalogState, DriverManifestEntry};
    let dir = std::env::temp_dir().join(format!("tablex-distrust-{}", uuid::Uuid::new_v4()));
    let mut ui = driver_manager::DriverManagerUi::new(std::sync::Arc::new(engine_driver_host::InstalledDriverStore::new(&dir)));
    let entry = |id: &str| DriverManifestEntry { id: id.into(), display_name: id.into(), version: "2.0.0".into(), ..Default::default() };
    let item = |id: &str, state| DriverCatalogItem {
        id: id.into(), display_name: id.into(), version: "2.0.0".into(), state, manifest: Some(entry(id)), disabled: false,
    };
    ui.catalog = vec![
        // Installed, but its entry is unavailable here (needs a newer app).
        item("legacy", DriverCatalogState::Unavailable { reason: "Needs TableX \u{2265} 9".into() }),
        item("sqlite", DriverCatalogState::UpdateAvailable { installed: "1.0.0".into(), latest: "2.0.0".into() }),
        item("redis", DriverCatalogState::Available),
    ];
    for (id, version) in [("legacy", "0.9.0"), ("sqlite", "1.0.0")] {
        ui.records.insert(id.into(), engine_driver_host::InstalledDriverRecord { id: id.into(), version: version.into(), ..Default::default() });
    }
    ui.catalog_policy = Some(TrustPolicy::Developer);
    let epoch = ui.trust_epoch;
    ui.distrust_catalog();
    let ids: Vec<&str> = ui.catalog.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, ["legacy", "sqlite"], "an installed driver stays whatever its entry said; the offer goes");
    assert!(ui.catalog.iter().all(|item| item.manifest.is_none() && matches!(item.state, DriverCatalogState::Installed { .. })));
    assert_eq!(ui.catalog[0].version, "0.9.0");
    assert!(ui.model.row("legacy").is_some_and(|row| row.is_installed() && row.entry.is_none()));
    assert!(ui.model.row("redis").is_none());
    assert!(ui.trust_epoch > epoch && !ui.catalog_is_current());
    let _ = std::fs::remove_dir_all(dir);
}

#[gpui::test]
fn the_missing_driver_dialog_does_not_claim_to_install_from_a_catalog_that_is_not_current(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    rig.publish("1.0.0");
    rig.update(|this, cx| this.refresh_driver_catalog(cx));
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none() && this.driver_ui.is_downloadable("sqlite"));
    let (_, tab) = rig.save_sqlite(&db, "needs driver");
    rig.update(|this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT 1".into();
        this.run_active_tab_query(cx);
    });
    rig.wait("the dialog", |this| this.driver_ui.prompt.is_some());
    settle(rig.visual);
    // The catalog stops being the one read under the current policy while the dialog is open.
    rig.update(|this, _| this.driver_ui.catalog_policy = None);
    rig.click("driver-prompt-install");
    let phase = rig.read(|this| this.driver_ui.prompt.as_ref().map(|prompt| prompt.phase.clone()));
    assert!(format!("{phase:?}").starts_with("Some(Failed("), "{phase:?}");
    assert!(rig.read(|this| this.driver_ui.queue.job("sqlite").is_none()), "nothing was queued");
}

#[gpui::test]
fn a_result_that_arrives_after_a_policy_change_is_not_adopted(cx: &mut TestAppContext) {
    let mut rig = DmRig::new(cx, TrustPolicy::Developer);
    let mut slow = SlowArtifact::new(&rig.root, std::fs::read(&rig.dylib).unwrap());
    rig.publish_at("1.0.0", &file_url(&slow.path));
    rig.update(|this, _| this.driver_ui.developer_mode = true);
    rig.open_manager();
    rig.click("driver-action-sqlite");
    rig.wait("download progress", |this| {
        matches!(this.driver_ui.queue.job("sqlite").map(|job| &job.state), Some(InstallState::Downloading { bytes, .. }) if *bytes > 0)
    });
    turn_developer_mode_off(&mut rig);
    assert!(rig.read(|this| this.driver_ui.queue.job("sqlite").unwrap().cancelling()));

    // The install finished all the same (past the last cancel check): the result is not adopted.
    let handle = engine_driver_host::DriverLoader.load_verified(&rig.dylib, &rig.sha).expect("the real driver loads");
    let record = engine_driver_host::InstalledDriverRecord { id: "sqlite".into(), version: "1.0.0".into(), ..Default::default() };
    rig.update(|this, cx| this.finish_driver_install_for_test("sqlite", record, handle, cx));
    assert!(rig.read(|this| {
        !this.driver_ui.handles.contains_key("sqlite")
            && !this.driver_ui.installed.contains("sqlite")
            && !this.driver_ui.records.contains_key("sqlite")
            && this.driver_ui.queue.job("sqlite").is_none()
    }));
    assert!(rig.read(|this| this.error_log.log.len()) >= 2, "the refused catalog and the warning about the late result are logged");
    slow.release();
    rig.wait("the worker ends", |this| this.driver_ui.install_task.is_none());
}
