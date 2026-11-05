// Real-interaction proofs for editor autocomplete: keystrokes go through the
// production editors, the provider reads the live navigator / result caches,
// and Enter / Escape act on the gpui-component completion menu. The menu's
// own state (`completion_menu_state`) is what it renders from.

/// `(open, labels, details)` of an editor's completion menu.
fn menu_state(
    editor: &gpui::Entity<EditorState>,
    visual: &gpui::VisualTestContext,
) -> (bool, Vec<String>, Vec<Option<String>>) {
    visual.cx.read(|cx| {
        let menu = editor.read(cx).completion_menu_state();
        (
            menu.open,
            menu.items.iter().map(|item| item.label.clone()).collect(),
            menu.items.iter().map(|item| item.detail.clone()).collect(),
        )
    })
}

fn query_editor(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> (u64, gpui::Entity<EditorState>) {
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        (tab, this.query_editors.by_tab[&tab].clone())
    })
}

/// Editor text. gpui-component's completion menu leaves the Enter keystroke
/// propagating (`CompletionMenu::handle_action` calls `cx.propagate()`), and the
/// test platform then also types its `\n` key character; macOS routes an
/// unhandled Return to `insertNewline:` instead, which GPUI ignores. The
/// trailing newline is that platform artifact, not part of the completion.
fn editor_value(editor: &gpui::Entity<EditorState>, visual: &gpui::VisualTestContext) -> String {
    visual
        .cx
        .read(|cx| editor.read(cx).value().trim_end_matches('\n').to_string())
}

#[gpui::test]
fn completion_sql_editor_menu_lists_schema_tables_then_enter_inserts_and_escape_closes(
    cx: &mut TestAppContext,
) {
    let (app, visual, _) = sql_table_app(cx);
    // A New Query tab scoped to the fixture schema whose tables the sidebar
    // already loaded (warehouse.public: orders, ...).
    app.update(visual, |this, cx| {
        let schema = NavigatorNodeId::Schema {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
            database: "warehouse".into(),
            schema: "public".into(),
        };
        assert!(this.open_new_query_for_node(&schema, cx));
    });
    settle(visual);
    let (tab, editor) = query_editor(&app, visual);

    type_in_editor(visual, "select * from ord");
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open, "the menu opens while typing a table name");
    assert_eq!(
        labels.first().map(String::as_str),
        Some("orders"),
        "{labels:?}"
    );
    assert_eq!(details[0].as_deref(), Some("table"));
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select * from orders");
    assert!(!menu_state(&editor, visual).0, "accepting closes the menu");

    // Statement start: snippet first, then keywords and PostgreSQL functions.
    type_in_editor(visual, "s");
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open);
    assert_eq!(labels[0], "select *");
    assert_eq!(
        details[0].as_deref(),
        Some("SELECT * FROM table LIMIT 100;")
    );
    for function in [
        "SUM",
        "SIGN",
        "SQRT",
        "STDDEV",
        "STRPOS",
        "SUBSTR",
        "SUBSTRING",
        "SPLIT_PART",
        "STRING_AGG",
    ] {
        assert!(
            labels.iter().any(|label| label == function),
            "{function} in {labels:?}"
        );
    }
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!menu_state(&editor, visual).0, "Escape closes the menu");
    assert_eq!(
        editor_value(&editor, visual),
        "s",
        "Escape does not edit the text"
    );
    let _ = tab;
}

#[gpui::test]
fn completion_mongo_script_menu_lists_shell_snippets_collections_and_methods(
    cx: &mut TestAppContext,
) {
    let (app, visual) = script_app(cx);
    let (_, editor) = query_editor(&app, visual);

    script_type(visual, "d");
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open);
    assert_eq!(labels[0], "db.collection.find");
    assert_eq!(details[0].as_deref(), Some("Find documents"));
    for label in [
        "db.collection.aggregate",
        "db.getCollection",
        "db.version",
        "db.stats",
        "db.serverStatus",
        "distinct",
        "deleteOne",
        "deleteMany",
        "dataSize",
    ] {
        assert!(
            labels.iter().any(|candidate| candidate == label),
            "{label} in {labels:?}"
        );
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
    let inserted = editor_value(&editor, visual);
    assert!(
        inserted.starts_with("db.") && inserted.ends_with(".find({})"),
        "{inserted}"
    );

    script_type(visual, "db.");
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open);
    assert!(
        labels.iter().any(|label| label == "fixture_collection_00"),
        "{labels:?}"
    );
    assert!(labels.iter().any(|label| label == "getCollection"));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!menu_state(&editor, visual).0);
    assert_eq!(editor_value(&editor, visual), "db.");

    script_type(visual, "db.fixture_collection_00.fi");
    let (_, labels, _) = menu_state(&editor, visual);
    assert_eq!(labels[0], "find", "{labels:?}");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(
        editor_value(&editor, visual),
        "db.fixture_collection_00.find({})"
    );
}

#[gpui::test]
fn completion_aggregate_pipeline_editor_menu_offers_stage_operators(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    agg_type_pipeline(visual, "[{ $m");
    let editor = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.aggregate_ui
            .tab(tab)
            .and_then(|ui| ui.text_editor.clone())
            .expect("text editor")
    });
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open, "{labels:?}");
    assert_eq!(labels[0], "$match");
    assert_eq!(details[0].as_deref(), Some("Filters documents"));
    assert!(labels.iter().any(|label| label == "$merge"));
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(
        editor_value(&editor, visual).starts_with("[{ $match: "),
        "{}",
        editor_value(&editor, visual)
    );
}

#[gpui::test]
fn completion_mongo_filter_editor_menu_completes_result_fields_as_quoted_json_keys(
    cx: &mut TestAppContext,
) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    let tab_id = app.update(visual, |this, cx| {
        let tab_id = this.workspace.open_query_tab("Mongo");
        this.workspace.active_tab_mut().unwrap().query_mode = Some(QueryMode::MongoFind);
        let runtime = this.workspace.runtime_mut(tab_id).unwrap();
        runtime
            .mongo
            .observe_result_columns(["status".to_string(), "total".to_string()]);
        runtime.mongo.set_find_expanded(true);
        cx.notify();
        tab_id
    });
    visual.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
    settle(visual);
    let editor = app.read_with(visual, |this, _| {
        this.mongo_ui
            .expanded_find_editors
            .get(&tab_id)
            .cloned()
            .expect("expanded Find editor")
    });
    app.update_in(visual, |_, window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value("{}", window, cx);
            state.focus(window, cx);
        });
    });
    settle(visual);
    visual.simulate_keystrokes("right");

    visual.simulate_input("st");
    settle(visual);
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "{labels:?} text={:?}", editor_value(&editor, visual));
    assert_eq!(labels[0], "status");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(
        editor_value(&editor, visual),
        "{\"status\": }",
        "strict JSON keys are quoted"
    );
}

#[gpui::test]
fn completion_warms_uncached_columns_of_tables_in_scope_then_completes_them(
    cx: &mut TestAppContext,
) {
    let (app, visual, _) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        let schema = NavigatorNodeId::Schema {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
            database: "warehouse".into(),
            schema: "public".into(),
        };
        assert!(this.open_new_query_for_node(&schema, cx));
    });
    settle(visual);
    let (_, editor) = query_editor(&app, visual);
    let orders = NavigatorNodeId::Object {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
        object: "orders".into(),
    };
    assert!(app.read_with(visual, |this, _| this
        .navigator
        .loaded_children(&orders)
        .is_none()));

    // Nothing is cached for `orders` yet. The first keystrokes that need its
    // columns request them through the navigator's regular load path (without
    // expanding the sidebar), so by the time `o.` is typed they complete.
    type_in_editor(visual, "select * from orders o where o.");
    let loaded = app.read_with(visual, |this, _| {
        this.navigator.loaded_children(&orders).map(<[_]>::len)
    });
    assert_eq!(loaded, Some(8), "columns were loaded in the background");
    assert!(
        app.read_with(visual, |this, _| {
            !this
                .navigator
                .visible_rows()
                .iter()
                .any(|row| matches!(&row.id, NavigatorNodeId::Column { .. }))
        }),
        "the sidebar tree was not expanded by the background load"
    );
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open);
    assert_eq!(
        labels,
        [
            "id",
            "name",
            "status",
            "active",
            "created",
            "score",
            "big_number",
            "payload"
        ]
    );
    assert_eq!(details[0].as_deref(), Some("column"));

    visual.simulate_input("st");
    settle(visual);
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "{labels:?}");
    assert_eq!(labels[0], "status");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(
        editor_value(&editor, visual),
        "select * from orders o where o.status"
    );
}

#[gpui::test]
fn completion_sql_table_where_editor_completes_columns_and_enter_accepts_instead_of_running(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = sql_table_app(cx);
    let column = |name: &str, data_type: &str| crate::catalog::CatalogColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable: true,
        primary_key: false,
        is_generated: None,
        default_expression: None,
        auto_increment: None,
        comment: None,
        attributes: None,
    };
    app.update(visual, |this, _| {
        this.workspace
            .runtime_mut(tab_id)
            .unwrap()
            .structure_columns = vec![
            column("id", "integer"),
            column("status", "text"),
            column("total", "numeric"),
        ];
    });
    app.update_in(visual, |this, window, cx| {
        this.set_sql_table_expanded(
            tab_id,
            results::sql_table::SqlField::Where,
            true,
            window,
            cx,
        );
    });
    settle(visual);
    let area = visual
        .debug_bounds("sql-where-editor-area")
        .expect("expanded WHERE editor");
    visual.simulate_click(area.center(), Default::default());
    settle(visual);
    visual.simulate_input("sta");
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap();
        assert_eq!(
            tab.table_where.trim_end_matches('\n'),
            "status",
            "Enter accepted the column"
        );
        assert_eq!(tab.applied_where, "", "and did not run the query");
    });
}

#[gpui::test]
fn sql_table_where_editor_enter_runs_the_query_when_no_menu_is_open(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update_in(visual, |this, window, cx| {
        this.set_sql_table_expanded(
            tab_id,
            results::sql_table::SqlField::Where,
            true,
            window,
            cx,
        );
    });
    settle(visual);
    let area = visual
        .debug_bounds("sql-where-editor-area")
        .expect("expanded WHERE editor");
    visual.simulate_click(area.center(), Default::default());
    settle(visual);
    visual.simulate_input("1");
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap();
        assert_eq!(tab.table_where, "1", "no newline typed");
        assert_eq!(tab.applied_where, "1", "Enter ran the query");
    });
}

#[gpui::test]
fn sql_table_where_editor_shift_enter_types_a_newline_instead_of_running(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update_in(visual, |this, window, cx| {
        this.set_sql_table_expanded(
            tab_id,
            results::sql_table::SqlField::Where,
            true,
            window,
            cx,
        );
    });
    settle(visual);
    let area = visual
        .debug_bounds("sql-where-editor-area")
        .expect("expanded WHERE editor");
    visual.simulate_click(area.center(), Default::default());
    settle(visual);
    visual.simulate_input("1");
    settle(visual);
    visual.simulate_keystrokes("shift-enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap();
        assert_eq!(tab.table_where, "1\n");
        assert_eq!(tab.applied_where, "", "Shift+Enter did not run the query");
    });
}
