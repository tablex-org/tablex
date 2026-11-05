//! Test doubles: a scripted [`HttpTransport`] (bodies that arrive in chosen chunks, break, or block
//! until cancelled), a sleeper that records instead of sleeping, an SSE fixture builder, and a
//! [`Harness`] that runs the assistant over a real tool layer with fakes and checks, after every
//! run, that the fake API key appears nowhere it must not.

use crate::anthropic::{AnthropicConfig, AnthropicProvider, SleepOutcome, Sleeper};
use crate::assistant::{Assistant, AssistantConfig, AssistantError, AssistantEvent, RunSummary};
use crate::conversation::{Conversation, Scope};
use crate::key::ApiKey;
use crate::provider::{KeyError, KeySource};
use crate::transport::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use engine_agent::table::{ColumnDescription, TableDescription};
use engine_agent::testing::{connection, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{AgentAccess, CancelToken, ConnectionKind, ToolLayer};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub const FAKE_KEY: &str = "sk-ant-api03-ZZSECRETZZ-0123456789abcdef";

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct FixedKey;

impl KeySource for FixedKey {
    fn api_key(&self) -> Result<ApiKey, KeyError> {
        Ok(ApiKey::new(FAKE_KEY))
    }
}

// ---------------------------------------------------------------------------- scripted transport

pub enum End {
    Eof,
    /// The connection resets after the data.
    Break,
    /// Blocks until the token is cancelled; sends on the channel (if any) once it is blocked.
    BlockUntilCancel(Option<Sender<()>>),
}

enum Item {
    Data(Vec<u8>),
    Hook(Box<dyn FnOnce() + Send>),
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    items: Vec<Item>,
    end: End,
}

impl Reply {
    pub fn ok(body: impl AsRef<[u8]>) -> Self {
        Self::with_status(200, body)
    }

    pub fn with_status(status: u16, body: impl AsRef<[u8]>) -> Self {
        Self {
            status,
            headers: vec![("Request-Id".into(), "req_scripted".into())],
            items: vec![Item::Data(body.as_ref().to_vec())],
            end: End::Eof,
        }
    }

    /// An API error answer.
    pub fn error(status: u16, kind: &str, message: &str) -> Self {
        let body = json!({"type": "error", "error": {"type": kind, "message": message},
                          "request_id": "req_in_body"});
        Self::with_status(status, body.to_string())
    }

    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Splits the data into pieces of `size` bytes.
    #[must_use]
    pub fn chunked(mut self, size: usize) -> Self {
        let data: Vec<u8> = self
            .items
            .drain(..)
            .flat_map(|item| match item {
                Item::Data(bytes) => bytes,
                Item::Hook(_) => Vec::new(),
            })
            .collect();
        self.items = data
            .chunks(size)
            .map(|piece| Item::Data(piece.to_vec()))
            .collect();
        self
    }

    /// Splits the data into two pieces at `at`.
    #[must_use]
    pub fn split_at(mut self, at: usize) -> Self {
        let Some(Item::Data(data)) = self.items.pop() else {
            return self;
        };
        let (a, b) = data.split_at(at);
        self.items = vec![Item::Data(a.to_vec()), Item::Data(b.to_vec())];
        self
    }

    /// Runs `hook` once the first `at` bytes were read (before the rest is).
    #[must_use]
    pub fn hook_at(mut self, at: usize, hook: impl FnOnce() + Send + 'static) -> Self {
        let Some(Item::Data(data)) = self.items.pop() else {
            return self;
        };
        let (a, b) = data.split_at(at);
        self.items = vec![Item::Data(a.to_vec()), Item::Hook(Box::new(hook))];
        // The rest arrives in small pieces, one read each.
        self.items
            .extend(b.chunks(50).map(|piece| Item::Data(piece.to_vec())));
        self
    }

    #[must_use]
    pub fn end(mut self, end: End) -> Self {
        self.end = end;
        self
    }
}

pub enum Step {
    Reply(Reply),
    Fail(TransportError),
}

impl From<Reply> for Step {
    fn from(reply: Reply) -> Self {
        Self::Reply(reply)
    }
}

/// What the transport was asked to send (the headers with the secret exposed, as a transport sees
/// them).
pub struct Recorded {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// `{:?}` of the request.
    pub debug: String,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("the body is JSON")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
pub struct ScriptedTransport {
    steps: Mutex<VecDeque<Step>>,
    pub requests: Mutex<Vec<Recorded>>,
}

impl ScriptedTransport {
    pub fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::default(),
        })
    }

    pub fn push(&self, step: impl Into<Step>) {
        lock(&self.steps).push_back(step.into());
    }

    pub fn request_count(&self) -> usize {
        lock(&self.requests).len()
    }

    pub fn request(&self, index: usize) -> Recorded {
        let requests = lock(&self.requests);
        let r = &requests[index];
        Recorded {
            method: r.method,
            url: r.url.clone(),
            headers: r.headers.clone(),
            body: r.body.clone(),
            debug: r.debug.clone(),
        }
    }

    pub fn steps_left(&self) -> usize {
        lock(&self.steps).len()
    }
}

struct ScriptedBody {
    items: VecDeque<Item>,
    end: Option<End>,
    cancel: CancelToken,
}

impl Read for ScriptedBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.items.pop_front() {
                Some(Item::Data(mut bytes)) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    if n < bytes.len() {
                        self.items.push_front(Item::Data(bytes.split_off(n)));
                    }
                    return Ok(n);
                }
                Some(Item::Hook(hook)) => hook(),
                None => break,
            }
        }
        match self.end.take() {
            Some(End::Break) => Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            )),
            Some(End::BlockUntilCancel(signal)) => {
                let (wake, woken) = mpsc::channel::<()>();
                let _hook = self.cancel.on_cancel(move || {
                    let _ = wake.send(());
                });
                if let Some(signal) = signal {
                    let _ = signal.send(());
                }
                let _ = woken.recv_timeout(Duration::from_secs(20));
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, "aborted"))
            }
            Some(End::Eof) | None => Ok(0),
        }
    }
}

impl HttpTransport for ScriptedTransport {
    fn send(
        &self,
        request: HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse, TransportError> {
        lock(&self.requests).push(Recorded {
            method: request.method.as_str(),
            url: request.url.clone(),
            headers: request
                .headers
                .iter()
                .map(|h| (h.name.to_owned(), h.value.expose().to_owned()))
                .collect(),
            body: request.body.to_vec(),
            debug: format!("{request:?}"),
        });
        match lock(&self.steps).pop_front() {
            Some(Step::Reply(reply)) => Ok(HttpResponse {
                status: reply.status,
                headers: reply.headers,
                body: Box::new(ScriptedBody {
                    items: reply.items.into(),
                    end: Some(reply.end),
                    cancel: cancel.clone(),
                }),
            }),
            Some(Step::Fail(error)) => Err(error),
            None => Err(TransportError::Failed("no scripted step left".into())),
        }
    }
}

// --------------------------------------------------------------------------------------- sleeper

/// Records the waits; never sleeps. With `block_on_cancel` it blocks until the token is cancelled
/// instead (after telling `entered`).
#[derive(Default)]
pub struct ScriptedSleeper {
    pub slept: Mutex<Vec<Duration>>,
    block: Mutex<Option<Sender<()>>>,
}

impl ScriptedSleeper {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn block_on_cancel(&self, entered: Sender<()>) {
        *lock(&self.block) = Some(entered);
    }

    pub fn waits(&self) -> Vec<Duration> {
        lock(&self.slept).clone()
    }
}

impl Sleeper for ScriptedSleeper {
    fn sleep(&self, duration: Duration, cancel: &CancelToken) -> SleepOutcome {
        lock(&self.slept).push(duration);
        let Some(entered) = lock(&self.block).take() else {
            return SleepOutcome::Elapsed;
        };
        let (wake, woken) = mpsc::channel::<()>();
        let _hook = cancel.on_cancel(move || {
            let _ = wake.send(());
        });
        let _ = entered.send(());
        let _ = woken.recv_timeout(Duration::from_secs(20));
        SleepOutcome::Cancelled
    }
}

// ----------------------------------------------------------------------------------- SSE fixtures

/// Builds the text of a Messages API stream.
pub struct Stream(String);

impl Stream {
    pub fn new() -> Self {
        Self::with_model("claude-opus-5-5")
    }

    pub fn with_model(model: &str) -> Self {
        let mut stream = Self(String::new());
        stream.event(
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_01", "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": null,
                "usage": {"input_tokens": 100, "output_tokens": 1,
                          "cache_read_input_tokens": 40, "cache_creation_input_tokens": 7}}}),
        );
        stream
    }

    fn event(&mut self, name: &str, data: Value) {
        self.0.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    }

    pub fn raw(mut self, text: &str) -> Self {
        self.0.push_str(text);
        self
    }

    pub fn block(mut self, index: usize, block: Value) -> Self {
        self.event(
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": block}),
        );
        self
    }

    pub fn delta(mut self, index: usize, delta: Value) -> Self {
        self.event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": index, "delta": delta}),
        );
        self
    }

    pub fn stop(mut self, index: usize) -> Self {
        self.event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        );
        self
    }

    pub fn ping(mut self) -> Self {
        self.event("ping", json!({"type": "ping"}));
        self
    }

    pub fn text(self, index: usize, parts: &[&str]) -> Self {
        let mut s = self.block(index, json!({"type": "text", "text": ""}));
        for part in parts {
            s = s.delta(index, json!({"type": "text_delta", "text": part}));
        }
        s.stop(index)
    }

    pub fn thinking(self, index: usize, parts: &[&str], signature: &str) -> Self {
        let mut s = self.block(
            index,
            json!({"type": "thinking", "thinking": "", "signature": ""}),
        );
        for part in parts {
            s = s.delta(index, json!({"type": "thinking_delta", "thinking": part}));
        }
        s.delta(
            index,
            json!({"type": "signature_delta", "signature": signature}),
        )
        .stop(index)
    }

    pub fn tool(self, index: usize, id: &str, name: &str, fragments: &[&str]) -> Self {
        let mut s = self.block(
            index,
            json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
        );
        for fragment in fragments {
            s = s.delta(
                index,
                json!({"type": "input_json_delta", "partial_json": fragment}),
            );
        }
        s.stop(index)
    }

    pub fn error_event(mut self, kind: &str, message: &str) -> Self {
        self.event(
            "error",
            json!({"type": "error", "error": {"type": kind, "message": message}}),
        );
        self
    }

    /// `message_delta` and `message_stop`.
    pub fn end(self, stop_reason: &str) -> String {
        self.end_with(stop_reason, json!({}))
    }

    pub fn end_with(self, stop_reason: &str, extra_delta: Value) -> String {
        self.end_full(stop_reason, extra_delta, json!({}))
    }

    pub fn end_full(mut self, stop_reason: &str, extra_delta: Value, extra_usage: Value) -> String {
        let mut delta = json!({"stop_reason": stop_reason, "stop_sequence": null});
        if let (Some(target), Some(extra)) = (delta.as_object_mut(), extra_delta.as_object()) {
            target.extend(extra.clone());
        }
        let mut usage = json!({"output_tokens": 55});
        if let (Some(target), Some(extra)) = (usage.as_object_mut(), extra_usage.as_object()) {
            target.extend(extra.clone());
        }
        self.event(
            "message_delta",
            json!({"type": "message_delta", "delta": delta, "usage": usage}),
        );
        self.event("message_stop", json!({"type": "message_stop"}));
        self.0
    }

    /// The stream so far, without an end (a stream that stops short).
    pub fn unfinished(self) -> String {
        self.0
    }
}

/// A one-text-block turn.
pub fn text_reply(text: &str) -> Reply {
    Reply::ok(Stream::new().text(0, &[text]).end("end_turn"))
}

/// A turn that asks for tools: `(id, name, input JSON text)`.
pub fn tool_reply(calls: &[(&str, &str, &str)]) -> Reply {
    let mut stream = Stream::new().text(0, &["Let me look."]);
    for (i, (id, name, input)) in calls.iter().enumerate() {
        stream = stream.tool(i + 1, id, name, &[input]);
    }
    Reply::ok(stream.end("tool_use"))
}

// ------------------------------------------------------------------------------------- harness

pub const CONNECTION: &str = "c1";

pub struct Harness {
    pub transport: Arc<ScriptedTransport>,
    pub sleeper: Arc<ScriptedSleeper>,
    pub session: Arc<FakeSession>,
    pub layer: ToolLayer,
    pub provider: AnthropicProvider,
    pub conversation: Conversation,
    pub config: AssistantConfig,
    /// `{:?}` of every event of every run.
    pub log: Vec<String>,
}

pub fn users_table() -> TableDescription {
    TableDescription {
        name: "users".into(),
        database: None,
        schema: None,
        columns: vec![
            ColumnDescription {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                default: None,
                comment: None,
            },
            ColumnDescription {
                name: "email".into(),
                data_type: "text".into(),
                nullable: false,
                primary_key: false,
                default: None,
                comment: None,
            },
        ],
        primary_key: vec!["id".into()],
        unique_columns: vec![],
        indexes: vec![],
        foreign_keys: vec![],
        ddl: None,
        ddl_truncated: false,
    }
}

impl Harness {
    pub fn new(steps: Vec<Step>) -> Self {
        Self::with(
            steps,
            AnthropicConfig::default(),
            Some(AgentAccess::Read),
            true,
        )
    }

    pub fn with(
        steps: Vec<Step>,
        config: AnthropicConfig,
        access: Option<AgentAccess>,
        scoped: bool,
    ) -> Self {
        let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
        lock(&session.tables).push(users_table());
        let catalog = FakeCatalog::new(vec![connection(
            CONNECTION,
            ConnectionKind::PostgreSql,
            access,
        )]);
        let sessions = FakeProvider::new().with_session(CONNECTION, Arc::clone(&session));
        let layer = ToolLayer::new(catalog, sessions, "assistant");
        let transport = ScriptedTransport::new(steps);
        let sleeper = ScriptedSleeper::new();
        let provider = AnthropicProvider::new(
            Arc::clone(&transport) as Arc<dyn HttpTransport>,
            Arc::new(FixedKey),
            config,
        )
        .expect("a valid test configuration")
        .with_sleeper(Arc::clone(&sleeper) as Arc<dyn Sleeper>);
        let scope = scoped.then(|| Scope {
            connection_id: CONNECTION.into(),
            database: None,
            schema: None,
        });
        Self {
            transport,
            sleeper,
            session,
            layer,
            provider,
            conversation: Conversation::new(scope),
            config: AssistantConfig::default(),
            log: Vec::new(),
        }
    }

    pub fn with_limits(mut self, limits: crate::anthropic::TurnLimits) -> Self {
        self.provider = self.provider.with_limits(limits);
        self
    }

    pub fn run(&mut self, text: &str) -> Result<RunSummary, AssistantError> {
        self.run_with(text, &CancelToken::new())
    }

    /// Runs one user message. Everything the run produces is checked for the key.
    pub fn run_with(
        &mut self,
        text: &str,
        cancel: &CancelToken,
    ) -> Result<RunSummary, AssistantError> {
        let assistant = Assistant::new(&self.provider, &self.layer, self.config.clone());
        let log = &mut self.log;
        let result = assistant.run(
            &mut self.conversation,
            text.into(),
            &mut |event: AssistantEvent<'_>| log.push(format!("{event:?}")),
            cancel,
        );
        self.assert_no_secret(&result);
        result
    }

    pub fn resume(&mut self, cancel: &CancelToken) -> Result<RunSummary, AssistantError> {
        let assistant = Assistant::new(&self.provider, &self.layer, self.config.clone());
        let log = &mut self.log;
        let result = assistant.resume(
            &mut self.conversation,
            &mut |event: AssistantEvent<'_>| log.push(format!("{event:?}")),
            cancel,
        );
        self.assert_no_secret(&result);
        result
    }

    /// The one check every test path feeds: the key is in no event, error, `Debug` output,
    /// serialized conversation or request body.
    pub fn assert_no_secret<T: std::fmt::Debug>(&self, result: &Result<T, AssistantError>) {
        let mut outputs = self.log.clone();
        match result {
            Ok(value) => outputs.push(format!("{value:?}")),
            Err(error) => {
                outputs.push(format!("{error}"));
                outputs.push(format!("{error:?}"));
            }
        }
        outputs.push(format!("{:?}", self.conversation));
        outputs.push(serde_json::to_string(&self.conversation).expect("serializes"));
        for index in 0..self.transport.request_count() {
            let request = self.transport.request(index);
            outputs.push(request.debug);
            outputs.push(String::from_utf8_lossy(&request.body).into_owned());
        }
        for output in outputs {
            assert!(
                !output.contains(FAKE_KEY) && !output.contains("ZZSECRETZZ"),
                "the API key leaked into: {output}"
            );
        }
    }

    pub fn last_request(&self) -> Recorded {
        self.transport.request(self.transport.request_count() - 1)
    }
}
