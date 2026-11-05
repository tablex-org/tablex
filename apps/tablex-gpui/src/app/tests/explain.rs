// Explain plan viewer proofs (R2-ui): the toolbar button, its Analyze menu entry, the shortcut and the
// palette entries hand the tab's driver route the statement `engine_plan::explain_sql` builds; the Plan
// presentation renders the `engine-plan` fixtures (virtualized rows, disclosure, selection, detail pane,
// Raw view, misestimate highlight); refusals are the tab's inline error. The SQLite tests run the
// real SQLite driver (built on demand like `sql_driver.rs`); the live ones at the end are `#[ignore]`d:
//
//   cargo test -p tablex-gpui --features bench live_explain -- --ignored --test-threads=1
//
// against the dedicated containers (PG 127.0.0.1:55432, MySQL 53306, MariaDB 53307, ClickHouse 58123, credentials
// tablex / tablex, database tablex), see the live tests for the environment variables.

use engine_plan::{ExplainDialect, ExplainMode};

/// `engine_plan::explain_sql` with the dialect's default lexer options (what an app tab gets on a
/// session that reports nothing special).
fn explain_sql(
    dialect: ExplainDialect,
    mode: ExplainMode,
    statement: &str,
    read_only: bool,
) -> Result<engine_plan::ExplainStatement, engine_plan::ExplainError> {
    engine_plan::explain_sql(dialect, mode, statement, read_only, &dialect.default_lex_options())
}

const ALL_EXPLAIN_KINDS: [(ConnectionKind, ExplainDialect); 5] = [
    (ConnectionKind::Postgres, ExplainDialect::Postgres),
    (ConnectionKind::Mysql, ExplainDialect::MySql),
    (ConnectionKind::Mariadb, ExplainDialect::MariaDb),
    (ConnectionKind::Sqlite, ExplainDialect::Sqlite),
    (ConnectionKind::Clickhouse, ExplainDialect::ClickHouse),
];

fn explain_connection(kind: ConnectionKind) -> SavedConnection {
    SavedConnection {
        id: uuid::Uuid::new_v4(),
        name: match super::explain::explain_dialect(kind) {
            Some(dialect) => dialect.name().to_string(),
            None => format!("{kind:?}"),
        },
        uri: "unused://nowhere".into(),
        kind,
        save_password: false,
        is_pinned: false,
        disabled: false,
        ssh_tunnel: false,
    }
}

/// An app (production theme, `Root`) whose SQL editor tabs sit on connections that have no driver
/// session and no installed driver: a run is handed to the driver route and fails there with the
/// statement it was given, which is what the tests read back.
struct ExplainRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
}

impl<'a> ExplainRig<'a> {
    fn new(cx: &'a mut TestAppContext) -> Self {
        cx.dispatcher.allow_parking();
        cx.update(|cx| cx.set_reduce_motion(true));
        let (app, visual) = sidebar_test_app(cx);
        visual.update(|_, cx| actions::bind_keys(cx));
        visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
        visual.update(|window, _| window.activate_window());
        let store_dir = std::env::temp_dir().join(format!("tablex-explain-store-{}", uuid::Uuid::new_v4()));
        app.update(visual, |this, _| {
            this.scenario_ui.service = None;
            this.driver_ui.store = std::sync::Arc::new(engine_driver_host::InstalledDriverStore::new(&store_dir));
        });
        Self { app, visual }
    }

    /// A SQL editor tab on a new connection of `kind`, made the active tab.
    fn tab(&mut self, kind: ConnectionKind, sql: &str) -> u64 {
        let tab = self.app.update(self.visual, |this, cx| {
            let saved = explain_connection(kind);
            let key = saved.id.to_string();
            this.saved_connections.push(saved);
            let tab = this.workspace.open_query_tab_with_metadata(
                "SQL",
                TabMetadata {
                    connection_id: key,
                    ..Default::default()
                },
            );
            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
            this.workspace.activate_tab(tab);
            cx.notify();
            tab
        });
        settle(self.visual);
        tab
    }

    fn click(&mut self, selector: &'static str) {
        click_debug_button(self.visual, selector);
        settle(self.visual);
    }

    fn rendered(&mut self, selector: &str) -> bool {
        // `debug_bounds` wants a 'static key; a leaked probe string per call is test-only.
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        self.visual.debug_bounds(selector).is_some()
    }

    fn status(&self, tab: u64) -> crate::query::QueryStatus {
        self.app
            .read_with(self.visual, |this, _| this.workspace.runtime(tab).unwrap().query.status().clone())
    }

    /// The statement of the tab's failed run (what the driver route was given).
    fn failed_sql(&self, tab: u64) -> Option<String> {
        self.app.read_with(self.visual, |this, _| {
            this.workspace
                .runtime(tab)
                .unwrap()
                .query
                .failed_run()
                .map(|run| run.sql.clone())
        })
    }

    fn wait_failed(&mut self, tab: u64) {
        let (app, visual) = (self.app.clone(), &mut *self.visual);
        wait_until(visual, "the explain run to reach the driver route", |visual| {
            app.read_with(visual, |this, _| {
                matches!(
                    this.workspace.runtime(tab).unwrap().query.status(),
                    crate::query::QueryStatus::Failed(_)
                )
            })
        });
        settle(self.visual);
    }
}

#[gpui::test]
fn the_explain_button_hands_every_dialect_the_statement_explain_sql_builds(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    for (kind, dialect) in ALL_EXPLAIN_KINDS {
        let tab = rig.tab(kind, "SELECT id, name FROM items WHERE id > 3;");
        assert!(rig.rendered("explain-query"), "{dialect:?}: the Explain button is offered");
        assert!(rig.rendered("run-query"));
        rig.click("explain-query");
        rig.wait_failed(tab);
        let expected = explain_sql(dialect, ExplainMode::Estimate, "SELECT id, name FROM items WHERE id > 3;", true)
            .unwrap()
            .sql;
        assert_eq!(rig.failed_sql(tab).as_deref(), Some(expected.as_str()), "{dialect:?}");
    }
}

impl ExplainRig<'_> {
    fn key_of(&self, tab: u64) -> String {
        self.app
            .read_with(self.visual, |this, _| this.workspace.tab(tab).unwrap().connection_id.clone())
    }

    /// Opens the Explain button's menu (its caret is the right edge of the split button).
    fn open_explain_menu(&mut self) {
        let bounds = self.visual.debug_bounds("explain-dropdown").expect("the split button is rendered");
        let caret = gpui::point(bounds.right() - gpui::px(8.0), bounds.center().y);
        self.visual.simulate_click(caret, Default::default());
        settle(self.visual);
        assert!(self.rendered("explain-menu-explain"), "the menu is open");
    }

    fn explain_analyze_block(&self, tab: u64) -> Option<String> {
        self.app
            .read_with(self.visual, |this, cx| this.explain_analyze_block(tab, cx))
    }

    fn editor(&mut self, tab: u64) -> gpui::Entity<EditorState> {
        let app = self.app.clone();
        self.visual
            .update(|window, cx| app.update(cx, |this, cx| this.editor_for_tab(tab, window, cx)))
    }
}

#[gpui::test]
fn explain_analyze_is_disabled_for_a_write_with_the_reason_and_never_runs(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "DELETE FROM items WHERE id = 1");
    assert_eq!(
        rig.explain_analyze_block(tab).as_deref(),
        Some("EXPLAIN ANALYZE executes the statement; only read-only statements are allowed")
    );
    rig.open_explain_menu();
    assert!(rig.rendered("explain-menu-analyze"), "the entry is listed, disabled");
    rig.click("explain-menu-analyze");
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Idle, "a disabled entry does nothing");
    assert!(rig.failed_sql(tab).is_none());
    // The menu stays open on a disabled entry; Explain (estimate) of the same write is fine: it
    // does not execute the statement.
    if !rig.rendered("explain-menu-explain") {
        rig.open_explain_menu();
    }
    rig.click("explain-menu-explain");
    rig.wait_failed(tab);
    assert_eq!(
        rig.failed_sql(tab).as_deref(),
        Some("EXPLAIN (FORMAT JSON) DELETE FROM items WHERE id = 1")
    );
}

#[gpui::test]
fn explain_analyze_runs_a_read_only_statement_and_is_disabled_where_the_dialect_has_none(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let select = "SELECT id FROM items WHERE id > 3";

    let postgres = rig.tab(ConnectionKind::Postgres, select);
    assert_eq!(rig.explain_analyze_block(postgres), None);
    rig.open_explain_menu();
    rig.click("explain-menu-analyze");
    rig.wait_failed(postgres);
    let expected = explain_sql(ExplainDialect::Postgres, ExplainMode::Analyze, select, true).unwrap().sql;
    assert_eq!(rig.failed_sql(postgres).as_deref(), Some(expected.as_str()));
    assert_eq!(expected, "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT id FROM items WHERE id > 3");

    for (kind, name) in [(ConnectionKind::Sqlite, "SQLite"), (ConnectionKind::Clickhouse, "ClickHouse")] {
        let tab = rig.tab(kind, select);
        assert_eq!(
            rig.explain_analyze_block(tab),
            Some(format!("{name} has no EXPLAIN ANALYZE")),
            "{name}"
        );
        rig.open_explain_menu();
        rig.click("explain-menu-analyze");
        assert_eq!(rig.status(tab), crate::query::QueryStatus::Idle, "{name}: nothing ran");
        assert!(rig.failed_sql(tab).is_none());
    }
    for (kind, expected) in [
        (ConnectionKind::Mysql, "EXPLAIN ANALYZE SELECT id FROM items WHERE id > 3"),
        (ConnectionKind::Mariadb, "ANALYZE FORMAT=JSON SELECT id FROM items WHERE id > 3"),
    ] {
        let tab = rig.tab(kind, select);
        assert_eq!(rig.explain_analyze_block(tab), None);
        rig.open_explain_menu();
        rig.click("explain-menu-analyze");
        rig.wait_failed(tab);
        assert_eq!(rig.failed_sql(tab).as_deref(), Some(expected), "{kind:?}");
    }
}

#[gpui::test]
fn a_refused_statement_is_the_tabs_inline_error_not_a_crash(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    for (sql, message) in [
        ("", "the statement is empty"),
        ("  -- nothing\n", "the statement is empty"),
        ("EXPLAIN SELECT 1", "the statement already starts with EXPLAIN"),
        ("describe items", "the statement already starts with DESCRIBE"),
    ] {
        let tab = rig.tab(ConnectionKind::Postgres, sql);
        rig.click("explain-query");
        assert_eq!(
            rig.status(tab),
            crate::query::QueryStatus::Failed(message.into()),
            "{sql:?}: refused before any run"
        );
    }
    // Several statements selected at once: one statement only.
    let tab = rig.tab(ConnectionKind::Postgres, "SELECT 1;\nSELECT 2");
    let editor = rig.editor(tab);
    let app = rig.app.clone();
    rig.visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value("SELECT 1;\nSELECT 2", window, cx);
            state.select_all(window, cx);
        });
        app.update(cx, |_, cx| cx.notify());
    });
    rig.click("explain-query");
    assert_eq!(
        rig.status(tab),
        crate::query::QueryStatus::Failed("only a single statement can be explained".into())
    );
    // A refusal is a warning in the Error Log, like Run's own refusals; nothing reached a driver.
    let reached_driver = rig
        .app
        .read_with(rig.visual, |this, _| this.query_tasks.pending_runs.contains_key(&tab));
    assert!(!reached_driver);
    // An unexplainable tab (MongoDB) is not offered Explain and the shortcut does nothing there.
    let mongo = rig.tab(ConnectionKind::Mongodb, "db.items.find({})");
    assert!(!rig.rendered("explain-query"), "no Explain button on a MongoDB tab");
    tb_focus_root(&rig.app, rig.visual);
    rig.visual.simulate_keystrokes(&primary("shift-e"));
    settle(rig.visual);
    assert_eq!(rig.status(mongo), crate::query::QueryStatus::Idle);
    let redis = rig.tab(ConnectionKind::Redis, "GET key");
    assert!(!rig.rendered("explain-query"), "no Explain button on a Redis tab");
    assert_eq!(rig.status(redis), crate::query::QueryStatus::Idle);
}

#[gpui::test]
fn explain_takes_the_selection_else_the_statement_under_the_cursor_else_everything(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let script = "SELECT 1;\nSELECT 2;\nSELECT 3";
    let explain = |sql: &str| explain_sql(ExplainDialect::Postgres, ExplainMode::Estimate, sql, true).unwrap().sql;
    let tab = rig.tab(ConnectionKind::Postgres, script);
    let editor = rig.editor(tab);
    let app = rig.app.clone();
    let target = |rig: &ExplainRig| rig.app.read_with(&*rig.visual, |this, cx| this.explain_target(tab, cx));

    // The cursor is in the second statement.
    rig.visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value(script, window, cx);
            state.set_cursor_position(gpui_component::input::Position::new(1, 3), window, cx);
        });
    });
    assert_eq!(target(&rig), "SELECT 2");
    rig.click("explain-query");
    rig.wait_failed(tab);
    assert_eq!(rig.failed_sql(tab).unwrap(), explain("SELECT 2"));

    // A selection wins over the cursor.
    let selection = script.rfind("SELECT 3").unwrap()..script.len();
    rig.visual.update(|_, cx| {
        editor.update(cx, |state, cx| state.set_selected_range(selection.clone(), cx));
    });
    assert_eq!(target(&rig), "SELECT 3");
    app.update(rig.visual, |this, cx| this.explain_tab_query(tab, ExplainMode::Estimate, cx));
    rig.wait_failed(tab);
    assert_eq!(rig.failed_sql(tab).unwrap(), explain("SELECT 3"));

    // No selection, an editor whose text is one statement: that statement (the whole editor).
    let single = rig.tab(ConnectionKind::Postgres, "SELECT 42 -- the whole editor");
    assert_eq!(
        rig.app.read_with(rig.visual, |this, cx| this.explain_target(single, cx)),
        "SELECT 42 -- the whole editor"
    );
}

#[gpui::test]
fn the_shortcut_and_the_palette_entries_run_explain_on_the_active_sql_tab(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let select = "SELECT id FROM items WHERE id > 3";
    let explain = |mode| explain_sql(ExplainDialect::Postgres, mode, select, true).unwrap().sql;

    let tab = rig.tab(ConnectionKind::Postgres, select);
    // Keyboard focus must be somewhere inside the app for an action to reach it.
    tb_focus_root(&rig.app, rig.visual);
    rig.visual.simulate_keystrokes(&primary("shift-e"));
    settle(rig.visual);
    rig.wait_failed(tab);
    assert_eq!(rig.failed_sql(tab).unwrap(), explain(ExplainMode::Estimate), "Cmd/Ctrl+Shift+E");

    // Explain Analyze has no default chord (AltGr clashes); the action is there for a binding.
    let tab = rig.tab(ConnectionKind::Postgres, select);
    tb_focus_root(&rig.app, rig.visual);
    rig.visual.update(|window, cx| window.dispatch_action(gpui::Action::boxed_clone(&ExplainPlanAnalyze), cx));
    settle(rig.visual);
    rig.wait_failed(tab);
    assert_eq!(rig.failed_sql(tab).unwrap(), explain(ExplainMode::Analyze), "the Explain Analyze action");
    let def = shortcuts::find("editor.explain_analyze").unwrap();
    assert!(def.default_chords().is_empty(), "no default chord");

    // The palette lists both entries for a SQL tab, and Enter on one runs it.
    let tab = rig.tab(ConnectionKind::Postgres, select);
    let titles = rig.app.update(rig.visual, |this, _| this.palette_action_titles());
    assert!(titles.contains(&"Explain".to_string()) && titles.contains(&"Explain Analyze".to_string()), "{titles:?}");
    palette_open(rig.visual);
    palette_type(rig.visual, "explain analyze");
    rig.visual.simulate_keystrokes("enter");
    palette_settle(rig.visual);
    rig.wait_failed(tab);
    assert_eq!(rig.failed_sql(tab).unwrap(), explain(ExplainMode::Analyze), "palette: Explain Analyze");

    let tab = rig.tab(ConnectionKind::Postgres, select);
    palette_open(rig.visual);
    palette_type(rig.visual, "explain");
    let snapshot = palette_snapshot(&rig.app, rig.visual);
    let actions = palette_titles(&snapshot, "Actions");
    assert_eq!(actions.first().map(String::as_str), Some("Explain"), "{actions:?}");
    rig.visual.simulate_keystrokes("enter");
    palette_settle(rig.visual);
    rig.wait_failed(tab);
    assert_eq!(rig.failed_sql(tab).unwrap(), explain(ExplainMode::Estimate), "palette: Explain");

    // Not offered on tabs that cannot explain.
    let _mongo = rig.tab(ConnectionKind::Mongodb, "db.items.find({})");
    let titles = rig.app.update(rig.visual, |this, _| this.palette_action_titles());
    assert!(!titles.iter().any(|title| title == "Explain" || title == "Explain Analyze"), "{titles:?}");
}

#[gpui::test]
fn explain_analyze_on_a_production_connection_asks_first_and_estimate_does_not(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let select = "SELECT id FROM items WHERE id > 3";
    let tab = rig.tab(ConnectionKind::Postgres, select);
    let key = rig.key_of(tab);
    give_profile(&rig.app.clone(), rig.visual, &key, typed(PRODUCTION));

    // Estimate does not execute the statement: no question.
    rig.click("explain-query");
    rig.wait_failed(tab);
    assert!(rig.visual.debug_bounds("execute-confirm-run").is_none());

    // Analyze does: the connection's execute confirmation applies; Cancel runs nothing.
    let tab = rig.tab(ConnectionKind::Postgres, select);
    let key = rig.key_of(tab);
    give_profile(&rig.app.clone(), rig.visual, &key, typed(PRODUCTION));
    rig.open_explain_menu();
    rig.click("explain-menu-analyze");
    assert!(rig.visual.debug_bounds("execute-confirm-run").is_some(), "the confirmation opened");
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Idle, "nothing ran yet");
    rig.click("execute-confirm-cancel");
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Idle);
    assert!(rig.failed_sql(tab).is_none());

    rig.open_explain_menu();
    rig.click("explain-menu-analyze");
    rig.click("execute-confirm-run");
    rig.wait_failed(tab);
    let expected = explain_sql(ExplainDialect::Postgres, ExplainMode::Analyze, select, true).unwrap().sql;
    assert_eq!(rig.failed_sql(tab).as_deref(), Some(expected.as_str()), "the confirmed run is the Analyze");
}

// ---- the Plan presentation ------------------------------------------------------------

use crate::plan_view::fixtures as plan_fixtures;

impl ExplainRig<'_> {
    /// A SQL tab on a connected connection (so the results area, not the reconnect card, shows).
    fn connected_tab(&mut self, kind: ConnectionKind) -> u64 {
        let tab = self.tab(kind, "SELECT 1");
        let key = self.key_of(tab);
        self.app.update(self.visual, |this, cx| {
            let epoch = this.sessions.begin_connect(&key).unwrap();
            this.sessions.finish_connect(&key, epoch, Ok(()));
            cx.notify();
        });
        settle(self.visual);
        tab
    }

    /// Delivers `outcome` as the result of an Explain run of `sql` on `tab`, through the real
    /// completion path (`finish_explain_run`).
    fn finish_explain(
        &mut self,
        tab: u64,
        explain: ExplainRun,
        sql: &str,
        outcome: Result<engine_plan::Plan, ExplainRunError>,
    ) {
        self.app.update(self.visual, |this, cx| {
            let connection = this.workspace.tab(tab).unwrap().connection_id.clone();
            let run = this.workspace.runtime_mut(tab).unwrap().query.begin().expect("no run in flight");
            assert!(this.query_tasks.registry.register(tab, run));
            let request = SqlRun {
                tab_id: tab,
                kind: RunKind::Explain(explain),
                connection,
                sql: sql.into(),
                database: String::new(),
                collection: String::new(),
                limit: drivers::EXPLAIN_ROW_LIMIT,
                offset: 0,
                confirmed: false,
                read_only: false,
            };
            this.finish_explain_run(run, request, explain, outcome, cx);
        });
        settle(self.visual);
    }

    /// Shows the `engine-plan` fixture `path` as the tab's plan.
    fn show_fixture(&mut self, tab: u64, dialect: ExplainDialect, mode: ExplainMode, path: &str) {
        let plan = plan_fixtures::plan(dialect, mode, path);
        self.finish_explain(tab, ExplainRun { dialect, mode, read_only: true }, "EXPLAIN fixture", Ok(plan));
        settle(self.visual);
    }

    fn with_plan<T>(&self, tab: u64, read: impl FnOnce(&crate::plan_view::PlanView) -> T) -> T {
        self.app.read_with(self.visual, |this, _| {
            read(this.workspace.runtime(tab).unwrap().plan.as_ref().expect("the tab has a plan"))
        })
    }

    /// Rows of the node list that are built right now.
    fn built_rows(&mut self, upto: usize) -> Vec<usize> {
        (0..upto)
            .filter(|index| {
                let selector: &'static str = Box::leak(format!("plan-row-{index}").into_boxed_str());
                self.visual.debug_bounds(selector).is_some()
            })
            .collect()
    }

    fn click_at(&mut self, selector: String) {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        click_debug_button(self.visual, selector);
        settle(self.visual);
    }
}

#[gpui::test]
fn a_postgres_analyze_plan_renders_its_root_and_children_with_header_and_columns(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    rig.show_fixture(tab, ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/join_analyze.json");
    assert!(rig.rendered("plan-view"));
    assert!(rig.rendered("plan-columns-header"));
    let nodes = rig.with_plan(tab, |view| view.plan.nodes.len());
    assert!(nodes > 3);
    // The root and its children are listed, the root at the left edge, children indented.
    let rows = rig.built_rows(nodes);
    assert_eq!(rows, (0..nodes).collect::<Vec<_>>(), "every node of a small plan is listed");
    let root = rig.visual.debug_bounds("plan-disclosure-0").unwrap();
    let child = rig.visual.debug_bounds("plan-disclosure-1").unwrap();
    let grandchild = rig.visual.debug_bounds("plan-disclosure-2").unwrap();
    assert!(child.left() > root.left() && grandchild.left() > child.left(), "indented by depth");
    assert!(child.top() > root.top(), "pre-order, one row each");
    // Header: total cost, planning / execution time, warnings.
    for selector in ["plan-title", "plan-total-cost", "plan-planning-time", "plan-execution-time", "plan-node-count"] {
        assert!(rig.rendered(selector), "{selector}");
    }
    let warnings = rig.with_plan(tab, |view| view.plan.warnings.len());
    assert_eq!(rig.rendered("plan-warnings"), warnings > 0);
    // The Plan rail button is lit, the grid area is the plan.
    assert!(rig.rendered("plan-rail-toggle"));
    assert!(rig.rendered("columns-rail-grid-button"));
    // A tab's rows are untouched by the plan.
    rig.app.read_with(rig.visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        assert_eq!(runtime.grid.row_count(), 0);
        assert!(runtime.last_executed_query.is_none());
    });
}

#[gpui::test]
fn a_mysql_estimate_plan_hides_the_columns_it_has_no_data_for(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Mysql);
    rig.show_fixture(tab, ExplainDialect::MySql, ExplainMode::Estimate, "mysql/join_estimate.json");
    let columns = rig.with_plan(tab, |view| view.columns);
    assert!(columns.cost && !columns.actual_rows && !columns.time && !columns.loops);
    let full_width = {
        let header = rig.visual.debug_bounds("plan-columns-header").unwrap();
        header.size.width
    };
    assert!(full_width > gpui::px(300.0));
    // SQLite: no costs either.
    let sqlite = rig.connected_tab(ConnectionKind::Sqlite);
    rig.show_fixture(sqlite, ExplainDialect::Sqlite, ExplainMode::Estimate, "sqlite/join_estimate.tsv");
    let columns = rig.with_plan(sqlite, |view| view.columns);
    assert!(!columns.cost && !columns.actual_rows);
    assert!(rig.rendered("plan-row-0"));
    assert!(!rig.rendered("plan-misestimate-0"));
}

#[gpui::test]
fn the_disclosure_collapses_and_expands_a_subtree_and_a_click_selects_a_node(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    rig.show_fixture(tab, ExplainDialect::Postgres, ExplainMode::Estimate, "postgres/cte_estimate.json");
    let nodes = rig.with_plan(tab, |view| view.plan.nodes.len());
    assert_eq!(rig.built_rows(nodes).len(), nodes);
    assert!(!rig.rendered("plan-detail-field-0"), "nothing selected: the pane asks for a node");

    // Node 1 has the child 2 and a later sibling 3 (depths 0 1 2 1 ...).
    rig.click_at("plan-disclosure-1".into());
    let (collapsed, selected) = rig.with_plan(tab, |view| (view.is_collapsed(1), view.selected));
    assert!(collapsed, "the disclosure collapsed node 1");
    assert_eq!(selected, Some(1), "a click selects the row it is in");
    assert!(!rig.rendered("plan-row-2"), "the child is gone");
    assert!(rig.rendered("plan-row-3"), "the sibling stays");
    assert_eq!(rig.built_rows(nodes).len(), nodes - 1);
    rig.click_at("plan-disclosure-1".into());
    assert!(rig.rendered("plan-row-2"), "expanded again");
    assert!(!rig.with_plan(tab, |view| view.is_collapsed(1)));

    // A click on the row (not the disclosure) selects it and fills the detail pane, props included.
    rig.click_at("plan-row-3".into());
    assert_eq!(rig.with_plan(tab, |view| view.selected), Some(3));
    assert!(rig.rendered("plan-detail-field-0"));
    let (typed, props) = rig.with_plan(tab, |view| {
        let node = view.node(3).unwrap();
        (crate::plan_view::PlanView::detail_fields(node).len() - node.props.len(), node.props.len())
    });
    assert!(typed > 2 && props > 0, "the fixture node has fields and props: {typed} {props}");
    assert!(rig.rendered(&format!("plan-detail-field-{}", typed - 1)));
    assert!(rig.rendered(&format!("plan-detail-prop-{}", props - 1)), "every prop is listed");
    assert!(!rig.rendered(&format!("plan-detail-prop-{props}")));
    // Another node replaces the detail.
    rig.click_at("plan-row-0".into());
    let first_props = rig.with_plan(tab, |view| view.node(0).unwrap().props.len());
    assert_eq!(rig.rendered(&format!("plan-detail-prop-{}", first_props.saturating_sub(1))), first_props > 0);

    // Keyboard: down moves, left collapses the node, right expands it again.
    rig.visual.simulate_keystrokes("down");
    settle(rig.visual);
    assert_eq!(rig.with_plan(tab, |view| view.selected), Some(1));
    rig.visual.simulate_keystrokes("left");
    settle(rig.visual);
    assert!(rig.with_plan(tab, |view| view.is_collapsed(1)));
    rig.visual.simulate_keystrokes("right");
    settle(rig.visual);
    assert!(!rig.with_plan(tab, |view| view.is_collapsed(1)));
    rig.visual.simulate_keystrokes("right");
    settle(rig.visual);
    assert_eq!(rig.with_plan(tab, |view| view.selected), Some(2), "right on an expanded node enters it");
    rig.visual.simulate_keystrokes("left");
    settle(rig.visual);
    assert_eq!(rig.with_plan(tab, |view| view.selected), Some(1), "left on a leaf selects the parent");

    // A double click toggles (anywhere in the row).
    let row = rig.visual.debug_bounds("plan-row-1").unwrap();
    rig.visual.simulate_event(gpui::MouseDownEvent {
        position: row.center(),
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    settle(rig.visual);
    assert!(rig.with_plan(tab, |view| view.is_collapsed(1)), "a double click toggled");
}

#[gpui::test]
fn row_estimates_the_engine_flags_are_highlighted_and_others_are_not(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Mysql);
    rig.show_fixture(tab, ExplainDialect::MySql, ExplainMode::Analyze, "mysql/union_analyze.txt");
    let (nodes, flagged) = rig.with_plan(tab, |view| {
        (
            view.plan.nodes.len(),
            (0..view.plan.nodes.len())
                .filter(|index| view.node(*index).unwrap().misestimate.is_some())
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(flagged, vec![0, 1, 2], "the engine flags three nodes of the union plan");
    for index in 0..nodes {
        assert_eq!(
            rig.rendered(&format!("plan-misestimate-{index}")),
            flagged.contains(&index),
            "node {index}"
        );
    }
    assert!(rig.rendered("plan-warning-0"), "and the plan says so in its warnings");
    // The detail pane says how far off it is.
    rig.click_at("plan-row-0".into());
    let estimate = rig.with_plan(tab, |view| {
        crate::plan_view::PlanView::detail_fields(view.node(0).unwrap())
            .into_iter()
            .find(|(label, _)| label == "Estimate")
    });
    assert!(estimate.is_some_and(|(_, text)| text.starts_with("off by ")));
    // A plan without any flagged node is not highlighted; an over-estimate under a Limit is not flagged.
    let other = rig.connected_tab(ConnectionKind::Postgres);
    rig.show_fixture(other, ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/cte_analyze.json");
    assert!(rig.with_plan(other, |view| view.plan.nodes.iter().all(|node| node.misestimate.is_none())));
    assert!(!rig.rendered("plan-misestimate-3") && !rig.rendered("plan-warning-0"));
}

#[gpui::test]
fn the_raw_view_shows_the_servers_output_read_only_and_copy_puts_it_on_the_clipboard(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    rig.show_fixture(tab, ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/join_analyze.json");
    let raw = rig.with_plan(tab, |view| view.plan.raw.clone());
    assert!(raw.contains("\"Node Type\""));
    assert!(!rig.rendered("plan-raw-editor"));
    rig.click("plan-view-raw");
    assert!(rig.rendered("plan-raw-editor"), "Raw replaces the list");
    assert!(!rig.rendered("plan-row-0"));
    let shown = rig.app.read_with(rig.visual, |this, cx| {
        this.view(tab).plan_raw.as_ref().map(|editor| editor.state_for_test().read(cx).value().to_string())
    });
    assert_eq!(shown.as_deref(), Some(raw.as_str()));
    // Read only: typing into it does not change the text.
    let editor = rig.app.read_with(rig.visual, |this, _| this.view(tab).plan_raw.as_ref().unwrap().state_for_test());
    let (cx_editor, bounds) = (editor.clone(), rig.visual.debug_bounds("plan-raw-editor").unwrap());
    rig.visual.simulate_click(bounds.center(), Default::default());
    rig.visual.simulate_input("typed");
    settle(rig.visual);
    assert_eq!(cx_editor.read_with(rig.visual, |editor, _| editor.value().to_string()), raw);
    // Copy.
    rig.click("plan-copy");
    assert_eq!(rig.visual.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(raw.as_str()));
    // Back to the plan: the list again with its state.
    rig.click("plan-view-tree");
    assert!(rig.rendered("plan-row-0") && !rig.rendered("plan-raw-editor"));
    // A new plan refills the Raw editor (other text) instead of keeping the old one.
    rig.show_fixture(tab, ExplainDialect::Postgres, ExplainMode::Estimate, "postgres/index_estimate.json");
    let next_raw = rig.with_plan(tab, |view| view.plan.raw.clone());
    assert_ne!(next_raw, raw);
    rig.click("plan-view-raw");
    let shown = rig.app.read_with(rig.visual, |this, cx| {
        this.view(tab).plan_raw.as_ref().map(|editor| editor.state_for_test().read(cx).value().to_string())
    });
    assert_eq!(shown.as_deref(), Some(next_raw.as_str()));
    // MySQL's tree text and SQLite rows also show (plain text).
    let mysql = rig.connected_tab(ConnectionKind::Mysql);
    rig.show_fixture(mysql, ExplainDialect::MySql, ExplainMode::Analyze, "mysql/join_analyze.txt");
    rig.click("plan-view-raw");
    assert!(rig.rendered("plan-raw-editor"));
}

#[gpui::test]
fn the_rail_switches_between_the_grid_and_the_plan_and_a_new_result_replaces_the_plan(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    rig.show_fixture(tab, ExplainDialect::Postgres, ExplainMode::Estimate, "postgres/join_estimate.json");
    assert!(rig.rendered("plan-view"));
    rig.click("columns-rail-grid-button");
    assert!(!rig.rendered("plan-view"), "Grid leaves the plan");
    assert!(rig.rendered("plan-rail-toggle"), "the plan stays one click away");
    rig.click("plan-rail-toggle");
    assert!(rig.rendered("plan-view"));
    // Rows landing from a Run replace the plan on screen.
    rig.app.update(rig.visual, |this, cx| {
        let batch = arrow_array::RecordBatch::try_new(
            std::sync::Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new("n", arrow_schema::DataType::Int64, false)])),
            vec![std::sync::Arc::new(arrow_array::Int64Array::from(vec![1, 2, 3]))],
        )
        .unwrap();
        this.workspace.runtime_mut(tab).unwrap().grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![batch.into()])));
        this.reset_tab_view(tab);
        cx.notify();
    });
    settle(rig.visual);
    rig.click("plan-rail-toggle");
    rig.click("columns-rail-grid-button");
    assert!(!rig.rendered("plan-view"));
}

#[gpui::test]
fn a_plan_that_cannot_be_read_fails_the_run_and_an_error_log_entry_keeps_the_reason(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    let explain = ExplainRun { dialect: ExplainDialect::Postgres, mode: ExplainMode::Estimate, read_only: true };
    rig.finish_explain(
        tab,
        explain,
        "EXPLAIN (FORMAT JSON) SELECT 1",
        Err(ExplainRunError::Plan(engine_plan::ExplainError::EmptyOutput)),
    );
    assert_eq!(
        rig.status(tab),
        crate::query::QueryStatus::Failed("Could not read the plan: the explain output is empty".into())
    );
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().plan.is_none());
        assert_eq!(this.error_log.log.len(), 1, "the Error Log holds it");
    });
    // A failure of the query itself goes through Run's own handling (here: the user's cancel is a
    // status, not an error).
    rig.finish_explain(tab, explain, "EXPLAIN (FORMAT JSON) SELECT 1", Err(ExplainRunError::Query(DriverQueryError::Cancelled)));
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Cancelled);
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.error_log.log.len()), 1);
    // And the next successful run still works.
    let plan = plan_fixtures::plan(ExplainDialect::Postgres, ExplainMode::Estimate, "postgres/index_estimate.json");
    let nodes = plan.nodes.len();
    rig.finish_explain(tab, explain, "EXPLAIN (FORMAT JSON) SELECT 1", Ok(plan));
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Ready { rows: nodes });
    assert!(rig.rendered("plan-view"));
}

#[gpui::test]
fn a_ten_thousand_node_plan_builds_only_the_rows_in_the_viewport(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    let plan = plan_fixtures::synthetic(10_000);
    let started = std::time::Instant::now();
    rig.finish_explain(
        tab,
        ExplainRun { dialect: ExplainDialect::Postgres, mode: ExplainMode::Analyze, read_only: true },
        "EXPLAIN synthetic",
        Ok(plan),
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(rig.rendered("plan-view"));
    let built = rig.built_rows(10_000);
    assert!(!built.is_empty() && built.len() <= 60, "only the viewport's rows exist: {}", built.len());
    // Rendering again builds the same bounded number of rows, not one per node.
    super::results::plan::PLAN_ROWS_BUILT.with(|count| count.set(0));
    for _ in 0..3 {
        rig.visual.update(|window, _| window.refresh());
        rig.visual.run_until_parked();
    }
    let built_per_frame = super::results::plan::PLAN_ROWS_BUILT.with(|count| count.get());
    assert!(built_per_frame <= 3 * 60, "{built_per_frame} rows built in three frames");
    // Collapsing the root leaves one row; scrolling to the end builds the last rows only.
    rig.click_at("plan-disclosure-0".into());
    assert_eq!(rig.built_rows(10_000), vec![0]);
    rig.click_at("plan-disclosure-0".into());
    let scroll = rig.app.read_with(rig.visual, |this, _| this.view(tab).plan_scroll.clone());
    rig.visual.update(|_, _| scroll.scroll_to_item_strict(9_999, gpui::ScrollStrategy::Bottom));
    settle(rig.visual);
    let built = rig.built_rows(10_000);
    assert!(built.contains(&9_999) && built.len() <= 60 && !built.contains(&0), "{built:?}");
}

// ---- Explain on the real SQLite driver ----------------------------------------------------

impl SqlRig<'_> {
    fn set_sql(&mut self, sql: &str) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| {
            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
            cx.notify();
        });
        settle(self.visual);
    }

    fn rendered(&mut self, selector: &'static str) -> bool {
        self.visual.debug_bounds(selector).is_some()
    }

    fn plan_nodes(&self) -> Option<usize> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().plan.as_ref().map(|view| view.plan.nodes.len())
        })
    }

    fn presentation(&self) -> crate::columns_panel::Presentation {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().columns_panel.presentation
        })
    }
}

#[gpui::test]
fn explain_on_the_sqlite_driver_shows_the_plan_and_leaves_the_rows_and_paging_alone(
    cx: &mut TestAppContext,
) {
    use crate::columns_panel::Presentation;
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run("SELECT * FROM items");
    rig.wait_not_running("the first Run");
    assert_eq!(rig.rows(), 5);
    let paging = rig.paging();

    rig.set_sql("SELECT * FROM items WHERE id > 2;");
    assert!(rig.rendered("explain-query"));
    rig.click("explain-query");
    rig.wait_status("the explain run", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    let nodes = rig.plan_nodes().expect("the run produced a plan");
    assert!(nodes >= 1);
    assert_eq!(rig.presentation(), Presentation::Plan);
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        let view = runtime.plan.as_ref().unwrap();
        // Exactly what `explain_sql` builds for the statement under the cursor (trailing `;` trimmed).
        assert_eq!(view.sql, "EXPLAIN QUERY PLAN SELECT * FROM items WHERE id > 2");
        assert_eq!(view.dialect, ExplainDialect::Sqlite);
        assert_eq!(view.mode, ExplainMode::Estimate);
        assert!(view.plan.nodes[0].operation.to_uppercase().contains("SEARCH"), "{:?}", view.plan.nodes[0]);
        assert!(view.plan.raw.contains("items"));
        assert_eq!(runtime.query.status(), &crate::query::QueryStatus::Ready { rows: nodes });
        // Rows, last executed statement and paging are those of the Run before.
        assert_eq!(runtime.grid.row_count(), 5);
        assert_eq!(runtime.last_executed_query.as_deref(), Some("SELECT * FROM items"));
    });
    assert_eq!(rig.paging(), paging);
    assert!(rig.rendered("plan-view") && rig.rendered("plan-row-0"));
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert!(!rig.operation_registered(), "the operation was released");

    // A Run brings the rows back; the plan stays behind its rail button.
    rig.click("run-query");
    rig.wait_not_running("the Run after the plan");
    assert_eq!(rig.rows(), 3);
    assert_eq!(rig.presentation(), Presentation::Grid);
    assert!(!rig.rendered("plan-view"));
    assert!(rig.plan_nodes().is_some() && rig.rendered("plan-rail-toggle"));
    rig.click("plan-rail-toggle");
    assert!(rig.rendered("plan-view"));
}

#[gpui::test]
fn explain_that_the_server_rejects_or_that_is_refused_is_an_inline_error_and_leaves_no_plan(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    // The server's own refusal: it reaches the Error Log, the tab shows it inline.
    rig.set_sql("SELECT * FROM missing_table");
    rig.click("explain-query");
    rig.wait_not_running("the rejected explain");
    match rig.status() {
        crate::query::QueryStatus::Failed(message) => {
            assert!(message.contains("no such table"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(rig.plan_nodes().is_none());
    assert!(rig.rendered("result-empty-state"), "the error is in the results area");
    assert_eq!(rig.log().len(), 1, "{:?}", rig.log());

    // explain_sql's refusals: no run reaches the driver, the message is the tab's status.
    rig.set_sql("EXPLAIN SELECT 1");
    rig.click("explain-query");
    assert_eq!(
        rig.status(),
        crate::query::QueryStatus::Failed("the statement already starts with EXPLAIN".into())
    );
    assert!(rig.rendered("result-empty-state"));
    assert!(!rig.operation_registered());
    // And the editor still works afterwards.
    rig.set_sql("SELECT * FROM items");
    rig.click("run-query");
    rig.wait_status("a Run after the refusals", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 5 }));
}

#[gpui::test]
fn an_explain_in_flight_can_be_cancelled_and_leaves_no_plan(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_sql("SELECT * FROM items");
    let tab = rig.tab;
    // Start it and cancel before the scheduler runs anything: the Run button reads Cancel meanwhile.
    rig.app.update(rig.visual, |this, cx| {
        this.explain_tab_query(tab, ExplainMode::Estimate, cx);
        assert!(!this.workspace.runtime(tab).unwrap().query.can_run(), "the explain is the tab's live run");
        this.cancel_tab_query(tab, cx);
    });
    rig.wait_not_running("the cancelled explain");
    settle(rig.visual);
    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert!(rig.plan_nodes().is_none(), "a cancelled explain shows no plan");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    rig.wait_driver_idle();
    // The tab recovers: Explain again works.
    rig.click("explain-query");
    rig.wait_status("the second explain", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert!(rig.plan_nodes().is_some());
}

#[gpui::test]
fn a_production_connection_does_not_ask_before_an_estimate_explain_on_the_driver(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.give_profile(typed(PRODUCTION));
    rig.set_sql("SELECT * FROM items");
    rig.click("explain-query");
    rig.wait_status("the explain", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert!(!rig.dialog_is_open("execute-confirm-run"));
    assert!(rig.plan_nodes().is_some());
}

#[test]
fn an_explain_result_over_the_size_cap_is_refused_not_parsed() {
    use arrow_array::{ArrayRef, RecordBatch, StringArray};
    let cell = "x".repeat(engine_plan::MAX_RAW_BYTES + 1);
    let batch = RecordBatch::try_from_iter([("QUERY PLAN", std::sync::Arc::new(StringArray::from(vec![cell])) as ArrayRef)]).unwrap();
    // Refused from the Arrow buffers, before a second copy of the output is made as text.
    let error = drivers::plan_rows(&batch).unwrap_err();
    assert!(
        matches!(error, ExplainRunError::Plan(engine_plan::ExplainError::OutputTooLarge { .. })),
        "{error:?}"
    );
}

#[test]
fn result_cells_of_any_type_become_the_text_a_plan_is_read_from() {
    use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use std::sync::Arc;
    // The shape of SQLite's EXPLAIN QUERY PLAN result: integer id / parent columns and text detail.
    let batch = RecordBatch::try_from_iter([
        ("id", Arc::new(Int64Array::from(vec![2, 5])) as ArrayRef),
        ("parent", Arc::new(Int64Array::from(vec![0, 2])) as ArrayRef),
        ("notused", Arc::new(Int64Array::from(vec![0, 0])) as ArrayRef),
        ("detail", Arc::new(StringArray::from(vec![Some("SCAN items"), None])) as ArrayRef),
    ])
    .unwrap();
    let rows = drivers::plan_rows(&batch).unwrap();
    assert_eq!(rows.columns, ["id", "parent", "notused", "detail"]);
    assert_eq!(rows.rows[0], [Some("2".into()), Some("0".into()), Some("0".into()), Some("SCAN items".into())]);
    assert_eq!(rows.rows[1][3], None, "NULL stays NULL");
}

// ---- live: the real drivers against the dedicated containers ---------------------------------
//
// Read-only SELECTs on catalog tables (nothing is created, nothing is written). Each test runs Explain, and
// Explain Analyze where the dialect has it, through the real Explain button and its menu, on the real driver:
//
//   cargo test -p tablex-gpui live_explain -- --ignored --test-threads=1
//
// The URIs default to the dedicated `tablex-test-*` containers (credentials tablex / tablex, database tablex):
// PG 127.0.0.1:55432, MySQL 53306, MariaDB 53307, ClickHouse 58123; override with TABLEX_TEST_PG_URI,
// TABLEX_TEST_MYSQL_URI, TABLEX_TEST_MARIADB_URI and TABLEX_TEST_CLICKHOUSE_URI.

impl SqlRig<'_> {
    fn open_explain_menu(&mut self) {
        let bounds = self.visual.debug_bounds("explain-dropdown").expect("the split button is rendered");
        let caret = gpui::point(bounds.right() - gpui::px(8.0), bounds.center().y);
        self.visual.simulate_click(caret, Default::default());
        settle(self.visual);
        assert!(self.rendered("explain-menu-explain"), "the menu is open");
    }
}

fn live_explain(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    uri: String,
    dialect: ExplainDialect,
    select: &str,
) {
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    rig.set_sql(select);
    let tab = rig.tab;
    for mode in [ExplainMode::Estimate, ExplainMode::Analyze] {
        let block = rig.app.read_with(rig.visual, |this, cx| this.explain_analyze_block(tab, cx));
        if mode == ExplainMode::Analyze && !dialect.supports_analyze() {
            assert_eq!(block, Some(format!("{} has no EXPLAIN ANALYZE", dialect.name())));
            rig.open_explain_menu();
            rig.click("explain-menu-analyze");
            assert!(rig.plan_nodes().is_some(), "the estimate plan is still shown");
            continue;
        }
        if mode == ExplainMode::Analyze {
            assert_eq!(block, None, "{dialect:?}: a SELECT may be analyzed");
        }
        if mode == ExplainMode::Estimate {
            rig.click("explain-query");
        } else {
            rig.open_explain_menu();
            rig.click("explain-menu-analyze");
        }
        rig.wait_status(
            "the explain run on the live server",
            |status| !matches!(status, crate::query::QueryStatus::Running { .. }),
        );
        let status = rig.status();
        assert!(matches!(status, crate::query::QueryStatus::Ready { .. }), "{dialect:?} {mode:?}: {status:?}");
        let expected = explain_sql(dialect, mode, select, true).unwrap().sql;
        rig.app.read_with(rig.visual, |this, _| {
            let view = this.workspace.runtime(tab).unwrap().plan.as_ref().expect("a plan");
            assert_eq!(view.sql, expected, "the statement sent is the one explain_sql builds");
            assert_eq!((view.dialect, view.mode), (dialect, mode));
            assert!(!view.plan.nodes.is_empty(), "{dialect:?} {mode:?}");
            assert!(!view.plan.raw.is_empty());
            if mode == ExplainMode::Analyze {
                assert!(view.columns.actual_rows, "{dialect:?}: analyze reports actual rows");
            } else {
                assert!(!view.columns.actual_rows, "{dialect:?}: an estimate has no actual rows");
            }
            eprintln!(
                "{dialect:?} {mode:?}: {} nodes, cost {:?}, time {:?}, warnings {:?}, columns {:?}",
                view.plan.nodes.len(),
                view.plan.total_cost,
                view.plan.total_time_ms,
                view.plan.warnings,
                view.columns
            );
        });
        assert!(rig.rendered("plan-view") && rig.rendered("plan-row-0"), "{dialect:?} {mode:?}: the plan is shown");
        assert!(rig.log().is_empty(), "{:?}", rig.log());
        assert_eq!(rig.rows(), 0, "the tab's rows are untouched");
    }
}

fn live_uri(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-pg container (127.0.0.1:55432)"]
fn live_explain_postgresql(cx: &mut TestAppContext) {
    live_explain(
        cx,
        ConnectionKind::Postgres,
        ("dbengine-postgres", "abi-v2"),
        live_uri("TABLEX_TEST_PG_URI", "postgres://tablex:tablex@127.0.0.1:55432/tablex"),
        ExplainDialect::Postgres,
        "SELECT c.relname, count(a.attnum) AS cols FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
         WHERE c.relkind = 'r' GROUP BY c.relname ORDER BY cols DESC LIMIT 10",
    );
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-mysql container (127.0.0.1:53306)"]
fn live_explain_mysql(cx: &mut TestAppContext) {
    live_explain(
        cx,
        ConnectionKind::Mysql,
        ("dbengine-mysql", "abi-v2"),
        live_uri("TABLEX_TEST_MYSQL_URI", "mysql://tablex:tablex@127.0.0.1:53306/tablex"),
        ExplainDialect::MySql,
        "SELECT table_schema, count(*) AS n FROM information_schema.tables GROUP BY table_schema ORDER BY n DESC LIMIT 10",
    );
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-mariadb container (127.0.0.1:53307)"]
fn live_explain_mariadb(cx: &mut TestAppContext) {
    live_explain(
        cx,
        ConnectionKind::Mariadb,
        ("dbengine-mariadb", "abi-v2"),
        live_uri("TABLEX_TEST_MARIADB_URI", "mariadb://tablex:tablex@127.0.0.1:53307/tablex"),
        ExplainDialect::MariaDb,
        "SELECT table_schema, count(*) AS n FROM information_schema.tables GROUP BY table_schema ORDER BY n DESC LIMIT 10",
    );
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-clickhouse container (127.0.0.1:58123)"]
fn live_explain_clickhouse(cx: &mut TestAppContext) {
    live_explain(
        cx,
        ConnectionKind::Clickhouse,
        CLICKHOUSE_DRIVER,
        live_uri("TABLEX_TEST_CLICKHOUSE_URI", "clickhouse://tablex:tablex@127.0.0.1:58123/tablex"),
        ExplainDialect::ClickHouse,
        "SELECT database, count() AS n FROM system.tables GROUP BY database ORDER BY n DESC LIMIT 10",
    );
}

#[gpui::test]
#[ignore = "live: runs the real SQLite driver on a temporary file"]
fn live_explain_sqlite(cx: &mut TestAppContext) {
    let db = SqliteDb::new(
        "CREATE TABLE a(id INTEGER PRIMARY KEY, grp INTEGER); CREATE TABLE b(id INTEGER PRIMARY KEY, a_id INTEGER, v REAL); \
         CREATE INDEX b_a ON b(a_id); INSERT INTO a(grp) VALUES (1),(2),(3); INSERT INTO b(a_id, v) VALUES (1,1.5),(2,2.5),(3,3.5);",
    );
    live_explain(
        cx,
        ConnectionKind::Sqlite,
        ("dbengine-sqlite", ""),
        db.uri(),
        ExplainDialect::Sqlite,
        "SELECT a.grp, sum(b.v) FROM a JOIN b ON b.a_id = a.id GROUP BY a.grp ORDER BY 2 DESC",
    );
}

// ---- the Analyze gate: server-accurate lexing and the server-side read-only run ----------------

#[gpui::test]
fn the_analyze_gate_reads_a_statement_the_way_the_server_does(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let analyze_block = |rig: &mut ExplainRig, kind, sql: &str| {
        let tab = rig.tab(kind, sql);
        (tab, rig.explain_analyze_block(tab))
    };
    let writes = Some("EXPLAIN ANALYZE executes the statement; only read-only statements are allowed".to_string());
    // PostgreSQL: the `\'` of an E'' string does not end it, so the INTO after it is code. A lexer
    // that ignores E'' strings reads it as text and calls the statement a read.
    let sql = r"SELECT E'it\'s' AS label, * INTO archive FROM orders";
    assert_eq!(analyze_block(&mut rig, ConnectionKind::Postgres, sql).1, writes);
    assert_eq!(analyze_block(&mut rig, ConnectionKind::Postgres, r"SELECT E'it\'s' AS label FROM orders").1, None);
    // MySQL / MariaDB: `'O\'Brien'` is one string; a lexer without backslash escapes lets the
    // INTO OUTFILE hide inside a "string".
    let sql = r"SELECT 'O\'Brien' AS n INTO OUTFILE '/tmp/tablex-explain-x'";
    assert_eq!(analyze_block(&mut rig, ConnectionKind::Mysql, sql).1, writes);
    assert_eq!(analyze_block(&mut rig, ConnectionKind::Mariadb, sql).1, writes);
    assert_eq!(analyze_block(&mut rig, ConnectionKind::Mysql, r"SELECT 'O\'Brien' AS n FROM t").1, None);
    // A hash comment hides a write only to a lexer that does not know it.
    assert_eq!(analyze_block(&mut rig, ConnectionKind::Mysql, "SELECT 1 # x\n INTO @v").1, writes);
    // Other writes and data-modifying CTEs (a script is cut to its statement at the cursor first).
    for sql in [
        "DELETE FROM t",
        "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
    ] {
        assert_eq!(analyze_block(&mut rig, ConnectionKind::Postgres, sql).1, writes, "{sql}");
    }
    // A refused Analyze never reaches the driver: it is the tab's inline error.
    let (tab, _) = analyze_block(&mut rig, ConnectionKind::Postgres, r"SELECT E'it\'s' AS label, * INTO archive FROM orders");
    rig.app.update(rig.visual, |this, cx| this.explain_tab_query(tab, ExplainMode::Analyze, cx));
    assert_eq!(
        rig.status(tab),
        crate::query::QueryStatus::Failed(
            "EXPLAIN ANALYZE executes the statement; only read-only statements are allowed".into()
        )
    );
    assert!(!rig.app.read_with(rig.visual, |this, _| this.query_tasks.pending_runs.contains_key(&tab)));
}

fn session_info(escapes: bool, double_quotes: bool, brackets: bool) -> engine_driver_api::SqlDialectInfo {
    engine_driver_api::SqlDialectInfo {
        identifier_quote: "`".into(),
        string_literal_quote: "'".into(),
        supports_schemas: false,
        database_qualified_names: true,
        like_requires_text_cast: false,
        string_backslash_escapes: escapes,
        double_quote_identifiers: double_quotes,
        bracket_identifiers: brackets,
    }
}

#[test]
fn the_session_can_make_double_quotes_and_brackets_identifiers_for_the_lexer() {
    use super::explain::{is_pure_read, lex_options};
    for dialect in [ExplainDialect::MySql, ExplainDialect::MariaDb] {
        assert!(!lex_options(dialect, None, None).double_quote_is_identifier, "default: a string");
        assert!(!lex_options(dialect, Some(&session_info(true, false, false)), None).double_quote_is_identifier);
        let ansi = lex_options(dialect, Some(&session_info(true, true, false)), None);
        assert!(ansi.double_quote_is_identifier && !ansi.bracket_identifiers, "ANSI_QUOTES");
        assert!(lex_options(dialect, Some(&session_info(true, true, true)), None).bracket_identifiers, "MSSQL");
        // To an ANSI_QUOTES server `"a\"` is an identifier, so the `DELETE` is a second statement.
        let sql = r#"SELECT "a\" FROM t; DELETE FROM t; SELECT "b""#;
        assert!(is_pure_read(sql, &lex_options(dialect, None, None)), "read as one string");
        assert!(!is_pure_read(sql, &ansi));
    }
    // PostgreSQL and SQLite keep `"` as an identifier when an older driver sends false.
    let old = session_info(false, false, false);
    assert!(lex_options(ExplainDialect::Postgres, Some(&old), None).double_quote_is_identifier);
    assert!(lex_options(ExplainDialect::Sqlite, Some(&old), None).double_quote_is_identifier);
    assert!(lex_options(ExplainDialect::Sqlite, Some(&old), None).bracket_identifiers);
}

#[test]
fn the_session_can_turn_backslash_escapes_off_for_the_lexer() {
    use super::explain::lex_options;
    let info = |escapes| session_info(escapes, false, false);
    assert!(lex_options(ExplainDialect::MySql, None, None).backslash_escapes, "MySQL default");
    assert!(lex_options(ExplainDialect::MySql, Some(&info(true)), None).backslash_escapes);
    assert!(
        !lex_options(ExplainDialect::MySql, Some(&info(false)), None).backslash_escapes,
        "NO_BACKSLASH_ESCAPES as the connect reply says"
    );
    // Under it, `'O\'` is a whole string and the next statement is a second one.
    let lex = lex_options(ExplainDialect::MySql, Some(&info(false)), None);
    assert!(!super::explain::is_pure_read(r"SELECT 'a\'; DELETE FROM t; SELECT 'b'", &lex));
    assert!(!lex_options(ExplainDialect::Postgres, None, None).backslash_escapes);
}

#[test]
fn every_explain_run_asks_the_driver_for_a_read_only_execution() {
    let request = |kind, confirmed| SqlRun {
        tab_id: 1,
        kind,
        connection: "c".into(),
        sql: "EXPLAIN SELECT 1".into(),
        database: String::new(),
        collection: String::new(),
        limit: 1,
        offset: 0,
        confirmed,
        read_only: false,
    };
    for mode in [ExplainMode::Estimate, ExplainMode::Analyze] {
        let kind = RunKind::Explain(ExplainRun { dialect: ExplainDialect::Postgres, mode, read_only: true });
        assert_eq!(request(kind, false).payload()["read_only"], true, "{mode:?}");
        assert_eq!(request(kind, true).payload()["read_only"], true);
        assert_eq!(request(kind, true).payload()["confirmed"], true);
    }
    assert!(request(RunKind::Sql, false).payload().get("read_only").is_none(), "a Run is not read-only");
    // A run on a read-only connection asks the driver for a read-only execution too.
    let read_only = |kind, confirmed| SqlRun { read_only: true, ..request(kind, confirmed) };
    assert_eq!(read_only(RunKind::Sql, false).payload(), serde_json::json!({"sql": "EXPLAIN SELECT 1", "read_only": true}));
    assert_eq!(
        read_only(RunKind::Sql, true).payload(),
        serde_json::json!({"sql": "EXPLAIN SELECT 1", "confirmed": true, "read_only": true})
    );
    let retried = RunKind::Explain(ExplainRun { dialect: ExplainDialect::Postgres, mode: ExplainMode::Estimate, read_only: false });
    assert_eq!(read_only(retried, false).payload()["read_only"], true, "the connection's flag is its own");
}

#[gpui::test]
fn explain_analyze_needs_a_driver_that_advertises_read_only_execution(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let select = "SELECT id FROM items";
    let tab = rig.tab(ConnectionKind::Postgres, select);
    let key = rig.key_of(tab);
    let capabilities = |features: &[&str]| {
        std::sync::Arc::new(engine_driver_api::DriverCapabilities::new(
            "postgresql",
            "PostgreSQL",
            features.iter().map(|feature| feature.to_string()).collect(),
        ))
    };
    let set = |rig: &mut ExplainRig, caps| {
        let key = key.clone();
        rig.app.update(rig.visual, |this, _| {
            this.driver_runtime.capabilities.insert(key, caps);
        });
    };
    set(&mut rig, capabilities(&["query.sql", "query.arrow_c_data"]));
    let reason = "Explain Analyze needs a driver that can run statements read-only (query.read_only); this one cannot";
    assert_eq!(rig.explain_analyze_block(tab).as_deref(), Some(reason));
    rig.open_explain_menu();
    rig.click("explain-menu-analyze");
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Idle, "a disabled entry does nothing");
    // The palette / shortcut path says it inline instead of running.
    rig.app.update(rig.visual, |this, cx| this.explain_tab_query(tab, ExplainMode::Analyze, cx));
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Failed(reason.into()));
    // Estimate does not need it.
    set(&mut rig, capabilities(&["query.sql", "query.arrow_c_data", "query.read_only"]));
    assert_eq!(rig.explain_analyze_block(tab), None);
}

#[gpui::test]
fn a_write_the_server_refused_in_the_read_only_run_is_said_plainly(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    let violation = engine_driver_host::LoaderError::DriverRejected {
        code: "query.read_only_violation".into(),
        message: "the statement tried to write in a read-only execution (SQLSTATE 25006)".into(),
        retryable: false,
    };
    rig.finish_explain(
        tab,
        ExplainRun { dialect: ExplainDialect::Postgres, mode: ExplainMode::Analyze, read_only: true },
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT nextval('s')",
        Err(ExplainRunError::Query(DriverQueryError::Driver(violation))),
    );
    assert_eq!(
        rig.status(tab),
        crate::query::QueryStatus::Failed(
            "Explain Analyze refused: the statement writes (it ran read-only, nothing was changed)".into()
        )
    );
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().plan.is_none());
        assert_eq!(this.error_log.log.len(), 1);
    });
}

// ---- live: the server's read-only guard (ignored; the dedicated containers only) ---------------

impl SqlRig<'_> {
    /// The first cell of the tab's result, after running `sql` as a normal Run.
    fn first_cell(&mut self, sql: &str) -> String {
        self.run(sql);
        self.wait_status("the probe", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace
                .runtime(tab)
                .unwrap()
                .grid
                .cell_text(CellPosition { row: 0, column: 0 })
                .map(|text| text.to_string())
                .unwrap_or_default()
        })
    }
}

/// Runs each `attack` (an EXPLAIN ANALYZE of a writing statement, built by hand: the UI gate would not let
/// it through) as an Explain run in the driver's read-only execution and checks that `probe` still reads
/// `expected` afterwards. An attack marked `refused` must fail with the plain read-only message (the server
/// raised its read-only error: needed for effects a rollback cannot undo, like `nextval`); an unmarked one
/// may be refused or run, but whatever it did was rolled back with the read-only transaction.
#[allow(clippy::too_many_arguments)]
fn live_explain_guard(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    uri: String,
    dialect: ExplainDialect,
    setup: &[&str],
    cleanup: &[&str],
    attacks: &[(&str, bool)],
    probe: &str,
    expected: &str,
) {
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let _cleanup = Cleanup {
        session: rig.session.clone(),
        statements: cleanup.iter().map(|sql| sql.to_string()).collect(),
    };
    for sql in cleanup.iter().chain(setup) {
        try_exec(&rig.session, sql);
    }
    let tab = rig.tab;
    for (attack, refused) in attacks {
        let sql = attack.to_string();
        rig.app.update(rig.visual, |this, cx| {
            this.start_sql_run(
                tab,
                sql,
                Some(0),
                Some(ExplainRun { dialect, mode: ExplainMode::Analyze, read_only: true }),
                cx,
            );
        });
        rig.wait_not_running("the read-only analyze");
        let status = rig.status();
        eprintln!("{dialect:?}: {attack} -> {status:?}");
        let message = "Explain Analyze refused: the statement writes (it ran read-only, nothing was changed)";
        if *refused {
            assert_eq!(status, crate::query::QueryStatus::Failed(message.into()), "{attack}");
            assert!(rig.plan_nodes().is_none());
        } else {
            assert!(
                status == crate::query::QueryStatus::Failed(message.into())
                    || matches!(status, crate::query::QueryStatus::Ready { .. }),
                "{attack}: {status:?}"
            );
        }
    }
    assert_eq!(rig.first_cell(probe), expected, "nothing the attacks tried to do happened");
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-pg container; creates and drops tablex_explain_* in database tablex"]
fn live_explain_guard_postgresql(cx: &mut TestAppContext) {
    live_explain_guard(
        cx,
        ConnectionKind::Postgres,
        ("dbengine-postgres", "abi-v2"),
        live_uri("TABLEX_TEST_PG_URI", "postgres://tablex:tablex@127.0.0.1:55432/tablex"),
        ExplainDialect::Postgres,
        &["CREATE SEQUENCE tablex_explain_seq"],
        &["DROP SEQUENCE IF EXISTS tablex_explain_seq", "DROP TABLE IF EXISTS tablex_explain_archive"],
        &[
            ("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT nextval('tablex_explain_seq')", true),
            // PostgreSQL runs this one inside the read-only transaction and the driver rolls it back.
            ("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT * INTO tablex_explain_archive FROM pg_class LIMIT 1", false),
        ],
        "SELECT (SELECT count(*) FROM pg_class WHERE relname = 'tablex_explain_archive')::text || ':' || \
         (SELECT is_called::text FROM tablex_explain_seq)",
        "0:false",
    );
}

const MYSQL_GUARD_SETUP: [&str; 2] = [
    "CREATE TABLE tablex_explain_guard (id INT PRIMARY KEY)",
    "INSERT INTO tablex_explain_guard VALUES (1), (2), (3)",
];
const MYSQL_GUARD_CLEANUP: [&str; 1] = ["DROP TABLE IF EXISTS tablex_explain_guard"];

#[gpui::test]
#[ignore = "live: needs the tablex-test-mysql container; creates and drops tablex_explain_guard in database tablex"]
fn live_explain_guard_mysql(cx: &mut TestAppContext) {
    live_explain_guard(
        cx,
        ConnectionKind::Mysql,
        ("dbengine-mysql", "abi-v2"),
        live_uri("TABLEX_TEST_MYSQL_URI", "mysql://tablex:tablex@127.0.0.1:53306/tablex"),
        ExplainDialect::MySql,
        &MYSQL_GUARD_SETUP,
        &MYSQL_GUARD_CLEANUP,
        &[("EXPLAIN ANALYZE DELETE FROM tablex_explain_guard WHERE id > 0", false)],
        "SELECT count(*) FROM tablex_explain_guard",
        "3",
    );
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-mariadb container; creates and drops tablex_explain_guard in database tablex"]
fn live_explain_guard_mariadb(cx: &mut TestAppContext) {
    live_explain_guard(
        cx,
        ConnectionKind::Mariadb,
        ("dbengine-mariadb", "abi-v2"),
        live_uri("TABLEX_TEST_MARIADB_URI", "mariadb://tablex:tablex@127.0.0.1:53307/tablex"),
        ExplainDialect::MariaDb,
        &MYSQL_GUARD_SETUP,
        &MYSQL_GUARD_CLEANUP,
        &[("ANALYZE FORMAT=JSON DELETE FROM tablex_explain_guard WHERE id > 0", false)],
        "SELECT count(*) FROM tablex_explain_guard",
        "3",
    );
}

// ---- the read-only requirement: capability check after connect, open transactions --------------

fn explain_request(explain: ExplainRun, sql: &str) -> SqlRun {
    SqlRun {
        tab_id: 1,
        kind: RunKind::Explain(explain),
        connection: "c".into(),
        sql: sql.into(),
        database: String::new(),
        collection: String::new(),
        limit: drivers::EXPLAIN_ROW_LIMIT,
        offset: 0,
        confirmed: false,
        read_only: false,
    }
}

fn rejected(code: &str) -> DriverQueryError {
    DriverQueryError::Driver(engine_driver_host::LoaderError::DriverRejected {
        code: code.into(),
        message: "refused".into(),
        retryable: false,
    })
}

#[test]
fn only_an_estimate_refused_for_an_open_transaction_is_sent_again_without_read_only() {
    use super::results::query_result::explain_retry;
    let unavailable = rejected("query.read_only_unavailable");
    let estimate = explain_request(ExplainRun::new(ExplainDialect::MySql, ExplainMode::Estimate), "EXPLAIN FORMAT=JSON SELECT 1");
    let retry = explain_retry(&unavailable, &estimate).expect("an estimate is retried");
    assert_eq!(retry.sql, estimate.sql, "the same statement");
    assert_eq!(estimate.payload()["read_only"], true);
    assert!(retry.payload().get("read_only").is_none(), "without the read-only request");
    assert!(explain_retry(&unavailable, &retry).is_none(), "once");
    // Never on a read-only connection: there the read-only run is the guarantee, and text the
    // app's lexer misread (a session in ANSI_QUOTES) could write without it.
    let guarded = SqlRun { read_only: true, ..estimate.clone() };
    assert_eq!(guarded.payload()["read_only"], true);
    assert!(explain_retry(&unavailable, &guarded).is_none(), "a read-only connection is never retried unguarded");
    // Analyze is never run unguarded; other failures are not retried.
    let analyze = explain_request(ExplainRun::new(ExplainDialect::MySql, ExplainMode::Analyze), "EXPLAIN ANALYZE SELECT 1");
    assert!(explain_retry(&unavailable, &analyze).is_none());
    assert!(explain_retry(&rejected("query.read_only_violation"), &estimate).is_none());
    assert!(explain_retry(&rejected("query.syntax"), &estimate).is_none());
    let mut run = estimate.clone();
    run.kind = RunKind::Sql;
    assert!(explain_retry(&unavailable, &run).is_none(), "a Run is not an Explain");
}

#[gpui::test]
fn an_open_transaction_retries_the_estimate_and_refuses_the_analyze_with_a_clear_message(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Mysql);
    let estimate = ExplainRun::new(ExplainDialect::MySql, ExplainMode::Estimate);
    rig.finish_explain(tab, estimate, "EXPLAIN FORMAT=JSON SELECT 1", Err(ExplainRunError::Query(rejected("query.read_only_unavailable"))));
    // The second attempt reached the driver route (which has no session here): the first refusal
    // was not the answer.
    assert_eq!(
        rig.status(tab),
        crate::query::QueryStatus::Failed("Driver session is not connected".into()),
        "the estimate was sent again"
    );
    let analyze = ExplainRun::new(ExplainDialect::MySql, ExplainMode::Analyze);
    rig.finish_explain(tab, analyze, "EXPLAIN ANALYZE SELECT 1", Err(ExplainRunError::Query(rejected("query.read_only_unavailable"))));
    assert_eq!(
        rig.status(tab),
        crate::query::QueryStatus::Failed("Explain Analyze needs a read-only run; finish the open transaction first".into())
    );
}

#[gpui::test]
fn explain_analyze_cannot_reach_a_driver_that_does_not_advertise_read_only_after_connect(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let (tab, key) = (rig.tab, rig.key.clone());
    let analyze = ExplainRun::new(ExplainDialect::Sqlite, ExplainMode::Analyze);
    let start = |rig: &mut SqlRig| {
        rig.app.update(rig.visual, |this, cx| {
            this.start_sql_run(tab, "EXPLAIN QUERY PLAN SELECT * FROM items".into(), Some(0), Some(analyze), cx);
        });
        rig.wait_not_running("the explain");
    };
    let features = |extra: &[&str]| {
        let mut features = vec!["query.sql".to_string(), "query.arrow_c_data".to_string()];
        features.extend(extra.iter().map(|feature| feature.to_string()));
        std::sync::Arc::new(engine_driver_api::DriverCapabilities::new("sqlite", "SQLite", features))
    };
    let unsupported = crate::query::QueryStatus::Failed(
        "Loaded driver does not support read-only execution (needed by Explain Analyze)".into(),
    );
    // A driver without `query.read_only`.
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.capabilities.insert(key.clone(), features(&[]));
    });
    start(&mut rig);
    assert_eq!(rig.status(), unsupported);
    assert!(rig.plan_nodes().is_none());
    // Unknown capabilities fail closed too (neither the session's nor the loaded handle's).
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.capabilities.remove(&key);
        this.driver_ui.handles.remove("sqlite");
    });
    start(&mut rig);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)));
    assert!(rig.plan_nodes().is_none());
    // With it the run goes through (SQLite answers rows the PostgreSQL-less reader may not parse: the
    // point is that the capability check passed).
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.capabilities.insert(key.clone(), features(&["query.read_only"]));
    });
    start(&mut rig);
    assert_ne!(rig.status(), unsupported);
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-mysql container (127.0.0.1:53306); opens and rolls back a transaction"]
fn live_explain_inside_an_open_transaction_estimates_and_refuses_analyze(cx: &mut TestAppContext) {
    let uri = live_uri("TABLEX_TEST_MYSQL_URI", "mysql://tablex:tablex@127.0.0.1:53306/tablex");
    let mut rig = SqlRig::new(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), &uri);
    let _rollback = Cleanup {
        session: rig.session.clone(),
        statements: vec!["ROLLBACK".to_string()],
    };
    assert!(try_exec(&rig.session, "START TRANSACTION"), "the user's transaction is open on the session");
    rig.set_sql("SELECT table_schema, count(*) AS n FROM information_schema.tables GROUP BY table_schema");
    // Estimate: refused read-only, sent again without it, the plan shows.
    rig.click("explain-query");
    rig.wait_status("the estimate", |status| !matches!(status, crate::query::QueryStatus::Running { .. }));
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { .. }), "{:?}", rig.status());
    assert!(rig.plan_nodes().is_some_and(|nodes| nodes > 0));
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    // Analyze: refused with the plain message, and the previous plan stays.
    let nodes = rig.plan_nodes();
    rig.open_explain_menu();
    rig.click("explain-menu-analyze");
    rig.wait_status("the analyze", |status| !matches!(status, crate::query::QueryStatus::Running { .. }));
    assert_eq!(
        rig.status(),
        crate::query::QueryStatus::Failed("Explain Analyze needs a read-only run; finish the open transaction first".into())
    );
    assert_eq!(rig.plan_nodes(), nodes);
}

// ---- server version in the lexer, Estimate of a write, the violation wording ---------------------

const VERSIONED_TRICK: &str = "SELECT 1 /*!99999 ' */ ; COMMIT; SELECT '";

#[test]
fn the_lexer_gets_the_session_version_for_mysql_and_mariadb() {
    use super::explain::{is_pure_read, lex_options};
    // On a 8.4.11 server `/*!99999` is a comment: the text is `SELECT 1`, `COMMIT`, `SELECT '...`.
    let known = lex_options(ExplainDialect::MySql, None, Some(80411));
    assert!(!is_pure_read(VERSIONED_TRICK, &known), "several statements");
    assert!(!engine_driver_kit::sql::classify(VERSIONED_TRICK, &known).version_ambiguous);
    assert!(is_pure_read("SELECT 1 /*!99999 x */", &known), "a future-version comment is a comment");
    assert!(
        matches!(
            engine_plan::explain_sql(ExplainDialect::MySql, ExplainMode::Estimate, VERSIONED_TRICK, false, &known),
            Err(engine_plan::ExplainError::MultipleStatements)
        ),
        "Estimate refuses it as more than one statement"
    );
    // Without the version the two readings differ: ambiguous, never a pure read.
    let unknown = lex_options(ExplainDialect::MySql, None, None);
    assert!(engine_driver_kit::sql::classify(VERSIONED_TRICK, &unknown).version_ambiguous);
    assert!(!is_pure_read(VERSIONED_TRICK, &unknown));
    // MariaDB takes its own numeric form; PostgreSQL ignores a version.
    assert!(!is_pure_read(VERSIONED_TRICK, &lex_options(ExplainDialect::MariaDb, None, Some(110809))));
    assert!(lex_options(ExplainDialect::Postgres, None, Some(170010)).dollar_quotes);
}

#[gpui::test]
fn text_that_depends_on_the_unknown_server_version_disables_analyze_with_a_reason(
    cx: &mut TestAppContext,
) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Mysql, VERSIONED_TRICK);
    // The editor's statement under the cursor is the first one: select everything.
    let editor = rig.editor(tab);
    rig.visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value(VERSIONED_TRICK, window, cx);
            state.select_all(window, cx);
        });
    });
    let reason = "Explain cannot judge this statement: it has version-dependent /*! ... */ comments and the server version is unknown";
    assert_eq!(rig.explain_analyze_block(tab).as_deref(), Some(reason));
    rig.app.update(rig.visual, |this, cx| this.explain_tab_query(tab, ExplainMode::Analyze, cx));
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Failed(reason.into()));
    // The Estimate is refused too: the server could read the comment as code and run the rest.
    rig.app.update(rig.visual, |this, cx| this.explain_tab_query(tab, ExplainMode::Estimate, cx));
    assert_eq!(rig.status(tab), crate::query::QueryStatus::Failed(reason.into()));
    // With the session's version the same text is judged like the server judges it: three
    // statements, so Explain refuses it as one statement and Analyze as not a read.
    let key = rig.key_of(tab);
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.server_versions.insert(key, 80411);
    });
    rig.app.update(rig.visual, |this, cx| this.explain_tab_query(tab, ExplainMode::Estimate, cx));
    assert_eq!(
        rig.status(tab),
        crate::query::QueryStatus::Failed("only a single statement can be explained".into())
    );
    assert!(rig.failed_sql(tab).is_some() && !rig.app.read_with(rig.visual, |this, _| this.query_tasks.pending_runs.contains_key(&tab)));
}

#[gpui::test]
fn only_the_estimate_of_a_pure_read_asks_for_a_read_only_run(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let run = |rig: &mut ExplainRig, kind, sql: &str, mode| {
        let tab = rig.tab(kind, sql);
        rig.app.update(rig.visual, |this, cx| this.explain_statement(tab, mode, sql, cx))
    };
    let select = "SELECT * FROM t WHERE id = 1";
    let write = "INSERT INTO tablex_explain_w (id) VALUES (1)";
    let (sql, explain) = run(&mut rig, ConnectionKind::Mysql, select, ExplainMode::Estimate).unwrap();
    assert_eq!(sql, "EXPLAIN FORMAT=JSON SELECT * FROM t WHERE id = 1");
    assert!(explain.read_only, "a read is explained read-only");
    let (sql, explain) = run(&mut rig, ConnectionKind::Mysql, write, ExplainMode::Estimate).unwrap();
    assert_eq!(sql, "EXPLAIN FORMAT=JSON INSERT INTO tablex_explain_w (id) VALUES (1)");
    assert!(!explain.read_only, "MySQL refuses a write inside READ ONLY; an EXPLAIN of it executes nothing");
    for (kind, sql) in [
        (ConnectionKind::Mariadb, "UPDATE t SET a = 1"),
        (ConnectionKind::Postgres, "DELETE FROM t WHERE id = 2"),
    ] {
        let (_, explain) = run(&mut rig, kind, sql, ExplainMode::Estimate).unwrap();
        assert!(!explain.read_only, "{kind:?} {sql}");
    }
    // Analyze is always read-only (and refused outright for a write).
    let (_, explain) = run(&mut rig, ConnectionKind::Mysql, select, ExplainMode::Analyze).unwrap();
    assert!(explain.read_only);
    assert!(run(&mut rig, ConnectionKind::Mysql, write, ExplainMode::Analyze).is_none());
    // The one-statement check still applies to a write's Estimate.
    assert!(
        run(&mut rig, ConnectionKind::Mysql, "INSERT INTO t VALUES (1); DROP TABLE t", ExplainMode::Estimate).is_none(),
        "several statements are refused even without read_only"
    );
}

#[gpui::test]
fn the_read_only_violation_says_which_explain_was_refused(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.connected_tab(ConnectionKind::Postgres);
    for (mode, expected) in [
        (
            ExplainMode::Estimate,
            "Explain refused: the statement writes (it ran read-only, nothing was changed)",
        ),
        (
            ExplainMode::Analyze,
            "Explain Analyze refused: the statement writes (it ran read-only, nothing was changed)",
        ),
    ] {
        rig.finish_explain(
            tab,
            ExplainRun::new(ExplainDialect::Postgres, mode),
            "EXPLAIN SELECT 1",
            Err(ExplainRunError::Query(rejected("query.read_only_violation"))),
        );
        assert_eq!(rig.status(tab), crate::query::QueryStatus::Failed(expected.into()), "{mode:?}");
    }
}

/// A throwaway table for the live write-Estimate tests.
const WRITE_TABLE: [&str; 1] = ["CREATE TABLE tablex_explain_w (id INT PRIMARY KEY)"];

fn live_explain_write(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    uri: String,
) {
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let _cleanup = Cleanup {
        session: rig.session.clone(),
        statements: vec!["DROP TABLE IF EXISTS tablex_explain_w".to_string()],
    };
    try_exec(&rig.session, "DROP TABLE IF EXISTS tablex_explain_w");
    for sql in WRITE_TABLE {
        assert!(try_exec(&rig.session, sql), "{sql}");
    }
    // Explain (Estimate) of an INSERT works and inserts nothing.
    rig.set_sql("INSERT INTO tablex_explain_w (id) VALUES (1), (2)");
    rig.click("explain-query");
    rig.wait_status("the explain of a write", |status| !matches!(status, crate::query::QueryStatus::Running { .. }));
    let status = rig.status();
    assert!(matches!(status, crate::query::QueryStatus::Ready { .. }), "{status:?}");
    assert!(rig.plan_nodes().is_some_and(|nodes| nodes > 0));
    assert_eq!(rig.first_cell("SELECT count(*) FROM tablex_explain_w"), "0", "nothing was inserted");
    // Analyze of the write is refused by the UI gate; nothing reaches the driver.
    rig.set_sql("INSERT INTO tablex_explain_w (id) VALUES (3)");
    let tab = rig.tab;
    assert!(rig.app.read_with(rig.visual, |this, cx| this.explain_analyze_block(tab, cx)).is_some());
    // The version-dependent trick is refused before it reaches the driver (the session's version
    // makes it several statements). Select it all: the editor's statement under the cursor is the first.
    let editor = {
        let app = rig.app.clone();
        rig.visual.update(|window, cx| app.update(cx, |this, cx| this.editor_for_tab(tab, window, cx)))
    };
    rig.visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            state.set_value(VERSIONED_TRICK, window, cx);
            state.select_all(window, cx);
        });
    });
    rig.app.update(rig.visual, |this, cx| this.explain_tab_query(tab, ExplainMode::Estimate, cx));
    assert_eq!(
        rig.status(),
        crate::query::QueryStatus::Failed("only a single statement can be explained".into())
    );
    assert!(!rig.operation_registered(), "it never reached the driver");
    assert_eq!(rig.first_cell("SELECT count(*) FROM tablex_explain_w"), "0");
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-mysql container; creates and drops tablex_explain_w in database tablex"]
fn live_explain_write_mysql(cx: &mut TestAppContext) {
    live_explain_write(
        cx,
        ConnectionKind::Mysql,
        ("dbengine-mysql", "abi-v2"),
        live_uri("TABLEX_TEST_MYSQL_URI", "mysql://tablex:tablex@127.0.0.1:53306/tablex"),
    );
}

#[gpui::test]
#[ignore = "live: needs the tablex-test-mariadb container; creates and drops tablex_explain_w in database tablex"]
fn live_explain_write_mariadb(cx: &mut TestAppContext) {
    live_explain_write(
        cx,
        ConnectionKind::Mariadb,
        ("dbengine-mariadb", "abi-v2"),
        live_uri("TABLEX_TEST_MARIADB_URI", "mariadb://tablex:tablex@127.0.0.1:53307/tablex"),
    );
}
