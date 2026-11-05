// Properties sub-tabs backed by `TableProperties` (Keys, Foreign Keys,
// Indexes, References, Triggers, DDL) served by the fixture engine.

use crate::columns_panel::PropertiesTab;
use crate::properties_model::LoadStatus;

fn properties_open(visual: &mut gpui::VisualTestContext) {
    let button = visual.debug_bounds("properties-rail-button").expect("Properties rail button");
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
}

fn properties_snapshot(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> (PropertiesTab, LoadStatus, u64, Vec<PropertiesTab>) {
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let panel = &this.workspace.runtime(tab.id).unwrap().columns_panel;
        (
            panel.properties_tab,
            panel.props.status.clone(),
            panel.props.fetches,
            panel.props.visible_tabs().collect(),
        )
    })
}

fn properties_click_tab(visual: &mut gpui::VisualTestContext, tab: PropertiesTab) {
    let bounds = visual
        .debug_bounds(tab.selector())
        .unwrap_or_else(|| panic!("{tab:?} sub-tab is shown"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

/// (headers, row cell texts, filtered count, title) of a list sub-tab.
fn properties_table(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab: PropertiesTab,
) -> (Vec<&'static str>, Vec<Vec<String>>, usize, String) {
    app.read_with(visual, |this, _| {
        let id = this.workspace.active_tab().unwrap().id;
        let table = &this.workspace.runtime(id).unwrap().columns_panel.props.tables[&tab];
        (
            table.columns.iter().map(|column| column.header).collect(),
            table
                .rows
                .iter()
                .map(|row| row.cells.iter().map(|cell| cell.to_string()).collect())
                .collect(),
            table.filtered.len(),
            table.title(),
        )
    })
}

fn properties_rendered_rows(visual: &mut gpui::VisualTestContext) -> usize {
    let mut count = 0;
    loop {
        // `debug_bounds` needs a `'static` key; tests may leak a few bytes.
        let selector: &'static str = Box::leak(format!("properties-row-{count}").into_boxed_str());
        if visual.debug_bounds(selector).is_none() {
            return count;
        }
        count += 1;
    }
}

#[gpui::test]
fn properties_tabs_follow_what_the_driver_reports_and_switch(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_table_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    properties_open(visual);

    // PostgreSQL reports every list: all seven sub-tabs, Columns first.
    let (active, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!(active, PropertiesTab::Columns);
    assert_eq!(status, LoadStatus::Loaded);
    assert_eq!(fetches, 1);
    assert_eq!(visible, PropertiesTab::ALL);
    let mut previous_right = None;
    for tab in PropertiesTab::ALL {
        let bounds = visual.debug_bounds(tab.selector()).expect("tab in the strip");
        if let Some(right) = previous_right {
            assert!(bounds.left() >= right, "{tab:?} is right of the previous tab");
        }
        previous_right = Some(bounds.right());
    }
    assert!(visual.debug_bounds("properties-loading").is_none());
    assert!(visual.debug_bounds("properties-error").is_none());
    assert!(visual.debug_bounds("columns-view-row").is_some(), "Columns still renders");

    // Every list renders its rows and columns from the fixture.
    let expected: [(PropertiesTab, &[&str], usize, &str); 5] = [
        (PropertiesTab::Keys, &["Name", "Type", "Columns", "Definition"], 3, "Keys (3)"),
        (
            PropertiesTab::ForeignKeys,
            &["Name", "Columns", "References", "On Update", "On Delete"],
            2,
            "Foreign Keys (2)",
        ),
        (
            PropertiesTab::Indexes,
            &["Name", "Columns", "Unique", "Primary", "Method", "Definition"],
            3,
            "Indexes (3)",
        ),
        (
            PropertiesTab::References,
            &["Name", "From Table", "Columns", "To Columns", "On Delete"],
            1,
            "References (1)",
        ),
        (
            PropertiesTab::Triggers,
            &["Name", "Timing", "Events", "Enabled", "Definition"],
            2,
            "Triggers (2)",
        ),
    ];
    for (tab, headers, rows, title) in expected {
        properties_click_tab(visual, tab);
        assert_eq!(properties_snapshot(&app, visual).0, tab);
        let (model_headers, model_rows, filtered, model_title) = properties_table(&app, visual, tab);
        assert_eq!(model_headers, headers, "{tab:?} columns");
        assert_eq!((model_rows.len(), filtered, model_title.as_str()), (rows, rows, title));
        assert_eq!(properties_rendered_rows(visual), rows, "{tab:?} rendered rows");
        assert!(visual.debug_bounds("columns-view-row").is_none(), "Columns table is not shown");
        assert!(visual.debug_bounds("properties-toolbar").is_some());
        assert!(visual.debug_bounds("columns-view-filter").is_some());
        let header = visual.debug_bounds("properties-table-header").expect("header row");
        let row = visual.debug_bounds("properties-row-0").expect("first row");
        assert_eq!(row.size.height, gpui::px(26.0), "26px rows");
        assert_eq!(header.size.height, gpui::px(24.0));
        assert_eq!(row.left(), header.left(), "left aligned, same width as the header");
        assert_eq!(row.size.width, header.size.width);
        let underline = visual.debug_bounds("properties-tab-underline").expect("active underline");
        let strip_tab = visual.debug_bounds(tab.selector()).unwrap();
        assert!(underline.left() >= strip_tab.left() && underline.right() <= strip_tab.right());
    }

    // Cell content, per driver: FK targets read `table(cols)`, triggers'
    // definitions are one-line (full text in the tooltip).
    let (_, keys, _, _) = properties_table(&app, visual, PropertiesTab::Keys);
    assert_eq!(keys[0], ["orders_pkey", "PK", "id", "PRIMARY KEY (id)"]);
    assert_eq!(keys[2][1], "Check");
    let (_, fks, _, _) = properties_table(&app, visual, PropertiesTab::ForeignKeys);
    assert_eq!(fks[1][2], "public.regions(id, country)");
    let (_, triggers, _, _) = properties_table(&app, visual, PropertiesTab::Triggers);
    assert!(!triggers[0][4].contains('\n'));
    assert_eq!(triggers[0][3], "Yes");
    assert_eq!(triggers[1][3], "No");
    app.read_with(visual, |this, _| {
        let id = this.workspace.active_tab().unwrap().id;
        let table = &this.workspace.runtime(id).unwrap().columns_panel.props.tables[&PropertiesTab::Triggers];
        assert!(table.rows[0].tooltip.as_ref().unwrap().contains('\n'));
    });

    // Back to Columns; nothing was refetched while switching tabs, leaving
    // Properties for the grid or coming back.
    properties_click_tab(visual, PropertiesTab::Columns);
    assert!(visual.debug_bounds("columns-view-row").is_some());
    let grid_button = visual.debug_bounds("columns-rail-grid-button").unwrap();
    visual.simulate_click(grid_button.center(), Default::default());
    settle(visual);
    properties_open(visual);
    assert_eq!(properties_snapshot(&app, visual).2, 1, "cached per result: one fetch");
}

#[gpui::test]
fn properties_hide_tabs_the_driver_cannot_report(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    // Same tab, now a SQLite table: the key change refetches and SQLite
    // cannot report References.
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab_id).unwrap().connection_id = crate::ui_fixture::SQLITE_ID.into();
    });
    properties_open(visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!(status, LoadStatus::Loaded);
    assert_eq!(fetches, 1);
    assert_eq!(
        visible,
        [
            PropertiesTab::Columns,
            PropertiesTab::Keys,
            PropertiesTab::ForeignKeys,
            PropertiesTab::Indexes,
            PropertiesTab::Triggers,
            PropertiesTab::Ddl,
        ]
    );
    assert!(visual.debug_bounds(PropertiesTab::References.selector()).is_none());
    assert!(visual.debug_bounds(PropertiesTab::Keys.selector()).is_some());

    // A reported but empty list keeps its tab and shows an empty state.
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    let (headers, rows, ..) = properties_table(&app, visual, PropertiesTab::ForeignKeys);
    assert!(rows.is_empty());
    assert_eq!(headers.len(), 5, "empty lists keep their headers");
    assert!(visual.debug_bounds("properties-empty").is_some());
    // Optional columns hide when no row has a value (SQLite index: no method/definition).
    properties_click_tab(visual, PropertiesTab::Indexes);
    let (headers, ..) = properties_table(&app, visual, PropertiesTab::Indexes);
    assert_eq!(headers, ["Name", "Columns", "Unique", "Primary"]);
}

#[gpui::test]
fn properties_show_loading_then_content_and_refetch_only_on_explicit_refresh(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| this.show_columns_presentation(tab_id, cx));
    // First frame: the fetch is in flight; only Columns is offered.
    visual.update(|window, _| window.refresh());
    assert_eq!(properties_snapshot(&app, visual).1, LoadStatus::Loading);
    assert!(visual.debug_bounds("properties-loading").is_some(), "loading row");
    assert!(visual.debug_bounds("columns-view-row").is_some(), "Columns works meanwhile");
    assert!(visual.debug_bounds(PropertiesTab::Keys.selector()).is_none());
    settle(visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!((status, fetches, visible.len()), (LoadStatus::Loaded, 1, 7));
    assert!(visual.debug_bounds("properties-loading").is_none());

    // Rendering again never refetches, and neither does a new result
    // (re-run / auto-refresh / paging): table metadata does not change.
    for _ in 0..3 {
        settle(visual);
    }
    assert_eq!(properties_snapshot(&app, visual).2, 1);
    let generation = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().query.generation())
    };
    let before = generation(visual);
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    settle(visual);
    assert!(generation(visual) > before, "a new result generation ran");
    app.update(visual, |this, cx| this.run_active_tab_query(cx));
    settle(visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!((status, fetches, visible.len()), (LoadStatus::Loaded, 1, 7), "no refetch");

    // The refresh icon refetches while the cached data stays on screen.
    let refresh = visual.debug_bounds("properties-refresh").expect("refresh icon in the strip row");
    let strip = visual.debug_bounds("properties-tab-strip").unwrap();
    assert!(refresh.right() <= strip.right() && refresh.left() > strip.center().x, "right end of the strip");
    visual.simulate_click(refresh.center(), Default::default());
    visual.update(|window, _| window.refresh());
    let (_, _, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!(fetches, 2, "manual refresh refetches");
    assert_eq!(visible.len(), 7, "no flicker: cached data stays");
    assert!(visual.debug_bounds("properties-loading").is_none());
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).1, LoadStatus::Loaded);

    // Sidebar Refresh of the table's connection / the table itself refetches;
    // another table's refresh does not.
    let node = |object: &str| NavigatorNodeId::Object {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
        object: object.into(),
    };
    app.update(visual, |this, _| this.invalidate_table_properties(&node("customers")));
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).2, 2, "other tables are not affected");
    app.update(visual, |this, _| this.invalidate_table_properties(&node("orders")));
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).2, 3, "the table's own Refresh");
    app.update(visual, |this, _| {
        this.invalidate_table_properties(&NavigatorNodeId::Connection {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
        })
    });
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).2, 4, "the connection's Refresh");
    assert_eq!(properties_snapshot(&app, visual).1, LoadStatus::Loaded);
}

#[gpui::test]
fn properties_error_keeps_columns_working_and_retry_recovers(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    crate::ui_fixture_properties::FAIL_NEXT.with(|fail| fail.set(1));
    app.update(visual, |this, cx| this.show_columns_presentation(tab_id, cx));
    settle(visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!(status, LoadStatus::Failed("fixture properties error".into()));
    assert_eq!(fetches, 1);
    assert_eq!(visible, [PropertiesTab::Columns], "no data: Columns only");
    assert!(visual.debug_bounds("properties-error").is_some(), "error row");
    assert!(visual.debug_bounds("columns-view-row").is_some(), "Columns keeps working");
    assert!(visual.debug_bounds(PropertiesTab::Keys.selector()).is_none());
    // No automatic retry loop while rendering.
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).2, 1);

    let retry = visual.debug_bounds("properties-retry").expect("Retry link");
    visual.simulate_click(retry.center(), Default::default());
    settle(visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!((status, fetches, visible.len()), (LoadStatus::Loaded, 2, 7));
    assert!(visual.debug_bounds("properties-error").is_none());
    assert!(visual.debug_bounds("properties-retry").is_none());
    properties_click_tab(visual, PropertiesTab::Indexes);
    assert_eq!(properties_rendered_rows(visual), 3);
}

#[gpui::test]
fn properties_list_tabs_filter_select_and_reset_the_filter_per_tab(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_table_app(cx);
    properties_open(visual);
    properties_click_tab(visual, PropertiesTab::Indexes);
    let input = app.read_with(visual, |this, _| {
        this.columns_filter_inputs.values().next().cloned().expect("filter input")
    });
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.set_value("GIN", window, cx));
    });
    settle(visual);
    let (_, _, filtered, title) = properties_table(&app, visual, PropertiesTab::Indexes);
    assert_eq!((filtered, title.as_str()), (1, "Indexes (1/3)"));
    assert_eq!(properties_rendered_rows(visual), 1);
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.set_value("zzz", window, cx));
    });
    settle(visual);
    assert_eq!(properties_rendered_rows(visual), 0);
    assert!(visual.debug_bounds("properties-empty").is_some(), "no matches");
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.set_value("", window, cx));
    });
    settle(visual);
    assert_eq!(properties_rendered_rows(visual), 3);

    // Row selection: click highlights, arrows step, the tab stays.
    let row = visual.debug_bounds("properties-row-1").unwrap();
    visual.simulate_click(row.center(), Default::default());
    settle(visual);
    let selected = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            let id = this.workspace.active_tab().unwrap().id;
            this.workspace.runtime(id).unwrap().columns_panel.props.tables[&PropertiesTab::Indexes].selected
        })
    };
    assert_eq!(selected(visual), Some(1));
    visual.simulate_keystrokes("down");
    settle(visual);
    assert_eq!(selected(visual), Some(2));
    visual.simulate_keystrokes("up up up");
    settle(visual);
    assert_eq!(selected(visual), Some(0), "clamped at the top");
    assert_eq!(properties_snapshot(&app, visual).0, PropertiesTab::Indexes);

    // Switching tab starts with an empty filter.
    app.update_in(visual, |_, window, cx| {
        input.update(cx, |state, cx| state.set_value("gin", window, cx));
    });
    settle(visual);
    properties_click_tab(visual, PropertiesTab::Keys);
    settle(visual);
    let (_, _, filtered, _) = properties_table(&app, visual, PropertiesTab::Keys);
    assert_eq!(filtered, 3, "filter did not leak into the Keys tab");
    assert_eq!(app.read_with(visual, |_, cx| input.read(cx).value().to_string()), "");
}

#[gpui::test]
fn ddl_tab_shows_a_read_only_sql_editor_and_copies_the_statement(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_table_app(cx);
    properties_open(visual);
    properties_click_tab(visual, PropertiesTab::Ddl);
    settle(visual);
    let ddl = app.read_with(visual, |this, _| {
        let id = this.workspace.active_tab().unwrap().id;
        this.workspace.runtime(id).unwrap().columns_panel.props.data.as_ref().unwrap().ddl.clone().unwrap()
    });
    assert!(ddl.starts_with("CREATE TABLE public.orders"));
    app.read_with(visual, |this, cx| {
        let id = this.workspace.active_tab().unwrap().id;
        let editor = this.workspace.runtime(id).unwrap().columns_panel.props.ddl.as_ref().expect("DDL editor");
        let state = editor.state.read(cx);
        assert_eq!(state.language_name().as_ref(), "sql", "tree-sitter SQL highlighting");
        assert!(!state.is_editable(), "read-only");
        assert_eq!(state.value().as_ref(), ddl);
    });
    let editor = visual.debug_bounds("properties-ddl-editor").expect("editor");
    let toolbar = visual.debug_bounds("properties-toolbar").expect("toolbar");
    assert!(editor.top() >= toolbar.bottom() - gpui::px(1.0));
    assert!(editor.size.height > gpui::px(100.0));
    assert!(visual.debug_bounds("properties-table-header").is_none(), "not a table");
    assert!(visual.debug_bounds("columns-view-filter").is_none(), "no filter on DDL");

    // Typing into the read-only editor changes nothing.
    visual.simulate_click(editor.center(), Default::default());
    visual.simulate_input("zzz");
    settle(visual);
    app.read_with(visual, |this, cx| {
        let id = this.workspace.active_tab().unwrap().id;
        let editor = this.workspace.runtime(id).unwrap().columns_panel.props.ddl.as_ref().unwrap();
        assert_eq!(editor.state.read(cx).value().as_ref(), ddl);
    });

    // Copy puts the whole statement on the clipboard.
    let copy = visual.debug_bounds("properties-ddl-copy").expect("Copy button");
    let title = visual.debug_bounds("properties-title").expect("title");
    assert!(copy.left() >= title.right(), "right of the title");
    assert!(copy.top() >= toolbar.top() && copy.bottom() <= toolbar.bottom(), "inside the toolbar row");
    visual.simulate_click(copy.center(), Default::default());
    settle(visual);
    assert_eq!(
        visual.read_from_clipboard().and_then(|item| item.text()).as_deref(),
        Some(ddl.as_str())
    );

    // Re-rendering does not rebuild the editor.
    let revision = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            let id = this.workspace.active_tab().unwrap().id;
            this.workspace.runtime(id).unwrap().columns_panel.props.ddl.as_ref().unwrap().revision
        })
    };
    let before = revision(visual);
    settle(visual);
    assert_eq!(revision(visual), before);
}

#[gpui::test]
fn mongo_properties_show_columns_only_and_never_fetch(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    properties_open(visual);
    assert!(visual.debug_bounds("columns-view").is_some());
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!((status, fetches), (LoadStatus::Idle, 0));
    assert_eq!(visible, [PropertiesTab::Columns]);
    for tab in &PropertiesTab::ALL[1..] {
        assert!(visual.debug_bounds(tab.selector()).is_none(), "{tab:?}");
    }
    assert!(visual.debug_bounds(PropertiesTab::Columns.selector()).is_some());
    assert!(visual.debug_bounds("properties-loading").is_none());
    assert!(visual.debug_bounds("properties-error").is_none());
}

#[gpui::test]
fn query_tabs_without_a_known_source_table_show_columns_only(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    // The result-grid scenario is a table-backed tab: it fetches...
    app.update(visual, |this, _| {
        let id = this.workspace.active_tab().unwrap().id;
        // ...an arbitrary SELECT has no source table.
        this.workspace.tab_mut(id).unwrap().query_mode = None;
    });
    properties_open(visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!((status, fetches), (LoadStatus::Idle, 0));
    assert_eq!(visible, [PropertiesTab::Columns]);
}

/// Real driver end-to-end: a temporary SQLite file served by the real SQLite
/// driver (`SqlRig`), a table-data tab whose result really ran, and the
/// un-mocked `describe_object` behind the Keys / Indexes / DDL sub-tabs. Only
/// the Keychain/storage part of the connect flow (saved connection -> URI) is
/// bypassed.
#[gpui::test]
fn real_driver_sqlite_properties_populate_keys_indexes_and_ddl(cx: &mut TestAppContext) {
    let db = SqliteDb::new(
        "CREATE TABLE customers(id INTEGER PRIMARY KEY, email TEXT UNIQUE);\n\
         CREATE TABLE orders(id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id), \
         qty INTEGER CHECK (qty > 0));\n\
         CREATE INDEX orders_customer_idx ON orders(customer_id);\n\
         CREATE TRIGGER orders_ai AFTER INSERT ON orders BEGIN SELECT 1; END;\n\
         INSERT INTO customers VALUES (1, 'a@example.com');\n\
         INSERT INTO orders VALUES (1, 1, 2), (2, 1, 3);",
    );
    let rig = SqlRig::sqlite(cx, &db);
    let (app, key) = (rig.app.clone(), rig.key.clone());
    let visual = rig.visual;
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));

    let tab_id = app.update(visual, |this, _| {
        let tab_id = this.workspace.open_query_tab_with_metadata(
            "orders",
            TabMetadata {
                connection_id: key.clone(),
                database: "main".into(),
                schema: Some("main".into()),
                collection: "orders".into(),
                query_mode: Some(QueryMode::SqlTable),
                ..Default::default()
            },
        );
        this.workspace.tab_mut(tab_id).unwrap().sql = "SELECT * FROM \"main\".\"orders\"".into();
        this.reset_active_view();
        tab_id
    });
    // The driver works on its own threads: poll until the result is in.
    let wait = |visual: &mut gpui::VisualTestContext, done: &dyn Fn(&AppRoot) -> bool| {
        for _ in 0..500 {
            visual.run_until_parked();
            if app.read_with(visual, |this, _| done(this)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for the driver");
    };
    // Runs the table's SELECT through the tab's own Run path (a new result
    // generation each time).
    let run_result = |visual: &mut gpui::VisualTestContext| {
        app.update(visual, |this, cx| this.run_active_tab_query(cx));
        wait(visual, &|this| {
            matches!(
                this.workspace.runtime(tab_id).unwrap().query.status(),
                crate::query::QueryStatus::Ready { .. }
            )
        });
    };
    run_result(visual);
    wait(visual, &|this| this.active_grid().row_count() == 2);
    settle(visual);

    properties_open(visual);
    wait(visual, &|this| {
        this.workspace.runtime(tab_id).unwrap().columns_panel.props.status != LoadStatus::Loading
    });
    settle(visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, visual);
    assert_eq!(status, LoadStatus::Loaded, "the driver's describe_object answered");
    assert_eq!(fetches, 1);
    assert_eq!(visible, PropertiesTab::ALL, "the real SQLite driver reports every list");

    properties_click_tab(visual, PropertiesTab::Keys);
    let (headers, keys, ..) = properties_table(&app, visual, PropertiesTab::Keys);
    assert!(headers.starts_with(&["Name", "Type", "Columns"]), "{headers:?}");
    assert!(
        keys.iter().any(|row| row[1] == "PK" && row[2] == "id"),
        "primary key of orders: {keys:?}"
    );
    assert_eq!(properties_rendered_rows(visual), keys.len());

    properties_click_tab(visual, PropertiesTab::Indexes);
    let (_, indexes, ..) = properties_table(&app, visual, PropertiesTab::Indexes);
    assert!(
        indexes.iter().any(|row| row[0] == "orders_customer_idx" && row[1] == "customer_id"),
        "explicit index: {indexes:?}"
    );

    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    let (_, fks, ..) = properties_table(&app, visual, PropertiesTab::ForeignKeys);
    assert!(fks.iter().any(|row| row.iter().any(|cell| cell.starts_with("customers"))), "{fks:?}");

    properties_click_tab(visual, PropertiesTab::Triggers);
    let (_, triggers, ..) = properties_table(&app, visual, PropertiesTab::Triggers);
    assert!(triggers.iter().any(|row| row[0] == "orders_ai"), "{triggers:?}");

    // Nothing references `orders`: the tab exists (driver-reported) but is empty.
    properties_click_tab(visual, PropertiesTab::References);
    let (_, references, ..) = properties_table(&app, visual, PropertiesTab::References);
    assert!(references.is_empty(), "{references:?}");
    assert!(visual.debug_bounds("properties-empty").is_some());

    properties_click_tab(visual, PropertiesTab::Ddl);
    app.read_with(visual, |this, cx| {
        let editor = this.workspace.runtime(tab_id).unwrap().columns_panel.props.ddl.as_ref().expect("DDL editor");
        let state = editor.state.read(cx);
        let text = state.value();
        assert!(text.contains("CREATE TABLE") && text.contains("orders"), "{text}");
        assert_eq!(state.language_name().as_ref(), "sql");
        assert!(!state.is_editable());
    });
    assert!(visual.debug_bounds("properties-ddl-editor").is_some());

    // Paging / re-running the result did not refetch; the refresh icon does.
    run_result(visual);
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).2, 1, "a new result does not refetch");
    let refresh = visual.debug_bounds("properties-refresh").expect("refresh icon");
    visual.simulate_click(refresh.center(), Default::default());
    wait(visual, &|this| {
        let props = &this.workspace.runtime(tab_id).unwrap().columns_panel.props;
        props.fetches == 2 && props.status != LoadStatus::Loading
    });
    assert_eq!(properties_snapshot(&app, visual).1, LoadStatus::Loaded);

}
