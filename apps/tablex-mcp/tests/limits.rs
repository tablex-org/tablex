//! At most `MAX_CONCURRENT_CALLS` tool calls run at once; the next one is refused at once, and
//! the slots come back when the calls return.

mod support;

use std::time::Duration;

use serde_json::json;
use support::*;
use tablex_mcp::server::MAX_CONCURRENT_CALLS;

const DEADLINE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn the_call_after_the_limit_is_refused_at_once_and_slots_come_back() {
    // MAX connections whose statements block until cancelled, and one more that answers.
    let ids = fake_ids(MAX_CONCURRENT_CALLS + 1);
    let mut blockers = Vec::new();
    let fixture = fake_fixture(&ids, |id| {
        if id == ids[MAX_CONCURRENT_CALLS] {
            engine_agent::testing::FakeSession::new(engine_agent::ConnectionKind::Sqlite).arc()
        } else {
            let session = blocking_session();
            blockers.push(session.clone());
            session
        }
    });
    let harness = Harness::start(fixture).await;
    for id in &ids {
        let opened = harness
            .call("open_session", json!({"connection_id": id}))
            .await;
        assert_eq!(code(&opened), "ok", "{}", text(&opened));
    }
    let statement = |id: &str| json!({"connection_id": id, "statement": "SELECT 1"});

    let mut handles = Vec::new();
    for id in &ids[..MAX_CONCURRENT_CALLS] {
        handles.push(harness.start_call("execute_query", statement(id)).await);
    }
    // Every one of them is inside its statement: all permits are taken.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while blockers.iter().any(|session| session.run_count() == 0) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the statements never started"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The next call is answered at once, with the layer's error shape, and never reaches a session.
    let refused = harness
        .call("execute_query", statement(&ids[MAX_CONCURRENT_CALLS]))
        .await;
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(code(&refused), "agent.limit");
    let body: serde_json::Value = serde_json::from_str(&text(&refused)).unwrap();
    assert_eq!(body["error"]["retryable"], true);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too many tool calls"),
        "{body}"
    );
    assert!(refused.structured_content.is_none());
    // Not even browsing gets through while the slots are taken (the pool is protected, not a queue).
    let browse = harness.call("list_connections", json!({})).await;
    assert_eq!(code(&browse), "agent.limit");

    // Cancel all of them; every call returns (the layer audits each as cancelled)...
    for handle in handles {
        handle.cancel(Some("test".into())).await.unwrap();
    }
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let cancelled = harness
            .fixture
            .audit_entries()
            .iter()
            .filter(|entry| entry.tool == "execute_query" && entry.outcome == "agent.cancelled")
            .count();
        if cancelled == MAX_CONCURRENT_CALLS {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {cancelled} calls returned"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // ...and a new call works (permits are released as the blocking calls end; poll for the last).
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let again = harness
            .call("execute_query", statement(&ids[MAX_CONCURRENT_CALLS]))
            .await;
        if code(&again) == "ok" {
            break;
        }
        assert_eq!(code(&again), "agent.limit", "{}", text(&again));
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slots never came back"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // All sixteen are free again, not just one.
    let mut held = Vec::new();
    for id in &ids[..MAX_CONCURRENT_CALLS] {
        held.push(harness.start_call("execute_query", statement(id)).await);
    }
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while blockers.iter().any(|session| session.run_count() < 2) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slots never came back"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for handle in held {
        handle.cancel(None).await.unwrap();
    }
}
