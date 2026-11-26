//! MCP resources, the user's tool allowlist in `settings.json`, and the owner of a pinned session
//! or a transaction when two clients share one layer: the real server and the real SQLite driver
//! over an in-memory pipe.

mod support;

use std::sync::Arc;

use rmcp::model::ReadResourceRequestParams;
use rmcp::{RoleClient, ServiceExt};
use rmcp::service::RunningService;
use serde_json::{json, Value};
use support::*;
use tablex_mcp::server::McpServer;

fn contents(result: &rmcp::model::ReadResourceResult) -> (String, String) {
    match &result.contents[0] {
        rmcp::model::ResourceContents::TextResourceContents { text, mime_type, .. } => {
            (text.clone(), mime_type.clone().unwrap_or_default())
        }
        other => panic!("{other:?}"),
    }
}

async fn read(harness: &Harness, uri: &str) -> Result<(String, String), String> {
    match harness.peer().read_resource(ReadResourceRequestParams::new(uri)).await {
        Ok(result) => Ok(contents(&result)),
        Err(error) => Err(error.to_string()),
    }
}

#[tokio::test]
async fn the_server_offers_the_connections_and_each_schema_as_resources() {
    let fixture = Fixture::new();
    let readable = fixture.connection(Some("read"));
    let hidden = fixture.connection(None);
    let harness = Harness::start(fixture).await;
    let info = harness.client.peer_info().expect("initialized");
    assert!(info.capabilities.resources.is_some(), "the server declares resources");
    let resources = harness.peer().list_all_resources().await.unwrap();
    let uris: Vec<&str> = resources.iter().map(|resource| resource.uri.as_str()).collect();
    assert!(uris.contains(&"tablex://connections"), "{uris:?}");
    assert!(uris.contains(&format!("tablex://{}/schema", readable.id).as_str()), "{uris:?}");
    assert!(!uris.iter().any(|uri| uri.contains(&hidden.id)), "a connection at off has nothing to read: {uris:?}");
    let templates = harness.peer().list_all_resource_templates().await.unwrap();
    let templates: Vec<&str> = templates.iter().map(|template| template.uri_template.as_str()).collect();
    assert!(templates.iter().any(|template| template.starts_with("tablex://{connection_id}/schema")), "{templates:?}");
    assert!(templates.iter().any(|template| template.contains("{database}/tables")), "{templates:?}");
}

#[tokio::test]
async fn reading_a_resource_is_calling_its_tool() {
    let fixture = Fixture::new();
    let readable = fixture.connection(Some("read"));
    let schema_only = fixture.connection(Some("schema"));
    let hidden = fixture.connection(None);
    let harness = Harness::start(fixture).await;
    // The connection list: the same JSON as `list_connections`.
    let (text, mime) = read(&harness, "tablex://connections").await.unwrap();
    assert_eq!(mime, "application/json");
    let listed: Value = serde_json::from_str(&text).unwrap();
    let tool = structured(&harness.call("list_connections", json!({})).await);
    assert_eq!(listed, tool);
    assert!(listed["connections"].as_array().unwrap().iter().any(|c| c["id"] == readable.id.as_str()));
    // The schema digest, with no session opened first.
    let (text, mime) = read(&harness, &format!("tablex://{}/schema", readable.id)).await.unwrap();
    assert_eq!(mime, "text/plain");
    assert!(text.contains("items"), "{text}");
    // The schema of a connection that lets the agent see only the schema is a schema.
    assert!(read(&harness, &format!("tablex://{}/schema", schema_only.id)).await.is_ok());
    // The tables of a database, as the tool lists them, with a page cursor taken from the query.
    let (text, mime) = read(&harness, &format!("tablex://{}/main/tables", readable.id)).await.unwrap();
    assert_eq!(mime, "application/json");
    let tables: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(tables["tables"][0]["name"], "items", "{tables}");
    // The access of the connection decides, exactly as for the tool.
    let refused = read(&harness, &format!("tablex://{}/schema", hidden.id)).await.unwrap_err();
    assert!(refused.contains("access"), "{refused}");
    // What is not a resource is not found.
    for uri in ["tablex://", "tablex://nope", "tablex://a/b/c/d", "file:///etc/passwd", "tablex://%zz/schema"] {
        assert!(read(&harness, uri).await.is_err(), "{uri}");
    }
}

#[tokio::test]
async fn a_resource_answers_to_the_tool_allowlist() {
    let fixture = Fixture::new();
    let readable = fixture.connection(Some("read"));
    fixture.write_settings(&json!({"schema_version": 4, "ai": {"mcp_tools": ["list_connections", "open_session", "list_tables"]}}).to_string());
    let harness = Harness::start(fixture).await;
    // Allowed: its tools are.
    assert!(read(&harness, "tablex://connections").await.is_ok());
    assert!(read(&harness, &format!("tablex://{}/main/tables", readable.id)).await.is_ok());
    // Not allowed: `get_schema_context` is not, so the schema is not readable either, and it is
    // not offered.
    let refused = read(&harness, &format!("tablex://{}/schema", readable.id)).await.unwrap_err();
    assert!(refused.contains("ai.mcp_tools"), "{refused}");
    let resources = harness.peer().list_all_resources().await.unwrap();
    assert!(resources.iter().all(|resource| !resource.uri.ends_with("/schema")));
}

#[tokio::test]
async fn the_users_tool_allowlist_is_read_at_every_call_and_filters_the_tool_list() {
    let fixture = Fixture::new();
    let conn = fixture.connection(Some("read"));
    let settings = fixture.settings_path();
    let harness = Harness::start(fixture).await;
    let names = |tools: Vec<rmcp::model::Tool>| -> Vec<String> { tools.into_iter().map(|tool| tool.name.to_string()).collect() };
    let all = names(harness.peer().list_all_tools().await.unwrap());
    assert!(all.contains(&"execute_query".to_owned()) && all.len() > 20, "{all:?}");
    // Only two tools.
    std::fs::write(&settings, json!({"schema_version": 4, "ai": {"mcp_tools": ["list_connections", "open_session"]}}).to_string()).unwrap();
    let some = names(harness.peer().list_all_tools().await.unwrap());
    assert_eq!(some, ["list_connections", "open_session"]);
    assert_eq!(code(&harness.call("list_connections", json!({})).await), "ok");
    let refused = harness.call("execute_query", json!({"connection_id": conn.id, "statement": "SELECT 1"})).await;
    assert_eq!(code(&refused), "agent.access_denied");
    assert!(text(&refused).contains("ai.mcp_tools"), "{}", text(&refused));
    // A list that cannot be used allows nothing; a file without it allows everything again.
    std::fs::write(&settings, r#"{"schema_version": 4, "ai": {"mcp_tools": "execute_query"}}"#).unwrap();
    assert!(harness.peer().list_all_tools().await.unwrap().is_empty());
    assert_eq!(code(&harness.call("list_connections", json!({})).await), "agent.access_denied");
    std::fs::write(&settings, json!({"schema_version": 4, "ai": {}}).to_string()).unwrap();
    assert_eq!(names(harness.peer().list_all_tools().await.unwrap()), all);
    assert_eq!(code(&harness.call("execute_query", json!({"connection_id": conn.id, "statement": "SELECT 1"})).await), "agent.not_connected");
}

/// A second client of the same layer, with an owner of its own.
async fn second_client<H: rmcp::ClientHandler>(harness: &Harness<H>, owner: &str) -> RunningService<RoleClient, ()> {
    let (client_end, server_end) = tokio::io::duplex(1 << 20);
    let (reader, writer) = tokio::io::split(server_end);
    let server = McpServer::new(Arc::clone(&harness.fixture.layer)).unwrap().with_owner(owner.to_owned());
    tokio::spawn(server.serve(reader, writer, never));
    ().serve(client_end).await.expect("initialize")
}

async fn call_on(client: &RunningService<RoleClient, ()>, tool: &str, arguments: Value) -> rmcp::model::CallToolResult {
    client.peer().call_tool(params(tool, arguments)).await.expect("tools/call")
}

#[tokio::test]
async fn a_pinned_session_and_a_transaction_belong_to_the_client_that_opened_them() {
    let fixture = Fixture::new();
    let conn = fixture.connection(Some("read_write"));
    fixture.set_mcp_writes(true);
    let harness = Harness::start_with(fixture, Asker::answering(true, Ok(accepted(true)))).await;
    let other = second_client(&harness, "another-client").await;
    // The first client (the owner "") opens a pinned session and begins a transaction.
    assert_eq!(code(&harness.call("open_session", json!({"connection_id": conn.id})).await), "ok");
    let opened = harness.call("open_session", json!({"connection_id": conn.id, "pinned": true})).await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    let session = structured(&opened)["session_id"].as_str().unwrap().to_owned();
    let begun = harness.call("begin_transaction", json!({"connection_id": conn.id})).await;
    assert_eq!(code(&begun), "ok", "{}", text(&begun));
    let tx = structured(&begun)["transaction_id"].as_str().unwrap().to_owned();
    // It uses them.
    let own = harness.call("execute_query", json!({"connection_id": conn.id, "session_id": session, "statement": "SELECT count(*) FROM items"})).await;
    assert_eq!(code(&own), "ok", "{}", text(&own));
    // The other client, with the ids, is told they do not exist.
    for (tool, arguments, words) in [
        ("execute_query", json!({"connection_id": conn.id, "session_id": session, "statement": "SELECT 1"}), "unknown session"),
        ("close_session", json!({"session_id": session}), "unknown session"),
        ("execute_query", json!({"connection_id": conn.id, "transaction_id": tx, "statement": "SELECT 1"}), "no open transaction"),
        ("commit_transaction", json!({"transaction_id": tx}), "no open transaction"),
        ("rollback_transaction", json!({"transaction_id": tx}), "no open transaction"),
    ] {
        let result = call_on(&other, tool, arguments).await;
        assert_eq!(result.is_error, Some(true), "{tool}");
        assert!(text(&result).contains(words), "{tool}: {}", text(&result));
    }
    // Both are intact, and the owner ends them.
    let still = harness.call("execute_query", json!({"connection_id": conn.id, "session_id": session, "statement": "SELECT 1"})).await;
    assert_eq!(code(&still), "ok");
    assert_eq!(code(&harness.call("rollback_transaction", json!({"transaction_id": tx})).await), "ok");
    assert_eq!(code(&harness.call("close_session", json!({"session_id": session})).await), "ok");
    other.cancel().await.unwrap();
}
