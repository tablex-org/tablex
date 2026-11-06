// Generate SQL: the navigator's table and view rows offer a SELECT / INSERT / UPDATE / DELETE that opens
// a new SQL tab with the object's real columns (the statement text itself is covered in
// `object_actions::generate_sql`). Driven through the real context menu and its submenu.
//
// * SQLite: the REAL driver describes the table of a real database file.
// * Postgres / ClickHouse fixtures: the fixture's `describe_object` answer; MongoDB: no entry.

/// Right-clicks the row of `id`, walks the menu with the keyboard to the Generate SQL submenu (selecting it
/// opens it), and clicks the entry `selector`.
fn gs_pick(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId, selector: &'static str) {
    gs_open_submenu(app, visual, id);
    let entry = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is in the submenu"));
    visual.simulate_click(entry.center(), Default::default());
    settle(visual);
}

/// The selectors of the Generate SQL entries the submenu of `id` shows.
fn gs_entries(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) -> Vec<&'static str> {
    gs_open_submenu(app, visual, id);
    let found = ["object-generate-select", "object-generate-insert", "object-generate-update", "object-generate-delete"]
        .into_iter()
        .filter(|selector| visual.debug_bounds(selector).is_some())
        .collect();
    visual.simulate_keystrokes("escape");
    visual.simulate_keystrokes("escape");
    settle(visual);
    found
}

/// Opens the row's menu and arrows down until a Generate SQL entry shows (no entry: the menu is left open).
fn gs_open_submenu(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) {
    settle(visual);
    let index = app.read_with(visual, |this, _| this.navigator.visible_index(id)).unwrap_or_else(|| panic!("{id:?} is a visible row"));
    right_click_row(visual, index);
    settle(visual);
    for _ in 0..16 {
        if visual.debug_bounds("object-generate-select").is_some() {
            return;
        }
        visual.simulate_keystrokes("down");
        settle(visual);
    }
}

/// Waits for the SQL tab the pick opens, and returns its connection, database, schema and text.
fn gs_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tabs_before: usize) -> (String, String, Option<String>, String) {
    let waiting = app.clone();
    wait_until(visual, "the SQL tab", |visual| waiting.read_with(visual, |this, _| this.workspace.tabs().len() > tabs_before));
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("an active tab");
        assert_eq!(tab.kind, TabKind::Query);
        assert!(!tab.query_mode.as_ref().is_some_and(|mode| mode.is_table_data()), "a plain SQL tab");
        assert!(matches!(this.workspace.runtime(tab.id).unwrap().query.status(), crate::query::QueryStatus::Idle), "it never runs on its own");
        (tab.connection_id.clone(), tab.database.clone(), tab.schema.clone(), tab.sql.clone())
    })
}

fn gs_tab_count(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| this.workspace.tabs().len())
}

const GENERATE_SEED: &str = "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT, note TEXT); \
     CREATE TABLE pairs(a INTEGER, b INTEGER, label TEXT, extra TEXT, PRIMARY KEY (a, b)); \
     CREATE TABLE loose(\"we\"\"ird\" TEXT, \"a b\" TEXT, a_b TEXT, \"1st\" TEXT); \
     CREATE VIEW item_names AS SELECT name FROM items;";

#[gpui::test]
fn generate_select_opens_a_sql_tab_on_the_table_with_its_real_columns(cx: &mut TestAppContext) {
    let db = SqliteDb::new(GENERATE_SEED);
    let rig = oa_rig(cx, &db);
    let items = oa_table(&rig, "items");
    let before = gs_tab_count(&rig.app, rig.visual);
    gs_pick(&rig.app, rig.visual, &items, "object-generate-select");
    let (connection, database, schema, sql) = gs_tab(&rig.app, rig.visual, before);
    assert_eq!((connection.as_str(), database.as_str(), schema.as_deref()), (rig.key.as_str(), "main", Some("main")));
    assert_eq!(sql, "SELECT \"id\", \"name\", \"note\"\nFROM \"main\".\"items\"\nLIMIT 100;");
}

#[gpui::test]
fn generate_insert_update_and_delete_use_the_primary_key_and_skip_the_identity_column(cx: &mut TestAppContext) {
    let db = SqliteDb::new(GENERATE_SEED);
    let rig = oa_rig(cx, &db);
    let items = oa_table(&rig, "items");
    let pairs = oa_table(&rig, "pairs");
    for (table, selector, expected) in [
        // `id INTEGER PRIMARY KEY` is the rowid: the server fills it.
        (&items, "object-generate-insert", "INSERT INTO \"main\".\"items\" (\"name\", \"note\")\nVALUES (:name, :note);"),
        (&items, "object-generate-update", "UPDATE \"main\".\"items\"\nSET \"name\" = :name, \"note\" = :note\nWHERE \"id\" = :id;"),
        (&items, "object-generate-delete", "DELETE FROM \"main\".\"items\"\nWHERE \"id\" = :id;"),
        // A composite key is every column of the WHERE; more than three columns go one per line.
        (
            &pairs,
            "object-generate-insert",
            "INSERT INTO \"main\".\"pairs\" (\n  \"a\",\n  \"b\",\n  \"label\",\n  \"extra\"\n)\nVALUES (\n  :a,\n  :b,\n  :label,\n  :extra\n);",
        ),
        (&pairs, "object-generate-update", "UPDATE \"main\".\"pairs\"\nSET \"label\" = :label, \"extra\" = :extra\nWHERE \"a\" = :a\n  AND \"b\" = :b;"),
        (&pairs, "object-generate-delete", "DELETE FROM \"main\".\"pairs\"\nWHERE \"a\" = :a\n  AND \"b\" = :b;"),
    ] {
        let before = gs_tab_count(&rig.app, rig.visual);
        gs_pick(&rig.app, rig.visual, table, selector);
        let (.., sql) = gs_tab(&rig.app, rig.visual, before);
        assert_eq!(sql, expected, "{selector}");
    }
}

#[gpui::test]
fn a_table_without_a_primary_key_gets_a_placeholder_where_and_a_hostile_column_name_is_quoted(cx: &mut TestAppContext) {
    let db = SqliteDb::new(GENERATE_SEED);
    let rig = oa_rig(cx, &db);
    let loose = oa_table(&rig, "loose");
    let before = gs_tab_count(&rig.app, rig.visual);
    gs_pick(&rig.app, rig.visual, &loose, "object-generate-update");
    let (.., sql) = gs_tab(&rig.app, rig.visual, before);
    // The quote is doubled; the parameter names are made valid and stay unique (`a b` and `a_b`).
    assert_eq!(
        sql,
        "UPDATE \"main\".\"loose\"\nSET\n  \"we\"\"ird\" = :we_ird,\n  \"a b\" = :a_b,\n  \"a_b\" = :a_b_2,\n  \"1st\" = :_1st\nWHERE <condition>;"
    );
    let before = gs_tab_count(&rig.app, rig.visual);
    gs_pick(&rig.app, rig.visual, &loose, "object-generate-delete");
    let (.., sql) = gs_tab(&rig.app, rig.visual, before);
    assert_eq!(sql, "DELETE FROM \"main\".\"loose\"\nWHERE <condition>;");
}

#[gpui::test]
fn a_view_offers_only_select_and_a_table_all_four(cx: &mut TestAppContext) {
    let db = SqliteDb::new(GENERATE_SEED);
    let rig = oa_rig(cx, &db);
    let (items, view) = (oa_table(&rig, "items"), oa_table(&rig, "item_names"));
    assert_eq!(
        gs_entries(&rig.app, rig.visual, &items),
        ["object-generate-select", "object-generate-insert", "object-generate-update", "object-generate-delete"]
    );
    assert_eq!(gs_entries(&rig.app, rig.visual, &view), ["object-generate-select"]);
    let before = gs_tab_count(&rig.app, rig.visual);
    gs_pick(&rig.app, rig.visual, &view, "object-generate-select");
    let (.., sql) = gs_tab(&rig.app, rig.visual, before);
    assert_eq!(sql, "SELECT \"name\"\nFROM \"main\".\"item_names\"\nLIMIT 100;");
}

#[gpui::test]
fn generate_sql_on_the_fixture_connections_quotes_for_each_dialect(cx: &mut TestAppContext) {
    let (app, visual, _, _) = oa_pg(cx);
    let before = gs_tab_count(&app, visual);
    gs_pick(&app, visual, &oa_pg_object("orders"), "object-generate-update");
    let (connection, database, schema, sql) = gs_tab(&app, visual, before);
    assert_eq!((connection.as_str(), database.as_str(), schema.as_deref()), (OA_PG, "warehouse", Some("public")));
    assert_eq!(
        sql,
        "UPDATE \"public\".\"orders\"\nSET\n  \"name\" = :name,\n  \"status\" = :status,\n  \"active\" = :active,\n  \"created\" = :created,\n  \"score\" = :score,\n  \"big_number\" = :big_number,\n  \"payload\" = :payload\nWHERE \"id\" = :id;"
    );
}

#[gpui::test]
fn a_mongodb_collection_has_no_generate_sql(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.open_or_activate_collection_tab(crate::ui_fixture::MONGO_ID, "fixture_db_0", "fixture_collection_01");
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    let collection = app.read_with(visual, |this, _| this.navigator.selected().cloned().expect("the located collection"));
    assert!(app.read_with(visual, |this, _| this.generate_sql_kinds(&collection).is_empty()));
    assert!(gs_entries(&app, visual, &collection).is_empty());
}

#[gpui::test]
fn a_clickhouse_table_offers_only_select_and_insert_because_its_primary_key_is_not_unique(cx: &mut TestAppContext) {
    let (app, visual, _, _) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    assert_eq!(gs_entries(&app, visual, &orders).len(), 4, "a PostgreSQL table offers all four");
    // The same table row, on a connection that is ClickHouse.
    app.update(visual, |this, _| {
        this.saved_connections.iter_mut().find(|saved| saved.matches_id(OA_PG)).expect("the fixture connection").kind = ConnectionKind::Clickhouse;
    });
    assert_eq!(gs_entries(&app, visual, &orders), ["object-generate-select", "object-generate-insert"]);
    let labels: Vec<_> = app.read_with(visual, |this, _| this.generate_sql_kinds(&orders).iter().map(|kind| kind.label()).collect());
    assert_eq!(labels, ["SELECT", "INSERT"]);
}
