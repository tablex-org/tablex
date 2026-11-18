// Real-interaction proofs for the status line under the query editors (cursor, selection, language,
// the Vim switch and badge, the bound file) and for the fit of the SQL editor's header in a narrow panel.

fn status_of(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> editor_status::StatusText {
    app.read_with(visual, |this, cx| {
        let editor = match this.query_editors.by_tab.get(&tab) {
            Some(editor) => editor.clone(),
            None => this.aggregate_ui.tab(tab).and_then(|ui| ui.text_editor.clone()).expect("an editor"),
        };
        let language = editor_status::EditorLanguage::of(this.workspace.tab(tab).unwrap().query_mode.as_ref());
        this.editor_status_text(tab, &editor, language, cx)
    })
}

fn status_bounds(visual: &mut gpui::VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"))
}

fn new_sql_tab(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    let (app, visual) = layout_app(cx);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let tab = active_tab(&app, visual);
    (app, visual, tab)
}

#[gpui::test]
fn the_status_line_sits_under_the_editor_and_follows_the_cursor_and_selection(cx: &mut TestAppContext) {
    let (app, visual, tab) = new_sql_tab(cx);
    let area = status_bounds(visual, "query-editor-area");
    let status = status_bounds(visual, "editor-status");
    let header = status_bounds(visual, "editor-header");
    assert!((status.size.height - gpui::px(theme::EDITOR_STATUS_HEIGHT)).abs() < gpui::px(1.0), "thin: {status:?}");
    // The editor box has a 1px border around the code and the status line.
    assert!((status.bottom() - area.bottom()).abs() < gpui::px(2.0), "at the bottom of the editor area: {status:?} {area:?}");
    assert!(
        (status.left() - area.left()).abs() < gpui::px(2.0) && (status.size.width - area.size.width).abs() <= gpui::px(2.0),
        "as wide as the editor"
    );
    assert!(header.bottom() <= status.top(), "the header is above, the status line below");
    assert_eq!(status_of(&app, visual, tab).position, "Ln 1, Col 1");
    assert!(visual.debug_bounds("editor-status-selection").is_none(), "nothing selected");

    type_in_editor(visual, "select 1");
    assert_eq!(status_of(&app, visual, tab).position, "Ln 1, Col 9");
    let short = status_bounds(visual, "editor-status-position").size.width;
    visual.simulate_keystrokes("enter");
    visual.simulate_input("from t");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).position, "Ln 2, Col 7");
    // What is drawn follows the text: three digits make the label wider.
    visual.simulate_input(&"x".repeat(100));
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).position, "Ln 2, Col 107");
    let long = status_bounds(visual, "editor-status-position").size.width;
    assert!(long > short, "the label was redrawn with the new column: {short:?} -> {long:?}");
    visual.simulate_keystrokes("home");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).position, "Ln 2, Col 1");
    assert!(status_bounds(visual, "editor-status-position").size.width < long, "and shrinks back");
    visual.simulate_keystrokes("up");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).position, "Ln 1, Col 1");

    // A selection: its size shows, and goes with the selection.
    visual.simulate_keystrokes("shift-right");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).selection.as_deref(), Some("Selected: 1 char"));
    visual.simulate_keystrokes("shift-right shift-right");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).selection.as_deref(), Some("Selected: 3 chars"));
    assert_eq!(status_of(&app, visual, tab).position, "Ln 1, Col 4", "the cursor is the moving end");
    assert!(visual.debug_bounds("editor-status-selection").is_some());
    visual.simulate_keystrokes("secondary-a");
    settle(visual);
    let all = status_of(&app, visual, tab).selection.expect("everything is selected");
    assert!(all.starts_with("Selected: 2 lines, ") && all.ends_with(" chars"), "{all}");
    visual.simulate_keystrokes("right");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).selection, None);
    assert!(visual.debug_bounds("editor-status-selection").is_none(), "the label is gone with the selection");
    // The same words in Vietnamese.
    crate::i18n::set(crate::i18n::Locale::Vi);
    visual.simulate_keystrokes("shift-left shift-left");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).selection.as_deref(), Some("\u{110}ang ch\u{1ecd}n: 2 k\u{fd} t\u{1ef1}"));
    crate::i18n::set(crate::i18n::Locale::En);
    // Both ends stay inside the status line however wide the labels are.
    for selector in ["editor-status-position", "editor-status-language"] {
        let label = status_bounds(visual, selector);
        assert!(label.left() >= status.left() && label.right() <= status.right(), "{selector} inside the status line");
    }
}

#[gpui::test]
fn the_vim_switch_moved_to_the_status_line_and_the_badge_shows_only_while_vim_is_on(cx: &mut TestAppContext) {
    let (app, visual, tab) = new_sql_tab(cx);
    let status = status_bounds(visual, "editor-status");
    let header = status_bounds(visual, "editor-header");
    let toggle = status_bounds(visual, "vim-toggle");
    assert!(status.contains(&toggle.center()), "the switch is in the status line");
    assert!(!header.contains(&toggle.center()), "and not in the header any more");
    assert!(visual.debug_bounds("vim-mode").is_none(), "no badge while Vim is off");

    // The control fits the 20 px status line whole.
    assert!(toggle.top() >= status.top() && toggle.bottom() <= status.bottom(), "inside the status line: {toggle:?} in {status:?}");
    assert!((status.size.height - gpui::px(20.0)).abs() < gpui::px(1.0), "the status line stays 20 px: {status:?}");
    // The label toggles, and so does the switch itself (once, not twice).
    visual.simulate_click(gpui::point(toggle.right() - gpui::px(6.0), toggle.center().y), Default::default());
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.vim_on(tab)), "a click on the label turned Vim on");
    visual.simulate_click(gpui::point(toggle.left() + gpui::px(10.0), toggle.center().y), Default::default());
    settle(visual);
    assert!(!app.read_with(visual, |this, _| this.vim_on(tab)), "a click on the switch turned it off, once");
    click_debug_button(visual, "vim-toggle");
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.vim_on(tab)), "the click turned Vim on");
    let badge = status_bounds(visual, "vim-mode");
    assert!(status.contains(&badge.center()), "the badge is in the status line");
    assert!(badge.left() >= status_bounds(visual, "vim-toggle").right(), "right of the switch");
    let colors: Vec<_> = [crate::vim::Mode::Normal, crate::vim::Mode::Insert, crate::vim::Mode::Visual, crate::vim::Mode::VisualLine]
        .map(editor_status::mode_color)
        .to_vec();
    assert_eq!(colors.len(), 4);
    // The badge follows the mode, in real keystrokes.
    vim_press(visual, "i");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Insert);
    assert!(visual.debug_bounds("vim-mode").is_some());
    visual.simulate_keystrokes("escape");
    vim_press(visual, "V");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::VisualLine);
    assert!(visual.debug_bounds("vim-mode").is_some());

    click_debug_button(visual, "vim-toggle");
    settle(visual);
    assert!(!app.read_with(visual, |this, _| this.vim_on(tab)));
    assert!(visual.debug_bounds("vim-mode").is_none(), "the badge goes with Vim");
    assert!(visual.debug_bounds("vim-toggle").is_some(), "the switch stays");
}

#[gpui::test]
fn the_toggle_vim_mode_command_flips_the_active_editors_vim_and_its_badge(cx: &mut TestAppContext) {
    let (app, visual, tab) = new_sql_tab(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    visual.update(|window, _| window.activate_window());
    assert!(!app.read_with(visual, |this, _| this.vim_on(tab)));
    for turned_on in [true, false] {
        palette_open(visual);
        palette_type(visual, "toggle vim");
        visual.simulate_keystrokes("enter");
        settle(visual);
        assert_eq!(app.read_with(visual, |this, _| this.vim_on(tab)), turned_on, "the command flips Vim");
        assert_eq!(visual.debug_bounds("vim-mode").is_some(), turned_on, "the badge follows");
    }
    assert!(crate::app::shortcuts::find("editor.toggle_vim").is_some_and(|def| def.default_chords().is_empty()), "listed, with no default key");
}

#[gpui::test]
fn the_status_line_names_the_language_of_each_kind_of_editor(cx: &mut TestAppContext) {
    // SQL.
    let (app, visual, sql) = new_sql_tab(cx);
    assert_eq!(status_of(&app, visual, sql).language, "SQL");
    let sql_width = status_bounds(visual, "editor-status-language").size.width;
    assert!(visual.debug_bounds("vim-toggle").is_some());
    // MongoDB script.
    open_mongo_script_tab_in_default_scenario(&app, visual);
    let script = active_tab(&app, visual);
    assert_ne!(script, sql);
    assert_eq!(status_of(&app, visual, script).language, "MongoDB Script");
    assert!(status_bounds(visual, "editor-status-language").size.width > sql_width, "the longer name is drawn");
    assert!(status_bounds(visual, "editor-status").bottom() <= status_bounds(visual, "query-editor-area").bottom() + gpui::px(1.0));
    assert!(visual.debug_bounds("vim-toggle").is_some());
    // Typing there moves the cursor shown.
    script_type(visual, "db.a.find()");
    assert_eq!(status_of(&app, visual, script).position, "Ln 1, Col 12");
}

#[gpui::test]
fn the_redis_console_has_a_status_line_with_the_vim_switch(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    app.update(visual, |this, cx| assert!(this.new_query_for_sidebar_node(&fixture_db_node(), cx)));
    settle(visual);
    let console = active_tab(&app, visual);
    assert_eq!(status_of(&app, visual, console).language, "Redis");
    assert!(status_bounds(visual, "editor-status").bottom() <= status_bounds(visual, "query-editor-area").bottom() + gpui::px(1.0));
    assert!(visual.debug_bounds("vim-toggle").is_some(), "the console has the switch");
    type_in_editor(visual, "GET user:1");
    assert_eq!(status_of(&app, visual, console).position, "Ln 1, Col 11");
}

#[gpui::test]
fn the_aggregation_editor_has_a_status_line_without_vim(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    let tab = active_tab(&app, visual);
    assert_eq!(status_of(&app, visual, tab).language, "Aggregation");
    let status = status_bounds(visual, "editor-status");
    let editor = status_bounds(visual, "agg-text-editor");
    assert!(status.top() >= editor.bottom() - gpui::px(1.0), "under the pipeline editor");
    assert!(visual.debug_bounds("vim-toggle").is_none(), "the pipeline editor has no Vim keys");
    agg_type_pipeline(visual, "[{\"$match\": {}}]");
    assert_eq!(status_of(&app, visual, tab).position, "Ln 1, Col 17");
}

#[gpui::test]
fn the_status_line_names_the_bound_file_and_marks_unsaved_changes(cx: &mut TestAppContext) {
    let (app, visual, tab) = new_sql_tab(cx);
    assert_eq!(status_of(&app, visual, tab).file, None);
    assert!(visual.debug_bounds("editor-status-file").is_none());
    type_in_editor(visual, "select 1");
    app.update(visual, |this, cx| {
        let tab = this.workspace.tab_mut(tab).unwrap();
        tab.file = Some(crate::workspace::SqlFile { path: "queries/orders.sql".into(), saved: Arc::from("select 1") });
        cx.notify();
    });
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).file.as_deref(), Some("orders.sql"));
    assert!(visual.debug_bounds("editor-status-file").is_some());
    // Typing makes it unsaved: a dot after the name.
    visual.simulate_input(" + 1");
    settle(visual);
    assert_eq!(status_of(&app, visual, tab).file.as_deref(), Some("orders.sql \u{25CF}"));
}

#[gpui::test]
fn the_status_line_is_inside_the_editor_and_lighter_than_the_results_header(cx: &mut TestAppContext) {
    let (_app, visual, _tab) = new_sql_tab(cx);
    type_in_editor(visual, "select 1");
    click_debug_button(visual, "run-query");
    settle(visual);
    let area = status_bounds(visual, "query-editor-area");
    let status = status_bounds(visual, "editor-status");
    let header = status_bounds(visual, "results-header");
    assert!(
        area.contains(&status.origin) && area.contains(&gpui::point(status.right() - gpui::px(1.0), status.bottom() - gpui::px(1.0))),
        "inside the editor box: {status:?} in {area:?}"
    );
    assert!(header.top() >= area.bottom() - gpui::px(1.0), "the Results header is outside, below");
    assert!(status.size.height < header.size.height, "lighter than the header: {status:?} vs {header:?}");
}

#[gpui::test]
fn the_bottom_bars_are_told_apart_and_the_results_row_is_inside_the_results(cx: &mut TestAppContext) {
    let (_app, visual, _tab) = new_sql_tab(cx);
    type_in_editor(visual, "select 1");
    click_debug_button(visual, "run-query");
    settle(visual);
    let footer = status_bounds(visual, "result-action-footer");
    let row = status_bounds(visual, "result-status-row");
    let bar = status_bounds(visual, "status-bar");
    assert!(footer.contains(&row.origin) && row.bottom() <= footer.bottom() + gpui::px(0.5) && row.right() <= footer.right() + gpui::px(0.5), "the row is inside the results: {row:?} in {footer:?}");
    assert!(row.size.height <= gpui::px(22.0) + gpui::px(0.5), "compact: {row:?}");
    assert!(row.bottom() <= bar.top() + gpui::px(1.0), "above the app status bar");
    // The app status bar is its own tone, not the panel's.
    assert_ne!(theme::selected(), theme::background());
    assert_ne!(theme::selected(), theme::chrome());
    for selector in ["status-language", "error-log-toggle"] {
        assert!(bar.contains(&status_bounds(visual, selector).center()), "{selector} is in the status bar");
    }
}

// ---- the header of a SQL editor in a narrow panel -------------------------------------------

/// A SQL tab on the Postgres fixture (it has the commit mode and Explain), run once.
fn sql_header_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
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
    let tab = active_tab(&app, visual);
    type_in_editor(visual, "select 1 as one");
    click_debug_button(visual, "run-query");
    settle(visual);
    (app, visual, tab)
}

/// Picks a commit mode the way a user does: opens the mode button's menu and clicks its item.
fn pick_commit_mode(visual: &mut gpui::VisualTestContext, item: &'static str) {
    click_debug_button(visual, "tx-mode");
    settle(visual);
    click_debug_button(visual, item);
    settle(visual);
}

const HEADER_CONTROLS: [&str; 9] = [
    "tx-mode",
    "tx-state",
    "run-query",
    "explain-dropdown",
    "query-history",
    "run-sql-file",
    "save-sql-script",
    "editor-more",
    "editor-header",
];

/// Every control of the SQL editor's header is inside the header (which is inside the editor's panel),
/// and none reaches the Results header.
fn assert_header_fits(visual: &mut gpui::VisualTestContext, what: &str) {
    let panel = status_bounds(visual, "query-editor-area");
    let header = status_bounds(visual, "editor-header");
    assert!(header.left() >= panel.left() - gpui::px(1.0) && header.right() <= panel.right() + gpui::px(1.0), "{what}: header inside its panel: {header:?} vs {panel:?}");
    let results = visual.debug_bounds("results-header");
    for selector in HEADER_CONTROLS {
        let Some(control) = visual.debug_bounds(selector) else {
            continue;
        };
        assert!(
            control.left() >= header.left() - gpui::px(1.0) && control.right() <= header.right() + gpui::px(1.0),
            "{what}: {selector} is outside the header: {control:?} vs {header:?}"
        );
        assert!(control.top() >= header.top() - gpui::px(1.0) && control.bottom() <= header.bottom() + gpui::px(1.0), "{what}: {selector} left the header row");
        if let Some(results) = results {
            let overlaps = control.left() < results.right() && control.right() > results.left() && control.top() < results.bottom() && control.bottom() > results.top();
            assert!(!overlaps, "{what}: {selector} overlaps the Results header: {control:?} vs {results:?}");
        }
    }
    assert!(visual.debug_bounds("run-query").is_some(), "{what}: Run is always there");
}

#[gpui::test]
fn the_sql_editor_header_never_leaves_its_panel_beside_the_results_or_below_them(cx: &mut TestAppContext) {
    let (app, visual, tab) = sql_header_app(cx);
    assert!(visual.debug_bounds("tx-mode").is_some(), "the fixture connection has the commit mode");
    assert!(visual.debug_bounds("explain-dropdown").is_some(), "and Explain");
    pick_results_place(visual, "right");
    app.read_with(visual, |this, _| {
        assert_eq!(this.results_place(tab), crate::app::results::placement::ResultsPlace::Right)
    });
    // The window sizes of the real use: the default 1100 (the harness sidebar is 180 wide, hence 1180
    // for the same panel) and narrower; the panel is half of what is left of the sidebar.
    let mut seen_full = false;
    let mut seen_collapsed = false;
    for width in [1600.0, 1180.0, 1100.0, 1000.0, 900.0, 800.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
        settle(visual);
        settle(visual);
        let what = format!("Right layout at {width}");
        assert_header_fits(visual, &what);
        let panel = status_bounds(visual, "query-editor-area");
        let results = status_bounds(visual, "results-header");
        assert!(results.left() >= panel.right() - gpui::px(1.0), "{what}: side by side");
        seen_full |= visual.debug_bounds("save-sql-script").is_some();
        seen_collapsed |= visual.debug_bounds("editor-more").is_some();
        // Collapsed means Explain, History, Run SQL file and Save are in the menu, not drawn.
        if visual.debug_bounds("editor-more").is_some() {
            for gone in ["explain-dropdown", "query-history", "run-sql-file", "save-sql-script"] {
                assert!(visual.debug_bounds(gone).is_none(), "{what}: {gone} moved into the menu");
            }
        } else {
            for kept in ["explain-dropdown", "query-history", "run-sql-file", "save-sql-script"] {
                assert!(visual.debug_bounds(kept).is_some(), "{what}: {kept} fits, so it is drawn");
            }
        }
    }
    assert!(seen_full && seen_collapsed, "the sizes cover both a full and a collapsed header");
    // Back below the editor the header has the whole width again.
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    pick_results_place(visual, "below");
    assert_header_fits(visual, "Below layout");
    assert!(visual.debug_bounds("editor-more").is_none(), "the whole width fits everything");
    for kept in ["explain-dropdown", "query-history", "run-sql-file", "save-sql-script"] {
        assert!(visual.debug_bounds(kept).is_some(), "{kept} is drawn below");
    }
}

/// While a statement runs the Run button reads Cancel, which is wider: the header is measured again for it,
/// and again when the statement ends (a header that collapsed for "Cancel" must not stay collapsed for "Run").
#[gpui::test]
fn the_header_is_measured_again_when_run_becomes_cancel_and_back(cx: &mut TestAppContext) {
    let (app, visual, tab) = sql_header_app(cx);
    pick_results_place(visual, "right");
    let settle_all = |visual: &mut gpui::VisualTestContext| (0..3).for_each(|_| settle(visual));
    let set_running = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, running: bool| {
        app.update(visual, |this, cx| {
            let query = &mut this.workspace.runtime_mut(tab).unwrap().query;
            if running {
                assert!(query.begin().is_some());
            } else {
                query.cancel();
            }
            cx.notify();
        });
        settle_all(visual);
    };
    // A width where "Run" fits in full and "Cancel" does not.
    let mut found = None;
    for width in (780..=1180).rev().step_by(10) {
        visual.simulate_resize(gpui::size(gpui::px(width as f32), gpui::px(850.0)));
        settle_all(visual);
        if visual.debug_bounds("editor-more").is_some() {
            continue;
        }
        set_running(&app, visual, true);
        let collapsed = visual.debug_bounds("editor-more").is_some();
        assert_header_fits(visual, &format!("running at {width}"));
        set_running(&app, visual, false);
        if collapsed {
            found = Some(width);
            break;
        }
    }
    let width = found.expect("a width that holds the header with Run but not with Cancel");
    assert!(visual.debug_bounds("editor-more").is_none(), "at {width} the idle header is whole");
    set_running(&app, visual, true);
    assert!(visual.debug_bounds("editor-more").is_some(), "Cancel collapses it");
    set_running(&app, visual, false);
    assert!(visual.debug_bounds("editor-more").is_none(), "Run is whole again");
    assert_header_fits(visual, "idle again");
}

#[gpui::test]
fn the_collapsed_header_menu_holds_the_buttons_that_do_not_fit(cx: &mut TestAppContext) {
    let (_app, visual, _tab) = sql_header_app(cx);
    pick_results_place(visual, "right");
    visual.simulate_resize(gpui::size(gpui::px(820.0), gpui::px(850.0)));
    settle(visual);
    settle(visual);
    assert_header_fits(visual, "narrow");
    click_debug_button(visual, "editor-more");
    settle(visual);
    let more = status_bounds(visual, "editor-more");
    for item in ["more-explain", "more-explain-analyze", "more-history", "more-run-sql-file", "more-save"] {
        let item_bounds = status_bounds(visual, item);
        assert!(item_bounds.top() >= more.bottom() - gpui::px(1.0), "{item} opens below the button");
        assert!(item_bounds.right() <= visual.update(|window, _| window.viewport_size().width), "{item} is on screen");
    }
    // Picking an entry does what the button did: Save opens the save prompt.
    assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));
    click_debug_button(visual, "more-save");
    settle(visual);
    assert!(visual.debug_bounds("more-save").is_none(), "the menu closed");
    assert!(visual.update(|window, cx| window.has_active_dialog(cx)), "Save from the menu asked for a name");
}

#[gpui::test]
fn the_commit_mode_button_stays_in_the_header_at_every_width_in_both_modes(cx: &mut TestAppContext) {
    let (_app, visual, _tab) = sql_header_app(cx);
    pick_results_place(visual, "right");
    for manual in [false, true] {
        if manual {
            pick_commit_mode(visual, "tx-mode-manual");
            settle(visual);
            assert!(visual.debug_bounds("tx-status").is_some(), "Manual shows the transaction state");
        }
        for width in [1700.0, 1400.0, 1180.0, 1100.0, 1000.0, 900.0, 800.0] {
            visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
            settle(visual);
            settle(visual);
            let what = format!("manual={manual} at {width}");
            assert_header_fits(visual, &what);
            let header = status_bounds(visual, "editor-header");
            let mode = status_bounds(visual, "tx-mode");
            let run = status_bounds(visual, "run-query");
            assert!(mode.size.width > gpui::px(60.0), "{what}: the commit-mode button keeps its size: {mode:?}");
            assert!(run.size.width > gpui::px(40.0), "{what}: Run keeps its size: {run:?}");
            assert!(mode.right() <= run.left(), "{what}: the switch is left of Run");
            assert!(header.contains(&mode.center()) && header.contains(&run.center()), "{what}");
            // Collapsed, the commit mode's own buttons may be clipped: the menu has them too.
            if manual && visual.debug_bounds("editor-more").is_some() {
                click_debug_button(visual, "editor-more");
                settle(visual);
                for item in ["more-commit", "more-rollback"] {
                    assert!(visual.debug_bounds(item).is_some(), "{what}: {item} is in the menu");
                }
                visual.simulate_keystrokes("escape");
                settle(visual);
                assert!(visual.debug_bounds("more-commit").is_none(), "{what}: Escape closes the menu");
            }
        }
    }
}

#[gpui::test]
fn the_commit_mode_menu_switches_between_auto_and_manual_and_the_commit_button_follows(cx: &mut TestAppContext) {
    let (app, visual, tab) = sql_header_app(cx);
    assert!(visual.debug_bounds("tx-commit").is_none(), "Auto shows only the mode button");
    click_debug_button(visual, "tx-mode");
    settle(visual);
    let button = status_bounds(visual, "tx-mode");
    for item in ["tx-mode-auto", "tx-mode-manual"] {
        let bounds = status_bounds(visual, item);
        assert!(bounds.top() >= button.bottom() - gpui::px(1.0), "{item} opens below the button");
    }
    click_debug_button(visual, "tx-mode-manual");
    settle(visual);
    assert!(visual.debug_bounds("tx-mode-manual").is_none(), "picking closes the menu");
    app.read_with(visual, |this, _| assert_eq!(this.commit_mode(tab), CommitMode::Manual));
    assert!(visual.debug_bounds("tx-commit").is_some() && visual.debug_bounds("tx-rollback").is_some(), "Manual shows Commit and Roll Back");
    pick_commit_mode(visual, "tx-mode-auto");
    app.read_with(visual, |this, _| assert_eq!(this.commit_mode(tab), CommitMode::Auto));
    assert!(visual.debug_bounds("tx-commit").is_none(), "Auto hides them again");
    assert!(visual.debug_bounds("tx-mode").is_some());
}

/// Longer texts (Vietnamese) make the commit mode wider: it must still sit inside the header at
/// every width, and the title (cut to an ellipsis) must not be squeezed out to nothing.
#[gpui::test]
fn the_vietnamese_header_keeps_the_commit_mode_inside_and_a_title_beside_it(cx: &mut TestAppContext) {
    crate::i18n::set(crate::i18n::Locale::Vi);
    let (_app, visual, _tab) = sql_header_app(cx);
    pick_results_place(visual, "right");
    for manual in [false, true] {
        if manual {
            pick_commit_mode(visual, "tx-mode-manual");
            settle(visual);
        }
        for width in [1100.0, 800.0, 640.0] {
            visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
            settle(visual);
            settle(visual);
            let what = format!("vi manual={manual} Right at {width}");
            assert_header_fits(visual, &what);
            let header = status_bounds(visual, "editor-header");
            let title = status_bounds(visual, "editor-header-title");
            let mode = status_bounds(visual, "tx-mode");
            let run = status_bounds(visual, "run-query");
            assert!(title.size.width > gpui::px(0.0), "{what}: the title keeps some room (its ellipsis): {title:?}");
            if width >= 800.0 {
                assert!(title.size.width > gpui::px(40.0), "{what}: the title survives the commit mode ({:?}): {title:?}", mode.size.width);
            }
            assert!(title.left() >= header.left() && title.right() <= mode.left() + gpui::px(1.0), "{what}: the title is left of the switch: {title:?} {mode:?}");
            assert!(mode.right() <= run.left() && mode.size.width > gpui::px(60.0), "{what}: the button keeps its size, left of Run: {mode:?} {run:?}");
        }
    }
    crate::i18n::set(crate::i18n::Locale::En);
}

// ---- the footer of the results in a narrow panel ---------------------------------------------

const FOOTER_CONTROLS: [&str; 14] = [
    "result-group-refresh",
    "result-group-commit",
    "result-group-row",
    "result-group-pagination",
    "result-group-output",
    "result-refresh-dropdown",
    "apply-edit",
    "discard-edit",
    "page-previous",
    "page-next",
    "record-view-toggle",
    "result-export",
    "result-limit",
    "result-more",
];

/// Every control of the footer's action strip is whole inside the strip (which is inside the footer): nothing
/// is clipped or scrolled away, and what does not fit is in the "..." menu instead.
fn assert_footer_fits(visual: &mut gpui::VisualTestContext, what: &str) {
    let footer = status_bounds(visual, "result-action-footer");
    let strip = status_bounds(visual, "result-action-strip");
    assert!(strip.left() >= footer.left() - gpui::px(1.0) && strip.right() <= footer.right() + gpui::px(1.0), "{what}: strip inside the footer");
    let mut shown = Vec::new();
    for selector in FOOTER_CONTROLS {
        let Some(control) = visual.debug_bounds(selector) else {
            continue;
        };
        assert!(
            control.left() >= strip.left() - gpui::px(1.0) && control.right() <= strip.right() + gpui::px(1.0),
            "{what}: {selector} is outside the strip: {control:?} vs {strip:?}"
        );
        assert!(control.top() >= strip.top() - gpui::px(1.0) && control.bottom() <= strip.bottom() + gpui::px(1.0), "{what}: {selector} left the strip's row");
        shown.push(selector);
    }
    for must in ["result-export", "apply-edit", "discard-edit"] {
        assert!(shown.contains(&must), "{what}: {must} is a primary action and stays");
    }
    // The groups keep their order and never overlap.
    let groups: Vec<_> = ["result-group-refresh", "result-group-commit", "result-group-row", "result-group-pagination", "result-group-output", "result-more"]
        .into_iter()
        .filter_map(|selector| visual.debug_bounds(selector).map(|bounds| (selector, bounds)))
        .collect();
    for pair in groups.windows(2) {
        assert!(pair[0].1.right() <= pair[1].1.left() + gpui::px(1.0), "{what}: {} overlaps {}: {:?} {:?}", pair[0].0, pair[1].0, pair[0].1, pair[1].1);
    }
    // Limit is the first to go: with it in the strip nothing is in the menu.
    if visual.debug_bounds("result-limit").is_some() {
        assert!(visual.debug_bounds("result-more").is_none(), "{what}: the menu is only there when something moved");
    }
}

fn footer_in_both_layouts(cx: &mut TestAppContext, locale: crate::i18n::Locale) {
    crate::i18n::set(locale);
    let (app, visual, tab) = sql_header_app(cx);
    let mut seen_menu = false;
    let mut seen_full = false;
    for place in ["right", "below"] {
        // The place menu is picked from a wide window (its button is in the Results header).
        visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
        settle(visual);
        pick_results_place(visual, place);
        for width in [1100.0, 800.0, 640.0] {
            visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
            settle(visual);
            settle(visual);
            let what = format!("{locale:?} {place} at {width}");
            assert_footer_fits(visual, &what);
            seen_menu |= visual.debug_bounds("result-more").is_some();
            seen_full |= visual.debug_bounds("result-more").is_none();
        }
    }
    assert!(seen_menu && seen_full, "the sizes cover a full and a collapsed footer");
    // Narrow, Right: what moved works from the menu.
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    pick_results_place(visual, "right");
    // (At 640 the Right layout reaches past the window's edge, where a click cannot land: 800 is the
    // narrowest one a click can use.)
    visual.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(850.0)));
    settle(visual);
    settle(visual);
    assert!(visual.debug_bounds("result-limit").is_none(), "Limit moved into the menu");
    click_debug_button(visual, "result-more");
    settle(visual);
    let more = status_bounds(visual, "result-more");
    // Each control is in the strip or in the menu, never in both and never in neither.
    for (item, control) in [
        ("more-refresh", "result-refresh-dropdown"),
        ("more-insert", "sql-add-row"),
        ("more-edit", "edit-cell"),
        ("more-delete", "delete-row"),
        ("more-previous", "page-previous"),
        ("more-next", "page-next"),
        ("more-record", "record-view-toggle"),
        ("more-limit-100", "result-limit"),
    ] {
        let (in_menu, in_strip) = (visual.debug_bounds(item), visual.debug_bounds(control));
        assert!(in_menu.is_some() != in_strip.is_some(), "{item} / {control}: in the menu {in_menu:?}, in the strip {in_strip:?} (button at {more:?})");
        if let Some(entry) = in_menu {
            assert!(entry.right() <= visual.update(|window, _| window.viewport_size().width), "{item} is on screen");
        }
    }
    click_debug_button(visual, "more-limit-100");
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tab(tab).unwrap().output_limit), 100, "Limit from the menu");
    assert!(visual.debug_bounds("more-limit-100").is_none(), "the menu closed");
    // Record view from the menu (when it has moved there at this width).
    click_debug_button(visual, "result-more");
    settle(visual);
    if visual.debug_bounds("more-record").is_some() {
        let was = app.read_with(visual, |this, _| this.record_view_active(tab));
        click_debug_button(visual, "more-record");
        settle(visual);
        assert_ne!(app.read_with(visual, |this, _| this.record_view_active(tab)), was, "Record view from the menu");
    }
    crate::i18n::set(crate::i18n::Locale::En);
}

#[gpui::test]
fn the_results_footer_collapses_into_a_menu_instead_of_clipping_in_english(cx: &mut TestAppContext) {
    footer_in_both_layouts(cx, crate::i18n::Locale::En);
}

#[gpui::test]
fn the_results_footer_collapses_into_a_menu_instead_of_clipping_in_vietnamese(cx: &mut TestAppContext) {
    footer_in_both_layouts(cx, crate::i18n::Locale::Vi);
}

/// The results status row under the footer's strip: what was fetched on the left, the selection on the right.
#[gpui::test]
fn the_results_status_row_names_the_fetched_rows_and_the_selection_and_gives_way_when_narrow(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_header_app(cx);
    for width in [1100.0, 360.0, 220.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
        settle(visual);
        settle(visual);
        assert!(visual.debug_bounds("result-status-summary").is_some(), "the fetched count is drawn at {width}");
        assert!(visual.debug_bounds("result-status-selection-stats").is_none(), "nothing selected, nothing said at {width}");
    }
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    app.update(visual, |this, _| this.av().grid_ui.selection.borrow_mut().select(crate::grid::CellPosition { row: 0, column: 0 }));
    settle(visual);
    assert!(visual.debug_bounds("result-status-selection-stats").is_some(), "a selection shows");
    assert!(visual.debug_bounds("result-status-selected-label").is_some(), "with its label when there is room");
    let mut dropped = false;
    for width in [1100.0, 700.0, 460.0, 360.0, 300.0, 240.0, 200.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
        settle(visual);
        settle(visual);
        let row = status_bounds(visual, "result-status-row");
        let summary = status_bounds(visual, "result-status-summary");
        let selection = status_bounds(visual, "result-status-selection-stats");
        assert!(summary.left() >= row.left() && summary.right() <= row.right(), "the fetched count is whole at {width}: {summary:?} in {row:?}");
        assert!(selection.right() <= row.right() + gpui::px(1.0), "the selection is inside the row at {width}");
        assert!(summary.right() <= selection.left() + gpui::px(1.0), "the two do not overlap at {width}");
        if visual.debug_bounds("result-status-selected-label").is_none() {
            dropped = true;
        } else {
            assert!(!dropped, "once the label is gone a narrower row does not bring it back ({width})");
        }
    }
    assert!(dropped, "a narrow row drops the \"Selected:\" label and keeps the numbers");
}

// ---- results: in a window, hidden, and run from a window / the empty state -------------------

/// Frames of the results window (a hover, a scroll, a selection each draw one) used to refresh the app's
/// window as a whole: its panes, tab strips and editors were rendered again for every one of them.
#[gpui::test]
fn frames_of_the_results_window_do_not_render_the_app_windows_panes_again(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    pick_results_place(visual, "window");
    let handle = app.read_with(visual, |this, _| this.results_window(query)).expect("the results window");
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    settle(&mut other);
    settle(visual);
    let main = app_roots_current_window(&app, visual).0;
    assert_eq!(app_roots_current_window(&app, visual).1, Some(main), "the app's window has AppRoot after the results window drew");

    let panes = || panes_ui::PANE_RENDERS.with(|renders| renders.borrow().values().sum::<u64>());
    let tab_results = || results::surface::RENDERS.with(|renders| renders.borrow().values().sum::<u64>());
    let (panes_before, results_before) = (panes(), tab_results());
    // A good many frames of the results window: hovering over it, and drawing it again.
    let area = other.debug_bounds("results-window").expect("the results window is drawn");
    for step in 0..20 {
        let x = area.left() + gpui::px(20.0 + step as f32 * 7.0);
        other.simulate_mouse_move(gpui::point(x, area.center().y), None, Default::default());
        other.update(|window, _| window.refresh());
        other.run_until_parked();
        visual.run_until_parked();
    }
    assert_eq!(
        panes() - panes_before,
        0,
        "twenty frames of the results window rendered the app window's panes again"
    );
    assert!(tab_results() - results_before >= 1, "the results window did draw its results");
    assert_eq!(app_roots_current_window(&app, visual).1, Some(main), "and the app's window still gets AppRoot back");
}

#[gpui::test]
fn hidden_results_leave_a_strip_that_shows_them_again(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    let query = ran_sql_tab(&app, visual, "select 1 as one");
    assert!(visual.debug_bounds("results-hidden-note").is_none(), "shown results need no strip");
    // Toggle Results (the key) hides them: the editor has the panel, with a strip to bring them back.
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    app.read_with(visual, |this, _| assert!(this.results_hidden(query)));
    assert!(visual.debug_bounds("results-header").is_none(), "the results are hidden");
    let strip = status_bounds(visual, "results-hidden-note");
    let editor = status_bounds(visual, "query-editor-area");
    assert!(strip.top() >= editor.bottom() - gpui::px(1.0), "the strip is under the editor: {strip:?} {editor:?}");
    let button = status_bounds(visual, "results-hidden-show");
    assert!(strip.contains(&button.center()));
    let key = status_bounds(visual, "results-hidden-key");
    assert!(strip.contains(&key.center()) && key.left() > button.left() - gpui::px(1.0), "the key is named on the strip");
    // The button shows them again, in the same place.
    click_debug_button(visual, "results-hidden-show");
    settle(visual);
    app.read_with(visual, |this, _| assert!(!this.results_hidden(query)));
    assert!(visual.debug_bounds("results-header").is_some(), "the results are back");
    assert!(visual.debug_bounds("results-hidden-note").is_none(), "and the strip goes");
    // And the key does it too.
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    assert!(visual.debug_bounds("results-hidden-note").is_some());
    visual.simulate_keystrokes("secondary-6");
    settle(visual);
    assert!(visual.debug_bounds("results-hidden-note").is_none());
}

/// Run from a results window and from the empty result state is the tab's own Run: an aggregation
/// checks its pipeline and asks before a write; it is not a re-run of the page (which refuses writes).
#[gpui::test]
fn an_aggregation_run_from_a_results_window_or_the_empty_state_goes_the_way_its_run_button_does(cx: &mut TestAppContext) {
    let (app, visual) = agg_open_tab(cx);
    let tab = active_tab(&app, visual);
    // A pipeline that writes: the tab's Run button asks before running it.
    agg_type_pipeline(visual, "[{\"$match\": {}}, {\"$out\": \"copy\"}]");
    app.update(visual, |this, cx| this.set_results_place(tab, crate::app::results::placement::ResultsPlace::Window, cx));
    settle(visual);
    let handle = app.read_with(visual, |this, _| this.results_window(tab)).expect("the results window");
    let mut other = gpui::VisualTestContext::from_window(handle, &visual.cx);
    settle(&mut other);
    let ran = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab).map_or(0, |runtime| runtime.query.generation()))
    };
    let before = ran(&app, visual);
    // The empty state's Run button.
    let empty_run = other.debug_bounds("empty-run-query").expect("the empty result state has a Run button");
    other.simulate_click(empty_run.center(), Default::default());
    other.run_until_parked();
    settle(visual);
    assert_eq!(ran(&app, visual), before, "a pipeline that writes does not run without its confirmation");
    assert!(
        other.update(|window, cx| window.has_active_dialog(cx)) || visual.update(|window, cx| window.has_active_dialog(cx)),
        "it asks first, as the tab's Run does"
    );
    // Cmd+Enter in the results window does the same.
    let before_asks = (other.update(|window, cx| window.has_active_dialog(cx)), visual.update(|window, cx| window.has_active_dialog(cx)));
    other.update(|window, cx| window.close_dialog(cx));
    visual.update(|window, cx| window.close_dialog(cx));
    other.dispatch_action(crate::app::RunActiveQuery);
    other.run_until_parked();
    settle(visual);
    assert_eq!(ran(&app, visual), before, "Cmd+Enter in the window does not run a write either");
    let asks = (other.update(|window, cx| window.has_active_dialog(cx)), visual.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(asks, before_asks, "and it asks, the same way");
    assert!(asks.0 || asks.1);
}

// ---- frames the app asks for by itself ---------------------------------------------------------

/// Runs the frames a real window draws without any input: each next-frame request is delivered (the
/// platform does that before it draws) and what it dirtied is drawn. Returns how many frames were asked
/// for (several requests of one frame are one). Panics if it never settles: a perpetual redraw loop.
fn run_requested_frames(visual: &mut gpui::VisualTestContext) -> usize {
    for frames in 0..16 {
        visual.run_until_parked();
        if visual.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            return frames;
        }
    }
    panic!("the window keeps asking for frames: 16 frames in a row");
}

/// Whether anything is waiting for a frame.
fn frame_requests_pending(visual: &mut gpui::VisualTestContext) -> usize {
    visual.update(|window, cx| window.simulate_next_frame(cx))
}

fn header_renders() -> usize {
    crate::app::panes_ui::HEADER_RENDERS.with(std::cell::Cell::get)
}

/// The Vietnamese header at 800 px with the results on the right: switching the commit mode to Manual
/// changes what the controls need. Only the frames the app draws by itself follow (no mouse move, no
/// refresh): the measure taken while painting has to bring its own frame.
#[gpui::test]
fn switching_the_commit_mode_settles_the_header_without_any_input(cx: &mut TestAppContext) {
    crate::i18n::set(crate::i18n::Locale::Vi);
    let (app, visual, tab) = sql_header_app(cx);
    pick_results_place(visual, "right");
    visual.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(850.0)));
    settle(visual);
    settle(visual);
    assert_header_fits(visual, "Auto, before the switch");

    click_debug_button(visual, "tx-mode");
    click_debug_button(visual, "tx-mode-manual");
    app.read_with(visual, |this, _| assert_eq!(this.commit_mode(tab), CommitMode::Manual));
    let requested = run_requested_frames(visual);
    assert!(requested >= 1, "the measure of the new controls asked for a frame");
    assert!(requested <= 3, "and it settles quickly: {requested} frames");

    assert_header_fits(visual, "Manual, no input");
    let header = status_bounds(visual, "editor-header");
    let run = status_bounds(visual, "run-query");
    let more = status_bounds(visual, "editor-more");
    for (name, bounds) in [("run-query", run), ("editor-more", more)] {
        assert!(bounds.left() >= header.left() && bounds.right() <= header.right(), "{name} is whole inside the header: {bounds:?} vs {header:?}");
    }
    // Commit is whole in the header or in the menu, never clipped.
    if let Some(commit) = visual.debug_bounds("tx-commit") {
        let row = status_bounds(visual, "editor-header-controls");
        assert!(commit.left() >= row.left() && commit.right() <= row.right(), "Commit is not clipped: {commit:?} in {row:?}");
    }
    // What the frames drew by themselves is what a full redraw draws.
    let drawn: Vec<_> = HEADER_CONTROLS.iter().map(|selector| visual.debug_bounds(selector)).collect();
    settle(visual);
    settle(visual);
    let redrawn: Vec<_> = HEADER_CONTROLS.iter().map(|selector| visual.debug_bounds(selector)).collect();
    assert_eq!(drawn, redrawn, "the header had converged");
    crate::i18n::set(crate::i18n::Locale::En);
}

/// Once settled, the header asks for nothing: idle frames build no header, and a full redraw that
/// measures the same widths requests no frame (no perpetual loop), at every width and in both modes.
#[gpui::test]
fn a_settled_header_requests_no_more_frames_and_builds_no_header_while_idle(cx: &mut TestAppContext) {
    crate::i18n::set(crate::i18n::Locale::Vi);
    let (_app, visual, _tab) = sql_header_app(cx);
    pick_results_place(visual, "right");
    for manual in [false, true] {
        if manual {
            click_debug_button(visual, "tx-mode");
            click_debug_button(visual, "tx-mode-manual");
            run_requested_frames(visual);
        }
        for width in [1700.0, 1180.0, 900.0, 800.0, 640.0] {
            visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
            run_requested_frames(visual);
            let what = format!("manual={manual} at {width}");
            let before = header_renders();
            for _ in 0..5 {
                visual.run_until_parked();
                assert_eq!(frame_requests_pending(visual), 0, "{what}: an idle frame asks for nothing");
            }
            assert_eq!(header_renders(), before, "{what}: idle frames build no header");
            // A redraw of everything (what a hover does) measures the same: nothing is asked for.
            visual.update(|window, _| window.refresh());
            visual.run_until_parked();
            assert!(header_renders() > before, "{what}: the redraw built the header");
            assert_eq!(frame_requests_pending(visual), 0, "{what}: the same measure asks for no frame");
        }
    }
    crate::i18n::set(crate::i18n::Locale::En);
}

/// The names of the footer's controls that are drawn, with their bounds.
fn footer_state(visual: &mut gpui::VisualTestContext) -> Vec<(&'static str, Option<gpui::Bounds<gpui::Pixels>>)> {
    FOOTER_CONTROLS.iter().map(|selector| (*selector, visual.debug_bounds(selector))).collect()
}

/// A language change or a resize measures the footer's parts again (drawing them all first): the
/// frames the app draws by itself, with no input, leave the footer fitting and equal to a full redraw.
#[gpui::test]
fn the_footer_settles_after_a_language_change_or_a_resize_without_any_input(cx: &mut TestAppContext) {
    crate::i18n::set(crate::i18n::Locale::En);
    let (_app, visual, _tab) = sql_header_app(cx);
    pick_results_place(visual, "right");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    settle(visual);
    assert_footer_fits(visual, "English at 1100");

    click_debug_button(visual, "status-language");
    click_debug_button(visual, "title-language-vi");
    assert_eq!(crate::i18n::current(), crate::i18n::Locale::Vi);
    run_requested_frames(visual);
    assert_footer_fits(visual, "Vietnamese, no input");
    assert_header_fits(visual, "Vietnamese header, no input");
    let drawn = footer_state(visual);
    settle(visual);
    settle(visual);
    assert_eq!(drawn, footer_state(visual), "the footer had converged after the language change");

    for width in [800.0, 1100.0, 800.0, 640.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
        run_requested_frames(visual);
        let what = format!("resize to {width}, no input");
        assert_footer_fits(visual, &what);
        let drawn = footer_state(visual);
        settle(visual);
        settle(visual);
        assert_eq!(drawn, footer_state(visual), "{what}: the footer had converged");
        // Steady state: a full redraw asks for no frame.
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        assert_eq!(frame_requests_pending(visual), 0, "{what}: the footer asks for nothing once settled");
    }
    crate::i18n::set(crate::i18n::Locale::En);
}
