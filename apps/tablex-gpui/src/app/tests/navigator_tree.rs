// Batch E of the connection tree: Copy / Paste of tables between connections, row counts and sizes, DDL compare
// and bookmarks. The rows come from the real SQLite driver (`oa_rig`) or from the scenario fixtures.

fn nt_clip(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Option<(Vec<String>, String)> {
    app.read_with(visual, |this, _| this.navigator_ui.clipboard.as_ref().map(|clip| (clip.tables.clone(), clip.source.schema.clone())))
}

#[gpui::test]
fn copied_tables_paste_into_a_prefilled_transfer_that_never_starts(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let (people, notes) = (oa_table(&rig, "people"), oa_table(&rig, "notes"));
    let schema = schema_node(&rig.key, "main", "main");

    // Nothing copied yet: Paste is in the menu, greyed out, and does nothing.
    oa_menu(&mut rig, &schema, "object-menu-paste");
    assert!(rig.app.read_with(rig.visual, |this, _| this.transfers.is_empty() && !this.tree_clipboard_filled()));
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);

    // Copy (the row menu) on a selection of two tables keeps both, in tree order, and puts their qualified names on the clipboard.
    rig.app.update(rig.visual, |this, _| {
        this.navigator.select_visible(&people, false, false);
        this.navigator.select_visible(&notes, true, false);
    });
    oa_menu(&mut rig, &people, "object-menu-copy");
    assert_eq!(nt_clip(&rig.app, rig.visual), Some((vec!["notes".to_owned(), "people".to_owned()], "main".to_owned())));
    let text = rig.visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains("notes") && text.contains("people"), "{text}");

    // Paste on the schema: the Data Transfer tab opens with the copied tables ticked and that schema as the target.
    oa_menu(&mut rig, &schema, "object-menu-paste");
    dt_wait(&mut rig, "the list of tables", |s| s.listing == "ready");
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.mode, snapshot.target_kind), ("source", "tables", "table"));
    assert_eq!(snapshot.tables, [("notes".to_owned(), true), ("orders".to_owned(), false), ("people".to_owned(), true)]);
    assert_eq!((snapshot.target.schema.as_str(), snapshot.target.connection == rig.key), ("main", true));
    // Nothing started: no jobs, no run, no report.
    assert!(snapshot.jobs.is_empty() && !snapshot.running && snapshot.report.is_none(), "{:?}", snapshot.message);
    // Next goes to the target step, which keeps the pasted namespace; Next again builds the jobs with "create" as the answer for missing tables.
    dt_next(&mut rig);
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!((snapshot.step, snapshot.namespaces.clone()), ("target", vec!["main".to_owned()]));
    dt_next(&mut rig);
    dt_wait(&mut rig, "the columns of every table", |s| s.jobs.len() == 2 && s.jobs.iter().all(|job| job.columns.is_some() || job.columns_error.is_some()));
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert!(!snapshot.running && snapshot.report.is_none());
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE type = 'table'"), "3", "pasting creates nothing");
}

#[gpui::test]
fn cmd_c_and_cmd_v_on_the_focused_tree_copy_and_paste_tables(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let rig = oa_rig(cx, &db);
    let (orders, database) = (oa_table(&rig, "orders"), database_node(&rig.key, "main"));
    let (app, visual) = (rig.app.clone(), rig.visual);
    sn_focus_sidebar_on(&app, visual, &orders);
    visual.simulate_keystrokes(&primary("c"));
    assert_eq!(nt_clip(&app, visual), Some((vec!["orders".to_owned()], "main".to_owned())));
    // Cmd+V on the database row pastes there.
    sn_focus_sidebar_on(&app, visual, &database);
    visual.simulate_keystrokes(&primary("v"));
    settle(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().map(|tab| (tab.kind, tab.title.clone())));
    assert_eq!(tab, Some((TabKind::Transfer, "Transfer orders".to_owned())));
    assert_eq!(app.read_with(visual, |this, _| this.transfers.len()), 1);
}

#[gpui::test]
fn a_connection_row_pastes_the_copied_tables_into_the_first_namespace_the_tree_lists(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let connection = connection_node(&rig.key);
    // Greyed out until something was copied.
    oa_menu(&mut rig, &connection, "connection-menu-paste");
    assert!(rig.app.read_with(rig.visual, |this, _| this.transfers.is_empty()));
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    let notes = oa_table(&rig, "notes");
    oa_menu(&mut rig, &notes, "object-menu-copy");
    oa_menu(&mut rig, &connection, "connection-menu-paste");
    dt_wait(&mut rig, "the list of tables", |s| s.listing == "ready");
    let snapshot = dt_snapshot(&rig.app, rig.visual);
    assert_eq!(snapshot.tables.iter().filter(|(_, ticked)| *ticked).map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["notes"]);
    assert_eq!((snapshot.target.database.as_str(), snapshot.target.schema.as_str()), ("main", "main"));
}

#[gpui::test]
fn only_tables_and_views_of_one_namespace_are_copied(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let rig = oa_rig(cx, &db);
    let key = rig.key.clone();
    let people = oa_table(&rig, "people");
    let schema = schema_node(&key, "main", "main");
    let other = object_node(&key, "main", "other_schema", "elsewhere");
    rig.app.update(rig.visual, |this, cx| {
        // A schema row is not a table.
        assert!(!this.copy_tree_objects(std::slice::from_ref(&schema), cx));
        assert!(!this.copy_tree_objects(&[], cx));
        assert!(this.navigator_ui.clipboard.is_none());
        // The tables of a namespace other than the first one's are left out, and the user is told.
        assert!(this.copy_tree_objects(&[people.clone(), other.clone()], cx));
        assert_eq!(this.navigator_ui.clipboard.as_ref().map(|clip| clip.tables.clone()), Some(vec!["people".to_owned()]));
        assert!(this.sidebar.notice.as_deref().is_some_and(|notice| notice.contains('1')), "{:?}", this.sidebar.notice);
    });
    // Paste targets: a schema, a database and a connection take it; a table row and a column do not.
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.paste_target(&schema).is_some() && this.paste_target(&database_node(&key, "main")).is_some());
        assert!(this.paste_target(&connection_node(&key)).is_some());
        assert!(this.paste_target(&people).is_none());
    });
}

// ---- row counts, sizes and comments ------------------------------------------------------------------------------

/// Every statement a read of estimates was sent, with the database it was sent to.
type NtLog = std::sync::Arc<std::sync::Mutex<Vec<(Option<String>, String)>>>;

/// A catalog that answers the statement of the estimates with two tables, keeping every statement it was asked.
struct NtCatalog {
    log: NtLog,
    database: Option<String>,
}

impl crate::sql_account::catalog::CatalogSession for NtCatalog {
    fn rows(&mut self, sql: &str) -> Result<crate::sql_account::catalog::TextRows, crate::sql_account::catalog::ReadError> {
        self.log.lock().unwrap().push((self.database.clone(), sql.to_owned()));
        let row = |parts: [&str; 5]| parts.iter().map(|part| (!part.is_empty()).then(|| part.to_string())).collect::<Vec<_>>();
        Ok(vec![
            row(["orders", "r", "1200000", "47185920", "Customer orders"]),
            row(["people", "r", "-1", "8192", ""]),
        ])
    }
}

fn nt_stats_rig<'a>(cx: &'a mut TestAppContext, db: &SqliteDb) -> (SqlRig<'a>, NtLog) {
    let rig = oa_rig(cx, db);
    let log: NtLog = Default::default();
    let reads = log.clone();
    rig.app.update(rig.visual, |this, _| {
        // The tree is the SQLite one; the connection says PostgreSQL, which is what decides what is read.
        this.saved_connections.iter_mut().find(|saved| saved.matches_id(&rig.key)).unwrap().kind = ConnectionKind::Postgres;
        this.table_stats_injected = Some(std::sync::Arc::new(move |database: Option<&str>| -> Box<dyn crate::sql_account::catalog::CatalogSession + Send> {
            Box::new(NtCatalog { log: reads.clone(), database: database.map(str::to_owned) })
        }));
    });
    (rig, log)
}

fn nt_stats_label(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) -> bool {
    settle(visual);
    let index = app.read_with(visual, |this, _| this.navigator.visible_index(id)).expect("a visible row");
    let selector: &'static str = Box::leak(format!("sidebar-stats-{index}").into_boxed_str());
    visual.debug_bounds(selector).is_some()
}

#[gpui::test]
fn table_estimates_are_read_in_one_statement_per_namespace_only_with_the_setting_on_and_again_after_a_reload(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let (rig, log) = nt_stats_rig(cx, &db);
    let (orders, people, notes) = (oa_table(&rig, "orders"), oa_table(&rig, "people"), oa_table(&rig, "notes"));
    let tables = group_node(&rig.key, "main", "main", ObjectGroup::Tables);
    settle(rig.visual);
    // Setting off (the default): the rows are drawn and nothing is read, nothing is shown.
    assert!(log.lock().unwrap().is_empty() && !nt_stats_label(&rig.app, rig.visual, &orders));
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.table_stats.len()), 0);

    // On: the namespace is read once, for all of its rows, on a session opened for the database; the estimates show.
    tools_save_settings(&rig.app, rig.visual, |settings| settings.navigation.show_table_stats = true);
    let reads = log.clone();
    wait_until(rig.visual, "the estimates", |_| !reads.lock().unwrap().is_empty());
    let app = rig.app.clone();
    let id = orders.clone();
    wait_until(rig.visual, "the estimates to be kept", |visual| app.read_with(visual, |this, _| this.table_stats_of(&id).is_some()));
    assert!(nt_stats_label(&rig.app, rig.visual, &orders), "the row shows its estimate");
    {
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "one statement for every table of the namespace");
        assert_eq!(log[0].0.as_deref(), Some("main"));
        assert!(log[0].1.contains("pg_class") && log[0].1.contains("'main'") && !log[0].1.to_lowercase().contains("count("), "{}", log[0].1);
    }
    rig.app.read_with(rig.visual, |this, _| {
        let found = this.table_stats_of(&orders).expect("orders");
        assert_eq!(found.label.as_deref(), Some("1.2M rows \u{00B7} 45 MB"));
        assert_eq!(found.tooltip_lines(), ["Customer orders", "About 1.2M rows (estimate)", "45 MB on disk"]);
        // reltuples = -1: no rows, the size is still known.
        assert_eq!(this.table_stats_of(&people).and_then(|found| found.label.clone()).as_deref(), Some("8.0 KB"));
        // A table the statement did not list has none.
        assert!(this.table_stats_of(&notes).is_none());
    });
    assert!(!nt_stats_label(&rig.app, rig.visual, &notes));

    // Hovering rows and drawing again read nothing more.
    let index = rig.app.read_with(rig.visual, |this, _| this.navigator.visible_index(&people)).unwrap();
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    let bounds = rig.visual.debug_bounds(selector).expect("the row");
    rig.visual.simulate_mouse_move(bounds.center(), None, Default::default());
    settle(rig.visual);
    assert_eq!(log.lock().unwrap().len(), 1);

    // The folder reloads: the estimates are stale and are read again for the new listing.
    rig.app.update(rig.visual, |this, cx| {
        let request = this.navigator.reload_children(&tables).expect("a loaded folder reloads");
        this.navigator_ui.generation = this.navigator.generation();
        this.ensure_connected_then_load(request, cx);
    });
    let reads = log.clone();
    wait_until(rig.visual, "the estimates again", |_| reads.lock().unwrap().len() == 2);
    let app = rig.app.clone();
    let id = orders.clone();
    wait_until(rig.visual, "the new estimates", |visual| app.read_with(visual, |this, _| this.table_stats_of(&id).is_some()));
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[gpui::test]
fn a_hover_reads_the_estimates_for_the_tooltip_even_with_the_setting_off(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let (rig, log) = nt_stats_rig(cx, &db);
    let orders = oa_table(&rig, "orders");
    settle(rig.visual);
    assert!(log.lock().unwrap().is_empty());
    let index = rig.app.read_with(rig.visual, |this, _| this.navigator.visible_index(&orders)).unwrap();
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    let bounds = rig.visual.debug_bounds(selector).expect("the row");
    rig.visual.simulate_mouse_move(bounds.center(), None, Default::default());
    let (app, id) = (rig.app.clone(), orders.clone());
    wait_until(rig.visual, "the estimates", |visual| app.read_with(visual, |this, _| this.table_stats_of(&id).is_some()));
    assert_eq!(log.lock().unwrap().len(), 1);
    // The setting is off: the tooltip has them, the row shows none.
    assert!(!nt_stats_label(&rig.app, rig.visual, &orders));
}

#[gpui::test]
fn kinds_without_a_catalog_estimate_are_never_asked(cx: &mut TestAppContext) {
    // SQLite (no estimate) and the fixture's MongoDB collections: no statement, no label, no cache entry.
    let db = SqliteDb::new(DT_SEED);
    let rig = oa_rig(cx, &db);
    let orders = oa_table(&rig, "orders");
    tools_save_settings(&rig.app, rig.visual, |settings| settings.navigation.show_table_stats = true);
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(!this.table_stats_supported(&orders));
        assert_eq!(this.table_stats.len(), 0);
    });
    assert!(!nt_stats_label(&rig.app, rig.visual, &orders));
}

#[gpui::test]
fn a_demo_scenario_shows_estimates_for_its_postgres_tables(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    settle(visual);
    assert!(!nt_stats_label(&app, visual, &orders), "off by default");
    tools_save_settings(&app, visual, |settings| settings.navigation.show_table_stats = true);
    settle(visual);
    settle(visual);
    assert!(nt_stats_label(&app, visual, &orders), "the demo rows carry estimates");
    assert_eq!(app.read_with(visual, |this, _| this.table_stats.len()), 1, "one namespace, one read");
}

// ---- DDL compare -----------------------------------------------------------------------------------------------

fn nt_compare_ready(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> u64 {
    let probe = app.clone();
    wait_until(visual, "the compare tab", |visual| probe.read_with(visual, |this, _| this.compare_tabs.values().any(|state| state.status != ddl_compare::CompareStatus::Loading)));
    settle(visual);
    app.read_with(visual, |this, _| *this.compare_tabs.keys().next().expect("a compare tab"))
}

#[gpui::test]
fn two_selected_tables_compare_their_ddl_in_a_diff_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DT_SEED);
    let mut rig = oa_rig(cx, &db);
    let (people, orders) = (oa_table(&rig, "people"), oa_table(&rig, "orders"));
    rig.app.update(rig.visual, |this, _| {
        this.navigator.select_visible(&orders, false, false);
        this.navigator.select_visible(&people, true, false);
        assert!(this.compare_pair(&people).is_some());
        assert_eq!(this.object_menu_items(&people).iter().filter(|item| matches!(item, crate::app::object_actions::ObjectMenuItem::CompareDdl)).count(), 1);
        assert!(!this.object_menu_items(&people).contains(&crate::app::object_actions::ObjectMenuItem::CompareDdlWith));
    });
    oa_menu(&mut rig, &people, "object-menu-compare");
    let tab_id = nt_compare_ready(&rig.app, rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().expect("the new tab");
        assert_eq!((tab.id, tab.kind), (tab_id, TabKind::Compare));
        let state = &this.compare_tabs[&tab_id];
        assert_eq!(state.status, ddl_compare::CompareStatus::Ready);
        let patch = state.patch.as_deref().expect("they differ");
        assert!(patch.contains("-CREATE TABLE orders") && patch.contains("+CREATE TABLE people"), "{patch}");
        assert!(state.additions > 0 && state.deletions > 0);
    });
    // The diff, the two sides it compares and its counts are on screen.
    for selector in ["compare-tab", "compare-diff", "compare-legend", "compare-copy", "compare-refresh"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Copy hands out the patch.
    rig.click("compare-copy");
    let copied = rig.visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())).unwrap();
    assert!(copied.starts_with("--- a/left.sql"), "{copied}");
    // Refresh reads both again.
    rig.click("compare-refresh");
    nt_compare_ready(&rig.app, rig.visual);
    // Closing the tab drops its state.
    rig.app.update(rig.visual, |this, cx| this.close_tab(tab_id, cx));
    assert!(rig.app.read_with(rig.visual, |this, _| this.compare_tabs.is_empty()));
}

#[gpui::test]
fn compare_ddl_with_picks_the_second_object_in_the_go_to_dialog(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        window.activate_window();
    });
    let (orders, customers) = (oa_pg_object("orders"), oa_pg_object("fixture_table_01"));
    app.update(visual, |this, _| {
        this.navigator.select_visible(&orders, false, false);
        assert!(this.compare_pair(&orders).is_none());
    });
    oa_menu_on(&app, visual, &orders, "object-menu-compare-with");
    assert!(palette_is_open(visual), "the dialog opens to pick the second object");
    visual.simulate_input("fixture_table_01");
    settle(visual);
    assert!(goto_rows(&app, visual).iter().any(|(badge, title, _)| *badge == "Table" && title == "fixture_table_01"));
    visual.simulate_keystrokes("enter");
    settle(visual);
    let tab_id = nt_compare_ready(&app, visual);
    app.read_with(visual, |this, _| {
        let state = &this.compare_tabs[&tab_id];
        assert_eq!((state.left.id.clone(), state.right.id.clone()), (orders.clone(), customers.clone()));
        assert!(state.left.label.contains("Postgres fixture") && state.left.label.contains("public.orders"), "{}", state.left.label);
        assert_eq!(state.status, ddl_compare::CompareStatus::Ready);
        assert!(!this.goto_compare_pending_for_test(), "the pick is over");
    });
    // The dialog is a Go to Object again afterwards: choosing a row reveals it and opens no compare.
    goto_open(visual);
    visual.simulate_input("orders");
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.compare_tabs.len()), 1);
}

#[gpui::test]
fn two_definitions_that_are_the_same_say_so_instead_of_showing_a_diff(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    app.update(visual, |this, cx| this.open_ddl_compare(orders.clone(), orders.clone(), cx));
    nt_compare_ready(&app, visual);
    assert!(visual.debug_bounds("compare-identical").is_some() && visual.debug_bounds("compare-diff").is_none());
    app.read_with(visual, |this, _| assert!(this.compare_tabs.values().all(|state| state.patch.is_none())));
}

#[gpui::test]
fn a_definition_that_cannot_be_read_is_reported_in_the_tab(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let (orders, other) = (oa_pg_object("orders"), oa_pg_object("fixture_table_01"));
    // The server refuses one of the two reads.
    crate::ui_fixture_properties::FAIL_NEXT.with(|fail| fail.set(1));
    app.update(visual, |this, cx| this.open_ddl_compare(orders.clone(), other.clone(), cx));
    nt_compare_ready(&app, visual);
    assert!(visual.debug_bounds("compare-error").is_some());
}

// ---- bookmarks ---------------------------------------------------------------------------------------------------

fn nt_bookmark_labels(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<(usize, String)> {
    settle(visual);
    app.read_with(visual, |this, _| {
        this.navigator
            .visible_rows()
            .into_iter()
            .filter(|row| matches!(row.id, NavigatorNodeId::BookmarkSection | NavigatorNodeId::Bookmark { .. }))
            .map(|row| (row.depth, row.label))
            .collect()
    })
}

fn nt_add_bookmark(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId, name: &str) {
    oa_menu_on(app, visual, id, "object-menu-add-bookmark");
    assert!(visual.debug_bounds("bookmark-name").is_some(), "the name dialog is open");
    if !name.is_empty() {
        oa_type(visual, "bookmark-name", name);
    }
    oa_click(visual, "bookmark-save");
    settle(visual);
}

#[gpui::test]
fn a_bookmark_is_added_with_a_name_listed_first_opened_renamed_and_removed(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        window.activate_window();
    });
    let (orders, items) = (oa_pg_object("orders"), oa_pg_object("fixture_table_01"));
    // No bookmark, no section.
    assert!(nt_bookmark_labels(&app, visual).is_empty());

    // Add Bookmark… on a table: the name is the object's own until the user types one; the section is the first row of the tree.
    nt_add_bookmark(&app, visual, &orders, "");
    assert_eq!(nt_bookmark_labels(&app, visual), [(0, "Bookmarks".to_owned()), (1, "orders".to_owned())]);
    assert_eq!(app.read_with(visual, |this, _| this.navigator.visible_rows()[0].id.clone()), NavigatorNodeId::BookmarkSection);
    nt_add_bookmark(&app, visual, &items, "  Fixture one ");
    assert_eq!(nt_bookmark_labels(&app, visual).iter().map(|(_, label)| label.as_str()).collect::<Vec<_>>(), ["Bookmarks", "orders", "Fixture one"]);
    app.read_with(visual, |this, _| {
        assert!(this.sidebar.error.is_none(), "{:?}", this.sidebar.error);
        // Not a pin: nothing moved in the tree.
        assert!(!this.navigator.is_pinned(&orders));
    });

    // The section collapses and opens again.
    let index = app.read_with(visual, |this, _| this.navigator.visible_index(&NavigatorNodeId::BookmarkSection)).unwrap();
    click_node_part(&app, visual, &NavigatorNodeId::BookmarkSection, "nav-disclosure");
    assert_eq!(nt_bookmark_labels(&app, visual), [(0, "Bookmarks".to_owned())]);
    click_node_part(&app, visual, &NavigatorNodeId::BookmarkSection, "nav-disclosure");
    assert_eq!(nt_bookmark_labels(&app, visual).len(), 3);
    assert_eq!(index, 0);

    // Enter on a bookmark reveals its object and opens it, as Go to Object does.
    let bookmark = app.read_with(visual, |this, _| this.navigator.preferences().bookmarks[1].id.clone());
    let row = NavigatorNodeId::Bookmark { id: bookmark.clone() };
    sn_focus_sidebar_on(&app, visual, &row);
    visual.simulate_keystrokes("enter");
    settle(visual);
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(matches!(this.navigator.selected(), Some(NavigatorNodeId::Object { object, .. }) if object == "fixture_table_01"), "{:?}", this.navigator.selected());
        let tab = this.workspace.active_tab().expect("the opened tab");
        assert!(tab.title.contains("fixture_table_01"), "{}", tab.title);
    });

    // Rename… from its menu.
    oa_menu_on(&app, visual, &row, "bookmark-menu-rename");
    oa_type(visual, "bookmark-name", "Renamed");
    oa_click(visual, "bookmark-save");
    settle(visual);
    assert_eq!(nt_bookmark_labels(&app, visual).last().map(|(_, label)| label.as_str()), Some("Renamed"));
    // Cancel changes nothing.
    oa_menu_on(&app, visual, &row, "bookmark-menu-rename");
    oa_type(visual, "bookmark-name", "Nope");
    oa_click(visual, "bookmark-cancel");
    assert_eq!(nt_bookmark_labels(&app, visual).last().map(|(_, label)| label.as_str()), Some("Renamed"));

    // Remove: the last bookmark takes the section with it.
    oa_menu_on(&app, visual, &row, "bookmark-menu-remove");
    assert_eq!(nt_bookmark_labels(&app, visual).len(), 2);
    let first = NavigatorNodeId::Bookmark { id: app.read_with(visual, |this, _| this.navigator.preferences().bookmarks[0].id.clone()) };
    oa_menu_on(&app, visual, &first, "bookmark-menu-remove");
    assert!(nt_bookmark_labels(&app, visual).is_empty());
}

#[gpui::test]
fn bookmarks_survive_a_restart_and_a_deleted_connection_takes_its_bookmarks_along(cx: &mut TestAppContext) {
    let (first, view, ..) = oa_pg(cx);
    let path = nc_prefs_path();
    first.update(view, |this, _| this.sidebar.path = Some(path.clone()));
    let orders = oa_pg_object("orders");
    nt_add_bookmark(&first, view, &orders, "Orders");
    let stored = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert_eq!(stored.bookmarks.len(), 1);
    assert_eq!(stored.bookmarks[0].name, "Orders");

    // A new app over the same file lists it before anything loads.
    let (second, view2) = sidebar_test_app(cx);
    second.update(view2, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.reload_sidebar_preferences(cx).unwrap();
        this.seed_connection_tree_scenario();
    });
    assert_eq!(nt_bookmark_labels(&second, view2), [(0, "Bookmarks".to_owned()), (1, "Orders".to_owned())]);

    // A bookmark of a connection that is not in the list is not shown, and deleting the connection drops it from the file.
    second.update(view2, |this, cx| {
        let mut prefs = this.navigator.preferences().clone();
        prefs.bookmarks[0].target = crate::sidebar_preferences::BookmarkTarget::Database { connection: "gone".into(), database: "x".into() };
        this.navigator.set_preferences(prefs).unwrap();
        cx.notify();
    });
    assert!(nt_bookmark_labels(&second, view2).is_empty(), "unavailable bookmarks are not listed");
    let mut prefs = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    prefs.remove_connection(crate::ui_fixture::POSTGRES_ID);
    assert!(prefs.bookmarks.is_empty());
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn functions_databases_schemas_and_collections_can_be_bookmarked_and_found_again(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let database = NavigatorNodeId::Database { connection: OA_PG.into(), database: "warehouse".into() };
    let schema = NavigatorNodeId::Schema { connection: OA_PG.into(), database: "warehouse".into(), schema: "public".into() };
    app.update(visual, |this, cx| {
        for id in [&database, &schema] {
            let (target, name) = this.bookmark_target_of(id).expect("a row that can be bookmarked");
            this.add_bookmark(target, "", cx).unwrap();
            assert!(!name.is_empty());
        }
        // A column, a folder and a connection cannot.
        assert!(this.bookmark_target_of(&NavigatorNodeId::Connection { connection: OA_PG.into() }).is_none());
        assert!(this.bookmark_target_of(&crate::navigator::object_group_id(OA_PG, "warehouse", "public", ObjectGroup::Tables)).is_none());
        // A MongoDB collection is kept by its database and name.
        this.add_bookmark(crate::sidebar_preferences::BookmarkTarget::Collection { connection: crate::ui_fixture::MONGO_ID.into(), database: "fixture_db_0".into(), collection: "fixture_collection_00".into() }, "", cx).unwrap();
    });
    assert_eq!(nt_bookmark_labels(&app, visual).iter().map(|(_, label)| label.as_str()).collect::<Vec<_>>(), ["Bookmarks", "warehouse", "public", "fixture_collection_00"]);
    // The collection opens its tab through the same path Go to Object uses.
    let collection = app.read_with(visual, |this, _| this.navigator.preferences().bookmarks[2].id.clone());
    app.update(visual, |this, cx| this.activate_bookmark(&collection, cx));
    settle(visual);
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("the collection's tab");
        assert_eq!((tab.connection_id.as_str(), tab.database.as_str(), tab.collection.as_str()), (crate::ui_fixture::MONGO_ID, "fixture_db_0", "fixture_collection_00"));
    });
    // The schema is revealed and selected.
    let schema_bookmark = app.read_with(visual, |this, _| this.navigator.preferences().bookmarks[1].id.clone());
    app.update(visual, |this, cx| this.activate_bookmark(&schema_bookmark, cx));
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.navigator.selected().cloned()), Some(schema));
}

#[gpui::test]
fn the_palette_lists_go_to_bookmark_only_with_bookmarks_and_opens_the_one_chosen(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        window.activate_window();
    });
    visual.update(|_, cx| actions::bind_keys(cx));
    assert!(!app.update(visual, |this, _| this.palette_action_titles()).contains(&"Go to Bookmark".to_owned()));
    // Nothing to go to: the command is not offered, and a direct call says so.
    let orders = oa_pg_object("orders");
    nt_add_bookmark(&app, visual, &orders, "Orders");
    nt_add_bookmark(&app, visual, &oa_pg_object("fixture_table_01"), "Other table");
    assert!(app.update(visual, |this, _| this.palette_action_titles()).contains(&"Go to Bookmark".to_owned()));

    palette_open(visual);
    palette_type(visual, "go to bookmark");
    visual.simulate_keystrokes("enter");
    settle(visual);
    settle(visual);
    assert!(visual.debug_bounds("bookmark-pick").is_some(), "the list of bookmarks is open");
    assert!(visual.debug_bounds("bookmark-pick-row-0").is_some() && visual.debug_bounds("bookmark-pick-row-1").is_some());
    // Typing narrows the list; Enter opens the bookmark.
    visual.simulate_input("other");
    settle(visual);
    assert!(visual.debug_bounds("bookmark-pick-row-0").is_some() && visual.debug_bounds("bookmark-pick-row-1").is_none());
    visual.simulate_keystrokes("enter");
    settle(visual);
    settle(visual);
    assert!(!palette_is_open(visual));
    app.read_with(visual, |this, _| {
        assert!(matches!(this.navigator.selected(), Some(NavigatorNodeId::Object { object, .. }) if object == "fixture_table_01"), "{:?}", this.navigator.selected());
    });
}

// The widths of the name, the estimate and the row of `orders` (Postgres fixture, estimates on) in a sidebar that is
// `sidebar_width` wide from its first frame, with the width the name needs on its own.
fn stats_row_widths(cx: &mut TestAppContext, sidebar_width: f32) -> (f32, f32, f32, f32) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionTree,
        fixture: FixtureKind::Sample,
        delay: Duration::ZERO,
        timeout_delay: Duration::ZERO,
    };
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| {
            let mut app = AppRoot::new_for_scenario(scenario, cx);
            app.shell.sidebar_width = sidebar_width;
            app
        });
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    for label in ["Postgres fixture", "warehouse", "public", "Tables"] {
        let row = app.update(visual, |this, _| this.navigator.visible_rows().into_iter().find(|row| row.label == label).unwrap());
        app.update(visual, |this, cx| this.handle_navigator_disclosure(&row, cx));
        visual.run_until_parked();
    }
    tools_save_settings(&app, visual, |settings| settings.navigation.show_table_stats = true);
    settle(visual);
    settle(visual);
    let index = app.read_with(visual, |this, _| this.navigator.visible_rows().iter().position(|row| row.label == "orders").unwrap());
    let width_of = |visual: &mut gpui::VisualTestContext, kind: &str| {
        let selector: &'static str = Box::leak(format!("sidebar-{kind}-{index}").into_boxed_str());
        f32::from(visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn")).size.width)
    };
    let name = visual.update(|window, _| {
        let style = window.text_style();
        let run = gpui::TextRun { color: gpui::black(), ..style.to_run("orders".len()) };
        f32::from(window.text_system().shape_line("orders".into(), gpui::px(12.0), &[run], None).width)
    });
    (width_of(visual, "label"), width_of(visual, "stats"), width_of(visual, "row"), name)
}

#[gpui::test]
fn a_table_name_keeps_its_width_when_the_sidebar_is_too_narrow_for_its_estimate(cx: &mut TestAppContext) {
    // Wide: the name is whole and the estimate has its whole label.
    let (label, stats, _row, name) = stats_row_widths(cx, 700.0);
    assert!(label >= name, "the name is whole: {label} vs {name}");
    assert!(stats > 60.0, "the estimate is shown: {stats}");
    let whole = stats;
    // The narrowest sidebar: the estimate gave way, the name did not lose a pixel to it.
    let (label, stats, row, name) = stats_row_widths(cx, crate::theme::SIDEBAR_MIN_WIDTH);
    assert!(label >= name, "the name is whole: {label} vs {name}");
    assert!(stats < whole, "the estimate gave way: {stats} vs {whole}");
    assert!(stats + label <= row, "nothing overflows the row: {stats} + {label} vs {row}");
}

#[gpui::test]
fn the_tooltip_of_a_table_opens_with_its_whole_name(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    let index = app.read_with(visual, |this, _| this.navigator.visible_index(&orders)).expect("a visible row");
    hover_row(visual, index);
    settle(visual);
    assert!(ns_has(visual, "sidebar-tooltip-name public.orders".into()), "the schema-qualified name leads the tooltip");
}

#[test]
fn the_copy_entry_of_a_table_row_names_what_it_copies() {
    assert_eq!(super::object_actions::ObjectMenuItem::CopyObjects.label_for(false), "Copy Table");
    assert_eq!(super::object_actions::ObjectMenuItem::CopyObjects.label_for(true), "Copy Tables");
    assert_eq!(super::object_actions::ObjectMenuItem::Backup.label_for(true), super::object_actions::ObjectMenuItem::Backup.label_for(false));
}
