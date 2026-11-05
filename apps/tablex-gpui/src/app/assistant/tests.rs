//! The assistant service without a panel: a scripted provider and the agent crate's fakes drive
//! real chats on real worker threads; nothing here uses the network (the two end-to-end tests talk
//! to a socket on 127.0.0.1) or the user's credential store, connections or audit log.

use super::approval::Answer;
use super::model::Transcript;
use super::run::{Item, Shared, MIN_INTERVAL};
use super::scripted::{call, failure, say, Scripted, Step};
use super::*;
use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{AgentAccess, AgentError, AuditConfig, AuditLog, ConnectionKind};
use engine_ai::{ChatProvider, Message, ProviderError};
use gpui::TestAppContext;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::time::Instant;

const CONN: &str = "c0ffee00-0000-4000-8000-000000000001";
const KEY: &str = "sk-secret-key-123456";

// ---- the rig -----------------------------------------------------------------------------------

struct Rig {
    layer: Arc<ToolLayer>,
    session: Arc<FakeSession>,
    sessions: Arc<FakeProvider>,
    /// The audit log of a rig that has one (a temporary file, never the user's).
    audit: Option<(Arc<AuditLog>, std::path::PathBuf)>,
}

fn rig() -> Rig {
    rig_with(AgentAccess::Read, false, false)
}

/// A rig whose connection has `access`; `production` makes its type a production one, `audited`
/// gives the layer an audit log in a temporary directory. The layer asks the app's approver.
fn rig_with(access: AgentAccess, production: bool, audited: bool) -> Rig {
    let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
    let mut info = connection(CONN, ConnectionKind::PostgreSql, Some(access));
    if production {
        info.connection_type = engine_agent::ConnectionTypeInfo {
            name: "Production".into(),
            production: true,
            ..Default::default()
        };
    }
    let catalog = FakeCatalog::new(vec![info]);
    let sessions = FakeProvider::new().with_session(CONN, Arc::clone(&session));
    let layer = ToolLayer::new(catalog.clone(), sessions.clone(), FRONT_END)
        .with_approver(AppApprover::new());
    let (layer, audit) = if audited {
        let path = std::env::temp_dir()
            .join(format!("tablex-approval-{}", uuid::Uuid::new_v4()))
            .join("audit.jsonl");
        let log =
            Arc::new(AuditLog::open(&path, AuditConfig::default()).expect("a temp audit log"));
        (layer.with_audit(Arc::clone(&log)), Some((log, path)))
    } else {
        (layer, None)
    };
    Rig {
        layer: Arc::new(layer),
        session,
        sessions,
        audit,
    }
}

impl Rig {
    fn setup(&self, provider: &Arc<Scripted>) -> RunSetup {
        let provider: Arc<dyn ChatProvider> = provider.clone();
        RunSetup {
            provider,
            layer: Arc::clone(&self.layer).into(),
            model: "claude-test".into(),
            provider_id: "anthropic",
            effort: engine_ai::Effort::Medium,
            persist: None,
        }
    }
}

fn scope() -> Scope {
    Scope {
        connection_id: CONN.into(),
        database: None,
        schema: None,
    }
}

fn chat(cx: &mut TestAppContext) -> Entity<ChatSession> {
    let chat = cx.new(|_| ChatSession::new(Some(scope()), "shop"));
    // What the app does once it knows the key and the access.
    chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
    chat
}

fn ready(access: AgentAccess) -> ChatStatus {
    ChatStatus {
        readiness: Readiness::Ready,
        access,
    }
}

/// Runs the scheduler until `done`, letting real worker threads make progress: the clock advances
/// by the coalescing interval each pass, so the session's 30 ms wait never stalls a test.
fn wait(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    cx.dispatcher.allow_parking();
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        cx.executor().advance_clock(MIN_INTERVAL);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn idle(chat: &Entity<ChatSession>) -> impl FnMut(&mut TestAppContext) -> bool + '_ {
    |cx| !chat.read_with(cx, |chat, _| chat.is_running())
}

fn send(
    chat: &Entity<ChatSession>,
    cx: &mut TestAppContext,
    rig: &Rig,
    provider: &Arc<Scripted>,
    text: &str,
) -> SendOutcome {
    let setup = rig.setup(provider);
    chat.update(cx, |chat, cx| chat.send(text.to_owned(), setup, cx))
}

fn items(chat: &Entity<ChatSession>, cx: &TestAppContext) -> Vec<ChatItem> {
    chat.read_with(cx, |chat, _| chat.items().to_vec())
}

/// The items without the schema-summary notice the first run may add (the fake has no schema).
fn without_context_notice(items: Vec<ChatItem>) -> Vec<ChatItem> {
    items
        .into_iter()
        .filter(|item| {
            !matches!(
                item,
                ChatItem::Notice(ChatNotice::ContextUnavailable { .. })
            )
        })
        .collect()
}

fn steps(items: &[ChatItem]) -> Vec<&ToolStep> {
    items
        .iter()
        .filter_map(|item| match item {
            ChatItem::Tool(step) => Some(step),
            _ => None,
        })
        .collect()
}

// ---- the display model -------------------------------------------------------------------------

#[gpui::test]
fn a_streamed_answer_builds_the_items_in_order(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![Step::Say {
        reasoning: vec!["Looking".into(), " at the schema".into()],
        text: vec!["Hello ".into(), "**world**".into()],
        tools: Vec::new(),
    }]);
    assert_eq!(send(&chat, cx, &rig, &provider, "hi"), SendOutcome::Started);
    assert!(chat.read_with(cx, |chat, _| chat.is_running()));
    wait(cx, "the answer", idle(&chat));

    assert_eq!(
        without_context_notice(items(&chat, cx)),
        vec![
            ChatItem::User("hi".into()),
            ChatItem::Reasoning("Looking at the schema".into()),
            ChatItem::Assistant("Hello **world**".into()),
        ]
    );
    let usage = chat
        .read_with(cx, |chat, _| chat.usage())
        .expect("usage of the last turn");
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 5));
}

#[gpui::test]
fn a_tool_round_shows_a_step_going_from_running_to_ok(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    // The statement waits for the test, so the step can be seen while it runs.
    let (release, released) = channel::<()>();
    let released = Mutex::new(released);
    rig.session
        .set_behavior(Behavior::Script(Box::new(move |request, _| {
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            Ok(engine_agent::limits::RowCollector::new(
                Vec::new(),
                request.row_limit,
                request.limits,
            )
            .finish(false, Some(0)))
        })));
    let provider = Scripted::new(vec![
        call(
            "t1",
            "execute_query",
            json!({"connection_id": CONN, "statement": "SELECT\n 1"}),
        ),
        say("One row."),
    ]);
    send(&chat, cx, &rig, &provider, "count");
    wait(cx, "the running step", |cx| {
        steps(&items(&chat, cx))
            .first()
            .is_some_and(|step| step.args == "SELECT 1")
    });
    let running = items(&chat, cx);
    let step = steps(&running)[0];
    assert_eq!(
        (step.name.as_str(), step.state),
        ("execute_query", StepState::Running)
    );
    assert!(chat.read_with(cx, |chat, _| chat.is_running()));

    release.send(()).unwrap();
    wait(cx, "the answer", idle(&chat));
    let done = items(&chat, cx);
    assert_eq!(steps(&done)[0].state, StepState::Ok);
    assert!(matches!(done.last(), Some(ChatItem::Assistant(text)) if text == "One row."));
    assert_eq!(rig.session.run_count(), 1);
}

#[gpui::test]
fn a_failing_tool_is_an_error_step_and_the_answer_continues(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    rig.session
        .set_behavior(Behavior::Fail(AgentError::invalid("no such table: nope")));
    let provider = Scripted::new(vec![
        call(
            "t1",
            "execute_query",
            json!({"connection_id": CONN, "statement": "SELECT * FROM nope"}),
        ),
        say("That table does not exist."),
    ]);
    send(&chat, cx, &rig, &provider, "show nope");
    wait(cx, "the answer", idle(&chat));
    let all = items(&chat, cx);
    let step = steps(&all)[0];
    assert_eq!(step.state, StepState::Error);
    assert!(step.result.contains("no such table"), "{}", step.result);
    assert!(
        matches!(all.last(), Some(ChatItem::Assistant(text)) if text.contains("does not exist"))
    );
    assert!(
        !all.iter().any(|item| matches!(item, ChatItem::Error(_))),
        "a failed tool is not a failed run"
    );
}

#[gpui::test]
fn stopping_mid_stream_leaves_a_sendable_chat_and_retry_sends_the_message_again(
    cx: &mut TestAppContext,
) {
    let (rig, chat) = (rig(), chat(cx));
    let (_hold, gate) = channel::<()>();
    let provider = Scripted::new(vec![Step::Wait {
        text: "partial".into(),
        gate,
    }]);
    send(&chat, cx, &rig, &provider, "long question");
    wait(cx, "the first words", |cx| {
        items(&chat, cx)
            .iter()
            .any(|item| matches!(item, ChatItem::Assistant(text) if text == "partial"))
    });
    chat.update(cx, |chat, cx| chat.stop(cx));
    assert!(chat.read_with(cx, |chat, _| chat.is_stopping()));
    wait(cx, "the stop", idle(&chat));
    assert!(provider.saw_cancel.load(Ordering::SeqCst));
    assert!(matches!(
        items(&chat, cx).last(),
        Some(ChatItem::Notice(ChatNotice::Cancelled))
    ));
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));

    // The engine took the unanswered message back; retry sends it again, once.
    provider.push(say("Here you go."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the retried answer", idle(&chat));
    let all = without_context_notice(items(&chat, cx));
    assert_eq!(
        all.iter()
            .filter(|item| matches!(item, ChatItem::User(_)))
            .count(),
        1,
        "{all:?}"
    );
    assert!(matches!(all.last(), Some(ChatItem::Assistant(text)) if text == "Here you go."));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1], requests[0], "the same request went out again");
}

#[gpui::test]
fn a_run_that_failed_after_a_tool_round_is_resumed_from_the_results(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![
        call(
            "t1",
            "execute_query",
            json!({"connection_id": CONN, "statement": "SELECT 1"}),
        ),
        Step::Fail(ProviderError::Overloaded(failure("busy"))),
    ]);
    send(&chat, cx, &rig, &provider, "go");
    wait(cx, "the failure", idle(&chat));
    let all = items(&chat, cx);
    assert!(matches!(
        all.last(),
        Some(ChatItem::Error(error)) if error.kind == ErrorKind::Overloaded && error.action == Some(ErrorAction::Retry)
    ));
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));

    provider.push(say("Done."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the resumed answer", idle(&chat));
    let after = items(&chat, cx);
    assert!(
        !after.iter().any(|item| matches!(item, ChatItem::Error(_))),
        "the error line went away"
    );
    assert!(matches!(after.last(), Some(ChatItem::Assistant(text)) if text == "Done."));
    // It continued from the tool results: one user text in the history, then the results.
    let requests = provider.requests.lock().unwrap();
    let last = requests.last().unwrap();
    let texts = last
        .iter()
        .filter(|message| matches!(message, Message::User { content } if content.iter().any(|b| matches!(b, engine_ai::UserBlock::Text { .. }))))
        .count();
    assert_eq!(texts, 1);
    assert!(matches!(last.last(), Some(Message::User { .. })));
}

#[gpui::test]
fn a_rate_limit_error_offers_retry_and_is_reported_once(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let reported = Arc::new(AtomicUsize::new(0));
    let _subscription = cx.update(|cx| {
        let reported = Arc::clone(&reported);
        cx.subscribe(&chat, move |_, _: &ChatEvent, _| {
            reported.fetch_add(1, Ordering::SeqCst);
        })
    });
    let provider = Scripted::new(vec![Step::Fail(ProviderError::RateLimited(failure(
        "slow down",
    )))]);
    send(&chat, cx, &rig, &provider, "hi");
    wait(cx, "the failure", idle(&chat));
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));
    assert_eq!(reported.load(Ordering::SeqCst), 1);
}

// ---- the key never shows -----------------------------------------------------------------------

/// A server on 127.0.0.1 that answers every request with `status` and the body `body(key)`, where
/// `key` is the `x-api-key` header it was sent (a careless gateway repeats it).
fn serve(status: &'static str, body: fn(&str) -> String) -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n > 0) {
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head).into_owned();
            // Read the body too: closing with it unread would reset the connection.
            let length = text
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut request_body = vec![0u8; length];
            let _ = stream.read_exact(&mut request_body);
            let sent = text
                .lines()
                .find_map(|line| line.strip_prefix("x-api-key: "))
                .unwrap_or("none")
                .to_owned();
            let body = body(&sent);
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nrequest-id: req_9\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    address
}

fn serve_rejection() -> std::net::SocketAddr {
    serve("401 Unauthorized", |sent| {
        format!(
            r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key {sent}"}}}}"#
        )
    })
}

/// `GET /v1/models` answered with one model the built-in list does not have.
pub(in crate::app) fn serve_models() -> std::net::SocketAddr {
    serve("200 OK", |_| {
        r#"{"data":[{"id":"claude-new-9","display_name":"New 9"},{"id":"claude-opus-5-5"}],"has_more":false}"#.to_owned()
    })
}

fn real_provider(address: std::net::SocketAddr) -> (Arc<dyn ChatProvider>, Arc<ApiKeyStore>) {
    let keys = Arc::new(ApiKeyStore::memory());
    keys.save(KEY).unwrap();
    let ui = AssistantUi::new(
        None,
        None,
        Arc::clone(&keys),
        Arc::new(crate::ai::transport::ReqwestTransport::new("TableX-test").direct()),
        Arc::new(records::MemoryRecords::default()),
        None,
        Arc::new(NoSecrets),
    );
    let mut settings = AiSettings::default();
    settings.selected_mut().base_url = format!("http://{address}");
    (
        ui.provider(&settings)
            .expect("a loopback address is allowed"),
        keys,
    )
}

#[gpui::test]
fn a_rejected_key_gives_the_open_settings_error_and_the_key_appears_nowhere(
    cx: &mut TestAppContext,
) {
    let (rig, chat) = (rig(), chat(cx));
    let (provider, _keys) = real_provider(serve_rejection());
    let setup = RunSetup {
        provider,
        layer: Arc::clone(&rig.layer).into(),
        model: "claude-test".into(),
        provider_id: "anthropic",
        effort: engine_ai::Effort::Medium,
        persist: None,
    };
    chat.update(cx, |chat, cx| chat.send("hi".into(), setup, cx));
    wait(cx, "the rejection", idle(&chat));

    let all = items(&chat, cx);
    let Some(ChatItem::Error(error)) = all.last() else {
        panic!("expected an error item, got {all:?}");
    };
    assert_eq!(error.kind, ErrorKind::Authentication, "{error:?}");
    assert_eq!(error.action, Some(ErrorAction::OpenSettings));
    assert_eq!(error.request_id.as_deref(), Some("req_9"));
    let shown = format!("{all:?}");
    assert!(
        !shown.contains(KEY),
        "the key is in the display model: {shown}"
    );
    assert!(!shown.contains("sk-secret"), "{shown}");
}

// ---- coalescing ----------------------------------------------------------------------------------

#[test]
fn a_thousand_deltas_in_the_queue_are_one_text_and_one_wake() {
    let (shared, woken) = Shared::new();
    for _ in 0..1000 {
        shared.event(engine_ai::AssistantEvent::TextDelta("x"));
    }
    assert_eq!(woken.len(), 1, "one wake for the whole burst");
    woken.try_recv().unwrap();
    let taken = shared.take();
    assert_eq!(taken.len(), 1);
    assert!(
        matches!(&taken[0], Item::Update(super::model::Update::Text(text)) if text.len() == 1000)
    );
    // Taken: the next delta wakes again.
    shared.event(engine_ai::AssistantEvent::TextDelta("y"));
    assert_eq!(woken.len(), 1);
    woken.try_recv().unwrap();
    // A tool step in between keeps the order and splits the text.
    shared.event(engine_ai::AssistantEvent::ToolCallStarted { id: "a", name: "t" });
    shared.event(engine_ai::AssistantEvent::TextDelta("z"));
    assert_eq!(shared.take().len(), 3);
}

#[gpui::test]
fn a_streaming_answer_notifies_the_view_at_most_once_per_interval(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![Step::Say {
        reasoning: Vec::new(),
        text: (0..1000).map(|_| "x".to_owned()).collect(),
        tools: Vec::new(),
    }]);
    let notified = Arc::new(AtomicUsize::new(0));
    let _observer = cx.update(|cx| {
        let notified = Arc::clone(&notified);
        cx.observe(&chat, move |_, _| {
            notified.fetch_add(1, Ordering::SeqCst);
        })
    });
    send(&chat, cx, &rig, &provider, "stream");
    let mut passes = 0;
    wait(cx, "the answer", |cx| {
        passes += 1;
        idle(&chat)(cx)
    });
    let all = items(&chat, cx);
    assert!(matches!(all.last(), Some(ChatItem::Assistant(text)) if text.len() == 1000));
    let count = notified.load(Ordering::SeqCst);
    // The session looks at the queue at most once per interval, and the test clock moves by one
    // interval per pass: so at most one notification per pass (and the one of the send itself),
    // however many deltas the worker thread got through meanwhile. Bounded by the passes, not by
    // how fast this machine's scheduler runs them, and a notification per delta would be 1000.
    assert!(
        count <= passes + 2,
        "{count} notifications in {passes} passes for 1000 deltas"
    );
}

// ---- concurrency, cancel, revoke -------------------------------------------------------------

#[gpui::test]
fn two_quick_sends_do_not_run_at_the_same_time(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let (release, gate) = channel::<()>();
    let provider = Scripted::new(vec![Step::Wait {
        text: "first".into(),
        gate,
    }]);
    assert_eq!(
        send(&chat, cx, &rig, &provider, "one"),
        SendOutcome::Started
    );
    assert_eq!(send(&chat, cx, &rig, &provider, "two"), SendOutcome::Busy);
    assert_eq!(
        send(&chat, cx, &rig, &provider, "  "),
        SendOutcome::Busy,
        "busy wins over empty"
    );
    wait(cx, "the first words", |cx| {
        items(&chat, cx)
            .iter()
            .any(|item| matches!(item, ChatItem::Assistant(_)))
    });
    assert_eq!(send(&chat, cx, &rig, &provider, "three"), SendOutcome::Busy);
    release.send(()).unwrap();
    wait(cx, "the answer", idle(&chat));
    assert_eq!(provider.calls(), 1);
    assert_eq!(provider.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(
        items(&chat, cx)
            .iter()
            .filter(|item| matches!(item, ChatItem::User(_)))
            .count(),
        1,
        "the refused messages are not in the chat"
    );
    // And once it is over, the chat takes the next message.
    provider.push(say("second"));
    assert_eq!(
        send(&chat, cx, &rig, &provider, "four"),
        SendOutcome::Started
    );
    wait(cx, "the second answer", idle(&chat));
    assert_eq!(provider.max_active.load(Ordering::SeqCst), 1);
}

#[gpui::test]
fn an_empty_message_and_a_chat_that_is_not_ready_do_not_start_a_run(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![say("never")]);
    assert_eq!(send(&chat, cx, &rig, &provider, " \n"), SendOutcome::Empty);
    for readiness in [
        Readiness::NoApiKey,
        Readiness::AccessOff,
        Readiness::NoConnection,
    ] {
        let status = ChatStatus {
            readiness: readiness.clone(),
            access: AgentAccess::Off,
        };
        chat.update(cx, |chat, cx| chat.set_status(status, cx));
        assert_eq!(
            send(&chat, cx, &rig, &provider, "hi"),
            SendOutcome::NotReady(readiness)
        );
    }
    assert_eq!(provider.calls(), 0);
    assert!(items(&chat, cx).is_empty());
}

#[gpui::test]
fn revoking_the_connection_while_a_statement_runs_ends_the_call(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    rig.session.set_behavior(Behavior::BlockUntilCancelled);
    let provider = Scripted::new(vec![
        call(
            "t1",
            "execute_query",
            json!({"connection_id": CONN, "statement": "SELECT sleep(100)"}),
        ),
        say("The connection was closed."),
    ]);
    send(&chat, cx, &rig, &provider, "slow one");
    wait(cx, "the statement to start", |_| {
        rig.session.run_count() == 1
    });
    // What `revoke_agent_access` does, on a thread of its own as there.
    let layer = Arc::clone(&rig.layer);
    let revoker = std::thread::spawn(move || layer.revoke(CONN));
    wait(cx, "the answer", idle(&chat));
    revoker.join().unwrap();
    let all = items(&chat, cx);
    assert_eq!(steps(&all)[0].state, StepState::Error);
    assert!(matches!(all.last(), Some(ChatItem::Assistant(text)) if text.contains("closed")));
    assert!(
        rig.sessions.closed.load(Ordering::SeqCst) >= 1,
        "the session was closed"
    );
}

#[gpui::test]
fn closing_the_chat_cancels_the_run_without_waiting_for_it(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let (_hold, gate) = channel::<()>();
    let provider = Scripted::new(vec![Step::Wait {
        text: "partial".into(),
        gate,
    }]);
    send(&chat, cx, &rig, &provider, "hi");
    wait(cx, "the first words", |cx| {
        items(&chat, cx)
            .iter()
            .any(|item| matches!(item, ChatItem::Assistant(_)))
    });
    let started = Instant::now();
    drop(chat);
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "dropping the chat did not wait"
    );
    let start = Instant::now();
    while !provider.saw_cancel.load(Ordering::SeqCst) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the run was never cancelled"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[gpui::test]
fn quitting_cancels_a_running_chat(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let (_hold, gate) = channel::<()>();
    let provider = Scripted::new(vec![Step::Wait {
        text: "partial".into(),
        gate,
    }]);
    send(&chat, cx, &rig, &provider, "hi");
    wait(cx, "the first words", |cx| {
        items(&chat, cx)
            .iter()
            .any(|item| matches!(item, ChatItem::Assistant(_)))
    });
    chat.update(cx, |chat, _| chat.cancel_for_quit());
    wait(cx, "the end of the run", idle(&chat));
    assert!(provider.saw_cancel.load(Ordering::SeqCst));
}

#[gpui::test]
fn a_new_chat_drops_the_conversation_and_takes_the_new_scope(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![say("one"), say("two")]);
    send(&chat, cx, &rig, &provider, "first");
    wait(cx, "the answer", idle(&chat));
    let other = Scope {
        connection_id: "other".into(),
        database: Some("db".into()),
        schema: Some("public".into()),
    };
    chat.update(cx, |chat, cx| {
        chat.new_chat(Some(other.clone()), "other", cx)
    });
    assert!(items(&chat, cx).is_empty());
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.scope().cloned()),
        Some(other)
    );
    assert_eq!(chat.read_with(cx, |chat, _| chat.usage()), None);
    send(&chat, cx, &rig, &provider, "second");
    wait(cx, "the answer", idle(&chat));
    // The second request has only the new message: nothing of the first chat.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.last().unwrap().len(), 1);
}

// ---- the app facade ------------------------------------------------------------------------------

fn app(cx: &mut TestAppContext) -> Entity<AppRoot> {
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::theme::init(cx);
    });
    cx.new(AppRoot::new_for_test)
}

fn read_profile() -> crate::settings::connection::ConnectionProfile {
    let mut profile = crate::settings::connection::ConnectionProfile::default();
    profile.overrides.set(
        engine_connect::profile::AGENT_ACCESS,
        engine_connect::profile::OverrideValue::Text("read".into()),
    );
    profile
}

#[gpui::test]
fn the_status_follows_the_key_and_the_connections_access(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let app = app(cx);
    let chat = app.update(cx, |app, cx| {
        let chat = app.assistant_chat(cx);
        chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
        app.assistant_sync(cx);
        chat
    });
    wait(cx, "the key check", |cx| {
        chat.read_with(cx, |chat, _| {
            chat.status().readiness != Readiness::CheckingKey
        })
    });
    let status = |cx: &TestAppContext| chat.read_with(cx, |chat, _| chat.status().clone());
    // A connection nobody gave an agent level: off.
    assert_eq!(status(cx).readiness, Readiness::AccessOff);
    assert_eq!(status(cx).access, AgentAccess::Off);

    app.update(cx, |app, cx| {
        app.connection_policy.set_profile(CONN, read_profile());
        app.assistant_sync(cx);
    });
    assert_eq!(status(cx).readiness, Readiness::NoApiKey);
    assert_eq!(status(cx).access, AgentAccess::Read);

    app.update(cx, |app, cx| {
        app.assistant_save_key(AiProvider::Anthropic, format!("  {KEY}\n"), cx)
    });
    wait(cx, "the key to be saved", |cx| {
        chat.read_with(cx, |chat, _| chat.status().readiness == Readiness::Ready)
    });
    assert_eq!(
        app.read_with(cx, |app, _| app.assistant.keys.is_set()),
        Ok(true)
    );

    app.update(cx, |app, cx| {
        app.assistant_remove_key(AiProvider::Anthropic, cx)
    });
    wait(cx, "the key to be removed", |cx| {
        chat.read_with(cx, |chat, _| chat.status().readiness == Readiness::NoApiKey)
    });

    // No connection at all.
    chat.update(cx, |chat, cx| chat.new_chat(None, "", cx));
    app.update(cx, |app, cx| app.assistant_sync(cx));
    assert_eq!(status(cx).readiness, Readiness::NoConnection);
}

#[gpui::test]
fn a_rejected_key_through_the_app_is_filed_in_the_error_log_without_the_key(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let app = app(cx);
    let rig = rig();
    let address = serve_rejection();
    app.update(cx, |app, cx| {
        let mut settings = (*app.settings).clone();
        settings.ai.selected_mut().base_url = format!("http://{address}");
        app.settings = Arc::new(settings);
        app.assistant.set_layer_for_test(Arc::clone(&rig.layer));
        app.connection_policy.set_profile(CONN, read_profile());
        let chat = app.assistant_chat(cx);
        chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
        app.assistant_save_key(AiProvider::Anthropic, KEY.to_owned(), cx);
    });
    let chat = app.update(cx, |app, cx| app.assistant_chat(cx));
    wait(cx, "the key to be saved", |cx| {
        app.update(cx, |app, cx| app.assistant_sync(cx));
        chat.read_with(cx, |chat, _| chat.status().readiness == Readiness::Ready)
    });
    assert_eq!(
        app.update(cx, |app, cx| app.assistant_send("hello".into(), cx)),
        SendOutcome::Started
    );
    wait(cx, "the rejection", idle(&chat));

    let all = items(&chat, cx);
    assert!(matches!(
        all.last(),
        Some(ChatItem::Error(error)) if error.action == Some(ErrorAction::OpenSettings)
    ));
    let log = app.read_with(cx, |app, _| app.error_log.log.export_text());
    assert!(
        log.contains("Assistant:"),
        "the failure is in the error log: {log}"
    );
    assert!(!log.contains(KEY) && !log.contains("sk-secret"), "{log}");
    assert!(!format!("{all:?}").contains(KEY));
}

/// Records the thread each close ran on.
struct ThreadNoting {
    inner: Arc<FakeProvider>,
    closed_on: Mutex<Vec<std::thread::ThreadId>>,
}

impl engine_agent::SessionProvider for ThreadNoting {
    fn open(
        &self,
        connection: &engine_agent::ConnectionInfo,
    ) -> Result<Arc<dyn engine_agent::Session>, AgentError> {
        self.inner.open(connection)
    }
    fn close(&self, connection: &engine_agent::ConnectionInfo) {
        self.closed_on
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.inner.close(connection);
    }
}

#[gpui::test]
fn revoking_agent_access_closes_the_tool_layers_session_off_the_ui_thread(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let app = app(cx);
    let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
    let catalog = FakeCatalog::new(vec![connection(
        CONN,
        ConnectionKind::PostgreSql,
        Some(AgentAccess::Read),
    )]);
    let provider = Arc::new(ThreadNoting {
        inner: FakeProvider::new().with_session(CONN, session),
        closed_on: Mutex::default(),
    });
    let layer = Arc::new(ToolLayer::new(catalog, provider.clone(), FRONT_END));
    let opened = layer.call("open_session", json!({"connection_id": CONN}));
    assert!(!opened.is_error(), "{:?}", opened.to_json());
    assert!(layer.is_open(CONN));

    app.update(cx, |app, _| {
        app.assistant.set_layer_for_test(Arc::clone(&layer));
        app.revoke_agent_access(CONN);
    });
    let start = Instant::now();
    while layer.is_open(CONN) || provider.closed_on.lock().unwrap().is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the session was never closed"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let closed_on = provider.closed_on.lock().unwrap().clone();
    assert_eq!(closed_on.len(), 1);
    assert_ne!(
        closed_on[0],
        std::thread::current().id(),
        "the close ran on the UI thread"
    );
}

#[test]
fn the_connections_own_agent_level_is_written_next_to_its_other_overrides() {
    let json = overrides_with_level(r#"{"query.query_timeout_secs":30}"#, AgentLevel::Read);
    assert!(json.contains(r#""agent.access":"read""#), "{json}");
    assert!(json.contains("query.query_timeout_secs"), "{json}");
    assert!(overrides_with_level(&json, AgentLevel::Off).contains(r#""agent.access":"off""#));
    assert!(overrides_with_level("", AgentLevel::Schema).contains(r#""agent.access":"schema""#));
}

#[test]
fn the_tool_layer_and_its_audit_log_are_built_on_the_threads_of_the_runs_not_the_callers() {
    use super::layers::LayerCell;
    let dir = std::env::temp_dir().join(format!("tablex-assistant-layer-{}", uuid::Uuid::new_v4()));
    let path = dir.join("audit.jsonl");
    let store = Arc::new(engine_driver_host::InstalledDriverStore::new(
        dir.join("drivers"),
    ));
    let connector = engine_connect::connect::Connector::new(store);
    let agent = Arc::new(AgentConnections::new(
        connector,
        Arc::new(engine_connect::connect::RefusePrompts),
    ));
    let cell = Arc::new(LayerCell::new(Some(agent), Some(path.clone())));
    // Nothing is built, and no file is touched, until a run asks.
    assert!(cell.built().is_none());
    assert!(!path.exists());

    let source = LayerSource::Cell(Arc::clone(&cell));
    let caller = std::thread::current().id();
    let (built_on, first) = std::thread::spawn({
        let source = source.clone();
        move || {
            (
                std::thread::current().id(),
                source.resolve().expect("a layer"),
            )
        }
    })
    .join()
    .unwrap();
    assert_ne!(built_on, caller);
    assert!(path.exists(), "the audit log was opened by that run");
    let second = source.resolve().expect("the same layer");
    assert!(Arc::ptr_eq(&first, &second), "the runs share one layer");
    assert!(Arc::ptr_eq(&first, &cell.built().unwrap()));

    cell.shut_down();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_layer_with_no_agent_connections_is_a_typed_failure_of_the_run_not_a_panic() {
    use super::layers::LayerCell;
    let source = LayerSource::Cell(Arc::new(LayerCell::new(None, None)));
    assert!(source.resolve().is_err());
}

#[gpui::test]
fn a_run_whose_tool_layer_cannot_be_built_ends_in_an_error_item_and_leaves_the_chat_sendable(
    cx: &mut TestAppContext,
) {
    let chat = chat(cx);
    let provider = Scripted::new(vec![say("never asked")]);
    let provider_dyn: Arc<dyn ChatProvider> = provider.clone();
    let broken = RunSetup {
        provider: provider_dyn,
        layer: LayerSource::Cell(Arc::new(super::layers::LayerCell::new(None, None))),
        model: "claude-test".into(),
        provider_id: "anthropic",
        effort: engine_ai::Effort::Medium,
        persist: None,
    };
    chat.update(cx, |chat, cx| chat.send("hi".into(), broken, cx));
    wait(cx, "the failure", idle(&chat));
    let all = items(&chat, cx);
    assert!(
        matches!(all.last(), Some(ChatItem::Error(error)) if error.message.contains("no connections")),
        "{all:?}"
    );
    assert_eq!(provider.calls(), 0, "the model was never asked");
    // Fixed, the same chat goes on.
    let rig = rig();
    assert!(chat.update(cx, |chat, cx| chat.retry(rig.setup(&provider), cx)));
    wait(cx, "the answer", idle(&chat));
    assert!(
        matches!(items(&chat, cx).last(), Some(ChatItem::Assistant(text)) if text == "never asked")
    );
}

#[gpui::test]
fn an_empty_answer_keeps_the_message_on_screen_and_retry_sends_it_again(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![Step::Say {
        reasoning: vec![],
        text: vec![],
        tools: vec![],
    }]);
    send(&chat, cx, &rig, &provider, "say something");
    wait(cx, "the empty answer", idle(&chat));
    let all = without_context_notice(items(&chat, cx));
    assert_eq!(all[0], ChatItem::User("say something".into()));
    assert!(
        matches!(
            all.last(),
            Some(ChatItem::Notice(ChatNotice::EmptyAnswer {
                user_message_taken_back: true
            }))
        ),
        "{all:?}"
    );
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));
    provider.push(say("Now I do."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the retry", idle(&chat));
    let all = without_context_notice(items(&chat, cx));
    assert_eq!(
        all.iter()
            .filter(|item| matches!(item, ChatItem::User(_)))
            .count(),
        1
    );
    assert!(matches!(all.last(), Some(ChatItem::Assistant(text)) if text == "Now I do."));
}

// ---- what Retry sends, and what the display says about it ----------------------------------------

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content
                .iter()
                .filter_map(|block| match block {
                    engine_ai::UserBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// Q1 runs a tool round and is stopped in its second turn: the conversation ends with the tool
/// results (Retry would resume it).
fn stopped_behind_results(
    cx: &mut TestAppContext,
    rig: &Rig,
    chat: &Entity<ChatSession>,
    provider: &Arc<Scripted>,
) {
    let (hold, gate) = channel::<()>();
    std::mem::forget(hold);
    provider.push(call(
        "t1",
        "execute_query",
        json!({"connection_id": CONN, "statement": "SELECT 1"}),
    ));
    provider.push(Step::Wait {
        text: "working".into(),
        gate,
    });
    send(chat, cx, rig, provider, "first question");
    wait(cx, "the second turn", |cx| {
        items(chat, cx)
            .iter()
            .any(|item| matches!(item, ChatItem::Assistant(text) if text == "working"))
    });
    chat.update(cx, |chat, cx| chat.stop(cx));
    wait(cx, "the stop", idle(chat));
    assert!(matches!(
        items(chat, cx).last(),
        Some(ChatItem::Notice(ChatNotice::Cancelled))
    ));
}

#[gpui::test]
fn retry_sends_the_new_message_that_failed_not_the_old_run_it_followed(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![]);
    stopped_behind_results(cx, &rig, &chat, &provider);

    // The user does not retry; they ask something else, and that fails in its first turn.
    provider.push(Step::Fail(ProviderError::Overloaded(failure("busy"))));
    send(&chat, cx, &rig, &provider, "second question");
    wait(cx, "the failure", idle(&chat));
    let all = items(&chat, cx);
    let second = all
        .iter()
        .position(|item| matches!(item, ChatItem::User(text) if text == "second question"))
        .expect("the question is on screen");
    assert!(
        chat.read_with(cx, |chat, _| chat.is_unsent(second)),
        "the display says the model never got it"
    );
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));

    provider.push(say("Answer two."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the retried answer", idle(&chat));
    assert!(chat.read_with(cx, |chat, _| !chat.is_unsent(second)));
    let requests = provider.requests.lock().unwrap();
    let last = user_texts(requests.last().unwrap());
    assert!(last.contains(&"second question".to_owned()), "{last:?}");
    assert_eq!(
        last.iter().filter(|text| *text == "first question").count(),
        1,
        "the first question is not sent twice: {last:?}"
    );
    drop(requests);
    assert!(matches!(
        without_context_notice(items(&chat, cx)).last(),
        Some(ChatItem::Assistant(text)) if text == "Answer two."
    ));
}

#[gpui::test]
fn an_empty_answer_to_a_new_message_behind_results_is_retried_as_that_message(
    cx: &mut TestAppContext,
) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![]);
    stopped_behind_results(cx, &rig, &chat, &provider);

    provider.push(Step::Say {
        reasoning: vec![],
        text: vec![],
        tools: vec![],
    });
    send(&chat, cx, &rig, &provider, "second question");
    wait(cx, "the empty answer", idle(&chat));
    assert!(matches!(
        items(&chat, cx).last(),
        Some(ChatItem::Notice(ChatNotice::EmptyAnswer {
            user_message_taken_back: true
        }))
    ));
    provider.push(say("Now."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the retry", idle(&chat));
    let requests = provider.requests.lock().unwrap();
    assert!(user_texts(requests.last().unwrap()).contains(&"second question".to_owned()));
    drop(requests);
    assert!(matches!(
        items(&chat, cx).last(),
        Some(ChatItem::Assistant(text)) if text == "Now."
    ));
}

#[gpui::test]
fn a_stop_behind_results_is_resumed_and_a_message_that_was_kept_is_not_marked(
    cx: &mut TestAppContext,
) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![]);
    stopped_behind_results(cx, &rig, &chat, &provider);
    // Q1's first turn completed: its message is in the conversation, nothing is "not kept".
    assert!(chat.read_with(cx, |chat, _| (0..chat.items().len())
        .all(|ix| !chat.is_unsent(ix))));
    provider.push(say("Carried on."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the resume", idle(&chat));
    let requests = provider.requests.lock().unwrap();
    let texts = user_texts(requests.last().unwrap());
    assert_eq!(texts, ["first question"], "resumed, nothing new sent");
}

#[gpui::test]
fn a_reasoning_only_turn_dropped_as_empty_is_not_kept_for_the_retry_to_append_to(
    cx: &mut TestAppContext,
) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![Step::Say {
        reasoning: vec!["first thoughts".into()],
        text: vec![],
        tools: vec![],
    }]);
    send(&chat, cx, &rig, &provider, "think about it");
    wait(cx, "the empty answer", idle(&chat));
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));
    provider.push(Step::Say {
        reasoning: vec!["second thoughts".into()],
        text: vec!["Done.".into()],
        tools: vec![],
    });
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the retry", idle(&chat));
    let reasoning: Vec<String> = items(&chat, cx)
        .into_iter()
        .filter_map(|item| match item {
            ChatItem::Reasoning(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasoning,
        ["second thoughts"],
        "the dropped turn's reasoning is gone"
    );
}

#[gpui::test]
fn the_round_limit_offers_retry_and_retry_continues_from_the_results(cx: &mut TestAppContext) {
    const IDS: [&str; 16] = [
        "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "r13",
        "r14", "r15",
    ];
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(
        IDS.iter()
            .map(|id| {
                call(
                    id,
                    "execute_query",
                    json!({"connection_id": CONN, "statement": "SELECT 1"}),
                )
            })
            .collect(),
    );
    send(&chat, cx, &rig, &provider, "keep going");
    wait(cx, "the limit", idle(&chat));
    assert!(matches!(
        items(&chat, cx).last(),
        Some(ChatItem::Notice(ChatNotice::RoundLimit { .. }))
    ));
    assert!(
        chat.read_with(cx, |chat, _| chat.can_retry()),
        "the notice says Retry continues, so Retry exists"
    );
    provider.push(say("Finished."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the continuation", idle(&chat));
    let all = items(&chat, cx);
    assert!(matches!(all.last(), Some(ChatItem::Assistant(text)) if text == "Finished."));
    assert!(!all
        .iter()
        .any(|item| matches!(item, ChatItem::Notice(ChatNotice::RoundLimit { .. }))));
}

// ---- readiness, failures, access ------------------------------------------------------------------

#[gpui::test]
fn a_new_chat_is_not_ready_until_the_app_has_said_so(cx: &mut TestAppContext) {
    let rig = rig();
    let chat = cx.new(|_| ChatSession::new(Some(scope()), "shop"));
    assert_ne!(
        chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
        Readiness::Ready
    );
    let provider = Scripted::new(vec![say("never")]);
    assert!(matches!(
        send(&chat, cx, &rig, &provider, "hi"),
        SendOutcome::NotReady(_)
    ));
    assert_eq!(provider.calls(), 0);
}

#[gpui::test]
fn a_failure_the_chat_reports_is_a_notification_the_panel_can_hear(cx: &mut TestAppContext) {
    let chat = chat(cx);
    let notified = Arc::new(AtomicUsize::new(0));
    let _observer = cx.update(|cx| {
        let notified = Arc::clone(&notified);
        cx.observe(&chat, move |_, _| {
            notified.fetch_add(1, Ordering::SeqCst);
        })
    });
    chat.update(cx, |chat, cx| {
        chat.fail(ChatError::config("The AI settings cannot be used."), cx)
    });
    cx.run_until_parked();
    assert!(
        notified.load(Ordering::SeqCst) >= 1,
        "a row was added and nobody was told"
    );
    // A setup failure offers the way out, not a Retry that would fail the same way.
    assert!(matches!(
        items(&chat, cx).last(),
        Some(ChatItem::Error(error)) if error.action == Some(ErrorAction::OpenSettings)
    ));
    assert!(!chat.read_with(cx, |chat, _| chat.can_retry()));
}

#[gpui::test]
fn lowering_the_access_under_a_chat_with_history_ends_it(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![say("one"), say("two")]);
    send(&chat, cx, &rig, &provider, "first");
    wait(cx, "the answer", idle(&chat));
    // Read -> schema only: the history may hold rows the new level would not have shown.
    chat.update(cx, |chat, cx| {
        chat.set_status(ready(AgentAccess::SchemaOnly), cx)
    });
    assert!(chat.read_with(cx, |chat, _| chat.is_ended()));
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
        Readiness::AccessLowered
    );
    assert!(matches!(
        items(&chat, cx).last(),
        Some(ChatItem::Notice(ChatNotice::AccessLowered))
    ));
    assert_eq!(
        send(&chat, cx, &rig, &provider, "second"),
        SendOutcome::NotReady(Readiness::AccessLowered)
    );
    assert_eq!(provider.calls(), 1, "nothing more went to the model");
    // Raising it again does not undo that; only a new chat starts clean.
    chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
        Readiness::AccessLowered
    );
    chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
    chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
    assert_eq!(
        send(&chat, cx, &rig, &provider, "fresh"),
        SendOutcome::Started
    );
    wait(cx, "the fresh answer", idle(&chat));
}

#[gpui::test]
fn lowering_the_access_stops_a_run_that_is_calling_the_model(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let (_hold, gate) = channel::<()>();
    let provider = Scripted::new(vec![Step::Wait {
        text: "partial".into(),
        gate,
    }]);
    send(&chat, cx, &rig, &provider, "long");
    wait(cx, "the first words", |cx| {
        items(&chat, cx)
            .iter()
            .any(|item| matches!(item, ChatItem::Assistant(_)))
    });
    chat.update(cx, |chat, cx| {
        chat.set_status(
            ChatStatus {
                readiness: Readiness::AccessOff,
                access: AgentAccess::Off,
            },
            cx,
        )
    });
    wait(cx, "the run to end", idle(&chat));
    assert!(
        provider.saw_cancel.load(Ordering::SeqCst),
        "the run was cancelled"
    );
    assert!(chat.read_with(cx, |chat, _| chat.is_ended()));
    // The run's own "Stopped." is the last row, but the chat cannot send any more: no Retry that
    // would do nothing.
    assert!(matches!(
        items(&chat, cx).last(),
        Some(ChatItem::Notice(ChatNotice::Cancelled))
    ));
    assert!(!chat.read_with(cx, |chat, _| chat.can_retry()));
    let setup = rig.setup(&provider);
    assert!(!chat.update(cx, |chat, cx| chat.retry(setup, cx)));
}

/// Answers, and is stopped just as the answer completes: the engine reports `Cancelled` with the
/// answer kept in the conversation.
struct StoppedAsItAnswers;

impl ChatProvider for StoppedAsItAnswers {
    fn stream_turn(
        &self,
        _: &engine_ai::TurnRequest<'_>,
        on_event: &mut dyn FnMut(engine_ai::StreamEvent<'_>),
        cancel: &engine_agent::CancelToken,
    ) -> Result<engine_ai::TurnResponse, ProviderError> {
        on_event(engine_ai::StreamEvent::TextDelta("All done."));
        cancel.cancel_detached();
        Ok(super::scripted::response(
            vec![json!({"type": "text", "text": "All done."})],
            engine_ai::StopReason::EndTurn,
        ))
    }

    fn list_models(
        &self,
        _: &engine_agent::CancelToken,
    ) -> Result<Vec<engine_ai::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

#[gpui::test]
fn a_stop_after_the_last_turn_completed_offers_no_retry(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let setup = RunSetup {
        provider: Arc::new(StoppedAsItAnswers),
        layer: Arc::clone(&rig.layer).into(),
        model: "claude-test".into(),
        provider_id: "anthropic",
        effort: engine_ai::Effort::Medium,
        persist: None,
    };
    chat.update(cx, |chat, cx| {
        chat.send("question".to_owned(), setup.clone(), cx)
    });
    wait(cx, "the run", idle(&chat));
    let all = without_context_notice(items(&chat, cx));
    assert!(
        matches!(all.as_slice(), [ChatItem::User(_), ChatItem::Assistant(text), ChatItem::Notice(ChatNotice::Cancelled)] if text == "All done."),
        "the answer is kept: {all:?}"
    );
    // Nothing to resume from (the conversation ends with the answer) and the message is not
    // taken back: Retry would only fail with "nothing to resume".
    assert!(!chat.read_with(cx, |chat, _| chat.can_retry()));
    assert!(!chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    assert!(!chat.read_with(cx, |chat, _| chat.is_unsent(0)));
    // The chat takes the next message.
    let provider = Scripted::new(vec![say("Next.")]);
    assert_eq!(
        send(&chat, cx, &rig, &provider, "more"),
        SendOutcome::Started
    );
    wait(cx, "the next answer", idle(&chat));
}

/// Retry with settings that cannot be used: the error says so once, behind the row, and the row
/// stays retryable for when the settings are fixed.
#[gpui::test]
fn a_retry_that_cannot_be_set_up_keeps_the_row_retryable(cx: &mut TestAppContext) {
    let (rig, chat) = (rig(), chat(cx));
    let provider = Scripted::new(vec![Step::Fail(ProviderError::Overloaded(failure("busy")))]);
    send(&chat, cx, &rig, &provider, "question");
    wait(cx, "the failure", idle(&chat));
    let before = items(&chat, cx).len();
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));
    let retry_row = chat.read_with(cx, |chat, _| chat.retry_item()).unwrap();
    for _ in 0..2 {
        chat.update(cx, |chat, cx| {
            chat.fail(ChatError::config("The AI settings cannot be used."), cx)
        });
    }
    assert_eq!(items(&chat, cx).len(), before + 1, "one message, not two");
    assert!(chat.read_with(cx, |chat, _| chat.can_retry()));
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.retry_item()),
        Some(retry_row)
    );
    // Fixed: the retry runs and the messages go with the failed turn.
    provider.push(say("Here."));
    let setup = rig.setup(&provider);
    assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
    wait(cx, "the retry", idle(&chat));
    let all = without_context_notice(items(&chat, cx));
    assert!(
        !all.iter().any(|item| matches!(item, ChatItem::Error(_))),
        "{all:?}"
    );
    assert!(matches!(all.last(), Some(ChatItem::Assistant(text)) if text == "Here."));
}

#[gpui::test]
fn lowering_the_access_under_an_empty_chat_changes_nothing_else(cx: &mut TestAppContext) {
    let chat = chat(cx);
    chat.update(cx, |chat, cx| {
        chat.set_status(
            ChatStatus {
                readiness: Readiness::AccessOff,
                access: AgentAccess::Off,
            },
            cx,
        )
    });
    assert!(!chat.read_with(cx, |chat, _| chat.is_ended()));
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
        Readiness::AccessOff
    );
}

/// The release of the app hands the tool layer's cell to a thread of its own. If that thread
/// were the one to drop the agent connections last, their prompt channel would wake a task of the
/// test scheduler from a thread the scheduler does not run on (a test failure of its own, seen as
/// "your test is not deterministic"). A cell without agent connections has nothing of the kind.
fn no_agent_connections_to_drop_on_the_release_thread(
    app: &Entity<AppRoot>,
    cx: &mut TestAppContext,
) {
    app.update(cx, |app, _| {
        app.assistant.layers = Arc::new(LayerCell::new(None, None))
    });
}

#[gpui::test]
fn dropping_the_app_without_a_quit_still_closes_the_agent_sessions(cx: &mut TestAppContext) {
    let app = app(cx);
    let rig = rig();
    no_agent_connections_to_drop_on_the_release_thread(&app, cx);
    let opened = rig
        .layer
        .call("open_session", json!({"connection_id": CONN}));
    assert!(!opened.is_error(), "{:?}", opened.to_json());
    assert!(rig.layer.is_open(CONN));
    app.update(cx, |app, _| {
        app.assistant.set_layer_for_test(Arc::clone(&rig.layer))
    });
    // The window goes away; no Quit ran.
    drop(app);
    cx.update(|_| {});
    let start = Instant::now();
    // The layer stops being open a moment before the fake session counts the close.
    while rig.layer.is_open(CONN) || rig.sessions.closed.load(Ordering::SeqCst) < 1 {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the session was never closed"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[gpui::test]
fn dropping_the_app_without_a_quit_writes_a_layout_change_that_was_waiting(
    cx: &mut TestAppContext,
) {
    let app = app(cx);
    no_agent_connections_to_drop_on_the_release_thread(&app, cx);
    let dir =
        std::env::temp_dir().join(format!("tablex-assistant-release-{}", uuid::Uuid::new_v4()));
    let path = dir.join("sidebar-preferences.json");
    app.update(cx, |app, cx| {
        app.sidebar.path = Some(path.clone());
        // A change waits for its quiet moment (400 ms) before it is written.
        app.assistant_resized(512.0, cx);
    });
    assert!(!path.exists(), "nothing is written at the change itself");
    // The window goes away within that moment; no Quit ran.
    drop(app);
    cx.update(|_| {});
    let preferences = crate::sidebar_preferences::SidebarPreferences::load(&path)
        .expect("the release wrote the layout");
    assert_eq!(preferences.resolve_assistant_width(), 512.0);
    let _ = std::fs::remove_dir_all(dir);
}

// ---- writes that ask first ----------------------------------------------------------------------------

const UPDATE: &str = "UPDATE orders\n   SET status = 'cancelled'\n WHERE id = 7";

fn write_call(statement: &str) -> Step {
    call(
        "t1",
        "execute_query",
        json!({"connection_id": CONN, "statement": statement}),
    )
}

/// Lets the card's delay pass on the executor's clock (no sleeping), then answers the question
/// the first step asked.
fn answer_armed(chat: &Entity<ChatSession>, cx: &mut TestAppContext, approve: bool) -> bool {
    cx.executor().advance_clock(super::approval::APPROVAL_DELAY);
    cx.run_until_parked();
    let question = question_of(chat, cx);
    chat.update(cx, |chat, cx| chat.answer_question(&question, approve, cx))
}

fn asking(chat: &Entity<ChatSession>) -> impl FnMut(&mut TestAppContext) -> bool + '_ {
    |cx| chat.read_with(cx, |chat, _| chat.is_asking())
}

/// The audit lines of the rig's write tool, newest first, as `(outcome, approval)`.
fn write_audit(rig: &Rig) -> Vec<(String, Option<String>)> {
    let (log, path) = rig.audit.as_ref().expect("an audited rig");
    log.flush();
    engine_agent::read_entries(path, 50)
        .unwrap()
        .entries
        .into_iter()
        .filter(|entry| entry.tool == "execute_query")
        .map(|entry| (entry.outcome, entry.approval))
        .collect()
}

/// The question of the first step.
fn question_of(chat: &Entity<ChatSession>, cx: &TestAppContext) -> Arc<super::approval::Question> {
    let all = items(chat, cx);
    steps(&all)[0].question.clone().expect("the step asked")
}

/// Waits until the worker thread of a run is gone: the run's setup (and so the provider it holds)
/// was dropped by the thread, which leaves only the test's own handle.
fn wait_for_worker_to_end(cx: &mut TestAppContext, provider: &Arc<Scripted>) {
    wait(cx, "the worker thread to end", |_| {
        Arc::strong_count(provider) == 1
    });
}

/// The events a chat emitted, in order.
fn record_events(
    chat: &Entity<ChatSession>,
    cx: &mut TestAppContext,
) -> Arc<Mutex<Vec<ChatEvent>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    cx.update(|cx| {
        cx.subscribe(chat, move |_, event: &ChatEvent, _| {
            sink.lock().unwrap().push(event.clone());
        })
        .detach();
    });
    events
}

#[gpui::test]
fn a_write_waits_for_the_click_and_an_approval_runs_it_once_and_says_what_it_did(
    cx: &mut TestAppContext,
) {
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, true), chat(cx));
    rig.session.set_behavior(Behavior::Affected(3));
    let events = record_events(&chat, cx);
    let provider = Scripted::new(vec![write_call(UPDATE), say("Done: three rows.")]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the question", asking(&chat));

    // Nothing ran; the step is the one that asked, and the card has what the layer asked with.
    assert_eq!(rig.session.run_count(), 0);
    let question = question_of(&chat, cx);
    assert_eq!(question.connection.as_ref(), format!("conn-{CONN}"));
    assert_eq!(question.connection_type.as_ref(), "Development");
    assert!(!question.production);
    assert_eq!(question.database.as_deref(), Some("main"));
    assert_eq!(
        question.statement.as_ref(),
        UPDATE,
        "the exact statement, in full"
    );
    assert_eq!(
        question.shown.as_ref(),
        UPDATE,
        "nothing was spelled out or reformatted"
    );
    assert!(question.dangerous.is_none());
    assert_eq!(steps(&items(&chat, cx))[0].state, StepState::Running);
    // One at a time, and no second run while it waits.
    assert_eq!(
        send(&chat, cx, &rig, &provider, "and again"),
        SendOutcome::Busy
    );
    assert_eq!(rig.session.run_count(), 0, "waiting is not approving");

    assert!(answer_armed(&chat, cx, true));
    wait(cx, "the end of the run", idle(&chat));
    // Exactly once, byte for byte what the card showed.
    assert_eq!(
        *rig.session.runs.lock().unwrap(),
        vec![(UPDATE.to_owned(), false)]
    );
    let all = items(&chat, cx);
    let step = steps(&all)[0];
    assert_eq!(step.state, StepState::Ok);
    assert_eq!(step.result, "ran: 3 rows affected");
    assert!(matches!(all.last(), Some(ChatItem::Assistant(text)) if text.contains("three")));
    assert_eq!(
        *events.lock().unwrap(),
        vec![ChatEvent::Asking(true), ChatEvent::Asking(false)]
    );

    // The audit log has the approved write, with its statement.
    let (log, path) = rig.audit.as_ref().unwrap();
    log.flush();
    let entries = engine_agent::read_entries(path, 50).unwrap().entries;
    assert!(
        entries
            .iter()
            .any(|entry| entry.statement.as_deref() == Some(UPDATE)
                && entry.front_end == FRONT_END
                && entry.tool == "execute_query"),
        "{entries:?}"
    );
    assert_eq!(
        write_audit(&rig),
        [
            ("ok".to_owned(), Some("approved".to_owned())),
            ("started".to_owned(), Some("approved".to_owned()))
        ],
        "the log says the user approved"
    );
    // Read back from the real conversation, the approved write reads as it did live.
    let restored = chat
        .read_with(cx, |chat, _| {
            chat.conversation_for_test().map(Transcript::restore)
        })
        .expect("the run is over");
    assert!(restored.items().iter().any(
        |item| matches!(item, ChatItem::Tool(step) if step.result == "ran: 3 rows affected" && step.state == StepState::Ok)
    ));
    wait_for_worker_to_end(cx, &provider);
}

#[gpui::test]
fn declining_runs_nothing_gives_the_model_the_refusal_and_the_run_goes_on(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, true), chat(cx));
    let provider = Scripted::new(vec![
        write_call(UPDATE),
        say("Understood, nothing changed."),
    ]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the question", asking(&chat));
    assert!(answer_armed(&chat, cx, false));
    wait(cx, "the end of the run", idle(&chat));

    assert_eq!(rig.session.run_count(), 0, "nothing reached the session");
    let all = items(&chat, cx);
    let step = steps(&all)[0];
    assert_eq!(step.state, StepState::Error);
    assert_eq!(step.result, "not run: declined");
    assert!(
        matches!(all.last(), Some(ChatItem::Assistant(text)) if text.contains("nothing changed"))
    );
    // The model saw the layer's typed refusal with the app's one fixed reason, nothing else.
    let requests = provider.requests.lock().unwrap();
    let results: Vec<String> = requests
        .last()
        .unwrap()
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            engine_ai::UserBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].contains("write_refused"), "{}", results[0]);
    assert!(
        results[0].contains("the user did not approve this SQL statement: the user declined"),
        "{}",
        results[0]
    );
    drop(requests);
    // Read back from the conversation of this real run, the step reads as it did live.
    let restored = chat
        .read_with(cx, |chat, _| {
            chat.conversation_for_test().map(Transcript::restore)
        })
        .expect("the run is over");
    let restored_steps: Vec<(StepState, String)> = restored
        .items()
        .iter()
        .filter_map(|item| match item {
            ChatItem::Tool(step) => Some((step.state, step.result.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        restored_steps,
        [(StepState::Error, "not run: declined".to_owned())]
    );
    // The model sees a policy-style refusal; the log says it was the user who declined.
    assert_eq!(
        write_audit(&rig),
        [(
            "agent.write_refused".to_owned(),
            Some("declined: the user declined".to_owned())
        )]
    );
    wait_for_worker_to_end(cx, &provider);
}

#[gpui::test]
fn with_read_the_same_call_is_refused_by_the_layer_and_nothing_is_asked(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::Read, false, false), chat(cx));
    let events = record_events(&chat, cx);
    let provider = Scripted::new(vec![write_call(UPDATE), say("I may not change data.")]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the end of the run", idle(&chat));
    assert_eq!(rig.session.run_count(), 0);
    let all = items(&chat, cx);
    let step = steps(&all)[0];
    assert!(step.question.is_none(), "no card for a refused call");
    assert_eq!(step.state, StepState::Error);
    assert!(step.result.contains("write_refused"), "{}", step.result);
    assert!(events.lock().unwrap().is_empty(), "nothing waited");
}

#[gpui::test]
fn a_dangerous_write_is_refused_without_the_flag_and_asked_with_it(cx: &mut TestAppContext) {
    // Without the model's flag the layer refuses and nobody is asked.
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, true, false), chat(cx));
    let provider = Scripted::new(vec![write_call("DELETE FROM orders"), say("Refused.")]);
    send(&chat, cx, &rig, &provider, "clear it");
    wait(cx, "the end of the run", idle(&chat));
    let all = items(&chat, cx);
    assert!(steps(&all)[0].question.is_none());
    assert!(
        steps(&all)[0].result.contains("dangerous"),
        "{}",
        steps(&all)[0].result
    );
    assert_eq!(rig.session.run_count(), 0);

    // With it, the card has the layer's reason and the type, and an approval runs it once, as a
    // confirmed statement.
    chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
    chat.update(cx, |chat, cx| {
        chat.set_status(ready(AgentAccess::ReadWrite), cx)
    });
    let provider = Scripted::new(vec![
        call(
            "t1",
            "execute_query",
            json!({"connection_id": CONN, "statement": "DELETE FROM orders", "dangerous": true}),
        ),
        say("Cleared."),
    ]);
    send(&chat, cx, &rig, &provider, "clear it, really");
    wait(cx, "the question", asking(&chat));
    let question = question_of(&chat, cx);
    assert!(
        question.production,
        "the connection's type is a production one"
    );
    assert_eq!(question.connection_type.as_ref(), "Production");
    let reason = question.dangerous.clone().expect("the layer's reason");
    assert!(!reason.is_empty());
    assert_eq!(rig.session.run_count(), 0);
    assert!(answer_armed(&chat, cx, true));
    wait(cx, "the end of the run", idle(&chat));
    assert_eq!(
        *rig.session.runs.lock().unwrap(),
        vec![("DELETE FROM orders".to_owned(), true)]
    );
    wait_for_worker_to_end(cx, &provider);
}

#[gpui::test]
fn stop_ends_the_question_as_not_approved_and_the_worker_comes_back(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, true), chat(cx));
    let events = record_events(&chat, cx);
    let provider = Scripted::new(vec![write_call(UPDATE), say("never")]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the question", asking(&chat));
    let question = question_of(&chat, cx);
    chat.update(cx, |chat, cx| chat.stop(cx));
    // At once, before the worker has noticed: a click now cannot approve.
    assert_eq!(question.answer(), Some(Answer::Stopped));
    assert!(!chat.update(cx, |chat, cx| chat.answer_question(&question, true, cx)));
    assert!(!chat.read_with(cx, |chat, _| chat.is_asking()));
    wait(cx, "the run to end", idle(&chat));
    assert_eq!(rig.session.run_count(), 0);
    let all = items(&chat, cx);
    assert_eq!(steps(&all)[0].result, "not run: stopped");
    assert!(matches!(
        all.last(),
        Some(ChatItem::Notice(ChatNotice::Cancelled))
    ));
    assert_eq!(
        *events.lock().unwrap(),
        vec![ChatEvent::Asking(true), ChatEvent::Asking(false)]
    );
    assert_eq!(
        write_audit(&rig),
        [("agent.cancelled".to_owned(), Some("cancelled".to_owned()))]
    );
    wait_for_worker_to_end(cx, &provider);
}

#[gpui::test]
fn a_new_chat_ends_the_question_and_the_worker_comes_back(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, false), chat(cx));
    let provider = Scripted::new(vec![write_call(UPDATE), say("never")]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the question", asking(&chat));
    let question = question_of(&chat, cx);
    chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
    assert_eq!(question.answer(), Some(Answer::Stopped));
    assert!(!chat.read_with(cx, |chat, _| chat.is_asking()));
    // The old run's worker is not the chat's any more; it must still end.
    wait_for_worker_to_end(cx, &provider);
    assert_eq!(rig.session.run_count(), 0);
    assert!(
        items(&chat, cx).is_empty(),
        "nothing of the old run came back"
    );
}

#[gpui::test]
fn lowering_the_access_ends_the_question_as_not_approved(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, false), chat(cx));
    chat.update(cx, |chat, cx| {
        chat.set_status(ready(AgentAccess::ReadWrite), cx)
    });
    let provider = Scripted::new(vec![write_call(UPDATE), say("never")]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the question", asking(&chat));
    let question = question_of(&chat, cx);
    chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
    assert_eq!(question.answer(), Some(Answer::Stopped));
    assert!(chat.read_with(cx, |chat, _| chat.is_ended()));
    wait(cx, "the run to end", idle(&chat));
    assert_eq!(rig.session.run_count(), 0);
    wait_for_worker_to_end(cx, &provider);
}

#[gpui::test]
fn raising_the_access_under_a_chat_with_history_does_not_end_it(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::Read, false, false), chat(cx));
    let provider = Scripted::new(vec![say("one"), say("two")]);
    send(&chat, cx, &rig, &provider, "first");
    wait(cx, "the answer", idle(&chat));
    chat.update(cx, |chat, cx| {
        chat.set_status(ready(AgentAccess::ReadWrite), cx)
    });
    assert!(!chat.read_with(cx, |chat, _| chat.is_ended()));
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
        Readiness::Ready
    );
    assert_eq!(
        send(&chat, cx, &rig, &provider, "second"),
        SendOutcome::Started
    );
    wait(cx, "the second answer", idle(&chat));
}

#[gpui::test]
fn dropping_the_chat_ends_the_question_and_the_worker_comes_back(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, false), chat(cx));
    let provider = Scripted::new(vec![write_call(UPDATE), say("never")]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the question", asking(&chat));
    let question = question_of(&chat, cx);
    drop(chat);
    cx.update(|_| {});
    cx.run_until_parked();
    wait_for_worker_to_end(cx, &provider);
    assert_eq!(question.answer(), Some(Answer::Stopped));
    assert_eq!(rig.session.run_count(), 0);
}

#[gpui::test]
fn quitting_ends_the_question_and_the_worker_comes_back(cx: &mut TestAppContext) {
    let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, false), chat(cx));
    let provider = Scripted::new(vec![write_call(UPDATE), say("never")]);
    send(&chat, cx, &rig, &provider, "cancel it");
    wait(cx, "the question", asking(&chat));
    let question = question_of(&chat, cx);
    chat.update(cx, |chat, _| chat.cancel_for_quit());
    assert_eq!(question.answer(), Some(Answer::Stopped));
    wait(cx, "the run to end", idle(&chat));
    assert_eq!(rig.session.run_count(), 0);
    wait_for_worker_to_end(cx, &provider);
}

#[test]
fn a_second_question_while_one_waits_is_not_shown_and_is_not_approved() {
    use super::model::{Transcript, Update};
    let mut transcript = Transcript::default();
    for id in ["a", "b"] {
        transcript.apply(Update::ToolStarted {
            id: id.into(),
            name: "execute_query".into(),
        });
    }
    let first = super::approval::tests::question("UPDATE a SET x = 1");
    let second = super::approval::tests::question("UPDATE b SET x = 1");
    transcript.apply(Update::Ask(first.clone()));
    transcript.apply(Update::Ask(second.clone()));
    assert!(first.is_waiting());
    assert_eq!(
        second.answer(),
        Some(Answer::Stopped),
        "refused, not queued"
    );
    // The question sits at the call that runs first, not at the last one started.
    let asked: Vec<bool> = steps(transcript.items())
        .iter()
        .map(|step| step.question.is_some())
        .collect();
    assert_eq!(asked, [true, false]);
    // A result for that step says what became of the question, in its own words.
    transcript.apply(Update::ToolResult {
        id: "a".into(),
        name: "execute_query".into(),
        is_error: true,
        summary: "agent.cancelled: Cancelled".into(),
    });
    assert_eq!(steps(transcript.items())[0].result, "not run: stopped");
}

#[gpui::test]
fn dropping_the_app_while_a_question_waits_declines_it_and_the_worker_comes_back(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let app = app(cx);
    let rig = rig_with(AgentAccess::ReadWrite, false, false);
    no_agent_connections_to_drop_on_the_release_thread(&app, cx);
    let provider = Scripted::new(vec![write_call(UPDATE), say("never")]);
    let chat = app.update(cx, |app, cx| {
        app.assistant.set_layer_for_test(Arc::clone(&rig.layer));
        let shared: Arc<dyn ChatProvider> = provider.clone();
        app.assistant.provider_override = Some(shared);
        app.assistant
            .set_key_state(AiProvider::Anthropic, KeyState::Set);
        let mut profile = read_profile();
        profile.overrides.set(
            engine_connect::profile::AGENT_ACCESS,
            engine_connect::profile::OverrideValue::Text("read_write".into()),
        );
        app.connection_policy.set_profile(CONN, profile);
        let chat = app.assistant_chat(cx);
        chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
        app.assistant_sync(cx);
        assert_eq!(
            app.assistant_send("cancel it".into(), cx),
            SendOutcome::Started
        );
        chat
    });
    wait(cx, "the question", asking(&chat));
    let question = question_of(&chat, cx);
    assert!(app.read_with(cx, |app, _| app.assistant.asking));
    drop(chat);
    // The window goes away; no Quit ran.
    drop(app);
    cx.update(|_| {});
    wait_for_worker_to_end(cx, &provider);
    assert_eq!(question.answer(), Some(Answer::Stopped));
    assert_eq!(rig.session.run_count(), 0);
}

#[gpui::test]
fn a_write_for_another_connection_than_the_chats_is_declined_with_a_reason_the_model_can_read(
    cx: &mut TestAppContext,
) {
    const OTHER: &str = "c0ffee00-0000-4000-8000-000000000002";
    let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
    let other = FakeSession::new(ConnectionKind::PostgreSql).arc();
    let catalog = FakeCatalog::new(vec![
        connection(
            CONN,
            ConnectionKind::PostgreSql,
            Some(AgentAccess::ReadWrite),
        ),
        connection(
            OTHER,
            ConnectionKind::PostgreSql,
            Some(AgentAccess::ReadWrite),
        ),
    ]);
    let sessions = FakeProvider::new()
        .with_session(CONN, Arc::clone(&session))
        .with_session(OTHER, Arc::clone(&other));
    let layer = Arc::new(
        ToolLayer::new(catalog, sessions.clone(), FRONT_END).with_approver(AppApprover::new()),
    );
    let rig = Rig {
        layer,
        session,
        sessions,
        audit: None,
    };
    let chat = chat(cx);
    // The model opened the other connection's session itself.
    let opened = rig
        .layer
        .call("open_session", json!({"connection_id": OTHER}));
    assert!(!opened.is_error(), "{:?}", opened.to_json());
    let provider = Scripted::new(vec![
        call(
            "t1",
            "execute_query",
            json!({"connection_id": OTHER, "statement": UPDATE}),
        ),
        say("That is another connection."),
    ]);
    send(&chat, cx, &rig, &provider, "change the other database");
    wait(cx, "the end of the run", idle(&chat));
    let all = items(&chat, cx);
    let step = steps(&all)[0];
    assert!(step.question.is_none(), "no card for another connection");
    assert_eq!(step.state, StepState::Error);
    assert!(
        step.result.contains("another connection"),
        "the model reads why: {}",
        step.result
    );
    assert_eq!(other.run_count(), 0);
    assert_eq!(rig.session.run_count(), 0);
}

fn ask_of(statement: &str) -> Item {
    Item::Update(super::model::Update::Ask(super::approval::tests::question(
        statement,
    )))
}

#[gpui::test]
fn a_question_that_arrives_after_a_stop_or_a_lowered_access_is_declined_at_once(
    cx: &mut TestAppContext,
) {
    for lowered in [false, true] {
        let (rig, chat) = (rig_with(AgentAccess::ReadWrite, false, false), chat(cx));
        chat.update(cx, |chat, cx| {
            chat.set_status(ready(AgentAccess::ReadWrite), cx)
        });
        let (_hold, gate) = channel::<()>();
        let provider = Scripted::new(vec![Step::Wait {
            text: "partial".into(),
            gate,
        }]);
        send(&chat, cx, &rig, &provider, "long");
        wait(cx, "the first words", |cx| {
            items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Assistant(_)))
        });
        if lowered {
            chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
        } else {
            chat.update(cx, |chat, cx| chat.stop(cx));
        }
        // The worker had already put the question in the queue when the stop landed.
        let late = super::approval::tests::question("UPDATE late SET a = 1 WHERE id = 1");
        chat.update(cx, |chat, cx| {
            chat.apply(
                vec![
                    Item::Update(super::model::Update::ToolStarted {
                        id: "x".into(),
                        name: "execute_query".into(),
                    }),
                    Item::Update(super::model::Update::Ask(late.clone())),
                ],
                cx,
            )
        });
        assert_eq!(late.answer(), Some(Answer::Stopped), "lowered: {lowered}");
        assert!(!chat.read_with(cx, |chat, _| chat.is_asking()));
        wait(cx, "the run to end", idle(&chat));
        wait_for_worker_to_end(cx, &provider);
    }
}

#[gpui::test]
fn an_answer_for_a_card_that_is_not_the_waiting_question_is_ignored(cx: &mut TestAppContext) {
    let chat = chat(cx);
    let (first, second) = (
        Arc::new(super::approval::tests::question(
            "UPDATE a SET x = 1 WHERE id = 1",
        )),
        Arc::new(super::approval::tests::question(
            "UPDATE b SET x = 1 WHERE id = 1",
        )),
    );
    chat.update(cx, |chat, cx| {
        chat.apply(
            vec![
                Item::Update(super::model::Update::ToolStarted {
                    id: "a".into(),
                    name: "execute_query".into(),
                }),
                ask_of("UPDATE a SET x = 1 WHERE id = 1"),
            ],
            cx,
        )
    });
    let waiting = chat
        .read_with(cx, |chat, _| chat.waiting())
        .expect("it waits");
    cx.executor().advance_clock(super::approval::APPROVAL_DELAY);
    cx.run_until_parked();
    // The card of another question (the one that was there a moment ago) answers nothing.
    for stale in [&first, &second] {
        stale.arm();
        assert!(!chat.update(cx, |chat, cx| chat.answer_question(stale, true, cx)));
    }
    assert!(waiting.is_waiting());
    assert!(chat.update(cx, |chat, cx| chat.answer_question(&waiting, true, cx)));
    assert_eq!(waiting.answer(), Some(Answer::Approved));
}

#[gpui::test]
fn an_approval_counts_from_exactly_700_ms_on_the_executors_clock(cx: &mut TestAppContext) {
    let chat = chat(cx);
    chat.update(cx, |chat, cx| {
        chat.apply(
            vec![
                Item::Update(super::model::Update::ToolStarted {
                    id: "a".into(),
                    name: "execute_query".into(),
                }),
                ask_of("UPDATE a SET x = 1 WHERE id = 1"),
            ],
            cx,
        )
    });
    let waiting = chat
        .read_with(cx, |chat, _| chat.waiting())
        .expect("it waits");
    cx.executor()
        .advance_clock(super::approval::APPROVAL_DELAY - Duration::from_millis(1));
    cx.run_until_parked();
    assert!(
        !chat.update(cx, |chat, cx| chat.answer_question(&waiting, true, cx)),
        "699 ms: refused"
    );
    assert!(waiting.is_waiting());
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    assert!(
        chat.update(cx, |chat, cx| chat.answer_question(&waiting, true, cx)),
        "700 ms: accepted"
    );
    // The delay starts over when the card is in front of the user again.
    let again = chat.update(cx, |chat, cx| {
        chat.apply(
            vec![
                Item::Update(super::model::Update::ToolStarted {
                    id: "b".into(),
                    name: "execute_query".into(),
                }),
                ask_of("UPDATE b SET x = 1 WHERE id = 1"),
            ],
            cx,
        );
        chat.waiting()
    });
    let again = again.expect("the next one waits");
    cx.executor().advance_clock(super::approval::APPROVAL_DELAY);
    cx.run_until_parked();
    chat.update(cx, |chat, cx| chat.restart_arming(cx));
    assert!(!chat.update(cx, |chat, cx| chat.answer_question(&again, true, cx)));
    cx.executor().advance_clock(super::approval::APPROVAL_DELAY);
    cx.run_until_parked();
    assert!(chat.update(cx, |chat, cx| chat.answer_question(&again, true, cx)));
}

// ---- conversations kept on this computer -----------------------------------------------------------

mod kept {
    use super::*;
    use crate::app::assistant::store::{self, ConversationStore};

    /// A store in a temporary folder (never the user's).
    fn temp_store() -> (tempfile::TempDir, Arc<ConversationStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ConversationStore::new(dir.path().join("conversations")));
        (dir, store)
    }

    impl Rig {
        fn keeping(&self, provider: &Arc<Scripted>, store: &Arc<ConversationStore>) -> RunSetup {
            RunSetup {
                persist: Some(Arc::clone(store)),
                ..self.setup(provider)
            }
        }
    }

    fn send_keeping(
        chat: &Entity<ChatSession>,
        cx: &mut TestAppContext,
        rig: &Rig,
        provider: &Arc<Scripted>,
        store: &Arc<ConversationStore>,
        text: &str,
    ) -> SendOutcome {
        let setup = rig.keeping(provider, store);
        chat.update(cx, |chat, cx| chat.send(text.to_owned(), setup, cx))
    }

    /// Runs the scheduler until `connection` has `count` stored conversations (the write runs on a
    /// background thread).
    fn wait_kept(cx: &mut TestAppContext, store: &Arc<ConversationStore>, count: usize) {
        let store = Arc::clone(store);
        wait(cx, "the file", move |_| {
            store
                .list(CONN)
                .is_ok_and(|listing| listing.entries.len() == count)
        });
    }

    /// The updated time of the only stored conversation once it is `at_least`.
    fn wait_updated(cx: &mut TestAppContext, store: &Arc<ConversationStore>, messages: usize) {
        let store = Arc::clone(store);
        wait(cx, "the rewritten file", move |_| {
            store
                .list(CONN)
                .ok()
                .and_then(|listing| listing.entries.first().map(|e| e.header.id.clone()))
                .and_then(|id| store.load(CONN, &id).ok())
                .is_some_and(|loaded| loaded.conversation.messages().len() == messages)
        });
    }

    #[gpui::test]
    fn a_finished_run_is_written_once_with_its_header_and_not_while_it_streams(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let (release, gate) = channel::<()>();
        let provider = Scripted::new(vec![Step::Wait {
            text: "Working on it".into(),
            gate,
        }]);
        assert_eq!(
            send_keeping(&chat, cx, &rig, &provider, &store, "  how many\norders?  "),
            SendOutcome::Started
        );
        wait(cx, "the text", |cx| {
            items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Assistant(_)))
        });
        // Mid-answer: nothing on disk.
        assert!(!store.root().exists(), "no write per delta");
        release.send(()).unwrap();
        wait(cx, "the end", idle(&chat));
        wait_kept(cx, &store, 1);

        let listing = store.list(CONN).unwrap();
        assert!(listing.problems.is_empty());
        let header = &listing.entries[0].header;
        assert_eq!(
            header.title, "how many orders?",
            "the first message on one line"
        );
        assert_eq!(header.access, AgentAccess::Read);
        assert_eq!(
            (header.provider.as_str(), header.model.as_str()),
            ("anthropic", "claude-test")
        );
        assert_eq!(header.format, store::FORMAT);
        assert_eq!(
            chat.read_with(cx, |chat, _| chat.conversation_id().map(str::to_owned)),
            Some(header.id.clone())
        );
        let loaded = store.load(CONN, &header.id).unwrap();
        assert_eq!(loaded.conversation.messages().len(), 2);
        assert_eq!(
            loaded
                .conversation
                .scope()
                .map(|s| s.connection_id.as_str()),
            Some(CONN)
        );
    }

    #[gpui::test]
    fn a_chat_that_is_not_kept_writes_nothing(cx: &mut TestAppContext) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let provider = Scripted::new(vec![say("Hello")]);
        // The run is given no store: "Keep conversations on this computer" is off.
        send(&chat, cx, &rig, &provider, "hi");
        wait(cx, "the answer", idle(&chat));
        assert_eq!(chat.read_with(cx, |chat, _| chat.writes_started()), 0);
        assert!(!store.root().exists());
    }

    /// The request a reopened conversation sends is the one the live chat would have sent.
    #[gpui::test]
    fn the_next_request_after_reopening_is_the_one_the_open_chat_sends(cx: &mut TestAppContext) {
        let rig = rig();
        let (_dir, store) = temp_store();
        let script = || {
            vec![
                Step::Say {
                    reasoning: vec!["Count first.".into()],
                    text: vec![],
                    tools: vec![(
                        "t1",
                        "execute_query",
                        json!({"connection_id": CONN, "statement": "SELECT 1"}),
                    )],
                },
                say("One row."),
                say("Anything else is fine."),
            ]
        };
        // The chat that never closes.
        let live = chat(cx);
        let live_provider = Scripted::new(script());
        send_keeping(&live, cx, &rig, &live_provider, &store, "count the rows");
        wait(cx, "the first answer", idle(&live));
        wait_kept(cx, &store, 1);
        send_keeping(&live, cx, &rig, &live_provider, &store, "and then?");
        wait(cx, "the second answer", idle(&live));

        // The chat reopened from the file written after the first answer.
        let id = store.list(CONN).unwrap().entries[0].header.id.clone();
        wait_updated(cx, &store, 6);
        let reopened = cx.new(|_| ChatSession::new(None, ""));
        // The file now holds both turns; rebuild the state after the first one instead.
        let first_only = {
            let loaded = store.load(CONN, &id).unwrap();
            let mut json = serde_json::to_value(&loaded.conversation).unwrap();
            json["messages"].as_array_mut().unwrap().truncate(4);
            serde_json::from_value::<engine_ai::Conversation>(json).unwrap()
        };
        let header = store.load(CONN, &id).unwrap().header;
        let reopened_provider = Scripted::new(script().into_iter().skip(2).collect());
        reopened.update(cx, |chat, cx| {
            chat.open_stored(
                Opened::new(store::Loaded {
                    header,
                    conversation: first_only,
                }),
                "shop",
                cx,
            );
            chat.set_status(ready(AgentAccess::Read), cx);
        });
        let setup = rig.setup(&reopened_provider);
        reopened.update(cx, |chat, cx| chat.send("and then?".to_owned(), setup, cx));
        wait(cx, "the reopened answer", idle(&reopened));

        let live_requests = live_provider.request_text.lock().unwrap().clone();
        let reopened_requests = reopened_provider.request_text.lock().unwrap().clone();
        assert_eq!(live_requests.len(), 3, "tool round, answer, second answer");
        assert_eq!(reopened_requests.len(), 1);
        assert_eq!(
            reopened_requests.last(),
            live_requests.last(),
            "a reopened conversation sends the same request"
        );
    }

    #[gpui::test]
    fn a_stored_conversation_goes_on_with_the_earlier_turns_unchanged(cx: &mut TestAppContext) {
        let rig = rig();
        let (_dir, store) = temp_store();
        let first = chat(cx);
        let provider = Scripted::new(vec![say("It is 42.")]);
        send_keeping(&first, cx, &rig, &provider, &store, "what is the answer?");
        wait(cx, "the answer", idle(&first));
        wait_kept(cx, &store, 1);
        let id = store.list(CONN).unwrap().entries[0].header.id.clone();
        let before = store.load(CONN, &id).unwrap();

        // A new chat opens it and goes on.
        let second = cx.new(|_| ChatSession::new(None, ""));
        second.update(cx, |chat, cx| {
            chat.open_stored(Opened::new(store.load(CONN, &id).unwrap()), "shop", cx);
            chat.set_status(ready(AgentAccess::Read), cx);
        });
        assert_eq!(
            items(&second, cx),
            vec![
                ChatItem::User("what is the answer?".into()),
                ChatItem::Assistant("It is 42.".into())
            ],
            "the display is rebuilt from the conversation"
        );
        let provider = Scripted::new(vec![say("Still 42.")]);
        send_keeping(&second, cx, &rig, &provider, &store, "really?");
        wait(cx, "the second answer", idle(&second));
        let requests = provider.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            &requests[0][..before.conversation.messages().len()],
            before.conversation.messages(),
            "the earlier turns are sent as they were"
        );
        // The same file, rewritten: no second conversation.
        wait_updated(cx, &store, 4);
        assert_eq!(store.list(CONN).unwrap().entries.len(), 1);
        assert_eq!(store.list(CONN).unwrap().entries[0].header.id, id);
        assert_eq!(
            store.list(CONN).unwrap().entries[0].header.created,
            before.header.created
        );
    }

    #[gpui::test]
    fn a_conversation_that_ran_higher_is_read_only_until_the_level_is_back(
        cx: &mut TestAppContext,
    ) {
        let rig = rig();
        let (_dir, store) = temp_store();
        let first = chat(cx);
        first.update(cx, |chat, cx| {
            chat.set_status(ready(AgentAccess::ReadWrite), cx)
        });
        let provider = Scripted::new(vec![say("Done.")]);
        send_keeping(&first, cx, &rig, &provider, &store, "write something");
        wait(cx, "the answer", idle(&first));
        wait_kept(cx, &store, 1);
        let id = store.list(CONN).unwrap().entries[0].header.id.clone();
        assert_eq!(
            store.list(CONN).unwrap().entries[0].header.access,
            AgentAccess::ReadWrite
        );

        let second = cx.new(|_| ChatSession::new(None, ""));
        second.update(cx, |chat, cx| {
            chat.open_stored(Opened::new(store.load(CONN, &id).unwrap()), "shop", cx);
            chat.set_status(ready(AgentAccess::Read), cx);
        });
        let status = second.read_with(cx, |chat, _| chat.status().clone());
        assert_eq!(status.readiness, Readiness::ReadOnly);
        assert!(second.read_with(cx, |chat, _| chat.is_read_only()));
        let provider = Scripted::new(vec![say("never")]);
        assert_eq!(
            send_keeping(&second, cx, &rig, &provider, &store, "more"),
            SendOutcome::NotReady(Readiness::ReadOnly)
        );
        assert_eq!(provider.calls(), 0);
        // Lowering it further neither adds a notice nor ends anything else.
        let before = items(&second, cx).len();
        second.update(cx, |chat, cx| {
            chat.set_status(ready(AgentAccess::SchemaOnly), cx)
        });
        assert_eq!(items(&second, cx).len(), before);
        // The level comes back: it can be continued.
        second.update(cx, |chat, cx| {
            chat.set_status(ready(AgentAccess::ReadWrite), cx)
        });
        assert_eq!(
            second.read_with(cx, |chat, _| chat.status().readiness.clone()),
            Readiness::Ready
        );
        let provider = Scripted::new(vec![say("ok")]);
        assert_eq!(
            send_keeping(&second, cx, &rig, &provider, &store, "more"),
            SendOutcome::Started
        );
        wait(cx, "the answer", idle(&second));
    }

    #[gpui::test]
    fn a_conversation_over_the_limit_is_not_written_and_the_chat_says_so_once(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let small = Scripted::new(vec![say("Small.")]);
        send_keeping(&chat, cx, &rig, &small, &store, "first");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 1);
        let id = store.list(CONN).unwrap().entries[0].header.id.clone();

        let big = Scripted::new(vec![
            say(&"x".repeat(store::MAX_FILE_BYTES + 1024)),
            say(&"y".repeat(64)),
        ]);
        send_keeping(&chat, cx, &rig, &big, &store, "second");
        wait(cx, "the big answer", idle(&chat));
        let notices = |cx: &TestAppContext| {
            items(&chat, cx)
                .iter()
                .filter(|item| matches!(item, ChatItem::Notice(ChatNotice::TooLargeToKeep)))
                .count()
        };
        assert_eq!(notices(cx), 1, "said once");
        send_keeping(&chat, cx, &rig, &big, &store, "third");
        wait(cx, "the next answer", idle(&chat));
        assert_eq!(notices(cx), 1, "not said again");
        // The last file that fitted is still what is on disk (no write was handed over since).
        assert_eq!(chat.read_with(cx, |chat, _| chat.writes_started()), 1);
        let loaded = store.load(CONN, &id).unwrap();
        assert_eq!(loaded.conversation.messages().len(), 2);
        // A new chat is a new conversation and is kept again.
        chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
        chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
        let small = Scripted::new(vec![say("Fresh.")]);
        send_keeping(&chat, cx, &rig, &small, &store, "fresh start");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 2);
    }

    /// Runs until the chat has finished a run and its `keep` decided (no sleeping: a negative
    /// claim about the files is made after this signal, not after a pause).
    fn finished_runs(chat: &Entity<ChatSession>, cx: &TestAppContext) -> &'static str {
        chat.read_with(cx, |chat, _| chat.last_saved())
    }

    #[gpui::test]
    fn a_retry_raises_the_stored_level_and_a_conversation_reopened_at_schema_is_read_only(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig_with(AgentAccess::Read, false, false), chat(cx));
        chat.update(cx, |chat, cx| {
            chat.set_status(ready(AgentAccess::SchemaOnly), cx)
        });
        let (_dir, store) = temp_store();
        // At schema level a table is listed, then the model's next turn fails.
        let provider = Scripted::new(vec![
            call("t1", "list_tables", json!({"connection_id": CONN})),
            Step::Fail(ProviderError::Overloaded(failure("busy"))),
        ]);
        send_keeping(&chat, cx, &rig, &provider, &store, "what tables are there?");
        wait(cx, "the failed run", idle(&chat));
        wait_kept(cx, &store, 1);
        let id = store.list(CONN).unwrap().entries[0].header.id.clone();
        assert_eq!(
            store.load(CONN, &id).unwrap().header.access,
            AgentAccess::SchemaOnly
        );

        // The connection is raised to read, and Retry goes on from the tool result.
        chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
        let provider = Scripted::new(vec![say("Two tables.")]);
        let setup = rig.keeping(&provider, &store);
        assert!(chat.update(cx, |chat, cx| chat.retry(setup, cx)));
        wait(cx, "the retried run", idle(&chat));
        wait_updated(cx, &store, 4);
        assert_eq!(
            store.load(CONN, &id).unwrap().header.access,
            AgentAccess::Read,
            "a retry runs at the level the connection has now"
        );

        // Reopened while the connection is back at schema: it holds data schema may not send.
        let reopened = cx.new(|_| ChatSession::new(None, ""));
        reopened.update(cx, |chat, cx| {
            chat.open_stored(Opened::new(store.load(CONN, &id).unwrap()), "shop", cx);
            chat.set_status(ready(AgentAccess::SchemaOnly), cx);
        });
        assert_eq!(
            reopened.read_with(cx, |chat, _| chat.status().readiness.clone()),
            Readiness::ReadOnly
        );
    }

    #[gpui::test]
    fn a_level_raised_while_a_run_goes_is_the_level_the_file_says(cx: &mut TestAppContext) {
        let (rig, chat) = (rig(), chat(cx));
        chat.update(cx, |chat, cx| {
            chat.set_status(ready(AgentAccess::SchemaOnly), cx)
        });
        let (_dir, store) = temp_store();
        let (release, gate) = channel::<()>();
        let provider = Scripted::new(vec![Step::Wait {
            text: "Working".into(),
            gate,
        }]);
        send_keeping(&chat, cx, &rig, &provider, &store, "slow");
        wait(cx, "the text", |cx| {
            items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Assistant(_)))
        });
        chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
        release.send(()).unwrap();
        wait(cx, "the end", idle(&chat));
        wait_kept(cx, &store, 1);
        assert_eq!(
            store.list(CONN).unwrap().entries[0].header.access,
            AgentAccess::Read
        );
    }

    #[gpui::test]
    fn a_header_that_says_less_than_the_file_holds_is_not_trusted(cx: &mut TestAppContext) {
        let (_dir, store) = temp_store();
        let conversation: engine_ai::Conversation = serde_json::from_value(json!({
            "scope": {"connection_id": CONN}, "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "execute_query", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                    "content": "{\"columns\":[{\"name\":\"a\",\"type\":\"int\"}],\"rows\":[[\"1\"]],\"row_count\":1}"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "One row."}]}
            ]
        })).unwrap();
        let header = store::Header {
            format: store::FORMAT,
            id: "planted".into(),
            title: "q".into(),
            created: 1,
            updated: 1,
            provider: "anthropic".into(),
            model: "m".into(),
            access: AgentAccess::SchemaOnly,
            seq: 0,
        };
        store
            .write(CONN, &store::encode(&header, &conversation).unwrap())
            .unwrap();
        let chat = cx.new(|_| ChatSession::new(None, ""));
        chat.update(cx, |chat, cx| {
            chat.open_stored(
                Opened::new(store.load(CONN, "planted").unwrap()),
                "shop",
                cx,
            );
            chat.set_status(ready(AgentAccess::SchemaOnly), cx);
        });
        assert_eq!(
            chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
            Readiness::ReadOnly,
            "the rows in it were read at read level, whatever the header says"
        );
    }

    #[gpui::test]
    fn a_refusal_that_is_no_race_is_said_once_and_not_on_every_run(cx: &mut TestAppContext) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let provider = Scripted::new(vec![say("one"), say("two"), say("three")]);
        send_keeping(&chat, cx, &rig, &provider, &store, "first");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 1);
        // The file says it has far more messages than this chat ever wrote (edited, or written
        // after a clock was set back by something that did not count): every later write is
        // refused.
        let id = store.list(CONN).unwrap().entries[0].header.id.clone();
        let path = store.root().join(CONN).join(format!("{id}.json"));
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"seq\":2", "\"seq\":1000", 1)).unwrap();
        for text in ["second", "third"] {
            send_keeping(&chat, cx, &rig, &provider, &store, text);
            wait(cx, "the answer", idle(&chat));
            let before = chat.read_with(cx, |chat, _| chat.writes_started());
            wait(cx, "the refused write to be said", |cx| {
                chat.read_with(cx, |chat, _| chat.writes_started()) == before
                    && (text != "second"
                        || items(&chat, cx)
                            .iter()
                            .any(|item| matches!(item, ChatItem::Notice(ChatNotice::Other(_)))))
            });
        }
        let said = items(&chat, cx)
            .iter()
            .filter(|item| matches!(item, ChatItem::Notice(ChatNotice::Other(text)) if text.contains("could not be kept")))
            .count();
        assert_eq!(said, 1, "once per chat, in the transcript");
    }

    #[gpui::test]
    fn switching_the_setting_off_under_a_run_in_flight_keeps_that_run_off_disk(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let (release, gate) = channel::<()>();
        let provider = Scripted::new(vec![Step::Wait {
            text: "Working".into(),
            gate,
        }]);
        send_keeping(&chat, cx, &rig, &provider, &store, "slow");
        wait(cx, "the text", |cx| {
            items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Assistant(_)))
        });
        // The setting goes off (the app tells the chat) while the run goes.
        chat.update(cx, |chat, _| chat.set_keeping(false));
        release.send(()).unwrap();
        wait(cx, "the end", idle(&chat));
        assert_eq!(finished_runs(&chat, cx), "bytes", "the run made the bytes");
        assert_eq!(
            chat.read_with(cx, |chat, _| chat.writes_started()),
            0,
            "and none were written"
        );
        assert!(!store.root().exists());
        // Turned on again: the next run is kept (a first file now).
        chat.update(cx, |chat, _| chat.set_keeping(true));
        let more = Scripted::new(vec![say("again")]);
        send_keeping(&chat, cx, &rig, &more, &store, "and again");
        wait(cx, "the next answer", idle(&chat));
        wait_kept(cx, &store, 1);
    }

    #[gpui::test]
    fn a_chat_detached_from_its_file_writes_nothing_under_the_old_id_and_goes_on_under_a_new_one(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let first = Scripted::new(vec![say("one")]);
        send_keeping(&chat, cx, &rig, &first, &store, "the first question");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 1);
        let old = chat
            .read_with(cx, |chat, _| chat.conversation_id().map(str::to_owned))
            .unwrap();

        // A run in flight when the file is deleted.
        let (release, gate) = channel::<()>();
        let slow = Scripted::new(vec![Step::Wait {
            text: "Working".into(),
            gate,
        }]);
        send_keeping(&chat, cx, &rig, &slow, &store, "a second one");
        wait(cx, "the text", |cx| {
            items(&chat, cx)
                .iter()
                .filter(|i| matches!(i, ChatItem::Assistant(_)))
                .count()
                == 2
        });
        assert!(store.delete(CONN, &old).unwrap());
        assert!(chat.update(cx, |chat, cx| chat
            .detach("Deleted. It goes on as a new conversation.", cx)));
        let fresh = chat
            .read_with(cx, |chat, _| chat.conversation_id().map(str::to_owned))
            .unwrap();
        assert_ne!(fresh, old);
        release.send(()).unwrap();
        wait(cx, "the end", idle(&chat));
        assert_eq!(
            chat.read_with(cx, |chat, _| chat.writes_started()),
            1,
            "nothing was written for the run that ended after the delete"
        );
        assert!(
            store.list(CONN).unwrap().entries.is_empty(),
            "the old content did not come back"
        );

        // The next run is a new conversation, named by the first message of the chat.
        let next = Scripted::new(vec![say("three")]);
        send_keeping(&chat, cx, &rig, &next, &store, "a third one");
        wait(cx, "the next answer", idle(&chat));
        wait_kept(cx, &store, 1);
        let listing = store.list(CONN).unwrap();
        assert_eq!(listing.entries[0].header.id, fresh);
        assert_eq!(listing.entries[0].header.title, "the first question");
        assert!(items(&chat, cx).iter().any(|item| matches!(item, ChatItem::Notice(ChatNotice::Other(text)) if text.contains("Deleted"))));
    }

    #[gpui::test]
    fn a_write_that_began_before_a_delete_of_everything_is_refused_by_the_store(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        // Listening from the start: a `Saved` that arrived at any time would be seen.
        let events = record_events(&chat, cx);
        let (release, gate) = channel::<()>();
        let slow = Scripted::new(vec![
            Step::Wait {
                text: "Working".into(),
                gate,
            },
            say("again"),
        ]);
        send_keeping(&chat, cx, &rig, &slow, &store, "slow one");
        wait(cx, "the text", |cx| {
            items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Assistant(_)))
        });
        // "Delete all conversations" during the run; the chat itself is not told here, so only
        // the store stands between the run's file and the disk.
        store.delete_all().unwrap();
        release.send(()).unwrap();
        wait(cx, "the end", idle(&chat));
        assert_eq!(chat.read_with(cx, |chat, _| chat.writes_started()), 1);
        // The write was handed over and refused: wait for the refusal to be over, then look.
        wait(cx, "the write to end", |cx| {
            chat.read_with(cx, |chat, _| chat.writes_ended()) == 1
        });
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !matches!(event, ChatEvent::Saved)));
        assert!(
            store.list(CONN).unwrap().entries.is_empty(),
            "nothing reappeared"
        );
        // The chat knows it is not on disk (History does not show it, replacing it asks) ...
        assert!(!chat.read_with(cx, |chat, _| chat.is_kept()));
        assert!(chat.read_with(cx, |chat, _| chat.has_unsaved_messages()));
        // ... and its next run writes it.
        send_keeping(&chat, cx, &rig, &slow, &store, "and again");
        wait(cx, "the next answer", idle(&chat));
        wait_kept(cx, &store, 1);
        assert!(chat.read_with(cx, |chat, _| chat.is_kept()));
    }

    #[gpui::test]
    fn a_delete_of_another_connections_conversations_does_not_refuse_this_chats_write(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let events = record_events(&chat, cx);
        let (release, gate) = channel::<()>();
        let slow = Scripted::new(vec![Step::Wait {
            text: "Working".into(),
            gate,
        }]);
        send_keeping(&chat, cx, &rig, &slow, &store, "slow one");
        wait(cx, "the text", |cx| {
            items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Assistant(_)))
        });
        // Another connection's conversations are deleted while this run goes.
        store
            .delete_connection("c0ffee00-0000-4000-8000-0000000000aa")
            .unwrap();
        release.send(()).unwrap();
        wait(cx, "the end", idle(&chat));
        wait_kept(cx, &store, 1);
        wait(cx, "the write to end", |cx| {
            chat.read_with(cx, |chat, _| chat.writes_ended()) == 1
        });
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ChatEvent::Saved)));
        assert!(chat.read_with(cx, |chat, _| chat.is_kept()));
    }

    #[gpui::test]
    fn a_write_that_fails_after_the_chat_was_replaced_is_for_the_error_log_only(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let events = record_events(&chat, cx);
        let first = Scripted::new(vec![say("one")]);
        send(&chat, cx, &rig, &first, "the old chat");
        wait(cx, "the answer", idle(&chat));
        let old = chat
            .read_with(cx, |chat, _| chat.conversation_id().map(str::to_owned))
            .unwrap();
        // The chat is replaced; then the write of the old one ends in a failure.
        chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
        let failure = || Err(store::StoreError::Missing);
        chat.update(cx, |chat, cx| chat.written(&old, 2, failure(), cx));
        assert!(
            !items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Notice(_))),
            "nothing in the new chat's transcript"
        );
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ChatEvent::NotKept(_))));
        // The new chat's own once-flag is untouched: its own failure is still said, once.
        let second = Scripted::new(vec![say("two")]);
        send(&chat, cx, &rig, &second, "the new chat");
        wait(cx, "the answer", idle(&chat));
        let own = chat
            .read_with(cx, |chat, _| chat.conversation_id().map(str::to_owned))
            .unwrap();
        for _ in 0..2 {
            chat.update(cx, |chat, cx| chat.written(&own, 2, failure(), cx));
        }
        let said = items(&chat, cx)
            .iter()
            .filter(|item| matches!(item, ChatItem::Notice(ChatNotice::Other(text)) if text.contains("could not be kept")))
            .count();
        assert_eq!(said, 1);
    }

    #[gpui::test]
    fn a_chat_saved_once_and_then_continued_off_disk_still_asks_before_it_is_replaced(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let provider = Scripted::new(vec![say("one"), say("two"), say("three")]);
        send_keeping(&chat, cx, &rig, &provider, &store, "first");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 1);
        wait(cx, "the write to end", |cx| {
            chat.read_with(cx, |chat, _| chat.writes_ended()) == 1
        });
        assert!(!chat.read_with(cx, |chat, _| chat.has_unsaved_messages()));
        // The switch goes off and the chat goes on: what it holds now is on no disk.
        chat.update(cx, |chat, _| chat.set_keeping(false));
        send_keeping(&chat, cx, &rig, &provider, &store, "second");
        wait(cx, "the answer", idle(&chat));
        assert!(
            chat.read_with(cx, |chat, _| chat.is_kept()),
            "a file exists ..."
        );
        assert!(
            chat.read_with(cx, |chat, _| chat.has_unsaved_messages()),
            "... but it lacks the second turn"
        );
        // Switched on again, the next run writes everything.
        chat.update(cx, |chat, _| chat.set_keeping(true));
        send_keeping(&chat, cx, &rig, &provider, &store, "third");
        wait(cx, "the answer", idle(&chat));
        wait_updated(cx, &store, 6);
        wait(cx, "the write to end", |cx| {
            chat.read_with(cx, |chat, _| chat.writes_ended()) == 2
        });
        assert!(!chat.read_with(cx, |chat, _| chat.has_unsaved_messages()));
    }

    #[gpui::test]
    fn a_conversation_that_outgrew_the_limit_after_a_save_asks_before_it_is_replaced(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let provider = Scripted::new(vec![
            say("small"),
            say(&"x".repeat(store::MAX_FILE_BYTES + 1024)),
        ]);
        send_keeping(&chat, cx, &rig, &provider, &store, "first");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 1);
        send_keeping(&chat, cx, &rig, &provider, &store, "second");
        wait(cx, "the big answer", idle(&chat));
        assert_eq!(finished_runs(&chat, cx), "too_large");
        assert!(chat.read_with(cx, |chat, _| chat.has_unsaved_messages()));
    }

    #[gpui::test]
    fn a_chat_whose_file_was_deleted_is_written_again_only_after_a_new_message(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let provider = Scripted::new(vec![say("one"), say("two"), say("three")]);
        send_keeping(&chat, cx, &rig, &provider, &store, "the first question");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 1);
        let old = chat
            .read_with(cx, |chat, _| chat.conversation_id().map(str::to_owned))
            .unwrap();
        assert!(store.delete(CONN, &old).unwrap());
        assert!(chat.update(cx, |chat, cx| chat.detach("Deleted.", cx)));
        assert!(chat.read_with(cx, |chat, _| chat.has_unsaved_messages()));
        // The user continues: the conversation (all of it) is written, as a new one.
        send_keeping(&chat, cx, &rig, &provider, &store, "the second question");
        wait(cx, "the answer", idle(&chat));
        wait_kept(cx, &store, 1);
        let fresh = chat
            .read_with(cx, |chat, _| chat.conversation_id().map(str::to_owned))
            .unwrap();
        assert_ne!(fresh, old);
        let text = serde_json::to_string(&store.load(CONN, &fresh).unwrap().conversation).unwrap();
        assert!(text.contains("the first question") && text.contains("the second question"));
    }

    #[gpui::test]
    fn the_second_run_of_a_conversation_over_the_limit_does_not_serialise_it_again(
        cx: &mut TestAppContext,
    ) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let big = Scripted::new(vec![
            say(&"x".repeat(store::MAX_FILE_BYTES + 1024)),
            say("small"),
        ]);
        send_keeping(&chat, cx, &rig, &big, &store, "first");
        wait(cx, "the big answer", idle(&chat));
        assert_eq!(finished_runs(&chat, cx), "too_large", "found out once");
        send_keeping(&chat, cx, &rig, &big, &store, "second");
        wait(cx, "the next answer", idle(&chat));
        assert_eq!(
            finished_runs(&chat, cx),
            "skipped",
            "and not again: no 8 MiB encoded for nothing"
        );
    }

    #[gpui::test]
    fn a_stopped_run_still_leaves_a_valid_conversation_on_disk(cx: &mut TestAppContext) {
        let (rig, chat) = (rig(), chat(cx));
        let (_dir, store) = temp_store();
        let (_release, gate) = channel::<()>();
        let provider = Scripted::new(vec![Step::Wait {
            text: "Thinking out loud".into(),
            gate,
        }]);
        send_keeping(&chat, cx, &rig, &provider, &store, "slow question");
        wait(cx, "the text", |cx| {
            items(&chat, cx)
                .iter()
                .any(|item| matches!(item, ChatItem::Assistant(_)))
        });
        chat.update(cx, |chat, cx| chat.stop(cx));
        wait(cx, "the stop", idle(&chat));
        // The engine takes a first message back when its first turn did not complete: nothing to
        // keep, and nothing half-written.
        assert_eq!(finished_runs(&chat, cx), "skipped");
        assert_eq!(chat.read_with(cx, |chat, _| chat.writes_started()), 0);
        assert!(
            store
                .list(CONN)
                .map_or(true, |listing| listing.entries.is_empty()
                    && listing.problems.is_empty()),
            "an empty conversation is not written"
        );
    }

    #[gpui::test]
    fn a_drop_to_a_level_the_chat_ran_at_does_not_end_it_and_a_drop_below_does(
        cx: &mut TestAppContext,
    ) {
        let (_dir, store) = temp_store();
        let conversation: engine_ai::Conversation = serde_json::from_value(json!({
            "scope": {"connection_id": CONN}, "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "a"}]}
            ]
        }))
        .unwrap();
        let header = store::Header {
            format: store::FORMAT,
            id: "ran-at-read".into(),
            title: "q".into(),
            created: 1,
            updated: 1,
            provider: "anthropic".into(),
            model: "m".into(),
            access: AgentAccess::Read,
            seq: 0,
        };
        store
            .write(CONN, &store::encode(&header, &conversation).unwrap())
            .unwrap();
        let chat = cx.new(|_| ChatSession::new(None, ""));
        chat.update(cx, |chat, cx| {
            chat.open_stored(
                Opened::new(store.load(CONN, "ran-at-read").unwrap()),
                "shop",
                cx,
            );
            chat.set_status(ready(AgentAccess::ReadWrite), cx);
        });
        // Read-write down to read: the conversation ran at read, so it may go on.
        chat.update(cx, |chat, cx| chat.set_status(ready(AgentAccess::Read), cx));
        assert_eq!(
            chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
            Readiness::Ready
        );
        assert!(!chat.read_with(cx, |chat, _| chat.is_ended()));
        // Below what it ran at: it can no longer go on.
        chat.update(cx, |chat, cx| {
            chat.set_status(ready(AgentAccess::SchemaOnly), cx)
        });
        assert_ne!(
            chat.read_with(cx, |chat, _| chat.status().readiness.clone()),
            Readiness::Ready
        );
    }
}

// ---- the providers ------------------------------------------------------------------------------

/// A choice of provider in Settings: what is built, which address it asks, whose key it carries.
mod providers {
    use super::*;
    use crate::settings::AiProvider;
    use engine_ai::{ConfigError, HttpRequest, HttpResponse, HttpTransport, TransportError};

    /// Answers every request with a model list and remembers what was asked: the address and the
    /// headers (the key included, as the transport reads it).
    #[derive(Default)]
    struct Recording {
        seen: Mutex<Vec<Seen>>,
    }

    /// The address of a request and its headers (lower-cased names, values as sent).
    type Seen = (String, Vec<(String, String)>);

    impl Recording {
        fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl HttpTransport for Recording {
        fn send(
            &self,
            request: HttpRequest,
            _: &CancelToken,
        ) -> Result<HttpResponse, TransportError> {
            let headers = request
                .headers
                .iter()
                .map(|header| {
                    (
                        header.name.to_ascii_lowercase(),
                        header.value.expose().to_owned(),
                    )
                })
                .collect();
            self.seen.lock().unwrap().push((request.url, headers));
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: Box::new(std::io::Cursor::new(
                    br#"{"data":[{"id":"m1"}],"has_more":false}"#.to_vec(),
                )),
            })
        }
    }

    fn ui(transport: &Arc<Recording>, keys: &Arc<ApiKeyStore>) -> AssistantUi {
        AssistantUi::new(
            None,
            None,
            Arc::clone(keys),
            Arc::clone(transport) as Arc<dyn HttpTransport>,
            Arc::new(records::MemoryRecords::default()),
            None,
            Arc::new(NoSecrets),
        )
    }

    fn with_provider(provider: AiProvider) -> AiSettings {
        let mut ai = AiSettings::default();
        ai.provider = provider;
        ai
    }

    fn settings_for(provider: AiProvider) -> AiSettings {
        let mut ai = with_provider(provider);
        if provider == AiProvider::OpenAiCompatible {
            ai.selected_mut().base_url = "http://127.0.0.1:1234/v1".into();
        }
        ai
    }

    /// Everything the requests carried, as one text.
    fn carried(requests: &[Seen]) -> String {
        requests
            .iter()
            .flat_map(|(_, headers)| {
                headers
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn each_choice_builds_its_provider_with_its_address_and_its_own_key() {
        let expected = [
            (AiProvider::Anthropic, "https://api.anthropic.com/v1/models"),
            (AiProvider::OpenAi, "https://api.openai.com/v1/models"),
            (
                AiProvider::OpenAiCompatible,
                "http://127.0.0.1:1234/v1/models",
            ),
            (AiProvider::Ollama, "http://localhost:11434/v1/models"),
        ];
        for (provider, url) in expected {
            let keys = Arc::new(ApiKeyStore::memory());
            // Every provider has a key of its own saved; each request may carry only its own.
            for other in AiProvider::ALL {
                keys.for_provider(other)
                    .save(&format!("key[{}]", other.id()))
                    .unwrap();
            }
            let transport = Arc::new(Recording::default());
            let built = ui(&transport, &keys)
                .provider(&settings_for(provider))
                .expect("a usable address");
            built.list_models(&CancelToken::new()).unwrap();
            let requests = transport.requests();
            assert!(
                requests[0].0.starts_with(url),
                "{provider:?}: {} is not {url}",
                requests[0].0
            );
            let carried = carried(&requests);
            assert!(
                carried.contains(&format!("key[{}]", provider.id())),
                "{provider:?} sent no key:\n{carried}"
            );
            for other in AiProvider::ALL
                .into_iter()
                .filter(|other| *other != provider)
            {
                assert!(
                    !carried.contains(&format!("key[{}]", other.id())),
                    "{provider:?} sent the key of {other:?}:\n{carried}"
                );
            }
            // Anthropic's own header, the others' bearer token.
            let header = if provider == AiProvider::Anthropic {
                "x-api-key"
            } else {
                "authorization"
            };
            assert!(carried.contains(header), "{provider:?}:\n{carried}");
        }
    }

    #[test]
    fn the_key_of_one_provider_is_never_read_by_another() {
        // Anthropic's key is saved; the others have none.
        let keys = Arc::new(ApiKeyStore::memory());
        keys.save(KEY).unwrap();
        for provider in [
            AiProvider::OpenAi,
            AiProvider::OpenAiCompatible,
            AiProvider::Ollama,
        ] {
            assert_eq!(
                keys.for_provider(provider).is_set(),
                Ok(false),
                "{provider:?}"
            );
            let transport = Arc::new(Recording::default());
            let built = ui(&transport, &keys)
                .provider(&settings_for(provider))
                .unwrap();
            let outcome = built.list_models(&CancelToken::new());
            if provider.requires_key() {
                // OpenAI: nothing is sent at all without its own key.
                assert!(
                    matches!(
                        outcome,
                        Err(ProviderError::Key(engine_ai::KeyError::NotSet))
                    ),
                    "{outcome:?}"
                );
                assert!(transport.requests().is_empty());
            } else {
                // Ollama and a compatible server go without a key: no credential header, and
                // certainly not Anthropic's.
                assert!(outcome.is_ok(), "{outcome:?}");
                let carried = carried(&transport.requests());
                assert!(!carried.contains(KEY), "{provider:?}:\n{carried}");
                assert!(
                    !carried.contains("authorization") && !carried.contains("x-api-key"),
                    "{provider:?}:\n{carried}"
                );
            }
        }
        // And the other way round: OpenAI's key does not open Anthropic.
        let keys = Arc::new(ApiKeyStore::memory());
        keys.for_provider(AiProvider::OpenAi)
            .save("openai-key")
            .unwrap();
        let transport = Arc::new(Recording::default());
        let built = ui(&transport, &keys)
            .provider(&settings_for(AiProvider::Anthropic))
            .unwrap();
        assert!(matches!(
            built.list_models(&CancelToken::new()),
            Err(ProviderError::Key(engine_ai::KeyError::NotSet))
        ));
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn removing_one_providers_key_leaves_the_others() {
        let keys = Arc::new(ApiKeyStore::memory());
        for provider in AiProvider::ALL {
            keys.for_provider(provider).save("k").unwrap();
        }
        keys.for_provider(AiProvider::OpenAi).remove().unwrap();
        for provider in AiProvider::ALL {
            assert_eq!(
                keys.for_provider(provider).is_set(),
                Ok(provider != AiProvider::OpenAi),
                "{provider:?}"
            );
        }
        // `keys` itself is Anthropic's account.
        assert_eq!(keys.is_set(), Ok(true));
    }

    #[test]
    fn an_address_the_key_must_not_go_to_is_refused_for_every_provider() {
        let keys = Arc::new(ApiKeyStore::memory());
        let transport = Arc::new(Recording::default());
        let assistant = ui(&transport, &keys);
        for provider in AiProvider::ALL {
            for (address, why) in [
                ("http://api.example.com", ConfigError::InsecureBaseUrl),
                ("not an address", ConfigError::InvalidBaseUrl),
                ("https://u:p@example.com", ConfigError::BaseUrlHasUserinfo),
            ] {
                let mut ai = with_provider(provider);
                ai.selected_mut().base_url = address.into();
                assert_eq!(
                    assistant.provider(&ai).err(),
                    Some(why),
                    "{provider:?} {address}"
                );
            }
        }
        // A compatible server has no address until it is given one.
        assert_eq!(
            assistant
                .provider(&with_provider(AiProvider::OpenAiCompatible))
                .err(),
            Some(ConfigError::InvalidBaseUrl)
        );
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn the_models_of_each_provider_are_its_own() {
        let mut assistant = ui(
            &Arc::new(Recording::default()),
            &Arc::new(ApiKeyStore::memory()),
        );
        assert_eq!(assistant.models_for(AiProvider::Anthropic).len(), 4);
        assert!(assistant.models_for(AiProvider::Ollama).is_empty());
        // A list fetched for Ollama is not offered to the others.
        assistant
            .fetched_models
            .insert(AiProvider::Ollama, vec!["qwen3:8b".into()]);
        assert_eq!(assistant.models_for(AiProvider::Ollama), ["qwen3:8b"]);
        assert_eq!(assistant.models_for(AiProvider::Anthropic).len(), 4);
        assert!(assistant.models_for(AiProvider::OpenAi).is_empty());
    }

    /// The Settings the app runs with: `provider` selected, `model` chosen for it.
    fn choose(app: &Entity<AppRoot>, cx: &mut TestAppContext, provider: AiProvider, model: &str) {
        app.update(cx, |app, cx| {
            let mut settings = (*app.settings).clone();
            settings.ai.provider = provider;
            settings.ai.selected_mut().model = model.into();
            app.settings = Arc::new(settings);
            app.assistant_sync(cx);
        });
    }

    #[gpui::test]
    fn the_status_asks_for_the_key_only_where_one_is_needed_and_then_for_a_model(
        cx: &mut TestAppContext,
    ) {
        cx.dispatcher.allow_parking();
        let app = app(cx);
        let chat = app.update(cx, |app, cx| {
            app.connection_policy.set_profile(CONN, read_profile());
            let chat = app.assistant_chat(cx);
            chat.update(cx, |chat, cx| chat.new_chat(Some(scope()), "shop", cx));
            app.assistant_sync(cx);
            chat
        });
        let status =
            |cx: &TestAppContext| chat.read_with(cx, |chat, _| chat.status().readiness.clone());
        let settled = |chat: &Entity<ChatSession>, want: Readiness| {
            let chat = chat.clone();
            move |cx: &mut TestAppContext| {
                chat.read_with(cx, |chat, _| chat.status().readiness == want)
            }
        };

        // Anthropic as before: no key, no assistant.
        wait(
            cx,
            "the Anthropic check",
            settled(&chat, Readiness::NoApiKey),
        );

        // Ollama needs no key, only a model.
        choose(&app, cx, AiProvider::Ollama, "");
        wait(cx, "the Ollama check", settled(&chat, Readiness::NoModel));
        choose(&app, cx, AiProvider::Ollama, "qwen3:8b");
        assert_eq!(status(cx), Readiness::Ready, "no key, and it works");

        // OpenAI needs the key first (its model list cannot be asked without it), then a model.
        choose(&app, cx, AiProvider::OpenAi, "");
        wait(cx, "the OpenAI check", settled(&chat, Readiness::NoApiKey));
        app.update(cx, |app, cx| {
            app.assistant_save_key(AiProvider::OpenAi, "openai-key".into(), cx)
        });
        wait(cx, "the OpenAI key", settled(&chat, Readiness::NoModel));
        choose(&app, cx, AiProvider::OpenAi, "gpt-x");
        assert_eq!(status(cx), Readiness::Ready);

        // The OpenAI key did not make Anthropic ready, and Anthropic's own does not touch OpenAI's.
        choose(&app, cx, AiProvider::Anthropic, engine_ai::DEFAULT_MODEL);
        assert_eq!(status(cx), Readiness::NoApiKey);
        app.update(cx, |app, cx| {
            app.assistant_save_key(AiProvider::Anthropic, KEY.into(), cx)
        });
        wait(cx, "the Anthropic key", settled(&chat, Readiness::Ready));
        app.update(cx, |app, cx| {
            app.assistant_remove_key(AiProvider::Anthropic, cx)
        });
        wait(cx, "the removal", settled(&chat, Readiness::NoApiKey));
        choose(&app, cx, AiProvider::OpenAi, "gpt-x");
        assert_eq!(status(cx), Readiness::Ready, "OpenAI keeps its key");
    }

    #[gpui::test]
    fn a_conversation_goes_on_when_the_provider_changes(cx: &mut TestAppContext) {
        let (rig, chat) = (rig(), chat(cx));
        let first = Scripted::new(vec![say("Hello from the first.")]);
        send(&chat, cx, &rig, &first, "hi");
        wait(cx, "the first answer", idle(&chat));
        // The next run is another provider's, with its own model; the history goes with it.
        let second = Scripted::new(vec![say("Hello from the second.")]);
        let setup = RunSetup {
            provider_id: "ollama",
            model: "qwen3:8b".into(),
            ..rig.setup(&second)
        };
        chat.update(cx, |chat, cx| chat.send("again".into(), setup, cx));
        wait(cx, "the second answer", idle(&chat));
        let requests = second.requests.lock().unwrap();
        assert!(
            requests[0].len() >= 3,
            "the first exchange was sent along: {} messages",
            requests[0].len()
        );
        drop(requests);
        let all = items(&chat, cx);
        assert!(
            matches!(all.last(), Some(ChatItem::Assistant(text)) if text == "Hello from the second.")
        );
    }

    #[test]
    fn the_panel_names_the_provider_it_asks_a_key_for() {
        use super::super::panel::settings_reason;
        assert_eq!(
            settings_reason(&Readiness::NoApiKey, "OpenAI"),
            "Add your OpenAI API key in Settings \u{2192} AI"
        );
        assert_eq!(
            settings_reason(&Readiness::NoModel, "Ollama"),
            "Choose a model in Settings \u{2192} AI"
        );
        assert!(
            settings_reason(&Readiness::KeyUnavailable("locked".into()), "Anthropic")
                .contains("Add your Anthropic API key")
        );
    }
}
