//! The SSE parser and the turn assembly, below the transport.

use super::sse::SseParser;
use super::stream::{StreamError, TurnAssembler, TurnLimits};
use crate::provider::{ProtocolError, StopReason, TurnResponse};
use crate::test_support::Stream;
use serde_json::{json, Value};

/// The events (as text) and the turn a byte stream produces when delivered in `chunks`.
fn assemble(chunks: &[&[u8]]) -> Result<(Vec<String>, TurnResponse), StreamError> {
    let mut parser = SseParser::default();
    let mut assembler = TurnAssembler::new(TurnLimits::default());
    let mut events = Vec::new();
    let mut finished = false;
    for chunk in chunks {
        if finished {
            break;
        }
        finished = parser.feed::<StreamError>(chunk, &mut |event| {
            assembler.handle(event.data, &mut |e| events.push(format!("{e:?}")))
        })?;
    }
    assert!(finished, "the fixture ends with message_stop");
    let turn = assembler
        .finish(Some("req_x".into()))
        .map_err(StreamError::Protocol)?;
    Ok((events, turn))
}

/// A stream with every event type and block kind the assembler knows, plus noise.
fn fixture() -> String {
    Stream::new()
        .raw(": a comment line\n\n")
        .ping()
        .thinking(0, &["Hmm, ü", " — 日本"], "SIG==")
        .block(1, json!({"type": "redacted_thinking", "data": "ENC"}))
        .stop(1)
        .raw("event: ping\ndata: {\"type\":\ndata: \"ping\"}\n\n")
        .block(2, json!({"type": "text", "text": ""}))
        .delta(
            2,
            json!({"type": "text_delta", "text": "Hello \"world\"\nline2 "}),
        )
        .delta(2, json!({"type": "citations_delta", "citation": {"x": 1}}))
        .delta(2, json!({"type": "text_delta", "text": "😀"}))
        .stop(2)
        .tool(
            3,
            "toolu_1",
            "list_tables",
            &["{\"connec", "tion_id\": \"c1\"", "}"],
        )
        .tool(4, "toolu_2", "execute_query", &["{\"statement\": \"SELECT"])
        .block(5, json!({"type": "future_block", "payload": {"a": [1, 2]}}))
        .delta(5, json!({"type": "future_delta", "x": 1}))
        .stop(5)
        .block(
            6,
            json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}),
        )
        .stop(6)
        .end_with("refusal", json!({"stop_details": {"category": "cyber"}}))
}

fn expected_content() -> Vec<Value> {
    vec![
        json!({"type": "thinking", "thinking": "Hmm, ü — 日本", "signature": "SIG=="}),
        json!({"type": "redacted_thinking", "data": "ENC"}),
        json!({"type": "text", "text": "Hello \"world\"\nline2 😀"}),
        json!({"type": "tool_use", "id": "toolu_1", "name": "list_tables",
               "input": {"connection_id": "c1"}}),
        json!({"type": "tool_use", "id": "toolu_2", "name": "execute_query", "input": {}}),
        json!({"type": "future_block", "payload": {"a": [1, 2]}}),
        json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}),
    ]
}

#[test]
fn a_stream_with_every_event_type_becomes_one_turn() {
    let text = fixture();
    let (events, turn) = assemble(&[text.as_bytes()]).unwrap();
    assert_eq!(turn.content, expected_content());
    assert_eq!(
        turn.stop_reason,
        StopReason::Refusal {
            category: Some("cyber".into())
        }
    );
    // message_start, then message_delta's output_tokens overlays.
    assert_eq!(turn.usage.input_tokens, 100);
    assert_eq!(turn.usage.output_tokens, 55);
    assert_eq!(turn.usage.cache_read_input_tokens, 40);
    assert_eq!(turn.usage.cache_creation_input_tokens, 7);
    assert_eq!(turn.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(turn.request_id.as_deref(), Some("req_x"));
    assert_eq!(turn.invalid_tool_inputs.len(), 1);
    assert_eq!(turn.invalid_tool_inputs[0].tool_use_id, "toolu_2");
    assert_eq!(turn.invalid_tool_inputs[0].raw, "{\"statement\": \"SELECT");
    assert!(events
        .iter()
        .any(|e| e.contains("ThinkingDelta(\"Hmm, ü\")")));
    assert!(events
        .iter()
        .any(|e| e.contains("ToolCallStarted") && e.contains("toolu_1")));
    assert!(events.iter().any(|e| e.contains("ToolInputReady")
        && e.contains("toolu_2")
        && e.contains("valid: false")));
    assert!(events.iter().any(|e| e.contains("ToolInputReady")
        && e.contains("toolu_1")
        && e.contains("valid: true")));
}

#[test]
fn the_result_does_not_depend_on_the_chunking() {
    let text = fixture();
    let bytes = text.as_bytes();
    let whole = assemble(&[bytes]).unwrap();
    // Every split point (this cuts lines, `\r\n` pairs and multi-byte characters).
    for at in 0..=bytes.len() {
        let split = assemble(&[&bytes[..at], &bytes[at..]]).unwrap();
        assert_eq!(split.0, whole.0, "events differ for a split at {at}");
        assert_eq!(split.1, whole.1, "turn differs for a split at {at}");
    }
    // Byte by byte.
    let singles: Vec<&[u8]> = bytes.chunks(1).collect();
    assert_eq!(assemble(&singles).unwrap(), whole);
}

#[test]
fn crlf_and_bare_cr_line_ends_parse_the_same() {
    let text = fixture();
    let whole = assemble(&[text.as_bytes()]).unwrap();
    for (name, replaced) in [
        ("crlf", text.replace('\n', "\r\n")),
        ("cr", text.replace('\n', "\r")),
    ] {
        let bytes = replaced.as_bytes();
        assert_eq!(assemble(&[bytes]).unwrap(), whole, "{name} in one chunk");
        for at in 0..=bytes.len() {
            let split = assemble(&[&bytes[..at], &bytes[at..]]).unwrap();
            assert_eq!(split, whole, "{name} split at {at}");
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(assemble(&singles).unwrap(), whole, "{name} byte by byte");
    }
}

#[test]
fn the_sse_parser_follows_the_field_rules() {
    let mut parser = SseParser::default();
    let mut seen: Vec<(Option<String>, String)> = Vec::new();
    let input = b": comment\nevent: a\ndata: one\ndata:two\ndata\n\nid: 7\nretry: 5\ndata:  x\n\ndata:\n\nevent: lonely\n\ndata: tail";
    parser
        .feed::<ProtocolError>(input, &mut |event| {
            seen.push((event.event.map(str::to_owned), event.data.to_owned()));
            Ok(false)
        })
        .unwrap();
    // `data:` lines join with a newline; one leading space is dropped; an event without data and
    // an event still open at the end of the stream are not delivered.
    assert_eq!(
        seen,
        vec![
            (Some("a".to_owned()), "one\ntwo\n".to_owned()),
            (None, " x".to_owned()),
        ]
    );
}

#[test]
fn an_invalid_utf8_event_is_a_protocol_error() {
    let mut parser = SseParser::default();
    let result = parser.feed::<ProtocolError>(b"data: \xff\xfe\n\n", &mut |_| Ok(false));
    assert_eq!(result, Err(ProtocolError::InvalidUtf8));
}

#[test]
fn an_error_event_after_the_start_is_reported_as_a_stream_failure() {
    let text = Stream::new()
        .text(0, &["partial"])
        .error_event("overloaded_error", "Overloaded")
        .unfinished();
    let mut parser = SseParser::default();
    let mut assembler = TurnAssembler::new(TurnLimits::default());
    let error = parser
        .feed::<StreamError>(text.as_bytes(), &mut |event| {
            assembler.handle(event.data, &mut |_| {})
        })
        .unwrap_err();
    assert!(matches!(
        error,
        StreamError::Api { ref kind, ref message, before_start: false }
            if kind == "overloaded_error" && message == "Overloaded"
    ));
}

#[test]
fn an_error_event_before_the_start_says_so() {
    let text = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n";
    let mut parser = SseParser::default();
    let mut assembler = TurnAssembler::new(TurnLimits::default());
    let error = parser
        .feed::<StreamError>(text.as_bytes(), &mut |event| {
            assembler.handle(event.data, &mut |_| {})
        })
        .unwrap_err();
    assert!(matches!(
        error,
        StreamError::Api {
            before_start: true,
            ..
        }
    ));
}

#[test]
fn protocol_violations_are_typed() {
    let run = |text: &str| {
        let mut parser = SseParser::default();
        let mut assembler = TurnAssembler::new(TurnLimits::default());
        parser
            .feed::<StreamError>(text.as_bytes(), &mut |event| {
                assembler.handle(event.data, &mut |_| {})
            })
            .unwrap_err()
    };
    let before = run("data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
    assert!(matches!(
        before,
        StreamError::Protocol(ProtocolError::BeforeMessageStart(_))
    ));
    let unknown = run(&Stream::new().stop(3).unfinished());
    assert!(matches!(
        unknown,
        StreamError::Protocol(ProtocolError::UnknownBlock(3))
    ));
    let mismatch = run(&Stream::new()
        .block(0, json!({"type": "text", "text": ""}))
        .delta(0, json!({"type": "thinking_delta", "thinking": "x"}))
        .unfinished());
    assert!(matches!(
        mismatch,
        StreamError::Protocol(ProtocolError::DeltaMismatch(0))
    ));
    let malformed = run("data: {not json}\n\n");
    assert!(matches!(
        malformed,
        StreamError::Protocol(ProtocolError::MalformedEvent(_))
    ));
}

#[test]
fn a_tool_call_without_input_deltas_keeps_an_empty_object_and_text_blocks_without_text_are_dropped()
{
    let text = Stream::new()
        .block(0, json!({"type": "text", "text": ""}))
        .stop(0)
        .block(
            1,
            json!({"type": "tool_use", "id": "t", "name": "list_connections", "input": {}}),
        )
        .stop(1)
        .end("tool_use");
    let (_, turn) = assemble(&[text.as_bytes()]).unwrap();
    assert_eq!(
        turn.content,
        vec![json!({"type": "tool_use", "id": "t", "name": "list_connections", "input": {}})]
    );
    assert!(turn.invalid_tool_inputs.is_empty());
}

#[test]
fn input_that_parses_but_is_not_an_object_is_invalid() {
    let text = Stream::new()
        .tool(0, "t", "x", &["[1,", "2]"])
        .end("tool_use");
    let (_, turn) = assemble(&[text.as_bytes()]).unwrap();
    assert_eq!(turn.content[0]["input"], json!({}));
    assert_eq!(turn.invalid_tool_inputs[0].raw, "[1,2]");
}

#[test]
fn a_fallback_message_in_the_usage_iterations_marks_the_turn_as_served_by_a_fallback() {
    let make = |stop: &str| {
        let mut s = Stream::new().text(0, &["ok"]);
        s = s.raw(&format!(
            "event: message_delta\ndata: {}\n\n",
            json!({"type": "message_delta", "delta": {"stop_reason": stop},
                   "usage": {"output_tokens": 3,
                             "iterations": [{"type": "message"}, {"type": "fallback_message"}]}})
        ));
        s.raw("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
            .unfinished()
    };
    let (_, served) = assemble(&[make("end_turn").as_bytes()]).unwrap();
    assert!(served.served_by_fallback);
    // A fallback model that refuses itself did not serve the turn.
    let (_, refused) = assemble(&[make("refusal").as_bytes()]).unwrap();
    assert!(!refused.served_by_fallback);
}

fn failure(text: &str) -> StreamError {
    let mut parser = SseParser::default();
    let mut assembler = TurnAssembler::new(TurnLimits::default());
    parser
        .feed::<StreamError>(text.as_bytes(), &mut |event| {
            assembler.handle(event.data, &mut |_| {})
        })
        .unwrap_err()
}

#[test]
fn a_second_message_start_a_repeated_block_index_and_deltas_after_a_stop_are_refused() {
    assert!(matches!(
        failure(&Stream::new().raw(&Stream::new().unfinished()).unfinished()),
        StreamError::Protocol(ProtocolError::SecondMessageStart)
    ));
    let twice = Stream::new()
        .block(0, json!({"type": "text", "text": ""}))
        .block(0, json!({"type": "text", "text": ""}))
        .unfinished();
    assert!(matches!(
        failure(&twice),
        StreamError::Protocol(ProtocolError::DuplicateBlock(0))
    ));
    let late = Stream::new()
        .text(0, &["a"])
        .delta(0, json!({"type": "text_delta", "text": "b"}))
        .unfinished();
    assert!(matches!(
        failure(&late),
        StreamError::Protocol(ProtocolError::DeltaAfterStop(0))
    ));
}

#[test]
fn a_message_without_a_stop_reason_cannot_be_finished() {
    let mut parser = SseParser::default();
    let mut assembler = TurnAssembler::new(TurnLimits::default());
    let text = Stream::new()
        .text(0, &["x"])
        .raw("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
        .unfinished();
    let done = parser
        .feed::<StreamError>(text.as_bytes(), &mut |event| {
            assembler.handle(event.data, &mut |_| {})
        })
        .unwrap();
    assert!(done);
    assert_eq!(
        assembler.finish(None).unwrap_err(),
        ProtocolError::MissingStopReason
    );
}

#[test]
fn a_tool_call_needs_an_id_and_a_name_and_ids_are_unique() {
    for block in [
        json!({"type": "tool_use", "name": "x", "input": {}}),
        json!({"type": "tool_use", "id": "t", "input": {}}),
        json!({"type": "tool_use", "id": "", "name": "x", "input": {}}),
        json!({"type": "tool_use", "id": 5, "name": "x", "input": {}}),
    ] {
        let text = Stream::new().block(0, block).unfinished();
        assert!(matches!(
            failure(&text),
            StreamError::Protocol(ProtocolError::BadToolUse(0))
        ));
    }
    let text = Stream::new()
        .tool(0, "same", "list_connections", &["{}"])
        .tool(1, "same", "list_connections", &["{}"])
        .unfinished();
    assert!(matches!(
        failure(&text),
        StreamError::Protocol(ProtocolError::DuplicateToolUseId(1))
    ));
}

#[test]
fn stop_details_are_read_leniently() {
    for (details, category) in [
        (json!({"category": "cyber"}), Some("cyber")),
        (json!({"category": 5}), None),
        (json!({"category": null}), None),
        (json!(null), None),
        (json!("odd"), None),
        (json!([1]), None),
    ] {
        let text = Stream::new().end_with("refusal", json!({"stop_details": details}));
        let (_, turn) = assemble(&[text.as_bytes()]).unwrap();
        assert_eq!(
            turn.stop_reason,
            StopReason::Refusal {
                category: category.map(str::to_owned)
            }
        );
    }
}

#[test]
fn text_blocks_without_real_text_are_not_kept() {
    let text = Stream::new()
        .text(0, &[" \n"])
        .text(1, &["kept"])
        .block(2, json!({"type": "text", "text": "\t"}))
        .stop(2)
        .end("end_turn");
    let (_, turn) = assemble(&[text.as_bytes()]).unwrap();
    assert_eq!(turn.content, vec![json!({"type": "text", "text": "kept"})]);
}

#[test]
fn input_transformations_are_read_from_message_start_and_the_final_message_delta() {
    let entries = |n: usize| {
        (0..n)
            .map(|i| {
                json!({"type": "thinking_dropped", "path": format!("messages.{i}.content.0"),
                            "reason": "prefix_binding_mismatch"})
            })
            .chain([json!({"type": "thinking_mismatch_allowed", "path": "messages.9.content.0"})])
            .collect::<Vec<_>>()
    };
    // Only in the final message_delta (after a mid-stream fallback): at the top level of the event.
    let text = Stream::new()
        .text(0, &["ok"])
        .raw(&format!(
            "event: message_delta\ndata: {}\n\n",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 3}, "input_transformations": entries(3)})
        ))
        .raw("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
        .unfinished();
    let (_, turn) = assemble(&[text.as_bytes()]).unwrap();
    assert_eq!(turn.thinking_dropped, 3, "other entry types are ignored");
    // Both: the larger count stands, not the sum.
    let both = text.replacen(
        "\"content\":[]",
        &format!(
            "\"content\":[],\"input_transformations\":{}",
            json!(entries(2))
        ),
        1,
    );
    assert_eq!(assemble(&[both.as_bytes()]).unwrap().1.thinking_dropped, 3);
    // Absent: nothing was dropped.
    let (_, quiet) = assemble(&[Stream::new().text(0, &["x"]).end("end_turn").as_bytes()]).unwrap();
    assert_eq!(quiet.thinking_dropped, 0);
}
