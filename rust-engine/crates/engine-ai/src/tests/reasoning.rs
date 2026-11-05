//! A conversation that went on with an OpenAI-compatible provider holds `reasoning` blocks; the
//! Anthropic request never sends them back, so the conversation can continue on Anthropic.

use crate::test_support::*;
use crate::{Conversation, ConversationError};
use serde_json::{json, Value};

fn conversation(assistant: Value) -> Result<Conversation, serde_json::Error> {
    serde_json::from_value(json!({
        "scope": {"connection_id": CONNECTION},
        "context": null,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "first"}]},
            {"role": "assistant", "content": assistant}]}))
}

#[test]
fn reasoning_blocks_of_an_openai_turn_are_not_sent_to_anthropic() {
    let mut harness = Harness::new(vec![text_reply("fine").into()]);
    harness.conversation = conversation(json!([
        {"type": "reasoning", "text": "I considered the schema."},
        {"type": "text", "text": "Answer one."}
    ]))
    .unwrap();
    harness.run("second").unwrap();
    let body = harness.transport.request(0).json();
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    let kinds: Vec<_> = assistant["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, vec!["text"]);
    let sent = harness.transport.request(0).body;
    assert!(!String::from_utf8_lossy(&sent).contains("I considered the schema."));
    // The stored turn still has it (the panel shows it).
    assert_eq!(
        harness.conversation.messages()[1],
        crate::Message::Assistant {
            content: vec![
                json!({"type": "reasoning", "text": "I considered the schema."}),
                json!({"type": "text", "text": "Answer one."})
            ]
        }
    );
}

#[test]
fn a_turn_of_only_reasoning_has_nothing_to_send_and_is_refused_like_a_thinking_only_turn() {
    let error = conversation(json!([{"type": "reasoning", "text": "hmm"}])).unwrap_err();
    assert!(error
        .to_string()
        .contains(&ConversationError::EmptyMessage(1).to_string()));
}
