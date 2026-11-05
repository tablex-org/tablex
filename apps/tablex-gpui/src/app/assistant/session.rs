//! One chat: its scope, the engine's conversation and the display model a panel renders.
//!
//! A view only reads this entity (`items`, `is_running`, `status`, ...) and calls its methods
//! (`send`, `stop`, `retry`); it never sees a thread, a token or a lock. A run lives on a worker
//! thread ([`super::run`]); its events arrive here batched, at most once per
//! [`MIN_INTERVAL`](super::run::MIN_INTERVAL), and each batch is one notification.
//!
//! The conversation goes to the worker with the run and comes back with its end, so at most one
//! run exists per chat. Each finished run says whether its user message is still in the
//! conversation (the engine takes it back when the first turn does not complete); that, not the
//! tail of the conversation, decides [`ChatSession::retry`]: send the message again, or resume
//! from the tool results the conversation ends with.

use super::approval::{Question, APPROVAL_DELAY};
use super::model::{ChatError, ChatItem, ChatNotice, Transcript, Update};
use super::run::{self, Finished, Item, KeepPlan, RunKind, RunSetup, Saved, Shared, MIN_INTERVAL};
use super::store::{self, ConversationStore, Header, Loaded};
use engine_agent::{AgentAccess, CancelToken};
use engine_ai::{Conversation, Scope, Usage};
use gpui::{Context, EventEmitter, SharedString};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Why a chat can or cannot send right now; the panel shows the matching state instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// The chat has no connection (the active tab had none when it started).
    NoConnection,
    /// The connection's agent access is `off`: the first-use disclosure sets it.
    AccessOff,
    /// The selected provider needs a key and none is saved.
    NoApiKey,
    /// The selected provider has no model chosen (OpenAI, a compatible server and Ollama start
    /// without one).
    NoModel,
    /// The credential store is still being asked.
    CheckingKey,
    /// The credential store could not be read.
    KeyUnavailable(String),
    /// The connection's access was lowered while this chat had history: it cannot go on, the
    /// history may hold more than the new level allows.
    AccessLowered,
    /// A conversation opened from History that ran at a higher level than the connection has
    /// now: it holds data that level no longer allows to send, so it is shown for reading only.
    ReadOnly,
}

/// What the status line and the empty states of the panel need besides the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatStatus {
    pub readiness: Readiness,
    /// The connection's effective agent access (type preset, own setting, caps applied).
    pub access: AgentAccess,
}

impl Default for ChatStatus {
    /// Not ready: a chat is usable only once the app has said what its connection, key and
    /// access are.
    fn default() -> Self {
        Self {
            readiness: Readiness::CheckingKey,
            access: AgentAccess::Off,
        }
    }
}

/// What Retry does after a run that did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RetryPlan {
    /// The engine took the message back: send it again.
    Resend(String),
    /// The message is in the conversation (with the tool results behind it): continue.
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Started,
    /// An answer is still running (one run at a time per chat).
    Busy,
    /// The message is empty.
    Empty,
    NotReady(Readiness),
    /// The assistant could not be set up (the audit log, say); an error item says why.
    SetupFailed,
}

/// What the app learns from a chat besides "redraw".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// A run failed; the app files it in its error log.
    Failed(ChatError),
    /// A write started or stopped waiting for the user's answer (the toolbar shows it, also with
    /// the dock closed).
    Asking(bool),
    /// A conversation file was written: the History list has changed.
    Saved,
    /// A conversation could not be kept (the app files it in its error log).
    NotKept(String),
}

impl EventEmitter<ChatEvent> for ChatSession {}

/// The run in flight. Dropping it (a new chat, the chat closing) cancels the run without waiting.
struct ActiveRun {
    cancel: CancelToken,
    stopping: bool,
    /// The message this run sends (`None`: a resume).
    sends: Option<String>,
    /// Where the conversation goes when the run ends (`None`: not kept).
    store: Option<Arc<ConversationStore>>,
    _task: gpui::Task<()>,
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        // Never `cancel()`: its hooks run the drivers' blocking cancels.
        self.cancel.cancel_detached();
    }
}

pub struct ChatSession {
    scope: Option<Scope>,
    /// The connection's name, for the header.
    scope_label: String,
    /// `None` while a run owns it.
    conversation: Option<Conversation>,
    transcript: Transcript,
    /// What Retry does; set when a run starts and corrected when it ends.
    retry_plan: Option<RetryPlan>,
    run: Option<ActiveRun>,
    status: ChatStatus,
    /// The connection's access was lowered under a chat with history.
    lowered: bool,
    /// What `ChatEvent::Asking` last said: a write waits for the user (kept, so a render does
    /// not have to scan the transcript for it).
    asking: bool,
    /// The one-shot timer that arms the waiting question (see [`APPROVAL_DELAY`]).
    arm_task: Option<gpui::Task<()>>,
    /// What identifies this chat's file, once it has a first message.
    record: Option<Record>,
    /// The chat was opened from History: it ran before, at `record.access`.
    opened: bool,
    /// The "too large to keep" notice was shown for this chat.
    too_large_said: bool,
    /// Counts the times the whole transcript was replaced (a new chat, a conversation opened from
    /// History): a view that cached rows of the old one starts over when this changes.
    epoch: u64,
    /// "Keep conversations on this computer" holds for this chat right now (the setting is on,
    /// there is a place to keep them, the connection exists). Decided when a run ENDS: turning it
    /// off under a run in flight keeps that run's conversation off disk.
    keeping: bool,
    /// The chat's scope was set on purpose (by an action, by opening a stored conversation): the
    /// active tab no longer pulls it elsewhere while it is empty. A plain new chat clears it.
    pinned: bool,
    /// The conversation does not fit a file any more: later runs do not try.
    too_large: bool,
    /// The conversation is on disk (or on its way): what History shows of it is this chat.
    on_disk: bool,
    /// A failed write was said (once per chat).
    write_failed_said: bool,
    /// The most messages any write of this chat carried.
    sent_seq: u64,
    /// The highest level the run in flight has seen the connection at (the file says so).
    ran_at: Arc<AtomicU8>,
    /// Writes handed to a background task (tests).
    #[cfg(test)]
    writes_started: usize,
    /// What the last run made of its file: "bytes", "too_large", "failed" or "skipped" (tests).
    #[cfg(test)]
    last_saved: &'static str,
    /// Writes that have ended, however they ended (tests).
    #[cfg(test)]
    writes_ended: usize,
}

/// A conversation read back and its transcript, both made off the UI thread.
pub struct Opened {
    pub loaded: Loaded,
    pub transcript: Transcript,
}

impl Opened {
    pub fn new(loaded: Loaded) -> Self {
        let transcript = Transcript::restore(&loaded.conversation);
        Self { loaded, transcript }
    }
}

/// What a chat knows of the file it is kept in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    id: SharedString,
    created: u64,
    title: String,
    /// The highest level the chat has run at.
    access: AgentAccess,
}

impl ChatSession {
    pub fn new(scope: Option<Scope>, scope_label: impl Into<String>) -> Self {
        Self {
            conversation: Some(Conversation::new(scope.clone())),
            scope,
            scope_label: scope_label.into(),
            transcript: Transcript::default(),
            retry_plan: None,
            run: None,
            status: ChatStatus::default(),
            lowered: false,
            asking: false,
            arm_task: None,
            record: None,
            opened: false,
            too_large_said: false,
            epoch: 0,
            keeping: true,
            pinned: false,
            too_large: false,
            on_disk: false,
            write_failed_said: false,
            sent_seq: 0,
            ran_at: Arc::new(AtomicU8::new(0)),
            #[cfg(test)]
            writes_started: 0,
            #[cfg(test)]
            last_saved: "",
            #[cfg(test)]
            writes_ended: 0,
        }
    }

    // ---- what a view renders ------------------------------------------------------------

    /// The chat in order: messages, answers, reasoning, tool steps, notices, errors.
    pub fn items(&self) -> &[ChatItem] {
        self.transcript.items()
    }

    /// An answer is being produced (also while a stop is being carried out).
    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    pub fn is_stopping(&self) -> bool {
        self.run.as_ref().is_some_and(|run| run.stopping)
    }

    /// A write waits for the user's answer.
    pub fn is_asking(&self) -> bool {
        self.asking
    }

    /// The write that waits, and the item of the step that asked.
    pub(super) fn waiting_at(&self) -> Option<(usize, Arc<Question>)> {
        self.transcript.waiting_at()
    }

    /// The write that waits for the user's answer.
    pub fn waiting(&self) -> Option<Arc<Question>> {
        self.transcript.waiting_question()
    }

    /// The tokens of the last model turn.
    pub fn usage(&self) -> Option<Usage> {
        self.transcript.usage()
    }

    /// Tables of the schema summary the assistant works from: `(included, total)`.
    pub fn context_tables(&self) -> Option<(usize, usize)> {
        self.transcript.context_tables()
    }

    pub fn status(&self) -> &ChatStatus {
        &self.status
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }

    /// The connection's name when the chat started.
    pub fn scope_label(&self) -> &str {
        &self.scope_label
    }

    /// The item whose row offers Retry: the chat ended with a failure or a stop that sending
    /// again can fix, there is something to send or resume, and the chat can send right now (a
    /// chat whose access was lowered or whose key is gone cannot, so it offers nothing that
    /// would do nothing).
    pub fn retry_item(&self) -> Option<usize> {
        let possible = self.run.is_none()
            && self.retry_plan.is_some()
            && self.status.readiness == Readiness::Ready;
        possible.then(|| self.transcript.retryable_item()).flatten()
    }

    pub fn can_retry(&self) -> bool {
        self.retry_item().is_some()
    }

    /// Nothing was said yet.
    pub fn is_empty(&self) -> bool {
        self.transcript.items().is_empty()
    }

    /// See the `epoch` field: changes when the transcript is replaced as a whole.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The id of the file this chat is kept in (once it has a first message, or when it was
    /// opened from History).
    pub fn conversation_id(&self) -> Option<&str> {
        self.record.as_ref().map(|record| &*record.id)
    }

    /// [`Self::conversation_id`] without a copy of the text.
    pub fn conversation_id_shared(&self) -> Option<SharedString> {
        self.record.as_ref().map(|record| record.id.clone())
    }

    /// The scope is chosen on purpose from now on (see the `pinned` field).
    pub(super) fn pin(&mut self) {
        self.pinned = true;
    }

    /// The reason for pinning an empty chat is gone (the message that was held for it was
    /// discarded): the active tab pulls it again. A chat with messages keeps its pin.
    pub(super) fn unpin_if_empty(&mut self) -> bool {
        let unpin = self.pinned && self.is_empty();
        if unpin {
            self.pinned = false;
        }
        unpin
    }

    /// The scope was set on purpose (see the `pinned` field).
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// The chat holds messages that are on no disk (and are not going to be): replacing the chat
    /// would lose them. A chat that was saved once and went on with the setting off, past the size
    /// limit or after a refused write holds some too: what was written covers `sent_seq` messages.
    pub fn has_unsaved_messages(&self) -> bool {
        match &self.conversation {
            // A run owns it: whatever it holds is not settled.
            None => true,
            Some(conversation) => {
                let held = conversation.messages().len() as u64;
                held > 0 && (!self.on_disk || held > self.sent_seq)
            }
        }
    }

    /// The conversation is on disk or being written (what History shows of this chat).
    pub fn is_kept(&self) -> bool {
        self.on_disk
    }

    /// Whether the app wants this chat's conversation kept; see the `keeping` field.
    pub fn set_keeping(&mut self, keeping: bool) {
        self.keeping = keeping;
    }

    /// Writes handed to a background task so far (tests).
    #[cfg(test)]
    pub fn writes_started(&self) -> usize {
        self.writes_started
    }

    /// Writes that have ended (tests).
    #[cfg(test)]
    pub fn writes_ended(&self) -> usize {
        self.writes_ended
    }

    /// What the last run made of its file (tests).
    #[cfg(test)]
    pub fn last_saved(&self) -> &'static str {
        self.last_saved
    }

    /// The conversation as the engine holds it (tests); `None` while a run owns it.
    #[cfg(test)]
    pub fn conversation_for_test(&self) -> Option<&Conversation> {
        self.conversation.as_ref()
    }

    /// The chat stops being kept under the id it has (the file it was in was deleted): it has a
    /// fresh id. Nothing is written until the user sends another message in it: a run in flight
    /// carries the old id and is refused at the end, and writes happen only at the end of a run.
    /// Then the whole conversation is written as a new one. Says so in the transcript.
    /// Returns whether there was anything to detach.
    pub(super) fn detach(&mut self, why: &str, cx: &mut Context<Self>) -> bool {
        let Some(record) = self.record.as_mut() else {
            return false;
        };
        record.id = uuid::Uuid::new_v4().to_string().into();
        record.created = store::now_ms();
        self.on_disk = false;
        self.sent_seq = 0;
        self.transcript
            .push(ChatItem::Notice(ChatNotice::Other(why.to_owned())));
        cx.notify();
        true
    }

    /// The chat was opened from History and the connection's level is below the one it ran at.
    #[cfg(test)]
    pub fn is_read_only(&self) -> bool {
        self.status.readiness == Readiness::ReadOnly
    }

    // ---- what a view calls ----------------------------------------------------------------

    /// Sends `text` and starts the answer. One run at a time: while one is going this does
    /// nothing and says `Busy`.
    pub fn send(&mut self, text: String, setup: RunSetup, cx: &mut Context<Self>) -> SendOutcome {
        if self.run.is_some() {
            return SendOutcome::Busy;
        }
        if text.trim().is_empty() {
            return SendOutcome::Empty;
        }
        if self.status.readiness != Readiness::Ready {
            return SendOutcome::NotReady(self.status.readiness.clone());
        }
        if self.record.is_none() {
            // The first message of the conversation names it (after a detach that is not this one).
            let first = self
                .transcript
                .items()
                .iter()
                .find_map(|item| match item {
                    ChatItem::User(first) => Some(first.as_str()),
                    _ => None,
                })
                .unwrap_or(&text);
            self.record = Some(Record {
                id: uuid::Uuid::new_v4().to_string().into(),
                created: store::now_ms(),
                title: store::title_of(first),
                access: self.status.access,
            });
        }
        self.transcript.push(ChatItem::User(text.clone()));
        self.start(RunKind::Send(text), setup, cx);
        SendOutcome::Started
    }

    /// Stops the answer: the run is cancelled without waiting for the network or a statement
    /// (`running` stays true until the worker has handed the conversation back, a moment later).
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.run.as_mut().filter(|run| !run.stopping) {
            run.stopping = true;
            run.cancel.cancel_detached();
            // At once, not when the cancel reaches the question's hook: a click that lands in
            // between cannot approve a write of a run that is being stopped.
            self.transcript.stop_questions();
            self.sync_asking(cx);
            cx.notify();
        }
    }

    /// Answers `question`, the one the card was drawn for: `true` runs it, `false` declines.
    /// Returns whether it was the question that waits and the answer counted (a stop that landed
    /// first has answered it already, an approval before the card is armed is ignored, and an
    /// answer for any other question, such as the card that was there a moment ago, is ignored).
    pub fn answer_question(
        &mut self,
        question: &Arc<Question>,
        approve: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let waiting = self
            .transcript
            .waiting_question()
            .is_some_and(|waiting| Arc::ptr_eq(&waiting, question));
        let answered = waiting
            && if approve {
                question.approve()
            } else {
                question.decline()
            };
        self.sync_asking(cx);
        cx.notify();
        answered
    }

    /// The card is in front of the user again (first painted, the dock opened onto it, the window
    /// came back): the delay starts over, so no click aimed at something else counts.
    pub fn restart_arming(&mut self, cx: &mut Context<Self>) {
        if let Some(question) = self.transcript.waiting_question() {
            question.disarm();
            self.arm_waiting_question(cx);
            cx.notify();
        }
    }

    /// Starts the clock of the question that just arrived: it takes no approval until
    /// [`APPROVAL_DELAY`] has passed, and the card is redrawn when it does. One timer, no polling.
    fn arm_waiting_question(&mut self, cx: &mut Context<Self>) {
        let Some(question) = self.transcript.waiting_question() else {
            return;
        };
        self.arm_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(APPROVAL_DELAY).await;
            question.arm();
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    /// Tells the app when the chat starts or stops waiting for an answer.
    fn sync_asking(&mut self, cx: &mut Context<Self>) {
        let asking = self.transcript.is_asking();
        if !asking {
            self.arm_task = None;
        }
        if asking != self.asking {
            self.asking = asking;
            cx.emit(ChatEvent::Asking(asking));
        }
    }

    /// Runs again after an error or a stop. Returns whether it started.
    pub fn retry(&mut self, setup: RunSetup, cx: &mut Context<Self>) -> bool {
        if !self.can_retry() {
            return false;
        }
        let kind = match self.retry_plan.clone() {
            Some(RetryPlan::Resend(text)) => RunKind::Send(text),
            Some(RetryPlan::Resume) => RunKind::Resume,
            None => return false,
        };
        self.transcript.rewind_for_retry();
        if matches!(kind, RunKind::Send(_)) {
            self.transcript.mark_last_user_sent();
        }
        self.start(kind, setup, cx);
        true
    }

    /// Drops the conversation and starts over on `scope` (the active tab's connection and
    /// namespace). A run in flight is cancelled without waiting.
    pub fn new_chat(
        &mut self,
        scope: Option<Scope>,
        label: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.run = None;
        self.conversation = Some(Conversation::new(scope.clone()));
        self.scope = scope;
        self.scope_label = label.into();
        // The questions of the old transcript end with it (and its run was cancelled above).
        self.transcript.stop_questions();
        self.transcript.clear();
        self.retry_plan = None;
        self.lowered = false;
        self.record = None;
        self.opened = false;
        self.too_large_said = false;
        self.too_large = false;
        self.on_disk = false;
        self.write_failed_said = false;
        self.sent_seq = 0;
        self.pinned = false;
        self.ran_at = Arc::new(AtomicU8::new(0));
        self.epoch += 1;
        self.sync_asking(cx);
        cx.notify();
    }

    /// [`Self::new_chat`] for a scope chosen on purpose (an action, not the active tab): the
    /// active tab does not pull the empty chat elsewhere.
    pub fn new_chat_pinned(
        &mut self,
        scope: Option<Scope>,
        label: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.new_chat(scope, label, cx);
        self.pinned = true;
    }

    /// Replaces the chat by a conversation read back from History: its display is rebuilt from
    /// the conversation alone (nothing runs, no model is asked), and the next message goes on the
    /// same conversation. Whether it can be continued is decided by the status the app sets next
    /// ([`Self::set_status`]): a level below the one it ran at makes it read-only.
    pub(super) fn open_stored(
        &mut self,
        opened: Opened,
        label: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let Opened { loaded, transcript } = opened;
        let Loaded {
            header,
            conversation,
        } = loaded;
        self.run = None;
        self.transcript.stop_questions();
        self.transcript = transcript;
        self.scope = conversation.scope().cloned();
        self.scope_label = label.into();
        self.conversation = Some(conversation);
        self.ran_at = Arc::new(AtomicU8::new(store::level_to_u8(header.access)));
        self.record = Some(Record {
            id: header.id.into(),
            created: header.created,
            title: header.title,
            access: header.access,
        });
        self.opened = true;
        self.pinned = true;
        self.on_disk = true;
        self.too_large = false;
        self.too_large_said = false;
        self.write_failed_said = false;
        self.sent_seq = header.seq;
        self.epoch += 1;
        self.retry_plan = None;
        self.lowered = false;
        // The status of the chat that was here says nothing about this one.
        self.status = ChatStatus::default();
        self.sync_asking(cx);
        cx.notify();
    }

    /// Puts a line in the transcript (a notice the app wants the user to see).
    pub(super) fn add_item(&mut self, item: ChatItem, cx: &mut Context<Self>) {
        self.transcript.push(item);
        cx.notify();
    }

    /// Whether the user message at item `ix` never reached the model (the engine took it back).
    pub fn is_unsent(&self, ix: usize) -> bool {
        self.transcript.is_unsent(ix)
    }

    /// The access of the connection was lowered under a chat that has history.
    #[cfg(test)]
    pub fn is_ended(&self) -> bool {
        self.lowered
    }

    /// The app's view of the chat changed (key, access, connection).
    ///
    /// A chat with history whose connection's access goes down is over: the history may hold
    /// results the lower level would not have shown (and still gets sent every turn), and a run in
    /// flight is stopped. Only a new chat starts clean.
    pub fn set_status(&mut self, mut status: ChatStatus, cx: &mut Context<Self>) {
        // The level the conversation has run at: its data is of that level. Lowering below it ends
        // the chat; a drop to a level at or above it does not (the data is still allowed). A
        // chat that has not run yet has none: it is compared with the level last seen.
        let ran_at = self
            .record
            .as_ref()
            .map(|record| record.access)
            .map(|access| access.max(store::level_from_u8(self.ran_at.load(Ordering::SeqCst))));
        // Lowering only ends a chat that could still send: one that was read-only is not "lowered"
        // by a further drop.
        let baseline = ran_at.unwrap_or(self.status.access);
        let could_send = self.status.access >= baseline;
        if !self.lowered
            && could_send
            && status.access < baseline
            && !self.transcript.items().is_empty()
        {
            self.lowered = true;
            if let Some(run) = self.run.as_mut() {
                run.stopping = true;
                run.cancel.cancel_detached();
            }
            self.transcript.stop_questions();
            self.transcript
                .push(ChatItem::Notice(ChatNotice::AccessLowered));
            self.sync_asking(cx);
            cx.notify();
        }
        if self.lowered {
            status.readiness = Readiness::AccessLowered;
        } else if self.opened && ran_at.is_some_and(|ran_at| status.access < ran_at) {
            status.readiness = Readiness::ReadOnly;
        }
        if self.run.is_some() {
            // A level raised under a run in flight is a level its conversation may hold.
            self.ran_at
                .fetch_max(store::level_to_u8(status.access), Ordering::SeqCst);
        }
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    /// The app is closing: cancel without waiting for anything.
    pub fn cancel_for_quit(&mut self) {
        if let Some(run) = &self.run {
            run.cancel.cancel_detached();
        }
        self.transcript.stop_questions();
    }

    // ---- the run ----------------------------------------------------------------------------

    fn start(&mut self, kind: RunKind, setup: RunSetup, cx: &mut Context<Self>) {
        let sends = match &kind {
            RunKind::Send(text) => Some(text.clone()),
            RunKind::Resume => None,
        };
        // Until the run says what became of its message, a retry sends it again (a resume keeps
        // resuming).
        if let Some(text) = &sends {
            self.retry_plan = Some(RetryPlan::Resend(text.clone()));
        }
        let conversation = self
            .conversation
            .take()
            .unwrap_or_else(|| Conversation::new(self.scope.clone()));
        // The conversation goes into the thread through a slot, so a thread that cannot start
        // gives it back.
        let slot = Arc::new(Mutex::new(Some(conversation)));
        let cancel = CancelToken::new();
        let scope = self.scope.as_ref().map(|scope| scope.connection_id.clone());
        // The level the conversation has now run at (a retry or a resume too); the run raises it
        // further if the connection's level rises while it goes.
        if let Some(record) = self.record.as_mut() {
            record.access = record.access.max(self.status.access);
            self.ran_at
                .store(store::level_to_u8(record.access), Ordering::SeqCst);
        }
        // What the file of this run is made from (its `updated` and level are set when it is made).
        let keep = if self.keeping && !self.too_large {
            setup
                .persist
                .as_ref()
                .zip(self.record.as_ref())
                .map(|(store, record)| KeepPlan {
                    header: Header {
                        format: store::FORMAT,
                        id: record.id.to_string(),
                        title: record.title.clone(),
                        created: record.created,
                        updated: record.created,
                        provider: setup.provider_id.to_owned(),
                        model: setup.model.clone(),
                        access: record.access,
                        seq: 0,
                    },
                    epoch: store.epoch(scope.as_deref().unwrap_or_default()),
                    access: Arc::clone(&self.ran_at),
                })
        } else {
            None
        };
        let persist = setup.persist.clone();
        let (shared, woken) = Shared::new();
        let spawned = std::thread::Builder::new()
            .name("assistant-run".into())
            .spawn({
                let (slot, cancel, shared) =
                    (Arc::clone(&slot), cancel.clone(), Arc::clone(&shared));
                move || {
                    let taken = slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    if let Some(conversation) = taken {
                        run::work(conversation, kind, setup, cancel, shared, scope, keep);
                    }
                }
            });
        if let Err(error) = spawned {
            if sends.is_some() {
                self.transcript.mark_last_user_unsent();
            }
            self.conversation = slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            self.fail(
                ChatError::internal(format!("The assistant could not start: {error}.")),
                cx,
            );
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            while woken.recv().await.is_ok() {
                let items = shared.take();
                let Ok(finished) = this.update(cx, |session, cx| session.apply(items, cx)) else {
                    return;
                };
                if finished {
                    return;
                }
                // The next look is no sooner than this: a streaming answer notifies the view at
                // most once per interval, however many deltas arrive.
                cx.background_executor().timer(MIN_INTERVAL).await;
            }
        });
        self.run = Some(ActiveRun {
            cancel,
            stopping: false,
            sends,
            store: persist,
            _task: task,
        });
        cx.notify();
    }

    /// A batch from the worker; true when it held the end of the run.
    pub(super) fn apply(&mut self, items: Vec<Item>, cx: &mut Context<Self>) -> bool {
        let mut finished = false;
        for item in items {
            match item {
                // A question that arrives once the run is being stopped (or the access was
                // lowered) is declined at once: the stop only covered the ones already shown.
                Item::Update(Update::Ask(question)) if self.is_stopping() || self.lowered => {
                    question.stop()
                }
                Item::Update(Update::Ask(question)) => {
                    self.transcript.apply(Update::Ask(question));
                    self.arm_waiting_question(cx);
                }
                Item::Update(update) => self.transcript.apply(update),
                Item::Finished(done) => {
                    finished = true;
                    self.finish(*done, cx);
                }
            }
        }
        self.sync_asking(cx);
        cx.notify();
        finished
    }

    fn finish(&mut self, done: Finished, cx: &mut Context<Self>) {
        self.transcript.settle_steps();
        self.transcript.note_rows_changed(&done.conversation);
        self.conversation = Some(done.conversation);
        // The run says whether its message is in the conversation; Retry follows that.
        let sent = self.run.as_ref().and_then(|run| run.sends.clone());
        if let Some(text) = sent {
            if done.input_kept {
                self.retry_plan = Some(RetryPlan::Resume);
            } else {
                self.retry_plan = Some(RetryPlan::Resend(text));
                self.transcript.mark_last_user_unsent();
            }
        }
        // A resume continues from the user's message the conversation ends with: after a stop
        // that landed once the last turn was complete there is nothing to continue from, and
        // Retry would only fail.
        if self.retry_plan == Some(RetryPlan::Resume)
            && !self
                .conversation
                .as_ref()
                .is_some_and(Conversation::ends_with_user)
        {
            self.retry_plan = None;
        }
        // The level the run saw the connection at is the level the conversation has run at.
        if let Some(record) = self.record.as_mut() {
            let seen = store::level_from_u8(self.ran_at.load(Ordering::SeqCst));
            record.access = record.access.max(seen);
        }
        // After this the run (and the task running this very call) is dropped; it has returned.
        let store = self.run.take().and_then(|run| run.store.clone());
        self.keep(done.saved, store, cx);
        if let Err(error) = done.result {
            self.fail(error, cx);
        }
    }

    /// What the run made of the conversation's file. Decided HERE, at the end: nothing is kept
    /// when the setting went off while the run was going, or when the chat was detached from the
    /// file the run was going to write. The write is handed to a background task (the store
    /// refuses one that is older than what is on disk, or that began before a delete); a
    /// conversation over the limit is not written, and the chat says so once.
    fn keep(
        &mut self,
        saved: Saved,
        store: Option<Arc<ConversationStore>>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        {
            self.last_saved = match &saved {
                Saved::Skipped => "skipped",
                Saved::Bytes(_) => "bytes",
                Saved::TooLarge => "too_large",
                Saved::Failed(_) => "failed",
            };
        }
        if !self.keeping {
            return;
        }
        match (saved, store, self.scope.as_ref()) {
            (Saved::Bytes(encoded), Some(store), Some(scope)) => {
                if self.record.as_ref().map(|record| &*record.id) != Some(encoded.id.as_str()) {
                    return;
                }
                let connection = scope.connection_id.clone();
                let seq = encoded.seq;
                self.sent_seq = self.sent_seq.max(seq);
                self.on_disk = true;
                #[cfg(test)]
                {
                    self.writes_started += 1;
                }
                cx.spawn(async move |this, cx| {
                    let id = encoded.id.clone();
                    let written = cx
                        .background_executor()
                        .spawn(async move { store.write(&connection, &encoded) })
                        .await;
                    let _ = this.update(cx, |session, cx| session.written(&id, seq, written, cx));
                })
                .detach();
            }
            (Saved::TooLarge, ..) => {
                self.too_large = true;
                if !self.too_large_said {
                    self.too_large_said = true;
                    self.transcript
                        .push(ChatItem::Notice(ChatNotice::TooLargeToKeep));
                }
            }
            (Saved::Failed(why), ..) => self.not_kept(&why, cx),
            _ => {}
        }
    }

    /// The end of a write.
    pub(super) fn written(
        &mut self,
        id: &str,
        seq: u64,
        written: Result<store::Written, store::StoreError>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        {
            self.writes_ended += 1;
        }
        match written {
            Ok(_) => cx.emit(ChatEvent::Saved),
            // The chat or its file was deleted meanwhile: nothing to say, but a chat that still is
            // this conversation is not on disk (the next run writes it again).
            Err(store::StoreError::Detached) => {
                if self.record.as_ref().map(|record| &*record.id) == Some(id) {
                    self.on_disk = false;
                }
            }
            // A newer write of this chat is on its way: this one raced it and lost, as it should.
            Err(store::StoreError::Stale) if seq < self.sent_seq => {}
            Err(error) => {
                // The chat may have been replaced since the write began: then it is not this
                // chat's to say (its once-flag and its transcript are the new chat's); only the
                // Error Log hears of it.
                if self.record.as_ref().map(|record| &*record.id) == Some(id) {
                    self.on_disk = false;
                    self.not_kept(&error.to_string(), cx);
                } else {
                    cx.emit(ChatEvent::NotKept(error.to_string()));
                }
            }
        }
    }

    /// A conversation could not be written: the chat says so once, the Error Log with it.
    fn not_kept(&mut self, why: &str, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.write_failed_said, true) {
            return;
        }
        self.transcript
            .push(ChatItem::Notice(ChatNotice::Other(format!(
                "This conversation could not be kept on this computer ({why}). It goes on here."
            ))));
        cx.emit(ChatEvent::NotKept(why.to_owned()));
        cx.notify();
    }

    /// Shows `error` in the chat and tells the app (error log).
    pub(super) fn fail(&mut self, error: ChatError, cx: &mut Context<Self>) {
        self.transcript.push_error(error.clone());
        cx.emit(ChatEvent::Failed(error));
        cx.notify();
    }
}
