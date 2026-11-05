//! The conversation: an append-only list of messages that is always valid to send.
//!
//! * Assistant content is kept as the provider's content blocks, verbatim JSON (thinking text and
//!   signatures, unknown block types, order). [`Message::tool_uses`] is the parsed view.
//! * No method edits or removes a message that is in the list. The assistant stages the messages of
//!   a run and takes them back ([`Checkpoint`]) only when the run's first turn did not complete.
//!   The tail of the list, the user message that no assistant turn has answered yet, may grow:
//!   new text goes behind its tool results, because the API refuses two user messages in a row.
//! * Operator notes are `system` messages. The API accepts one only right after a user message and
//!   only if it is last or followed by an assistant turn, so a note cannot be placed: it is queued
//!   with [`Conversation::note`] and written right behind the next user message (or, for
//!   [`crate::Assistant::resume`], behind the tool results the run stopped at).
//! * A conversation read from storage is checked against the same rules ([`ConversationError`]).

use crate::provider::Effort;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a conversation is about: the connection (and namespace) the user works on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

/// The per-conversation system block (connection, schema digest), fixed once the first turn is
/// committed so the cached prefix never changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemContext {
    pub text: String,
    /// The text includes the schema digest (it is big and gets a cache breakpoint).
    pub has_schema: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserBlock {
    Text {
        text: String,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

impl UserBlock {
    fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<UserBlock>,
    },
    /// The content blocks exactly as the provider sent them.
    Assistant {
        content: Vec<Value>,
    },
    /// An operator note (see the module docs).
    System {
        content: String,
    },
}

/// A tool call of an assistant message.
#[derive(Debug, Clone, Copy)]
pub struct ToolUse<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
}

/// How the history is written into a request. Both switches are set by the assistant when the API
/// refused the history in the usual form, and are stored with the conversation so that every later
/// request is written the same way (a repeated strip is deterministic, and the cache stays valid).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryForm {
    /// Leave `thinking` and `redacted_thinking` blocks out of the assistant messages with an index
    /// below this one. A boundary, not a flag: the strip is a one-time recovery for the history
    /// the API refused, and the thinking the model produces afterwards stays valid and stays in.
    /// The messages before it are written the same way in every request, so the stripped prefix is
    /// byte-identical from request to request. `0`: nothing is stripped.
    #[serde(default)]
    pub drop_thinking_before: usize,
    /// Send operator notes as a `<system-reminder>` text block at the end of the user message they
    /// follow, for a model that does not take `system` messages in the conversation.
    #[serde(default)]
    pub notes_in_user_turn: bool,
}

/// What the provider adds to [`HistoryForm`] when it writes assistant content back.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EchoForm {
    pub drop_thinking: bool,
    /// The `fallback` marker block goes back (only when the request carries the fallback beta).
    pub keep_fallback: bool,
}

impl EchoForm {
    /// The least a turn can send back: no thinking, no `fallback` marker. A turn that has nothing
    /// left in this form could become an empty assistant message, which the API refuses.
    pub const STRICTEST: Self = Self {
        drop_thinking: true,
        keep_fallback: false,
    };
}

/// One content block of an assistant message.
#[derive(Clone, Copy)]
pub(crate) struct Block<'a>(pub &'a Value);

impl<'a> Block<'a> {
    pub fn kind(self) -> &'a str {
        self.str("type").unwrap_or("")
    }

    pub fn str(self, field: &str) -> Option<&'a str> {
        self.0.get(field).and_then(Value::as_str)
    }

    fn is_thinking(self) -> bool {
        matches!(self.kind(), "thinking" | "redacted_thinking")
    }

    fn tool_use(self) -> Option<ToolUse<'a>> {
        if self.kind() != "tool_use" {
            return None;
        }
        Some(ToolUse {
            id: self.str("id")?,
            name: self.str("name")?,
            input: self.0.get("input")?,
        })
    }
}

/// The content blocks of an assistant message.
#[derive(Clone, Copy)]
pub(crate) struct Content<'a>(pub &'a [Value]);

impl<'a> Content<'a> {
    /// Index of the last `fallback` block: what comes before it was produced by a model that
    /// declined, and the API wants only its text back.
    fn boundary(self) -> Option<usize> {
        self.0.iter().rposition(|b| Block(b).kind() == "fallback")
    }

    /// The blocks that are sent back. After a mid-output fallback everything before the last
    /// `fallback` block is dropped except text (thinking, redacted thinking, tool calls and unknown
    /// model-internal blocks are not echoed there); the `fallback` block and what follows it go back
    /// as they are, the marker only when `keep_fallback`. With `drop_thinking`, thinking blocks stay
    /// out everywhere.
    pub fn echo(self, form: EchoForm) -> impl Iterator<Item = &'a Value> {
        let boundary = self.boundary();
        self.0.iter().enumerate().filter_map(move |(index, value)| {
            let block = Block(value);
            let kind = block.kind();
            let before = boundary.is_some_and(|boundary| index < boundary);
            let keep = (!before || kind == "text")
                && !(form.drop_thinking && block.is_thinking())
                // A `reasoning` block is what an OpenAI-compatible model showed; no request
                // sends it back, so a conversation can go on with any provider.
                && kind != "reasoning"
                && (form.keep_fallback || kind != "fallback");
            keep.then_some(value)
        })
    }

    /// The tool calls to answer: those the API will see again, in order.
    pub fn tool_uses(self) -> impl Iterator<Item = ToolUse<'a>> {
        let boundary = self.boundary().unwrap_or(0);
        self.0[boundary..]
            .iter()
            .filter_map(|value| Block(value).tool_use())
    }

    /// Tool calls before the fallback boundary: streamed, but not part of the turn any more.
    pub fn skipped_tool_uses(self) -> impl Iterator<Item = ToolUse<'a>> {
        let boundary = self.boundary().unwrap_or(0);
        self.0[..boundary]
            .iter()
            .filter_map(|value| Block(value).tool_use())
    }

    /// The model the last `fallback` block names as the one that continued.
    pub fn fallback_target(self) -> Option<&'a str> {
        let block = self.0.get(self.boundary()?)?;
        block.get("to")?.get("model")?.as_str()
    }

    /// Something is left when the turn is written back in its strictest form
    /// ([`EchoForm::STRICTEST`]): text, or a tool call or unknown block behind the last fallback
    /// boundary. Thinking, markers and what the boundary drops do not count: a turn without
    /// substance would be an empty assistant message in some request.
    pub fn has_substance(self) -> bool {
        self.echo(EchoForm::STRICTEST).next().is_some()
    }

    fn is_valid(self) -> bool {
        !self.0.is_empty()
            && self.0.iter().all(|value| {
                let block = Block(value);
                value.is_object()
                    && !block.kind().is_empty()
                    && (block.kind() != "text"
                        || block.str("text").is_some_and(|t| !t.trim().is_empty()))
            })
    }
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![UserBlock::Text { text: text.into() }],
        }
    }

    /// The tool calls of an assistant message (empty for the other roles), in order.
    pub fn tool_uses(&self) -> Vec<ToolUse<'_>> {
        match self {
            Self::Assistant { content } => Content(content).tool_uses().collect(),
            _ => Vec::new(),
        }
    }

    /// The text blocks of an assistant message, joined (a view for display and tests).
    pub fn assistant_text(&self) -> String {
        let Self::Assistant { content } = self else {
            return String::new();
        };
        content
            .iter()
            .filter(|block| Block(block).kind() == "text")
            .filter_map(|block| Block(block).str("text"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    #[error("message {0} is not from the user, but the first message must be")]
    FirstNotUser(usize),
    #[error("system note {0} does not follow a user message")]
    NoteNotAfterUser(usize),
    #[error("system note {0} is followed by something other than an assistant turn")]
    NoteNotBeforeAssistant(usize),
    #[error("system note {0} is empty")]
    EmptyNote(usize),
    #[error("message {0} has no content")]
    EmptyMessage(usize),
    #[error("message {0} has a text block without text")]
    EmptyText(usize),
    #[error("assistant message {0} holds a block that is not an object with a type")]
    BadBlock(usize),
    #[error("message {0} follows a message of the same role")]
    SameRoleTwice(usize),
    #[error("the tool calls of assistant message {0} are not answered, in order, by the next user message")]
    UnansweredToolUse(usize),
    #[error(
        "user message {0} holds tool results that answer no tool call of the message before it"
    )]
    OrphanToolResult(usize),
    #[error("user message {0} puts text before a tool result")]
    TextBeforeToolResult(usize),
}

/// A list of messages, checked against the rules of the API.
pub(crate) struct Transcript<'a>(pub &'a [Message]);

impl Transcript<'_> {
    pub fn validate(&self) -> Result<(), ConversationError> {
        use ConversationError as E;
        let messages = self.0;
        for (index, message) in messages.iter().enumerate() {
            let previous = index.checked_sub(1).map(|p| &messages[p]);
            match (message, previous) {
                (Message::User { .. }, Some(Message::User { .. }))
                | (Message::Assistant { .. }, Some(Message::Assistant { .. })) => {
                    return Err(E::SameRoleTwice(index))
                }
                (Message::User { .. }, _) => {}
                (_, None) => return Err(E::FirstNotUser(index)),
                _ => {}
            }
            match message {
                Message::User { content } => Self::validate_user(index, content, previous)?,
                Message::System { content } => {
                    if content.trim().is_empty() {
                        return Err(E::EmptyNote(index));
                    }
                    if !matches!(previous, Some(Message::User { .. })) {
                        return Err(E::NoteNotAfterUser(index));
                    }
                    if messages
                        .get(index + 1)
                        .is_some_and(|next| !matches!(next, Message::Assistant { .. }))
                    {
                        return Err(E::NoteNotBeforeAssistant(index));
                    }
                }
                Message::Assistant { content } => {
                    if !Content(content).has_substance() {
                        return Err(E::EmptyMessage(index));
                    }
                    if !Content(content).is_valid() {
                        return Err(if content.iter().any(|b| Block(b).kind() == "text") {
                            E::EmptyText(index)
                        } else {
                            E::BadBlock(index)
                        });
                    }
                    let asks = Content(content).tool_uses().next().is_some();
                    if asks && !matches!(messages.get(index + 1), Some(Message::User { .. })) {
                        return Err(E::UnansweredToolUse(index));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_user(
        index: usize,
        content: &[UserBlock],
        previous: Option<&Message>,
    ) -> Result<(), ConversationError> {
        use ConversationError as E;
        if content.is_empty() {
            return Err(E::EmptyMessage(index));
        }
        let mut seen_text = false;
        let mut answered = Vec::new();
        for block in content {
            match block {
                UserBlock::Text { text } if text.trim().is_empty() => {
                    return Err(E::EmptyText(index))
                }
                UserBlock::Text { .. } => seen_text = true,
                UserBlock::ToolResult { tool_use_id, .. } => {
                    if seen_text {
                        return Err(E::TextBeforeToolResult(index));
                    }
                    answered.push(tool_use_id.as_str());
                }
            }
        }
        let asked: Vec<&str> = match previous {
            Some(Message::Assistant { content }) => {
                Content(content).tool_uses().map(|call| call.id).collect()
            }
            _ => Vec::new(),
        };
        if asked == answered {
            return Ok(());
        }
        Err(if answered.is_empty() {
            E::UnansweredToolUse(index - 1)
        } else if asked.is_empty() {
            E::OrphanToolResult(index)
        } else {
            E::UnansweredToolUse(index - 1)
        })
    }
}

/// `form` as stored: older versions wrote `drop_thinking: true` (strip every thinking block, for
/// good) where this one writes a boundary.
#[derive(Deserialize, Default)]
struct StoredForm {
    #[serde(default)]
    drop_thinking: bool,
    #[serde(default)]
    drop_thinking_before: usize,
    #[serde(default)]
    notes_in_user_turn: bool,
}

impl StoredForm {
    /// The flag meant "all thinking that is in the history"; that is the boundary behind the
    /// last stored message. Nothing that was stripped before comes back, and what the model
    /// says from now on keeps its reasoning.
    fn into_form(self, stored_messages: usize) -> HistoryForm {
        HistoryForm {
            drop_thinking_before: if self.drop_thinking {
                self.drop_thinking_before.max(stored_messages)
            } else {
                self.drop_thinking_before
            },
            notes_in_user_turn: self.notes_in_user_turn,
        }
    }
}

#[derive(Deserialize)]
struct ConversationData {
    scope: Option<Scope>,
    context: Option<SystemContext>,
    messages: Vec<Message>,
    #[serde(default)]
    pending_notes: Vec<String>,
    #[serde(default)]
    form: StoredForm,
    #[serde(default)]
    effort: Option<Effort>,
    #[serde(default)]
    drops_reported: bool,
}

impl TryFrom<ConversationData> for Conversation {
    type Error = ConversationError;

    fn try_from(data: ConversationData) -> Result<Self, Self::Error> {
        Transcript(&data.messages).validate()?;
        let form = data.form.into_form(data.messages.len());
        Ok(Self {
            scope: data.scope,
            context: data.context,
            messages: data.messages,
            pending_notes: data.pending_notes,
            form,
            effort: data.effort,
            drops_reported: data.drops_reported,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ConversationData")]
pub struct Conversation {
    scope: Option<Scope>,
    context: Option<SystemContext>,
    messages: Vec<Message>,
    pending_notes: Vec<String>,
    form: HistoryForm,
    effort: Option<Effort>,
    /// The API told us once that it dropped thinking blocks of this conversation; the user has
    /// been told.
    drops_reported: bool,
}

/// What a run undoes when its first turn does not complete: the tail it may have grown.
pub(crate) struct Checkpoint {
    base: usize,
    tail: Vec<Message>,
    notes: Vec<String>,
}

impl Conversation {
    pub fn new(scope: Option<Scope>) -> Self {
        Self {
            scope,
            context: None,
            messages: Vec::new(),
            pending_notes: Vec::new(),
            form: HistoryForm::default(),
            effort: None,
            drops_reported: false,
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }

    pub fn context(&self) -> Option<&SystemContext> {
        self.context.as_ref()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Notes waiting for the next user message.
    pub fn pending_notes(&self) -> &[String] {
        &self.pending_notes
    }

    /// How the history is written into requests (set when the API refused the usual form).
    pub fn form(&self) -> HistoryForm {
        self.form
    }

    /// The effort this conversation runs at, fixed when its first run starts: `effort` is a
    /// top-level request field, and changing it between two requests of a conversation would
    /// invalidate the cached messages. A new conversation takes the setting of its first run.
    pub fn effort(&self) -> Option<Effort> {
        self.effort
    }

    /// Queues an operator note ("the user switched to schema X"). It is sent as a mid-conversation
    /// `system` message right behind the next user message, which is the only place the API
    /// accepts one; several notes are joined into one message.
    ///
    /// The text is sent as an operator instruction. **Names that come from a database** (schemas,
    /// tables, anything a user or an attacker could have chosen) must go through
    /// [`Scope::quote`] before they are put into a note.
    pub fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            self.pending_notes.push(text);
        }
    }

    /// The last message that is not an operator note is from the user: a run stopped behind its
    /// tool results (or before any answer), and [`crate::Assistant::resume`] can go on from here.
    pub fn ends_with_user(&self) -> bool {
        self.tail_user().is_some()
    }

    /// Index of the user message at the tail, looking through one trailing note.
    fn tail_user(&self) -> Option<usize> {
        let last = self.messages.len().checked_sub(1)?;
        match &self.messages[last] {
            Message::User { .. } => Some(last),
            Message::System { .. } if last > 0 => {
                matches!(self.messages[last - 1], Message::User { .. }).then_some(last - 1)
            }
            _ => None,
        }
    }

    pub(crate) fn set_context(&mut self, context: SystemContext) {
        // Only before the first turn is committed: afterwards the prefix is frozen.
        if self.messages.is_empty() {
            self.context = Some(context);
        }
    }

    pub(crate) fn freeze_effort(&mut self, effort: Effort) {
        if self.messages.is_empty() {
            self.effort = Some(effort);
        }
    }

    pub(crate) fn set_form(&mut self, form: HistoryForm) {
        self.form = form;
    }

    /// Where a strip of the thinking in the history ends: everything up to and including the last
    /// assistant message that exists now. An assistant message that is added later has a higher
    /// index (also after a run was taken back), so what the model produces afterwards is not
    /// stripped.
    pub(crate) fn thinking_strip_boundary(&self) -> usize {
        self.messages
            .iter()
            .rposition(|message| matches!(message, Message::Assistant { .. }))
            .map_or(0, |last| last + 1)
    }

    /// True once: the first time it is called after the API reported dropped thinking blocks.
    pub(crate) fn first_drop_report(&mut self) -> bool {
        !std::mem::replace(&mut self.drops_reported, true)
    }

    pub(crate) fn checkpoint(&self) -> Checkpoint {
        let base = self.tail_user().unwrap_or(self.messages.len());
        Checkpoint {
            base,
            tail: self.messages[base..].to_vec(),
            notes: self.pending_notes.clone(),
        }
    }

    pub(crate) fn restore(&mut self, checkpoint: Checkpoint) {
        self.messages.truncate(checkpoint.base);
        self.messages.extend(checkpoint.tail);
        self.pending_notes = checkpoint.notes;
    }

    /// Stages the user's text (and the queued notes behind it). It goes into the user message at
    /// the tail when there is one, behind its tool results; two user messages in a row are
    /// refused by the API.
    pub(crate) fn begin_user(&mut self, text: String) -> Checkpoint {
        let checkpoint = self.checkpoint();
        match self.tail_user() {
            Some(index) => {
                if let Message::User { content } = &mut self.messages[index] {
                    content.push(UserBlock::Text { text });
                }
            }
            None => self.messages.push(Message::user_text(text)),
        }
        self.flush_notes();
        checkpoint
    }

    /// Writes the queued notes behind the user message at the tail (into the note that is
    /// already there, if any).
    pub(crate) fn flush_notes(&mut self) {
        let Some(index) = self.tail_user() else {
            return;
        };
        if self.pending_notes.is_empty() {
            return;
        }
        let joined = std::mem::take(&mut self.pending_notes).join("\n");
        match self.messages.get_mut(index + 1) {
            Some(Message::System { content }) => {
                content.push('\n');
                content.push_str(&joined);
            }
            _ => self.messages.push(Message::System { content: joined }),
        }
    }

    pub(crate) fn push_assistant(&mut self, content: Vec<Value>) {
        self.messages.push(Message::Assistant { content });
    }

    /// An assistant turn with tool calls goes in together with its results, never alone.
    pub(crate) fn push_tool_round(&mut self, content: Vec<Value>, results: Vec<UserBlock>) {
        debug_assert!(results.iter().all(UserBlock::is_tool_result));
        self.messages.push(Message::Assistant { content });
        self.messages.push(Message::User { content: results });
    }
}
