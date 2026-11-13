//! The request body of `POST /v1/messages`.
//!
//! Serialized straight from the borrowed conversation, with a fixed field order, so the part that
//! is cached (tools, then system) is byte-identical from request to request: nothing in it depends
//! on time, ids or the messages. `cache_control` goes on the context block of the system prompt
//! (when there is a schema digest) and on the last block of the last user message; never more than
//! two breakpoints, the API allows four.

use super::{AnthropicConfig, Wire};
use crate::conversation::{Content, EchoForm, Message, UserBlock};
use crate::provider::TurnRequest;
use crate::wire::Reminder;
use engine_agent::ToolSpec;
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

pub(crate) const NATIVE_BASE_URL: &str = "https://api.anthropic.com";
pub(crate) const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
pub(crate) const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

pub(crate) struct Body<'a> {
    pub config: &'a AnthropicConfig,
    pub request: &'a TurnRequest<'a>,
    /// The optional features this request carries (the configured ones the API has not refused).
    pub wire: Wire,
}

impl Serialize for Body<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let native = self.config.is_native();
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("model", self.request.model)?;
        map.serialize_entry("max_tokens", &self.config.max_tokens)?;
        map.serialize_entry("stream", &true)?;
        map.serialize_entry(
            "thinking",
            &Thinking {
                drop_mismatched: self.wire.thinking_binding,
            },
        )?;
        map.serialize_entry("output_config", &OutputConfig(self.request.effort.as_str()))?;
        if self.wire.fallbacks {
            map.serialize_entry("fallbacks", "default")?;
        }
        map.serialize_entry(
            "tools",
            &Tools {
                specs: self.request.tools,
                eager: native,
            },
        )?;
        map.serialize_entry("system", &System(self.request))?;
        map.serialize_entry(
            "messages",
            &Messages {
                messages: self.request.messages,
                drop_thinking_before: self.request.history.drop_thinking_before,
                keep_fallback: self.wire.fallbacks,
                notes_in_user_turn: self.request.history.notes_in_user_turn,
            },
        )?;
        map.end()
    }
}

/// `thinking`: adaptive, with a summary. With `drop_mismatched` (the real API, under the beta
/// header [`THINKING_BINDING_BETA`]) the API drops a thinking block that no longer matches the
/// conversation before it instead of refusing the request.
struct Thinking {
    drop_mismatched: bool,
}

impl Serialize for Thinking {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", "adaptive")?;
        map.serialize_entry("display", "summarized")?;
        if self.drop_mismatched {
            map.serialize_entry("block_binding", &BlockBinding)?;
        }
        map.end()
    }
}

struct BlockBinding;

impl Serialize for BlockBinding {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("prefix_mismatch_behavior", "drop_block")?;
        map.end()
    }
}

struct OutputConfig(&'static str);

impl Serialize for OutputConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("effort", self.0)?;
        map.end()
    }
}

struct Ephemeral;

impl Serialize for Ephemeral {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("type", "ephemeral")?;
        map.end()
    }
}

struct Tools<'a> {
    specs: &'a [ToolSpec],
    eager: bool,
}

impl Serialize for Tools<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let strict = strict_tools(self.specs);
        let mut seq = serializer.serialize_seq(Some(self.specs.len()))?;
        for (spec, strict) in self.specs.iter().zip(strict) {
            seq.serialize_element(&Tool {
                spec,
                eager: self.eager,
                strict,
            })?;
        }
        seq.end()
    }
}

/// The API's complexity limits for strict schemas, summed over every `strict: true` tool of a
/// request (structured outputs docs, "Schema complexity limits"); over them the request is a 400.
pub(crate) const MAX_STRICT_TOOLS: usize = 20;
pub(crate) const MAX_STRICT_OPTIONAL: usize = 24;
pub(crate) const MAX_STRICT_UNIONS: usize = 16;

/// Tools whose input runs something on the database: strict first, so their arguments always
/// match the schema; the rest are strict while the budget lasts.
const STRICT_FIRST: [&str; 3] = ["execute_query", "execute_redis_command", "explain_query"];

/// Optional and union-typed parameters of a schema, nested objects and array items included (how
/// the API counts them).
pub(crate) fn schema_cost(schema: &serde_json::Value) -> (usize, usize) {
    let mut cost = (0, 0);
    let Some(properties) = schema.get("properties").and_then(serde_json::Value::as_object) else {
        return cost;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|names| names.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    for (name, property) in properties {
        cost.0 += usize::from(!required.contains(&name.as_str()));
        cost.1 += usize::from(property["type"].is_array() || property.get("anyOf").is_some());
        for nested in [Some(property), property.get("items")].into_iter().flatten() {
            let (optional, unions) = schema_cost(nested);
            cost.0 += optional;
            cost.1 += unions;
        }
    }
    cost
}

/// Which of `specs` (same order) go out with `strict: true`: the [`STRICT_FIRST`] tools, then the
/// others in list order, each one only while the request stays inside the limits. Depends on the
/// specs alone, so the cached tools block stays byte-identical between requests.
pub(crate) fn strict_tools(specs: &[ToolSpec]) -> Vec<bool> {
    let mut strict = vec![false; specs.len()];
    let (mut tools, mut optional, mut unions) = (0, 0, 0);
    let first = |spec: &ToolSpec| STRICT_FIRST.contains(&spec.name);
    let order = (0..specs.len())
        .filter(|&i| first(&specs[i]))
        .chain((0..specs.len()).filter(|&i| !first(&specs[i])));
    for index in order {
        let (o, u) = schema_cost(&specs[index].input_schema);
        if tools < MAX_STRICT_TOOLS
            && optional + o <= MAX_STRICT_OPTIONAL
            && unions + u <= MAX_STRICT_UNIONS
        {
            strict[index] = true;
            tools += 1;
            optional += o;
            unions += u;
        }
    }
    strict
}

struct Tool<'a> {
    spec: &'a ToolSpec,
    eager: bool,
    strict: bool,
}

impl Serialize for Tool<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", self.spec.name)?;
        map.serialize_entry("description", self.spec.description)?;
        map.serialize_entry("input_schema", &self.spec.input_schema)?;
        if self.strict {
            map.serialize_entry("strict", &true)?;
        }
        if self.eager {
            map.serialize_entry("eager_input_streaming", &true)?;
        }
        map.end()
    }
}

struct System<'a>(&'a TurnRequest<'a>);

impl Serialize for System<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let system = &self.0.system;
        let mut seq = serializer.serialize_seq(None)?;
        seq.serialize_element(&TextBlock {
            text: system.core,
            cache: false,
        })?;
        if let Some(context) = system.context {
            seq.serialize_element(&TextBlock {
                text: context.text,
                cache: context.cacheable,
            })?;
        }
        seq.end()
    }
}

struct TextBlock<'a> {
    text: &'a str,
    cache: bool,
}

impl Serialize for TextBlock<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", "text")?;
        map.serialize_entry("text", self.text)?;
        if self.cache {
            map.serialize_entry("cache_control", &Ephemeral)?;
        }
        map.end()
    }
}

struct Messages<'a> {
    messages: &'a [Message],
    /// Assistant messages below this index are written without their thinking blocks.
    drop_thinking_before: usize,
    keep_fallback: bool,
    notes_in_user_turn: bool,
}

impl Serialize for Messages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let marked = self
            .messages
            .iter()
            .rposition(|message| matches!(message, Message::User { .. }));
        let mut seq = serializer.serialize_seq(None)?;
        for (index, message) in self.messages.iter().enumerate() {
            // A note that rides in the user turn has no message of its own.
            let reminder = match (self.messages.get(index + 1), self.notes_in_user_turn) {
                (Some(Message::System { content }), true) => Some(content.as_str()),
                _ => None,
            };
            if self.notes_in_user_turn && matches!(message, Message::System { .. }) {
                continue;
            }
            seq.serialize_element(&ApiMessage {
                message,
                reminder,
                echo: EchoForm {
                    drop_thinking: index < self.drop_thinking_before,
                    keep_fallback: self.keep_fallback,
                },
                cache_last: Some(index) == marked,
            })?;
        }
        seq.end()
    }
}

struct ApiMessage<'a> {
    message: &'a Message,
    reminder: Option<&'a str>,
    echo: EchoForm,
    cache_last: bool,
}

impl Serialize for ApiMessage<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        match self.message {
            Message::User { content } => {
                map.serialize_entry("role", "user")?;
                map.serialize_entry(
                    "content",
                    &UserBlocks {
                        blocks: content,
                        reminder: self.reminder,
                        cache_last: self.cache_last,
                    },
                )?;
            }
            Message::Assistant { content } => {
                map.serialize_entry("role", "assistant")?;
                map.serialize_entry("content", &Echo(content, self.echo))?;
            }
            Message::System { content } => {
                map.serialize_entry("role", "system")?;
                map.serialize_entry("content", content)?;
            }
        }
        map.end()
    }
}

struct Echo<'a>(&'a [serde_json::Value], EchoForm);

impl Serialize for Echo<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(Content(self.0).echo(self.1))
    }
}

struct UserBlocks<'a> {
    blocks: &'a [UserBlock],
    reminder: Option<&'a str>,
    cache_last: bool,
}

impl Serialize for UserBlocks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let total = self.blocks.len() + usize::from(self.reminder.is_some());
        let mut seq = serializer.serialize_seq(Some(total))?;
        for (index, block) in self.blocks.iter().enumerate() {
            seq.serialize_element(&UserBlockView {
                block,
                cache: self.cache_last && index + 1 == total,
            })?;
        }
        if let Some(note) = self.reminder {
            seq.serialize_element(&ReminderBlock {
                note,
                cache: self.cache_last,
            })?;
        }
        seq.end()
    }
}

/// An operator note as text in the user turn, for a model without `system` messages in the
/// conversation.
struct ReminderBlock<'a> {
    note: &'a str,
    cache: bool,
}

impl Serialize for ReminderBlock<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", "text")?;
        map.serialize_entry("text", &Reminder(self.note).to_string())?;
        if self.cache {
            map.serialize_entry("cache_control", &Ephemeral)?;
        }
        map.end()
    }
}

struct UserBlockView<'a> {
    block: &'a UserBlock,
    cache: bool,
}

impl Serialize for UserBlockView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        match self.block {
            UserBlock::Text { text } => {
                map.serialize_entry("type", "text")?;
                map.serialize_entry("text", text)?;
            }
            UserBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                map.serialize_entry("type", "tool_result")?;
                map.serialize_entry("tool_use_id", tool_use_id)?;
                map.serialize_entry("content", content)?;
                if *is_error {
                    map.serialize_entry("is_error", &true)?;
                }
            }
        }
        if self.cache {
            map.serialize_entry("cache_control", &Ephemeral)?;
        }
        map.end()
    }
}
