//! Turns the Messages API stream events into one assistant turn.
//!
//! Content blocks are accumulated by `index`, each as the JSON object `content_block_start`
//! delivered, changed only by the deltas the API defines for its type:
//! `text_delta` appends to `text`, `thinking_delta` to `thinking`, `signature_delta` to
//! `signature`, `input_json_delta` fragments are concatenated and parsed strictly at
//! `content_block_stop`. `redacted_thinking` and every type this code does not know stay exactly
//! as they started.
//!
//! Events are read into one flat struct of optional, borrowed fields: serde's internally tagged
//! enums would buffer every event into an intermediate tree first. serde's derive borrows a field
//! only when the field's own type is `Cow` (an `Option<Cow<str>>` is always deserialized as
//! `Cow::Owned`, `#[serde(borrow)]` or not), so the optional strings are [`Str`], a newtype around
//! the `Cow`. A delta then allocates only when one of its strings holds an escape (serde has to
//! unescape it into an owned string) and when the block it belongs to grows; `delta.type` and an
//! escape-free payload are slices of the event text. `content_block_start` keeps its block as a
//! `Value`, once per block.
//!
//! A turn is bounded: a cap on bytes and content blocks, and on the number of consecutive events
//! that carry no content (pings, unknown events), so a stream cannot keep the reader busy for
//! ever. Text that comes from the server and ends up in an event or a result (stop reason, stop
//! category, tool call ids and names) is scrubbed of the API key and cut like a model name.

use crate::conversation::Block;
use crate::key::ApiKey;
use crate::provider::{
    InvalidToolInput, ProtocolError, StopReason, StreamEvent, TurnResponse, Usage,
};
use crate::wire::errors::{Scrub, MAX_TOKEN_CHARS};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Deref;

pub(crate) use crate::wire::TurnLimits;

/// A string borrowed from the event text when it holds no escape. `Option<Cow<str>>` would not
/// borrow (see the module docs); a newtype whose field is a `Cow` does.
#[derive(Deserialize)]
struct Str<'a>(#[serde(borrow)] Cow<'a, str>);

impl Deref for Str<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

#[derive(Deserialize)]
struct WireEvent<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default, borrow)]
    message: Option<WireMessage<'a>>,
    #[serde(default)]
    content_block: Option<Value>,
    #[serde(default, borrow)]
    delta: Option<WireDelta<'a>>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default, borrow)]
    error: Option<WireError<'a>>,
    /// Informational: read leniently.
    #[serde(default)]
    stop_details: Option<Value>,
    /// After a mid-stream fallback the final `message_delta` lists the dropped blocks again.
    #[serde(default)]
    input_transformations: Option<Value>,
}

#[derive(Deserialize)]
struct WireMessage<'a> {
    #[serde(default, borrow)]
    model: Option<Str<'a>>,
    #[serde(default)]
    usage: Option<WireUsage>,
    /// Present on every response from a thinking-capable model once the thinking-binding beta
    /// header is sent: `[]`, or one entry per block that was dropped.
    #[serde(default)]
    input_transformations: Option<Value>,
}

/// The union of the delta shapes; which fields are set depends on `type`.
#[derive(Deserialize)]
struct WireDelta<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Option<Str<'a>>,
    #[serde(default, borrow)]
    text: Option<Str<'a>>,
    #[serde(default, borrow)]
    thinking: Option<Str<'a>>,
    #[serde(default, borrow)]
    signature: Option<Str<'a>>,
    #[serde(default, borrow)]
    partial_json: Option<Str<'a>>,
    #[serde(default, borrow)]
    stop_reason: Option<Str<'a>>,
    #[serde(default)]
    stop_details: Option<Value>,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    iterations: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct WireError<'a> {
    #[serde(default, rename = "type", borrow)]
    kind: Option<Str<'a>>,
    #[serde(default, borrow)]
    message: Option<Str<'a>>,
}

pub(crate) use crate::wire::StreamError;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Thinking,
    ToolUse,
    /// `redacted_thinking`, `fallback`, anything unknown: never changed after the start.
    Opaque,
}

struct Slot {
    kind: Kind,
    block: Value,
    /// The `input_json_delta` fragments, concatenated.
    input: String,
    closed: bool,
}

impl Slot {
    /// Appends to a string field of the block (creating it when the start did not have it).
    fn append(&mut self, field: &str, text: &str) {
        match self.block.get_mut(field) {
            Some(Value::String(existing)) => existing.push_str(text),
            _ => {
                if let Some(object) = self.block.as_object_mut() {
                    object.insert(field.to_owned(), Value::String(text.to_owned()));
                }
            }
        }
    }

    /// Ends the block. A tool call's input is parsed strictly here; text that is not a JSON
    /// object is remembered raw and the stored block gets `input: {}`.
    fn close(
        &mut self,
        invalid: &mut Vec<InvalidToolInput>,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) {
        if self.closed {
            return;
        }
        self.closed = true;
        if self.kind != Kind::ToolUse {
            return;
        }
        let mut valid = true;
        if !self.input.is_empty() {
            match serde_json::from_str::<Value>(&self.input) {
                Ok(input @ Value::Object(_)) => self.block["input"] = input,
                _ => {
                    valid = false;
                    self.block["input"] = Value::Object(serde_json::Map::new());
                    invalid.push(InvalidToolInput {
                        tool_use_id: Block(&self.block).str("id").unwrap_or("").to_owned(),
                        raw: std::mem::take(&mut self.input),
                    });
                }
            }
        } else if !matches!(self.block.get("input"), Some(Value::Object(_))) {
            self.block["input"] = Value::Object(serde_json::Map::new());
        }
        self.input = String::new();
        let block = Block(&self.block);
        emit(StreamEvent::ToolInputReady {
            id: block.str("id").unwrap_or(""),
            name: block.str("name").unwrap_or(""),
            input: &self.block["input"],
            valid,
        });
    }
}

pub(crate) struct TurnAssembler {
    limits: TurnLimits,
    started: bool,
    idle: usize,
    model: Option<String>,
    usage: Usage,
    fallback_message: bool,
    stop_reason: Option<String>,
    category: Option<String>,
    slots: BTreeMap<usize, Slot>,
    invalid: Vec<InvalidToolInput>,
    /// Server text that reaches an event or a result goes through this (bounds it, and removes
    /// the API key when one is set).
    scrub: Scrub,
    thinking_dropped: usize,
}

impl TurnAssembler {
    pub fn new(limits: TurnLimits) -> Self {
        Self {
            scrub: Scrub::new(ApiKey::new("")),
            thinking_dropped: 0,
            limits,
            started: false,
            idle: 0,
            model: None,
            usage: Usage::default(),
            fallback_message: false,
            stop_reason: None,
            category: None,
            slots: BTreeMap::new(),
            invalid: Vec::new(),
        }
    }

    /// Scrubs the API key from server text that is passed on, and cuts it to the bound of a
    /// short token.
    #[must_use]
    pub fn with_scrub(mut self, scrub: Scrub) -> Self {
        self.scrub = scrub;
        self
    }

    fn clean(&self, text: &str) -> String {
        self.scrub.bounded(text, MAX_TOKEN_CHARS)
    }

    /// `input_transformations`: the entries of type `thinking_dropped` are blocks the API
    /// removed from the history. Entries of other types (`thinking_mismatch_allowed`, and the
    /// ones later checks add) are ignored, as the reference says. The array may come twice (in
    /// `message_start` and again in the final `message_delta`): the larger count stands.
    fn note_transformations(&mut self, entries: Option<&Value>) {
        let dropped = entries.and_then(Value::as_array).map_or(0, |entries| {
            entries
                .iter()
                .filter(|entry| Block(entry).kind() == "thinking_dropped")
                .count()
        });
        self.thinking_dropped = self.thinking_dropped.max(dropped);
    }

    fn overlay(&mut self, usage: &WireUsage) {
        let set = |slot: &mut u64, value: Option<u64>| {
            if let Some(value) = value {
                *slot = value;
            }
        };
        set(&mut self.usage.input_tokens, usage.input_tokens);
        set(&mut self.usage.output_tokens, usage.output_tokens);
        set(
            &mut self.usage.cache_read_input_tokens,
            usage.cache_read_input_tokens,
        );
        set(
            &mut self.usage.cache_creation_input_tokens,
            usage.cache_creation_input_tokens,
        );
        if usage.iterations.as_ref().is_some_and(|iterations| {
            iterations
                .iter()
                .any(|entry| Block(entry).kind() == "fallback_message")
        }) {
            self.fallback_message = true;
        }
    }

    /// Handles one SSE event; `Ok(true)` once `message_stop` has been seen.
    pub fn handle(
        &mut self,
        data: &str,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<bool, StreamError> {
        let event: WireEvent<'_> = serde_json::from_str(data).map_err(|error| {
            // The classification and position only: the error text would quote the stream.
            ProtocolError::MalformedEvent(format!(
                "{:?} at line {} column {}",
                error.classify(),
                error.line(),
                error.column()
            ))
        })?;
        let index = event.index;
        if self.is_known(&event.kind) {
            self.idle = 0;
        } else if &*event.kind != "error" {
            self.idle += 1;
            if self.idle > self.limits.max_idle_events {
                return Err(ProtocolError::TooManyIdleEvents {
                    limit: self.limits.max_idle_events,
                }
                .into());
            }
        }
        match &*event.kind {
            "message_start" => {
                if self.started {
                    return Err(ProtocolError::SecondMessageStart.into());
                }
                self.started = true;
                if let Some(message) = event.message {
                    self.model = message.model.map(|model| model.0.into_owned());
                    if let Some(usage) = &message.usage {
                        self.overlay(usage);
                    }
                    self.note_transformations(message.input_transformations.as_ref());
                }
            }
            "content_block_start" => {
                self.require_started("content_block_start")?;
                let index = index.ok_or(ProtocolError::UnknownBlock(0))?;
                let block = event
                    .content_block
                    .ok_or(ProtocolError::MalformedBlock(index))?;
                self.start_block(index, block, emit)?;
            }
            "content_block_delta" => {
                self.require_started("content_block_delta")?;
                let index = index.ok_or(ProtocolError::UnknownBlock(0))?;
                if let Some(delta) = event.delta {
                    self.delta(index, delta, emit)?;
                }
            }
            "content_block_stop" => {
                self.require_started("content_block_stop")?;
                let index = index.ok_or(ProtocolError::UnknownBlock(0))?;
                let slot = self
                    .slots
                    .get_mut(&index)
                    .ok_or(ProtocolError::UnknownBlock(index))?;
                slot.close(&mut self.invalid, emit);
            }
            "message_delta" => {
                self.require_started("message_delta")?;
                if let Some(delta) = event.delta {
                    if let Some(reason) = delta.stop_reason {
                        self.stop_reason = Some(self.clean(&reason));
                    }
                    let category = delta
                        .stop_details
                        .as_ref()
                        .and_then(|d| Block(d).str("category"))
                        .map(|category| self.clean(category));
                    if category.is_some() {
                        self.category = category;
                    }
                }
                let category = event
                    .stop_details
                    .as_ref()
                    .and_then(|d| Block(d).str("category"))
                    .map(|category| self.clean(category));
                if category.is_some() {
                    self.category = category;
                }
                if let Some(usage) = &event.usage {
                    self.overlay(usage);
                }
                self.note_transformations(event.input_transformations.as_ref());
            }
            "message_stop" => {
                self.require_started("message_stop")?;
                for slot in self.slots.values_mut() {
                    slot.close(&mut self.invalid, emit);
                }
                return Ok(true);
            }
            "error" => {
                let error = event.error;
                return Err(StreamError::Api {
                    kind: error
                        .as_ref()
                        .and_then(|e| e.kind.as_deref())
                        .unwrap_or_default()
                        .to_owned(),
                    message: error
                        .and_then(|e| e.message)
                        .map(|message| message.0.into_owned())
                        .unwrap_or_default(),
                    before_start: !self.started,
                });
            }
            // `ping` and event types this code does not know.
            _ => {}
        }
        Ok(false)
    }

    fn is_known(&self, kind: &str) -> bool {
        matches!(
            kind,
            "message_start"
                | "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
        )
    }

    fn require_started(&self, event: &'static str) -> Result<(), ProtocolError> {
        if self.started {
            Ok(())
        } else {
            Err(ProtocolError::BeforeMessageStart(event))
        }
    }

    fn start_block(
        &mut self,
        index: usize,
        block: Value,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<(), ProtocolError> {
        if self.slots.contains_key(&index) {
            return Err(ProtocolError::DuplicateBlock(index));
        }
        if self.slots.len() >= self.limits.max_blocks {
            return Err(ProtocolError::TooManyBlocks {
                limit: self.limits.max_blocks,
            });
        }
        if !block.is_object() {
            return Err(ProtocolError::MalformedBlock(index));
        }
        let kind = match Block(&block).kind() {
            "" => return Err(ProtocolError::MalformedBlock(index)),
            "text" => Kind::Text,
            "thinking" => Kind::Thinking,
            "tool_use" => Kind::ToolUse,
            _ => Kind::Opaque,
        };
        if kind == Kind::ToolUse {
            // A call without an id or a name could never be answered; two with one id would be
            // answered once.
            // The id and the name go into events, results and the stored turn: one that holds the
            // API key, a control character or is longer than any real one is refused.
            let id = match (Block(&block).str("id"), Block(&block).str("name")) {
                (Some(id), Some(name))
                    if !id.is_empty()
                        && !name.is_empty()
                        && self.clean(id) == id
                        && self.clean(name) == name =>
                {
                    id
                }
                _ => return Err(ProtocolError::BadToolUse(index)),
            };
            if self
                .slots
                .values()
                .any(|slot| slot.kind == Kind::ToolUse && Block(&slot.block).str("id") == Some(id))
            {
                return Err(ProtocolError::DuplicateToolUseId(index));
            }
            emit(StreamEvent::ToolCallStarted {
                id,
                name: Block(&block).str("name").unwrap_or(""),
            });
        }
        // The text a text block starts with is delivered like a delta.
        if kind == Kind::Text {
            let text = Block(&block).str("text").unwrap_or("");
            if !text.is_empty() {
                emit(StreamEvent::TextDelta(text));
            }
        }
        self.slots.insert(
            index,
            Slot {
                kind,
                block,
                input: String::new(),
                closed: false,
            },
        );
        Ok(())
    }

    fn delta(
        &mut self,
        index: usize,
        delta: WireDelta<'_>,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<(), ProtocolError> {
        let slot = self
            .slots
            .get_mut(&index)
            .ok_or(ProtocolError::UnknownBlock(index))?;
        if slot.closed {
            return Err(ProtocolError::DeltaAfterStop(index));
        }
        let mismatch = ProtocolError::DeltaMismatch(index);
        match (delta.kind.as_deref(), slot.kind) {
            (Some("text_delta"), Kind::Text) => {
                let text = delta.text.ok_or(mismatch)?;
                emit(StreamEvent::TextDelta(&text));
                slot.append("text", &text);
            }
            (Some("thinking_delta"), Kind::Thinking) => {
                let thinking = delta.thinking.ok_or(mismatch)?;
                emit(StreamEvent::ThinkingDelta(&thinking));
                slot.append("thinking", &thinking);
            }
            (Some("signature_delta"), Kind::Thinking) => {
                slot.append("signature", &delta.signature.ok_or(mismatch)?);
            }
            (Some("input_json_delta"), Kind::ToolUse) => {
                slot.input.push_str(&delta.partial_json.ok_or(mismatch)?);
            }
            // A block this code does not know stays as it started.
            (_, Kind::Opaque) => {}
            // A delta type this code does not know changes nothing it could echo.
            (Some(other), _)
                if !matches!(
                    other,
                    "text_delta" | "thinking_delta" | "signature_delta" | "input_json_delta"
                ) => {}
            (None, _) => {}
            _ => return Err(mismatch),
        }
        Ok(())
    }

    /// The turn, once `message_stop` was handled. Text blocks without text (empty or only
    /// whitespace) are left out: the API rejects them when they come back.
    pub fn finish(self, request_id: Option<String>) -> Result<TurnResponse, ProtocolError> {
        let reason = self.stop_reason.ok_or(ProtocolError::MissingStopReason)?;
        let stop_reason = StopReason::from_wire(&reason, self.category);
        let served_by_fallback =
            self.fallback_message && !matches!(stop_reason, StopReason::Refusal { .. });
        let content: Vec<Value> = self
            .slots
            .into_values()
            .map(|slot| slot.block)
            .filter(|block| {
                let block = Block(block);
                block.kind() != "text" || block.str("text").is_some_and(|t| !t.trim().is_empty())
            })
            .collect();
        let fallback_to = crate::conversation::Content(&content)
            .fallback_target()
            .map(str::to_owned);
        Ok(TurnResponse {
            content,
            stop_reason,
            usage: self.usage,
            model: self.model,
            served_by_fallback,
            fallback_to,
            invalid_tool_inputs: self.invalid,
            request_id,
            thinking_dropped: self.thinking_dropped,
        })
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn is_borrowed(text: &Option<Str<'_>>) -> bool {
        matches!(text, Some(Str(Cow::Borrowed(_))))
    }

    #[test]
    fn a_delta_without_escapes_borrows_all_its_strings_and_one_with_an_escape_owns_only_that_one() {
        let plain = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"plain ü text"}}"#;
        let event: WireEvent<'_> = serde_json::from_str(plain).unwrap();
        let delta = event.delta.unwrap();
        assert!(
            is_borrowed(&delta.kind),
            "delta.type is a slice of the event"
        );
        assert!(
            is_borrowed(&delta.text),
            "an escape-free payload is a slice too"
        );

        let escaped = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"line\nbreak"}}"#;
        let event: WireEvent<'_> = serde_json::from_str(escaped).unwrap();
        let delta = event.delta.unwrap();
        assert!(is_borrowed(&delta.kind));
        assert!(
            matches!(delta.text, Some(Str(Cow::Owned(ref text))) if text == "line\nbreak"),
            "an escape has to be unescaped into an owned string"
        );

        // Every optional string field of the event types borrows the same way.
        let json = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#;
        let delta = serde_json::from_str::<WireEvent<'_>>(json)
            .unwrap()
            .delta
            .unwrap();
        assert!(matches!(delta.partial_json, Some(Str(Cow::Owned(_)))));
        let json = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let error = serde_json::from_str::<WireEvent<'_>>(json)
            .unwrap()
            .error
            .unwrap();
        assert!(is_borrowed(&error.kind) && is_borrowed(&error.message));
        let json = r#"{"type":"message_start","message":{"model":"claude-opus-5-5"}}"#;
        let message = serde_json::from_str::<WireEvent<'_>>(json)
            .unwrap()
            .message
            .unwrap();
        assert!(is_borrowed(&message.model));
    }
}
