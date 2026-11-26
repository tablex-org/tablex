// Real-interaction proofs for the Mongo New Query toolbar: buttons, the
// editor selection / cursor, native file dialogs, and the connection /
// database pickers with "Set Default".

fn script_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = mongo_admin_app(cx);
    app.update(visual, |this, _| {
        // The default scenario refuses script runs; these tests opt in.
        this.scenario_ui
            .service
            .as_mut()
            .unwrap()
            .fixture_mut()
            .set_mongo_scripts_enabled(true);
    });
    right_click_label(&app, visual, "fixture_collection_00");
    click_debug_button(visual, "mongo-menu-new-query");
    settle(visual);
    (app, visual)
}

fn script_text(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> String {
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.query_editors.by_tab[&tab].read(cx).value().to_string()
    })
}

/// Replaces the editor text with real keystrokes (the editor is below the toolbar).
fn script_type(visual: &mut gpui::VisualTestContext, text: &str) {
    let toolbar = visual.debug_bounds("mongo-script-toolbar").expect("toolbar");
    // The editor fills the strip under the toolbar.
    visual.simulate_click(
        gpui::point(toolbar.center().x, toolbar.bottom() + gpui::px(40.0)),
        Default::default(),
    );
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

fn script_rows(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| this.active_grid().row_count())
}

#[gpui::test]
fn toolbar_renders_every_control_and_replaces_the_title_bar_run(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    for selector in [
        "mongo-script-toolbar",
        "run-query",
        "script-run-selected",
        "script-format",
        "script-open",
        "script-save-file",
        "save-sql-script",
        "script-export",
        "script-connection",
        "script-database",
        "script-set-default",
    ] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} renders");
    }
    // The whole toolbar stays on one row at the real default layout (1100pt
    // window, ~260pt sidebar => ~840pt toolbar; this harness's sidebar is
    // 180pt, hence 1020) and when narrower, where the pickers shrink instead.
    for width in [1020.0, 900.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
        settle(visual);
        let toolbar = visual.debug_bounds("mongo-script-toolbar").unwrap();
        let run = visual.debug_bounds("run-query").unwrap();
        for selector in ["script-export", "script-connection", "script-database", "script-set-default"] {
            let control = visual.debug_bounds(selector).unwrap();
            assert!(
                (control.center().y - run.center().y).abs() < gpui::px(2.0),
                "{selector} wrapped off the Run row at {width}: {control:?} vs Run {run:?}"
            );
            assert!(control.right() <= toolbar.right(), "{selector} clipped at {width}: {control:?} in {toolbar:?}");
        }
    }
    // One Run only: the title bar no longer has its own for script tabs.
    let runs = visual.debug_bounds("run-query").unwrap();
    let toolbar = visual.debug_bounds("mongo-script-toolbar").unwrap();
    assert!(toolbar.contains(&runs.center()), "Run lives in the toolbar");
    let _ = app;
}

#[gpui::test]
fn run_runs_the_whole_script_and_run_selected_runs_the_statement_or_selection(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    click_debug_button(visual, "run-query");
    settle(visual);
    assert_eq!(script_rows(&app, visual), 200, "the template runs: first page of the collection");

    script_type(
        visual,
        "db.fixture_collection_00.find({ active: true });\ndb.fixture_collection_00.find({ active: false }).limit(3)",
    );
    // Cursor is at the end: the current statement is the second one.
    click_debug_button(visual, "script-run-selected");
    settle(visual);
    assert_eq!(script_rows(&app, visual), 3, "the statement at the cursor ran, not the whole text");
    // Select the first line with the keyboard: that selection runs.
    visual.simulate_keystrokes("up");
    visual.simulate_keystrokes("home");
    visual.simulate_keystrokes("shift-end");
    click_debug_button(visual, "script-run-selected");
    settle(visual);
    assert_eq!(script_rows(&app, visual), 120, "the selected statement ran");
    // Run with "Run executes" on the whole script: a two-statement script is no single find but
    // a MongoDB shell script, which needs the plugin (this catalog has none), so the dialog says
    // so and nothing runs. That proves Run Selected was what made the difference.
    app.update(visual, |this, _| {
        let mut settings = (**this.settings).clone();
        settings.editor.run_scope = crate::settings::RunScope::All;
        *this.settings = std::sync::Arc::new(settings);
    });
    click_debug_button(visual, "run-query");
    settle(visual);
    assert!(
        visual.debug_bounds("shell-prompt-body").is_some(),
        "the shell script asks about the plugin"
    );
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        assert!(!matches!(
            this.workspace.runtime(tab).unwrap().query.status(),
            crate::query::QueryStatus::Running { .. }
        ));
    });
}

#[gpui::test]
fn format_reindents_the_script_and_reports_what_it_cannot_do(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    script_type(visual, "db.fixture_collection_00.find({b:1,a:{ $gt : 2 }}).sort({ zeta:-1, alpha:1 })");
    click_debug_button(visual, "script-format");
    assert_eq!(
        script_text(&app, visual),
        "db.fixture_collection_00.find({ b: 1, a: { $gt: 2 } }).sort({ zeta: -1, alpha: 1 })"
    );
    assert_eq!(
        app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().sql.clone()),
        script_text(&app, visual),
        "the tab text follows"
    );
    click_debug_button(visual, "script-format");
    assert!(app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.script_toolbar.notice(tab) == Some("Already formatted")
    }));
    script_type(visual, "db.fixture_collection_00.find({ x: function() { return 1 } })");
    let before = script_text(&app, visual);
    click_debug_button(visual, "script-format");
    assert_eq!(script_text(&app, visual), before, "unparsable text is left alone");
    assert!(visual.debug_bounds("script-notice").is_some());
}

#[gpui::test]
fn open_and_save_use_the_native_file_dialogs(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    let dir = std::env::temp_dir().join(format!("tablex-script-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("orders.js");
    std::fs::write(&input, "db.fixture_collection_00.find({ active: true })\n").unwrap();

    click_debug_button(visual, "script-open");
    assert!(visual.did_prompt_for_paths(), "Open asks the platform for a file");
    visual.simulate_path_prompt_response(|options| {
        assert!(options.files && !options.multiple);
        Some(vec![input.clone()])
    });
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert_eq!(script_text(&app, visual), "db.fixture_collection_00.find({ active: true })\n");
    assert!(visual.debug_bounds("script-notice").is_some());
    // Cancelling the dialog changes nothing.
    click_debug_button(visual, "script-open");
    visual.simulate_path_prompt_response(|_| None);
    settle(visual);
    assert_eq!(script_text(&app, visual), "db.fixture_collection_00.find({ active: true })\n");

    // Save As… writes the editor text where the user chose.
    let output = dir.join("saved.js");
    click_debug_button(visual, "script-save-file");
    assert!(visual.did_prompt_for_new_path());
    visual.simulate_new_path_selection(|_| Some(output.clone()));
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "db.fixture_collection_00.find({ active: true })\n"
    );
    // An unreadable file is reported, not swallowed.
    click_debug_button(visual, "script-open");
    let missing = dir.join("missing.js");
    visual.simulate_path_prompt_response(|_| Some(vec![missing.clone()]));
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert!(app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.script_toolbar.notice(tab).is_some_and(|notice| notice.starts_with("Could not read"))
    }));
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn export_dropdown_is_disabled_without_a_result_and_lists_formats_with_one(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    let trigger = visual.debug_bounds("script-export").unwrap();
    visual.simulate_click(trigger.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("script-export-csv").is_none(), "nothing to export yet");
    click_debug_button(visual, "run-query");
    settle(visual);
    assert!(script_rows(&app, visual) > 0);
    visual.simulate_click(trigger.center(), Default::default());
    settle(visual);
    for selector in ["script-export-csv", "script-export-json", "script-export-xlsx"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} is offered");
    }
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("script-export-csv").is_none(), "Escape dismisses the menu");
}

#[gpui::test]
fn connection_and_database_pickers_retarget_the_tab_and_set_default_uses_the_preference(
    cx: &mut TestAppContext,
) {
    let (app, visual) = script_app(cx);
    let (connection, database) = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.connection_id.clone(), tab.database.clone())
    });
    assert_eq!((connection.as_str(), database.as_str()), (crate::ui_fixture::MONGO_ID, "fixture_db_0"));
    // The database Select lists the connection's databases.
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        let select = this.script_toolbar.database_select(tab).unwrap();
        assert_eq!(select.read(cx).selected_value().map(String::as_str), Some("fixture_db_0"));
    });
    // Pick another database through the real Select.
    click_debug_button(visual, "script-database");
    settle(visual);
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let picked = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().database.clone());
    assert_ne!(picked, "fixture_db_0");
    assert!(picked.starts_with("fixture_db_"), "{picked}");

    // Set Default stores it in the sidebar preference (persisted); clicking
    // again clears it.
    let path = std::env::temp_dir().join(format!("tablex-script-default-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, _| this.sidebar.path = Some(path.clone()));
    click_debug_button(visual, "script-set-default");
    assert_eq!(
        app.read_with(visual, |this, _| this.mongo_default_database(crate::ui_fixture::MONGO_ID)),
        Some(picked.clone())
    );
    let stored = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert_eq!(stored.default_database(crate::ui_fixture::MONGO_ID), Some(picked.as_str()));
    settle(visual);
    click_debug_button(visual, "script-set-default");
    assert_eq!(
        app.read_with(visual, |this, _| this.mongo_default_database(crate::ui_fixture::MONGO_ID)),
        None
    );
    let _ = std::fs::remove_file(path);
}

const SECOND_MONGO: &str = "44444444-4444-4444-8444-444444444444";

fn add_second_connection(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    app.update(visual, |this, cx| {
        let saved = this
            .scenario_ui
            .service
            .as_mut()
            .unwrap()
            .fixture_mut()
            .add_mongo_connection(SECOND_MONGO, "Mongo second");
        this.saved_connections.push(saved);
        cx.notify();
    });
    settle(visual);
}

#[gpui::test]
fn switching_the_connection_releases_the_old_page_through_the_old_connection_and_clears_the_result(
    cx: &mut TestAppContext,
) {
    let (app, visual) = script_app(cx);
    add_second_connection(&app, visual);
    click_debug_button(visual, "run-query");
    settle(visual);
    let (tab, page) = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        (tab, this.workspace.runtime(tab).unwrap().driver_page_id)
    });
    let page = page.expect("the run holds a driver page");
    assert_eq!(script_rows(&app, visual), 200);

    // Real pick in the connection Select: items are [Mongo fixture, Mongo second].
    app.read_with(visual, |this, cx| {
        let select = this.script_toolbar.connection_select(tab).unwrap();
        assert_eq!(select.read(cx).selected_value().map(String::as_str), Some("Mongo fixture"));
    });
    click_debug_button(visual, "script-connection");
    settle(visual);
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, cx| {
        let tab_state = this.workspace.tab(tab).unwrap();
        assert_eq!(tab_state.connection_id, SECOND_MONGO, "the tab moved to the second connection");
        assert_eq!(tab_state.database, "", "no default database on the new connection");
        let runtime = this.workspace.runtime(tab).unwrap();
        assert_eq!(runtime.driver_page_id, None, "the old page id must not follow the tab to the new connection");
        assert_eq!(runtime.pagination.page(), 0);
        assert!(matches!(runtime.query.status(), crate::query::QueryStatus::Idle));
        assert_eq!(
            this.driver_runtime.released_pages,
            vec![(crate::ui_fixture::MONGO_ID.to_string(), page)],
            "released through the connection that owns the page"
        );
        let select = this.script_toolbar.connection_select(tab).unwrap();
        assert_eq!(select.read(cx).selected_value().map(String::as_str), Some("Mongo second"));
    });
    assert_eq!(script_rows(&app, visual), 0, "the old connection's rows are gone");

    // Picking the same connection again changes nothing and releases nothing more.
    app.update(visual, |this, cx| this.retarget_script_connection(tab, SECOND_MONGO, cx));
    app.read_with(visual, |this, _| assert_eq!(this.driver_runtime.released_pages.len(), 1));
}

#[gpui::test]
fn switching_the_database_keeps_the_page_but_restarts_paging(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    click_debug_button(visual, "run-query");
    settle(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    app.update(visual, |this, cx| this.set_output_limit(tab, 50, cx));
    settle(visual);
    click_debug_button(visual, "page-next");
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.runtime(tab).unwrap().pagination.page(), 1);
    });
    let page = app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().driver_page_id);
    click_debug_button(visual, "script-database");
    settle(visual);
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        assert_ne!(this.workspace.tab(tab).unwrap().database, "fixture_db_0");
        assert_eq!(runtime.pagination.page(), 0, "the page offset belonged to the old database");
        assert_eq!(runtime.driver_page_id, page, "same connection: the page is still its own");
        assert!(this.driver_runtime.released_pages.is_empty(), "nothing to release on a database switch");
    });
}

#[gpui::test]
fn a_failed_database_listing_is_remembered_with_its_error_and_retried_only_on_demand(
    cx: &mut TestAppContext,
) {
    let (app, visual) = script_app(cx);
    add_second_connection(&app, visual);
    app.update(visual, |this, cx| {
        this.scenario_ui.service.as_mut().unwrap().fixture_mut().set_catalog_failing(true);
        this.load_script_databases(SECOND_MONGO, cx);
    });
    app.read_with(visual, |this, _| {
        assert!(this.script_toolbar.database_error(SECOND_MONGO).is_some(), "the error is kept");
        assert!(!this.script_toolbar.has_database_list(SECOND_MONGO), "not cached as an empty list");
    });
    // Recovery of the server alone does not retry: no hidden polling.
    app.update(visual, |this, cx| {
        this.scenario_ui.service.as_mut().unwrap().fixture_mut().set_catalog_failing(false);
        this.load_script_databases(SECOND_MONGO, cx);
        this.load_script_databases(SECOND_MONGO, cx);
    });
    app.read_with(visual, |this, _| assert!(!this.script_toolbar.has_database_list(SECOND_MONGO)));
    // The user's retry loads it.
    app.update(visual, |this, cx| this.retry_script_databases(SECOND_MONGO, cx));
    app.read_with(visual, |this, _| {
        assert!(this.script_toolbar.database_error(SECOND_MONGO).is_none());
        assert!(this.script_toolbar.has_database_list(SECOND_MONGO));
    });
}

/// The byte ranges the active Mongo script editor paints as SQL (adjacent runs merged).
fn sql_painted_ranges(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> Vec<std::ops::Range<usize>> {
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for range in this.query_editors.sql_highlight[&tab].collection.get_ranges(cx) {
            match merged.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => merged.push(range),
            }
        }
        merged
    })
}

#[gpui::test]
fn a_sql_highlight_computed_for_older_text_or_a_closed_tab_is_not_applied(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    script_type(visual, "SELECT 1 FROM a");
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    let paint = |this: &mut AppRoot, generation: u64, cx: &mut Context<AppRoot>| {
        let style = gpui::HighlightStyle::default();
        this.apply_sql_highlight(tab, generation, vec![(0..6, style)], cx);
    };
    // The result of a run that started before the last edit is dropped...
    app.update(visual, |this, cx| {
        let started = this.query_editors.sql_highlight[&tab].generation;
        this.schedule_sql_highlight(tab, cx);
        paint(this, started, cx);
    });
    assert!(sql_painted_ranges(&app, visual).is_empty(), "stale result painted");
    // ...the current one is applied...
    app.update(visual, |this, cx| {
        let current = this.query_editors.sql_highlight[&tab].generation;
        paint(this, current, cx);
    });
    assert_eq!(sql_painted_ranges(&app, visual), vec![0..6]);
    // ...and one that lands after the tab closed finds nothing to paint (and does not panic).
    app.update(visual, |this, cx| {
        let current = this.query_editors.sql_highlight[&tab].generation;
        this.query_editors.by_tab.remove(&tab);
        paint(this, current, cx);
    });
}

#[gpui::test]
fn sql_statements_of_a_mongo_script_are_painted_as_sql_and_follow_edits(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    assert!(sql_painted_ranges(&app, visual).is_empty(), "an empty script has no SQL");

    script_type(visual, "use shop;\nSELECT status FROM orders;\nshow dbs");
    // Debounced: nothing is recomputed per keystroke.
    assert!(sql_painted_ranges(&app, visual).is_empty());
    visual.cx.executor().advance_clock(Duration::from_millis(200));
    settle(visual);
    let text = script_text(&app, visual);
    let sql = text.find("SELECT").unwrap()..text.find("orders").unwrap() + "orders".len();
    assert_eq!(sql_painted_ranges(&app, visual), vec![sql]);

    // A second SQL statement is picked up after the next edit; the shell lines stay unpainted.
    script_type(visual, "SELECT 1 FROM shop;\nshow dbs;\n\nselect status from orders");
    visual.cx.executor().advance_clock(Duration::from_millis(200));
    settle(visual);
    let text = script_text(&app, visual);
    let first = 0.."SELECT 1 FROM shop".len();
    let second = text.find("select").unwrap()..text.len();
    assert_eq!(sql_painted_ranges(&app, visual), vec![first, second]);

    // Back to shell only: the paint goes away.
    script_type(visual, "show dbs");
    visual.cx.executor().advance_clock(Duration::from_millis(200));
    settle(visual);
    assert!(sql_painted_ranges(&app, visual).is_empty());
}

/// What the picker draws for `text`: the theme's font at 0.875 of the rem size, and its chrome.
fn picker_text_and_chrome(visual: &mut gpui::VisualTestContext, text: &str) -> (f32, f32) {
    visual.update(|window, cx| {
        let run = gpui::TextRun {
            len: text.len(),
            font: gpui::font(gpui_component::theme::Theme::global(cx).font_family.clone()),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let size = window.rem_size() * 0.875;
        let width = f32::from(window.text_system().layout_line(text, size, &[run], None).width);
        (width, 2.0 + 16.0 + 4.0 + f32::from(size))
    })
}

#[gpui::test]
fn a_connection_or_database_name_that_fits_the_picker_is_shown_whole(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    let names = [
        "Mongo fixture",
        "Mongo analytics replica (eu-west)",
        // Longer than the picker's most: it is cut there.
        "A connection name that is far longer than any picker is meant to be wide",
    ];
    for name in names {
        app.update(visual, |this, cx| {
            let tab = this.workspace.active_tab().unwrap().connection_id.clone();
            let saved = this.saved_connections.iter_mut().find(|saved| saved.matches_id(&tab)).unwrap();
            saved.name = name.to_string();
            cx.notify();
        });
        settle(visual);
        settle(visual);
        let (text, chrome) = picker_text_and_chrome(visual, name);
        let picker = visual.debug_bounds("script-connection").expect("connection picker");
        let expected = (text + chrome).clamp(120.0, 240.0);
        assert!(
            (f32::from(picker.size.width) - expected).abs() < 1.0,
            "{name:?}: the picker is {:?} wide, its name needs {expected} (text {text} + chrome {chrome})",
            picker.size.width
        );
        if text + chrome <= 240.0 {
            assert!(f32::from(picker.size.width) >= text + chrome - 0.5, "{name:?} fits whole: room for its {text} + {chrome}");
        }
        // The two pickers and the Set Default button stay on the row, inside the toolbar.
        let toolbar = visual.debug_bounds("mongo-script-toolbar").unwrap();
        for selector in ["script-connection", "script-database", "script-set-default"] {
            let control = visual.debug_bounds(selector).unwrap();
            assert!(control.right() <= toolbar.right() + gpui::px(1.0), "{selector} inside the toolbar for {name:?}");
        }
    }
    // The database picker is as wide as its name too (up to 360).
    let database = "a_database_with_quite_a_long_name";
    app.update(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.choose_script_database(tab, database.to_string(), cx);
    });
    settle(visual);
    let (text, chrome) = picker_text_and_chrome(visual, database);
    let picker = visual.debug_bounds("script-database").unwrap();
    assert!(f32::from(picker.size.width) >= (text + chrome).min(360.0) - 0.5, "database name whole: {picker:?} for {text}+{chrome}");
}
