//! Asking the user before the assistant changes anything.
//!
//! The tool layer calls [`AppApprover`] on a run's worker thread when the model proposes a write.
//! The approver turns the request into a [`Question`], hands it to the UI through the run's own
//! event queue (so it arrives in order with the tool step that asked) and waits for the answer. The
//! wait ends in exactly three ways: the user answers (the card's buttons), the call's
//! [`CancelToken`] fires (Stop, a new chat, the access lowered, the app closing), or every UI-side
//! handle of the question is dropped (the chat is gone). Nothing else ends it: there is no timeout
//! and no answer is remembered, so the only way a statement runs is a click on its own card.
//!
//! The UI thread never blocks here: answering is a mutex and a condvar notify.

use super::run::Shared;
use engine_agent::sanitize::{is_right_to_left, spell_out};
use engine_agent::{ApprovalDecision, CancelToken, WriteApproval, WriteApprover};
use gpui::SharedString;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How long after a card appears "Run" and "Run anyway" ignore clicks: a double-click on the
/// button of one card must not land on the card of the next write that takes its place.
pub const APPROVAL_DELAY: Duration = Duration::from_millis(700);

/// What became of a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Approved,
    Declined,
    /// Nobody answered: the run was stopped, the chat ended or the app is closing.
    Stopped,
}

/// The worker's side of a question: the first answer wins.
struct Pending {
    answer: Mutex<Option<Answer>>,
    ready: Condvar,
    /// The card has been on screen long enough for an approval to count.
    armed: AtomicBool,
}

impl Pending {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(None),
            ready: Condvar::new(),
            armed: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<Answer>> {
        self.answer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records `answer` unless the question already has one; returns whether it was this one.
    fn settle(&self, answer: Answer) -> bool {
        let mut slot = self.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(answer);
        self.ready.notify_all();
        true
    }

    fn wait(&self) -> Answer {
        let mut slot = self.lock();
        loop {
            if let Some(answer) = *slot {
                return answer;
            }
            slot = self
                .ready
                .wait(slot)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// The UI's side of a question. Dropping the last handle stops the question, so a worker never
/// waits for a card nobody can see any more.
pub struct Reply(Arc<Pending>);

impl Reply {
    /// Approves, if the card is armed (see [`APPROVAL_DELAY`]) and nothing answered it first.
    pub fn approve(&self) -> bool {
        self.0.armed.load(Ordering::Acquire) && self.0.settle(Answer::Approved)
    }

    /// Declining is never delayed.
    pub fn decline(&self) -> bool {
        self.0.settle(Answer::Declined)
    }

    pub fn stop(&self) {
        self.0.settle(Answer::Stopped);
    }

    pub fn arm(&self) {
        self.0.armed.store(true, Ordering::Release);
    }

    /// The delay starts over (the card is on screen again, or in front of the user again).
    pub fn disarm(&self) {
        self.0.armed.store(false, Ordering::Release);
    }

    pub fn is_armed(&self) -> bool {
        self.0.armed.load(Ordering::Acquire)
    }

    pub fn answer(&self) -> Option<Answer> {
        *self.0.lock()
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.0.settle(Answer::Stopped);
    }
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Reply").field(&self.answer()).finish()
    }
}

impl PartialEq for Reply {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A write waiting for the user: what the card shows, and the way to answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// The connection's name, as drawn (characters without a shape spelled out).
    pub connection: SharedString,
    /// The name of its connection type, as drawn; "No type" when it has none.
    pub connection_type: SharedString,
    pub production: bool,
    /// The database the statement runs in, when the layer names one, as drawn.
    pub database: Option<SharedString>,
    /// The statement exactly as the layer will run it.
    pub statement: SharedString,
    /// The statement as the card draws it: the same text, but a character that has no shape of its
    /// own (a control, a direction change, a zero-width character, a bare carriage return) is
    /// spelled `<U+XXXX>`, so nothing in what the user reads hides or reorders anything.
    pub shown: SharedString,
    /// How many characters `shown` spelled out.
    pub spelled: usize,
    /// The lines of the statement (at least one).
    pub lines: usize,
    /// The statement has letters of a right-to-left script, which reorder the quotes and comment
    /// marks next to them without any control character.
    pub right_to_left: bool,
    /// The layer's reason when the statement is dangerous.
    pub dangerous: Option<SharedString>,
    reply: Arc<Reply>,
}

impl Question {
    fn new(request: &WriteApproval<'_>, reply: Arc<Reply>) -> Self {
        let (shown, spelled) = spell_out(request.statement, true);
        let label = |text: &str| -> SharedString { spell_out(text, false).0.into() };
        let connection_type = if request.connection_type.trim().is_empty() {
            SharedString::from("No type")
        } else {
            label(request.connection_type)
        };
        Self {
            connection: label(request.connection_name),
            connection_type,
            production: request.production,
            database: request.database.map(label),
            statement: request.statement.to_owned().into(),
            shown: shown.into(),
            spelled,
            lines: request.statement.split('\n').count(),
            right_to_left: request.statement.chars().any(is_right_to_left),
            dangerous: request.dangerous.map(|reason| reason.to_owned().into()),
            reply,
        }
    }

    pub fn answer(&self) -> Option<Answer> {
        self.reply.answer()
    }

    pub fn is_waiting(&self) -> bool {
        self.answer().is_none()
    }

    pub fn approve(&self) -> bool {
        self.reply.approve()
    }

    pub fn decline(&self) -> bool {
        self.reply.decline()
    }

    pub fn stop(&self) {
        self.reply.stop();
    }

    pub fn arm(&self) {
        self.reply.arm();
    }

    pub fn disarm(&self) {
        self.reply.disarm();
    }

    pub fn is_armed(&self) -> bool {
        self.reply.is_armed()
    }

    /// What the step's row says once the layer has answered the model: "ran", the layer's own error
    /// when the approved statement failed, "not run: declined" / "not run: stopped" otherwise.
    pub fn outcome(&self, is_error: bool, summary: &str) -> String {
        match self.answer() {
            Some(Answer::Approved) if is_error => summary.to_owned(),
            Some(Answer::Approved) => "ran".to_owned(),
            Some(Answer::Declined) => "not run: declined".to_owned(),
            Some(Answer::Stopped) | None => "not run: stopped".to_owned(),
        }
    }
}

/// The run on this thread: the queue its questions go to, and the connection the chat is about.
struct RunContext {
    shared: Arc<Shared>,
    scope: Option<String>,
}

// The run on this thread, if any.
thread_local! {
    static RUN: RefCell<Option<RunContext>> = const { RefCell::new(None) };
}

/// Marks the current thread as the worker of a run until dropped. The layer calls the approver on
/// the thread that called it (`WriteApprover` says so), which is this one.
pub(super) struct RunScope(());

impl RunScope {
    pub(super) fn enter(shared: Arc<Shared>, scope: Option<String>) -> Self {
        RUN.with(|slot| *slot.borrow_mut() = Some(RunContext { shared, scope }));
        Self(())
    }
}

impl Drop for RunScope {
    fn drop(&mut self) {
        RUN.with(|slot| *slot.borrow_mut() = None);
    }
}

/// The app's [`WriteApprover`]: asks the user, in the chat. It asks about the chat's own
/// connection only: a card for another one would sit under a status line that speaks of this one.
pub struct AppApprover;

impl AppApprover {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    /// A person said no.
    fn deny(reason: &str) -> ApprovalDecision {
        ApprovalDecision::Deny {
            reason: Some(reason.to_owned()),
        }
    }

    /// Nobody was asked.
    fn refuse(reason: &str) -> ApprovalDecision {
        ApprovalDecision::Refuse {
            reason: reason.to_owned(),
        }
    }

    /// What the layer is told once the wait is over. A click that raced a cancel does not run
    /// the statement of a call that is over.
    fn decision(answer: Answer, cancelled: bool) -> ApprovalDecision {
        match answer {
            Answer::Approved if !cancelled => ApprovalDecision::Approve,
            Answer::Approved | Answer::Stopped => Self::deny("cancelled"),
            Answer::Declined => Self::deny("the user declined"),
        }
    }
}

impl WriteApprover for AppApprover {
    fn approve(&self, request: &WriteApproval<'_>, cancel: &CancelToken) -> ApprovalDecision {
        let context = RUN.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|run| (Arc::clone(&run.shared), run.scope.clone()))
        });
        let Some((run, scope)) = context else {
            // Not called from a run: nobody can be asked.
            return Self::refuse("there was nobody to ask");
        };
        if request.connection_id.is_empty() || request.connection_name.trim().is_empty() {
            return Self::refuse("the app cannot name this connection, so it cannot ask about it");
        }
        if !scope
            .as_deref()
            .is_some_and(|scope| scope.eq_ignore_ascii_case(request.connection_id))
        {
            return Self::refuse(
                "this chat is about another connection; ask the user to open a chat for it",
            );
        }
        let pending = Pending::new();
        // Registered before the question is shown: a cancel that lands first settles it at once
        // (a token that is cancelled already runs the hook right here).
        let hook = cancel.on_cancel({
            let pending = Arc::clone(&pending);
            move || {
                pending.settle(Answer::Stopped);
            }
        });
        if cancel.is_cancelled() {
            return Self::deny("cancelled");
        }
        run.ask(Question::new(
            request,
            Arc::new(Reply(Arc::clone(&pending))),
        ));
        let answer = pending.wait();
        // The wait is over: the hook has nothing left to do, and a cancel must not start a thread
        // to run it.
        drop(hook);
        if answer == Answer::Stopped {
            // Whoever stopped the question (a Stop whose cancel is still on its way to this
            // call's token, a chat that is gone) ended the call: the layer answers "cancelled",
            // not "the user declined".
            cancel.cancel_detached();
        }
        Self::decision(answer, cancel.is_cancelled())
    }

    /// The user is asked, with the warning.
    fn allows_dangerous(&self) -> bool {
        true
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::app::assistant::model::Update;
    use crate::app::assistant::run::{Item, Shared};
    use engine_agent::Language;
    use std::time::Instant;

    /// The request the tests ask about: connection `c1` named "shop", a development type.
    pub(in crate::app::assistant) fn request(statement: &str) -> WriteApproval<'_> {
        WriteApproval {
            connection_id: "c1",
            connection_name: "shop",
            connection_type: "Development",
            production: false,
            language: Language::Sql,
            database: Some("main"),
            statement,
            dangerous: None,
            transaction: None,
        }
    }

    /// A question nobody asks a worker about, to put in a transcript.
    pub(in crate::app::assistant) fn question(statement: &str) -> Question {
        Question::new(&request(statement), Arc::new(Reply(Pending::new())))
    }

    fn approver() -> Arc<AppApprover> {
        AppApprover::new()
    }

    /// What the worker has handed the UI so far.
    fn asked(shared: &Shared) -> Vec<Question> {
        shared
            .take()
            .into_iter()
            .filter_map(|item| match item {
                Item::Update(Update::Ask(question)) => Some(question),
                _ => None,
            })
            .collect()
    }

    /// Runs `approve` on a thread that is the worker of a run about connection `c1`; returns the
    /// thread, the queue and the wake channel.
    fn worker_for(
        cancel: &CancelToken,
        statement: &'static str,
        scope: Option<&'static str>,
    ) -> (
        std::thread::JoinHandle<ApprovalDecision>,
        Arc<Shared>,
        smol::channel::Receiver<()>,
    ) {
        let (shared, woken) = Shared::new();
        let (approver, cancel) = (approver(), cancel.clone());
        let handle = std::thread::spawn({
            let shared = Arc::clone(&shared);
            move || {
                let _scope = RunScope::enter(shared, scope.map(str::to_owned));
                approver.approve(&request(statement), &cancel)
            }
        });
        (handle, shared, woken)
    }

    fn worker(
        cancel: &CancelToken,
        statement: &'static str,
    ) -> (
        std::thread::JoinHandle<ApprovalDecision>,
        Arc<Shared>,
        smol::channel::Receiver<()>,
    ) {
        worker_for(cancel, statement, Some("c1"))
    }

    fn wait_for_question(shared: &Shared) -> Question {
        let start = Instant::now();
        loop {
            if let Some(question) = asked(shared).pop() {
                return question;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "no question was asked"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn joined(handle: std::thread::JoinHandle<ApprovalDecision>) -> ApprovalDecision {
        // The test fails rather than hangs if the wait never ends; bound it.
        let start = Instant::now();
        while !handle.is_finished() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the worker never came back"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        handle.join().unwrap()
    }

    #[test]
    fn names_are_one_line_without_hidden_characters_and_a_missing_type_is_said() {
        let mut ask = request("UPDATE t SET a = 1");
        ask.connection_name = "shop\n\u{202E}evil";
        ask.connection_type = "";
        ask.database = Some("db\u{200B}x");
        let question = Question::new(&ask, Arc::new(Reply(Pending::new())));
        assert_eq!(question.connection.as_ref(), "shop<U+000A><U+202E>evil");
        assert_eq!(question.connection_type.as_ref(), "No type");
        assert_eq!(question.database.as_deref(), Some("db<U+200B>x"));
    }

    #[test]
    fn the_length_and_right_to_left_letters_are_noted() {
        let question = question("UPDATE t SET a = 'x'\nWHERE id = 1");
        assert_eq!((question.lines, question.right_to_left), (2, false));
        let hebrew = self::question("UPDATE t SET a = '\u{5D0}\u{5D1}' -- \u{5D2}");
        assert!(hebrew.right_to_left);
        assert_eq!(question_lines("a\n\n\nb"), 4);
    }

    fn question_lines(statement: &str) -> usize {
        question(statement).lines
    }

    #[test]
    fn the_first_answer_wins_and_a_dropped_reply_stops_the_question() {
        let pending = Pending::new();
        let reply = Reply(Arc::clone(&pending));
        assert!(reply.decline());
        assert!(!reply.approve(), "a declined question cannot be approved");
        assert_eq!(pending.wait(), Answer::Declined);

        let pending = Pending::new();
        drop(Reply(Arc::clone(&pending)));
        assert_eq!(pending.wait(), Answer::Stopped);
    }

    #[test]
    fn an_approval_counts_only_once_the_card_is_armed_and_a_decline_always_does() {
        let question = question("UPDATE t SET a = 1");
        assert!(!question.is_armed());
        assert!(!question.approve(), "too early: the click is ignored");
        assert!(question.is_waiting(), "and it did not use up the question");
        question.arm();
        assert!(question.approve());

        let early = self::question("UPDATE t SET a = 1");
        assert!(early.decline(), "declining is never delayed");
    }

    #[test]
    fn a_click_that_raced_the_cancel_does_not_run_the_statement() {
        assert_eq!(
            AppApprover::decision(Answer::Approved, false),
            ApprovalDecision::Approve
        );
        // Approved first, cancelled before the worker woke: not approved.
        assert!(matches!(
            AppApprover::decision(Answer::Approved, true),
            ApprovalDecision::Deny { .. }
        ));
        assert!(matches!(
            AppApprover::decision(Answer::Stopped, false),
            ApprovalDecision::Deny { .. }
        ));
        assert_eq!(
            AppApprover::decision(Answer::Declined, false),
            ApprovalDecision::Deny {
                reason: Some("the user declined".into())
            }
        );
    }

    #[test]
    fn outside_a_run_nobody_can_be_asked_and_nothing_is_approved() {
        let decision = std::thread::spawn(|| {
            approver().approve(&request("DELETE FROM t"), &CancelToken::new())
        })
        .join()
        .unwrap();
        assert!(matches!(decision, ApprovalDecision::Refuse { .. }));
    }

    #[test]
    fn a_thread_that_is_not_the_runs_cannot_ask_while_the_run_waits() {
        let cancel = CancelToken::new();
        let (handle, shared, _woken) = worker(&cancel, "UPDATE t SET a = 1");
        let question = wait_for_question(&shared);
        // Another thread (a helper of the layer, a test) calls the approver during the run.
        let foreign = std::thread::spawn(|| {
            approver().approve(&request("DELETE FROM t"), &CancelToken::new())
        })
        .join()
        .unwrap();
        assert!(matches!(foreign, ApprovalDecision::Refuse { .. }));
        assert!(asked(&shared).is_empty(), "no second question appeared");
        assert!(question.is_waiting(), "and the first one is untouched");
        question.stop();
        assert!(matches!(joined(handle), ApprovalDecision::Deny { .. }));
    }

    #[test]
    fn the_app_allows_dangerous_statements_to_be_asked_about() {
        assert!(approver().allows_dangerous());
    }

    #[test]
    fn the_question_carries_what_the_card_shows_and_a_click_approves() {
        let cancel = CancelToken::new();
        let (handle, shared, woken) = worker(&cancel, "UPDATE t SET a = 1");
        // The queue is filled before the wake is sent: wait for the wake, then read.
        let start = Instant::now();
        while woken.try_recv().is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the UI thread was never woken"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let question = wait_for_question(&shared);
        assert_eq!(question.connection.as_ref(), "shop");
        assert_eq!(question.connection_type.as_ref(), "Development");
        assert_eq!(question.database.as_deref(), Some("main"));
        assert_eq!(question.statement.as_ref(), "UPDATE t SET a = 1");
        assert!(!question.production && question.dangerous.is_none());
        assert!(question.is_waiting());
        question.arm();
        assert!(question.approve());
        assert_eq!(joined(handle), ApprovalDecision::Approve);
    }

    #[test]
    fn a_connection_that_is_not_the_chats_is_declined_without_a_card() {
        let cancel = CancelToken::new();
        for scope in [Some("other"), None] {
            let (handle, shared, _woken) = worker_for(&cancel, "UPDATE t SET a = 1", scope);
            let ApprovalDecision::Refuse { reason } = joined(handle) else {
                panic!("approved")
            };
            assert!(reason.contains("another connection"), "{reason:?}");
            assert!(asked(&shared).is_empty());
        }
        // The id is compared the way the catalog spells it, upper or lower case.
        let (handle, shared, _woken) = worker_for(&cancel, "UPDATE t SET a = 1", Some("C1"));
        let question = wait_for_question(&shared);
        question.decline();
        assert!(matches!(joined(handle), ApprovalDecision::Deny { .. }));
    }

    #[test]
    fn a_connection_the_app_cannot_name_is_declined() {
        let cancel = CancelToken::new();
        let (shared, _woken) = Shared::new();
        let handle = std::thread::spawn({
            let cancel = cancel.clone();
            move || {
                let _scope = RunScope::enter(shared, Some("c1".into()));
                let mut ask = request("UPDATE t SET a = 1");
                ask.connection_name = "  ";
                approver().approve(&ask, &cancel)
            }
        });
        assert!(matches!(joined(handle), ApprovalDecision::Refuse { .. }));
    }

    #[test]
    fn a_decline_denies_with_a_fixed_reason() {
        let cancel = CancelToken::new();
        let (handle, shared, _woken) = worker(&cancel, "UPDATE t SET a = 1");
        let question = wait_for_question(&shared);
        assert!(question.decline());
        assert!(!question.approve(), "the first answer stands");
        assert_eq!(
            joined(handle),
            ApprovalDecision::Deny {
                reason: Some("the user declined".into())
            }
        );
    }

    #[test]
    fn the_calls_token_ends_the_wait_as_not_approved() {
        let cancel = CancelToken::new();
        let (handle, shared, _woken) = worker(&cancel, "UPDATE t SET a = 1");
        let question = wait_for_question(&shared);
        cancel.cancel_detached();
        assert!(matches!(joined(handle), ApprovalDecision::Deny { .. }));
        assert_eq!(question.answer(), Some(Answer::Stopped));
        question.arm();
        assert!(!question.approve(), "too late to approve");
    }

    #[test]
    fn a_token_that_is_cancelled_already_asks_nothing() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let (handle, shared, _woken) = worker(&cancel, "UPDATE t SET a = 1");
        assert!(matches!(joined(handle), ApprovalDecision::Deny { .. }));
        assert!(asked(&shared).is_empty());
    }

    #[test]
    fn dropping_every_handle_of_the_question_ends_the_wait() {
        let cancel = CancelToken::new();
        let (handle, shared, _woken) = worker(&cancel, "UPDATE t SET a = 1");
        let question = wait_for_question(&shared);
        drop(question);
        drop(shared);
        assert!(matches!(joined(handle), ApprovalDecision::Deny { .. }));
    }
}
