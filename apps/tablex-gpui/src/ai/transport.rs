//! [`HttpTransport`] on the app's blocking `zed_reqwest` client (the one `drivers.rs` uses).
//!
//! The blocking client cannot abort a read that is waiting for the network, and the assistant must
//! react to a cancel at once, so every request runs on a helper thread and the caller only waits on
//! a channel:
//!
//! * the helper sends the request, then reads the body in chunks and queues them;
//! * the caller's `send` and every `Read::read` of the body wait on that queue, with the cancel
//!   token's hook as one more sender, so a cancel wakes a blocked caller within a scheduling
//!   quantum (no polling) and the idle timeout is a `recv_timeout`;
//! * a cancelled or abandoned request sets a flag the helper checks after every chunk, so the
//!   connection is closed no later than the next chunk (the server's SSE `ping`s make that a few
//!   seconds at most); the client's own wait limit (`max(first_byte, idle)` per read) ends a helper
//!   whose server went silent for good.
//!
//! No total timeout is set (a long answer must not be cut). Redirects are followed only within one
//! origin: the key header is never forwarded to another host (reqwest strips `Authorization`, not
//! `x-api-key`). The proxy and TLS behaviour are the client's defaults, as in `drivers.rs`; nothing
//! here turns certificate checks off. The key is read through `HeaderValue::expose` while the
//! request is built, marked sensitive and not kept.

use engine_agent::cancel::HookGuard;
use engine_agent::CancelToken;
use engine_ai::{HttpRequest, HttpResponse, HttpTransport, Method, TransportError};
use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use zed_reqwest::blocking::{Client, Request};
use zed_reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use zed_reqwest::redirect::Policy;

/// Bytes read from the socket at a time.
const READ_CHUNK: usize = 16 * 1024;

/// One client per `(connect timeout, read wait, proxy allowed)`; clients keep their connections
/// warm between the turns of a conversation.
type ClientKey = (Duration, Duration, bool);

/// Chunks the helper may have queued for a reader that is slower than the server.
const QUEUE_CAP: usize = 32;

pub struct ReqwestTransport {
    clients: Mutex<Vec<(ClientKey, Client)>>,
    user_agent: String,
    /// Tests talk to 127.0.0.1: a machine-wide proxy must not get in the way.
    #[cfg(test)]
    direct: bool,
    /// Tests give the client a proxy of their own, instead of the machine's.
    #[cfg(test)]
    test_proxy: Option<String>,
}

impl ReqwestTransport {
    pub fn new(user_agent: impl Into<String>) -> Self {
        Self {
            clients: Mutex::new(Vec::new()),
            user_agent: user_agent.into(),
            #[cfg(test)]
            direct: false,
            #[cfg(test)]
            test_proxy: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn direct(mut self) -> Self {
        self.direct = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_proxy(mut self, proxy: &str) -> Self {
        self.test_proxy = Some(proxy.to_owned());
        self
    }

    /// The client for these limits. `proxy_allowed` is false for a plain-http address on this
    /// machine: a system proxy has no loopback exemption of its own (only `NO_PROXY`), and it
    /// would be handed the request, key included, in clear text.
    fn client(
        &self,
        connect: Duration,
        wait: Duration,
        proxy_allowed: bool,
    ) -> Result<Client, TransportError> {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (connect, wait, proxy_allowed);
        if let Some((_, client)) = clients.iter().find(|(known, _)| *known == key) {
            return Ok(client.clone());
        }
        let builder = Client::builder()
            .user_agent(self.user_agent.clone())
            .connect_timeout(connect)
            // Per wait for the response head and per body read, never for the whole transfer.
            .timeout(Some(wait))
            .redirect(same_origin_only());
        // A test gives the client a proxy of its own first, whatever the address: the line below
        // that keeps a loopback address away from any proxy is the production code under test.
        #[cfg(test)]
        let builder = match self
            .test_proxy
            .as_deref()
            .map(zed_reqwest::Proxy::all)
            .transpose()
        {
            Ok(Some(proxy)) => builder.no_proxy().proxy(proxy),
            _ => builder,
        };
        let builder = if proxy_allowed {
            builder
        } else {
            builder.no_proxy()
        };
        #[cfg(test)]
        let builder = if self.direct {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder
            .build()
            .map_err(|error| TransportError::Failed(describe(&error)))?;
        clients.push((key, client.clone()));
        Ok(client)
    }

    /// The reqwest request for `request`. The secret header values are read here, at the moment
    /// the request is built.
    fn build(&self, request: &HttpRequest) -> Result<(Client, Request), TransportError> {
        let url = url::Url::parse(&request.url)
            .map_err(|_| TransportError::Failed("the request URL is not valid".into()))?;
        let secret = request
            .headers
            .iter()
            .any(|header| header.value.is_secret());
        if secret && !crate::ai::sends_secrets_to(&url) {
            return Err(TransportError::Failed(
                "refusing to send the API key over an unencrypted connection".into(),
            ));
        }
        let mut headers = HeaderMap::with_capacity(request.headers.len());
        for header in &request.headers {
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| {
                TransportError::Failed(format!("`{}` is not a valid header name", header.name))
            })?;
            let mut value = HeaderValue::from_str(header.value.expose()).map_err(|_| {
                TransportError::Failed(format!("the `{}` header value is not valid", header.name))
            })?;
            value.set_sensitive(header.value.is_secret());
            // `append`: two headers of one name both go (a header may be repeated).
            headers.append(name, value);
        }
        let timeouts = request.timeouts;
        let wait = timeouts.first_byte.max(timeouts.idle);
        let proxy_allowed = !crate::ai::is_plain_loopback(&url);
        let client = self.client(timeouts.connect, wait, proxy_allowed)?;
        let method = match request.method {
            Method::Get => zed_reqwest::Method::GET,
            Method::Post => zed_reqwest::Method::POST,
        };
        let mut builder = client.request(method, url).headers(headers);
        if !request.body.is_empty() {
            // The client takes owned bytes (`Vec`, `String`, `Bytes`; `Bytes` is not nameable
            // here without a new dependency), so each attempt copies the body once.
            builder = builder.body(request.body.to_vec());
        }
        let built = builder
            .build()
            .map_err(|error| TransportError::Failed(describe(&error)))?;
        Ok((client, built))
    }
}

/// Follow a redirect only when it stays on the origin (scheme, host, port) it came from.
fn same_origin_only() -> Policy {
    Policy::custom(|attempt| {
        let same = attempt
            .previous()
            .last()
            .is_some_and(|from| from.origin() == attempt.url().origin());
        if same && attempt.previous().len() < 5 {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// What went wrong, without the URL (it may carry a query) and without any header.
fn describe(error: &zed_reqwest::Error) -> String {
    let mut text = String::new();
    let mut source: Option<&dyn std::error::Error> = Some(error);
    while let Some(current) = source {
        if !text.is_empty() {
            text.push_str(": ");
        }
        text.push_str(&current.to_string());
        source = current.source();
    }
    // `Display` of a reqwest error appends the URL; the chain above repeats the short form.
    match error.url() {
        Some(url) => text.replace(url.as_str(), "<url>"),
        None => text,
    }
}

fn map_error(error: &zed_reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Failed(describe(error))
    }
}

enum Message {
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    Chunk(Vec<u8>),
    End,
    Failed(TransportError),
}

/// What the reader gets from the queue.
enum Popped {
    Message(Message),
    Timeout,
    Cancelled,
    /// The helper ended without saying how.
    HelperGone,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Message>,
    cancelled: bool,
    reader_gone: bool,
    helper_gone: bool,
}

/// The helper thread's queue to the reader: bounded, so a server that is faster than the reader
/// waits in the socket (TCP back-pressure) instead of in memory; the cancel is not a message on
/// it but a flag, so it never waits for room and wakes both ends at once.
struct Channel {
    state: Mutex<State>,
    readable: Condvar,
    writable: Condvar,
}

impl Channel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            readable: Condvar::new(),
            writable: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Helper side. Waits while the queue is full. `false`: nobody wants more (cancelled, or the
    /// reader is gone): the helper closes the connection and ends.
    fn push(&self, message: Message) -> bool {
        let mut state = self.lock();
        while state.queue.len() >= QUEUE_CAP && !state.cancelled && !state.reader_gone {
            state = self
                .writable
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if state.cancelled || state.reader_gone {
            return false;
        }
        state.queue.push_back(message);
        self.readable.notify_one();
        true
    }

    /// Reader side.
    fn pop(&self, timeout: Duration) -> Popped {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if state.cancelled {
                return Popped::Cancelled;
            }
            if let Some(message) = state.queue.pop_front() {
                self.writable.notify_one();
                return Popped::Message(message);
            }
            if state.helper_gone {
                return Popped::HelperGone;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Popped::Timeout;
            }
            state = self
                .readable
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// From the cancel hook: never blocks beyond the lock.
    fn cancel(&self) {
        self.lock().cancelled = true;
        self.readable.notify_all();
        self.writable.notify_all();
    }

    /// The reader will not read any more (it timed out, finished or was dropped).
    fn reader_done(&self) {
        self.lock().reader_gone = true;
        self.writable.notify_all();
    }

    fn helper_done(&self) {
        self.lock().helper_gone = true;
        self.readable.notify_all();
    }

    fn wanted(&self) -> bool {
        let state = self.lock();
        !state.cancelled && !state.reader_gone
    }
}

/// Tells the reader that the helper thread is over, however it ends.
struct HelperDone(Arc<Channel>);

impl Drop for HelperDone {
    fn drop(&mut self) {
        self.0.helper_done();
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse, TransportError> {
        if cancel.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        let (client, built) = self.build(&request)?;
        let timeouts = request.timeouts;
        drop(request);

        let channel = Channel::new();
        let hook = cancel.on_cancel({
            let channel = Arc::clone(&channel);
            move || channel.cancel()
        });
        std::thread::Builder::new()
            .name("ai-http".into())
            .spawn({
                let channel = Arc::clone(&channel);
                move || {
                    // Told even when the helper panics: the reader would wait out its timeout.
                    let _done = HelperDone(Arc::clone(&channel));
                    run_request(client, built, &channel);
                }
            })
            .map_err(|error| TransportError::Failed(format!("cannot start a worker: {error}")))?;

        let wait = timeouts.connect + timeouts.first_byte;
        let (status, headers) = match channel.pop(wait) {
            Popped::Message(Message::Head { status, headers }) => (status, headers),
            Popped::Message(Message::Failed(error)) => return Err(error),
            Popped::Message(Message::Chunk(_) | Message::End) => {
                channel.reader_done();
                return Err(TransportError::Failed(
                    "the worker lost the response head".into(),
                ));
            }
            Popped::Cancelled => return Err(TransportError::Cancelled),
            Popped::Timeout => {
                channel.reader_done();
                return Err(TransportError::Timeout);
            }
            Popped::HelperGone => {
                return Err(TransportError::Failed("the HTTP worker stopped".into()))
            }
        };
        Ok(HttpResponse {
            status,
            headers,
            body: Box::new(Body {
                channel,
                chunk: Vec::new(),
                taken: 0,
                finished: None,
                idle: timeouts.idle,
                _hook: hook,
            }),
        })
    }
}

/// The helper thread: the request, then the body, chunk by chunk, until the end, a failure or
/// nobody wanting more. Dropping the response at the end closes the connection.
fn run_request(client: Client, request: Request, channel: &Channel) {
    let mut response = match client.execute(request) {
        Ok(response) => response,
        Err(error) => {
            channel.push(Message::Failed(map_error(&error)));
            return;
        }
    };
    if !channel.wanted() {
        return;
    }
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    let head = Message::Head {
        status: response.status().as_u16(),
        headers,
    };
    if !channel.push(head) {
        return;
    }
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        let message = match response.read(&mut buffer) {
            Ok(0) => Message::End,
            Ok(read) => Message::Chunk(buffer[..read].to_vec()),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                Message::Failed(TransportError::Timeout)
            }
            Err(error) => Message::Failed(TransportError::Failed(format!(
                "the response stream broke: {error}"
            ))),
        };
        let last = !matches!(message, Message::Chunk(_));
        // `push` is false when the reader is gone or a cancel came in while this read waited.
        if !channel.push(message) || last {
            return;
        }
    }
}

/// The response body as the provider reads it.
struct Body {
    channel: Arc<Channel>,
    chunk: Vec<u8>,
    taken: usize,
    /// How the stream ended, once it has: `None` = clean end of the body.
    finished: Option<Option<io::ErrorKind>>,
    idle: Duration,
    _hook: HookGuard,
}

impl Body {
    fn end(&mut self, error: Option<(io::ErrorKind, &str)>) -> io::Result<usize> {
        self.channel.reader_done();
        self.finished = Some(error.map(|(kind, _)| kind));
        match error {
            Some((kind, text)) => Err(io::Error::new(kind, text)),
            None => Ok(0),
        }
    }
}

impl Read for Body {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.taken < self.chunk.len() {
                let count = out.len().min(self.chunk.len() - self.taken);
                out[..count].copy_from_slice(&self.chunk[self.taken..self.taken + count]);
                self.taken += count;
                return Ok(count);
            }
            match self.finished {
                Some(None) => return Ok(0),
                Some(Some(kind)) => return Err(io::Error::new(kind, "the response stream ended")),
                None => {}
            }
            match self.channel.pop(self.idle) {
                Popped::Message(Message::Chunk(chunk)) => {
                    self.chunk = chunk;
                    self.taken = 0;
                }
                Popped::Message(Message::End) => return self.end(None),
                Popped::Cancelled => {
                    return self.end(Some((
                        io::ErrorKind::ConnectionAborted,
                        "the request was cancelled",
                    )))
                }
                Popped::Message(Message::Failed(TransportError::Timeout)) | Popped::Timeout => {
                    return self.end(Some((
                        io::ErrorKind::TimedOut,
                        "the response stream went quiet",
                    )))
                }
                Popped::Message(Message::Failed(error)) => {
                    return self.end(Some((io::ErrorKind::ConnectionReset, &error.to_string())))
                }
                Popped::Message(Message::Head { .. }) | Popped::HelperGone => {
                    return self.end(Some((
                        io::ErrorKind::UnexpectedEof,
                        "the HTTP worker stopped",
                    )))
                }
            }
        }
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        // The provider is done with the stream: let the helper close the connection.
        self.channel.reader_done();
    }
}

#[cfg(test)]
mod tests;
