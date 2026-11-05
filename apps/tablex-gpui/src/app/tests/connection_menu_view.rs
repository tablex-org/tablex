// Native interaction proofs for the approved connection menu.
// All fixture credentials stay fake; the with-password clipboard path is
// never invoked here (see product tests with explicit fixture secrets).
//
// Scenario storage is isolated by an explicit project root; no test mutates HOME.

struct TempSqlProject {
    dir: std::path::PathBuf,
}

impl TempSqlProject {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("tablex-menu-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("isolated SQL projects root");
        Self { dir }
    }
}

impl Drop for TempSqlProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Polls the deterministic scheduler while real disk I/O completes on
/// blocking threads: each pass runs ready tasks, then yields the thread so
/// foreign completion can schedule back in. Exits on the condition, panics
/// on timeout. `allow_parking` keeps the foreign wake deterministic-clean.
fn wait_until(
    visual: &mut gpui::VisualTestContext,
    label: &'static str,
    mut ready: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    visual.dispatcher.allow_parking();
    let start = std::time::Instant::now();
    loop {
        visual.run_until_parked();
        if ready(visual) {
            return;
        }
        if start.elapsed() > std::time::Duration::from_secs(15) {
            panic!("timed out waiting for {label}");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn connection_row_index(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
    label: &str,
) -> usize {
    let index = app
        .read_with(visual, |this, _| {
            this.navigator
                .visible_rows()
                .iter()
                .position(|row| row.label == label)
        })
        .expect("connection row is visible");
    index
}

fn right_click_row(visual: &mut gpui::VisualTestContext, index: usize) {
    // debug_bounds needs a 'static selector; leaking one short probe string
    // per call is test-only and bounded by the call count below.
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    let row = visual
        .debug_bounds(selector)
        .expect("row bounds are measured");
    visual.simulate_mouse_down(row.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(row.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
}

fn click_debug_button(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is rendered"));
    visual.simulate_click(bounds.center(), Default::default());
    visual.run_until_parked();
}

fn open_filter_dialog(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    connection: &str,
) {
    let connection = connection.to_string();
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.open_visible_filter_dialog(&connection, window, cx);
        });
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("dialog-layer").is_some(),
        "filter dialog is mounted"
    );
}

#[gpui::test]
fn connection_menu_copy_name_writes_display_name_through_real_menu(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Disconnected Mongo menu: Connect(0), Copy Name(1) — first down lands on
    // index 0, second on the first clickable past the separator.
    let index = connection_row_index(&app, visual, "Mongo fixture");
    right_click_row(visual, index);
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    wait_until(visual, "copy-name history prefetch", |visual| {
        app.read_with(visual, |this, _| {
            !this.saved_query_history(crate::ui_fixture::MONGO_ID).0
        })
    });
    let text = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("menu copied the fixture display name");
    assert_eq!(text.to_string(), "Mongo fixture");
}

#[gpui::test]
fn connection_menu_new_query_opens_scoped_tab_through_real_menu(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    // Disconnected Postgres menu: Connect, Copy Name, Copy Info, New Query.
    let tabs_before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let index = connection_row_index(&app, visual, "Postgres fixture");
    right_click_row(visual, index);
    for _ in 0..4 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs_before + 1);
        let tab = this.workspace.active_tab().expect("menu opened a tab");
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
    });
    wait_until(visual, "new-query history prefetch", |visual| {
        app.read_with(visual, |this, _| {
            !this.saved_query_history(crate::ui_fixture::POSTGRES_ID).0
        })
    });
}

#[gpui::test]
fn disconnected_mongo_menu_new_query_opens_idle_script_editor(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let index = connection_row_index(&app, visual, "Mongo fixture");
    right_click_row(visual, index);
    for _ in 0..4 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.connection_id, crate::ui_fixture::MONGO_ID);
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
    wait_until(visual, "Mongo menu history prefetch", |visual| {
        app.read_with(visual, |this, _| {
            !this.saved_query_history(crate::ui_fixture::MONGO_ID).0
        })
    });
}

#[gpui::test]
fn connection_menu_escape_dismisses_without_side_effects(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let tabs_before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let index = connection_row_index(&app, visual, "Mongo fixture");
    right_click_row(visual, index);
    // Drive deep into the menu, then dismiss: nothing may open, select, or
    // delete. A subsequent Enter must fall through to the sidebar (which
    // moves focus) rather than activating a menu item.
    for _ in 0..10 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs_before);
        assert!(this.sidebar.notice.is_none());
    });
    wait_until(visual, "dismissed-menu history prefetch", |visual| {
        app.read_with(visual, |this, _| {
            !this.saved_query_history(crate::ui_fixture::MONGO_ID).0
        })
    });
}

#[gpui::test]
fn connection_edit_dialog_cancel_changes_nothing(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let before = app.read_with(visual, |this, _| this.saved_connections.clone());
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            let saved = this
                .saved_connections
                .iter()
                .find(|saved| saved.id.to_string() == crate::ui_fixture::MONGO_ID)
                .cloned();
            this.open_connection_dialog(saved, window, cx);
        });
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("dialog-layer").is_some(),
        "edit dialog is mounted"
    );
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("dialog-layer").is_none(),
        "Escape cancels the edit dialog"
    );
    app.update(visual, |this, _| {
        assert_eq!(this.saved_connections, before);
    });
}

#[gpui::test]
fn create_database_dialog_validates_the_name_and_cancel_starts_no_backend_work(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    // The clicks below land in the dialog as soon as it opens: no slide-in to miss them under load.
    visual.update(|_, cx| cx.set_reduce_motion(true));
    let path = std::env::temp_dir().join(format!(
        "tablex-menu-create-db-{}.json",
        uuid::Uuid::new_v4()
    ));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path.clone());
        this.seed_connection_tree_scenario();
        // Fake a live Postgres session so Create Database is capability-true
        // without touching any server.
        let id = crate::ui_fixture::POSTGRES_ID;
        let epoch = this.sessions.begin_connect(id).unwrap();
        this.sessions.finish_connect(id, epoch, Ok(()));
        fake_create_database_capability(this, id);
        assert!(this.can_create_database(id));
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.open_create_database_dialog(crate::ui_fixture::POSTGRES_ID, window, cx);
        });
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("dialog-layer").is_some(),
        "create dialog is mounted"
    );
    // An empty name and a padded one are refused where they are typed; the dialog stays.
    for typed in ["", " padded "] {
        if !typed.is_empty() {
            oa_type(visual, "object-name-input", typed);
        }
        let submit = visual.debug_bounds("object-name-submit").expect("the submit button");
        visual.simulate_click(submit.center(), Default::default());
        visual.run_until_parked();
        assert!(visual.debug_bounds("object-name-error").is_some(), "{typed:?} is refused");
        assert!(visual.debug_bounds("object-name-input").is_some(), "the dialog stays");
    }
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert!(this.object_action.is_none(), "cancel reviews and sends nothing");
        assert_eq!(this.diagram.applies.len(), 0);
    });
    std::fs::remove_file(path).unwrap_or(());
}

/// A driver that can create databases, as the connect flow would cache it.
fn fake_create_database_capability(app: &mut AppRoot, connection: &str) {
    let capabilities = serde_json::from_value(serde_json::json!({
        "abi_version": 2,
        "driver": "postgresql",
        "display_name": "PostgreSQL",
        "uri_schemes": ["postgres"],
        "features": ["database.create"],
        "editors": [],
        "navigator_kinds": [],
        "single_connection_pool": false,
    }))
    .expect("capabilities parse");
    app.driver_runtime
        .capabilities
        .insert(connection.into(), std::sync::Arc::new(capabilities));
}

#[gpui::test]
fn saved_history_production_path_request_save_open_on_temp_project_root(cx: &mut TestAppContext) {
    // Full production async chain on isolated storage: background request,
    // save from an active tab, and scoped open — nothing executed, nothing
    // outside the injected project root.
    cx.dispatcher.allow_parking();
    let home = TempSqlProject::new();
    let (app, visual) = sidebar_test_app(cx);
    let id = crate::ui_fixture::POSTGRES_ID;
    let general = home
        .dir
        .join("Library/Application Support/tablex/Projects/General");
    let seeded = crate::saved_sql::save_sql(&general, id, "warehouse", "Orders", "SELECT 1", 42.0)
        .expect("fixture SQL seeds the temp General root");
    app.update(visual, |this, cx| {
        this.saved_sql_ui.set_source(
            home.dir.join("Library/Application Support/tablex/Projects"),
            false,
        );
        this.request_saved_query_history(id, cx)
            .expect("production history request dispatches");
    });
    wait_until(visual, "history request", |visual| {
        app.read_with(visual, |this, _| {
            let (loading, entries, error) = this.saved_query_history(id);
            !loading && error.is_none() && entries.is_some()
        })
    });
    app.update(visual, |this, _| {
        let (_, entries, _) = this.saved_query_history(id);
        assert_eq!(entries.unwrap(), std::slice::from_ref(&seeded));
    });
    app.update(visual, |this, _| {
        this.workspace.open_query_tab_with_metadata(
            "Query",
            crate::workspace::TabMetadata {
                connection_id: id.into(),
                database: "warehouse".into(),
                ..Default::default()
            },
        );
        this.workspace.active_tab_mut().unwrap().sql = "SELECT 2".into();
    });
    app.update(visual, |this, cx| {
        this.save_active_sql_as("Second", cx)
            .expect("production save dispatches");
    });
    wait_until(visual, "save plus history refresh", |visual| {
        app.read_with(visual, |this, _| {
            let (loading, entries, _) = this.saved_query_history(id);
            !loading && entries.is_some_and(|entries| entries.len() == 2)
        })
    });
    let second = app.update(visual, |this, _| {
        let (_, entries, error) = this.saved_query_history(id);
        assert!(error.is_none());
        let entries = entries.expect("history refreshed after save");
        assert_eq!(entries.len(), 2);
        entries
            .iter()
            .find(|entry| entry.title == "Second")
            .expect("saved script is listed")
            .clone()
    });
    app.update(visual, |this, cx| {
        this.open_saved_sql_entry(&second, id, cx)
            .expect("production open dispatches");
    });
    wait_until(visual, "saved open", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace
                .active_tab()
                .is_some_and(|tab| tab.sql == "SELECT 2")
        })
    });
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("saved SQL opened a tab");
        assert_eq!(tab.sql, "SELECT 2");
        assert_eq!(tab.connection_id, id);
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
}

#[gpui::test]
fn mongo_script_history_saves_js_and_opens_without_execution(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let project = TempSqlProject::new();
    let (app, visual) = sidebar_test_app(cx);
    let connection = crate::ui_fixture::MONGO_ID;
    app.update(visual, |this, cx| {
        this.saved_sql_ui.set_source(project.dir.clone(), false);
        this.seed_connection_tree_scenario();
        let node = NavigatorNodeId::Connection {
            connection: connection.into(),
        };
        assert!(this.new_query_for_sidebar_node(&node, cx));
        let tab = this.workspace.active_tab_mut().unwrap();
        tab.database = "warehouse".into();
        tab.sql = "db.orders.find({ active: true })".into();
        this.save_active_sql_as("Mongo orders", cx).unwrap();
    });
    wait_until(visual, "Mongo script saved and listed", |visual| {
        app.read_with(visual, |this, _| {
            this.saved_query_history(connection)
                .1
                .is_some_and(|entries| entries.len() == 1)
        })
    });
    let entry = app.read_with(visual, |this, _| {
        this.saved_query_history(connection).1.unwrap()[0].clone()
    });
    assert_eq!(entry.dialect, "mongoScript");
    assert_eq!(
        entry.path.extension().and_then(|ext| ext.to_str()),
        Some("js")
    );
    app.update(visual, |this, cx| {
        this.open_saved_sql_entry(&entry, connection, cx).unwrap()
    });
    wait_until(visual, "Mongo script file opened", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.active_tab().is_some_and(|tab| {
                tab.query_mode.is_mongo_script() && tab.title == "Mongo orders"
            })
        })
    });
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.sql, "db.orders.find({ active: true })");
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
}

#[gpui::test]
fn mongo_menu_history_opens_saved_js_from_same_menu_without_running(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let project = TempSqlProject::new();
    let (app, visual) = sidebar_test_app(cx);
    let id = crate::ui_fixture::MONGO_ID;
    crate::saved_sql::save_script(
        &project.dir.join("General"),
        id,
        "warehouse",
        "Recent Mongo",
        "db.items.find({})",
        8.0,
    )
    .unwrap();
    app.update(visual, |this, cx| {
        this.saved_sql_ui.set_source(project.dir.clone(), false);
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let index = connection_row_index(&app, visual, "Mongo fixture");
    right_click_row(visual, index);
    wait_until(visual, "Mongo script history loaded", |visual| {
        app.read_with(visual, |this, _| {
            this.saved_query_history(id)
                .1
                .is_some_and(|entries| entries.len() == 1)
        })
    });
    for _ in 0..5 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("right");
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    wait_until(visual, "Mongo script history opened", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.active_tab().is_some_and(|tab| {
                tab.title == "Recent Mongo" && tab.query_mode.is_mongo_script()
            })
        })
    });
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.sql, "db.items.find({})");
        assert!(matches!(
            this.workspace.runtime(tab.id).unwrap().query.status(),
            crate::query::QueryStatus::Idle
        ));
    });
}

#[gpui::test]
fn open_menu_history_populates_and_opens_file_without_reopening_menu(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let project = TempSqlProject::new();
    let (app, visual) = sidebar_test_app(cx);
    let id = crate::ui_fixture::POSTGRES_ID;
    let general = project.dir.join("General");
    crate::saved_sql::save_sql(&general, id, "warehouse", "Orders", "SELECT 42", 42.0).unwrap();
    app.update(visual, |this, cx| {
        this.saved_sql_ui.set_source(project.dir.clone(), false);
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let index = connection_row_index(&app, visual, "Postgres fixture");
    right_click_row(visual, index);
    wait_until(visual, "history menu completion", |visual| {
        app.read_with(visual, |this, _| {
            this.saved_query_history(id)
                .1
                .is_some_and(|entries| entries.len() == 1)
        })
    });
    for _ in 0..5 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("right");
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    wait_until(visual, "history menu file open", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace
                .active_tab()
                .is_some_and(|tab| tab.sql == "SELECT 42")
        })
    });
    assert_eq!(
        app.read_with(visual, |this, _| this
            .workspace
            .active_tab()
            .unwrap()
            .database
            .clone()),
        "warehouse"
    );
}

fn offline_filter_harness(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    path: std::path::PathBuf,
) {
    offline_filter_harness_with(app, visual, path, vec!["warehouse".into(), "analytics".into(), "admin".into()]);
}

/// [`offline_filter_harness`] over the databases `names`.
fn offline_filter_harness_with(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    path: std::path::PathBuf,
    names: Vec<String>,
) {
    // Generation without a live engine session: the dialog's enumeration
    // request fails closed before touching engine worker threads (forbidden
    // in tests) while the epoch still authorizes seeded enumeration + apply.
    visual.update(|_, cx| cx.set_reduce_motion(true));
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(path);
        this.seed_connection_tree_scenario();
        let id = crate::ui_fixture::POSTGRES_ID;
        let epoch = this.sessions.begin_connect(id).unwrap();
        this.sessions
            .finish_connect(id, epoch, Err("fixture offline".into()));
        let (token, _) = this.filter_enumeration.begin(id, epoch);
        this.filter_enumeration.finish(id, token, epoch, Ok(names));
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
}

fn draft_selection(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> (bool, Vec<String>, bool) {
    let draft = app.read_with(visual, |this, _| {
        this.visible_filter_dialog
            .as_ref()
            .expect("filter dialog state exists")
            .1
            .clone()
    });
    draft.read_with(visual, |state, _| {
        let mut selected: Vec<String> = state.selected.iter().cloned().collect();
        selected.sort();
        (state.explicit, selected, state.show_system)
    })
}

#[test]
fn visible_filter_projection_reuses_one_immutable_100k_name_buffer() {
    let mut state = connection_menu_view::VisibleFilterDialogState::init(Default::default());
    state.sync_enumeration(
        (0..100_000).map(|i| format!("db{i}")).collect(),
        false,
        None,
    );
    let shared = state.available.clone();
    let shown = connection_menu_view::displayed_indices(&shared, "db", true);
    assert_eq!(shown.len(), 100_000);
    assert!(std::sync::Arc::ptr_eq(&shared, &state.available));
    assert_eq!(shared[shown[99_999]], "db99999");
}

#[test]
#[ignore = "allocation benchmark; run alone with --test-threads=1"]
fn visible_filter_100k_projection_allocation_budget() {
    let mut state = connection_menu_view::VisibleFilterDialogState::init(Default::default());
    state.sync_enumeration(
        (0..100_000).map(|i| format!("db{i}")).collect(),
        false,
        None,
    );
    let before = crate::grid_bench::allocation_count();
    let shared = state.available.clone();
    let indices = connection_menu_view::displayed_indices(&shared, "db", true);
    let projected = crate::grid_bench::allocation_count() - before;
    assert_eq!(indices.len(), 100_000);
    assert!(
        projected < 256,
        "projection cloned names: {projected} allocations"
    );
    let row_before = crate::grid_bench::allocation_count();
    let visible_rows = indices
        .iter()
        .take(25)
        .map(|&i| shared[i].clone())
        .collect::<Vec<_>>();
    let row_allocations = crate::grid_bench::allocation_count() - row_before;
    assert_eq!(visible_rows.len(), 25);
    assert!(
        row_allocations < 64,
        "rendered rows cloned full list: {row_allocations} allocations"
    );
}

#[test]
fn multiline_filter_patterns_keep_regex_commas_and_show_all_drops_schema_draft() {
    let patterns = connection_menu_view::parse_filter_patterns("^db[0-9]{1,3}$\n^(sales,archive)$");
    assert_eq!(patterns, ["^db[0-9]{1,3}$", "^(sales,archive)$"]);
    let mut prefs = crate::sidebar_preferences::SidebarPreferences::default();
    prefs
        .set_visible_filter(
            "conn",
            crate::sidebar_preferences::VisibleObjectFilter {
                databases: Some(Vec::new()),
                patterns,
                ..Default::default()
            },
        )
        .unwrap();
    let restored: crate::sidebar_preferences::SidebarPreferences =
        serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
    assert_eq!(
        restored.visible_filters["conn"].patterns,
        ["^db[0-9]{1,3}$", "^(sales,archive)$"]
    );
    let mut draft = connection_menu_view::VisibleFilterDialogState::init(
        crate::sidebar_preferences::VisibleObjectFilter {
            schemas: std::collections::HashMap::from([(
                "warehouse".into(),
                Some(vec!["public".into()]),
            )]),
            ..Default::default()
        },
    );
    draft.show_all();
    assert!(draft.schemas.is_empty());
}

#[test]
fn schema_draft_scopes_selections_per_database_and_show_all_resets() {
    let mut draft = connection_menu_view::VisibleFilterDialogState::init(Default::default());
    draft.schema_database = Some("warehouse".into());
    draft.sync_schemas(vec!["public".into(), "private".into()], false, None);
    draft.set_schema_checked("private", false);
    assert_eq!(draft.schemas["warehouse"], Some(vec!["public".into()]));
    draft.schema_database = Some("analytics".into());
    draft.sync_schemas(vec!["main".into()], false, None);
    draft.set_schema_checked("main", false);
    assert_eq!(draft.schemas["analytics"], Some(vec![]));
    assert_eq!(draft.schemas["warehouse"], Some(vec!["public".into()]));
    draft.show_all();
    assert!(draft.schemas.is_empty());
}

#[gpui::test]
fn schema_select_event_targets_configured_database_and_reports_unavailable_session(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    let path =
        std::env::temp_dir().join(format!("tablex-schema-menu-{}.json", uuid::Uuid::new_v4()));
    offline_filter_harness(&app, visual, path.clone());
    open_filter_dialog(&app, visual, crate::ui_fixture::POSTGRES_ID);
    let select = app.read_with(visual, |this, _| {
        this.visible_filter_schema_select
            .as_ref()
            .unwrap()
            .1
            .clone()
    });
    select.update(visual, |_, cx| {
        cx.emit(gpui_component::select::SelectEvent::<Vec<String>>::Confirm(
            Some("warehouse".into()),
        ))
    });
    visual.run_until_parked();
    let draft = app.read_with(visual, |this, _| {
        this.visible_filter_dialog.as_ref().unwrap().1.clone()
    });
    draft.read_with(visual, |state, _| {
        assert_eq!(state.schema_database.as_deref(), Some("warehouse"));
        assert!(!state.schema_loading);
        assert!(state.schema_error.is_some());
    });
    visual.simulate_keystrokes("escape");
    std::fs::remove_file(path).ok();
}

/// The wheel over a list of the Visible Object Filter dialog, in a window the dialog does not fit:
/// the dialog body is a `Scrollable`, whose `ScrollBounce` then takes every wheel event. The list
/// goes first; the dialog takes the wheel once the list cannot move.
fn filter_wheel_over_a_list(
    visual: &mut gpui::VisualTestContext,
    (list, first, last): (&str, String, String),
    dialog_probe: &str,
) {
    let bounds = |visual: &mut gpui::VisualTestContext, selector: &str| {
        visual.debug_bounds(d3_sel(selector.to_string())).unwrap_or_else(|| panic!("{selector} is drawn"))
    };
    let from_start = |visual: &mut gpui::VisualTestContext| bounds(visual, list).origin.y - bounds(visual, &first).origin.y;
    let to_end = |visual: &mut gpui::VisualTestContext| bounds(visual, list).bottom() - bounds(visual, &last).bottom();
    let dialog = |visual: &mut gpui::VisualTestContext| bounds(visual, dialog_probe).origin.y;
    let wheel = |visual: &mut gpui::VisualTestContext, dy: f32| {
        let list = bounds(visual, list);
        visual.simulate_event(gpui::ScrollEvent {
            position: gpui::point(list.center().x, list.origin.y + gpui::px(30.0)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(dy))),
            ..Default::default()
        });
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    };
    let window = visual.update(|window, _| window.viewport_size());
    assert!(bounds(visual, list).origin.y + gpui::px(30.0) < window.height, "the list is in view");
    let start_dialog = dialog(visual);
    assert!(from_start(visual).abs() <= gpui::px(1.0), "the list starts at its first row");

    // Less than a row, so that the first row is still drawn to measure by.
    wheel(visual, -10.0);
    assert!(from_start(visual) > gpui::px(5.0), "the list scrolled");
    assert_eq!(dialog(visual), start_dialog, "and the dialog stayed where it was");

    wheel(visual, -100_000.0);
    assert!(to_end(visual).abs() <= gpui::px(3.0), "the list went to its end: {:?}", to_end(visual));
    assert_eq!(dialog(visual), start_dialog, "the dialog still did not move");
    wheel(visual, -10.0);
    assert!(to_end(visual).abs() <= gpui::px(3.0), "the list is at its end");
    assert!(dialog(visual) < start_dialog, "so the dialog scrolls");
}

#[gpui::test]
fn the_wheel_scrolls_the_filter_database_list_before_the_dialog_around_it(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-menu-visible-filter-{}.json", uuid::Uuid::new_v4()));
    let names: Vec<String> = (0..20).map(|n| format!("db{n:02}")).collect();
    offline_filter_harness_with(&app, visual, path.clone(), names);
    visual.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(480.0)));
    open_filter_dialog(&app, visual, crate::ui_fixture::POSTGRES_ID);
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    filter_wheel_over_a_list(
        visual,
        ("visible-filter-list", "visible-filter-row-db00".into(), "visible-filter-row-db19".into()),
        "visible-filter-select-all",
    );
    std::fs::remove_file(path).ok();
}

#[gpui::test]
fn the_wheel_scrolls_the_filter_schema_list_before_the_dialog_around_it(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!("tablex-menu-visible-filter-{}.json", uuid::Uuid::new_v4()));
    offline_filter_harness(&app, visual, path.clone());
    visual.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(700.0)));
    open_filter_dialog(&app, visual, crate::ui_fixture::POSTGRES_ID);
    let draft = app.read_with(visual, |this, _| this.visible_filter_dialog.as_ref().unwrap().1.clone());
    draft.update(visual, |state, cx| {
        state.schema_database = Some("warehouse".into());
        state.sync_schemas((0..20).map(|n| format!("schema{n:02}")).collect(), false, None);
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    filter_wheel_over_a_list(
        visual,
        ("visible-schema-list", "visible-schema-row-schema00".into(), "visible-schema-row-schema19".into()),
        "visible-filter-select-all",
    );
    std::fs::remove_file(path).ok();
}

#[gpui::test]
fn visible_filter_dialog_apply_persists_cancel_discards_and_reopens(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-menu-visible-filter-{}.json",
        uuid::Uuid::new_v4()
    ));
    offline_filter_harness(&app, visual, path.clone());
    let id = crate::ui_fixture::POSTGRES_ID;
    open_filter_dialog(&app, visual, id);
    // Draft starts at show-all (no persisted filter): everything checked.
    assert_eq!(
        draft_selection(&app, visual),
        (false, Vec::new(), true),
        "fresh dialog drafts show-all"
    );
    // Uncheck one name through a real row click (native event path into
    // the draft), then Apply through the real button.
    click_debug_button(visual, "visible-filter-row-warehouse");
    assert_eq!(
        draft_selection(&app, visual),
        (
            true,
            vec!["admin".to_string(), "analytics".to_string()],
            true
        ),
        "row click toggles the draft without persisting"
    );
    click_debug_button(visual, "visible-filter-apply");
    let persisted =
        crate::sidebar_preferences::SidebarPreferences::load(&path).expect("prefs persisted");
    assert_eq!(
        persisted.visible_filters[id].databases,
        Some(vec!["admin".to_string(), "analytics".to_string()]),
        "Apply persists the draft selection"
    );
    // Reopen: the draft reflects the persisted selection.
    open_filter_dialog(&app, visual, id);
    assert_eq!(
        draft_selection(&app, visual),
        (
            true,
            vec!["admin".to_string(), "analytics".to_string()],
            true
        ),
        "reopened draft matches persisted filter"
    );
    // Change the draft, then Cancel via Escape: prefs must not move.
    app.update(visual, |this, cx| {
        let available = this.visible_filter_names(id).unwrap().to_vec();
        let (_, draft) = this.visible_filter_dialog.as_ref().unwrap();
        draft.update(cx, |state, cx| {
            state.set_checked("analytics", false, &available);
            cx.notify();
        });
    });
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    assert!(
        visual.debug_bounds("dialog-layer").is_none(),
        "Escape cancels the filter dialog"
    );
    let restored =
        crate::sidebar_preferences::SidebarPreferences::load(&path).expect("prefs reloaded");
    assert_eq!(
        restored.visible_filters[id].databases,
        Some(vec!["admin".to_string(), "analytics".to_string()]),
        "Cancel persists nothing"
    );
    std::fs::remove_file(path).unwrap_or(());
}

#[gpui::test]
fn visible_filter_dialog_empty_vs_all_and_async_names_without_reopen(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let path = std::env::temp_dir().join(format!(
        "tablex-menu-visible-empty-{}.json",
        uuid::Uuid::new_v4()
    ));
    offline_filter_harness(&app, visual, path.clone());
    let id = crate::ui_fixture::POSTGRES_ID;
    // Async completion without reopen: leave enumeration pending, open the
    // dialog (Loading), then finish the pending run as the server would.
    // The dialog's failed-closed request leaves the cache intact.
    let token = app.update(visual, |this, cx| {
        let epoch = this.sessions.generation(id).unwrap();
        let (token, _) = this.filter_enumeration.begin(id, epoch);
        cx.notify();
        token
    });
    open_filter_dialog(&app, visual, id);
    app.update(visual, |this, cx| {
        let epoch = this.sessions.generation(id).unwrap();
        assert!(
            this.filter_enumeration.finish(
                id,
                token,
                epoch,
                Ok(vec!["warehouse".into(), "analytics".into()])
            ),
            "pending enumeration completes"
        );
        // Same observation the async continuation performs on completion.
        this.sync_visible_filter_dialog(id, cx);
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    assert!(
        visual
            .debug_bounds("visible-filter-row-warehouse")
            .is_some(),
        "async enumeration lands in the open dialog without reopen"
    );
    // Clear -> Apply persists explicit empty (hide all); Show All -> Apply
    // persists show-all (None).
    app.update(visual, |this, cx| {
        let (_, draft) = this.visible_filter_dialog.as_ref().unwrap();
        draft.update(cx, |state, cx| {
            state.clear();
            cx.notify();
        });
    });
    click_debug_button(visual, "visible-filter-apply");
    let persisted =
        crate::sidebar_preferences::SidebarPreferences::load(&path).expect("prefs persisted");
    assert_eq!(
        persisted.visible_filters[id].databases,
        Some(Vec::new()),
        "Clear applies explicit empty, distinct from show-all"
    );
    open_filter_dialog(&app, visual, id);
    app.update(visual, |this, cx| {
        let (_, draft) = this.visible_filter_dialog.as_ref().unwrap();
        draft.update(cx, |state, cx| {
            state.show_all();
            cx.notify();
        });
    });
    click_debug_button(visual, "visible-filter-apply");
    let restored =
        crate::sidebar_preferences::SidebarPreferences::load(&path).expect("prefs reloaded");
    assert_eq!(
        restored.visible_filters[id].databases, None,
        "Show All applies show-all"
    );
    std::fs::remove_file(path).unwrap_or(());
}

#[gpui::test]
fn delete_connection_menu_item_requires_confirmation(cx: &mut TestAppContext) {
    // Scenario connections cannot be deleted; the menu must route through the
    // confirmation dialog, and Escape must leave every connection intact.
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, cx| {
        this.seed_connection_tree_scenario();
        cx.notify();
    });
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let count = app.read_with(visual, |this, _| this.saved_connections.len());
    let index = connection_row_index(&app, visual, "Mongo fixture");
    right_click_row(visual, index);
    // Last clickable item is Delete Connection; dismiss instead of confirming.
    for _ in 0..10 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    app.update(visual, |this, _| {
        assert_eq!(this.saved_connections.len(), count);
        assert!(this.sidebar.notice.is_none());
    });
    wait_until(visual, "delete-menu history prefetch", |visual| {
        app.read_with(visual, |this, _| {
            !this.saved_query_history(crate::ui_fixture::MONGO_ID).0
        })
    });
}

#[gpui::test]
#[ignore = "requires scripts/test-mongo-driver-ephemeral.sh with an owned loopback mongod"]
fn ephemeral_mongo_filter_hides_server_database_without_deleting_cached_node(
    cx: &mut TestAppContext,
) {
    let uri = std::env::var("TABLEX_EPHEMERAL_MONGO_URI").expect("owned ephemeral URI required");
    let owner = std::env::var("TABLEX_EPHEMERAL_MONGO_OWNER").expect("owned marker required");
    let pidfile = std::env::var("TABLEX_EPHEMERAL_MONGO_PIDFILE").expect("owned pidfile required");
    let driver = std::path::PathBuf::from(
        std::env::var_os("TABLEX_EPHEMERAL_MONGO_DRIVER").expect("isolated driver path required"),
    );
    assert!(owner.starts_with("ephem_") && owner.len() >= 14 && owner.len() < 40);
    assert!(
        uri.starts_with("mongodb://127.0.0.1:")
            && uri.contains("directConnection=true")
            && !uri.contains('@')
    );
    assert!(std::path::Path::new(&pidfile).exists());
    assert!(driver.canonicalize().unwrap().starts_with(
        std::path::Path::new(&pidfile)
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
    ));
    let database = format!("tablex_{owner}");
    let sibling_databases = ["service-b", "service-c", "service-d"]
        .map(|suffix| format!("tx_{}_{}", owner.trim_start_matches("ephem_"), suffix));
    let sibling_collections = ["items_b", "items_c", "items_d"];
    let project = TempSqlProject::new();
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    let connection = crate::ui_fixture::MONGO_ID;
    app.update(visual, |this, cx| {
        this.scenario_ui.service = None;
        this.sidebar.path = Some(project.dir.join("prefs.json"));
        let handle = engine_driver_host::DriverLoader.load(&driver).unwrap();
        assert!(handle.capabilities().has(Capability::QueryMongoScript));
        let session = handle
            .connect(&serde_json::json!({"uri":uri}).to_string())
            .unwrap();
        let engine_id = session.connection_id();
        let session = std::sync::Arc::new(engine_driver_host::SharedDriverSession::new(session));
        let epoch = this.sessions.begin_connect(connection).unwrap();
        this.sessions.finish_connect(connection, epoch, Ok(()));
        this.driver_runtime
            .sessions
            .insert(connection.into(), session);
        this.driver_runtime
            .connection_ids
            .insert(connection.into(), engine_id);
        this.driver_ui.handles.insert("mongodb".into(), handle);
        let session = this.driver_runtime.sessions.get(connection).unwrap();
        let mut session = session.lock().unwrap();
        for (index, sibling) in sibling_databases.iter().enumerate() {
            session
                .mutate(&engine_driver_api::MutationRequest {
                    operation_id: engine_driver_api::OperationId(600 + index as u64),
                    kind: "create_database".into(),
                    database: Some(sibling.clone()),
                    object: None,
                    identity: serde_json::Value::Null,
                    payload: serde_json::Value::Null,
                })
                .unwrap();
            session
                .mutate(&engine_driver_api::MutationRequest {
                    operation_id: engine_driver_api::OperationId(700 + index as u64),
                    kind: "insert".into(),
                    database: Some(sibling.clone()),
                    object: Some(sibling_collections[index].into()),
                    identity: serde_json::Value::Null,
                    payload: serde_json::json!({"owner": owner}),
                })
                .unwrap();
        }
        drop(session);
        this.navigator.enable_provider(connection);
        let root = NavigatorNodeId::Connection {
            connection: connection.into(),
        };
        let request = this.navigator.toggle_expansion(&root).request.unwrap();
        this.dispatch_provider_load(request, cx);
        this.request_visible_filter_names(connection, cx).unwrap();
    });
    wait_until(visual, "owned MongoDB namespace enumeration", |visual| {
        app.read_with(visual, |this, _| {
            let node = NavigatorNodeId::Provider {
                connection: connection.into(),
                node: database.clone(),
            };
            this.visible_filter_names(connection)
                .is_some_and(|names| names.contains(&database))
                && this.navigator.provider_node(&node).is_some()
        })
    });
    app.update(visual, |this, cx| {
        let node = NavigatorNodeId::Provider {
            connection: connection.into(),
            node: database.clone(),
        };
        assert!(this.navigator.visible_index(&node).is_some());
        this.apply_visible_filter(
            connection,
            crate::sidebar_preferences::VisibleObjectFilter {
                databases: Some(vec![]),
                ..Default::default()
            },
            cx,
        )
        .unwrap();
        assert!(this.navigator.visible_index(&node).is_none());
        assert!(this.navigator.provider_node(&node).is_some());
        this.show_all_visible_objects(connection, cx).unwrap();
        assert!(this.navigator.visible_index(&node).is_some());
        let session = this
            .driver_runtime
            .sessions
            .get(connection)
            .unwrap()
            .clone();
        session
            .lock()
            .unwrap()
            .mutate(&engine_driver_api::MutationRequest {
                operation_id: engine_driver_api::OperationId(500),
                kind: "insert".into(),
                database: Some(database.clone()),
                object: Some("orders".into()),
                identity: serde_json::Value::Null,
                payload: serde_json::json!({"active":true,"value":43}),
            })
            .unwrap();
        let root = NavigatorNodeId::Connection {
            connection: connection.into(),
        };
        assert!(this.new_query_for_sidebar_node(&root, cx));
        let tab_id = this.workspace.active_tab().unwrap().id;
        let tab = this.workspace.tab_mut(tab_id).unwrap();
        tab.database = database.clone();
        tab.sql = "db.orders.find({ active: true }).limit(2)".into();
        tab.output_limit = 1;
        this.workspace.runtime_mut(tab_id).unwrap().pagination =
            crate::query::PaginationState::new(1);
        this.run_active_tab_query(cx);
    });
    wait_until(visual, "owned MongoDB script first page", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.active_tab().is_some_and(|tab| {
                matches!(
                    this.workspace.runtime(tab.id).unwrap().query.status(),
                    crate::query::QueryStatus::Ready { rows: 1 }
                ) && this
                    .workspace
                    .runtime(tab.id)
                    .unwrap()
                    .pagination
                    .exact_total_count()
                    == Some(2)
            })
        })
    });
    app.update(visual, |this, cx| this.next_query_page(cx));
    wait_until(visual, "owned MongoDB script second page", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.active_tab().is_some_and(|tab| {
                this.workspace.runtime(tab.id).unwrap().pagination.page() == 1
                    && matches!(
                        this.workspace.runtime(tab.id).unwrap().query.status(),
                        crate::query::QueryStatus::Ready { rows: 1 }
                    )
            })
        })
    });
    app.update(visual, |this, cx| {
        for sibling in &sibling_databases {
            let node = NavigatorNodeId::Provider {
                connection: connection.into(),
                node: sibling.clone(),
            };
            let index = this
                .navigator
                .visible_index(&node)
                .expect("sibling database row");
            assert_eq!(
                this.navigator.visible_row_at(index).unwrap().label,
                *sibling
            );
            let request = this
                .navigator
                .toggle_expansion(&node)
                .request
                .expect("sibling database load request");
            this.dispatch_provider_load(request, cx);
        }
        for sibling in &sibling_databases {
            let node = NavigatorNodeId::Provider {
                connection: connection.into(),
                node: sibling.clone(),
            };
            let row = this
                .navigator
                .visible_rows()
                .into_iter()
                .find(|row| row.id == node)
                .expect("expanded sibling database row");
            assert_eq!(row.load, Some(crate::navigator::NodeLoadState::Loading));
        }
    });
    let siblings_started = std::time::Instant::now();
    loop {
        visual.run_until_parked();
        let terminal = app.read_with(visual, |this, _| {
            sibling_databases.iter().all(|sibling| {
                let node = NavigatorNodeId::Provider {
                    connection: connection.into(),
                    node: sibling.clone(),
                };
                this.navigator
                    .visible_index(&node)
                    .and_then(|index| this.navigator.visible_row_at(index))
                    .is_some_and(|row| {
                        !matches!(row.load, Some(crate::navigator::NodeLoadState::Loading))
                    })
            })
        });
        if terminal {
            break;
        }
        assert!(
            siblings_started.elapsed() < std::time::Duration::from_secs(10),
            "sibling database loads remained Loading"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    app.read_with(visual, |this, _| {
        for (sibling, collection) in sibling_databases.iter().zip(sibling_collections) {
            let parent = NavigatorNodeId::Provider {
                connection: connection.into(),
                node: sibling.clone(),
            };
            let index = this.navigator.visible_index(&parent).unwrap();
            let row = this.navigator.visible_row_at(index).unwrap();
            assert_eq!(
                row.load,
                Some(crate::navigator::NodeLoadState::Loaded),
                "failed to list collections for synthetic database {sibling}"
            );
            assert!(
                this.navigator
                    .find_provider_child(&parent, "collection", sibling, Some(collection))
                    .is_some(),
                "loaded children: {:?}",
                this.navigator
                    .visible_rows()
                    .iter()
                    .map(|row| (&row.label, &row.id, row.depth))
                    .collect::<Vec<_>>()
            );
        }
    });
    app.update(visual, |this, cx| {
        let session = this
            .driver_runtime
            .sessions
            .get(connection)
            .unwrap()
            .clone();
        let mut session = session.lock().unwrap();
        for seq in 0..750 {
            session
                .mutate(&engine_driver_api::MutationRequest {
                    operation_id: engine_driver_api::OperationId(1_000 + seq),
                    kind: "insert".into(),
                    database: Some(database.clone()),
                    object: Some("orders".into()),
                    identity: serde_json::Value::Null,
                    payload: serde_json::json!({"batch":"owned","seq":seq}),
                })
                .unwrap();
        }
        drop(session);
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.tab_mut(tab_id).unwrap().sql =
            "db.orders.find({ batch: 'owned' }).sort({ seq: 1 })".into();
        this.workspace.tab_mut(tab_id).unwrap().output_limit = 500;
        this.workspace.runtime_mut(tab_id).unwrap().pagination =
            crate::query::PaginationState::new(500);
        this.run_active_tab_query(cx);
    });
    for requested in [500, 1000] {
        if requested == 1000 {
            app.update(visual, |this, cx| {
                let tab_id = this.workspace.active_tab().unwrap().id;
                this.workspace.tab_mut(tab_id).unwrap().output_limit = 1000;
                this.workspace.runtime_mut(tab_id).unwrap().pagination =
                    crate::query::PaginationState::new(1000);
                this.run_active_tab_query(cx);
            });
        }
        let mut observed = Vec::new();
        for page in 0..4 {
            wait_until(
                visual,
                "owned MongoDB bounded 750-document page",
                |visual| {
                    app.read_with(visual, |this, _| {
                        let tab_id = this.workspace.active_tab().unwrap().id;
                        let runtime = this.workspace.runtime(tab_id).unwrap();
                        runtime.pagination.page() == page
                            && matches!(
                                runtime.query.status(),
                                crate::query::QueryStatus::Ready { .. }
                            )
                    })
                },
            );
            let values = app.read_with(visual, |this, _| {
                let tab_id = this.workspace.active_tab().unwrap().id;
                let runtime = this.workspace.runtime(tab_id).unwrap();
                assert_eq!(runtime.pagination.page_size(), 200);
                assert_eq!(this.workspace.active_tab().unwrap().output_limit, 200);
                let col = (0..this.active_grid().column_count())
                    .find(|&column| this.active_grid().column_name(column) == Some("seq"))
                    .unwrap();
                (0..this.active_grid().row_count())
                    .map(|row| {
                        this.active_grid()
                            .cell_text(crate::grid::CellPosition { row, column: col })
                            .unwrap()
                            .to_string()
                            .parse::<usize>()
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            });
            observed.extend(values);
            if page < 3 {
                app.update(visual, |this, cx| this.next_query_page(cx));
            }
        }
        assert_eq!(
            observed,
            (0..750).collect::<Vec<_>>(),
            "requested limit {requested} omitted or repeated a document"
        );
    }
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.workspace.tab_mut(tab_id).unwrap().sql =
            "db.orders.find({ batch: 'owned' }).sort({ seq: 1 }).limit(300)".into();
        this.workspace.tab_mut(tab_id).unwrap().output_limit = 1000;
        this.workspace.runtime_mut(tab_id).unwrap().pagination =
            crate::query::PaginationState::new(1000);
        this.run_active_tab_query(cx);
    });
    for (page, count) in [(0, 200), (1, 100)] {
        wait_until(visual, "owned MongoDB script-limit page", |visual| {
            app.read_with(visual, |this, _| {
                let runtime = this
                    .workspace
                    .runtime(this.workspace.active_tab().unwrap().id)
                    .unwrap();
                runtime.pagination.page() == page
                    && matches!(
                        runtime.query.status(),
                        crate::query::QueryStatus::Ready { .. }
                    )
            })
        });
        app.read_with(visual, |this, _| {
            let runtime = this
                .workspace
                .runtime(this.workspace.active_tab().unwrap().id)
                .unwrap();
            assert_eq!(this.active_grid().row_count(), count);
            assert_eq!(runtime.pagination.exact_total_count(), Some(300));
        });
        if page == 0 {
            app.update(visual, |this, cx| this.next_query_page(cx));
        }
    }
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        this.run_active_tab_query(cx);
        this.cancel_tab_query(tab_id, cx);
        assert!(matches!(
            this.workspace.runtime(tab_id).unwrap().query.status(),
            crate::query::QueryStatus::Cancelled
        ));
    });
    visual.run_until_parked();
    app.update(visual, |this, cx| {
        let tab_id = this.workspace.active_tab().unwrap().id;
        assert!(matches!(
            this.workspace.runtime(tab_id).unwrap().query.status(),
            crate::query::QueryStatus::Cancelled
        ));
        this.close_tab(tab_id, cx);
        this.reset_active_view();
        this.cancel_visible_filter_names(connection, cx);
        let session = this.driver_runtime.sessions.remove(connection).unwrap();
        session.lock().unwrap().disconnect().unwrap();
        this.driver_runtime.connection_ids.remove(connection);
        this.sessions.disconnect(connection);
        let handle = this.driver_ui.handles.remove("mongodb").unwrap();
        handle.shutdown().unwrap();
    });
}
