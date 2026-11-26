//! The body of `POST <base>/chat/completions`.
//!
//! Serialized straight from the borrowed conversation as compact JSON, with a fixed key order.
//! The roles are `system`, `user`, `assistant` (with `tool_calls`) and `tool` (with
//! `tool_call_id`). Reasoning the model showed (`reasoning`, `thinking`, `redacted_thinking`
//! blocks) is never sent back.

use super::effort::{thinking, uses_completion_tokens, Thinking};
use super::OpenAiConfig;
use crate::conversation::{Block, Content, EchoForm, Message, UserBlock};
use crate::provider::TurnRequest;
use crate::wire::Reminder;
use engine_agent::ToolSpec;
use serde::ser::{Error as _, SerializeMap, SerializeSeq, Serializer};
use serde::Serialize;
use std::borrow::Cow;

pub(super) struct Body<'a> {
    pub config: &'a OpenAiConfig,
    pub request: &'a TurnRequest<'a>,
}

impl Serialize for Body<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let request = self.request;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("model", request.model)?;
        map.serialize_entry("messages", &Messages(request))?;
        // OpenAI refuses an empty `tools` list.
        if !request.tools.is_empty() {
            map.serialize_entry("tools", &Tools(request.tools))?;
            map.serialize_entry("tool_choice", "auto")?;
        }
        map.serialize_entry("stream", &true)?;
        map.serialize_entry("stream_options", &StreamOptions { include_usage: true })?;
        let limit_field = if uses_completion_tokens(self.config, request.model) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        map.serialize_entry(limit_field, &self.config.max_tokens)?;
        match thinking(self.config, request.model, request.effort) {
            Thinking::Nothing => {}
            Thinking::Effort(word) => map.serialize_entry("reasoning_effort", word)?,
            Thinking::TemplateOff => map.serialize_entry(
                "extra_body",
                &ExtraBody {
                    chat_template_kwargs: TemplateKwargs {
                        enable_thinking: false,
                    },
                },
            )?,
        }
        map.end()
    }
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ExtraBody {
    chat_template_kwargs: TemplateKwargs,
}

#[derive(Serialize)]
struct TemplateKwargs {
    enable_thinking: bool,
}

struct Tools<'a>(&'a [ToolSpec]);

impl Serialize for Tools<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for spec in self.0 {
            seq.serialize_element(&Tool {
                kind: "function",
                function: ToolFunction {
                    name: spec.name,
                    description: spec.description,
                    parameters: &spec.input_schema,
                },
            })?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct Tool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolFunction<'a>,
}

#[derive(Serialize)]
struct ToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
struct Chat<'a> {
    role: &'static str,
    content: Cow<'a, str>,
}

#[derive(Serialize)]
struct ToolResult<'a> {
    role: &'static str,
    tool_call_id: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct Assistant<'a> {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall<'a>>,
}

#[derive(Serialize)]
struct ToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: CalledFunction<'a>,
}

#[derive(Serialize)]
struct CalledFunction<'a> {
    name: &'a str,
    /// The input as one JSON text, as the API wants it.
    arguments: String,
}

struct Messages<'a>(&'a TurnRequest<'a>);

impl Serialize for Messages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let request = self.0;
        let mut seq = serializer.serialize_seq(None)?;
        let system = &request.system;
        seq.serialize_element(&Chat {
            role: "system",
            content: match system.context {
                Some(context) => Cow::Owned(format!("{}\n\n{}", system.core, context.text)),
                None => Cow::Borrowed(system.core),
            },
        })?;
        for (index, message) in request.messages.iter().enumerate() {
            match message {
                Message::User { content } => {
                    // The note after a user message is part of it.
                    let note = match request.messages.get(index + 1) {
                        Some(Message::System { content }) => Some(content.as_str()),
                        _ => None,
                    };
                    write_user(&mut seq, content, note)?;
                }
                Message::Assistant { content } => write_assistant(&mut seq, content)?,
                // A note that follows a user message went out with it; any other is dropped.
                Message::System { .. } => {}
            }
        }
        seq.end()
    }
}

fn write_user<S: SerializeSeq>(
    seq: &mut S,
    content: &[UserBlock],
    note: Option<&str>,
) -> Result<(), S::Error> {
    let mut parts: Vec<Cow<'_, str>> = Vec::new();
    for block in content {
        match block {
            UserBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => seq.serialize_element(&ToolResult {
                role: "tool",
                tool_call_id: tool_use_id,
                content,
            })?,
            UserBlock::Text { text } if !text.is_empty() => parts.push(Cow::Borrowed(text)),
            UserBlock::Text { .. } => {}
        }
    }
    if let Some(note) = note {
        parts.push(Cow::Owned(Reminder(note).to_string()));
    }
    if parts.is_empty() {
        return Ok(());
    }
    seq.serialize_element(&Chat {
        role: "user",
        content: Cow::Owned(parts.join("\n\n")),
    })
}

fn write_assistant<S: SerializeSeq>(seq: &mut S, content: &[serde_json::Value]) -> Result<(), S::Error> {
    let content = Content(content);
    let texts: Vec<&str> = content
        .echo(EchoForm::STRICTEST)
        .map(Block)
        .filter(|block| block.kind() == "text")
        .filter_map(|block| block.str("text"))
        .collect();
    let mut tool_calls = Vec::new();
    for tool in content.tool_uses() {
        tool_calls.push(ToolCall {
            id: tool.id,
            kind: "function",
            function: CalledFunction {
                name: tool.name,
                arguments: serde_json::to_string(tool.input).map_err(S::Error::custom)?,
            },
        });
    }
    if texts.is_empty() && tool_calls.is_empty() {
        return Ok(());
    }
    seq.serialize_element(&Assistant {
        role: "assistant",
        content: texts.join("\n"),
        tool_calls,
    })
}
