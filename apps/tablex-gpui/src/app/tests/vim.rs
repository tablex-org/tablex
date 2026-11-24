// Real-keystroke proofs for the Vim keys of the query editors: the status line switch, the modes, the
// edits, undo, the shared register, the Settings default, and that a tab without Vim is untouched.

/// A fresh SQL tab holding `text`, typed with Vim off; its id.
fn vim_sql_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, text: &str) -> u64 {
    click_debug_button(visual, "global-new-query");
    settle(visual);
    type_in_editor(visual, text);
    active_tab(app, visual)
}

/// Types plain characters (not named keys) into the focused element, one keystroke each, in the platform's
/// order: the frame that follows a key runs its next-frame callbacks BEFORE it draws anything.
fn vim_press(visual: &mut gpui::VisualTestContext, keys: &str) {
    for ch in keys.chars() {
        let key = ch.to_string();
        visual.update(|window, cx| {
            let keystroke = gpui::Keystroke { modifiers: Default::default(), key: key.clone(), key_char: Some(key) };
            window.dispatch_keystroke(keystroke, cx);
            window.simulate_next_frame(cx);
        });
        vim_frame(visual);
    }
}

/// The frames after that one (a test window has no frame loop of its own): draw, then the callbacks of
/// the next frame, until none is left. `u` and Ctrl-R need two of them.
fn vim_frame(visual: &mut gpui::VisualTestContext) {
    settle(visual);
    for _ in 0..8 {
        if visual.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            break;
        }
        settle(visual);
    }
}

/// A chord (`ctrl-r`, `secondary-z`) in the same order as `vim_press`.
fn vim_chord(visual: &mut gpui::VisualTestContext, chord: &'static str) {
    visual.update(|window, cx| {
        let keystroke = gpui::Keystroke::parse_with_os(chord, cx.platform_info().os).expect("a chord");
        window.dispatch_keystroke(keystroke, cx);
        window.simulate_next_frame(cx);
    });
    vim_frame(visual);
}

fn vim_cursor(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64, offset: usize) {
    app.update(visual, |this, cx| {
        let editor = this.query_editors.by_tab[&tab].clone();
        editor.update(cx, |state, cx| state.set_selected_range(offset..offset, cx));
    });
    settle(visual);
}

fn vim_selection(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> std::ops::Range<usize> {
    app.read_with(visual, |this, cx| this.query_editors.by_tab[&tab].read(cx).selected_range())
}

fn vim_mode_of(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> crate::vim::Mode {
    app.read_with(visual, |this, _| this.vim_mode(tab))
}

/// The status line switch, by a real click; the editor has the focus afterwards.
fn vim_toggle(visual: &mut gpui::VisualTestContext) {
    click_debug_button(visual, "vim-toggle");
    settle(visual);
}

/// Whether the mode badge is in the editor's status line.
fn vim_label_shown(visual: &mut gpui::VisualTestContext) -> bool {
    visual.debug_bounds("vim-mode").is_some()
}

#[gpui::test]
fn a_normal_mode_caret_is_on_the_last_char_of_the_line_after_turning_vim_on_and_after_a_click(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "select 1");
    assert_eq!(vim_selection(&app, visual, tab), 8..8, "typing leaves the caret after the text");
    vim_toggle(visual);
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    assert_eq!(vim_selection(&app, visual, tab), 7..7, "Normal starts on the last char, not after it");
    // A click past the end of the line puts the caret there; Normal takes it back onto the last char.
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    visual.simulate_click(gpui::point(area.center().x, area.bottom() - gpui::px(40.0)), Default::default());
    settle(visual);
    assert_eq!(vim_selection(&app, visual, tab), 7..7, "a click at the line end clamps");
    // Insert mode may sit after the last char: `A` goes there and the click leaves it be.
    vim_press(visual, "A");
    assert_eq!(vim_selection(&app, visual, tab), 8..8);
    visual.simulate_click(gpui::point(area.center().x, area.bottom() - gpui::px(40.0)), Default::default());
    settle(visual);
    assert_eq!(vim_selection(&app, visual, tab), 8..8, "Insert keeps the caret after the text");
}

#[gpui::test]
fn without_vim_the_editor_types_as_before(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "select 1");
    assert!(!app.read_with(visual, |this, _| this.vim_on(tab)), "off by default");
    assert!(!vim_label_shown(visual), "no mode label");
    assert!(visual.debug_bounds("vim-toggle").is_some(), "the switch is in the status line");
    // Letters, Escape, Enter and Backspace all behave as in any editor.
    visual.simulate_keystrokes("escape");
    vim_press(visual, "dd");
    assert_eq!(editor_text(&app, visual, tab), "select 1dd");
    visual.simulate_keystrokes("enter backspace");
    assert_eq!(editor_text(&app, visual, tab), "select 1dd");
    visual.simulate_keystrokes("enter");
    assert_eq!(editor_text(&app, visual, tab), "select 1dd\n");
}

#[gpui::test]
fn the_status_line_switch_turns_vim_on_for_the_tab_and_shows_the_mode(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "select 1");
    vim_toggle(visual);
    assert!(app.read_with(visual, |this, _| this.vim_on(tab)));
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal, "starts in Normal");
    assert!(vim_label_shown(visual), "the mode is shown");
    vim_press(visual, "i");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Insert);
    visual.simulate_keystrokes("escape");
    vim_press(visual, "v");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Visual);
    vim_press(visual, "V");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::VisualLine);
    visual.simulate_keystrokes("escape");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    // Off again: typing is plain and the label goes.
    vim_toggle(visual);
    assert!(!app.read_with(visual, |this, _| this.vim_on(tab)));
    assert!(!vim_label_shown(visual));
    vim_press(visual, "zz");
    assert!(editor_text(&app, visual, tab).contains("zz"), "plain typing again");
}

#[gpui::test]
fn insert_escape_and_letters_in_normal_mode(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "world");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    // Letters in Normal mode are commands (or nothing), never text (`f,` finds nothing, `;` repeats it).
    vim_press(visual, "zqZ:f,;");
    assert_eq!(editor_text(&app, visual, tab), "world");
    vim_press(visual, "i");
    vim_press(visual, "hello ");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "hello world");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    vim_press(visual, "AX");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "hello worldX");
    // Ctrl-[ is Escape too.
    vim_press(visual, "iY");
    visual.simulate_keystrokes("ctrl-[");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    assert_eq!(editor_text(&app, visual, tab), "hello worldYX");
}

#[gpui::test]
fn normal_mode_edits_with_real_keystrokes(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "one two\nsecond line\nthird");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    // 0x deletes a char.
    vim_press(visual, "0x");
    assert_eq!(editor_text(&app, visual, tab), "ne two\nsecond line\nthird");
    // dw deletes a word, dd a line.
    vim_press(visual, "dw");
    assert_eq!(editor_text(&app, visual, tab), "two\nsecond line\nthird");
    vim_press(visual, "j");
    vim_press(visual, "dd");
    assert_eq!(editor_text(&app, visual, tab), "two\nthird");
    // u undoes (each command is a step), Ctrl-R redoes.
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "two\nsecond line\nthird");
    vim_chord(visual, "ctrl-r");
    assert_eq!(editor_text(&app, visual, tab), "two\nthird");
    vim_press(visual, "uu");
    assert_eq!(editor_text(&app, visual, tab), "ne two\nsecond line\nthird");
    // Motions: G / gg, $ and 0, w and b.
    vim_press(visual, "G");
    vim_press(visual, "$");
    assert_eq!(vim_selection(&app, visual, tab), 23..23);
    vim_press(visual, "gg");
    assert_eq!(vim_selection(&app, visual, tab), 0..0);
    vim_press(visual, "w");
    assert_eq!(vim_selection(&app, visual, tab), 3..3);
    vim_press(visual, "b");
    assert_eq!(vim_selection(&app, visual, tab), 0..0);
    // Enter, Backspace and Tab are not edits in Normal mode.
    visual.simulate_keystrokes("enter backspace tab delete");
    assert_eq!(editor_text(&app, visual, tab), "ne two\nsecond line\nthird");
}

#[gpui::test]
fn visual_mode_selects_and_acts_on_the_selection(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abcdef\nsecond");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    // v, l, d deletes the two chars.
    vim_press(visual, "vl");
    assert_eq!(vim_selection(&app, visual, tab), 0..2, "the motion extends the selection");
    vim_press(visual, "d");
    assert_eq!(editor_text(&app, visual, tab), "cdef\nsecond");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    // V selects the whole line; d removes it.
    vim_press(visual, "V");
    assert_eq!(vim_selection(&app, visual, tab), 0..5);
    vim_press(visual, "d");
    assert_eq!(editor_text(&app, visual, tab), "second");
    // y and c.
    vim_press(visual, "vey");
    assert_eq!(app.read_with(visual, |this, _| this.query_editors.vim.register().text.clone()), "second");
    vim_press(visual, "vlc");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Insert);
    vim_press(visual, "XY");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "XYcond");
}

#[gpui::test]
fn yank_fills_the_register_and_the_clipboard_and_p_puts_it(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let first = vim_sql_tab(&app, visual, "alpha\nbeta");
    vim_toggle(visual);
    vim_cursor(&app, visual, first, 0);
    vim_press(visual, "yy");
    assert_eq!(visual.read_from_clipboard().and_then(|item| item.text()), Some("alpha\n".to_string()));
    vim_press(visual, "jp");
    assert_eq!(editor_text(&app, visual, first), "alpha\nbeta\nalpha");
    // The register is shared: a second tab puts what the first yanked.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let second = active_tab(&app, visual);
    type_in_editor(visual, "x");
    vim_toggle(visual);
    vim_cursor(&app, visual, second, 0);
    vim_press(visual, "p");
    assert_eq!(editor_text(&app, visual, second), "x\nalpha");
}

#[gpui::test]
fn undo_steps_are_one_per_command_and_one_per_insert_session(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "tail");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    // An Insert session is one step.
    vim_press(visual, "ihello ");
    visual.simulate_keystrokes("escape");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "tail");
    // Two sessions are two steps.
    vim_press(visual, "ione ");
    visual.simulate_keystrokes("escape");
    vim_press(visual, "Atwo");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "one tailtwo");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "one tail");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "tail");
}

#[gpui::test]
fn cmd_enter_still_runs_in_every_mode(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    let tab = vim_sql_tab(&app, visual, "select 1");
    vim_toggle(visual);
    assert_editor_only(visual, "before the run");
    visual.simulate_keystrokes("secondary-enter");
    settle(visual);
    assert_results_revealed(visual, "Cmd+Enter in Normal mode");
    // The chord inserted nothing and the mode stayed.
    assert_eq!(editor_text(&app, visual, tab), "select 1");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
}

#[gpui::test]
fn the_switch_is_per_tab_and_settings_give_the_default(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let first = vim_sql_tab(&app, visual, "one");
    vim_toggle(visual);
    assert!(app.read_with(visual, |this, _| this.vim_on(first)));
    // A second tab is not affected by the first one's switch.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let second = active_tab(&app, visual);
    assert!(!app.read_with(visual, |this, _| this.vim_on(second)), "the switch is for its tab only");
    type_in_editor(visual, "two");
    vim_press(visual, "dd");
    assert_eq!(editor_text(&app, visual, second), "twodd", "plain typing in the other tab");
    // Back on the first tab Vim is still on, in the mode it was left in.
    activate_tab_by_click(visual, first);
    assert!(app.read_with(visual, |this, _| this.vim_on(first)));
    assert!(vim_label_shown(visual));

    // Settings > Vim keys is the default of the tabs without a switch of their own.
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        next.editor.vim_mode = true;
        this.apply_settings(next, None, cx);
    });
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.vim_on(second)), "second follows Settings");
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let third = active_tab(&app, visual);
    assert!(app.read_with(visual, |this, _| this.vim_on(third)), "a new tab follows Settings");
    assert_eq!(vim_mode_of(&app, visual, third), crate::vim::Mode::Normal);
    type_in_editor(visual, "qzz");
    assert_eq!(editor_text(&app, visual, third), "", "Normal mode: nothing typed");
    // The tab's own switch overrides the default (session only).
    vim_toggle(visual);
    assert!(!app.read_with(visual, |this, _| this.vim_on(third)));
    vim_press(visual, "abc");
    assert_eq!(editor_text(&app, visual, third), "abc");
    assert!(app.read_with(visual, |this, _| this.vim_on(first)), "other tabs keep their own");
}

#[gpui::test]
fn the_mongo_script_status_line_has_the_switch_too(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    open_mongo_script_tab_in_default_scenario(&app, visual);
    let tab = active_tab(&app, visual);
    assert!(visual.debug_bounds("mongo-script-toolbar").is_some());
    assert!(visual.debug_bounds("vim-toggle").is_some(), "the switch is in the status line, not the script toolbar");
    assert!(!vim_label_shown(visual));
    type_in_editor(visual, "db.a.find()");
    vim_toggle(visual);
    assert!(vim_label_shown(visual), "the mode is shown in the status line");
    vim_cursor(&app, visual, tab, 0);
    vim_press(visual, "dw");
    // `.` is punctuation: a word of its own.
    assert_eq!(editor_text(&app, visual, tab), ".a.find()");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "db.a.find()");
}

/// In Insert mode the editor's completion menu keeps Enter (accept) and Escape (close the menu
/// first; the next Escape leaves Insert mode).
#[gpui::test]
fn the_completion_menu_keeps_enter_and_escape_in_insert_mode(cx: &mut TestAppContext) {
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
    let (tab, editor) = query_editor(&app, visual);
    vim_toggle(visual);
    vim_press(visual, "i");
    vim_press(visual, "select * from ord");
    assert!(menu_state(&editor, visual).0, "the menu opens while typing in Insert mode");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "select * from orders", "Enter accepted the suggestion");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Insert);

    vim_press(visual, " s");
    assert!(menu_state(&editor, visual).0);
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!menu_state(&editor, visual).0, "the first Escape closes the menu");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Insert, "and stays in Insert mode");
    visual.simulate_keystrokes("escape");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal, "the second one leaves it");
    assert_eq!(editor_value(&editor, visual), "select * from orders s");
}

#[gpui::test]
fn the_redis_console_status_line_has_the_switch_and_vim_edits_its_command(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    app.update(visual, |this, cx| assert!(this.new_query_for_sidebar_node(&fixture_db_node(), cx)));
    settle(visual);
    let tab = active_tab(&app, visual);
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab).unwrap().query_mode.is_redis_command()));
    assert!(visual.debug_bounds("vim-toggle").is_some(), "the switch is in the console status line");
    type_in_editor(visual, "GET user:1");
    vim_toggle(visual);
    assert!(vim_label_shown(visual));
    vim_cursor(&app, visual, tab, 0);
    vim_press(visual, "dw");
    assert_eq!(editor_text(&app, visual, tab), "user:1");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "GET user:1");
}

/// What the editor API allows: `c` and `o` / `O` are an atomic edit followed by an Insert session,
/// so undoing them takes two steps (the typed text, then the cut or the new line).
#[gpui::test]
fn a_change_command_undoes_in_two_steps(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "hello world");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    vim_press(visual, "cwbye");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "bye world");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), " world", "the typed text goes first");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "hello world", "then the cut");
}

/// Text an input method hands the editor (a composition, a dead key, an Option chord) reaches its input
/// handler without a key press, so the wrapper cannot stop it: outside Insert mode the editor refuses it.
#[gpui::test]
fn text_that_is_not_a_key_press_is_refused_outside_insert_mode(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler as _;
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abc");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 1);
    let editor = app.read_with(visual, |this, _| this.query_editors.by_tab[&tab].clone());
    let ime = |visual: &mut gpui::VisualTestContext, composed: &str| {
        editor.update_in(visual, |state, window, cx| {
            state.replace_text_in_range(None, composed, window, cx);
            state.replace_and_mark_text_in_range(None, composed, None, window, cx);
        });
        settle(visual);
    };
    ime(visual, "\u{e9}");
    assert_eq!(editor_text(&app, visual, tab), "abc", "Normal mode takes no composed text");
    // Cmd-V, Cmd-X and the delete chords are Insert mode's.
    visual.write_to_clipboard(gpui::ClipboardItem::new_string("PASTED".into()));
    visual.simulate_keystrokes("secondary-v");
    settle(visual);
    assert_eq!(editor_text(&app, visual, tab), "abc", "Cmd-V pastes nothing in Normal mode");
    vim_press(visual, "vl");
    assert_eq!(vim_selection(&app, visual, tab), 1..3);
    ime(visual, "\u{e9}");
    assert_eq!(editor_text(&app, visual, tab), "abc", "Visual mode takes none either");
    visual.simulate_keystrokes("secondary-x alt-backspace secondary-backspace secondary-v");
    settle(visual);
    assert_eq!(editor_text(&app, visual, tab), "abc", "no cut, no delete, no paste in Visual mode");
    assert_eq!(visual.read_from_clipboard().and_then(|item| item.text()), Some("PASTED".to_string()), "Cmd-X copied nothing");
    visual.simulate_keystrokes("escape");

    // Insert mode is a plain editor: the same text goes in.
    vim_press(visual, "i");
    ime(visual, "\u{e9}");
    assert!(editor_text(&app, visual, tab).contains('\u{e9}'), "Insert mode takes composed text");
    visual.simulate_keystrokes("secondary-v");
    settle(visual);
    assert!(editor_text(&app, visual, tab).contains("PASTED"), "and Cmd-V");
    visual.simulate_keystrokes("escape");
    let settled = editor_text(&app, visual, tab);
    ime(visual, "\u{e9}");
    assert_eq!(editor_text(&app, visual, tab), settled, "back in Normal mode it is refused again");
    // Undo still works behind all that (`u`, one frame later).
    vim_press(visual, "u");
    assert!(!editor_text(&app, visual, tab).contains("PASTED"), "u undid the paste");
}

/// In the platform's frame order (see `vim_press`) the line comes back with `u`, goes again with Ctrl-R,
/// and Cmd-Z / Cmd-Shift-Z do the same in Normal mode; the editor is read-only again each time.
#[gpui::test]
fn u_and_ctrl_r_and_cmd_z_undo_and_redo_in_normal_mode(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "one\ntwo\nthree");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 4);
    let editable = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| this.query_editors.by_tab[&tab].read(cx).is_editable())
    };
    vim_press(visual, "dd");
    assert_eq!(editor_text(&app, visual, tab), "one\nthree");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "one\ntwo\nthree", "u restores the line");
    assert!(!editable(&app, visual), "read-only again after the undo");
    vim_chord(visual, "ctrl-r");
    assert_eq!(editor_text(&app, visual, tab), "one\nthree", "Ctrl-R redoes it");
    assert!(!editable(&app, visual));
    vim_chord(visual, "secondary-z");
    assert_eq!(editor_text(&app, visual, tab), "one\ntwo\nthree", "Cmd-Z undoes in Normal mode");
    assert!(!editable(&app, visual));
    // The editor's redo: Cmd+Shift+Z on macOS, Ctrl+Y elsewhere (gpui-base's input bindings).
    vim_chord(visual, if cfg!(target_os = "macos") { "secondary-shift-z" } else { "ctrl-y" });
    assert_eq!(editor_text(&app, visual, tab), "one\nthree", "the platform redo chord redoes");
    assert!(!editable(&app, visual));
    // Typing still goes nowhere in Normal mode.
    vim_press(visual, "zz");
    assert_eq!(editor_text(&app, visual, tab), "one\nthree");
}

/// Layout rounds the box to whole pixels (a 7.5px advance is a 8px box): half a pixel of play.
const TOLERANCE: gpui::Pixels = gpui::px(1.0);

/// The block caret's bounds in the last drawn frame.
fn vim_block(visual: &mut gpui::VisualTestContext) -> Option<gpui::Bounds<gpui::Pixels>> {
    visual.debug_bounds("vim-block-caret")
}

/// The editor's own caret (the thin bar) of the last drawn frame, scroll applied.
fn vim_thin_caret(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> gpui::Bounds<gpui::Pixels> {
    app.read_with(visual, |this, cx| {
        let geometry = this.query_editors.by_tab[&tab].read(cx).prepaint_caret_geometry().expect("a drawn caret");
        let mut caret = geometry.cursor_bounds();
        caret.origin.y += geometry.scroll_offset().y;
        caret
    })
}

/// The block is one char wide at the editor's caret and as high as a row, centred on it.
fn assert_block_at_caret(block: gpui::Bounds<gpui::Pixels>, caret: gpui::Bounds<gpui::Pixels>, why: &str) {
    assert!(block.size.width > gpui::px(2.0), "{why}: a char wide, not a bar: {block:?}");
    assert!((block.left() - caret.left()).abs() < TOLERANCE, "{why}: at the caret's x: {block:?} vs {caret:?}");
    assert!((block.center().y - caret.center().y).abs() < TOLERANCE, "{why}: on the caret's row: {block:?} vs {caret:?}");
    assert!(block.size.height >= caret.size.height, "{why}: a whole row: {block:?} vs {caret:?}");
}

#[gpui::test]
fn normal_mode_draws_a_block_caret_that_insert_removes_and_escape_brings_back(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "select 1");
    assert!(vim_block(visual).is_none(), "Vim off: the editor's own caret only");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    let block = vim_block(visual).expect("Normal mode has a block caret");
    assert_block_at_caret(block, vim_thin_caret(&app, visual, tab), "Normal");
    // Insert mode: the thin caret again.
    vim_press(visual, "i");
    assert!(vim_block(visual).is_none(), "Insert has no block");
    visual.simulate_keystrokes("escape");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    let block = vim_block(visual).expect("Escape brings the block back");
    assert_block_at_caret(block, vim_thin_caret(&app, visual, tab), "after Escape");
    // Off again: gone.
    vim_toggle(visual);
    assert!(vim_block(visual).is_none(), "Vim off: no block");
}

#[gpui::test]
fn the_block_caret_follows_the_caret_in_the_same_frame(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abcd\nefgh\n\nxy");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    let start = vim_block(visual).expect("block");
    assert_block_at_caret(start, vim_thin_caret(&app, visual, tab), "start");
    // `l`: one char right, the box moves by its own width.
    vim_press(visual, "l");
    let right = vim_block(visual).expect("block");
    assert_block_at_caret(right, vim_thin_caret(&app, visual, tab), "after l");
    assert!(((right.left() - start.left()) - start.size.width).abs() < TOLERANCE, "moved one char: {start:?} -> {right:?}");
    // `j`: one row down, same column.
    vim_press(visual, "j");
    let down = vim_block(visual).expect("block");
    assert_block_at_caret(down, vim_thin_caret(&app, visual, tab), "after j");
    assert!(down.top() > right.top() + gpui::px(2.0), "moved a row down: {right:?} -> {down:?}");
    assert!((down.left() - right.left()).abs() < TOLERANCE, "same column");
    // An empty line: a one-space box.
    vim_press(visual, "j");
    let empty = vim_block(visual).expect("block on an empty line");
    assert_block_at_caret(empty, vim_thin_caret(&app, visual, tab), "empty line");
    // `$` then the caret is on the last char; `0` goes home.
    vim_press(visual, "$");
    let last = vim_block(visual).expect("block");
    assert_block_at_caret(last, vim_thin_caret(&app, visual, tab), "after $");
    vim_press(visual, "k0");
    assert_block_at_caret(vim_block(visual).expect("block"), vim_thin_caret(&app, visual, tab), "after k0");
}

#[gpui::test]
fn the_block_caret_covers_the_selected_head_in_visual_mode_and_is_hollow_when_unfocused(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abcdef");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    let normal = vim_block(visual).expect("block");
    // `v` selects the char under the caret: the editor's caret is after it, the box on it.
    vim_press(visual, "v");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Visual);
    let one = vim_block(visual).expect("Visual has a block caret");
    assert!((one.left() - normal.left()).abs() < TOLERANCE, "on the first char: {normal:?} vs {one:?}");
    assert!((one.right() - vim_thin_caret(&app, visual, tab).left()).abs() < TOLERANCE, "ends where the editor's caret is");
    // `ll`: the box is on the third char, the head. A char is the Normal box's width: a box left of
    // the caret is wider by the editor's bar it covers.
    vim_press(visual, "ll");
    let head = vim_block(visual).expect("block");
    assert!(((head.left() - one.left()) - normal.size.width * 2.0).abs() < TOLERANCE, "two chars on: {one:?} -> {head:?}");
    // Visual line: the lines are the feedback.
    vim_press(visual, "V");
    assert!(vim_block(visual).is_none(), "no block in Visual line");
    visual.simulate_keystrokes("escape");
    assert!(vim_block(visual).is_some());
    // Focus elsewhere: the box stays as an outline, still at the caret.
    visual.update(|window, cx| window.blur(cx));
    settle(visual);
    let hollow = vim_block(visual).expect("an unfocused editor keeps its caret as an outline");
    assert!(hollow.size.width > gpui::px(2.0));
}

#[gpui::test]
fn the_block_caret_is_not_a_frame_behind_the_editor_caret(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abcd\nefgh");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    let editor = app.read_with(visual, |this, _| this.query_editors.by_tab[&tab].clone());
    // The caret moves and exactly ONE frame is drawn behind it (a test window draws by itself after a
    // key, which would let a box that reads last frame's caret catch up): the box is already there.
    for (offset, why) in [(2, "two chars right"), (6, "next row"), (1, "back up")] {
        let previous = vim_block(visual).expect("block");
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| state.set_selected_range(offset..offset, cx));
            window.draw(cx).clear(cx);
        });
        let now = vim_block(visual).expect("block");
        assert_block_at_caret(now, vim_thin_caret(&app, visual, tab), why);
        assert_ne!(now.origin, previous.origin, "{why}: {previous:?} -> {now:?}");
    }
}

// ---- the Vim settings, the search prompt, `.` and the view ------------------------------------

/// Changes Settings > Editor (the way the page's Apply does) and lets the app settle.
fn vim_set(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    change: impl FnOnce(&mut crate::settings::EditorSettings),
) {
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        change(&mut next.editor);
        this.apply_settings(next, None, cx);
    });
    settle(visual);
}

fn vim_prompt(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> Option<String> {
    app.read_with(visual, |this, _| this.vim_prompt_text(tab))
}

fn vim_clipboard_text(visual: &gpui::VisualTestContext) -> Option<String> {
    visual.read_from_clipboard().and_then(|item| item.text())
}

/// Time passes for the app's timers (a test window has a clock of its own).
fn vim_wait(visual: &mut gpui::VisualTestContext, millis: u64) {
    visual.cx.executor().advance_clock(std::time::Duration::from_millis(millis));
    settle(visual);
}

#[gpui::test]
fn counts_dot_find_and_text_objects_work_with_real_keystrokes(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "one two three\nfour (five) six\nseven\neight");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    vim_press(visual, "2w");
    assert_eq!(vim_selection(&app, visual, tab), 8..8, "2w");
    vim_cursor(&app, visual, tab, 2);
    vim_press(visual, "2j");
    assert_eq!(vim_selection(&app, visual, tab), 32..32, "2j keeps the column");
    vim_press(visual, "gg");
    vim_press(visual, "x.");
    assert_eq!(editor_text(&app, visual, tab), "e two three\nfour (five) six\nseven\neight", "x then . takes two chars");
    vim_press(visual, "dw");
    assert_eq!(editor_text(&app, visual, tab), "two three\nfour (five) six\nseven\neight");
    vim_press(visual, "jf(");
    assert_eq!(vim_selection(&app, visual, tab), 15..15, "f( finds the bracket");
    vim_press(visual, "ci(X");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "two three\nfour (X) six\nseven\neight", "ci( changes what is inside");
    vim_press(visual, "gg2dd");
    assert_eq!(editor_text(&app, visual, tab), "seven\neight");
    vim_press(visual, "u");
    assert_eq!(editor_text(&app, visual, tab), "two three\nfour (X) six\nseven\neight", "one undo step for 2dd");
    vim_cursor(&app, visual, tab, 0);
    vim_press(visual, "3J");
    assert_eq!(editor_text(&app, visual, tab), "two three four (X) six seven\neight", "3J joins three lines");
}

#[gpui::test]
fn the_search_prompt_is_typed_in_the_status_line_and_run_with_enter(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "one two\nthree two\nfour two");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    assert!(visual.debug_bounds("vim-prompt").is_none(), "no prompt before /");
    vim_press(visual, "/two");
    assert_eq!(vim_prompt(&app, visual, tab), Some("/two".into()));
    assert!(visual.debug_bounds("vim-prompt").is_some(), "the prompt is in the status line");
    assert_eq!(editor_text(&app, visual, tab), "one two\nthree two\nfour two", "what is typed in the prompt is not text");
    assert_eq!(vim_selection(&app, visual, tab), 0..0, "nothing moves before Enter");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(vim_prompt(&app, visual, tab), None);
    assert!(visual.debug_bounds("vim-prompt").is_none());
    assert_eq!(editor_text(&app, visual, tab), "one two\nthree two\nfour two", "Enter ran the search, it did not break the line");
    assert_eq!(vim_selection(&app, visual, tab), 4..4);
    for (keys, at) in [("n", 14), ("n", 23), ("n", 4), ("N", 23)] {
        vim_press(visual, keys);
        assert_eq!(vim_selection(&app, visual, tab), at..at, "{keys}");
    }
    // Escape cancels, Backspace edits.
    vim_press(visual, "?one");
    assert_eq!(vim_prompt(&app, visual, tab), Some("?one".into()));
    visual.simulate_keystrokes("escape");
    assert_eq!(vim_prompt(&app, visual, tab), None);
    assert_eq!(vim_selection(&app, visual, tab), 23..23, "Escape searched nothing");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    vim_press(visual, "/tw");
    visual.simulate_keystrokes("backspace");
    assert_eq!(vim_prompt(&app, visual, tab), Some("/t".into()));
    assert_eq!(editor_text(&app, visual, tab), "one two\nthree two\nfour two", "Backspace did not delete text");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(vim_selection(&app, visual, tab), 4..4, "the next t wraps to the first line");
    // `*` searches the word under the cursor, in whole words.
    vim_cursor(&app, visual, tab, 4);
    vim_press(visual, "*");
    assert_eq!(vim_selection(&app, visual, tab), 14..14);
}

#[gpui::test]
fn search_follows_the_ignore_case_and_smart_case_settings(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "x foo Foo");
    vim_toggle(visual);
    let search = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, pattern: &str| {
        vim_cursor(app, visual, tab, 0);
        vim_press(visual, &format!("/{pattern}"));
        visual.simulate_keystrokes("enter");
        settle(visual);
        vim_selection(app, visual, tab).start
    };
    assert_eq!(search(&app, visual, "Foo"), 6, "smart case: a capital letter is exact");
    vim_set(&app, visual, |editor| editor.vim_smart_case = false);
    assert_eq!(search(&app, visual, "Foo"), 2, "without smart case the capital is ignored");
    vim_set(&app, visual, |editor| editor.vim_ignore_case = false);
    assert_eq!(search(&app, visual, "Foo"), 6, "case sensitive");
    assert_eq!(search(&app, visual, "foo"), 2);
    vim_set(&app, visual, |editor| editor.vim_ignore_case = true);
    assert_eq!(search(&app, visual, "FOO"), 2, "ignore case");
}

#[gpui::test]
fn the_clipboard_setting_decides_what_reaches_and_comes_from_the_system_clipboard(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "one\ntwo\nthree");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    let write = |visual: &mut gpui::VisualTestContext, text: &str| {
        visual.write_to_clipboard(gpui::ClipboardItem::new_string(text.into()));
    };
    // On yank (the default): a delete leaves the clipboard alone, a yank writes it.
    write(visual, "KEEP");
    vim_press(visual, "dd");
    assert_eq!(editor_text(&app, visual, tab), "two\nthree");
    assert_eq!(vim_clipboard_text(visual), Some("KEEP".into()), "a delete is not copied");
    vim_press(visual, "yy");
    assert_eq!(vim_clipboard_text(visual), Some("two\n".into()), "a yank is");
    // Always: a delete is copied, and `p` puts what the clipboard holds.
    vim_set(&app, visual, |editor| editor.vim_clipboard = crate::settings::VimClipboard::Always);
    vim_press(visual, "jdd");
    assert_eq!(editor_text(&app, visual, tab), "two");
    assert_eq!(vim_clipboard_text(visual), Some("three\n".into()), "a delete is copied");
    write(visual, "EXT");
    vim_press(visual, "p");
    assert_eq!(editor_text(&app, visual, tab), "tEXTwo", "p puts the system clipboard, not Vim's register");
    // Never: the clipboard is not touched in either direction.
    vim_set(&app, visual, |editor| editor.vim_clipboard = crate::settings::VimClipboard::Never);
    write(visual, "KEEP2");
    vim_cursor(&app, visual, tab, 0);
    vim_press(visual, "yl$p");
    assert_eq!(vim_clipboard_text(visual), Some("KEEP2".into()), "a yank is not copied");
    assert_eq!(editor_text(&app, visual, tab), "tEXTwot", "p puts Vim's own register");
}

#[gpui::test]
fn the_escape_sequence_leaves_insert_mode_and_its_keys_never_stay_in_the_text(cx: &mut TestAppContext) {
    use crate::vim::Mode::{Insert, Normal};
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abc");
    vim_set(&app, visual, |editor| editor.vim_escape_sequence = "jk".into());
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    // jk: no j, no k, Normal mode.
    vim_press(visual, "Axy");
    vim_press(visual, "jk");
    assert_eq!(editor_text(&app, visual, tab), "abcxy");
    assert_eq!(vim_mode_of(&app, visual, tab), Normal);
    // A j alone is held back, then typed when the time is up.
    vim_press(visual, "Aj");
    assert_eq!(editor_text(&app, visual, tab), "abcxy", "the first key is held, not typed yet");
    assert_eq!(vim_mode_of(&app, visual, tab), Insert);
    vim_wait(visual, 299);
    assert_eq!(editor_text(&app, visual, tab), "abcxy", "still within the timeout");
    vim_wait(visual, 2);
    assert_eq!(editor_text(&app, visual, tab), "abcxyj", "the timeout types it");
    assert_eq!(vim_mode_of(&app, visual, tab), Insert);
    // j then another key: both are typed, in order.
    vim_press(visual, "jx");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjx");
    // jjk: the first j is text, the second and k leave.
    vim_press(visual, "jjk");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxj");
    assert_eq!(vim_mode_of(&app, visual, tab), Normal);
    // j then Escape: the j is typed first.
    vim_press(visual, "Aj");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxjj");
    assert_eq!(vim_mode_of(&app, visual, tab), Normal);
    // Ctrl-[ is Escape too: the held j is typed first.
    vim_press(visual, "Aj");
    visual.simulate_keystrokes("ctrl-[");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxjjj");
    assert_eq!(vim_mode_of(&app, visual, tab), Normal);
    // A timer left from a key that was typed does not type it twice.
    vim_wait(visual, 1000);
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxjjj");
    // The second key alone is plain text.
    vim_press(visual, "Ak");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxjjjk");
    visual.simulate_keystrokes("escape");
    // Same key twice.
    vim_set(&app, visual, |editor| editor.vim_escape_sequence = "jj".into());
    vim_press(visual, "Ajj");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxjjjk");
    assert_eq!(vim_mode_of(&app, visual, tab), Normal);
    // Off: typed as they come.
    vim_set(&app, visual, |editor| editor.vim_escape_sequence = String::new());
    vim_press(visual, "Ajk");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxjjjkjk");
    assert_eq!(vim_mode_of(&app, visual, tab), Insert);
    // One key is not a sequence.
    vim_set(&app, visual, |editor| editor.vim_escape_sequence = "j".into());
    vim_press(visual, "j");
    assert_eq!(editor_text(&app, visual, tab), "abcxyjjxjjjkjkj");
}

#[gpui::test]
fn a_held_escape_key_is_typed_before_a_key_that_moves_the_caret_or_the_selection(cx: &mut TestAppContext) {
    use crate::vim::Mode::{Insert, Normal};
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abc");
    vim_set(&app, visual, |editor| editor.vim_escape_sequence = "jk".into());
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    let mut expected = String::from("abc");
    for chord in ["shift-left", "secondary-a", "home", "alt-left", "shift-end"] {
        vim_press(visual, "Aj");
        assert_eq!(editor_text(&app, visual, tab), expected, "{chord}: the j is held");
        vim_chord(visual, chord);
        // The timer must find nothing left to type: the j is already in the text, where it was
        // pressed, and the key's selection (or caret move) was not typed over.
        vim_wait(visual, 2100);
        expected.push('j');
        assert_eq!(editor_text(&app, visual, tab), expected, "{chord}: typed once, at the caret it was pressed at");
        assert_eq!(vim_mode_of(&app, visual, tab), Insert, "{chord}");
        visual.simulate_keystrokes("escape");
        assert_eq!(vim_mode_of(&app, visual, tab), Normal, "{chord}");
    }
}

#[gpui::test]
fn a_count_and_dot_repeat_only_what_was_typed_after_the_caret_moved_in_insert(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abc def");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    // 3iX, End, Esc: the move ended the insert, so the count has nothing to repeat.
    vim_press(visual, "3iX");
    vim_chord(visual, "end");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "Xabc def");
    // 3iX, End, Y, Esc, `.`: Y only, once more.
    vim_cursor(&app, visual, tab, 0);
    vim_press(visual, "3iX");
    vim_chord(visual, "end");
    vim_press(visual, "Y");
    visual.simulate_keystrokes("escape");
    assert_eq!(editor_text(&app, visual, tab), "XXabc defY");
    vim_press(visual, ".");
    assert_eq!(editor_text(&app, visual, tab), "XXabc defYY");
}

#[gpui::test]
fn ctrl_keys_are_vims_until_the_setting_leaves_them_to_the_editor(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let text: String = (0..120).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let tab = vim_sql_tab(&app, visual, &text);
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    let row = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        editor_text(app, visual, tab)[..vim_selection(app, visual, tab).start].matches('\n').count()
    };
    let half = app.read_with(visual, |this, cx| this.query_editors.by_tab[&tab].read(cx).visible_row_range().expect("drawn").len() / 2);
    assert!(half >= 3, "a window with rows: {half}");
    vim_chord(visual, "ctrl-d");
    assert_eq!(row(&app, visual), half, "Ctrl-D: half a page down");
    vim_chord(visual, "ctrl-u");
    assert_eq!(row(&app, visual), 0, "Ctrl-U: back");
    // Copy and select all are the app's in every mode.
    vim_press(visual, "Vj");
    vim_chord(visual, "secondary-c");
    assert_eq!(vim_clipboard_text(visual).as_deref().map(|text| text.trim_end()), Some("line 0\nline 1"), "copy works in Visual mode");
    visual.simulate_keystrokes("escape");
    vim_chord(visual, "secondary-a");
    assert_eq!(vim_selection(&app, visual, tab), 0..text.len(), "select all works in Normal mode");
    vim_cursor(&app, visual, tab, 0);
    // Off: Ctrl-D, Ctrl-U and Ctrl-R are not Vim's.
    vim_set(&app, visual, |editor| editor.vim_ctrl_keys = false);
    vim_chord(visual, "ctrl-d");
    assert_eq!(row(&app, visual), 0, "Ctrl-D is left alone");
    vim_press(visual, "ddu");
    assert_eq!(editor_text(&app, visual, tab), text);
    vim_chord(visual, "ctrl-r");
    assert_eq!(editor_text(&app, visual, tab), text, "Ctrl-R is not a redo now");
    // Escape works either way.
    vim_press(visual, "i");
    visual.simulate_keystrokes("escape");
    assert_eq!(vim_mode_of(&app, visual, tab), crate::vim::Mode::Normal);
    vim_set(&app, visual, |editor| editor.vim_ctrl_keys = true);
    vim_chord(visual, "ctrl-r");
    assert_eq!(editor_text(&app, visual, tab), text.split_once('\n').unwrap().1, "and with the setting on it redoes again");
}

#[gpui::test]
fn a_yank_is_highlighted_for_the_configured_time(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let tab = vim_sql_tab(&app, visual, "abc def ghi\nsecond");
    vim_toggle(visual);
    vim_cursor(&app, visual, tab, 0);
    assert!(visual.debug_bounds("vim-yank-highlight").is_none());
    let caret = vim_block(visual).expect("block caret");
    vim_press(visual, "yw");
    let flash = visual.debug_bounds("vim-yank-highlight").expect("a yank is highlighted");
    assert!((flash.left() - caret.left()).abs() < TOLERANCE * 2.0, "starts at the text: {flash:?} vs {caret:?}");
    assert!((flash.size.width - caret.size.width * 4.0).abs() < TOLERANCE * 4.0, "four chars wide: {flash:?} vs {caret:?}");
    vim_wait(visual, 150);
    assert!(visual.debug_bounds("vim-yank-highlight").is_some(), "still there after 150 ms");
    vim_wait(visual, 100);
    assert!(visual.debug_bounds("vim-yank-highlight").is_none(), "gone after the 200 ms");
    // The next key ends it at once.
    vim_press(visual, "yw");
    assert!(visual.debug_bounds("vim-yank-highlight").is_some());
    vim_press(visual, "l");
    assert!(visual.debug_bounds("vim-yank-highlight").is_none(), "a key moves on");
    // A line yank covers the line; a delete highlights nothing.
    vim_press(visual, "yy");
    let line = visual.debug_bounds("vim-yank-highlight").expect("a line yank is highlighted");
    assert!(line.size.width > caret.size.width * 10.0, "the line's width: {line:?}");
    vim_wait(visual, 250);
    vim_press(visual, "x");
    assert!(visual.debug_bounds("vim-yank-highlight").is_none(), "x is not a yank");
    // 0 turns it off.
    vim_set(&app, visual, |editor| editor.vim_highlight_yank = 0);
    vim_press(visual, "yw");
    assert!(visual.debug_bounds("vim-yank-highlight").is_none(), "off");
    // A longer time lasts longer.
    vim_set(&app, visual, |editor| editor.vim_highlight_yank = 1000);
    vim_press(visual, "yw");
    vim_wait(visual, 900);
    assert!(visual.debug_bounds("vim-yank-highlight").is_some());
    vim_wait(visual, 200);
    assert!(visual.debug_bounds("vim-yank-highlight").is_none());
}

#[gpui::test]
fn the_start_mode_setting_decides_the_mode_of_a_new_editor_and_of_a_switch(cx: &mut TestAppContext) {
    use crate::settings::VimDefaultMode;
    use crate::vim::Mode::{Insert, Normal};
    let (app, visual) = layout_app(cx);
    let first = vim_sql_tab(&app, visual, "abc");
    vim_set(&app, visual, |editor| editor.vim_default_mode = VimDefaultMode::Insert);
    // The switch starts in the setting's mode.
    vim_toggle(visual);
    assert_eq!(vim_mode_of(&app, visual, first), Insert);
    vim_press(visual, "X");
    assert_eq!(editor_text(&app, visual, first), "abcX", "typing is text in Insert");
    visual.simulate_keystrokes("escape");
    assert_eq!(vim_mode_of(&app, visual, first), Normal);
    // A new editor with Vim on in Settings.
    vim_set(&app, visual, |editor| editor.vim_mode = true);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let second = active_tab(&app, visual);
    assert_eq!(vim_mode_of(&app, visual, second), Insert, "a new editor starts in Insert");
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    visual.simulate_click(gpui::point(area.center().x, area.bottom() - gpui::px(40.0)), Default::default());
    settle(visual);
    vim_press(visual, "hello");
    assert_eq!(editor_text(&app, visual, second), "hello");
    // And the other way.
    vim_set(&app, visual, |editor| editor.vim_default_mode = VimDefaultMode::Normal);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let third = active_tab(&app, visual);
    assert_eq!(vim_mode_of(&app, visual, third), Normal);
}

#[gpui::test]
fn z_commands_scroll_the_cursor_line_to_the_top_middle_and_bottom(cx: &mut TestAppContext) {
    let (app, visual) = layout_app(cx);
    let text: String = (0..300).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
    let tab = vim_sql_tab(&app, visual, &text);
    vim_toggle(visual);
    let middle = text.find("l150\n").unwrap();
    vim_cursor(&app, visual, tab, middle);
    let place = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| {
            let geometry = this.query_editors.by_tab[&tab].read(cx).prepaint_caret_geometry().expect("drawn");
            let caret = vim_thin_caret(app, visual, tab);
            (caret, geometry.input_bounds(), geometry.line_height())
        })
    };
    let mut tops = Vec::new();
    for (keys, name) in [("zt", "top"), ("zz", "middle"), ("zb", "bottom")] {
        vim_press(visual, keys);
        let (caret, area, row) = place(&app, visual);
        assert_eq!(vim_selection(&app, visual, tab), middle..middle, "{keys}: the cursor stays");
        match name {
            "top" => assert!((caret.top() - area.top()).abs() < row * 2.0, "zt: {caret:?} in {area:?}"),
            "middle" => assert!((caret.center().y - area.center().y).abs() < row, "zz: {caret:?} in {area:?}"),
            _ => assert!((area.bottom() - caret.bottom()).abs() < row * 2.0, "zb: {caret:?} in {area:?}"),
        }
        tops.push(caret.top());
    }
    assert!(tops[0] < tops[1] && tops[1] < tops[2], "the line moves down the view: {tops:?}");
}

#[gpui::test]
fn the_vim_settings_have_their_own_group_with_every_row(cx: &mut TestAppContext) {
    let _ = cx;
    let rows: Vec<_> = crate::settings::registry::registry()
        .iter()
        .filter(|descriptor| descriptor.group == "Vim")
        .map(|descriptor| descriptor.id)
        .collect();
    assert_eq!(
        rows,
        [
            "editor.vim_mode",
            "editor.vim_default_mode",
            "editor.vim_clipboard",
            "editor.vim_escape_sequence",
            "editor.vim_escape_timeout",
            "editor.vim_highlight_yank",
            "editor.vim_ctrl_keys",
            "editor.vim_ignore_case",
            "editor.vim_smart_case",
        ]
    );
}
