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
    // Letters in Normal mode are commands (or nothing), never text.
    vim_press(visual, "zqZ:/f,;");
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
    vim_chord(visual, "secondary-shift-z");
    assert_eq!(editor_text(&app, visual, tab), "one\nthree", "Cmd-Shift-Z redoes");
    assert!(!editable(&app, visual));
    // Typing still goes nowhere in Normal mode.
    vim_press(visual, "zz");
    assert_eq!(editor_text(&app, visual, tab), "one\nthree");
}
