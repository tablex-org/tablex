//! The tool loop.

use crate::provider::StopReason;
use crate::test_support::*;
use crate::{DoneReason, Message, UserBlock};
use engine_agent::testing::Behavior;
use engine_agent::{AgentError, CancelToken};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::sync::Mutex;

const QUERY_A: &str = r#"{"connection_id":"c1","statement":"SELECT 1"}"#;
const QUERY_B: &str = r#"{"connection_id":"c1","statement":"SELECT 2"}"#;

fn results(harness: &Harness, message: usize) -> Vec<(String, String, bool)> {
    let Message::User { content } = &harness.conversation.messages()[message] else {
        panic!(
            "message {message} is not from the user: {:?}",
            harness.conversation.messages()[message]
        );
    };
    content
        .iter()
        .map(|block| match block {
            UserBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => (tool_use_id.clone(), content.clone(), *is_error),
            UserBlock::Text { .. } => panic!("not a tool result"),
        })
        .collect()
}

fn roles(harness: &Harness) -> Vec<&'static str> {
    harness
        .conversation
        .messages()
        .iter()
        .map(|m| match m {
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
            Message::System { .. } => "system",
        })
        .collect()
}

#[test]
fn a_plain_answer_streams_events_and_ends_the_turn() {
    let mut harness = Harness::new(vec![text_reply("Three users.").into()]);
    let summary = harness.run("count").unwrap();
    assert_eq!(summary.reason, DoneReason::EndTurn);
    assert_eq!(summary.rounds, 0);
    assert_eq!(summary.usage.input_tokens, 100);
    assert_eq!(summary.usage.output_tokens, 55);
    assert_eq!(summary.usage.cache_read_input_tokens, 40);
    assert_eq!(roles(&harness), ["user", "assistant"]);
    assert_eq!(
        harness.conversation.messages()[1].assistant_text(),
        "Three users."
    );
    let log = harness.log.join("\n");
    assert!(log.contains("ContextLoaded"), "{log}");
    assert!(log.contains("TextDelta(\"Three users.\")"));
    assert!(log.contains("Usage(Usage"));
    assert!(log.ends_with("Done(EndTurn)"));
}

#[test]
fn two_calls_in_one_turn_get_one_user_message_with_both_results_in_order() {
    let mut harness = Harness::new(vec![
        tool_reply(&[
            ("toolu_1", "execute_query", QUERY_A),
            ("toolu_2", "execute_query", QUERY_B),
        ])
        .into(),
        text_reply("done").into(),
    ]);
    let summary = harness.run("run both").unwrap();
    assert_eq!(summary.rounds, 1);
    assert_eq!(roles(&harness), ["user", "assistant", "user", "assistant"]);
    let answers = results(&harness, 2);
    assert_eq!(
        answers
            .iter()
            .map(|(id, _, err)| (id.as_str(), *err))
            .collect::<Vec<_>>(),
        [("toolu_1", false), ("toolu_2", false)]
    );
    // Run one after the other, in order, through the layer.
    let ran: Vec<String> = lock(&harness.session.runs)
        .iter()
        .map(|(t, _)| t.clone())
        .collect();
    assert_eq!(ran, ["SELECT 1", "SELECT 2"]);
    let content: Value = serde_json::from_str(&answers[0].1).unwrap();
    assert!(
        content.get("rows").is_some() || content.get("columns").is_some(),
        "{content}"
    );
    let log = harness.log.join("\n");
    assert!(log.contains("ToolCallStarted { id: \"toolu_1\""));
    assert!(log.contains("ToolResult { id: \"toolu_2\""));
}

#[test]
fn input_that_is_not_json_is_not_run_and_is_answered_with_invalid_json() {
    let raw = r#"{"connection_id":"c1","statement":"SELECT \"x"#;
    let mut harness = Harness::new(vec![
        tool_reply(&[("toolu_1", "execute_query", raw)]).into(),
        text_reply("sorry").into(),
    ]);
    harness.run("go").unwrap();
    assert_eq!(harness.session.run_count(), 0);
    let answers = results(&harness, 2);
    assert_eq!(answers.len(), 1);
    assert!(answers[0].2, "is_error");
    assert_eq!(answers[0].1, json!({"INVALID_JSON": raw}).to_string());
    // The stored call has an empty input and goes back so.
    let Message::Assistant { content } = &harness.conversation.messages()[1] else {
        panic!()
    };
    assert_eq!(content[1]["input"], json!({}));
    assert!(harness.log.iter().any(|e| e.contains("valid: false")));
}

#[test]
fn a_schema_violation_is_the_layers_invalid_arguments_error() {
    let mut harness = Harness::new(vec![
        tool_reply(&[("toolu_1", "execute_query", r#"{"connection_id":"c1"}"#)]).into(),
        text_reply("ok").into(),
    ]);
    harness.run("go").unwrap();
    assert_eq!(harness.session.run_count(), 0);
    let answers = results(&harness, 2);
    assert!(answers[0].2);
    assert!(
        answers[0].1.contains("agent.invalid_arguments"),
        "{}",
        answers[0].1
    );
}

#[test]
fn a_tool_that_fails_is_an_error_result_and_the_loop_goes_on() {
    let mut harness = Harness::new(vec![
        tool_reply(&[("toolu_1", "execute_query", QUERY_A)]).into(),
        text_reply("It failed.").into(),
    ]);
    harness
        .session
        .set_behavior(Behavior::Fail(AgentError::driver(
            "test.boom",
            "no such table",
            false,
        )));
    let summary = harness.run("go").unwrap();
    assert_eq!(summary.reason, DoneReason::EndTurn);
    let answers = results(&harness, 2);
    assert!(answers[0].2);
    assert!(answers[0].1.contains("test.boom"));
    assert_eq!(harness.transport.request_count(), 2);
}

#[test]
fn a_tool_that_panics_is_an_error_result_not_a_broken_conversation() {
    let mut harness = Harness::new(vec![
        tool_reply(&[("toolu_1", "execute_query", QUERY_A)]).into(),
        text_reply("ok").into(),
    ]);
    harness
        .session
        .set_behavior(Behavior::Script(Box::new(|_, _| panic!("driver bug"))));
    harness.run("go").unwrap();
    let answers = results(&harness, 2);
    assert!(answers[0].2);
    assert!(
        answers[0].1.contains("assistant.tool_panicked"),
        "{}",
        answers[0].1
    );
}

#[test]
fn max_tokens_with_tool_calls_runs_none_and_asks_for_a_smaller_step() {
    let cut = Reply::ok(
        Stream::new()
            .tool(
                0,
                "toolu_1",
                "execute_query",
                &[r#"{"connection_id":"c1","statement":"SELECT 1"}"#],
            )
            .tool(
                1,
                "toolu_2",
                "execute_query",
                &[r#"{"connection_id":"c1","statem"#],
            )
            .end("max_tokens"),
    );
    let mut harness = Harness::new(vec![cut.into(), text_reply("Smaller then.").into()]);
    let summary = harness.run("go").unwrap();
    assert_eq!(summary.reason, DoneReason::EndTurn);
    assert_eq!(
        harness.session.run_count(),
        0,
        "a cut-off turn runs nothing, valid input or not"
    );
    let answers = results(&harness, 2);
    assert_eq!(answers.len(), 2);
    assert!(answers
        .iter()
        .all(|(_, text, err)| *err && text.contains("cut off")));
    assert!(harness
        .log
        .iter()
        .any(|e| e.contains("CutOff { tool_calls: 2 }")));
    // The turn was kept and the model saw the answers.
    let second = harness.transport.request(1).json();
    assert_eq!(second["messages"][2]["content"][0]["is_error"], json!(true));
}

#[test]
fn max_tokens_without_tool_calls_keeps_the_text_and_stops() {
    let cut = Reply::ok(
        Stream::new()
            .text(0, &["The answer is long and"])
            .end("max_tokens"),
    );
    let mut harness = Harness::new(vec![cut.into()]);
    let summary = harness.run("go").unwrap();
    assert_eq!(summary.reason, DoneReason::MaxTokens);
    assert_eq!(roles(&harness), ["user", "assistant"]);
}

#[test]
fn a_refusal_runs_nothing_and_the_turn_is_not_kept() {
    let refusal = Reply::ok(
        Stream::new()
            .text(0, &["I can"])
            .tool(1, "toolu_1", "execute_query", &[QUERY_A])
            .end_with("refusal", json!({"stop_details": {"category": "cyber"}})),
    );
    let mut harness = Harness::new(vec![refusal.into()]);
    let summary = harness.run("go").unwrap();
    assert_eq!(
        summary.reason,
        DoneReason::Refused {
            category: Some("cyber".into())
        }
    );
    assert_eq!(harness.session.run_count(), 0);
    assert!(
        harness.conversation.is_empty(),
        "the refused request is taken back too"
    );
    assert!(harness
        .log
        .iter()
        .any(|e| e.contains("Refused { category: Some(\"cyber\") }")));
}

#[test]
fn a_refusal_after_a_tool_round_leaves_the_conversation_ending_in_the_results() {
    let refusal = Reply::ok(Stream::new().end_with("refusal", json!({"stop_details": null})));
    let mut harness = Harness::new(vec![
        tool_reply(&[("toolu_1", "execute_query", QUERY_A)]).into(),
        refusal.into(),
        text_reply("fine").into(),
    ]);
    let summary = harness.run("go").unwrap();
    assert_eq!(summary.reason, DoneReason::Refused { category: None });
    assert_eq!(roles(&harness), ["user", "assistant", "user"]);
    assert!(harness.conversation.ends_with_user());
    // Still sendable.
    let summary = harness.resume(&CancelToken::new()).unwrap();
    assert_eq!(summary.reason, DoneReason::EndTurn);
    assert_eq!(roles(&harness), ["user", "assistant", "user", "assistant"]);
}

#[test]
fn the_round_limit_stops_the_run_with_a_valid_conversation() {
    let mut harness = Harness::new(vec![
        tool_reply(&[("t1", "execute_query", QUERY_A)]).into(),
        tool_reply(&[("t2", "execute_query", QUERY_B)]).into(),
        text_reply("never reached").into(),
    ]);
    harness.config.max_rounds = 2;
    let summary = harness.run("loop").unwrap();
    assert_eq!(summary.reason, DoneReason::RoundLimit);
    assert_eq!(summary.rounds, 2);
    assert_eq!(harness.transport.request_count(), 2);
    assert_eq!(harness.transport.steps_left(), 1);
    assert_eq!(
        roles(&harness),
        ["user", "assistant", "user", "assistant", "user"]
    );
    assert!(harness
        .log
        .iter()
        .any(|e| e.contains("RoundLimit { rounds: 2 }")));
    // A resumed run gets a fresh budget and finishes.
    harness.config.max_rounds = 16;
    assert_eq!(
        harness.resume(&CancelToken::new()).unwrap().reason,
        DoneReason::EndTurn
    );
}

#[test]
fn pause_turn_and_unknown_stop_reasons_stop_the_run_and_are_reported() {
    for (stop, expected) in [
        ("pause_turn", DoneReason::PauseTurn),
        (
            "model_context_window_exceeded",
            DoneReason::ContextWindowExceeded,
        ),
        ("stop_sequence", DoneReason::StopSequence),
        ("something_new", DoneReason::Other("something_new".into())),
    ] {
        let reply = Reply::ok(Stream::new().text(0, &["x"]).end(stop));
        let mut harness = Harness::new(vec![reply.into()]);
        assert_eq!(harness.run("go").unwrap().reason, expected, "{stop}");
        assert_eq!(roles(&harness), ["user", "assistant"]);
    }
    for (stop, done) in [
        (StopReason::ToolUse, DoneReason::EndTurn),
        (StopReason::MaxTokens, DoneReason::MaxTokens),
        (
            StopReason::Refusal {
                category: Some("bio".into()),
            },
            DoneReason::Refused {
                category: Some("bio".into()),
            },
        ),
        (StopReason::Other("x".into()), DoneReason::Other("x".into())),
    ] {
        assert_eq!(DoneReason::from(stop), done);
    }
}

#[test]
fn tool_calls_in_a_turn_that_did_not_ask_for_tools_are_answered_not_run() {
    let reply = Reply::ok(
        Stream::new()
            .tool(0, "toolu_1", "execute_query", &[QUERY_A])
            .end("end_turn"),
    );
    let mut harness = Harness::new(vec![reply.into()]);
    assert_eq!(harness.run("go").unwrap().reason, DoneReason::EndTurn);
    assert_eq!(harness.session.run_count(), 0);
    assert!(results(&harness, 2)[0].1.contains("assistant.not_run"));
}

#[test]
fn the_schema_context_is_fetched_once_and_frozen() {
    let mut harness = Harness::new(vec![text_reply("a").into(), text_reply("b").into()]);
    harness.run("one").unwrap();
    let frozen = harness.conversation.context().cloned().unwrap();
    // The schema changes; the conversation does not look again.
    lock(&harness.session.tables).clear();
    harness.run("two").unwrap();
    assert_eq!(harness.conversation.context(), Some(&frozen));
    assert_eq!(
        harness
            .log
            .iter()
            .filter(|e| e.starts_with("ContextLoaded"))
            .count(),
        1
    );
    let (a, b) = (
        harness.transport.request(0).json(),
        harness.transport.request(1).json(),
    );
    assert_eq!(a["system"], b["system"]);
}

// ---- cancellation (threads: looped) ---------------------------------------------------------

#[test]
fn a_cancel_during_streaming_drops_the_partial_turn() {
    for _ in 0..30 {
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let partial = Stream::new().text(0, &["half an ans"]).unfinished();
        let mut harness = Harness::new(vec![Reply::ok(partial)
            .end(End::BlockUntilCancel(Some(blocked_tx)))
            .into()]);
        let cancel = CancelToken::new();
        let summary = std::thread::scope(|scope| {
            let worker = scope.spawn(|| harness.run_with("go", &cancel));
            blocked_rx.recv().unwrap();
            cancel.cancel();
            worker.join().unwrap()
        })
        .unwrap();
        assert_eq!(summary.reason, DoneReason::Cancelled);
        assert!(
            harness.conversation.is_empty(),
            "nothing of the cancelled run is kept"
        );
        assert!(harness
            .log
            .iter()
            .any(|e| e.contains("TextDelta(\"half an ans\")")));
        // The same conversation can send again.
        harness.transport.push(text_reply("ok"));
        assert_eq!(harness.run("go").unwrap().reason, DoneReason::EndTurn);
    }
}

#[test]
fn a_cancel_between_two_tool_calls_answers_the_second_with_a_cancelled_error() {
    for _ in 0..30 {
        let mut harness = Harness::new(vec![
            tool_reply(&[
                ("toolu_1", "execute_query", QUERY_A),
                ("toolu_2", "execute_query", QUERY_B),
            ])
            .into(),
            text_reply("ok").into(),
        ]);
        let cancel = CancelToken::new();
        let (started_tx, started_rx) = mpsc::channel();
        let started_tx = Mutex::new(started_tx);
        let watched = cancel.clone();
        harness
            .session
            .set_behavior(Behavior::Script(Box::new(move |_, call_cancel| {
                let _ = lock(&started_tx).send(());
                while !call_cancel.is_cancelled() {
                    std::thread::yield_now();
                }
                assert!(watched.is_cancelled());
                Err(AgentError::Cancelled)
            })));
        let summary = std::thread::scope(|scope| {
            let worker = scope.spawn(|| harness.run_with("go", &cancel));
            started_rx.recv().unwrap();
            cancel.cancel();
            worker.join().unwrap()
        })
        .unwrap();
        assert_eq!(summary.reason, DoneReason::Cancelled);
        assert_eq!(roles(&harness), ["user", "assistant", "user"]);
        let answers = results(&harness, 2);
        assert_eq!(answers.len(), 2);
        assert!(answers.iter().all(|(_, _, err)| *err));
        assert!(answers[0].1.contains("agent.cancelled"), "{}", answers[0].1);
        assert!(
            answers[1].1.contains("Cancelled before this tool call ran"),
            "{}",
            answers[1].1
        );
        assert_eq!(
            harness.session.run_count(),
            1,
            "the second call never reached the session"
        );
        // The conversation is sendable, and valid when stored and read back.
        let stored = serde_json::to_string(&harness.conversation).unwrap();
        let _: crate::Conversation = serde_json::from_str(&stored).unwrap();
        harness.session.set_behavior(Behavior::Numbers { count: 1 });
        assert_eq!(
            harness.resume(&CancelToken::new()).unwrap().reason,
            DoneReason::EndTurn
        );
        let last = harness.last_request().json();
        assert_eq!(last["messages"][2]["content"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn one_statement_timing_out_does_not_cancel_the_run() {
    // The layer expires the *call's* token; the run's token stays live.
    let mut harness = Harness::new(vec![
        tool_reply(&[("toolu_1", "execute_query", QUERY_A)]).into(),
        text_reply("It timed out.").into(),
    ]);
    let run_cancel = CancelToken::new();
    harness
        .session
        .set_behavior(Behavior::Script(Box::new(|_, call_cancel| {
            call_cancel.cancel();
            Err(AgentError::Cancelled)
        })));
    let summary = harness.run_with("go", &run_cancel).unwrap();
    assert_eq!(summary.reason, DoneReason::EndTurn);
    assert!(!run_cancel.is_cancelled());
    assert!(results(&harness, 2)[0].2);
}

#[test]
fn a_run_cancelled_before_it_starts_changes_nothing() {
    let mut harness = Harness::new(vec![]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let summary = harness.run_with("go", &cancel).unwrap();
    assert_eq!(summary.reason, DoneReason::Cancelled);
    assert!(harness.conversation.is_empty());
    assert_eq!(harness.transport.request_count(), 0);
}

#[test]
fn resume_needs_a_conversation_that_ends_with_the_user() {
    let mut harness = Harness::new(vec![text_reply("ok").into()]);
    assert!(matches!(
        harness.resume(&CancelToken::new()),
        Err(crate::AssistantError::NothingToResume)
    ));
    harness.run("go").unwrap();
    assert!(matches!(
        harness.resume(&CancelToken::new()),
        Err(crate::AssistantError::NothingToResume)
    ));
}

#[test]
fn the_transports_chunking_does_not_change_the_conversation() {
    let body = || {
        Stream::new()
            .thinking(0, &["ü ", "日本"], "SIG")
            .text(1, &["Hello 😀 ", "world"])
            .tool(2, "toolu_1", "list_tables", &[r#"{"connection_id":"c1"}"#])
            .end("tool_use")
    };
    let mut reference = Harness::new(vec![Reply::ok(body()).into(), text_reply("x").into()]);
    reference.run("go").unwrap();
    for make in [
        (|b: String| Reply::ok(b).chunked(1)) as fn(String) -> Reply,
        |b| Reply::ok(b).chunked(7),
        |b| Reply::ok(b).chunked(4096),
        |b| Reply::ok(b).split_at(61),
    ] {
        let mut harness = Harness::new(vec![make(body()).into(), text_reply("x").into()]);
        harness.run("go").unwrap();
        assert_eq!(
            harness.conversation.messages(),
            reference.conversation.messages()
        );
        assert_eq!(harness.log, reference.log);
    }
}

#[test]
fn a_cancel_that_arrives_between_two_reads_drops_the_turn() {
    for _ in 0..30 {
        let cancel = CancelToken::new();
        let hook = cancel.clone();
        let body = Stream::new()
            .text(0, &["one ", "two ", "three"])
            .end("end_turn");
        let mut harness = Harness::new(vec![Reply::ok(body)
            .hook_at(200, move || hook.cancel())
            .into()]);
        let summary = harness.run_with("go", &cancel).unwrap();
        assert_eq!(summary.reason, DoneReason::Cancelled);
        assert!(harness.conversation.is_empty());
    }
}
