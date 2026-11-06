// Settings tab proofs with real clicks, keys and dialogs. The app is built on
// the production theme and asset-less test platform; every persisted file lives
// in a temp dir, never in the user's config folder.

use crate::settings::registry;
use crate::settings::{AiProvider, RunScope, Settings};

fn settings_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|_, cx| {
        actions::bind_keys(cx);
        // Dialogs close at once, so focus is back on the page when they are gone.
        cx.set_reduce_motion(true);
    });
    // Tall enough that every row of the longest page is on screen.
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(2300.0)));
    visual.update(|window, _| window.activate_window());
    settle(visual);
    (app, visual)
}

fn settings_temp_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("tablex-settings-ui-{}", uuid::Uuid::new_v4()))
        .join("settings.json")
}

fn click(visual: &mut gpui::VisualTestContext, selector: String) {
    click_debug_button(visual, leak(selector));
    settle(visual);
}

fn rendered(visual: &mut gpui::VisualTestContext, selector: String) -> bool {
    visual.debug_bounds(leak(selector)).is_some()
}

fn active_kind(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> Option<TabKind> {
    app.read_with(visual, |this, _| {
        this.workspace.active_tab().map(|tab| tab.kind)
    })
}

fn open_settings_page(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    click(visual, "open-settings".into());
    assert_eq!(active_kind(app, visual), Some(TabKind::Settings));
}

fn draft_of(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Settings {
    app.read_with(visual, |this, _| {
        this.settings_ui.page.as_ref().expect("page").draft.clone()
    })
}

fn applied(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Settings {
    app.read_with(visual, |this, _| (**this.settings()).clone())
}

fn settings_nav(visual: &mut gpui::VisualTestContext, key: &str) {
    click(visual, format!("settings-nav-{key}"));
}

fn toggle_setting(visual: &mut gpui::VisualTestContext, id: &str) {
    click(visual, format!("setting-control-{id}"));
}

#[gpui::test]
fn settings_open_from_the_gear_the_shortcut_and_the_palette(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    assert_ne!(active_kind(&app, visual), Some(TabKind::Settings));

    // Gear in the title bar.
    open_settings_page(&app, visual);
    assert!(rendered(visual, "settings-page".into()));
    assert!(rendered(visual, "settings-footer".into()));
    for page in [
        "appearance",
        "editor",
        "results",
        "query",
        "navigation",
        "drivers",
        "about",
    ] {
        assert!(
            rendered(visual, format!("settings-nav-{page}")),
            "{page} in the nav"
        );
    }
    // Opening again reuses the singleton tab.
    open_settings_page(&app, visual);
    let tabs = app.read_with(visual, |this, _| {
        this.workspace
            .tabs()
            .iter()
            .filter(|tab| tab.kind == TabKind::Settings)
            .count()
    });
    assert_eq!(tabs, 1);

    // Close it (nothing pending), reopen with the shortcut.
    click(visual, "settings-close".into());
    assert_ne!(active_kind(&app, visual), Some(TabKind::Settings));
    assert!(
        app.read_with(visual, |this, _| this.settings_ui.page.is_none()),
        "the draft goes with the tab"
    );
    visual.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-,"
    } else {
        "ctrl-,"
    });
    settle(visual);
    assert_eq!(
        active_kind(&app, visual),
        Some(TabKind::Settings),
        "secondary-, opens Settings"
    );
    click(visual, "settings-close".into());

    // Command palette.
    visual.simulate_keystrokes("secondary-k");
    settle(visual);
    visual.simulate_input("Settings");
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(
        active_kind(&app, visual),
        Some(TabKind::Settings),
        "the palette lists Settings"
    );
}

#[gpui::test]
fn an_edit_is_a_draft_until_apply_and_apply_publishes_it(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");

    // Nothing to apply yet.
    assert!(rendered(visual, "setting-editor.soft_wrap".into()));
    toggle_setting(visual, "editor.soft_wrap");
    assert!(draft_of(&app, visual).editor.soft_wrap, "the draft changed");
    assert!(!applied(&app, visual).editor.soft_wrap, "the app did not");
    assert!(app.read_with(visual, |this, _| this.settings_dirty()));

    // An editor that already exists follows on Apply.
    let tab = app.update(visual, |this, cx| {
        let tab = this.workspace.open_query_tab("Script");
        cx.notify();
        tab
    });
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.editor_for_tab(tab, window, cx);
        });
    });
    app.update(visual, |this, _| this.workspace.open_settings_tab());
    settle(visual);
    let before = app.read_with(visual, |this, _| this.settings_ui.editors_reconfigured);
    click(visual, "settings-apply".into());
    assert!(
        applied(&app, visual).editor.soft_wrap,
        "Apply publishes the draft"
    );
    assert!(!app.read_with(visual, |this, _| this.settings_dirty()));
    assert!(
        app.read_with(visual, |this, _| this.settings_ui.editors_reconfigured) > before,
        "the open editor was reconfigured in place"
    );

    // The page stays open after Apply and the modified marker is gone.
    assert_eq!(active_kind(&app, visual), Some(TabKind::Settings));
    // Persisted after the debounce, off the UI thread.
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the settings file", |_| path.exists());
    let saved = crate::settings::store::load(&path, &crate::settings::store::Legacy::default());
    assert!(saved.settings.editor.soft_wrap);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[gpui::test]
fn apply_and_close_applies_then_closes(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    toggle_setting(visual, "editor.line_numbers");
    assert!(!draft_of(&app, visual).editor.line_numbers);
    click(visual, "settings-apply-close".into());
    assert!(!applied(&app, visual).editor.line_numbers);
    assert_ne!(active_kind(&app, visual), Some(TabKind::Settings));
    assert!(app.read_with(visual, |this, _| this.settings_ui.page.is_none()));
}

#[gpui::test]
fn closing_with_pending_changes_asks_and_discard_throws_the_draft_away(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    toggle_setting(visual, "editor.soft_wrap");

    // Close button: the dialog appears, Cancel keeps everything.
    click(visual, "settings-close".into());
    assert!(
        rendered(visual, "settings-discard-body".into()),
        "Discard changes? is asked"
    );
    click(visual, "settings-discard-cancel".into());
    assert_eq!(active_kind(&app, visual), Some(TabKind::Settings));
    assert!(
        draft_of(&app, visual).editor.soft_wrap,
        "Cancel keeps the draft"
    );

    // Escape asks as well; Discard closes and nothing was applied.
    let in_page = visual.update(|window, cx| {
        app.read(cx)
            .settings_ui
            .page
            .as_ref()
            .unwrap()
            .focus
            .contains_focused(window, cx)
    });
    assert!(in_page, "the page has focus again once the dialog is gone");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(
        rendered(visual, "settings-discard-body".into()),
        "Escape with pending changes asks"
    );
    click(visual, "settings-discard-confirm".into());
    assert_ne!(active_kind(&app, visual), Some(TabKind::Settings));
    assert!(!applied(&app, visual).editor.soft_wrap);
    assert!(app.read_with(visual, |this, _| this.settings_ui.page.is_none()));

    // Reopening starts from the applied values.
    open_settings_page(&app, visual);
    assert!(!draft_of(&app, visual).editor.soft_wrap);
    // The tab's own close button asks too.
    settings_nav(visual, "editor");
    toggle_setting(visual, "editor.soft_wrap");
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    visual.update(|window, cx| app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(visual);
    assert!(rendered(visual, "settings-discard-body".into()));
    click(visual, "settings-discard-cancel".into());
}

#[gpui::test]
fn reset_to_default_reset_section_and_reset_defaults_edit_the_draft(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    assert!(
        !rendered(visual, "setting-reset-editor.soft_wrap".into()),
        "no reset at the default"
    );

    toggle_setting(visual, "editor.soft_wrap");
    toggle_setting(visual, "editor.line_numbers");
    assert!(
        rendered(visual, "setting-reset-editor.soft_wrap".into()),
        "a changed value offers its reset"
    );

    // One setting.
    click(visual, "setting-reset-editor.soft_wrap".into());
    let draft = draft_of(&app, visual);
    assert!(!draft.editor.soft_wrap && !draft.editor.line_numbers);
    assert!(!rendered(visual, "setting-reset-editor.soft_wrap".into()));

    // The section.
    click(visual, "settings-reset-section-editor".into());
    assert_eq!(draft_of(&app, visual).editor, Settings::default().editor);

    // Everything, across sections; the app itself never moved.
    toggle_setting(visual, "editor.soft_wrap");
    settings_nav(visual, "results");
    toggle_setting(visual, "results.right_align_numbers");
    click(visual, "settings-reset-defaults".into());
    assert_eq!(draft_of(&app, visual), Settings::default());
    assert_eq!(applied(&app, visual), Settings::default());
    assert!(!app.read_with(visual, |this, _| this.settings_dirty()));
}

#[gpui::test]
fn search_filters_every_page_then_jumps_to_and_highlights_the_row(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    let search = visual.debug_bounds("settings-search").expect("search box");
    visual.simulate_click(search.center(), Default::default());
    visual.simulate_input("NULL DISPLAY");
    settle(visual);

    // Case-insensitive over title / description / section; other rows are gone.
    assert!(rendered(visual, "setting-results.null_text".into()));
    assert!(!rendered(visual, "setting-editor.soft_wrap".into()));
    assert!(rendered(visual, "settings-result-count".into()));

    // The result names its page; clicking jumps there and highlights the row.
    click(visual, "setting-goto-results.null_text".into());
    app.read_with(visual, |this, cx| {
        let page = this.settings_ui.page.as_ref().unwrap();
        assert!(page.is_on_section(registry::Section::Results));
        assert_eq!(page.highlighted(), Some("results.null_text"));
        assert!(
            page.search_text(cx).is_empty(),
            "the search box is cleared by the jump"
        );
    });
    assert!(rendered(visual, "setting-results.null_text".into()));
    assert!(
        rendered(visual, "setting-results.page_size".into()),
        "the whole page is back"
    );

    // Section names match too; Enter jumps to the first result.
    let search = visual.debug_bounds("settings-search").unwrap();
    visual.simulate_click(search.center(), Default::default());
    visual.simulate_input("navigation");
    settle(visual);
    assert!(rendered(visual, "setting-navigation.restore_tabs".into()));
    visual.simulate_keystrokes("enter");
    settle(visual);
    app.read_with(visual, |this, _| {
        let page = this.settings_ui.page.as_ref().unwrap();
        assert!(page.is_on_section(registry::Section::Navigation));
        assert!(page
            .highlighted()
            .is_some_and(|id| id.starts_with("navigation.")));
    });
}

#[gpui::test]
fn numbers_validate_inline_and_keep_the_last_valid_value(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    let input = visual
        .debug_bounds("setting-control-editor.tab_size")
        .expect("tab size input");
    visual.simulate_click(input.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("99");
    settle(visual);

    assert!(
        rendered(visual, "setting-error-editor.tab_size".into()),
        "out of range shows an inline error"
    );
    // "9" (typed first) was valid; "99" is not, and the draft keeps the 9.
    assert_eq!(
        draft_of(&app, visual).editor.tab_size,
        9,
        "the last valid value stays"
    );
    let apply = app.read_with(visual, |this, _| {
        this.settings_ui.page.as_ref().unwrap().has_errors()
    });
    assert!(apply, "Apply is blocked while a value is invalid");
    click(visual, "settings-apply".into());
    assert_eq!(
        applied(&app, visual).editor.tab_size,
        2,
        "an invalid draft is never applied"
    );
    assert!(
        app.read_with(visual, |this, _| this.settings_dirty()),
        "the valid part of the draft is still pending"
    );

    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("4");
    settle(visual);
    assert!(!rendered(visual, "setting-error-editor.tab_size".into()));
    assert_eq!(draft_of(&app, visual).editor.tab_size, 4);
    click(visual, "settings-apply".into());
    assert_eq!(applied(&app, visual).editor.tab_size, 4);
}

#[gpui::test]
fn a_dropdown_opens_selects_updates_the_draft_and_reopens_on_the_new_value(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    assert_eq!(draft_of(&app, visual).editor.run_scope, RunScope::All);

    // Open, move to the first option ("Current statement"), confirm.
    click(visual, "setting-control-editor.run_scope".into());
    visual.simulate_keystrokes("up up enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).editor.run_scope, RunScope::Current);
    assert_eq!(
        applied(&app, visual).editor.run_scope,
        RunScope::All,
        "still a draft"
    );
    assert!(rendered(visual, "setting-reset-editor.run_scope".into()));

    // Reopening shows the chosen option as selected.
    app.read_with(visual, |this, cx| {
        let select = this
            .settings_ui
            .page
            .as_ref()
            .unwrap()
            .select_for_test("editor.run_scope");
        assert_eq!(
            select
                .read(cx)
                .selected_value()
                .map(|label| label.to_string()),
            Some("Current statement".to_string())
        );
    });
    click(visual, "setting-control-editor.run_scope".into());
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(
        draft_of(&app, visual).editor.run_scope,
        RunScope::Current,
        "dismissal changes nothing"
    );

    // Resetting the row moves the select back too.
    click(visual, "setting-reset-editor.run_scope".into());
    app.read_with(visual, |this, cx| {
        let select = this
            .settings_ui
            .page
            .as_ref()
            .unwrap()
            .select_for_test("editor.run_scope");
        assert_eq!(
            select
                .read(cx)
                .selected_value()
                .map(|label| label.to_string()),
            Some("Whole script".to_string())
        );
    });
}

#[gpui::test]
fn settings_survive_a_relaunch_and_a_changed_value_reaches_the_grid_display(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "results");
    // Type a NULL text into the text input and apply.
    let input = visual
        .debug_bounds("setting-control-results.null_text")
        .expect("null text input");
    visual.simulate_click(input.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("\u{2205}");
    settle(visual);
    assert_eq!(draft_of(&app, visual).results.null_text, "\u{2205}");
    assert_eq!(
        app.read_with(visual, |this, _| this.grid_display.null_text.to_string()),
        "NULL"
    );
    click(visual, "settings-apply".into());
    assert_eq!(
        app.read_with(visual, |this, _| this.grid_display.null_text.to_string()),
        "\u{2205}"
    );

    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the settings file", |_| path.exists());

    // "Relaunch": the settings are read back from the same file.
    app.update(visual, |this, cx| {
        this.settings = std::sync::Arc::new(Settings::default());
        this.settings_ui.path = Some(path.clone());
        this.init_settings(cx);
    });
    assert_eq!(
        app.read_with(visual, |this, _| this.settings.results.null_text.clone()),
        "\u{2205}",
        "the value came back from disk"
    );
    assert_eq!(
        app.read_with(visual, |this, _| this.grid_display.null_text.to_string()),
        "\u{2205}"
    );
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[gpui::test]
fn the_fps_counter_follows_its_setting(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    for show in [true, false, true] {
        let mut next = applied(&app, visual);
        next.appearance.show_fps = show;
        visual.update(|window, cx| {
            app.update(cx, |this, cx| this.apply_settings(next, Some(window), cx))
        });
        settle(visual);
        assert_eq!(rendered(visual, "status-fps".into()), show);
    }
}

#[gpui::test]
fn the_unsaved_script_check_follows_its_setting(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let tab = app.update(visual, |this, cx| {
        let tab = this.workspace.open_query_tab("Draft");
        this.workspace.tab_mut(tab).unwrap().sql = "select 1".into();
        cx.notify();
        tab
    });
    assert!(app.read_with(visual, |this, _| this.is_unsaved_script(tab)));

    // Off (the default): closing just closes.
    visual.update(|window, cx| app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab).is_none()));

    // On: closing asks first; confirming closes.
    let tab = app.update(visual, |this, cx| {
        let mut next = (*this.settings).clone();
        next.editor.confirm_unsaved_close = true;
        this.apply_settings(next, None, cx);
        let tab = this.workspace.open_query_tab("Draft");
        this.workspace.tab_mut(tab).unwrap().sql = "select 2".into();
        tab
    });
    visual.update(|window, cx| app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(visual);
    assert!(rendered(visual, "unsaved-script-body".into()));
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab).is_some()));
    // A saved script is not asked about.
    app.update(visual, |this, _| {
        this.settings_ui
            .saved_scripts
            .insert(tab, "select 2".into());
        assert!(!this.is_unsaved_script(tab));
    });
}

#[gpui::test]
fn run_executes_the_setting_scope(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let script = "select 1;\nselect 2;\nselect 3";
    let tab = app.update(visual, |this, cx| {
        let tab = this.workspace.open_query_tab("Scope");
        this.workspace.tab_mut(tab).unwrap().sql = script.into();
        cx.notify();
        tab
    });
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            let editor = this.editor_for_tab(tab, window, cx);
            editor.update(cx, |state, cx| {
                state.set_value(script, window, cx);
                // Cursor inside "select 2".
                state.set_cursor_position(gpui_component::input::Position::new(1, 3), window, cx);
            });
        });
    });
    let run = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| {
            this.sql_in_run_scope(tab, script.to_string(), cx)
        })
    };
    // The default runs the whole script.
    assert_eq!(run(&app, visual), script);

    let scope =
        |value: RunScope, app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
            let mut next = applied(app, visual);
            next.editor.run_scope = value;
            app.update(visual, |this, cx| this.apply_settings(next, None, cx));
        };
    scope(RunScope::Current, &app, visual);
    assert_eq!(run(&app, visual), "select 2");
    scope(RunScope::Selection, &app, visual);
    assert_eq!(run(&app, visual), script, "no selection: everything");

    // A selection is preferred by Selection and Current, ignored by All.
    visual.update(|_window, cx| {
        app.update(cx, |this, cx| {
            let editor = this.query_editors.by_tab[&tab].clone();
            editor.update(cx, |state, cx| {
                state.set_selected_range(0..8, cx);
            });
        });
    });
    assert_eq!(run(&app, visual), "select 1");
    scope(RunScope::Current, &app, visual);
    assert_eq!(run(&app, visual), "select 1");
    scope(RunScope::All, &app, visual);
    assert_eq!(run(&app, visual), script);
}

#[gpui::test]
fn turning_the_dangerous_statement_confirmation_off_runs_the_statement_without_asking(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.app.update(rig.visual, |this, cx| {
        let mut next = (*this.settings).clone();
        next.editor.confirm_dangerous = false;
        this.apply_settings(next, None, cx);
    });
    rig.run("DELETE FROM items");
    rig.wait_status("the unconfirmed DELETE", |status| {
        matches!(status, crate::query::QueryStatus::Ready { .. })
    });
    assert!(
        rig.visual.debug_bounds("sql-confirm-run").is_none(),
        "no dialog was shown"
    );
    assert_eq!(
        db.sql("SELECT count(*) FROM items"),
        "0",
        "the driver's refusal was answered"
    );
}

#[gpui::test]
fn the_query_timeout_cancels_a_run_that_outlives_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.app.update(rig.visual, |this, cx| {
        let mut next = (*this.settings).clone();
        next.query.query_timeout_secs = 1;
        this.apply_settings(next, None, cx);
    });
    rig.run(LONG_QUERY);
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(150));
    // Time passes (the test clock only moves when told to).
    rig.visual
        .executor()
        .advance_clock(std::time::Duration::from_secs(2));
    rig.wait_status("the timed-out run", |status| {
        matches!(status, crate::query::QueryStatus::Failed(message) if message.contains("timed out after 1 s"))
    });
    assert!(
        !rig.operation_registered(),
        "the driver operation was cancelled and released"
    );
    rig.wait_driver_idle();
    // A run that finishes inside the limit is left alone.
    rig.app.update(rig.visual, |this, cx| {
        let mut next = (*this.settings).clone();
        next.query.query_timeout_secs = 60;
        this.apply_settings(next, None, cx);
    });
    rig.run("SELECT 1");
    rig.wait_status("a fast query", |status| {
        matches!(status, crate::query::QueryStatus::Ready { .. })
    });
}

#[gpui::test]
fn a_new_tab_uses_the_default_page_size_setting(cx: &mut TestAppContext) {
    let (app, visual) = settings_app(cx);
    app.update(visual, |this, cx| {
        let mut next = (*this.settings).clone();
        next.results.page_size = 50;
        this.apply_settings(next, None, cx);
        let tab = this.workspace.open_query_tab("Paged");
        assert_eq!(this.workspace.tab(tab).unwrap().output_limit, 50);
        assert_eq!(
            this.workspace.runtime(tab).unwrap().pagination.page_size(),
            50
        );
    });
}

#[gpui::test]
fn restore_tabs_on_start_honours_none_and_pinned(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    use crate::settings::RestoreTabs;
    let dir = std::env::temp_dir().join(format!("tablex-restore-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let session_path = dir.join("session.json");
    let (app, visual) = settings_app(cx);
    app.update(visual, |this, _| {
        this.workspace.open_query_tab("Loose");
        let pinned = this.workspace.open_query_tab("Pinned");
        this.workspace.tab_mut(pinned).unwrap().is_pinned = true;
        this.workspace.normalize();
        let bytes = crate::gpui_session::encode(&this.workspace.snapshot()).unwrap();
        std::fs::write(&session_path, bytes).unwrap();
    });
    let titles = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            this.workspace
                .tabs()
                .iter()
                .map(|tab| tab.title.clone())
                .collect::<Vec<_>>()
        })
    };
    for (mode, expected) in [
        (RestoreTabs::All, vec!["Pinned", "Welcome", "Loose"]),
        (RestoreTabs::Pinned, vec!["Pinned", "Welcome"]),
        (RestoreTabs::None, vec!["Welcome"]),
    ] {
        let (app, visual) = (&app, &mut *visual);
        app.update(visual, |this, cx| {
            // A fresh workspace, as at startup.
            for id in this
                .workspace
                .tabs()
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>()
            {
                this.close_tab(id, cx);
            }
            this.workspace.open_welcome_tab();
            this.shell.closed_tabs.clear();
            let mut next = (*this.settings).clone();
            next.navigation.restore_tabs = mode;
            this.settings = std::sync::Arc::new(next);
            this.shell.session.path = Some(session_path.clone());
            this.restore_workspace_session(false, |_, _| {}, cx);
        });
        let mut got = titles(app, visual);
        got.sort();
        let mut want: Vec<String> = expected.iter().map(|title| title.to_string()).collect();
        want.sort();
        assert_eq!(got, want, "{mode:?}");
        // The tabs the pinned-only start left out were not closed by the user.
        assert!(app.read_with(visual, |this, _| this.shell.closed_tabs.is_empty()), "{mode:?}: nothing to reopen");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn the_drivers_page_surfaces_developer_mode_and_the_driver_manager(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "drivers");
    assert!(rendered(visual, "settings-developer-mode".into()));
    click(visual, "settings-open-driver-manager".into());
    assert_eq!(active_kind(&app, visual), Some(TabKind::DriverManager));
}

#[gpui::test]
fn the_title_bar_names_the_app_version_and_the_active_tab(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let title = visual.debug_bounds("window-title").expect("centred title");
    let gear = visual.debug_bounds("open-settings").expect("gear");
    assert!(title.right() <= gear.left(), "title sits left of the gear");
    // The title bar carries only the title and the gear.
    for gone in ["add-query", "run-query", "query-history"] {
        assert!(
            visual.debug_bounds(gone).is_none(),
            "{gone} is not in the title bar"
        );
    }
    assert_eq!(title.center().y, gear.center().y, "same bar");
    let version = concat!("TableX ", env!("CARGO_PKG_VERSION"));
    click(visual, "open-settings".into());
    let (shown, os_title) = app.read_with(visual, |this, _| {
        (this.window_title(), this.shell.window_title.clone())
    });
    assert_eq!(shown.as_ref(), format!("{version} - Settings"));
    assert_eq!(
        os_title, shown,
        "the OS window title follows the active tab"
    );
    app.update(visual, |this, cx| {
        let ids: Vec<u64> = this.workspace.tabs().iter().map(|tab| tab.id).collect();
        for id in ids {
            this.workspace.close_tab(id);
        }
        cx.notify();
    });
    settle(visual);
    let (shown, os_title) = app.read_with(visual, |this, _| {
        (this.window_title(), this.shell.window_title.clone())
    });
    assert_eq!((shown.as_ref(), os_title.as_ref()), (version, version));
}

#[gpui::test]
fn the_global_toolbar_opens_the_driver_manager_next_to_data_transfer(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let transfer = visual
        .debug_bounds("global-transfer")
        .expect("Data Transfer button");
    let manager = visual
        .debug_bounds("global-driver-manager")
        .expect("Driver Manager button");
    assert!(
        manager.left() >= transfer.right(),
        "Driver Manager sits right of Data Transfer"
    );
    assert_eq!(manager.center().y, transfer.center().y, "same toolbar row");
    click(visual, "global-driver-manager".into());
    assert_eq!(active_kind(&app, visual), Some(TabKind::DriverManager));
    // A second click focuses the same tab instead of opening another one.
    click(visual, "open-settings".into());
    click(visual, "global-driver-manager".into());
    let managers = app.read_with(visual, |this, _| {
        this.workspace
            .tabs()
            .iter()
            .filter(|tab| tab.kind == TabKind::DriverManager)
            .count()
    });
    assert_eq!(
        (active_kind(&app, visual), managers),
        (Some(TabKind::DriverManager), 1)
    );
}

#[gpui::test]
fn every_visible_setting_renders_a_row_with_a_working_control(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    for section in registry::Section::ALL {
        settings_nav(visual, section.key());
        for descriptor in registry::visible(section) {
            assert!(
                rendered(visual, format!("setting-{}", descriptor.id)),
                "{} has a row",
                descriptor.id
            );
            assert!(
                rendered(visual, format!("setting-control-{}", descriptor.id)),
                "{} has a control",
                descriptor.id
            );
        }
    }
    // Hidden ("coming soon") settings never reach a page.
    settings_nav(visual, "appearance");
    assert!(!rendered(visual, "setting-appearance.ui_scale".into()));
}

#[gpui::test]
fn the_settings_scenario_opens_the_editor_page_with_pending_edits(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = context_test_app(cx, "settings", "sample");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    visual.update(|window, _| window.activate_window());
    settle(visual);
    assert_eq!(active_kind(&app, visual), Some(TabKind::Settings));
    assert!(rendered(visual, "settings-page".into()));
    assert!(
        rendered(visual, "setting-editor.soft_wrap".into()),
        "the Editor page is shown"
    );
    assert!(
        app.read_with(visual, |this, _| this.settings_dirty()),
        "the seeded edits are pending"
    );
    assert!(rendered(visual, "setting-reset-editor.soft_wrap".into()));
    assert!(
        !applied(&app, visual).editor.soft_wrap,
        "nothing was applied"
    );
}

fn query_tab_app(
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<AppRoot>,
    &mut gpui::VisualTestContext,
    gpui::Entity<EditorState>,
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
    (app, visual, editor)
}

fn apply_editor_setting(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    change: impl FnOnce(&mut crate::settings::EditorSettings),
) {
    let mut next = applied(app, visual);
    change(&mut next.editor);
    visual.update(|window, cx| {
        app.update(cx, |this, cx| this.apply_settings(next, Some(window), cx))
    });
    settle(visual);
}

#[gpui::test]
fn autocomplete_settings_limit_the_list_add_a_space_and_can_switch_it_off(cx: &mut TestAppContext) {
    let (app, visual, editor) = query_tab_app(cx);

    type_in_editor(visual, "s");
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open && labels.len() > 5, "{labels:?}");

    // Maximum suggestions.
    apply_editor_setting(&app, visual, |editor| editor.autocomplete_max_items = 5);
    type_in_editor(visual, "s");
    let (open, labels, _) = menu_state(&editor, visual);
    assert!(open);
    assert_eq!(labels.len(), 5, "{labels:?}");
    apply_editor_setting(&app, visual, |editor| editor.autocomplete_max_items = 50);

    // A space after an accepted keyword.
    apply_editor_setting(&app, visual, |editor| {
        editor.autocomplete_insert_space = true
    });
    type_in_editor(visual, "select 1 wher");
    let (_, labels, _) = menu_state(&editor, visual);
    assert_eq!(
        labels.first().map(String::as_str),
        Some("WHERE"),
        "{labels:?}"
    );
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select 1 where ");
    apply_editor_setting(&app, visual, |editor| {
        editor.autocomplete_insert_space = false
    });

    // Off: typing opens no list.
    apply_editor_setting(&app, visual, |editor| {
        editor.autocomplete_auto_trigger = false
    });
    type_in_editor(visual, "select * from ord");
    assert!(!menu_state(&editor, visual).0, "no list while typing");
    apply_editor_setting(&app, visual, |editor| {
        editor.autocomplete_auto_trigger = true
    });
    type_in_editor(visual, "select * from ord");
    assert!(menu_state(&editor, visual).0, "back on");
}

#[test]
fn the_grid_shows_the_configured_null_text_and_real_values_untouched() {
    use arrow_array::{ArrayRef, RecordBatch, StringArray};
    let batch = RecordBatch::try_from_iter([(
        "name",
        std::sync::Arc::new(StringArray::from(vec![Some("a"), None])) as ArrayRef,
    )])
    .unwrap();
    let grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
        std::sync::Arc::new(batch),
    ])));
    let mut settings = Settings::default();
    let cell = |settings: &Settings, row: usize| {
        let (text, null) = settings_state::GridDisplay::from_settings(settings)
            .cell_text(&grid, CellPosition { row, column: 0 });
        (text.to_string(), null)
    };
    assert_eq!(cell(&settings, 1), ("NULL".to_string(), true));
    settings.results.null_text = "\u{2205}".into();
    assert_eq!(cell(&settings, 1), ("\u{2205}".to_string(), true));
    assert_eq!(cell(&settings, 0), ("a".to_string(), false));
    // The grid layout follows the column settings, within their limits.
    settings.results.column_width_default = 200;
    settings.results.column_width_min = 100;
    settings.results.column_width_max = 220;
    settings.results.show_row_numbers = false;
    let display = settings_state::GridDisplay::from_settings(&settings);
    let layout = crate::grid::ColumnLayout::with_limits(
        2,
        display.column_width_default,
        display.column_width_min,
        display.column_width_max,
        settings_state::gutter_width(&display),
    );
    assert_eq!(layout.widths, vec![200.0, 200.0]);
    assert_eq!(
        layout.gutter_width,
        crate::grid::ColumnLayout::HIDDEN_GUTTER
    );
    let mut layout = layout;
    layout.set_width(0, 500.0);
    layout.set_width(1, 10.0);
    assert_eq!(
        layout.widths,
        vec![220.0, 100.0],
        "resizes stay inside min and max"
    );
    // Auto-size: text of 10 chars at 12 px is about 90 px, clamped into the limits.
    layout.fit_column(0, 10, 12.0);
    assert_eq!(layout.widths[0], 100.0, "never below the minimum");
    layout.fit_column(0, 25, 12.0);
    assert_eq!(layout.widths[0], 198.0);
    layout.fit_column(0, 400, 12.0);
    assert_eq!(layout.widths[0], 220.0, "never above the maximum");
}

#[gpui::test]
fn sort_and_tab_bar_choices_flow_between_the_sidebar_and_the_settings(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::{ConnectionSort, TabGroupBy};
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let prefs = std::env::temp_dir().join(format!("tablex-sidebar-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, _| this.sidebar.path = Some(prefs.clone()));

    // Settings -> sidebar: Apply pushes the choices into the preferences.
    let mut next = applied(&app, visual);
    next.navigation.connection_sort = ConnectionSort::Descending;
    next.appearance.tab_group_by = TabGroupBy::Database;
    next.appearance.compact_tab_titles = true;
    app.update(visual, |this, cx| this.apply_settings(next, None, cx));
    app.read_with(visual, |this, _| {
        let prefs = this.navigator.preferences();
        assert_eq!(prefs.sort, ConnectionSort::Descending);
        assert_eq!(prefs.tab_group_by, TabGroupBy::Database);
        assert!(prefs.compact_tab_titles);
        assert!(this.shell.tab_bar.compact_titles);
    });
    let saved = crate::sidebar_preferences::SidebarPreferences::load(&prefs).unwrap();
    assert_eq!(
        saved.sort,
        ConnectionSort::Descending,
        "persisted in the sidebar file"
    );

    // Sidebar -> settings: a sort picked in the sidebar menu shows up in Settings.
    app.update(visual, |this, cx| {
        this.sort_sidebar_connections(ConnectionSort::Ascending, cx)
            .unwrap();
        this.set_pane_group_by(this.workspace.focused_pane(), TabGroupBy::Connection, cx);
        this.set_compact_tab_titles(false, cx);
    });
    let now = applied(&app, visual);
    assert_eq!(now.navigation.connection_sort, ConnectionSort::Ascending);
    assert_eq!(now.appearance.tab_group_by, TabGroupBy::Connection);
    assert!(!now.appearance.compact_tab_titles);

    // An open page with no edits follows; one with an edit keeps it.
    open_settings_page(&app, visual);
    settings_nav(visual, "navigation");
    toggle_setting(visual, "navigation.global_search");
    app.update(visual, |this, cx| {
        this.sort_sidebar_connections(ConnectionSort::Manual, cx)
            .unwrap();
    });
    let draft = draft_of(&app, visual);
    assert_eq!(
        draft.navigation.connection_sort,
        ConnectionSort::Manual,
        "untouched draft values follow"
    );
    assert!(draft.navigation.global_search, "an edited value stays");
    std::fs::remove_file(prefs).ok();
}

#[gpui::test]
fn navigation_settings_drive_the_tree_defaults_and_open_actions(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let mut next = applied(&app, visual);
    next.navigation.show_system_objects = false;
    next.navigation.navigator_page_size = 25;
    app.update(visual, |this, cx| this.apply_settings(next, None, cx));
    app.read_with(visual, |this, _| {
        assert!(this.navigator.hide_system_by_default);
        assert_eq!(this.navigator.provider_page_size(), 25);
    });
    let mut back = applied(&app, visual);
    back.navigation.show_system_objects = true;
    back.navigation.navigator_page_size = 200;
    app.update(visual, |this, cx| this.apply_settings(back, None, cx));
    app.read_with(visual, |this, _| {
        assert!(!this.navigator.hide_system_by_default);
        assert_eq!(
            this.navigator.provider_page_size(),
            crate::navigator::PROVIDER_PAGE_SIZE
        );
    });
}

#[gpui::test]
fn first_launch_imports_the_sidebar_choices_and_the_old_copy_default(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::{ConnectionSort, TabGroupBy};
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let dir = std::env::temp_dir().join(format!("tablex-first-launch-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let sidebar = dir.join("sidebar-preferences.json");
    std::fs::write(
        &sidebar,
        r#"{"sort": "descending", "global_search": true, "tab_group_by": "connection",
            "compact_tab_titles": true, "tab_rows": true}"#,
    )
    .unwrap();
    let settings_path = dir.join("settings.json");
    app.update(visual, |this, cx| {
        this.sidebar.path = Some(sidebar.clone());
        this.settings_ui.path = Some(settings_path.clone());
        this.settings = std::sync::Arc::new(Settings::default());
        // The copy default used to live in its own file.
        this.grid_prefs.copy_default = results::copy::CopyFormat::Csv;
        this.init_settings(cx);
    });
    let settings = applied(&app, visual);
    assert_eq!(
        settings.navigation.connection_sort,
        ConnectionSort::Descending
    );
    assert!(settings.navigation.global_search);
    assert_eq!(settings.appearance.tab_group_by, TabGroupBy::Connection);
    assert!(settings.appearance.compact_tab_titles && settings.appearance.tab_rows);
    assert_eq!(
        settings.results.default_copy_format,
        crate::settings::CopyDefault::Csv
    );
    // The file was written right away (it is a first run), by the background writer.
    wait_until(visual, "the first settings file", |_| {
        settings_path.exists()
    });
    let on_disk =
        crate::settings::store::load(&settings_path, &crate::settings::store::Legacy::default());
    assert_eq!(
        on_disk.settings.navigation.connection_sort,
        ConnectionSort::Descending
    );
    // The sidebar file keeps its own data untouched.
    assert!(std::fs::read_to_string(&sidebar)
        .unwrap()
        .contains("descending"));
    std::fs::remove_dir_all(dir).unwrap();
}

// ---- theme (S4) -----------------------------------------------------------------

/// Opens the theme dropdown, which starts on the draft's current value, and
/// walks to another option with `keys`.
fn pick_theme(visual: &mut gpui::VisualTestContext, keys: &str) {
    click(visual, "setting-control-appearance.theme".into());
    visual.simulate_keystrokes(keys);
    settle(visual);
}

fn component_is_dark(visual: &mut gpui::VisualTestContext) -> bool {
    visual.update(|_, cx| gpui_component::theme::Theme::global(cx).is_dark())
}

#[gpui::test]
fn the_theme_switches_dark_light_system_on_apply_and_system_follows_the_os(
    cx: &mut TestAppContext,
) {
    use crate::settings::ThemeChoice;
    use gpui::WindowAppearance as Os;
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    visual.update(|window, cx| AppRoot::follow_system_appearance(&app, window, cx));
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Dark);
    open_settings_page(&app, visual);
    settings_nav(visual, "appearance");
    assert_eq!(draft_of(&app, visual).appearance.theme, ThemeChoice::Dark);

    // Dark -> Light: a draft changes nothing until Apply, then both the TableX
    // palette and the component theme switch.
    pick_theme(visual, "down enter");
    assert_eq!(draft_of(&app, visual).appearance.theme, ThemeChoice::Light);
    assert!(component_is_dark(visual), "still a draft");
    assert_eq!(theme::background(), theme::palette().background);
    let dark_background = theme::background();
    click(visual, "settings-apply".into());
    assert_eq!(applied(&app, visual).appearance.theme, ThemeChoice::Light);
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Light);
    assert!(!component_is_dark(visual));
    assert_ne!(theme::background(), dark_background);

    // Light -> System while the OS is dark: Apply adopts the OS appearance.
    pick_theme(visual, "down enter");
    assert_eq!(draft_of(&app, visual).appearance.theme, ThemeChoice::System);
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Light);
    app.update(visual, |this, cx| {
        this.system_appearance_changed(Os::Dark, cx)
    });
    assert_eq!(
        theme::mode(),
        gpui_component::theme::ThemeMode::Light,
        "Light ignores the OS"
    );
    // The test window reports a light OS: Apply resolves against it.
    click(visual, "settings-apply".into());
    assert_eq!(applied(&app, visual).appearance.theme, ThemeChoice::System);
    assert_eq!(
        theme::mode(),
        gpui_component::theme::ThemeMode::Light,
        "OS is light"
    );

    // Under System the theme follows live appearance changes both ways.
    app.update(visual, |this, cx| {
        this.system_appearance_changed(Os::Dark, cx)
    });
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Dark);
    assert!(component_is_dark(visual));
    assert_eq!(theme::background(), dark_background);
    app.update(visual, |this, cx| {
        this.system_appearance_changed(Os::VibrantLight, cx)
    });
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Light);
    assert!(!component_is_dark(visual));

    // Back to Dark: the OS no longer matters.
    pick_theme(visual, "up up enter");
    assert_eq!(draft_of(&app, visual).appearance.theme, ThemeChoice::Dark);
    click(visual, "settings-apply".into());
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Dark);
    app.update(visual, |this, cx| {
        this.system_appearance_changed(Os::Light, cx)
    });
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Dark);
}

#[gpui::test]
fn a_saved_theme_applies_at_startup_and_the_user_fonts_survive_a_switch(cx: &mut TestAppContext) {
    use crate::settings::ThemeChoice;
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        next.appearance.theme = ThemeChoice::Light;
        next.appearance.interface_font = "Menlo".into();
        this.apply_settings(next, None, cx);
    });
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Light);
    let font = |visual: &mut gpui::VisualTestContext| {
        visual.update(|_, cx| {
            gpui_component::theme::Theme::global(cx)
                .font_family
                .to_string()
        })
    };
    assert_eq!(font(visual), "Menlo");
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        next.appearance.theme = ThemeChoice::Dark;
        this.apply_settings(next, None, cx);
    });
    assert_eq!(theme::mode(), gpui_component::theme::ThemeMode::Dark);
    assert_eq!(
        font(visual),
        "Menlo",
        "re-deriving the theme keeps the chosen font"
    );
}

// ---- Settings > AI Assistant -----------------------------------------------------------------

fn type_setting(visual: &mut gpui::VisualTestContext, control: &str, text: &str) {
    click(visual, format!("setting-control-{control}"));
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

#[gpui::test]
fn the_ai_page_shows_the_key_row_and_every_setting_with_its_default(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    assert!(rendered(visual, "settings-nav-ai".into()));
    settings_nav(visual, "ai");
    for selector in [
        "ai-key-row",
        "ai-key-input",
        "ai-key-save",
        "ai-key-remove",
        "ai-key-status",
        "ai-models-refresh",
        "setting-control-ai.provider",
        "setting-control-ai.model",
        "setting-control-ai.effort",
        "setting-control-ai.fallback",
        "setting-control-ai.mcp_writes",
        "setting-control-ai.base_url",
    ] {
        assert!(rendered(visual, selector.into()), "{selector}");
    }
    let ai = draft_of(&app, visual).ai;
    assert_eq!(ai, crate::settings::AiSettings::default());
    assert_eq!(
        (ai.selected().model.as_str(), ai.fallback),
        ("claude-opus-5-5", true)
    );
    // Nothing is saved yet, and that is what the row says once the store was asked.
    wait_until(visual, "the key check", |visual| {
        app.read_with(visual, |this, _| {
            this.assistant.key_state_label(AiProvider::Anthropic) == "Not set"
        })
    });
}

#[gpui::test]
fn the_mcp_switch_is_off_by_default_and_apply_persists_it_where_the_mcp_server_reads_it(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    assert!(rendered(visual, "setting-ai.mcp_writes".into()));
    assert!(rendered(visual, "setting-control-ai.mcp_writes".into()));
    assert!(!Settings::default().ai.mcp_writes);
    assert!(!draft_of(&app, visual).ai.mcp_writes, "off by default");
    assert!(!applied(&app, visual).ai.mcp_writes);

    // The draft changes at once, the app's settings only on Apply.
    toggle_setting(visual, "ai.mcp_writes");
    assert!(draft_of(&app, visual).ai.mcp_writes);
    assert!(!applied(&app, visual).ai.mcp_writes);
    click(visual, "settings-apply".into());
    assert!(applied(&app, visual).ai.mcp_writes);
    // Persisted after the debounce, and `tablex-mcp`'s reader sees it in that very file.
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the switch on disk", |_| {
        engine_connect::profile::mcp_writes_enabled(Some(&path))
    });
    let saved = crate::settings::store::load(&path, &crate::settings::store::Legacy::default());
    assert!(saved.settings.ai.mcp_writes);
    assert_eq!(
        saved.settings.schema_version,
        crate::settings::SCHEMA_VERSION
    );

    // Back off: Apply again, and the file says so at once, not after the 300 ms debounce (a
    // client's yes in that window must not run a write): no clock advance, no waiting.
    toggle_setting(visual, "ai.mcp_writes");
    click(visual, "settings-apply".into());
    assert!(!applied(&app, visual).ai.mcp_writes);
    assert!(
        std::fs::read_to_string(&path).is_ok_and(|text| text.contains(r#""mcp_writes": false"#))
    );
    assert!(!engine_connect::profile::mcp_writes_enabled(Some(&path)));
    assert!(
        app.read_with(visual, |this, _| this.settings_ui.pending.is_none()),
        "no debounce is left running"
    );
    // The reset button puts the default back in the draft.
    toggle_setting(visual, "ai.mcp_writes");
    assert!(draft_of(&app, visual).ai.mcp_writes);
    click(visual, "setting-reset-ai.mcp_writes".into());
    assert!(!draft_of(&app, visual).ai.mcp_writes);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[gpui::test]
fn a_failed_write_of_the_mcp_switch_off_is_said_and_tried_again(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    toggle_setting(visual, "ai.mcp_writes");
    click(visual, "settings-apply".into());
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the switch on disk", |_| {
        engine_connect::profile::mcp_writes_enabled(Some(&path))
    });

    // The file can no longer be replaced: a non-empty directory sits where it is (the rename of
    // the new file onto it fails), whatever the platform.
    let kept = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir_all(path.join("blocker")).unwrap();
    let errors_before = app.read_with(visual, |this, _| this.sidebar.notice.clone());
    assert_eq!(errors_before, None);

    toggle_setting(visual, "ai.mcp_writes");
    click(visual, "settings-apply".into());
    assert!(!applied(&app, visual).ai.mcp_writes, "the page says off");
    let notice = app
        .read_with(visual, |this, _| this.sidebar.notice.clone())
        .expect("the failure is shown");
    assert!(
        notice.contains("Could not save the settings")
            && notice.contains("still ON in the settings file"),
        "{notice}"
    );
    assert!(
        app.read_with(visual, |this, _| this.settings_ui.pending.is_some()),
        "a retry is pending"
    );

    // The obstacle goes; the retry writes `false`.
    std::fs::remove_dir_all(&path).unwrap();
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the retried write", |_| {
        std::fs::read_to_string(&path).is_ok_and(|text| text.contains(r#""mcp_writes": false"#))
    });
    assert!(!engine_connect::profile::mcp_writes_enabled(Some(&path)));
    assert!(kept.contains(r#""mcp_writes": true"#));
    // The notice is no longer true once the write landed, so it is taken back.
    wait_until(visual, "the notice to go", |cx| {
        app.read_with(cx, |this, _| this.sidebar.notice.is_none())
    });
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[gpui::test]
fn a_retry_of_the_mcp_switch_off_that_fails_too_still_says_the_switch_is_on(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    toggle_setting(visual, "ai.mcp_writes");
    click(visual, "settings-apply".into());
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the switch on disk", |_| {
        engine_connect::profile::mcp_writes_enabled(Some(&path))
    });
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir_all(path.join("blocker")).unwrap();

    toggle_setting(visual, "ai.mcp_writes");
    click(visual, "settings-apply".into());
    let first = app
        .read_with(visual, |this, _| this.sidebar.notice.clone())
        .expect("the failure is shown");
    assert!(first.contains("TableX will try again"), "{first}");

    // The obstacle stays: the retry fails as well, and the notice still names the switch.
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the retry to fail", |cx| {
        app.read_with(cx, |this, _| {
            this.sidebar
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("next settings change"))
        })
    });
    let second = app
        .read_with(visual, |this, _| this.sidebar.notice.clone())
        .unwrap();
    assert!(second.contains("still ON in the settings file"), "{second}");
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[gpui::test]
fn the_model_and_effort_dropdowns_open_select_and_reopen_on_the_chosen_value(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    let selected = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &str| {
        app.read_with(visual, |this, cx| {
            let select = this.settings_ui.page.as_ref().unwrap().select_for_test(id);
            select
                .read(cx)
                .selected_value()
                .map(|label| label.to_string())
        })
    };
    assert_eq!(
        selected(&app, visual, "ai.model").as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(
        selected(&app, visual, "ai.effort").as_deref(),
        Some("Medium")
    );

    click(visual, "setting-control-ai.model".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(
        draft_of(&app, visual).ai.selected().model,
        "claude-sonnet-5-5"
    );
    assert_eq!(
        applied(&app, visual).ai.selected().model,
        "claude-opus-5-5",
        "still a draft"
    );
    assert_eq!(
        selected(&app, visual, "ai.model").as_deref(),
        Some("claude-sonnet-5-5")
    );
    // Reopen and dismiss: nothing changes.
    click(visual, "setting-control-ai.model".into());
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(
        draft_of(&app, visual).ai.selected().model,
        "claude-sonnet-5-5"
    );

    click(visual, "setting-control-ai.effort".into());
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).ai.effort, engine_ai::Effort::XHigh);
    click(visual, "setting-reset-ai.model".into());
    assert_eq!(
        draft_of(&app, visual).ai.selected().model,
        "claude-opus-5-5"
    );
    assert_eq!(
        selected(&app, visual, "ai.model").as_deref(),
        Some("claude-opus-5-5")
    );

    toggle_setting(visual, "ai.fallback");
    assert!(!draft_of(&app, visual).ai.fallback);
    click(visual, "settings-apply".into());
    let ai = applied(&app, visual).ai;
    assert_eq!((ai.effort, ai.fallback), (engine_ai::Effort::XHigh, false));
}

#[gpui::test]
fn a_base_url_that_would_send_the_key_in_clear_text_blocks_apply(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    type_setting(visual, "ai.base_url", "http://api.example.com");
    assert!(rendered(visual, "setting-error-ai.base_url".into()));
    assert!(app.read_with(visual, |this, _| this
        .settings_ui
        .page
        .as_ref()
        .unwrap()
        .has_errors()));
    click(visual, "settings-apply".into());
    assert_eq!(
        applied(&app, visual).ai.selected().base_url,
        "https://api.anthropic.com",
        "refused"
    );

    type_setting(visual, "ai.base_url", "http://localhost:8080");
    assert!(!rendered(visual, "setting-error-ai.base_url".into()));
    click(visual, "settings-apply".into());
    assert_eq!(
        applied(&app, visual).ai.selected().base_url,
        "http://localhost:8080",
        "this machine is fine"
    );
}

#[gpui::test]
fn the_key_is_saved_to_the_store_at_once_and_never_shown_again(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    wait_until(visual, "the key check", |visual| {
        app.read_with(visual, |this, _| {
            this.assistant.key_state_label(AiProvider::Anthropic) == "Not set"
        })
    });

    click(visual, "ai-key-input".into());
    visual.simulate_input("sk-ui-test-key-9876");
    settle(visual);
    click(visual, "ai-key-save".into());
    wait_until(visual, "the key to be saved", |visual| {
        app.read_with(visual, |this, _| {
            this.assistant.key_state_label(AiProvider::Anthropic) == "Saved"
        })
    });
    assert_eq!(
        app.read_with(visual, |this, _| this.assistant.keys.is_set()),
        Ok(true)
    );
    // It did not go through Apply, and the input is empty: nothing shows it again.
    assert!(!app.read_with(visual, |this, _| this.settings_dirty()));
    let typed = app.read_with(visual, |this, cx| {
        this.settings_ui
            .page
            .as_ref()
            .unwrap()
            .ai
            .key_input
            .as_ref()
            .unwrap()
            .read(cx)
            .value()
            .to_string()
    });
    assert_eq!(typed, "", "the key left the input");
    // Whatever else gets saved, the file has no key.
    crate::settings::store::save(&path, &applied(&app, visual)).unwrap();
    let file = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(file.contains("\"ai\""), "{file}");
    assert!(
        !file.contains("sk-ui-test"),
        "the key is not in settings.json: {file}"
    );

    click(visual, "ai-key-remove".into());
    wait_until(visual, "the key to be removed", |visual| {
        app.read_with(visual, |this, _| {
            this.assistant.key_state_label(AiProvider::Anthropic) == "Not set"
        })
    });
    assert_eq!(
        app.read_with(visual, |this, _| this.assistant.keys.is_set()),
        Ok(false)
    );
    // An empty box is refused with a sentence, not stored.
    click(visual, "ai-key-save".into());
    wait_until(visual, "the refusal", |visual| {
        rendered(visual, "ai-key-error".into())
    });
    assert_eq!(
        app.read_with(visual, |this, _| this.assistant.keys.is_set()),
        Ok(false)
    );
}

#[gpui::test]
fn refresh_loads_the_provider_models_into_the_picker(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let address = assistant::tests::serve_models();
    app.update(visual, |this, _| {
        this.assistant.keys.save("sk-ui-test-key-9876").unwrap()
    });
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    type_setting(visual, "ai.base_url", &format!("http://{address}"));
    click(visual, "ai-models-refresh".into());
    wait_until(visual, "the models", |visual| {
        app.read_with(visual, |this, _| this.assistant.models_version == 1)
    });
    let models = app.read_with(visual, |this, _| {
        this.assistant.models_for(AiProvider::Anthropic)
    });
    assert_eq!(models.len(), 5, "{models:?}");
    assert_eq!(models.last().map(String::as_str), Some("claude-new-9"));

    // The fifth entry of the opened dropdown is the new model; choose it.
    click(visual, "setting-control-ai.model".into());
    visual.simulate_keystrokes("down down down down enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).ai.selected().model, "claude-new-9");
    // Without a key the refresh says why.
    app.update(visual, |this, _| this.assistant.keys.remove().unwrap());
    click(visual, "ai-models-refresh".into());
    wait_until(visual, "the refusal", |visual| {
        app.read_with(visual, |this, _| {
            this.assistant.models_error(AiProvider::Anthropic).is_some()
        })
    });
}

// ---- Settings > AI Assistant: the provider -----------------------------------------------------

fn shown_label(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &str,
) -> Option<String> {
    app.read_with(visual, |this, cx| {
        let select = this.settings_ui.page.as_ref().unwrap().select_for_test(id);
        select
            .read(cx)
            .selected_value()
            .map(|label| label.to_string())
    })
}

fn typed_text(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &str,
) -> String {
    app.read_with(visual, |this, cx| {
        this.settings_ui
            .page
            .as_ref()
            .unwrap()
            .input_text_for_test(id, cx)
    })
}

/// Opens the provider dropdown, which starts on the draft's provider, and moves `keys` from there.
fn pick_provider(visual: &mut gpui::VisualTestContext, keys: &str) {
    click(visual, "setting-control-ai.provider".into());
    visual.simulate_keystrokes(keys);
    settle(visual);
}

#[gpui::test]
fn the_provider_dropdown_opens_selects_and_the_rows_below_follow(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    assert_eq!(
        shown_label(&app, visual, "ai.provider").as_deref(),
        Some("Anthropic")
    );
    assert_eq!(
        shown_label(&app, visual, "ai.model").as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(
        typed_text(&app, visual, "ai.base_url"),
        "https://api.anthropic.com"
    );
    assert!(!app.read_with(visual, |this, _| this.settings_dirty()));

    // Open and dismiss: nothing changes.
    pick_provider(visual, "escape");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Anthropic);

    // Open, one down, confirm: OpenAI. The model and the address are OpenAI's, at once.
    pick_provider(visual, "down enter");
    let draft = draft_of(&app, visual);
    assert_eq!(draft.ai.provider, AiProvider::OpenAi);
    assert_eq!(
        applied(&app, visual).ai.provider,
        AiProvider::Anthropic,
        "still a draft"
    );
    assert!(
        app.read_with(visual, |this, _| this.settings_dirty()),
        "Changes not applied"
    );
    assert_eq!(
        shown_label(&app, visual, "ai.provider").as_deref(),
        Some("OpenAI")
    );
    assert_eq!(
        shown_label(&app, visual, "ai.model"),
        None,
        "no model is chosen yet"
    );
    assert_eq!(draft.ai.selected().model, "");
    assert_eq!(
        typed_text(&app, visual, "ai.base_url"),
        "https://api.openai.com/v1"
    );
    assert_eq!(
        draft.ai.providers.anthropic.model, "claude-opus-5-5",
        "Anthropic keeps its own"
    );
    assert!(rendered(visual, "setting-reset-ai.provider".into()));

    // Reopened, the list starts on OpenAI: one down is the compatible server, two down is Ollama
    // (all four options are there). A compatible server has no address until it is given one.
    pick_provider(visual, "down enter");
    assert_eq!(
        draft_of(&app, visual).ai.provider,
        AiProvider::OpenAiCompatible
    );
    assert_eq!(
        shown_label(&app, visual, "ai.provider").as_deref(),
        Some("OpenAI-compatible")
    );
    assert_eq!(typed_text(&app, visual, "ai.base_url"), "");
    assert!(rendered(visual, "setting-error-ai.base_url".into()));
    click(visual, "settings-apply".into());
    assert_eq!(
        applied(&app, visual).ai.provider,
        AiProvider::Anthropic,
        "refused"
    );
    pick_provider(visual, "down enter");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Ollama);
    assert_eq!(
        typed_text(&app, visual, "ai.base_url"),
        "http://localhost:11434/v1"
    );
    assert!(
        !rendered(visual, "setting-error-ai.base_url".into()),
        "the error was the other one's"
    );
    assert_eq!(shown_label(&app, visual, "ai.model"), None);

    // Reset puts the provider, and with it the rows, back.
    click(visual, "setting-reset-ai.provider".into());
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Anthropic);
    assert_eq!(
        shown_label(&app, visual, "ai.provider").as_deref(),
        Some("Anthropic")
    );
    assert_eq!(
        shown_label(&app, visual, "ai.model").as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(
        typed_text(&app, visual, "ai.base_url"),
        "https://api.anthropic.com"
    );
    assert!(
        !app.read_with(visual, |this, _| this.settings_dirty()),
        "back to what is applied"
    );
}

#[gpui::test]
fn what_is_typed_for_one_provider_does_not_reach_another_and_apply_keeps_it(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    type_setting(
        visual,
        "ai.base_url",
        "https://anthropic-gateway.example.com",
    );
    pick_provider(visual, "down down down enter");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Ollama);
    // Ollama shows its own address, not the gateway typed for Anthropic.
    assert_eq!(
        typed_text(&app, visual, "ai.base_url"),
        "http://localhost:11434/v1"
    );
    type_setting(visual, "ai.base_url", "http://127.0.0.1:9999/v1");
    let draft = draft_of(&app, visual);
    assert_eq!(
        draft.ai.providers.ollama.base_url,
        "http://127.0.0.1:9999/v1"
    );
    assert_eq!(
        draft.ai.providers.anthropic.base_url,
        "https://anthropic-gateway.example.com"
    );
    assert_eq!(
        draft.ai.providers.openai.base_url,
        "https://api.openai.com/v1"
    );

    // Back on Anthropic the gateway is shown again; Apply keeps both entries.
    pick_provider(visual, "up up up enter");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Anthropic);
    assert_eq!(
        typed_text(&app, visual, "ai.base_url"),
        "https://anthropic-gateway.example.com"
    );
    click(visual, "settings-apply".into());
    let ai = applied(&app, visual).ai;
    assert_eq!(ai.provider, AiProvider::Anthropic);
    assert_eq!(ai.providers.ollama.base_url, "http://127.0.0.1:9999/v1");
    assert_eq!(
        ai.selected().base_url,
        "https://anthropic-gateway.example.com"
    );
}

#[gpui::test]
fn the_key_row_is_the_selected_providers_and_keys_never_cross(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    let label = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, provider| {
        app.read_with(visual, |this, _| this.assistant.key_state_label(provider))
    };
    let is_set = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, provider| {
        app.read_with(visual, |this, _| {
            this.assistant.keys.for_provider(provider).is_set()
        })
    };
    wait_until(visual, "the Anthropic check", |visual| {
        label(&app, visual, AiProvider::Anthropic) == "Not set"
    });

    // A key saved for Anthropic ...
    click(visual, "ai-key-input".into());
    visual.simulate_input("sk-ant-1");
    settle(visual);
    click(visual, "ai-key-save".into());
    wait_until(visual, "the Anthropic key", |visual| {
        label(&app, visual, AiProvider::Anthropic) == "Saved"
    });

    // ... is not OpenAI's: selecting OpenAI, the row asks about OpenAI's own.
    pick_provider(visual, "down enter");
    wait_until(visual, "the OpenAI check", |visual| {
        label(&app, visual, AiProvider::OpenAi) == "Not set"
    });
    assert_eq!(is_set(&app, visual, AiProvider::OpenAi), Ok(false));
    assert_eq!(is_set(&app, visual, AiProvider::Anthropic), Ok(true));
    // Saving here is OpenAI's; removing here removes OpenAI's only.
    click(visual, "ai-key-input".into());
    visual.simulate_input("sk-openai-1");
    settle(visual);
    click(visual, "ai-key-save".into());
    wait_until(visual, "the OpenAI key", |visual| {
        label(&app, visual, AiProvider::OpenAi) == "Saved"
    });
    assert_eq!(is_set(&app, visual, AiProvider::OpenAi), Ok(true));
    click(visual, "ai-key-remove".into());
    wait_until(visual, "the OpenAI removal", |visual| {
        label(&app, visual, AiProvider::OpenAi) == "Not set"
    });
    assert_eq!(
        is_set(&app, visual, AiProvider::Anthropic),
        Ok(true),
        "Anthropic's stays"
    );
    assert_eq!(label(&app, visual, AiProvider::Anthropic), "Saved");

    // Ollama: the key is optional, and says so.
    pick_provider(visual, "down down enter");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Ollama);
    wait_until(visual, "the Ollama check", |visual| {
        label(&app, visual, AiProvider::Ollama) == "Not set (optional)"
    });
    assert_eq!(is_set(&app, visual, AiProvider::Ollama), Ok(false));
    // Back on Anthropic the row is Anthropic's again.
    click(visual, "setting-reset-ai.provider".into());
    assert_eq!(label(&app, visual, AiProvider::Anthropic), "Saved");
}

#[gpui::test]
fn the_fallback_row_is_off_with_a_reason_unless_the_provider_is_anthropic(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    assert!(!rendered(visual, "setting-unavailable-ai.fallback".into()));
    toggle_setting(visual, "ai.fallback");
    assert!(!draft_of(&app, visual).ai.fallback, "a switch that works");
    toggle_setting(visual, "ai.fallback");
    assert!(draft_of(&app, visual).ai.fallback);

    pick_provider(visual, "down down down enter");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Ollama);
    assert!(
        rendered(visual, "setting-unavailable-ai.fallback".into()),
        "the reason is on the row"
    );
    toggle_setting(visual, "ai.fallback");
    assert!(draft_of(&app, visual).ai.fallback, "the click did nothing");

    click(visual, "setting-reset-ai.provider".into());
    assert!(!rendered(visual, "setting-unavailable-ai.fallback".into()));
}

#[gpui::test]
fn refresh_asks_the_selected_provider_and_its_models_stay_its_own(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let address = assistant::tests::serve_models();
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    // A compatible server on this machine, no key: the list is its own, with nothing built in.
    pick_provider(visual, "down down enter");
    assert_eq!(
        draft_of(&app, visual).ai.provider,
        AiProvider::OpenAiCompatible
    );
    type_setting(visual, "ai.base_url", &format!("http://{address}/v1"));
    click(visual, "ai-models-refresh".into());
    wait_until(visual, "the models", |visual| {
        app.read_with(visual, |this, _| this.assistant.models_version == 1)
    });
    assert_eq!(
        app.read_with(visual, |this, _| this
            .assistant
            .models_for(AiProvider::OpenAiCompatible)),
        ["claude-new-9", "claude-opus-5-5"]
    );
    // The list is the provider's own: Anthropic still has its built-in list only.
    assert_eq!(
        app.read_with(visual, |this, _| this
            .assistant
            .models_for(AiProvider::Anthropic)
            .len()),
        4
    );
    // Nothing was chosen, so the opened list (the helper under the typed id) starts at the top:
    // its first entry is chosen, for this provider only, and the typed box shows it.
    click(visual, "setting-pick-ai.model".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    let draft = draft_of(&app, visual);
    assert_eq!(draft.ai.providers.openai_compatible.model, "claude-new-9");
    assert_eq!(
        shown_label(&app, visual, "ai.model").as_deref(),
        Some("claude-new-9")
    );
    assert_eq!(draft.ai.providers.openai.model, "");

    // Anthropic's rows show Anthropic's built-in list, not the server's.
    click(visual, "setting-reset-ai.provider".into());
    assert_eq!(
        shown_label(&app, visual, "ai.model").as_deref(),
        Some("claude-opus-5-5")
    );
    click(visual, "setting-control-ai.model".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(
        draft_of(&app, visual).ai.selected().model,
        "claude-sonnet-5-5"
    );
    assert_eq!(
        draft_of(&app, visual).ai.providers.openai_compatible.model,
        "claude-new-9"
    );
}

#[gpui::test]
fn a_key_typed_for_one_provider_is_not_saved_as_another_providers(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    let typed = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| {
            this.settings_ui
                .page
                .as_ref()
                .unwrap()
                .ai
                .key_input
                .as_ref()
                .map(|input| input.read(cx).value().to_string())
        })
    };
    let nothing_stored = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        AiProvider::ALL.into_iter().all(|provider| {
            app.read_with(visual, |this, _| {
                this.assistant.keys.for_provider(provider).is_set() == Ok(false)
            })
        })
    };

    // Typed for Anthropic, then the Provider select is changed: the box is empty, Save stores
    // nothing in any account and says so under OpenAI.
    click(visual, "ai-key-input".into());
    visual.simulate_input("sk-ant-typed-1");
    settle(visual);
    pick_provider(visual, "down enter");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::OpenAi);
    assert_eq!(
        typed(&app, visual).as_deref(),
        Some(""),
        "the typed key did not follow"
    );
    click(visual, "ai-key-save".into());
    wait_until(visual, "the refusal", |visual| {
        rendered(visual, "ai-key-error".into())
    });
    assert!(nothing_stored(&app, visual));
    assert!(app.read_with(visual, |this, _| this
        .assistant
        .key_error(AiProvider::OpenAi)
        .is_some()));

    // The refusal is OpenAI's: it is not under Anthropic's row, and Anthropic's is not under OpenAI's.
    click(visual, "setting-reset-ai.provider".into());
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Anthropic);
    assert!(!rendered(visual, "ai-key-error".into()));
    assert!(app.read_with(visual, |this, _| this
        .assistant
        .key_error(AiProvider::Anthropic)
        .is_none()));

    // Through Reset too: typed for OpenAI, Reset puts Anthropic back, and Save stores nothing.
    pick_provider(visual, "down enter");
    click(visual, "ai-key-input".into());
    visual.simulate_input("sk-openai-typed-2");
    settle(visual);
    click(visual, "setting-reset-ai.provider".into());
    assert_eq!(typed(&app, visual).as_deref(), Some(""));
    click(visual, "ai-key-save".into());
    wait_until(visual, "the second refusal", |visual| {
        rendered(visual, "ai-key-error".into())
    });
    assert!(nothing_stored(&app, visual));
}

#[gpui::test]
fn a_model_id_is_typed_for_the_other_providers_and_anthropics_row_stays_a_list(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    assert!(
        !rendered(visual, "setting-pick-ai.model".into()),
        "Anthropic: the list alone"
    );

    // A compatible server whose model list is empty: the id is typed, and Apply keeps it.
    pick_provider(visual, "down down enter");
    assert_eq!(
        draft_of(&app, visual).ai.provider,
        AiProvider::OpenAiCompatible
    );
    assert!(
        rendered(visual, "setting-pick-ai.model".into()),
        "the list is still there to pick from"
    );
    assert_eq!(typed_text(&app, visual, "ai.model"), "");
    type_setting(visual, "ai.base_url", "http://127.0.0.1:1234/v1");
    type_setting(visual, "ai.model", "my-local-model");
    assert_eq!(
        draft_of(&app, visual).ai.providers.openai_compatible.model,
        "my-local-model"
    );
    assert!(!rendered(visual, "setting-error-ai.model".into()));
    click(visual, "settings-apply".into());
    let ai = applied(&app, visual).ai;
    assert_eq!(ai.provider, AiProvider::OpenAiCompatible);
    assert_eq!(ai.selected().model, "my-local-model");
    assert_eq!(
        ai.providers.anthropic.model, "claude-opus-5-5",
        "the others are untouched"
    );

    // Clearing the box means no model chosen; it is not an error for these providers.
    click(visual, "setting-control-ai.model".into());
    visual.simulate_keystrokes("secondary-a backspace");
    settle(visual);
    assert_eq!(draft_of(&app, visual).ai.selected().model, "");
    assert!(!rendered(visual, "setting-error-ai.model".into()));

    // Another provider shows its own id; Anthropic's row is the list again.
    click(visual, "setting-reset-ai.provider".into());
    assert!(!rendered(visual, "setting-pick-ai.model".into()));
    assert_eq!(
        shown_label(&app, visual, "ai.model").as_deref(),
        Some("claude-opus-5-5")
    );
    click(visual, "setting-control-ai.model".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(
        draft_of(&app, visual).ai.selected().model,
        "claude-sonnet-5-5"
    );
}

#[gpui::test]
fn the_error_of_one_provider_is_not_shown_under_another(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    // Anthropic: no key, so Refresh fails and says so.
    click(visual, "ai-models-refresh".into());
    wait_until(visual, "the refusal", |visual| {
        app.read_with(visual, |this, _| {
            this.assistant.models_error(AiProvider::Anthropic).is_some()
        })
    });
    let note_is_error = |visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            this.assistant
                .models_error(this.settings_ui.page.as_ref().unwrap().draft.ai.provider)
                .is_some()
        })
    };
    assert!(note_is_error(visual));
    pick_provider(visual, "down down down enter");
    assert_eq!(draft_of(&app, visual).ai.provider, AiProvider::Ollama);
    assert!(!note_is_error(visual), "Ollama has no error of its own");
    assert!(app.read_with(visual, |this, _| !this
        .assistant
        .models_refreshing(AiProvider::Ollama)));
    click(visual, "setting-reset-ai.provider".into());
    assert!(note_is_error(visual), "and Anthropic's is still its own");
}

// ---- Settings > Editor > Format SQL ------------------------------------------------------------

/// A tools folder (a temp dir) for the page to look in; `installed` puts a SQL Formatter (sqruff)
/// build and its record there.
fn formatter_tools_dir(installed: bool) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let root = std::env::temp_dir().join(format!("tablex-format-tools-{}", uuid::Uuid::new_v4()));
    if installed {
        let dir = root.join("tablex-sqruff");
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "tablex-sqruff.exe" } else { "tablex-sqruff" };
        let bytes = b"not really sqruff";
        std::fs::write(dir.join(name), bytes).unwrap();
        let sha256: String = Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        std::fs::write(
            dir.join("installed.json"),
            serde_json::json!({"id": "tablex-sqruff", "version": "0.1.0", "sha256": sha256, "sizeBytes": bytes.len(), "installedAt": "2026-10-22T09:30:00Z"}).to_string(),
        )
        .unwrap();
    }
    root
}

fn use_formatter_tools(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, root: &std::path::Path) {
    app.update(visual, |this, _| {
        this.driver_ui.tools.store = Some(std::sync::Arc::new(engine_driver_host::ToolStore::new(root)));
        this.format_ui_reset_for_test();
    });
}

fn wait_for_format_scan(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    wait_until(visual, "the formatter scan", |visual| app.read_with(visual, |this, _| this.format_ui_scan_for_test().is_some()));
}

#[gpui::test]
fn the_formatter_is_chosen_and_the_external_command_typed_then_applied(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    use_formatter_tools(&app, visual, &formatter_tools_dir(false));
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    wait_for_format_scan(&app, visual);

    // Built-in is the default; the other rows are there, the command's are marked as unused.
    assert_eq!(draft_of(&app, visual).editor.formatter, crate::settings::FormatterChoice::BuiltIn);
    for selector in [
        "setting-control-editor.formatter",
        "setting-control-editor.format_external_program",
        "setting-control-editor.format_external_args",
        "setting-control-editor.format_timeout",
        "format-tool-status",
        "format-tool-open-driver-manager",
    ] {
        assert!(rendered(visual, selector.into()), "{selector}");
    }
    assert!(rendered(visual, "setting-unavailable-editor.format_external_program".into()));

    // SQL Formatter (sqruff) is not installed: choosing it is refused, with the way to get it.
    click(visual, "setting-control-editor.formatter".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).editor.formatter, crate::settings::FormatterChoice::BuiltIn, "refused");
    assert!(rendered(visual, "setting-error-editor.formatter".into()));
    click(visual, "format-tool-open-driver-manager".into());
    assert_eq!(active_kind(&app, visual), Some(TabKind::DriverManager));
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");

    // The external command: the third option, then the program and its arguments.
    click(visual, "setting-control-editor.formatter".into());
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).editor.formatter, crate::settings::FormatterChoice::External);
    assert!(!rendered(visual, "setting-error-editor.formatter".into()), "a new choice clears the refusal");
    assert!(!rendered(visual, "setting-unavailable-editor.format_external_program".into()), "now the command's rows are used");
    type_setting(visual, "editor.format_external_program", "sqlfluff");
    type_setting(visual, "editor.format_external_args", "format --dialect {dialect} -");
    type_setting(visual, "editor.format_timeout", "30");
    let draft = draft_of(&app, visual);
    assert_eq!(draft.editor.format_external_program, "sqlfluff");
    assert_eq!(draft.editor.format_external_args, "format --dialect {dialect} -");
    assert_eq!(draft.editor.format_timeout, 30);
    assert_eq!(applied(&app, visual).editor.formatter, crate::settings::FormatterChoice::BuiltIn, "still a draft");

    click(visual, "settings-apply".into());
    let saved = applied(&app, visual).editor;
    assert_eq!(saved.formatter, crate::settings::FormatterChoice::External);
    assert_eq!((saved.format_external_program.as_str(), saved.format_timeout), ("sqlfluff", 30));
    // The row shows it is changed, and a reset puts the default back.
    assert!(rendered(visual, "setting-reset-editor.formatter".into()));
    click(visual, "setting-reset-editor.formatter".into());
    assert_eq!(draft_of(&app, visual).editor.formatter, crate::settings::FormatterChoice::BuiltIn);
}

#[gpui::test]
fn the_formatter_tool_can_be_chosen_once_it_is_installed(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    use_formatter_tools(&app, visual, &formatter_tools_dir(true));
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    wait_for_format_scan(&app, visual);
    assert!(rendered(visual, "format-tool-status".into()));
    assert!(!rendered(visual, "format-tool-open-driver-manager".into()), "nothing to install");
    click(visual, "setting-control-editor.formatter".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).editor.formatter, crate::settings::FormatterChoice::Sqruff);
    assert!(!rendered(visual, "setting-error-editor.formatter".into()));
    // The select shows it when it is opened again.
    app.read_with(visual, |this, cx| {
        let select = this.settings_ui.page.as_ref().unwrap().select_for_test("editor.formatter");
        assert_eq!(select.read(cx).selected_value().map(|label| label.to_string()), Some("SQL Formatter (sqruff)".to_string()));
    });
    // Its row says which version is installed.
    let scan = app.read_with(visual, |this, _| this.format_ui_scan_for_test());
    assert!(matches!(scan.map(|scan| scan.tool), Some(super::settings_format::FormatterToolState::Installed { version }) if version == "0.1.0"));
}

#[gpui::test]
fn the_formatter_tool_can_be_chosen_right_after_the_driver_manager_installed_it(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    use_formatter_tools(&app, visual, &formatter_tools_dir(false));
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    wait_for_format_scan(&app, visual);
    assert!(app.read_with(visual, |this, _| this.formatter_tool_missing()), "not installed yet");
    // The Driver Manager finishes the install (its catalog says so before any rescan answers).
    app.update(visual, |this, _| this.seed_sql_formatter_fixture(driver_manager::FormatterSeed::Installed));
    assert!(app.read_with(visual, |this, _| !this.formatter_tool_missing()));
    click(visual, "setting-control-editor.formatter".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).editor.formatter, crate::settings::FormatterChoice::Sqruff);
    assert!(!rendered(visual, "setting-error-editor.formatter".into()));
    // And the page's own look is repeated after an install, so its status row follows.
    use_formatter_tools(&app, visual, &formatter_tools_dir(true));
    app.update(visual, |this, cx| this.scan_format_tools(cx));
    wait_for_format_scan(&app, visual);
    let scan = app.read_with(visual, |this, _| this.format_ui_scan_for_test());
    assert!(matches!(scan.map(|scan| scan.tool), Some(super::settings_format::FormatterToolState::Installed { .. })));
}

#[gpui::test]
fn a_preset_found_on_the_path_fills_the_command(cx: &mut TestAppContext) {
    use crate::sql_format::external::PRESETS;
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    use_formatter_tools(&app, visual, &formatter_tools_dir(false));
    open_settings_page(&app, visual);
    settings_nav(visual, "editor");
    wait_for_format_scan(&app, visual);
    // The machine's own PATH decides what a scan finds; the page is shown what a machine with
    // sqlfluff and pg_format would find.
    let found: Vec<_> = PRESETS.iter().copied().filter(|preset| matches!(preset.program, "sqlfluff" | "pg_format")).collect();
    app.update(visual, |this, cx| {
        this.format_ui_set_scan_for_test(super::settings_format::FormatScan {
            tool: super::settings_format::FormatterToolState::NotInstalled,
            presets: found,
        });
        cx.notify();
    });
    settle(visual);
    for selector in ["format-presets", "format-preset-sqlfluff-format", "format-preset-sqlfluff-fix", "format-preset-pg_format"] {
        assert!(rendered(visual, selector.into()), "{selector}");
    }
    assert!(!rendered(visual, "format-preset-sqruff".into()), "not on this PATH");

    click(visual, "format-preset-sqlfluff-format".into());
    let editor = draft_of(&app, visual).editor;
    assert_eq!(editor.formatter, crate::settings::FormatterChoice::External);
    assert_eq!(editor.format_external_program, "sqlfluff");
    assert_eq!(editor.format_external_args, "format --dialect {dialect} -");
    assert_eq!(editor.format_external_exit_codes, "0,1", "sqlfluff exits 1 when it leaves something unfixed");
    // The text boxes show it.
    let shown = |visual: &mut gpui::VisualTestContext, id: &str| {
        app.read_with(visual, |this, cx| this.settings_ui.page.as_ref().unwrap().input_text_for_test(id, cx))
    };
    assert_eq!(shown(visual, "editor.format_external_program"), "sqlfluff");
    assert_eq!(shown(visual, "editor.format_external_args"), "format --dialect {dialect} -");
    click(visual, "format-preset-pg_format".into());
    assert_eq!(shown(visual, "editor.format_external_program"), "pg_format");
    assert_eq!(draft_of(&app, visual).editor.format_external_args, "-");
    assert_eq!(draft_of(&app, visual).editor.format_external_exit_codes, "0");
}
