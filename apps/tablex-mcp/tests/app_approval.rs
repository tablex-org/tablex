//! A client that cannot ask through MCP elicitation gets its question put to the user in the
//! tablex app. The real server and tool layer over the real SQLite driver, an rmcp client without
//! the elicitation capability, and a fake app on a temporary socket: either the real `Listener`
//! (which authenticates and signs, as the app does) or a hand-written peer that answers wrongly.
//! Nothing here touches a running tablex, the user's data dir or Keychain.

// The app channel has no Windows implementation yet: there is no app to fake there.
#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use engine_connect::app_channel::{
    answer_proof, ask_mac, AppClient, ApprovalAnswer, ApprovalAsk, Listener, Reply, Request,
};
use serde_json::{json, Value};
use support::*;
use tablex_mcp::approver::{
    no_way_to_ask, refusal, switch_off, OUTSIDE_TABLEX, REASON_NOT_DELIVERED, REASON_PRODUCTION,
};

const INSERT: &str = "INSERT INTO items(label) VALUES ('written')";
const COUNT: &str = "SELECT count(*) AS n FROM items WHERE label = 'written'";

/// What the fake app does with a question.
#[derive(Clone, Copy)]
enum Mode {
    Answer(bool),
    /// Waits until the test decides (or the question is withdrawn).
    Hold,
}

#[derive(Default)]
struct Gate {
    state: Mutex<(Option<bool>, bool)>,
    changed: Condvar,
}

impl Gate {
    fn decide(&self, approve: bool) {
        self.state.lock().unwrap().0 = Some(approve);
        self.changed.notify_all();
    }

    fn withdrawn(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }

    fn wait(&self) -> Reply {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(approve) = state.0 {
                return Reply::approval(approve);
            }
            if state.1 {
                return Reply::error("withdrawn");
            }
            state = self.changed.wait(state).unwrap();
        }
    }
}

/// A fake tablex over the real listener: records the questions and what is withdrawn.
struct App {
    _dir: tempfile::TempDir,
    path: PathBuf,
    asks: Arc<Mutex<Vec<ApprovalAsk>>>,
    withdrawn: Arc<Mutex<Vec<String>>>,
    gate: Arc<Gate>,
    _listener: Listener,
}

impl App {
    fn listening(mode: Mode) -> Self {
        let dir = private_dir();
        let path = dir.path().join("app.sock");
        let asks = Arc::new(Mutex::new(Vec::new()));
        let withdrawn = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Gate::default());
        let (log, gone, held) = (Arc::clone(&asks), Arc::clone(&withdrawn), Arc::clone(&gate));
        let listener = Listener::bind(
            &path,
            Arc::new(move |request| match request {
                Request::Approval { ask, nonce, mac } => {
                    assert!(nonce.is_empty() && mac.is_empty(), "the handler never sees the signature");
                    log.lock().unwrap().push(ask);
                    match mode {
                        Mode::Answer(approve) => Reply::approval(approve),
                        Mode::Hold => held.wait(),
                    }
                }
                Request::Withdraw { id } => {
                    gone.lock().unwrap().push(id);
                    held.withdrawn();
                    Reply::ok()
                }
                _ => Reply::ok(),
            }),
        )
        .unwrap();
        Self { _dir: dir, path, asks, withdrawn, gate, _listener: listener }
    }

    fn client(&self) -> AppClient {
        AppClient::new(self.path.clone())
    }

    fn asked(&self) -> Vec<ApprovalAsk> {
        self.asks.lock().unwrap().clone()
    }
}

/// A hand-written app: a socket and token file like the real one, and a `behaviour` for the
/// connection that carries a question (given the stream, the question and the token).
struct Raw {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Raw {
    fn start(behaviour: impl Fn(&mut UnixStream, ApprovalAsk, &Signer) + Send + 'static) -> Self {
        Self::start_with_token(&"ab".repeat(32), behaviour)
    }

    /// The same, with `token` in the token file (the app's launch key).
    fn start_with_token(token: &str, behaviour: impl Fn(&mut UnixStream, ApprovalAsk, &Signer) + Send + 'static) -> Self {
        let dir = private_dir();
        let path = dir.path().join("app.sock");
        let token = token.to_owned();
        let token_path = dir.path().join("app.token");
        std::fs::write(&token_path, &token).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
                while let Some(Ok(line)) = lines.next() {
                    match serde_json::from_str::<Request>(&line) {
                        Ok(Request::Ping { .. }) => {
                            // The launch id of the token: what the real app says.
                            let _ = stream.write_all(format!("{{\"ok\":true,\"protocol\":1,\"launch\":\"{}\"}}\n", launch_of(&token)).as_bytes());
                        }
                        Ok(Request::Approval { ask, nonce, mac }) => {
                            // The client proves it has the token without sending it.
                            assert_eq!(mac, ask_mac(&token, &ask, &nonce), "the question is signed with the token of the file");
                            assert!(!line.contains(&token), "the token never travels");
                            let signer = Signer { token: token.clone(), nonce };
                            behaviour(&mut stream, ask, &signer);
                            break;
                        }
                        _ => {
                            let _ = stream.write_all(b"{\"ok\":true}\n");
                        }
                    }
                }
            }
        });
        Self { _dir: dir, path }
    }

    fn client(&self) -> AppClient {
        AppClient::new(self.path.clone())
    }
}

/// What a hand-written app signs its answers with.
struct Signer {
    token: String,
    nonce: String,
}

impl Signer {
    fn proof(&self, ask: &ApprovalAsk, approve: bool) -> String {
        answer_proof(&self.token, ask, &self.nonce, approve)
    }
}

/// The launch id the real app puts in its answer to a ping (a hash of the token).
fn launch_of(token: &str) -> String {
    engine_connect::app_channel::launch_id(token)
}

fn answer_line(id: &str, approve: bool, proof: String) -> Vec<u8> {
    let mut line = serde_json::to_vec(&Reply {
        answer: Some(ApprovalAnswer { id: id.to_owned(), approve, proof }),
        ..Reply::ok()
    })
    .unwrap();
    line.push(b'\n');
    line
}

fn error_message(result: &rmcp::model::CallToolResult) -> String {
    let body: Value = serde_json::from_str(&text(result)).unwrap();
    body["error"]["message"].as_str().unwrap().to_owned()
}

/// A server whose client cannot ask (or can, with `capability`), the app's switch `switch`, and an
/// open session on a connection of type `connection_type`.
struct Rig {
    harness: Harness<Asker>,
    seen: Arc<Seen>,
    connection: String,
}

async fn rig(app: Option<AppClient>, capability: bool, switch: bool, connection_type: &str) -> Rig {
    let fixture = Fixture::with_notify(app);
    fixture.set_mcp_writes(switch);
    let conn = fixture.typed_connection(Some("read_write"), connection_type);
    let asker = Asker::answering(capability, Ok(accepted(true)));
    let seen = Arc::clone(&asker.seen);
    let harness = Harness::start_with(fixture, asker).await;
    let opened = harness.call("open_session", json!({"connection_id": conn.id})).await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    Rig { harness, seen, connection: conn.id }
}

impl Rig {
    fn insert_args(&self) -> Value {
        json!({"connection_id": self.connection, "statement": INSERT})
    }

    async fn insert(&self) -> rmcp::model::CallToolResult {
        self.harness.call("execute_query", self.insert_args()).await
    }

    async fn written(&self) -> i64 {
        let counted = self
            .harness
            .call("execute_query", json!({"connection_id": self.connection, "statement": COUNT}))
            .await;
        let rows = &structured(&counted)["rows"][0][0];
        rows.as_str().and_then(|n| n.parse().ok()).or_else(|| rows.as_i64()).expect("a count")
    }
}

#[tokio::test]
async fn the_app_is_asked_when_the_client_cannot_and_an_approval_runs_the_write() {
    let app = App::listening(Mode::Answer(true));
    let rig = rig(Some(app.client()), false, true, "").await;
    let result = rig.insert().await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    assert_eq!(rig.written().await, 1);
    assert!(rig.seen.asked().is_empty(), "nothing went through MCP elicitation");
    let asked = app.asked();
    assert_eq!(asked.len(), 1);
    let ask = &asked[0];
    // The same facts the elicitation shows, as fields.
    assert_eq!(ask.title, "Run this statement");
    assert_eq!(ask.statements, [INSERT]);
    assert_eq!(ask.client.as_deref(), Some("test-client"));
    assert!(ask.connection.as_deref().is_some_and(|name| !name.is_empty()));
    assert_eq!(ask.danger, None);
    assert_eq!(ask.id.len(), 32, "a random id of 128 bits");
    let entries = rig.harness.fixture.audit_entries();
    let entry = entries.iter().find(|entry| entry.statement.as_deref() == Some(INSERT)).unwrap();
    assert_eq!(entry.approval.as_deref(), Some("approved"));
}

/// The same over Streamable HTTP: a session of a client that cannot ask goes to the app, not to the
/// refusal, because the approver reaches the client of the call and the app channel by the same
/// paths as on stdio.
#[cfg(feature = "http")]
#[tokio::test]
async fn a_client_over_http_that_cannot_ask_is_asked_in_the_app_too() {
    use rmcp::transport::streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientTransportConfig};
    use rmcp::ServiceExt;
    use tablex_mcp::http::HttpConfig;
    use tablex_mcp::server::McpServer;
    use tablex_mcp::token::Token;

    const TOKEN: &str = "test-token-0123456789abcdef";
    let app = App::listening(Mode::Answer(true));
    let fixture = Fixture::with_notify(Some(app.client()));
    fixture.set_mcp_writes(true);
    let conn = fixture.typed_connection(Some("read_write"), "");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = McpServer::new(Arc::clone(&fixture.layer)).unwrap();
    let stop = Arc::new(tokio::sync::Notify::new());
    let stopped = Arc::clone(&stop);
    let serving = tokio::spawn(server.serve_http(listener, HttpConfig::new(Token::parse(TOKEN).unwrap()), async move || {
        stopped.notified().await;
    }));
    let asker = Asker::answering(false, Ok(accepted(true)));
    let seen = Arc::clone(&asker.seen);
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")).auth_header(TOKEN),
    );
    let client = within("the client handshake", asker.serve(transport)).await.expect("initialize over HTTP");
    let call = |tool: &str, arguments: Value| {
        let peer = client.peer().clone();
        let params = params(tool, arguments);
        async move { within("a tool call", peer.call_tool(params)).await.unwrap() }
    };
    let opened = call("open_session", json!({"connection_id": conn.id})).await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    let result = call("execute_query", json!({"connection_id": conn.id, "statement": INSERT})).await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    assert!(seen.asked().is_empty(), "nothing went through MCP elicitation");
    let asked = app.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].statements, [INSERT]);
    assert_eq!(asked[0].client.as_deref(), Some("test-client"));
    client.cancel().await.unwrap();
    stop.notify_one();
    within("the server to stop", serving).await.unwrap().expect("a clean stop");
}

#[tokio::test]
async fn a_dangerous_statement_shows_its_danger_in_the_app() {
    let app = App::listening(Mode::Answer(false));
    let rig = rig(Some(app.client()), false, true, "").await;
    rig.harness.fixture.set_mcp_writes(true);
    let result = rig
        .harness
        .call(
            "execute_query",
            json!({"connection_id": rig.connection, "statement": "DELETE FROM items", "dangerous": true}),
        )
        .await;
    assert_eq!(code(&result), "agent.write_refused");
    let ask = &app.asked()[0];
    assert!(ask.danger.as_deref().is_some_and(|danger| !danger.is_empty()), "{ask:?}");
}

#[tokio::test]
async fn a_decline_in_the_app_runs_nothing() {
    let app = App::listening(Mode::Answer(false));
    let rig = rig(Some(app.client()), false, true, "").await;
    let result = rig.insert().await;
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(error_message(&result), "the user did not approve this SQL statement");
    assert_eq!(rig.written().await, 0);
    let entries = rig.harness.fixture.audit_entries();
    let entry = entries.iter().find(|entry| entry.statement.as_deref() == Some(INSERT)).unwrap();
    assert_eq!(entry.approval.as_deref(), Some("declined"));
}

#[tokio::test]
async fn an_app_that_closes_the_channel_or_is_not_running_never_approves() {
    // The channel drops while the question is open: not delivered.
    let gone = Raw::start(|_, _, _| {});
    let rig_gone = rig(Some(gone.client()), false, true, "").await;
    let result = rig_gone.insert().await;
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(
        error_message(&result),
        format!("this SQL statement was not run, and the user was not asked: {}", refusal(REASON_NOT_DELIVERED))
    );
    assert_eq!(rig_gone.written().await, 0);
    // No app at all: the refusal says what the user can do.
    let dir = tempfile::tempdir().unwrap();
    let nobody = AppClient::new(dir.path().join("app.sock"));
    let rig_none = rig(Some(nobody), false, true, "").await;
    let result = rig_none.insert().await;
    let message = error_message(&result);
    assert!(message.contains("open the tablex app and try again, or to use a client that supports MCP elicitation"), "{message}");
    assert_eq!(message, format!("this SQL statement was not run, and the user was not asked: {}", refusal(&no_way_to_ask(None))));
    assert_eq!(rig_none.written().await, 0);
}

#[tokio::test]
async fn cancelling_the_call_withdraws_the_question_in_the_app() {
    let app = App::listening(Mode::Hold);
    let rig = rig(Some(app.client()), false, true, "").await;
    let call = rig.harness.start_call("execute_query", rig.insert_args()).await;
    poll("the question in the app", async || (!app.asked().is_empty()).then_some(())).await;
    call.cancel(Some("test".into())).await.unwrap();
    let id = app.asked()[0].id.clone();
    poll("the withdrawal", async || (app.withdrawn.lock().unwrap().contains(&id)).then_some(())).await;
    // A late answer changes nothing.
    app.gate.decide(true);
    assert_eq!(rig.written().await, 0);
    let entry = poll("the cancelled call in the audit log", async || {
        rig.harness
            .fixture
            .audit_entries()
            .into_iter()
            .find(|entry| entry.tool == "execute_query" && entry.outcome == "agent.cancelled")
    })
    .await;
    assert_eq!(entry.approval.as_deref(), Some("cancelled"));
}

#[tokio::test]
async fn an_answer_for_another_id_or_with_a_wrong_proof_is_ignored() {
    // Before the real answer (a decline) come a recorded approval of an earlier question and an
    // approval of this one that the app did not sign: neither counts.
    let app = Raw::start(|stream, ask, signer| {
        // A recorded approval of another question (another id), signed with the right token.
        let mut earlier = ask.clone();
        earlier.id = "0123456789abcdef0123456789abcdef".into();
        let _ = stream.write_all(&answer_line(&earlier.id, true, signer.proof(&earlier, true)));
        // A proof of this id for another statement (the question as shown is what is signed).
        let mut other = ask.clone();
        other.statements = vec!["SELECT 1".into()];
        let _ = stream.write_all(&answer_line(&ask.id, true, signer.proof(&other, true)));
        // The same question under another nonce.
        let elsewhere = Signer { token: signer.token.clone(), nonce: "00".repeat(16) };
        let _ = stream.write_all(&answer_line(&ask.id, true, elsewhere.proof(&ask, true)));
        let _ = stream.write_all(&answer_line(&ask.id, true, "0".repeat(64)));
        let _ = stream.write_all(&answer_line(&ask.id, true, answer_proof("not-the-token", &ask, &signer.nonce, true)));
        // A proof of a decline does not make an approval either.
        let _ = stream.write_all(&answer_line(&ask.id, true, signer.proof(&ask, false)));
        let _ = stream.write_all(&answer_line(&ask.id, false, signer.proof(&ask, false)));
        let _ = stream.flush();
        std::thread::sleep(Duration::from_millis(200));
    });
    let rig = rig(Some(app.client()), false, true, "").await;
    let result = rig.insert().await;
    assert_eq!(code(&result), "agent.write_refused", "{}", text(&result));
    assert_eq!(error_message(&result), "the user did not approve this SQL statement");
    assert_eq!(rig.written().await, 0);
}

#[tokio::test]
async fn an_app_that_only_sends_answers_it_did_not_sign_never_approves() {
    let app = Raw::start(|stream, ask, _| {
        for _ in 0..20 {
            if stream.write_all(&answer_line(&ask.id, true, "f".repeat(64))).is_err() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    });
    let rig = rig(Some(app.client()), false, true, "").await;
    let result = rig.insert().await;
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(rig.written().await, 0);
}

#[tokio::test]
async fn an_answer_of_a_replayed_connection_cannot_approve_a_new_question() {
    // A first question is approved for real; its answer, recorded, is replayed to the next one.
    type Recorded = Arc<Mutex<Option<(String, Vec<u8>)>>>;
    let recorded: Recorded = Arc::default();
    let log = Arc::clone(&recorded);
    let app = Raw::start(move |stream, ask, signer| {
        let mut recorded = log.lock().unwrap();
        match recorded.as_ref() {
            None => {
                let line = answer_line(&ask.id, true, signer.proof(&ask, true));
                *recorded = Some((ask.id.clone(), line.clone()));
                let _ = stream.write_all(&line);
            }
            Some((_, old)) => {
                // The old answer (another id), then a decline for this question.
                let _ = stream.write_all(old);
                let _ = stream.write_all(&answer_line(&ask.id, false, signer.proof(&ask, false)));
            }
        }
        let _ = stream.flush();
        std::thread::sleep(Duration::from_millis(200));
    });
    let rig = rig(Some(app.client()), false, true, "").await;
    assert_eq!(code(&rig.insert().await), "ok");
    let again = rig.insert().await;
    assert_eq!(code(&again), "agent.write_refused", "{}", text(&again));
    assert_eq!(rig.written().await, 1, "only the first question was approved");
}

#[tokio::test]
async fn a_switch_turned_off_while_the_app_asks_stops_the_write() {
    let app = App::listening(Mode::Hold);
    let rig = rig(Some(app.client()), false, true, "").await;
    let call = rig.harness.start_call("execute_query", rig.insert_args()).await;
    poll("the question in the app", async || (!app.asked().is_empty()).then_some(())).await;
    rig.harness.fixture.set_mcp_writes(false);
    app.gate.decide(true);
    let result = unwrap_call(call.await_response().await.unwrap());
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(
        error_message(&result),
        format!("the change was approved, but it can no longer run: {}", refusal(&switch_off(None)))
    );
    assert_eq!(rig.written().await, 0);
}

#[tokio::test]
async fn a_production_connection_is_refused_before_the_app_is_asked() {
    let app = App::listening(Mode::Answer(true));
    let rig = rig(Some(app.client()), false, true, "production").await;
    let result = rig.insert().await;
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(
        error_message(&result),
        format!("this SQL statement was not run, and the user was not asked: {}", refusal(REASON_PRODUCTION))
    );
    assert!(app.asked().is_empty());
    // And with the switch off, the switch is the reason, still without asking.
    let off = rig_with_switch_off(&app).await;
    let result = off.insert().await;
    assert!(error_message(&result).contains("Let MCP clients run changes"), "{}", error_message(&result));
    assert!(app.asked().is_empty());
}

async fn rig_with_switch_off(app: &App) -> Rig {
    rig(Some(app.client()), false, false, "").await
}

#[tokio::test]
async fn elicitation_is_preferred_when_the_client_can_ask() {
    let app = App::listening(Mode::Answer(false));
    let rig = rig(Some(app.client()), true, true, "").await;
    let result = rig.insert().await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    assert_eq!(rig.seen.asked().len(), 1, "the client was asked");
    assert!(app.asked().is_empty(), "the app was not");
}

#[tokio::test]
async fn the_commit_of_a_transaction_is_put_to_the_app_with_every_change() {
    let app = App::listening(Mode::Answer(true));
    let rig = rig(Some(app.client()), false, true, "").await;
    let begun = rig.harness.call("begin_transaction", json!({"connection_id": rig.connection})).await;
    assert_eq!(code(&begun), "ok", "{}", text(&begun));
    let transaction = structured(&begun)["transaction_id"].as_str().unwrap().to_owned();
    let inserted = rig
        .harness
        .call("execute_query", json!({"connection_id": rig.connection, "transaction_id": transaction, "statement": INSERT}))
        .await;
    assert_eq!(code(&inserted), "ok", "{}", text(&inserted));
    let committed = rig.harness.call("commit_transaction", json!({"transaction_id": transaction})).await;
    assert_eq!(code(&committed), "ok", "{}", text(&committed));
    let asked = app.asked();
    assert_eq!(asked.len(), 2, "the statement, then the commit");
    assert_eq!(asked[1].title, "Commit this transaction");
    assert_eq!(asked[1].statements, [INSERT]);
    assert_eq!(rig.written().await, 1);
    assert!(OUTSIDE_TABLEX.starts_with("Do not connect"));
}

#[tokio::test]
async fn a_program_that_only_bound_the_socket_is_never_asked_and_cannot_approve() {
    // It owns a socket where the app's would be, but has no token file: there is nothing to sign
    // with, so the question is not sent to it at all.
    let dir = private_dir();
    let path = dir.path().join("app.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let saw_a_question = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&saw_a_question);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
            while let Some(Ok(line)) = lines.next() {
                if line.contains("approval") {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    // It would approve everything, if it were asked.
                    let _ = stream.write_all(&answer_line("any", true, "0".repeat(64)));
                } else {
                    let _ = stream.write_all(b"{\"ok\":true,\"protocol\":1}\n");
                }
            }
        }
    });
    let rig = rig(Some(AppClient::new(path)), false, true, "").await;
    let result = rig.insert().await;
    assert_eq!(code(&result), "agent.write_refused");
    let message = error_message(&result);
    assert!(message.contains("could not be asked") && message.contains("no approval token"), "{message}");
    assert!(!saw_a_question.load(std::sync::atomic::Ordering::SeqCst), "the question never reached it");
    assert_eq!(rig.written().await, 0);
}
