//! The transport against an in-process server speaking raw HTTP/1.1 on 127.0.0.1: nothing here
//! leaves the machine or touches a credential store.

use super::*;
use engine_ai::{ApiKey, Header, Timeouts};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::thread::JoinHandle;

const KEY: &str = "sk-test-0123456789";

/// What the server read of a request.
#[derive(Debug, Clone)]
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn read_request(stream: &mut TcpStream) -> Seen {
    let mut data = Vec::new();
    let mut byte = [0u8; 1];
    while !data.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).expect("request head") == 0 {
            break;
        }
        data.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&data).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).expect("request body");
    Seen {
        request_line,
        headers,
        body,
    }
}

fn head(status: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut text = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in extra {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    text.into_bytes()
}

fn chunk(data: &str) -> Vec<u8> {
    format!("{:x}\r\n{data}\r\n", data.len()).into_bytes()
}

const SSE_HEAD: &[(&str, &str)] = &[
    ("content-type", "text/event-stream"),
    ("transfer-encoding", "chunked"),
];

struct Server {
    address: SocketAddr,
    seen: Receiver<Seen>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Accepts one connection, reads its request, reports it, then runs `script` on the socket.
    fn start(script: impl FnOnce(TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (report, seen) = channel();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            report.send(request).unwrap();
            script(stream);
        });
        Self {
            address,
            seen,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn seen(&self) -> Seen {
        self.seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the server saw a request")
    }

    fn finish(mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("server script");
        }
    }
}

fn transport() -> ReqwestTransport {
    ReqwestTransport::new("TableX-test").direct()
}

fn timeouts(first_byte_ms: u64, idle_ms: u64) -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(5),
        first_byte: Duration::from_millis(first_byte_ms),
        idle: Duration::from_millis(idle_ms),
    }
}

fn post(url: String, timeouts: Timeouts) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        url,
        headers: vec![
            Header::secret("x-api-key", ApiKey::new(KEY)),
            Header::plain("anthropic-version", "2023-06-01"),
            Header::plain("content-type", "application/json"),
        ],
        body: br#"{"stream":true}"#.as_slice().into(),
        timeouts,
    }
}

fn read_some(body: &mut dyn Read) -> io::Result<String> {
    let mut buffer = [0u8; 512];
    let count = body.read(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer[..count]).into_owned())
}

#[test]
fn a_chunked_stream_reaches_the_reader_as_it_arrives() {
    let (release, released) = channel::<()>();
    let server = Server::start(move |mut stream| {
        stream.write_all(&head("200 OK", SSE_HEAD)).unwrap();
        stream.write_all(&chunk("data: one\n\n")).unwrap();
        // The second event is held back until the client has read the first.
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        stream.write_all(&chunk("data: two\n\n")).unwrap();
        stream.write_all(b"0\r\n\r\n").unwrap();
    });
    let cancel = CancelToken::new();
    let mut response = transport()
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 5000)),
            &cancel,
        )
        .expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("Content-Type"), Some("text/event-stream"));

    let first = read_some(&mut response.body).unwrap();
    assert_eq!(
        first, "data: one\n\n",
        "the first event arrives while the server still holds the second"
    );
    release.send(()).unwrap();
    let mut rest = String::new();
    response.body.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "data: two\n\n");
    server.finish();
}

#[test]
fn the_headers_sent_are_the_providers_and_nothing_else_of_note() {
    let server = Server::start(|mut stream| {
        stream
            .write_all(&head("200 OK", &[("content-length", "0")]))
            .unwrap();
    });
    let cancel = CancelToken::new();
    transport()
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 5000)),
            &cancel,
        )
        .expect("response");
    let seen = server.seen();
    assert_eq!(seen.request_line, "POST /v1/messages HTTP/1.1");
    assert_eq!(seen.header("x-api-key"), Some(KEY));
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.body, br#"{"stream":true}"#);
    // Besides the provider's headers only what any HTTP client adds.
    let transport_headers = [
        "host",
        "accept",
        "content-length",
        "user-agent",
        "accept-encoding",
    ];
    let provider_headers = ["x-api-key", "anthropic-version", "content-type"];
    for (name, _) in &seen.headers {
        let name = name.to_ascii_lowercase();
        assert!(
            provider_headers.contains(&name.as_str()) || transport_headers.contains(&name.as_str()),
            "unexpected header {name}"
        );
    }
    assert_eq!(seen.header("user-agent"), Some("TableX-test"));
    server.finish();
}

#[test]
fn a_429_comes_back_as_a_response_with_its_retry_after_and_request_id() {
    let server = Server::start(|mut stream| {
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        stream
            .write_all(&head(
                "429 Too Many Requests",
                &[
                    ("retry-after", "7"),
                    ("request-id", "req_abc123"),
                    ("content-type", "application/json"),
                    ("content-length", &body.len().to_string()),
                ],
            ))
            .unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    });
    let cancel = CancelToken::new();
    let mut response = transport()
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 5000)),
            &cancel,
        )
        .expect("a 429 is a response, not an error");
    assert_eq!(response.status, 429);
    assert_eq!(response.header("retry-after"), Some("7"));
    assert_eq!(response.header("Request-Id"), Some("req_abc123"));
    let mut text = String::new();
    response.body.read_to_string(&mut text).unwrap();
    assert!(text.contains("rate_limit_error"));
    server.finish();
}

#[test]
fn a_cancel_during_a_stalled_body_returns_promptly_and_the_connection_closes() {
    let (stall, stalled) = channel::<()>();
    let (observed, closed) = channel::<bool>();
    let (again, hold) = channel::<()>();
    let server = Server::start(move |mut stream| {
        stream.write_all(&head("200 OK", SSE_HEAD)).unwrap();
        stream.write_all(&chunk("data: one\n\n")).unwrap();
        stall.send(()).unwrap();
        // Silence until the test says the client cancelled, then one more chunk: the client side
        // closes the connection no later than that chunk.
        hold.recv_timeout(Duration::from_secs(10)).unwrap();
        let _ = stream.write_all(&chunk(": ping\n\n"));
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut byte = [0u8; 1];
        let ended = matches!(stream.read(&mut byte), Ok(0) | Err(_));
        observed.send(ended).unwrap();
    });
    let cancel = CancelToken::new();
    let mut response = transport()
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 30_000)),
            &cancel,
        )
        .expect("response");
    assert_eq!(read_some(&mut response.body).unwrap(), "data: one\n\n");
    stalled.recv_timeout(Duration::from_secs(5)).unwrap();

    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let at = Instant::now();
            cancel.cancel_detached();
            at
        })
    };
    // Blocked in a read while the server says nothing.
    let error = read_some(&mut response.body).expect_err("the cancel ends the read");
    let woke = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    // The point is that the stalled server (idle limit 30 s) did not decide when the read
    // ended; the figure is a measurement, the bound is generous so a loaded machine passes.
    eprintln!(
        "WAKE-METRICS read returned {:?} after the cancel",
        woke.duration_since(cancelled_at)
    );
    assert!(
        woke.duration_since(cancelled_at) < Duration::from_secs(5),
        "the read returned {:?} after the cancel",
        woke.duration_since(cancelled_at)
    );
    // Reads after the cancel keep failing.
    assert!(response.body.read(&mut [0u8; 8]).is_err());
    again.send(()).unwrap();
    assert!(
        closed.recv_timeout(Duration::from_secs(5)).unwrap(),
        "the server saw the socket close"
    );
    server.finish();
}

#[test]
fn a_cancel_during_the_wait_for_the_response_returns_promptly() {
    let (release, released) = channel::<()>();
    let server = Server::start(move |_stream| {
        // Holds the connection open and answers nothing.
        let _ = released.recv_timeout(Duration::from_secs(10));
    });
    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let at = Instant::now();
            cancel.cancel_detached();
            at
        })
    };
    let result = transport().send(
        post(server.url("/v1/messages"), timeouts(30_000, 30_000)),
        &cancel,
    );
    let woke = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert!(
        matches!(result, Err(TransportError::Cancelled)),
        "{result:?}"
    );
    eprintln!(
        "WAKE-METRICS send returned {:?} after the cancel",
        woke.duration_since(cancelled_at)
    );
    assert!(woke.duration_since(cancelled_at) < Duration::from_secs(5));
    release.send(()).unwrap();
    server.finish();

    // A token that is cancelled already never connects.
    let result = transport().send(
        post(
            "http://127.0.0.1:9/v1/messages".into(),
            timeouts(1000, 1000),
        ),
        &cancel,
    );
    assert!(matches!(result, Err(TransportError::Cancelled)));
}

#[test]
fn a_silent_body_ends_with_a_timeout_after_the_idle_limit() {
    let (release, released) = channel::<()>();
    let server = Server::start(move |mut stream| {
        stream.write_all(&head("200 OK", SSE_HEAD)).unwrap();
        stream.write_all(&chunk("data: one\n\n")).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(10));
    });
    let cancel = CancelToken::new();
    let mut response = transport()
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 300)),
            &cancel,
        )
        .expect("response");
    assert_eq!(read_some(&mut response.body).unwrap(), "data: one\n\n");
    let started = Instant::now();
    let error = read_some(&mut response.body).expect_err("idle");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_secs(8),
        "{waited:?}"
    );
    release.send(()).unwrap();
    server.finish();
}

#[test]
fn a_server_that_never_answers_ends_with_a_timeout() {
    let (release, released) = channel::<()>();
    let server = Server::start(move |_stream| {
        let _ = released.recv_timeout(Duration::from_secs(10));
    });
    let cancel = CancelToken::new();
    let started = Instant::now();
    let result = transport().send(
        post(server.url("/v1/messages"), timeouts(300, 300)),
        &cancel,
    );
    assert!(matches!(result, Err(TransportError::Timeout)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(8));
    release.send(()).unwrap();
    server.finish();
}

#[test]
fn a_redirect_to_another_host_is_not_followed_and_never_carries_the_key() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    // `localhost` is another host than `127.0.0.1`, on this machine.
    let location = format!(
        "http://localhost:{}/stolen",
        target.local_addr().unwrap().port()
    );
    let server = Server::start(move |mut stream| {
        stream
            .write_all(&head(
                "307 Temporary Redirect",
                &[("location", &location), ("content-length", "0")],
            ))
            .unwrap();
    });
    let cancel = CancelToken::new();
    let response = transport()
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 5000)),
            &cancel,
        )
        .expect("the redirect itself is the response");
    assert_eq!(response.status, 307);
    assert!(response
        .header("location")
        .is_some_and(|value| value.ends_with("/stolen")));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        target.accept().is_err(),
        "nothing connected to the other host: the key was not sent there"
    );
    server.finish();
}

#[test]
fn a_redirect_within_the_origin_is_followed_with_the_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (report, seen) = channel::<Seen>();
    let thread = std::thread::spawn(move || {
        // Redirect and final answer may come on one connection or two.
        let mut served = 0;
        while served < 2 {
            let (mut stream, _) = listener.accept().unwrap();
            loop {
                let mut peek = [0u8; 1];
                if stream.peek(&mut peek).map_or(true, |count| count == 0) {
                    break;
                }
                let request = read_request(&mut stream);
                let redirect = request.request_line.contains("/first");
                report.send(request).unwrap();
                served += 1;
                if redirect {
                    stream
                        .write_all(&head(
                            "307 Temporary Redirect",
                            &[("location", "/second"), ("content-length", "0")],
                        ))
                        .unwrap();
                } else {
                    stream
                        .write_all(&head("200 OK", &[("content-length", "0")]))
                        .unwrap();
                    return;
                }
            }
        }
    });
    let cancel = CancelToken::new();
    let response = transport()
        .send(
            post(format!("http://{address}/first"), timeouts(5000, 5000)),
            &cancel,
        )
        .expect("response");
    assert_eq!(response.status, 200);
    let first = seen.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = seen.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(first.request_line.contains("/first") && second.request_line.contains("/second"));
    assert_eq!(
        second.header("x-api-key"),
        Some(KEY),
        "the same origin keeps the key"
    );
    thread.join().unwrap();
}

#[test]
fn the_key_is_not_sent_over_plain_http_to_another_machine() {
    let cancel = CancelToken::new();
    let result = transport().send(
        post("http://192.0.2.1/v1/messages".into(), timeouts(1000, 1000)),
        &cancel,
    );
    match result {
        Err(TransportError::Failed(text)) => {
            assert!(text.contains("unencrypted"), "{text}");
            assert!(!text.contains(KEY));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_failed_connection_names_the_problem_without_the_key() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let cancel = CancelToken::new();
    let result = transport().send(
        post(
            format!("http://{address}/v1/messages"),
            timeouts(2000, 2000),
        ),
        &cancel,
    );
    match result {
        Err(TransportError::Failed(text)) => {
            assert!(!text.contains(KEY), "{text}");
            assert!(
                !text.contains("v1/messages"),
                "the URL is not echoed: {text}"
            );
        }
        other => panic!("expected a connection failure, got {other:?}"),
    }
}

#[test]
fn a_bad_header_value_is_refused_without_echoing_it() {
    let cancel = CancelToken::new();
    let mut request = post("http://127.0.0.1:9/".into(), timeouts(1000, 1000));
    request.headers = vec![Header::secret("x-api-key", ApiKey::new("bad\nkey-value"))];
    match transport().send(request, &cancel) {
        Err(TransportError::Failed(text)) => assert!(!text.contains("key-value"), "{text}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn two_headers_of_one_name_both_go() {
    let server = Server::start(|mut stream| {
        stream
            .write_all(&head("200 OK", &[("content-length", "0")]))
            .unwrap();
    });
    let cancel = CancelToken::new();
    let mut request = post(server.url("/v1/messages"), timeouts(5000, 5000));
    request
        .headers
        .push(Header::plain("anthropic-beta", "first-2026-01-01"));
    request
        .headers
        .push(Header::plain("anthropic-beta", "second-2026-02-02"));
    transport().send(request, &cancel).expect("response");
    let seen = server.seen();
    let betas: Vec<&str> = seen
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(betas, ["first-2026-01-01", "second-2026-02-02"], "{seen:?}");
    server.finish();
}

#[test]
fn a_large_body_arrives_whole_through_the_bounded_queue() {
    let total = 6 * 1024 * 1024;
    let server = Server::start(move |mut stream| {
        stream.write_all(&head("200 OK", SSE_HEAD)).unwrap();
        let piece = "x".repeat(32 * 1024);
        for _ in 0..(total / piece.len()) {
            if stream.write_all(&chunk(&piece)).is_err() {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    let cancel = CancelToken::new();
    let mut response = transport()
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 5000)),
            &cancel,
        )
        .expect("response");
    // Let the server get ahead of the reader first.
    std::thread::sleep(Duration::from_millis(200));
    let mut count = 0;
    let mut buffer = vec![0u8; 8192];
    loop {
        match response.body.read(&mut buffer).expect("read") {
            0 => break,
            read => count += read,
        }
    }
    assert_eq!(count, total);
    server.finish();
}

#[test]
fn the_queue_is_bounded_and_a_cancel_or_a_gone_reader_frees_a_waiting_helper() {
    let channel = Channel::new();
    let pushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let helper = {
        let (channel, pushed) = (Arc::clone(&channel), Arc::clone(&pushed));
        std::thread::spawn(move || {
            for _ in 0..(QUEUE_CAP + 10) {
                if !channel.push(Message::Chunk(vec![0; 8])) {
                    return false;
                }
                pushed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            true
        })
    };
    // Nobody reads: the helper stops at the bound.
    let start = Instant::now();
    while pushed.load(std::sync::atomic::Ordering::SeqCst) < QUEUE_CAP {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        pushed.load(std::sync::atomic::Ordering::SeqCst),
        QUEUE_CAP,
        "it waits, it does not buffer"
    );
    // A cancel frees it without anyone reading.
    channel.cancel();
    assert!(!helper.join().unwrap(), "the helper is told to stop");
    assert!(matches!(
        channel.pop(Duration::from_secs(1)),
        Popped::Cancelled
    ));

    // A reader that drops frees it too.
    let channel = Channel::new();
    let helper = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || (0..(QUEUE_CAP + 10)).all(|_| channel.push(Message::End)))
    };
    std::thread::sleep(Duration::from_millis(100));
    channel.reader_done();
    assert!(!helper.join().unwrap());

    // A reader that reads lets it go on.
    let channel = Channel::new();
    let helper = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || (0..(QUEUE_CAP * 3)).all(|_| channel.push(Message::End)))
    };
    let mut seen = 0;
    while seen < QUEUE_CAP * 3 {
        match channel.pop(Duration::from_secs(10)) {
            Popped::Message(_) => seen += 1,
            _ => panic!("the queue stalled after {seen}"),
        }
    }
    assert!(helper.join().unwrap());
}

/// A listener that records whether anything connected to it.
fn spy() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (heard, receiver) = channel();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut first = [0u8; 256];
            let count = stream.read(&mut first).unwrap_or(0);
            let _ = heard.send(String::from_utf8_lossy(&first[..count]).into_owned());
        }
    });
    (address, receiver)
}

#[test]
fn a_plain_loopback_address_never_reaches_a_proxy_with_the_key() {
    let (proxy, heard) = spy();
    let server = Server::start(|mut stream| {
        stream
            .write_all(&head("200 OK", &[("content-length", "0")]))
            .unwrap();
    });
    let cancel = CancelToken::new();
    // This transport has a proxy configured, as a machine with HTTP_PROXY would.
    let transport =
        ReqwestTransport::new("TableX-test").with_test_proxy(&format!("http://{proxy}"));
    let response = transport
        .send(
            post(server.url("/v1/messages"), timeouts(5000, 5000)),
            &cancel,
        )
        .expect("the loopback server answers directly");
    assert_eq!(response.status, 200);
    let seen = server.seen();
    assert_eq!(
        seen.header("x-api-key"),
        Some(KEY),
        "it arrived at the server itself"
    );
    assert!(
        heard.recv_timeout(Duration::from_millis(500)).is_err(),
        "the proxy saw a connection"
    );
    server.finish();
}

#[test]
fn control_a_remote_https_address_does_go_through_the_proxy() {
    let (proxy, heard) = spy();
    let cancel = CancelToken::new();
    let transport =
        ReqwestTransport::new("TableX-test").with_test_proxy(&format!("http://{proxy}"));
    let _ = transport.send(
        post(
            "https://gateway.example.invalid/v1/messages".into(),
            timeouts(3000, 3000),
        ),
        &cancel,
    );
    let first = heard
        .recv_timeout(Duration::from_secs(5))
        .expect("the proxy is the one that was connected to");
    // An https target goes through a proxy as a tunnel: the key is inside the TLS, not in this.
    assert!(
        first.starts_with("CONNECT gateway.example.invalid"),
        "{first}"
    );
    assert!(!first.contains(KEY));
}

/// A helper that panics still tells the reader it is gone: the reader is not left waiting for the
/// whole idle timeout of a request nobody is working on.
#[test]
fn a_helper_that_panics_still_wakes_the_reader() {
    let channel = Channel::new();
    let helper = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || {
            let _done = HelperDone(Arc::clone(&channel));
            panic!("the helper failed in a way nobody expected");
        })
    };
    assert!(helper.join().is_err());
    let started = Instant::now();
    assert!(matches!(
        channel.pop(Duration::from_secs(30)),
        Popped::HelperGone
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}
