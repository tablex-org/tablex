//! Turns the chunks of a Chat Completions stream into one assistant turn.
//!
//! Each SSE `data:` is one JSON chunk (`choices[0].delta` with `content` or `tool_calls`, a
//! `finish_reason`, and, with `stream_options.include_usage`, a last chunk with `usage` and no
//! choices) or `[DONE]`. Servers that speak the protocol loosely are read leniently: a field of the
//! wrong type reads as absent, a chunk of the wrong shape is skipped, and the text, the reasoning
//! and the tool arguments may come in the shapes proxies use (see the helpers below).
//!
//! A turn is bounded by the shared [`TurnLimits`]: bytes (in the pump), tool calls, and
//! consecutive chunks that carry nothing. Server text that ends up in an event or a result (ids,
//! names, model, finish reason, the explanation of an unusable call) is scrubbed of the API key
//! and cut like a model name.

use crate::provider::{
    InvalidToolInput, ProtocolError, StopReason, StreamEvent, TurnResponse, Usage,
};
use crate::wire::errors::{Scrub, MAX_TOKEN_CHARS};
use crate::wire::{StreamError, TurnLimits};
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The name stored, and reported, for a tool call whose name cannot be used.
pub(super) const PLACEHOLDER_NAME: &str = "invalid_tool_name";

/// The largest integer a float holds exactly.
const MAX_FLOAT_COUNT: f64 = 9_007_199_254_740_992.0;

/// A tool call under assembly.
struct Call {
    index: u64,
    /// The id the server gave it, if it did.
    server_id: Option<String>,
    /// The id that is stored and reported.
    id: Option<String>,
    name: String,
    arguments: String,
    started: bool,
}

pub(super) struct TurnAssembler<'a> {
    limits: TurnLimits,
    scrub: &'a Scrub,
    /// Ids of the tool calls of the history: no new call may take one.
    taken: &'a HashSet<&'a str>,
    model: Option<String>,
    usage: Option<Usage>,
    reasoning: String,
    text: String,
    calls: Vec<Call>,
    finish: Option<String>,
    /// Something of the turn reached the caller: an error now is final.
    delivered: bool,
    idle: usize,
}

impl<'a> TurnAssembler<'a> {
    pub fn new(limits: TurnLimits, scrub: &'a Scrub, taken: &'a HashSet<&'a str>) -> Self {
        Self {
            limits,
            scrub,
            taken,
            model: None,
            usage: None,
            reasoning: String::new(),
            text: String::new(),
            calls: Vec::new(),
            finish: None,
            delivered: false,
            idle: 0,
        }
    }

    /// A finish reason was seen: the turn is complete even if the body does not say `[DONE]`.
    pub fn has_finish(&self) -> bool {
        self.finish.is_some()
    }

    fn idle_event(&mut self) -> Result<bool, StreamError> {
        self.idle += 1;
        if self.idle > self.limits.max_idle_events {
            return Err(ProtocolError::TooManyIdleEvents {
                limit: self.limits.max_idle_events,
            }
            .into());
        }
        Ok(false)
    }

    /// Handles the `data` of one event; `Ok(true)` at `[DONE]`.
    pub fn handle(
        &mut self,
        data: &str,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<bool, StreamError> {
        let data = data.trim();
        if data == "[DONE]" {
            return Ok(true);
        }
        let Ok(Value::Object(event)) = serde_json::from_str::<Value>(data) else {
            return self.idle_event();
        };
        if let Some((kind, message)) = event.get("error").and_then(error_payload) {
            return Err(StreamError::Api {
                kind,
                message,
                before_start: !self.delivered,
            });
        }
        let Some(chunk) = Chunk::of(&event) else {
            return self.idle_event();
        };
        if self.model.is_none() {
            if let Some(model) = event
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            {
                self.model = Some(model.to_owned());
            }
        }
        let mut progress = false;
        if let Some(usage) = usage_of(&event) {
            self.usage = Some(usage);
            progress = true;
        }
        if let Some(text) = chunk.reasoning() {
            emit(StreamEvent::ThinkingDelta(text));
            self.reasoning.push_str(text);
            progress = true;
        }
        if let Some(text) = chunk.text(&event) {
            emit(StreamEvent::TextDelta(&text));
            self.text.push_str(&text);
            progress = true;
        }
        for entry in chunk.tool_calls {
            progress |= self.tool_call_entry(entry, emit)?;
        }
        if let Some(reason) = chunk.finish_reason() {
            if self.finish.is_none() {
                self.finish = Some(self.scrub.bounded(reason, MAX_TOKEN_CHARS));
            }
            progress = true;
        }
        if progress {
            self.idle = 0;
            self.delivered = true;
            Ok(false)
        } else {
            self.idle_event()
        }
    }

    /// One entry of `delta.tool_calls`; true when it carried something.
    fn tool_call_entry(
        &mut self,
        entry: &Value,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<bool, StreamError> {
        let Some(entry) = entry.as_object() else {
            return Ok(false);
        };
        let index = entry.get("index").and_then(count).unwrap_or(0);
        let id = non_empty(entry.get("id"));
        let function = entry.get("function").and_then(Value::as_object);
        let name = non_empty(function.and_then(|function| function.get("name")));
        let arguments = function
            .and_then(|function| function.get("arguments"))
            .and_then(arguments_text);
        if id.is_none() && name.is_none() && arguments.is_none() {
            return Ok(false);
        }
        let position = match self.find(index, id) {
            Some(position) => position,
            None => {
                if self.calls.len() >= self.limits.max_blocks {
                    return Err(ProtocolError::TooManyBlocks {
                        limit: self.limits.max_blocks,
                    }
                    .into());
                }
                self.calls.push(Call {
                    index,
                    server_id: None,
                    id: None,
                    name: String::new(),
                    arguments: String::new(),
                    started: false,
                });
                self.calls.len() - 1
            }
        };
        if let Some(id) = id {
            if self.calls[position].server_id.is_none() {
                let stored = self.stored_id(id);
                let call = &mut self.calls[position];
                call.server_id = Some(id.to_owned());
                call.id = Some(stored);
            }
        }
        let call = &mut self.calls[position];
        if let Some(name) = name {
            name.clone_into(&mut call.name);
        }
        if let Some(arguments) = arguments {
            call.arguments.push_str(&arguments);
        }
        if let (false, Some(id), false) = (call.started, &call.id, call.name.is_empty()) {
            call.started = true;
            emit(StreamEvent::ToolCallStarted {
                id,
                name: reported_name(self.scrub, &call.name),
            });
        }
        Ok(true)
    }

    /// The call a delta belongs to. With an id: the latest call of that index with that server id,
    /// else the latest of the index that has none yet (a new id on a used index is a new call:
    /// some servers number every call 0). Without one: the latest call of the index.
    fn find(&self, index: u64, id: Option<&str>) -> Option<usize> {
        let latest = |matches: &dyn Fn(&Call) -> bool| {
            self.calls
                .iter()
                .rposition(|call| call.index == index && matches(call))
        };
        match id {
            Some(id) => latest(&|call| call.server_id.as_deref() == Some(id))
                .or_else(|| latest(&|call| call.server_id.is_none())),
            None => latest(&|_| true),
        }
    }

    fn is_taken(&self, id: &str) -> bool {
        self.taken.contains(id) || self.calls.iter().any(|call| call.id.as_deref() == Some(id))
    }

    /// The id to store for a call the server named `server_id`: that one, when it can be stored
    /// and no other call has it, else a generated one.
    fn stored_id(&self, server_id: &str) -> String {
        if is_storable(self.scrub, server_id) && !self.is_taken(server_id) {
            server_id.to_owned()
        } else {
            generated_id(|id| self.is_taken(id))
        }
    }

    fn stop_reason(&self) -> StopReason {
        match self.finish.as_deref() {
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal { category: None },
            // Whatever the reason says, a turn with calls is a tool turn.
            _ if !self.calls.is_empty() => StopReason::ToolUse,
            None | Some("stop") => StopReason::EndTurn,
            Some("tool_calls" | "function_call") => StopReason::ToolUse,
            Some(other) => StopReason::Other(other.to_owned()),
        }
    }

    /// The turn: reasoning, text, then the tool calls, each reported complete.
    pub fn finish(
        mut self,
        request_id: Option<String>,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> TurnResponse {
        let stop_reason = self.stop_reason();
        let mut content = Vec::new();
        if !self.reasoning.trim().is_empty() {
            content.push(json!({"type": "reasoning", "text": self.reasoning}));
        }
        if !self.text.trim().is_empty() {
            content.push(json!({"type": "text", "text": self.text}));
        }
        let mut invalid = Vec::new();
        for position in 0..self.calls.len() {
            if self.calls[position].id.is_none() {
                let id = generated_id(|id| self.is_taken(id));
                self.calls[position].id = Some(id);
            }
            let call = &self.calls[position];
            let id = call.id.as_deref().unwrap_or_default();
            let usable = is_storable(self.scrub, &call.name);
            let name = if usable { call.name.as_str() } else { PLACEHOLDER_NAME };
            if !call.started {
                emit(StreamEvent::ToolCallStarted { id, name });
            }
            let (input, valid) = if !usable {
                invalid.push(InvalidToolInput {
                    tool_use_id: id.to_owned(),
                    raw: unusable_name(self.scrub, &call.name),
                });
                (json!({}), false)
            } else {
                match parse_arguments(&call.arguments) {
                    Some(input) => (input, true),
                    None => {
                        invalid.push(InvalidToolInput {
                            tool_use_id: id.to_owned(),
                            raw: self.scrub.bounded(&call.arguments, usize::MAX),
                        });
                        (json!({}), false)
                    }
                }
            };
            emit(StreamEvent::ToolInputReady {
                id,
                name,
                input: &input,
                valid,
            });
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
        TurnResponse {
            content,
            stop_reason,
            usage: self.usage.unwrap_or_default(),
            model: self
                .model
                .map(|model| self.scrub.bounded(&model, MAX_TOKEN_CHARS)),
            served_by_fallback: false,
            fallback_to: None,
            invalid_tool_inputs: invalid,
            request_id,
            thinking_dropped: 0,
        }
    }
}

/// The parts of a chunk the assembler reads, once its shape is known to be right.
struct Chunk<'v> {
    choice: Option<&'v Map<String, Value>>,
    delta: Option<&'v Map<String, Value>>,
    tool_calls: &'v [Value],
}

impl<'v> Chunk<'v> {
    /// `None` when `choices`, the first choice, its `delta` or `delta.tool_calls` has the wrong
    /// kind of value.
    fn of(event: &'v Map<String, Value>) -> Option<Self> {
        let choice = match event.get("choices") {
            None | Some(Value::Null) => None,
            // Only the first choice is used (`n` is never asked for).
            Some(Value::Array(choices)) => match choices.first() {
                None => None,
                Some(Value::Object(choice)) => Some(choice),
                Some(_) => return None,
            },
            Some(_) => return None,
        };
        let delta = match choice.and_then(|choice| choice.get("delta")) {
            None | Some(Value::Null) => None,
            Some(Value::Object(delta)) => Some(delta),
            Some(_) => return None,
        };
        let tool_calls = match delta.and_then(|delta| delta.get("tool_calls")) {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(calls)) => calls,
            Some(_) => return None,
        };
        Some(Self {
            choice,
            delta,
            tool_calls,
        })
    }

    /// The first non-empty of `reasoning_content`, `reasoning` and `thinking`.
    fn reasoning(&self) -> Option<&'v str> {
        let delta = self.delta?;
        ["reasoning_content", "reasoning", "thinking"]
            .iter()
            .find_map(|key| non_empty(delta.get(*key)))
    }

    /// The first non-empty text among the places servers put it.
    fn text(&self, event: &'v Map<String, Value>) -> Option<Cow<'v, str>> {
        let choice = self.choice;
        [
            self.delta.and_then(|delta| delta.get("content")),
            choice
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content")),
            choice.and_then(|choice| choice.get("text")),
            event.get("content"),
            event.get("delta"),
        ]
        .into_iter()
        .flatten()
        .find_map(content_text)
    }

    fn finish_reason(&self) -> Option<&'v str> {
        non_empty(self.choice?.get("finish_reason"))
    }
}

/// A non-empty string value; anything else reads as absent.
fn non_empty(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|text| !text.is_empty())
}

/// Text from a string, or from a list of parts (the first string of `text`, `content`,
/// `input_text` and `output_text` of each, joined). Empty is absent.
fn content_text(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::String(text) if !text.is_empty() => Some(Cow::Borrowed(text)),
        Value::Array(parts) => {
            let joined: String = parts
                .iter()
                .filter_map(|part| {
                    ["text", "content", "input_text", "output_text"]
                        .iter()
                        .find_map(|key| part.get(key).and_then(Value::as_str))
                })
                .collect();
            (!joined.is_empty()).then_some(Cow::Owned(joined))
        }
        _ => None,
    }
}

/// A count: a non-negative integer, or a float that is one (up to 2^53).
fn count(value: &Value) -> Option<u64> {
    let Value::Number(number) = value else {
        return None;
    };
    if let Some(count) = number.as_u64() {
        return Some(count);
    }
    let float = number.as_f64()?;
    (float >= 0.0 && float.fract() == 0.0 && float <= MAX_FLOAT_COUNT).then_some(float as u64)
}

/// `function.arguments`: the text of a string, or a JSON object taken as its compact text.
fn arguments_text(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::String(text) if !text.is_empty() => Some(Cow::Borrowed(text)),
        Value::Object(_) => Some(Cow::Owned(value.to_string())),
        _ => None,
    }
}

/// `usage`, when both `prompt_tokens` and `completion_tokens` are counts. The prompt tokens the
/// server served from its cache are counted apart from the others.
fn usage_of(event: &Map<String, Value>) -> Option<Usage> {
    let usage = event.get("usage")?.as_object()?;
    let prompt = count(usage.get("prompt_tokens")?)?;
    let completion = count(usage.get("completion_tokens")?)?;
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(count)
        .unwrap_or(0);
    Some(Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: completion,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
    })
}

/// The error of a chunk, as the `(kind, message)` of an Anthropic-style error event: `error` is an
/// object (`message`, `type`, `code`) or, from Ollama, a bare string. `null` and the rest are no
/// error.
fn error_payload(value: &Value) -> Option<(String, String)> {
    match value {
        Value::String(message) => Some((String::new(), message.clone())),
        Value::Object(error) => {
            let field = |key: &str| error.get(key).and_then(Value::as_str).unwrap_or("");
            let kind = match (field("type"), field("code")) {
                ("invalid_request_error", _) | (_, "context_length_exceeded") => {
                    "invalid_request_error"
                }
                ("authentication_error", _) | (_, "invalid_api_key") => "authentication_error",
                ("rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota", _)
                | (_, "rate_limit_exceeded" | "insufficient_quota") => "rate_limit_error",
                ("server_error" | "api_error", _) => "api_error",
                ("overloaded_error" | "engine_overloaded_error", _) => "overloaded_error",
                (other, _) => other,
            };
            Some((kind.to_owned(), field("message").to_owned()))
        }
        _ => None,
    }
}

/// 1 to 64 letters, digits, `_` or `-`, and nothing the scrub would change (the key).
fn is_storable(scrub: &Scrub, text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && scrub.bounded(text, MAX_TOKEN_CHARS) == text
}

fn reported_name<'n>(scrub: &Scrub, name: &'n str) -> &'n str {
    if is_storable(scrub, name) {
        name
    } else {
        PLACEHOLDER_NAME
    }
}

fn unusable_name(scrub: &Scrub, name: &str) -> String {
    let shown = if name.is_empty() {
        "(none)".to_owned()
    } else {
        format!("\"{}\"", scrub.bounded(name, MAX_TOKEN_CHARS))
    };
    format!(
        "the tool name {shown} cannot be used (a name has 1 to 64 letters, digits, _ or -, and must be one of the tools offered)"
    )
}

/// The arguments of a call as its input: empty is `{}`, a JSON object is that object; anything else
/// (cut-off JSON, an array, a scalar) is `None`.
fn parse_arguments(arguments: &str) -> Option<Value> {
    if arguments.trim().is_empty() {
        return Some(json!({}));
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(input @ Value::Object(_)) => Some(input),
        _ => None,
    }
}

/// `call_<8 hex digits, fixed for the process>_<counter>`, skipping what `taken` says is in use.
/// The counter is process-wide, so ids do not repeat across turns or providers.
pub(super) fn generated_id(taken: impl Fn(&str) -> bool) -> String {
    static PREFIX: OnceLock<u32> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let prefix = *PREFIX.get_or_init(|| {
        // `RandomState` is seeded by the operating system, once per process.
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish() as u32
    });
    loop {
        let id = format!("call_{prefix:08x}_{}", COUNTER.fetch_add(1, Ordering::Relaxed));
        if !taken(&id) {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::ApiKey;
    use crate::wire::errors::Scrub;

    const KEY: &str = "sk-test-SECRETKEY-12345";

    struct Run {
        events: Vec<String>,
        result: Result<(bool, Option<TurnResponse>), StreamError>,
    }

    impl Run {
        fn turn(&self) -> &TurnResponse {
            match &self.result {
                Ok((_, Some(turn))) => turn,
                other => panic!("no turn: {:?}", other.as_ref().map(|(done, _)| done)),
            }
        }

        fn content(&self) -> Vec<Value> {
            self.turn().content.clone()
        }
    }

    /// Feeds `chunks` (JSON or raw text) through an assembler and finishes it.
    fn run_with(chunks: &[&str], limits: TurnLimits, taken: &[&str]) -> Run {
        let scrub = Scrub::new(ApiKey::new(KEY));
        let taken: HashSet<&str> = taken.iter().copied().collect();
        let mut assembler = TurnAssembler::new(limits, &scrub, &taken);
        let mut events = Vec::new();
        let mut done = false;
        for chunk in chunks {
            match assembler.handle(chunk, &mut |event| events.push(format!("{event:?}"))) {
                Ok(true) => {
                    done = true;
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    return Run {
                        events,
                        result: Err(error),
                    }
                }
            }
        }
        let turn = assembler.finish(Some("req_1".into()), &mut |event| {
            events.push(format!("{event:?}"))
        });
        Run {
            events,
            result: Ok((done, Some(turn))),
        }
    }

    fn run(chunks: &[&str]) -> Run {
        run_with(chunks, TurnLimits::default(), &[])
    }

    fn error_of(run: &Run) -> &StreamError {
        run.result.as_ref().expect_err("the stream fails")
    }

    fn call(index: u64, id: &str, name: &str, args: &str) -> String {
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": index, "id": id, "type": "function",
            "function": {"name": name, "arguments": args}}]}}]})
        .to_string()
    }

    fn finish(reason: &str) -> String {
        json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
    }

    fn text(piece: &str) -> String {
        json!({"choices": [{"delta": {"content": piece}}]}).to_string()
    }

    #[test]
    fn text_deltas_are_emitted_and_joined() {
        let run = run(&[&text("Hel"), &text("lo "), &text("world"), &finish("stop"), "[DONE]"]);
        assert_eq!(
            run.events,
            ["TextDelta(\"Hel\")", "TextDelta(\"lo \")", "TextDelta(\"world\")"]
        );
        assert_eq!(run.content(), [json!({"type": "text", "text": "Hello world"})]);
        assert_eq!(run.turn().stop_reason, StopReason::EndTurn);
        assert_eq!(run.turn().request_id.as_deref(), Some("req_1"));
        assert!(!run.turn().served_by_fallback);
        assert_eq!(run.turn().thinking_dropped, 0);
    }

    #[test]
    fn a_done_without_a_finish_reason_is_an_end_of_turn() {
        let run = run(&[&text("x"), "[DONE]"]);
        assert!(matches!(run.result, Ok((true, _))));
        assert_eq!(run.turn().stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn text_is_stored_untrimmed_and_blank_text_is_left_out() {
        let kept = run(&[&text("  a  "), &finish("stop")]);
        assert_eq!(kept.content(), [json!({"type": "text", "text": "  a  "})]);
        let blank = run(&[&text("  \n"), &finish("stop")]);
        assert!(blank.content().is_empty());
    }

    #[test]
    fn reasoning_comes_first_from_the_first_non_empty_field() {
        let chunk = |field: &str, value: &str| {
            json!({"choices": [{"delta": {field: value, "content": ""}}]}).to_string()
        };
        let run = run(&[
            &chunk("reasoning_content", "a"),
            &chunk("reasoning", "b"),
            &chunk("thinking", "c"),
            &json!({"choices": [{"delta": {"reasoning_content": "", "reasoning": "d",
                                           "thinking": "e"}}]})
            .to_string(),
            &text("answer"),
            &finish("stop"),
        ]);
        assert_eq!(
            run.events[..4],
            ["ThinkingDelta(\"a\")", "ThinkingDelta(\"b\")", "ThinkingDelta(\"c\")", "ThinkingDelta(\"d\")"]
        );
        assert_eq!(
            run.content(),
            [
                json!({"type": "reasoning", "text": "abcd"}),
                json!({"type": "text", "text": "answer"})
            ]
        );
    }

    #[test]
    fn blank_reasoning_is_not_stored() {
        let run = run(&[
            &json!({"choices": [{"delta": {"reasoning": " \n"}}]}).to_string(),
            &finish("stop"),
        ]);
        assert!(run.content().is_empty());
    }

    #[test]
    fn text_is_found_where_servers_put_it() {
        let chunks = [
            json!({"choices": [{"delta": {"content": "a"}}]}),
            json!({"choices": [{"message": {"content": "b"}}]}),
            json!({"choices": [{"text": "c"}]}),
            json!({"content": "d"}),
            json!({"delta": "e"}),
            json!({"choices": [{"delta": {"content": [
                {"type": "text", "text": "f"}, {"content": "g"},
                {"input_text": "h"}, {"output_text": "i"}, {"other": "x"}, 7]}}]}),
            // The first non-empty place wins: no double text.
            json!({"choices": [{"delta": {"content": "j"}, "text": "XX"}], "content": "YY"}),
        ]
        .map(|chunk| chunk.to_string());
        let stop = finish("stop");
        let mut refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        refs.push(&stop);
        let run = run(&refs);
        assert_eq!(run.content(), [json!({"type": "text", "text": "abcdefghij"})]);
    }

    #[test]
    fn a_field_of_the_wrong_type_reads_as_absent() {
        let run = run(&[
            &json!({"choices": [{"delta": {"content": 5, "reasoning": true, "thinking": {"a": 1}},
                                 "finish_reason": 3}], "model": 7}).to_string(),
            &text("ok"),
            &finish("stop"),
        ]);
        assert_eq!(run.content(), [json!({"type": "text", "text": "ok"})]);
        assert_eq!(run.turn().model, None);
    }

    #[test]
    fn a_chunk_of_the_wrong_shape_is_skipped_and_counts_as_idle() {
        let limits = TurnLimits {
            max_idle_events: 3,
            ..TurnLimits::default()
        };
        let skipped = [
            "not json",
            "{",
            "[1,2]",
            "7",
            r#"{"choices": "x"}"#,
            r#"{"choices": {"a": 1}}"#,
            r#"{"choices": ["x"]}"#,
            r#"{"choices": [{"delta": "x"}]}"#,
            r#"{"choices": [{"delta": {"tool_calls": {"a": 1}}}]}"#,
        ];
        for chunk in skipped {
            let run = run_with(&[chunk, &text("ok"), &finish("stop")], limits, &[]);
            assert_eq!(run.content(), [json!({"type": "text", "text": "ok"})], "{chunk}");
        }
    }

    #[test]
    fn too_many_idle_chunks_in_a_row_fail_and_progress_resets_the_count() {
        let limits = TurnLimits {
            max_idle_events: 3,
            ..TurnLimits::default()
        };
        let idle = r#"{"choices": [{"delta": {"role": "assistant"}}]}"#;
        let ok = run_with(&[idle, idle, idle, &text("a"), idle, idle, idle, &finish("stop")], limits, &[]);
        assert!(ok.result.is_ok());
        let failed = run_with(&[idle, idle, "junk", idle, &text("late")], limits, &[]);
        assert!(matches!(
            error_of(&failed),
            StreamError::Protocol(ProtocolError::TooManyIdleEvents { limit: 3 })
        ));
    }

    #[test]
    fn a_model_alone_is_not_progress_and_the_first_one_is_kept_scrubbed() {
        let limits = TurnLimits {
            max_idle_events: 1,
            ..TurnLimits::default()
        };
        let named = |model: &str| json!({"model": model, "choices": []}).to_string();
        let failed = run_with(&[&named("m"), &named("m"), &text("x")], limits, &[]);
        assert!(matches!(
            error_of(&failed),
            StreamError::Protocol(ProtocolError::TooManyIdleEvents { .. })
        ));
        let run = run(&[&named(""), &named(&format!("gpt-{KEY}-x")), &named("later"), &finish("stop")]);
        let model = run.turn().model.clone().unwrap();
        assert_eq!(model, "gpt-***-x");
    }

    #[test]
    fn usage_needs_both_counts_and_the_last_one_wins() {
        let usage = |value: Value| json!({"choices": [], "usage": value}).to_string();
        let run = run(&[
            &text("x"),
            &finish("stop"),
            &usage(json!({"prompt_tokens": 10, "completion_tokens": 2})),
            &usage(json!({"prompt_tokens": 100, "completion_tokens": 5,
                          "prompt_tokens_details": {"cached_tokens": 40}})),
            // Not both counts: ignored.
            &usage(json!({"prompt_tokens": 1})),
            &usage(json!({"prompt_tokens": "9", "completion_tokens": 1})),
            &usage(json!({"prompt_tokens": -3, "completion_tokens": 1})),
        ]);
        assert_eq!(
            run.turn().usage,
            Usage {
                input_tokens: 60,
                output_tokens: 5,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 0,
            }
        );
    }

    #[test]
    fn counts_accept_integer_floats_and_a_cache_larger_than_the_prompt_saturates() {
        let run = run(&[
            &text("x"),
            &json!({"usage": {"prompt_tokens": 12.0, "completion_tokens": 3.0,
                              "prompt_tokens_details": {"cached_tokens": 20.0}}}).to_string(),
            &finish("stop"),
        ]);
        assert_eq!(run.turn().usage.input_tokens, 0);
        assert_eq!(run.turn().usage.cache_read_input_tokens, 20);
        assert_eq!(run.turn().usage.output_tokens, 3);
        // A fractional cached count is absent for that field only.
        let run = run_with(
            &[
                &json!({"usage": {"prompt_tokens": 12, "completion_tokens": 3,
                                  "prompt_tokens_details": {"cached_tokens": 1.5}}}).to_string(),
                &finish("stop"),
            ],
            TurnLimits::default(),
            &[],
        );
        assert_eq!(run.turn().usage.input_tokens, 12);
        assert_eq!(run.turn().usage.cache_read_input_tokens, 0);
        assert_eq!(count(&json!(9_007_199_254_740_992.0)), Some(9_007_199_254_740_992));
        assert_eq!(count(&json!(1.0e300)), None);
        assert_eq!(count(&json!(-0.5)), None);
        assert_eq!(count(&json!("3")), None);
    }

    #[test]
    fn only_the_first_choice_is_used() {
        let run = run(&[
            &json!({"choices": [{"delta": {"content": "one"}}, {"delta": {"content": "two"}}]})
                .to_string(),
            &finish("stop"),
        ]);
        assert_eq!(run.content(), [json!({"type": "text", "text": "one"})]);
    }

    #[test]
    fn the_stop_reason_follows_the_finish_reason_and_the_calls() {
        let reason = |reason: &str, with_call: bool| {
            let mut chunks = vec![text("x")];
            if with_call {
                chunks.push(call(0, "c1", "list_tables", "{}"));
            }
            chunks.push(finish(reason));
            let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
            run(&refs).turn().stop_reason.clone()
        };
        assert_eq!(reason("stop", false), StopReason::EndTurn);
        assert_eq!(reason("length", false), StopReason::MaxTokens);
        assert_eq!(reason("length", true), StopReason::MaxTokens);
        assert_eq!(reason("content_filter", false), StopReason::Refusal { category: None });
        assert_eq!(reason("tool_calls", false), StopReason::ToolUse);
        assert_eq!(reason("function_call", false), StopReason::ToolUse);
        assert_eq!(reason("weird", false), StopReason::Other("weird".into()));
        for word in ["stop", "eos", "end_turn", "tool_calls", "weird"] {
            assert_eq!(reason(word, true), StopReason::ToolUse, "{word}");
        }
    }

    #[test]
    fn a_tool_call_arrives_in_pieces_and_is_reported_once_started_and_once_ready() {
        let first = json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call_abc", "type": "function",
            "function": {"name": "list_tables", "arguments": ""}}]}}]}).to_string();
        let piece = |args: &str| {
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": args}}]}}]})
                .to_string()
        };
        let run = run(&[&first, &piece("{\"connec"), &piece("tion_id\":\"c1\"}"), &finish("tool_calls"), "[DONE]"]);
        assert_eq!(
            run.events,
            [
                "ToolCallStarted { id: \"call_abc\", name: \"list_tables\" }",
                "ToolInputReady { id: \"call_abc\", name: \"list_tables\", input: Object {\"connection_id\": String(\"c1\")}, valid: true }"
            ]
        );
        assert_eq!(
            run.content(),
            [json!({"type": "tool_use", "id": "call_abc", "name": "list_tables",
                    "input": {"connection_id": "c1"}})]
        );
        assert_eq!(run.turn().stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn a_name_that_comes_later_is_waited_for() {
        let run = run(&[
            &call(0, "c1", "", ""),
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "list_tables"}}]}}]})
                .to_string(),
            &finish("tool_calls"),
        ]);
        assert!(run.events[0].starts_with("ToolCallStarted { id: \"c1\", name: \"list_tables\""));
        assert_eq!(run.events.iter().filter(|e| e.starts_with("ToolCallStarted")).count(), 1);
    }

    #[test]
    fn an_entry_with_nothing_in_it_is_ignored_and_is_not_progress() {
        let limits = TurnLimits {
            max_idle_events: 1,
            ..TurnLimits::default()
        };
        let empty = call(0, "", "", "");
        let failed = run_with(&[&empty, &empty, &text("x")], limits, &[]);
        assert!(matches!(
            error_of(&failed),
            StreamError::Protocol(ProtocolError::TooManyIdleEvents { .. })
        ));
        let run = run(&[&empty, &text("x"), &finish("stop")]);
        assert_eq!(run.content().len(), 1);
    }

    #[test]
    fn an_empty_id_on_a_later_delta_counts_as_no_id() {
        let run = run(&[
            &call(0, "c1", "list_tables", "{\"a\":"),
            &call(0, "", "", "1}"),
            &finish("tool_calls"),
        ]);
        assert_eq!(
            run.content(),
            [json!({"type": "tool_use", "id": "c1", "name": "list_tables", "input": {"a": 1}})]
        );
    }

    #[test]
    fn a_new_id_on_a_used_index_is_a_new_call_and_an_index_may_be_missing() {
        let run = run(&[
            &call(0, "a", "one", "{}"),
            &call(0, "b", "two", "{}"),
            // No index at all counts as 0: it continues the latest call at 0, which is `b`.
            &json!({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": " "}}]}}]})
                .to_string(),
            &finish("tool_calls"),
        ]);
        let ids: Vec<_> = run
            .content()
            .iter()
            .map(|b| (b["id"].as_str().unwrap().to_owned(), b["name"].as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(ids, [("a".into(), "one".into()), ("b".into(), "two".into())]);
    }

    #[test]
    fn the_same_server_id_on_two_indexes_is_two_calls_with_distinct_stored_ids() {
        let run = run(&[
            &call(0, "dup", "one", "{\"x\":1}"),
            &call(1, "dup", "two", "{\"y\":2}"),
            &finish("tool_calls"),
        ]);
        let blocks = run.content();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["id"], "dup");
        assert_eq!(blocks[0]["input"], json!({"x": 1}));
        assert_ne!(blocks[1]["id"], "dup");
        assert!(blocks[1]["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(blocks[1]["input"], json!({"y": 2}));
    }

    #[test]
    fn arguments_may_be_a_json_object() {
        let chunk = json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "c1", "function": {"name": "list_tables", "arguments": {"a": [1, 2]}}}]}}]})
        .to_string();
        let run = run(&[&chunk, &finish("tool_calls")]);
        assert_eq!(run.content()[0]["input"], json!({"a": [1, 2]}));
    }

    #[test]
    fn an_index_that_is_not_a_count_is_zero_and_a_float_index_is_a_count() {
        let entry = |index: Value, id: &str| {
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": index, "id": id, "function": {"name": "n", "arguments": "{}"}}]}}]})
            .to_string()
        };
        let run = run(&[&entry(json!(1.0), "a"), &entry(json!("x"), "b"), &entry(json!(-1), "c"), &finish("tool_calls")]);
        assert_eq!(run.content().len(), 3);
        // `b` and `c` are both at index 0, with different ids: two calls.
        let again = run_with(
            &[&entry(json!(1.0), "a"), &entry(json!(1), "a"), &finish("tool_calls")],
            TurnLimits::default(),
            &[],
        );
        assert_eq!(again.content().len(), 1);
    }

    #[test]
    fn an_unstorable_server_id_is_replaced_by_a_generated_one() {
        let long = "x".repeat(65);
        let with_key = format!("id-{KEY}");
        for bad in ["call.1", "a:b", "", "has space", long.as_str(), with_key.as_str()] {
            let run = run(&[&call(0, bad, "list_tables", "{}"), &finish("tool_calls")]);
            let id = run.content()[0]["id"].as_str().unwrap().to_owned();
            assert!(id.starts_with("call_"), "{bad:?} -> {id}");
            assert!(is_storable(&Scrub::new(ApiKey::new(KEY)), &id));
            assert!(!run.events.join("").contains(bad) || bad.is_empty(), "{bad}");
        }
        let run = run(&[&call(0, &"y".repeat(64), "list_tables", "{}"), &finish("tool_calls")]);
        assert_eq!(run.content()[0]["id"], "y".repeat(64));
    }

    #[test]
    fn a_server_id_taken_by_the_history_or_the_response_is_not_reused() {
        let run = run_with(
            &[
                &call(0, "0", "one", "{}"),
                &call(1, "1", "two", "{}"),
                &finish("tool_calls"),
            ],
            TurnLimits::default(),
            &["0"],
        );
        let blocks = run.content();
        assert!(blocks[0]["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(blocks[1]["id"], "1");
    }

    #[test]
    fn a_call_without_a_server_id_gets_a_generated_one_at_the_end() {
        let chunk = json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "function": {"name": "list_tables", "arguments": "{}"}}]}}]})
        .to_string();
        let run = run(&[&chunk, &finish("tool_calls")]);
        let id = run.content()[0]["id"].as_str().unwrap().to_owned();
        assert!(id.starts_with("call_"));
        assert!(run.events[0].contains(&id));
        assert!(run.events[0].starts_with("ToolCallStarted"));
    }

    #[test]
    fn generated_ids_do_not_repeat_and_skip_what_is_taken() {
        let a = generated_id(|_| false);
        let b = generated_id(|_| false);
        assert_ne!(a, b);
        let (prefix, counter) = a.rsplit_once('_').unwrap();
        assert!(prefix.starts_with("call_") && prefix.len() == "call_".len() + 8);
        let next: u64 = counter.parse().unwrap();
        let busy: HashSet<String> = (next + 1..next + 500).map(|n| format!("{prefix}_{n}")).collect();
        let fresh = generated_id(|id| busy.contains(id));
        assert!(!busy.contains(&fresh) && fresh.starts_with(prefix));
    }

    #[test]
    fn an_unusable_tool_name_is_replaced_and_reported() {
        let long = "n".repeat(65);
        let with_key = format!("tool{KEY}");
        for bad in ["list tables", "a.b", long.as_str(), with_key.as_str()] {
            let run = run(&[&call(0, "c1", bad, "{\"a\":1}"), &finish("tool_calls")]);
            let block = &run.content()[0];
            assert_eq!(block["name"], PLACEHOLDER_NAME, "{bad}");
            assert_eq!(block["input"], json!({}));
            assert!(run.events.iter().any(|e| e.contains("valid: false")));
            let invalid = &run.turn().invalid_tool_inputs;
            assert_eq!(invalid.len(), 1);
            assert_eq!(invalid[0].tool_use_id, "c1");
            assert!(invalid[0].raw.starts_with("the tool name \""), "{}", invalid[0].raw);
            assert!(invalid[0].raw.ends_with(
                "cannot be used (a name has 1 to 64 letters, digits, _ or -, and must be one of the tools offered)"
            ));
            assert!(!invalid[0].raw.contains(KEY));
            assert!(!run.events.join("").contains(KEY));
        }
    }

    #[test]
    fn a_call_without_a_name_is_reported_as_none() {
        let chunk = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1",
            "function": {"arguments": "{}"}}]}}]}).to_string();
        let run = run(&[&chunk, &finish("tool_calls")]);
        assert_eq!(run.content()[0]["name"], PLACEHOLDER_NAME);
        assert!(run.turn().invalid_tool_inputs[0].raw.starts_with("the tool name (none) cannot be used"));
        assert!(run.events[0].contains("invalid_tool_name"));
    }

    #[test]
    fn arguments_that_are_not_a_json_object_are_reported_invalid() {
        for (args, raw) in [("{\"a\":", "{\"a\":"), ("[1]", "[1]"), ("7", "7"), ("\"s\"", "\"s\"")] {
            let run = run(&[&call(0, "c1", "list_tables", args), &finish("tool_calls")]);
            assert_eq!(run.content()[0]["input"], json!({}), "{args}");
            assert_eq!(
                run.turn().invalid_tool_inputs,
                [InvalidToolInput { tool_use_id: "c1".into(), raw: raw.into() }]
            );
            assert!(run.events.last().unwrap().contains("valid: false"));
        }
        let key_raw = run(&[&call(0, "c1", "list_tables", &format!("{{\"k\":\"{KEY}")), &finish("tool_calls")]);
        assert!(!key_raw.turn().invalid_tool_inputs[0].raw.contains(KEY));
    }

    #[test]
    fn empty_arguments_are_an_empty_input_and_valid() {
        let run = run(&[&call(0, "c1", "list_tables", ""), &finish("tool_calls")]);
        assert_eq!(run.content()[0]["input"], json!({}));
        assert!(run.turn().invalid_tool_inputs.is_empty());
        assert!(run.events.last().unwrap().contains("valid: true"));
    }

    #[test]
    fn calls_are_in_order_of_first_appearance_behind_reasoning_and_text() {
        let run = run(&[
            &call(1, "b", "two", "{}"),
            &call(0, "a", "one", "{}"),
            &json!({"choices": [{"delta": {"reasoning": "r", "content": "t"}}]}).to_string(),
            &finish("tool_calls"),
        ]);
        let kinds: Vec<_> = run.content().iter().map(|b| b["type"].as_str().unwrap().to_owned()).collect();
        assert_eq!(kinds, ["reasoning", "text", "tool_use", "tool_use"]);
        assert_eq!(run.content()[2]["id"], "b");
    }

    #[test]
    fn more_calls_than_the_limit_fail() {
        let limits = TurnLimits {
            max_blocks: 2,
            ..TurnLimits::default()
        };
        let run = run_with(
            &[&call(0, "a", "n", "{}"), &call(1, "b", "n", "{}"), &call(2, "c", "n", "{}")],
            limits,
            &[],
        );
        assert!(matches!(
            error_of(&run),
            StreamError::Protocol(ProtocolError::TooManyBlocks { limit: 2 })
        ));
    }

    #[test]
    fn the_finish_reason_is_kept_scrubbed_and_the_first_one_stands() {
        let run = run(&[&text("x"), &finish("stop"), &finish("length")]);
        assert_eq!(run.turn().stop_reason, StopReason::EndTurn);
        let run = run_with(&[&text("x"), &finish(&format!("odd-{KEY}"))], TurnLimits::default(), &[]);
        assert_eq!(run.turn().stop_reason, StopReason::Other("odd-***".into()));
    }

    fn api_error(run: &Run) -> (String, String, bool) {
        match error_of(run) {
            StreamError::Api { kind, message, before_start } => (kind.clone(), message.clone(), *before_start),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_payloads_map_to_kinds() {
        let kind = |error: Value| {
            let run = run(&[&json!({"error": error}).to_string()]);
            api_error(&run).0
        };
        let cases = [
            (json!({"type": "invalid_request_error", "message": "m"}), "invalid_request_error"),
            (json!({"code": "context_length_exceeded"}), "invalid_request_error"),
            (json!({"type": "authentication_error"}), "authentication_error"),
            (json!({"code": "invalid_api_key"}), "authentication_error"),
            (json!({"type": "rate_limit_error"}), "rate_limit_error"),
            (json!({"type": "rate_limit_exceeded"}), "rate_limit_error"),
            (json!({"type": "insufficient_quota"}), "rate_limit_error"),
            (json!({"code": "rate_limit_exceeded"}), "rate_limit_error"),
            (json!({"code": "insufficient_quota"}), "rate_limit_error"),
            (json!({"type": "server_error"}), "api_error"),
            (json!({"type": "api_error"}), "api_error"),
            (json!({"type": "overloaded_error"}), "overloaded_error"),
            (json!({"type": "engine_overloaded_error"}), "overloaded_error"),
            (json!({"type": "something_else"}), "something_else"),
            (json!({"message": "only a message"}), ""),
            (json!("bare ollama text"), ""),
        ];
        for (error, want) in cases {
            assert_eq!(kind(error.clone()), want, "{error}");
        }
    }

    #[test]
    fn an_error_carries_its_message_and_is_retryable_only_before_anything_was_delivered() {
        let run1 = run(&[&json!({"error": {"message": "boom", "type": "server_error"}}).to_string()]);
        assert_eq!(api_error(&run1), ("api_error".into(), "boom".into(), true));
        let run2 = run(&["\"junk\"", &json!({"error": "model gone"}).to_string()]);
        assert_eq!(api_error(&run2), (String::new(), "model gone".into(), true));
        let run3 = run(&[&text("hi"), &json!({"error": {"message": "late"}}).to_string()]);
        assert_eq!(api_error(&run3), (String::new(), "late".into(), false));
    }

    #[test]
    fn error_null_or_bool_is_no_error_and_is_checked_before_anything_else() {
        let run = run(&[
            &json!({"error": null, "choices": [{"delta": {"content": "a"}}]}).to_string(),
            &json!({"error": false, "choices": [{"delta": {"content": "b"}}]}).to_string(),
            &finish("stop"),
        ]);
        assert_eq!(run.content(), [json!({"type": "text", "text": "ab"})]);
        let first = run_with(
            &[&json!({"error": {"message": "x"}, "choices": [{"delta": {"content": "a"}}]}).to_string()],
            TurnLimits::default(),
            &[],
        );
        assert!(first.events.is_empty());
        assert!(matches!(error_of(&first), StreamError::Api { .. }));
    }

    #[test]
    fn data_is_trimmed_and_done_ends_the_stream() {
        let run = run(&[&format!("  {}\n", text("a")), " [DONE] ", &text("never")]);
        assert!(matches!(run.result, Ok((true, _))));
        assert_eq!(run.content(), [json!({"type": "text", "text": "a"})]);
    }
}
