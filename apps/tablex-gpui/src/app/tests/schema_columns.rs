// The schema-wide column request (`metadata.schema_columns`) on the driver route: the REAL SQLite
// driver behind a real host session (see `sql_navigator.rs`). The cache itself (`SchemaColumnsCache`)
// is checked without a driver; the rest counts the driver calls of Columns folders and completion.

const SCHEMA_SEED: &str = "CREATE TABLE t1(id INTEGER PRIMARY KEY, a TEXT); \
     CREATE TABLE t2(id INTEGER PRIMARY KEY, b TEXT, c INT); \
     CREATE TABLE t3(z INT); \
     CREATE VIEW v1 AS SELECT id FROM t1;";

impl SqlRig<'_> {
    fn schema_key(&self) -> super::schema_columns::SchemaKey {
        let key = self.key.clone();
        self.app.read_with(self.visual, |this, _| this.schema_key(&key, "main", "main"))
    }

    fn schema_state(&self) -> super::schema_columns::SchemaState {
        let schema = self.schema_key();
        self.app.read_with(self.visual, |this, _| this.schema_columns.state(&schema))
    }

    fn wait_schema_state(&mut self, state: super::schema_columns::SchemaState) {
        let (app, schema) = (self.app.clone(), self.schema_key());
        wait_until(self.visual, "the schema columns", |visual| {
            app.read_with(visual, |this, _| this.schema_columns.state(&schema) == state)
        });
        settle(self.visual);
    }

    /// `(schema requests, driver calls for one table's columns)` so far.
    fn column_calls(&self) -> (usize, usize) {
        self.app.read_with(self.visual, |this, _| (this.schema_columns.fetches, this.schema_columns.per_table_calls))
    }

    /// Opens the table, then its Columns folder, reads the folder's rows and closes both again (the
    /// sidebar shows only so many rows).
    fn open_columns(&mut self, table: &str) -> Vec<String> {
        let key = self.key.clone();
        let object = object_node(&key, "main", "main", table);
        let folder = folder_node(&key, table, crate::navigator::TableFolder::Columns);
        self.expand(&object);
        self.expand(&folder);
        let columns = self.tree_children(&folder);
        self.click_node(&folder, "nav-disclosure");
        self.click_node(&object, "nav-disclosure");
        columns
    }
}

#[gpui::test]
fn a_columns_folder_after_the_schema_is_read_asks_the_driver_for_nothing(cx: &mut TestAppContext) {
    use super::schema_columns::SchemaState;
    let db = SqliteDb::new(SCHEMA_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");
    rig.expand(&group_node(&key, "main", "main", ObjectGroup::Views));
    assert_eq!(rig.column_calls(), (0, 0), "nothing is asked until a folder opens");
    assert!(rig.app.read_with(rig.visual, |this, _| this.driver_lists_schema_columns(&key)), "the driver says it can");

    // The first folder asks for its table alone and starts the schema in the background.
    assert_eq!(rig.open_columns("t1"), ["id", "a"]);
    assert_eq!(rig.column_calls(), (1, 1));
    rig.wait_schema_state(SchemaState::Ready);

    // The next ones open from it: no call at all, in the driver's column order.
    assert_eq!(rig.open_columns("t2"), ["id", "b", "c"]);
    assert_eq!(rig.open_columns("t3"), ["z"]);
    assert_eq!(rig.open_columns("v1"), ["id"], "a view too");
    assert_eq!(rig.column_calls(), (1, 1), "three more folders, no driver call");
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
    rig.app.read_with(rig.visual, |this, _| {
        let session = this.driver_runtime.connection_ids[&key];
        let stored = this.schema_columns.table(&key, "main", "main", session, "t2").unwrap();
        assert_eq!(stored.iter().map(|column| &*column.data_type).collect::<Vec<_>>(), ["INTEGER", "TEXT", "INT"]);
    });
}

#[gpui::test]
fn a_driver_without_the_capability_is_asked_per_table_and_never_for_the_schema(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCHEMA_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    // An installed driver that predates `metadata.schema_columns`.
    rig.app.update(rig.visual, |this, _| {
        let mut wire = serde_json::to_value(this.driver_runtime.capabilities[&key].as_ref()).unwrap();
        wire["features"].as_array_mut().unwrap().retain(|feature| feature != "metadata.schema_columns");
        let capabilities: engine_driver_api::DriverCapabilities = serde_json::from_value(wire).unwrap();
        this.driver_runtime.capabilities.insert(key.clone(), std::sync::Arc::new(capabilities));
    });
    rig.expand_to_tables("main", "main");
    assert!(!rig.app.read_with(rig.visual, |this, _| this.driver_lists_schema_columns(&key)));
    for table in ["t1", "t2", "t3"] {
        assert!(!rig.open_columns(table).is_empty());
    }
    assert_eq!(rig.column_calls(), (0, 3), "one call per table, no schema request");
    assert_eq!(rig.schema_state(), super::schema_columns::SchemaState::Absent);
}

#[gpui::test]
fn a_ddl_run_from_the_editor_never_serves_the_columns_it_changed(cx: &mut TestAppContext) {
    use super::schema_columns::SchemaState;
    let db = SqliteDb::new(SCHEMA_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let tables = rig.expand_to_tables("main", "main");
    rig.open_columns("t1");
    rig.wait_schema_state(SchemaState::Ready);

    // An ALTER run from the editor (the confirmation the app asks for a dangerous statement is not
    // the subject here: the statement is run on the side and the editor's bookkeeping is called).
    db.sql("ALTER TABLE t2 ADD COLUMN added TEXT");
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        this.note_ddl_run(tab, "ALTER TABLE t2 ADD COLUMN added TEXT");
        this.refresh_navigator_after_ddl(tab, cx);
    });
    settle(rig.visual);
    assert_eq!(rig.schema_state(), SchemaState::Absent, "the schema's columns are forgotten with the change");
    // The table is read again: the new column is there (the stored one would not have it).
    let before = rig.column_calls();
    assert_eq!(rig.open_columns("t2"), ["id", "b", "c", "added"]);
    assert_eq!(rig.column_calls().1, before.1 + 1);

    // A table created from the editor is not behind the stored columns either: the schema is read again.
    rig.wait_schema_state(SchemaState::Ready);
    rig.run("CREATE TABLE t4(q INT)");
    rig.wait_status("the CREATE TABLE to run", ready);
    rig.wait_children_include(&tables, "t4");
    assert_eq!(rig.schema_state(), SchemaState::Absent);
    assert_eq!(rig.open_columns("t4"), ["q"]);
}

#[gpui::test]
fn a_refresh_a_reconnect_and_a_table_refresh_forget_what_they_cover(cx: &mut TestAppContext) {
    use super::schema_columns::SchemaState;
    let db = SqliteDb::new(SCHEMA_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");
    rig.open_columns("t1");
    rig.wait_schema_state(SchemaState::Ready);
    let session = rig.app.read_with(rig.visual, |this, _| this.driver_runtime.connection_ids[&key]);

    // One table's refresh forgets that table only: the others stay stored.
    let t2 = object_node(&key, "main", "main", "t2");
    rig.app.update(rig.visual, |this, _| this.invalidate_table_properties(&t2));
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.schema_columns.table(&key, "main", "main", session, "t2").is_none());
        assert!(this.schema_columns.table(&key, "main", "main", session, "t1").is_some());
        assert_eq!(this.schema_columns.state(&this.schema_key(&key, "main", "main")), SchemaState::Ready);
    });

    // Another session of the connection reads none of the old one's columns.
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.schema_columns.table(&key, "main", "main", session + 1, "t1").is_none());
    });

    // The refresh of the schema (or what holds it) forgets all of it.
    let schema = schema_node(&key, "main", "main");
    rig.app.update(rig.visual, |this, _| this.invalidate_table_properties(&schema));
    assert_eq!(rig.schema_state(), SchemaState::Absent);

    // A disconnect leaves nothing behind.
    rig.open_columns("t3");
    rig.wait_schema_state(SchemaState::Ready);
    rig.app.update(rig.visual, |this, _| this.schema_columns.forget_connection(&key));
    assert_eq!(rig.schema_state(), SchemaState::Absent);
}

#[gpui::test]
fn completion_of_a_join_reads_the_columns_of_every_table_with_one_schema_request(cx: &mut TestAppContext) {
    use super::schema_columns::SchemaState;
    let db = SqliteDb::new(SCHEMA_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let (app, visual, key) = (rig.app.clone(), &mut *rig.visual, rig.key.clone());
    settle(visual);
    type_in_editor(visual, "select * from t1 join t2 on t2.");
    let (_, editor) = query_editor(&app, visual);
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "the menu opens with the columns of the joined table");
    for column in ["id", "b", "c"] {
        assert!(labels.iter().any(|label| label == column), "{column} in {labels:?}");
    }
    let (fetches, per_table) = app.read_with(visual, |this, _| (this.schema_columns.fetches, this.schema_columns.per_table_calls));
    assert_eq!((fetches, per_table), (1, 0), "one schema request, no request for one table");
    app.read_with(visual, |this, _| {
        assert_eq!(this.schema_columns.state(&this.schema_key(&key, "main", "main")), SchemaState::Ready);
        assert!(this.completion_loads.is_idle());
    });

    // The columns of t1 come from the same store.
    visual.simulate_input(" and t1.");
    settle(visual);
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open);
    assert!(labels.iter().any(|label| label == "a"), "{labels:?}");
    let (fetches, per_table) = app.read_with(visual, |this, _| (this.schema_columns.fetches, this.schema_columns.per_table_calls));
    assert_eq!((fetches, per_table), (1, 0));
}

#[gpui::test]
fn completion_in_a_schema_over_the_prefetch_cap_asks_for_the_tables_it_needs_not_the_schema(cx: &mut TestAppContext) {
    // 1 001 tables: one over the cap, so the whole schema is not read for the columns of two of them.
    let mut seed = String::from("CREATE TABLE t1(id INTEGER PRIMARY KEY, a TEXT); CREATE TABLE t2(id INTEGER PRIMARY KEY, b TEXT, c INT);");
    for n in 3..=1_001 {
        seed.push_str(&format!("CREATE TABLE t{n}(z INT);"));
    }
    let db = SqliteDb::new(&seed);
    let rig = SqlRig::sqlite(cx, &db);
    let (app, visual) = (rig.app.clone(), &mut *rig.visual);
    settle(visual);
    type_in_editor(visual, "select * from t1 join t2 on t2.");
    let (_, editor) = query_editor(&app, visual);
    // The tables are asked for one by one, off the UI thread: the menu opens when they land.
    let (mut open, mut labels) = (false, Vec::new());
    for _ in 0..300 {
        settle(visual);
        (open, labels, _) = menu_state(&editor, visual);
        if open {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(open, "the menu opens with the columns of the joined table");
    for column in ["id", "b", "c"] {
        assert!(labels.iter().any(|label| label == column), "{column} in {labels:?}");
    }
    let (fetches, per_table) = app.read_with(visual, |this, _| (this.schema_columns.fetches, this.schema_columns.per_table_calls));
    assert_eq!(fetches, 0, "no schema request for a schema of 1 001 tables");
    assert!(per_table >= 1, "the tables were asked for one by one");
}

// ---- the cache without a driver ------------------------------------------------------------------

#[test]
fn the_schema_cache_joins_waiters_keeps_one_slot_per_session_and_forgets_what_is_covered() {
    use super::schema_columns::{Outcome, SchemaColumnsCache, SchemaKey, SchemaState};
    let key = |session| SchemaKey { connection: "c".into(), database: "app".into(), schema: "public".into(), session };
    let load = |object: &str| crate::navigator::LoadRequest::Columns {
        connection: "c".into(),
        database: "app".into(),
        schema: "public".into(),
        object: object.into(),
    };
    let other_key = |schema: &str| SchemaKey { connection: "c".into(), database: "app".into(), schema: schema.into(), session: 1 };
    let columns = |tables: &[&str]| {
        let nodes: Vec<_> = tables
            .iter()
            .map(|table| engine_driver_api::NavigatorNode {
                id: format!("column:{table}:id"),
                kind: "column".into(),
                label: "id".into(),
                subtitle: Some("int".into()),
                icon: None,
                has_children: false,
                payload: serde_json::json!({"object": table, "column": "id", "data_type": "int"}),
                actions: Vec::new(),
            })
            .collect();
        crate::navigator::schema_columns::collect(crate::navigator::schema_columns::request("app", "public", true), |_| {
            Ok::<_, String>(engine_driver_api::NavigatorPage { nodes: nodes.clone(), next_cursor: None })
        })
        .unwrap()
    };
    let mut cache = SchemaColumnsCache::default();
    assert_eq!(cache.state(&key(1)), SchemaState::Absent);
    assert!(cache.begin(&key(1), Some((load("a"), 7))), "the first caller asks");
    assert!(!cache.begin(&key(1), Some((load("b"), 8))), "the second one joins it");
    assert_eq!(cache.fetches, 1);
    assert_eq!(cache.state(&key(1)), SchemaState::Loading);
    let waiting = cache.finish(&key(1), Outcome::Ready(columns(&["a", "b"])));
    assert_eq!(waiting.len(), 2);
    assert_eq!(cache.state(&key(1)), SchemaState::Ready);
    assert!(cache.table("c", "app", "public", 1, "a").is_some());
    assert!(cache.table("c", "app", "public", 2, "a").is_none(), "another session reads nothing of it");
    assert!(cache.table("c", "app", "other", 1, "a").is_none());
    assert!(!cache.begin(&key(1), None), "a settled schema is not asked again");

    // One table is forgotten alone; the whole schema with it when the schema is.
    cache.forget(|other| other.connection == "c", Some("a"));
    assert!(cache.table("c", "app", "public", 1, "a").is_none() && cache.table("c", "app", "public", 1, "b").is_some());
    // A reconnect asks again and drops the old session's columns.
    assert!(cache.begin(&key(2), None));
    assert_eq!(cache.state(&key(1)), SchemaState::Absent);

    // An invalidation during the request: its answer is delivered but not kept.
    cache.begin(&key(3), Some((load("a"), 1)));
    cache.forget(|other| other.schema == "public", None);
    assert_eq!(cache.finish(&key(3), Outcome::Ready(columns(&["a"]))).len(), 1);
    assert_eq!(cache.state(&key(3)), SchemaState::Absent);

    // A request that was over the cap is remembered: the tables are asked for one by one.
    cache.begin(&key(4), None);
    cache.finish(&key(4), Outcome::Unavailable);
    assert_eq!(cache.state(&key(4)), SchemaState::Unavailable);
    assert!(!cache.begin(&key(4), None));
    cache.forget(|other| other.session == 4, Some("a"));
    assert_eq!(cache.state(&key(4)), SchemaState::Unavailable, "one table's refresh does not retry the schema");
    cache.forget(|other| other.session == 4, None);
    assert_eq!(cache.state(&key(4)), SchemaState::Absent, "a refresh of the schema does");

    // A request that failed is not a verdict: the tables are asked for one by one for a while, then
    // the schema is asked again (a connection that blipped once is not served per table until a reconnect).
    cache.begin(&key(5), Some((load("a"), 1)));
    assert_eq!(cache.finish(&key(5), Outcome::Failed).len(), 1);
    assert_eq!(cache.state(&key(5)), SchemaState::Unavailable, "not at once");
    assert!(!cache.begin(&key(5), None));
    cache.expire_failures();
    assert_eq!(cache.state(&key(5)), SchemaState::Absent, "later");
    assert!(cache.begin(&key(5), None), "asked again");
    assert_eq!(cache.state(&key(5)), SchemaState::Loading);

    // A connection dropped while a request is in flight hands its waiters back.
    let mut cache = SchemaColumnsCache::default();
    cache.begin(&key(1), Some((load("a"), 3)));
    cache.begin(&key(1), Some((load("b"), 4)));
    cache.begin(&other_key("d"), None);
    cache.finish(&other_key("d"), Outcome::Unavailable);
    assert_eq!(cache.forget_connection("c").len(), 2);
    assert_eq!(cache.state(&key(1)), SchemaState::Absent);
    assert_eq!(cache.state(&other_key("d")), SchemaState::Absent);
    assert!(cache.forget_connection("c").is_empty());

    // Eight schemas at most: the one used least recently goes, never one in flight.
    let mut cache = SchemaColumnsCache::default();
    let numbered = |n: u64| SchemaKey { connection: "c".into(), database: "app".into(), schema: format!("s{n}"), session: 1 };
    for n in 0..8 {
        cache.begin(&numbered(n), None);
        cache.finish(&numbered(n), Outcome::Unavailable);
    }
    assert!(cache.table("c", "app", "s0", 1, "a").is_none(), "using s0 makes it the most recent");
    cache.begin(&numbered(8), None);
    assert_eq!(cache.state(&numbered(0)), SchemaState::Unavailable, "s0 was used last");
    assert_eq!(cache.state(&numbered(1)), SchemaState::Absent, "s1 went");
    assert_eq!(cache.state(&numbered(8)), SchemaState::Loading, "the one in flight stays");
    assert_eq!(cache.slot_count(), 8);
}

// ---- round trips and time ---------------------------------------------------------------------

/// A schema of 1 000 tables of 12 columns: the columns of every table one request per table against the
/// schema request, through the real SQLite driver and host. The numbers are printed (`--nocapture`).
#[gpui::test]
fn the_schema_request_replaces_a_round_trip_per_table(cx: &mut TestAppContext) {
    const TABLES: usize = 1_000;
    const COLUMNS: usize = 12;
    let mut seed = String::new();
    for table in 0..TABLES {
        seed.push_str(&format!("CREATE TABLE tbl_{table:04}(id INTEGER PRIMARY KEY"));
        for column in 1..COLUMNS {
            seed.push_str(&format!(", col_{column} TEXT"));
        }
        seed.push_str(");\n");
    }
    let db = SqliteDb::new(&seed);
    let rig = SqlRig::sqlite(cx, &db);
    let session = rig.session.clone();

    let started = std::time::Instant::now();
    let mut calls = 0;
    let mut per_table_columns = 0;
    for table in 0..TABLES {
        let request = crate::navigator::sql_driver::catalog_request(
            &LoadRequest::Columns { connection: "c".into(), database: "main".into(), schema: "main".into(), object: format!("tbl_{table:04}") },
            false,
            None,
        )
        .unwrap();
        let nodes = crate::navigator::sql_driver::collect_pages(request, false, |request| {
            calls += 1;
            super::navigator_ui::list_driver_nav_nodes(&session, request)
        })
        .unwrap();
        per_table_columns += nodes.len();
    }
    let per_table = started.elapsed();

    let started = std::time::Instant::now();
    let mut batch_calls = 0;
    let stored = crate::navigator::schema_columns::collect(crate::navigator::schema_columns::request("main", "main", false), |request| {
        batch_calls += 1;
        super::navigator_ui::list_driver_nav_nodes(&session, request)
    })
    .unwrap();
    let batch = started.elapsed();

    eprintln!(
        "{TABLES} tables x {COLUMNS} columns: per table {calls} calls {per_table:?}; schema request {batch_calls} calls {batch:?} ({:.1}x fewer calls, {:.1}x faster)",
        calls as f64 / batch_calls as f64,
        per_table.as_secs_f64() / batch.as_secs_f64()
    );
    assert_eq!((calls, per_table_columns), (TABLES, TABLES * COLUMNS));
    assert_eq!(stored.table_count(), TABLES);
    assert_eq!(stored.column_count(), TABLES * COLUMNS);
    // 12 000 columns at 5 000 a page.
    assert_eq!(batch_calls, 3);
}

/// The bound: a schema of 10 000 tables of 50 columns (500 000 columns) is refused at the cap after
/// `MAX_COLUMNS / 5 000` pages, and a schema just under it is stored in a few MB.
#[test]
fn a_schema_of_ten_thousand_wide_tables_stays_under_the_cap_and_a_big_one_is_compact() {
    use crate::navigator::schema_columns::{collect, request, SchemaColumnsError, MAX_COLUMNS};
    let node = |table: usize, column: usize| {
        let data_type = ["bigint", "text", "timestamp", "numeric(12,2)"][column % 4];
        engine_driver_api::NavigatorNode {
            id: String::new(),
            kind: "column".into(),
            label: format!("column_{column}"),
            subtitle: Some(data_type.into()),
            icon: None,
            has_children: false,
            payload: serde_json::json!({"object": format!("table_{table:05}"), "column": format!("column_{column}"), "data_type": data_type}),
            actions: Vec::new(),
        }
    };
    let serve = |tables: usize, columns: usize| {
        let total = tables * columns;
        move |request: &engine_driver_api::NavigatorRequest| -> Result<engine_driver_api::NavigatorPage, String> {
            let from: usize = request.cursor.as_deref().map_or(0, |cursor| cursor.parse().unwrap());
            let to = (from + 5_000).min(total);
            Ok(engine_driver_api::NavigatorPage {
                nodes: (from..to).map(|at| node(at / columns, at % columns)).collect(),
                next_cursor: (to < total).then(|| to.to_string()),
            })
        }
    };
    let started = std::time::Instant::now();
    assert!(matches!(collect(request("d", "s", true), serve(10_000, 50)), Err(SchemaColumnsError::TooLarge)));
    let refused = started.elapsed();

    let started = std::time::Instant::now();
    let stored = collect(request("d", "s", true), serve(2_000, 50)).unwrap();
    let read = started.elapsed();
    assert_eq!((stored.table_count(), stored.column_count()), (2_000, MAX_COLUMNS));
    let first = stored.table("table_00000").unwrap();
    let again = stored.table("table_01999").unwrap();
    assert!(std::sync::Arc::ptr_eq(&first[3].data_type, &again[3].data_type), "one string per distinct type");
    assert!(std::sync::Arc::ptr_eq(&first[0].name, &again[0].name), "one string per distinct name");
    eprintln!("10 000 x 50 refused at the cap in {refused:?}; 2 000 x 50 ({MAX_COLUMNS} columns) read in {read:?}");
}
