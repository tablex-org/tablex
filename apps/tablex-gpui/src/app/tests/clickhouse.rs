// ClickHouse on the driver route. The pure parts (apply notice, fixture) run
// everywhere; the live tests load the REAL ClickHouse driver dylib (built on
// demand like the SQLite one, never stripped) and are `#[ignore]`d: they need
// `TABLEX_TEST_CLICKHOUSE_URI`, e.g.
// `clickhouse://tablex:tablex@127.0.0.1:58123/tablex` (the dedicated
// `tablex-test-clickhouse` container; every table and database they create is
// dropped again, pass or fail).

const CLICKHOUSE_DRIVER: (&str, &str) = ("dbengine-clickhouse", "");

fn clickhouse_uri() -> Option<String> {
    let uri = std::env::var("TABLEX_TEST_CLICKHOUSE_URI").ok();
    if uri.is_none() {
        eprintln!("skipped: set TABLEX_TEST_CLICKHOUSE_URI to the tablex-test-clickhouse container URI");
    }
    uri
}

/// `prefix_xxxxxxxx`: a name no other run uses.
fn scratch_name(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string().replace('-', "");
    format!("{prefix}_{}", &id[..8])
}

/// Runs `sql` on `session`, bypassing the UI; `false` when it failed.
fn try_exec(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>, sql: &str) -> bool {
    let Ok(runner) = session.query_runner() else {
        return false;
    };
    let request = serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "sql".into(),
        operation_id: runner.operation_id(),
        database: None,
        object: None,
        offset: 0,
        limit: 0,
        payload: serde_json::json!({ "sql": sql, "confirmed": true }),
    })
    .unwrap();
    match runner.open_query(&request) {
        Ok(page) => {
            runner.release_page(page.page_id);
            true
        }
        Err(_) => false,
    }
}

/// Runs the statements it holds when the test ends, pass or fail.
struct Cleanup {
    session: std::sync::Arc<engine_driver_host::SharedDriverSession>,
    statements: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for sql in &self.statements {
            try_exec(&self.session, sql);
        }
    }
}

#[test]
fn clickhouse_apply_dialog_says_batches_are_not_atomic() {
    use results::mutations::apply_notice;
    let atomic = apply_notice(ConnectionKind::Postgres);
    assert!(atomic.contains("commit together or roll back together"));
    let clickhouse = apply_notice(ConnectionKind::Clickhouse);
    assert!(clickhouse.contains("no transactions"), "{clickhouse}");
    assert!(clickhouse.contains("some of them applied"), "{clickhouse}");
    assert!(!clickhouse.contains("commit together"));
    assert!(clickhouse.contains("Cancel preserves them"));
}

#[gpui::test]
fn the_apply_dialog_of_a_clickhouse_tab_uses_the_non_atomic_notice(cx: &mut TestAppContext) {
    use results::mutations::apply_notice;
    let (app, visual) = sidebar_test_app(cx);
    let (clickhouse_tab, sqlite_tab) = app.update(visual, |this, _| {
        this.scenario_ui.service = None;
        let mut tab_for = |kind| {
            let id = uuid::Uuid::new_v4();
            this.saved_connections.push(SavedConnection {
                id,
                name: "notice".into(),
                uri: "x://h".into(),
                kind,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            this.workspace.open_query_tab_with_metadata(
                "SQL",
                TabMetadata {
                    connection_id: id.to_string(),
                    ..Default::default()
                },
            )
        };
        (tab_for(ConnectionKind::Clickhouse), tab_for(ConnectionKind::Sqlite))
    });
    app.read_with(visual, |this, _| {
        assert_eq!(this.apply_dialog_notice(clickhouse_tab), apply_notice(ConnectionKind::Clickhouse));
        assert_eq!(this.apply_dialog_notice(sqlite_tab), apply_notice(ConnectionKind::Sqlite));
        assert_ne!(this.apply_dialog_notice(clickhouse_tab), this.apply_dialog_notice(sqlite_tab));
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (container tablex-test-clickhouse)"]
fn live_clickhouse_form_test_connection_and_connect_list_databases(cx: &mut TestAppContext) {
    use crate::connection_dialog::TestStatus;
    let Some(uri) = clickhouse_uri() else { return };
    // The connection form turns the container's URL into fields and back.
    let mut form = ConnectionFormState::new(ConnectionKind::Clickhouse);
    form.name = "clickhouse live".into();
    form.uri = uri.clone();
    form.parse_uri_fields();
    assert!(!form.use_uri && !form.host.is_empty());
    form.validate().expect("the form validates");
    let runtime_uri = form.runtime_uri().expect("runtime URI");
    let database = form.database.clone();

    let dylib = built_driver(CLICKHOUSE_DRIVER.0, CLICKHOUSE_DRIVER.1);
    let (app, visual) = sidebar_test_app(cx);
    visual.dispatcher.allow_parking();
    visual.update(|_, cx| cx.set_reduce_motion(true));
    let store_dir = std::env::temp_dir().join(format!("tablex-ch-store-{}", uuid::Uuid::new_v4()));
    let store = engine_driver_host::InstalledDriverStore::new(&store_dir);
    let sha256 = driver_sha256(&dylib);
    store
        .save(engine_driver_host::InstalledDriverRecord {
            id: "clickhouse".into(),
            version: "0.0.0-test".into(),
            path: dylib,
            sha256,
            team_id: "TEST".into(),
            installed_at: "2026-10-08T00:00:00Z".into(),
            app_abi_version: 2,
            uri_schemes: vec!["clickhouse".into(), "http".into(), "https".into()],
            license: "MIT".into(),
            source_repo: String::new(),
            disabled: false,
            ..Default::default()
        })
        .unwrap();
    let key = app.update(visual, |this, _| {
        this.scenario_ui.service = None;
        this.driver_ui.store = std::sync::Arc::new(store.clone());
        this.driver_ui.installed = drivers::DriverManagerUi::installed_ids(&store);
        let id = uuid::Uuid::new_v4();
        this.saved_connections.push(SavedConnection {
            id,
            name: form.name.clone(),
            uri: form.persisted_uri().unwrap(),
            kind: ConnectionKind::Clickhouse,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        });
        this.navigator.set_connections(this.saved_connections.clone());
        id.to_string()
    });
    app.update(visual, |this, cx| this.sync_sidebar_connections(cx));

    // The dialog's Test button: through the driver route.
    let status = std::rc::Rc::new(std::cell::RefCell::new(TestStatus::Idle));
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.start_connection_test(form.clone(), None, window.window_handle(), status.clone(), cx);
        });
    });
    let status_for_wait = status.clone();
    wait_until(visual, "the connection test", |_| {
        !matches!(*status_for_wait.borrow(), TestStatus::Idle | TestStatus::Running)
    });
    assert!(matches!(*status.borrow(), TestStatus::Success { .. }), "{:?}", status.borrow());

    // Connect (the saved record holds no password: the form's URI carries it),
    // then the sidebar lists the server's databases.
    app.update(visual, |this, cx| {
        this.ensure_connected_with_uri(key.clone(), Some(runtime_uri.clone().into()), None, cx);
    });
    wait_until(visual, "the driver connection", |visual| {
        app.read_with(visual, |this, _| {
            matches!(this.sessions.status(&key), ConnectionStatus::Connected)
        })
    });
    app.read_with(visual, |this, _| {
        assert!(this.driver_ui.handles.contains_key("clickhouse"));
        let capabilities = this.driver_capabilities(&key, "clickhouse").expect("capabilities");
        assert!(capabilities.has(engine_driver_api::Capability::DatabaseCreate));
    });
    click_node_part(&app, visual, &connection_node(&key), "sidebar-row");
    let (app_for_wait, node) = (app.clone(), connection_node(&key));
    wait_until(visual, "the databases", |visual| {
        app_for_wait.read_with(visual, |this, _| this.navigator.has_loaded_children(&node))
    });
    settle(visual);
    let databases = app.read_with(visual, |this, _| {
        this.navigator.visible_rows().into_iter().map(|row| row.label).collect::<Vec<_>>()
    });
    assert!(databases.contains(&database), "{database} in {databases:?}");
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (container tablex-test-clickhouse)"]
fn live_clickhouse_navigator_columns_and_create_database(cx: &mut TestAppContext) {
    let Some(uri) = clickhouse_uri() else { return };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let (table, view) = (scratch_name("tablex_ch_nav"), scratch_name("tablex_ch_view"));
    let scratch_db = scratch_name("cf_ch");
    let mut rig = nav_rig(cx, ConnectionKind::Clickhouse, CLICKHOUSE_DRIVER, &uri);
    let key = rig.key.clone();
    let _cleanup = Cleanup {
        session: rig.session.clone(),
        statements: vec![
            format!("DROP VIEW IF EXISTS {view}"),
            format!("DROP TABLE IF EXISTS {table}"),
            format!("DROP DATABASE IF EXISTS {scratch_db}"),
        ],
    };
    run_confirmed(&mut rig, &format!("DROP TABLE IF EXISTS {table}"));
    run_confirmed(
        &mut rig,
        &format!("CREATE TABLE {table} (id UInt64, name String) ENGINE = MergeTree ORDER BY id"),
    );
    run_confirmed(&mut rig, &format!("CREATE VIEW {view} AS SELECT id FROM {table}"));

    // database -> (schema named like it: ClickHouse has no schema level) ->
    // Tables / Views -> object -> columns.
    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_children(&connection_node(&key));
    let databases = rig.tree_children(&connection_node(&key));
    assert!(databases.contains(&database), "{database} in {databases:?}");
    rig.expand(&database_node(&key, &database));
    assert_eq!(rig.tree_children(&database_node(&key, &database)), std::slice::from_ref(&database));
    rig.expand(&schema_node(&key, &database, &database));
    let tables = group_node(&key, &database, &database, ObjectGroup::Tables);
    rig.expand(&tables);
    assert!(rig.tree_children(&tables).contains(&table), "{:?}", rig.tree_children(&tables));
    let views = group_node(&key, &database, &database, ObjectGroup::Views);
    rig.expand(&views);
    assert!(rig.tree_children(&views).contains(&view), "{:?}", rig.tree_children(&views));
    let object = object_node(&key, &database, &database, &table);
    rig.expand(&object);
    assert_eq!(rig.tree_children(&object), ["id", "name"]);

    // Create Database through the menu's reviewed path, listed, then dropped again.
    rig.app.update(rig.visual, |this, _| {
        assert!(this.can_create_database(&key), "the driver advertises database.create");
    });
    oa_create_database(&mut rig, &scratch_db);
    let (app, wanted) = (rig.app.clone(), scratch_db.clone());
    wait_until(rig.visual, "the new database in the sidebar", |visual| {
        app.read_with(visual, |this, _| {
            this.navigator.visible_rows().iter().any(|row| row.label == wanted)
        })
    });
    run_confirmed(&mut rig, &format!("DROP DATABASE {scratch_db}"));
    run_confirmed(&mut rig, &format!("DROP VIEW IF EXISTS {view}"));
    run_confirmed(&mut rig, &format!("DROP TABLE IF EXISTS {table}"));
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (container tablex-test-clickhouse)"]
fn live_clickhouse_open_page_edit_unsupported_and_properties(cx: &mut TestAppContext) {
    let Some(uri) = clickhouse_uri() else { return };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let (merge, swap, memory) = (
        scratch_name("tablex_ch_mt"),
        scratch_name("tablex_ch_swap"),
        scratch_name("tablex_ch_mem"),
    );
    let mut rig = SqlRig::new(cx, ConnectionKind::Clickhouse, CLICKHOUSE_DRIVER, &uri);
    let _cleanup = Cleanup {
        session: rig.session.clone(),
        statements: [&merge, &swap, &memory].map(|table| format!("DROP TABLE IF EXISTS {table}")).into(),
    };
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    exec_confirmed(
        &rig.session,
        &format!(
            "CREATE TABLE {merge} (id UInt64, name String, created DateTime) ENGINE = MergeTree \
             PARTITION BY toYYYYMM(created) ORDER BY id TTL created + INTERVAL 30 DAY"
        ),
    );
    exec_confirmed(
        &rig.session,
        &format!("INSERT INTO {merge} SELECT number + 1, concat('n', toString(number + 1)), now() FROM numbers(120)"),
    );

    // Open a MergeTree table: rows and paging.
    rig.set_page_size(50);
    rig.open_table_in(&database, None, &merge);
    rig.rerun_table("", "id");
    assert_eq!(rig.rows(), 50);
    assert_eq!(rig.paging(), (0, true));
    for (page, rows, has_next) in [(1, 50, true), (2, 20, false)] {
        rig.app.update(rig.visual, |this, cx| this.next_query_page(cx));
        rig.wait_status("the next page", move |status| {
            matches!(status, crate::query::QueryStatus::Ready { rows: ready } if *ready == rows)
        });
        assert_eq!(rig.paging(), (page, has_next), "page {page}");
    }
    rig.rerun_table("", "id");
    let grid = rig.grid_rows();
    assert_eq!((grid[0][0].as_str(), grid[0][1].as_str()), ("1", "n1"));

    // Edit a cell: the Apply dialog says ClickHouse batches are not atomic,
    // and the change is persisted (reloaded from the server).
    rig.edit_cell(1, 1, "edited");
    assert_eq!(rig.staged(), (1, 0));
    let tab = rig.tab;
    let notice = rig.app.read_with(rig.visual, |this, _| this.apply_dialog_notice(tab));
    assert!(notice.contains("no transactions"), "{notice}");
    rig.apply();
    rig.wait_apply_finished();
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(rig.staged(), (0, 0));
    rig.wait_not_running("the reload after Apply");
    assert_eq!(rig.grid_rows()[1][1], "edited");

    // Properties: ORDER BY / PARTITION BY / TTL as `other` keys, and the DDL
    // behind the stats comment line.
    let app = rig.app.clone();
    properties_open(rig.visual);
    wait_until(rig.visual, "the properties fetch", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().columns_panel.props.status != LoadStatus::Loading
        })
    });
    settle(rig.visual);
    assert_eq!(properties_snapshot(&app, rig.visual).1, LoadStatus::Loaded);
    properties_click_tab(rig.visual, PropertiesTab::Keys);
    let (_, keys, ..) = properties_table(&app, rig.visual, PropertiesTab::Keys);
    for name in ["ORDER BY", "PARTITION BY", "TTL"] {
        assert!(keys.iter().any(|key| key[0] == name && key[1] == "Other"), "{name}: {keys:?}");
    }
    let ddl = app.read_with(rig.visual, |this, _| {
        this.workspace
            .runtime(tab)
            .unwrap()
            .columns_panel
            .props
            .data
            .as_ref()
            .and_then(|data| data.ddl.clone())
            .expect("DDL")
    });
    assert!(ddl.starts_with("-- engine: MergeTree, rows: 120"), "{ddl}");
    assert!(ddl.contains("ENGINE = MergeTree") && ddl.contains("ORDER BY id"), "{ddl}");

    // A table the driver refuses to mutate (Memory has no sorting key): the
    // table is swapped for one under the staged edit's feet, so the app still
    // sees a primary key but the driver answers `mutation.unsupported`.
    exec_confirmed(
        &rig.session,
        &format!("CREATE TABLE {swap} (id UInt64, name String) ENGINE = MergeTree ORDER BY id"),
    );
    exec_confirmed(&rig.session, &format!("INSERT INTO {swap} VALUES (1, 'a'), (2, 'b')"));
    exec_confirmed(&rig.session, &format!("CREATE TABLE {memory} (id UInt64, name String) ENGINE = Memory"));
    exec_confirmed(&rig.session, &format!("INSERT INTO {memory} VALUES (1, 'a'), (2, 'b')"));
    rig.open_table_in(&database, None, &swap);
    rig.rerun_table("", "id");
    rig.edit_cell(0, 1, "changed");
    assert_eq!(rig.staged(), (1, 0));
    exec_confirmed(&rig.session, &format!("EXCHANGE TABLES {swap} AND {memory}"));
    rig.apply();
    rig.wait_apply_finished();
    let error = rig.detail_error().expect("the driver's refusal is shown");
    assert!(error.contains("mutation.unsupported"), "{error}");
    assert_eq!(rig.staged(), (1, 0), "the staged edit is kept");
    let log = rig.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].0.as_deref(), Some("mutation.unsupported"));
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (container tablex-test-clickhouse)"]
fn live_clickhouse_sleep_query_is_cancelled_by_the_run_button(cx: &mut TestAppContext) {
    let Some(uri) = clickhouse_uri() else { return };
    let mut rig = SqlRig::new(cx, ConnectionKind::Clickhouse, CLICKHOUSE_DRIVER, &uri);
    rig.run("SELECT sleep(3)");
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    let started = std::time::Instant::now();
    rig.click("run-query");
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert!(!rig.operation_registered());
    rig.wait_driver_idle();
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "stopped early: {:?}", started.elapsed());
    assert!(rig.log().is_empty(), "{:?}", rig.log());

    // The session still works after a cancel.
    rig.run("SELECT 1 AS one");
    rig.wait_status("SELECT 1", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 1 }));
}
