//! A running statement is cancelled by the MCP cancel notification for its request and by the
//! client disconnecting; in the second case the sessions are torn down too.
//!
//! No sleeps decide anything: the test waits until the layer itself says the statement is running
//! (a second statement on the session is refused with `agent.limit`) before it cancels, and then
//! for the audit line the cancelled call writes. The statement timeout is 30 s, so an outcome of
//! `agent.cancelled` (not `agent.limit`) proves the cancel did it.

mod support;

use std::time::Duration;

use serde_json::json;
use support::*;

const DEADLINE: Duration = Duration::from_secs(60);

/// Under rmcp's 5 s drain of in-flight handlers, far over what a cancel takes.
const PROMPT: Duration = Duration::from_secs(3);

/// Runs for far longer than any test would wait.
const FOREVER: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c \
                       WHERE x < 1000000000000) SELECT count(*) FROM c";

async fn poll<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

type Handle = rmcp::service::RequestHandle<rmcp::RoleClient>;

/// A session with `FOREVER` running on it, started through MCP.
///
/// The signal that it runs is the layer's own: a second statement on the session is refused with
/// `agent.limit`. A probe can also catch the session at the instant `FOREVER` arrives, and then
/// `FOREVER` is the one refused (it answers `agent.limit` at once and nothing runs): that case is
/// told apart by the answer and the statement is sent again.
async fn running_statement(harness: &Harness) -> (Conn, Handle) {
    let conn = harness.fixture.connection(Some("read"));
    let opened = harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    let probe = json!({"connection_id": conn.id, "statement": "SELECT 1"});
    let forever = json!({"connection_id": conn.id, "statement": FOREVER});
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let mut handle = harness.start_call("execute_query", forever.clone()).await;
        loop {
            if code(&harness.call("execute_query", probe.clone()).await) == "agent.limit" {
                return (conn, handle);
            }
            if let Ok(answer) = handle.rx.try_recv() {
                let refused = unwrap_call(answer.expect("answered"));
                assert_eq!(code(&refused), "agent.limit", "{}", text(&refused));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the statement never ran"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

async fn cancelled_in_audit(harness: &Harness, conn: &Conn) {
    poll("the cancelled call in the audit log", async || {
        harness
            .fixture
            .audit_entries()
            .iter()
            .any(|entry| {
                entry.tool == "execute_query"
                    && entry.connection_id.as_deref() == Some(conn.id.as_str())
                    && entry.outcome == "agent.cancelled"
            })
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_cancel_notification_stops_the_running_statement_and_keeps_the_session() {
    let harness = Harness::start(Fixture::new()).await;
    let (conn, handle) = running_statement(&harness).await;

    handle.cancel(Some("test".into())).await.unwrap();
    cancelled_in_audit(&harness, &conn).await;

    // The statement is gone, the session is not: it answers the next question.
    let after = harness
        .call(
            "execute_query",
            json!({"connection_id": conn.id, "statement": "SELECT 41 + 1 AS answer"}),
        )
        .await;
    assert!(
        structured(&after).to_string().contains("42"),
        "{}",
        text(&after)
    );

    // Ending the connection closes the session.
    let Harness {
        fixture,
        client,
        server,
    } = harness;
    client.cancel().await.unwrap();
    server.await.unwrap().unwrap();
    assert!(!fixture.layer.is_open(&conn.id));
}

#[tokio::test]
async fn the_client_disconnecting_cancels_the_statement_and_closes_the_session() {
    let harness = Harness::start(Fixture::new()).await;
    let (conn, handle) = running_statement(&harness).await;
    assert!(harness.fixture.layer.is_open(&conn.id));

    let Harness {
        fixture,
        client,
        server,
    } = harness;
    drop(handle);
    let gone_at = std::time::Instant::now();
    // The server notices the closed pipe, cancels the call, and closes everything. Without the
    // disconnect watch rmcp lets go of the running call only after its own 5 s drain, so a bound
    // under that drain (wall clock; virtual time cannot be used while a blocking thread works)
    // tells the two apart.
    tokio::time::timeout(PROMPT, async {
        client.cancel().await.unwrap();
        server.await.unwrap().unwrap();
    })
    .await
    .expect("the server ends promptly when the client is gone");
    assert!(gone_at.elapsed() < PROMPT);
    assert!(
        !fixture.layer.is_open(&conn.id),
        "the session was torn down"
    );
    let cancelled = fixture.audit_entries().into_iter().any(|entry| {
        entry.tool == "execute_query"
            && entry.connection_id.as_deref() == Some(conn.id.as_str())
            && entry.outcome == "agent.cancelled"
    });
    assert!(cancelled, "the statement was cancelled, not timed out");
}
