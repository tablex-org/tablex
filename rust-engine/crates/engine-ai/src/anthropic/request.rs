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
        let mut seq = serializer.serialize_seq(Some(self.specs.len()))?;
        for spec in self.specs {
            seq.serialize_element(&Tool {
                spec,
                eager: self.eager,
            })?;
        }
        seq.end()
    }
}

struct Tool<'a> {
    spec: &'a ToolSpec,
    eager: bool,
}

impl Serialize for Tool<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", self.spec.name)?;
        map.serialize_entry("description", self.spec.description)?;
        map.serialize_entry("input_schema", &self.spec.input_schema)?;
        map.serialize_entry("strict", &true)?;
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
