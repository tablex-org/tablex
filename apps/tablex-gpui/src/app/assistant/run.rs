//! One run of the assistant on a worker thread, and the queue that carries its events to the UI
//! thread.
//!
//! The engine blocks (network, statements), so a run owns a thread of its own (a GPUI background
//! task would run it on the shared pool, and the test scheduler cannot run a blocking job at all).
//! The thread never touches GPUI: it pushes [`Update`]s into a [`Shared`] queue. Consecutive text
//! deltas are merged *in the queue*, so a thousand deltas between two looks of the UI thread are
//! one `String`, and the queue wakes the UI thread (one message on a one-slot channel) only when it
//! was empty. The session's task turns each wake into one drain and one notification, then waits
//! [`MIN_INTERVAL`] before it looks again, which is the bound on how often a streaming answer
//! notifies the view. No timer runs while nothing streams.

use super::approval::{Question, RunScope};
use super::layers::LayerSource;
use super::model::{summarize_args, ChatError, Update};
use super::store::{self, ConversationStore};
use engine_agent::CancelToken;
use engine_ai::{
    Assistant, AssistantConfig, AssistantError, AssistantEvent, ChatProvider, Conversation, Effort,
    Message, RunSummary, UserInput,
};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The least time between two looks of the UI thread at a streaming run.
pub const MIN_INTERVAL: Duration = Duration::from_millis(30);

/// What a run needs besides the conversation: the provider (with its key source and address), the
/// tool layer, and the choices of Settings at the moment the run starts.
#[derive(Clone)]
pub struct RunSetup {
    pub provider: Arc<dyn ChatProvider>,
    pub layer: LayerSource,
    pub model: String,
    /// The id of the provider that answers (`anthropic`, `openai`, ...), for the conversation file.
    pub provider_id: &'static str,
    pub effort: Effort,
    /// Where the conversation is kept after the run; `None`: it is not kept ("Keep conversations
    /// on this computer" is off).
    pub persist: Option<Arc<ConversationStore>>,
}

/// What the run is asked to do.
pub(super) enum RunKind {
    /// A new user message.
    Send(String),
    /// Continue from the tool results the conversation ends with.
    Resume,
}

/// How a run ended, with the conversation handed back (it is sendable in every case).
pub(super) struct Finished {
    pub conversation: Conversation,
    pub result: Result<RunSummary, ChatError>,
    /// The run's own user message is in the conversation: the engine takes it back when the first
    /// turn does not complete (error, stop, refusal, empty answer). Meaningless for a resume.
    pub input_kept: bool,
    /// What the run made of the file for its conversation.
    pub saved: Saved,
}

/// What a run needs to make the file of its conversation: the header as it was when the run began
/// (its time and level are set when the file is made), the store's epoch at that moment (a delete
/// since then refuses the write) and the highest level the connection was seen at while it went.
pub(super) struct KeepPlan {
    pub header: store::Header,
    pub epoch: store::Epoch,
    pub access: Arc<std::sync::atomic::AtomicU8>,
}

/// The conversation as it goes to disk, made on the run's thread (no copy of it on the UI thread;
/// the file's I/O is the UI's to hand to a background task).
pub(super) enum Saved {
    /// Not kept: the switch is off, or the conversation is empty.
    Skipped,
    Bytes(store::Encoded),
    /// Over the size limit: not written, and the chat says so once.
    TooLarge,
    Failed(String),
}

pub(super) enum Item {
    Update(Update),
    Finished(Box<Finished>),
}

#[derive(Default)]
struct Queue {
    items: Vec<Item>,
    /// The UI thread has been told to look and has not taken the items yet.
    signalled: bool,
}

pub(super) struct Shared {
    queue: Mutex<Queue>,
    wake: smol::channel::Sender<()>,
}

impl Shared {
    pub fn new() -> (Arc<Self>, smol::channel::Receiver<()>) {
        let (wake, woken) = smol::channel::bounded(1);
        (
            Arc::new(Self {
                queue: Mutex::default(),
                wake,
            }),
            woken,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn push(&self, item: Item) {
        let mut queue = self.lock();
        match (item, queue.items.last_mut()) {
            // Deltas of one kind arriving back to back are one delta.
            (Item::Update(Update::Text(delta)), Some(Item::Update(Update::Text(open))))
            | (
                Item::Update(Update::Reasoning(delta)),
                Some(Item::Update(Update::Reasoning(open))),
            ) => open.push_str(&delta),
            (item, _) => queue.items.push(item),
        }
        if !queue.signalled {
            queue.signalled = true;
            drop(queue);
            // A full slot means a wake is already waiting; a closed channel, a dropped session.
            let _ = self.wake.try_send(());
        }
    }

    fn update(&self, update: Update) {
        self.push(Item::Update(update));
    }

    /// A write waits for the user's answer (the approver, on the worker thread).
    pub(super) fn ask(&self, question: Question) {
        self.update(Update::Ask(question));
    }

    /// Everything queued so far; the next push wakes the UI thread again.
    pub fn take(&self) -> Vec<Item> {
        let mut queue = self.lock();
        queue.signalled = false;
        std::mem::take(&mut queue.items)
    }

    pub(super) fn event(&self, event: AssistantEvent<'_>) {
        let update = match event {
            AssistantEvent::TextDelta(delta) => Update::Text(delta.to_owned()),
            AssistantEvent::ThinkingDelta(delta) => Update::Reasoning(delta.to_owned()),
            AssistantEvent::ToolCallStarted { id, name } => Update::ToolStarted {
                id: id.to_owned(),
                name: name.to_owned(),
            },
            AssistantEvent::ToolCallSkipped { id, .. } => Update::ToolSkipped { id: id.to_owned() },
            AssistantEvent::ToolCallInputReady { id, input, .. } => Update::ToolInput {
                id: id.to_owned(),
                args: summarize_args(input),
                connection: input
                    .get("connection_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            },
            AssistantEvent::ToolResult {
                id,
                name,
                is_error,
                summary,
            } => Update::ToolResult {
                id: id.to_owned(),
                name: name.to_owned(),
                is_error,
                summary: summary.to_owned(),
            },
            AssistantEvent::Usage(usage) => Update::Usage(usage),
            AssistantEvent::Notice(notice) => Update::Notice(Update::notice(notice)),
            AssistantEvent::ContextLoaded {
                tables_included,
                tables_total,
            } => Update::Context {
                included: tables_included,
                total: tables_total,
            },
            AssistantEvent::ContextUnavailable { reason } => {
                Update::Notice(super::model::ChatNotice::ContextUnavailable {
                    reason: reason.to_owned(),
                })
            }
            AssistantEvent::Done(reason) => Update::Done(reason),
        };
        self.update(update);
    }
}

/// What a run leaves of the conversation, to tell whether its user message is still in it: the
/// number of messages and the blocks of the user message at the tail (the engine adds the new
/// text to a tail user message that holds tool results). One operator note behind that message
/// is looked through, as the engine's own tail logic does: with it, the message the text went
/// into would read as gone and be sent twice.
fn fingerprint(conversation: &Conversation) -> (usize, usize) {
    let messages = conversation.messages();
    let tail = match messages {
        [.., Message::User { content }]
        | [.., Message::User { content }, Message::System { .. }] => content.len(),
        _ => 0,
    };
    (messages.len(), tail)
}

/// Reports the end of a run when it is dropped, whatever happened: a panic anywhere in the run
/// still hands the conversation back, so the chat is never "running" for good.
struct Report {
    shared: Arc<Shared>,
    conversation: Option<Conversation>,
    result: Option<Result<RunSummary, ChatError>>,
    input_kept: bool,
    saved: Saved,
}

impl Drop for Report {
    fn drop(&mut self) {
        let Some(conversation) = self.conversation.take() else {
            return;
        };
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err(ChatError::internal("The assistant stopped unexpectedly.")));
        self.shared.push(Item::Finished(Box::new(Finished {
            conversation,
            result,
            input_kept: self.input_kept,
            saved: std::mem::replace(&mut self.saved, Saved::Skipped),
        })));
    }
}

/// The body of the worker thread. `scope` is the id of the connection the chat is about: the
/// approver asks about no other.
pub(super) fn work(
    conversation: Conversation,
    kind: RunKind,
    setup: RunSetup,
    cancel: CancelToken,
    shared: Arc<Shared>,
    scope: Option<String>,
    keep: Option<KeepPlan>,
) {
    let before = fingerprint(&conversation);
    let mut report = Report {
        shared: Arc::clone(&shared),
        conversation: Some(conversation),
        result: None,
        input_kept: false,
        saved: Saved::Skipped,
    };
    // The approver is called on this thread: it finds the queue of this run through the scope.
    let _scope = RunScope::enter(Arc::clone(&shared), scope);
    // The tool layer is built here, by the first run: it opens the audit log.
    let layer = match setup.layer.resolve() {
        Ok(layer) => layer,
        Err(error) => {
            report.result = Some(Err(ChatError::internal(error.to_string())));
            return;
        }
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let Some(conversation) = report.conversation.as_mut() else {
            return Err(AssistantError::NothingToResume);
        };
        let assistant = Assistant::new(
            setup.provider.as_ref(),
            &layer,
            AssistantConfig {
                model: setup.model.clone(),
                effort: setup.effort,
                ..AssistantConfig::default()
            },
        );
        let mut on_event = |event: AssistantEvent<'_>| shared.event(event);
        match kind {
            RunKind::Send(text) => {
                assistant.run(conversation, UserInput::from(text), &mut on_event, &cancel)
            }
            RunKind::Resume => assistant.resume(conversation, &mut on_event, &cancel),
        }
    }));
    report.input_kept = report
        .conversation
        .as_ref()
        .is_some_and(|conversation| fingerprint(conversation) != before);
    // The file for it, whatever the run's end (a stop or an error leaves a valid conversation):
    // made here, off the UI thread, with nothing copied.
    report.saved = match (keep, report.conversation.as_ref()) {
        (Some(plan), Some(conversation)) if !conversation.is_empty() => {
            let mut header = plan.header;
            header.updated = store::now_ms();
            header.access = header.access.max(store::level_from_u8(
                plan.access.load(std::sync::atomic::Ordering::SeqCst),
            ));
            match store::encode(&header, conversation) {
                Ok(mut encoded) => {
                    encoded.epoch = plan.epoch;
                    Saved::Bytes(encoded)
                }
                Err(store::StoreError::TooLarge) => Saved::TooLarge,
                Err(error) => Saved::Failed(error.to_string()),
            }
        }
        _ => Saved::Skipped,
    };
    report.result = Some(match outcome {
        Ok(Ok(summary)) => Ok(summary),
        Ok(Err(AssistantError::Provider(error))) => Err(ChatError::from_provider(&error)),
        Ok(Err(other)) => Err(ChatError::internal(format!(
            "The assistant could not run: {other}."
        ))),
        Err(_) => Err(ChatError::internal("The assistant stopped unexpectedly.")),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(messages: serde_json::Value) -> Conversation {
        serde_json::from_value(
            serde_json::json!({ "scope": null, "context": null, "messages": messages }),
        )
        .expect("a valid conversation")
    }

    /// The text of a message the engine kept goes into the user message at the tail, even when an
    /// operator note stands behind it; the fingerprint has to see that, or the message reads as
    /// taken back and a retry sends it a second time.
    #[test]
    fn a_kept_message_is_seen_through_a_trailing_note() {
        let results = serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]});
        let with_text = serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
            {"type": "text", "text": "and then?"}]});
        let assistant = serde_json::json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "x", "input": {}}]});
        let first = serde_json::json!({"role": "user", "content": [{"type": "text", "text": "q"}]});
        let note = serde_json::json!({"role": "system", "content": "the user switched schema"});
        let before = fingerprint(&conversation(serde_json::json!([
            first, assistant, results, note
        ])));
        let after = fingerprint(&conversation(serde_json::json!([
            first, assistant, with_text, note
        ])));
        assert_ne!(before, after, "the kept text must show behind a note");
        // Without a note it always did.
        let before = fingerprint(&conversation(serde_json::json!([
            first, assistant, results
        ])));
        let after = fingerprint(&conversation(serde_json::json!([
            first, assistant, with_text
        ])));
        assert_ne!(before, after);
    }

    /// A panic anywhere in a run, even outside the guarded call, still reports its end.
    #[test]
    fn a_run_that_panics_outside_the_engine_call_still_reports_its_end() {
        let (shared, _woken) = Shared::new();
        let reporting = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            let _report = Report {
                shared: reporting,
                conversation: Some(Conversation::new(None)),
                result: None,
                input_kept: false,
                saved: Saved::Skipped,
            };
            panic!("the layer failed in a way nobody expected");
        });
        assert!(thread.join().is_err());
        let items = shared.take();
        match items.as_slice() {
            [Item::Finished(done)] => {
                assert!(done.result.is_err());
                assert!(!done.input_kept);
            }
            _ => panic!("expected one Finished"),
        }
    }
}
