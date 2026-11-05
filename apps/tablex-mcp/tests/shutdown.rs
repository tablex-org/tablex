//! Teardown is bounded and interruptible; an oversized message ends the connection.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_agent::testing::{Behavior, FakeSession};
use engine_agent::{AgentError, ConnectionKind, Session};
use rmcp::model::{ClientRequest, Request};
use rmcp::service::{PeerRequestOptions, RequestHandle};
use rmcp::ServiceExt;
use serde_json::json;
use support::*;
use tablex_mcp::server::McpServer;
use tablex_mcp::transport::{ServeError, MAX_FRAME_BYTES};
use tokio::io::AsyncWriteExt;

/// A fixture with one connection whose session hangs in `disconnect` until `release` opens;
/// `entered` hears when the disconnect started.
fn hanging_fixture() -> (Fixture, Arc<Gate>, mpsc::Receiver<()>) {
    let release = Arc::new(Gate::default());
    let (entered_tx, entered) = mpsc::channel();
    let ids = fake_ids(1);
    let fixture = fake_fixture(&ids, |_| {
        Arc::new(HangingDisconnect {
            inner: FakeSession::new(ConnectionKind::Sqlite).arc(),
            entered: Mutex::new(entered_tx.clone()),
            release: Arc::clone(&release),
        }) as Arc<dyn Session>
    });
    (fixture, release, entered)
}

async fn opened_hanging_session(
    peer: &rmcp::Peer<rmcp::RoleClient>,
) -> rmcp::model::CallToolResult {
    peer.call_tool(params("open_session", json!({"connection_id": "fake-0"})))
        .await
        .expect("open_session")
}

#[tokio::test]
async fn a_hung_disconnect_cannot_hold_the_server_past_the_deadline() {
    let (fixture, release, _entered) = hanging_fixture();
    let deadline = Duration::from_millis(400);
    let server = McpServer::new(Arc::clone(&fixture.layer))
        .unwrap()
        .with_shutdown_deadline(deadline);
    let (client_end, reader, writer) = pipe();
    let serve = server.serve(reader, writer, never);
    let scenario = async {
        let client = ().serve(client_end).await.unwrap();
        assert_eq!(code(&opened_hanging_session(client.peer()).await), "ok");
        // The client leaves; the disconnect of its session hangs.
        client.cancel().await.unwrap();
        Instant::now()
    };
    let (result, left_at) = tokio::join!(serve, scenario);
    let waited = left_at.elapsed();
    assert!(
        matches!(result, Err(ServeError::TeardownIncomplete)),
        "{result:?}"
    );
    assert!(waited >= deadline, "it waited for the deadline: {waited:?}");
    assert!(
        waited < Duration::from_secs(10),
        "and not longer: {waited:?}"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("may not have been closed cleanly"),
        "the message says sessions may be left open"
    );
    release.open();
}

#[tokio::test]
async fn a_second_signal_during_a_hung_teardown_ends_it_at_once() {
    let (fixture, release, entered) = hanging_fixture();
    // A deadline far beyond the test: only the second signal can end the wait.
    let server = McpServer::new(Arc::clone(&fixture.layer))
        .unwrap()
        .with_shutdown_deadline(Duration::from_secs(600));
    let (client_end, reader, writer) = pipe();
    let (signal, mut signals) = tokio::sync::mpsc::unbounded_channel::<()>();
    let serve = server.serve(reader, writer, async || {
        signals.recv().await;
    });
    let scenario = async {
        let client = ().serve(client_end).await.unwrap();
        assert_eq!(code(&opened_hanging_session(client.peer()).await), "ok");
        signal.send(()).unwrap(); // SIGTERM: stop serving, start closing sessions
        let entered =
            tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(60)));
        entered
            .await
            .unwrap()
            .expect("the disconnect started and hangs");
        let second = Instant::now();
        signal.send(()).unwrap(); // the impatient second one
        drop(client);
        second
    };
    let (result, second) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(serve, scenario)
    })
    .await
    .expect("the second signal ends the teardown");
    assert!(
        matches!(result, Err(ServeError::TeardownIncomplete)),
        "{result:?}"
    );
    assert!(second.elapsed() < Duration::from_secs(30));
    release.open();
}

#[tokio::test]
async fn a_message_longer_than_the_limit_ends_the_connection_and_closes_sessions() {
    let fixture = Fixture::new();
    let conn = fixture.connection(Some("read"));
    let layer = Arc::clone(&fixture.layer);
    let server = McpServer::new(Arc::clone(&layer)).unwrap();
    let (mut client_end, reader, writer) = pipe();
    // A session is open when the oversized line arrives, to prove it is closed afterwards.
    layer.call("open_session", json!({"connection_id": conn.id}));
    assert!(layer.is_open(&conn.id));
    let serve = tokio::spawn(server.serve(reader, writer, never));
    let flood = tokio::spawn(async move {
        // No newline, ever: one endless "line". The write fails once the server stops reading.
        let chunk = vec![b'a'; 64 * 1024];
        let mut sent = 0usize;
        while sent <= MAX_FRAME_BYTES + 64 * 1024 {
            if client_end.write_all(&chunk).await.is_err() {
                break;
            }
            sent += chunk.len();
        }
        sent
    });
    let result = tokio::time::timeout(Duration::from_secs(60), serve)
        .await
        .expect("the server ends")
        .unwrap();
    assert!(
        matches!(result, Err(ServeError::FrameTooLarge)),
        "{result:?}"
    );
    assert!(!layer.is_open(&conn.id), "sessions are closed");
    // The reader stopped at the limit (plus at most the chunk in flight), not at the end of the flood.
    let sent = flood.await.unwrap();
    assert!(
        sent >= MAX_FRAME_BYTES,
        "the flood reached the limit: {sent}"
    );
}

#[tokio::test]
async fn a_large_message_under_the_limit_is_served() {
    let harness = Harness::start(Fixture::new()).await;
    // 1 MiB of statement: far over the layer's 64 KiB statement limit, far under the frame limit.
    let result = harness
        .call(
            "execute_query",
            json!({"connection_id": "nobody", "statement": "x".repeat(1 << 20)}),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(!code(&result).is_empty());
}

// ---- the driver's cancel never runs on the runtime thread ---------------------------------------

/// Runs `test` on a thread of its own and fails (instead of hanging) when it does not finish in
/// `limit`: a frozen runtime cannot time itself out. `release` is opened first so the threads
/// the test left blocked can end.
fn without_freezing(limit: Duration, release: &Arc<Gate>, test: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(test)));
    });
    match finished.recv_timeout(limit) {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => {
            release.open();
            panic!("the server froze: no progress within {limit:?}");
        }
    }
}

/// Opens a gate when dropped.
struct OpenOnDrop(Arc<Gate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// Runs `test` on the runtime the server really runs on: one thread. `release` is opened before
/// the runtime is dropped (also when the test fails), which waits for the blocking threads.
fn on_one_thread(release: &Arc<Gate>, test: impl std::future::Future<Output = ()>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _release_first = OpenOnDrop(Arc::clone(release));
    runtime.block_on(test);
}

async fn start_call(
    peer: &rmcp::Peer<rmcp::RoleClient>,
    tool: &str,
    arguments: serde_json::Value,
) -> RequestHandle<rmcp::RoleClient> {
    let request = ClientRequest::CallToolRequest(Request::new(params(tool, arguments)));
    peer.send_cancellable_request(request, PeerRequestOptions::no_options())
        .await
        .expect("send")
}

/// A session whose statement behaves like a driver talking to a server that cannot be reached: the
/// cancel hook it registers hangs until `release` opens, and the statement cannot end before its
/// cancel went out. `running` hears when the statement registered its hook, `hook_entered` when
/// the hook started.
fn session_with_a_hanging_cancel(
    release: &Arc<Gate>,
) -> (Arc<FakeSession>, mpsc::Receiver<()>, mpsc::Receiver<()>) {
    let (running_tx, running) = mpsc::channel();
    let (entered_tx, hook_entered) = mpsc::channel();
    let (running_tx, entered_tx) = (Mutex::new(running_tx), Mutex::new(entered_tx));
    let release = Arc::clone(release);
    let session = FakeSession::new(ConnectionKind::Sqlite).arc();
    session.set_behavior(Behavior::Script(Box::new(move |_, cancel| {
        let sent = Arc::new(AtomicBool::new(false));
        let (entered, gate, sent_by_hook) = (
            entered_tx.lock().unwrap().clone(),
            Arc::clone(&release),
            Arc::clone(&sent),
        );
        let _hook = cancel.on_cancel(move || {
            let _ = entered.send(());
            gate.wait();
            sent_by_hook.store(true, Ordering::SeqCst);
        });
        let _ = running_tx.lock().unwrap().send(());
        while !sent.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(AgentError::Cancelled)
    })));
    (session, running, hook_entered)
}

/// Waits for `signal` on a blocking thread (the receiver is not async).
async fn heard(signal: mpsc::Receiver<()>) -> mpsc::Receiver<()> {
    tokio::task::spawn_blocking(move || {
        signal
            .recv_timeout(Duration::from_secs(60))
            .expect("the signal came");
        signal
    })
    .await
    .unwrap()
}

#[test]
fn a_driver_cancel_that_hangs_cannot_freeze_the_protocol_or_the_shutdown() {
    let _ = shared();
    let release = Arc::new(Gate::default());
    let hooks = Arc::clone(&release);
    without_freezing(Duration::from_secs(30), &release, move || {
        let release = hooks;
        let (session, running, hook_entered) = session_with_a_hanging_cancel(&release);
        let ids = fake_ids(1);
        let fixture = fake_fixture(&ids, |_| session.clone() as Arc<dyn Session>);
        let deadline = Duration::from_millis(400);
        let server = McpServer::new(Arc::clone(&fixture.layer))
            .unwrap()
            .with_shutdown_deadline(deadline);
        on_one_thread(&release, async {
            let (client_end, reader, writer) = pipe();
            let (signal, mut signals) = tokio::sync::mpsc::unbounded_channel::<()>();
            let serve = server.serve(reader, writer, async || {
                signals.recv().await;
            });
            let scenario = async {
                let client = ().serve(client_end).await.unwrap();
                let peer = client.peer().clone();
                let opened = peer
                    .call_tool(params("open_session", json!({"connection_id": ids[0]})))
                    .await
                    .unwrap();
                assert_eq!(code(&opened), "ok", "{}", text(&opened));
                let call = start_call(
                    &peer,
                    "execute_query",
                    json!({"connection_id": ids[0], "statement": "SELECT 1"}),
                )
                .await;
                let running = heard(running).await;
                drop(running);

                // The client cancels the request: the driver's cancel starts, and hangs.
                call.cancel(Some("test".into())).await.unwrap();
                let _hook_entered = heard(hook_entered).await;

                // The protocol still answers while the cancel hangs. With the cancel running on the
                // runtime thread this never gets here.
                let listed = peer
                    .call_tool(params("list_connections", json!({})))
                    .await
                    .unwrap();
                assert_eq!(code(&listed), "ok", "{}", text(&listed));

                // A stop: the call cannot return (its cancel is stuck), so the teardown runs into
                // its deadline - on time, not later.
                let stopped = Instant::now();
                signal.send(()).unwrap();
                (client, stopped)
            };
            let (result, (client, stopped)) = tokio::join!(serve, scenario);
            assert!(
                matches!(result, Err(ServeError::TeardownIncomplete)),
                "{result:?}"
            );
            let waited = stopped.elapsed();
            assert!(waited >= deadline, "it waited for the deadline: {waited:?}");
            assert!(
                waited < Duration::from_secs(10),
                "and not longer: {waited:?}"
            );
            drop(client);
        });
    });
}

#[test]
fn a_second_signal_ends_a_teardown_held_by_a_hanging_driver_cancel() {
    let _ = shared();
    let release = Arc::new(Gate::default());
    let hooks = Arc::clone(&release);
    without_freezing(Duration::from_secs(30), &release, move || {
        let release = hooks;
        let (session, running, hook_entered) = session_with_a_hanging_cancel(&release);
        let ids = fake_ids(1);
        let fixture = fake_fixture(&ids, |_| session.clone() as Arc<dyn Session>);
        // A deadline far beyond the test: only the second signal can end the wait.
        let server = McpServer::new(Arc::clone(&fixture.layer))
            .unwrap()
            .with_shutdown_deadline(Duration::from_secs(600));
        on_one_thread(&release, async {
            let (client_end, reader, writer) = pipe();
            let (signal, mut signals) = tokio::sync::mpsc::unbounded_channel::<()>();
            let serve = server.serve(reader, writer, async || {
                signals.recv().await;
            });
            let scenario = async {
                let client = ().serve(client_end).await.unwrap();
                let peer = client.peer().clone();
                let opened = peer
                    .call_tool(params("open_session", json!({"connection_id": ids[0]})))
                    .await
                    .unwrap();
                assert_eq!(code(&opened), "ok", "{}", text(&opened));
                let call = start_call(
                    &peer,
                    "execute_query",
                    json!({"connection_id": ids[0], "statement": "SELECT 1"}),
                )
                .await;
                drop(heard(running).await);
                call.cancel(Some("test".into())).await.unwrap();
                drop(heard(hook_entered).await);

                signal.send(()).unwrap(); // stop: the teardown starts and cannot finish
                while session.disconnects.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let second = Instant::now();
                signal.send(()).unwrap(); // the impatient second one
                (client, second)
            };
            let (result, (client, second)) = tokio::join!(serve, scenario);
            assert!(
                matches!(result, Err(ServeError::TeardownIncomplete)),
                "{result:?}"
            );
            assert!(second.elapsed() < Duration::from_secs(10));
            drop(client);
        });
    });
}

// ---- a cancelled call's audit line is there when the server returns -----------------------------

#[tokio::test]
async fn a_stop_waits_for_the_cancelled_call_so_its_audit_line_is_written_before_serve_returns() {
    // The statement ends 300 ms after it saw the cancel: the shutdown must wait for it, or the
    // process would exit between the cancel and the line.
    const SLOW_TO_STOP: Duration = Duration::from_millis(300);
    let (started_tx, started) = mpsc::channel();
    let started_tx = Mutex::new(started_tx);
    let session = FakeSession::new(ConnectionKind::Sqlite).arc();
    session.set_behavior(Behavior::Script(Box::new(move |_, cancel| {
        let _ = started_tx.lock().unwrap().send(());
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(SLOW_TO_STOP);
        Err(AgentError::Cancelled)
    })));
    let ids = fake_ids(1);
    let fixture = fake_fixture(&ids, |_| session.clone() as Arc<dyn Session>);
    let server = McpServer::new(Arc::clone(&fixture.layer)).unwrap();
    let (client_end, reader, writer) = pipe();
    let (signal, mut signals) = tokio::sync::mpsc::unbounded_channel::<()>();
    let serve = server.serve(reader, writer, async || {
        signals.recv().await;
    });
    let scenario = async {
        let client = ().serve(client_end).await.unwrap();
        let peer = client.peer().clone();
        let opened = peer
            .call_tool(params("open_session", json!({"connection_id": ids[0]})))
            .await
            .unwrap();
        assert_eq!(code(&opened), "ok", "{}", text(&opened));
        let _call = start_call(
            &peer,
            "execute_query",
            json!({"connection_id": ids[0], "statement": "SELECT 1"}),
        )
        .await;
        drop(heard(started).await);
        signal.send(()).unwrap();
        client
    };
    let (result, client) = tokio::join!(serve, scenario);
    result.expect("a clean shutdown: the call came back inside the deadline");
    // No polling: the line is in the log the moment `serve` has returned.
    let cancelled = fixture
        .audit_entries()
        .into_iter()
        .filter(|entry| entry.tool == "execute_query" && entry.outcome == "agent.cancelled")
        .count();
    assert_eq!(cancelled, 1, "the cancelled call's audit line");
    drop(client);
}

// ---- a signal after end of input does not cut the teardown short --------------------------------

#[tokio::test]
async fn the_first_signal_after_the_client_closed_its_input_does_not_end_the_teardown() {
    let (fixture, release, entered) = hanging_fixture();
    // Also when an assertion fails: the runtime waits for the blocked disconnect.
    let _release_on_exit = OpenOnDrop(Arc::clone(&release));
    // A deadline far beyond the test: only a signal can end the wait.
    let server = McpServer::new(Arc::clone(&fixture.layer))
        .unwrap()
        .with_shutdown_deadline(Duration::from_secs(600));
    let (client_end, reader, writer) = pipe();
    let (signal, mut signals) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut serve = tokio::spawn(server.serve(reader, writer, async move || {
        signals.recv().await;
    }));
    let client = ().serve(client_end).await.unwrap();
    assert_eq!(code(&opened_hanging_session(client.peer()).await), "ok");
    client.cancel().await.unwrap(); // end of input: the teardown starts, its disconnect hangs
    drop(heard(entered).await);
    assert!(
        !serve.is_finished(),
        "no signal yet, and the teardown is hanging"
    );

    // The first signal does not cut the teardown short: it was the end of input that started it.
    // (A negative check, so it can only miss a regression, never fail on a slow machine.)
    signal.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut serve)
            .await
            .is_err(),
        "the first signal after end of input must not end the teardown"
    );
    // The next one does, at once.
    signal.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(30), &mut serve)
        .await
        .expect("the second signal ends the teardown")
        .unwrap();
    assert!(
        matches!(result, Err(ServeError::TeardownIncomplete)),
        "{result:?}"
    );
    release.open();
}
