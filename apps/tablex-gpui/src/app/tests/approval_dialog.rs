// The approval dialog of an MCP client that cannot ask through MCP elicitation: it appears with
// what the client wants to run, Approve and Decline answer through the channel, Escape and Enter
// decline, questions are shown one at a time in order, a withdrawn question goes away, and
// nothing is ever answered for the user. Approve cannot be hit by accident: it waits after the
// dialog opens, after the window is activated and after the keyboard lands on it, it ignores the
// click that brought the window forward, and it waits for the end of a long statement to be seen.
// The buttons are clicked by their measured bounds, and one test goes through the real socket with
// the signed question and answer.

use engine_connect::app_channel::ApprovalAsk;

fn approval_ask(id: &str, statement: &str) -> ApprovalAsk {
    ApprovalAsk {
        id: id.into(),
        title: "Run this statement".into(),
        client: Some("test-agent".into()),
        connection: Some("Warehouse".into()),
        connection_type: Some("Development".into()),
        database: Some("shop".into()),
        danger: Some("DELETE without WHERE removes every row".into()),
        notes: vec!["The statement has 1 line.".into()],
        statements: vec![statement.into()],
        ..ApprovalAsk::default()
    }
}

/// A question that is not dangerous: its Approve can be pressed with Space.
fn plain_ask(id: &str, statement: &str) -> ApprovalAsk {
    ApprovalAsk { danger: None, ..approval_ask(id, statement) }
}

/// A key pressed and released (a button is pressed on the release).
fn press(visual: &mut gpui::VisualTestContext, key: &str) {
    press_key_with_release(visual, key);
    settle(visual);
}

fn focus_approve(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    let approve = app.read_with(visual, |this, _| this.approvals.approve_focus()).expect("Approve is on screen");
    visual.update(|window, cx| approve.focus(window, cx));
    settle(visual);
}

fn decline_focused(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> bool {
    visual.update(|window, cx| app.read(cx).approvals.decline_is_focused(window))
}

/// A statement taller than the box (200 lines).
fn long_statement() -> String {
    (0..200).map(|n| format!("UPDATE t SET a = {n} WHERE id = {n};")).collect::<Vec<_>>().join("\n")
}

/// Puts a question to the app as the channel does and returns where its answer will come.
fn ask_the_app(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    ask: ApprovalAsk,
) -> std::sync::mpsc::Receiver<Reply> {
    let (reply, answers) = std::sync::mpsc::channel();
    app.update(visual, |this, cx| this.enqueue_approval(ask, reply, cx));
    settle(visual);
    answers
}

fn approval_ids(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> (Option<String>, Vec<String>) {
    app.read_with(visual, |this, _| this.approvals.ids())
}

fn approval_open(visual: &mut gpui::VisualTestContext) -> bool {
    visual.debug_bounds("approval-statement").is_some()
}

fn can_approve(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> bool {
    app.read_with(visual, |this, _| this.approvals.can_approve())
}

/// Waits for the dialog's exit.
fn wait_approval_closed(visual: &mut gpui::VisualTestContext) {
    for _ in 0..200 {
        settle(visual);
        if !approval_open(visual) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the approval dialog never closed");
}

fn wait_approval_open(visual: &mut gpui::VisualTestContext) {
    for _ in 0..200 {
        settle(visual);
        if approval_open(visual) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the approval dialog never opened");
}

fn the_answer(answers: &std::sync::mpsc::Receiver<Reply>) -> bool {
    let reply = answers.try_recv().expect("the question was answered");
    assert!(reply.ok, "{reply:?}");
    reply.answer.expect("an answer").approve
}

/// The question ended with no answer: an error reply (not delivered), never an approval.
fn no_answer(answers: &std::sync::mpsc::Receiver<Reply>) -> String {
    let reply = answers.try_recv().expect("the question was ended");
    assert!(!reply.ok && reply.answer.is_none(), "{reply:?}");
    reply.error.expect("an error says why")
}

/// Past the moment after which Approve counts, and the dialog repainted with it enabled.
fn wait_until_armed(visual: &mut gpui::VisualTestContext) {
    std::thread::sleep(super::approval_dialog::ARM_DELAY + Duration::from_millis(60));
    settle(visual);
}

fn first_mouse_click(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
    visual.simulate_event(gpui::MouseDownEvent {
        position: bounds.center(),
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 1,
        first_mouse: true,
    });
    visual.simulate_event(gpui::MouseUpEvent {
        position: bounds.center(),
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 1,
    });
    settle(visual);
}

#[gpui::test]
fn the_question_appears_in_a_modal_dialog_with_what_the_client_wants_to_run(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    for selector in [
        "approval-intro",
        "approval-ask",
        "approval-client",
        "approval-connection",
        "approval-type",
        "approval-database",
        "approval-danger",
        "approval-size",
        "approval-statement",
        "approval-decline",
        "approval-approve",
    ] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} is shown");
    }
    assert!(visual.debug_bounds("approval-collection").is_none(), "no collection was given");
    assert!(visual.debug_bounds("approval-change-0").is_none(), "one statement is not numbered");
    // Nothing is answered by itself, and Approve does not work yet.
    assert!(answers.try_recv().is_err());
    assert!(!can_approve(&app, visual));
    assert_eq!(approval_ids(&app, visual), (Some("q1".into()), vec![]));
    // The statement box scrolls when the statement is long.
    let _more = ask_the_app(&app, visual, approval_ask("q2", &long_statement()));
    assert_eq!(approval_ids(&app, visual).1, ["q2"], "queued behind the first");
    let bounds = visual.debug_bounds("approval-statement").unwrap();
    assert!(bounds.size.height <= gpui::px(250.0), "{:?}", bounds.size);
}

#[gpui::test]
fn the_changes_of_a_commit_are_numbered_by_the_app(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let mut ask = approval_ask("q1", "INSERT INTO t VALUES (1)");
    ask.statements.push("DELETE FROM t WHERE a = 2".into());
    let _answers = ask_the_app(&app, visual, ask);
    wait_approval_open(visual);
    assert!(visual.debug_bounds("approval-change-0").is_some() && visual.debug_bounds("approval-change-1").is_some());
    assert!(visual.debug_bounds("approval-change-2").is_none());
}

#[gpui::test]
fn decline_answers_no_and_closes_the_dialog(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    click_debug_button(visual, "approval-decline");
    wait_approval_closed(visual);
    assert!(!the_answer(&answers));
    assert!(answers.try_recv().is_err(), "answered once");
    assert_eq!(approval_ids(&app, visual), (None, vec![]));
}

#[gpui::test]
fn approve_answers_yes_but_not_before_the_dialog_has_been_quiet_a_moment(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "UPDATE items SET a = 1 WHERE id = 1"));
    wait_approval_open(visual);
    // A click right away (meant for what was under the dialog, or for the dialog before it) is ignored.
    click_debug_button(visual, "approval-approve");
    settle(visual);
    assert!(approval_open(visual), "still asking");
    assert!(answers.try_recv().is_err(), "nothing was answered");
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    click_debug_button(visual, "approval-approve");
    wait_approval_closed(visual);
    assert!(the_answer(&answers));
}

#[gpui::test]
fn the_click_that_brings_the_window_forward_does_not_approve(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "UPDATE items SET a = 1 WHERE id = 1"));
    wait_approval_open(visual);
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    // The window's first-focus click (macOS `acceptsFirstMouse`) lands on an armed Approve: refused.
    first_mouse_click(visual, "approval-approve");
    assert!(approval_open(visual), "the first-focus click is not an approval");
    assert!(answers.try_recv().is_err());
    // The same click without the flag is the user's.
    click_debug_button(visual, "approval-approve");
    wait_approval_closed(visual);
    assert!(the_answer(&answers));
}

#[gpui::test]
fn the_window_coming_to_the_front_starts_the_delay_again(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "UPDATE items SET a = 1 WHERE id = 1"));
    wait_approval_open(visual);
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    // Another window takes the front, then this one is activated again: the click that did it may be
    // the next one, and must find Approve disarmed.
    let _elsewhere = visual.cx.add_empty_window();
    settle(visual);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    assert!(!can_approve(&app, visual), "activation disarms Approve");
    click_debug_button(visual, "approval-approve");
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "the click is ignored");
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    click_debug_button(visual, "approval-approve");
    wait_approval_closed(visual);
    assert!(the_answer(&answers));
}

#[gpui::test]
fn a_statement_longer_than_the_box_cannot_be_approved_until_its_end_has_been_seen(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", &long_statement()));
    wait_approval_open(visual);
    wait_until_armed(visual);
    assert!(!can_approve(&app, visual), "the end is below the fold");
    click_debug_button(visual, "approval-approve");
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "Approve does nothing yet");
    // Scrolled to the end: now it works.
    let scroll = app.read_with(visual, |this, _| this.approvals.scroll()).expect("the box");
    assert!(scroll.max_offset().y > gpui::px(100.0), "the box really scrolls");
    scroll.set_offset(gpui::point(gpui::px(0.0), -scroll.max_offset().y));
    settle(visual);
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    click_debug_button(visual, "approval-approve");
    wait_approval_closed(visual);
    assert!(the_answer(&answers));
}

#[gpui::test]
fn escape_declines(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    // Typed in a hurry (the dialog had not been quiet): ignored, and not a decline.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "Escape right after the dialog opened is ignored");
    visual.simulate_keystrokes("a");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "nor right after typing");
    wait_until_armed(visual);
    visual.simulate_keystrokes("escape");
    wait_approval_closed(visual);
    assert!(!the_answer(&answers));
}

#[gpui::test]
fn enter_presses_decline_because_it_has_the_focus(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    wait_until_armed(visual);
    // Decline has the focus when the dialog opens.
    assert!(visual.update(|window, cx| app.read(cx).approvals.decline_is_focused(window)), "Decline has the focus");
    visual.simulate_keystrokes("enter");
    wait_approval_closed(visual);
    assert!(!the_answer(&answers), "Enter must not approve");
}

#[gpui::test]
fn the_dialogs_confirm_action_declines_and_space_alone_approves_a_plain_question(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    wait_until_armed(visual);
    // The dialog's confirm action (what Enter reaches when no button has the focus) declines.
    visual.update(|window, cx| window.dispatch_action(Box::new(gpui_component::dialog::Confirm { secondary: false }), cx));
    wait_approval_closed(visual);
    assert!(!the_answer(&answers), "the dialog's confirm action is a decline");
    // A plain question: the keyboard landing on Approve starts the delay again ...
    let answers = ask_the_app(&app, visual, plain_ask("q2", "DELETE FROM items WHERE id = 1"));
    wait_approval_open(visual);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    wait_until_armed(visual);
    focus_approve(&app, visual);
    assert!(!can_approve(&app, visual), "focus on Approve disarms it");
    // ... Enter never approves, at once or after a quiet moment: it hands the focus to Decline.
    press(visual, "enter");
    assert!(approval_open(visual) && answers.try_recv().is_err() && decline_focused(&app, visual));
    focus_approve(&app, visual);
    wait_until_armed(visual);
    press(visual, "enter");
    assert!(approval_open(visual) && answers.try_recv().is_err(), "Enter never approves");
    assert!(decline_focused(&app, visual), "and it hands the focus to Decline");
    // Space on an armed, focused Approve is the user's.
    focus_approve(&app, visual);
    press(visual, "space");
    assert!(approval_open(visual) && answers.try_recv().is_err(), "Space right after the focus landed does nothing");
    focus_approve(&app, visual);
    wait_until_armed(visual);
    press(visual, "space");
    wait_approval_closed(visual);
    assert!(the_answer(&answers), "Space on the armed, focused Approve approves");
}

#[gpui::test]
fn repeated_presses_on_a_disarmed_approve_never_approve_however_they_are_spaced(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, plain_ask("q1", "DELETE FROM items WHERE id = 1"));
    wait_approval_open(visual);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    wait_until_armed(visual);
    // Space every 250 ms with the focus on Approve (the first press hands it to Decline, and the
    // later ones are not a decision either).
    focus_approve(&app, visual);
    let started = std::time::Instant::now();
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(250));
        press(visual, "space");
    }
    assert!(started.elapsed() > super::approval_dialog::ARM_DELAY * 2);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "repeated Space never approved (or declined)");
    // The same with the focus put back on Approve before each press (a Tab each time).
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(250));
        focus_approve(&app, visual);
        press(visual, "space");
    }
    assert!(approval_open(visual) && answers.try_recv().is_err());
    // And Enter every 250 ms, on Approve or on Decline.
    focus_approve(&app, visual);
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(250));
        press(visual, "enter");
    }
    assert!(approval_open(visual) && answers.try_recv().is_err(), "repeated Enter never decided anything");
}

#[gpui::test]
fn a_held_key_never_approves(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, plain_ask("q1", "DELETE FROM items WHERE id = 1"));
    wait_approval_open(visual);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    wait_until_armed(visual);
    focus_approve(&app, visual);
    // Space held down: the first press and then auto-repeats, for longer than the quiet period.
    let space = gpui::Keystroke::parse("space").unwrap();
    visual.simulate_event(gpui::KeyDownEvent { keystroke: space.clone(), is_held: false, prefer_character_input: false });
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(150));
        visual.simulate_event(gpui::KeyDownEvent { keystroke: space.clone(), is_held: true, prefer_character_input: false });
        settle(visual);
    }
    visual.simulate_event(gpui::KeyUpEvent { keystroke: space });
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "a held key approved nothing");
    assert!(!can_approve(&app, visual), "and Approve is still disarmed");
}

#[gpui::test]
fn a_dangerous_statement_and_a_commit_are_approved_by_a_click_only_and_say_so(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    // A dangerous statement: Space on an armed, focused Approve does nothing; a click does.
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    assert!(visual.debug_bounds("approval-click-hint").is_some(), "the hint is under the buttons");
    visual.update(|window, _| window.activate_window());
    settle(visual);
    wait_until_armed(visual);
    focus_approve(&app, visual);
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    press(visual, "space");
    assert!(approval_open(visual) && answers.try_recv().is_err(), "the keyboard does nothing on Approve");
    wait_until_armed(visual);
    click_debug_button(visual, "approval-approve");
    wait_approval_closed(visual);
    assert!(the_answer(&answers), "a click on the armed Approve approves");
    // A commit (flagged by the sender, nothing dangerous about it): the same.
    let commit = ApprovalAsk { click_only: true, ..plain_ask("q2", "INSERT INTO items VALUES (1)") };
    let answers = ask_the_app(&app, visual, commit);
    wait_approval_open(visual);
    assert!(visual.debug_bounds("approval-click-hint").is_some());
    visual.update(|window, _| window.activate_window());
    settle(visual);
    focus_approve(&app, visual);
    wait_until_armed(visual);
    press(visual, "space");
    assert!(approval_open(visual) && answers.try_recv().is_err());
    wait_until_armed(visual);
    click_debug_button(visual, "approval-approve");
    wait_approval_closed(visual);
    assert!(the_answer(&answers));
    // Any other question has no hint.
    let _plain = ask_the_app(&app, visual, plain_ask("q3", "INSERT INTO items VALUES (2)"));
    wait_approval_open(visual);
    assert!(visual.debug_bounds("approval-click-hint").is_none());
}

#[gpui::test]
fn typing_after_the_keyboard_landed_on_approve_ends_on_decline_and_needs_a_quiet_moment(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, plain_ask("q1", "DELETE FROM items WHERE id = 1"));
    wait_approval_open(visual);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    wait_until_armed(visual);
    // The user is typing: a Tab lands on Approve (the dialog came up under their hands) ...
    focus_approve(&app, visual);
    // ... and they go on typing, letters spread over more than the quiet period.
    let started = std::time::Instant::now();
    for letter in ["h", "e", "l", "l", "o"] {
        std::thread::sleep(Duration::from_millis(300));
        visual.simulate_keystrokes(letter);
        settle(visual);
    }
    assert!(started.elapsed() > super::approval_dialog::ARM_DELAY, "typed for longer than the delay");
    assert!(!can_approve(&app, visual), "every key started the quiet period again");
    assert!(decline_focused(&app, visual), "the first letter handed the focus back to Decline");
    // The Space (or Enter) that follows the last letter decides nothing.
    press(visual, "space");
    press(visual, "enter");
    assert!(approval_open(visual) && answers.try_recv().is_err(), "no decision right after typing");
    // After a quiet moment the user can Tab to Approve and press Space.
    wait_until_armed(visual);
    focus_approve(&app, visual);
    wait_until_armed(visual);
    press(visual, "space");
    wait_approval_closed(visual);
    assert!(the_answer(&answers));
}

#[gpui::test]
fn enter_typed_in_a_hurry_is_ignored_and_only_a_quiet_enter_declines(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    wait_until_armed(visual);
    visual.simulate_keystrokes("a");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "an Enter right after typing is not a decline");
    wait_until_armed(visual);
    visual.simulate_keystrokes("enter");
    wait_approval_closed(visual);
    assert!(!the_answer(&answers));
}

#[gpui::test]
fn clicks_on_a_disarmed_approve_the_footer_or_the_backdrop_start_the_quiet_period_again(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    // Clicks on Approve every 250 ms while it is disarmed: each one is input, so the dialog is
    // never quiet for the delay, and none of them approves (the one after 700 ms would, if the
    // clicks that a disabled button swallows did not restart the wait).
    let started = std::time::Instant::now();
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(250));
        click_debug_button(visual, "approval-approve");
        settle(visual);
    }
    assert!(started.elapsed() > super::approval_dialog::ARM_DELAY * 2);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "repeated clicks never approved");
    assert!(!can_approve(&app, visual));
    // Armed after a quiet moment; a click on the backdrop (outside the dialog) disarms it again, and
    // so does one on the footer next to the buttons.
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    visual.simulate_click(gpui::point(gpui::px(5.0), gpui::px(400.0)), Default::default());
    settle(visual);
    assert!(!can_approve(&app, visual), "a click on the backdrop is input");
    click_debug_button(visual, "approval-approve");
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err(), "the click right after it is ignored");
    wait_until_armed(visual);
    let hint = visual.debug_bounds("approval-click-hint").expect("the hint");
    visual.simulate_click(hint.center(), Default::default());
    settle(visual);
    assert!(!can_approve(&app, visual), "a click in the footer is input");
    // A click on an armed Approve is the one that does not restart it, and it approves.
    wait_until_armed(visual);
    click_debug_button(visual, "approval-approve");
    wait_approval_closed(visual);
    assert!(the_answer(&answers));
}

#[gpui::test]
fn a_command_key_is_let_through_on_approve_and_a_commit_is_click_only_by_its_title(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    // Cmd-C with the focus on Approve: not swallowed, the focus stays (it still restarts the quiet).
    let _answers = ask_the_app(&app, visual, plain_ask("q1", "DELETE FROM items WHERE id = 1"));
    wait_approval_open(visual);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    focus_approve(&app, visual);
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    visual.simulate_keystrokes("cmd-c");
    settle(visual);
    assert!(!decline_focused(&app, visual), "a command key does not hand the focus to Decline");
    assert!(!can_approve(&app, visual), "but it is input: the quiet starts again");
    visual.simulate_keystrokes("x");
    assert!(decline_focused(&app, visual), "a plain key does");
    click_debug_button(visual, "approval-decline");
    wait_approval_closed(visual);
    // A question titled as a commit is click-only whatever its flag says.
    let by_title = ApprovalAsk { title: "Commit this transaction".into(), ..plain_ask("q2", "INSERT INTO items VALUES (1)") };
    let answers = ask_the_app(&app, visual, by_title);
    wait_approval_open(visual);
    assert!(visual.debug_bounds("approval-click-hint").is_some());
    visual.update(|window, _| window.activate_window());
    settle(visual);
    focus_approve(&app, visual);
    wait_until_armed(visual);
    press(visual, "space");
    assert!(approval_open(visual) && answers.try_recv().is_err(), "Space does nothing on a commit");
}

#[gpui::test]
fn a_click_on_the_body_starts_the_quiet_period_again(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let _answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    click_debug_button(visual, "approval-ask");
    settle(visual);
    assert!(!can_approve(&app, visual), "a click on the dialog is input");
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
}

#[gpui::test]
fn the_statement_box_is_scrolled_from_the_keyboard_and_a_window_that_cuts_it_off_is_no_way_to_approve(
    cx: &mut TestAppContext,
) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", &long_statement()));
    wait_approval_open(visual);
    wait_until_armed(visual);
    assert!(!can_approve(&app, visual));
    // The box takes the keyboard: End goes to the last line.
    let focus = app.read_with(visual, |this, _| this.approvals.box_focus()).expect("the box");
    visual.update(|window, cx| focus.focus(window, cx));
    settle(visual);
    visual.simulate_keystrokes("pagedown");
    let scroll = app.read_with(visual, |this, _| this.approvals.scroll()).expect("the box");
    assert!(scroll.offset().y < gpui::px(-100.0), "Page Down scrolled the box");
    visual.simulate_keystrokes("end");
    settle(visual);
    assert!(-scroll.offset().y >= scroll.max_offset().y - gpui::px(1.0), "End reached the last line");
    wait_until_armed(visual);
    assert!(can_approve(&app, visual), "the end has been seen");
    // A window too short to show the bottom of the box: the end cannot be seen, Approve stays off.
    visual.simulate_resize(gpui::size(gpui::px(1000.0), gpui::px(300.0)));
    settle(visual);
    wait_until_armed(visual);
    assert!(!can_approve(&app, visual), "the box is cut off by the window");
    visual.simulate_resize(gpui::size(gpui::px(1000.0), gpui::px(800.0)));
    settle(visual);
    wait_until_armed(visual);
    assert!(can_approve(&app, visual));
    assert!(answers.try_recv().is_err());
}

#[gpui::test]
fn a_request_of_the_channel_does_not_take_the_window_while_a_question_is_up(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM items"));
    wait_approval_open(visual);
    assert!(app.read_with(visual, |this, _| this.approvals.busy()));
    // The request is taken up as usual; the dialog stays, unanswered.
    let reply = channel_request(&app, visual, show(crate::ui_fixture::POSTGRES_ID, Some("warehouse"), "SELECT 1", false));
    assert!(reply.ok, "{reply:?}");
    settle(visual);
    assert!(approval_open(visual) && answers.try_recv().is_err());
}

#[gpui::test]
fn two_questions_are_shown_one_at_a_time_in_order(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let first = ask_the_app(&app, visual, approval_ask("first", "DELETE FROM a"));
    let second = ask_the_app(&app, visual, approval_ask("second", "DELETE FROM b"));
    wait_approval_open(visual);
    assert_eq!(approval_ids(&app, visual), (Some("first".into()), vec!["second".into()]));
    // The second is not answered while the first is on screen.
    assert!(second.try_recv().is_err());
    wait_until_armed(visual);
    click_debug_button(visual, "approval-approve");
    wait_approval_open(visual);
    assert!(the_answer(&first));
    assert_eq!(approval_ids(&app, visual), (Some("second".into()), vec![]));
    assert!(second.try_recv().is_err(), "the first answer is not the second's");
    // The second has a moment of its own before Approve counts: the same double click cannot approve it.
    click_debug_button(visual, "approval-approve");
    settle(visual);
    assert!(second.try_recv().is_err());
    click_debug_button(visual, "approval-decline");
    wait_approval_closed(visual);
    assert!(!the_answer(&second));
    assert_eq!(approval_ids(&app, visual), (None, vec![]));
}

#[gpui::test]
fn a_withdrawn_question_leaves_the_queue_or_closes_its_dialog(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let first = ask_the_app(&app, visual, approval_ask("first", "DELETE FROM a"));
    let second = ask_the_app(&app, visual, approval_ask("second", "DELETE FROM b"));
    let third = ask_the_app(&app, visual, approval_ask("third", "DELETE FROM c"));
    wait_approval_open(visual);
    // A waiting one goes without ever being shown.
    let reply = app.update(visual, |this, cx| this.handle_channel_request(Request::Withdraw { id: "second".into() }, cx));
    assert!(reply.ok);
    assert_eq!(approval_ids(&app, visual), (Some("first".into()), vec!["third".into()]));
    assert!(matches!(second.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)), "nobody is told");
    // The one on screen (Decline has the focus) closes, and the next one takes its place.
    app.update(visual, |this, cx| this.handle_channel_request(Request::Withdraw { id: "first".into() }, cx));
    settle(visual);
    wait_approval_open(visual);
    assert!(matches!(first.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)));
    assert_eq!(approval_ids(&app, visual), (Some("third".into()), vec![]));
    // An id nobody asked about changes nothing.
    app.update(visual, |this, cx| this.handle_channel_request(Request::Withdraw { id: "nope".into() }, cx));
    assert_eq!(approval_ids(&app, visual), (Some("third".into()), vec![]));
    assert!(third.try_recv().is_err());
}

#[gpui::test]
fn a_withdrawal_never_closes_another_dialog_it_turns_its_own_into_a_notice(cx: &mut TestAppContext) {
    use gpui_component::WindowExt as _;
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let _answers = ask_the_app(&app, visual, approval_ask("q1", "DELETE FROM a"));
    wait_approval_open(visual);
    // Another dialog of the app opens over it and takes the focus.
    visual.update(|window, cx| {
        window.open_dialog(cx, |dialog, _, _| {
            dialog.title("Another").child(gpui::div().debug_selector(|| "other-dialog".into()).child("other"))
        });
    });
    settle(visual);
    assert!(visual.debug_bounds("other-dialog").is_some());
    app.update(visual, |this, cx| this.handle_channel_request(Request::Withdraw { id: "q1".into() }, cx));
    settle(visual);
    assert!(visual.debug_bounds("other-dialog").is_some(), "the other dialog was not closed");
    assert!(visual.debug_bounds("approval-withdrawn").is_some(), "the question says it was withdrawn");
    assert!(visual.debug_bounds("approval-approve").is_none(), "and cannot be approved");
    assert_eq!(approval_ids(&app, visual), (None, vec![]));
}

#[gpui::test]
fn a_dialog_closed_by_other_code_ends_its_question_and_the_next_one_is_shown(cx: &mut TestAppContext) {
    use gpui_component::WindowExt as _;
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let first = ask_the_app(&app, visual, approval_ask("first", "DELETE FROM a"));
    let second = ask_the_app(&app, visual, approval_ask("second", "DELETE FROM b"));
    wait_approval_open(visual);
    // Other code closes the top dialog (what `close_dialog` does), with no answer given.
    visual.update(|window, cx| window.close_dialog(cx));
    for _ in 0..200 {
        settle(visual);
        if approval_ids(&app, visual).0.as_deref() == Some("second") {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(approval_ids(&app, visual), (Some("second".into()), vec![]), "no later question waits for ever");
    assert!(no_answer(&first).contains("without an answer"), "the first was not delivered, not declined");
    wait_approval_open(visual);
    assert!(second.try_recv().is_err());
}

#[gpui::test]
fn quitting_ends_every_open_question_without_an_answer(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let first = ask_the_app(&app, visual, approval_ask("first", "DELETE FROM a"));
    let second = ask_the_app(&app, visual, approval_ask("second", "DELETE FROM b"));
    wait_approval_open(visual);
    app.update(visual, |this, _| this.decline_approvals());
    // Not delivered: an error reply, which is neither an approval nor a decline the user gave.
    assert!(no_answer(&first).contains("closed"));
    assert!(no_answer(&second).contains("closed"));
    assert_eq!(approval_ids(&app, visual), (None, vec![]));
    // A root that is dropped ends them too.
    let (reply, answers) = std::sync::mpsc::channel();
    let mut queue = super::approval_dialog::ApprovalQueue::default();
    queue.push_waiting_for_test(approval_ask("x", "DELETE FROM a"), reply);
    drop(queue);
    assert!(no_answer(&answers).contains("closed"));
}

#[gpui::test]
fn a_repeated_id_and_too_many_waiting_questions_are_refused_not_queued(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let _first = ask_the_app(&app, visual, approval_ask("same", "DELETE FROM a"));
    let again = ask_the_app(&app, visual, approval_ask("same", "DELETE FROM a"));
    let reply = again.try_recv().expect("refused at once");
    assert!(!reply.ok && reply.error.unwrap().contains("already open"));
    let mut waiting = Vec::new();
    for n in 0..super::approval_dialog::MAX_WAITING {
        waiting.push(ask_the_app(&app, visual, approval_ask(&format!("w{n}"), "DELETE FROM a")));
    }
    let extra = ask_the_app(&app, visual, approval_ask("extra", "DELETE FROM a"));
    let reply = extra.try_recv().expect("refused at once");
    assert!(!reply.ok && reply.error.unwrap() == super::approval_dialog::TOO_MANY);
    assert_eq!(approval_ids(&app, visual).1.len(), super::approval_dialog::MAX_WAITING);
}

#[test]
fn what_the_client_sent_is_shown_with_its_odd_characters_spelled_out() {
    assert_eq!(super::approval_dialog::plain("DROP\u{202E} TABLE t\u{200B}", true), "DROP<U+202E> TABLE t<U+200B>");
    assert_eq!(super::approval_dialog::plain("a\nb", false), "a<U+000A>b");
    assert_eq!(super::approval_dialog::plain("a\nb", true), "a\nb");
}

#[cfg(unix)]
#[gpui::test]
fn through_the_real_socket_a_click_on_approve_answers_the_client_with_the_apps_proof(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let dir = tempfile::Builder::new().prefix("tx").permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700)).tempdir().unwrap();
    let path = dir.path().join("app.sock");
    app.update(visual, |this, cx| {
        this.listen_on_app_channel(&path, cx).expect("the socket binds");
    });
    let client = AppClient::new(path.clone());
    let ask = |id: &str| {
        let (client, ask) = (client.clone(), approval_ask(id, "DELETE FROM items WHERE id = 1"));
        std::thread::spawn(move || client.ask_approval(&ask, &|| false))
    };
    let pump = |visual: &mut gpui::VisualTestContext, until: &dyn Fn(&mut gpui::VisualTestContext) -> bool| {
        for _ in 0..2000 {
            settle(visual);
            if until(visual) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the condition never held");
    };
    let thread = ask("0123456789abcdef0123456789abcdef");
    pump(visual, &|visual| approval_open(visual));
    wait_until_armed(visual);
    click_debug_button(visual, "approval-approve");
    pump(visual, &|_| thread.is_finished());
    assert!(thread.join().unwrap().unwrap());
    wait_approval_closed(visual);
    // Declined with Escape, and the client hears no.
    let thread = ask("fedcba9876543210fedcba9876543210");
    pump(visual, &|visual| approval_open(visual));
    wait_until_armed(visual);
    visual.simulate_keystrokes("escape");
    pump(visual, &|_| thread.is_finished());
    assert!(!thread.join().unwrap().unwrap());
    // A client that gives up takes its dialog off the screen.
    let gave_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread = {
        let (client, flag) = (client.clone(), gave_up.clone());
        std::thread::spawn(move || {
            client.ask_approval(&approval_ask("00112233445566778899aabbccddeeff", "DELETE FROM items WHERE id = 2"), &|| {
                flag.load(std::sync::atomic::Ordering::SeqCst)
            })
        })
    };
    pump(visual, &|visual| approval_open(visual));
    gave_up.store(true, std::sync::atomic::Ordering::SeqCst);
    pump(visual, &|_| thread.is_finished());
    assert!(matches!(thread.join().unwrap(), Err(ClientError::Cancelled)));
    pump(visual, &|visual| !approval_open(visual));
    assert_eq!(approval_ids(&app, visual), (None, vec![]));
    app.update(visual, |this, _| this.app_channel.stop());
}
