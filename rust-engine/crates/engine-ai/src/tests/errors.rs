//! Error mapping and retry.

use crate::provider::{ProtocolError, ProviderError};
use crate::test_support::*;
use crate::{AssistantError, DoneReason, TransportError};
use engine_agent::CancelToken;
use std::sync::mpsc;
use std::time::Duration;

fn provider_error(error: AssistantError) -> ProviderError {
    match error {
        AssistantError::Provider(error) => error,
        other => panic!("not a provider error: {other:?}"),
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn client_errors_are_typed_and_never_retried() {
    type Check = fn(&ProviderError) -> bool;
    let table: [(u16, &str, Check); 6] = [
        (400, "invalid_request_error", |e| {
            matches!(e, ProviderError::BadRequest(_))
        }),
        (401, "authentication_error", |e| {
            matches!(e, ProviderError::Authentication(_))
        }),
        (403, "permission_error", |e| {
            matches!(e, ProviderError::Permission(_))
        }),
        (404, "not_found_error", |e| {
            matches!(e, ProviderError::NotFound(_))
        }),
        (413, "request_too_large", |e| {
            matches!(e, ProviderError::TooLarge(_))
        }),
        (402, "billing_error", |e| {
            matches!(e, ProviderError::Rejected(_))
        }),
    ];
    for (status, kind, check) in table {
        let mut harness = Harness::new(vec![
            Reply::error(status, kind, "the API's own words").into(),
            text_reply("must not be asked for").into(),
        ]);
        let error = provider_error(harness.run("hi").unwrap_err());
        assert!(check(&error), "{status}: {error:?}");
        assert!(!error.is_retryable());
        assert_eq!(
            harness.transport.request_count(),
            1,
            "{status} is not retried"
        );
        assert!(harness.sleeper.waits().is_empty());
        // The API's message and the request id are kept; the conversation is untouched.
        assert!(error.to_string().contains("the API's own words"), "{error}");
        assert_eq!(error.request_id(), Some("req_scripted"));
        assert_eq!(error.api_failure().unwrap().status, Some(status));
        assert!(harness.conversation.is_empty());
    }
}

#[test]
fn the_request_id_comes_from_the_body_when_the_header_is_missing() {
    let mut reply = Reply::error(401, "authentication_error", "bad key");
    reply.headers.clear();
    let mut harness = Harness::new(vec![reply.into()]);
    let error = provider_error(harness.run("hi").unwrap_err());
    assert_eq!(error.request_id(), Some("req_in_body"));
}

#[test]
fn a_gateway_answer_that_is_not_json_still_gives_a_typed_error() {
    let mut harness = Harness::new(vec![
        Reply::with_status(502, "<html>Bad gateway</html>").into(),
        Reply::with_status(502, "<html>Bad gateway</html>").into(),
        Reply::with_status(502, "<html>Bad gateway</html>").into(),
        Reply::with_status(502, "<html>Bad gateway</html>").into(),
    ]);
    let error = provider_error(harness.run("hi").unwrap_err());
    let ProviderError::Server(failure) = &error else {
        panic!("{error:?}")
    };
    assert_eq!(failure.message, "the server answered HTTP 502");
}

#[test]
fn a_429_waits_for_retry_after_and_is_sent_again() {
    let mut harness = Harness::new(vec![
        Reply::error(429, "rate_limit_error", "slow down")
            .header("Retry-After", "7")
            .into(),
        text_reply("ok").into(),
    ]);
    let summary = harness.run("hi").unwrap();
    assert_eq!(summary.reason, DoneReason::EndTurn);
    assert_eq!(harness.sleeper.waits(), [secs(7)]);
    assert_eq!(harness.transport.request_count(), 2);
    assert_eq!(
        harness.transport.request(0).body,
        harness.transport.request(1).body
    );
    assert!(harness
        .log
        .iter()
        .any(|e| e.contains("Retrying") && e.contains("RateLimited") && e.contains("attempt: 1")));
}

#[test]
fn a_529_and_a_500_are_retried_up_to_the_bound_then_the_typed_error() {
    for (status, kind, overloaded) in [(529, "overloaded_error", true), (500, "api_error", false)] {
        let mut harness = Harness::new(vec![
            Reply::error(status, kind, "busy").into(),
            Reply::error(status, kind, "busy").into(),
            Reply::error(status, kind, "busy").into(),
            Reply::error(status, kind, "busy").into(),
            text_reply("never").into(),
        ]);
        let error = provider_error(harness.run("hi").unwrap_err());
        if overloaded {
            assert!(matches!(error, ProviderError::Overloaded(_)), "{error:?}");
        } else {
            assert!(matches!(error, ProviderError::Server(_)), "{error:?}");
        }
        assert!(error.is_retryable());
        assert_eq!(harness.transport.request_count(), 4, "1 try + 3 retries");
        assert_eq!(harness.sleeper.waits(), [secs(1), secs(2), secs(4)]);
        assert!(harness.conversation.is_empty());
    }
}

#[test]
fn the_total_wait_is_bounded() {
    let mut harness = Harness::new(vec![
        Reply::error(429, "rate_limit_error", "slow")
            .header("retry-after", "40")
            .into(),
        Reply::error(429, "rate_limit_error", "slow")
            .header("retry-after", "40")
            .into(),
        text_reply("never").into(),
    ]);
    let error = provider_error(harness.run("hi").unwrap_err());
    assert!(matches!(error, ProviderError::RateLimited(_)));
    assert_eq!(
        harness.sleeper.waits(),
        [secs(40)],
        "the second 40 s would pass the 60 s bound"
    );
    assert_eq!(harness.transport.request_count(), 2);
}

#[test]
fn a_connection_failure_before_the_first_byte_is_retried_once() {
    let mut harness = Harness::new(vec![
        Step::Fail(TransportError::Failed("connection refused".into())),
        text_reply("ok").into(),
    ]);
    harness.run("hi").unwrap();
    assert_eq!(harness.transport.request_count(), 2);
    assert_eq!(harness.sleeper.waits(), [Duration::from_millis(500)]);
    assert!(harness.log.iter().any(|e| e.contains("cause: Transport")));

    let mut harness = Harness::new(vec![
        Step::Fail(TransportError::Failed("connection refused".into())),
        Step::Fail(TransportError::Timeout),
        text_reply("never").into(),
    ]);
    let error = provider_error(harness.run("hi").unwrap_err());
    assert!(matches!(
        error,
        ProviderError::Transport {
            source: TransportError::Timeout,
            ..
        }
    ));
    assert_eq!(harness.transport.request_count(), 2);
    assert!(harness.conversation.is_empty());
}

#[test]
fn a_stream_that_breaks_after_it_started_is_an_error_and_is_not_retried() {
    let partial = Stream::new().text(0, &["half"]).unfinished();
    let mut harness = Harness::new(vec![
        Reply::ok(partial).end(End::Break).into(),
        text_reply("must not be asked for").into(),
    ]);
    let error = provider_error(harness.run("hi").unwrap_err());
    let ProviderError::Transport { request_id, .. } = &error else {
        panic!("{error:?}")
    };
    assert_eq!(request_id.as_deref(), Some("req_scripted"));
    assert_eq!(harness.transport.request_count(), 1);
    assert!(harness.sleeper.waits().is_empty());
    assert!(harness.conversation.is_empty(), "conversation unchanged");
    // The same input can be sent again.
    harness.transport.push(text_reply("ok"));
    assert_eq!(harness.run("hi").unwrap().reason, DoneReason::EndTurn);
}

#[test]
fn a_stream_that_ends_without_message_stop_is_a_protocol_error() {
    let partial = Stream::new().text(0, &["half"]).unfinished();
    let mut harness = Harness::new(vec![Reply::ok(partial).into()]);
    let error = provider_error(harness.run("hi").unwrap_err());
    assert!(matches!(
        error,
        ProviderError::Protocol {
            source: ProtocolError::Truncated,
            ..
        }
    ));
    assert!(harness.conversation.is_empty());
}

#[test]
fn an_error_event_in_the_middle_of_a_stream_maps_to_the_typed_error_without_a_retry() {
    for (kind, check) in [
        (
            "overloaded_error",
            (|e: &ProviderError| matches!(e, ProviderError::Overloaded(_)))
                as fn(&ProviderError) -> bool,
        ),
        ("rate_limit_error", |e| {
            matches!(e, ProviderError::RateLimited(_))
        }),
        ("api_error", |e| matches!(e, ProviderError::Server(_))),
        ("invalid_request_error", |e| {
            matches!(e, ProviderError::BadRequest(_))
        }),
    ] {
        let body = Stream::new()
            .text(0, &["partial"])
            .error_event(kind, "mid-stream trouble")
            .unfinished();
        let mut harness = Harness::new(vec![Reply::ok(body).into(), text_reply("never").into()]);
        let error = provider_error(harness.run("hi").unwrap_err());
        assert!(check(&error), "{kind}: {error:?}");
        assert!(error.to_string().contains("mid-stream trouble"));
        assert_eq!(error.request_id(), Some("req_scripted"));
        assert_eq!(
            harness.transport.request_count(),
            1,
            "nothing is retried after the stream started"
        );
        assert!(harness.conversation.is_empty());
    }
}

#[test]
fn an_error_event_before_message_start_is_retried_like_the_http_error_it_stands_for() {
    let early = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let mut harness = Harness::new(vec![Reply::ok(early).into(), text_reply("ok").into()]);
    harness.run("hi").unwrap();
    assert_eq!(harness.transport.request_count(), 2);
    assert_eq!(harness.sleeper.waits(), [secs(1)]);
}

#[test]
fn a_cancel_while_waiting_to_retry_ends_the_run_cleanly() {
    for _ in 0..30 {
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut harness = Harness::new(vec![
            Reply::error(529, "overloaded_error", "busy").into(),
            text_reply("never").into(),
        ]);
        harness.sleeper.block_on_cancel(entered_tx);
        let cancel = CancelToken::new();
        let summary = std::thread::scope(|scope| {
            let worker = scope.spawn(|| harness.run_with("hi", &cancel));
            entered_rx.recv().unwrap();
            cancel.cancel();
            worker.join().unwrap()
        })
        .unwrap();
        assert_eq!(summary.reason, DoneReason::Cancelled);
        assert_eq!(
            harness.transport.request_count(),
            1,
            "no second request after the cancel"
        );
        assert!(harness.conversation.is_empty());
    }
}

#[test]
fn the_real_sleeper_wakes_on_cancel_and_waits_otherwise() {
    use crate::{SleepOutcome, Sleeper, ThreadSleeper};
    let cancel = CancelToken::new();
    assert_eq!(
        ThreadSleeper.sleep(Duration::from_millis(1), &cancel),
        SleepOutcome::Elapsed
    );
    for _ in 0..30 {
        let cancel = CancelToken::new();
        let outcome = std::thread::scope(|scope| {
            let sleeper = scope.spawn(|| ThreadSleeper.sleep(Duration::from_secs(30), &cancel));
            cancel.cancel();
            sleeper.join().unwrap()
        });
        assert_eq!(outcome, SleepOutcome::Cancelled);
    }
}
