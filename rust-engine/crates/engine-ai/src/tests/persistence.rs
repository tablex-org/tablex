//! What goes back to the API is what came from it, and a stored conversation sends the same bytes.

use crate::test_support::*;
use crate::{Conversation, ConversationError, Message};
use serde_json::{json, Value};

/// A turn with every kind of block, two tool calls and an unknown block, as the API would send it.
fn rich_turn() -> Reply {
    Reply::ok(
        Stream::new()
            .thinking(0, &["Let me ", "think."], "c2lnbmF0dXJl")
            .block(
                1,
                json!({"type": "redacted_thinking", "data": "EmEBCkgIARABGAIiQ"}),
            )
            .stop(1)
            .text(2, &["I will ", "check two things."])
            .tool(
                3,
                "toolu_a",
                "list_tables",
                &[r#"{"connec"#, r#"tion_id":"c1"}"#],
            )
            .tool(
                4,
                "toolu_b",
                "execute_query",
                &[r#"{"connection_id":"c1","statement":"SELECT 1"}"#],
            )
            .block(
                5,
                json!({"type": "future_block", "payload": {"z": [1, {"y": null}], "a": true}}),
            )
            .stop(5)
            .end("tool_use"),
    )
}

fn rich_blocks() -> Vec<Value> {
    vec![
        json!({"type": "thinking", "thinking": "Let me think.", "signature": "c2lnbmF0dXJl"}),
        json!({"type": "redacted_thinking", "data": "EmEBCkgIARABGAIiQ"}),
        json!({"type": "text", "text": "I will check two things."}),
        json!({"type": "tool_use", "id": "toolu_a", "name": "list_tables",
               "input": {"connection_id": "c1"}}),
        json!({"type": "tool_use", "id": "toolu_b", "name": "execute_query",
               "input": {"connection_id": "c1", "statement": "SELECT 1"}}),
        json!({"type": "future_block", "payload": {"z": [1, {"y": null}], "a": true}}),
    ]
}

#[test]
fn an_assistant_turn_is_sent_back_unchanged_in_the_next_request() {
    let mut harness = Harness::new(vec![rich_turn().into(), text_reply("Both done.").into()]);
    harness.run("go").unwrap();
    let Message::Assistant { content } = &harness.conversation.messages()[1] else {
        panic!()
    };
    assert_eq!(content, &rich_blocks());
    let second = harness.transport.request(1).json();
    assert_eq!(second["messages"][1]["role"], "assistant");
    assert_eq!(
        second["messages"][1]["content"],
        Value::Array(rich_blocks())
    );
    // The block order and the thinking text/signature are byte-exact in the wire text too.
    let text = String::from_utf8(harness.transport.request(1).body).unwrap();
    assert!(text.contains(r#""signature":"c2lnbmF0dXJl""#));
    assert!(text.contains(r#""thinking":"Let me think.""#));
    // One user message holds both results, in the order of the calls.
    let results = second["messages"][2]["content"].as_array().unwrap();
    assert_eq!(
        results
            .iter()
            .map(|r| r["tool_use_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["toolu_a", "toolu_b"]
    );
    assert!(results[0].get("cache_control").is_none());
    assert_eq!(results[1]["cache_control"], json!({"type": "ephemeral"}));
}

#[test]
fn after_a_mid_output_fallback_only_text_before_the_fallback_block_goes_back() {
    let fallback = Reply::ok(
        Stream::new()
            .thinking(0, &["first model thinks"], "SIG_OLD")
            .text(1, &["partial from the first model"])
            .tool(
                2,
                "toolu_old",
                "list_tables",
                &[r#"{"connection_id":"c1"}"#],
            )
            .block(
                3,
                json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}),
            )
            .stop(3)
            .thinking(4, &["second model thinks"], "SIG_NEW")
            .tool(
                5,
                "toolu_new",
                "list_tables",
                &[r#"{"connection_id":"c1"}"#],
            )
            .end("tool_use"),
    );
    let mut harness = Harness::new(vec![fallback.into(), text_reply("ok").into()]);
    harness.run("go").unwrap();
    // The stored turn is verbatim ...
    let Message::Assistant { content } = &harness.conversation.messages()[1] else {
        panic!()
    };
    assert_eq!(content.len(), 6);
    // ... only the call after the boundary ran and was answered ...
    let Message::User { content } = &harness.conversation.messages()[2] else {
        panic!()
    };
    assert_eq!(content.len(), 1);
    // ... and the request drops what the API wants dropped.
    let sent = harness.transport.request(1).json();
    let types: Vec<&str> = sent["messages"][1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["text", "fallback", "thinking", "tool_use"]);
    assert_eq!(sent["messages"][1]["content"][3]["id"], "toolu_new");
    assert_eq!(
        sent["messages"][2]["content"][0]["tool_use_id"],
        "toolu_new"
    );
}

#[test]
fn a_stored_conversation_sends_byte_identical_requests() {
    let mut original = Harness::new(vec![rich_turn().into(), text_reply("Both done.").into()]);
    original.run("go").unwrap();
    original
        .conversation
        .note("The user switched to schema `ops`.");
    // An error result and a note are part of the stored conversation too.
    let stored = serde_json::to_string(&original.conversation).unwrap();
    let restored: Conversation = serde_json::from_str(&stored).unwrap();
    assert_eq!(restored, original.conversation);
    assert_eq!(
        serde_json::to_string(&restored).unwrap(),
        stored,
        "stable text"
    );

    let mut copy = Harness::new(vec![text_reply("next").into()]);
    copy.conversation = restored;
    original.transport.push(text_reply("next"));
    original.run("next question").unwrap();
    copy.run("next question").unwrap();
    assert_eq!(
        original.last_request().body,
        copy.last_request().body,
        "the request after a storage round trip is byte-identical"
    );
    assert_eq!(copy.last_request().json()["messages"][5]["role"], "system");
}

#[test]
fn a_conversation_with_every_message_kind_round_trips() {
    let mut harness = Harness::new(vec![
        rich_turn().into(),
        text_reply("Both done.").into(),
        text_reply("Ok.").into(),
    ]);
    harness.conversation.note("note before the first message");
    harness.run("go").unwrap();
    harness.run("and again").unwrap();
    let stored = serde_json::to_value(&harness.conversation).unwrap();
    let restored: Conversation = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(restored, harness.conversation);
    assert_eq!(serde_json::to_value(&restored).unwrap(), stored);
    let kinds: Vec<&str> = stored["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "user",
            "system",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
    assert_eq!(harness.conversation.messages().len(), 7);
}

#[test]
fn a_stored_conversation_that_breaks_the_rules_is_refused() {
    let load = |messages: Value| {
        serde_json::from_value::<Conversation>(
            json!({"scope": null, "context": null, "messages": messages}),
        )
    };
    let user = json!({"role": "user", "content": [{"type": "text", "text": "hi"}]});
    let note = json!({"role": "system", "content": "n"});
    let assistant = json!({"role": "assistant", "content": [{"type": "text", "text": "yo"}]});
    let asks = json!({"role": "assistant", "content": [
        {"type": "tool_use", "id": "t1", "name": "list_connections", "input": {}}]});

    assert!(load(json!([user, note.clone(), assistant.clone()])).is_ok());
    assert!(load(json!([])).is_ok());
    let rejected = |messages: Value, expected: ConversationError| {
        let error = load(messages).unwrap_err().to_string();
        assert!(
            error.contains(&expected.to_string()),
            "{error} / {expected}"
        );
    };
    rejected(
        json!([note.clone(), user.clone()]),
        ConversationError::FirstNotUser(0),
    );
    rejected(
        json!([assistant.clone()]),
        ConversationError::FirstNotUser(0),
    );
    rejected(
        json!([
            user.clone(),
            assistant.clone(),
            note.clone(),
            assistant.clone()
        ]),
        ConversationError::NoteNotAfterUser(2),
    );
    rejected(
        json!([user.clone(), note.clone(), user.clone()]),
        ConversationError::NoteNotBeforeAssistant(1),
    );
    rejected(
        json!([user.clone(), asks.clone()]),
        ConversationError::UnansweredToolUse(1),
    );
    rejected(
        json!([user.clone(), asks, {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "other", "content": "x"}]}]),
        ConversationError::UnansweredToolUse(1),
    );
    rejected(
        json!([user, {"role": "assistant", "content": []}]),
        ConversationError::EmptyMessage(1),
    );
    // A note may close the conversation (it waits for the model's answer).
    assert!(load(json!([
        {"role": "user", "content": [{"type": "text", "text": "hi"}]}, note
    ]))
    .is_ok());
}
