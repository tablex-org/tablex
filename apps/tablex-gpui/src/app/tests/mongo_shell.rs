// MongoDB shell scripts in a script tab, driven through the real UI: Run, the install prompt, the
// Driver Manager job, the result grid, the Output pane, Stop. The plugin is a fake engine
// (`FakeShell`) that records requests and answers with scripted replies, behind the same seam the
// app uses for the real host engine; the plugin FILE is installed through the real tool store (a
// stand-in executable), so the verified-path rule is exercised too. Nothing here touches the
// user's driver data directory.

use crate::app::mongo_shell::ShellEngine;
use engine_driver_api::mongo_shell::{
    CursorRef, FailureKind, ShellFailure, ShellReply, ShellRequest, ShellResult,
};
use engine_driver_host::{ShellHostError, MONGO_SHELL_TOOL_ID};

/// What the fake engine does with one run.
enum Scripted {
    Reply(Box<ShellReply>),
    /// Never finishes by itself; ends as cancelled when it is stopped.
    Hang,
    Fail(String),
    /// Ends with exactly this host error.
    FailHost(ShellHostError),
}

#[derive(Default)]
struct FakeShell {
    scripted: std::sync::Mutex<std::collections::VecDeque<Scripted>>,
    requests: std::sync::Mutex<Vec<ShellRequest>>,
    stops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The plugin paths the app asked to run.
    executables: std::sync::Mutex<Vec<std::path::PathBuf>>,
}

impl FakeShell {
    fn push(&self, scripted: Scripted) {
        self.scripted.lock().unwrap().push_back(scripted);
    }

    fn requests(&self) -> Vec<ShellRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn stops(&self) -> usize {
        self.stops.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ShellEngine for FakeShell {
    fn needs_session(&self) -> bool {
        false
    }

    fn start(
        &self,
        launch: crate::app::mongo_shell::ShellLaunch,
    ) -> Result<crate::app::mongo_shell::RunningShell, ShellHostError> {
        self.requests.lock().unwrap().push(launch.request);
        self.executables.lock().unwrap().push(launch.exe);
        let next = self.scripted.lock().unwrap().pop_front();
        let counter = std::sync::Arc::clone(&self.stops);
        match next.unwrap_or(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "app")))) {
            Scripted::Reply(reply) => Ok(crate::app::mongo_shell::RunningShell {
                stop: Box::new(move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }),
                wait: Box::new(move || Ok(*reply)),
            }),
            Scripted::Hang => {
                let (release, released) = std::sync::mpsc::channel::<()>();
                Ok(crate::app::mongo_shell::RunningShell {
                    stop: Box::new(move || {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = release.send(());
                    }),
                    wait: Box::new(move || {
                        let _ = released.recv();
                        Err(ShellHostError::Cancelled)
                    }),
                })
            }
            Scripted::FailHost(error) => Ok(crate::app::mongo_shell::RunningShell {
                stop: Box::new(|| {}),
                wait: Box::new(move || Err(error)),
            }),
            Scripted::Fail(message) => Ok(crate::app::mongo_shell::RunningShell {
                stop: Box::new(|| {}),
                wait: Box::new(move || Err(ShellHostError::Crashed(format!(": {message}")))),
            }),
        }
    }
}

fn reply(output: &[&str], result: ShellResult, database_after: &str) -> ShellReply {
    ShellReply {
        output: output.iter().map(|line| line.to_string()).collect(),
        output_truncated: false,
        result,
        database_after: database_after.into(),
        error: None,
    }
}

fn raw(text: &str) -> engine_driver_api::mongo_shell::RawDocument {
    serde_json::value::RawValue::from_string(text.into()).unwrap()
}

fn documents(texts: &[&str]) -> ShellResult {
    ShellResult::Documents { documents: texts.iter().map(|text| raw(text)).collect() }
}

fn failure(kind: FailureKind, message: &str, place: Option<(u32, u32)>) -> ShellFailure {
    ShellFailure {
        kind,
        message: message.into(),
        line: place.map(|(line, _)| line),
        column: place.map(|(_, column)| column),
        code: None,
        code_name: None,
    }
}

/// A script tab on the fixture MongoDB connection with a fake shell engine, a scratch driver and
/// tool store, and a local manifest that offers the plugin.
struct ShellRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    root: std::path::PathBuf,
    tab_id: u64,
    connection: String,
    engine: std::sync::Arc<FakeShell>,
    plugin: Vec<u8>,
    /// The MongoDB driver the manifest offers: `(url, sha256, size)`.
    driver_artifact: (String, String, u64),
}

const PLUGIN_MIN_DRIVER: &str = "0.2.0";

impl<'a> ShellRig<'a> {
    /// `driver`: the version of the MongoDB driver that is installed (none: not installed).
    fn new(cx: &'a mut TestAppContext, driver: Option<&str>) -> Self {
        let (app, visual) = script_app(cx);
        visual.simulate_resize(gpui::size(gpui::px(1500.0), gpui::px(850.0)));
        let root = std::env::temp_dir().join(format!("tablex-shell-{}", uuid::Uuid::new_v4()));
        let store_dir = root.join("store");
        std::fs::create_dir_all(&store_dir).unwrap();
        if let Some(version) = driver {
            engine_driver_host::InstalledDriverStore::new(&store_dir)
                .save(engine_driver_host::InstalledDriverRecord {
                    id: "mongodb".into(),
                    version: version.into(),
                    ..Default::default()
                })
                .unwrap();
        }
        let engine = std::sync::Arc::new(FakeShell::default());
        let (tools_dir, manifest) = (root.join("tools"), root.join("manifest.json"));
        let shared = engine.clone();
        let (tab_id, connection) = app.update(visual, |this, cx| {
            this.scenario_ui.service = None;
            this.driver_ui.tools.store = Some(std::sync::Arc::new(engine_driver_host::ToolStore::new(&tools_dir)));
            this.driver_ui.store = std::sync::Arc::new(engine_driver_host::InstalledDriverStore::new(&store_dir));
            this.driver_ui.policy = TrustPolicy::Developer;
            this.driver_ui.manifest_source = Some(file_url(&manifest));
            this.driver_ui.installed_loaded = true;
            this.mongo_shell.engine = shared;
            cx.notify();
            let tab = this.workspace.active_tab().unwrap();
            (tab.id, tab.connection_id.clone())
        });
        settle(visual);
        let mut rig = Self {
            app,
            visual,
            root,
            tab_id,
            connection,
            engine,
            driver_artifact: ("file:///nonexistent-mongodb.dylib".into(), "00".into(), 4096),
            plugin: {
                // A few MB of padding after a stand-in script: big enough that the download
                // reports progress before it ends.
                let mut bytes = b"#!/bin/sh\nexit 0\n".to_vec();
                bytes.resize(3 * 1024 * 1024, b'#');
                bytes
            },
        };
        rig.publish("0.1.0", PLUGIN_MIN_DRIVER);
        rig
    }

    /// Lists plugin `version` (needing the MongoDB driver >= `minimum`) in the local manifest.
    fn publish(&mut self, version: &str, minimum: &str) {
        let artifact = self.root.join(format!("plugin-{version}"));
        std::fs::write(&artifact, &self.plugin).unwrap();
        self.publish_with_url(version, minimum, &file_url(&artifact));
    }

    /// [`Self::publish`] with the plugin's artifact at `url`.
    fn publish_with_url(&mut self, version: &str, minimum: &str, url: &str) {
        let target = crate::drivers::current_target().id();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "generatedAt": "2026-10-16T00:00:00Z",
            "drivers": [{
                "id": "mongodb", "displayName": "MongoDB", "version": "0.3.0",
                "appAbiVersion": 2, "uriSchemes": ["mongodb"],
                "features": ["navigator", "query.mongo_script", "mongo.command"],
                "license": "MIT", "sourceRepo": "https://example.com/tablex", "teamId": "",
                "artifacts": [{"target": target, "url": self.driver_artifact.0, "sha256": self.driver_artifact.1, "sizeBytes": self.driver_artifact.2}],
                "category": "nosql", "description": "MongoDB test driver",
            }],
            "tools": [{
                "id": MONGO_SHELL_TOOL_ID, "kind": "mongoShell", "displayName": "MongoDB Shell",
                "version": version, "description": "mongosh-style scripts",
                "requires": [{"driver": "mongodb", "minVersion": minimum}],
                "artifacts": [{
                    "target": target, "url": url,
                    "sha256": sha256_hex(&self.plugin), "sizeBytes": self.plugin.len(),
                }],
            }],
        });
        std::fs::write(self.root.join("manifest.json"), manifest.to_string()).unwrap();
    }

    /// The manifest offers a MongoDB driver that really installs: the SQLite test dylib under the
    /// MongoDB id (the installer checks the file's hash and size, not what the driver speaks).
    fn offer_installable_driver(&mut self) {
        let dylib = built_driver("dbengine-sqlite", "");
        let artifact = self.root.join("mongodb-driver.dylib");
        std::fs::copy(&dylib, &artifact).unwrap();
        self.driver_artifact = (
            file_url(&artifact),
            driver_sha256(&artifact),
            std::fs::metadata(&artifact).unwrap().len(),
        );
        self.publish("0.1.0", PLUGIN_MIN_DRIVER);
    }

    fn read<R>(&self, f: impl FnOnce(&AppRoot) -> R) -> R {
        self.app.read_with(self.visual, |this, _| f(this))
    }

    fn update<R>(&mut self, f: impl FnOnce(&mut AppRoot, &mut Context<AppRoot>) -> R) -> R {
        self.app.update(self.visual, f)
    }

    fn wait(&mut self, label: &'static str, ready: impl Fn(&AppRoot) -> bool) {
        self.visual.dispatcher.allow_parking();
        let started = std::time::Instant::now();
        loop {
            self.visual.run_until_parked();
            if self.read(&ready) {
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(60), "timed out waiting for {label}");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        settle(self.visual);
    }

    /// Reads the local manifest into the Driver Manager's catalog.
    fn load_catalog(&mut self) {
        self.update(|this, cx| this.refresh_driver_catalog(cx));
        self.wait("the catalog", |this| {
            this.driver_ui.catalog_task.is_none() && this.driver_ui.tools.shell_item().is_some()
        });
    }

    /// Opens the Driver Manager tab with the catalog read.
    fn open_manager(&mut self) {
        self.update(|this, cx| this.open_driver_manager(cx));
        self.wait("the catalog", |this| {
            this.driver_ui.catalog_task.is_none() && this.driver_ui.tools.shell_item().is_some()
        });
    }

    fn click(&mut self, selector: &str) {
        click_debug_button(self.visual, leak(selector.to_string()));
        settle(self.visual);
    }

    fn rendered(&mut self, selector: &str) -> bool {
        self.visual.debug_bounds(leak(selector.to_string())).is_some()
    }

    /// Types `script` into the editor and presses Run.
    fn run_script(&mut self, script: &str) {
        script_type(self.visual, script);
        self.click("run-query");
    }

    /// [`Self::run_script`] on a tab that already ran something (the old text is replaced).
    fn run_script_replacing(&mut self, script: &str) {
        self.run_script(script);
    }

    fn status(&self) -> crate::query::QueryStatus {
        let tab = self.tab_id;
        self.read(|this| this.workspace.runtime(tab).unwrap().query.status().clone())
    }

    fn shell_view(&self) -> Option<crate::mongo_shell::ShellView> {
        let tab = self.tab_id;
        self.read(|this| this.shell_view(tab).cloned())
    }

    fn rows(&self) -> usize {
        let tab = self.tab_id;
        self.read(|this| this.grid(tab).row_count())
    }

    fn wait_ready(&mut self) {
        let tab = self.tab_id;
        self.wait("the shell run", move |this| {
            !matches!(this.workspace.runtime(tab).unwrap().query.status(), crate::query::QueryStatus::Running { .. })
        });
    }

    fn plugin_installed(&self) -> bool {
        self.read(|this| {
            this.driver_ui
                .tools
                .store
                .as_ref()
                .and_then(|store| store.installed(MONGO_SHELL_TOOL_ID).ok().flatten())
                .is_some()
        })
    }

    /// Installs the plugin through the real tool store, the way the Driver Manager does.
    fn install_plugin(&mut self) {
        self.load_catalog();
        let entry = self.read(|this| this.driver_ui.tools.shell_item().and_then(|item| item.entry.clone()).unwrap());
        let (tools, drivers) = self.read(|this| {
            (this.driver_ui.tools.store.clone().unwrap(), this.driver_ui.store.clone())
        });
        engine_driver_host::ToolInstaller::new(crate::drivers::current_target(), &tools)
            .with_trust_policy(TrustPolicy::Developer)
            .with_drivers(&drivers)
            .install(
                &entry,
                |artifact, transport| {
                    crate::drivers::open_artifact(
                        &artifact.url,
                        artifact.size_bytes,
                        transport,
                        TrustPolicy::Developer,
                        &std::sync::Arc::new(AtomicBool::new(false)),
                    )
                },
                &AtomicBool::new(false),
                &engine_driver_host::NoProgress,
            )
            .expect("install the stand-in plugin");
        self.load_catalog();
    }
}

impl Drop for ShellRig<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn plugin_size_label(bytes: usize) -> String {
    driver_manager::format_size(bytes as u64)
}

// ---- the install prompt -----------------------------------------------------------------------

#[gpui::test]
fn a_shell_script_without_the_plugin_asks_and_cancel_runs_nothing(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.load_catalog();
    assert!(!rig.plugin_installed());

    rig.run_script("print(1);\nprint(2)");
    assert!(rig.rendered("shell-prompt-body"), "the prompt opens");
    assert!(rig.rendered("shell-prompt-install") && rig.rendered("shell-prompt-cancel"));
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "nothing runs while it asks");
    let size = plugin_size_label(rig.plugin.len());
    assert!(!size.is_empty());

    rig.click("shell-prompt-cancel");
    assert!(!rig.rendered("shell-prompt-body"), "the dialog closed");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle);
    assert!(rig.engine.requests().is_empty(), "Cancel runs nothing");
    assert!(!rig.plugin_installed(), "and installs nothing");
    assert!(rig.read(|this| this.mongo_shell.waits_len() == 0));
}

#[gpui::test]
fn install_runs_the_script_by_itself_and_shows_grid_output_and_database(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.load_catalog();
    rig.engine.push(Scripted::Reply(Box::new(reply(
        &["hello", "world"],
        documents(&[r#"{"_id":{"$numberInt":"1"},"name":"a"}"#, r#"{"_id":{"$numberInt":"2"},"name":"b"}"#]),
        "app",
    ))));

    rig.run_script("print('hello');\nprint('world')");
    rig.click("shell-prompt-install");
    rig.wait("the plugin install and the script", |this| {
        this.driver_ui.tools.job.is_none()
            && !matches!(this.workspace.runtime(this.active_id()).unwrap().query.status(), crate::query::QueryStatus::Running { .. })
            && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.is_some())
    });
    assert!(rig.plugin_installed());
    // The stand-in is where the store puts it and is executable.
    let path = rig.read(|this| this.driver_ui.tools.shell_item().unwrap().path.clone());
    assert_eq!(rig.engine.executables.lock().unwrap().clone(), std::slice::from_ref(&path), "the verified path is what runs");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
    }
    let requests = rig.engine.requests();
    assert_eq!(requests.len(), 1, "the script ran once, by itself");
    assert_eq!(requests[0].script, "print('hello');\nprint('world')");
    assert!(requests[0].allow_writes && requests[0].cursor.is_none());
    assert_eq!(rig.rows(), 2, "the documents are in the grid");
    assert_eq!(rig.status(), crate::query::QueryStatus::Ready { rows: 2 });
    let view = rig.shell_view().expect("a shell view");
    assert_eq!(view.output, ["hello", "world"]);
    assert!(rig.rendered("shell-strip") && rig.rendered("shell-output"), "strip and Output pane render");
    assert!(!rig.rendered("shell-error") && !rig.rendered("shell-more"));
}

#[gpui::test]
fn cancel_forgets_the_waiting_script_and_a_changed_script_never_runs_after_the_install(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    let mut slow = SlowArtifact::new(&rig.root, rig.plugin.clone());
    let artifact = file_url(&slow.path);
    rig.publish_with_url("0.1.0", PLUGIN_MIN_DRIVER, &artifact);
    rig.load_catalog();

    // Attempt 1: Install, then Cancel while it downloads: the script is forgotten.
    rig.run_script("print(1)");
    rig.click("shell-prompt-install");
    rig.wait("the download to start", |this| {
        matches!(
            this.driver_ui.tools.job.as_ref().map(|job| &job.state),
            Some(InstallState::Downloading { bytes, .. }) if *bytes > 0
        )
    });
    assert!(rig.rendered("shell-waiting"), "the tab shows the install it waits for");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    rig.click("run-query"); // the Run button reads Cancel while it waits
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert!(rig.read(|this| this.mongo_shell.waits_len() == 0));
    assert!(!rig.rendered("shell-waiting"));

    // Attempt 2: Run again while the install is still going: the script follows it, but the
    // tab's text changes before it finishes, so it is not the script that asked.
    rig.run_script("print(2)");
    assert!(rig.read(|this| this.mongo_shell.waits_len() == 1), "no second prompt: the install is running");
    assert!(!rig.rendered("shell-prompt-body"));
    rig.update(|this, _| this.workspace.tab_mut(this.active_id()).unwrap().sql = "print(3)".into());
    slow.release();
    rig.wait("the install", |this| this.driver_ui.tools.job.is_none() && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.is_some()));
    settle(rig.visual);
    assert!(rig.engine.requests().is_empty(), "neither the cancelled nor the changed script ran");
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Running { .. }), "the waiting run ended");
    assert!(rig.read(|this| this.mongo_shell.waits_len() == 0));

    // The plugin is in now: the next Run just runs.
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "app"))));
    rig.click("run-query");
    rig.wait_ready();
    assert_eq!(rig.engine.requests().len(), 1);
    assert_eq!(rig.engine.requests()[0].script, "print(3)");
}

#[gpui::test]
fn a_driver_that_is_missing_or_too_old_is_fixed_first_not_hidden(cx: &mut TestAppContext) {
    for (driver, expect_update) in [(Some("0.1.0"), true), (None, false)] {
        let mut rig = ShellRig::new(cx, driver);
        rig.load_catalog();
        rig.run_script("print(1)");
        assert!(rig.rendered("shell-prompt-body"), "the prompt opens ({driver:?})");
        let state = rig.read(|this| this.shell_plugin_state());
        let crate::app::mongo_shell::PluginState::DriverFirst { need, bytes } = state else {
            panic!("expected the driver first for {driver:?}, got {state:?}")
        };
        assert_eq!(matches!(need.status, engine_driver_host::RequirementStatus::TooOld { .. }), expect_update);
        assert!(need.reason().contains("0.2.0"), "{}", need.reason());
        assert!(bytes > rig.plugin.len() as u64, "the driver's download counts too");
        assert_eq!(rig.status(), crate::query::QueryStatus::Idle);
        rig.click("shell-prompt-cancel");
        assert!(rig.engine.requests().is_empty());
        drop(rig);
    }
}

#[gpui::test]
fn an_installed_plugin_runs_at_once_with_the_connections_write_policy(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.update(|this, _| {
        let mut settings = (*this.settings).clone();
        settings.query.query_timeout_secs = 60;
        this.settings = std::sync::Arc::new(settings);
    });
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], documents(&[r#"{"a":1}"#]), "app"))));
    rig.run_script("db.c.deleteMany({})");
    rig.wait_ready();
    assert!(!rig.rendered("shell-prompt-body"), "no question: the plugin is there");
    assert!(rig.engine.requests()[0].allow_writes, "an untyped connection may write");
    assert_eq!(rig.engine.requests()[0].timeout_ms, Some(60_000), "the app's query timeout is the run's limit");
    // "No limit" (0) sends no limit.
    rig.update(|this, _| {
        let mut settings = (*this.settings).clone();
        settings.query.query_timeout_secs = 0;
        this.settings = std::sync::Arc::new(settings);
    });
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "app"))));
    rig.click("run-query");
    rig.wait_ready();
    assert_eq!(rig.engine.requests()[1].timeout_ms, None);
    rig.update(|this, _| {
        let mut settings = (*this.settings).clone();
        settings.query.query_timeout_secs = 60;
        this.settings = std::sync::Arc::new(settings);
    });

    // A read-only connection runs the script with writes off; the plugin's refusal is the message.
    publish_types(&rig.app, rig.visual, false);
    let connection = rig.connection.clone();
    give_profile(&rig.app, rig.visual, &connection, typed("replica"));
    let mut refused = reply(&[], ShellResult::None, "app");
    refused.error = Some(failure(
        FailureKind::ReadOnly,
        "deleteMany is refused: this connection is read-only",
        Some((1, 4)),
    ));
    rig.engine.push(Scripted::Reply(Box::new(refused)));
    rig.click("run-query");
    rig.wait_ready();
    assert!(!rig.engine.requests()[2].allow_writes, "read-only connection: no writes");
    let status = rig.status();
    assert!(
        matches!(&status, crate::query::QueryStatus::Failed(message) if message.contains("read-only") && message.contains("line 1, column 4")),
        "{status:?}"
    );
    assert!(rig.rendered("shell-error"), "the error shows inline");
    assert!(rig.read(|this| this.error_log.log.len()) >= 1, "and the Error Log has it");
}

#[gpui::test]
fn a_production_connection_shows_the_script_before_it_runs(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    publish_types(&rig.app, rig.visual, false);
    let connection = rig.connection.clone();
    give_profile(&rig.app, rig.visual, &connection, typed(PRODUCTION));
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "app"))));

    rig.run_script("db.c.drop()");
    assert!(rig.rendered("execute-confirm-statement") && rig.rendered("execute-confirm-run"), "the script is shown");
    assert!(rig.engine.requests().is_empty() && rig.status() == crate::query::QueryStatus::Idle);
    rig.click("execute-confirm-cancel");
    assert!(rig.engine.requests().is_empty(), "Cancel runs nothing");

    rig.click("run-query");
    rig.click("execute-confirm-run");
    rig.wait_ready();
    let requests = rig.engine.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].script, "db.c.drop()", "the confirmed text is what runs");
}

#[gpui::test]
fn use_moves_the_tabs_database_and_the_run_is_recorded_in_the_history(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.engine.push(Scripted::Reply(Box::new(reply(&["switched"], ShellResult::None, "other"))));
    rig.run_script("use other");
    rig.wait_ready();
    let tab = rig.tab_id;
    assert_eq!(rig.read(|this| this.workspace.tab(tab).unwrap().database.clone()), "other");
    assert!(!rig.engine.requests()[0].database.is_empty());
    assert_eq!(rig.status(), crate::query::QueryStatus::Ready { rows: 0 });
    assert_eq!(rig.read(|this| this.recorded_history.len()), 1);
    assert_eq!(rig.read(|this| this.recorded_history[0].sql.clone()), "use other");
    assert_eq!(rig.read(|this| this.recorded_history[0].database.clone()), "other");

    // The next script starts in the database `use` chose.
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "other"))));
    rig.click("run-query");
    rig.wait_ready();
    assert_eq!(rig.engine.requests()[1].database, "other");

    // A failed run is not history.
    let mut broken = reply(&[], ShellResult::None, "other");
    broken.error = Some(failure(FailureKind::Syntax, "unexpected token", Some((1, 9))));
    rig.engine.push(Scripted::Reply(Box::new(broken)));
    rig.click("run-query");
    rig.wait_ready();
    assert_eq!(rig.read(|this| this.recorded_history.len()), 2);
}

#[gpui::test]
fn a_scalar_or_write_result_is_shell_text_and_errors_show_line_and_column(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.engine.push(Scripted::Reply(Box::new(reply(
        &[],
        ShellResult::Value { text: "{\n  acknowledged: true,\n  insertedId: ObjectId('507f1f77bcf86cd799439011')\n}".into() },
        "app",
    ))));
    rig.run_script("db.c.insertOne({ a: 1 })");
    rig.wait_ready();
    assert!(rig.rendered("shell-text"), "the text replaces the grid");
    assert!(rig.rendered("shell-copy-text"));
    assert_eq!(rig.shell_view().unwrap().value_lines.len(), 4);
    assert_eq!(rig.rows(), 0);
    assert!(!rig.rendered("shell-error"));

    let mut failing = reply(&["before the error"], ShellResult::None, "app");
    failing.error = Some(ShellFailure {
        code: Some(11000),
        code_name: Some("DuplicateKey".into()),
        ..failure(FailureKind::Server, "E11000 duplicate key error", Some((2, 7)))
    });
    rig.engine.push(Scripted::Reply(Box::new(failing)));
    rig.click("run-query");
    rig.wait_ready();
    let status = rig.status();
    assert!(
        matches!(&status, crate::query::QueryStatus::Failed(message) if message.contains("line 2, column 7") && message.contains("DuplicateKey")),
        "{status:?}"
    );
    assert!(rig.rendered("shell-error"), "inline");
    assert!(!rig.rendered("shell-text"), "an error shows no stale text");
    assert_eq!(rig.shell_view().unwrap().output, ["before the error"], "what was printed is kept");
    assert!(rig.rendered("shell-output"), "and visible");
    // The Output pane closes and opens again.
    rig.click("shell-output-close");
    assert!(!rig.rendered("shell-output"));
    rig.click("shell-output-toggle");
    assert!(rig.rendered("shell-output"));
}

#[gpui::test]
fn more_continues_the_cursor_through_the_plugin(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    let cursor = CursorRef { id: "4242".into(), database: "app".into(), collection: "items".into(), batch_size: 50 };
    rig.engine.push(Scripted::Reply(Box::new(reply(
        &[],
        ShellResult::Cursor { documents: vec![raw(r#"{"n":1}"#), raw(r#"{"n":2}"#)], cursor: Some(cursor.clone()) },
        "app",
    ))));
    rig.run_script("db.items.aggregate([{ $sort: { n: 1 } }])");
    rig.wait_ready();
    assert!(rig.rendered("shell-more"), "the server has more");
    assert_eq!(rig.rows(), 2);

    rig.engine.push(Scripted::Reply(Box::new(reply(
        &[],
        ShellResult::Cursor { documents: vec![raw(r#"{"n":3}"#)], cursor: None },
        "app",
    ))));
    rig.click("shell-more");
    rig.wait_ready();
    let requests = rig.engine.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].script, "it");
    assert_eq!(requests[1].cursor, Some(cursor), "the cursor the plugin returned, nothing else");
    assert_eq!(rig.rows(), 1, "the next page");
    assert!(!rig.rendered("shell-more"), "the server is done");
    assert_eq!(rig.read(|this| this.recorded_history.len()), 1, "More is not a run of its own");
}

#[gpui::test]
fn a_cursor_the_plugin_makes_up_is_not_offered(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    // A cursor on a database the run never touched: dropped, the page still shows.
    let foreign = CursorRef { id: "7".into(), database: "admin".into(), collection: "system.users".into(), batch_size: 5 };
    rig.engine.push(Scripted::Reply(Box::new(reply(
        &[],
        ShellResult::Cursor { documents: vec![raw(r#"{"n":1}"#)], cursor: Some(foreign) },
        "app",
    ))));
    rig.run_script("db.items.aggregate([])");
    rig.wait_ready();
    assert_eq!(rig.rows(), 1);
    assert!(!rig.rendered("shell-more"));

    // A document that is not an object, or a database name that is not one, never reaches the grid.
    rig.engine.push(Scripted::Reply(Box::new(reply(&["x"], documents(&["42"]), "app"))));
    rig.click("run-query");
    rig.wait_ready();
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));
    assert_eq!(rig.rows(), 0);
    let mut odd_name = reply(&[], ShellResult::None, "../etc/passwd");
    odd_name.output = vec!["hi".into()];
    rig.engine.push(Scripted::Reply(Box::new(odd_name)));
    rig.click("run-query");
    rig.wait_ready();
    let tab = rig.tab_id;
    assert_eq!(
        rig.read(|this| this.workspace.tab(tab).unwrap().database.clone()),
        "fixture_db_00".to_string().replace("fixture_db_00", &rig.read(|this| this.workspace.tab(tab).unwrap().database.clone())),
        "the tab keeps its own database"
    );
    assert!(!rig.read(|this| this.workspace.tab(tab).unwrap().database.contains('/')));
}

#[gpui::test]
fn stop_cancels_a_running_script_and_its_late_result_is_ignored(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.engine.push(Scripted::Hang);
    rig.run_script("while (true) {}");
    rig.wait("the run to start", |this| this.mongo_shell.runs_len() == 1);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    rig.click("run-query"); // Cancel while it runs
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    rig.wait("the run to be stopped", {
        let engine = rig.engine.clone();
        move |_| engine.stops() == 1
    });
    settle(rig.visual);
    assert_eq!(rig.engine.stops(), 1, "stopped once");
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled, "the cancelled run's end changes nothing");
    assert!(rig.shell_view().is_none());
    assert!(rig.read(|this| this.mongo_shell.runs_len() == 0));
}

#[gpui::test]
fn a_run_that_dies_reports_why_and_leaves_no_result(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.engine.push(Scripted::Fail("out of memory".into()));
    rig.run_script("var a = []; while (true) a.push(1)");
    rig.wait_ready();
    let status = rig.status();
    assert!(matches!(&status, crate::query::QueryStatus::Failed(message) if message.contains("out of memory")), "{status:?}");
    assert!(rig.read(|this| this.error_log.log.len()) >= 1);
    assert_eq!(rig.read(|this| this.recorded_history.len()), 0);
}

#[gpui::test]
fn a_plugin_file_that_changed_since_the_install_is_never_run(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    let path = rig.read(|this| this.driver_ui.tools.shell_item().unwrap().path.clone());
    // Someone swaps the executable (same size, so only the digest tells).
    let mut swapped = std::fs::read(&path).unwrap();
    swapped[0] ^= 0xff;
    std::fs::write(&path, swapped).unwrap();

    rig.run_script("print(1)");
    rig.wait_ready();
    assert!(rig.engine.requests().is_empty(), "the swapped file was not started");
    let status = rig.status();
    assert!(
        matches!(&status, crate::query::QueryStatus::Failed(message) if message.contains("MongoDB Shell plugin")),
        "{status:?}"
    );
}

#[gpui::test]
fn a_single_find_never_reaches_the_plugin(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    let engine = std::sync::Arc::new(FakeShell::default());
    let shared = engine.clone();
    app.update(visual, |this, _| this.mongo_shell.engine = shared);
    click_debug_button(visual, "run-query");
    settle(visual);
    assert_eq!(script_rows(&app, visual), 200, "the template find ran on the fast path");
    assert!(engine.requests().is_empty());
    assert!(visual.debug_bounds("shell-prompt-body").is_none());
    assert!(app.read_with(visual, |this, _| this.shell_view(this.active_id()).is_none()));
}

// ---- the add-on row in the Driver Manager -----------------------------------------------------

#[gpui::test]
fn the_add_on_row_sits_under_the_mongodb_row_and_installs_updates_and_removes(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.open_manager();
    let driver = rig.visual.debug_bounds("driver-row-mongodb").expect("the MongoDB row");
    let addon = rig.visual.debug_bounds("addon-row").expect("the add-on row");
    assert!(addon.top() >= driver.bottom() - gpui::px(1.0), "right under it: {driver:?} {addon:?}");
    assert!((addon.size.height - driver.size.height).abs() < gpui::px(1.0), "the same fixed row height");
    assert!(rig.rendered("addon-size") && rig.rendered("addon-version"));
    assert!(!rig.rendered("addon-reason"), "nothing is missing");
    assert!(!rig.rendered("addon-more"), "nothing to remove yet");

    // Install: a real click on the row's button, real progress, the file at its stable path.
    click_driver_row_button(&mut rig, "addon-action");
    rig.wait("the plugin install", |this| {
        this.driver_ui.tools.job.is_none() && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.is_some())
    });
    assert!(rig.plugin_installed());
    assert!(rig.rendered("addon-more"), "now it can be removed");
    assert!(!rig.rendered("addon-action"), "up to date: no button");

    // A newer plugin in the manifest: Update (the button is back, the version shows the arrow).
    rig.publish("0.2.0", PLUGIN_MIN_DRIVER);
    rig.load_catalog();
    assert!(rig.rendered("addon-action"));
    let state = rig.read(|this| this.driver_ui.tools.shell_item().unwrap().state.clone());
    assert!(matches!(state, engine_driver_host::ToolCatalogState::UpdateAvailable { .. }), "{state:?}");
    click_driver_row_button(&mut rig, "addon-action");
    rig.wait("the plugin update", |this| {
        this.driver_ui.tools.job.is_none()
            && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.as_ref().is_some_and(|record| record.version == "0.2.0"))
    });

    // Remove from the row's menu.
    rig.click("addon-more");
    rig.visual.simulate_keystrokes("down");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait("the removal", |this| {
        this.driver_ui.op_task.is_none() && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.is_none())
    });
    assert!(!rig.plugin_installed());
    assert!(rig.rendered("addon-row"), "still offered");
}

/// A real mouse click on the centre of the add-on row's button (the list hit-tests its rows).
fn click_driver_row_button(rig: &mut ShellRig<'_>, selector: &str) {
    rig.click(selector);
}

#[gpui::test]
fn without_its_driver_the_add_on_says_so_and_the_button_installs_the_driver_first(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, None);
    rig.offer_installable_driver();
    rig.open_manager();
    assert!(rig.rendered("addon-reason"), "the reason is shown");
    let need = rig.read(|this| this.addon_need(this.driver_ui.tools.shell_item().unwrap()));
    let need = need.expect("an unmet requirement");
    assert!(need.reason().contains("MongoDB") && need.reason().contains("0.2.0") && need.reason().contains("not installed"));
    assert!(need.fixable);
    let actions = rig.read(|this| {
        let item = this.driver_ui.tools.shell_item().unwrap();
        driver_manager::addon_actions(item, None, this.addon_need(item).as_ref())
    });
    assert_eq!(actions, [driver_manager::ToolAction::InstallDriverFirst], "no plain Install while the driver is missing");
    let button = rig.visual.debug_bounds("addon-action").expect("the button");
    assert!(button.size.width >= gpui::px(130.0), "wide enough for 'Install driver first': {button:?}");

    // One click queues the driver, then the plugin follows it.
    rig.click("addon-action");
    rig.wait("the driver and then the plugin", |this| {
        this.driver_ui.queue.job("mongodb").is_none()
            && this.driver_ui.tools.job.is_none()
            && this.driver_ui.tools.after_driver.is_none()
            && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.is_some())
    });
    assert!(rig.plugin_installed());
    assert!(rig.read(|this| this.driver_ui.has_driver("mongodb")), "the driver is in");
    assert!(!rig.rendered("addon-reason"), "nothing is missing any more");
}

#[gpui::test]
fn a_too_old_driver_updates_first_and_a_failed_driver_install_leaves_the_add_on_waiting(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.1.0"));
    rig.open_manager();
    let actions = rig.read(|this| {
        let item = this.driver_ui.tools.shell_item().unwrap();
        driver_manager::addon_actions(item, None, this.addon_need(item).as_ref())
    });
    assert_eq!(actions, [driver_manager::ToolAction::UpdateDriverFirst]);
    // The manifest's driver build cannot be downloaded: the driver job fails, the add-on does not
    // install and the row keeps saying what it needs.
    rig.click("addon-action");
    rig.wait("the driver job to fail", |this| {
        matches!(this.driver_ui.queue.job("mongodb").map(|job| &job.state), Some(InstallState::Failed(_)))
    });
    assert!(rig.read(|this| this.driver_ui.tools.after_driver.is_none()));
    assert!(!rig.plugin_installed());
    assert!(rig.rendered("addon-reason"));
}

#[gpui::test]
fn an_add_on_whose_driver_went_away_is_shown_as_needing_it_and_never_runs(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    // The driver goes (its record is removed behind the app's back, as an uninstall would).
    let store = rig.read(|this| this.driver_ui.store.clone());
    store.remove_record("mongodb").unwrap();
    rig.load_catalog();
    rig.wait("the catalog", |this| !this.driver_ui.installed.contains("mongodb"));

    // A script does not run through a plugin that cannot work: it offers the driver first.
    rig.run_script("print(1)");
    assert!(rig.rendered("shell-prompt-body"));
    assert!(matches!(
        rig.read(|this| this.shell_plugin_state()),
        crate::app::mongo_shell::PluginState::DriverFirst { .. }
    ));
    rig.click("shell-prompt-cancel");
    assert!(rig.engine.requests().is_empty());

    rig.open_manager();
    assert!(rig.rendered("addon-reason"), "an orphan says what it needs");
    let need = rig.read(|this| this.addon_need(this.driver_ui.tools.shell_item().unwrap())).unwrap();
    assert!(need.reason().starts_with("Needs MongoDB driver"), "{}", need.reason());
    assert!(rig.rendered("addon-more"), "it can still be removed");
}

#[gpui::test]
fn uninstalling_the_driver_warns_that_the_shell_stops_and_offers_to_remove_it_too(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.open_manager();
    request_removal(&mut rig, true);
    assert!(rig.rendered("driver-confirm-also"), "the dialog offers to remove the add-on too");
    rig.click("driver-confirm-cancel");
    assert!(rig.plugin_installed() && rig.read(|this| this.driver_ui.installed.contains("mongodb")), "Cancel changes nothing");

    // Driver only: the plugin stays, orphaned.
    request_removal(&mut rig, true);
    rig.click("driver-confirm-ok");
    rig.wait("the driver removal", |this| this.driver_ui.op_task.is_none() && !this.driver_ui.installed.contains("mongodb"));
    assert!(rig.plugin_installed(), "only the driver went");
    rig.wait("the catalog", |this| this.driver_ui.catalog_task.is_none());
    assert!(rig.rendered("addon-reason"));

    // Both: bring the driver back, then remove it with the add-on.
    rig.update(|this, _| {
        this.driver_ui
            .store
            .save(engine_driver_host::InstalledDriverRecord { id: "mongodb".into(), version: "0.3.0".into(), ..Default::default() })
            .unwrap();
    });
    rig.load_catalog();
    request_removal(&mut rig, true);
    rig.click("driver-confirm-also");
    rig.wait("both removals", |this| {
        this.driver_ui.op_task.is_none()
            && !this.driver_ui.installed.contains("mongodb")
            && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.is_none())
    });
    assert!(!rig.plugin_installed());
}

#[gpui::test]
fn rolling_the_driver_back_below_the_minimum_warns_too(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    // 0.3.0 is current; 0.1.0 (older than the plugin's minimum) is the previous version.
    rig.update(|this, _| {
        this.driver_ui
            .store
            .save(engine_driver_host::InstalledDriverRecord {
                id: "mongodb".into(),
                version: "0.3.0".into(),
                previous_version: Some("0.1.0".into()),
                ..Default::default()
            })
            .unwrap();
    });
    rig.open_manager();
    rig.wait("the record", |this| this.driver_ui.records.get("mongodb").is_some_and(|r| r.previous_version.is_some()));
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| this.request_driver_rollback("mongodb".into(), window, cx));
    });
    settle(rig.visual);
    assert!(rig.rendered("driver-confirm-also"), "the rollback would break the shell: it says so");
    rig.click("driver-confirm-cancel");
    assert_eq!(rig.read(|this| this.driver_ui.records["mongodb"].version.clone()), "0.3.0");

    // A rollback to a version that still satisfies it asks nothing (the dialog is not opened).
    rig.update(|this, _| {
        this.driver_ui
            .store
            .save(engine_driver_host::InstalledDriverRecord {
                id: "mongodb".into(),
                version: "0.4.0".into(),
                previous_version: Some("0.3.0".into()),
                ..Default::default()
            })
            .unwrap();
    });
    rig.load_catalog();
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| this.request_driver_rollback("mongodb".into(), window, cx));
    });
    settle(rig.visual);
    assert!(!rig.rendered("driver-confirm-also") && !rig.rendered("driver-confirm-ok"), "no question for a safe rollback");
}

fn request_removal(rig: &mut ShellRig<'_>, remove: bool) {
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| this.request_driver_removal("mongodb".into(), remove, window, cx));
    });
    settle(rig.visual);
}

// ---- live: the real plugin and the real driver against a PRIVATE mongod ---------------------------
//
//   mongod --dbpath <tmp> --port 27999 --bind_ip 127.0.0.1            # a private server, never 27017
//   cargo build -p dbengine-mongo --features abi-v2 && cargo build -p tablex-mongo-shell   (in rust-engine)
//   TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:27999 \
//   TABLEX_TEST_MONGO_DYLIB=<rust-engine target>/debug/libdbengine_mongo.dylib \
//   TABLEX_TEST_SHELL_EXE=<rust-engine target>/debug/tablex-mongo-shell \
//     cargo test -p tablex-gpui live_shell -- --ignored --nocapture --test-threads=1
//
// The plugin is installed through the real tool store (its file is verified before every run), the
// script goes through the real host (a child process, framed protocol) and the real driver's
// `mongo_command` to the server. Data lives in one database of this process's own name, dropped at the end.

#[gpui::test]
#[ignore = "needs a private mongod on 127.0.0.1:27999, the built driver and the built plugin (see above)"]
fn live_shell_runs_scripts_pages_with_more_switches_database_and_stops(cx: &mut TestAppContext) {
    let (Ok(uri), Ok(dylib), Ok(exe)) = (
        std::env::var("TABLEX_TEST_MONGO_URI"),
        std::env::var("TABLEX_TEST_MONGO_DYLIB"),
        std::env::var("TABLEX_TEST_SHELL_EXE"),
    ) else {
        eprintln!("live shell test skipped: TABLEX_TEST_MONGO_URI / _DYLIB / TABLEX_TEST_SHELL_EXE not set");
        return;
    };
    assert!(uri.contains("27999"), "the live shell tests only run against the private mongod on 27999, not {uri}");
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    // The real plugin through the real store, and the real engine.
    rig.plugin = std::fs::read(&exe).expect("the built plugin");
    rig.publish("0.1.0", PLUGIN_MIN_DRIVER);
    rig.install_plugin();
    let staged = rig.root.join("libdbengine_mongo.dylib");
    std::fs::copy(&dylib, &staged).expect("copy the mongo driver");
    let handle = engine_driver_host::DriverLoader.load(&staged).expect("the mongo driver loads");
    let session = handle
        .connect(&serde_json::json!({"uri": uri, "name": "live"}).to_string())
        .expect("connect to the private mongod");
    let database = format!("tablex_gpui_shell_{}", std::process::id());
    let (tab_id, connection) = (rig.tab_id, rig.connection.clone());
    let db = database.clone();
    rig.update(|this, _| {
        let engine_id = session.connection_id();
        let capabilities = std::sync::Arc::new(handle.capabilities().clone());
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        this.driver_runtime.sessions.insert(connection.clone(), session);
        this.driver_runtime.connection_ids.insert(connection.clone(), engine_id);
        this.driver_runtime.capabilities.insert(connection.clone(), capabilities);
        this.driver_ui.handles.insert("mongodb".into(), handle);
        this.mongo_shell.engine = std::sync::Arc::new(crate::app::mongo_shell::HostEngine);
        let tab = this.workspace.tab_mut(tab_id).unwrap();
        tab.database = db;
        tab.collection = "items".into();
        tab.output_limit = 50;
    });
    let no_run = move |this: &AppRoot| {
        !matches!(this.workspace.runtime(tab_id).unwrap().query.status(), crate::query::QueryStatus::Running { .. })
    };

    // A loop with inserts, a print, then a cursor: the first page with More.
    rig.run_script(
        "for (let i = 0; i < 120; i++) { db.items.insertOne({ _id: i, n: NumberLong(i) }) }\nprint('inserted');\ndb.items.aggregate([{ $sort: { _id: 1 } }])",
    );
    rig.wait("the first run", no_run);
    let status = rig.status();
    assert_eq!(status, crate::query::QueryStatus::Ready { rows: 50 }, "{status:?}");
    assert_eq!(rig.shell_view().unwrap().output, ["inserted"]);
    assert!(rig.rendered("shell-more") && rig.rendered("shell-output"));
    // More: the next 50, then the last 20, then no more.
    for (expected, more_after) in [(50, true), (20, false)] {
        rig.click("shell-more");
        rig.wait("More", no_run);
        assert_eq!(rig.rows(), expected);
        assert_eq!(rig.rendered("shell-more"), more_after);
    }

    // A count is shell text; `use` moves the tab's database and the next run starts there.
    rig.run_script_replacing("db.items.countDocuments({})");
    rig.wait("the count", no_run);
    assert_eq!(rig.shell_view().unwrap().value_text.as_deref(), Some("120"));
    let other = format!("{database}_other");
    rig.run_script_replacing(&format!("use {other}"));
    rig.wait("use", no_run);
    assert_eq!(rig.read(|this| this.workspace.tab(tab_id).unwrap().database.clone()), other);
    rig.run_script_replacing(&format!("use {database}"));
    rig.wait("use back", no_run);

    // Stop kills the plugin, not just the tab's wait.
    rig.run_script_replacing("print('sleeping'); sleep(60000)");
    rig.wait("the script to start", |this| this.mongo_shell.runs_len() == 1);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let installed = rig.read(|this| this.driver_ui.tools.shell_item().unwrap().path.clone());
    let alive = |installed: &std::path::Path| {
        std::process::Command::new("pgrep").arg("-f").arg(installed).output().map(|out| out.status.success()).unwrap_or(false)
    };
    assert!(alive(&installed), "the plugin process runs the script");
    rig.click("run-query"); // Cancel
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    let started = std::time::Instant::now();
    while alive(&installed) && started.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!alive(&installed), "Stop killed the plugin process");

    // A read-only connection refuses a write with the plugin's message and changes nothing.
    publish_types(&rig.app, rig.visual, false);
    let connection = rig.connection.clone();
    give_profile(&rig.app, rig.visual, &connection, typed("replica"));
    rig.run_script_replacing("db.items.deleteMany({})");
    rig.wait("the refused write", no_run);
    let status = rig.status();
    assert!(matches!(&status, crate::query::QueryStatus::Failed(message) if message.to_lowercase().contains("read-only") || message.to_lowercase().contains("read only")), "{status:?}");
    rig.run_script_replacing("db.items.countDocuments({})");
    rig.wait("the count again", no_run);
    assert_eq!(rig.shell_view().unwrap().value_text.as_deref(), Some("120"), "nothing was deleted");

    // Clean up (writes allowed again).
    give_profile(&rig.app, rig.visual, &connection, ConnectionProfile::default());
    rig.run_script_replacing("db.dropDatabase()");
    rig.wait("the cleanup", no_run);
    rig.run_script_replacing(&format!("db.getSiblingDB('{other}').dropDatabase()"));
    rig.wait("the other cleanup", no_run);
}

#[gpui::test]
fn a_shell_results_footer_has_no_refresh_staging_or_paging_and_nothing_re_runs_it(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], documents(&[r#"{"a":1}"#]), "app"))));
    rig.run_script("db.c.insertOne({ a: 1 }); db.c.aggregate([])");
    rig.wait_ready();
    assert_eq!(rig.engine.requests().len(), 1);
    for gone in [
        "result-group-commit",
        "result-group-row",
        "result-group-history",
        "result-group-pagination",
        "result-refresh-dropdown",
    ] {
        assert!(!rig.rendered(gone), "{gone} is not part of a shell result's footer");
    }
    assert!(rig.rendered("result-refresh") && rig.rendered("result-group-output"), "a disabled Refresh and Export stay");
    // Clicking the disabled Refresh does nothing: a script that writes is not run again by it.
    rig.click("result-refresh");
    assert_eq!(rig.engine.requests().len(), 1);
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    // A failed run shows its error once: the strip has it, the empty area only says there is no result.
    let mut broken = reply(&[], ShellResult::None, "app");
    broken.error = Some(failure(FailureKind::Script, "boom", Some((1, 2))));
    rig.engine.push(Scripted::Reply(Box::new(broken)));
    rig.click("run-query");
    rig.wait_ready();
    assert!(rig.rendered("shell-error") && rig.rendered("result-empty-state"));
}

// ---- review fixes: refresh, output text, threads, waits ----------------------------------------

/// Runs a script that produced one document and leaves the grid focused.
fn shown_shell_result(rig: &mut ShellRig<'_>) {
    rig.install_plugin();
    rig.engine.push(Scripted::Reply(Box::new(reply(&["done"], documents(&[r#"{"a":1}"#]), "app"))));
    rig.run_script("db.c.insertOne({ a: 1 }); db.c.aggregate([])");
    rig.wait_ready();
    assert_eq!(rig.engine.requests().len(), 1);
    rig.visual.update(|window, cx| {
        // The default key bindings (Cmd+R is `RefreshGridResults` in the grid's key context).
        actions::bind_keys(cx);
        window.activate_window();
    });
    let cell = rig.visual.debug_bounds("grid-body-cell-0-0").expect("the shell result's first cell");
    rig.visual.simulate_click(cell.center(), Default::default());
    settle(rig.visual);
    let app = rig.app.clone();
    assert!(rig.visual.update(|window, cx| app.read(cx).av().grid_is_focused(window)), "the grid has the keyboard");
}

#[gpui::test]
fn cmd_r_the_refresh_action_auto_refresh_and_the_limit_never_re_run_a_shell_result(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    shown_shell_result(&mut rig);
    let (tab, runs) = (rig.tab_id, rig.engine.clone());
    // The grid's context menu: Refresh is its last item, reached with Up from the top.
    let cell = rig.visual.debug_bounds("grid-body-cell-0-0").expect("the cell");
    rig.visual.simulate_mouse_down(cell.center(), gpui::MouseButton::Right, Default::default());
    rig.visual.simulate_mouse_up(cell.center(), gpui::MouseButton::Right, Default::default());
    settle(rig.visual);
    rig.visual.simulate_keystrokes("up enter");
    settle(rig.visual);
    assert_eq!(runs.requests().len(), 1, "the context menu's Refresh did not run the script again");
    // Cmd+R with the grid focused.
    rig.visual.simulate_keystrokes("secondary-r");
    settle(rig.visual);
    assert_eq!(runs.requests().len(), 1, "Cmd+R did not run the script again");
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    // The action itself (what the menu item dispatches).
    rig.visual.dispatch_action(RefreshGridResults);
    settle(rig.visual);
    assert_eq!(runs.requests().len(), 1, "the Refresh action did not either");
    // Everything that asks to re-run the shown result (an edit, auto refresh) is refused at the root.
    rig.update(|this, cx| {
        this.rerun_shown_result(tab, false, cx);
        this.rerun_shown_result(tab, true, cx);
    });
    settle(rig.visual);
    assert_eq!(runs.requests().len(), 1);
    // The limit is not offered for a shell result; asking anyway only remembers it for the next Run.
    assert!(!rig.rendered("result-limit"), "no Limit beside a shell result");
    rig.update(|this, cx| this.set_output_limit(tab, 100, cx));
    settle(rig.visual);
    assert_eq!(runs.requests().len(), 1, "changing the limit did not run the script");
    assert_eq!(rig.read(|this| this.workspace.tab(tab).unwrap().output_limit), 100);
    assert!(rig.shell_view().is_some(), "the result stays");
    // Run is the explicit way, and it uses the remembered limit.
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "app"))));
    rig.click("run-query");
    rig.wait_ready();
    assert_eq!(runs.requests().len(), 2);
    assert_eq!(runs.requests()[1].page_size, 100);
}

#[gpui::test]
fn a_printjson_block_is_several_lines_and_hostile_text_is_bounded_and_clean(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    let block = "{\n  \"name\": \"a\",\n  \"tags\": [\n    \"x\"\n  ]\n}";
    rig.engine.push(Scripted::Reply(Box::new(reply(&["before", block, "after"], ShellResult::None, "app"))));
    rig.run_script("printjson(db.c.findOne())");
    rig.wait_ready();
    let view = rig.shell_view().unwrap();
    let lines: Vec<&str> = view.output.iter().map(AsRef::as_ref).collect();
    assert_eq!(lines, ["before", "{", "  \"name\": \"a\",", "  \"tags\": [", "    \"x\"", "  ]", "}", "after"]);
    assert!(rig.rendered("shell-output") && rig.rendered("shell-output-toggle"));

    // A 30 MB error message and ESC / bidi characters: bounded and visible, the UI is not held.
    let started = std::time::Instant::now();
    let mut hostile = reply(&["x\u{1b}[2Jy\u{202e}z"], ShellResult::None, "app");
    hostile.error = Some(ShellFailure {
        kind: FailureKind::Script,
        message: format!("\u{1b}[31m{}", "x".repeat(30_000_000)),
        line: Some(1),
        column: Some(1),
        code: None,
        code_name: Some("C".repeat(1_000_000)),
    });
    rig.engine.push(Scripted::Reply(Box::new(hostile)));
    rig.click("run-query");
    rig.wait_ready();
    let view = rig.shell_view().unwrap();
    let error = view.error.as_ref().unwrap();
    assert!(error.message.len() < 9 * 1024 && error.message.ends_with("(cut at 8 KiB)"), "{}", error.message.len());
    assert!(!error.message.contains('\u{1b}'));
    assert!(error.code_name.as_ref().unwrap().len() < 9 * 1024);
    assert!(view.output[0].chars().all(|c| !c.is_control() && c != '\u{202e}'), "{:?}", view.output[0]);
    let status = rig.status();
    assert!(matches!(&status, crate::query::QueryStatus::Failed(message) if message.len() < 10 * 1024), "bounded status");
    assert!(rig.rendered("shell-error"));
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "the run did not hang");
}

#[gpui::test]
fn the_reply_is_checked_and_its_page_built_off_the_ui_thread(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    let before = crate::mongo_shell::THREADS.lock().unwrap().len();
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], documents(&[r#"{"a":1}"#, r#"{"a":2}"#]), "app"))));
    rig.run_script("db.c.aggregate([])");
    rig.wait_ready();
    assert_eq!(rig.rows(), 2);
    let ui_thread = std::thread::current().id();
    let steps: Vec<(&str, std::thread::ThreadId)> = crate::mongo_shell::THREADS.lock().unwrap()[before..].to_vec();
    for step in ["check_reply", "documents_page"] {
        assert!(steps.iter().any(|(name, _)| *name == step), "{step} ran: {steps:?}");
    }
    assert!(steps.iter().all(|(_, thread)| *thread != ui_thread), "the UI thread parsed nothing: {steps:?}");
}

#[gpui::test]
fn a_waiting_script_does_not_run_after_the_tab_moved_to_another_database(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    let mut slow = SlowArtifact::new(&rig.root, rig.plugin.clone());
    let artifact = file_url(&slow.path);
    rig.publish_with_url("0.1.0", PLUGIN_MIN_DRIVER, &artifact);
    rig.load_catalog();
    rig.run_script("print(1)");
    rig.click("shell-prompt-install");
    rig.wait("the download to start", |this| {
        matches!(this.driver_ui.tools.job.as_ref().map(|job| &job.state), Some(InstallState::Downloading { bytes, .. }) if *bytes > 0)
    });
    let tab = rig.tab_id;
    rig.update(|this, cx| this.choose_script_database(tab, "somewhere_else".into(), cx));
    slow.release();
    rig.wait("the install", |this| this.driver_ui.tools.job.is_none() && this.driver_ui.tools.shell_item().is_some_and(|item| item.installed.is_some()));
    settle(rig.visual);
    assert!(rig.engine.requests().is_empty(), "the script was not run in the database the tab moved to");
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
}

#[gpui::test]
fn a_queued_driver_install_that_leaves_the_queue_ends_the_scripts_wait(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.1.0"));
    rig.load_catalog();
    for cancel_all in [false, true] {
        // Another driver install is running (a stand-in), so the MongoDB one queues behind it.
        rig.update(|this, _| {
            let entry = this.driver_ui.model.row("mongodb").and_then(|row| row.entry.clone()).unwrap();
            let origin = this.driver_ui.current_origin();
            this.driver_ui.queue.set_origin(origin);
            if this.driver_ui.queue.running().is_none() {
                let mut other = entry;
                other.id = "other".into();
                this.driver_ui.queue.enqueue(driver_manager::JobKind::Install, other);
                this.driver_ui.queue.start_next();
            }
        });
        rig.run_script("print(1)");
        rig.click("shell-prompt-install");
        assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }), "the script waits");
        assert!(rig.read(|this| this.driver_ui.tools.after_driver.is_some()));
        assert!(rig.rendered("shell-waiting"));
        assert!(matches!(
            rig.read(|this| this.driver_ui.queue.job("mongodb").map(|job| job.state.clone())),
            Some(InstallState::Queued)
        ));
        rig.update(|this, cx| {
            if cancel_all {
                this.cancel_all_driver_installs(cx)
            } else {
                this.cancel_driver_install("mongodb", cx)
            }
        });
        settle(rig.visual);
        assert!(rig.read(|this| this.driver_ui.tools.after_driver.is_none()), "no wait is left for the driver");
        assert!(rig.read(|this| this.mongo_shell.waits_len() == 0));
        let status = rig.status();
        assert!(
            matches!(&status, crate::query::QueryStatus::Failed(message) if message.contains("Could not install the MongoDB Shell plugin")),
            "{status:?}"
        );
        assert!(!rig.rendered("shell-waiting"), "the waiting banner is gone");
        assert!(rig.engine.requests().is_empty());
    }
}

#[gpui::test]
fn an_old_shell_result_does_not_stay_after_a_failed_run_or_a_database_change(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.engine.push(Scripted::Reply(Box::new(reply(&["old"], documents(&[r#"{"a":1}"#]), "app"))));
    rig.run_script("db.c.aggregate([])");
    rig.wait_ready();
    assert!(rig.shell_view().is_some());
    // A run that dies: the previous run's strip and Output are gone, not left beside the failure.
    rig.engine.push(Scripted::Fail("boom".into()));
    rig.click("run-query");
    rig.wait_ready();
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));
    assert!(rig.shell_view().is_none() && !rig.rendered("shell-output"));
    // A new result, then the database changes: the result is not shown for the new target.
    rig.engine.push(Scripted::Reply(Box::new(reply(&["fresh"], documents(&[r#"{"a":2}"#]), "app"))));
    rig.click("run-query");
    rig.wait_ready();
    assert!(rig.shell_view().is_some());
    let tab = rig.tab_id;
    rig.update(|this, cx| this.choose_script_database(tab, "another".into(), cx));
    assert!(rig.shell_view().is_none());
}

#[gpui::test]
fn only_the_drivers_own_not_connected_marks_the_connection_lost(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    let connection = rig.connection.clone();
    let connected = |rig: &ShellRig<'_>| {
        let connection = connection.clone();
        rig.read(move |this| matches!(this.sessions.status(&connection), ConnectionStatus::Connected))
    };
    assert!(connected(&rig));
    // Text from the plugin that looks like a lost session proves nothing.
    rig.engine.push(Scripted::Fail("session.not_connected: not connected".into()));
    rig.run_script("print(1)");
    rig.wait_ready();
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));
    assert!(connected(&rig), "plugin text does not drop the connection");
    // The driver saying so does.
    rig.engine.push(Scripted::FailHost(ShellHostError::Driver(engine_driver_host::LoaderError::DriverRejected {
        code: "session.not_connected".into(),
        message: "gone".into(),
        retryable: false,
    })));
    rig.click("run-query");
    rig.wait_ready();
    assert!(!connected(&rig), "a typed not-connected from the driver marks the connection lost");
}

#[gpui::test]
fn a_crashed_plugins_stderr_is_cleaned_before_the_pane_and_the_error_log(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    rig.install_plugin();
    rig.engine.push(Scripted::FailHost(ShellHostError::Crashed(
        ": boom\u{1b}[2J\u{202e}gnp.exe\u{2028}next".into(),
    )));
    rig.run_script("print(1)");
    rig.wait_ready();
    let crate::query::QueryStatus::Failed(message) = rig.status() else {
        panic!("the run failed");
    };
    assert!(message.contains("boom") && message.contains("next"), "{message:?}");
    assert!(
        !message.chars().any(|c| matches!(c, '\u{1b}' | '\u{202e}' | '\u{2028}')),
        "{message:?}"
    );
    let logged = rig.read(|this| this.error_log.log.last().map(|entry| entry.to_line()).unwrap_or_default());
    assert!(!logged.chars().any(|c| matches!(c, '\u{1b}' | '\u{202e}' | '\u{2028}')), "{logged:?}");
}

// ---- implicit re-runs never run a script ------------------------------------------------------

const RUN_AGAIN: &str = "Run the script again with Run.";

/// Everything that re-runs a tab's result without the user pressing Run: paging, the grid's Refresh,
/// the Limit, a staged edit's reload, a reconnect.
fn implicit_reruns(this: &mut AppRoot, tab: u64, window: &mut gpui::Window, cx: &mut Context<AppRoot>) {
    this.run_tab_query(tab, cx);
    this.page_tab_query(tab, true, cx);
    this.page_tab_query(tab, false, cx);
    this.rerun_shown_result(tab, false, cx);
    this.rerun_shown_result(tab, true, cx);
    this.refresh_tab_results(tab, window, cx);
    this.set_output_limit(tab, 50, cx);
    this.finish_mongo_commit(tab, "conn", Ok(1), cx);
    this.rerun_stale_tab(tab, cx);
}

fn detail_error(this: &AppRoot, tab: u64) -> Option<String> {
    this.view(tab).detail_ui.error.clone()
}

/// Focuses the grid (its first cell) so its key bindings dispatch.
fn focus_grid_cell(rig: &mut ShellRig<'_>) -> bool {
    rig.visual.update(|window, cx| {
        actions::bind_keys(cx);
        window.activate_window();
    });
    let Some(cell) = rig.visual.debug_bounds("grid-body-cell-0-0") else {
        return false;
    };
    rig.visual.simulate_click(cell.center(), Default::default());
    settle(rig.visual);
    true
}

#[gpui::test]
fn a_stopped_write_loop_is_not_run_again_by_refresh_the_limit_or_cmd_r(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    shown_shell_result(&mut rig);
    let (tab, runs) = (rig.tab_id, rig.engine.clone());
    // The write loop starts and is stopped: the shell result is gone, the script is still the editor's.
    rig.engine.push(Scripted::Hang);
    rig.click("run-query");
    rig.wait("the run to start", |this| this.mongo_shell.runs_len() == 1);
    rig.click("run-query"); // Cancel
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    rig.wait("the run to be stopped", {
        let engine = runs.clone();
        move |_| engine.stops() == 1
    });
    assert!(rig.shell_view().is_none(), "Stop cleared the shell result");
    assert_eq!(runs.requests().len(), 2);
    // Cmd+R and the context menu on whatever grid is left.
    if focus_grid_cell(&mut rig) {
        rig.visual.simulate_keystrokes("secondary-r");
        settle(rig.visual);
    }
    rig.visual.dispatch_action(RefreshGridResults);
    settle(rig.visual);
    rig.update(|this, cx| this.set_output_limit(tab, 100, cx));
    settle(rig.visual);
    rig.visual.update(|window, cx| {
        let app = rig.app.clone();
        app.update(cx, |this, cx| implicit_reruns(this, tab, window, cx));
    });
    settle(rig.visual);
    assert_eq!(runs.requests().len(), 2, "nothing re-ran the script");
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    assert_eq!(rig.read(|this| detail_error(this, tab)).as_deref(), Some(RUN_AGAIN));
    // Run is the way, and it still works.
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "app"))));
    rig.click("run-query");
    rig.wait_ready();
    assert_eq!(runs.requests().len(), 3);
}

#[gpui::test]
fn a_shell_script_is_not_run_again_in_the_database_the_tab_moved_to(cx: &mut TestAppContext) {
    let mut rig = ShellRig::new(cx, Some("0.3.0"));
    shown_shell_result(&mut rig);
    let (tab, runs) = (rig.tab_id, rig.engine.clone());
    rig.update(|this, cx| this.choose_script_database(tab, "another".into(), cx));
    settle(rig.visual);
    assert!(rig.shell_view().is_none(), "the result is not shown for the new database");
    if focus_grid_cell(&mut rig) {
        rig.visual.simulate_keystrokes("secondary-r");
        settle(rig.visual);
    }
    rig.visual.update(|window, cx| {
        let app = rig.app.clone();
        app.update(cx, |this, cx| implicit_reruns(this, tab, window, cx));
    });
    settle(rig.visual);
    assert_eq!(runs.requests().len(), 1, "the script did not run in the new database");
    // Run does, there.
    rig.engine.push(Scripted::Reply(Box::new(reply(&[], ShellResult::None, "another"))));
    rig.click("run-query");
    rig.wait_ready();
    assert_eq!(runs.requests().len(), 2);
    assert_eq!(runs.requests()[1].database, "another");
}

#[gpui::test]
fn text_typed_after_a_find_ran_is_not_run_by_refresh_paging_a_commit_or_a_reload(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    let engine = std::sync::Arc::new(FakeShell::default());
    let shared = engine.clone();
    app.update(visual, |this, _| this.mongo_shell.engine = shared);
    click_debug_button(visual, "run-query");
    settle(visual);
    assert_eq!(script_rows(&app, visual), 200, "the find ran on the fast path");
    let (tab, shown) = app.read_with(visual, |this, _| {
        let tab = this.active_id();
        (tab, this.workspace.runtime(tab).unwrap().last_executed_query.clone().expect("the executed find"))
    });
    // The user types a delete, without pressing Run.
    script_type(visual, "db.fixture_collection_00.deleteMany({})");
    visual.update(|window, cx| {
        actions::bind_keys(cx);
        window.activate_window();
    });
    let cell = visual.debug_bounds("grid-body-cell-0-0").expect("the grid");
    visual.simulate_click(cell.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("secondary-r");
    settle(visual);
    visual.dispatch_action(RefreshGridResults);
    settle(visual);
    let root = app.clone();
    visual.update(|window, cx| root.update(cx, |this, cx| implicit_reruns(this, tab, window, cx)));
    settle(visual);
    assert!(engine.requests().is_empty(), "the delete never reached the plugin");
    assert!(visual.debug_bounds("shell-prompt-body").is_none(), "and no install prompt opened for it");
    // The find on screen did re-run (the Limit of 50 took effect); the delete did not.
    assert_eq!(script_rows(&app, visual), 50, "the grid shows the find, re-run with the new limit");
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        assert_eq!(runtime.last_executed_query.as_deref(), Some(shown.as_str()));
        assert!(!matches!(runtime.query.status(), crate::query::QueryStatus::Failed(_)));
    });
    // What re-ran is the find that was on screen, not the editor's text.
    assert!(script_text(&app, visual).contains("deleteMany"));
    // Run itself asks for the plugin (the delete is a shell script): the explicit way works.
    click_debug_button(visual, "run-query");
    settle(visual);
    assert!(visual.debug_bounds("shell-prompt-body").is_some(), "Run reaches the shell gate");
}

#[gpui::test]
fn an_unchanged_find_is_still_re_run_by_refresh_and_paging(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    click_debug_button(visual, "run-query");
    settle(visual);
    let tab = app.read_with(visual, |this, _| this.active_id());
    // A refresh re-runs what was executed: a failed or cancelled state would show.
    app.update(visual, |this, cx| {
        this.workspace.runtime_mut(tab).unwrap().query.cancel();
        this.run_tab_query(tab, cx);
    });
    settle(visual);
    assert_eq!(script_rows(&app, visual), 200);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.runtime(tab).unwrap().query.status(), &crate::query::QueryStatus::Ready { rows: 200 });
        assert!(this.view(tab).detail_ui.error.is_none());
    });
}
