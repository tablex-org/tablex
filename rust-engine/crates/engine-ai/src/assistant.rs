//! The tool loop: [`Assistant::run`] sends the conversation to a [`ChatProvider`], runs the tools
//! the model asks for through the agent [`ToolLayer`], sends the results back, and repeats.
//!
//! What holds at every point, including cancellation and errors:
//!
//! * The conversation is sendable. An assistant turn with `tool_use` blocks goes into it together
//!   with one user message holding all their `tool_result`s, in order, or not at all.
//! * A turn is appended only when the provider completed it. If the *first* turn of a run does not
//!   complete (error, cancel, refusal) the run's user message is taken back too, so the same input
//!   can be sent again; later turns leave the conversation ending in the last tool results, and
//!   [`Assistant::resume`] continues from there.
//! * Tools run one after the other on the calling thread. Each call gets its own token tied to the
//!   run's: the statement timeout of one call expires only that call.

use crate::conversation::{
    Checkpoint, Content, Conversation, HistoryForm, Message, SystemContext, ToolUse, UserBlock,
};
use crate::prompt::CORE_PROMPT;
use crate::provider::{
    ApiFeature, ChatProvider, ContextBlock, Effort, InvalidToolInput, ProviderError, RetryCause,
    RunSession, StopReason, StreamEvent, SystemPrompt, TurnRequest, TurnResponse, Usage,
};
use engine_agent::sanitize::clean;
use engine_agent::{CancelToken, ToolLayer, ToolOutcome};
use serde_json::{json, Map, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

/// Longest tool-result summary in an event, in characters.
const SUMMARY_CHARS: usize = 200;
/// Tool calls of one turn that are run; the rest are answered with an error saying so.
pub const MAX_TOOL_CALLS_PER_TURN: usize = 16;
/// Most characters of a tool input that was not valid JSON, sent back to the model.
const MAX_INVALID_JSON_CHARS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantConfig {
    pub model: String,
    pub effort: Effort,
    /// Most rounds (a model turn that asked for tools, and their answers) one run takes.
    pub max_rounds: usize,
}

impl Default for AssistantConfig {
    fn default() -> Self {
        Self {
            model: crate::anthropic::DEFAULT_MODEL.to_owned(),
            effort: Effort::default(),
            max_rounds: 16,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self { text: text.into() }
    }
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self { text }
    }
}

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneReason {
    EndTurn,
    StopSequence,
    /// The answer was cut off by the output limit.
    MaxTokens,
    /// The provider paused the turn (server-side tools; this crate has none).
    PauseTurn,
    ContextWindowExceeded,
    /// The model declined. The turn is not in the conversation.
    Refused {
        category: Option<String>,
    },
    /// `max_rounds` rounds ran; the conversation ends with the last tool results.
    RoundLimit,
    Cancelled,
    /// The model answered with nothing (no text, no tool call). The turn is not in the
    /// conversation. `user_message_taken_back`: the run's first turn was empty, so the user's
    /// text was taken back too (the conversation is as before the run, and the same input can be
    /// sent again); false: a later turn was empty, and the conversation keeps the user message
    /// (the tool results, with the text of this run if it came first) it ends with.
    EmptyTurn {
        user_message_taken_back: bool,
    },
    /// A stop reason this crate does not know.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub reason: DoneReason,
    /// Rounds in which tools were asked for.
    pub rounds: usize,
    /// Tokens of all model turns of the run.
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A request is being sent again after `after`.
    Retrying {
        attempt: u32,
        after: Duration,
        cause: RetryCause,
    },
    /// A fallback model served this turn.
    FallbackServed {
        model: Option<String>,
    },
    /// The output limit cut the turn off; with `tool_calls` pending calls, none was run.
    CutOff {
        tool_calls: usize,
    },
    Refused {
        category: Option<String>,
    },
    RoundLimit {
        rounds: usize,
    },
    /// The API refused the thinking blocks of the history (they no longer match the conversation
    /// before them); the ones in the history up to now are left out of this conversation's
    /// requests. What the model thinks from now on is kept.
    ThinkingDropped,
    /// The API dropped `count` thinking blocks of the history by itself (the thinking-binding
    /// beta: they no longer match the conversation before them or the model). Said once per
    /// conversation; the model went on without that reasoning.
    ThinkingBlocksDropped {
        count: usize,
    },
    /// The API does not accept an optional feature of the request (a beta header or its field);
    /// the provider leaves it out from now on and sent the request again. Said once per feature.
    FeatureDropped(ApiFeature),
    /// The model takes no `system` messages in the conversation; operator notes go into the user
    /// turn from now on.
    NotesInUserTurn,
}

/// What the caller hears while a run goes on. Text is borrowed: copy it if it must be kept.
#[derive(Debug)]
pub enum AssistantEvent<'a> {
    /// The schema digest is part of this conversation.
    ContextLoaded {
        tables_included: usize,
        tables_total: usize,
    },
    /// The conversation runs without a schema digest.
    ContextUnavailable {
        reason: &'a str,
    },
    TextDelta(&'a str),
    ThinkingDelta(&'a str),
    ToolCallStarted {
        id: &'a str,
        name: &'a str,
    },
    /// A tool call that was streamed but is not part of the turn any more (it came before a
    /// fallback boundary): it is not run and has no result.
    ToolCallSkipped {
        id: &'a str,
        name: &'a str,
    },
    ToolCallInputReady {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
        /// False: the input was not a JSON object; the call is answered with an error.
        valid: bool,
    },
    ToolResult {
        id: &'a str,
        name: &'a str,
        is_error: bool,
        /// A short line, not the payload (which is in the conversation).
        summary: &'a str,
    },
    /// Tokens of one model turn.
    Usage(Usage),
    Notice(Notice),
    Done(DoneReason),
}

#[derive(Debug, thiserror::Error)]
pub enum AssistantError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("the message is empty")]
    EmptyInput,
    #[error("nothing to resume: the conversation does not end with a user message")]
    NothingToResume,
}

pub struct Assistant<'a> {
    provider: &'a dyn ChatProvider,
    tools: &'a ToolLayer,
    pub config: AssistantConfig,
}

struct Answer {
    content: String,
    is_error: bool,
    summary: String,
}

impl Answer {
    fn error(code: &str, message: &str) -> Self {
        let content =
            json!({"error": {"code": code, "message": message, "retryable": false}}).to_string();
        Self {
            content,
            is_error: true,
            summary: clean(&format!("{code}: {message}"), SUMMARY_CHARS),
        }
    }
}

impl From<StopReason> for DoneReason {
    fn from(stop: StopReason) -> Self {
        match stop {
            StopReason::EndTurn | StopReason::ToolUse => Self::EndTurn,
            StopReason::StopSequence => Self::StopSequence,
            StopReason::MaxTokens => Self::MaxTokens,
            StopReason::PauseTurn => Self::PauseTurn,
            StopReason::ModelContextWindowExceeded => Self::ContextWindowExceeded,
            StopReason::Refusal { category } => Self::Refused { category },
            StopReason::Other(other) => Self::Other(other),
        }
    }
}

impl<'a> Assistant<'a> {
    pub fn new(
        provider: &'a dyn ChatProvider,
        tools: &'a ToolLayer,
        config: AssistantConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            config,
        }
    }

    /// Sends `input` and runs the tool loop to the end of the model's answer. Blocking; call it
    /// from a worker thread and cancel it with `cancel`.
    ///
    /// When the previous run stopped behind its tool results (a cancel, an error, the round
    /// limit), the text goes into that user message, after the results.
    pub fn run(
        &self,
        conversation: &mut Conversation,
        input: UserInput,
        on_event: &mut dyn FnMut(AssistantEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<RunSummary, AssistantError> {
        if input.text.trim().is_empty() {
            return Err(AssistantError::EmptyInput);
        }
        if cancel.is_cancelled() {
            return Ok(Self::cancelled(on_event));
        }
        let session = self.provider.begin_run()?;
        if !self.load_context(conversation, on_event, cancel) {
            return Ok(Self::cancelled(on_event));
        }
        conversation.freeze_effort(self.config.effort);
        let checkpoint = conversation.begin_user(input.text);
        self.drive(conversation, &session, Some(checkpoint), on_event, cancel)
    }

    /// Continues a run that stopped with the tool results as the last message (a cancel, an
    /// error, or the round limit), without a new user message. Queued operator notes are written
    /// behind the results first.
    pub fn resume(
        &self,
        conversation: &mut Conversation,
        on_event: &mut dyn FnMut(AssistantEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<RunSummary, AssistantError> {
        if !conversation.ends_with_user() {
            return Err(AssistantError::NothingToResume);
        }
        if cancel.is_cancelled() {
            return Ok(Self::cancelled(on_event));
        }
        let session = self.provider.begin_run()?;
        conversation.flush_notes();
        self.drive(conversation, &session, None, on_event, cancel)
    }

    fn cancelled(on_event: &mut dyn FnMut(AssistantEvent<'_>)) -> RunSummary {
        on_event(AssistantEvent::Done(DoneReason::Cancelled));
        RunSummary {
            reason: DoneReason::Cancelled,
            rounds: 0,
            usage: Usage::default(),
        }
    }

    /// Takes the connection summary once, before the first turn of the conversation, through the
    /// tool layer (`open_session`, then `get_schema_context`). Failing to is not fatal: the
    /// conversation runs without the digest. Returns false when the run was cancelled meanwhile.
    fn load_context(
        &self,
        conversation: &mut Conversation,
        on_event: &mut dyn FnMut(AssistantEvent<'_>),
        cancel: &CancelToken,
    ) -> bool {
        let Some(scope) = conversation.scope().cloned() else {
            return true;
        };
        let has_schema = conversation.context().is_some_and(|c| c.has_schema);
        if !conversation.is_empty() || has_schema {
            return true;
        }
        let mut open = Map::new();
        open.insert(
            "connection_id".into(),
            Value::String(scope.connection_id.clone()),
        );
        let mut describe = open.clone();
        if let Some(database) = &scope.database {
            describe.insert("database".into(), Value::String(database.clone()));
        }
        if let Some(schema) = &scope.schema {
            describe.insert("schema".into(), Value::String(schema.clone()));
        }
        let mut failure = None;
        let mut digest = None;
        match self.call(cancel, "open_session", Value::Object(open)) {
            Ok(ToolOutcome::Failure(error)) => failure = Some(error),
            Ok(ToolOutcome::Success(_)) => {
                match self.call(cancel, "get_schema_context", Value::Object(describe)) {
                    Ok(ToolOutcome::Success(value)) => digest = Some(value),
                    Ok(ToolOutcome::Failure(error)) => failure = Some(error),
                    Err(panicked) => failure = Some(panicked),
                }
            }
            Err(panicked) => failure = Some(panicked),
        }
        if cancel.is_cancelled() {
            return false;
        }
        let text = digest
            .as_ref()
            .and_then(|value| value.get("text"))
            .and_then(Value::as_str);
        match (text, failure) {
            (Some(text), _) => {
                let count = |key: &str| {
                    digest
                        .as_ref()
                        .and_then(|value| value.get(key))
                        .and_then(Value::as_u64)
                        .map_or(0, |count| usize::try_from(count).unwrap_or(usize::MAX))
                };
                on_event(AssistantEvent::ContextLoaded {
                    tables_included: count("tables_included"),
                    tables_total: count("tables_total"),
                });
                conversation.set_context(SystemContext {
                    text: scope.render_context(Some(text)),
                    has_schema: true,
                });
            }
            (None, failure) => {
                let reason = failure.map_or_else(
                    || "the schema digest was empty".to_owned(),
                    |error| clean(&format!("{}: {}", error.code, error.message), SUMMARY_CHARS),
                );
                on_event(AssistantEvent::ContextUnavailable { reason: &reason });
                conversation.set_context(SystemContext {
                    text: scope.render_context(None),
                    has_schema: false,
                });
            }
        }
        true
    }

    /// One tool call with a token of its own, tied to the run's. A panic in the layer becomes an
    /// error outcome so the conversation stays sendable.
    fn call(
        &self,
        run_cancel: &CancelToken,
        name: &str,
        arguments: Value,
    ) -> Result<ToolOutcome, engine_agent::ToolError> {
        let call_cancel = CancelToken::new();
        let _link = run_cancel.on_cancel({
            let call_cancel = call_cancel.clone();
            move || call_cancel.cancel_detached()
        });
        catch_unwind(AssertUnwindSafe(|| {
            self.tools.call_cancellable(name, arguments, &call_cancel)
        }))
        .map_err(|_| engine_agent::ToolError {
            code: "assistant.tool_panicked".into(),
            message: "the tool failed unexpectedly".into(),
            retryable: false,
        })
    }

    fn drive(
        &self,
        conversation: &mut Conversation,
        session: &RunSession,
        mut checkpoint: Option<Checkpoint>,
        on_event: &mut dyn FnMut(AssistantEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<RunSummary, AssistantError> {
        let mut rounds = 0;
        let mut usage = Usage::default();
        let undo = |conversation: &mut Conversation, checkpoint: &mut Option<Checkpoint>| {
            if let Some(checkpoint) = checkpoint.take() {
                conversation.restore(checkpoint);
            }
        };
        let reason = loop {
            if cancel.is_cancelled() {
                undo(conversation, &mut checkpoint);
                break DoneReason::Cancelled;
            }
            let turn = match self.stream_turn(conversation, session, on_event, cancel) {
                Ok(turn) => turn,
                Err(ProviderError::Cancelled) => {
                    undo(conversation, &mut checkpoint);
                    break DoneReason::Cancelled;
                }
                Err(error) => {
                    undo(conversation, &mut checkpoint);
                    return Err(error.into());
                }
            };
            usage += turn.usage;
            on_event(AssistantEvent::Usage(turn.usage));
            if turn.thinking_dropped > 0 && conversation.first_drop_report() {
                on_event(AssistantEvent::Notice(Notice::ThinkingBlocksDropped {
                    count: turn.thinking_dropped,
                }));
            }
            if turn.served_by_fallback {
                // The last fallback block names the model that continued; the response's own
                // `model` is what served the message.
                on_event(AssistantEvent::Notice(Notice::FallbackServed {
                    model: turn.fallback_to.clone().or_else(|| turn.model.clone()),
                }));
            }
            if let StopReason::Refusal { category } = &turn.stop_reason {
                let category = category.clone();
                on_event(AssistantEvent::Notice(Notice::Refused {
                    category: category.clone(),
                }));
                undo(conversation, &mut checkpoint);
                break DoneReason::Refused { category };
            }
            let TurnResponse {
                content,
                stop_reason: stop,
                invalid_tool_inputs,
                ..
            } = turn;
            let view = Content(&content);
            for skipped in view.skipped_tool_uses() {
                on_event(AssistantEvent::ToolCallSkipped {
                    id: skipped.id,
                    name: skipped.name,
                });
            }
            if !view.has_substance() {
                // Nothing to keep: a turn of only thinking, or no content at all, would come
                // back as an empty assistant message, which the API refuses.
                if stop == StopReason::MaxTokens {
                    on_event(AssistantEvent::Notice(Notice::CutOff { tool_calls: 0 }));
                }
                let user_message_taken_back = checkpoint.is_some();
                undo(conversation, &mut checkpoint);
                break DoneReason::EmptyTurn {
                    user_message_taken_back,
                };
            }
            let calls: Vec<ToolUse<'_>> = view.tool_uses().collect();
            let call_count = calls.len();
            let results = (call_count > 0)
                .then(|| self.answer_calls(&calls, &stop, &invalid_tool_inputs, on_event, cancel));
            match results {
                Some(results) => conversation.push_tool_round(content, results),
                None => conversation.push_assistant(content),
            }
            checkpoint = None;
            if cancel.is_cancelled() {
                break DoneReason::Cancelled;
            }
            let more = match (&stop, call_count > 0) {
                (StopReason::ToolUse, true) => true,
                (StopReason::MaxTokens, has_calls) => {
                    on_event(AssistantEvent::Notice(Notice::CutOff {
                        tool_calls: call_count,
                    }));
                    has_calls
                }
                _ => false,
            };
            if !more {
                break stop.into();
            }
            rounds += 1;
            if rounds >= self.config.max_rounds {
                on_event(AssistantEvent::Notice(Notice::RoundLimit { rounds }));
                break DoneReason::RoundLimit;
            }
        };
        on_event(AssistantEvent::Done(reason.clone()));
        Ok(RunSummary {
            reason,
            rounds,
            usage,
        })
    }

    /// One model turn. When the API refuses the history in its usual form, the conversation
    /// switches to the documented alternative, remembers it, and the turn is sent once more:
    /// the thinking blocks of the history that no longer match the conversation are left out (a
    /// boundary: the thinking of later turns stays), and operator notes ride in the user turn for
    /// a model without `system` messages.
    fn stream_turn(
        &self,
        conversation: &mut Conversation,
        session: &RunSession,
        on_event: &mut dyn FnMut(AssistantEvent<'_>),
        cancel: &CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        loop {
            let result = {
                let request = TurnRequest {
                    model: &self.config.model,
                    effort: conversation.effort().unwrap_or(self.config.effort),
                    system: SystemPrompt {
                        core: CORE_PROMPT,
                        context: conversation.context().map(|context| ContextBlock {
                            text: &context.text,
                            cacheable: context.has_schema,
                        }),
                    },
                    tools: self.tools.tools(),
                    messages: conversation.messages(),
                    history: conversation.form(),
                };
                self.provider.stream_turn_in_run(
                    session,
                    &request,
                    &mut |event| match event {
                        StreamEvent::TextDelta(text) => on_event(AssistantEvent::TextDelta(text)),
                        StreamEvent::ThinkingDelta(text) => {
                            on_event(AssistantEvent::ThinkingDelta(text));
                        }
                        StreamEvent::ToolCallStarted { id, name } => {
                            on_event(AssistantEvent::ToolCallStarted { id, name });
                        }
                        StreamEvent::ToolInputReady {
                            id,
                            name,
                            input,
                            valid,
                        } => on_event(AssistantEvent::ToolCallInputReady {
                            id,
                            name,
                            input,
                            valid,
                        }),
                        StreamEvent::Retrying {
                            attempt,
                            after,
                            cause,
                        } => on_event(AssistantEvent::Notice(Notice::Retrying {
                            attempt,
                            after,
                            cause,
                        })),
                        StreamEvent::FeatureDropped(feature) => {
                            on_event(AssistantEvent::Notice(Notice::FeatureDropped(feature)));
                        }
                    },
                    cancel,
                )
            };
            let form = conversation.form();
            let has_note = conversation
                .messages()
                .iter()
                .any(|message| matches!(message, Message::System { .. }));
            // A strip covers the assistant messages that exist now. It is a one-time recovery: a
            // second refusal with nothing new to strip is the caller's error, not a loop.
            let strip_to = conversation.thinking_strip_boundary();
            match &result {
                Err(error)
                    if error.is_thinking_binding_mismatch()
                        && strip_to > form.drop_thinking_before =>
                {
                    conversation.set_form(HistoryForm {
                        drop_thinking_before: strip_to,
                        ..form
                    });
                    on_event(AssistantEvent::Notice(Notice::ThinkingDropped));
                }
                Err(error)
                    if error.is_system_role_unsupported()
                        && has_note
                        && !form.notes_in_user_turn =>
                {
                    conversation.set_form(HistoryForm {
                        notes_in_user_turn: true,
                        ..form
                    });
                    on_event(AssistantEvent::Notice(Notice::NotesInUserTurn));
                }
                _ => return result,
            }
        }
    }

    /// One result per call, in the order of the calls, whatever happens. Calls past
    /// [`MAX_TOOL_CALLS_PER_TURN`] are answered with an error saying they were not run.
    fn answer_calls(
        &self,
        calls: &[ToolUse<'_>],
        stop: &StopReason,
        invalid: &[InvalidToolInput],
        on_event: &mut dyn FnMut(AssistantEvent<'_>),
        cancel: &CancelToken,
    ) -> Vec<UserBlock> {
        calls
            .iter()
            .enumerate()
            .map(|(position, call)| {
                let answer = if position >= MAX_TOOL_CALLS_PER_TURN {
                    Answer::error(
                        "assistant.too_many_calls",
                        &format!(
                            "A turn may make {MAX_TOOL_CALLS_PER_TURN} tool calls; this one was \
                             not run. Ask for it again in the next step."
                        ),
                    )
                } else {
                    self.answer(call, stop, invalid, cancel)
                };
                on_event(AssistantEvent::ToolResult {
                    id: call.id,
                    name: call.name,
                    is_error: answer.is_error,
                    summary: &answer.summary,
                });
                UserBlock::ToolResult {
                    tool_use_id: call.id.to_owned(),
                    content: answer.content,
                    is_error: answer.is_error,
                }
            })
            .collect()
    }

    fn answer(
        &self,
        call: &ToolUse<'_>,
        stop: &StopReason,
        invalid: &[InvalidToolInput],
        cancel: &CancelToken,
    ) -> Answer {
        match stop {
            StopReason::ToolUse => {}
            StopReason::MaxTokens => {
                return Answer::error(
                    "assistant.cut_off",
                    "The response was cut off by the output limit, so this tool call was not run. \
                     Try again with a smaller step.",
                )
            }
            _ => {
                return Answer::error(
                    "assistant.not_run",
                    "The turn ended before this tool call could be run, so it was not run.",
                )
            }
        }
        if let Some(bad) = invalid.iter().find(|bad| bad.tool_use_id == call.id) {
            // Built with the JSON library so quotes in the bad input are escaped; capped, because
            // it stays in the conversation.
            let mut raw: String = bad.raw.chars().take(MAX_INVALID_JSON_CHARS).collect();
            if raw.len() < bad.raw.len() {
                raw.push_str(" [cut]");
            }
            let content = json!({ "INVALID_JSON": raw }).to_string();
            return Answer {
                content,
                is_error: true,
                summary: "the input was not valid JSON; not run".to_owned(),
            };
        }
        if cancel.is_cancelled() {
            return Answer::error("agent.cancelled", "Cancelled before this tool call ran.");
        }
        match self.call(cancel, call.name, call.input.clone()) {
            Ok(outcome) => {
                let content = outcome.to_text();
                let summary = match &outcome {
                    ToolOutcome::Success(_) => format!("ok, {} bytes", content.len()),
                    ToolOutcome::Failure(error) => {
                        clean(&format!("{}: {}", error.code, error.message), SUMMARY_CHARS)
                    }
                };
                Answer {
                    content,
                    is_error: outcome.is_error(),
                    summary,
                }
            }
            Err(panicked) => Answer::error(&panicked.code, &panicked.message),
        }
    }
}
