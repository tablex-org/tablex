// Batch D of the connection tree: the "New ..." entries (templates in a new SQL tab) and the per-folder
// filters, both through the real context menus, the submenu, the Filter dialog and the sidebar rows.
//
// * The Postgres fixture tree stands in for every dialect: its connection's kind is changed, which is
//   all the menus read (the rows stay the PostgreSQL ones). The template text itself is covered per
//   dialect in `object_actions::create_templates`.
// * MongoDB: the scenario fixture's database and collection rows.

use crate::app::object_actions::CreateKind;

const NC_CONNECTION: &str = crate::ui_fixture::POSTGRES_ID;

fn nc_create_selectors(visual: &mut gpui::VisualTestContext) -> Vec<&'static str> {
    CreateKind::ALL.iter().map(|kind| kind.selector()).filter(|selector| visual.debug_bounds(selector).is_some()).collect()
}

/// The Postgres fixture rig in a window tall enough for every row of its long tree.
fn nc_rig(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, PeLog, PeFail) {
    let (app, visual, ran, fail) = oa_pg(cx);
    visual.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(3000.0)));
    settle(visual);
    (app, visual, ran, fail)
}

fn nc_names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| format!("object-create-{name}")).collect()
}

fn nc_row_index(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) -> usize {
    settle(visual);
    app.read_with(visual, |this, _| this.navigator.visible_index(id)).unwrap_or_else(|| panic!("{id:?} is a visible row"))
}

/// Right-clicks the row of `id` and walks the menu with the keyboard until a "New ..." entry shows (a
/// submenu opens when it is reached; a folder's entry is in the menu itself). No entry: the menu is left open.
fn nc_open(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) {
    let index = nc_row_index(app, visual, id);
    right_click_row(visual, index);
    settle(visual);
    for _ in 0..40 {
        if !nc_create_selectors(visual).is_empty() {
            return;
        }
        visual.simulate_keystrokes("down");
        settle(visual);
    }
}

/// The "New ..." selectors the menu of `id` shows.
fn nc_entries(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) -> Vec<String> {
    nc_open(app, visual, id);
    let found = nc_create_selectors(visual).into_iter().map(str::to_owned).collect();
    visual.simulate_keystrokes("escape");
    visual.simulate_keystrokes("escape");
    settle(visual);
    found
}

/// Picks `selector` in the menu of `id`; returns the SQL tab it opened: connection, database, schema, text.
fn nc_pick(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &NavigatorNodeId,
    selector: &'static str,
) -> (String, String, Option<String>, String) {
    let before = gs_tab_count(app, visual);
    nc_open(app, visual, id);
    let entry = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is in the menu"));
    visual.simulate_click(entry.center(), Default::default());
    settle(visual);
    gs_tab(app, visual, before)
}

/// What the active tab's editor has selected (the editor is made when the tab is first drawn).
fn nc_selected(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> String {
    settle(visual);
    app.update(visual, |this, cx| {
        let tab = this.workspace.active_tab().expect("an active tab").id;
        let editor = this.query_editors.by_tab.get(&tab).cloned().expect("the tab's editor is drawn");
        let state = editor.read(cx);
        let (text, range) = (state.value().to_string(), state.selected_range());
        text[range].to_owned()
    })
}

fn nc_set_kind(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, kind: ConnectionKind) {
    app.update(visual, |this, _| {
        this.saved_connections.iter_mut().find(|saved| saved.matches_id(NC_CONNECTION)).expect("the fixture connection").kind = kind;
    });
}

fn nc_schema() -> NavigatorNodeId {
    NavigatorNodeId::Schema { connection: NC_CONNECTION.into(), database: "warehouse".into(), schema: "public".into() }
}

fn nc_database() -> NavigatorNodeId {
    NavigatorNodeId::Database { connection: NC_CONNECTION.into(), database: "warehouse".into() }
}

/// The folders under the `orders` table, loaded and visible.
fn nc_table_folder(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, folder: crate::navigator::TableFolder) -> NavigatorNodeId {
    let orders = oa_pg_object("orders");
    app.update(visual, |this, _| {
        if !this.navigator.is_expanded(&orders) {
            this.apply_scenario_expansion(&orders);
        }
    });
    settle(visual);
    NavigatorNodeId::TableFolder {
        connection: NC_CONNECTION.into(),
        database: "warehouse".into(),
        schema: "public".into(),
        object: "orders".into(),
        folder,
    }
}

// ---- which entry where, per dialect --------------------------------------------------------------------------------

#[gpui::test]
fn each_dialect_offers_a_new_entry_only_where_it_can_create_it(cx: &mut TestAppContext) {
    use crate::navigator::TableFolder;
    let (app, visual, _, _) = nc_rig(cx);
    let view = nc_children(&app, visual, ObjectGroup::Views).into_iter().next().expect("a fixture view");
    let offered = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId| -> Vec<String> {
        app.read_with(visual, |this, _| this.create_menu_kinds(id).into_iter().map(|kind| kind.selector().to_owned()).collect())
    };
    let groups = [
        ObjectGroup::Tables,
        ObjectGroup::Views,
        ObjectGroup::MaterializedViews,
        ObjectGroup::Functions,
        ObjectGroup::Procedures,
        ObjectGroup::Sequences,
        ObjectGroup::Triggers,
        ObjectGroup::Types,
    ];
    let folders = [TableFolder::Columns, TableFolder::Indexes, TableFolder::Constraints, TableFolder::ForeignKeys, TableFolder::Triggers];
    let orders = oa_pg_object("orders");
    // (kind, the schema row, each group in `groups`, the table row, each table folder in `folders`)
    type Row = (ConnectionKind, &'static [&'static str], [&'static [&'static str]; 8], &'static [&'static str], [&'static [&'static str]; 5]);
    let matrix: [Row; 5] = [
        (
            ConnectionKind::Postgres,
            &["view", "materialized-view", "function", "procedure", "sequence", "trigger", "type"],
            [&[], &["view"], &["materialized-view"], &["function"], &["procedure"], &["sequence"], &["trigger"], &["type"]],
            &["trigger", "column", "index", "constraint", "foreign-key"],
            [&["column"], &["index"], &["constraint"], &["foreign-key"], &["trigger"]],
        ),
        (
            ConnectionKind::Mysql,
            &["view", "function", "procedure", "trigger"],
            [&[], &["view"], &[], &["function"], &["procedure"], &[], &["trigger"], &[]],
            &["trigger", "column", "index", "constraint", "foreign-key"],
            [&["column"], &["index"], &["constraint"], &["foreign-key"], &["trigger"]],
        ),
        (
            ConnectionKind::Mariadb,
            &["view", "function", "procedure", "sequence", "trigger"],
            [&[], &["view"], &[], &["function"], &["procedure"], &["sequence"], &["trigger"], &[]],
            &["trigger", "column", "index", "constraint", "foreign-key"],
            [&["column"], &["index"], &["constraint"], &["foreign-key"], &["trigger"]],
        ),
        (
            ConnectionKind::Sqlite,
            &["view"],
            [&[], &["view"], &[], &[], &[], &[], &[], &[]],
            &["column", "index", "constraint"],
            [&["column"], &["index"], &["constraint"], &[], &[]],
        ),
        (
            ConnectionKind::Clickhouse,
            &["view", "materialized-view"],
            [&[], &["view", "materialized-view"], &[], &[], &[], &[], &[], &[]],
            &["column", "index"],
            [&["column"], &["index"], &[], &[], &[]],
        ),
    ];
    for (kind, schema, by_group, table, by_folder) in matrix {
        nc_set_kind(&app, visual, kind);
        assert_eq!(offered(&app, visual, &nc_schema()), nc_names(schema), "{kind:?} schema row");
        // A database row creates in its default schema: the same entries.
        assert_eq!(offered(&app, visual, &nc_database()), nc_names(schema), "{kind:?} database row");
        for (group, expected) in groups.into_iter().zip(by_group) {
            assert_eq!(offered(&app, visual, &fixture_group(group)), nc_names(expected), "{kind:?} {group:?} folder");
        }
        assert_eq!(offered(&app, visual, &orders), nc_names(table), "{kind:?} table row");
        for (folder, expected) in folders.into_iter().zip(by_folder) {
            let id = nc_table_folder(&app, visual, folder);
            assert_eq!(offered(&app, visual, &id), nc_names(expected), "{kind:?} {folder:?} folder");
        }
        // A view takes none.
        assert!(offered(&app, visual, &oa_pg_object(&view)).is_empty(), "{kind:?} view row");
    }
}

#[gpui::test]
fn the_context_menus_show_the_new_entries_of_their_dialect(cx: &mut TestAppContext) {
    use crate::navigator::TableFolder;
    let (app, visual, _, _) = nc_rig(cx);
    // PostgreSQL: a schema row has the lot in its New submenu; a folder has its own entry in the menu itself.
    assert_eq!(
        nc_entries(&app, visual, &nc_schema()),
        nc_names(&["view", "materialized-view", "function", "procedure", "sequence", "trigger", "type"])
    );
    assert_eq!(nc_entries(&app, visual, &fixture_group(ObjectGroup::Views)), nc_names(&["view"]));
    assert_eq!(nc_entries(&app, visual, &fixture_group(ObjectGroup::Sequences)), nc_names(&["sequence"]));
    assert_eq!(nc_entries(&app, visual, &oa_pg_object("orders")), nc_names(&["trigger", "column", "index", "constraint", "foreign-key"]));
    let indexes = nc_table_folder(&app, visual, TableFolder::Indexes);
    assert_eq!(nc_entries(&app, visual, &indexes), nc_names(&["index"]));
    // The Tables folder keeps its New Table and offers no other "New".
    assert!(nc_entries(&app, visual, &fixture_group(ObjectGroup::Tables)).is_empty());
    assert_eq!(oa_entries_on(&app, visual, &fixture_group(ObjectGroup::Tables))[0], "object-menu-new-table");

    nc_set_kind(&app, visual, ConnectionKind::Mysql);
    assert_eq!(nc_entries(&app, visual, &nc_schema()), nc_names(&["view", "function", "procedure", "trigger"]), "no sequence on MySQL");
    assert_eq!(nc_entries(&app, visual, &oa_pg_object("orders")), nc_names(&["trigger", "column", "index", "constraint", "foreign-key"]));
    nc_set_kind(&app, visual, ConnectionKind::Sqlite);
    assert_eq!(nc_entries(&app, visual, &nc_schema()), nc_names(&["view"]));
    assert_eq!(nc_entries(&app, visual, &oa_pg_object("orders")), nc_names(&["column", "index", "constraint"]), "no foreign key on SQLite");
    nc_set_kind(&app, visual, ConnectionKind::Clickhouse);
    assert_eq!(nc_entries(&app, visual, &nc_schema()), nc_names(&["view", "materialized-view"]));
    assert_eq!(nc_entries(&app, visual, &oa_pg_object("orders")), nc_names(&["column", "index"]), "no foreign key on ClickHouse");
}

// ---- what a click opens ---------------------------------------------------------------------------------------------

#[gpui::test]
fn a_new_entry_opens_a_sql_tab_on_the_row_with_the_template_and_its_first_placeholder_selected(cx: &mut TestAppContext) {
    use crate::navigator::TableFolder;
    let (app, visual, ran, _) = nc_rig(cx);
    let (connection, database, schema, sql) = nc_pick(&app, visual, &fixture_group(ObjectGroup::Views), "object-create-view");
    assert_eq!((connection.as_str(), database.as_str(), schema.as_deref()), (NC_CONNECTION, "warehouse", Some("public")));
    assert_eq!(
        sql,
        "-- Name the view and write its query, then run the statement.\nCREATE VIEW \"public\".\"new_view\" AS\nSELECT column_name\nFROM table_name;"
    );
    assert_eq!(nc_selected(&app, visual), "new_view");

    let (.., sql) = nc_pick(&app, visual, &fixture_group(ObjectGroup::Sequences), "object-create-sequence");
    assert!(sql.contains("CREATE SEQUENCE \"public\".\"new_sequence\""), "{sql}");
    assert_eq!(nc_selected(&app, visual), "new_sequence");

    // From the schema row's submenu.
    let (.., schema, sql) = nc_pick(&app, visual, &nc_schema(), "object-create-type");
    assert_eq!(schema.as_deref(), Some("public"));
    assert!(sql.contains("CREATE TYPE \"public\".\"new_type\" AS ENUM"), "{sql}");

    // A table row: the table is filled in.
    let (connection, database, schema, sql) = nc_pick(&app, visual, &oa_pg_object("orders"), "object-create-index");
    assert_eq!((connection.as_str(), database.as_str(), schema.as_deref()), (NC_CONNECTION, "warehouse", Some("public")));
    assert!(sql.contains("CREATE INDEX \"new_index\"\n  ON \"public\".\"orders\" (\"column_name\");"), "{sql}");
    assert_eq!(nc_selected(&app, visual), "new_index");
    let (.., sql) = nc_pick(&app, visual, &oa_pg_object("orders"), "object-create-trigger");
    assert!(sql.contains("BEFORE INSERT ON \"public\".\"orders\""), "{sql}");

    // The folders under the table.
    for (folder, selector, expected) in [
        (TableFolder::Columns, "object-create-column", "ALTER TABLE \"public\".\"orders\"\n  ADD COLUMN \"new_column\" text;"),
        (TableFolder::ForeignKeys, "object-create-foreign-key", "ADD CONSTRAINT \"new_foreign_key\" FOREIGN KEY (\"column_name\")"),
        (TableFolder::Constraints, "object-create-constraint", "ADD CONSTRAINT \"new_constraint\" UNIQUE (\"column_name\")"),
    ] {
        let id = nc_table_folder(&app, visual, folder);
        let (.., sql) = nc_pick(&app, visual, &id, selector);
        assert!(sql.contains(expected), "{folder:?}: {sql}");
    }

    // The other dialects spell it their way, on the same row.
    nc_set_kind(&app, visual, ConnectionKind::Mysql);
    let (.., sql) = nc_pick(&app, visual, &fixture_group(ObjectGroup::Functions), "object-create-function");
    assert!(sql.contains("CREATE FUNCTION `public`.`new_function`(arg1 INT)"), "{sql}");
    nc_set_kind(&app, visual, ConnectionKind::Clickhouse);
    let (.., sql) = nc_pick(&app, visual, &oa_pg_object("orders"), "object-create-index");
    assert!(sql.contains("ALTER TABLE `public`.`orders`\n  ADD INDEX `new_index` `column_name` TYPE minmax GRANULARITY 1;"), "{sql}");
    // Nothing ran: no statement reached the executor.
    assert!(ran.lock().unwrap().is_empty(), "{:?}", ran.lock().unwrap());
}

#[gpui::test]
fn an_entry_the_dialect_cannot_create_opens_nothing(cx: &mut TestAppContext) {
    let (app, visual, _, _) = nc_rig(cx);
    nc_set_kind(&app, visual, ConnectionKind::Mysql);
    let before = gs_tab_count(&app, visual);
    app.update(visual, |this, cx| this.open_create_template(CreateKind::Sequence, &nc_schema(), cx));
    app.update(visual, |this, cx| this.open_create_template(CreateKind::View, &oa_pg_object("orders"), cx));
    assert_eq!(gs_tab_count(&app, visual), before);
}

#[gpui::test]
fn a_mongodb_database_offers_new_collection_and_a_collection_the_index_dialog(cx: &mut TestAppContext) {
    let (app, visual) = mongo_admin_app(cx);
    // New Collection: a script tab on the database, `createCollection` with its name selected.
    let before = gs_tab_count(&app, visual);
    right_click_label(&app, visual, "fixture_db_0");
    click_debug_button(visual, "mongo-menu-new-collection");
    settle(visual);
    assert_eq!(gs_tab_count(&app, visual), before + 1);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("the new tab");
        assert!(tab.query_mode.is_mongo_script());
        assert_eq!((tab.connection_id.as_str(), tab.database.as_str()), (crate::ui_fixture::MONGO_ID, "fixture_db_0"));
        assert_eq!(tab.sql, "// Name the collection, then run the script.\ndb.createCollection(\"new_collection\")");
    });
    assert_eq!(nc_selected(&app, visual), "new_collection");

    // New Index: the existing index manager, its create form already open.
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-index");
    settle(visual);
    assert!(visual.debug_bounds("mongo-index-table").is_some(), "the index manager opens");
    assert!(visual.debug_bounds("mongo-index-form").is_some(), "with its form open");
    // The manager's own entry opens it without the form.
    visual.simulate_keystrokes("escape");
    settle(visual);
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-manage-indexes");
    settle(visual);
    assert!(visual.debug_bounds("mongo-index-table").is_some() && visual.debug_bounds("mongo-index-form").is_none());
}

// ---- per-folder filters ---------------------------------------------------------------------------------------------

fn nc_prefs_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tablex-folder-filter-{}.json", uuid::Uuid::new_v4()))
}

/// The labels of the visible children of a fixture folder (opened first when it is closed).
fn nc_children(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, group: ObjectGroup) -> Vec<String> {
    let id = fixture_group(group);
    app.update(visual, |this, _| {
        if !this.navigator.is_expanded(&id) {
            this.apply_scenario_expansion(&id);
        }
        let children = this.navigator.loaded_children(&id).unwrap_or_default().to_vec();
        children
            .iter()
            .filter_map(|child| this.navigator.visible_index(child).and_then(|at| this.navigator.visible_row_at(at)))
            .map(|row| row.label)
            .collect()
    })
}

fn nc_tables(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    nc_children(app, visual, ObjectGroup::Tables)
}

/// Opens the Filter dialog of the folder `id` from its menu, fills the two fields and applies.
fn nc_filter(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId, include: &str, exclude: &str) {
    oa_menu_on(app, visual, id, "folder-menu-filter");
    for (selector, text) in [("folder-filter-include", include), ("folder-filter-exclude", exclude)] {
        oa_type(visual, selector, text);
        if text.is_empty() {
            // Typing nothing leaves the selection: delete it.
            visual.simulate_keystrokes("backspace");
        }
    }
    oa_click(visual, "folder-filter-apply");
    assert!(visual.debug_bounds("folder-filter-apply").is_none(), "Apply closes the dialog");
}

fn nc_filter_mark(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) -> bool {
    let index = nc_row_index(app, visual, id);
    let selector: &'static str = Box::leak(format!("sidebar-filter-{index}").into_boxed_str());
    visual.debug_bounds(selector).is_some()
}

#[gpui::test]
fn a_folder_filter_narrows_that_folder_only_is_marked_and_clears_from_the_menu(cx: &mut TestAppContext) {
    let (app, visual, _, _) = nc_rig(cx);
    let path = nc_prefs_path();
    app.update(visual, |this, _| this.sidebar.path = Some(path.clone()));
    let (tables, views) = (fixture_group(ObjectGroup::Tables), fixture_group(ObjectGroup::Views));
    let all_tables = nc_tables(&app, visual);
    let all_views = nc_children(&app, visual, ObjectGroup::Views);
    assert!(all_tables.len() >= 4 && all_tables.contains(&"fixture_table_03".to_owned()), "{all_tables:?}");
    assert!(!nc_filter_mark(&app, visual, &tables));

    // Include (wildcards, any case) and exclude.
    nc_filter(&app, visual, &tables, "FIXTURE_TABLE_0?, orders", "*_03");
    let expected: Vec<String> = all_tables
        .iter()
        .filter(|name| (name.starts_with("fixture_table_0") && name.len() == "fixture_table_0".len() + 1 || *name == "orders") && !name.ends_with("_03"))
        .cloned()
        .collect();
    assert!(expected.len() < all_tables.len() && expected.contains(&"orders".to_owned()), "{expected:?}");
    assert_eq!(nc_tables(&app, visual), expected);
    // Only that folder: the Views folder lists what it listed, and has no mark; the Tables one has it.
    assert_eq!(nc_children(&app, visual, ObjectGroup::Views), all_views);
    assert!(nc_filter_mark(&app, visual, &tables) && !nc_filter_mark(&app, visual, &views));
    let note = app.read_with(visual, |this, _| this.navigator.folder_filter_note(&tables));
    assert_eq!(note.flatten().map(|count| (count.shown, count.total)), Some((expected.len(), all_tables.len())));
    // The filter is stored in the sidebar preferences file.
    let stored = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert_eq!(stored.folder_filters.len(), 1);
    assert_eq!(stored.folder_filters[0].include, ["FIXTURE_TABLE_0?", "orders"]);
    assert_eq!(stored.folder_filters[0].exclude, ["*_03"]);

    // Cancel changes nothing; the dialog opens with what is set.
    oa_menu_on(&app, visual, &tables, "folder-menu-filter");
    oa_type(visual, "folder-filter-include", "nothing_like_this");
    oa_click(visual, "folder-filter-cancel");
    assert_eq!(nc_tables(&app, visual), expected);

    // Clear filter is in the folder's menu while a filter is set, and not on a folder without one.
    let index = nc_row_index(&app, visual, &views);
    right_click_row(visual, index);
    settle(visual);
    assert!(visual.debug_bounds("folder-menu-filter").is_some() && visual.debug_bounds("folder-menu-clear-filter").is_none());
    visual.simulate_keystrokes("escape");
    settle(visual);
    oa_menu_on(&app, visual, &tables, "folder-menu-clear-filter");
    assert_eq!(nc_tables(&app, visual), all_tables);
    assert!(!nc_filter_mark(&app, visual, &tables));
    assert!(crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().folder_filters.is_empty());

    // The dialog's own Clear filter.
    nc_filter(&app, visual, &tables, "orders", "");
    assert_eq!(nc_tables(&app, visual), ["orders"]);
    oa_menu_on(&app, visual, &tables, "folder-menu-filter");
    oa_click(visual, "folder-filter-clear");
    assert_eq!(nc_tables(&app, visual), all_tables);
    // Applying two empty fields clears too.
    nc_filter(&app, visual, &tables, "orders", "");
    nc_filter(&app, visual, &tables, "", "");
    assert_eq!(nc_tables(&app, visual), all_tables);
    assert!(app.read_with(visual, |this, _| this.navigator.preferences().folder_filters.is_empty()));
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn a_folder_filter_survives_a_restart(cx: &mut TestAppContext) {
    let (first, view, _, _) = nc_rig(cx);
    let path = nc_prefs_path();
    first.update(view, |this, _| this.sidebar.path = Some(path.clone()));
    let all_tables = nc_tables(&first, view);
    nc_filter(&first, view, &fixture_group(ObjectGroup::Tables), "orders", "");
    assert_eq!(nc_tables(&first, view), ["orders"]);

    // A new app over the same file reads the filter before it builds the tree.
    let (second, view2) = sidebar_test_app(cx);
    second.update(view2, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.reload_sidebar_preferences(cx).unwrap();
        this.seed_connection_tree_scenario();
    });
    settle(view2);
    assert_eq!(nc_tables(&second, view2), ["orders"]);
    assert!(all_tables.len() > 1);
    // The Views folder of the restarted app is not narrowed.
    assert!(second.read_with(view2, |this, _| this.navigator.folder_filter_note(&fixture_group(ObjectGroup::Views)).is_none()));
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn the_databases_of_a_connection_and_the_schemas_of_a_database_have_their_own_filter(cx: &mut TestAppContext) {
    let (app, visual, _, _) = nc_rig(cx);
    let path = nc_prefs_path();
    app.update(visual, |this, _| this.sidebar.path = Some(path.clone()));
    let connection = NavigatorNodeId::Connection { connection: NC_CONNECTION.into() };
    let databases = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> Vec<String> {
        app.read_with(visual, |this, _| {
            this.navigator
                .loaded_children(&NavigatorNodeId::Connection { connection: NC_CONNECTION.into() })
                .unwrap_or_default()
                .iter()
                .filter(|id| this.navigator.visible_index(id).is_some())
                .filter_map(|id| match id {
                    NavigatorNodeId::Database { database, .. } => Some(database.clone()),
                    _ => None,
                })
                .collect()
        })
    };
    assert_eq!(databases(&app, visual), ["warehouse", "reporting"]);
    nc_filter(&app, visual, &connection, "ware*", "");
    assert_eq!(databases(&app, visual), ["warehouse"]);
    assert!(nc_filter_mark(&app, visual, &connection));
    oa_menu_on(&app, visual, &connection, "folder-menu-clear-filter");
    assert_eq!(databases(&app, visual), ["warehouse", "reporting"]);

    // The schemas of the warehouse database: a filter that matches none leaves the database with none listed.
    let schemas = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> usize {
        app.read_with(visual, |this, _| {
            this.navigator.visible_rows().iter().filter(|row| matches!(row.id, NavigatorNodeId::Schema { .. })).count()
        })
    };
    let before = schemas(&app, visual);
    assert!(before > 0);
    nc_filter(&app, visual, &nc_database(), "zzz*", "");
    assert_eq!(schemas(&app, visual), 0);
    assert!(nc_filter_mark(&app, visual, &nc_database()));
    // Another folder of the same level is not narrowed: the other database lists the same.
    assert_eq!(databases(&app, visual), ["warehouse", "reporting"]);
    oa_menu_on(&app, visual, &nc_database(), "folder-menu-clear-filter");
    assert_eq!(schemas(&app, visual), before);
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn a_folder_filter_applies_in_memory_without_a_preferences_file_and_reports_nothing(cx: &mut TestAppContext) {
    let (app, visual, _, _) = nc_rig(cx);
    app.update(visual, |this, _| this.sidebar.path = None);
    let all_tables = nc_tables(&app, visual);
    nc_filter(&app, visual, &fixture_group(ObjectGroup::Tables), "orders", "");
    assert_eq!(nc_tables(&app, visual), ["orders"]);
    app.read_with(visual, |this, _| {
        assert!(this.sidebar.error.is_none() && this.sidebar.notice.is_none(), "{:?} {:?}", this.sidebar.error, this.sidebar.notice);
    });
    oa_menu_on(&app, visual, &fixture_group(ObjectGroup::Tables), "folder-menu-clear-filter");
    assert_eq!(nc_tables(&app, visual), all_tables);
}

#[gpui::test]
fn a_folder_filter_that_cannot_be_saved_is_reported_once(cx: &mut TestAppContext) {
    let (app, visual, _, _) = nc_rig(cx);
    // A directory where the file should be: the save fails.
    let dir = std::env::temp_dir().join(format!("tablex-folder-filter-dir-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    app.update(visual, |this, _| this.sidebar.path = Some(dir.clone()));
    app.update(visual, |this, cx| {
        let filter = crate::sidebar_preferences::FolderFilter {
            key: crate::sidebar_preferences::FolderKey {
                connection: NC_CONNECTION.into(),
                database: "warehouse".into(),
                schema: "public".into(),
                folder: crate::sidebar_preferences::FolderKind::Objects(ObjectGroup::Tables),
            },
            include: vec!["orders".into()],
            exclude: Vec::new(),
        };
        this.apply_folder_filter(filter, cx);
        assert!(this.sidebar.error.as_deref().is_some_and(|error| error.contains("Could not save")));
        assert!(this.sidebar.notice.is_none(), "the error line is the one report: {:?}", this.sidebar.notice);
    });
    std::fs::remove_dir_all(dir).unwrap();
}
