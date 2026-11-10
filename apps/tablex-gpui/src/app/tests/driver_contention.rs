// A busy driver session is not an unsupported one: navigator calls hold the
// shared session lock during network I/O, and Run must never report that as a
// missing capability. Uses the real host loader with the host crate's mock
// driver dylib (built on demand into a private target dir).

fn mock_driver_dylib() -> std::path::PathBuf {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../rust-engine/crates/engine-driver-host/tests/fixtures/mock-driver/Cargo.toml",
            );
            let target = std::env::temp_dir().join("tablex-gpui-mock-driver-target");
            let output = std::process::Command::new(env!("CARGO"))
                .arg("build")
                .arg("--manifest-path")
                .arg(&manifest)
                .arg("--release")
                .arg("--target-dir")
                .arg(&target)
                .output()
                .expect("cargo build mock-driver");
            assert!(
                output.status.success(),
                "mock-driver build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
            target.join("release").join(format!("{prefix}mock_driver{suffix}"))
        })
        .clone()
}

/// A Mongo script tab (scenario-created) whose connection is then backed by a
/// real host driver session. `features` are the capabilities cached for it,
/// as the connect path does. Returns the tab and the shared session.
fn script_tab_on_real_session<'a>(
    cx: &'a mut TestAppContext,
    features: &[&str],
) -> (
    gpui::Entity<AppRoot>,
    &'a mut gpui::VisualTestContext,
    u64,
    std::sync::Arc<engine_driver_host::SharedDriverSession>,
) {
    let (app, visual) = script_app(cx);
    let tab = active_tab(&app, visual);
    let connection = crate::ui_fixture::MONGO_ID;
    let session = app.update(visual, |this, _| {
        this.scenario_ui.service = None;
        let handle = engine_driver_host::DriverLoader
            .load(&mock_driver_dylib())
            .expect("mock driver loads");
        let session = handle
            .connect(r#"{"uri":"mock://local","name":"test"}"#)
            .expect("mock session");
        let engine_id = session.connection_id();
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        this.driver_runtime.sessions.insert(connection.into(), session.clone());
        this.driver_runtime.connection_ids.insert(connection.into(), engine_id);
        this.driver_runtime.capabilities.insert(
            connection.into(),
            std::sync::Arc::new(engine_driver_api::DriverCapabilities::new(
                "mongodb",
                "MongoDB",
                features.iter().map(|feature| feature.to_string()).collect(),
            )),
        );
        // The mock handle stands in for the Mongo driver handle.
        this.driver_ui.handles.insert("mongodb".into(), handle);
        session
    });
    (app, visual, tab, session)
}

fn query_failure(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
    tab: u64,
) -> Option<String> {
    app.read_with(visual, |this, _| match this.workspace.runtime(tab).unwrap().query.status() {
        crate::query::QueryStatus::Failed(error) => Some(error.clone()),
        _ => None,
    })
}

#[gpui::test]
fn run_is_not_rejected_while_another_thread_holds_the_driver_session_lock(cx: &mut TestAppContext) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "query.mongo_script"]);
    visual.dispatcher.allow_parking();

    // A navigator-style call holds the session lock (network I/O) while the
    // user presses Run.
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let holder = {
        let session = session.clone();
        std::thread::spawn(move || {
            let guard = session.lock().expect("session lock");
            locked_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(guard);
        })
    };
    locked_rx.recv().expect("lock held");
    assert!(session.try_lock().is_err(), "the session really is busy");

    click_debug_button(visual, "run-query");
    holder.join().unwrap();
    wait_until(visual, "the script run to finish", |visual| {
        app.read_with(visual, |this, _| {
            !matches!(
                this.workspace.runtime(tab).unwrap().query.status(),
                crate::query::QueryStatus::Running { .. } | crate::query::QueryStatus::Idle
            )
        })
    });
    let failure = query_failure(&app, visual, tab);
    assert!(
        failure.as_deref().is_none_or(|error| !error.contains("does not support") && !error.contains("busy")),
        "contention must not surface as a capability error, got {failure:?}"
    );
    app.read_with(visual, |this, _| {
        assert!(
            this.workspace.runtime(tab).unwrap().last_executed_query.is_some(),
            "the run was dispatched to the driver"
        );
    });
}

#[gpui::test]
fn a_driver_without_script_support_gets_the_does_not_support_message(cx: &mut TestAppContext) {
    let (app, visual, tab, _session) =
        script_tab_on_real_session(cx, &["navigator", "query.find"]);
    click_debug_button(visual, "run-query");
    settle(visual);
    assert_eq!(
        query_failure(&app, visual, tab).as_deref(),
        Some("The installed MongoDB driver does not support script queries; update it in the Driver Manager")
    );
}

#[gpui::test]
fn the_outdated_driver_message_names_the_installed_version_and_where_to_update(cx: &mut TestAppContext) {
    let (app, visual, tab, _session) =
        script_tab_on_real_session(cx, &["navigator", "query.mongo_find", "bson_rendering"]);
    app.update(visual, |this, _| {
        this.driver_ui.records.insert(
            "mongodb".into(),
            engine_driver_host::InstalledDriverRecord {
                id: "mongodb".into(),
                version: "0.1.0".into(),
                ..Default::default()
            },
        );
    });
    click_debug_button(visual, "run-query");
    settle(visual);
    assert_eq!(
        query_failure(&app, visual, tab).as_deref(),
        Some("The installed MongoDB driver (0.1.0) does not support script queries; update it in the Driver Manager")
    );
}

#[gpui::test]
fn the_new_query_menu_entry_does_not_depend_on_the_session_lock(cx: &mut TestAppContext) {
    let (app, visual, _tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "query.mongo_script"]);
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let session = session.clone();
        std::thread::spawn(move || {
            let _guard = session.lock().expect("session lock");
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        })
    };
    locked_rx.recv().unwrap();
    app.read_with(visual, |this, _| {
        assert!(this.mongo_supports_script(crate::ui_fixture::MONGO_ID));
        assert!(!this.mongo_supports_script("not-connected"));
    });
    release_tx.send(()).unwrap();
    holder.join().unwrap();
}

/// Holds the session lock on another thread until `release` fires (or a safety
/// timeout, so a regression fails the assertions instead of hanging the run).
struct SessionLockHolder {
    release: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SessionLockHolder {
    fn hold(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>) -> Self {
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let session = session.clone();
        let thread = std::thread::spawn(move || {
            let _guard = session.lock().expect("session lock");
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(20));
        });
        locked_rx.recv().expect("lock held");
        Self {
            release,
            thread: Some(thread),
        }
    }

    fn release(mut self) {
        let _ = self.release.send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[gpui::test]
fn run_never_blocks_the_ui_thread_on_a_busy_session(cx: &mut TestAppContext) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "query.mongo_script"]);
    visual.dispatcher.allow_parking();
    let holder = SessionLockHolder::hold(&session);
    assert!(session.try_lock().is_err(), "the session really is busy");

    // The click itself must return at once: UI-thread work may not wait for
    // the lock that the holder keeps for up to four seconds.
    // The Run handler is invoked directly: the test harness's click helpers
    // park until background work finishes, which would hide a blocked UI.
    assert!(visual.debug_bounds("run-query").is_some(), "Run is rendered");
    let started = std::time::Instant::now();
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    let blocked_for = started.elapsed();
    assert!(
        blocked_for < std::time::Duration::from_secs(2),
        "Run blocked the UI thread for {blocked_for:?} on the session lock"
    );
    app.read_with(visual, |this, _| {
        assert!(
            matches!(
                this.workspace.runtime(tab).unwrap().query.status(),
                crate::query::QueryStatus::Running { .. }
            ),
            "the run is in flight while the session is busy"
        );
        let operation = this
            .driver_runtime
            .query_operations
            .get(&tab)
            .expect("the run owns a cancellable driver operation");
        assert_ne!(
            operation.operation_id.0, 0,
            "the request carries a host-allocated id"
        );
    });

    holder.release();
    wait_until(visual, "the run to finish", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().driver_page_id.is_some()
        })
    });
    assert_eq!(query_failure(&app, visual, tab), None);
}

#[gpui::test]
fn row_inspector_neither_blocks_nor_fails_on_a_busy_session(cx: &mut TestAppContext) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "query.mongo_script"]);
    visual.dispatcher.allow_parking();
    click_debug_button(visual, "run-query");
    wait_until(visual, "the run to finish", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().driver_page_id.is_some()
        })
    });

    let holder = SessionLockHolder::hold(&session);
    let started = std::time::Instant::now();
    app.update(visual, |this, cx| this.select_mongo_document(tab, 0, cx));
    let blocked_for = started.elapsed();
    assert!(
        blocked_for < std::time::Duration::from_secs(2),
        "opening the row inspector blocked the UI thread for {blocked_for:?}"
    );
    // The synchronous reader used by edit / copy handlers never calls the
    // driver: it serves the per-selection cache, which the frame fills in the
    // background even while the session is busy.
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab).unwrap().query_mode = Some(QueryMode::MongoFind);
    });
    wait_until(visual, "the selection's row fields", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.mongo_row_fields(tab, 0).is_ok())
    });
    // The snapshot arrives while the session is still held elsewhere.
    wait_until(visual, "the inspector snapshot", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().mongo.inspector.snapshot.is_some()
        })
    });
    assert!(session.try_lock().is_err(), "the session was busy throughout");
    app.read_with(visual, |this, _| {
        let inspector = &this.workspace.runtime(tab).unwrap().mongo.inspector;
        assert_eq!(inspector.error, None, "never a \"Driver session is busy\" error");
        assert!(!inspector.loading);
    });
    holder.release();
}

/// The driver can drop a page behind the app's back (another tab's commit
/// invalidates every page of the connection; a reconnect clears them) while
/// the grid keeps showing the rows it already holds. Opening a document then
/// must not surface the raw driver error: the tab re-runs for a current page.
#[gpui::test]
fn row_inspector_refreshes_a_page_the_driver_no_longer_holds(cx: &mut TestAppContext) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "query.mongo_script"]);
    visual.dispatcher.allow_parking();
    click_debug_button(visual, "run-query");
    wait_until(visual, "the run to finish", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().driver_page_id.is_some()
        })
    });
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab).unwrap().query_mode = Some(QueryMode::MongoFind);
    });
    let stale = app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().driver_page_id.unwrap());
    session.lock().expect("session lock").release_page(stale).expect("release behind the app");

    app.update(visual, |this, cx| this.select_mongo_document(tab, 0, cx));
    wait_until(visual, "the inspection to settle", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| {
            let runtime = this.workspace.runtime(tab).unwrap();
            runtime.mongo.inspector.error.is_some()
                || (runtime.driver_page_id.is_some_and(|page| page != stale)
                    && runtime.mongo.inspector.snapshot.is_some())
        })
    });
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        assert_eq!(runtime.mongo.inspector.error, None, "no raw driver error for a stale page");
        assert!(runtime.driver_page_id.is_some_and(|page| page != stale), "the tab re-ran for a current page");
        assert!(runtime.mongo.inspector.snapshot.is_some());
        assert!(!runtime.mongo.inspector.loading);
    });
}

/// A page that is stale again right after a refresh (nothing changed since)
/// must report instead of re-running forever.
#[gpui::test]
fn a_stale_page_after_a_refresh_reports_instead_of_looping(cx: &mut TestAppContext) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "query.mongo_script"]);
    visual.dispatcher.allow_parking();
    click_debug_button(visual, "run-query");
    wait_until(visual, "the run to finish", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().driver_page_id.is_some()
        })
    });
    let stale = app.update(visual, |this, _| {
        this.workspace.tab_mut(tab).unwrap().query_mode = Some(QueryMode::MongoFind);
        let runtime = this.workspace.runtime_mut(tab).unwrap();
        runtime.mongo.stale_refresh = true;
        runtime.driver_page_id.unwrap()
    });
    session.lock().expect("session lock").release_page(stale).expect("release behind the app");

    app.update(visual, |this, cx| this.select_mongo_document(tab, 0, cx));
    wait_until(visual, "the inspection to settle", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().mongo.inspector.error.is_some()
        })
    });
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        let error = runtime.mongo.inspector.error.as_deref().unwrap();
        assert!(error.contains("out of date") && !error.contains("driver rejected"), "{error}");
        assert_eq!(runtime.driver_page_id, Some(stale), "no second run was started");
    });
}

/// Mongo find tabs on the mock session. The first is the script tab of
/// `script_tab_on_real_session` turned into a find on `orders`; the mock drops,
/// for a write to database `scoped`, only the pages of the written collection
/// (like the MongoDB driver). Returns (issuer, other tab ids by collection).
fn scoped_find_tabs(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab: u64,
    others: &[&str],
) -> Vec<u64> {
    let find = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64| {
        app.update(visual, |this, cx| this.run_tab_query(tab, cx));
        wait_until(visual, "the find to finish", |visual| {
            settle(visual);
            app.read_with(visual, |this, _| {
                this.workspace.runtime(tab).unwrap().driver_page_id.is_some()
            })
        });
    };
    app.update(visual, |this, _| {
        let tab = this.workspace.tab_mut(tab).unwrap();
        tab.query_mode = Some(QueryMode::MongoFind);
        tab.database = "scoped".into();
        tab.collection = "orders".into();
    });
    find(app, visual, tab);
    let mut tabs = vec![tab];
    for collection in others {
        let id = app.update(visual, |this, _| {
            this.workspace.open_query_tab_with_metadata(
                *collection,
                crate::workspace::TabMetadata {
                    source_id: None,
                    connection_id: crate::ui_fixture::MONGO_ID.into(),
                    database: "scoped".into(),
                    schema: None,
                    collection: (*collection).into(),
                    query_mode: Some(QueryMode::MongoFind),
                    is_pinned: false,
                },
            )
        });
        find(app, visual, id);
        tabs.push(id);
    }
    tabs
}

fn page_of(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> Option<u64> {
    app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().driver_page_id)
}

/// Tab A (`orders`, pane 1) commits. Tab B (`orders`, pane 2, on screen)
/// re-runs at once, tab D (`orders`, hidden) when activated, and tab C
/// (`users`, hidden) keeps its page: the write cannot have made it stale.
#[gpui::test]
fn a_commit_refreshes_the_tabs_showing_the_written_collection_only(cx: &mut TestAppContext) {
    let (app, visual, tab_a, _session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "mutation.update"]);
    visual.dispatcher.allow_parking();
    let tabs = scoped_find_tabs(&app, visual, tab_a, &["orders", "users", "orders"]);
    let (b, c, d) = (tabs[1], tabs[2], tabs[3]);
    app.update(visual, |this, _| {
        // Pane 2 shows B; pane 1 keeps A visible and C, D hidden.
        assert!(this.workspace.split_tab(b, crate::panes::Edge::Right));
        this.workspace.activate_tab(tab_a);
        let visible = this.visible_tab_ids();
        assert!(visible.contains(&tab_a) && visible.contains(&b), "{visible:?}");
        assert!(!visible.contains(&c) && !visible.contains(&d), "{visible:?}");
    });
    let before: Vec<_> = tabs.iter().map(|tab| page_of(&app, visual, *tab)).collect();

    app.update(visual, |this, cx| {
        this.stage_mongo_mutation(
            tab_a,
            PendingMongoMutation::Insert {
                request: engine_driver_api::MutationRequest {
                    database: Some("scoped".into()),
                    object: Some("orders".into()),
                    ..mongo_update_request(serde_json::json!({"$set": {"a": 1}}))
                },
                document: serde_json::json!({"a": 1}),
            },
            cx,
        );
        this.commit_mongo_mutations(tab_a, cx);
    });
    wait_until(visual, "the write to refresh tab A and the visible tab B", |visual| {
        settle(visual);
        page_of(&app, visual, tab_a) != before[0] && page_of(&app, visual, b) != before[1]
    });

    // C is another collection: untouched, and its documents still open.
    assert_eq!(page_of(&app, visual, c), before[2], "the users page survives the write");
    app.update(visual, |this, cx| this.select_mongo_document(c, 0, cx));
    wait_until(visual, "the users document", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| {
            this.workspace.runtime(c).unwrap().mongo.inspector.snapshot.is_some()
        })
    });
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.runtime(c).unwrap().mongo.inspector.error, None);
        assert!(!this.connection_ui.rerun_on_activate.contains(&c));
        // D shows a dropped page but is hidden: it waits for activation.
        assert!(this.connection_ui.rerun_on_activate.contains(&d));
    });
    assert_eq!(page_of(&app, visual, d), before[3], "a hidden tab does not re-run");

    app.update(visual, |this, cx| this.activate_tab_ui(d, cx));
    wait_until(visual, "the activated tab D to re-run", |visual| {
        settle(visual);
        page_of(&app, visual, d) != before[3]
    });
    app.read_with(visual, |this, _| assert!(!this.connection_ui.rerun_on_activate.contains(&d)));
}

/// A reconnect drops every page of the connection: the active tab of each pane
/// re-runs at once, a hidden tab when it is activated.
#[gpui::test]
fn a_reconnect_refreshes_the_tab_on_screen_in_every_pane(cx: &mut TestAppContext) {
    let (app, visual, tab_a, _session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "mutation.update"]);
    visual.dispatcher.allow_parking();
    let tabs = scoped_find_tabs(&app, visual, tab_a, &["users", "users"]);
    let (b, hidden) = (tabs[1], tabs[2]);
    app.update(visual, |this, _| {
        assert!(this.workspace.split_tab(b, crate::panes::Edge::Right));
        this.workspace.activate_tab(tab_a);
    });
    let before: Vec<_> = tabs.iter().map(|tab| page_of(&app, visual, *tab)).collect();

    app.update(visual, |this, cx| this.rerun_tabs_after_reconnect(crate::ui_fixture::MONGO_ID, cx));
    wait_until(visual, "both panes' visible tabs to re-run", |visual| {
        settle(visual);
        page_of(&app, visual, tab_a) != before[0] && page_of(&app, visual, b) != before[1]
    });
    assert_eq!(page_of(&app, visual, hidden), before[2], "the hidden tab waits");
    app.read_with(visual, |this, _| assert!(this.connection_ui.rerun_on_activate.contains(&hidden)));
    app.update(visual, |this, cx| this.activate_tab_ui(hidden, cx));
    wait_until(visual, "the activated tab to re-run", |visual| {
        settle(visual);
        page_of(&app, visual, hidden) != before[2]
    });
}

/// Staged edits keep their rows only while the result stays: a visible tab
/// with edits pending is not re-run, its document view says it is out of date.
#[gpui::test]
fn a_stale_tab_with_staged_edits_is_not_re_run(cx: &mut TestAppContext) {
    let (app, visual, tab, _session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "mutation.update"]);
    visual.dispatcher.allow_parking();
    let tabs = scoped_find_tabs(&app, visual, tab, &[]);
    let page = page_of(&app, visual, tab);
    app.update(visual, |this, cx| this.select_mongo_document(tab, 0, cx));
    wait_until(visual, "the document", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().mongo.inspector.snapshot.is_some()
        })
    });
    app.update(visual, |this, cx| {
        this.stage_mongo_mutation(
            tab,
            PendingMongoMutation::Insert {
                request: mongo_update_request(serde_json::json!({"$set": {"a": 1}})),
                document: serde_json::json!({"a": 1}),
            },
            cx,
        );
        this.invalidate_driver_pages(crate::ui_fixture::MONGO_ID, &[page.unwrap()], None, cx);
    });
    settle(visual);
    assert_eq!(tabs, [tab]);
    assert_eq!(page_of(&app, visual, tab), page, "nothing re-ran");
    app.read_with(visual, |this, _| {
        let error = this.workspace.runtime(tab).unwrap().mongo.inspector.error.as_deref();
        assert!(error.is_some_and(|error| error.contains("out of date")), "{error:?}");
    });
}

fn mongo_update_request(payload: serde_json::Value) -> engine_driver_api::MutationRequest {
    engine_driver_api::MutationRequest {
        operation_id: crate::mongo_result::STAGED_OPERATION_ID,
        kind: "update".into(),
        database: Some("app".into()),
        object: Some("users".into()),
        identity: serde_json::json!({"_id": 1}),
        payload,
    }
}

#[gpui::test]
fn staged_mutations_are_sent_under_a_host_operation_without_the_session_lock(
    cx: &mut TestAppContext,
) {
    let (_app, _visual, _tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "mutation.update"]);
    // Registered operations are observable through a probe operation: its
    // id is the next one the host allocates.
    let probe = || session.begin_operation().expect("probe").id();
    let before = probe();
    // The session is busy elsewhere: the mutation does not wait for it.
    let holder = SessionLockHolder::hold(&session);
    let operation = session.begin_operation().expect("operation");
    let request = mongo_update_request(serde_json::json!({"$set": {"a": 1}}));
    let result = session.mutate(&operation, request.clone());
    assert!(result.is_ok(), "{result:?}");
    // The mock handle is shared with concurrently running tests, so ids are
    // only ordered, not consecutive.
    assert!(operation.id().0 > before.0, "the host allocated a fresh id");
    assert_ne!(operation.id(), crate::mongo_result::STAGED_OPERATION_ID);
    let used = operation.id();
    drop(operation);
    holder.release();
    assert!(probe().0 > used.0, "ids keep increasing");
    assert_eq!(request.operation_id, crate::mongo_result::STAGED_OPERATION_ID);
}

fn stage_and_commit_mongo_update(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab: u64,
    payload: serde_json::Value,
) {
    app.update(visual, |this, cx| {
        this.stage_mongo_mutation(
            tab,
            PendingMongoMutation::Insert {
                request: mongo_update_request(payload),
                document: serde_json::json!({"a": 1}),
            },
            cx,
        );
        this.commit_mongo_mutations(this.active_id(), cx);
    });
}

fn mongo_commit_status(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
    tab: u64,
) -> crate::mongo_result::MongoCommitStatus {
    app.read_with(visual, |this, _| {
        this.workspace.runtime(tab).unwrap().mongo.commit_status.clone()
    })
}

#[gpui::test]
fn a_mongo_commit_completes_while_the_session_lock_is_held_elsewhere(cx: &mut TestAppContext) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "mutation.update"]);
    visual.dispatcher.allow_parking();
    let holder = SessionLockHolder::hold(&session);
    stage_and_commit_mongo_update(&app, visual, tab, serde_json::json!({"$set": {"a": 1}}));
    wait_until(visual, "the commit to finish", |visual| {
        mongo_commit_status(&app, visual, tab) != crate::mongo_result::MongoCommitStatus::Running
    });
    assert!(session.try_lock().is_err(), "the session was busy throughout");
    assert_eq!(mongo_commit_status(&app, visual, tab), crate::mongo_result::MongoCommitStatus::Idle);
    app.read_with(visual, |this, _| {
        assert!(!this.driver_runtime.query_operations.contains_key(&tab));
        assert!(this.workspace.runtime(tab).unwrap().mongo.mutations.active().is_empty());
    });
    holder.release();
}

#[gpui::test]
fn a_slow_mongo_commit_does_not_block_the_navigator_and_can_be_cancelled(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "mutation.update"]);
    visual.dispatcher.allow_parking();
    // The mock driver keeps this mutation in flight until it is cancelled.
    stage_and_commit_mongo_update(
        &app,
        visual,
        tab,
        serde_json::json!({"block_until_cancelled": true}),
    );
    settle(visual);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(mongo_commit_status(&app, visual, tab), crate::mongo_result::MongoCommitStatus::Running);
    app.read_with(visual, |this, _| {
        assert!(
            this.driver_runtime.query_operations.contains_key(&tab),
            "the commit owns a cancellable operation"
        );
    });

    // A navigator call takes the session lock while the mutation is on the driver.
    let started = std::time::Instant::now();
    session
        .lock()
        .expect("the session lock is free")
        .list_nav_nodes(
            &serde_json::json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 0})
                .to_string(),
        )
        .expect("the navigator answers while the mutation runs");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(mongo_commit_status(&app, visual, tab), crate::mongo_result::MongoCommitStatus::Running);

    // The tab's Cancel reaches the mutation through its operation id.
    app.update(visual, |this, cx| this.cancel_tab_query(tab, cx));
    wait_until(visual, "the cancelled commit to end", |visual| {
        mongo_commit_status(&app, visual, tab) != crate::mongo_result::MongoCommitStatus::Running
    });
    assert!(
        matches!(
            mongo_commit_status(&app, visual, tab),
            crate::mongo_result::MongoCommitStatus::Partial { committed: 0, .. }
        ),
        "{:?}",
        mongo_commit_status(&app, visual, tab)
    );
    app.read_with(visual, |this, _| {
        assert!(!this.driver_runtime.query_operations.contains_key(&tab));
        assert_eq!(
            this.workspace.runtime(tab).unwrap().mongo.mutations.active().len(),
            1,
            "the cancelled change stays staged"
        );
        assert_eq!(this.error_log.log.len(), 0, "a user cancel is not an Error Log entry");
    });
}

#[gpui::test]
fn a_cancel_between_staged_requests_stops_the_batch_and_keeps_the_unsent_changes(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab, _session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "mutation.update"]);
    visual.dispatcher.allow_parking();
    app.update(visual, |this, cx| {
        for _ in 0..2 {
            this.stage_mongo_mutation(
                tab,
                PendingMongoMutation::Insert {
                    request: mongo_update_request(serde_json::json!({"$set": {"a": 1}})),
                    document: serde_json::json!({"a": 1}),
                },
                cx,
            );
        }
        this.commit_mongo_mutations(this.active_id(), cx);
        // Request 1 is on its way (the mock driver does not interrupt it) and
        // the tab's own operation is registered: Cancel takes that one.
        assert!(this.driver_runtime.query_operations.contains_key(&tab));
        this.cancel_tab_query(tab, cx);
    });
    wait_until(visual, "the cancelled commit to end", |visual| {
        mongo_commit_status(&app, visual, tab) != crate::mongo_result::MongoCommitStatus::Running
    });
    assert!(
        matches!(
            mongo_commit_status(&app, visual, tab),
            crate::mongo_result::MongoCommitStatus::Partial { committed: 1, .. }
        ),
        "request 2 must not be sent after Cancel: {:?}",
        mongo_commit_status(&app, visual, tab)
    );
    app.read_with(visual, |this, _| {
        assert!(!this.driver_runtime.query_operations.contains_key(&tab));
        assert_eq!(
            this.workspace.runtime(tab).unwrap().mongo.mutations.active().len(),
            1,
            "the unsent change stays staged"
        );
        assert_eq!(this.error_log.log.len(), 0, "a user cancel is not an Error Log entry");
    });
}

/// A click on Next shows that it took: while the page loads, Next spins and both
/// page buttons wait (a second click does not page twice); once it lands they
/// are back. The run is held at the session lock so "loading" lasts.
#[gpui::test]
fn next_page_spins_and_waits_while_the_page_loads(cx: &mut TestAppContext) {
    // A find tab (the "View Data" one) with its first page shown.
    let (app, visual, tab, session) = find_tab_on_real_session(cx);
    wait_until(visual, "the first page", |visual| {
        app.read_with(visual, |this, _| {
            !this.query_tasks.tasks.contains_key(&tab)
                && this.workspace.runtime(tab).is_some_and(|runtime| runtime.driver_page_id.is_some())
        })
    });
    let page = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().pagination.page())
    };
    // The mock finds one document; report a full page so Next is offered.
    app.update(visual, |this, cx| {
        let pagination = &mut this.workspace.runtime_mut(tab).unwrap().pagination;
        let full = pagination.page_size() as usize;
        pagination.set_result_count(full);
        cx.notify();
    });
    visual.run_until_parked();
    assert!(app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().pagination.has_next()));
    assert_eq!(app.read_with(visual, |this, _| this.query_tasks.paging_direction(tab)), None);
    let first = page(&app, visual);

    // The handlers are invoked directly: the harness's click helpers park until
    // background work finishes, which would wait out the held lock.
    let holder = SessionLockHolder::hold(&session);
    app.update(visual, |this, cx| this.page_tab_query(tab, true, cx));
    assert_eq!(app.read_with(visual, |this, _| this.query_tasks.paging_direction(tab)), Some(true), "Next spins");
    assert_eq!(page(&app, visual), first + 1);
    app.update(visual, |this, cx| this.page_tab_query(tab, true, cx));
    assert_eq!(page(&app, visual), first + 1, "Next while the page loads does not page twice");
    app.update(visual, |this, cx| this.page_tab_query(tab, false, cx));
    assert_eq!(page(&app, visual), first + 1, "Previous waits too");

    holder.release();
    wait_until(visual, "the second page", |visual| {
        app.read_with(visual, |this, _| !this.query_tasks.tasks.contains_key(&tab))
    });
    visual.run_until_parked();
    assert_eq!(app.read_with(visual, |this, _| this.query_tasks.paging_direction(tab)), None);
    assert_eq!(page(&app, visual), first + 1);
    // Back to normal: Previous pages again.
    click_debug_button(visual, "page-previous");
    wait_until(visual, "the first page again", |visual| {
        app.read_with(visual, |this, _| !this.query_tasks.tasks.contains_key(&tab))
    });
    assert_eq!(page(&app, visual), first);
}
