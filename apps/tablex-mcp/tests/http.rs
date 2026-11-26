//! The Streamable HTTP transport: the real server on a loopback socket, driven by raw HTTP for what
//! a client library would hide (missing and wrong tokens, forged `Host` and `Origin`, oversized
//! bodies, stalled connections) and by an in-process rmcp client for what the protocol does
//! (a full session, two sessions with their own approvals). The built binary is checked for its
//! address rules and its token. Nothing here leaves the machine or touches the user's stores.

#![cfg(feature = "http")]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeSession};
use engine_agent::{AgentAccess, ConnectionKind, Session, ToolLayer};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, ServiceExt};
use serde_json::json;
use support::*;
use tablex_mcp::approver::ElicitingApprover;
use tablex_mcp::http::HttpConfig;
use tablex_mcp::server::McpServer;
use tablex_mcp::token::Token;
use tablex_mcp::transport::ServeError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const TOKEN: &str = "test-token-0123456789abcdef";

struct Http {
    fixture: Fixture,
    addr: SocketAddr,
    stop: Arc<Notify>,
    server: tokio::task::JoinHandle<Result<(), ServeError>>,
}

impl Http {
    async fn start(fixture: Fixture) -> Self {
        Self::start_with(fixture, |_| {}).await
    }

    async fn start_with(fixture: Fixture, tweak: impl FnOnce(&mut HttpConfig)) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = HttpConfig::new(Token::parse(TOKEN).unwrap());
        tweak(&mut config);
        let server = McpServer::new(Arc::clone(&fixture.layer)).unwrap();
        let stop = Arc::new(Notify::new());
        let stopped = Arc::clone(&stop);
        let server = tokio::spawn(server.serve_http(listener, config, async move || {
            stopped.notified().await;
        }));
        Self {
            fixture,
            addr,
            stop,
            server,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// An rmcp client of this server.
    async fn client<H: ClientHandler>(&self, handler: H) -> rmcp::service::RunningService<rmcp::RoleClient, H> {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(self.url()).auth_header(TOKEN),
        );
        within("the client handshake", handler.serve(transport))
            .await
            .expect("initialize over HTTP")
    }

    /// One raw request with this server's `Host`, the right token and the headers a client sends.
    async fn post(&self, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut all = vec![
            ("Authorization", format!("Bearer {TOKEN}")),
            ("Accept", "application/json, text/event-stream".to_owned()),
            ("Content-Type", "application/json".to_owned()),
        ];
        all.extend(headers.iter().map(|(name, value)| (*name, (*value).to_owned())));
        send(self.addr, "POST", "/mcp", &all, body).await
    }

    async fn stop(self) {
        self.stop.notify_one();
        within("the server to stop", self.server)
            .await
            .unwrap()
            .expect("a clean stop");
    }
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

/// One HTTP/1.1 request on a socket of its own, `Connection: close`; the reply is read to the end
/// of the stream or for one second, whichever comes first (an event stream does not end).
async fn send(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, String)], body: &str) -> Reply {
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("host")) {
        request.push_str(&format!("Host: {addr}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
    {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    request.push_str(body);
    raw(addr, request.as_bytes()).await
}

async fn raw(addr: SocketAddr, request: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // A server that refuses early may close before all of a large body is written.
    let _ = stream.write_all(request).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut bytes)).await;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    Reply {
        status,
        headers: head.to_owned(),
        body: body.to_owned(),
    }
}

fn initialize_body() -> String {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "raw-client", "version": "0"}
        }
    })
    .to_string()
}

/// Fake SQLite connections `fake-0`, ... with access `read_write`, over sessions that record every
/// statement, behind the real approver (the switch on).
fn fakes(count: usize) -> (Fixture, Vec<Arc<FakeSession>>, Vec<String>) {
    fakes_with(count, false)
}

/// [`fakes`], with the transactions and pinned sessions of the layer on when `sessions_on`.
fn fakes_with(count: usize, sessions_on: bool) -> (Fixture, Vec<Arc<FakeSession>>, Vec<String>) {
    let ids = fake_ids(count);
    let sessions: Vec<Arc<FakeSession>> = ids
        .iter()
        .map(|_| {
            let session = FakeSession::new(ConnectionKind::Sqlite).arc();
            session.set_behavior(Behavior::Affected(1));
            session
        })
        .collect();
    let catalog = FakeCatalog::new(
        ids.iter()
            .map(|id| {
                let mut info = connection(id, ConnectionKind::Sqlite, Some(AgentAccess::ReadWrite));
                info.name = format!("Warehouse {id}");
                info
            })
            .collect(),
    );
    let provider = MapProvider(
        ids.iter()
            .cloned()
            .zip(sessions.iter().map(|s| Arc::clone(s) as Arc<dyn Session>))
            .collect(),
    );
    let fixture = Fixture::with_layer_in(|audit, settings| {
        let layer = ToolLayer::new(catalog, Arc::new(provider), "mcp")
            .with_approver(Arc::new(ElicitingApprover::new(Some(settings))))
            .with_audit(audit);
        if sessions_on {
            layer.with_transactions().with_pinned_sessions()
        } else {
            layer
        }
    });
    fixture.set_mcp_writes(true);
    (fixture, sessions, ids)
}

fn runs(session: &FakeSession) -> Vec<String> {
    session
        .runs
        .lock()
        .unwrap()
        .iter()
        .map(|(text, _)| text.clone())
        .collect()
}

// ---- the token ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_request_needs_the_right_token_and_a_refusal_says_nothing() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start(fixture).await;
    let init = initialize_body();
    let json = |extra: Vec<(&'static str, String)>| {
        let mut headers = vec![
            ("Accept", "application/json, text/event-stream".to_owned()),
            ("Content-Type", "application/json".to_owned()),
        ];
        headers.extend(extra);
        headers
    };
    let wrong = [
        // No header at all.
        json(vec![]),
        // The wrong token, the same length; a prefix; the token with more after it; another case.
        json(vec![("Authorization", "Bearer test-token-0123456789abcdeX".to_owned())]),
        json(vec![("Authorization", "Bearer test-token-0123456789abcde".to_owned())]),
        json(vec![("Authorization", format!("Bearer {TOKEN}0"))]),
        json(vec![("Authorization", format!("Bearer {}", TOKEN.to_uppercase()))]),
        // The right token with another scheme, with no scheme, with no value at all.
        json(vec![("Authorization", format!("Basic {TOKEN}"))]),
        json(vec![("Authorization", TOKEN.to_owned())]),
        json(vec![("Authorization", "Bearer".to_owned())]),
        json(vec![("Authorization", "Bearer ".to_owned())]),
    ];
    for headers in &wrong {
        let reply = send(http.addr, "POST", "/mcp", headers, &init).await;
        assert_eq!(reply.status, 401, "{headers:?}");
        assert_eq!(reply.body, "", "a refusal has no body: {headers:?}");
        assert!(!reply.headers.contains(TOKEN));
    }
    // The token in the query string or another header is not a token.
    let reply = send(
        http.addr,
        "POST",
        &format!("/mcp?token={TOKEN}"),
        &json(vec![("X-Api-Key", TOKEN.to_owned())]),
        &init,
    )
    .await;
    assert_eq!(reply.status, 401);
    // Every method and path is behind the token, so nothing is learned from the answers.
    for (method, path) in [("GET", "/mcp"), ("DELETE", "/mcp"), ("POST", "/elsewhere"), ("GET", "/")] {
        let reply = send(http.addr, method, path, &json(vec![]), "").await;
        assert_eq!((reply.status, reply.body.as_str()), (401, ""), "{method} {path}");
    }
    // The right token, in any case of the scheme, gets through to the protocol.
    for scheme in ["Bearer", "bearer", "BEARER"] {
        let reply = http
            .post(&[("Authorization", &format!("{scheme} {TOKEN}"))], &init)
            .await;
        assert_eq!(reply.status, 200, "{scheme}: {}", reply.headers);
        assert!(reply.header("mcp-session-id").is_some());
    }
    // And a path that is not the endpoint is a 404 only after the token.
    let reply = send(
        http.addr,
        "GET",
        "/elsewhere",
        &[("Authorization", format!("Bearer {TOKEN}"))],
        "",
    )
    .await;
    assert_eq!(reply.status, 404);
    http.stop().await;
}

// ---- Host and Origin ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_forged_host_or_origin_is_refused_after_the_token() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start(fixture).await;
    let init = initialize_body();
    let port = http.addr.port();
    let status = |reply: &Reply| reply.status;

    for host in ["localhost", "127.0.0.1", "localhost:80", &format!("127.0.0.1:{port}"), &format!("[::1]:{port}")] {
        let reply = http.post(&[("Host", host)], &init).await;
        assert_eq!(status(&reply), 200, "Host: {host}");
    }
    for host in ["evil.example", &format!("evil.example:{port}"), "127.0.0.1.evil.example", "0.0.0.0", "192.168.1.5"] {
        let reply = http.post(&[("Host", host)], &init).await;
        assert_eq!(status(&reply), 403, "Host: {host}");
    }
    // Without an Origin (every non-browser client) the request passes; a loopback page passes.
    for origin in ["http://localhost:3000", "http://127.0.0.1:6274", "http://[::1]:8080"] {
        let reply = http.post(&[("Origin", origin)], &init).await;
        assert_eq!(status(&reply), 200, "Origin: {origin}");
    }
    // A page of another site, whatever it claims, even with the right Host (a rebound name has
    // its own name in Host; a forged Host is checked above).
    for origin in ["https://evil.example", "http://evil.example:3000", "null", "http://localhost.evil.example", "https://localhost:3000", "garbage"] {
        let reply = http.post(&[("Origin", origin)], &init).await;
        assert_eq!(status(&reply), 403, "Origin: {origin}");
    }
    // The token comes first: a forged page without it learns only 401.
    let reply = send(
        http.addr,
        "POST",
        "/mcp",
        &[
            ("Host", "evil.example".to_owned()),
            ("Origin", "https://evil.example".to_owned()),
            ("Content-Type", "application/json".to_owned()),
            ("Accept", "application/json, text/event-stream".to_owned()),
        ],
        &init,
    )
    .await;
    assert_eq!((reply.status, reply.body.as_str()), (401, ""));
    http.stop().await;

    // Names and pages the user added (for --allow-remote) pass; the rest still does not.
    let (fixture, _, _) = fakes(1);
    let http = Http::start_with(fixture, |config| {
        config.extra_hosts = vec!["mcp.internal".into()];
        config.extra_origins = vec!["https://tools.internal:8443".into()];
    })
    .await;
    assert_eq!(http.post(&[("Host", "mcp.internal")], &init).await.status, 200);
    assert_eq!(http.post(&[("Host", "other.internal")], &init).await.status, 403);
    assert_eq!(
        http.post(&[("Origin", "https://tools.internal:8443")], &init).await.status,
        200
    );
    assert_eq!(
        http.post(&[("Origin", "https://tools.internal:9")], &init).await.status,
        403
    );
    http.stop().await;
}

// ---- limits ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_body_over_the_cap_is_refused_however_it_is_sent() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start_with(fixture, |config| config.max_body_bytes = 2048).await;
    let padding = |n: usize| format!(",\"pad\":\"{}\"", "x".repeat(n));
    let with_padding = |n: usize| {
        let mut body = initialize_body();
        body.insert_str(body.len() - 1, &padding(n));
        body
    };
    // Under the cap it is served.
    assert_eq!(http.post(&[], &with_padding(100)).await.status, 200);
    // Over it by Content-Length: refused before the body is read.
    let big = with_padding(4000);
    assert!(big.len() > 2048);
    assert_eq!(http.post(&[], &big).await.status, 413);
    // Over it without a Content-Length (chunked): refused as it is read.
    let chunked = format!("{:x}\r\n{big}\r\n0\r\n\r\n", big.len());
    let reply = http.post(&[("Transfer-Encoding", "chunked")], &chunked).await;
    assert_eq!(reply.status, 413, "{}", reply.headers);
    // A lie about the length does not help either: the cap is on what is read.
    let reply = raw(
        http.addr,
        format!(
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\n\
             Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n\
             Content-Length: 100\r\n\r\n{big}",
            http.addr
        )
        .as_bytes(),
    )
    .await;
    assert_ne!(reply.status, 200);
    // The server is fine afterwards.
    assert_eq!(http.post(&[], &initialize_body()).await.status, 200);
    http.stop().await;
}

#[tokio::test]
async fn sessions_are_capped_and_a_closed_one_frees_its_place() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start_with(fixture, |config| config.max_sessions = 2).await;
    let init = initialize_body();
    let first = http.post(&[], &init).await;
    let second = http.post(&[], &init).await;
    assert_eq!((first.status, second.status), (200, 200));
    let third = http.post(&[], &init).await;
    assert_eq!((third.status, third.body.as_str()), (503, ""));
    // Not an initialize: not a new session, so not counted against the cap.
    let first_id = first.header("mcp-session-id").unwrap().to_owned();
    // The client says it is done (DELETE): the place is free.
    let deleted = send(
        http.addr,
        "DELETE",
        "/mcp",
        &[
            ("Authorization", format!("Bearer {TOKEN}")),
            ("Mcp-Session-Id", first_id.clone()),
            ("MCP-Protocol-Version", "2025-06-18".to_owned()),
        ],
        "",
    )
    .await;
    assert_eq!(deleted.status, 202, "{}", deleted.headers);
    poll("a place after the DELETE", async || {
        (http.post(&[], &init).await.status == 200).then_some(())
    })
    .await;
    http.stop().await;
}

#[tokio::test]
async fn a_session_nobody_uses_is_closed_after_the_idle_timeout() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start_with(fixture, |config| {
        config.idle_timeout = Duration::from_millis(300);
        config.max_sessions = 1;
    })
    .await;
    let opened = http.post(&[], &initialize_body()).await;
    assert_eq!(opened.status, 200);
    let id = opened.header("mcp-session-id").unwrap().to_owned();
    // The only place is taken...
    assert_eq!(http.post(&[], &initialize_body()).await.status, 503);
    // ...until the timeout frees it; the old session is gone for its client.
    poll("the idle session to be closed", async || {
        (http.post(&[], &initialize_body()).await.status == 200).then_some(())
    })
    .await;
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string();
    let reply = http
        .post(&[("Mcp-Session-Id", &id), ("MCP-Protocol-Version", "2025-06-18")], &list)
        .await;
    assert_eq!(reply.status, 404, "{}", reply.headers);
    http.stop().await;
}

#[tokio::test]
async fn a_connection_that_sends_nothing_is_dropped_and_connections_are_capped() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start_with(fixture, |config| {
        config.header_timeout = Duration::from_millis(300);
        config.max_connections = 2;
    })
    .await;
    // Two idle connections fill the cap; a third is closed at once.
    let mut idle = vec![
        TcpStream::connect(http.addr).await.unwrap(),
        TcpStream::connect(http.addr).await.unwrap(),
    ];
    // Let the server accept both before the third arrives.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut third = TcpStream::connect(http.addr).await.unwrap();
    let mut byte = [0u8; 1];
    let read = within("the third connection to be closed", third.read(&mut byte)).await;
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    // The two that never send a request are dropped at the header timeout.
    for stream in &mut idle {
        let read = within("the idle connection to be closed", stream.read(&mut byte)).await;
        assert!(matches!(read, Ok(0) | Ok(_) | Err(_)));
    }
    // And a place is free again.
    poll("a free connection", async || {
        (http.post(&[], &initialize_body()).await.status == 200).then_some(())
    })
    .await;
    http.stop().await;
}

// ---- the protocol over HTTP --------------------------------------------------------------------------

#[tokio::test]
async fn initialize_list_and_call_round_trip_and_the_audit_names_the_transport_and_client() {
    let (fixture, sessions, ids) = fakes(1);
    sessions[0].set_behavior(Behavior::Numbers { count: 1 });
    let http = Http::start(fixture).await;
    let asker = Asker::answering(false, Ok(accepted(false)));
    let client = http.client(asker).await;
    let info = client.peer_info().expect("the server answered initialize");
    assert_eq!(info.server_info.as_ref().expect("server info").name, "tablex-mcp");
    let tools = within("tools/list", client.peer().list_all_tools()).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    for expected in ["list_connections", "open_session", "execute_query"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    let listed = within(
        "list_connections",
        client.peer().call_tool(params("list_connections", json!({}))),
    )
    .await
    .unwrap();
    assert_eq!(code(&listed), "ok", "{}", text(&listed));
    assert!(text(&listed).contains(&ids[0]), "{}", text(&listed));
    let opened = client
        .peer()
        .call_tool(params("open_session", json!({"connection_id": ids[0]})))
        .await
        .unwrap();
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    let read = client
        .peer()
        .call_tool(params(
            "execute_query",
            json!({"connection_id": ids[0], "statement": "SELECT 1"}),
        ))
        .await
        .unwrap();
    assert_eq!(code(&read), "ok", "{}", text(&read));
    assert_eq!(runs(&sessions[0]), ["SELECT 1"]);
    // A call the layer does not know is a protocol error, as over stdio.
    assert!(client
        .peer()
        .call_tool(params("no_such_tool", json!({})))
        .await
        .is_err());
    client.cancel().await.unwrap();

    let entries = http.fixture.audit_entries();
    let queried = entries
        .iter()
        .find(|entry| entry.tool == "execute_query")
        .expect("audited");
    assert_eq!(queried.transport.as_deref(), Some("http"));
    assert_eq!(queried.client.as_deref(), Some("test-client"));
    for entry in &entries {
        assert_eq!(entry.transport.as_deref(), Some("http"), "{entry:?}");
        assert_eq!(entry.client.as_deref(), Some("test-client"), "{entry:?}");
        assert_eq!(entry.front_end, "mcp");
    }
    http.stop().await;
}

#[tokio::test]
async fn two_sessions_are_asked_separately_and_the_answers_do_not_cross() {
    let (fixture, sessions, ids) = fakes(2);
    let http = Http::start(fixture).await;
    let (asker_a, mut desk_a) = Asker::manual(true);
    let (asker_b, mut desk_b) = Asker::manual(true);
    let client_a = http.client(asker_a).await;
    let client_b = http.client(asker_b).await;
    for (client, id) in [(&client_a, &ids[0]), (&client_b, &ids[1])] {
        let opened = client
            .peer()
            .call_tool(params("open_session", json!({"connection_id": id})))
            .await
            .unwrap();
        assert_eq!(code(&opened), "ok", "{}", text(&opened));
    }
    let statement_a = "INSERT INTO items(label) VALUES ('from-a')";
    let statement_b = "INSERT INTO items(label) VALUES ('from-b')";
    let call = |client: &rmcp::service::RunningService<rmcp::RoleClient, Asker>, id: &str, statement: &str| {
        let peer = client.peer().clone();
        let arguments = json!({"connection_id": id, "statement": statement});
        tokio::spawn(async move { peer.call_tool(params("execute_query", arguments)).await })
    };
    let call_a = call(&client_a, &ids[0], statement_a);
    let call_b = call(&client_b, &ids[1], statement_b);
    // Each question arrives at the client of its own session, and only there.
    let question_a = within("the question of A", desk_a.recv()).await.unwrap();
    let question_b = within("the question of B", desk_b.recv()).await.unwrap();
    assert!(question_a.message.contains("from-a") && !question_a.message.contains("from-b"));
    assert!(question_b.message.contains("from-b") && !question_b.message.contains("from-a"));
    assert!(question_a.message.contains("Warehouse fake-0"));
    assert!(question_b.message.contains("Warehouse fake-1"));
    // B says yes first, A says no afterwards.
    question_b.accept(true);
    let result_b = within("the answer to B's call", call_b).await.unwrap().unwrap();
    assert_eq!(code(&result_b), "ok", "{}", text(&result_b));
    assert!(runs(&sessions[0]).is_empty(), "A is still waiting");
    question_a.accept(false);
    let result_a = within("the answer to A's call", call_a).await.unwrap().unwrap();
    assert_eq!(code(&result_a), "agent.write_refused", "{}", text(&result_a));
    assert!(runs(&sessions[0]).is_empty(), "A said no");
    assert_eq!(runs(&sessions[1]), [statement_b.to_owned()]);
    // Nobody was asked twice and nobody was asked about the other's statement.
    assert!(desk_a.try_recv().is_err() && desk_b.try_recv().is_err());
    client_a.cancel().await.unwrap();
    client_b.cancel().await.unwrap();
    http.stop().await;
}

/// One process serves many HTTP clients, so a session or transaction id one of them holds must be no
/// use to another: the layer binds it to the owner `McpServer::for_session` gave that client.
#[tokio::test]
async fn a_pinned_session_or_transaction_of_one_http_session_cannot_be_used_by_another() {
    let (fixture, sessions, ids) = fakes_with(1, true);
    let http = Http::start(fixture).await;
    let client_a = http.client(Asker::answering(true, Ok(accepted(true)))).await;
    let client_b = http.client(Asker::answering(true, Ok(accepted(true)))).await;
    let id = &ids[0];
    let call = |client: &rmcp::service::RunningService<rmcp::RoleClient, Asker>, tool: &str, arguments: serde_json::Value| {
        let peer = client.peer().clone();
        let params = params(tool, arguments);
        async move { within("a tool call", peer.call_tool(params)).await.unwrap() }
    };
    let opened = call(&client_a, "open_session", json!({"connection_id": id})).await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    let pinned = call(&client_a, "open_session", json!({"connection_id": id, "pinned": true})).await;
    assert_eq!(code(&pinned), "ok", "{}", text(&pinned));
    let session = structured(&pinned)["session_id"].as_str().unwrap().to_owned();
    assert!(session.starts_with("ses-"), "{session}");
    let begun = call(&client_a, "begin_transaction", json!({"connection_id": id})).await;
    assert_eq!(code(&begun), "ok", "{}", text(&begun));
    let transaction = structured(&begun)["transaction_id"].as_str().unwrap().to_owned();
    assert!(transaction.starts_with("tx-"), "{transaction}");
    let before = runs(&sessions[0]).len();

    // B names A's ids: for B they do not exist, in the words of an id that never did.
    let attempts = [
        ("execute_query", json!({"connection_id": id, "session_id": session, "statement": "SELECT 'b'"}), "agent.not_connected", "unknown session"),
        ("close_session", json!({"session_id": session}), "agent.not_connected", "unknown session"),
        ("execute_query", json!({"connection_id": id, "transaction_id": transaction, "statement": "SELECT 'b'"}), "agent.transaction_gone", "no open transaction"),
        ("commit_transaction", json!({"transaction_id": transaction}), "agent.transaction_gone", "no open transaction"),
    ];
    for (tool, arguments, expected, words) in attempts {
        let refused = call(&client_b, tool, arguments).await;
        assert_eq!(code(&refused), expected, "{tool}: {}", text(&refused));
        assert!(text(&refused).contains(words), "{tool}: {}", text(&refused));
    }
    // Nothing of B reached the connection, and A still has both.
    assert_eq!(runs(&sessions[0]).len(), before);
    let used = call(&client_a, "execute_query", json!({"connection_id": id, "session_id": session, "statement": "SELECT 'a'"})).await;
    assert_eq!(code(&used), "ok", "{}", text(&used));
    let in_transaction = call(&client_a, "execute_query", json!({"connection_id": id, "transaction_id": transaction, "statement": "SELECT 'a'"})).await;
    assert_eq!(code(&in_transaction), "ok", "{}", text(&in_transaction));
    // B's own pinned session is its own, and the cap of four is per client.
    let pinned_b = call(&client_b, "open_session", json!({"connection_id": id, "pinned": true})).await;
    assert_eq!(code(&pinned_b), "ok", "{}", text(&pinned_b));
    assert_ne!(structured(&pinned_b)["session_id"], json!(session));
    for (client, arguments) in [
        (&client_a, json!({"session_id": session})),
        (&client_b, json!({"session_id": structured(&pinned_b)["session_id"]})),
    ] {
        let closed = call(client, "close_session", arguments).await;
        assert_eq!(code(&closed), "ok", "{}", text(&closed));
    }
    let rolled = call(&client_a, "rollback_transaction", json!({"transaction_id": transaction})).await;
    assert_eq!(code(&rolled), "ok", "{}", text(&rolled));
    client_a.cancel().await.unwrap();
    client_b.cancel().await.unwrap();
    http.stop().await;
}

/// A request of the newest protocol revision is served alone, by a handler that never saw an
/// `initialize`: the `ses-` and `tx-` ids it got would name nothing for any later request and would
/// hold a connection until their idle timeout, so they are refused, with the reason.
#[tokio::test]
async fn a_request_outside_a_session_cannot_open_a_transaction_or_a_pinned_session() {
    let (fixture, sessions, ids) = fakes_with(1, true);
    let http = Http::start(fixture).await;
    let call = |name: &str, arguments: serde_json::Value| {
        json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {
                "name": name, "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        })
        .to_string()
    };
    let headers = |name: &'static str| [("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/call"), ("Mcp-Name", name)];
    for (name, arguments) in [
        ("begin_transaction", json!({"connection_id": ids[0]})),
        ("open_session", json!({"connection_id": ids[0], "pinned": true})),
    ] {
        let reply = http.post(&headers(name), &call(name, arguments)).await;
        assert_eq!(reply.status, 200, "{name}: {} {}", reply.headers, reply.body);
        assert!(reply.body.contains("not part of an MCP session"), "{name}: {}", reply.body);
        assert!(reply.body.contains("agent.unsupported") && !reply.body.contains("tx-") && !reply.body.contains("ses-"), "{name}: {}", reply.body);
    }
    // Nothing was opened on the database, and an ordinary call of such a client still works.
    assert_eq!(sessions[0].disconnects.load(std::sync::atomic::Ordering::SeqCst), 0);
    let plain = http.post(&headers("open_session"), &call("open_session", json!({"connection_id": ids[0]}))).await;
    assert!(plain.body.contains("already_open") || plain.body.contains("connection_id"), "{}", plain.body);
    assert!(!plain.body.contains("not part of an MCP session"), "{}", plain.body);
    http.stop().await;
}

/// What a session opened goes with the session: its transactions are rolled back and its pinned
/// sessions closed when the session is closed by its client or expires, and not before.
#[tokio::test]
async fn a_session_that_ends_takes_its_pinned_sessions_and_transactions_with_it() {
    let (fixture, sessions, ids) = fakes_with(1, true);
    let http = Http::start_with(fixture, |config| config.idle_timeout = Duration::from_millis(800)).await;
    let disconnects = || sessions[0].disconnects.load(std::sync::atomic::Ordering::SeqCst);
    let client = http.client(Asker::answering(true, Ok(accepted(true)))).await;
    let call = |tool: &str, arguments: serde_json::Value| {
        let peer = client.peer().clone();
        let params = params(tool, arguments);
        async move { within("a tool call", peer.call_tool(params)).await.unwrap() }
    };
    for arguments in [json!({"connection_id": ids[0]}), json!({"connection_id": ids[0], "pinned": true})] {
        let opened = call("open_session", arguments).await;
        assert_eq!(code(&opened), "ok", "{}", text(&opened));
    }
    let begun = call("begin_transaction", json!({"connection_id": ids[0]})).await;
    assert_eq!(code(&begun), "ok", "{}", text(&begun));
    assert_eq!(disconnects(), 0);
    // Nobody speaks on the session: it expires, and the pinned session and the transaction go.
    poll("the pinned session and the transaction of the expired session to be closed", async || {
        (disconnects() >= 2).then_some(())
    })
    .await;
    let audit = http.fixture.audit_entries();
    assert!(audit.iter().any(|entry| entry.statement.as_deref() == Some("ROLLBACK (client ended)")), "{audit:?}");
    assert!(
        audit.iter().any(|entry| entry.statement.as_deref() == Some("close pinned session (client ended)")),
        "{audit:?}"
    );
    drop(client);
    http.stop().await;
}

#[tokio::test]
async fn an_unknown_session_is_a_404_and_a_session_id_is_no_substitute_for_the_token() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start(fixture).await;
    let opened = http.post(&[], &initialize_body()).await;
    let id = opened.header("mcp-session-id").unwrap().to_owned();
    // An unknown id is a 404, not a new session.
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string();
    let reply = http
        .post(&[("Mcp-Session-Id", "no-such-session"), ("MCP-Protocol-Version", "2025-06-18")], &list)
        .await;
    assert_eq!(reply.status, 404);
    // The id is only good with the token: without it, a 401, whatever the id.
    let reply = send(
        http.addr,
        "POST",
        "/mcp",
        &[
            ("Mcp-Session-Id", id),
            ("Content-Type", "application/json".to_owned()),
            ("Accept", "application/json, text/event-stream".to_owned()),
        ],
        &list,
    )
    .await;
    assert_eq!(reply.status, 401);
    http.stop().await;
}

// ---- the stdio transport records itself too ----------------------------------------------------------

#[tokio::test]
async fn the_stdio_audit_lines_name_the_transport_and_client_as_well() {
    let (fixture, sessions, ids) = fakes(1);
    sessions[0].set_behavior(Behavior::Numbers { count: 1 });
    let harness = Harness::start_with(fixture, Asker::answering(false, Ok(accepted(false)))).await;
    harness
        .call("open_session", json!({"connection_id": ids[0]}))
        .await;
    for entry in harness.fixture.audit_entries() {
        assert_eq!(entry.transport.as_deref(), Some("stdio"), "{entry:?}");
        assert_eq!(entry.client.as_deref(), Some("test-client"), "{entry:?}");
    }
    harness.client.cancel().await.unwrap();
    harness.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn stopping_closes_the_sessions_and_the_listener() {
    let (fixture, _, _) = fakes(1);
    let http = Http::start(fixture).await;
    let addr = http.addr;
    let client = http.client(Asker::answering(false, Ok(accepted(false)))).await;
    http.stop().await;
    assert!(TcpStream::connect(addr).await.is_err(), "the listener is gone");
    drop(client);
}

// ---- the binary --------------------------------------------------------------------------------------

mod binary {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    const EXE: &str = env!("CARGO_BIN_EXE_tablex-mcp");

    fn command(data: &std::path::Path) -> Command {
        let mut command = Command::new(EXE);
        command
            .env("TABLEX_DATA_DIR", data)
            .env("TABLEX_DRIVER_DATA_DIR", shared().driver_dir())
            .env_remove("TABLEX_MCP_TOKEN")
            .env_remove("TABLEX_MCP_ALLOWED_HOSTS")
            .env_remove("TABLEX_MCP_ALLOWED_ORIGINS");
        command
    }

    /// A running `--http` binary and the lines it wrote to stderr so far.
    struct Running {
        child: Child,
        lines: mpsc::Receiver<String>,
        seen: Vec<String>,
    }

    impl Running {
        fn start(mut command: Command, args: &[&str]) -> Self {
            let mut child = command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start tablex-mcp");
            let stderr = child.stderr.take().unwrap();
            let (sender, lines) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            });
            Self {
                child,
                lines,
                seen: Vec::new(),
            }
        }

        /// The first stderr line that contains `needle`.
        fn wait_for(&mut self, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(line) = self.seen.iter().find(|line| line.contains(needle)) {
                    return line.clone();
                }
                let left = deadline.saturating_duration_since(Instant::now());
                match self.lines.recv_timeout(left) {
                    Ok(line) => self.seen.push(line),
                    Err(_) => panic!("no stderr line with {needle:?}; saw {:#?}", self.seen),
                }
            }
        }

        fn address(&mut self) -> SocketAddr {
            let line = self.wait_for("serving MCP over HTTP at http://");
            let rest = line.split("http://").nth(1).unwrap();
            rest.trim_end_matches("/mcp").parse().expect("an address")
        }

        fn all_stderr(&mut self) -> String {
            while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(200)) {
                self.seen.push(line);
            }
            self.seen.join("\n")
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn a_non_loopback_address_needs_allow_remote_and_is_warned_about() {
        let data = tempfile::tempdir().unwrap();
        let refused = command(data.path())
            .args(["--http", "0.0.0.0:0"])
            .env("TABLEX_MCP_TOKEN", TOKEN)
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains("--allow-remote"), "{stderr}");
        assert!(refused.stdout.is_empty());
        // `::` (all IPv6 interfaces) and a LAN address are not loopback either.
        for addr in ["[::]:0", "192.168.1.5:7717"] {
            let refused = command(data.path())
                .args(["--http", addr])
                .env("TABLEX_MCP_TOKEN", TOKEN)
                .output()
                .unwrap();
            assert_eq!(refused.status.code(), Some(2), "{addr}");
        }

        let mut running = Running::start(
            {
                let mut c = command(data.path());
                c.env("TABLEX_MCP_TOKEN", TOKEN);
                c
            },
            &["--http", "0.0.0.0:0", "--allow-remote"],
        );
        let warning = running.wait_for("WARNING");
        assert!(warning.contains("plain HTTP"), "{warning}");
        let addr = running.address();
        assert!(!addr.ip().is_loopback());
        assert!(!running.all_stderr().contains(TOKEN));
    }

    #[tokio::test]
    async fn the_default_is_loopback_and_the_token_is_never_logged() {
        let data = tempfile::tempdir().unwrap();
        let mut running = Running::start(command(data.path()), &["--http", "127.0.0.1:0"]);
        let addr = running.address();
        assert!(addr.ip().is_loopback());
        let line = running.wait_for("bearer token");
        // The token was made in the data directory; the log says where, not what.
        let file = data.path().join("tablex").join("mcp-http-token");
        let token = std::fs::read_to_string(&file).expect("the token file").trim().to_owned();
        assert_eq!(token.len(), 64, "{token}");
        assert!(line.contains(file.to_str().unwrap()) && !line.contains(&token), "{line}");
        assert!(!running.all_stderr().contains(&token));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // It serves: refused without the token, answered with it.
        let init = initialize_body();
        let headers = |token: Option<&str>| {
            let mut headers = vec![
                ("Accept", "application/json, text/event-stream".to_owned()),
                ("Content-Type", "application/json".to_owned()),
            ];
            if let Some(token) = token {
                headers.push(("Authorization", format!("Bearer {token}")));
            }
            headers
        };
        assert_eq!(send(addr, "POST", "/mcp", &headers(None), &init).await.status, 401);
        assert_eq!(send(addr, "POST", "/mcp", &headers(Some(&token)), &init).await.status, 200);
        // `--print-token` shows it on stdout, on request, and it is the same one each time.
        for _ in 0..2 {
            let printed = command(data.path()).arg("--print-token").output().unwrap();
            assert_eq!(printed.status.code(), Some(0));
            assert_eq!(String::from_utf8_lossy(&printed.stdout), format!("{token}\n"));
            assert!(printed.stderr.is_empty());
        }
    }

    #[test]
    fn the_environment_token_wins_and_a_weak_one_is_refused_at_start() {
        let data = tempfile::tempdir().unwrap();
        let printed = command(data.path())
            .arg("--print-token")
            .env("TABLEX_MCP_TOKEN", TOKEN)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&printed.stdout), format!("{TOKEN}\n"));
        assert!(!data.path().join("tablex").join("mcp-http-token").exists());
        let weak = command(data.path())
            .args(["--http", "127.0.0.1:0"])
            .env("TABLEX_MCP_TOKEN", "short")
            .output()
            .unwrap();
        assert_eq!(weak.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&weak.stderr);
        assert!(stderr.contains("not usable") && !stderr.contains("short"), "{stderr}");
    }
}
