//! The display model of a chat: what a panel renders, in order. Plain data, no GPUI: the session
//! (`session.rs`) feeds it [`Update`]s on the UI thread and notifies the view.

use super::approval::{Answer, Question};
use engine_ai::{
    Conversation, DoneReason, Message, Notice, ProviderError, RetryCause, Usage, UserBlock,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Longest one-line argument summary of a tool step, in characters.
const ARGS_CHARS: usize = 100;

/// One entry of the chat, in the order it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    User(String),
    /// The model's answer as Markdown source; it grows while the answer streams.
    Assistant(String),
    /// The reasoning summary; grows while it streams.
    Reasoning(String),
    Tool(ToolStep),
    Notice(ChatNotice),
    Error(ChatError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolStep {
    /// The provider's id of the call (matches the result to the step).
    pub id: String,
    pub name: String,
    /// One line: the statement or the main arguments (empty until the input is complete).
    pub args: String,
    pub state: StepState,
    /// A short line about the result or the failure (empty while it runs).
    pub result: String,
    /// The write this step asked the user to approve, if it was one.
    pub question: Option<Arc<Question>>,
    /// The connection the call names, once its input is complete.
    pub connection: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Ok,
    Error,
}

/// Something the user should know that is not an answer and not a failure.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatNotice {
    /// The provider asked for a pause; the request is sent again after `after`.
    Retrying {
        attempt: u32,
        after: Duration,
        cause: RetryCause,
    },
    /// The model declined to answer (`category` as the provider names it).
    Declined { category: Option<String> },
    /// The output limit cut the answer off; `tool_calls` calls it asked for were not run.
    CutOff { tool_calls: usize },
    /// A fallback model gave this answer.
    FallbackServed { model: Option<String> },
    /// The chat runs without the schema summary.
    ContextUnavailable { reason: String },
    /// The tool loop stopped after `rounds` rounds.
    RoundLimit { rounds: usize },
    /// The conversation no longer fits the model; start a new chat.
    ContextFull,
    /// The user stopped the answer.
    Cancelled,
    /// The model answered with nothing. `user_message_taken_back`: the conversation is as before
    /// the run (Retry sends the message again); otherwise it keeps the message and the tool
    /// results (Retry asks the model to continue).
    EmptyAnswer { user_message_taken_back: bool },
    /// The agent access of the chat's connection was lowered while the chat had history: the
    /// history may hold more than the new level allows, so the chat cannot go on.
    AccessLowered,
    /// The conversation grew over the size limit for a file kept on this computer: it goes on in
    /// this window but is no longer written.
    TooLargeToKeep,
    /// Anything the engine reports that this build has no wording for: shown, never dropped.
    Other(String),
}

/// What went wrong, in terms the user can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// No API key is saved.
    NoKey,
    /// The credential store could not be read.
    KeyUnavailable,
    /// The provider rejected the key.
    Authentication,
    Permission,
    /// The model or address does not exist at the provider.
    NotFound,
    BadRequest,
    TooLarge,
    RateLimited,
    Overloaded,
    Server,
    /// Any other refusal by the provider (billing, conflicts).
    Rejected,
    /// The provider could not be reached or the connection broke.
    Network,
    /// The provider's answer broke the protocol.
    Protocol,
    /// The assistant itself failed.
    Internal,
    /// The assistant cannot be set up as Settings stand.
    Settings,
}

/// What the user can do about an error, as a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAction {
    /// Open Settings on the AI page (missing or rejected key, unknown model).
    OpenSettings,
    /// Send the same request again.
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatError {
    pub kind: ErrorKind,
    /// A sentence for the user. Never holds the API key or the request.
    pub message: String,
    pub action: Option<ErrorAction>,
    /// The provider's request id, for a support request.
    pub request_id: Option<String>,
}

impl ChatError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: message.into(),
            action: Some(ErrorAction::Retry),
            request_id: None,
        }
    }

    /// The assistant cannot be set up as Settings stand: fixing them is the way on (a retry
    /// would only fail the same way, so none is offered).
    pub fn config(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Settings,
            message: message.into(),
            action: Some(ErrorAction::OpenSettings),
            request_id: None,
        }
    }

    /// The error of a failed run. The engine's texts are scrubbed of the key already; this only
    /// words them for a chat.
    pub fn from_provider(error: &ProviderError) -> Self {
        use ErrorAction::{OpenSettings, Retry};
        use ErrorKind as K;
        let detail = |sentence: &str| match error.api_failure() {
            Some(failure) if !failure.message.is_empty() => {
                format!("{sentence} ({})", failure.message)
            }
            _ => sentence.to_owned(),
        };
        let (kind, message, action) = match error {
            ProviderError::Key(engine_ai::KeyError::NotSet | engine_ai::KeyError::Empty) => (
                K::NoKey,
                "No API key is set. Add one in Settings \u{2192} AI.".to_owned(),
                Some(OpenSettings),
            ),
            ProviderError::Key(engine_ai::KeyError::Unavailable(why)) => (
                K::KeyUnavailable,
                format!("The API key could not be read from the system credential store ({why})."),
                Some(OpenSettings),
            ),
            ProviderError::Authentication(_) => (
                K::Authentication,
                detail("The provider rejected the API key. Check it in Settings \u{2192} AI."),
                Some(OpenSettings),
            ),
            ProviderError::Permission(_) => (
                K::Permission,
                detail("The API key is not allowed to do this."),
                Some(OpenSettings),
            ),
            ProviderError::NotFound(_) => (
                K::NotFound,
                detail(
                    "The provider does not know this model or address. Check Settings \u{2192} AI.",
                ),
                Some(OpenSettings),
            ),
            ProviderError::BadRequest(_) => (
                K::BadRequest,
                detail("The provider refused the request."),
                None,
            ),
            ProviderError::TooLarge(_) => (
                K::TooLarge,
                detail("The conversation is too large for the provider. Start a new chat."),
                None,
            ),
            ProviderError::RateLimited(_) => (
                K::RateLimited,
                detail("The provider is limiting requests. Try again in a moment."),
                Some(Retry),
            ),
            ProviderError::Overloaded(_) => (
                K::Overloaded,
                detail("The provider is overloaded right now. Try again in a moment."),
                Some(Retry),
            ),
            ProviderError::Server(_) => (
                K::Server,
                detail("The provider had an error. Try again in a moment."),
                Some(Retry),
            ),
            ProviderError::Rejected(_) => (
                K::Rejected,
                detail("The provider rejected the request."),
                None,
            ),
            ProviderError::Transport { source, .. } => (
                K::Network,
                format!("Could not get an answer from the provider: {source}."),
                Some(Retry),
            ),
            ProviderError::Protocol { .. } => (
                K::Protocol,
                "The provider's answer could not be understood. Try again.".to_owned(),
                Some(Retry),
            ),
            ProviderError::ForeignRunSession => (
                K::Internal,
                "The assistant lost track of its request. Try again.".to_owned(),
                Some(Retry),
            ),
            ProviderError::Serialize(why) => (
                K::Internal,
                format!("The request could not be put together ({why})."),
                None,
            ),
            ProviderError::Cancelled => (
                K::Internal,
                "The request was cancelled.".to_owned(),
                Some(Retry),
            ),
        };
        Self {
            kind,
            message,
            action,
            request_id: error.request_id().map(str::to_owned),
        }
    }
}

/// What the session applies to the model; produced on the worker thread from the engine's events.
#[derive(Debug)]
pub(super) enum Update {
    Text(String),
    Reasoning(String),
    ToolStarted {
        id: String,
        name: String,
    },
    ToolInput {
        id: String,
        args: String,
        /// The connection the call names (the model chooses it; it need not be the chat's).
        connection: Option<String>,
    },
    ToolResult {
        id: String,
        name: String,
        is_error: bool,
        summary: String,
    },
    /// A streamed call that is not part of the turn any more: never run, never answered.
    ToolSkipped {
        id: String,
    },
    /// The tool call that runs now is a write and waits for the user.
    Ask(Question),
    Usage(Usage),
    Notice(ChatNotice),
    Context {
        included: usize,
        total: usize,
    },
    Done(DoneReason),
}

impl Update {
    /// The engine's notice as a chat notice.
    pub(super) fn notice(notice: Notice) -> ChatNotice {
        match notice {
            Notice::Retrying {
                attempt,
                after,
                cause,
            } => ChatNotice::Retrying {
                attempt,
                after,
                cause,
            },
            Notice::FallbackServed { model } => ChatNotice::FallbackServed { model },
            Notice::CutOff { tool_calls } => ChatNotice::CutOff { tool_calls },
            Notice::Refused { category } => ChatNotice::Declined { category },
            Notice::RoundLimit { rounds } => ChatNotice::RoundLimit { rounds },
            Notice::ThinkingDropped => ChatNotice::Other(
                "Earlier reasoning no longer matches the conversation and was left out of it.".to_owned(),
            ),
            Notice::ThinkingBlocksDropped { count } => ChatNotice::Other(format!(
                "The provider dropped {count} earlier reasoning block(s) that no longer matched the conversation; the model went on without them."
            )),
            Notice::FeatureDropped(feature) => ChatNotice::Other(format!(
                "The provider did not accept {}; continuing without it.",
                match feature {
                    engine_ai::ApiFeature::ThinkingBinding => "the reasoning-binding option",
                    engine_ai::ApiFeature::ServerFallback => "the fallback-model option",
                }
            )),
            Notice::NotesInUserTurn => ChatNotice::Other(
                "This model takes notes inside the user message; they are sent that way from now on.".to_owned(),
            ),
        }
    }
}

/// The ordered items of one chat.
#[derive(Debug, Default)]
pub struct Transcript {
    items: Vec<ChatItem>,
    usage: Option<Usage>,
    /// Tables of the schema summary the assistant was given: `(included, total)`.
    context: Option<(usize, usize)>,
    /// Items before this index belong to turns the engine kept; what follows is the turn in
    /// flight (or the one that failed), which a retry throws away and redoes.
    committed: usize,
    /// Where `committed` moves to once the engine has *kept* the turn whose tokens were just
    /// reported: it reports them before it drops a refused or an empty turn.
    pending_commit: bool,
    /// User messages (by item index) the engine took back: the model never saw them.
    unsent: Vec<usize>,
}

impl Transcript {
    /// The display of a conversation read back from disk, built from the conversation alone:
    /// the user's texts, the model's answers and reasoning summaries, and each tool call with the
    /// state and one-line result its answer gives. A write that was approved reads "ran" (with the
    /// rows it changed), one that was declined "not run: declined": there is no waiting question.
    /// Notices and errors are not part of a conversation and are not rebuilt. Nothing is run and
    /// no model is asked.
    pub(super) fn restore(conversation: &Conversation) -> Self {
        // The answers by call id, from the user messages that carry them.
        let results: HashMap<&str, (&str, bool)> = conversation
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => Some(content),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                UserBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
                UserBlock::Text { .. } => None,
            })
            .collect();
        let mut transcript = Self::default();
        for message in conversation.messages() {
            match message {
                Message::User { content } => {
                    for block in content {
                        if let UserBlock::Text { text } = block {
                            transcript.items.push(ChatItem::User(text.clone()));
                        }
                    }
                }
                Message::Assistant { content } => {
                    // After a fallback mid-answer only the text before the last `fallback`
                    // block counts as shown (the engine writes nothing else of it back).
                    let boundary = content
                        .iter()
                        .rposition(|block| {
                            block.get("type").and_then(Value::as_str) == Some("fallback")
                        })
                        .unwrap_or(0);
                    for (ix, block) in content.iter().enumerate() {
                        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                        let field = |name: &str| {
                            block
                                .get(name)
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                        };
                        match kind {
                            "text" => {
                                if let Some(text) = field("text") {
                                    transcript.items.push(ChatItem::Assistant(text.to_owned()));
                                }
                            }
                            // `reasoning` is the OpenAI wire's thinking (text in `text`).
                            "thinking" | "reasoning" if ix >= boundary => {
                                let name = if kind == "thinking" {
                                    "thinking"
                                } else {
                                    "text"
                                };
                                if let Some(text) = field(name) {
                                    transcript.items.push(ChatItem::Reasoning(text.to_owned()));
                                }
                            }
                            "tool_use" if ix >= boundary => {
                                let (Some(id), Some(name)) = (field("id"), field("name")) else {
                                    continue;
                                };
                                let input = block.get("input").unwrap_or(&Value::Null);
                                let (state, result) = match results.get(id) {
                                    Some((content, is_error)) => (
                                        if *is_error {
                                            StepState::Error
                                        } else {
                                            StepState::Ok
                                        },
                                        result_line(name, content, *is_error),
                                    ),
                                    None => (StepState::Error, "not run".to_owned()),
                                };
                                transcript.items.push(ChatItem::Tool(ToolStep {
                                    id: id.to_owned(),
                                    name: name.to_owned(),
                                    args: summarize_args(input),
                                    state,
                                    result,
                                    question: None,
                                    connection: input
                                        .get("connection_id")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                Message::System { .. } => {}
            }
        }
        transcript.committed = transcript.items.len();
        transcript
    }

    pub fn items(&self) -> &[ChatItem] {
        &self.items
    }

    /// The tokens of the last model turn.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    pub fn context_tables(&self) -> Option<(usize, usize)> {
        self.context
    }

    pub(super) fn push(&mut self, item: ChatItem) {
        let user = matches!(item, ChatItem::User(_));
        self.items.push(item);
        if user {
            self.committed = self.items.len();
        }
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Before a retry: what the failed turn had shown goes (the engine kept none of it) and so
    /// does the error or "stopped" marker; the retry shows its own.
    pub(super) fn rewind_for_retry(&mut self) {
        self.items.truncate(self.committed);
        while matches!(
            self.items.last(),
            Some(
                ChatItem::Error(_)
                    | ChatItem::Notice(
                        ChatNotice::Cancelled
                            | ChatNotice::EmptyAnswer { .. }
                            | ChatNotice::RoundLimit { .. }
                    )
            )
        ) {
            self.items.pop();
        }
        let len = self.items.len();
        self.unsent.retain(|ix| *ix < len);
    }

    /// The item whose row offers Retry: the last item when it is an error or a stop the user can
    /// retry (and the chat can continue from: the tool loop's round limit is one), looking
    /// through one trailing settings error (a retry that could not be set up says so after the
    /// row it belongs to and leaves it retryable).
    pub fn retryable_item(&self) -> Option<usize> {
        let settings_error = matches!(
            self.items.last(),
            Some(ChatItem::Error(error)) if error.kind == ErrorKind::Settings
        );
        let ix = self
            .items
            .len()
            .checked_sub(1 + usize::from(settings_error))?;
        match &self.items[ix] {
            ChatItem::Error(error) => (error.action == Some(ErrorAction::Retry)).then_some(ix),
            ChatItem::Notice(
                ChatNotice::Cancelled
                | ChatNotice::EmptyAnswer { .. }
                | ChatNotice::RoundLimit { .. },
            ) => Some(ix),
            _ => None,
        }
    }

    /// Shows `error` as the newest item. A settings error that follows another one replaces it:
    /// asking again with the same unusable settings does not stack the same message.
    pub(super) fn push_error(&mut self, error: ChatError) {
        if error.kind == ErrorKind::Settings
            && matches!(
                self.items.last(),
                Some(ChatItem::Error(last)) if last.kind == ErrorKind::Settings
            )
        {
            self.items.pop();
        }
        self.items.push(ChatItem::Error(error));
    }

    /// The newest user message was taken back by the engine: the model never saw it.
    pub(super) fn mark_last_user_unsent(&mut self) {
        if let Some(ix) = self
            .items
            .iter()
            .rposition(|item| matches!(item, ChatItem::User(_)))
        {
            if !self.unsent.contains(&ix) {
                self.unsent.push(ix);
            }
        }
    }

    /// The message is being sent again.
    pub(super) fn mark_last_user_sent(&mut self) {
        if let Some(ix) = self
            .items
            .iter()
            .rposition(|item| matches!(item, ChatItem::User(_)))
        {
            self.unsent.retain(|other| *other != ix);
        }
    }

    /// Whether the user message at `ix` never reached the model.
    pub fn is_unsent(&self, ix: usize) -> bool {
        self.unsent.contains(&ix)
    }

    /// A step that never got its result (the run ended) is an error, not "running" forever.
    pub(super) fn settle_steps(&mut self) {
        for item in &mut self.items {
            if let ChatItem::Tool(step) = item {
                if step.state == StepState::Running {
                    step.state = StepState::Error;
                    step.result = "not run".to_owned();
                    if let Some(question) = &step.question {
                        question.stop();
                    }
                }
            }
        }
    }

    /// A write is waiting for the user's answer.
    pub fn is_asking(&self) -> bool {
        self.waiting().is_some()
    }

    /// The step whose write waits for the user.
    pub fn waiting(&self) -> Option<(usize, &Arc<Question>)> {
        self.items
            .iter()
            .enumerate()
            .find_map(|(ix, item)| match item {
                ChatItem::Tool(ToolStep {
                    question: Some(question),
                    ..
                }) if question.is_waiting() => Some((ix, question)),
                _ => None,
            })
    }

    /// The item index of the step whose write waits for the user, and its question.
    pub(super) fn waiting_at(&self) -> Option<(usize, Arc<Question>)> {
        self.waiting()
            .map(|(ix, question)| (ix, Arc::clone(question)))
    }

    /// The question of the step whose write waits for the user, if any.
    pub(super) fn waiting_question(&self) -> Option<Arc<Question>> {
        self.waiting().map(|(_, question)| question.clone())
    }

    /// Ends every waiting question as not answered (Stop, the access lowered, the app closing).
    pub(super) fn stop_questions(&mut self) {
        for item in &mut self.items {
            if let ChatItem::Tool(ToolStep {
                question: Some(question),
                ..
            }) = item
            {
                question.stop();
            }
        }
    }

    /// The step that runs now: the first of the turn's calls that has no result yet (calls run one
    /// after the other, in the order they were asked for) and has not asked yet.
    fn executing_step_mut(&mut self) -> Option<&mut ToolStep> {
        let from = self
            .items
            .iter()
            .rposition(|item| matches!(item, ChatItem::User(_)))
            .unwrap_or(0);
        self.items[from..].iter_mut().find_map(|item| match item {
            ChatItem::Tool(step) if step.state == StepState::Running && step.question.is_none() => {
                Some(step)
            }
            _ => None,
        })
    }

    /// Says how many rows the writes that ran changed, from the results the conversation holds
    /// (the engine's event for a result carries only a size). A step that ran says "ran" until
    /// then.
    pub(super) fn note_rows_changed(&mut self, conversation: &Conversation) {
        for item in &mut self.items {
            let ChatItem::Tool(step) = item else {
                continue;
            };
            let ran = matches!(&step.question, Some(question) if question.answer() == Some(Answer::Approved))
                && step.state == StepState::Ok;
            if !ran {
                continue;
            }
            if let Some(rows) = rows_changed(conversation, &step.id) {
                step.result = match rows {
                    1 => "ran: 1 row affected".to_owned(),
                    rows => format!("ran: {rows} rows affected"),
                };
            }
        }
    }

    pub(super) fn apply(&mut self, update: Update) {
        match update {
            Update::Text(delta) => match self.items.last_mut() {
                Some(ChatItem::Assistant(text)) => text.push_str(&delta),
                _ => self.items.push(ChatItem::Assistant(delta)),
            },
            Update::Reasoning(delta) => match self.items.last_mut() {
                Some(ChatItem::Reasoning(text)) => text.push_str(&delta),
                _ => self.items.push(ChatItem::Reasoning(delta)),
            },
            Update::ToolStarted { id, name } => self.items.push(ChatItem::Tool(ToolStep {
                id,
                name,
                args: String::new(),
                state: StepState::Running,
                result: String::new(),
                question: None,
                connection: None,
            })),
            Update::ToolInput {
                id,
                args,
                connection,
            } => {
                if let Some(step) = self.step_mut(&id) {
                    step.args = args;
                    step.connection = connection;
                }
            }
            Update::ToolResult {
                id,
                name,
                is_error,
                summary,
            } => {
                // Results are only produced for a turn the engine kept.
                self.commit_pending();
                let state = if is_error {
                    StepState::Error
                } else {
                    StepState::Ok
                };
                match self.step_mut(&id) {
                    Some(step) => {
                        step.state = state;
                        step.result = match &step.question {
                            Some(question) => question.outcome(is_error, &summary),
                            None => summary,
                        };
                    }
                    // A result for a call whose start was not seen: still shown.
                    None => self.items.push(ChatItem::Tool(ToolStep {
                        id,
                        name,
                        args: String::new(),
                        state,
                        result: summary,
                        question: None,
                        connection: None,
                    })),
                }
            }
            Update::Ask(question) => {
                // One question at a time: the engine runs one call after another, so a second one
                // is a bug upstream, and it is not shown (it would be an answer to nothing).
                let step = (!self.is_asking())
                    .then(|| self.executing_step_mut())
                    .flatten();
                match step {
                    Some(step) => step.question = Some(Arc::new(question)),
                    None => question.stop(),
                }
            }
            Update::ToolSkipped { id } => {
                // Not a sign that the turn is kept: the engine reports skipped calls before it
                // drops a refused or an empty turn.
                if let Some(step) = self.step_mut(&id) {
                    step.state = StepState::Error;
                    step.result = "not run".to_owned();
                }
            }
            Update::Usage(usage) => {
                // The turn is complete, but kept only if the engine does not drop it next.
                self.usage = Some(usage);
                self.pending_commit = true;
            }
            Update::Context { included, total } => self.context = Some((included, total)),
            Update::Notice(notice) => {
                // A retry that retries again updates its line instead of stacking lines.
                if let (
                    ChatNotice::Retrying { .. },
                    Some(ChatItem::Notice(ChatNotice::Retrying { .. })),
                ) = (&notice, self.items.last())
                {
                    self.items.pop();
                }
                self.items.push(ChatItem::Notice(notice));
            }
            Update::Done(reason) => {
                // A refused or an empty turn is dropped by the engine; any other end keeps it.
                if matches!(
                    reason,
                    DoneReason::Refused { .. } | DoneReason::EmptyTurn { .. }
                ) {
                    self.pending_commit = false;
                } else {
                    self.commit_pending();
                }
                let notice = match reason {
                    DoneReason::Cancelled => Some(ChatNotice::Cancelled),
                    DoneReason::ContextWindowExceeded => Some(ChatNotice::ContextFull),
                    DoneReason::EmptyTurn {
                        user_message_taken_back,
                    } => Some(ChatNotice::EmptyAnswer {
                        user_message_taken_back,
                    }),
                    DoneReason::PauseTurn => Some(ChatNotice::Other(
                        "The provider paused the answer before it was complete.".to_owned(),
                    )),
                    DoneReason::Other(reason) => Some(ChatNotice::Other(format!(
                        "The answer ended with a reason this version does not know ({reason})."
                    ))),
                    // These were announced by their own notice, or are the normal end.
                    DoneReason::EndTurn
                    | DoneReason::StopSequence
                    | DoneReason::MaxTokens
                    | DoneReason::Refused { .. }
                    | DoneReason::RoundLimit => None,
                };
                if let Some(notice) = notice {
                    self.items.push(ChatItem::Notice(notice));
                }
            }
        }
    }

    fn commit_pending(&mut self) {
        if std::mem::take(&mut self.pending_commit) {
            self.committed = self.items.len();
        }
    }

    /// The newest step of a call, searched from the end (the open one is near it).
    fn step_mut(&mut self, id: &str) -> Option<&mut ToolStep> {
        self.items.iter_mut().rev().find_map(|item| match item {
            ChatItem::Tool(step) if step.id == id => Some(step),
            _ => None,
        })
    }
}

/// `affected_rows` of the result the conversation holds for the call `id`.
fn rows_changed(conversation: &Conversation, id: &str) -> Option<u64> {
    conversation
        .messages()
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::User { content } => Some(content),
            _ => None,
        })
        .flatten()
        .find_map(|block| match block {
            UserBlock::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } if tool_use_id == id => serde_json::from_str::<Value>(content)
                .ok()?
                .get("affected_rows")?
                .as_u64(),
            _ => None,
        })
}

/// The one-line result of a tool call, as the live display words it (`ok, N bytes`, the layer's
/// `code: message` for a failure), with what a write says about itself: the rows it changed, or
/// that the user did not approve it.
fn result_line(name: &str, content: &str, is_error: bool) -> String {
    use super::store::{ran, Ran};
    use engine_agent::sanitize::clean;
    if is_error {
        let json: Option<Value> = serde_json::from_str(content).ok();
        let error = json.as_ref().and_then(|json| json.get("error"));
        let code = error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str);
        let message = error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str);
        return match (code, message) {
            (Some(engine_agent::error::WRITE_REFUSED), Some(message))
                if message.starts_with("the user did not approve") =>
            {
                "not run: declined".to_owned()
            }
            (Some(code), Some(message)) => clean(&format!("{code}: {message}"), 200),
            _ => clean(content, 200),
        };
    }
    match ran(name, content) {
        Some(Ran::Wrote { rows: Some(1) }) => "ran: 1 row affected".to_owned(),
        Some(Ran::Wrote { rows: Some(rows) }) => format!("ran: {rows} rows affected"),
        // A statement that changed something and returns no rows (DDL).
        Some(Ran::Wrote { rows: None }) => "ran".to_owned(),
        _ => format!("ok, {} bytes", content.len()),
    }
}

/// The main arguments of a tool call on one line: the statement or command if it has one, else
/// `name: value` pairs of the simple arguments (the connection id is the chat's, not worth a word).
pub(super) fn summarize_args(input: &Value) -> String {
    let Some(map) = input.as_object() else {
        return String::new();
    };
    let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = ["statement", "command"]
        .iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
        .map(one_line)
        .unwrap_or_else(|| {
            map.iter()
                .filter(|(key, _)| key.as_str() != "connection_id")
                .filter_map(|(key, value)| match value {
                    Value::String(text) => Some(format!("{key}: {}", one_line(text))),
                    Value::Number(number) => Some(format!("{key}: {number}")),
                    Value::Bool(flag) => Some(format!("{key}: {flag}")),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(", ")
        });
    if text.chars().count() <= ARGS_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(ARGS_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A conversation as the engine writes it: a question with a read, a write that was approved,
    /// a statement without a result set, a failure, and the answers. (A declined write is restored
    /// from a real declined run, in the service tests: the layer's wording is not repeated here.)
    fn stored() -> Conversation {
        let result = |id: &str, content: &str, is_error: bool| {
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}
            ]})
        };
        let call = |id: &str, statement: &str| {
            json!({"type": "tool_use", "id": id, "name": "execute_query",
                   "input": {"connection_id": "c1", "statement": statement}})
        };
        serde_json::from_value(json!({
            "scope": {"connection_id": "c1", "database": "app"},
            "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "cancel order 7 and 8"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Read first.", "signature": "s"},
                    {"type": "redacted_thinking", "data": "xx"},
                    {"type": "text", "text": "Let me look."},
                    call("a", "SELECT 1")]},
                result("a", "{\"rows\":[]}", false),
                {"role": "assistant", "content": [call("b", "UPDATE orders SET s = 'x' WHERE id = 7")]},
                result("b", "{\"affected_rows\":1}", false),
                {"role": "assistant", "content": [call("c", "CREATE INDEX i ON orders (id)")]},
                result("c", "{\"columns\":[],\"rows\":[],\"row_count\":0}", false),
                {"role": "assistant", "content": [call("d", "SELEKT")]},
                result("d", "{\"error\":{\"code\":\"agent.invalid_arguments\",\"message\":\"bad\"}}", true),
                {"role": "assistant", "content": [{"type": "text", "text": "Order 7 is cancelled; 8 was left alone."}]},
                {"role": "user", "content": [{"type": "text", "text": "thanks"}]},
                {"role": "system", "content": "the user switched schema"},
                {"role": "assistant", "content": [{"type": "text", "text": "You are welcome."}]}
            ]
        }))
        .expect("a valid conversation")
    }

    #[test]
    fn a_reasoning_block_is_shown_like_a_thinking_block() {
        let restore = |block: serde_json::Value| {
            let conversation: Conversation = serde_json::from_value(json!({
                "scope": null, "context": null,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "why?"}]},
                    {"role": "assistant", "content": [block, {"type": "text", "text": "Because."}]}
                ]
            }))
            .expect("a valid conversation");
            Transcript::restore(&conversation).items().to_vec()
        };
        // Anthropic's `thinking` (text in `thinking`) and the OpenAI wire's `reasoning` (in `text`).
        let thinking =
            restore(json!({"type": "thinking", "thinking": "Look first.", "signature": "s"}));
        let reasoning = restore(json!({"type": "reasoning", "text": "Look first."}));
        assert_eq!(thinking, reasoning);
        assert!(matches!(&reasoning[1], ChatItem::Reasoning(text) if text == "Look first."));
        assert!(matches!(&reasoning[2], ChatItem::Assistant(_)));
        // An empty one shows nothing, as before.
        assert_eq!(restore(json!({"type": "reasoning", "text": ""})).len(), 2);
    }

    #[test]
    fn a_stored_conversation_is_shown_from_the_conversation_alone() {
        let transcript = Transcript::restore(&stored());
        let items = transcript.items();
        let kinds: Vec<&str> = items
            .iter()
            .map(|item| match item {
                ChatItem::User(_) => "user",
                ChatItem::Assistant(_) => "answer",
                ChatItem::Reasoning(_) => "reasoning",
                ChatItem::Tool(_) => "tool",
                ChatItem::Notice(_) => "notice",
                ChatItem::Error(_) => "error",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "user",
                "reasoning",
                "answer",
                "tool",
                "tool",
                "tool",
                "tool",
                "answer",
                "user",
                "answer"
            ],
            "{items:?}"
        );
        let steps: Vec<(&str, StepState, &str, Option<&str>)> = items
            .iter()
            .filter_map(|item| match item {
                ChatItem::Tool(step) => Some((
                    step.id.as_str(),
                    step.state,
                    step.result.as_str(),
                    step.connection.as_deref(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            steps,
            [
                ("a", StepState::Ok, "ok, 11 bytes", Some("c1")),
                ("b", StepState::Ok, "ran: 1 row affected", Some("c1")),
                // A statement that changed something and returned no rows (DDL).
                ("c", StepState::Ok, "ran", Some("c1")),
                (
                    "d",
                    StepState::Error,
                    "agent.invalid_arguments: bad",
                    Some("c1")
                ),
            ]
        );
        // An answered write is an answered step, never a waiting question.
        assert!(!transcript.is_asking());
        assert!(items
            .iter()
            .all(|item| !matches!(item, ChatItem::Tool(step) if step.question.is_some())));
        assert!(matches!(&items[1], ChatItem::Reasoning(text) if text == "Read first."));
        assert!(matches!(&items[8], ChatItem::User(text) if text == "thanks"));
        // Nothing of a note, a notice or an error is rebuilt, and everything is committed.
        assert!(items
            .iter()
            .all(|item| !matches!(item, ChatItem::Notice(_) | ChatItem::Error(_))));
        assert_eq!(transcript.committed, items.len());
    }

    #[test]
    fn the_calls_before_a_fallback_are_not_shown_as_part_of_the_turn() {
        let conversation: Conversation = serde_json::from_value(json!({
            "scope": null, "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "kept text"},
                    {"type": "tool_use", "id": "gone", "name": "execute_query", "input": {}},
                    {"type": "fallback", "to": {"model": "m2"}},
                    {"type": "text", "text": "after"}]}
            ]
        }))
        .unwrap();
        let transcript = Transcript::restore(&conversation);
        assert!(
            transcript
                .items()
                .iter()
                .all(|item| !matches!(item, ChatItem::Tool(_))),
            "{:?}",
            transcript.items()
        );
        assert_eq!(transcript.items().len(), 3);
    }

    #[test]
    fn deltas_grow_the_open_item_and_a_tool_step_closes_it() {
        let mut transcript = Transcript::default();
        transcript.apply(Update::Reasoning("think".into()));
        transcript.apply(Update::Reasoning("ing".into()));
        transcript.apply(Update::Text("Hel".into()));
        transcript.apply(Update::Text("lo".into()));
        transcript.apply(Update::ToolStarted {
            id: "t1".into(),
            name: "list_tables".into(),
        });
        transcript.apply(Update::Text("More".into()));
        assert_eq!(
            transcript.items(),
            [
                ChatItem::Reasoning("thinking".into()),
                ChatItem::Assistant("Hello".into()),
                ChatItem::Tool(ToolStep {
                    id: "t1".into(),
                    name: "list_tables".into(),
                    args: String::new(),
                    state: StepState::Running,
                    result: String::new(),
                    question: None,
                    connection: None,
                }),
                ChatItem::Assistant("More".into()),
            ]
        );
    }

    #[test]
    fn a_result_settles_its_own_step_even_when_calls_overlap() {
        let mut transcript = Transcript::default();
        for id in ["a", "b"] {
            transcript.apply(Update::ToolStarted {
                id: id.into(),
                name: "t".into(),
            });
        }
        transcript.apply(Update::ToolResult {
            id: "b".into(),
            name: "t".into(),
            is_error: true,
            summary: "boom".into(),
        });
        let states: Vec<_> = transcript
            .items()
            .iter()
            .filter_map(|item| match item {
                ChatItem::Tool(step) => Some((step.id.as_str(), step.state)),
                _ => None,
            })
            .collect();
        assert_eq!(states, [("a", StepState::Running), ("b", StepState::Error)]);
        transcript.settle_steps();
        assert!(
            matches!(&transcript.items()[0], ChatItem::Tool(step) if step.state == StepState::Error)
        );
    }

    /// The engine reports the calls it skipped before it drops an empty turn: a skip does not say
    /// the turn is kept, so the rows of a dropped turn do not survive a retry.
    #[test]
    fn a_skipped_call_does_not_commit_a_turn_the_engine_then_drops() {
        let mut transcript = Transcript::default();
        transcript.push(ChatItem::User("q".into()));
        transcript.apply(Update::Reasoning("hmm".into()));
        transcript.apply(Update::ToolStarted {
            id: "a".into(),
            name: "t".into(),
        });
        transcript.apply(Update::Usage(Usage::default()));
        transcript.apply(Update::ToolSkipped { id: "a".into() });
        transcript.apply(Update::Done(DoneReason::EmptyTurn {
            user_message_taken_back: true,
        }));
        transcript.rewind_for_retry();
        assert_eq!(transcript.items(), [ChatItem::User("q".into())]);

        // A kept turn is committed by its result, as before.
        let mut transcript = Transcript::default();
        transcript.push(ChatItem::User("q".into()));
        transcript.apply(Update::ToolStarted {
            id: "a".into(),
            name: "t".into(),
        });
        transcript.apply(Update::Usage(Usage::default()));
        transcript.apply(Update::ToolSkipped { id: "b".into() });
        transcript.apply(Update::ToolResult {
            id: "a".into(),
            name: "t".into(),
            is_error: false,
            summary: "ok".into(),
        });
        transcript.apply(Update::Done(DoneReason::Cancelled));
        transcript.rewind_for_retry();
        assert_eq!(transcript.items().len(), 2, "{:?}", transcript.items());
    }

    /// A retry that cannot be set up says so behind the row it belongs to, once, and the row stays
    /// retryable.
    #[test]
    fn a_settings_error_after_a_retryable_row_leaves_it_retryable_and_is_not_stacked() {
        let mut transcript = Transcript::default();
        transcript.push(ChatItem::User("q".into()));
        transcript.apply(Update::Done(DoneReason::Cancelled));
        assert_eq!(transcript.retryable_item(), Some(1));
        transcript.push_error(ChatError::config("the address cannot be used"));
        assert_eq!(transcript.items().len(), 3);
        assert_eq!(transcript.retryable_item(), Some(1));
        transcript.push_error(ChatError::config("still not"));
        assert_eq!(transcript.items().len(), 3, "not stacked");
        assert!(matches!(
            transcript.items().last(),
            Some(ChatItem::Error(error)) if error.message == "still not"
        ));
        // Another kind of error after it is not looked through, and a lone settings error is not
        // retryable.
        transcript.push_error(ChatError::internal("boom"));
        assert_eq!(transcript.retryable_item(), Some(3));
        let mut lone = Transcript::default();
        lone.push_error(ChatError::config("x"));
        assert_eq!(lone.retryable_item(), None);
        // Retry throws the stop and the settings error away together.
        transcript.rewind_for_retry();
        assert_eq!(transcript.items(), [ChatItem::User("q".into())]);
    }

    #[test]
    fn a_second_retry_notice_replaces_the_first() {
        let mut transcript = Transcript::default();
        for attempt in 1..=2 {
            transcript.apply(Update::Notice(ChatNotice::Retrying {
                attempt,
                after: Duration::from_secs(u64::from(attempt)),
                cause: RetryCause::RateLimited,
            }));
        }
        assert_eq!(transcript.items().len(), 1);
        assert!(matches!(
            &transcript.items()[0],
            ChatItem::Notice(ChatNotice::Retrying { attempt: 2, .. })
        ));
    }

    #[test]
    fn arguments_are_one_short_line() {
        assert_eq!(
            summarize_args(&json!({"connection_id": "c1", "statement": "SELECT *\n  FROM  users"})),
            "SELECT * FROM users"
        );
        assert_eq!(
            summarize_args(
                &json!({"connection_id": "c1", "table": "orders", "limit": 5, "x": [1]})
            ),
            "table: orders, limit: 5"
        );
        assert_eq!(summarize_args(&json!({"connection_id": "c1"})), "");
        assert_eq!(summarize_args(&json!("nope")), "");
        let long = summarize_args(&json!({"statement": "x".repeat(500)}));
        assert_eq!(long.chars().count(), ARGS_CHARS + 1);
        assert!(long.ends_with('\u{2026}'));
    }

    #[test]
    fn provider_errors_become_a_sentence_and_an_action() {
        let failure = |message: &str| engine_ai::ApiFailure {
            status: Some(401),
            message: message.into(),
            request_id: Some("req_1".into()),
            retry_after: None,
        };
        let auth =
            ChatError::from_provider(&ProviderError::Authentication(failure("invalid x-api-key")));
        assert_eq!(auth.kind, ErrorKind::Authentication);
        assert_eq!(auth.action, Some(ErrorAction::OpenSettings));
        assert_eq!(auth.request_id.as_deref(), Some("req_1"));
        assert!(auth.message.contains("invalid x-api-key"));
        let none = ChatError::from_provider(&ProviderError::Key(engine_ai::KeyError::NotSet));
        assert_eq!(
            (none.kind, none.action),
            (ErrorKind::NoKey, Some(ErrorAction::OpenSettings))
        );
        let limited = ChatError::from_provider(&ProviderError::RateLimited(failure("slow")));
        assert_eq!(limited.action, Some(ErrorAction::Retry));
        let bad =
            ChatError::from_provider(&ProviderError::BadRequest(failure("max_tokens too big")));
        assert_eq!(bad.action, None);
    }
}
