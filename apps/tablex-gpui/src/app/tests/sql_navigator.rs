// SQL navigator and Create Database on the driver route. Same harness as
// `sql_driver.rs`: the REAL SQLite driver dylib behind a real host session,
// the real sidebar rows clicked by their measured bounds. The live PostgreSQL /
// MySQL / MariaDB smoke tests at the end are `#[ignore]`d (dedicated
// `tablex-test-*` containers only).

const NAV_SEED: &str = "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); \
     INSERT INTO items(name) VALUES ('a'),('b'),('c'),('d'),('e'); \
     CREATE VIEW item_names AS SELECT name FROM items;";

/// A rig whose sidebar shows exactly the driver connection.
fn nav_rig<'a>(
    cx: &'a mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    uri: &str,
) -> SqlRig<'a> {
    let rig = SqlRig::new(cx, kind, driver, uri);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        this.saved_connections.retain(|saved| saved.matches_id(&key));
        this.navigator.set_connections(this.saved_connections.clone());
        this.sync_sidebar_connections(cx);
    });
    settle(rig.visual);
    rig
}

fn nav_rig_sqlite<'a>(cx: &'a mut TestAppContext, db: &SqliteDb) -> SqlRig<'a> {
    nav_rig(cx, ConnectionKind::Sqlite, ("dbengine-sqlite", ""), &db.uri())
}

fn connection_node(key: &str) -> NavigatorNodeId {
    NavigatorNodeId::Connection { connection: key.into() }
}

fn database_node(key: &str, database: &str) -> NavigatorNodeId {
    NavigatorNodeId::Database { connection: key.into(), database: database.into() }
}

fn schema_node(key: &str, database: &str, schema: &str) -> NavigatorNodeId {
    NavigatorNodeId::Schema {
        connection: key.into(),
        database: database.into(),
        schema: schema.into(),
    }
}

fn group_node(key: &str, database: &str, schema: &str, group: ObjectGroup) -> NavigatorNodeId {
    crate::navigator::object_group_id(key, database, schema, group)
}

fn object_node(key: &str, database: &str, schema: &str, object: &str) -> NavigatorNodeId {
    NavigatorNodeId::Object {
        connection: key.into(),
        database: database.into(),
        schema: schema.into(),
        object: object.into(),
    }
}

/// Clicks the measured bounds of `part` (`sidebar-row` or `nav-disclosure`)
/// of the row showing `id`.
fn click_node_part(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &NavigatorNodeId,
    part: &str,
) {
    settle(visual);
    let index = app
        .read_with(visual, |this, _| this.navigator.visible_index(id))
        .unwrap_or_else(|| panic!("{id:?} is a visible row"));
    let selector: &'static str = Box::leak(format!("{part}-{index}").into_boxed_str());
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is rendered"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

impl SqlRig<'_> {
    fn node_index(&self, id: &NavigatorNodeId) -> Option<usize> {
        self.app.read_with(self.visual, |this, _| this.navigator.visible_index(id))
    }

    fn labels(&self) -> Vec<String> {
        self.app.read_with(self.visual, |this, _| {
            this.navigator.visible_rows().into_iter().map(|row| row.label).collect()
        })
    }

    fn click_node(&mut self, id: &NavigatorNodeId, part: &str) {
        click_node_part(&self.app, self.visual, id, part);
    }

    fn wait_children(&mut self, id: &NavigatorNodeId) {
        let (app, id) = (self.app.clone(), id.clone());
        wait_until(self.visual, "the children of a node", |visual| {
            app.read_with(visual, |this, _| this.navigator.has_loaded_children(&id))
        });
        settle(self.visual);
    }

    /// Expands a node with its chevron and waits for its children.
    fn expand(&mut self, id: &NavigatorNodeId) {
        self.click_node(id, "nav-disclosure");
        self.wait_children(id);
    }

    /// Expands connection -> database -> schema -> Tables of a one-database,
    /// one-schema tree.
    fn expand_to_tables(&mut self, database: &str, schema: &str) -> NavigatorNodeId {
        let key = self.key.clone();
        self.expand(&connection_node(&key));
        self.expand(&database_node(&key, database));
        self.expand(&schema_node(&key, database, schema));
        let tables = group_node(&key, database, schema, ObjectGroup::Tables);
        self.expand(&tables);
        tables
    }

    fn tree_children(&self, id: &NavigatorNodeId) -> Vec<String> {
        self.app.read_with(self.visual, |this, _| {
            let index = this.navigator.visible_index(id).expect("parent row");
            let depth = this.navigator.visible_row_at(index).unwrap().depth;
            (index + 1..)
                .map_while(|at| this.navigator.visible_row_at(at))
                .take_while(|row| row.depth > depth)
                .filter(|row| row.depth == depth + 1)
                .map(|row| row.label)
                .collect()
        })
    }

    fn navigator_errors(&self) -> Vec<Option<String>> {
        self.log().into_iter().map(|(code, ..)| code).collect()
    }
}

#[gpui::test]
fn the_sidebar_expands_databases_schemas_tables_and_columns_through_the_driver(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();

    // A plain click on the connection row loads its databases.
    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_children(&connection_node(&key));
    assert_eq!(rig.tree_children(&connection_node(&key)), ["main"]);

    // SQLite has no schema level: its one schema is named like the database.
    rig.expand(&database_node(&key, "main"));
    assert_eq!(rig.tree_children(&database_node(&key, "main")), ["main"]);
    rig.expand(&schema_node(&key, "main", "main"));
    assert_eq!(
        rig.tree_children(&schema_node(&key, "main", "main")),
        ["Tables", "Views", "Triggers"]
    );

    let tables = group_node(&key, "main", "main", ObjectGroup::Tables);
    let views = group_node(&key, "main", "main", ObjectGroup::Views);
    rig.expand(&tables);
    rig.expand(&views);
    assert_eq!(rig.tree_children(&tables), ["items"]);
    assert_eq!(rig.tree_children(&views), ["item_names"]);
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(
            this.navigator.object_kind(&object_node(&key, "main", "main", "items")),
            Some(KindOfObject::Table)
        );
        assert_eq!(
            this.navigator.object_kind(&object_node(&key, "main", "main", "item_names")),
            Some(KindOfObject::View)
        );
    });

    let items = object_node(&key, "main", "main", "items");
    rig.expand(&items);
    assert_eq!(rig.tree_children(&items), ["id", "name"]);
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.driver_runtime.sessions.contains_key(&key), "served by the driver session");
    });
}

#[gpui::test]
fn opening_a_table_from_the_tree_runs_on_the_driver_and_loads_its_columns(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");
    let items = object_node(&key, "main", "main", "items");

    let index = rig.node_index(&items).expect("the items row");
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    let center = rig.visual.debug_bounds(selector).expect("row bounds").center();
    double_click(rig.visual, center);

    let (app, visual) = (rig.app.clone(), &mut *rig.visual);
    wait_until(visual, "the table tab to load", |visual| {
        app.read_with(visual, |this, _| {
            let tab = this.workspace.active_tab().unwrap();
            tab.collection == "items"
                && matches!(
                    this.workspace.runtime(tab.id).unwrap().query.status(),
                    crate::query::QueryStatus::Ready { rows: 5 }
                )
                && !this.workspace.runtime(tab.id).unwrap().structure_columns.is_empty()
        })
    });
    rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert!(tab.query_mode.is_table_data(), "a table-data tab, not the SQL editor");
        assert_eq!(tab.connection_id, key);
        assert_eq!(tab.schema.as_deref(), Some("main"));
        let columns = &this.workspace.runtime(tab.id).unwrap().structure_columns;
        assert_eq!(
            columns.iter().map(|c| (c.name.as_str(), c.primary_key)).collect::<Vec<_>>(),
            [("id", true), ("name", false)],
            "the catalog columns come from the driver"
        );
        assert!(tab.sql.contains("\"main\".\"items\""), "{}", tab.sql);
    });
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn remote_search_filters_object_groups_on_the_driver(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let tables = rig.expand_to_tables("main", "main");
    assert_eq!(rig.tree_children(&tables), ["items"]);

    // Local search off: the Views group (never opened) loads with the filter,
    // and the opened Tables group reloads with it.
    rig.app.update(rig.visual, |this, cx| this.set_sidebar_local_search(false, cx));
    search_query(&rig.app, rig.visual, "names");
    settle_search(rig.visual);
    let app = rig.app.clone();
    wait_until(rig.visual, "the remote search", |visual| {
        app.read_with(visual, |this, _| !this.sidebar.remote.loading)
    });
    settle(rig.visual);
    let labels = rig.labels();
    assert!(!labels.contains(&"items".to_string()), "no table matches `names`: {labels:?}");
    assert!(labels.contains(&"item_names".to_string()), "{labels:?}");
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.navigator.has_filtered_groups());
    });

    // Clearing the query restores the unfiltered listing.
    search_query(&rig.app, rig.visual, "");
    settle_search(rig.visual);
    let app = rig.app.clone();
    wait_until(rig.visual, "the restore", |visual| {
        app.read_with(visual, |this, _| !this.sidebar.remote.loading)
    });
    settle(rig.visual);
    assert_eq!(rig.tree_children(&tables), ["items"]);
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn a_large_catalog_is_paged_in_full_and_a_filtered_one_reports_its_cap(cx: &mut TestAppContext) {
    const TABLES: usize = 5_200;
    let seed = (0..TABLES).fold(String::from("BEGIN;"), |mut sql, index| {
        sql.push_str(&format!("CREATE TABLE t{index:05}(id INTEGER);"));
        sql
    }) + "COMMIT;";
    let db = SqliteDb::new(&seed);
    let mut rig = nav_rig_sqlite(cx, &db);
    rig.expand_to_tables("main", "main");

    // The driver serves 5 000 per call; the second page is followed.
    let loaded = rig.app.read_with(rig.visual, |this, _| {
        this.navigator
            .visible_rows()
            .iter()
            .filter(|row| matches!(&row.id, NavigatorNodeId::Object { .. }))
            .count()
    });
    assert_eq!(loaded, TABLES, "every page of the listing is in the tree");
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.navigator.truncated_group_count(), 0);
        assert!(this.remote_truncated_notice().is_none());
    });

    // A remote search is one capped call: the existing truncation notice.
    rig.app.update(rig.visual, |this, cx| this.set_sidebar_local_search(false, cx));
    search_query(&rig.app, rig.visual, "t0");
    settle_search(rig.visual);
    let app = rig.app.clone();
    wait_until(rig.visual, "the capped remote search", |visual| {
        app.read_with(visual, |this, _| !this.sidebar.remote.loading)
    });
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.navigator.truncated_group_count(), 1);
        assert!(this.remote_truncated_notice().is_some());
        let shown = this
            .navigator
            .visible_rows()
            .iter()
            .filter(|row| matches!(&row.id, NavigatorNodeId::Object { .. }))
            .count();
        assert_eq!(shown, crate::navigator::REMOTE_FILTER_LIMIT);
    });
}

#[gpui::test]
fn not_connected_during_expand_flips_the_connection_to_its_reconnect_state(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.session.lock().unwrap().disconnect().expect("disconnect");

    rig.click_node(&connection_node(&key), "sidebar-row");
    let (app, key_for_wait) = (rig.app.clone(), key.clone());
    wait_until(rig.visual, "the lost connection", |visual| {
        app.read_with(visual, |this, _| {
            matches!(this.sessions.status(&key_for_wait), ConnectionStatus::Failed(_))
        })
    });
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(
            this.sessions.status(&key),
            ConnectionStatus::Failed(crate::sessions::CONNECTION_LOST_REASON.into())
        );
        let index = this.navigator.visible_index(&connection_node(&key)).unwrap();
        assert!(matches!(
            this.navigator.visible_row_at(index).unwrap().load,
            Some(crate::navigator::NodeLoadState::Failed(_))
        ));
    });
    assert_eq!(rig.navigator_errors(), [Some("session.not_connected".to_string())]);
}

#[gpui::test]
fn expanding_the_tree_does_not_wait_for_a_running_query(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.run(LONG_QUERY);
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));

    let started = std::time::Instant::now();
    rig.expand_to_tables("main", "main");
    assert_eq!(
        rig.tree_children(&group_node(&key, "main", "main", ObjectGroup::Tables)),
        ["items"]
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the navigator waited behind the query: {:?}",
        started.elapsed()
    );
    assert!(
        matches!(rig.status(), crate::query::QueryStatus::Running { .. }),
        "the query was running throughout"
    );

    rig.click("run-query");
    rig.wait_driver_idle();
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn expanding_a_cold_connection_connects_through_the_driver_and_lists_its_databases(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let rig = ColdRig::new(cx, &db.uri());
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        this.saved_connections.retain(|saved| saved.matches_id(&key));
        this.navigator.set_connections(this.saved_connections.clone());
        this.sync_sidebar_connections(cx);
    });
    // The row click connects (no session yet) and then loads the databases.
    click_node_part(&rig.app, rig.visual, &connection_node(&key), "sidebar-row");
    let (app, id) = (rig.app.clone(), connection_node(&key));
    wait_until(rig.visual, "the connect and the first load", |visual| {
        app.read_with(visual, |this, _| this.navigator.has_loaded_children(&id))
    });
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.driver_runtime.sessions.contains_key(&key));
        assert!(this.sessions.is_connected(&key));
        assert!(this.navigator.visible_rows().iter().any(|row| row.label == "main"));
        assert!(
            this.navigator.provider_node(&database_node(&key, "main")).is_none(),
            "a SQL driver fills the typed tree, not provider nodes"
        );
    });
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    let _ = std::fs::remove_dir_all(&rig.store_dir);
}

#[gpui::test]
fn create_database_is_unavailable_without_the_capability_and_reads_the_cached_one(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(NAV_SEED);
    let rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    // The SQLite driver advertises no `database.create`.
    rig.app.update(rig.visual, |this, _| {
        assert!(!this.can_create_database(&key));
    });
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| this.open_create_database_dialog(&key, window, cx));
    });
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("dialog-layer").is_none(), "no dialog is offered");
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(
            this.sidebar.notice.as_deref(),
            Some("Create Database is unavailable for this connection")
        );
        assert!(this.object_action.is_none(), "nothing was reviewed or sent");
    });

    // The gate reads the capabilities cached at connect time (no session
    // lock): a busy session cannot hide the item.
    let held = rig.session.clone();
    let guard = held.lock().unwrap();
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.capabilities.insert(
            key.clone(),
            std::sync::Arc::new(engine_driver_api::DriverCapabilities::new(
                "sqlite",
                "SQLite",
                vec!["navigator".to_string(), "database.create".to_string()],
            )),
        );
        assert!(this.can_create_database(&key), "a held session lock does not matter");
    });
    drop(guard);
}

#[gpui::test]
fn a_create_database_the_dialect_cannot_do_is_refused_in_the_review_and_nothing_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NAV_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    // Cached capability says yes, SQLite has no databases to create: the review shows the refusal
    // where the SQL would be, Apply stays shut and nothing is sent.
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.capabilities.insert(
            key.clone(),
            std::sync::Arc::new(engine_driver_api::DriverCapabilities::new(
                "sqlite",
                "SQLite",
                vec!["navigator".to_string(), "database.create".to_string()],
            )),
        );
    });
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_create_database_dialog(&key, window, cx)));
    settle(rig.visual);
    oa_type(rig.visual, "object-name-input", "other");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("changes-error-0").is_some(), "the refusal is shown in the review");
    rig.click("changes-apply");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.diagram.applies.len()), 0, "nothing was sent");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master"), "2");
}

/// Runs a statement that needs the confirmation dialog and confirms it.
fn run_confirmed(rig: &mut SqlRig<'_>, sql: &str) {
    rig.run(sql);
    rig.wait_not_running_or_confirm();
}

impl SqlRig<'_> {
    /// Waits until the run finished; a dangerous statement is confirmed.
    fn wait_not_running_or_confirm(&mut self) {
        let (app, tab) = (self.app.clone(), self.tab);
        for _ in 0..400 {
            settle(self.visual);
            if self.visual.debug_bounds("sql-confirm-run").is_some() {
                self.click("sql-confirm-run");
            }
            let done = app.read_with(self.visual, |this, _| {
                !matches!(
                    this.workspace.runtime(tab).unwrap().query.status(),
                    crate::query::QueryStatus::Running { .. }
                )
            });
            if done && self.visual.debug_bounds("sql-confirm-run").is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the statement did not finish");
    }
}

/// Live navigator + Create Database smoke for one SQL server: creates a table
/// in the container's own database and a scratch database, walks the sidebar
/// to the table and its columns, and drops everything it created.
fn live_navigator_smoke(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let table = "tablex_w2_nav";
    // `cf_` is the name pattern the MySQL / MariaDB test users may create.
    let scratch = format!("cf_w2_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]);
    let mut rig = nav_rig(cx, kind, driver, &uri);
    let key = rig.key.clone();

    run_confirmed(&mut rig, &format!("DROP TABLE IF EXISTS {table}"));
    run_confirmed(&mut rig, &format!("CREATE TABLE {table}(id integer primary key, name varchar(20))"));

    // Databases -> schema -> Tables -> the table -> its columns.
    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_children(&connection_node(&key));
    let databases = rig.tree_children(&connection_node(&key));
    assert!(databases.contains(&database), "{database} in {databases:?}");
    rig.expand(&database_node(&key, &database));
    let schemas = rig.tree_children(&database_node(&key, &database));
    assert!(!schemas.is_empty(), "the database has a schema level: {schemas:?}");
    let schema = if schemas.contains(&"public".to_string()) { "public".to_string() } else { database.clone() };
    rig.expand(&schema_node(&key, &database, &schema));
    let tables = group_node(&key, &database, &schema, ObjectGroup::Tables);
    rig.expand(&tables);
    assert!(rig.tree_children(&tables).contains(&table.to_string()), "{:?}", rig.tree_children(&tables));
    let object = object_node(&key, &database, &schema, table);
    rig.expand(&object);
    assert_eq!(rig.tree_children(&object), ["id", "name"]);

    // Create Database through the menu's reviewed path, then the sidebar lists it. The scratch
    // database goes whatever happens.
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP DATABASE IF EXISTS {scratch}"), format!("DROP TABLE IF EXISTS {table}")]);
    rig.app.update(rig.visual, |this, _| {
        assert!(this.can_create_database(&key), "the driver advertises database.create");
    });
    oa_create_database(&mut rig, &scratch);
    let (app, scratch_for_wait) = (rig.app.clone(), scratch.clone());
    wait_until(rig.visual, "the new database in the sidebar", |visual| {
        app.read_with(visual, |this, _| {
            this.navigator.visible_rows().iter().any(|row| row.label == scratch_for_wait)
        })
    });
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_navigator_and_create_database_smoke(cx: &mut TestAppContext) {
    live_navigator_smoke(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_navigator_and_create_database_smoke(cx: &mut TestAppContext) {
    live_navigator_smoke(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_navigator_and_create_database_smoke(cx: &mut TestAppContext) {
    live_navigator_smoke(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}
