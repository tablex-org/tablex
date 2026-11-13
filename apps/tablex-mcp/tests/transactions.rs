//! Explicit transactions through the MCP server, over the real SQLite driver and the layer as
//! `setup::build_layer` builds it: begin is refused where a write would be, every write and the
//! COMMIT are put to the user, a declined commit commits nothing, and whatever is left open when
//! the client goes away is rolled back.

mod support;

use std::time::Duration;

use serde_json::json;
use support::*;
use tablex_mcp::approver::{REASON_NO_ELICITATION, REASON_PRODUCTION, REASON_SWITCH_OFF};
use tablex_mcp::server::McpServer;

const INSERT: &str = "INSERT INTO items(label) VALUES ('tx-row')";
const COUNT: &str = "SELECT count(*) AS n FROM items WHERE label = 'tx-row'";

fn count(result: &rmcp::model::CallToolResult) -> String {
    structured(result)["rows"][0][0].as_str().expect("a count").to_owned()
}

fn message(result: &rmcp::model::CallToolResult) -> String {
    let body: serde_json::Value = serde_json::from_str(&text(result)).expect("JSON error");
    body["error"]["message"].as_str().unwrap().to_owned()
}

async fn writer_harness(asker: Asker) -> (Harness<Asker>, String) {
    let fixture = Fixture::new();
    fixture.set_mcp_writes(true);
    let conn = fixture.connection(Some("read_write"));
    let harness = Harness::start_with(fixture, asker).await;
    let opened = harness.call("open_session", json!({"connection_id": conn.id})).await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    (harness, conn.id)
}

async fn begin(harness: &Harness<Asker>, id: &str) -> String {
    let result = harness.call("begin_transaction", json!({"connection_id": id})).await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    structured(&result)["transaction_id"].as_str().unwrap().to_owned()
}

async fn query(
    harness: &Harness<Asker>,
    id: &str,
    tx: Option<&str>,
    statement: &str,
) -> rmcp::model::CallToolResult {
    let mut arguments = json!({"connection_id": id, "statement": statement});
    if let Some(tx) = tx {
        arguments["transaction_id"] = json!(tx);
    }
    harness.call("execute_query", arguments).await
}

#[tokio::test]
async fn an_accepted_commit_makes_the_changes_final_and_the_question_lists_them() {
    let (harness, id) = writer_harness(Asker::answering(true, Ok(accepted(true)))).await;
    let tx = begin(&harness, &id).await;
    let inserted = query(&harness, &id, Some(&tx), INSERT).await;
    assert_eq!(code(&inserted), "ok", "{}", text(&inserted));
    // Not visible to the connection's own session before the commit.
    assert_eq!(count(&query(&harness, &id, None, COUNT).await), "0");
    assert_eq!(count(&query(&harness, &id, Some(&tx), COUNT).await), "1");
    let committed = harness
        .call("commit_transaction", json!({"transaction_id": tx}))
        .await;
    assert_eq!(code(&committed), "ok", "{}", text(&committed));
    assert_eq!(
        structured(&committed),
        json!({"transaction_id": tx, "committed": true, "changes": 1})
    );
    assert_eq!(count(&query(&harness, &id, None, COUNT).await), "1");
    // Two questions: the change inside the transaction, then the commit with the list.
    let asked = harness.client.service().seen.asked();
    assert_eq!(asked.len(), 2, "{asked:#?}");
    let (change, change_schema) = &asked[0];
    assert!(change.contains(&format!("runs inside transaction `{tx}`")), "{change}");
    assert!(change.contains("NOT committed"), "{change}");
    assert_eq!(change_schema["properties"]["run"]["title"], "Run this statement");
    let (commit, commit_schema) = &asked[1];
    assert!(commit.starts_with("An MCP client asks to COMMIT a transaction."), "{commit}");
    assert!(commit.contains(&format!("Transaction: `{tx}`")), "{commit}");
    assert!(commit.contains("Change 1 of 1"), "{commit}");
    assert!(commit.ends_with(&format!("\n```\n{INSERT}\n```")), "{commit}");
    assert_eq!(commit_schema["properties"]["run"]["title"], "Commit this transaction");
    // The audit log has the whole story.
    let entries = harness.fixture.audit_entries();
    for (tool, outcome) in [
        ("begin_transaction", "ok"),
        ("commit_transaction", "started"),
        ("commit_transaction", "ok"),
    ] {
        assert!(
            entries.iter().any(|e| e.tool == tool && e.outcome == outcome),
            "{tool} {outcome}: {entries:#?}"
        );
    }
}

#[tokio::test]
async fn a_declined_commit_commits_nothing_and_the_transaction_can_be_rolled_back() {
    let (asker, mut desk) = Asker::manual(true);
    let (harness, id) = writer_harness(asker).await;
    let tx = begin(&harness, &id).await;
    // The change is approved ...
    let change = harness
        .start_call(
            "execute_query",
            json!({"connection_id": id, "transaction_id": tx, "statement": INSERT}),
        )
        .await;
    within("the change question", desk.recv())
        .await
        .expect("the change is asked")
        .accept(true);
    let change = unwrap_call(within("the change", change.await_response()).await.unwrap());
    assert_eq!(code(&change), "ok", "{}", text(&change));
    // ... the commit is not.
    let commit = harness
        .start_call("commit_transaction", json!({"transaction_id": tx}))
        .await;
    let question = within("the commit question", desk.recv())
        .await
        .expect("the commit is asked");
    assert!(question.message.contains("COMMIT"), "{}", question.message);
    question.answer(Ok(rmcp::model::ElicitResult::new(
        rmcp::model::ElicitationAction::Decline,
    )));
    let declined = unwrap_call(within("the commit", commit.await_response()).await.unwrap());
    assert_eq!(code(&declined), "agent.write_refused");
    assert!(message(&declined).contains("still open"), "{}", message(&declined));
    // Nothing is committed; the transaction is still there and rolls back.
    assert_eq!(count(&query(&harness, &id, None, COUNT).await), "0");
    let rolled = harness
        .call("rollback_transaction", json!({"transaction_id": tx}))
        .await;
    assert_eq!(structured(&rolled)["rolled_back"], true);
    assert_eq!(structured(&rolled)["changes"], 1);
    assert_eq!(count(&query(&harness, &id, None, COUNT).await), "0");
}

#[tokio::test]
async fn begin_is_refused_where_a_write_would_be_refused_without_asking() {
    // Switch off.
    let fixture = Fixture::new();
    fixture.set_mcp_writes(false);
    let conn = fixture.connection(Some("read_write"));
    let harness = Harness::start_with(fixture, Asker::answering(true, Ok(accepted(true)))).await;
    let refused = harness
        .call("begin_transaction", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(code(&refused), "agent.write_refused");
    assert!(message(&refused).contains(REASON_SWITCH_OFF), "{}", message(&refused));
    // A client that cannot be asked.
    let fixture = Fixture::new();
    fixture.set_mcp_writes(true);
    let conn = fixture.connection(Some("read_write"));
    let harness = Harness::start_with(fixture, Asker::answering(false, Ok(accepted(true)))).await;
    let refused = harness
        .call("begin_transaction", json!({"connection_id": conn.id}))
        .await;
    assert!(message(&refused).contains(REASON_NO_ELICITATION), "{}", message(&refused));
    // A production type.
    let fixture = Fixture::new();
    fixture.set_mcp_writes(true);
    let conn = fixture.typed_connection(Some("read_write"), "production");
    let harness = Harness::start_with(fixture, Asker::answering(true, Ok(accepted(true)))).await;
    let refused = harness
        .call("begin_transaction", json!({"connection_id": conn.id}))
        .await;
    assert!(message(&refused).contains(REASON_PRODUCTION), "{}", message(&refused));
    // Read access is not enough either.
    let fixture = Fixture::new();
    fixture.set_mcp_writes(true);
    let conn = fixture.connection(Some("read"));
    let harness = Harness::start_with(fixture, Asker::answering(true, Ok(accepted(true)))).await;
    let refused = harness
        .call("begin_transaction", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(code(&refused), "agent.access_denied");
    // None of them asked anything.
    assert!(harness.client.service().seen.asked().is_empty());
}

#[tokio::test]
async fn shutting_the_server_down_rolls_an_open_transaction_back() {
    let fixture = Fixture::new();
    fixture.set_mcp_writes(true);
    let conn = fixture.connection(Some("read_write"));
    let layer = std::sync::Arc::clone(&fixture.layer);
    let harness = Harness::start_with(fixture, Asker::answering(true, Ok(accepted(true)))).await;
    let id = conn.id.clone();
    let opened = harness.call("open_session", json!({"connection_id": id})).await;
    assert_eq!(code(&opened), "ok");
    let tx = begin(&harness, &id).await;
    let inserted = query(&harness, &id, Some(&tx), INSERT).await;
    assert_eq!(code(&inserted), "ok", "{}", text(&inserted));
    // The client leaves without committing or rolling back.
    let Harness { client, server, .. } = harness;
    client.cancel().await.unwrap();
    server.await.unwrap().unwrap();
    // The layer was closed: the transaction is gone and nothing was committed.
    let gone = tokio::task::spawn_blocking({
        let (layer, id, tx) = (layer.clone(), id.clone(), tx.clone());
        move || {
            layer.call("open_session", json!({"connection_id": id}));
            let rows = layer.call(
                "execute_query",
                json!({"connection_id": id, "statement": COUNT}),
            );
            let again = layer.call(
                "execute_query",
                json!({"connection_id": id, "transaction_id": tx, "statement": "SELECT 1"}),
            );
            (rows, again)
        }
    })
    .await
    .unwrap();
    assert_eq!(gone.0.to_json()["rows"][0][0], "0");
    assert_eq!(gone.1.code(), Some("agent.transaction_gone"));
}

#[tokio::test]
async fn the_server_rolls_back_an_idle_transaction_on_its_own_timer() {
    use engine_agent::testing::{connection, FakeCatalog, FakeSession};
    use engine_agent::{AgentAccess, ConnectionKind, Session, ToolLayer, TxControl};
    use rmcp::ServiceExt;
    use std::sync::Arc;
    use tablex_mcp::approver::ElicitingApprover;

    let session = FakeSession::new(ConnectionKind::Sqlite).arc();
    let catalog = FakeCatalog::new(vec![connection(
        "fake-0",
        ConnectionKind::Sqlite,
        Some(AgentAccess::ReadWrite),
    )]);
    let provider = MapProvider(
        [("fake-0".to_owned(), Arc::clone(&session) as Arc<dyn Session>)].into(),
    );
    let fixture = Fixture::with_layer_in(|audit, settings| {
        ToolLayer::new(catalog, Arc::new(provider), "mcp")
            .with_transactions()
            .with_transaction_idle_timeout(Duration::from_millis(150))
            .with_approver(Arc::new(ElicitingApprover::new(Some(settings))))
            .with_audit(audit)
    });
    fixture.set_mcp_writes(true);
    let server = McpServer::new(Arc::clone(&fixture.layer))
        .unwrap()
        .with_idle_sweep_interval(Duration::from_millis(40));
    let (client_end, reader, writer) = pipe();
    let serving = tokio::spawn(server.serve(reader, writer, never));
    let client = Asker::answering(true, Ok(accepted(true)))
        .serve(client_end)
        .await
        .unwrap();
    let begun = unwrap_call(
        rmcp::model::ServerResult::CallToolResult(
            client
                .peer()
                .call_tool(params("begin_transaction", json!({"connection_id": "fake-0"})))
                .await
                .unwrap(),
        ),
    );
    assert_eq!(code(&begun), "ok", "{}", text(&begun));
    // Nobody calls again: the server's own timer rolls it back.
    poll("the idle rollback", async || {
        session.control_calls().contains(&TxControl::Rollback).then_some(())
    })
    .await;
    let entries = fixture.audit_entries();
    assert!(entries.iter().any(|entry| entry.tool == "expire_transaction"), "{entries:#?}");
    client.cancel().await.unwrap();
    serving.await.unwrap().unwrap();
}
