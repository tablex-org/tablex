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
    }
    .table_folder(crate::navigator::TableFolder::Columns)
    .expect("a table");
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

/// A New Query tab scoped to the fixture schema (warehouse.public) whose tables the sidebar already loaded.
fn new_query_in_fixture_schema(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, gpui::Entity<EditorState>) {
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
    (app, visual, editor)
}

fn cursor_of(editor: &gpui::Entity<EditorState>, visual: &gpui::VisualTestContext) -> usize {
    visual.cx.read(|cx| editor.read(cx).cursor())
}

#[gpui::test]
fn completion_ctrl_space_opens_the_menu_after_an_empty_word_where_typing_shows_none(
    cx: &mut TestAppContext,
) {
    let (_, visual, editor) = new_query_in_fixture_schema(cx);
    // Typing a space after the table name asks the engine too, and it has nothing to offer.
    type_in_editor(visual, "select * from orders ");
    assert!(!menu_state(&editor, visual).0, "no menu while typing");

    visual.simulate_keystrokes("ctrl-space");
    settle(visual);
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "Ctrl+Space opens the menu");
    assert!(labels.iter().any(|label| label == "WHERE"), "{labels:?}");
    assert_eq!(editor_value(&editor, visual), "select * from orders ", "nothing was typed");

    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!menu_state(&editor, visual).0);
}

#[gpui::test]
fn completion_ctrl_space_inserts_a_single_proposal_without_the_menu_but_typing_never_does(
    cx: &mut TestAppContext,
) {
    let (_, visual, editor) = new_query_in_fixture_schema(cx);
    // Typing shows the menu even when one table matches: nothing is inserted behind the user's back.
    type_in_editor(visual, "select * from ord");
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open);
    assert_eq!(labels, ["orders"]);
    assert_eq!(editor_value(&editor, visual), "select * from ord");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!menu_state(&editor, visual).0);

    // By hand: with one proposal the word is completed and no menu opens.
    visual.simulate_keystrokes("ctrl-space");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select * from orders");
    assert!(!menu_state(&editor, visual).0, "inserted, not listed");
    assert_eq!(cursor_of(&editor, visual), "select * from orders".len());

    // Several proposals are listed, nothing is inserted.
    visual.simulate_input(" where s");
    settle(visual);
    visual.simulate_keystrokes("escape");
    settle(visual);
    let before = editor_value(&editor, visual);
    visual.simulate_keystrokes("ctrl-space");
    settle(visual);
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open && labels.len() > 1, "{labels:?}");
    assert_eq!(editor_value(&editor, visual), before);
}

#[gpui::test]
fn completion_enter_on_a_function_with_arguments_leaves_the_caret_between_the_parentheses(
    cx: &mut TestAppContext,
) {
    let (_, visual, editor) = new_query_in_fixture_schema(cx);
    type_in_editor(visual, "select su");
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "{labels:?}");
    assert_eq!(labels[0], "SUM");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select SUM()");
    assert_eq!(cursor_of(&editor, visual), "select sum(".len(), "the caret is inside the parentheses");

    // The arguments go where the caret is.
    visual.simulate_input("total");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select SUM(total)");

    // A function without arguments keeps the caret after `)`.
    visual.simulate_keystrokes("end");
    visual.simulate_input(", now");
    settle(visual);
    assert_eq!(menu_state(&editor, visual).1[0], "NOW");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select SUM(total), NOW()");
    assert_eq!(cursor_of(&editor, visual), "select SUM(total), NOW()".len());
}

#[gpui::test]
fn completion_ctrl_space_in_the_mongo_script_editor_lists_statements_on_an_empty_script(
    cx: &mut TestAppContext,
) {
    let (app, visual) = script_app(cx);
    // The script fixture app does not install the keymap; Ctrl+Space comes from the shortcut registry.
    visual.cx.update(actions::bind_keys);
    let (_, editor) = query_editor(&app, visual);
    script_type(visual, "x");
    visual.simulate_keystrokes("backspace");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "");
    assert!(!menu_state(&editor, visual).0, "nothing is listed while typing");
    visual.simulate_keystrokes("ctrl-space");
    settle(visual);
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "{labels:?}");
    assert_eq!(labels[0], "db.collection.find");
}

/// The key the completion caches the foreign keys of fixture table `object` by.
fn foreign_key_cache_key(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
    object: &str,
) -> crate::properties_model::PropertiesKey {
    app.read_with(visual, |this, _| crate::properties_model::PropertiesKey {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
        object: object.into(),
        session: this
            .driver_runtime
            .connection_ids
            .get(crate::ui_fixture::POSTGRES_ID)
            .copied()
            .unwrap_or(0),
    })
}

#[gpui::test]
fn completion_asks_for_the_foreign_keys_of_the_tables_in_scope_only_once_a_join_is_typed(
    cx: &mut TestAppContext,
) {
    use completion::ForeignKeys;
    let (app, visual, _) = new_query_in_fixture_schema(cx);
    let key = foreign_key_cache_key(&app, visual, "orders");
    let cached = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            this.completion_foreign_keys.0.get(&key).map(|state| match state {
                ForeignKeys::Loading => "loading".to_owned(),
                ForeignKeys::Failed => "failed".to_owned(),
                ForeignKeys::Ready(keys) => format!("ready {}", keys.len()),
            })
        })
    };
    type_in_editor(visual, "select * from orders o where ");
    assert_eq!(cached(&app, visual), None, "plain queries load nothing");
    type_in_editor(visual, "select * from orders o join ");
    // The fixture describes `orders` with two foreign keys of its own and one pointing at it.
    assert_eq!(cached(&app, visual).as_deref(), Some("ready 3"));
}

#[gpui::test]
fn completion_join_lists_foreign_key_partners_first_then_the_condition_and_enter_inserts_them(
    cx: &mut TestAppContext,
) {
    use completion::{ForeignKeys, StoredForeignKey};
    let (app, visual, editor) = new_query_in_fixture_schema(cx);
    // `orders.customer_id` references fixture_table_03 (the fixture's own keys point at tables the
    // schema does not list).
    let key = foreign_key_cache_key(&app, visual, "orders");
    app.update(visual, |this, _| {
        this.completion_foreign_keys.0.insert(
            key,
            ForeignKeys::Ready(vec![StoredForeignKey {
                name: "orders_customer_fk".into(),
                table: "orders".into(),
                columns: vec!["customer_id".into()],
                referenced: "fixture_table_03".into(),
                referenced_columns: vec!["id".into()],
            }]),
        );
    });
    type_in_editor(visual, "select * from orders o join ");
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open, "{labels:?}");
    assert_eq!(labels[0], "fixture_table_03", "{labels:?}");
    assert_eq!(details[0].as_deref(), Some("table, foreign key"));
    assert_eq!(details[1].as_deref(), Some("table"));
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select * from orders o join fixture_table_03");

    visual.simulate_input(" f on ");
    settle(visual);
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open, "{labels:?}");
    assert_eq!(labels[0], "o.customer_id = f.id", "{labels:?}");
    assert_eq!(details[0].as_deref(), Some("foreign key orders_customer_fk"));
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(
        editor_value(&editor, visual),
        "select * from orders o join fixture_table_03 f on o.customer_id = f.id"
    );
}

#[gpui::test]
fn completion_ctrl_space_after_a_star_expands_it_to_the_columns_of_the_table(
    cx: &mut TestAppContext,
) {
    let (_, visual, editor) = new_query_in_fixture_schema(cx);
    type_in_editor(visual, "select * from orders");
    visual.simulate_keystrokes("escape home");
    visual.simulate_keystrokes("right right right right right right right right");
    settle(visual);
    assert_eq!(cursor_of(&editor, visual), 8, "right after the star");
    // The columns of `orders` are not cached yet: the request asks for them, and answers again
    // when they arrive, without another Ctrl+Space.
    visual.simulate_keystrokes("ctrl-space");
    settle(visual);
    assert_eq!(
        editor_value(&editor, visual),
        "select id, name, status, active, created, score, big_number, payload from orders",
        "the single proposal replaced the star in place"
    );
}

/// A query tab of a connected session whose sidebar tree was never expanded: nothing is cached
/// (databases, schemas, tables, columns), the way a fresh connection looks.
fn cold_query_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, gpui::Entity<EditorState>) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionTree,
        fixture: FixtureKind::Sample,
        delay: Duration::ZERO,
        timeout_delay: Duration::ZERO,
    };
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    app.update(visual, |this, cx| {
        let id = crate::ui_fixture::POSTGRES_ID;
        let attempt = this.sessions.begin_connect(id).expect("not connecting yet");
        assert!(this.sessions.finish_connect(id, attempt, Ok(())));
        this.sync_connected_ids();
        let schema = NavigatorNodeId::Schema {
            connection: id.into(),
            database: "warehouse".into(),
            schema: "public".into(),
        };
        assert!(this.open_new_query_for_node(&schema, cx));
        assert!(
            this.navigator.loaded_children(&NavigatorNodeId::Connection { connection: id.into() }).is_none(),
            "nothing was listed"
        );
    });
    settle(visual);
    let (_, editor) = query_editor(&app, visual);
    (app, visual, editor)
}

/// How many sidebar rows there are and whether any is expanded.
fn sidebar_shape(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> (usize, bool) {
    app.read_with(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        (rows.len(), rows.iter().any(|row| row.expanded))
    })
}

/// The "suggestions while typing" setting off: only a request by hand asks.
fn suggest_by_hand_only(visual: &mut gpui::VisualTestContext) {
    visual.cx.update(|cx| cx.set_global(completion::CompletionPrefs { auto_trigger: false }));
}

#[gpui::test]
fn completion_offers_the_tables_of_a_never_expanded_tree_once_they_are_loaded_in_the_background(
    cx: &mut TestAppContext,
) {
    let (app, visual, editor) = cold_query_app(cx);
    let before = sidebar_shape(&app, visual);
    assert!(!before.1);
    type_in_editor(visual, "select * from ord");
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "the menu opens without another keystroke");
    assert_eq!(labels.first().map(String::as_str), Some("orders"), "{labels:?}");
    assert_eq!(editor_value(&editor, visual), "select * from ord", "typing never inserts");
    assert_eq!(sidebar_shape(&app, visual), before, "the sidebar stays as it was");
    app.read_with(visual, |this, _| {
        assert!(this.completion_loads.is_idle(), "nothing is waited for any more");
    });
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select * from orders");
}

#[gpui::test]
fn completion_by_hand_after_join_on_a_cold_cache_lists_the_tables_without_a_second_request(
    cx: &mut TestAppContext,
) {
    let (app, visual, editor) = cold_query_app(cx);
    suggest_by_hand_only(visual);
    type_in_editor(visual, "select * from orders o join ");
    assert!(!menu_state(&editor, visual).0, "typing asks for nothing");
    let before = sidebar_shape(&app, visual);
    visual.simulate_keystrokes("ctrl-space");
    settle(visual);
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open, "the tables arrived and the menu shows them");
    assert!(labels.iter().any(|label| label == "orders"), "{labels:?}");
    assert!(details.iter().flatten().any(|detail| detail.starts_with("table")), "{details:?}");
    assert_eq!(editor_value(&editor, visual), "select * from orders o join ");
    assert_eq!(sidebar_shape(&app, visual), before);
}

#[gpui::test]
fn completion_by_hand_expands_a_star_on_a_cold_cache_once_tables_and_columns_arrive(
    cx: &mut TestAppContext,
) {
    let (app, visual, editor) = cold_query_app(cx);
    suggest_by_hand_only(visual);
    type_in_editor(visual, "select * from orders");
    visual.simulate_keystrokes("escape home");
    visual.simulate_keystrokes("right right right right right right right right");
    settle(visual);
    assert_eq!(cursor_of(&editor, visual), 8, "right after the star");
    let before = sidebar_shape(&app, visual);
    // The table lists, then the columns: two background loads, one request by hand.
    visual.simulate_keystrokes("ctrl-space");
    settle(visual);
    assert_eq!(
        editor_value(&editor, visual),
        "select id, name, status, active, created, score, big_number, payload from orders"
    );
    assert!(!menu_state(&editor, visual).0, "a single proposal by hand is inserted, not listed");
    assert_eq!(sidebar_shape(&app, visual), before);
}

#[gpui::test]
fn completion_does_not_answer_again_when_the_caret_moved_before_the_load_landed(
    cx: &mut TestAppContext,
) {
    let (app, visual, editor) = cold_query_app(cx);
    suggest_by_hand_only(visual);
    type_in_editor(visual, "select * from ord");
    // The request is made and the caret moves before anything it started is answered.
    visual.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.show_completions(window, cx);
            editor.set_cursor_position(gpui_component::input::Position::new(0, 3), window, cx);
        });
    });
    settle(visual);
    assert!(!menu_state(&editor, visual).0, "no menu appears at a place the user left");
    assert_eq!(editor_value(&editor, visual), "select * from ord");
    app.read_with(visual, |this, _| {
        assert!(this.completion_loads.is_idle());
        let group = crate::navigator::object_group_id(
            crate::ui_fixture::POSTGRES_ID,
            "warehouse",
            "public",
            crate::navigator::ObjectGroup::Tables,
        );
        assert!(this.navigator.loaded_children(&group).is_some(), "the load itself still landed");
    });
}

#[gpui::test]
fn completion_does_not_answer_again_for_a_load_the_sidebar_started(cx: &mut TestAppContext) {
    let (app, visual, editor) = cold_query_app(cx);
    suggest_by_hand_only(visual);
    type_in_editor(visual, "select * from ord");
    // The user opens the connection in the sidebar: that load is theirs, not the completion's.
    let root = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
    app.update(visual, |this, cx| {
        let request = this.navigator.toggle_expansion(&root).request.expect("databases load");
        this.ensure_connected_then_load(request, cx);
    });
    settle(visual);
    assert!(!menu_state(&editor, visual).0, "nothing asked for it");
    assert_eq!(editor_value(&editor, visual), "select * from ord");
}

#[gpui::test]
fn completion_answers_with_the_tables_that_landed_while_the_request_was_being_answered(
    cx: &mut TestAppContext,
) {
    let (_, visual, editor) = cold_query_app(cx);
    suggest_by_hand_only(visual);
    type_in_editor(visual, "select * from ord");
    // The editor's own request: the provider's task resolves after the (synchronous) fixture loads
    // it started have landed, and its answer is what the editor applies last.
    let task = visual.update(|window, cx| {
        let (provider, rope, offset) = editor.read_with(cx, |editor, _| {
            (editor.lsp().completion_provider.clone().expect("provider"), editor.text().clone(), editor.cursor())
        });
        provider.completions(
            &rope,
            offset,
            lsp_types::CompletionContext {
                trigger_kind: lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some("d".into()),
            },
            window,
            cx,
        )
    });
    settle(visual);
    let lsp_types::CompletionResponse::Array(items) = visual.cx.foreground_executor().block_test(task).unwrap() else {
        panic!("an array");
    };
    let start = "select * from ".len();
    visual.update(|_, cx| editor.update(cx, |editor, cx| editor.present_completion_items(start, "ord", items, cx)));
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open, "the answer the editor applies last carries the tables");
    assert_eq!(labels.first().map(String::as_str), Some("orders"), "{labels:?}");
}
