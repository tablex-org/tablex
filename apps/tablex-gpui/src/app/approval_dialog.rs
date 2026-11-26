//! The question of an MCP client that cannot ask the user itself (it does not support MCP
//! elicitation), put to the user in a modal dialog of the app.
//!
//! It arrives on the app channel as `Request::Approval` (see `engine_connect::app_channel`, which
//! has already authenticated the request and signs the answer). This is a trust boundary: a
//! deliberate click on Approve is the only thing that ever answers yes, and nothing here answers it
//! for the user: no timeout, no default, no remembered choice.
//!
//! - One dialog at a time; the others wait in a queue and are shown in order, each after the one
//!   before it has been answered or withdrawn.
//! - **Approve cannot be hit by accident.** It stays disabled until the dialog has been *quiet* for
//!   [`ARM_DELAY`]: no input at all. Every key-down while a question is on screen (typing, Tab, an
//!   Enter, a repeat of a held key: all of them) and every mouse-down anywhere in the window (the
//!   body, the footer, the backdrop, a button that is not armed: a window-wide capture-phase
//!   listener) starts the wait again, and so do the window being activated and the keyboard landing
//!   on Approve; mouse moves do not count. The only key and click that do not are the press and the
//!   click that go on to approve (below), which are judged by the quiet before them. A click that is
//!   the window's first-focus click is ignored. Approve also stays disabled until the end of the
//!   statement has been scrolled into view (the box takes the keyboard: Page Down, End, the arrows),
//!   and is never enabled while the box is cut off by the bottom of the window (a window too small
//!   to show it: Decline is what is left).
//! - **Which keys can approve.** A dangerous statement and a commit (`click_only`, or a `danger`
//!   line) are approved by a mouse click on an armed Approve and by nothing else: the keyboard does
//!   nothing on its Approve button, and a hint under the buttons says "Click Approve to run it".
//!   Any other question can also be approved with **Space** on an armed, focused Approve. **Enter
//!   never approves**: on Approve it does nothing.
//! - **Any other key moves the focus back to Decline.** While Approve has the focus, every key but
//!   Tab, Shift-Tab, a bare modifier, Escape and a Cmd or Ctrl command (copy, zoom), apart from that
//!   approving Space, is swallowed and hands the focus to Decline (so Tab, typing, a pause and an
//!   Enter ends on Decline, not on Approve). Space and Enter on Decline are swallowed too unless the
//!   dialog was quiet before them; Enter on Decline declines after a quiet moment. **Escape** declines
//!   only after a quiet moment too: typed in a hurry (a vim user leaving insert mode) it is ignored
//!   and not counted as a decline. A commit is click-only from its kind (its title) as well as from the
//!   signed flag, and so is the removal of a connection (`click_only`).
//! - **The window is not forced forward.** A question asks the system for attention (the dock icon
//!   bounces) instead of activating tablex over what the user is doing: stealing the focus is how a
//!   keystroke meant for another program would reach this dialog. While a question is on screen or
//!   waiting, the channel's other requests (`open_object`, `show_query`) ask for attention too and
//!   do not activate the window either.
//! - Decline has the focus when the dialog opens. The component reports every dialog action with one
//!   and the same event, so Approve is a button of its own and not the dialog's confirm action.
//! - A question whose client went away is withdrawn: the dialog closes if it is the one the user is
//!   on, and otherwise turns into a notice with a Close button (the component can close only the
//!   top dialog, never one by identity, so another dialog is never closed by mistake). A dialog
//!   that disappears without an answer (closed by other code) is not delivered, and the next
//!   question is shown. The app quitting, or its root being dropped, gives every open question a
//!   "not delivered", never a decline the user did not give.
//! - Everything the client sent is plain text: shown as it is, with characters without a shape
//!   of their own spelled `<U+XXXX>`, never as markup, in a read-only monospace box that scrolls.

use super::*;
use engine_agent::sanitize::spell_out;
use engine_connect::app_channel::{ApprovalAsk, Reply};
use gpui::{Pixels, ScrollHandle, SharedString, Subscription, Task};
use gpui_component::dialog::{DialogClose, DialogFooter};
use gpui_component::ActiveTheme as _;
use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc::Sender;

/// Questions that may wait behind the one on screen; one more is told tablex is busy.
pub(super) const MAX_WAITING: usize = 8;
/// How long the dialog must have been quiet (not opened, not activated, Approve not just focused)
/// before Approve works.
pub(super) const ARM_DELAY: Duration = Duration::from_millis(700);
/// The tallest the statement box grows before it scrolls.
const STATEMENT_MAX_HEIGHT: f32 = 240.0;
/// What the app tells the client when it cannot take a question.
pub(super) const TOO_MANY: &str = "tablex has too many questions waiting";

/// A question and where its answer goes.
struct Question {
    ask: ApprovalAsk,
    reply: Sender<Reply>,
}

impl Question {
    /// Ends the question with no answer: the client is told it was not delivered (an error reply),
    /// which is never an approval and never counts as the user's decline.
    fn close(self, why: &str) {
        let _ = self.reply.send(Reply::error(why));
    }
}

/// What the dialog's builder and the app share: when Approve was last disturbed, whether the end
/// of the statement has been seen, whether the question was withdrawn, and the box's scroll.
struct Gate {
    /// The last input (opened, activated, Approve focused, a key, a click on the body).
    since: Cell<Instant>,
    /// What `since` was before the latest key: a key that acts (Enter) is judged by the quiet
    /// that came before it, not by itself.
    before_key: Cell<Instant>,
    end_seen: Cell<bool>,
    withdrawn: Cell<bool>,
    /// Where Approve was painted last (for the window-wide click rule).
    approve_bounds: Cell<Option<gpui::Bounds<Pixels>>>,
    scroll: ScrollHandle,
}

impl Gate {
    fn new() -> Rc<Self> {
        let now = Instant::now();
        Rc::new(Self {
            since: Cell::new(now),
            before_key: Cell::new(now),
            end_seen: Cell::new(false),
            withdrawn: Cell::new(false),
            approve_bounds: Cell::new(None),
            scroll: ScrollHandle::new(),
        })
    }

    /// Input that is not a key (the window activated, Approve focused, a click).
    fn restart(&self) {
        let now = Instant::now();
        self.since.set(now);
        self.before_key.set(now);
    }

    /// A key went down.
    fn key(&self) {
        self.before_key.set(self.since.get());
        self.since.set(Instant::now());
    }

    fn armed(&self) -> bool {
        self.since.get().elapsed() >= ARM_DELAY
    }

    /// Whether the dialog was quiet before the key being handled.
    fn armed_for_key(&self) -> bool {
        self.before_key.get().elapsed() >= ARM_DELAY
    }

    fn can_approve(&self) -> bool {
        self.armed() && self.end_seen.get() && !self.withdrawn.get()
    }

    /// Whether the box's last line is in view (or the statement needs no scrolling). Not before
    /// the box has been laid out, and not while the bottom of the box is cut off by the window.
    fn see_the_end(&self, window: &Window) {
        let bounds = self.scroll.bounds();
        let (max, offset) = (self.scroll.max_offset(), self.scroll.offset());
        let laid_out = bounds.size.height > px(0.0);
        let cut_off = bounds.bottom() > window.viewport_size().height - px(8.0);
        self.end_seen
            .set(laid_out && !cut_off && (max.y <= px(1.0) || -offset.y >= max.y - px(1.0)));
    }

    /// Scrolls the box by `by` (positive: down), within its range.
    fn scroll_by(&self, by: Pixels) {
        let (max, offset) = (self.scroll.max_offset(), self.scroll.offset());
        let y = (offset.y - by).clamp(-max.y, px(0.0));
        self.scroll.set_offset(gpui::point(offset.x, y));
    }

    /// A key of the box's keyboard scrolling: `true` when it was one.
    fn scroll_key(&self, key: &str) -> bool {
        let page = (self.scroll.bounds().size.height - px(24.0)).max(px(24.0));
        match key {
            "pagedown" | "space" => self.scroll_by(page),
            "pageup" => self.scroll_by(-page),
            "down" => self.scroll_by(px(24.0)),
            "up" => self.scroll_by(px(-24.0)),
            "home" => self.scroll_by(px(-1.0e6)),
            "end" => self.scroll_by(px(1.0e6)),
            _ => return false,
        }
        true
    }
}

/// What exists once the question's dialog has been opened.
struct Opened {
    gate: Rc<Gate>,
    decline_focus: gpui::FocusHandle,
    approve_focus: gpui::FocusHandle,
    /// The statement box takes the keyboard (Page Down, End, the arrows scroll it). The dialog's
    /// builder holds the handle too; this one is for the tests to move the focus there.
    #[cfg_attr(not(test), allow(dead_code))]
    box_focus: gpui::FocusHandle,
    _subscriptions: Vec<Subscription>,
    /// Repaints the dialog when the delay has run out (Approve becomes usable).
    arming: Option<Task<()>>,
}

/// The question on screen.
struct Shown {
    question: Question,
    opened: Option<Opened>,
}

/// The questions the app holds: the one on screen and the ones behind it.
#[derive(Default)]
pub(super) struct ApprovalQueue {
    waiting: VecDeque<Question>,
    shown: Option<Shown>,
}

impl ApprovalQueue {
    /// Whether a question is on screen or waiting.
    pub(super) fn busy(&self) -> bool {
        self.shown.is_some() || !self.waiting.is_empty()
    }

    fn knows(&self, id: &str) -> bool {
        self.shown.as_ref().is_some_and(|shown| shown.question.ask.id == id)
            || self.waiting.iter().any(|question| question.ask.id == id)
    }

    #[cfg(test)]
    fn opened(&self) -> Option<&Opened> {
        self.shown.as_ref().and_then(|shown| shown.opened.as_ref())
    }

    /// Ends every question without an answer, the one on screen first: not delivered.
    pub(super) fn close_all(&mut self) {
        if let Some(shown) = self.shown.take() {
            shown.question.close(CLOSING);
        }
        for question in self.waiting.drain(..) {
            question.close(CLOSING);
        }
    }

    /// Whether Decline of the question on screen has the focus.
    #[cfg(test)]
    pub(super) fn decline_is_focused(&self, window: &Window) -> bool {
        self.opened().is_some_and(|opened| opened.decline_focus.is_focused(window))
    }

    /// Approve's focus handle, to move the focus there as Tab would.
    #[cfg(test)]
    pub(super) fn approve_focus(&self) -> Option<gpui::FocusHandle> {
        self.opened().map(|opened| opened.approve_focus.clone())
    }

    /// The statement box's focus handle.
    #[cfg(test)]
    pub(super) fn box_focus(&self) -> Option<gpui::FocusHandle> {
        self.opened().map(|opened| opened.box_focus.clone())
    }

    /// Whether Approve of the question on screen works right now.
    #[cfg(test)]
    pub(super) fn can_approve(&self) -> bool {
        self.opened().is_some_and(|opened| opened.gate.can_approve())
    }

    /// The statement box's scroll handle.
    #[cfg(test)]
    pub(super) fn scroll(&self) -> Option<ScrollHandle> {
        self.opened().map(|opened| opened.gate.scroll.clone())
    }

    #[cfg(test)]
    pub(super) fn push_waiting_for_test(&mut self, ask: ApprovalAsk, reply: Sender<Reply>) {
        self.waiting.push_back(Question { ask, reply });
    }

    /// The id of the question on screen, and of the ones waiting, in order.
    #[cfg(test)]
    pub(super) fn ids(&self) -> (Option<String>, Vec<String>) {
        (
            self.shown.as_ref().map(|shown| shown.question.ask.id.clone()),
            self.waiting.iter().map(|question| question.ask.id.clone()).collect(),
        )
    }
}

/// What a question that the app ends without an answer is told.
const CLOSING: &str = "tablex closed before the question was answered";

/// A root that goes away (a closed window, the app exiting) ends what is still open.
impl Drop for ApprovalQueue {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// Held by the dialog's builder for as long as the dialog exists: when the dialog goes away without
/// having been answered (other code closed it), the question is told so and the next one is shown.
struct DialogGuard {
    id: String,
    root: gpui::WeakEntity<AppRoot>,
    app: gpui::AsyncApp,
}

impl Drop for DialogGuard {
    fn drop(&mut self) {
        let (root, id) = (self.root.clone(), self.id.clone());
        // Dropped inside a window update: the root is updated afterwards.
        self.app
            .spawn(async move |cx| {
                let _ = root.update(cx, |this, cx| this.approval_dialog_gone(&id, cx));
            })
            .detach();
    }
}

/// `text` as shown: characters without a shape of their own spelled out, a name on one line.
pub(super) fn plain(text: &str, multiline: bool) -> String {
    spell_out(text, multiline).0
}

/// The body of the dialog: who asks, on what, how dangerous, and what runs. `mono` is the
/// monospace font of the statement box, `scroll` its scroll handle.
fn approval_body(
    ask: &ApprovalAsk,
    mono: SharedString,
    gate: &Rc<Gate>,
    box_focus: &gpui::FocusHandle,
) -> Div {
    let line = |selector: &'static str, text: String| {
        div().debug_selector(move || selector.into()).child(text)
    };
    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .debug_selector(|| "approval-intro".into())
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(t!("approval.intro").into_owned()),
        )
        .child(
            div()
                .debug_selector(|| "approval-ask".into())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(plain(&ask.title, false)),
        );
    if let Some(client) = &ask.client {
        body = body.child(line(
            "approval-client",
            t!("approval.client", name = plain(client, false)).into_owned(),
        ));
    }
    if let Some(connection) = &ask.connection {
        body = body.child(line(
            "approval-connection",
            t!("approval.connection", name = plain(connection, false)).into_owned(),
        ));
    }
    if let Some(kind) = &ask.connection_type {
        body = body.child(line(
            "approval-type",
            t!("approval.connection_type", name = plain(kind, false)).into_owned(),
        ));
    }
    if let Some(database) = &ask.database {
        body = body.child(line(
            "approval-database",
            t!("approval.database", name = plain(database, false)).into_owned(),
        ));
    }
    if let Some(collection) = &ask.collection {
        body = body.child(line(
            "approval-collection",
            t!("approval.collection", name = plain(collection, false)).into_owned(),
        ));
    }
    if let Some(danger) = &ask.danger {
        body = body.child(
            div()
                .debug_selector(|| "approval-danger".into())
                .text_color(theme::danger())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(t!("approval.danger", reason = plain(danger, false)).into_owned()),
        );
    }
    for (n, note) in ask.notes.iter().enumerate() {
        body = body.child(
            div()
                .id(("approval-note", n))
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .child(plain(note, true)),
        );
    }
    let shown: Vec<String> = ask.statements.iter().map(|statement| plain(statement, true)).collect();
    // The size is the app's own count of what it shows, as the elicitation states it.
    let lines: usize = shown.iter().map(|text| text.split('\n').count()).sum();
    let characters: usize = shown.iter().map(|text| text.chars().count()).sum();
    body = body.child(
        div()
            .debug_selector(|| "approval-size".into())
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .child(t!("approval.size", lines = lines, characters = characters).into_owned()),
    );
    let total = shown.len();
    body.child(
        div()
            .id("approval-statement")
            .debug_selector(|| "approval-statement".into())
            .max_h(px(STATEMENT_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&gate.scroll)
            .track_focus(box_focus)
            .on_key_down({
                let gate = gate.clone();
                move |event, _, cx| {
                    if gate.scroll_key(event.keystroke.key.as_str()) {
                        cx.stop_propagation();
                    }
                }
            })
            .p(px(8.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface())
            .font_family(mono)
            .text_size(px(12.0))
            .text_color(theme::text())
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(shown.into_iter().enumerate().map(|(n, statement)| {
                div()
                    .flex()
                    .flex_col()
                    // Every change of a commit says which it is, in the app's own words, outside
                    // the text of the statement.
                    .children((total > 1).then(|| {
                        div()
                            .id(("approval-change", n))
                            .debug_selector(move || format!("approval-change-{n}"))
                            .text_size(px(11.0))
                            .text_color(theme::text_muted())
                            .child(t!("approval.change", k = n + 1, n = total).into_owned())
                    }))
                    .child(statement)
            })),
    )
}

impl AppRoot {
    /// Takes a question from the app channel: shown at once when none is on screen, else queued.
    /// `reply` gets the answer (see [`Reply::approval`]) or an error when the question cannot be
    /// taken or is ended without one.
    pub(super) fn enqueue_approval(&mut self, ask: ApprovalAsk, reply: Sender<Reply>, cx: &mut Context<Self>) {
        let refuse = |text: &str| {
            let _ = reply.send(Reply::error(text));
        };
        if self.driver_runtime.window.is_none() {
            return refuse("tablex has no window to ask in");
        }
        if self.approvals.knows(&ask.id) {
            return refuse("this question is already open");
        }
        if self.approvals.waiting.len() >= MAX_WAITING {
            return refuse(TOO_MANY);
        }
        self.approvals.waiting.push_back(Question { ask, reply });
        self.show_next_approval(cx);
    }

    /// Puts the next waiting question on screen, unless one is.
    fn show_next_approval(&mut self, cx: &mut Context<Self>) {
        if self.approvals.shown.is_some() {
            return;
        }
        let Some(question) = self.approvals.waiting.pop_front() else {
            return;
        };
        let Some(window) = self.driver_runtime.window else {
            question.close("tablex has no window to ask in");
            return;
        };
        let id = question.ask.id.clone();
        self.approvals.shown = Some(Shown { question, opened: None });
        let root = cx.entity().downgrade();
        // The window cannot be updated while this entity is: open the dialog once the current
        // update has finished.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| this.open_approval_dialog(&id, window, cx));
            });
        });
    }

    fn open_approval_dialog(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Withdrawn, or answered, since it was queued: nothing to open.
        let Some(shown) = self.approvals.shown.as_mut().filter(|shown| shown.question.ask.id == id) else {
            return;
        };
        let ask = shown.question.ask.clone();
        let gate = Gate::new();
        let (decline_focus, approve_focus) = (cx.focus_handle(), cx.focus_handle());
        let box_focus = cx.focus_handle().tab_stop(true);
        let root = cx.entity().downgrade();
        // A dangerous statement and a commit are approved by a click on Approve only.
        // (Also from the kind of question: a commit is click-only whatever the flag says.)
        let click_only =
            ask.click_only || ask.danger.is_some() || ask.title == engine_connect::app_channel::COMMIT_TITLE;
        // Approve is disturbed, and its delay starts over, whenever the window comes to the front,
        // whenever the keyboard lands on it, and at every key that goes down while the question is
        // on screen (before the key does anything), except the Space or Enter that presses a
        // focused Approve: that one is judged by the quiet before it.
        let subscriptions = vec![
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.restart_approval_arming(false, cx);
                }
            }),
            cx.on_focus_in(&approve_focus, window, |this, _, cx| this.restart_approval_arming(false, cx)),
            cx.intercept_keystrokes({
                let (root, approve_focus, decline_focus, gate) =
                    (root.clone(), approve_focus.clone(), decline_focus.clone(), gate.clone());
                move |event, window, cx| {
                    let key = event.keystroke.key.as_str();
                    let on_approve = approve_focus.is_focused(window);
                    // Cmd or Ctrl with a key (copy, zoom): a command, not typing.
                    let chord = event.keystroke.modifiers.platform || event.keystroke.modifiers.control;
                    // The one press that goes on to approve: Space on an armed, focused Approve
                    // of a question that is not click-only. It is judged by the quiet before it
                    // and does not restart it; every other key does, before it does anything
                    // (so repeated presses, and a held key, only ever push the quiet away).
                    if on_approve && key == "space" && !chord && !click_only && gate.can_approve() {
                        return;
                    }
                    let quiet_before = gate.armed();
                    let _ = root.update(cx, |this, cx| this.restart_approval_arming(true, cx));
                    // Escape declines, but only after a quiet moment: typed in a hurry (a vim user
                    // leaving insert mode) it is ignored, and so it is not counted as a decline.
                    if key == "escape" {
                        if !quiet_before {
                            cx.stop_propagation();
                        }
                        return;
                    }
                    // Moving on purpose (Tab, Shift-Tab) and bare modifiers are let through, and so
                    // are commands on Approve (Cmd-C, Cmd-+): none of them acts on the dialog.
                    if matches!(key, "tab" | "shift" | "control" | "alt" | "platform" | "function" | "capslock")
                        || (on_approve && chord && !matches!(key, "space" | "enter"))
                    {
                        return;
                    }
                    if on_approve {
                        // Any other key with the focus on Approve (Enter never approves; Space may
                        // not yet, or at all) hands the focus to Decline and does nothing else.
                        decline_focus.focus(window, cx);
                        cx.stop_propagation();
                    } else if decline_focus.is_focused(window) && matches!(key, "space" | "enter") && !quiet_before {
                        // Space or Enter on Decline soon after other input: not a decision.
                        cx.stop_propagation();
                    }
                }
            }),
        ];
        shown.opened = Some(Opened {
            gate: gate.clone(),
            decline_focus: decline_focus.clone(),
            approve_focus: approve_focus.clone(),
            box_focus: box_focus.clone(),
            _subscriptions: subscriptions,
            arming: None,
        });
        self.restart_approval_arming(false, cx);
        // Ask for attention (a dock bounce); the user brings the window forward when they choose.
        window.request_attention();
        let guard = Rc::new(DialogGuard { id: ask.id.clone(), root: root.clone(), app: cx.to_async() });
        let focus = decline_focus.clone();
        window.open_alert_dialog(cx, move |dialog, window, cx| {
            // Alive for as long as the dialog is.
            let _alive = &guard;
            if gate.withdrawn.get() {
                // The client went away while another dialog had the top: nothing to answer.
                return dialog
                    .width(px(640.0))
                    .title(t!("approval.title").into_owned())
                    .child(div().debug_selector(|| "approval-withdrawn".into()).child(t!("approval.withdrawn").into_owned()))
                    .footer(DialogFooter::new().child(
                        div().debug_selector(|| "approval-close".into()).child(
                            DialogClose::new().trigger(|button| button.label(t!("approval.close").into_owned())),
                        ),
                    ));
            }
            gate.see_the_end(window);
            if gate.scroll.bounds().size.height <= px(0.0) {
                // Not laid out yet: look again at the next frame.
                window.on_next_frame(|window, _| window.refresh());
            }
            let can_approve = gate.can_approve();
            let (approve_root, decline_root, enter_root, click_root) =
                (root.clone(), root.clone(), root.clone(), root.clone());
            let click_gate = gate.clone();
            let (enter_gate, body_gate, body_focus) = (gate.clone(), gate.clone(), box_focus.clone());
            let (approve_id, decline_id, enter_id) = (ask.id.clone(), ask.id.clone(), ask.id.clone());
            let mono = cx.theme().mono_font_family.clone();
            let decline_focus = decline_focus.clone();
            let (approve_track, enter_focus) = (approve_focus.clone(), approve_focus.clone());
            let danger = ask.danger.is_some();
            dialog
                .width(px(640.0))
                .title(t!("approval.title").into_owned())
                .child(approval_body(&ask, mono, &body_gate, &body_focus))
                .confirm()
                .footer(
                    div().flex().flex_col().gap(px(6.0)).child(
                    DialogFooter::new()
                        .child(
                            div().debug_selector(|| "approval-decline".into()).child(
                                DialogClose::new().trigger(move |button| {
                                    button.label(t!("approval.decline").into_owned()).track_focus(&decline_focus)
                                }),
                            ),
                        )
                        .child(div().relative().debug_selector(|| "approval-approve".into()).child(
                            // Paints nothing: records where Approve is and, for each frame it is on
                            // screen, watches every mouse-down in the window (capture phase). A click
                            // anywhere (the body, the footer, the backdrop, a button that is not
                            // armed) is input and starts the quiet period again; only a click on an
                            // armed Approve does not.
                            gpui::canvas(
                                |bounds, _, _| bounds,
                                move |_, bounds, window, _| {
                                    click_gate.approve_bounds.set(Some(bounds));
                                    let (gate, root) = (click_gate.clone(), click_root.clone());
                                    window.on_mouse_event(move |event: &gpui::MouseDownEvent, phase, _, cx| {
                                        if phase != gpui::DispatchPhase::Capture {
                                            return;
                                        }
                                        let on_armed_approve = gate.can_approve()
                                            && gate.approve_bounds.get().is_some_and(|bounds| bounds.contains(&event.position));
                                        if !on_armed_approve {
                                            let _ = root.update(cx, |this, cx| this.restart_approval_arming(false, cx));
                                        }
                                    });
                                },
                            )
                            .absolute()
                            .size_full(),
                        ).child({
                            let button = Button::new("approval-approve")
                                .label(t!("approval.approve").into_owned())
                                .track_focus(&approve_track)
                                // Only while it works: a disarmed Approve looks and acts disabled.
                                .disabled(!can_approve)
                                // Only this button approves. Not the dialog's confirm action: Enter
                                // anywhere else in the dialog reaches that, and it is a decline.
                                .on_click(move |event, window, cx| {
                                    // The click that brought a background window forward (macOS
                                    // `acceptsFirstMouse`) was not aimed at this button, and a
                                    // click-only question is not approved from the keyboard.
                                    if event.first_focus() || (click_only && event.is_keyboard()) {
                                        return;
                                    }
                                    let answered = approve_root
                                        .update(cx, |this, cx| this.approve_clicked(&approve_id, cx))
                                        .unwrap_or(false);
                                    if answered {
                                        use gpui_component::WindowExt as _;
                                        window.close_dialog(cx);
                                    }
                                });
                            // The theme's own buttons: armed, a dangerous statement's Approve is the
                            // danger button (red text on a red tint) and anything else the accent;
                            // disarmed, either is the component's faded disabled look.
                            if danger { button.danger() } else { button.primary() }
                        })),
                    )
                    // Said under the buttons when the keyboard cannot approve.
                    .children(click_only.then(|| {
                        div()
                            .debug_selector(|| "approval-click-hint".into())
                            .flex()
                            .justify_end()
                            .text_size(px(11.0))
                            .text_color(theme::text_muted())
                            .child(t!("approval.click_hint").into_owned())
                    })),
                )
                // Enter reaches the dialog's confirm action whatever has the focus. On Approve it
                // does nothing (Enter never approves); anywhere else, Decline included, it declines,
                // after a quiet moment.
                .on_ok(move |_, window, cx| {
                    // Enter never approves: on Approve it does nothing.
                    if enter_focus.is_focused(window) {
                        return false;
                    }
                    // Typed in a hurry (the dialog had not been quiet): ignored, not a decline.
                    if !enter_gate.armed_for_key() {
                        return false;
                    }
                    let _ = enter_root.update(cx, |this, cx| this.answer_approval(&enter_id, false, cx));
                    true
                })
                // Decline, Escape: the same.
                .on_cancel(move |_, _, cx| {
                    let _ = decline_root.update(cx, |this, cx| this.answer_approval(&decline_id, false, cx));
                    true
                })
        });
        focus.focus(window, cx);
        cx.notify();
    }

    /// The dialog was disturbed (opened, activated, Approve focused, a key, a click): Approve waits
    /// again, and the window is repainted when the wait is over. `key`: it was a key going down.
    fn restart_approval_arming(&mut self, key: bool, cx: &mut Context<Self>) {
        let Some(opened) = self.approvals.shown.as_mut().and_then(|shown| shown.opened.as_mut()) else {
            return;
        };
        if key {
            opened.gate.key();
        } else {
            opened.gate.restart();
        }
        let window = self.driver_runtime.window;
        opened.arming = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ARM_DELAY + Duration::from_millis(30)).await;
            let _ = this.update(cx, |_, cx| {
                if let Some(window) = window {
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |_, window, _| window.refresh());
                    });
                }
            });
        }));
    }

    /// A click on Approve (or Enter with it focused). `true` when it approved: the dialog has been
    /// quiet long enough, the end of the statement has been seen, and the question is still the one
    /// on screen.
    fn approve_clicked(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let allowed = self
            .approvals
            .shown
            .as_ref()
            .filter(|shown| shown.question.ask.id == id)
            .and_then(|shown| shown.opened.as_ref())
            .is_some_and(|opened| opened.gate.can_approve());
        if allowed {
            self.answer_approval(id, true, cx);
        }
        allowed
    }

    /// The user's answer to the question on screen (`id`): sent once, and the next one shown.
    pub(super) fn answer_approval(&mut self, id: &str, approve: bool, cx: &mut Context<Self>) {
        if self.approvals.shown.as_ref().is_none_or(|shown| shown.question.ask.id != id) {
            return;
        }
        if let Some(shown) = self.approvals.shown.take() {
            let _ = shown.question.reply.send(Reply::approval(approve));
        }
        self.show_next_approval(cx);
    }

    /// The dialog of question `id` went away without the user answering (other code closed it).
    /// The client is told it was not delivered, and the next question is shown.
    fn approval_dialog_gone(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.approvals.shown.as_ref().is_none_or(|shown| shown.question.ask.id != id) {
            return;
        }
        if let Some(shown) = self.approvals.shown.take() {
            shown.question.close("the question was closed without an answer");
        }
        self.show_next_approval(cx);
    }

    /// The client of question `id` is gone: it leaves the queue, or its dialog closes. Nothing is
    /// answered (nobody is listening).
    pub(super) fn withdraw_approval(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(at) = self.approvals.waiting.iter().position(|question| question.ask.id == id) {
            self.approvals.waiting.remove(at);
            return;
        }
        if self.approvals.shown.as_ref().is_none_or(|shown| shown.question.ask.id != id) {
            return;
        }
        let shown = self.approvals.shown.take().expect("checked above");
        if let (Some(opened), Some(window)) = (shown.opened.as_ref(), self.driver_runtime.window) {
            let (gate, decline, approve) = (opened.gate.clone(), opened.decline_focus.clone(), opened.approve_focus.clone());
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| {
                    // The component closes the top dialog, whichever it is. Ours is the top one when
                    // the keyboard is on one of its buttons (every other dialog takes the focus when
                    // it opens); otherwise it turns into a notice the user closes themselves.
                    if decline.is_focused(window) || approve.is_focused(window) {
                        use gpui_component::WindowExt as _;
                        window.close_dialog(cx);
                    } else {
                        gate.withdrawn.set(true);
                        window.refresh();
                    }
                });
            });
        }
        drop(shown);
        self.show_next_approval(cx);
    }

    /// The app is quitting: every open question ends without an answer (not delivered).
    pub(super) fn decline_approvals(&mut self) {
        self.approvals.close_all();
    }
}
