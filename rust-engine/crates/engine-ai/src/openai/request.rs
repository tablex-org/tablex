// Adapted from dbx (https://github.com/t8y2/dbx), Apache-2.0; changed. See `mod.rs` and
// `licenses/README.md` at the root of the repository.

//! The body of `POST …/chat/completions`, serialized straight from the borrowed conversation.
//!
//! Field order and values follow dbx `stream_openai_with_tools`: `model`, `messages`, `tools`,
//! `tool_choice: "auto"`, `stream`, `stream_options.include_usage`, the token limit, then the
//! thinking field. The history is written as the API wants it: the system prompt first, a user
//! turn's tool results as `role: "tool"` messages before its text, an assistant turn's text and
//! `tool_calls` (arguments as a JSON string). Thinking and reasoning blocks are never sent.

use super::effort::{is_kimi_model, is_openai_api, reasoning_effort, uses_max_completion_tokens};
use super::{Flavor, OpenAiConfig};
use crate::conversation::{Content, EchoForm, Message, UserBlock};
use crate::provider::TurnRequest;
use crate::wire::Reminder;
use engine_agent::ToolSpec;
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use std::fmt;

pub(super) struct Body<'a> {
    pub config: &'a OpenAiConfig,
    pub request: &'a TurnRequest<'a>,
}

/// The one field that says how much the model thinks.
enum ThinkingField {
    None,
    /// `reasoning_effort`.
    Effort(&'static str),
    /// `extra_body.chat_template_kwargs.enable_thinking: false`, for servers that take it.
    TemplateOff,
}

impl Body<'_> {
    /// dbx `apply_chat_completion_thinking_toggle` (thinking off) and `apply_openai_effort`
    /// (thinking on).
    fn thinking(&self) -> ThinkingField {
        let (config, model) = (self.config, self.request.model);
        if config.enable_thinking {
            return reasoning_effort(config.flavor, model, self.request.effort)
                .map_or(ThinkingField::None, ThinkingField::Effort);
        }
        if is_openai_api(config.flavor, &config.base_url) {
            // `extra_body` is no OpenAI parameter; OpenAI models keep their defaults.
            ThinkingField::None
        } else if config.flavor == Flavor::Ollama {
            ThinkingField::Effort("none")
        } else if is_kimi_model(model) {
            ThinkingField::None
        } else {
            ThinkingField::TemplateOff
        }
    }
}

impl Serialize for Body<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (config, request) = (self.config, self.request);
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("model", request.model)?;
        map.serialize_entry("messages", &Messages(request))?;
        if !request.tools.is_empty() {
            map.serialize_entry("tools", &Tools(request.tools))?;
            map.serialize_entry("tool_choice", "auto")?;
        }
        map.serialize_entry("stream", &true)?;
        map.serialize_entry("stream_options", &IncludeUsage)?;
        let limit = if uses_max_completion_tokens(config.flavor, &config.base_url, request.model) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        map.serialize_entry(limit, &config.max_tokens)?;
        match self.thinking() {
            ThinkingField::None => {}
            ThinkingField::Effort(value) => map.serialize_entry("reasoning_effort", value)?,
            ThinkingField::TemplateOff => map.serialize_entry("extra_body", &TemplateOff)?,
        }
        map.end()
    }
}

struct IncludeUsage;

impl Serialize for IncludeUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("include_usage", &true)?;
        map.end()
    }
}

struct TemplateOff;

impl Serialize for TemplateOff {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("chat_template_kwargs", &EnableThinkingOff)?;
        map.end()
    }
}

struct EnableThinkingOff;

impl Serialize for EnableThinkingOff {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("enable_thinking", &false)?;
        map.end()
    }
}

struct Tools<'a>(&'a [ToolSpec]);

impl Serialize for Tools<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for spec in self.0 {
            seq.serialize_element(&Tool(spec))?;
        }
        seq.end()
    }
}

/// dbx `ToolDefinition::to_openai_tool`.
struct Tool<'a>(&'a ToolSpec);

impl Serialize for Tool<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("type", "function")?;
        map.serialize_entry("function", &Function(self.0))?;
        map.end()
    }
}

struct Function<'a>(&'a ToolSpec);

impl Serialize for Function<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("name", self.0.name)?;
        map.serialize_entry("description", self.0.description)?;
        map.serialize_entry("parameters", &self.0.input_schema)?;
        map.end()
    }
}

/// `{"role": .., "content": ..}` with the content written from pieces, without joining them
/// first.
fn role_message<S: Serializer>(
    serializer: S,
    role: &str,
    content: &impl fmt::Display,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(2))?;
    map.serialize_entry("role", role)?;
    map.serialize_entry("content", &Collect(content))?;
    map.end()
}

/// A `Display` written as one JSON string (escaped as it is formatted).
struct Collect<'a, T>(&'a T);

impl<T: fmt::Display> Serialize for Collect<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self.0)
    }
}

struct Messages<'a>(&'a TurnRequest<'a>);

impl Serialize for Messages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let system = &self.0.system;
        let mut seq = serializer.serialize_seq(None)?;
        seq.serialize_element(&SystemMessage {
            core: system.core,
            context: system.context.map(|context| context.text),
        })?;
        let messages = self.0.messages;
        for (index, message) in messages.iter().enumerate() {
            match message {
                Message::User { content } => {
                    // A note rides in the user turn it follows.
                    let note = match messages.get(index + 1) {
                        Some(Message::System { content }) => Some(content.as_str()),
                        _ => None,
                    };
                    for block in content {
                        if let UserBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = block
                        {
                            seq.serialize_element(&ToolMessage {
                                id: tool_use_id,
                                content,
                            })?;
                        }
                    }
                    let user = UserText {
                        blocks: content,
                        note,
                    };
                    if !user.is_empty() {
                        seq.serialize_element(&user)?;
                    }
                }
                Message::Assistant { content } => {
                    let assistant = Assistant(content);
                    if assistant.has_content() {
                        seq.serialize_element(&assistant)?;
                    }
                }
                Message::System { .. } => {}
            }
        }
        seq.end()
    }
}

/// The system prompt: the core, then the context, as one message.
struct SystemMessage<'a> {
    core: &'a str,
    context: Option<&'a str>,
}

impl fmt::Display for SystemMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.core)?;
        if let Some(context) = self.context {
            write!(f, "\n\n{context}")?;
        }
        Ok(())
    }
}

impl Serialize for SystemMessage<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        role_message(serializer, "system", self)
    }
}

struct ToolMessage<'a> {
    id: &'a str,
    content: &'a str,
}

impl Serialize for ToolMessage<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("role", "tool")?;
        map.serialize_entry("tool_call_id", self.id)?;
        map.serialize_entry("content", self.content)?;
        map.end()
    }
}

/// The text of a user turn: its text blocks and the note that follows, blank line between.
struct UserText<'a> {
    blocks: &'a [UserBlock],
    note: Option<&'a str>,
}

impl UserText<'_> {
    fn texts(&self) -> impl Iterator<Item = &str> {
        self.blocks.iter().filter_map(|block| match block {
            UserBlock::Text { text } => Some(text.as_str()),
            UserBlock::ToolResult { .. } => None,
        })
    }

    fn is_empty(&self) -> bool {
        self.note.is_none() && self.texts().next().is_none()
    }
}

impl fmt::Display for UserText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        let mut part = |f: &mut fmt::Formatter<'_>, text: &dyn fmt::Display| {
            if !std::mem::take(&mut first) {
                f.write_str("\n\n")?;
            }
            write!(f, "{text}")
        };
        for text in self.texts() {
            part(f, &text)?;
        }
        if let Some(note) = self.note {
            part(f, &Reminder(note))?;
        }
        Ok(())
    }
}

impl Serialize for UserText<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        role_message(serializer, "user", self)
    }
}

/// An assistant turn as it goes back: the text, and the tool calls. What the API will see again
/// is what [`Content::echo`] keeps in its strictest form; thinking, reasoning and markers are not
/// part of it.
struct Assistant<'a>(&'a [serde_json::Value]);

impl Assistant<'_> {
    fn texts(&self) -> impl Iterator<Item = &str> {
        Content(self.0)
            .echo(EchoForm::STRICTEST)
            .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
    }

    fn has_content(&self) -> bool {
        self.texts().next().is_some() || Content(self.0).tool_uses().next().is_some()
    }
}

impl fmt::Display for Assistant<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, text) in self.texts().enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            f.write_str(text)?;
        }
        Ok(())
    }
}

impl Serialize for Assistant<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut calls = Content(self.0).tool_uses().peekable();
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("role", "assistant")?;
        // As dbx: a turn of only tool calls has an empty string as its content.
        map.serialize_entry("content", &Collect(self))?;
        if calls.peek().is_some() {
            map.serialize_entry("tool_calls", &ToolCalls(self.0))?;
        }
        map.end()
    }
}

struct ToolCalls<'a>(&'a [serde_json::Value]);

impl Serialize for ToolCalls<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(None)?;
        for call in Content(self.0).tool_uses() {
            seq.serialize_element(&ToolCall {
                id: call.id,
                name: call.name,
                input: call.input,
            })?;
        }
        seq.end()
    }
}

struct ToolCall<'a> {
    id: &'a str,
    name: &'a str,
    input: &'a serde_json::Value,
}

impl Serialize for ToolCall<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("id", self.id)?;
        map.serialize_entry("type", "function")?;
        map.serialize_entry(
            "function",
            &CalledFunction {
                name: self.name,
                input: self.input,
            },
        )?;
        map.end()
    }
}

/// `{"name": .., "arguments": "<the input as JSON text>"}`.
struct CalledFunction<'a> {
    name: &'a str,
    input: &'a serde_json::Value,
}

impl Serialize for CalledFunction<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("name", self.name)?;
        // `Value`'s `Display` is compact JSON; it is written as one escaped string.
        map.serialize_entry("arguments", &Collect(self.input))?;
        map.end()
    }
}
