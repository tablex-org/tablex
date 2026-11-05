//! The exact request bodies, headers and cache layout.

use crate::anthropic::AnthropicConfig;
use crate::prompt::CORE_PROMPT;
use crate::provider::Effort;
use crate::test_support::*;
use crate::UserBlock;
use engine_agent::AgentAccess;
use serde_json::{json, Value};

fn expected_tools(harness: &Harness, eager: bool) -> Value {
    Value::Array(
        harness
            .layer
            .tools()
            .iter()
            .map(|spec| {
                let mut tool = json!({
                    "name": spec.name,
                    "description": spec.description,
                    "input_schema": spec.input_schema,
                    "strict": true,
                });
                if eager {
                    tool["eager_input_streaming"] = json!(true);
                }
                tool
            })
            .collect(),
    )
}

/// The bytes of a request body before `"messages":` (the cached prefix: tools and system).
fn prefix(body: &[u8]) -> &[u8] {
    let text = std::str::from_utf8(body).unwrap();
    &body[..text.find("\"messages\":").unwrap()]
}

fn cache_marks(body: &[u8]) -> usize {
    String::from_utf8_lossy(body)
        .matches("\"cache_control\"")
        .count()
}

fn tool_result_text(harness: &Harness, message: usize) -> String {
    let crate::Message::User { content } = &harness.conversation.messages()[message] else {
        panic!("message {message} is not a user message");
    };
    let UserBlock::ToolResult { content, .. } = &content[0] else {
        panic!("not a tool result");
    };
    content.clone()
}

#[test]
fn the_first_request_body_is_exactly_as_designed() {
    let mut harness = Harness::new(vec![text_reply("There are 3.").into()]);
    harness.run("How many users?").unwrap();
    let request = harness.transport.request(0);
    let context = harness.conversation.context().unwrap().text.clone();
    assert!(context.starts_with("Active connection id: c1\n"));
    assert!(
        context.contains("users"),
        "the digest names the table: {context}"
    );
    let expected = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 64000,
        "stream": true,
        "thinking": {"type": "adaptive", "display": "summarized",
                     "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
        "output_config": {"effort": "medium"},
        "fallbacks": "default",
        "tools": expected_tools(&harness, true),
        "system": [
            {"type": "text", "text": CORE_PROMPT},
            {"type": "text", "text": context, "cache_control": {"type": "ephemeral"}},
        ],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "How many users?", "cache_control": {"type": "ephemeral"}}
            ]},
        ],
    });
    assert_eq!(request.json(), expected);
    // Field order is part of the contract: the cached prefix comes first.
    let text = String::from_utf8(request.body.clone()).unwrap();
    let positions: Vec<usize> = [
        "\"model\"",
        "\"max_tokens\"",
        "\"stream\"",
        "\"thinking\"",
        "\"output_config\"",
        "\"fallbacks\"",
        "\"tools\"",
        "\"system\"",
        "\"messages\"",
    ]
    .iter()
    .map(|key| text.find(key).unwrap())
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{positions:?}"
    );
    for forbidden in [
        "tool_choice",
        "temperature",
        "top_p",
        "top_k",
        "budget_tokens",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} must not be sent");
    }
}

#[test]
fn the_headers_are_exact_and_the_debug_output_hides_the_key() {
    let mut harness = Harness::new(vec![text_reply("ok").into()]);
    harness.run("hi").unwrap();
    let request = harness.transport.request(0);
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, "https://api.anthropic.com/v1/messages");
    let headers: Vec<(&str, &str)> = request
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        headers,
        vec![
            ("x-api-key", FAKE_KEY),
            ("anthropic-version", "2023-06-01"),
            ("content-type", "application/json"),
            (
                "anthropic-beta",
                "server-side-fallback-2026-07-01,thinking-binding-controls-2026-08-01"
            ),
        ]
    );
    assert!(
        request.debug.contains("\"***\"") || request.debug.contains("***"),
        "{}",
        request.debug
    );
    assert!(!request.debug.contains(FAKE_KEY));
}

#[test]
fn the_second_request_after_a_tool_round_matches_and_shares_the_prefix_byte_for_byte() {
    let mut harness = Harness::new(vec![
        tool_reply(&[("toolu_a", "list_tables", r#"{"connection_id":"c1"}"#)]).into(),
        text_reply("One table.").into(),
    ]);
    harness.run("What tables exist?").unwrap();
    assert_eq!(harness.transport.request_count(), 2);
    let (first, second) = (harness.transport.request(0), harness.transport.request(1));
    let context = harness.conversation.context().unwrap().text.clone();
    let result_text = tool_result_text(&harness, 2);
    assert!(result_text.contains("users"));
    let expected = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 64000,
        "stream": true,
        "thinking": {"type": "adaptive", "display": "summarized",
                     "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
        "output_config": {"effort": "medium"},
        "fallbacks": "default",
        "tools": expected_tools(&harness, true),
        "system": [
            {"type": "text", "text": CORE_PROMPT},
            {"type": "text", "text": context, "cache_control": {"type": "ephemeral"}},
        ],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "What tables exist?"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_a", "name": "list_tables",
                 "input": {"connection_id": "c1"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": result_text,
                 "cache_control": {"type": "ephemeral"}},
            ]},
        ],
    });
    assert_eq!(second.json(), expected);
    assert_eq!(
        prefix(&first.body),
        prefix(&second.body),
        "tools+system+settings are byte-identical"
    );
    assert_eq!(cache_marks(&first.body), 2);
    assert_eq!(cache_marks(&second.body), 2);
    assert!(cache_marks(&second.body) <= 4);
}

#[test]
fn a_gateway_gets_neither_eager_streaming_nor_fallbacks_nor_the_beta_header() {
    let config = AnthropicConfig {
        base_url: "https://gateway.example/anthropic/".into(),
        ..AnthropicConfig::default()
    };
    let mut harness = Harness::with(
        vec![text_reply("ok").into()],
        config,
        Some(AgentAccess::Read),
        true,
    );
    harness.run("hi").unwrap();
    let request = harness.transport.request(0);
    assert_eq!(request.url, "https://gateway.example/anthropic/v1/messages");
    let text = String::from_utf8(request.body.clone()).unwrap();
    assert!(!text.contains("eager_input_streaming"));
    assert!(!text.contains("fallbacks"));
    assert!(request.header("anthropic-beta").is_none());
    let body = request.json();
    assert_eq!(body["tools"], expected_tools(&harness, false));
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
}

#[test]
fn switching_the_fallback_off_drops_the_field_and_the_header() {
    let config = AnthropicConfig {
        server_fallback: false,
        ..AnthropicConfig::default()
    };
    let mut harness = Harness::with(
        vec![text_reply("ok").into()],
        config,
        Some(AgentAccess::Read),
        true,
    );
    harness.run("hi").unwrap();
    let request = harness.transport.request(0);
    assert!(request.json().get("fallbacks").is_none());
    assert_eq!(
        request.header("anthropic-beta"),
        Some("thinking-binding-controls-2026-08-01")
    );
    assert!(String::from_utf8(request.body)
        .unwrap()
        .contains("eager_input_streaming"));
}

#[test]
fn effort_is_always_explicit() {
    for (effort, name) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::XHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let mut harness = Harness::new(vec![text_reply("ok").into()]);
        harness.config.effort = effort;
        harness.run("hi").unwrap();
        assert_eq!(
            harness.last_request().json()["output_config"],
            json!({"effort": name})
        );
    }
}

#[test]
fn without_a_schema_digest_only_the_last_user_turn_is_a_breakpoint() {
    // Access `off`: the context cannot be taken; the conversation runs on and says so.
    let mut harness = Harness::with(
        vec![text_reply("ok").into()],
        AnthropicConfig::default(),
        Some(AgentAccess::Off),
        true,
    );
    harness.run("hi").unwrap();
    assert!(
        harness
            .log
            .iter()
            .any(|e| e.starts_with("ContextUnavailable")),
        "{:?}",
        harness.log
    );
    let body = harness.last_request().json();
    let system = body["system"].as_array().unwrap();
    assert_eq!(system.len(), 2);
    assert!(system[1].get("cache_control").is_none());
    assert!(system[1]["text"]
        .as_str()
        .unwrap()
        .contains("No schema summary is available"));
    assert_eq!(cache_marks(&harness.last_request().body), 1);
}

#[test]
fn an_unscoped_conversation_has_the_core_prompt_alone() {
    let mut harness = Harness::with(
        vec![text_reply("ok").into()],
        AnthropicConfig::default(),
        Some(AgentAccess::Read),
        false,
    );
    harness.run("hi").unwrap();
    assert_eq!(
        harness.last_request().json()["system"],
        json!([{"type": "text", "text": CORE_PROMPT}])
    );
}

#[test]
fn operator_notes_become_system_messages_right_behind_the_next_user_message() {
    let mut harness = Harness::new(vec![text_reply("a").into(), text_reply("b").into()]);
    // Queued before the first message: it still cannot be first.
    harness.conversation.note("The user opened schema `sales`.");
    harness.conversation.note("Prefer short answers.");
    harness.run("first").unwrap();
    harness
        .conversation
        .note("The user switched to schema `ops`.");
    harness.run("second").unwrap();
    let first = harness.transport.request(0).json();
    assert_eq!(
        first["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "first",
                                           "cache_control": {"type": "ephemeral"}}]},
            {"role": "system", "content": "The user opened schema `sales`.\nPrefer short answers."},
        ])
    );
    let second = harness.transport.request(1).json();
    let messages = second["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "system", "assistant", "user", "system"]);
    assert_eq!(messages[4]["content"], "The user switched to schema `ops`.");
    // The breakpoint is on the last *user* turn, not on the note.
    assert_eq!(
        messages[3]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert!(messages[0]["content"][0].get("cache_control").is_none());
    assert_eq!(cache_marks(&harness.transport.request(1).body), 2);
    assert_eq!(
        prefix(&harness.transport.request(0).body),
        prefix(&harness.transport.request(1).body)
    );
    assert!(harness.conversation.pending_notes().is_empty());
}

#[test]
fn blank_notes_and_empty_input_are_ignored_or_refused() {
    let mut harness = Harness::new(vec![]);
    harness.conversation.note("   ");
    assert!(harness.conversation.pending_notes().is_empty());
    let error = harness.run("   ").unwrap_err();
    assert!(matches!(error, crate::AssistantError::EmptyInput));
    assert_eq!(harness.transport.request_count(), 0);
    assert!(harness.conversation.is_empty());
}
