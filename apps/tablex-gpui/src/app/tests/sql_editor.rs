// SQL editor text commands, driven through real keystrokes, the palette, the production theme and
// `Root`'s dialog layer: Format SQL (selection, undo, settings, the guard), find / replace in the
// editor beside the result grid's find, and `.sql` files (open, save, save as, the unsaved dot, the
// close question, the size and UTF-8 refusals). Prompts are answered through the test platform.

/// The platform spelling of a registered chord (`secondary-` is Cmd on macOS, Ctrl elsewhere).
fn sql_chord(id: &str) -> String {
    let chord = shortcuts::find(id).expect("registered action").default_chords().remove(0);
    chord.replace("secondary", if cfg!(target_os = "macos") { "cmd" } else { "ctrl" })
}

fn sql_primary(key: &str) -> String {
    format!("{}-{key}", if cfg!(target_os = "macos") { "cmd" } else { "ctrl" })
}

fn sql_editor(rig: &ExplainRig<'_>, tab: u64) -> gpui::Entity<EditorState> {
    rig.app.read_with(rig.visual, |this, _| this.query_editors.by_tab[&tab].clone())
}

fn sql_text(rig: &ExplainRig<'_>, tab: u64) -> String {
    let editor = sql_editor(rig, tab);
    rig.app.read_with(rig.visual, |_, cx| editor.read(cx).value().to_string())
}

fn sql_tab_text(rig: &ExplainRig<'_>, tab: u64) -> String {
    rig.app.read_with(rig.visual, |this, _| this.workspace.tab(tab).unwrap().sql.clone())
}

/// Focuses the editor and puts the selection at `range`.
fn sql_select(rig: &mut ExplainRig<'_>, tab: u64, range: std::ops::Range<usize>) {
    let editor = sql_editor(rig, tab);
    rig.visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            state.focus(window, cx);
            state.set_selected_range(range, cx);
        });
    });
    settle(rig.visual);
}

fn sql_last_error(rig: &ExplainRig<'_>) -> Option<String> {
    rig.app.read_with(rig.visual, |this, _| this.error_log.log.last().map(|entry| entry.message.to_string()))
}

fn sql_notes(rig: &mut ExplainRig<'_>) -> usize {
    rig.visual.update(|window, cx| window.notifications(cx).len())
}

fn sql_set_settings(rig: &mut ExplainRig<'_>, edit: impl FnOnce(&mut crate::settings::Settings)) {
    rig.app.update(rig.visual, |this, _| {
        let mut next = (*this.settings).clone();
        edit(&mut next);
        this.settings = std::sync::Arc::new(next);
    });
}

const SQL_TWO: &str = "SELECT\n  a,\n  b\nFROM\n  t\nWHERE\n  x = 1";

#[gpui::test]
fn the_format_shortcut_formats_the_editor_as_one_undo_step_and_keeps_the_cursor(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let original = "select a,b from t where x=1";
    let tab = rig.tab(ConnectionKind::Postgres, original);
    // The cursor sits on the `t` after `from`.
    let at = original.find("t where").unwrap();
    sql_select(&mut rig, tab, at..at);

    rig.visual.simulate_keystrokes(&sql_chord("editor.format_sql"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), SQL_TWO);
    assert_eq!(sql_tab_text(&rig, tab), SQL_TWO, "the tab follows the editor");
    let editor = sql_editor(&rig, tab);
    let cursor = rig.app.read_with(rig.visual, |_, cx| editor.read(cx).cursor());
    assert_eq!(&SQL_TWO[cursor..cursor + 1], "t", "the cursor stayed on its token");

    rig.visual.simulate_keystrokes(&sql_primary("z"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), original, "one undo restores the text");
    // The editor's redo: Cmd+Shift+Z on macOS, Ctrl+Y elsewhere (gpui-base's input bindings).
    rig.visual.simulate_keystrokes(if cfg!(target_os = "macos") { "cmd-shift-z" } else { "ctrl-y" });
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), SQL_TWO, "and redo formats again");
}

#[gpui::test]
fn the_palette_formats_the_statements_a_selection_touches(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let original = "select 1;\nselect a,b from t where x=1;\nselect  3";
    let tab = rig.tab(ConnectionKind::Postgres, original);
    // A selection inside the second statement only.
    let from = original.find("a,b").unwrap();
    sql_select(&mut rig, tab, from..from + 3);

    palette_open(rig.visual);
    palette_type(rig.visual, "format sql");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), format!("select 1;\n{SQL_TWO};\nselect  3"));
    // The formatted statement stays selected.
    let editor = sql_editor(&rig, tab);
    let selected = rig.app.read_with(rig.visual, |_, cx| editor.read(cx).selected_value().to_string());
    assert_eq!(selected, SQL_TWO);

    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_primary("z"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), original);
}

#[gpui::test]
fn the_formatting_settings_change_the_output(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "SELECT a FROM t WHERE x=1; SELECT 2");
    sql_set_settings(&mut rig, |settings| {
        settings.editor.format_keyword_case = crate::settings::KeywordCase::Lower;
        settings.editor.format_indent = 4;
        settings.editor.format_lines_between_statements = 2;
    });
    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_chord("editor.format_sql"));
    settle(rig.visual);
    assert_eq!(
        sql_text(&rig, tab),
        "select\n    a\nfrom\n    t\nwhere\n    x = 1;\n\n\nselect\n    2"
    );
    // Indent 0 follows the tab size.
    sql_set_settings(&mut rig, |settings| {
        settings.editor.format_indent = 0;
        settings.editor.tab_size = 3;
        settings.editor.format_keyword_case = crate::settings::KeywordCase::Keep;
        settings.editor.format_lines_between_statements = 0;
    });
    rig.visual.simulate_keystrokes(&sql_primary("a"));
    rig.visual.simulate_input("Select a From t");
    settle(rig.visual);
    rig.visual.simulate_keystrokes(&sql_chord("editor.format_sql"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), "Select\n   a\nFrom\n   t");
}

#[gpui::test]
fn identifiers_keep_their_case_and_the_guard_refuses_what_it_cannot_prove(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "select action, \"Mixed\" from t where action = 'X'");
    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_chord("editor.format_sql"));
    settle(rig.visual);
    let formatted = sql_text(&rig, tab);
    assert!(formatted.contains("action,") && formatted.contains("\"Mixed\"") && formatted.contains("'X'"), "{formatted}");
    assert!(formatted.starts_with("SELECT") && formatted.contains("FROM") && formatted.contains("WHERE"), "{formatted}");
    assert_eq!(sql_notes(&mut rig), 0);

    // A MySQL executable comment is code to the server: nothing is changed and the user is told.
    let mysql = rig.tab(ConnectionKind::Mysql, "select /*!50000 1 */ ,2 from t");
    sql_select(&mut rig, mysql, 0..0);
    rig.visual.simulate_keystrokes(&sql_chord("editor.format_sql"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, mysql), "select /*!50000 1 */ ,2 from t");
    assert_eq!(sql_notes(&mut rig), 1, "the refusal is announced");
}

#[gpui::test]
fn a_format_does_not_overwrite_text_typed_while_it_ran(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "select a,b from t");
    sql_select(&mut rig, tab, 0..0);
    // The formatter has not been heard from yet (its answer is only read once this update ends):
    // the user types.
    let editor = sql_editor(&rig, tab);
    let app = rig.app.clone();
    rig.visual.update(|window, cx| {
        app.update(cx, |this, cx| this.format_sql(window, cx));
        editor.update(cx, |state, cx| state.insert("x", window, cx));
    });
    settle(rig.visual);
    let text = sql_text(&rig, tab);
    assert_eq!(text, "xselect a,b from t", "the late result was dropped");
    assert_eq!(sql_notes(&mut rig), 1, "and the user was told");
}

#[gpui::test]
fn the_editor_menu_and_palette_offer_format_sql_only_for_a_sql_editor(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    rig.tab(ConnectionKind::Postgres, "select 1");
    let titles = rig.app.update(rig.visual, |this, _| this.palette_action_titles());
    for title in ["Format SQL", "Save SQL File As", "Open SQL File"] {
        assert!(titles.iter().any(|t| t == title), "{title} in {titles:?}");
    }
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.open_welcome_tab();
        cx.notify();
    });
    let titles = rig.app.update(rig.visual, |this, _| this.palette_action_titles());
    assert!(!titles.iter().any(|t| t == "Format SQL"), "{titles:?}");
    assert!(titles.iter().any(|t| t == "Open SQL File"));
}

// ---- Format SQL with a tool or an external command ---------------------------------------------
//
// The formatter's process is a stub shell script (cfg(unix); a Windows equivalent would be a .cmd
// file, and this suite is not run there). Everything else is the real thing: the settings, the
// keystroke, the background process, the guard, the dialog and the editor.

#[cfg(unix)]
fn sql_stub(name: &str, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = sql_dir();
    let path = dir.join(format!("{name}.sh"));
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

#[cfg(unix)]
fn sql_use_command(rig: &mut ExplainRig<'_>, program: &std::path::Path, args: &str) {
    let (program, args) = (program.display().to_string(), args.to_owned());
    sql_set_settings(rig, move |settings| {
        settings.editor.formatter = crate::settings::FormatterChoice::External;
        settings.editor.format_external_program = program;
        settings.editor.format_external_args = args;
        settings.editor.format_timeout = 20;
    });
}

#[cfg(unix)]
fn sql_format_and_wait(rig: &mut ExplainRig<'_>, done: impl FnMut(&mut ExplainRig<'_>) -> bool) {
    rig.visual.simulate_keystrokes(&sql_chord("editor.format_sql"));
    sql_wait_for(rig, "the formatter to answer", done);
}

#[cfg(unix)]
#[gpui::test]
fn an_external_command_formats_the_editor(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let original = "select a,b from t where x=1";
    let tab = rig.tab(ConnectionKind::Postgres, original);
    // Breaks the line after each comma; the arguments carry the dialect of the connection.
    let (dir, command) = sql_stub("lay-out", "echo \"$@\" > \"$(dirname \"$0\")/args.txt\"\nsed 's/,/,\\n  /g'");
    sql_use_command(&mut rig, &command, "--dialect {dialect} --language {language} -");
    sql_select(&mut rig, tab, 0..0);
    sql_format_and_wait(&mut rig, |rig| sql_text(rig, tab) != original);
    assert_eq!(sql_text(&rig, tab), "select a,\n  b from t where x=1");
    assert_eq!(std::fs::read_to_string(dir.join("args.txt")).unwrap().trim(), "--dialect postgres --language postgresql -");
    assert_eq!(sql_notes(&mut rig), 0);
    // One undo step puts the original back.
    rig.visual.simulate_keystrokes(&sql_primary("z"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), original);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[gpui::test]
fn output_the_guard_disagrees_with_asks_before_it_is_applied(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let original = "select a, b from t";
    let tab = rig.tab(ConnectionKind::Postgres, original);
    // Rewrites the first column: not layout.
    let (dir, command) = sql_stub("rewrite", "cat >/dev/null\necho 'select z, b from t'");
    sql_use_command(&mut rig, &command, "");
    sql_select(&mut rig, tab, 0..0);

    // Cancel keeps the text.
    sql_format_and_wait(&mut rig, sql_dialog_open);
    assert_eq!(sql_text(&rig, tab), original, "nothing is applied before the answer");
    assert!(rig.rendered("format-disagree-body"), "the question is asked");
    rig.click("format-disagree-cancel");
    assert!(!rig.rendered("format-disagree-body"));
    assert_eq!(sql_text(&rig, tab), original);

    // Escape is Cancel too.
    sql_format_and_wait(&mut rig, sql_dialog_open);
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(!rig.rendered("format-disagree-body"));
    assert_eq!(sql_text(&rig, tab), original);

    // Apply anyway puts the command's text in, as one undo step.
    sql_format_and_wait(&mut rig, sql_dialog_open);
    rig.click("format-disagree-apply");
    assert!(!rig.rendered("format-disagree-body"));
    assert_eq!(sql_text(&rig, tab), "select z, b from t");
    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_primary("z"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), original);

    // Enter is Cancel, not Apply: the refused text is never one keystroke away.
    sql_format_and_wait(&mut rig, sql_dialog_open);
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert!(!rig.rendered("format-disagree-body"), "the dialog closed");
    assert_eq!(sql_text(&rig, tab), original, "and applied nothing");

    // Apply anyway after the text changed while the question was open applies nothing.
    sql_format_and_wait(&mut rig, sql_dialog_open);
    let editor = sql_editor(&rig, tab);
    rig.visual.update(|window, cx| editor.update(cx, |state, cx| state.insert("x", window, cx)));
    let typed = sql_text(&rig, tab);
    assert_ne!(typed, original);
    let notes = sql_notes(&mut rig);
    rig.click("format-disagree-apply");
    assert!(!rig.rendered("format-disagree-body"));
    assert_eq!(sql_text(&rig, tab), typed, "the user's newer text stays");
    assert_eq!(sql_notes(&mut rig), notes + 1, "and the user is told nothing was applied");
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
fn sql_dialog_open(rig: &mut ExplainRig<'_>) -> bool {
    rig.visual.debug_bounds("format-disagree-body").is_some()
}

#[cfg(unix)]
#[gpui::test]
fn a_failing_command_leaves_the_text_and_says_why(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let original = "select a, b from t";
    let tab = rig.tab(ConnectionKind::Postgres, original);
    let (dir, command) = sql_stub("fail", "cat >/dev/null\necho 'boom: bad syntax near b' >&2\nexit 2");
    sql_use_command(&mut rig, &command, "");
    sql_select(&mut rig, tab, 0..0);
    sql_format_and_wait(&mut rig, |rig| sql_last_error(rig).is_some());
    assert_eq!(sql_text(&rig, tab), original);
    let logged = sql_last_error(&rig).unwrap();
    assert!(logged.contains("boom: bad syntax near b"), "{logged}");
    assert_eq!(sql_notes(&mut rig), 1, "the user is told");

    // A missing program and an unset command are told apart.
    sql_set_settings(&mut rig, |settings| settings.editor.format_external_program = "tablex-no-such-formatter".into());
    sql_format_and_wait(&mut rig, |rig| sql_last_error(rig).is_some_and(|message| message.contains("tablex-no-such-formatter")));
    sql_set_settings(&mut rig, |settings| settings.editor.format_external_program.clear());
    sql_format_and_wait(&mut rig, |rig| sql_last_error(rig).is_some_and(|message| message.contains("No external formatter command is set")));
    assert_eq!(sql_text(&rig, tab), original);
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn the_formatter_tool_that_is_not_installed_says_where_to_get_it(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let original = "select a,b from t";
    let tab = rig.tab(ConnectionKind::Postgres, original);
    let empty = std::env::temp_dir().join(format!("tablex-no-tools-{}", uuid::Uuid::new_v4()));
    rig.app.update(rig.visual, |this, _| {
        this.driver_ui.tools.store = Some(std::sync::Arc::new(engine_driver_host::ToolStore::new(&empty)));
    });
    sql_set_settings(&mut rig, |settings| settings.editor.formatter = crate::settings::FormatterChoice::Sqruff);
    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_chord("editor.format_sql"));
    sql_wait_for(&mut rig, "the notice", |rig| sql_last_error(rig).is_some_and(|message| message.contains("not installed")));
    assert_eq!(sql_text(&rig, tab), original);
    assert!(sql_notes(&mut rig) >= 1);
    let logged = sql_last_error(&rig).unwrap_or_default();
    assert!(logged.contains("Driver Manager"), "{logged}");
}

/// A format's process is stopped by the editor closing and by a newer format in the same tab; another
/// tab's format is left alone (the host proves that the token kills the process, group and all; here
/// the app hands the token over and fires it).
#[gpui::test]
fn closing_the_editor_or_formatting_again_stops_that_tabs_format_only(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let first = rig.tab(ConnectionKind::Postgres, "select a, b from t");
    let second = rig.tab(ConnectionKind::Postgres, "select 1");
    let (one, two) = rig.app.update(rig.visual, |this, _| (this.format_context(first), this.format_context(second)));
    assert!(!one.cancel.is_cancelled(), "a format in tab B does not stop tab A's");
    assert!(!two.cancel.is_cancelled());
    let again = rig.app.update(rig.visual, |this, _| this.format_context(first));
    assert!(one.cancel.is_cancelled(), "a newer format in the same tab stops the one before it");
    assert!(!again.cancel.is_cancelled() && !two.cancel.is_cancelled());
    rig.app.update(rig.visual, |this, cx| this.close_tab(first, cx));
    assert!(again.cancel.is_cancelled(), "its editor closing stops it");
    assert!(!two.cancel.is_cancelled(), "another tab's editor closing leaves it running");
    rig.app.update(rig.visual, |this, cx| this.close_tab(second, cx));
    assert!(two.cancel.is_cancelled());
    assert_eq!(one.dialect, engine_driver_api::sql_format::Dialect::Postgres);
}

// ---- find / replace ------------------------------------------------------------------

fn sql_search_open(rig: &ExplainRig<'_>, tab: u64) -> bool {
    let editor = sql_editor(rig, tab);
    rig.app.read_with(rig.visual, |_, cx| editor.read(cx).search_session().open)
}

fn sql_search_query(rig: &ExplainRig<'_>, tab: u64) -> String {
    let editor = sql_editor(rig, tab);
    rig.app.read_with(rig.visual, |_, cx| editor.read(cx).search_session().query.clone())
}

fn sql_search_label(rig: &ExplainRig<'_>, tab: u64) -> String {
    let editor = sql_editor(rig, tab);
    rig.app.read_with(rig.visual, |_, cx| editor.read(cx).search_session().matcher.label())
}

#[gpui::test]
fn cmd_f_in_the_sql_editor_opens_editor_search_with_next_previous_and_escape(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "select 'a', 'b' from t where x = 'a'");
    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_primary("f"));
    settle(rig.visual);
    assert!(sql_search_open(&rig, tab), "Cmd+F in the editor opens the editor's search");
    rig.visual.simulate_input("'a'");
    settle(rig.visual);
    assert_eq!(sql_search_query(&rig, tab), "'a'", "no typed character is lost");
    assert_eq!(sql_search_label(&rig, tab), "1/2");
    rig.visual.simulate_keystrokes("enter");
    assert_eq!(sql_search_label(&rig, tab), "2/2", "Enter goes to the next match");
    rig.visual.simulate_keystrokes("shift-enter");
    assert_eq!(sql_search_label(&rig, tab), "1/2", "Shift+Enter goes back");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(!sql_search_open(&rig, tab), "Escape closes the panel");
    // Focus is back in the editor: typing lands in the text.
    rig.visual.simulate_input("z");
    assert!(sql_text(&rig, tab).contains('z'));
}

#[gpui::test]
fn replace_in_the_sql_editor_rewrites_matches_through_the_panel(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "select a, a, a from a");
    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_primary("f"));
    settle(rig.visual);
    rig.visual.simulate_input("a,");
    settle(rig.visual);
    assert_eq!(sql_search_query(&rig, tab), "a,", "no typed character is lost");
    assert_eq!(sql_search_label(&rig, tab), "1/2");
    // The panel's replace row: the toggle chord shows it and focuses its field.
    let toggle = if cfg!(target_os = "macos") { "cmd-shift-f" } else { "ctrl-h" };
    rig.visual.simulate_keystrokes(toggle);
    settle(rig.visual);
    rig.visual.simulate_input("b,");
    settle(rig.visual);
    let editor = sql_editor(&rig, tab);
    let replace_mode = rig.app.read_with(rig.visual, |_, cx| editor.read(cx).search_session().replace_mode);
    assert!(replace_mode, "the replace row is showing");
    // Replace All is the right-most button of the panel's second row (the component gives its
    // buttons no selector to find them by): the panel is the top strip of the editor.
    let area = rig.visual.debug_bounds("query-editor-area").expect("editor area");
    let editor_top = area.top() + gpui::px(theme::HEADER_HEIGHT);
    let button = gpui::point(area.right() - gpui::px(12.0 + 40.0), editor_top + gpui::px(56.0));
    rig.visual.simulate_click(button, Default::default());
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), "select b, b, a from a");
    assert_eq!(sql_tab_text(&rig, tab), "select b, b, a from a", "the tab follows");
    // Escape leaves the panel for the editor, where one undo takes the replacement back.
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    rig.visual.simulate_keystrokes(&sql_primary("z"));
    settle(rig.visual);
    assert_eq!(sql_text(&rig, tab), "select a, a, a from a", "one undo");
}

#[gpui::test]
fn cmd_f_keeps_opening_the_grid_find_when_the_grid_has_focus(cx: &mut TestAppContext) {
    let (app, visual) = find_app(cx);
    // The tab shows an editor above the grid: with the editor focused Cmd+F is the editor's.
    let editor = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.query_editors.by_tab[&tab].clone()
    });
    visual.update(|window, cx| editor.update(cx, |state, cx| state.focus(window, cx)));
    settle(visual);
    find_open(visual);
    assert!(editor.read_with(visual, |state, _| state.search_session().open), "editor search opens");
    assert!(visual.debug_bounds("grid-find-bar").is_none(), "the grid's find stays closed");
    visual.simulate_keystrokes("escape");
    settle(visual);

    // With the grid focused it is the grid's find bar.
    let cell = visual.debug_bounds("grid-body-cell-2-2").unwrap();
    visual.simulate_click(cell.center(), Default::default());
    settle(visual);
    assert!(find_grid_focused(&app, visual));
    find_open(visual);
    assert!(visual.debug_bounds("grid-find-bar").is_some(), "grid find opens");
    assert!(find_input_focused(&app, visual));
    assert!(!editor.read_with(visual, |state, _| state.search_session().open), "editor search stays closed");
}

// ---- files ------------------------------------------------------------------------------

fn sql_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tablex-sql-files-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sql_active(rig: &ExplainRig<'_>) -> crate::workspace::Tab {
    rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().clone())
}

fn sql_tab_count(rig: &ExplainRig<'_>) -> usize {
    rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().len())
}

/// Answers the open prompt with `path` and waits for the file to be read.
fn sql_open(rig: &mut ExplainRig<'_>, path: &std::path::Path) {
    rig.visual.simulate_keystrokes(&sql_chord("editor.open_sql_file"));
    assert!(rig.visual.did_prompt_for_paths(), "Open SQL File asks the platform for a file");
    let chosen = path.to_owned();
    rig.visual.simulate_path_prompt_response(move |options| {
        assert!(options.files && !options.multiple && !options.directories);
        Some(vec![chosen.clone()])
    });
    settle(rig.visual);
    rig.visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(rig.visual);
}

fn sql_wait_for(rig: &mut ExplainRig<'_>, label: &'static str, mut ready: impl FnMut(&mut ExplainRig<'_>) -> bool) {
    let start = std::time::Instant::now();
    rig.visual.dispatcher.allow_parking();
    loop {
        settle(rig.visual);
        if ready(rig) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[gpui::test]
fn open_binds_a_tab_to_the_file_and_save_writes_it_back(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let dir = sql_dir();
    let orders = dir.join("orders.sql");
    std::fs::write(&orders, "select * from orders;\n").unwrap();
    let first = rig.tab(ConnectionKind::Postgres, "");
    let tabs = sql_tab_count(&rig);

    sql_open(&mut rig, &orders);
    sql_wait_for(&mut rig, "the file to open", |rig| sql_active(rig).file.is_some());
    // The empty tab you were on took the file.
    assert_eq!(sql_tab_count(&rig), tabs);
    let tab = sql_active(&rig);
    assert_eq!(tab.id, first);
    assert_eq!(tab.title, "orders.sql");
    assert_eq!(tab.sql, "select * from orders;\n");
    assert_eq!(tab.file.as_ref().unwrap().path, orders);
    assert!(!tab.has_unsaved_file());
    assert_eq!(sql_text(&rig, first), "select * from orders;\n", "the editor shows it");
    assert_eq!(tab.connection_id, rig.app.read_with(rig.visual, |this, _| this.saved_connections.last().unwrap().id.to_string()));

    // Typing marks the tab unsaved (a dot after the name), until Save.
    sql_select(&mut rig, first, 0..0);
    rig.visual.simulate_input("-- x\n");
    settle(rig.visual);
    let tab = sql_active(&rig);
    assert!(tab.has_unsaved_file());
    assert_eq!(tab_bar::tab_label(&tab, false), "orders.sql \u{2022}");
    rig.visual.simulate_keystrokes(&sql_primary("s"));
    sql_wait_for(&mut rig, "the save", |rig| !sql_active(rig).has_unsaved_file());
    assert_eq!(std::fs::read_to_string(&orders).unwrap(), "-- x\nselect * from orders;\n");
    assert_eq!(tab_bar::tab_label(&sql_active(&rig), false), "orders.sql");
    assert!(!rig.visual.did_prompt_for_new_path(), "Save on a bound tab never asks for a name");

    // A second file opens in a new tab on the same connection; the same file twice finds its tab.
    let reports = dir.join("reports.sql");
    std::fs::write(&reports, "select 2").unwrap();
    sql_open(&mut rig, &reports);
    sql_wait_for(&mut rig, "the second file", |rig| sql_tab_count(rig) == tabs + 1);
    let second = sql_active(&rig);
    assert_eq!(second.title, "reports.sql");
    assert_eq!(second.connection_id, tab.connection_id);
    sql_open(&mut rig, &orders);
    settle(rig.visual);
    assert_eq!(sql_active(&rig).id, first, "an open file is not opened twice");
    assert_eq!(sql_tab_count(&rig), tabs + 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn save_as_asks_where_and_binds_the_tab(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let dir = sql_dir();
    let tab = rig.tab(ConnectionKind::Postgres, "select 42");
    rig.visual.simulate_keystrokes(&sql_chord("editor.save_sql_file_as"));
    assert!(rig.visual.did_prompt_for_new_path());
    // Cancelling changes nothing.
    rig.visual.simulate_new_path_selection(|_| None);
    settle(rig.visual);
    assert!(sql_active(&rig).file.is_none());

    rig.visual.simulate_keystrokes(&sql_chord("editor.save_sql_file_as"));
    let target = dir.join("answer.sql");
    let chosen = target.clone();
    rig.visual.simulate_new_path_selection(move |directory| {
        assert_eq!(directory, crate::platform::save_dialog_dir());
        Some(chosen.clone())
    });
    sql_wait_for(&mut rig, "the save", |rig| sql_active(rig).file.is_some());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "select 42");
    let bound = sql_active(&rig);
    assert_eq!((bound.id, bound.title.as_str()), (tab, "answer.sql"));
    assert!(!bound.has_unsaved_file());

    // Save As again starts in the file's folder, and rebinds.
    rig.visual.simulate_keystrokes(&sql_chord("editor.save_sql_file_as"));
    let (folder, copy) = (dir.clone(), dir.join("copy.sql"));
    rig.visual.simulate_new_path_selection(move |directory| {
        assert_eq!(directory, folder, "the file's own folder");
        Some(copy.clone())
    });
    sql_wait_for(&mut rig, "the second save", |rig| sql_active(rig).title == "copy.sql");
    assert!(dir.join("answer.sql").exists() && dir.join("copy.sql").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn save_on_a_tab_without_a_file_keeps_the_save_script_dialog(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    rig.tab(ConnectionKind::Postgres, "select 1");
    rig.visual.simulate_keystrokes(&sql_primary("s"));
    settle(rig.visual);
    assert!(rig.visual.update(|window, cx| window.has_active_dialog(cx)), "Save Script asks for a name");
    assert!(!rig.visual.did_prompt_for_new_path());
}

fn sql_dirty_file_tab(rig: &mut ExplainRig<'_>, dir: &std::path::Path, name: &str) -> (u64, std::path::PathBuf) {
    let path = dir.join(name);
    std::fs::write(&path, "select 1").unwrap();
    rig.tab(ConnectionKind::Postgres, "");
    sql_open(rig, &path);
    sql_wait_for(rig, "the file to open", |rig| sql_active(rig).file.as_ref().is_some_and(|f| f.path == path));
    let tab = sql_active(rig).id;
    sql_select(rig, tab, 8..8);
    rig.visual.simulate_input(" -- edit");
    settle(rig.visual);
    assert!(sql_active(rig).has_unsaved_file());
    (tab, path)
}

#[gpui::test]
fn closing_an_unsaved_file_asks_save_dont_save_or_cancel(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let dir = sql_dir();
    // Whatever the "confirm closing" setting says.
    sql_set_settings(&mut rig, |settings| settings.editor.confirm_unsaved_close = false);

    // Cancel keeps the tab and its changes.
    let (tab, path) = sql_dirty_file_tab(&mut rig, &dir, "a.sql");
    let tabs = sql_tab_count(&rig);
    rig.visual.simulate_keystrokes(&sql_primary("w"));
    settle(rig.visual);
    assert!(rig.rendered("unsaved-file-body"), "the question is asked");
    rig.click("unsaved-file-cancel");
    assert_eq!(sql_tab_count(&rig), tabs);
    assert!(sql_active(&rig).has_unsaved_file());

    // Escape is Cancel too.
    rig.visual.simulate_keystrokes(&sql_primary("w"));
    settle(rig.visual);
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert_eq!(sql_tab_count(&rig), tabs);
    assert!(!rig.rendered("unsaved-file-body"));

    // Don't Save closes it and leaves the file alone.
    rig.visual.simulate_keystrokes(&sql_primary("w"));
    settle(rig.visual);
    rig.click("unsaved-file-discard");
    assert_eq!(sql_tab_count(&rig), tabs - 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 1");
    let _ = tab;

    // Save writes the file, then closes.
    let (_, path) = sql_dirty_file_tab(&mut rig, &dir, "b.sql");
    let tabs = sql_tab_count(&rig);
    rig.visual.simulate_keystrokes(&sql_primary("w"));
    settle(rig.visual);
    rig.click("unsaved-file-save");
    sql_wait_for(&mut rig, "the tab to close", |rig| sql_tab_count(rig) == tabs - 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 1 -- edit");

    // A clean file-bound tab closes without a question.
    let clean = dir.join("c.sql");
    std::fs::write(&clean, "select 3").unwrap();
    rig.tab(ConnectionKind::Postgres, "");
    sql_open(&mut rig, &clean);
    sql_wait_for(&mut rig, "the file to open", |rig| sql_active(rig).file.is_some());
    let tabs = sql_tab_count(&rig);
    let clean_tab = sql_active(&rig).id;
    sql_select(&mut rig, clean_tab, 0..0);
    rig.visual.simulate_keystrokes(&sql_primary("w"));
    settle(rig.visual);
    assert!(!rig.rendered("unsaved-file-body"));
    assert_eq!(sql_tab_count(&rig), tabs - 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn closing_several_tabs_names_the_files_with_unsaved_changes(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let dir = sql_dir();
    let (tab, _) = sql_dirty_file_tab(&mut rig, &dir, "multi.sql");
    let other = rig.tab(ConnectionKind::Postgres, "select 2");
    let tabs = sql_tab_count(&rig);
    let app = rig.app.clone();
    rig.visual.update(|window, cx| {
        app.update(cx, |this, cx| this.close_tabs_confirmed(vec![tab, other], None, window, cx));
    });
    settle(rig.visual);
    assert!(rig.rendered("discard-pending-body"), "closing both asks first");
    assert_eq!(sql_tab_count(&rig), tabs);
    rig.click("discard-pending-cancel");
    assert_eq!(sql_tab_count(&rig), tabs);
    rig.visual.update(|window, cx| {
        app.update(cx, |this, cx| this.close_tabs_confirmed(vec![other], None, window, cx));
    });
    settle(rig.visual);
    assert!(!rig.rendered("discard-pending-body"), "a plain script closes without a question");
    assert_eq!(sql_tab_count(&rig), tabs - 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn a_failed_save_keeps_the_tab_and_says_why(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let dir = sql_dir();
    let (tab, path) = sql_dirty_file_tab(&mut rig, &dir, "gone.sql");
    // The folder vanished: the write fails, the tab keeps its text and stays unsaved.
    std::fs::remove_dir_all(&dir).unwrap();
    rig.visual.simulate_keystrokes(&sql_primary("s"));
    sql_wait_for(&mut rig, "the failure", |rig| sql_last_error(rig).is_some_and(|message| message.contains("Could not save")));
    assert!(sql_active(&rig).has_unsaved_file());
    assert_eq!(sql_active(&rig).id, tab);
    assert!(sql_notes(&mut rig) >= 1);
    assert!(!path.exists());
}

#[gpui::test]
fn files_over_the_limit_or_not_utf8_are_refused_and_nothing_is_loaded(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let dir = sql_dir();
    rig.tab(ConnectionKind::Postgres, "");
    let tabs = sql_tab_count(&rig);

    let big = dir.join("big.sql");
    std::fs::write(&big, vec![b' '; crate::sql_file::MAX_BYTES as usize + 1]).unwrap();
    sql_open(&mut rig, &big);
    sql_wait_for(&mut rig, "the size refusal", |rig| sql_last_error(rig).is_some_and(|message| message.contains("larger than 5 MB")));
    assert_eq!(sql_notes(&mut rig), 1);
    assert_eq!(sql_tab_count(&rig), tabs);
    assert!(sql_active(&rig).file.is_none() && sql_active(&rig).sql.is_empty());

    let binary = dir.join("latin1.sql");
    std::fs::write(&binary, [b's', b'e', b'l', 0xe9, b'c', b't']).unwrap();
    sql_open(&mut rig, &binary);
    sql_wait_for(&mut rig, "the UTF-8 refusal", |rig| sql_last_error(rig).is_some_and(|message| message.contains("not UTF-8")));
    assert_eq!(sql_tab_count(&rig), tabs);
    assert!(sql_active(&rig).file.is_none() && sql_active(&rig).sql.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}
