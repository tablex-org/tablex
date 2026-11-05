//! The real server driven by a real MCP client over an in-memory pipe: tool list, policy matrix,
//! result mapping, errors, secrets and audit.

mod support;

use engine_agent::schema::validate;
use rmcp::model::ErrorCode;
use serde_json::{json, Value};
use support::*;

#[tokio::test]
async fn tools_list_is_exactly_what_the_layer_says() {
    let harness = Harness::start(Fixture::new()).await;
    let listed = harness.peer().list_all_tools().await.unwrap();
    let specs = harness.fixture.layer.tools();
    assert_eq!(listed.len(), specs.len());
    for (tool, spec) in listed.iter().zip(specs) {
        assert_eq!(tool.name, spec.name);
        assert_eq!(tool.description.as_deref(), Some(spec.description));
        assert_eq!(
            Value::Object((*tool.input_schema).clone()),
            spec.input_schema
        );
        let output = tool
            .output_schema
            .as_ref()
            .map(|schema| Value::Object((**schema).clone()));
        assert_eq!(output, Some(spec.output_schema.clone()), "{}", spec.name);
    }
}

#[tokio::test]
async fn the_server_identifies_itself_and_says_where_access_is_set() {
    let harness = Harness::start(Fixture::new()).await;
    let info = harness.client.peer_info().expect("server info");
    assert_eq!(info.server_info.name, "tablex-mcp");
    assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    let instructions = info.instructions.clone().expect("instructions");
    assert!(instructions.contains("Agent access"), "{instructions}");
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn list_connections_shows_access_levels_and_no_uri_or_secret() {
    let harness = Harness::start(Fixture::new()).await;
    let off = harness.fixture.connection(None);
    let read = harness.fixture.connection(Some("read"));
    let result = harness.call("list_connections", json!({})).await;
    let body = structured(&result);
    let find = |id: &str| {
        body["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} not listed: {body}"))
    };
    assert_eq!(find(&off.id)["access"], "off");
    assert_eq!(find(&read.id)["access"], "read");
    assert_eq!(find(&read.id)["kind"], "sqlite");
    for serialized in [text(&result), serde_json::to_string(&result).unwrap()] {
        for secret in [FIXTURE_PASSWORD, URI_MARKER, "sqlite://", ".db"] {
            assert!(!serialized.contains(secret), "{secret} in {serialized}");
        }
    }
    validate(&output_schema(&harness, "list_connections"), &body).expect("matches the schema");
}

fn output_schema(harness: &Harness, tool: &str) -> Value {
    harness
        .fixture
        .layer
        .tools()
        .iter()
        .find(|spec| spec.name == tool)
        .unwrap()
        .output_schema
        .clone()
}

fn query(id: &str, statement: &str) -> Value {
    json!({"connection_id": id, "statement": statement})
}

#[tokio::test]
async fn access_off_refuses_open_session_with_the_typed_code() {
    let harness = Harness::start(Fixture::new()).await;
    let conn = harness.fixture.connection(Some("off"));
    let result = harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(result.structured_content.is_none());
    assert_eq!(code(&result), "agent.access_denied");
    assert!(!harness.fixture.layer.is_open(&conn.id));
    // Not set at all is `off` too.
    let unset = harness.fixture.connection(None);
    let result = harness
        .call("open_session", json!({"connection_id": unset.id}))
        .await;
    assert_eq!(code(&result), "agent.access_denied");
}

#[tokio::test]
async fn schema_only_describes_tables_but_returns_no_rows() {
    let harness = Harness::start(Fixture::new()).await;
    let conn = harness.fixture.connection(Some("schema"));
    let opened = harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    let described = harness
        .call(
            "describe_table",
            json!({"connection_id": conn.id, "table": "items"}),
        )
        .await;
    let body = structured(&described);
    assert!(body.to_string().contains("label"), "{body}");
    validate(&output_schema(&harness, "describe_table"), &body).expect("matches the schema");
    let refused = harness
        .call("execute_query", query(&conn.id, "SELECT label FROM items"))
        .await;
    assert_eq!(code(&refused), "agent.access_denied");
    assert!(!text(&refused).contains("alpha"));
}

#[tokio::test]
async fn read_returns_rows_as_structured_content_and_refuses_writes() {
    let harness = Harness::start(Fixture::new()).await;
    let conn = harness.fixture.connection(Some("read"));
    let opened = harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    let rows = harness
        .call(
            "execute_query",
            query(&conn.id, "SELECT id, label FROM items ORDER BY id"),
        )
        .await;
    assert_eq!(rows.is_error, Some(false));
    let body = structured(&rows);
    validate(&output_schema(&harness, "execute_query"), &body).expect("matches the schema");
    assert_eq!(text(&rows), body.to_string(), "the text is the same result");
    let shown = body.to_string();
    assert!(
        shown.contains("alpha") && shown.contains("gamma"),
        "{shown}"
    );
    let write = harness
        .call(
            "execute_query",
            query(&conn.id, "INSERT INTO items(label) VALUES ('delta')"),
        )
        .await;
    assert_eq!(code(&write), "agent.write_refused");
    let count = harness
        .call(
            "execute_query",
            query(&conn.id, "SELECT count(*) AS n FROM items"),
        )
        .await;
    assert!(
        structured(&count).to_string().contains('3'),
        "nothing was inserted"
    );
}

#[tokio::test]
async fn read_write_still_refuses_writes_because_there_is_no_approver() {
    let harness = Harness::start(Fixture::new()).await;
    let conn = harness.fixture.connection(Some("read_write"));
    harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    for statement in [
        "INSERT INTO items(label) VALUES ('delta')",
        "DELETE FROM items",
        "DROP TABLE items",
    ] {
        let result = harness
            .call("execute_query", query(&conn.id, statement))
            .await;
        assert_eq!(result.is_error, Some(true), "{statement}");
        assert!(
            ["agent.write_refused", "agent.dangerous_refused"].contains(&code(&result).as_str()),
            "{statement}: {}",
            text(&result)
        );
    }
    let count = harness
        .call(
            "execute_query",
            query(&conn.id, "SELECT count(*) AS n FROM items"),
        )
        .await;
    assert!(structured(&count).to_string().contains('3'));
}

#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error_and_bad_arguments_a_tool_error() {
    let harness = Harness::start(Fixture::new()).await;
    let error = harness
        .try_call("drop_everything", json!({}))
        .await
        .expect_err("unknown tool");
    match error {
        rmcp::ServiceError::McpError(data) => assert_eq!(data.code, ErrorCode::INVALID_PARAMS),
        other => panic!("not a protocol error: {other:?}"),
    }
    let invalid = harness
        .call("open_session", json!({"connection_id": 7}))
        .await;
    assert_eq!(invalid.is_error, Some(true));
    assert_eq!(code(&invalid), "agent.invalid_arguments");
    let missing = harness.call("describe_table", json!({})).await;
    assert_eq!(code(&missing), "agent.invalid_arguments");
}

#[tokio::test]
async fn a_call_is_audited_as_mcp_without_any_secret() {
    let harness = Harness::start(Fixture::new()).await;
    let conn = harness.fixture.connection(Some("read"));
    harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    harness
        .call("execute_query", query(&conn.id, "SELECT label FROM items"))
        .await;
    let entries = harness.fixture.audit_entries();
    let select = entries
        .iter()
        .find(|entry| entry.tool == "execute_query")
        .expect("the query is audited");
    assert_eq!(select.front_end, "mcp");
    assert_eq!(select.outcome, "ok");
    assert_eq!(select.connection_id.as_deref(), Some(conn.id.as_str()));
    assert_eq!(select.statement.as_deref(), Some("SELECT label FROM items"));
    assert!(entries.iter().all(|entry| entry.front_end == "mcp"));
    let raw = std::fs::read_to_string(&harness.fixture.audit_path).unwrap();
    for secret in [FIXTURE_PASSWORD, URI_MARKER, "alpha"] {
        assert!(!raw.contains(secret), "{secret} in the audit log");
    }
}

/// The forms a saved password can take on its way into a URI.
fn password_forms() -> Vec<String> {
    vec![
        FIXTURE_PASSWORD.to_owned(),
        "p%40ss%2Fw0rd%3AZq81-Lm".to_owned(),
        "p%40ss%2fw0rd%3aZq81-Lm".to_owned(),
        "w0rd:Zq81".to_owned(),
        "127.0.0.1".to_owned(),
    ]
}

#[tokio::test]
async fn a_saved_password_and_the_address_never_reach_a_response_or_the_audit_log() {
    let harness = Harness::start(Fixture::new()).await;
    let id = harness.fixture.secret_connection();
    let opened = harness
        .call("open_session", json!({"connection_id": id}))
        .await;
    // It fails at driver loading, after the runtime URI was built with the password...
    assert_eq!(opened.is_error, Some(true), "{}", text(&opened));
    assert!(code(&opened).starts_with("connect."), "{}", text(&opened));
    // ...and the password really was read from the credential store (the test can fail: without
    // this read there is nothing that could leak).
    let loaded = harness.fixture.keys.loaded_accounts();
    assert!(
        loaded
            .iter()
            .any(|account| account.eq_ignore_ascii_case(&id)),
        "the credential store was not asked for the connection's password: {loaded:?}"
    );
    let listed = harness.call("list_connections", json!({})).await;
    let audit = std::fs::read_to_string({
        harness.fixture.audit.flush();
        &harness.fixture.audit_path
    })
    .unwrap();
    assert!(audit.contains(&id), "the failed open is audited");
    for (what, haystack) in [
        ("open_session text", text(&opened)),
        (
            "open_session frame",
            serde_json::to_string(&opened).unwrap(),
        ),
        ("list_connections", serde_json::to_string(&listed).unwrap()),
        ("audit log", audit),
    ] {
        for form in password_forms() {
            assert!(
                !haystack.contains(&form),
                "{form} in the {what}: {haystack}"
            );
        }
    }
}

#[tokio::test]
async fn a_failing_open_does_not_say_where_the_database_is() {
    let harness = Harness::start(Fixture::new()).await;
    let id = harness.fixture.unreachable_sqlite_connection();
    let opened = harness
        .call("open_session", json!({"connection_id": id}))
        .await;
    assert_eq!(opened.is_error, Some(true), "{}", text(&opened));
    let audit = {
        harness.fixture.audit.flush();
        std::fs::read_to_string(&harness.fixture.audit_path).unwrap()
    };
    for haystack in [serde_json::to_string(&opened).unwrap(), audit] {
        for needle in [URI_MARKER, "no-such-dir", "x.db"] {
            assert!(!haystack.contains(needle), "{needle} in {haystack}");
        }
    }
}

#[tokio::test]
async fn tools_call_without_arguments_is_accepted() {
    let harness = Harness::start(Fixture::new()).await;
    // `params` leaves `arguments` out for a non-object.
    let params = params("list_connections", Value::Null);
    assert!(params.arguments.is_none());
    let result = harness.peer().call_tool(params).await.expect("tools/call");
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert!(structured(&result)["connections"].is_array());
}

#[tokio::test]
async fn a_call_that_panics_is_an_internal_error_result_and_the_server_keeps_serving() {
    use engine_agent::testing::{Behavior, FakeSession};
    use engine_agent::ConnectionKind;
    use tablex_mcp::server::MAX_CONCURRENT_CALLS;
    // One connection per permit: the panic must give its permit back, which only shows when all
    // MAX_CONCURRENT_CALLS calls run at once afterwards.
    let ids = fake_ids(MAX_CONCURRENT_CALLS);
    let mut sessions = Vec::new();
    let fixture = fake_fixture(&ids, |_| {
        let session = FakeSession::new(ConnectionKind::Sqlite).arc();
        sessions.push(session.clone());
        session
    });
    let harness = Harness::start(fixture).await;
    for id in &ids {
        let opened = harness
            .call("open_session", json!({"connection_id": id}))
            .await;
        assert_eq!(code(&opened), "ok", "{}", text(&opened));
    }
    let args = |id: &str| json!({"connection_id": id, "statement": "SELECT 1"});

    sessions[0].set_behavior(Behavior::Script(Box::new(|_, _| {
        panic!("boom-payload-with-a-secret-p@ss")
    })));
    let result = harness.call("execute_query", args(&ids[0])).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(code(&result), "mcp.internal");
    assert!(result.structured_content.is_none());
    assert!(!serde_json::to_string(&result)
        .unwrap()
        .contains("boom-payload"));
    // The layer left its own line for the call that never returned, without the payload.
    let audited = harness.fixture.audit_entries();
    let line = audited
        .iter()
        .find(|entry| entry.tool == "execute_query" && entry.outcome == "agent.internal")
        .expect("the panicking call was audited");
    assert_eq!(line.connection_id.as_deref(), Some(ids[0].as_str()));
    assert!(!std::fs::read_to_string(&harness.fixture.audit_path)
        .unwrap()
        .contains("boom-payload"));

    // The session still answers ...
    sessions[0].set_behavior(Behavior::Numbers { count: 2 });
    let again = harness.call("execute_query", args(&ids[0])).await;
    assert_eq!(code(&again), "ok", "{}", text(&again));
    assert_eq!(structured(&again)["row_count"], 2);

    // ... and the permit came back: all sixteen calls can be in flight at once (with the permit
    // leaked the sixteenth would be answered `agent.limit` at once).
    let baseline: Vec<usize> = sessions.iter().map(|session| session.run_count()).collect();
    for session in &sessions {
        session.set_behavior(Behavior::BlockUntilCancelled);
    }
    let mut held = Vec::new();
    for id in &ids {
        held.push(harness.start_call("execute_query", args(id)).await);
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while sessions
        .iter()
        .zip(&baseline)
        .any(|(session, before)| session.run_count() == *before)
    {
        for handle in &mut held {
            if let Ok(answer) = handle.rx.try_recv() {
                let refused = unwrap_call(answer.expect("answered"));
                panic!("a call was answered at once: {}", text(&refused));
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not all calls started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    for handle in held {
        handle.cancel(None).await.unwrap();
    }
}
