// Adapted from dbx (https://github.com/t8y2/dbx), Apache-2.0; changed. See `mod.rs` and
// `licenses/README.md` at the root of the repository.

//! Turns a Chat Completions event stream into one assistant turn.
//!
//! Ported from dbx `stream_openai_with_tools` and its helpers (`openai_stream_text`,
//! `openai_stream_reasoning`, `openai_stream_has_finish_reason`, `text_from_content_value`):
//! the same fields in the same order of preference, a tool call starts only on a non-empty `id`
//! (GLM and others send `id: ""` on the later deltas), `[DONE]` ends the stream, and so does a
//! `finish_reason` followed by silence (see [`GraceReader`]).
//!
//! Like the Anthropic assembler, an event is read into one flat struct of borrowed fields: a
//! delta allocates only when a string holds an escape, and the `choices` and `tool_calls` arrays
//! are read without a `Vec` for the usual one entry. A field of an unexpected scalar type reads
//! as absent (dbx reads with `as_str()` and ignores the rest), numbers are read leniently
//! (`index` and the token counts may be integer-valued floats, anything else is ignored for that
//! field only) and `function.arguments` may be a JSON object instead of text, so one odd field
//! does not fail the event. An event whose structure is wrong (`delta` that is not an object,
//! say) is still skipped as a whole.
//!
//! Tool call ids and names are the server's, used only to match the deltas of one response. What
//! is stored and reported is an id of `[A-Za-z0-9_-]{1,64}` that occurs nowhere else in the
//! conversation (the server's when it qualifies, otherwise a generated one), and a name of the
//! same form (an unusable name is stored as [`PLACEHOLDER_NAME`] and reported as an invalid
//! call, so the assistant answers it as not run). Both Anthropic and OpenAI refuse other forms
//! when the history is sent back.

use crate::conversation::{Block, Message};
use crate::provider::{
    InvalidToolInput, ProtocolError, StopReason, StreamEvent, TurnResponse, Usage,
};
use crate::wire::errors::{Scrub, MAX_TOKEN_CHARS};
use crate::wire::{StreamError, TurnLimits};
use engine_agent::CancelToken;
use serde::de::value::MapAccessDeserializer;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A JSON string, borrowed from the event text when it has no escape; any other JSON value is
/// absent.
#[derive(Default)]
pub(super) struct OptStr<'a>(pub(super) Option<Cow<'a, str>>);

impl<'a> OptStr<'a> {
    /// The text, when there is some.
    fn text(&self) -> Option<&str> {
        self.0.as_deref().filter(|text| !text.is_empty())
    }
}

struct OptStrVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for OptStrVisitor<'a> {
    type Value = OptStr<'a>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(OptStr(Some(Cow::Borrowed(v))))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(OptStr(Some(Cow::Owned(v.to_owned()))))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(OptStr(Some(Cow::Owned(v))))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(OptStr(None))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(OptStr(None))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(OptStr(None))
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(OptStr(None))
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(OptStr(None))
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(OptStr(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(OptStr(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(OptStr(None))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for OptStr<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(OptStrVisitor(PhantomData))
    }
}

/// Message content: a string, or a list of parts whose `text` / `content` / `input_text` /
/// `output_text` are joined (dbx `text_from_content_value`). Empty is absent.
#[derive(Default)]
struct Content<'a>(Option<Cow<'a, str>>);

struct ContentVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for ContentVisitor<'a> {
    type Value = Content<'a>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(Content((!v.is_empty()).then_some(Cow::Borrowed(v))))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Content((!v.is_empty()).then(|| Cow::Owned(v.to_owned()))))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(Content((!v.is_empty()).then_some(Cow::Owned(v))))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Content(None))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Content(None))
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Content(None))
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Content(None))
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Content(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut joined = String::new();
        while let Some(part) = seq.next_element::<Value>()? {
            let text = ["text", "content", "input_text", "output_text"]
                .iter()
                .find_map(|field| part.get(field).and_then(Value::as_str));
            joined.push_str(text.unwrap_or(""));
        }
        Ok(Content((!joined.is_empty()).then_some(Cow::Owned(joined))))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Content(None))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Content<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ContentVisitor(PhantomData))
    }
}

/// A non-negative whole number; a float with an integer value is accepted, anything else is
/// absent.
#[derive(Default)]
struct Count(Option<u64>);

struct CountVisitor;

impl<'de> Visitor<'de> for CountVisitor {
    type Value = Count;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(Count(Some(v)))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(Count(u64::try_from(v).ok()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        // 2^53: the whole numbers a float holds exactly.
        let whole = v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v <= 9_007_199_254_740_992.0;
        Ok(Count(whole.then_some(v as u64)))
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Count(None))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Count(None))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Count(None))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Count(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Count(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Count(None))
    }
}

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(CountVisitor)
    }
}

/// `function.arguments`: the text of the JSON input, or (some servers) the input itself as an
/// object, which is taken as its JSON text. Anything else is absent.
#[derive(Default)]
struct ArgsText<'a>(Option<Cow<'a, str>>);

impl ArgsText<'_> {
    fn text(&self) -> Option<&str> {
        self.0.as_deref().filter(|text| !text.is_empty())
    }
}

struct ArgsVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for ArgsVisitor<'a> {
    type Value = ArgsText<'a>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(ArgsText(Some(Cow::Borrowed(v))))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(ArgsText(Some(Cow::Owned(v.to_owned()))))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(ArgsText(Some(Cow::Owned(v))))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(ArgsText(None))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(ArgsText(None))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(ArgsText(None))
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(ArgsText(None))
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(ArgsText(None))
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(ArgsText(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(ArgsText(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        let input = Value::deserialize(MapAccessDeserializer::new(map))?;
        Ok(ArgsText(Some(Cow::Owned(input.to_string()))))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for ArgsText<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ArgsVisitor(PhantomData))
    }
}

/// The first entry of a JSON array, the rest skipped (a request never asks for `n > 1`).
struct First<T>(Option<T>);

impl<T> Default for First<T> {
    fn default() -> Self {
        Self(None)
    }
}

struct FirstVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for FirstVisitor<T> {
    type Value = First<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(First(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let first = seq.next_element::<T>()?;
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(First(first))
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for First<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FirstVisitor(PhantomData))
    }
}

/// The calls of one delta: the first inline, the others (parallel calls in one event are rare)
/// in a `Vec` that stays unallocated when empty.
#[derive(Default)]
struct Calls<'a> {
    first: Option<WireToolCall<'a>>,
    rest: Vec<WireToolCall<'a>>,
}

struct CallsVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for CallsVisitor<'a> {
    type Value = Calls<'a>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Calls::default())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut calls = Calls::default();
        while let Some(call) = seq.next_element::<WireToolCall<'a>>()? {
            if calls.first.is_none() {
                calls.first = Some(call);
            } else {
                calls.rest.push(call);
            }
        }
        Ok(calls)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Calls<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(CallsVisitor(PhantomData))
    }
}

impl<'a> Calls<'a> {
    fn iter(&self) -> impl Iterator<Item = &WireToolCall<'a>> {
        self.first.iter().chain(self.rest.iter())
    }
}

/// `error`: `{"message", "type", "code"}` (OpenAI and most servers) or the bare text (Ollama).
#[derive(Default)]
struct WireError<'a> {
    present: bool,
    kind: OptStr<'a>,
    code: OptStr<'a>,
    message: OptStr<'a>,
}

struct WireErrorVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for WireErrorVisitor<'a> {
    type Value = WireError<'a>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an error object or text")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(WireError {
            present: true,
            message: OptStr(Some(Cow::Borrowed(v))),
            ..WireError::default()
        })
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(WireError {
            present: true,
            message: OptStr(Some(Cow::Owned(v.to_owned()))),
            ..WireError::default()
        })
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(WireError::default())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(WireError::default())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut error = WireError {
            present: true,
            ..WireError::default()
        };
        while let Some(key) = map.next_key::<OptStr<'a>>()? {
            match key.0.as_deref() {
                Some("message") => error.message = map.next_value()?,
                Some("type") => error.kind = map.next_value()?,
                Some("code") => error.code = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(error)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for WireError<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(WireErrorVisitor(PhantomData))
    }
}

#[derive(Deserialize, Default)]
struct WireChunk<'a> {
    #[serde(default, borrow)]
    choices: First<WireChoice<'a>>,
    /// Some gateways put the text at the top level (dbx reads `event.content` and `event.delta`).
    #[serde(default, borrow)]
    content: Content<'a>,
    #[serde(default, borrow)]
    delta: OptStr<'a>,
    #[serde(default, borrow)]
    model: OptStr<'a>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default, borrow)]
    error: WireError<'a>,
}

#[derive(Deserialize)]
struct WireChoice<'a> {
    #[serde(default, borrow)]
    delta: Option<WireDelta<'a>>,
    #[serde(default, borrow)]
    message: Option<WireMessage<'a>>,
    #[serde(default, borrow)]
    text: Content<'a>,
    #[serde(default, borrow)]
    finish_reason: OptStr<'a>,
}

#[derive(Deserialize)]
struct WireDelta<'a> {
    #[serde(default, borrow)]
    content: Content<'a>,
    #[serde(default, borrow)]
    reasoning_content: OptStr<'a>,
    #[serde(default, borrow)]
    reasoning: OptStr<'a>,
    #[serde(default, borrow)]
    thinking: OptStr<'a>,
    #[serde(default, borrow)]
    tool_calls: Calls<'a>,
}

#[derive(Deserialize)]
struct WireMessage<'a> {
    #[serde(default, borrow)]
    content: Content<'a>,
}

#[derive(Deserialize)]
struct WireToolCall<'a> {
    #[serde(default)]
    index: Count,
    #[serde(default, borrow)]
    id: OptStr<'a>,
    #[serde(default, borrow)]
    function: Option<WireFunction<'a>>,
}

#[derive(Deserialize)]
struct WireFunction<'a> {
    #[serde(default, borrow)]
    name: OptStr<'a>,
    #[serde(default, borrow)]
    arguments: ArgsText<'a>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Count,
    #[serde(default)]
    completion_tokens: Count,
    #[serde(default)]
    prompt_tokens_details: Option<WireTokenDetails>,
}

#[derive(Deserialize)]
struct WireTokenDetails {
    #[serde(default)]
    cached_tokens: Count,
}

/// A tool call being assembled, found by its `index` (and the server's `id`).
struct ToolSlot {
    index: u32,
    /// The id the server gave, only to match the deltas of this call.
    wire_id: Option<String>,
    /// The id that is stored and reported (see the module docs); a call the server never named
    /// gets it when the stream ends.
    id: Option<String>,
    name: String,
    arguments: String,
    /// `ToolCallStarted` was sent.
    started: bool,
}

/// What an unusable tool name is stored as.
const PLACEHOLDER_NAME: &str = "invalid_tool_name";

/// Ids given to calls the server did not name (or named in a form that cannot be stored).
static GENERATED_IDS: AtomicU64 = AtomicU64::new(1);

/// The random part of generated ids, drawn once per process from the standard library's hash
/// keys: a conversation opened after a restart does not meet the ids of an earlier run.
fn process_part() -> u32 {
    static PART: OnceLock<u32> = OnceLock::new();
    *PART.get_or_init(|| (RandomState::new().build_hasher().finish() & 0xffff_ffff) as u32)
}

/// The ids the next `count` generated calls would get (for a test that plays a restart).
#[cfg(test)]
pub(super) fn next_generated_ids_for_test(count: u64) -> Vec<String> {
    let first = GENERATED_IDS.load(Ordering::Relaxed);
    (first..first + count)
        .map(|n| format!("call_{:08x}_{n}", process_part()))
        .collect()
}

/// `[A-Za-z0-9_-]{1,64}`: what Anthropic requires of a `tool_use` id and a tool name, and OpenAI
/// of a function name.
fn has_wire_form(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Text of the server that may be stored and reported: of the wire form, and without the key.
fn is_storable(scrub: &Scrub, text: &str) -> bool {
    has_wire_form(text) && scrub.bounded(text, MAX_TOKEN_CHARS) == text
}

pub(super) struct ChatAssembler<'h> {
    limits: TurnLimits,
    scrub: Scrub,
    /// The conversation so far: the ids of its tool calls are taken.
    history: &'h [Message],
    /// Something of the turn was passed on: an `error` now is not a failure of the request itself.
    delivered: bool,
    idle: usize,
    done: bool,
    model: Option<String>,
    usage: Usage,
    finish: Option<String>,
    text: String,
    reasoning: String,
    tools: Vec<ToolSlot>,
}

impl<'h> ChatAssembler<'h> {
    pub fn new(limits: TurnLimits, scrub: Scrub, history: &'h [Message]) -> Self {
        Self {
            limits,
            scrub,
            history,
            delivered: false,
            idle: 0,
            done: false,
            model: None,
            usage: Usage::default(),
            finish: None,
            text: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
        }
    }

    fn clean(&self, text: &str) -> String {
        self.scrub.bounded(text, MAX_TOKEN_CHARS)
    }

    /// `id` is the id of a tool call of the history or of this response so far.
    fn id_taken(&self, id: &str) -> bool {
        self.tools.iter().any(|slot| slot.id.as_deref() == Some(id))
            || self.history.iter().any(|message| match message {
                Message::Assistant { content } => content.iter().any(|block| {
                    let block = Block(block);
                    block.kind() == "tool_use" && block.str("id") == Some(id)
                }),
                _ => false,
            })
    }

    /// An id nobody has: `call_<process part>_<counter>`.
    fn fresh_id(&self) -> String {
        loop {
            let n = GENERATED_IDS.fetch_add(1, Ordering::Relaxed);
            let id = format!("call_{:08x}_{n}", process_part());
            if !self.id_taken(&id) {
                return id;
            }
        }
    }

    /// The id to store for a call the server named `wire`: that one when it may be stored and
    /// is new, otherwise a generated one.
    fn stored_id_for(&self, wire: &str) -> String {
        if is_storable(&self.scrub, wire) && !self.id_taken(wire) {
            wire.to_owned()
        } else {
            self.fresh_id()
        }
    }

    /// A `finish_reason` has been seen: the stream may end without `[DONE]`.
    pub fn finish_reason_seen(&self) -> bool {
        self.finish.is_some()
    }

    /// The stream ended the way a stream may: `[DONE]`, or a `finish_reason`.
    pub fn is_complete(&self) -> bool {
        self.done || self.finish.is_some()
    }

    /// Handles one SSE event; `Ok(true)` at `[DONE]`.
    pub fn handle(
        &mut self,
        data: &str,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<bool, StreamError> {
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(true);
        }
        // dbx skips an event that is no JSON; here too, within the bound on idle events.
        let Ok(chunk) = serde_json::from_str::<WireChunk<'_>>(data) else {
            self.note_idle()?;
            return Ok(false);
        };
        if chunk.error.present {
            let error = &chunk.error;
            return Err(StreamError::Api {
                kind: error_kind(
                    error.kind.0.as_deref().unwrap_or(""),
                    error.code.0.as_deref().unwrap_or(""),
                )
                .to_owned(),
                message: error.message.0.as_deref().unwrap_or("").to_owned(),
                before_start: !self.delivered,
            });
        }
        let mut progress = false;
        if let Some(usage) = &chunk.usage {
            // As dbx: only a chunk that has both counts.
            if let (Some(prompt), Some(completion)) =
                (usage.prompt_tokens.0, usage.completion_tokens.0)
            {
                let cached = usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens.0)
                    .unwrap_or(0);
                // `prompt_tokens` counts the cached ones; `Usage::input_tokens` does not.
                self.usage = Usage {
                    input_tokens: prompt.saturating_sub(cached),
                    output_tokens: completion,
                    cache_read_input_tokens: cached,
                    cache_creation_input_tokens: 0,
                };
                progress = true;
            }
        }
        if self.model.is_none() {
            self.model = chunk.model.text().map(str::to_owned);
        }
        let choice = chunk.choices.0.as_ref();
        let delta = choice.and_then(|choice| choice.delta.as_ref());
        let reasoning = delta.and_then(|delta| {
            delta
                .reasoning_content
                .text()
                .or_else(|| delta.reasoning.text())
                .or_else(|| delta.thinking.text())
        });
        if let Some(reasoning) = reasoning {
            emit(StreamEvent::ThinkingDelta(reasoning));
            self.reasoning.push_str(reasoning);
            progress = true;
        }
        let text = choice
            .and_then(|choice| {
                delta
                    .and_then(|delta| delta.content.0.as_deref())
                    .or_else(|| {
                        choice
                            .message
                            .as_ref()
                            .and_then(|message| message.content.0.as_deref())
                    })
                    .or(choice.text.0.as_deref())
            })
            .or(chunk.content.0.as_deref())
            .or_else(|| chunk.delta.text());
        if let Some(text) = text {
            emit(StreamEvent::TextDelta(text));
            self.text.push_str(text);
            progress = true;
        }
        if let Some(delta) = delta {
            for call in delta.tool_calls.iter() {
                progress |= self.tool_call(call, emit)?;
            }
        }
        if let Some(reason) = choice.and_then(|choice| choice.finish_reason.text()) {
            if self.finish.is_none() {
                self.finish = Some(self.clean(reason));
            }
            progress = true;
        }
        if progress {
            self.idle = 0;
            self.delivered = true;
            Ok(false)
        } else {
            self.note_idle()?;
            Ok(false)
        }
    }

    fn note_idle(&mut self) -> Result<(), ProtocolError> {
        self.idle += 1;
        if self.idle > self.limits.max_idle_events {
            return Err(ProtocolError::TooManyIdleEvents {
                limit: self.limits.max_idle_events,
            });
        }
        Ok(())
    }

    /// One entry of `delta.tool_calls`; true when it carried something.
    fn tool_call(
        &mut self,
        call: &WireToolCall<'_>,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<bool, ProtocolError> {
        let index = call
            .index
            .0
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0);
        let id = call.id.text();
        let function = call.function.as_ref();
        let name = function.and_then(|function| function.name.text());
        let arguments = function.and_then(|function| function.arguments.text());
        if id.is_none() && name.is_none() && arguments.is_none() {
            return Ok(false);
        }
        // A call is found by its index and id; one without an id belongs to the latest call of
        // its index (a call that has no id yet takes the first id that comes). A new id is a new
        // call even on a used index (some servers number every call 0), and the same id on
        // another index is another call.
        let found = match id {
            Some(id) => self
                .tools
                .iter()
                .rposition(|slot| slot.index == index && slot.wire_id.as_deref() == Some(id))
                .or_else(|| {
                    self.tools
                        .iter()
                        .rposition(|slot| slot.index == index && slot.wire_id.is_none())
                }),
            None => self.tools.iter().rposition(|slot| slot.index == index),
        };
        let at = match found {
            Some(at) => at,
            None => {
                if self.tools.len() >= self.limits.max_blocks {
                    return Err(ProtocolError::TooManyBlocks {
                        limit: self.limits.max_blocks,
                    });
                }
                self.tools.push(ToolSlot {
                    index,
                    wire_id: None,
                    id: None,
                    name: String::new(),
                    arguments: String::new(),
                    started: false,
                });
                self.tools.len() - 1
            }
        };
        let stored = match id {
            Some(wire) if self.tools[at].wire_id.is_none() => {
                Some((wire, self.stored_id_for(wire)))
            }
            _ => None,
        };
        let scrub = &self.scrub;
        let slot = &mut self.tools[at];
        if let Some(name) = name {
            if slot.name != name {
                name.clone_into(&mut slot.name);
            }
        }
        if let Some((wire, stored)) = stored {
            slot.wire_id = Some(wire.to_owned());
            slot.id = Some(stored);
        }
        // Reported once there is an id and a name (a name that comes later is waited for).
        if let (Some(id), false, false) = (&slot.id, slot.started, slot.name.is_empty()) {
            slot.started = true;
            emit(StreamEvent::ToolCallStarted {
                id,
                name: if is_storable(scrub, &slot.name) {
                    &slot.name
                } else {
                    PLACEHOLDER_NAME
                },
            });
        }
        if let Some(arguments) = arguments {
            slot.arguments.push_str(arguments);
        }
        Ok(true)
    }

    /// The turn, once the stream ended. Text without text (blank) is left out: the API rejects
    /// such a block when it comes back. Calls are reported ready here, in order.
    pub fn finish(
        mut self,
        request_id: Option<String>,
        emit: &mut dyn FnMut(StreamEvent<'_>),
    ) -> Result<TurnResponse, ProtocolError> {
        let has_calls = !self.tools.is_empty();
        let stop_reason = match self.finish.as_deref() {
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal { category: None },
            // As dbx: calls that were collected are run whatever else the reason says.
            _ if has_calls => StopReason::ToolUse,
            None | Some("stop") => StopReason::EndTurn,
            Some("tool_calls" | "function_call") => StopReason::ToolUse,
            Some(other) => StopReason::Other(other.to_owned()),
        };
        let mut content = Vec::with_capacity(2 + self.tools.len());
        if !self.reasoning.trim().is_empty() {
            content.push(json!({"type": "reasoning", "text": self.reasoning}));
        }
        if !self.text.trim().is_empty() {
            content.push(json!({"type": "text", "text": self.text}));
        }
        // A call the server never named gets its id now, one nobody else has.
        for at in 0..self.tools.len() {
            if self.tools[at].id.is_none() {
                let id = self.fresh_id();
                self.tools[at].id = Some(id);
            }
        }
        let mut invalid = Vec::new();
        for slot in std::mem::take(&mut self.tools) {
            let id = slot.id.unwrap_or_default();
            let usable_name = is_storable(&self.scrub, &slot.name);
            let name = if usable_name {
                slot.name.as_str()
            } else {
                PLACEHOLDER_NAME
            };
            if !slot.started {
                emit(StreamEvent::ToolCallStarted { id: &id, name });
            }
            let mut valid = true;
            let input = if !usable_name {
                // Not a tool this app offered, and a name that cannot go back to either API:
                // stored under a placeholder, answered as not run.
                valid = false;
                let shown = if slot.name.is_empty() {
                    "(none)".to_owned()
                } else {
                    format!("{:?}", self.clean(&slot.name))
                };
                invalid.push(InvalidToolInput {
                    tool_use_id: id.clone(),
                    raw: format!(
                        "the tool name {shown} cannot be used (a name has 1 to 64 letters, digits, \
                         _ or -, and must be one of the tools offered)"
                    ),
                });
                Value::Object(serde_json::Map::new())
            } else if slot.arguments.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str::<Value>(&slot.arguments) {
                    Ok(input @ Value::Object(_)) => input,
                    _ => {
                        valid = false;
                        invalid.push(InvalidToolInput {
                            tool_use_id: id.clone(),
                            raw: slot.arguments,
                        });
                        Value::Object(serde_json::Map::new())
                    }
                }
            };
            emit(StreamEvent::ToolInputReady {
                id: &id,
                name,
                input: &input,
                valid,
            });
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
        // The same check the stored turn goes through later.
        debug_assert!(content.iter().all(|block| Block(block).kind() != ""));
        Ok(TurnResponse {
            content,
            stop_reason,
            usage: self.usage,
            model: self.model,
            served_by_fallback: false,
            fallback_to: None,
            invalid_tool_inputs: invalid,
            request_id,
            thinking_dropped: 0,
        })
    }
}

/// What an `error` object stands for, in the words [`crate::ProviderError::from_stream_event`]
/// maps: OpenAI's own `type` / `code` values, then what the other servers send.
fn error_kind<'a>(kind: &'a str, code: &'a str) -> &'a str {
    match (kind, code) {
        ("invalid_request_error", _) | (_, "context_length_exceeded") => "invalid_request_error",
        ("authentication_error", _) | (_, "invalid_api_key") => "authentication_error",
        ("rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota", _)
        | (_, "rate_limit_exceeded" | "insufficient_quota") => "rate_limit_error",
        ("server_error" | "api_error", _) => "api_error",
        ("overloaded_error" | "engine_overloaded_error", _) => "overloaded_error",
        (kind, _) => kind,
    }
}

/// Ends a stream that went quiet after its `finish_reason`: some servers (LM Studio, gateways)
/// keep the connection open and send no `[DONE]`. dbx waits a second for the next chunk and
/// ends the stream then; a blocking read cannot be given a deadline, so once the assembler has
/// seen a `finish_reason` (`armed`) the rest of the body is read on a helper thread and this
/// reader reports the end of the body when no chunk comes within `grace`. The helper ends with
/// the connection, which `abort` (the token given to the transport) closes.
pub(super) struct GraceReader<'a> {
    source: Source,
    armed: &'a Cell<bool>,
    grace: Duration,
    abort: CancelToken,
}

enum Source {
    Direct(Box<dyn Read + Send>),
    Relay(Relay),
    Gone,
}

struct Relay {
    chunks: Receiver<io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    at: usize,
    deadline: Instant,
}

impl<'a> GraceReader<'a> {
    pub fn new(
        body: Box<dyn Read + Send>,
        armed: &'a Cell<bool>,
        grace: Duration,
        abort: CancelToken,
    ) -> Self {
        Self {
            source: Source::Direct(body),
            armed,
            grace,
            abort,
        }
    }

    fn relay(&mut self) {
        let Source::Direct(mut body) = std::mem::replace(&mut self.source, Source::Gone) else {
            return;
        };
        let (sender, chunks) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("ai-stream-tail".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    let sent = match body.read(&mut buffer) {
                        Ok(0) => return,
                        Ok(read) => sender.send(Ok(buffer[..read].to_vec())),
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            return;
                        }
                    };
                    if sent.is_err() {
                        return;
                    }
                }
            });
        // No thread, no more of the body: the turn has its `finish_reason`, so it ends here.
        if spawned.is_ok() {
            self.source = Source::Relay(Relay {
                chunks,
                pending: Vec::new(),
                at: 0,
                deadline: Instant::now() + self.grace,
            });
        } else {
            self.abort.cancel_detached();
        }
    }
}

impl Read for GraceReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.armed.get() && matches!(self.source, Source::Direct(_)) {
            self.relay();
        }
        match &mut self.source {
            Source::Direct(body) => body.read(buf),
            Source::Gone => Ok(0),
            Source::Relay(relay) => {
                if relay.at >= relay.pending.len() {
                    let left = relay.deadline.saturating_duration_since(Instant::now());
                    match relay.chunks.recv_timeout(left) {
                        Ok(Ok(chunk)) => {
                            relay.pending = chunk;
                            relay.at = 0;
                        }
                        Ok(Err(error)) => return Err(error),
                        // The body ended.
                        Err(RecvTimeoutError::Disconnected) => return Ok(0),
                        // Silence: close the connection, which ends the helper.
                        Err(RecvTimeoutError::Timeout) => {
                            self.abort.cancel_detached();
                            return Ok(0);
                        }
                    }
                }
                let n = buf.len().min(relay.pending.len() - relay.at);
                buf[..n].copy_from_slice(&relay.pending[relay.at..relay.at + n]);
                relay.at += n;
                Ok(n)
            }
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn borrowed(text: &OptStr<'_>) -> bool {
        matches!(text.0, Some(Cow::Borrowed(_)))
    }

    #[test]
    fn a_delta_without_escapes_borrows_its_strings_and_a_odd_field_reads_as_absent() {
        let plain = r#"{"choices":[{"delta":{"content":"plain ü text","reasoning_content":null,"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{}"}}]},"finish_reason":null}],"usage":null}"#;
        let chunk: WireChunk<'_> = serde_json::from_str(plain).unwrap();
        let choice = chunk.choices.0.unwrap();
        let delta = choice.delta.unwrap();
        assert!(matches!(
            delta.content.0,
            Some(Cow::Borrowed("plain ü text"))
        ));
        assert!(delta.reasoning_content.0.is_none());
        let call = delta.tool_calls.first.as_ref().unwrap();
        assert!(borrowed(&call.id));
        assert!(delta.tool_calls.rest.is_empty());
        assert!(borrowed(&call.function.as_ref().unwrap().name));

        let escaped = r#"{"choices":[{"delta":{"content":"line\nbreak"}}]}"#;
        let chunk: WireChunk<'_> = serde_json::from_str(escaped).unwrap();
        let delta = chunk.choices.0.unwrap().delta.unwrap();
        assert!(matches!(delta.content.0, Some(Cow::Owned(ref t)) if t == "line\nbreak"));

        // Numbers, objects and arrays where a string is expected are absent, not an error.
        let odd = r#"{"choices":[{"delta":{"content":7,"reasoning":{"a":1},"thinking":[1,2]},"finish_reason":3}],"model":false}"#;
        let chunk: WireChunk<'_> = serde_json::from_str(odd).unwrap();
        let choice = chunk.choices.0.unwrap();
        assert!(choice.finish_reason.text().is_none());
        assert!(choice.delta.unwrap().content.0.is_none());
    }
}
