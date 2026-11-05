// P6b: the navigator's object actions (New table, Drop, Rename, Truncate, New schema / database)
// through the real context menus, name dialog, new-table form and the shared review dialog.
//
// * The SQLite tests run the REAL SQLite driver behind a real host session and apply on the REAL
//   dedicated apply session, then read the file back with the `sqlite3` CLI.
// * The fixture tests walk the Postgres fixture tree with the injected executor (it logs the
//   statements instead of running them): CASCADE / RESTART IDENTITY, Production, read-only.
// * The live tests at the end are `#[ignore]`d (dedicated `tablex-test-*` containers only).

const ACTIONS_SEED: &str = "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); \
     INSERT INTO items(name) VALUES ('a'),('b'),('c'); \
     CREATE TABLE other(id INTEGER PRIMARY KEY); \
     CREATE VIEW item_names AS SELECT name FROM items;";

/// The SQLite rig with Tables and Views opened.
fn oa_rig<'a>(cx: &'a mut TestAppContext, db: &SqliteDb) -> SqlRig<'a> {
    let mut rig = nav_rig_sqlite(cx, db);
    rig.expand_to_tables("main", "main");
    let key = rig.key.clone();
    rig.expand(&group_node(&key, "main", "main", ObjectGroup::Views));
    rig
}

fn oa_table(rig: &SqlRig<'_>, name: &str) -> NavigatorNodeId {
    object_node(&rig.key, "main", "main", name)
}

/// Right-clicks the row of `id` and clicks the menu entry `selector`.
fn oa_menu_on(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId, selector: &'static str) {
    settle(visual);
    let index = app.read_with(visual, |this, _| this.navigator.visible_index(id)).unwrap_or_else(|| panic!("{id:?} is a visible row"));
    right_click_row(visual, index);
    settle(visual);
    let entry = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is in the menu"));
    visual.simulate_click(entry.center(), Default::default());
    settle(visual);
}

fn oa_menu(rig: &mut SqlRig<'_>, id: &NavigatorNodeId, selector: &'static str) {
    oa_menu_on(&rig.app, rig.visual, id, selector);
}

/// The selectors of the object-action entries the menu of `id` offers.
fn oa_entries(rig: &mut SqlRig<'_>, id: &NavigatorNodeId) -> Vec<&'static str> {
    oa_entries_on(&rig.app, rig.visual, id)
}

fn oa_entries_on(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) -> Vec<&'static str> {
    settle(visual);
    let index = app.read_with(visual, |this, _| this.navigator.visible_index(id)).unwrap_or_else(|| panic!("{id:?} is a visible row"));
    right_click_row(visual, index);
    settle(visual);
    let found = [
        "object-menu-new-table",
        "object-menu-export",
        "object-menu-transfer",
        "object-menu-import",
        "object-menu-new-schema",
        "object-menu-new-database",
        "object-menu-truncate",
        "object-menu-rename",
        "object-menu-drop",
    ]
    .into_iter()
    .filter(|selector| visual.debug_bounds(selector).is_some())
    .collect();
    visual.simulate_keystrokes("escape");
    settle(visual);
    found
}

/// Types into the input drawn at `selector` (replacing what it holds).
fn oa_type(visual: &mut gpui::VisualTestContext, selector: &'static str, text: &str) {
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
    visual.simulate_click(bounds.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

/// The SQL the review dialog shows (its Copy SQL).
fn oa_script(visual: &mut gpui::VisualTestContext) -> String {
    d3_copy_sql(visual)
}

fn oa_host(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> gpui::Entity<ObjectActionHost> {
    app.read_with(visual, |this, _| this.object_action.clone().expect("an object action is open"))
}

fn oa_wait_applied(rig: &mut SqlRig<'_>) {
    let host = oa_host(&rig.app, rig.visual);
    wait_until(rig.visual, "the apply to answer", |visual| host.read_with(visual, |host, _| !host.review_applying()));
    settle(rig.visual);
}

/// `oa_wait_applied`, and the run must have succeeded.
fn oa_wait_ok(rig: &mut SqlRig<'_>) {
    oa_wait_applied(rig);
    let host = oa_host(&rig.app, rig.visual);
    if let Some((report, true)) = host.read_with(rig.visual, |host, _| host.review_report()) {
        panic!("the apply failed: {report}");
    }
}

fn oa_tabs(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<(String, String)> {
    app.read_with(visual, |this, _| {
        this.workspace.tabs().iter().filter(|tab| tab.query_mode.is_table_data()).map(|tab| (tab.title.clone(), tab.collection.clone())).collect()
    })
}

fn oa_open_table_tab(rig: &mut SqlRig<'_>, name: &str) {
    let id = oa_table(rig, name);
    rig.double_click_node(&id);
    let (app, collection) = (rig.app.clone(), name.to_owned());
    wait_until(rig.visual, "the table tab", |visual| {
        app.read_with(visual, |this, _| this.workspace.active_tab().is_some_and(|tab| tab.collection == collection))
    });
    settle(rig.visual);
}

#[gpui::test]
fn the_menus_offer_the_actions_each_row_can_do(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let (table, view) = (oa_table(&rig, "items"), oa_table(&rig, "item_names"));
    assert_eq!(oa_entries(&mut rig, &table), ["object-menu-export", "object-menu-transfer", "object-menu-import", "object-menu-truncate", "object-menu-rename", "object-menu-drop"]);
    // SQLite cannot rename a view: the entry is not offered; a view is not truncated.
    assert_eq!(oa_entries(&mut rig, &view), ["object-menu-drop"]);
    assert_eq!(oa_entries(&mut rig, &group_node(&key, "main", "main", ObjectGroup::Tables)), ["object-menu-new-table", "object-menu-export", "object-menu-transfer", "object-menu-import"]);
    // A schema and a database offer a table; SQLite has no schema or database to create.
    assert_eq!(oa_entries(&mut rig, &schema_node(&key, "main", "main")), ["object-menu-new-table", "object-menu-export", "object-menu-transfer", "object-menu-import"]);
    assert_eq!(oa_entries(&mut rig, &database_node(&key, "main")), ["object-menu-new-table", "object-menu-export", "object-menu-transfer", "object-menu-import"]);
    // The Views group has no action of its own.
    assert!(oa_entries(&mut rig, &group_node(&key, "main", "main", ObjectGroup::Views)).is_empty());
}

#[gpui::test]
fn truncate_deletes_the_rows_after_an_acknowledged_review_that_says_sqlite_has_no_truncate(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let items = oa_table(&rig, "items");
    oa_menu(&mut rig, &items, "object-menu-truncate");

    assert!(rig.visual.debug_bounds("changes-subject").is_some(), "the review dialog is open");
    assert_eq!(oa_script(rig.visual), "DELETE FROM \"main\".\"items\";");
    let warnings = rig.visual.debug_bounds("changes-warning-0").is_some();
    assert!(warnings, "the warnings are listed");
    // SQLite has transactional DDL; the dialog says so, and there are no CASCADE boxes.
    assert!(rig.visual.debug_bounds("changes-mode").is_some());
    assert!(rig.visual.debug_bounds("changes-option-cascade").is_none());

    // Nothing runs before the acknowledgement.
    rig.click("changes-apply");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "3");
    rig.click("changes-understood");
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "0", "the rows are gone");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'items'"), "1", "the table stays");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "the dialog closed after the apply");
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn rename_validates_the_name_then_reviews_it_and_the_navigator_and_the_open_tab_follow(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    oa_open_table_tab(&mut rig, "items");
    assert_eq!(oa_tabs(&rig.app, rig.visual), [("items".to_owned(), "items".to_owned())]);
    let items = oa_table(&rig, "items");
    oa_menu(&mut rig, &items, "object-menu-rename");
    assert!(rig.visual.debug_bounds("object-name-input").is_some(), "the name dialog is open");

    // Refused with the reason, and the dialog stays: the current name, a name in use, a blank.
    for (typed, reason) in [("items", "current name"), ("other", "exists already"), (" padded", "starts or ends with a space")] {
        oa_type(rig.visual, "object-name-input", typed);
        rig.click("object-name-submit");
        let error = rig.visual.debug_bounds("object-name-error").unwrap_or_else(|| panic!("{typed:?} is refused"));
        let _ = error;
        let shown = rig.app.read_with(rig.visual, |this, _| this.object_action.is_none());
        assert!(shown, "no review was opened for {typed:?}");
        assert!(rig.visual.debug_bounds("object-name-input").is_some(), "the dialog stays");
        let _ = reason;
    }

    // Enter takes a good name to the review.
    oa_type(rig.visual, "object-name-input", "things");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("object-name-input").is_none(), "the name dialog is gone");
    assert_eq!(oa_script(rig.visual), "ALTER TABLE \"main\".\"items\" RENAME TO \"things\";");
    assert!(rig.visual.debug_bounds("changes-understood").is_none(), "a rename loses no data");
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);

    assert_eq!(db.sql("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name"), "other\nthings");
    assert_eq!(db.sql("SELECT count(*) FROM things"), "3");
    // Only the Tables group reloaded, in place: the Views group is still open with its row, and the
    // renamed table is the selected row.
    let (app, tables) = (rig.app.clone(), group_node(&key, "main", "main", ObjectGroup::Tables));
    wait_until(rig.visual, "the Tables group to list the new name", |visual| {
        app.read_with(visual, |this, _| {
            this.navigator.loaded_children(&tables).is_some_and(|children| children.contains(&object_node(&key, "main", "main", "things")))
        })
    });
    settle(rig.visual);
    assert_eq!(rig.tree_children(&tables), ["other", "things"]);
    assert_eq!(rig.tree_children(&group_node(&key, "main", "main", ObjectGroup::Views)), ["item_names"]);
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.navigator.selected(), Some(&object_node(&key, "main", "main", "things")));
    });
    // The open tab points at the new name, and shows its rows.
    assert_eq!(oa_tabs(&rig.app, rig.visual), [("things".to_owned(), "things".to_owned())]);
    rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.tabs().iter().find(|tab| tab.collection == "things").unwrap();
        assert_eq!(tab.sql, "SELECT * FROM \"main\".\"things\"");
    });
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn drop_needs_the_acknowledgement_and_closes_the_tabs_without_unsaved_work(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    oa_open_table_tab(&mut rig, "other");
    oa_open_table_tab(&mut rig, "items");
    assert_eq!(oa_tabs(&rig.app, rig.visual).len(), 2);

    let other = oa_table(&rig, "other");
    oa_menu(&mut rig, &other, "object-menu-drop");
    assert_eq!(oa_script(rig.visual), "DROP TABLE \"main\".\"other\";");
    assert!(rig.visual.debug_bounds("changes-understood").is_some(), "dropping a table loses data");
    // SQLite has no CASCADE: the box is not offered.
    assert!(rig.visual.debug_bounds("changes-option-cascade").is_none());
    rig.click("changes-apply");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'other'"), "1", "not without the acknowledgement");
    rig.click("changes-understood");
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);

    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'other'"), "0");
    let (app, tables) = (rig.app.clone(), group_node(&key, "main", "main", ObjectGroup::Tables));
    wait_until(rig.visual, "the Tables group to reload", |visual| {
        app.read_with(visual, |this, _| this.navigator.loaded_children(&tables).is_some_and(|children| children.len() == 1))
    });
    settle(rig.visual);
    assert_eq!(rig.tree_children(&tables), ["items"]);
    // The dropped table's tab closed, the other table's stays.
    assert_eq!(oa_tabs(&rig.app, rig.visual), [("items".to_owned(), "items".to_owned())]);
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());

    // A view is dropped the same way, from the Views group.
    let view = oa_table(&rig, "item_names");
    oa_menu(&mut rig, &view, "object-menu-drop");
    assert_eq!(oa_script(rig.visual), "DROP VIEW \"main\".\"item_names\";");
    rig.click("changes-understood");
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'item_names'"), "0");
}

#[gpui::test]
fn new_table_checks_the_form_then_reviews_and_creates_it_and_the_navigator_selects_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let tables = group_node(&key, "main", "main", ObjectGroup::Tables);
    oa_menu(&mut rig, &tables, "object-menu-new-table");
    assert!(rig.visual.debug_bounds("object-table-name").is_some(), "the form is open");
    assert!(rig.visual.debug_bounds("object-column-0").is_some(), "it starts with an id column");
    // SQLite has no column comments: that cell is not offered.
    assert!(rig.visual.debug_bounds("object-column-comment-0").is_none());

    let refused = |rig: &mut SqlRig<'_>| {
        rig.click("object-table-submit");
        assert!(rig.visual.debug_bounds("object-table-error").is_some(), "the form says what is wrong");
        assert!(rig.visual.debug_bounds("object-table-name").is_some(), "and stays open");
        assert!(rig.app.read_with(rig.visual, |this, _| this.object_action.is_none()), "no review");
    };
    // No name; a name in use.
    refused(&mut rig);
    oa_type(rig.visual, "object-table-name", "items");
    refused(&mut rig);
    oa_type(rig.visual, "object-table-name", "gadgets");
    // A type that smuggles in a second statement is refused by the dialect's own checks.
    rig.click("object-table-add-column");
    oa_type(rig.visual, "object-column-name-1", "label");
    oa_type(rig.visual, "object-column-type-1", "text); DROP TABLE items; --");
    refused(&mut rig);
    // A column twice.
    oa_type(rig.visual, "object-column-type-1", "TEXT");
    oa_type(rig.visual, "object-column-name-1", "id");
    refused(&mut rig);
    oa_type(rig.visual, "object-column-name-1", "label");
    oa_type(rig.visual, "object-column-default-1", "none yet");

    rig.click("object-table-submit");
    assert!(rig.visual.debug_bounds("object-table-name").is_none(), "the form is gone");
    assert_eq!(
        oa_script(rig.visual),
        "CREATE TABLE \"main\".\"gadgets\" (\n  \"id\" INTEGER NOT NULL,\n  \"label\" TEXT DEFAULT 'none yet',\n  PRIMARY KEY (\"id\")\n);"
    );
    assert!(rig.visual.debug_bounds("changes-understood").is_none(), "a new table loses nothing");
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);
    assert!(db.sql("SELECT sql FROM sqlite_master WHERE name = 'gadgets'").contains("PRIMARY KEY"));
    assert_eq!(db.sql("SELECT count(*) FROM items"), "3", "the other table was not touched");

    // The Tables group reloaded, and the new table is the selected row.
    let app = rig.app.clone();
    let new = object_node(&key, "main", "main", "gadgets");
    wait_until(rig.visual, "the new table to be selected", |visual| app.read_with(visual, |this, _| this.navigator.selected() == Some(&new)));
    settle(rig.visual);
    assert_eq!(rig.tree_children(&tables), ["gadgets", "items", "other"]);
    // And it opens.
    oa_open_table_tab(&mut rig, "gadgets");
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn a_table_comment_is_refused_by_sqlite_in_the_form_and_escape_dismisses_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let schema = schema_node(&rig.key, "main", "main");
    oa_menu(&mut rig, &schema, "object-menu-new-table");
    oa_type(rig.visual, "object-table-name", "gadgets");
    oa_type(rig.visual, "object-table-comment", "what they are");
    rig.click("object-table-submit");
    assert!(rig.visual.debug_bounds("object-table-error").is_some(), "SQLite has no table comments");
    assert!(rig.app.read_with(rig.visual, |this, _| this.object_action.is_none()));
    // Escape dismisses the form without a review, and nothing was created.
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("object-table-name").is_none());
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'gadgets'"), "0");
}

// ---- Postgres fixture tree, injected executor ---------------------------------------------------

const OA_PG: &str = crate::ui_fixture::POSTGRES_ID;

/// The Postgres fixture tree with Tables and Materialized Views opened, and an executor that logs.
fn oa_pg(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, PeLog, PeFail) {
    cx.dispatcher.allow_parking();
    // Dialog open animations run on wall-clock time: under a loaded machine they would move the
    // controls between measuring and clicking.
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = fixture_postgres_schema(cx);
    visual.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(900.0)));
    // The Mongo sample tree above would push the Postgres rows off the screen.
    app.update(visual, |this, _| {
        this.navigator.toggle_expansion(&NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() });
    });
    settle(visual);
    let (ran, fail) = pe_inject(&app, visual);
    (app, visual, ran, fail)
}

fn oa_pg_object(name: &str) -> NavigatorNodeId {
    object_node(OA_PG, "warehouse", "public", name)
}

fn oa_click(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    diagram_click(visual, selector);
}

fn oa_wait_host(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    let host = oa_host(app, visual);
    wait_until(visual, "the apply to answer", |visual| host.read_with(visual, |host, _| !host.review_applying()));
    settle(visual);
}

#[gpui::test]
fn postgres_drop_offers_cascade_as_an_opt_in_box_that_changes_the_sql_and_asks_again(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    oa_menu_on(&app, visual, &oa_pg_object("orders"), "object-menu-drop");
    assert_eq!(oa_script(visual), "DROP TABLE \"public\".\"orders\";");
    assert!(visual.debug_bounds("changes-option-cascade").is_some(), "PostgreSQL has CASCADE");
    assert!(visual.debug_bounds("changes-mode").is_some());

    // Ticking CASCADE rewrites the SQL, and the data-loss acknowledgement has to be given again.
    oa_click(visual, "changes-understood");
    oa_click(visual, "changes-option-cascade");
    assert_eq!(oa_script(visual), "DROP TABLE \"public\".\"orders\" CASCADE;");
    oa_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "the acknowledgement was reset by the edit");
    oa_click(visual, "changes-understood");
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    // What ran is the reviewed SQL, inside a transaction, on the apply session.
    let statements = ran.lock().unwrap().clone();
    assert_eq!(statements.first().map(String::as_str), Some("BEGIN"), "{statements:?}");
    assert_eq!(statements.last().map(String::as_str), Some("COMMIT"), "{statements:?}");
    assert!(statements.iter().any(|sql| sql == "DROP TABLE \"public\".\"orders\" CASCADE"), "{statements:?}");
    assert!(visual.debug_bounds("changes-subject").is_none(), "the dialog closed after the apply");
}

#[gpui::test]
fn postgres_truncate_offers_restart_identity_and_cascade_both_off_by_default(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    oa_menu_on(&app, visual, &oa_pg_object("orders"), "object-menu-truncate");
    assert_eq!(oa_script(visual), "TRUNCATE TABLE \"public\".\"orders\";");
    oa_click(visual, "changes-option-restart_identity");
    assert_eq!(oa_script(visual), "TRUNCATE TABLE \"public\".\"orders\" RESTART IDENTITY;");
    oa_click(visual, "changes-option-cascade");
    assert_eq!(oa_script(visual), "TRUNCATE TABLE \"public\".\"orders\" RESTART IDENTITY CASCADE;");
    oa_click(visual, "changes-option-restart_identity");
    assert_eq!(oa_script(visual), "TRUNCATE TABLE \"public\".\"orders\" CASCADE;");
    oa_click(visual, "changes-understood");
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    let statements = ran.lock().unwrap().clone();
    assert!(statements.iter().any(|sql| sql == "TRUNCATE TABLE \"public\".\"orders\" CASCADE"), "{statements:?}");
}

#[gpui::test]
fn a_materialized_view_is_renamed_and_dropped_with_its_own_keywords(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    // The 20 sample tables push the group below the first screenful: close them.
    app.update(visual, |this, _| {
        this.navigator.toggle_expansion(&fixture_group(ObjectGroup::Tables));
    });
    fixture_children(&app, visual, ObjectGroup::MaterializedViews);
    settle(visual);
    let view = oa_pg_object("monthly_revenue");
    assert_eq!(oa_entries_on(&app, visual, &view), ["object-menu-rename", "object-menu-drop"]);
    oa_menu_on(&app, visual, &view, "object-menu-rename");
    oa_type(visual, "object-name-input", "yearly_revenue");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(oa_script(visual), "ALTER MATERIALIZED VIEW \"public\".\"monthly_revenue\" RENAME TO \"yearly_revenue\";");
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    assert!(ran.lock().unwrap().iter().any(|sql| sql == "ALTER MATERIALIZED VIEW \"public\".\"monthly_revenue\" RENAME TO \"yearly_revenue\""));

    oa_menu_on(&app, visual, &view, "object-menu-drop");
    assert_eq!(oa_script(visual), "DROP MATERIALIZED VIEW \"public\".\"monthly_revenue\";");
    assert!(visual.debug_bounds("changes-option-cascade").is_some());
}

#[gpui::test]
fn a_production_connection_asks_for_its_confirmation_and_a_read_only_one_refuses(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, ran, _) = oa_pg(cx);
    publish_types(&app, visual, false);
    give_profile(&app, visual, OA_PG, typed(PRODUCTION));
    oa_menu_on(&app, visual, &oa_pg_object("orders"), "object-menu-drop");
    assert!(visual.debug_bounds("changes-confirm").is_some(), "Production asks for a named confirmation");
    oa_click(visual, "changes-understood");
    oa_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "without the confirmation nothing runs");
    oa_click(visual, "changes-confirm");
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    assert!(ran.lock().unwrap().iter().any(|sql| sql.starts_with("DROP TABLE")));
    // Dismissed with Escape; the next action starts from a fresh dialog.
    ran.lock().unwrap().clear();

    // The same connection read-only: the review says why and nothing can run, whatever is ticked.
    publish_types(&app, visual, true);
    oa_menu_on(&app, visual, &oa_pg_object("orders"), "object-menu-truncate");
    assert!(visual.debug_bounds("changes-disabled").is_some(), "the read-only reason is shown");
    oa_click(visual, "changes-understood");
    oa_click(visual, "changes-confirm");
    oa_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "a read-only connection refuses");
    let host = oa_host(&app, visual);
    assert!(host.read_with(visual, |host, _| host.draft_changes().len()) == 1, "the change is still pending");
}

#[gpui::test]
fn postgres_new_schema_is_named_reviewed_and_run_in_a_transaction(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    let schema = schema_node(OA_PG, "warehouse", "public");
    assert_eq!(oa_entries_on(&app, visual, &schema), ["object-menu-new-table", "object-menu-export", "object-menu-transfer", "object-menu-import", "object-menu-new-schema", "object-menu-new-database"]);
    oa_menu_on(&app, visual, &schema, "object-menu-new-schema");
    // A schema the database has already is refused where the user types it.
    oa_type(visual, "object-name-input", "public");
    oa_click(visual, "object-name-submit");
    assert!(visual.debug_bounds("object-name-error").is_some());
    oa_type(visual, "object-name-input", "re\"po;rts");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(oa_script(visual), "CREATE SCHEMA \"re\"\"po;rts\";");
    assert!(visual.debug_bounds("changes-understood").is_none());
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    let statements = ran.lock().unwrap().clone();
    assert_eq!(statements.first().map(String::as_str), Some("BEGIN"), "{statements:?}");
    assert!(statements.iter().any(|sql| sql == "CREATE SCHEMA \"re\"\"po;rts\""), "{statements:?}");
    assert_eq!(statements.last().map(String::as_str), Some("COMMIT"), "{statements:?}");
}

#[gpui::test]
fn postgres_new_table_form_offers_comments_types_and_keeps_a_key_column_not_null(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    let tables = fixture_group(ObjectGroup::Tables);
    oa_menu_on(&app, visual, &tables, "object-menu-new-table");
    assert!(visual.debug_bounds("object-column-comment-0").is_some(), "PostgreSQL has column comments");
    oa_type(visual, "object-table-name", "ledger");
    oa_type(visual, "object-table-comment", "who owes what");
    // The id column is the key; a key column cannot be NULL.
    // The caret lists the dialect's types: the second one, by keyboard, is bigint.
    oa_click(visual, "object-column-types-0");
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    oa_click(visual, "object-table-add-column");
    oa_type(visual, "object-column-name-1", "amount");
    oa_type(visual, "object-column-type-1", "numeric(12,2)");
    oa_type(visual, "object-column-comment-1", "in cents");
    oa_click(visual, "object-column-key-1");
    oa_click(visual, "object-table-submit");
    assert!(visual.debug_bounds("object-table-name").is_none(), "{:?}", visual.debug_bounds("object-table-error"));
    let script = oa_script(visual);
    for expected in [
        "CREATE TABLE \"public\".\"ledger\" (\n  \"id\" bigint NOT NULL,\n  \"amount\" numeric(12,2) NOT NULL,\n  PRIMARY KEY (\"id\", \"amount\")\n);",
        "COMMENT ON COLUMN \"public\".\"ledger\".\"amount\" IS 'in cents';",
        "COMMENT ON TABLE \"public\".\"ledger\" IS 'who owes what';",
    ] {
        assert!(script.contains(expected), "{expected}\nin\n{script}");
    }
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    let statements = ran.lock().unwrap().clone();
    assert!(statements.iter().any(|sql| sql == "COMMENT ON TABLE \"public\".\"ledger\" IS 'who owes what'"), "{statements:?}");
    assert_eq!(statements.last().map(String::as_str), Some("COMMIT"));
}

#[gpui::test]
fn a_tab_with_unsaved_work_is_not_closed_by_a_drop_nor_pointed_at_a_new_name(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    oa_open_table_tab(&mut rig, "items");
    oa_open_table_tab(&mut rig, "other");
    // Staged row edits on `items` (rows are not committed).
    rig.app.update(rig.visual, |this, _| {
        let tab = this.workspace.tabs().iter().find(|tab| tab.collection == "items").unwrap().id;
        this.workspace.runtime_mut(tab).unwrap().pending_deleted_rows.insert(0);
    });
    let items = oa_table(&rig, "items");
    oa_menu(&mut rig, &items, "object-menu-rename");
    oa_type(rig.visual, "object-name-input", "things");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);
    assert_eq!(db.sql("SELECT count(*) FROM things"), "3");
    // The tab with staged edits keeps its identity: it is not silently re-pointed.
    let names: Vec<String> = oa_tabs(&rig.app, rig.visual).into_iter().map(|(_, collection)| collection).collect();
    assert_eq!(names, ["items", "other"]);

    // A drop of the table whose tab has no unsaved work closes that tab only.
    let other = oa_table(&rig, "other");
    oa_menu(&mut rig, &other, "object-menu-drop");
    rig.click("changes-understood");
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);
    let names: Vec<String> = oa_tabs(&rig.app, rig.visual).into_iter().map(|(_, collection)| collection).collect();
    assert_eq!(names, ["items"]);
}

#[gpui::test]
fn a_failed_apply_keeps_the_dialog_with_the_report_and_changes_nothing(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let items = oa_table(&rig, "items");
    oa_menu(&mut rig, &items, "object-menu-rename");
    oa_type(rig.visual, "object-name-input", "things");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    // The table goes away behind the dialog's back: the apply's own check refuses, nothing runs.
    db.sql("DROP TABLE items");
    rig.click("changes-apply");
    oa_wait_applied(&mut rig);
    assert!(rig.visual.debug_bounds("changes-report").is_some(), "the report is shown");
    assert!(rig.visual.debug_bounds("changes-subject").is_some(), "the dialog stays");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'things'"), "0");
    let host = oa_host(&rig.app, rig.visual);
    let (report, failed) = host.read_with(rig.visual, |host, _| host.review_report()).expect("a report");
    assert!(failed && report.contains("no longer exists"), "{report}");
    assert_eq!(host.read_with(rig.visual, |host, _| host.draft_changes().len()), 1, "the change is kept");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.diagram.applies.len()), 0, "the connection is free again");
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) --------------------

/// Drops what a live test created, on success and on a panic alike.
struct OaCleanup(std::sync::Arc<engine_driver_host::SharedDriverSession>, Vec<String>);

impl Drop for OaCleanup {
    fn drop(&mut self) {
        for sql in &self.1 {
            exec_confirmed(&self.0, sql);
        }
    }
}

/// Create Database from the connection menu's entry: name, review, apply (it must have run).
fn oa_create_database(rig: &mut SqlRig<'_>, name: &str) {
    let key = rig.key.clone();
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_create_database_dialog(&key, window, cx)));
    settle(rig.visual);
    oa_type(rig.visual, "object-name-input", name);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.click("changes-apply");
    oa_wait_ok(rig);
}

fn oa_scalar(rig: &mut SqlRig<'_>, sql: &str) -> String {
    run_confirmed(rig, sql);
    let tab = rig.tab;
    rig.tab_cell(tab)
}

fn oa_wait_selected(rig: &mut SqlRig<'_>, id: &NavigatorNodeId) {
    for _ in 0..1500 {
        rig.visual.run_until_parked();
        if rig.app.read_with(rig.visual, |this, _| this.navigator.selected() == Some(id)) {
            settle(rig.visual);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let (selected, error, children) = rig.app.read_with(rig.visual, |this, _| {
        (
            this.navigator.selected().cloned(),
            this.sidebar.locate_error.clone(),
            this.navigator.visible_rows().into_iter().map(|row| format!("{}:{:?}", row.label, row.load)).collect::<Vec<_>>(),
        )
    });
    panic!("{id:?} was never selected; selected {selected:?}, locate error {error:?}, rows {children:?}");
}

/// Create a schema / database, a table in it, truncate it, rename it to a hostile name and drop it,
/// all through the menus and the review on one live server; PostgreSQL also drops a table a view
/// depends on (refused, then with CASCADE).
fn live_object_actions(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let postgres = kind == ConnectionKind::Postgres;
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    // `cf_` is the name pattern the MySQL / MariaDB test users may create.
    let scratch = format!("cf_p6b_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]);
    let mut rig = nav_rig(cx, kind, driver, &uri);
    let key = rig.key.clone();
    let _cleanup = OaCleanup(
        rig.session.clone(),
        vec![if postgres { format!("DROP SCHEMA IF EXISTS {scratch} CASCADE") } else { format!("DROP DATABASE IF EXISTS {scratch}") }],
    );
    let dialect = crate::sql_ddl::dialect_for(kind).unwrap();
    let quote = |name: &str| dialect.quote(name);
    let (ns_database, ns_schema) = if postgres { (database.clone(), scratch.clone()) } else { (scratch.clone(), scratch.clone()) };

    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_children(&connection_node(&key));
    rig.expand(&database_node(&key, &database));

    // 1. New schema (PostgreSQL) / New database (MySQL, MariaDB), from the database row.
    let entry = if postgres { "object-menu-new-schema" } else { "object-menu-new-database" };
    oa_menu(&mut rig, &database_node(&key, &database), entry);
    oa_type(rig.visual, "object-name-input", &scratch);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    let created = oa_script(rig.visual);
    assert_eq!(created, format!("CREATE {} {};", if postgres { "SCHEMA" } else { "DATABASE" }, quote(&scratch)));
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    let namespace = if postgres { schema_node(&key, &database, &scratch) } else { database_node(&key, &scratch) };
    oa_wait_selected(&mut rig, &namespace);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM information_schema.schemata WHERE schema_name = '{scratch}'")), "1");

    // Objects the tree has to list when the group opens: a table, and (PostgreSQL) a view on it.
    let clickhouse = kind == ConnectionKind::Clickhouse;
    run_confirmed(
        &mut rig,
        &if clickhouse {
            format!("CREATE TABLE {scratch}.t_base (id Int32) ENGINE = MergeTree ORDER BY id")
        } else {
            format!("CREATE TABLE {scratch}.t_base(id int primary key)")
        },
    );
    if postgres {
        run_confirmed(&mut rig, &format!("CREATE VIEW {scratch}.v_dep AS SELECT id FROM {scratch}.t_base"));
    }
    if !postgres {
        rig.expand(&database_node(&key, &scratch));
    }
    rig.expand(&schema_node(&key, &ns_database, &ns_schema));
    let tables = group_node(&key, &ns_database, &ns_schema, ObjectGroup::Tables);
    rig.expand(&tables);
    assert_eq!(rig.tree_children(&tables), ["t_base"]);

    // 2. New table, from the Tables group.
    oa_menu(&mut rig, &tables, "object-menu-new-table");
    oa_type(rig.visual, "object-table-name", "t_one");
    oa_type(rig.visual, "object-table-comment", "it's a table; -- not a comment");
    rig.click("object-table-add-column");
    oa_type(rig.visual, "object-column-name-1", "label");
    oa_type(rig.visual, "object-column-type-1", "varchar(20)");
    rig.click("object-table-submit");
    assert!(rig.visual.debug_bounds("object-table-error").is_none(), "the form is accepted");
    let qualified = |name: &str| format!("{}.{}", quote(&ns_schema), quote(name));
    assert!(oa_script(rig.visual).starts_with(&format!("CREATE TABLE {} (", qualified("t_one"))));
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    oa_wait_selected(&mut rig, &object_node(&key, &ns_database, &ns_schema, "t_one"));
    let comment_sql = if postgres {
        format!("SELECT obj_description('{scratch}.t_one'::regclass)")
    } else if clickhouse {
        format!("SELECT comment FROM system.tables WHERE database = '{scratch}' AND name = 't_one'")
    } else {
        format!("SELECT table_comment FROM information_schema.tables WHERE table_schema = '{scratch}' AND table_name = 't_one'")
    };
    assert_eq!(oa_scalar(&mut rig, &comment_sql), "it's a table; -- not a comment");
    run_confirmed(&mut rig, &format!("INSERT INTO {scratch}.t_one VALUES (1, 'a'), (2, 'b')"));
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {scratch}.t_one")), "2");

    // 3. Truncate.
    oa_menu(&mut rig, &object_node(&key, &ns_database, &ns_schema, "t_one"), "object-menu-truncate");
    assert_eq!(oa_script(rig.visual), format!("TRUNCATE TABLE {};", qualified("t_one")));
    rig.click("changes-understood");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {scratch}.t_one")), "0");

    // 4. Rename to a name made of every character that could end an identifier or a statement.
    // Quotes, a backslash right before a backtick, a semicolon and a comment marker. ClickHouse
    // reads that backslash as an escape and its reads cannot quote it, so there the name is
    // refused in the dialog and the rest of the name is used.
    let backslashed = "t_ho\"st;il\\`e -- x";
    oa_menu(&mut rig, &object_node(&key, &ns_database, &ns_schema, "t_one"), "object-menu-rename");
    let hostile = if kind == ConnectionKind::Clickhouse {
        oa_type(rig.visual, "object-name-input", backslashed);
        rig.visual.simulate_keystrokes("enter");
        settle(rig.visual);
        assert!(rig.visual.debug_bounds("object-name-error").is_some(), "a backslash is refused where it is typed");
        assert!(rig.visual.debug_bounds("changes-copy").is_none(), "nothing is reviewed");
        "t_ho\"st;il`e -- x"
    } else {
        backslashed
    };
    oa_type(rig.visual, "object-name-input", hostile);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    let script = oa_script(rig.visual);
    assert!(script.contains(&quote(hostile)) && script.matches(';').count() == 2, "{script}");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    let renamed = object_node(&key, &ns_database, &ns_schema, hostile);
    oa_wait_selected(&mut rig, &renamed);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM {}", qualified(hostile))), "0");
    let mut listed = rig.tree_children(&tables);
    listed.sort();
    assert_eq!(listed, ["t_base".to_string(), hostile.to_string()]);

    // 5. Drop it.
    oa_menu(&mut rig, &renamed, "object-menu-drop");
    assert_eq!(oa_script(rig.visual), format!("DROP TABLE {};", qualified(hostile)));
    rig.click("changes-understood");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{scratch}' AND table_name = 't_base'")), "1");
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{scratch}' AND table_name <> 't_base' AND table_name <> 'v_dep'")), "0");

    // 6. PostgreSQL: the view depends on t_base. The drop is refused by the server, nothing changes;
    // with CASCADE (opt in) it goes through.
    if postgres {
        let views = group_node(&key, &ns_database, &ns_schema, ObjectGroup::Views);
        rig.expand(&views);
        assert_eq!(rig.tree_children(&views), ["v_dep"]);
        let base = object_node(&key, &ns_database, &ns_schema, "t_base");
        oa_menu(&mut rig, &base, "object-menu-drop");
        rig.click("changes-understood");
        rig.click("changes-apply");
        oa_wait_applied(&mut rig);
        assert!(rig.visual.debug_bounds("changes-report").is_some(), "the refusal is reported");
        assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{scratch}'")), "2");
        rig.click("changes-option-cascade");
        rig.click("changes-understood");
        rig.click("changes-apply");
        oa_wait_ok(&mut rig);
        assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{scratch}'")), "0");
        // The view CASCADE dropped is gone from the Views group too, which reloaded on its own.
        let (app, views_group) = (rig.app.clone(), views.clone());
        wait_until(rig.visual, "the Views group to reload", |visual| {
            app.read_with(visual, |this, _| this.navigator.loaded_children(&views_group).is_some_and(|children| children.is_empty()))
        });
    }
    // The only error the Error Log heard of is the refused drop of PostgreSQL.
    let expected: Vec<Option<String>> = if postgres { vec![Some("query.failed".to_owned())] } else { Vec::new() };
    assert_eq!(rig.navigator_errors(), expected);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_object_actions(cx: &mut TestAppContext) {
    live_object_actions(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_object_actions(cx: &mut TestAppContext) {
    live_object_actions(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI (container tablex-test-clickhouse)"]
fn live_clickhouse_object_actions(cx: &mut TestAppContext) {
    live_object_actions(cx, ConnectionKind::Clickhouse, CLICKHOUSE_DRIVER, "TABLEX_TEST_CLICKHOUSE_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_object_actions(cx: &mut TestAppContext) {
    live_object_actions(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// An object action never runs on a SQL tab's manual transaction: tab A holds an uncommitted row,
/// a rename (and a truncate of another table) is applied from the review dialog on the dedicated
/// session, and A's transaction is still open with its row, uncommitted, afterwards.
fn live_manual_transaction_is_not_touched(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let postgres = kind == ConnectionKind::Postgres;
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let schema = if postgres { "public".to_owned() } else { database.clone() };
    let suffix = &uuid::Uuid::new_v4().to_simple().to_string()[..8];
    let (held, moved, renamed) = (format!("p6b_held_{suffix}"), format!("p6b_move_{suffix}"), format!("p6b_moved_{suffix}"));
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let _cleanup = OaCleanup(
        rig.session.clone(),
        [&held, &moved, &renamed].iter().map(|name| format!("DROP TABLE IF EXISTS {name}")).collect(),
    );
    let (a, b) = (rig.tab, rig.second_tab());
    rig.run_on(a, &format!("CREATE TABLE {held}(id int PRIMARY KEY)"));
    rig.run_on(a, &format!("CREATE TABLE {moved}(id int PRIMARY KEY)"));
    rig.activate(a);
    rig.click("tx-mode-manual");
    rig.run_on(a, &format!("INSERT INTO {held} VALUES (1)"));
    let own = rig.own_session(a).expect("tab A has its own session");
    assert_eq!(rig.pending(a), 1);

    // Rename `moved` through the review dialog.
    let origin = crate::app::structure_edit::apply::TableOrigin {
        connection: rig.key.clone(),
        database: database.clone(),
        schema: schema.clone(),
        object: moved.clone(),
    };
    let table = crate::sql_ddl::TableId { schema: Some(schema.clone()), name: moved.clone() };
    let to = renamed.clone();
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| {
            this.open_object_review(origin, move |_, _| vec![Change::RenameTable { table, to }], window, cx)
        });
    });
    settle(rig.visual);
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);

    // The rename happened (tab B, auto-commit, sees it); tab A's transaction was not joined or ended.
    let tables = format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{schema}' AND table_name = '{renamed}'");
    rig.run_on(b, &tables);
    assert_eq!(rig.tab_cell(b), "1", "the rename was applied");
    assert_eq!(rig.own_session(a), Some(own), "tab A kept its session");
    assert_eq!(rig.pending(a), 1, "and its open transaction");
    rig.run_on(b, &format!("SELECT count(*) FROM {held}"));
    assert_eq!(rig.tab_cell(b), "0", "A's row is still uncommitted");
    rig.activate(a);
    rig.click("tx-rollback");
    rig.wait_tx("the rollback", a, |tx| tx.statements == 0);
    rig.run_on(b, &format!("SELECT count(*) FROM {held}"));
    assert_eq!(rig.tab_cell(b), "0", "the rollback still works on the row");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_object_actions_leave_a_manual_transaction_alone(cx: &mut TestAppContext) {
    live_manual_transaction_is_not_touched(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_object_actions_leave_a_manual_transaction_alone(cx: &mut TestAppContext) {
    live_manual_transaction_is_not_touched(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_object_actions_leave_a_manual_transaction_alone(cx: &mut TestAppContext) {
    live_manual_transaction_is_not_touched(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

#[gpui::test]
fn the_connection_menus_create_database_is_reviewed_and_obeys_production_and_read_only(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, ran, _) = oa_pg(cx);
    app.update(visual, |this, cx| {
        let epoch = this.sessions.begin_connect(OA_PG).unwrap();
        this.sessions.finish_connect(OA_PG, epoch, Ok(()));
        fake_create_database_capability(this, OA_PG);
        cx.notify();
    });
    publish_types(&app, visual, false);
    give_profile(&app, visual, OA_PG, typed(PRODUCTION));
    let open = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        visual.update(|window, cx| app.update(cx, |this, cx| this.open_create_database_dialog(OA_PG, window, cx)));
        settle(visual);
    };
    open(&app, visual);
    assert!(visual.debug_bounds("object-name-input").is_some(), "the name dialog is open");
    oa_type(visual, "object-name-input", "reports");
    visual.simulate_keystrokes("enter");
    settle(visual);
    // PostgreSQL cannot run CREATE DATABASE in a transaction: the review says so and shows the SQL.
    assert_eq!(oa_script(visual), "CREATE DATABASE \"reports\";");
    assert!(visual.debug_bounds("changes-mode").is_some());
    assert!(visual.debug_bounds("changes-confirm").is_some(), "Production asks for its confirmation");
    oa_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "nothing runs without it");
    oa_click(visual, "changes-confirm");
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    // The reviewed statement is the only thing that ran: no BEGIN around it.
    assert_eq!(ran.lock().unwrap().clone(), ["CREATE DATABASE \"reports\""]);

    // Read-only: the review refuses.
    ran.lock().unwrap().clear();
    publish_types(&app, visual, true);
    open(&app, visual);
    oa_type(visual, "object-name-input", "again");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(visual.debug_bounds("changes-disabled").is_some());
    oa_click(visual, "changes-confirm");
    oa_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty());
}

#[gpui::test]
fn a_database_created_through_the_review_joins_the_connections_visible_object_filter(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::VisibleObjectFilter;
    let (app, visual, ran, _) = oa_pg(cx);
    app.update(visual, |this, cx| {
        let epoch = this.sessions.begin_connect(OA_PG).unwrap();
        this.sessions.finish_connect(OA_PG, epoch, Ok(()));
        fake_create_database_capability(this, OA_PG);
        this.sidebar.path = Some(std::env::temp_dir().join(format!("tablex-pg-create-db-{}.json", uuid::Uuid::new_v4())));
        this.change_sidebar_preferences(
            |prefs| prefs.set_visible_filter(OA_PG, VisibleObjectFilter { databases: Some(vec!["warehouse".into()]), ..Default::default() }),
            cx,
        )
        .unwrap();
    });
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_create_database_dialog(OA_PG, window, cx)));
    settle(visual);
    oa_type(visual, "object-name-input", "reports");
    visual.simulate_keystrokes("enter");
    settle(visual);
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    assert_eq!(ran.lock().unwrap().clone(), ["CREATE DATABASE \"reports\""]);
    app.read_with(visual, |this, _| {
        let filter = &this.navigator.preferences().visible_filters[OA_PG];
        assert_eq!(filter.databases.as_deref(), Some(&["warehouse".to_string(), "reports".to_string()][..]));
    });
}

#[gpui::test]
fn a_schema_created_through_the_review_joins_its_databases_schema_whitelist(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::VisibleObjectFilter;
    let (app, visual, _, _) = oa_pg(cx);
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(std::env::temp_dir().join(format!("tablex-pg-create-schema-{}.json", uuid::Uuid::new_v4())));
        let schemas = [("warehouse".to_string(), Some(vec!["public".to_string()]))].into_iter().collect();
        this.change_sidebar_preferences(|prefs| prefs.set_visible_filter(OA_PG, VisibleObjectFilter { schemas, ..Default::default() }), cx)
            .unwrap();
    });
    oa_menu_on(&app, visual, &schema_node(OA_PG, "warehouse", "public"), "object-menu-new-schema");
    oa_type(visual, "object-name-input", "reports");
    visual.simulate_keystrokes("enter");
    settle(visual);
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    app.read_with(visual, |this, _| {
        let filter = &this.navigator.preferences().visible_filters[OA_PG];
        assert_eq!(filter.schemas["warehouse"].as_deref(), Some(&["public".to_string(), "reports".to_string()][..]));
    });
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_create_database_runs_reviewed_outside_a_transaction(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI");
        return;
    };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let scratch = format!("cf_p6b_db_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]);
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let key = rig.key.clone();
    let _cleanup = OaCleanup(rig.session.clone(), vec![format!("DROP DATABASE IF EXISTS {scratch}")]);
    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_children(&connection_node(&key));
    // The connection shows one database only (a Visible Object Filter): the new one must join it,
    // or it would stay hidden and the reveal below would fail.
    let wanted = database.clone();
    rig.app.update(rig.visual, |this, cx| {
        this.sidebar.path = Some(std::env::temp_dir().join(format!("tablex-live-create-db-{}.json", uuid::Uuid::new_v4())));
        this.change_sidebar_preferences(
            |prefs| prefs.set_visible_filter(&key, crate::sidebar_preferences::VisibleObjectFilter { databases: Some(vec![wanted]), ..Default::default() }),
            cx,
        )
        .unwrap();
    });
    settle(rig.visual);
    oa_menu(&mut rig, &database_node(&key, &database), "object-menu-new-database");
    oa_type(rig.visual, "object-name-input", &scratch);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert_eq!(oa_script(rig.visual), format!("CREATE DATABASE \"{scratch}\";"));
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    oa_wait_selected(&mut rig, &database_node(&key, &scratch));
    rig.app.read_with(rig.visual, |this, _| {
        let filter = &this.navigator.preferences().visible_filters[&key];
        assert_eq!(filter.databases.as_deref(), Some(&[database.clone(), scratch.clone()][..]));
    });
    assert_eq!(oa_scalar(&mut rig, &format!("SELECT count(*) FROM pg_database WHERE datname = '{scratch}'")), "1");
}

#[gpui::test]
fn a_connection_that_becomes_production_while_the_review_is_open_is_not_applied_unconfirmed(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, ran, _) = oa_pg(cx);
    publish_types(&app, visual, false);
    oa_menu_on(&app, visual, &oa_pg_object("orders"), "object-menu-truncate");
    assert!(visual.debug_bounds("changes-confirm").is_none(), "the connection asked for nothing when the review opened");
    oa_click(visual, "changes-understood");
    // The connection's type changes behind the open dialog.
    give_profile(&app, visual, OA_PG, typed(PRODUCTION));
    oa_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "the review never asked for the confirmation: nothing runs");
    let host = oa_host(&app, visual);
    let (report, failed) = host.read_with(visual, |host, _| host.review_report()).expect("the refusal is reported");
    assert!(failed && report.contains("asks for a confirmation"), "{report}");
    assert!(app.read_with(visual, |this, _| (0..this.error_log.log.len()).filter_map(|i| this.error_log.log.get(i)).any(|e| e.message.contains("asks for a confirmation"))));
    // The dialog now shows the confirmation, and with it ticked the apply goes through.
    assert!(visual.debug_bounds("changes-confirm").is_some(), "the box appears");
    oa_click(visual, "changes-understood");
    oa_click(visual, "changes-confirm");
    oa_click(visual, "changes-apply");
    oa_wait_host(&app, visual);
    assert!(ran.lock().unwrap().iter().any(|sql| sql.starts_with("TRUNCATE TABLE")));
}

#[gpui::test]
fn an_open_schema_diagram_reloads_after_a_drop_and_after_a_new_table(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let labels = |rig: &SqlRig<'_>, tab: u64| -> Vec<String> {
        rig.app.read_with(rig.visual, |this, _| {
            this.schema_diagram_graph(tab).map(|snapshot| snapshot.graph.entities.iter().map(|entity| entity.label.clone()).collect()).unwrap_or_default()
        })
    };
    rig.app.update(rig.visual, |this, cx| this.open_schema_diagram(&key, "main", Some("main"), cx));
    settle(rig.visual);
    let tab = rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().id);
    let app = rig.app.clone();
    let wait_labels = |visual: &mut gpui::VisualTestContext, want: &[&str], app: &gpui::Entity<AppRoot>| {
        let want: Vec<String> = want.iter().map(|name| name.to_string()).collect();
        wait_until(visual, "the diagram to list the tables", |visual| {
            app.read_with(visual, |this, _| {
                this.schema_diagram_graph(tab).is_some_and(|snapshot| {
                    let mut now: Vec<String> = snapshot.graph.entities.iter().map(|entity| entity.label.clone()).collect();
                    now.sort();
                    now == want && snapshot.progress.is_none()
                })
            })
        });
    };
    wait_labels(rig.visual, &["items", "other"], &app);

    let other = oa_table(&rig, "other");
    oa_menu(&mut rig, &other, "object-menu-drop");
    rig.click("changes-understood");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    wait_labels(rig.visual, &["items"], &app);
    assert_eq!(labels(&rig, tab), ["items"]);

    // A new table shows up in it too.
    let tables = group_node(&key, "main", "main", ObjectGroup::Tables);
    oa_menu(&mut rig, &tables, "object-menu-new-table");
    oa_type(rig.visual, "object-table-name", "gadgets");
    rig.click("object-table-submit");
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    wait_labels(rig.visual, &["gadgets", "items"], &app);
}

#[gpui::test]
fn an_object_action_applies_while_the_shared_session_is_busy(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ACTIONS_SEED);
    let mut rig = oa_rig(cx, &db);
    let items = oa_table(&rig, "items");
    oa_menu(&mut rig, &items, "object-menu-rename");
    oa_type(rig.visual, "object-name-input", "things");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    // The connection's own session is held by something long running: the apply runs on a session of
    // its own and does not wait for it.
    let holder = SessionLockHolder::hold(&rig.session);
    rig.click("changes-apply");
    oa_wait_ok(&mut rig);
    assert_eq!(db.sql("SELECT count(*) FROM things"), "3");
    assert!(rig.session.try_lock().is_err(), "the session was busy throughout");
    holder.release();
}

// ---- Create Database on a connection without SQL DDL (MongoDB): the driver's mutation -----------

const OA_MONGO: &str = crate::ui_fixture::MONGO_ID;

/// The Mongo fixture connection, connected, whose driver advertises `database.create`.
fn oa_mongo(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = mongo_admin_app(cx);
    app.update(visual, |this, cx| {
        fake_create_database_capability(this, OA_MONGO);
        this.sidebar.path = Some(std::env::temp_dir().join(format!("tablex-mongo-create-db-{}.json", uuid::Uuid::new_v4())));
        cx.notify();
    });
    (app, visual)
}

/// Opens the connection menu's Create Database on the Mongo connection and types `name` into it.
fn oa_mongo_name_dialog(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, name: &str) {
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_create_database_dialog(OA_MONGO, window, cx)));
    settle(visual);
    assert!(visual.debug_bounds("driver-create-db-input").is_some(), "the name dialog is open");
    assert!(visual.debug_bounds("object-name-input").is_none(), "MongoDB has no SQL to review");
    oa_type(visual, "driver-create-db-input", name);
}

#[gpui::test]
fn mongo_create_database_goes_to_the_driver_not_the_sql_review_and_lists_the_database(cx: &mut TestAppContext) {
    let (app, visual) = oa_mongo(cx);
    assert!(!sidebar_labels(&app, visual).contains(&"aaa_created".to_string()));
    oa_mongo_name_dialog(&app, visual, "aaa_created");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(mongo_fixture_operations(&app, visual), ["create_database"]);
    assert!(sidebar_labels(&app, visual).contains(&"aaa_created".to_string()), "{:?}", sidebar_labels(&app, visual));
    app.read_with(visual, |this, _| {
        assert!(this.object_action.is_none(), "nothing was reviewed");
        assert_eq!(this.sidebar.notice.as_deref(), Some("Database \"aaa_created\" created"));
    });
    assert!(visual.debug_bounds("driver-create-db-input").is_none(), "the dialog closed");
}

#[gpui::test]
fn mongo_create_database_refuses_a_bad_name_where_it_is_typed_and_cancel_sends_nothing(cx: &mut TestAppContext) {
    let (app, visual) = oa_mongo(cx);
    for bad in [" padded", "padded "] {
        oa_mongo_name_dialog(&app, visual, bad);
        visual.simulate_keystrokes("enter");
        settle(visual);
        assert!(mongo_fixture_operations(&app, visual).is_empty(), "{bad:?} was sent");
        // The refusal is an alert over the name dialog: dismiss both.
        visual.simulate_keystrokes("escape");
        settle(visual);
        visual.simulate_keystrokes("escape");
        settle(visual);
    }
    oa_mongo_name_dialog(&app, visual, "never_sent");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(mongo_fixture_operations(&app, visual).is_empty());
    assert!(visual.debug_bounds("driver-create-db-input").is_none(), "cancel closed the dialog");
    assert!(!sidebar_labels(&app, visual).contains(&"never_sent".to_string()));
}

#[gpui::test]
fn a_refused_mongo_create_database_reaches_the_status_and_the_error_log(cx: &mut TestAppContext) {
    let (app, visual) = oa_mongo(cx);
    // The fixture refuses a second database of the same name, like the driver.
    oa_mongo_name_dialog(&app, visual, "fixture_db_0");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(mongo_fixture_operations(&app, visual).is_empty(), "nothing was created");
    app.read_with(visual, |this, _| {
        let notice = this.sidebar.notice.as_deref().expect("the status says why");
        assert!(notice.starts_with("Could not create database") && notice.contains("already exists"), "{notice}");
        let logged: Vec<_> = (0..this.error_log.log.len()).filter_map(|i| this.error_log.log.get(i)).collect();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert_eq!(logged[0].severity, crate::error_log::Severity::Error);
        assert!(logged[0].message.contains("already exists"));
    });
}

#[gpui::test]
fn mongo_create_database_obeys_read_only_and_asks_a_production_connection_first(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual) = oa_mongo(cx);
    publish_types(&app, visual, true);
    give_profile(&app, visual, OA_MONGO, typed(PRODUCTION));
    // Production, read-only: refused with the reason, nothing asked, nothing sent.
    oa_mongo_name_dialog(&app, visual, "blocked");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(visual.debug_bounds("driver-create-db-confirm").is_none());
    assert!(mongo_fixture_operations(&app, visual).is_empty());
    let notice = app.read_with(visual, |this, _| this.sidebar.notice.clone()).expect("the refusal is shown");
    assert!(notice.contains("Creating a database is blocked") && notice.contains("read-only"), "{notice}");

    // Production, writable: the dialog asks, Cancel sends nothing, Create does.
    publish_types(&app, visual, false);
    oa_mongo_name_dialog(&app, visual, "aaa_confirmed");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(visual.debug_bounds("driver-create-db-confirm").is_some(), "Production asks for its confirmation");
    assert!(mongo_fixture_operations(&app, visual).is_empty(), "nothing is sent before the answer");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(mongo_fixture_operations(&app, visual).is_empty());
    oa_mongo_name_dialog(&app, visual, "aaa_confirmed");
    visual.simulate_keystrokes("enter");
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(mongo_fixture_operations(&app, visual), ["create_database"]);
    assert!(sidebar_labels(&app, visual).contains(&"aaa_confirmed".to_string()));
}

#[gpui::test]
fn a_database_created_behind_a_visible_object_filter_joins_its_whitelist_and_shows(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::VisibleObjectFilter;
    let (app, visual) = oa_mongo(cx);
    app.update(visual, |this, cx| {
        this.change_sidebar_preferences(
            |prefs| prefs.set_visible_filter(OA_MONGO, VisibleObjectFilter { databases: Some(vec!["fixture_db_0".into()]), ..Default::default() }),
            cx,
        )
        .unwrap();
    });
    settle(visual);
    let labels = sidebar_labels(&app, visual);
    assert!(labels.contains(&"fixture_db_0".to_string()) && !labels.contains(&"fixture_db_1".to_string()), "{labels:?}");
    oa_mongo_name_dialog(&app, visual, "aaa_created");
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let filter = &this.navigator.preferences().visible_filters[OA_MONGO];
        assert_eq!(filter.databases.as_deref(), Some(&["fixture_db_0".to_string(), "aaa_created".to_string()][..]));
    });
    let labels = sidebar_labels(&app, visual);
    assert!(labels.contains(&"aaa_created".to_string()), "the new database shows through the filter: {labels:?}");
    assert!(!labels.contains(&"fixture_db_1".to_string()), "the filter still hides the others: {labels:?}");
}

#[gpui::test]
fn the_visible_object_filter_takes_a_created_database_and_the_system_ones_it_hides(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::VisibleObjectFilter;
    let (app, visual) = oa_mongo(cx);
    app.update(visual, |this, cx| {
        this.change_sidebar_preferences(
            |prefs| prefs.set_visible_filter(OA_MONGO, VisibleObjectFilter { databases: Some(vec!["keep".into()]), show_system: false, ..Default::default() }),
            cx,
        )
        .unwrap();
        // A user database joins the whitelist; one already on it, or no filter at all, changes nothing.
        this.make_created_database_visible(OA_MONGO, "mine", cx);
        this.make_created_database_visible(OA_MONGO, "keep", cx);
        this.make_created_database_visible("no-filter-connection", "x", cx);
        let filter = &this.navigator.preferences().visible_filters[OA_MONGO];
        assert_eq!(filter.databases.as_deref(), Some(&["keep".to_string(), "mine".to_string()][..]));
        assert!(!filter.show_system);
        assert!(!this.navigator.preferences().visible_filters.contains_key("no-filter-connection"));
        // A system database also turns the system databases on.
        this.make_created_database_visible(OA_MONGO, "mysql", cx);
        let filter = &this.navigator.preferences().visible_filters[OA_MONGO];
        assert!(filter.show_system && filter.databases.as_ref().unwrap().contains(&"mysql".to_string()));
    });
}

/// Create Database on a real MongoDB through the connection menu's entry: the driver's
/// `create_database` mutation, the database listed behind a Visible Object Filter, found again on the
/// server. Run by `rust-engine/scripts/test-mongo-driver-ephemeral.sh` against an owned loopback `mongod`.
#[gpui::test]
#[ignore = "requires rust-engine/scripts/test-mongo-driver-ephemeral.sh with an owned loopback mongod"]
fn ephemeral_mongo_create_database_from_the_menu_lists_it_behind_a_filter(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::VisibleObjectFilter;
    let (Ok(uri), Ok(owner), Some(driver)) =
        (std::env::var("TABLEX_EPHEMERAL_MONGO_URI"), std::env::var("TABLEX_EPHEMERAL_MONGO_OWNER"), std::env::var_os("TABLEX_EPHEMERAL_MONGO_DRIVER"))
    else {
        eprintln!("skipped: run rust-engine/scripts/test-mongo-driver-ephemeral.sh (owned loopback mongod)");
        return;
    };
    let driver = std::path::PathBuf::from(driver);
    assert!(owner.starts_with("ephem_") && owner.len() >= 14 && owner.len() < 40);
    assert!(uri.starts_with("mongodb://127.0.0.1:") && uri.contains("directConnection=true") && !uri.contains('@'));
    let created = format!("tx_{}_created", owner.trim_start_matches("ephem_"));
    let project = TempSqlProject::new();
    cx.dispatcher.allow_parking();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = sidebar_test_app(cx);
    let connection = OA_MONGO;
    app.update(visual, |this, cx| {
        this.scenario_ui.service = None;
        this.sidebar.path = Some(project.dir.join("prefs.json"));
        let handle = engine_driver_host::DriverLoader.load(&driver).unwrap();
        assert!(handle.capabilities().has(Capability::DatabaseCreate));
        this.driver_runtime.capabilities.insert(connection.into(), std::sync::Arc::new(handle.capabilities().clone()));
        let session = handle.connect(&serde_json::json!({"uri": uri}).to_string()).unwrap();
        let engine_id = session.connection_id();
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        let epoch = this.sessions.begin_connect(connection).unwrap();
        this.sessions.finish_connect(connection, epoch, Ok(()));
        this.driver_runtime.sessions.insert(connection.into(), session);
        this.driver_runtime.connection_ids.insert(connection.into(), engine_id);
        this.driver_ui.handles.insert("mongodb".into(), handle);
        this.navigator.enable_provider(connection);
        let root = NavigatorNodeId::Connection { connection: connection.into() };
        let request = this.navigator.toggle_expansion(&root).request.unwrap();
        this.dispatch_provider_load(request, cx);
    });
    let listed = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, name: &str| {
        app.read_with(visual, |this, _| this.navigator.visible_rows().iter().any(|row| row.label == name))
    };
    wait_until(visual, "the server's databases", |visual| listed(&app, visual, "local"));
    // Only `local` is shown (system databases on): the created one is not, until it joins the filter.
    app.update(visual, |this, cx| {
        this.change_sidebar_preferences(
            |prefs| prefs.set_visible_filter(connection, VisibleObjectFilter { databases: Some(vec!["local".into()]), show_system: true, ..Default::default() }),
            cx,
        )
        .unwrap();
    });
    settle(visual);
    assert!(!listed(&app, visual, "admin"), "the filter hides the other databases");

    oa_mongo_name_dialog(&app, visual, &created);
    visual.simulate_keystrokes("enter");
    settle(visual);
    wait_until(visual, "the created database in the sidebar", |visual| listed(&app, visual, &created));
    app.read_with(visual, |this, _| {
        let filter = &this.navigator.preferences().visible_filters[connection];
        assert_eq!(filter.databases.as_deref(), Some(&["local".to_string(), created.clone()][..]));
        assert_eq!(this.sidebar.notice.as_deref(), Some(format!("Database \"{created}\" created").as_str()));
        assert!(this.object_action.is_none());
    });
    assert!(!listed(&app, visual, "admin"), "and the filter still hides the rest");

    // The server refuses a name it cannot use, and the refusal reaches the status.
    oa_mongo_name_dialog(&app, visual, "bad$name");
    visual.simulate_keystrokes("enter");
    wait_until(visual, "the refusal", |visual| {
        app.read_with(visual, |this, _| this.sidebar.notice.as_deref().is_some_and(|notice| notice.starts_with("Could not create database")))
    });
}

/// Cached capability says "database.create", the real SQLite driver refuses the mutation: the answer
/// is the proof that it was sent on the driver route (SQLite itself goes to the review).
fn oa_driver_refusal_rig<'a>(cx: &'a mut TestAppContext, db: &SqliteDb) -> SqlRig<'a> {
    let rig = nav_rig_sqlite(cx, db);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.capabilities.insert(
            key.clone(),
            std::sync::Arc::new(engine_driver_api::DriverCapabilities::new("sqlite", "SQLite", vec!["navigator".to_string(), "database.create".to_string()])),
        );
    });
    rig
}

#[gpui::test]
fn a_refused_driver_create_database_reaches_the_error_log_and_the_status(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NAV_SEED);
    let rig = oa_driver_refusal_rig(cx, &db);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.create_database_on_driver(&key, "other", cx));
    let app = rig.app.clone();
    wait_until(rig.visual, "the refused create", |visual| {
        app.read_with(visual, |this, _| this.sidebar.notice.as_deref().is_some_and(|notice| notice.starts_with("Could not create database")))
    });
    let notice = rig.app.read_with(rig.visual, |this, _| this.sidebar.notice.clone().unwrap());
    assert!(notice.contains("database.create"), "{notice}");
    assert_eq!(rig.log().len(), 1, "{:?}", rig.log());
    assert_eq!(rig.log()[0].1, crate::error_log::Severity::Error);
}

#[gpui::test]
fn a_driver_create_database_does_not_wait_for_the_session_lock(cx: &mut TestAppContext) {
    let db = SqliteDb::new(NAV_SEED);
    let rig = oa_driver_refusal_rig(cx, &db);
    let key = rig.key.clone();
    let holder = SessionLockHolder::hold(&rig.session);
    rig.app.update(rig.visual, |this, cx| this.create_database_on_driver(&key, "other", cx));
    let app = rig.app.clone();
    wait_until(rig.visual, "the refused create", |visual| {
        app.read_with(visual, |this, _| this.sidebar.notice.as_deref().is_some_and(|notice| notice.starts_with("Could not create database")))
    });
    assert!(rig.session.try_lock().is_err(), "the session was busy throughout");
    holder.release();
}
