//! SOCKS5 / HTTP CONNECT proxies against in-process proxy servers (`tests/support/proxy.rs`): the proxy-only
//! route, SSH over a proxy, and a jump chain behind a proxy. Loopback only, no Docker.

// Errors are cold-path values; their size does not matter here.
#![allow(clippy::result_large_err)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use engine_net::{
    trust_host_key, Auth, Jump, NoPrompts, ProxyCredentials, ProxyKind, ProxyRoute, ProxySpec,
    Secret, Target, Tunnel, TunnelError, TunnelSpec,
};
use support::proxy::{Protocol, ProxyOptions, Refusal, TestProxy};
use support::{eventually, AuthPlan, ServerOptions, TestServer, PORT_ECHO};

const LOGIN_USER: &str = "proxy-user";
const LOGIN_PASSWORD: &str = "proxy-pw-needle-7";

fn kind(protocol: Protocol) -> ProxyKind {
    match protocol {
        Protocol::Socks5 => ProxyKind::Socks5,
        Protocol::Http => ProxyKind::HttpConnect,
    }
}

fn proxy_spec(proxy: &TestProxy, protocol: Protocol, password: Option<&str>) -> ProxySpec {
    let mut spec = ProxySpec::new(kind(protocol), "127.0.0.1", proxy.addr.port());
    spec.credentials = password.map(|password| ProxyCredentials {
        user: LOGIN_USER.into(),
        password: Secret::new(password),
    });
    spec
}

/// A TCP echo server on its own thread; returns its address.
fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let (mut reader, mut writer) = (&stream, &stream);
                let _ = std::io::copy(&mut reader, &mut writer);
            });
        }
    });
    addr
}

fn echo_works(addr: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reply = [0u8; 4];
    stream.write_all(b"ping").is_ok() && stream.read_exact(&mut reply).is_ok() && &reply == b"ping"
}

fn route(proxy: ProxySpec, target: Target) -> ProxyRoute {
    let mut route = ProxyRoute::new(proxy);
    route.targets = vec![target];
    route.timeouts.connect = Duration::from_secs(2);
    route
}

fn open_route(route: ProxyRoute) -> Tunnel {
    Tunnel::open_proxy(route).unwrap()
}

fn db() -> Target {
    Target::new("db.internal.test", 5432)
}

/// The proxy resolves `db.internal.test` itself: the name reaches it unresolved.
fn proxy_for_db(protocol: Protocol, echo: SocketAddr, login: bool) -> TestProxy {
    let options = ProxyOptions::new(protocol).resolving("db.internal.test", 5432, echo);
    TestProxy::start(if login {
        options.with_login(LOGIN_USER, LOGIN_PASSWORD)
    } else {
        options
    })
}

#[test]
fn proxy_only_route_forwards_with_the_name_sent_to_the_proxy() {
    let echo = echo_server();
    for protocol in [Protocol::Socks5, Protocol::Http] {
        for login in [false, true] {
            let proxy = proxy_for_db(protocol, echo, login);
            let password = login.then_some(LOGIN_PASSWORD);
            let tunnel = open_route(route(proxy_spec(&proxy, protocol, password), db()));
            assert!(tunnel.is_alive());
            let local = tunnel.local_addr(&db()).unwrap();
            assert!(local.ip().is_loopback());
            assert_eq!(tunnel.local_addrs().count(), 1);
            assert_eq!(
                tunnel.via(),
                match protocol {
                    Protocol::Socks5 => format!("SOCKS5 proxy 127.0.0.1:{}", proxy.addr.port()),
                    Protocol::Http => format!("HTTP proxy 127.0.0.1:{}", proxy.addr.port()),
                }
            );
            assert_eq!(
                proxy.requests().len(),
                0,
                "nothing is dialled before a client connects"
            );
            assert!(echo_works(local));
            tunnel.probe(&db()).unwrap();
            let expected = match protocol {
                Protocol::Socks5 => "domain:db.internal.test:5432",
                Protocol::Http => "connect:db.internal.test:5432",
            };
            assert_eq!(
                proxy.requests(),
                [expected, expected],
                "{protocol:?} login={login}"
            );
            assert_eq!(proxy.tunnels(), 2);
            assert!(tunnel.last_forward_error(&db()).is_none());
        }
    }
}

#[test]
fn http_basic_auth_is_sent_only_with_credentials() {
    let echo = echo_server();
    let plain = proxy_for_db(Protocol::Http, echo, false);
    let tunnel = open_route(route(proxy_spec(&plain, Protocol::Http, None), db()));
    tunnel.probe(&db()).unwrap();
    assert!(plain.stats.authorizations.lock().unwrap().is_empty());

    let secured = proxy_for_db(Protocol::Http, echo, true);
    let tunnel = open_route(route(
        proxy_spec(&secured, Protocol::Http, Some(LOGIN_PASSWORD)),
        db(),
    ));
    tunnel.probe(&db()).unwrap();
    assert_eq!(
        *secured.stats.authorizations.lock().unwrap(),
        [format!(
            "Basic {}",
            BASE64.encode(format!("{LOGIN_USER}:{LOGIN_PASSWORD}"))
        )]
    );
}

#[test]
fn a_wrong_proxy_password_is_a_proxy_error_without_the_secret() {
    let echo = echo_server();
    for protocol in [Protocol::Socks5, Protocol::Http] {
        let proxy = proxy_for_db(protocol, echo, true);
        let tunnel = open_route(route(
            proxy_spec(&proxy, protocol, Some("WRONG-proxy-secret")),
            db(),
        ));
        let error = tunnel.probe(&db()).unwrap_err();
        assert_eq!(error.code(), "tunnel.proxy", "{error}");
        assert!(matches!(error, TunnelError::Proxy { .. }));
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("WRONG-proxy-secret") && !shown.contains(LOGIN_PASSWORD),
            "{shown}"
        );
        assert!(
            shown.contains(&format!("127.0.0.1:{}", proxy.addr.port())),
            "{shown}"
        );
        assert_eq!(proxy.stats.rejected_logins.load(Ordering::SeqCst), 1);

        // A database client that connects meanwhile is reset, and the tunnel says why.
        let mut client = TcpStream::connect(tunnel.local_addr(&db()).unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 1];
        assert!(matches!(client.read(&mut buf), Ok(0) | Err(_)));
        let recorded = tunnel
            .last_forward_error(&db())
            .expect("the failure is recorded");
        assert_eq!(recorded.code(), "tunnel.proxy");
        assert!(
            tunnel.is_alive(),
            "a refused forward does not end the tunnel"
        );
    }
}

#[test]
fn every_refusal_is_a_proxy_error_with_the_reason() {
    let echo = echo_server();
    for code in 1..=8u8 {
        let proxy = TestProxy::start(
            ProxyOptions::new(Protocol::Socks5)
                .resolving("db.internal.test", 5432, echo)
                .refusing(Refusal::Socks5(code)),
        );
        let tunnel = open_route(route(proxy_spec(&proxy, Protocol::Socks5, None), db()));
        let error = tunnel.probe(&db()).unwrap_err();
        assert_eq!(error.code(), "tunnel.proxy");
        assert!(
            error.to_string().contains("db.internal.test:5432"),
            "{error}"
        );
        assert!(
            !error.to_string().contains("unknown reply code"),
            "{code}: {error}"
        );
    }
    for status in [
        "400 Bad Request",
        "403 Forbidden",
        "502 Bad Gateway",
        "503 Service Unavailable",
    ] {
        let proxy =
            TestProxy::start(ProxyOptions::new(Protocol::Http).refusing(Refusal::Http(status)));
        let tunnel = open_route(route(proxy_spec(&proxy, Protocol::Http, None), db()));
        let error = tunnel.probe(&db()).unwrap_err();
        assert_eq!(error.code(), "tunnel.proxy");
        assert!(
            error.to_string().contains(&format!("HTTP/1.1 {status}")),
            "{error}"
        );
    }
}

#[test]
fn a_proxy_that_never_answers_times_out_and_one_that_is_down_is_a_proxy_error() {
    for protocol in [Protocol::Socks5, Protocol::Http] {
        let silent = TestProxy::start(ProxyOptions::new(protocol).refusing(Refusal::Silent));
        let mut slow = route(proxy_spec(&silent, protocol, None), db());
        slow.timeouts.connect = Duration::from_millis(600);
        let tunnel = open_route(slow);
        let started = Instant::now();
        let error = tunnel.probe(&db()).unwrap_err();
        assert_eq!(error.code(), "tunnel.timeout", "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let tunnel = open_route(route(
        ProxySpec::new(ProxyKind::Socks5, "127.0.0.1", closed),
        db(),
    ));
    let error = tunnel.probe(&db()).unwrap_err();
    assert_eq!(error.code(), "tunnel.proxy");
    assert!(
        error.to_string().contains("cannot connect to the proxy"),
        "{error}"
    );
}

#[test]
fn the_forward_cap_and_teardown_apply_to_a_proxy_route() {
    let echo = echo_server();
    let proxy = proxy_for_db(Protocol::Socks5, echo, false);
    let mut limited = route(proxy_spec(&proxy, Protocol::Socks5, None), db());
    limited.max_concurrent_forwards = 1;
    let tunnel = open_route(limited);
    let local = tunnel.local_addr(&db()).unwrap();

    let mut first = TcpStream::connect(local).unwrap();
    first
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    first.write_all(b"ping").unwrap();
    let mut reply = [0u8; 4];
    first.read_exact(&mut reply).unwrap();

    // The second connection is over the cap: reset, and the reason is recorded.
    let mut second = TcpStream::connect(local).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 1];
    assert!(matches!(second.read(&mut buf), Ok(0) | Err(_)));
    assert!(eventually(Duration::from_secs(5), || matches!(
        tunnel.last_forward_error(&db()),
        Some(TunnelError::ForwardRefused { .. })
    )));

    // When the first one ends there is room again.
    drop(first);
    assert!(eventually(Duration::from_secs(5), || echo_works(local)));

    drop(tunnel);
    assert!(
        eventually(Duration::from_secs(5), || TcpStream::connect_timeout(
            &local,
            Duration::from_millis(300)
        )
        .is_err()),
        "dropping the tunnel closes its listener"
    );
}

/// A server that sends `total` patterned bytes to whoever connects, then keeps the connection open.
fn flood_server(total: usize, sent: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let sent = sent.clone();
            thread::spawn(move || {
                let chunk: Vec<u8> = (0..64 * 1024)
                    .map(|i| support::flood_byte(i as u64))
                    .collect();
                let mut done = 0;
                while done < total {
                    let n = chunk.len().min(total - done);
                    if stream.write_all(&chunk[..n]).is_err() {
                        return;
                    }
                    done += n;
                    sent.store(done, Ordering::SeqCst);
                }
                thread::sleep(Duration::from_secs(30));
            });
        }
    });
    addr
}

#[test]
fn bulk_data_arrives_intact_through_a_proxy_route() {
    let sent = Arc::new(AtomicUsize::new(0));
    let total = 6 << 20;
    let source = flood_server(total, sent);
    let proxy = TestProxy::start(ProxyOptions::new(Protocol::Http).resolving(
        "db.internal.test",
        5432,
        source,
    ));
    let tunnel = open_route(route(proxy_spec(&proxy, Protocol::Http, None), db()));
    let mut client = TcpStream::connect(tunnel.local_addr(&db()).unwrap()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let (mut received, mut buf) = (0usize, vec![0u8; 256 * 1024]);
    let pattern: Vec<u8> = (0..64 * 1024)
        .map(|i| support::flood_byte(i as u64))
        .collect();
    while received < total {
        let n = client.read(&mut buf).unwrap();
        assert!(n > 0, "closed after {received} bytes");
        for (i, byte) in buf[..n].iter().enumerate() {
            assert_eq!(
                *byte,
                pattern[(received + i) % pattern.len()],
                "offset {}",
                received + i
            );
        }
        received += n;
    }
}

#[test]
fn a_reader_that_never_reads_is_cut_off_after_the_stall_time_with_bounded_memory() {
    let sent = Arc::new(AtomicUsize::new(0));
    let source = flood_server(64 << 20, sent.clone());
    let proxy = TestProxy::start(ProxyOptions::new(Protocol::Socks5).resolving(
        "db.internal.test",
        5432,
        source,
    ));
    let mut limited = route(proxy_spec(&proxy, Protocol::Socks5, None), db());
    limited.forward_buffer_bytes = 64 << 10;
    limited.tunnel_buffer_bytes = 64 << 10;
    limited.timeouts.forward_stall = Duration::from_secs(1);
    let tunnel = open_route(limited);
    let _client = TcpStream::connect(tunnel.local_addr(&db()).unwrap()).unwrap();
    assert!(
        eventually(Duration::from_secs(15), || matches!(
            tunnel.last_forward_error(&db()),
            Some(TunnelError::ForwardStalled { .. })
        )),
        "the stalled forward is closed"
    );
    assert!(
        tunnel.peak_buffered_bytes() < 1 << 20,
        "peak {}",
        tunnel.peak_buffered_bytes()
    );
    // Backpressure: the source was not drained far beyond the buffers plus the socket buffers.
    assert!(
        sent.load(Ordering::SeqCst) < 48 << 20,
        "sent {}",
        sent.load(Ordering::SeqCst)
    );
}

#[test]
fn validation_covers_the_proxy_route() {
    let proxy = ProxySpec::new(ProxyKind::Socks5, "127.0.0.1", 1080);
    for (edit, needle) in [
        (
            Box::new(|r: &mut ProxyRoute| r.targets.clear()) as Box<dyn Fn(&mut ProxyRoute)>,
            "target",
        ),
        (Box::new(|r| r.proxy.port = 0), "proxy port"),
        (Box::new(|r| r.proxy.host = "a b".into()), "proxy host"),
        (
            Box::new(|r| r.timeouts.connect = Duration::ZERO),
            "timeouts",
        ),
        (
            Box::new(|r| r.forward_buffer_bytes = 1),
            "forward_buffer_bytes",
        ),
        (Box::new(|r| r.targets[0].port = 0), "port"),
    ] {
        let mut bad = route(proxy.clone(), db());
        edit(&mut bad);
        let error = Tunnel::open_proxy(bad).unwrap_err();
        assert_eq!(error.code(), "tunnel.config");
        assert!(error.to_string().contains(needle), "{needle}: {error}");
    }
}

#[test]
fn open_proxy_async_works_on_a_foreign_executor() {
    let echo = echo_server();
    let proxy = proxy_for_db(Protocol::Socks5, echo, false);
    let spec = proxy_spec(&proxy, Protocol::Socks5, None);
    let tunnel = block_on(Tunnel::open_proxy_async(route(spec, db()))).unwrap();
    assert!(echo_works(tunnel.local_addr(&db()).unwrap()));
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        thread::park();
    }
}

// ---- SSH through a proxy ---------------------------------------------------------------------------------

fn ssh_server() -> TestServer {
    TestServer::start(ServerOptions {
        plan: AuthPlan::Password {
            password: "ssh-pw".into(),
        },
        relay: true,
        ..ServerOptions::default()
    })
}

/// A spec for `bastion.test:<port>`, a name only the proxy can resolve.
fn ssh_spec(dir: &std::path::Path, server: &TestServer, proxy: ProxySpec) -> TunnelSpec {
    let mut spec = TunnelSpec::new(
        "bastion.test",
        "tester",
        Auth::Password(Secret::new("ssh-pw")),
        dir.join("app_known_hosts"),
    );
    spec.port = server.addr.port();
    spec.user_known_hosts = None;
    spec.system_known_hosts = None;
    spec.targets = vec![Target::new("echo.example.com", PORT_ECHO)];
    spec.timeouts.connect = Duration::from_secs(2);
    spec.proxy = Some(proxy);
    trust_host_key(&spec, "bastion.test", spec.port, &server.host_public_key).unwrap();
    spec
}

#[test]
fn ssh_goes_through_a_proxy_with_and_without_a_login() {
    for protocol in [Protocol::Socks5, Protocol::Http] {
        for login in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let server = ssh_server();
            let options = ProxyOptions::new(protocol).resolving(
                "bastion.test",
                server.addr.port(),
                server.addr,
            );
            let proxy = TestProxy::start(if login {
                options.with_login(LOGIN_USER, LOGIN_PASSWORD)
            } else {
                options
            });
            let spec = ssh_spec(
                dir.path(),
                &server,
                proxy_spec(&proxy, protocol, login.then_some(LOGIN_PASSWORD)),
            );
            let tunnel = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap();
            assert!(tunnel.is_alive());
            assert!(echo_works(
                tunnel
                    .local_addr(&Target::new("echo.example.com", PORT_ECHO))
                    .unwrap()
            ));
            let asked = match protocol {
                Protocol::Socks5 => format!("domain:bastion.test:{}", server.addr.port()),
                Protocol::Http => format!("connect:bastion.test:{}", server.addr.port()),
            };
            assert_eq!(
                proxy.requests(),
                [asked],
                "the SSH connection is the only thing sent through"
            );
            assert_eq!(
                tunnel.via(),
                format!("tester@bastion.test:{}", server.addr.port())
            );
        }
    }
}

#[test]
fn a_wrong_proxy_password_stops_ssh_before_the_server_is_contacted() {
    let dir = tempfile::tempdir().unwrap();
    let server = ssh_server();
    for protocol in [Protocol::Socks5, Protocol::Http] {
        let proxy = TestProxy::start(
            ProxyOptions::new(protocol)
                .resolving("bastion.test", server.addr.port(), server.addr)
                .with_login(LOGIN_USER, LOGIN_PASSWORD),
        );
        let spec = ssh_spec(
            dir.path(),
            &server,
            proxy_spec(&proxy, protocol, Some("WRONG-proxy-secret")),
        );
        let error = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap_err();
        assert_eq!(error.code(), "tunnel.proxy", "{error}");
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("WRONG-proxy-secret") && !shown.contains("ssh-pw"),
            "{shown}"
        );
        assert!(
            shown.contains("bastion.test"),
            "the proxy error names what was asked for: {shown}"
        );
        assert_eq!(proxy.stats.rejected_logins.load(Ordering::SeqCst), 1);
    }
    assert_eq!(server.stats.connections.load(Ordering::SeqCst), 0);
}

#[test]
fn a_proxy_that_refuses_the_ssh_connection_or_stays_silent() {
    let dir = tempfile::tempdir().unwrap();
    let server = ssh_server();
    let refusing =
        TestProxy::start(ProxyOptions::new(Protocol::Socks5).refusing(Refusal::Socks5(2)));
    let spec = ssh_spec(
        dir.path(),
        &server,
        proxy_spec(&refusing, Protocol::Socks5, None),
    );
    let error = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap_err();
    assert_eq!(error.code(), "tunnel.proxy");
    assert!(error.to_string().contains("rule set"), "{error}");

    let silent = TestProxy::start(ProxyOptions::new(Protocol::Http).refusing(Refusal::Silent));
    let mut spec = ssh_spec(
        dir.path(),
        &server,
        proxy_spec(&silent, Protocol::Http, None),
    );
    spec.timeouts.connect = Duration::from_millis(700);
    let started = Instant::now();
    let error = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap_err();
    assert_eq!(error.code(), "tunnel.timeout", "{error}");
    assert!(
        error.to_string().contains("proxy connect"),
        "the message names the proxy phase, not SSH: {error}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// A proxy negotiation that is still waiting for the proxy ends with the tunnel, not after its own timeout.
#[test]
fn dropping_the_tunnel_ends_a_negotiation_in_flight() {
    let silent = TestProxy::start(ProxyOptions::new(Protocol::Socks5).refusing(Refusal::Silent));
    let mut slow = route(proxy_spec(&silent, Protocol::Socks5, None), db());
    slow.timeouts.connect = Duration::from_secs(60);
    let tunnel = open_route(slow);
    let mut client = TcpStream::connect(tunnel.local_addr(&db()).unwrap()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert!(
        eventually(Duration::from_secs(5), || silent
            .stats
            .connections
            .load(Ordering::SeqCst)
            == 1),
        "the forward reached the proxy and waits for its answer"
    );
    let started = Instant::now();
    drop(tunnel);
    let mut byte = [0u8; 1];
    assert!(matches!(client.read(&mut byte), Ok(0) | Err(_)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// The proxy carries the TCP connection to the first hop only; later hops ride inside the SSH channels.
#[test]
fn the_proxy_carries_only_the_first_hop_of_a_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (jump_server, bastion) = (ssh_server(), ssh_server());
    let proxy = TestProxy::start(ProxyOptions::new(Protocol::Socks5).resolving(
        "bastion.test",
        jump_server.addr.port(),
        jump_server.addr,
    ));
    let mut spec = ssh_spec(
        dir.path(),
        &jump_server,
        proxy_spec(&proxy, Protocol::Socks5, None),
    );
    // `ssh_spec` made the jump server the "bastion.test" host; make it the jump and add the real bastion.
    let mut jump = Jump::new(
        "bastion.test",
        "hopper",
        Auth::Password(Secret::new("ssh-pw")),
    );
    jump.port = jump_server.addr.port();
    spec.jumps = vec![jump];
    spec.host = "127.0.0.1".into();
    spec.port = bastion.addr.port();
    trust_host_key(
        &spec,
        "127.0.0.1",
        bastion.addr.port(),
        &bastion.host_public_key,
    )
    .unwrap();
    let tunnel = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap();
    assert!(echo_works(
        tunnel
            .local_addr(&Target::new("echo.example.com", PORT_ECHO))
            .unwrap()
    ));
    assert_eq!(
        proxy.requests(),
        [format!("domain:bastion.test:{}", jump_server.addr.port())]
    );
    assert_eq!(bastion.stats.connections.load(Ordering::SeqCst), 1);
    // A hop's failure through the proxy is still named by hop.
    let mut wrong = Jump::new(
        "bastion.test",
        "hopper",
        Auth::Password(Secret::new("WRONG-hop-secret")),
    );
    wrong.port = jump_server.addr.port();
    let mut spec = ssh_spec(
        dir.path(),
        &jump_server,
        proxy_spec(&proxy, Protocol::Socks5, None),
    );
    spec.jumps = vec![wrong];
    spec.host = "127.0.0.1".into();
    spec.port = bastion.addr.port();
    let error = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("jump 1 of 1 hopper@bastion.test:"),
        "{error}"
    );
    assert!(!error.to_string().contains("WRONG-hop-secret"));
}
