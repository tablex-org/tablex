// The sidebar's names and keys: Copy Name / Copy Qualified Name, dragging objects onto a query
// editor, and F2 / Delete / F4 on the focused tree (see `sidebar_names`).
//
// * The Postgres fixture tree (`oa_pg`, with the injected executor that logs instead of running)
//   stands for PostgreSQL; its saved connection is flipped to MySQL / ClickHouse where the quoting
//   differs. The Mongo and Redis fixtures stand for their kinds.
// * Keys go through the real keymap (`actions::bind_keys`) and the focused sidebar.

use crate::app::sidebar_names::NameStyle;

fn sn_clipboard(visual: &mut gpui::VisualTestContext) -> Option<String> {
    visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

fn sn_copy(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, ids: &[NavigatorNodeId], style: NameStyle) -> Option<String> {
    visual.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into())));
    let ids = ids.to_vec();
    app.update(visual, |this, cx| this.copy_navigator_names(&ids, style, cx));
    sn_clipboard(visual)
}

fn sn_column(connection: &str, database: &str, schema: &str, object: &str, column: &str) -> NavigatorNodeId {
    NavigatorNodeId::Column {
        connection: connection.into(),
        database: database.into(),
        schema: schema.into(),
        object: object.into(),
        column: column.into(),
    }
}

fn sn_set_kind(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str, kind: ConnectionKind) {
    app.update(visual, |this, _| {
        this.saved_connections.iter_mut().find(|saved| saved.matches_id(connection)).expect("the fixture connection").kind = kind;
    });
}

/// A SQL tab on the fixture connection with `sql` in it; its editor is rendered.
fn sn_editor_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, database: &str, schema: Option<&str>, sql: &str) -> u64 {
    let (database, schema, sql) = (database.to_owned(), schema.map(str::to_owned), sql.to_owned());
    let tab = app.update(visual, |this, _| {
        let id = this.workspace.open_query_tab_with_metadata(
            "Query",
            TabMetadata { connection_id: OA_PG.into(), database, schema, ..Default::default() },
        );
        this.workspace.tab_mut(id).expect("the new tab").sql = sql;
        id
    });
    settle(visual);
    tab
}

fn sn_tab_sql(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64) -> String {
    app.read_with(visual, |this, _| this.workspace.tab(tab).expect("the tab").sql.clone())
}

fn sn_editor(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64) -> gpui::Entity<EditorState> {
    app.read_with(visual, |this, _| this.query_editors.by_tab.get(&tab).expect("the editor is rendered").clone())
}

/// Drops `ids` on the editor of `tab` at the window point `at`.
fn sn_drop(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64, ids: &[NavigatorNodeId], at: gpui::Point<gpui::Pixels>) {
    let ids = ids.to_vec();
    visual.update(|window, cx| app.update(cx, |this, cx| this.drop_objects_on_editor(tab, &ids, at, window, cx)));
    settle(visual);
}

fn sn_nowhere() -> gpui::Point<gpui::Pixels> {
    gpui::point(gpui::px(-10.0), gpui::px(-10.0))
}

fn sn_focus_sidebar_on(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) {
    let id = id.clone();
    visual.update(|window, cx| {
        actions::bind_keys(cx);
        app.update(cx, |this, cx| {
            assert!(this.navigator.select_visible(&id, false, false), "{id:?} is a visible row");
            this.sidebar.focus.focus(window, cx);
            cx.notify();
        });
    });
    settle(visual);
}

#[gpui::test]
fn copy_name_is_the_bare_name_and_copy_qualified_name_is_quoted_for_each_dialect(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let table = oa_pg_object("orders");
    let column = sn_column(OA_PG, "warehouse", "public", "orders", "we\"ird");
    let schema = schema_node(OA_PG, "warehouse", "public");
    let database = database_node(OA_PG, "warehouse");
    // PostgreSQL: "schema"."table", every part quoted, a quote doubled.
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&table), NameStyle::Plain).as_deref(), Some("orders"));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&table), NameStyle::Qualified).as_deref(), Some("\"public\".\"orders\""));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&column), NameStyle::Plain).as_deref(), Some("we\"ird"));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&column), NameStyle::Qualified).as_deref(), Some("\"public\".\"orders\".\"we\"\"ird\""));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&schema), NameStyle::Plain).as_deref(), Some("public"));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&schema), NameStyle::Qualified).as_deref(), Some("\"public\""));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&database), NameStyle::Qualified).as_deref(), Some("\"warehouse\""));
    // MySQL: backticks; the schema of its tree is the database.
    sn_set_kind(&app, visual, OA_PG, ConnectionKind::Mysql);
    let shop = object_node(OA_PG, "shop", "shop", "or`ders");
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&shop), NameStyle::Plain).as_deref(), Some("or`ders"));
    assert_eq!(sn_copy(&app, visual, &[shop], NameStyle::Qualified).as_deref(), Some("`shop`.`or``ders`"));
    // A group row has no name to copy: the clipboard is untouched.
    let objects = NavigatorNodeId::ObjectGroup { connection: OA_PG.into(), database: "warehouse".into(), schema: "public".into(), group: ObjectGroup::Tables };
    assert_eq!(sn_copy(&app, visual, &[objects], NameStyle::Plain).as_deref(), Some("untouched"));
}

#[gpui::test]
fn copying_several_rows_puts_one_name_per_line_and_an_unsafe_name_is_refused_with_a_notice(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let (a, b) = (oa_pg_object("orders"), oa_pg_object("customers"));
    assert_eq!(sn_copy(&app, visual, &[a.clone(), b.clone()], NameStyle::Plain).as_deref(), Some("orders\ncustomers"));
    assert_eq!(sn_copy(&app, visual, &[a, b], NameStyle::Qualified).as_deref(), Some("\"public\".\"orders\"\n\"public\".\"customers\""));
    // ClickHouse cannot tell how a backslash in a quoted name reads: no quoted name is made of it.
    sn_set_kind(&app, visual, OA_PG, ConnectionKind::Clickhouse);
    let odd = object_node(OA_PG, "shop", "shop", "back\\slash");
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&odd), NameStyle::Qualified).as_deref(), Some("untouched"));
    let notice = app.read_with(visual, |this, _| this.sidebar.notice.clone());
    assert!(notice.as_deref().is_some_and(|notice| notice.contains("backslash")), "{notice:?}");
    assert_eq!(sn_copy(&app, visual, &[odd], NameStyle::Plain).as_deref(), Some("back\\slash"), "the bare name is no statement");
}

#[gpui::test]
fn cmd_c_and_cmd_shift_c_on_the_focused_tree_copy_the_selection_one_per_line(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let (orders, items) = (oa_pg_object("orders"), oa_pg_object("fixture_table_01"));
    // Both are visible rows once Tables is open.
    sn_focus_sidebar_on(&app, visual, &orders);
    app.update(visual, |this, _| {
        this.navigator.select_visible(&items, true, false);
    });
    settle(visual);
    visual.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into())));
    visual.simulate_keystrokes(&primary("c"));
    settle(visual);
    let plain = sn_clipboard(visual).expect("copied");
    assert_eq!(plain.lines().count(), 2, "{plain}");
    assert!(plain.contains("orders") && plain.contains("fixture_table_01") && !plain.contains('"'), "{plain}");
    visual.simulate_keystrokes(&primary("shift-c"));
    settle(visual);
    let qualified = sn_clipboard(visual).expect("copied");
    assert_eq!(qualified.lines().count(), 2, "{qualified}");
    assert!(qualified.contains("\"public\".\"orders\"") && qualified.contains("\"public\".\"fixture_table_01\""), "{qualified}");
}

#[gpui::test]
fn the_object_menu_offers_copy_name_and_copy_qualified_name_and_they_copy(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    oa_menu_on(&app, visual, &orders, "sidebar-menu-copy-qualified-name");
    assert_eq!(sn_clipboard(visual).as_deref(), Some("\"public\".\"orders\""));
    oa_menu_on(&app, visual, &orders, "sidebar-menu-copy-name");
    assert_eq!(sn_clipboard(visual).as_deref(), Some("orders"));
}

#[gpui::test]
fn mongodb_collections_copy_their_name_and_database_dot_collection(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.open_or_activate_collection_tab(crate::ui_fixture::MONGO_ID, "fixture_db_0", "fixture_collection_01");
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    let collection = app.read_with(visual, |this, _| this.navigator.selected().cloned().expect("the located collection"));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&collection), NameStyle::Plain).as_deref(), Some("fixture_collection_01"));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&collection), NameStyle::Qualified).as_deref(), Some("fixture_db_0.fixture_collection_01"));
    assert!(app.read_with(visual, |this, _| this.object_drag_supported(&collection)), "a collection drags");
}

#[gpui::test]
fn redis_keys_copy_their_key_name(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    let key = fixture_key_node("user:1001");
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&key), NameStyle::Plain).as_deref(), Some("user:1001"));
    assert_eq!(sn_copy(&app, visual, std::slice::from_ref(&key), NameStyle::Qualified).as_deref(), Some("user:1001"));
    assert!(!app.read_with(visual, |this, _| this.object_drag_supported(&key)), "a key is not dropped on an editor");
}

#[gpui::test]
fn only_tables_views_columns_schemas_and_collections_drag(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    app.read_with(visual, |this, _| {
        assert!(this.object_drag_supported(&oa_pg_object("orders")));
        assert!(this.object_drag_supported(&sn_column(OA_PG, "warehouse", "public", "orders", "id")));
        assert!(this.object_drag_supported(&schema_node(OA_PG, "warehouse", "public")));
        assert!(!this.object_drag_supported(&database_node(OA_PG, "warehouse")));
        assert!(!this.object_drag_supported(&connection_node(OA_PG)));
        assert!(!this.object_drag_supported(&group_node(OA_PG, "warehouse", "public", ObjectGroup::Tables)));
    });
}

#[gpui::test]
fn a_table_dropped_on_an_editor_is_quoted_and_qualified_unless_it_is_in_the_editors_namespace(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    let elsewhere = object_node(OA_PG, "warehouse", "audit", "log");

    // The editor's own schema: the bare, quoted name.
    let tab = sn_editor_tab(&app, visual, "warehouse", Some("public"), "SELECT * FROM ");
    let editor = sn_editor(&app, visual, tab);
    visual.update(|window, cx| editor.update(cx, |editor, cx| editor.set_cursor_position(gpui_component::input::Position::new(0, 14), window, cx)));
    sn_drop(&app, visual, tab, std::slice::from_ref(&orders), sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, tab), "SELECT * FROM \"orders\"");

    // Another schema, and a second object: qualified, comma-separated, at the caret.
    sn_drop(&app, visual, tab, &[elsewhere.clone(), orders.clone()], sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, tab), "SELECT * FROM \"orders\"\"audit\".\"log\", \"orders\"");

    // An editor without a schema qualifies everything; a column is its bare name; a schema its own.
    let loose = sn_editor_tab(&app, visual, "warehouse", None, "");
    let column = sn_column(OA_PG, "warehouse", "public", "orders", "id");
    sn_drop(&app, visual, loose, &[orders.clone(), column, schema_node(OA_PG, "warehouse", "public")], sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, loose), "\"public\".\"orders\", \"id\", \"public\"");

    // Another connection's table is never the editor's own, whatever its schema is called.
    let other = object_node(crate::ui_fixture::SQLITE_ID, "warehouse", "public", "orders");
    let third = sn_editor_tab(&app, visual, "warehouse", Some("public"), "");
    sn_drop(&app, visual, third, &[other], sn_nowhere());
    assert!(sn_tab_sql(&app, visual, third).contains("\"public\".\"orders\""), "{}", sn_tab_sql(&app, visual, third));
}

#[gpui::test]
fn mysql_and_clickhouse_editors_quote_with_backticks_and_qualify_by_database(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    sn_set_kind(&app, visual, OA_PG, ConnectionKind::Mysql);
    let in_shop = object_node(OA_PG, "shop", "shop", "or`ders");
    let in_other = object_node(OA_PG, "other", "other", "t");
    // The tab's database is the namespace: no schema.
    let tab = sn_editor_tab(&app, visual, "shop", None, "");
    sn_drop(&app, visual, tab, &[in_shop.clone(), in_other.clone()], sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, tab), "`or``ders`, `other`.`t`");
    let column = sn_column(OA_PG, "shop", "shop", "or`ders", "we`ird");
    let tab = sn_editor_tab(&app, visual, "other", None, "");
    sn_drop(&app, visual, tab, &[in_shop, column], sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, tab), "`shop`.`or``ders`, `we``ird`");
    // A name ClickHouse could read two ways is refused with a notice, and nothing is inserted.
    sn_set_kind(&app, visual, OA_PG, ConnectionKind::Clickhouse);
    let tab = sn_editor_tab(&app, visual, "shop", None, "x");
    sn_drop(&app, visual, tab, &[object_node(OA_PG, "shop", "shop", "back\\slash")], sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, tab), "x");
    assert!(app.read_with(visual, |this, _| this.sidebar.notice.clone()).is_some_and(|notice| notice.contains("backslash")));
}

#[gpui::test]
fn a_drop_lands_where_the_pointer_is_not_at_the_caret_and_a_collection_goes_into_a_mongo_editor(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let tab = sn_editor_tab(&app, visual, "warehouse", Some("public"), "SELECT  FROM t");
    let editor = sn_editor(&app, visual, tab);
    // The caret is at the end; the pointer is over the gap between the two spaces (character 7).
    visual.update(|window, cx| editor.update(cx, |editor, cx| editor.set_cursor_position(gpui_component::input::Position::new(0, 14), window, cx)));
    settle(visual);
    let at = visual.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            let bounds = editor.text_bounds().expect("the editor is laid out");
            let glyph = gpui::EntityInputHandler::bounds_for_range(editor, 7..8, bounds, window, cx).expect("the glyph is drawn");
            gpui::point(glyph.origin.x + gpui::px(1.0), glyph.center().y)
        })
    });
    sn_drop(&app, visual, tab, &[oa_pg_object("orders")], at);
    assert_eq!(sn_tab_sql(&app, visual, tab), "SELECT \"orders\" FROM t");

    // MongoDB script editor: the collection of the editor's own database is its bare name, another's is database.collection.
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.workspace.open_or_activate_collection_tab(crate::ui_fixture::MONGO_ID, "fixture_db_0", "fixture_collection_01");
        this.locate_active_sidebar_tab(cx);
    });
    visual.run_until_parked();
    let collection = app.read_with(visual, |this, _| this.navigator.selected().cloned().expect("the located collection"));
    let script = app.update(visual, |this, _| {
        this.open_mongo_script_tab(crate::ui_fixture::MONGO_ID, "fixture_db_0", "");
        this.workspace.active_tab().map(|tab| tab.id).expect("the script tab")
    });
    settle(visual);
    app.update(visual, |this, _| this.workspace.tab_mut(script).expect("tab").sql.clear());
    sn_drop(&app, visual, script, std::slice::from_ref(&collection), sn_nowhere());
    assert!(sn_tab_sql(&app, visual, script).ends_with("fixture_collection_01"), "{}", sn_tab_sql(&app, visual, script));
    let other = app.update(visual, |this, _| {
        this.open_mongo_script_tab(crate::ui_fixture::MONGO_ID, "elsewhere", "");
        this.workspace.active_tab().map(|tab| tab.id).expect("the script tab")
    });
    settle(visual);
    app.update(visual, |this, _| this.workspace.tab_mut(other).expect("tab").sql.clear());
    sn_drop(&app, visual, other, &[collection], sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, other), "fixture_db_0.fixture_collection_01");
}

#[gpui::test]
fn dragging_a_table_row_onto_the_editor_with_the_mouse_inserts_its_name(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let tab = sn_editor_tab(&app, visual, "warehouse", Some("public"), "SELECT * FROM ");
    let orders = oa_pg_object("orders");
    let index = app.read_with(visual, |this, _| this.navigator.visible_index(&orders)).expect("a visible row");
    let row = visual.debug_bounds(Box::leak(format!("sidebar-row-{index}").into_boxed_str())).expect("the row is measured");
    let area = visual.debug_bounds("query-editor-area").expect("the editor area is measured");
    let target = gpui::point(area.center().x, area.origin.y + gpui::px(80.0));
    visual.simulate_mouse_down(row.center(), gpui::MouseButton::Left, Default::default());
    for step in 1..=6 {
        let t = step as f32 / 6.0;
        let position = gpui::point(row.center().x + (target.x - row.center().x) * t, row.center().y + (target.y - row.center().y) * t);
        visual.simulate_mouse_move(position, gpui::MouseButton::Left, Default::default());
    }
    visual.simulate_mouse_up(target, gpui::MouseButton::Left, Default::default());
    settle(visual);
    assert!(sn_tab_sql(&app, visual, tab).contains("\"orders\""), "{:?}", sn_tab_sql(&app, visual, tab));
    // The row stayed where it was: a drag onto the editor moves and opens nothing.
    assert!(app.read_with(visual, |this, _| this.navigator.visible_index(&orders).is_some()));
}

#[gpui::test]
fn dropping_objects_on_other_targets_does_nothing_new(cx: &mut TestAppContext) {
    let (app, visual, ..) = oa_pg(cx);
    let tab = sn_editor_tab(&app, visual, "warehouse", Some("public"), "SELECT 1");
    // A Redis console takes commands, not names: no drop target there (the drop handler is not mounted).
    let before = app.read_with(visual, |this, _| (this.workspace.tabs().len(), this.sidebar.notice.clone()));
    // A row that has no name to insert leaves the editor alone.
    sn_drop(&app, visual, tab, &[connection_node(OA_PG), database_node(OA_PG, "warehouse")], sn_nowhere());
    assert_eq!(sn_tab_sql(&app, visual, tab), "SELECT 1");
    assert_eq!(app.read_with(visual, |this, _| (this.workspace.tabs().len(), this.sidebar.notice.clone())), before);
}

#[gpui::test]
fn f4_opens_the_selected_table_and_f2_asks_for_its_new_name_before_anything_is_reviewed(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    sn_focus_sidebar_on(&app, visual, &orders);
    visual.simulate_keystrokes("f4");
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.workspace.active_tab().is_some_and(|tab| tab.collection == "orders" && tab.query_mode.is_table_data())), "F4 opens the table, as Open does");

    sn_focus_sidebar_on(&app, visual, &orders);
    visual.simulate_keystrokes("f2");
    settle(visual);
    assert!(visual.debug_bounds("object-name-input").is_some(), "F2 opens the rename dialog of the table");
    assert!(app.read_with(visual, |this, _| this.object_action.is_none()), "nothing is reviewed before a name is typed");
    assert!(ran.lock().unwrap().is_empty());
}

#[gpui::test]
fn delete_on_a_table_opens_the_review_dialog_and_never_drops_without_it(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    for chord in ["delete", "backspace"] {
        sn_focus_sidebar_on(&app, visual, &orders);
        visual.simulate_keystrokes(chord);
        settle(visual);
        assert!(visual.debug_bounds("changes-subject").is_some(), "{chord}: the review dialog is open");
        assert_eq!(oa_script(visual), "DROP TABLE \"public\".\"orders\";");
        assert!(ran.lock().unwrap().is_empty(), "{chord}: nothing ran");
        visual.simulate_keystrokes("escape");
        settle(visual);
        assert!(ran.lock().unwrap().is_empty());
    }
    if cfg!(target_os = "macos") {
        sn_focus_sidebar_on(&app, visual, &orders);
        visual.simulate_keystrokes("cmd-backspace");
        settle(visual);
        assert!(visual.debug_bounds("changes-subject").is_some(), "Cmd+Backspace asks as well");
    }
    assert!(ran.lock().unwrap().is_empty(), "no statement ran without the dialog's Apply");
}

#[gpui::test]
fn f2_and_delete_on_a_group_and_a_connection_open_their_dialogs_and_delete_nothing(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-sidebar-keys-{}.json", uuid::Uuid::new_v4()));
    visual.update(|_, cx| cx.set_reduce_motion(true));
    let group = app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.create_sidebar_group("Keys", None, cx).expect("a group");
        this.navigator.preferences().groups.iter().find(|group| group.name == "Keys").map(|group| NavigatorNodeId::Group { id: group.id.clone() }).expect("the group row")
    });
    settle(visual);
    let connections_before = app.read_with(visual, |this, _| this.saved_connections.len());

    sn_focus_sidebar_on(&app, visual, &group);
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert!(visual.debug_bounds("dialog-layer").is_some(), "Delete on a group asks first");
    assert_eq!(app.read_with(visual, |this, _| this.navigator.preferences().groups.len()), 1, "the group is still there");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("dialog-layer").is_none());
    assert_eq!(app.read_with(visual, |this, _| this.navigator.preferences().groups.len()), 1);

    sn_focus_sidebar_on(&app, visual, &group);
    visual.simulate_keystrokes("f2");
    settle(visual);
    assert!(visual.debug_bounds("dialog-layer").is_some(), "F2 on a group opens the rename dialog");
    visual.simulate_keystrokes("escape");
    settle(visual);

    // The connection's own confirmation is not offered inside a UI scenario: leave it, as the driver tests do.
    app.update(visual, |this, _| this.scenario_ui.service = None);
    let connection = connection_node(crate::ui_fixture::POSTGRES_ID);
    sn_focus_sidebar_on(&app, visual, &connection);
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert!(visual.debug_bounds("dialog-layer").is_some(), "Delete on a connection asks first");
    assert_eq!(app.read_with(visual, |this, _| this.saved_connections.len()), connections_before, "nothing was deleted");
    visual.simulate_keystrokes("escape");
    settle(visual);

    sn_focus_sidebar_on(&app, visual, &connection);
    visual.simulate_keystrokes("f2");
    settle(visual);
    assert!(visual.debug_bounds("dialog-layer").is_some(), "F2 on a connection opens its edit dialog");
    visual.simulate_keystrokes("escape");
    settle(visual);
    std::fs::remove_file(path).unwrap_or(());
}

#[gpui::test]
fn typing_in_the_search_box_is_not_taken_for_the_delete_and_rename_keys(cx: &mut TestAppContext) {
    let (app, visual, ran, _) = oa_pg(cx);
    let orders = oa_pg_object("orders");
    sn_focus_sidebar_on(&app, visual, &orders);
    let search = app.read_with(visual, |this, _| this.sidebar.search_input.clone()).expect("the search input is built");
    visual.update(|window, cx| {
        search.update(cx, |state, cx| {
            state.set_value("abc", window, cx);
            state.focus(window, cx);
        })
    });
    settle(visual);
    visual.simulate_keystrokes("backspace");
    settle(visual);
    assert!(visual.debug_bounds("changes-subject").is_none(), "Backspace in the search box does not drop a table");
    assert!(ran.lock().unwrap().is_empty());
    assert_eq!(search.read_with(visual, |state, _| state.value().to_string()), "ab", "the field took its own Backspace");
    visual.simulate_keystrokes("f2");
    settle(visual);
    assert!(visual.debug_bounds("object-name-input").is_none(), "F2 in the search box renames nothing");
}

#[gpui::test]
fn the_new_sidebar_shortcuts_are_registered_for_the_sidebar_and_do_not_clash(cx: &mut TestAppContext) {
    let _ = cx;
    for (id, chord) in [
        ("sidebar.copy_qualified_name", "secondary-shift-c"),
        ("sidebar.rename_selection", "f2"),
        ("sidebar.open_selection", "f4"),
        ("sidebar.delete_selection", "delete"),
    ] {
        let def = shortcuts::find(id).unwrap_or_else(|| panic!("{id} is registered"));
        assert_eq!(def.contexts, [Some("sidebar")], "{id}");
        assert!(def.default_chords().iter().any(|candidate| candidate == chord), "{id}: {:?}", def.default_chords());
        assert!(shortcuts::conflict(&Default::default(), def, chord).is_none(), "{id} {chord}");
    }
    let delete = shortcuts::find("sidebar.delete_selection").unwrap().default_chords();
    assert!(delete.iter().any(|chord| chord == if cfg!(target_os = "macos") { "secondary-backspace" } else { "backspace" }));
    // The Cmd+C of the sidebar is still the existing copy.
    assert_eq!(shortcuts::find("sidebar.copy_connection_name").unwrap().default_chords(), ["secondary-c"]);
}
