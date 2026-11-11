// Driver-route SQL (connect + run) proofs. They load the REAL SQLite driver
// dylib through the real host loader (built on demand into `target/test-drivers`,
// never stripped: `CARGO_PROFILE_RELEASE_STRIP=none`, see the packaging
// strip rule) and drive the real Run / Cancel buttons and the real
// confirmation dialog. The live PostgreSQL / MySQL / MariaDB smoke tests at
// the end are `#[ignore]`d and need `TABLEX_TEST_PG_URI`,
// `TABLEX_TEST_MYSQL_URI` and `TABLEX_TEST_MARIADB_URI` (the dedicated
// `tablex-test-*` containers; nothing else is ever touched).

/// Builds `package` (once per process) and returns its cdylib.
fn built_driver(package: &'static str, features: &'static str) -> std::path::PathBuf {
    static BUILT: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<&'static str, std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    let mut built = BUILT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    built
        .entry(package)
        .or_insert_with(|| {
            let manifest =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rust-engine/Cargo.toml");
            // Under the workspace `target/`, not the OS temp dir: macOS purges
            // old temp files piecemeal, which leaves cargo fingerprints for
            // build outputs that are gone (`bindgen.rs` not found).
            let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-drivers");
            let artifact = target.join("release").join(format!(
                "{}{}{}",
                std::env::consts::DLL_PREFIX,
                package.replace('-', "_"),
                std::env::consts::DLL_SUFFIX
            ));
            // Escape hatch while the engine workspace does not build (another change in flight):
            // reuse the last artifact instead of rebuilding.
            if std::env::var_os("TABLEX_TEST_SKIP_DRIVER_BUILD").is_some() && artifact.exists() {
                return artifact;
            }
            let mut command = std::process::Command::new(env!("CARGO"));
            command
                .args(["build", "--release", "--manifest-path"])
                .arg(&manifest)
                .args(["-p", package, "--target-dir"])
                .arg(&target)
                // Packaging rule: a stripped cdylib can be rejected by dyld.
                .env("CARGO_PROFILE_RELEASE_STRIP", "none");
            if !features.is_empty() {
                command.args(["--features", features]);
            }
            let output = command.output().expect("cargo build driver");
            assert!(
                output.status.success(),
                "{package} build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            target
                .join("release")
                .join(format!(
                    "{}{}{}",
                    std::env::consts::DLL_PREFIX,
                    package.replace('-', "_"),
                    std::env::consts::DLL_SUFFIX
                ))
        })
        .clone()
}

/// The artifact checksum the app verifies on load. Every load in the test
/// process goes through `load_verified`: the host caches handles per path, and
/// a checksum-less first load would make the later verified load fail.
fn driver_sha256(dylib: &std::path::Path) -> String {
    let output = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .arg(dylib)
        .output()
        .expect("shasum");
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

/// A scratch SQLite file; the `sqlite3` CLI seeds and inspects it from
/// outside the driver.
struct SqliteDb(std::path::PathBuf);

impl SqliteDb {
    fn new(seed: &str) -> Self {
        let db = Self(std::env::temp_dir().join(format!("tablex-sql-driver-{}.db", uuid::Uuid::new_v4())));
        db.sql(seed);
        db
    }

    /// `sql` goes in on standard input: Linux caps one argument at 128 KiB, and a seed can be longer.
    fn sql(&self, sql: &str) -> String {
        use std::io::Write as _;
        let mut child = std::process::Command::new("sqlite3")
            .arg(&self.0)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the sqlite3 CLI");
        let mut stdin = child.stdin.take().unwrap();
        let script = sql.to_owned();
        let writer = std::thread::spawn(move || stdin.write_all(script.as_bytes()));
        let output = child.wait_with_output().expect("the sqlite3 CLI");
        writer.join().unwrap().expect("the script reaches sqlite3");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn uri(&self) -> String {
        format!("sqlite://{}", self.0.display())
    }
}

impl Drop for SqliteDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const ITEMS_SEED: &str = "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); \
     INSERT INTO items(name) VALUES ('a'),('b'),('c'),('d'),('e');";
/// Runs far longer than any test waits, unless the driver is cancelled.
const LONG_QUERY: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
     WHERE x < 4000000000) SELECT count(*) FROM c";

/// A copy of the built driver that one rig loads, deleted with the rig. The host caches one
/// `DriverHandle` per path, so every rig on the shared artifact would share its operation count
/// too: a test that ends with an operation still in flight (the rerun after an Apply, whose
/// result nobody collects) would keep every other test's `wait_driver_idle` above 0 for as long
/// as the process lives. A handle of its own makes the count this rig's alone.
struct DriverCopy(std::path::PathBuf);

impl DriverCopy {
    fn of(dylib: &std::path::Path) -> Self {
        let copy = std::env::temp_dir().join(format!(
            "tablex-rig-driver-{}{}",
            uuid::Uuid::new_v4(),
            std::env::consts::DLL_SUFFIX
        ));
        std::fs::copy(dylib, &copy).expect("the rig's own copy of the driver");
        Self(copy)
    }
}

impl Drop for DriverCopy {
    fn drop(&mut self) {
        // Unlinking a loaded library is fine on unix; Windows refuses and the copy stays in temp.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A fixture app whose one tab runs SQL on a live driver session.
struct SqlRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    tab: u64,
    key: String,
    session: std::sync::Arc<engine_driver_host::SharedDriverSession>,
    handle: std::sync::Arc<engine_driver_host::DriverHandle>,
    _driver_copy: DriverCopy,
}

impl<'a> SqlRig<'a> {
    fn new(
        cx: &'a mut TestAppContext,
        kind: ConnectionKind,
        (package, features): (&'static str, &'static str),
        uri: &str,
    ) -> Self {
        let driver_copy = DriverCopy::of(&built_driver(package, features));
        let dylib = driver_copy.0.clone();
        cx.dispatcher.allow_parking();
        // Dialog open animations run on wall-clock time.
        cx.update(|cx| cx.set_reduce_motion(true));
        let (app, visual) = sidebar_test_app(cx);
        visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
        settle(visual);
        let (tab, key, session, handle) = app.update(visual, |this, cx| {
            this.scenario_ui.service = None;
            let id = uuid::Uuid::new_v4();
            this.saved_connections.push(SavedConnection {
                id,
                name: "driver-test".into(),
                uri: uri.into(),
                kind,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            let key = id.to_string();
            let handle = engine_driver_host::DriverLoader
                .load_verified(&dylib, &driver_sha256(&dylib))
                .expect("the driver loads");
            let session = handle
                .connect(&serde_json::json!({ "uri": uri }).to_string())
                .expect("the driver connects");
            let engine_id = session.connection_id();
            // As the connect path records what the connect reply said (the lexer needs the version).
            if let Some(version) = session.server_version() {
                this.driver_runtime.server_versions.insert(key.clone(), version);
            }
            let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
            this.driver_runtime.sessions.insert(key.clone(), session.clone());
            this.driver_runtime.connection_ids.insert(key.clone(), engine_id);
            this.driver_runtime.capabilities.insert(
                key.clone(),
                std::sync::Arc::new(handle.capabilities().clone()),
            );
            this.driver_ui
                .handles
                .insert(kind.driver_id().into(), handle.clone());
            this.sync_navigator_kinds(&key);
            // As the connect path records which driver generation the session connected through.
            this.driver_runtime.session_handles.insert(key.clone(), handle.clone());
            let epoch = this.sessions.begin_connect(&key).unwrap();
            this.sessions.finish_connect(&key, epoch, Ok(()));
            let tab = this.workspace.open_query_tab_with_metadata(
                "SQL",
                TabMetadata {
                    connection_id: key.clone(),
                    ..Default::default()
                },
            );
            this.reset_active_view();
            cx.notify();
            (tab, key, session, handle)
        });
        settle(visual);
        Self {
            app,
            visual,
            tab,
            key,
            session,
            handle,
            _driver_copy: driver_copy,
        }
    }

    fn sqlite(cx: &'a mut TestAppContext, db: &SqliteDb) -> Self {
        Self::new(cx, ConnectionKind::Sqlite, ("dbengine-sqlite", ""), &db.uri())
    }

    /// Sets the tab's statement and runs it (the Run button's own handler). The rig's tab is
    /// brought back first: a test may have opened another tab, which Run would otherwise run.
    fn run(&mut self, sql: &str) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| {
            this.activate_tab_ui(tab, cx);
            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
            this.run_active_tab_query(cx);
        });
    }

    fn status(&self) -> crate::query::QueryStatus {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().query.status().clone()
        })
    }

    fn wait_status(&mut self, label: &'static str, expected: impl Fn(&crate::query::QueryStatus) -> bool) {
        let (app, tab) = (self.app.clone(), self.tab);
        wait_until(self.visual, label, |visual| {
            app.read_with(visual, |this, _| {
                expected(this.workspace.runtime(tab).unwrap().query.status())
            })
        });
        settle(self.visual);
    }

    fn wait_not_running(&mut self, label: &'static str) {
        self.wait_status(label, |status| {
            !matches!(status, crate::query::QueryStatus::Running { .. })
        });
    }

    fn rows(&self) -> usize {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().grid.row_count()
        })
    }

    /// `(page, has_next)` of the tab's pagination.
    fn paging(&self) -> (u64, bool) {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            let pagination = &this.workspace.runtime(tab).unwrap().pagination;
            (pagination.page(), pagination.has_next())
        })
    }

    fn set_page_size(&mut self, size: i64) {
        let tab = self.tab;
        self.app.update(self.visual, |this, _| {
            this.workspace.tab_mut(tab).unwrap().output_limit = size;
            this.workspace.runtime_mut(tab).unwrap().pagination =
                crate::query::PaginationState::new(size);
        });
    }

    /// `(code, severity, retryable)` of every Error Log entry.
    fn log(&self) -> Vec<(Option<String>, crate::error_log::Severity, bool)> {
        self.app.read_with(self.visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .map(|entry| (entry.code.clone(), entry.severity, entry.retryable))
                .collect()
        })
    }

    fn click(&mut self, selector: &'static str) {
        click_debug_button(self.visual, selector);
        settle(self.visual);
    }

    fn wait_rendered(&mut self, selector: &'static str) {
        let mut found = false;
        for _ in 0..200 {
            settle(self.visual);
            if self.visual.debug_bounds(selector).is_some() {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(found, "{selector} is rendered");
    }

    fn operation_registered(&self) -> bool {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.driver_runtime.query_operations.contains_key(&tab)
        })
    }

    /// Waits until the driver has let go of every operation (a cancelled
    /// long query returns to the host only when the driver stopped it).
    fn wait_driver_idle(&mut self) {
        let handle = self.handle.clone();
        wait_until(self.visual, "the driver to finish its operations", |_| {
            handle.active_operation_count() == 0
        });
    }
}

#[gpui::test]
fn every_kind_connects_through_its_driver_and_reports_a_missing_install(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    visual.dispatcher.allow_parking();
    let store_dir = std::env::temp_dir().join(format!("tablex-empty-store-{}", uuid::Uuid::new_v4()));
    let ids = app.update(visual, |this, cx| {
        this.scenario_ui.service = None;
        this.driver_ui.store = std::sync::Arc::new(engine_driver_host::InstalledDriverStore::new(&store_dir));
        // No driver installed: PostgreSQL and SQLite report it like MySQL does.
        let ids = [
            (ConnectionKind::Mysql, "mysql://u@h/db"),
            (ConnectionKind::Postgres, "postgres://u@h/db"),
            (ConnectionKind::Sqlite, "sqlite:///tmp/missing-driver.db"),
        ]
        .map(|(kind, uri)| {
            let id = uuid::Uuid::new_v4();
            this.saved_connections.push(SavedConnection {
                id,
                name: "connect".into(),
                uri: uri.into(),
                kind,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            let key = id.to_string();
            this.ensure_connected_with_uri(key.clone(), Some(uri.into()), None, cx);
            key
        });
        cx.notify();
        ids
    });
    for (key, driver) in ids.into_iter().zip(["mysql", "postgresql", "sqlite"]) {
        wait_until(visual, "the connect attempt to finish", |visual| {
            app.read_with(visual, |this, _| {
                matches!(this.sessions.status(&key), ConnectionStatus::Failed(_))
            })
        });
        app.read_with(visual, |this, _| {
            let ConnectionStatus::Failed(error) = this.sessions.status(&key) else {
                unreachable!()
            };
            assert!(
                error.contains(&format!("Driver '{driver}' is not installed")),
                "{driver} connects through its driver: {error}"
            );
        });
    }
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[gpui::test]
fn an_installed_sqlite_driver_connects_and_runs_sql_through_the_driver_route(
    cx: &mut TestAppContext,
) {
    let dylib = built_driver("dbengine-sqlite", "");
    let db = SqliteDb::new(ITEMS_SEED);
    let (app, visual) = sidebar_test_app(cx);
    visual.dispatcher.allow_parking();
    let store_dir = std::env::temp_dir().join(format!("tablex-sqlite-store-{}", uuid::Uuid::new_v4()));
    let store = engine_driver_host::InstalledDriverStore::new(&store_dir);
    let sha256 = driver_sha256(&dylib);
    store
        .save(engine_driver_host::InstalledDriverRecord {
            id: "sqlite".into(),
            version: "0.0.0-test".into(),
            path: dylib,
            sha256,
            team_id: "TEST".into(),
            installed_at: "2026-10-08T00:00:00Z".into(),
            app_abi_version: 2,
            uri_schemes: vec!["sqlite".into()],
            license: "MIT".into(),
            source_repo: String::new(),
            disabled: false,
            ..Default::default()
        })
        .unwrap();
    let (key, tab) = app.update(visual, |this, cx| {
        this.scenario_ui.service = None;
        this.driver_ui.store = std::sync::Arc::new(store.clone());
        this.driver_ui.installed = drivers::DriverManagerUi::installed_ids(&store);
        let id = uuid::Uuid::new_v4();
        let saved = SavedConnection {
            id,
            name: "installed sqlite".into(),
            uri: db.uri(),
            kind: ConnectionKind::Sqlite,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        };
        this.saved_connections.push(saved);
        let key = id.to_string();
        this.ensure_connected_with_uri(key.clone(), Some(db.uri().into()), None, cx);
        let tab = this.workspace.open_query_tab_with_metadata(
            "SQL",
            TabMetadata {
                connection_id: key.clone(),
                ..Default::default()
            },
        );
        (key, tab)
    });
    wait_until(visual, "the driver connection", |visual| {
        app.read_with(visual, |this, _| {
            !matches!(
                this.sessions.status(&key),
                ConnectionStatus::Connecting | ConnectionStatus::Disconnected
            )
        })
    });
    app.read_with(visual, |this, _| {
        assert!(
            matches!(this.sessions.status(&key), ConnectionStatus::Connected),
            "{:?}",
            this.sessions.status(&key)
        );
        assert!(this.driver_runtime.sessions.contains_key(&key));
    });
    app.update(visual, |this, cx| {
        assert!(this.driver_ui.handles.contains_key("sqlite"), "loaded under the driver id");
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT name FROM items ORDER BY id".into();
        this.run_active_tab_query(cx);
    });
    wait_until(visual, "the SELECT", |visual| {
        app.read_with(visual, |this, _| {
            matches!(
                this.workspace.runtime(tab).unwrap().query.status(),
                crate::query::QueryStatus::Ready { .. }
            )
        })
    });
    assert_eq!(app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().grid.row_count()), 5);
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[gpui::test]
fn run_select_fills_the_grid_and_records_history(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let sql = "SELECT id, name FROM items ORDER BY id";
    rig.run(sql);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    assert!(rig.operation_registered(), "the run owns a cancellable driver operation");
    rig.wait_status("the SELECT", |status| {
        matches!(status, crate::query::QueryStatus::Ready { rows: 5 })
    });

    assert_eq!(rig.rows(), 5);
    assert_eq!(rig.paging(), (0, false), "5 rows fit one page of 200");
    assert!(!rig.operation_registered(), "the finished run released its operation");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    let key = rig.key.clone();
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(
            this.recorded_history,
            vec![workspace::RecordedHistory {
                connection: key,
                database: String::new(),
                collection: String::new(),
                sql: sql.into(),
                rows: 5,
            }],
            "a successful driver run lands in the query history like a native one"
        );
    });
}

#[gpui::test]
fn failed_statement_is_an_error_with_its_driver_code_and_is_not_recorded(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run("SELECT * FROM no_such_table");
    rig.wait_not_running("the failing SELECT");
    let crate::query::QueryStatus::Failed(message) = rig.status() else {
        panic!("expected a failure, got {:?}", rig.status());
    };
    assert!(message.contains("no_such_table"), "{message}");
    assert_eq!(rig.log().len(), 1, "{:?}", rig.log());
    let (_, severity, retryable) = rig.log().remove(0);
    assert_eq!((severity, retryable), (crate::error_log::Severity::Error, false));
    rig.app.read_with(rig.visual, |this, _| assert!(this.recorded_history.is_empty()));
}

#[gpui::test]
fn paging_follows_the_drivers_truncated_flag(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    let sql = "SELECT id FROM items ORDER BY id";
    rig.run(sql);
    rig.wait_status("page 1", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 2 }));
    assert_eq!(rig.paging(), (0, true), "5 rows, 2 per page: truncated");

    for (page, rows, has_next) in [(1, 2, true), (2, 1, false)] {
        rig.app.update(rig.visual, |this, cx| this.next_query_page(cx));
        rig.wait_status("the next page", move |status| {
            matches!(status, crate::query::QueryStatus::Ready { rows: ready } if *ready == rows)
        });
        assert_eq!(rig.paging(), (page, has_next), "page {page}");
        assert_eq!(rig.rows(), rows);
    }
    rig.app.update(rig.visual, |this, cx| this.next_query_page(cx));
    settle(rig.visual);
    assert_eq!(rig.paging(), (2, false), "the last page has no next page");

    // A different statement starts from its first page: the driver would
    // refuse `offset > 0` for it.
    rig.run("SELECT name FROM items ORDER BY id");
    rig.wait_status("a new statement", |status| {
        matches!(status, crate::query::QueryStatus::Ready { rows: 2 })
    });
    assert_eq!(rig.paging(), (0, true));
}

#[gpui::test]
fn a_non_pageable_statement_offers_no_next_page_and_runs_once(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    // More rows than a page, but it is a write: the extras are dropped and no
    // next page is offered.
    rig.run("INSERT INTO items(name) VALUES ('x'),('y'),('z') RETURNING id");
    rig.wait_not_running("the INSERT ... RETURNING");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "{:?}", rig.status());
    assert_eq!(rig.rows(), 2, "one page of the 3 returned rows");
    assert_eq!(rig.paging(), (0, false), "truncated = false: never offered as a page");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "8", "executed exactly once");

    rig.app.update(rig.visual, |this, cx| this.next_query_page(cx));
    settle(rig.visual);
    assert_eq!(rig.paging(), (0, false));
    assert_eq!(db.sql("SELECT count(*) FROM items"), "8", "Next never re-runs the statement");
}

#[gpui::test]
fn a_not_pageable_refusal_keeps_the_tab_healthy_and_never_runs_the_statement(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let sql = "INSERT INTO items(name) VALUES ('p') RETURNING id";
    // The tab believes page 2 of this statement exists (a stale paging state).
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, _| {
        let runtime = this.workspace.runtime_mut(tab).unwrap();
        runtime.last_executed_query = Some(sql.into());
        runtime.pagination.set_driver_page(None, false, true);
    });
    rig.app.update(rig.visual, |this, cx| this.next_query_page(cx));
    rig.wait_not_running("the refused request");

    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "not an error: the tab is as before");
    assert_eq!(rig.paging(), (0, false), "paging is off for this result");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "the refused statement never ran");
    let log = rig.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].0.as_deref(), Some("query.not_pageable"));
    assert_eq!(log[0].1, crate::error_log::Severity::Warning);
}

#[gpui::test]
fn dangerous_statement_asks_first_cancel_keeps_the_table_confirm_runs_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run("DELETE FROM items");
    rig.wait_rendered("sql-confirm-run");
    assert!(rig.visual.debug_bounds("sql-confirm-reason").is_some(), "the driver's reason is shown");
    assert!(rig.visual.debug_bounds("sql-confirm-statement").is_some(), "so is the statement");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "the run has not started");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Cancel: the tab is idle again, the table intact, nothing in the log.
    rig.click("sql-confirm-cancel");
    assert!(rig.visual.debug_bounds("sql-confirm-run").is_none(), "the dialog closed");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert!(!rig.operation_registered());

    // Run again and confirm: the same statement is resent with `confirmed`.
    rig.run("DELETE FROM items");
    rig.wait_rendered("sql-confirm-run");
    rig.click("sql-confirm-run");
    rig.wait_status("the confirmed DELETE", |status| {
        matches!(status, crate::query::QueryStatus::Ready { .. })
    });
    assert_eq!(db.sql("SELECT count(*) FROM items"), "0");
    assert!(rig.visual.debug_bounds("sql-confirm-run").is_none());
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

#[gpui::test]
fn the_run_button_cancels_a_running_driver_query(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run(LONG_QUERY);
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    // The same button reads Cancel while a run is in flight.
    assert!(rig.visual.debug_bounds("run-query").is_some());
    rig.click("run-query");

    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert!(!rig.operation_registered());
    // The driver really stopped (an uncancelled 4e9-row count never would
    // within the wait), and nothing was logged as a failure.
    rig.wait_driver_idle();
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
}

#[gpui::test]
fn a_driver_side_cancel_is_a_cancelled_status_not_an_error(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run(LONG_QUERY);
    // Let the statement reach the driver before telling it to stop.
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    let tab = rig.tab;
    let (operation_id, connection_id) = rig.app.read_with(rig.visual, |this, _| {
        let operation = &this.driver_runtime.query_operations[&tab];
        (operation.operation_id, operation.connection_id)
    });
    // Not through the UI: the driver is told directly, so the run itself ends
    // with `query.cancelled`.
    rig.handle
        .cancel_operation(connection_id, &engine_driver_api::CancelRequest { operation_id })
        .expect("cancel");
    rig.wait_status("the cancelled run", |status| {
        matches!(status, crate::query::QueryStatus::Cancelled)
    });
    assert!(rig.log().is_empty(), "a cancel is not an Error Log entry: {:?}", rig.log());
    assert!(!rig.operation_registered());
}

#[gpui::test]
fn a_long_query_never_holds_the_session_lock(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run(LONG_QUERY);
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }), "still running");

    // A navigator call takes the session lock and lists databases while the
    // statement is running on the driver.
    let started = std::time::Instant::now();
    let nodes = rig
        .session
        .lock()
        .expect("the session lock is free")
        .list_nav_nodes(
            &serde_json::json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 0})
                .to_string(),
        )
        .expect("the navigator answers while a query runs");
    assert!(!nodes.nodes.is_empty());
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }), "the query was running throughout");
    assert!(rig.session.try_lock().is_ok());

    rig.click("run-query");
    rig.wait_driver_idle();
}

#[gpui::test]
fn a_dropped_session_flips_the_connection_to_its_reconnect_state(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.session.lock().unwrap().disconnect().expect("disconnect");
    // A write: a read would be reconnected and retried (see `recovery.rs`), a write is only shown.
    rig.run("DELETE FROM items WHERE id < 0");
    rig.wait_not_running("the run on a dropped session");
    settle(rig.visual);

    let key = rig.key.clone();
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(
            this.sessions.status(&key),
            ConnectionStatus::Failed(crate::sessions::CONNECTION_LOST_REASON.into())
        );
    });
    assert!(matches!(
        lost_state(&rig.app, rig.visual),
        Some(ResultConnectionState::Failed(_))
    ));
    assert!(rig.visual.debug_bounds("connection-lost-reconnect").is_some(), "the Reconnect action is offered");
    let log = rig.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].0.as_deref(), Some("session.not_connected"));
}

#[gpui::test]
fn driver_error_codes_map_to_status_and_error_log(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let rejected = |code: &str, retryable| {
        DriverQueryError::Driver(engine_driver_host::LoaderError::DriverRejected {
            code: code.into(),
            message: "driver says no".into(),
            retryable,
        })
    };
    let (tab, key) = (rig.tab, rig.key.clone());
    for (code, retryable, hint) in [
        ("query.interrupted", true, true),
        ("query.reply_too_large", false, false),
    ] {
        rig.app.update(rig.visual, |this, _| {
            let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
            assert!(this.query_tasks.registry.register(tab, run));
            this.apply_driver_arrow_query_result(tab, run, key.clone(), Err(rejected(code, retryable)));
        });
        let crate::query::QueryStatus::Failed(message) = rig.status() else {
            panic!("{code}: expected a failure");
        };
        assert_eq!(message.contains("run it again"), hint, "{code}: {message}");
        assert!(message.contains("driver says no"), "{message}");
    }
    let log = rig.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(
        log[0],
        (Some("query.interrupted".into()), crate::error_log::Severity::Warning, true),
        "a retryable interrupt is a retryable warning"
    );
    assert_eq!(
        log[1],
        (Some("query.reply_too_large".into()), crate::error_log::Severity::Error, false)
    );
    // Neither is a lost connection.
    rig.app.read_with(rig.visual, |this, _| {
        assert!(matches!(this.sessions.status(&key), ConnectionStatus::Connected));
    });
}

#[gpui::test]
fn a_driver_without_sql_support_fails_the_run_cleanly(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.capabilities.insert(
            key.clone(),
            std::sync::Arc::new(engine_driver_api::DriverCapabilities::new(
                "sqlite",
                "SQLite",
                vec!["navigator".to_string()],
            )),
        );
    });
    rig.run("SELECT 1");
    rig.wait_not_running("the unsupported run");
    let crate::query::QueryStatus::Failed(message) = rig.status() else {
        panic!("expected a failure");
    };
    assert!(message.contains("does not support SQL queries"), "{message}");
    assert!(!rig.operation_registered());
}

/// One live smoke run per SQL server: SELECT, then a dangerous statement that
/// is confirmed-or-cancelled before it can touch anything (the table does not
/// exist).
fn live_smoke(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    rig.run("SELECT 1 AS one");
    rig.wait_status("SELECT 1", |status| {
        matches!(status, crate::query::QueryStatus::Ready { rows: 1 })
    });
    assert_eq!(rig.rows(), 1);
    assert_eq!(rig.paging(), (0, false));

    rig.run("DELETE FROM tablex_smoke_never_created");
    rig.wait_rendered("sql-confirm-run");
    // The tab keeps the SELECT result it showed before the prompt.
    assert_eq!(rig.status(), crate::query::QueryStatus::Ready { rows: 1 });
    rig.click("sql-confirm-cancel");
    assert_eq!(rig.status(), crate::query::QueryStatus::Ready { rows: 1 });
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert!(rig.visual.debug_bounds("sql-confirm-run").is_none());
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_driver_route_smoke(cx: &mut TestAppContext) {
    live_smoke(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

/// Runs `sql` on the rig's session outside the app's checks (setup and inspection of a live
/// test); returns the row count of its page.
fn raw_sql(rig: &SqlRig, sql: &str) -> u64 {
    let runner = rig.session.query_runner().expect("a runner");
    let request = serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "sql".into(),
        operation_id: runner.operation_id(),
        database: None,
        object: None,
        offset: 0,
        limit: 100,
        payload: serde_json::json!({"sql": sql, "confirmed": true}),
    })
    .unwrap();
    let page = runner.open_query(&request).unwrap_or_else(|error| panic!("{sql}: {error}"));
    runner.release_page(page.page_id);
    page.row_count as u64
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_read_only_connection_refuses_a_write_behind_a_carriage_return_comment(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let table = format!("tablex_ro_probe_{}", uuid::Uuid::new_v4().to_simple());
    raw_sql(&rig, &format!("CREATE TABLE {table}(id int PRIMARY KEY)"));
    raw_sql(&rig, &format!("INSERT INTO {table} VALUES (1)"));
    let app = rig.app.clone();
    publish_types(&app, rig.visual, false);
    rig.give_profile(typed("replica"));
    // PostgreSQL ends `-- x` at the `\r`: this is a SELECT and a DELETE.
    let sql = format!("SELECT 2 -- x\r; DELETE FROM {table} WHERE id = 1");

    // The app's own check splits it the way the server does and refuses it before the driver.
    rig.run(&sql);
    settle(rig.visual);
    let checked = rig.status();
    let reached_driver = rig.operation_registered();
    let rows_after_check = raw_sql(&rig, &format!("SELECT * FROM {table}"));

    // Below the check (`run_sql`), the driver's read-only run refuses it.
    let tab = rig.tab;
    app.update(rig.visual, |this, cx| this.run_sql(tab, sql.clone(), None, cx));
    rig.wait_status("the refused script", |status| matches!(status, crate::query::QueryStatus::Failed(_)));
    let driver_refused = rig.status();
    let log = rig.log();
    let rows_after_driver = raw_sql(&rig, &format!("SELECT * FROM {table}"));
    raw_sql(&rig, &format!("DROP TABLE {table}"));

    assert!(
        matches!(&checked, crate::query::QueryStatus::Failed(message) if message.contains("DELETE") && message.contains("read-only")),
        "{checked:?}"
    );
    assert!(!reached_driver, "the app's check refused it");
    assert_eq!(rows_after_check, 1, "the row survived the app's check");
    assert!(
        matches!(&driver_refused, crate::query::QueryStatus::Failed(message) if message.contains("read-only")),
        "{driver_refused:?}"
    );
    assert_eq!(log.last().and_then(|entry| entry.0.as_deref()), Some("query.read_only_violation"), "{log:?}");
    assert_eq!(rows_after_driver, 1, "the row survived the driver's read-only run");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_driver_route_smoke(cx: &mut TestAppContext) {
    live_smoke(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_driver_route_smoke(cx: &mut TestAppContext) {
    live_smoke(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

#[gpui::test]
fn a_cancel_that_beats_the_query_to_the_driver_is_still_a_cancel(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run(LONG_QUERY);
    // No scheduler turn yet: the request has not reached the driver, which
    // remembers the early cancel and refuses the statement.
    let tab = rig.tab;
    let (operation_id, connection_id) = rig.app.read_with(rig.visual, |this, _| {
        let operation = &this.driver_runtime.query_operations[&tab];
        (operation.operation_id, operation.connection_id)
    });
    rig.handle
        .cancel_operation(connection_id, &engine_driver_api::CancelRequest { operation_id })
        .expect("cancel");
    rig.wait_not_running("the refused statement");
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

/// An app with the real SQLite driver *installed* (not loaded) and a SQLite
/// connection with no session yet: the state of a restored tab on a connection
/// the user has not opened.
struct ColdRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    tab: u64,
    key: String,
    store_dir: std::path::PathBuf,
}

impl<'a> ColdRig<'a> {
    fn new(cx: &'a mut TestAppContext, uri: &str) -> Self {
        let dylib = built_driver("dbengine-sqlite", "");
        let (app, visual) = sidebar_test_app(cx);
        visual.dispatcher.allow_parking();
        visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
        settle(visual);
        let store_dir = std::env::temp_dir().join(format!("tablex-cold-store-{}", uuid::Uuid::new_v4()));
        let store = engine_driver_host::InstalledDriverStore::new(&store_dir);
        store
            .save(engine_driver_host::InstalledDriverRecord {
                id: "sqlite".into(),
                version: "0.0.0-test".into(),
                sha256: driver_sha256(&dylib),
                path: dylib,
                team_id: "TEST".into(),
                installed_at: "2026-10-08T00:00:00Z".into(),
                app_abi_version: 2,
                uri_schemes: vec!["sqlite".into()],
                license: "MIT".into(),
                source_repo: String::new(),
                disabled: false,
                ..Default::default()
            })
            .unwrap();
        let (tab, key) = app.update(visual, |this, cx| {
            this.scenario_ui.service = None;
            this.driver_ui.store = std::sync::Arc::new(store.clone());
            this.driver_ui.installed = drivers::DriverManagerUi::installed_ids(&store);
            let id = uuid::Uuid::new_v4();
            this.saved_connections.push(SavedConnection {
                id,
                name: "cold sqlite".into(),
                uri: uri.into(),
                kind: ConnectionKind::Sqlite,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            let key = id.to_string();
            let tab = this.workspace.open_query_tab_with_metadata(
                "SQL",
                TabMetadata {
                    connection_id: key.clone(),
                    ..Default::default()
                },
            );
            this.reset_active_view();
            cx.notify();
            (tab, key)
        });
        settle(visual);
        Self { app, visual, tab, key, store_dir }
    }

    fn run(&mut self, sql: &str) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| {
            this.activate_tab_ui(tab, cx);
            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
            this.run_active_tab_query(cx);
        });
    }

    fn status(&self) -> crate::query::QueryStatus {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().query.status().clone()
        })
    }

    fn pending(&self) -> bool {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| this.query_tasks.pending_runs.contains_key(&tab))
    }

    fn connected(&self) -> bool {
        let key = self.key.clone();
        self.app.read_with(self.visual, |this, _| {
            matches!(this.sessions.status(&key), ConnectionStatus::Connected)
                && this.driver_runtime.sessions.contains_key(&key)
        })
    }

    fn wait_settled(&mut self, label: &'static str) {
        let (app, tab) = (self.app.clone(), self.tab);
        wait_until(self.visual, label, |visual| {
            app.read_with(visual, |this, _| {
                !matches!(
                    this.workspace.runtime(tab).unwrap().query.status(),
                    crate::query::QueryStatus::Running { .. }
                )
            })
        });
        settle(self.visual);
    }

    fn log(&self) -> Vec<(crate::error_log::Area, Option<String>)> {
        self.app.read_with(self.visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .map(|entry| (entry.area, entry.code.clone()))
                .collect()
        })
    }
}

impl Drop for ColdRig<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.store_dir);
    }
}

#[gpui::test]
fn run_on_an_unconnected_driver_connection_connects_then_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = ColdRig::new(cx, &db.uri());
    assert!(!rig.connected());
    rig.run("SELECT id FROM items ORDER BY id");
    // One Run: the tab is running, the statement waits for the connection.
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    assert!(rig.pending(), "queued behind the connect");
    rig.wait_settled("connect then run");

    assert!(rig.connected());
    assert!(!rig.pending());
    assert_eq!(rig.status(), crate::query::QueryStatus::Ready { rows: 5 });
    let tab = rig.tab;
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().grid.row_count()), 5);
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

#[gpui::test]
fn run_after_the_session_was_lost_reconnects_then_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = ColdRig::new(cx, &db.uri());
    rig.run("SELECT 1");
    rig.wait_settled("the first run");
    assert!(rig.connected());
    // The driver reports the session gone: every tab flips to the reconnect state.
    let (tab, key) = (rig.tab, rig.key.clone());
    rig.app.update(rig.visual, |this, _| {
        let session = this.driver_runtime.sessions[&key].clone();
        session.lock().unwrap().disconnect().unwrap();
        let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        this.apply_driver_arrow_query_result(
            tab,
            run,
            key.clone(),
            Err(DriverQueryError::Driver(engine_driver_host::LoaderError::DriverRejected {
                code: "query.not_connected".into(),
                message: "gone".into(),
                retryable: false,
            })),
        );
    });
    assert!(!rig.connected());
    rig.run("SELECT id FROM items");
    assert!(rig.pending(), "reconnecting first");
    rig.wait_settled("reconnect then run");
    assert!(rig.connected());
    assert_eq!(rig.status(), crate::query::QueryStatus::Ready { rows: 5 });
}

#[gpui::test]
fn a_connect_failure_fails_the_queued_run_cleanly(cx: &mut TestAppContext) {
    let mut rig = ColdRig::new(cx, "sqlite:///tablex-no-such-dir-8f3a/none/db.sqlite");
    rig.run("SELECT 1");
    assert!(rig.pending());
    rig.wait_settled("the failed connect");

    let crate::query::QueryStatus::Failed(message) = rig.status() else {
        panic!("expected the run to fail, got {:?}", rig.status());
    };
    assert!(!message.is_empty());
    assert!(!rig.pending() && !rig.connected());
    let areas: Vec<_> = rig.log().into_iter().map(|(area, _)| area).collect();
    assert!(areas.contains(&crate::error_log::Area::Query), "tab error is logged: {areas:?}");
    assert!(areas.contains(&crate::error_log::Area::Connect), "so is the connect failure: {areas:?}");
    // A later Run is a fresh attempt, not blocked by the dead one.
    rig.run("SELECT 2");
    assert!(rig.pending());
}

#[gpui::test]
fn cancel_and_close_supersede_a_queued_run(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = ColdRig::new(cx, &db.uri());
    // Cancel (the Run button's handler) before the connection opened: no
    // scheduler turn happens in between.
    rig.run("DELETE FROM items WHERE id > 0");
    assert!(rig.pending());
    rig.app.update(rig.visual, |this, cx| this.cancel_active_tab_query(cx));
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert!(!rig.pending());
    // The connection still opens, but the cancelled statement never runs.
    let key = rig.key.clone();
    let app = rig.app.clone();
    wait_until(rig.visual, "the connection", |visual| {
        app.read_with(visual, |this, _| matches!(this.sessions.status(&key), ConnectionStatus::Connected))
    });
    settle(rig.visual);
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Closing the tab drops a queued run too.
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        let key = this.workspace.tab(tab).unwrap().connection_id.clone();
        this.disconnect_connection(&key, cx);
    });
    rig.run("SELECT 1");
    assert!(rig.pending(), "{:?}", rig.status());
    rig.app.update(rig.visual, |this, cx| this.close_tab(tab, cx));
    assert!(!rig.pending());
}

#[gpui::test]
fn disabling_a_driver_removes_it_at_once_and_enabling_restores_it(cx: &mut TestAppContext) {
    // Not the network: the catalog refresh after the toggle reads this file.
    std::env::set_var("TABLEX_DRIVER_MANIFEST_URL", "file:///tablex-no-such-manifest.json");
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = ColdRig::new(cx, &db.uri());
    let dylib = built_driver("dbengine-sqlite", "");
    let has_driver = |rig: &ColdRig<'_>| {
        rig.app.read_with(rig.visual, |this, _| this.driver_ui.has_driver("sqlite"))
    };
    let toggle = |rig: &mut ColdRig<'_>, disabled: bool| {
        rig.app.update(rig.visual, |this, cx| this.set_driver_disabled("sqlite".into(), disabled, cx));
        let app = rig.app.clone();
        wait_until(rig.visual, "the toggle", |visual| {
            app.read_with(visual, |this, _| this.driver_ui.op_task.is_none())
        });
        settle(rig.visual);
    };
    // The driver is loaded (as after a first connection).
    rig.app.update(rig.visual, |this, _| {
        let handle = engine_driver_host::DriverLoader
            .load_verified(&dylib, &driver_sha256(&dylib))
            .unwrap();
        this.driver_ui.handles.insert("sqlite".into(), handle);
    });
    assert!(has_driver(&rig));

    toggle(&mut rig, true);
    assert!(!has_driver(&rig), "disabled: the loaded handle is gone");
    rig.app.read_with(rig.visual, |this, _| {
        assert!(!this.driver_ui.handles.contains_key("sqlite"), "handle evicted");
        assert!(!this.driver_ui.installed.contains("sqlite"));
    });

    toggle(&mut rig, false);
    assert!(has_driver(&rig), "enabled again (installed, not yet loaded)");

    // A driver with a live session is not disabled underneath it: the menu
    // asks first, and Cancel leaves everything as it was.
    rig.run("SELECT 1");
    rig.wait_settled("connect");
    assert!(rig.connected());
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| {
            this.request_driver_removal("sqlite".into(), false, window, cx);
        });
    });
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("driver-confirm-body").is_some(), "the confirm dialog lists the connections");
    click_debug_button(rig.visual, "driver-confirm-cancel");
    settle(rig.visual);
    assert!(has_driver(&rig));
    let record = engine_driver_host::InstalledDriverStore::new(&rig.store_dir).record("sqlite").unwrap().unwrap();
    assert!(!record.disabled, "cancelled: the store still has it enabled");
}
