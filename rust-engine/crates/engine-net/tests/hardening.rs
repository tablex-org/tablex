//! Security and robustness tests against an in-process SSH server (no Docker, no network beyond loopback).

// Errors are cold-path values; their size does not matter here.
#![allow(clippy::result_large_err)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use engine_net::{
    trust_host_key, Auth, AuthError, KeyboardInteractiveRequest, NoPrompts, Prompts, Secret,
    Target, Tunnel, TunnelError, TunnelSpec,
};
use support::{
    eventually, flood_byte, generate_key, poll_until_abandoned, AuthPlan, HostKey, ServerOptions,
    Stats, TestServer, PORT_DELAYED, PORT_ECHO, PORT_FLOOD, PORT_FLOOD_SMALL, PORT_SINK_STALL,
};

fn echo() -> Target {
    Target::new("echo.example.com", PORT_ECHO)
}
fn flood() -> Target {
    Target::new("flood.example.com", PORT_FLOOD)
}
fn flood_small() -> Target {
    Target::new("small.example.com", PORT_FLOOD_SMALL)
}
fn delayed() -> Target {
    Target::new("delayed.example.com", PORT_DELAYED)
}
fn stall() -> Target {
    Target::new("stall.example.com", PORT_SINK_STALL)
}

fn spec_for(server: &TestServer, dir: &Path, auth: Auth) -> TunnelSpec {
    let mut spec = TunnelSpec::new("127.0.0.1", "tester", auth, dir.join("app_known_hosts"));
    spec.port = server.addr.port();
    spec.user_known_hosts = None;
    spec.system_known_hosts = None;
    spec.targets = vec![echo(), flood(), flood_small(), delayed(), stall()];
    spec
}

fn password_auth() -> Auth {
    Auth::Password(Secret::new("pw"))
}

/// Opens with the server's host key already trusted (through the real `trust_host_key`).
fn open_trusted(
    server: &TestServer,
    spec: &TunnelSpec,
    prompts: Arc<dyn Prompts>,
) -> Result<Tunnel, TunnelError> {
    trust_host_key(spec, &spec.host, spec.port, &server.host_public_key).unwrap();
    Tunnel::open(spec.clone(), prompts)
}

fn err(result: Result<Tunnel, TunnelError>) -> TunnelError {
    match result {
        Ok(tunnel) => panic!("expected an error, got {tunnel:?}"),
        Err(error) => error,
    }
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
}

fn echo_round_trip(addr: SocketAddr) -> Duration {
    let started = Instant::now();
    let mut stream = connect(addr);
    stream.write_all(b"ping").unwrap();
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"ping");
    started.elapsed()
}

/// A prompt callback that answers every prompt with a fixed string and records what it was asked.
struct Scripted {
    answer: Option<String>,
    asked: Mutex<Vec<Vec<(String, bool)>>>,
}

impl Scripted {
    fn new(answer: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.map(str::to_owned),
            asked: Mutex::default(),
        })
    }
    fn calls(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl Prompts for Scripted {
    fn keyboard_interactive(&self, request: &KeyboardInteractiveRequest) -> Option<Vec<Secret>> {
        self.asked.lock().unwrap().push(
            request
                .prompts
                .iter()
                .map(|p| (p.text.clone(), p.echo))
                .collect(),
        );
        let answer = self.answer.clone()?;
        Some(
            request
                .prompts
                .iter()
                .map(|_| Secret::new(answer.clone()))
                .collect(),
        )
    }
}

// ------------------------------------------------------------------------------------------------ B1

/// The downgrade attack: the host is recorded with an ed25519 key, the server only presents RSA.
#[test]
fn recorded_ed25519_host_presenting_only_rsa_is_not_plain_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        host_key: HostKey::Rsa,
        ..ServerOptions::default()
    });
    let spec = spec_for(&server, dir.path(), password_auth());
    let (_, recorded_ed25519) = generate_key(dir.path(), "recorded", "ed25519");
    trust_host_key(&spec, &spec.host, spec.port, &recorded_ed25519).unwrap();
    let before = std::fs::read_to_string(&spec.app_known_hosts).unwrap();

    let error = err(Tunnel::open(spec.clone(), Arc::new(NoPrompts)));
    let TunnelError::HostKeyTypeChanged {
        presented_algorithm,
        recorded_algorithms,
        ..
    } = &error
    else {
        panic!("expected HostKeyTypeChanged, got {error:?}");
    };
    assert_eq!(presented_algorithm, "ssh-rsa");
    assert_eq!(recorded_algorithms, &["ssh-ed25519".to_string()]);
    assert_eq!(error.code(), "tunnel.host_key_mismatch");

    // Not one-click trustable: the attacker's key must not be appended.
    let refused =
        trust_host_key(&spec, &spec.host, spec.port, &server.host_public_key).unwrap_err();
    assert!(
        matches!(refused, TunnelError::HostKeyTypeChanged { .. }),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&spec.app_known_hosts).unwrap(),
        before
    );
    assert_eq!(
        Stats::get(&server.stats.auth_requests),
        0,
        "no credential may reach an unverified host"
    );
}

// ----------------------------------------------------------------------------------------------- N17

#[test]
fn no_userauth_request_is_sent_when_host_key_is_unknown_or_changed() {
    let dir = tempfile::tempdir().unwrap();

    // Unknown host.
    let server = TestServer::start(ServerOptions::default());
    let spec = spec_for(&server, dir.path(), password_auth());
    let error = err(Tunnel::open(spec.clone(), Arc::new(NoPrompts)));
    assert!(
        matches!(&error, TunnelError::HostKeyUnknown { public_key, .. } if *public_key == server.host_public_key),
        "{error:?}"
    );

    // Changed key (same type).
    let (_, other_ed25519) = generate_key(dir.path(), "other", "ed25519");
    trust_host_key(&spec, &spec.host, spec.port, &other_ed25519).unwrap();
    let error = err(Tunnel::open(spec.clone(), Arc::new(NoPrompts)));
    assert!(
        matches!(error, TunnelError::HostKeyMismatch { .. }),
        "{error:?}"
    );

    // Revoked key.
    let revoked_file = dir.path().join("user_known_hosts");
    std::fs::write(
        &revoked_file,
        format!(
            "@revoked [127.0.0.1]:{} {}\n",
            server.addr.port(),
            server.host_public_key
        ),
    )
    .unwrap();
    let mut revoked_spec = spec.clone();
    revoked_spec.user_known_hosts = Some(revoked_file);
    let error = err(Tunnel::open(revoked_spec, Arc::new(NoPrompts)));
    assert!(
        matches!(error, TunnelError::HostKeyRevoked { .. }),
        "{error:?}"
    );

    assert_eq!(
        Stats::get(&server.stats.auth_started),
        0,
        "the client must not start user authentication"
    );
    assert_eq!(
        Stats::get(&server.stats.auth_requests),
        0,
        "not a single auth request"
    );
    assert!(server.stats.passwords.lock().unwrap().is_empty());
    assert!(eventually(Duration::from_secs(3), || Stats::get(
        &server.stats.connections_closed
    ) == 3));
}

#[test]
fn system_known_hosts_are_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions::default());
    let mut spec = spec_for(&server, dir.path(), password_auth());
    let system = dir.path().join("ssh_known_hosts");
    std::fs::write(
        &system,
        format!(
            "[127.0.0.1]:{} {}\n",
            server.addr.port(),
            server.host_public_key
        ),
    )
    .unwrap();
    spec.system_known_hosts = Some(system);
    let tunnel =
        Tunnel::open(spec.clone(), Arc::new(NoPrompts)).expect("trusted through the system file");
    assert!(echo_round_trip(tunnel.local_addr(&echo()).unwrap()) < Duration::from_secs(5));
    assert!(!spec.app_known_hosts.exists());
}

// ------------------------------------------------------------------------------------------------ B3

/// Server with only keyboard-interactive and three rounds that all ask for a "Password:". The stored password
/// answers the first round only; the callback answers the rest.
#[test]
fn stored_password_is_used_at_most_once_across_ki_rounds() {
    let dir = tempfile::tempdir().unwrap();
    let round = vec![("Password: ".to_string(), false)];
    let server = TestServer::start(ServerOptions {
        plan: AuthPlan::Ki {
            rounds: vec![round.clone(), round.clone(), round],
            accept_last: "from-callback".into(),
        },
        ..ServerOptions::default()
    });
    let spec = spec_for(
        &server,
        dir.path(),
        Auth::Password(Secret::new("stored-secret")),
    );
    let prompts = Scripted::new(Some("from-callback"));
    open_trusted(&server, &spec, prompts.clone()).expect("opens");
    let seen = server.stats.ki_responses.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            vec!["stored-secret".to_string()],
            vec!["from-callback".into()],
            vec!["from-callback".into()]
        ]
    );
    assert_eq!(prompts.calls(), 2, "rounds 2 and 3 go to the callback");
}

/// `password` is a first factor (partial success); the second factor prompt even looks like a password prompt
/// but must be answered by the callback, never by the stored password.
#[test]
fn password_partial_success_second_factor_goes_to_callback() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        plan: AuthPlan::PasswordThenKi {
            password: "stored-secret".into(),
            prompt: ("Verification password: ".into(), false),
            answer: "123456".into(),
        },
        ..ServerOptions::default()
    });
    let spec = spec_for(
        &server,
        dir.path(),
        Auth::Password(Secret::new("stored-secret")),
    );
    let prompts = Scripted::new(Some("123456"));
    open_trusted(&server, &spec, prompts.clone()).expect("opens");
    assert_eq!(
        *server.stats.passwords.lock().unwrap(),
        vec!["stored-secret".to_string()]
    );
    assert_eq!(
        *server.stats.ki_responses.lock().unwrap(),
        vec![vec!["123456".to_string()]]
    );
    assert_eq!(prompts.calls(), 1);

    // Without a callback the second factor fails typed, and the password is not tried again.
    let server = TestServer::start(ServerOptions {
        plan: AuthPlan::PasswordThenKi {
            password: "stored-secret".into(),
            prompt: ("Verification password: ".into(), false),
            answer: "123456".into(),
        },
        ..ServerOptions::default()
    });
    let spec = spec_for(
        &server,
        dir.path(),
        Auth::Password(Secret::new("stored-secret")),
    );
    std::fs::remove_file(&spec.app_known_hosts).ok();
    let error = err(open_trusted(&server, &spec, Arc::new(NoPrompts)));
    assert!(
        matches!(error, TunnelError::Auth(AuthError::PromptDeclined)),
        "{error:?}"
    );
    assert!(
        server.stats.ki_responses.lock().unwrap().is_empty(),
        "nothing may answer the second factor"
    );
}

#[test]
fn echo_true_password_prompt_is_not_autofilled() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        plan: AuthPlan::Ki {
            rounds: vec![vec![("Password: ".into(), true)]],
            accept_last: "from-callback".into(),
        },
        ..ServerOptions::default()
    });
    let spec = spec_for(
        &server,
        dir.path(),
        Auth::Password(Secret::new("stored-secret")),
    );
    let prompts = Scripted::new(Some("from-callback"));
    open_trusted(&server, &spec, prompts.clone()).expect("opens");
    assert_eq!(
        *server.stats.ki_responses.lock().unwrap(),
        vec![vec!["from-callback".to_string()]]
    );
    assert_eq!(prompts.calls(), 1);
}

// ------------------------------------------------------------------------------------------------ N7

#[test]
fn session_dying_mid_auth_is_not_reported_as_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        plan: AuthPlan::DropOnPassword,
        ..ServerOptions::default()
    });
    let spec = spec_for(&server, dir.path(), password_auth());
    let error = err(open_trusted(&server, &spec, Arc::new(NoPrompts)));
    assert!(matches!(error, TunnelError::Connect { .. }), "{error:?}");
    assert_eq!(error.code(), "tunnel.connect");
}

#[test]
fn wrong_password_is_rejected_after_exactly_one_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions::default());
    let spec = spec_for(&server, dir.path(), Auth::Password(Secret::new("wrong")));
    let error = err(open_trusted(&server, &spec, Arc::new(NoPrompts)));
    assert!(
        matches!(error, TunnelError::Auth(AuthError::Rejected { .. })),
        "{error:?}"
    );
    assert_eq!(server.stats.passwords.lock().unwrap().len(), 1);
}

// ------------------------------------------------------------------------------------------------ N8

struct WaitForCancel {
    asked: AtomicBool,
    saw_cancel: AtomicBool,
}

impl Prompts for WaitForCancel {
    fn keyboard_interactive(&self, request: &KeyboardInteractiveRequest) -> Option<Vec<Secret>> {
        self.asked.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if request.cancel.is_cancelled() {
                self.saw_cancel.store(true, Ordering::SeqCst);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

#[test]
fn open_async_dropped_future_closes_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        plan: AuthPlan::Ki {
            rounds: vec![vec![("Code: ".into(), false)]],
            accept_last: "x".into(),
        },
        ..ServerOptions::default()
    });
    let spec = spec_for(&server, dir.path(), Auth::KeyboardInteractive);
    trust_host_key(&spec, &spec.host, spec.port, &server.host_public_key).unwrap();
    let prompts = Arc::new(WaitForCancel {
        asked: AtomicBool::new(false),
        saw_cancel: AtomicBool::new(false),
    });

    let abandon = Arc::new(AtomicBool::new(false));
    let worker = {
        let (abandon, prompts) = (abandon.clone(), prompts.clone());
        thread::spawn(move || {
            poll_until_abandoned(Tunnel::open_async(spec, prompts), &abandon).map(|r| r.is_ok())
        })
    };
    assert!(
        eventually(Duration::from_secs(10), || prompts
            .asked
            .load(Ordering::SeqCst)),
        "the prompt was never shown"
    );
    abandon.store(true, Ordering::SeqCst); // the UI user pressed Cancel: the future is dropped
    assert_eq!(worker.join().unwrap(), None, "the future was abandoned");

    assert!(
        eventually(Duration::from_secs(5), || prompts
            .saw_cancel
            .load(Ordering::SeqCst)),
        "callback must be told"
    );
    assert!(
        eventually(Duration::from_secs(5), || Stats::get(
            &server.stats.connections_closed
        ) == 1),
        "the SSH connection must be closed"
    );
    assert_eq!(Stats::get(&server.stats.auth_success), 0);
}

// ------------------------------------------------------------------------------------- N12 / forwards

#[test]
fn forward_cap_refuses_beyond_the_limit_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions::default());
    let mut spec = spec_for(&server, dir.path(), password_auth());
    spec.max_concurrent_forwards = 2;
    let tunnel = open_trusted(&server, &spec, Arc::new(NoPrompts)).unwrap();
    let addr = tunnel.local_addr(&echo()).unwrap();

    let mut held = Vec::new();
    for _ in 0..2 {
        let mut stream = connect(addr);
        stream.write_all(b"ping").unwrap();
        stream.read_exact(&mut [0u8; 4]).unwrap();
        held.push(stream);
    }
    let mut third = connect(addr);
    let mut byte = [0u8; 1];
    assert!(
        matches!(third.read(&mut byte), Ok(0) | Err(_)),
        "the third forward must be refused"
    );
    let refusal = tunnel
        .last_forward_error(&echo())
        .expect("refusal recorded");
    assert!(
        matches!(&refusal, TunnelError::ForwardRefused { reason, .. } if reason.contains("too many")),
        "{refusal:?}"
    );
    assert_eq!(refusal.code(), "tunnel.forward_refused");

    drop(held.pop());
    assert!(
        eventually(Duration::from_secs(5), || TcpStream::connect(addr)
            .and_then(|mut s| {
                s.set_read_timeout(Some(Duration::from_secs(2)))?;
                s.write_all(b"ping")?;
                s.read_exact(&mut [0u8; 4])
            })
            .is_ok()),
        "a slot must free up when a forward ends"
    );
}

#[test]
fn session_down_closes_listeners_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions::default());
    let tunnel = open_trusted(
        &server,
        &spec_for(&server, dir.path(), password_auth()),
        Arc::new(NoPrompts),
    )
    .unwrap();
    let addr = tunnel.local_addr(&echo()).unwrap();
    assert!(tunnel.is_alive());
    server.drop_connections();
    assert!(
        eventually(Duration::from_secs(3), || !tunnel.is_alive()),
        "transport loss must be noticed"
    );
    assert!(
        eventually(Duration::from_secs(3), || TcpStream::connect_timeout(
            &addr,
            Duration::from_millis(500)
        )
        .is_err()),
        "the local listener must close"
    );
}

/// Reads `total` flood bytes from `stream` (optionally throttled), verifying every byte. Returns when done.
fn read_flood(stream: &mut TcpStream, total: usize, chunk: usize, pause: Duration) {
    let mut buf = vec![0u8; chunk];
    let mut offset = 0usize;
    while offset < total {
        let n = stream.read(&mut buf).expect("the forward must stay open");
        assert!(n > 0, "forward closed after {offset} of {total} bytes");
        assert!(
            buf[..n]
                .iter()
                .enumerate()
                .all(|(i, b)| *b == flood_byte((offset + i) as u64)),
            "corruption near byte {offset}"
        );
        offset += n;
        if !pause.is_zero() {
            thread::sleep(pause);
        }
    }
}

/// A local client that stops reading must not freeze the other forwards while it is below its budget; once it
/// is at budget and has made no progress for the stall timeout, it alone is closed (`forward_stalled`).
#[test]
fn one_stalled_forward_does_not_block_another() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions::default());
    let mut spec = spec_for(&server, dir.path(), password_auth());
    spec.forward_buffer_bytes = 16 << 20;
    spec.timeouts.forward_stall = Duration::from_secs(2);
    let tunnel = open_trusted(&server, &spec, Arc::new(NoPrompts)).unwrap();
    let echo_addr = tunnel.local_addr(&echo()).unwrap();

    // Phase 1: A receives 4 MiB (below its 16 MiB budget) and never reads. B is unaffected.
    let mut below_budget = connect(tunnel.local_addr(&flood_small()).unwrap());
    assert!(
        eventually(Duration::from_secs(20), || Stats::get(
            &server.stats.flood_sent
        ) >= 4 << 20),
        "the server must be able to deliver all 4 MiB without the client reading"
    );
    let slowest = (0..20).map(|_| echo_round_trip(echo_addr)).max().unwrap();
    assert!(
        slowest < Duration::from_secs(2),
        "another forward stalled for {slowest:?} while A was below budget"
    );
    assert!(tunnel.last_forward_error(&flood_small()).is_none());

    // Phase 2: C is flooded far past its budget and never reads. Backpressure first, then only C is closed.
    let mut at_budget = connect(tunnel.local_addr(&flood()).unwrap());
    assert!(
        eventually(Duration::from_secs(30), || tunnel
            .last_forward_error(&flood())
            .is_some()),
        "the abandoned reader must be closed after the stall timeout"
    );
    let error = tunnel.last_forward_error(&flood()).unwrap();
    assert!(
        matches!(
            error,
            TunnelError::ForwardStalled {
                port: PORT_FLOOD,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.code(), "tunnel.forward_stalled");

    let slowest = (0..10).map(|_| echo_round_trip(echo_addr)).max().unwrap();
    assert!(
        slowest < Duration::from_secs(3),
        "B must continue after C was closed ({slowest:?})"
    );
    assert!(tunnel.is_alive());

    // C is closed (its buffered bytes drain, then EOF) ...
    at_budget
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut sink = vec![0u8; 1 << 20];
    let started = Instant::now();
    loop {
        match at_budget.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => assert!(
                started.elapsed() < Duration::from_secs(30),
                "forward C never ended"
            ),
        }
    }
    // ... while A, which was never at budget, is still open and still holds all its data.
    read_flood(&mut below_budget, 4 << 20, 64 << 10, Duration::ZERO);
}

// The "dead reader holds the whole tunnel budget" scenario lives in tests/live.rs
// (`tunnel_budget_held_by_a_dead_reader_never_wedges_the_tunnel`): it needs a real OpenSSH peer. The in-process
// russh *server* freezes its own outbound side when a channel is closed while it still holds window-blocked
// data for it (the russh defect described at `outbound_liveness` in src/tunnel.rs), which made the test flaky.

/// A reader that is slow but alive is never closed: every byte arrives, in order, even though the sender is
/// much faster and the buffer is tiny (so backpressure is exercised throughout).
#[test]
fn slow_but_live_reader_is_never_closed() {
    const TOTAL: usize = 64 << 20;
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        flood_total: TOTAL,
        ..ServerOptions::default()
    });
    let mut spec = spec_for(&server, dir.path(), password_auth());
    spec.forward_buffer_bytes = 2 << 20;
    spec.timeouts.forward_stall = Duration::from_secs(2);
    let tunnel = open_trusted(&server, &spec, Arc::new(NoPrompts)).unwrap();

    let started = Instant::now();
    let mut stream = connect(tunnel.local_addr(&flood()).unwrap());
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    read_flood(&mut stream, TOTAL, 16 << 10, Duration::from_millis(1));
    println!("slow reader: {TOTAL} bytes in {:?}", started.elapsed());
    assert!(
        tunnel.last_forward_error(&flood()).is_none(),
        "{:?}",
        tunnel.last_forward_error(&flood())
    );
    assert!(tunnel.is_alive());
    assert!(
        tunnel.peak_buffered_bytes() >= (2 << 20) - (64 << 10),
        "the buffer must have filled (backpressure was exercised); peak {}",
        tunnel.peak_buffered_bytes()
    );
}

/// Buffered bytes never exceed the budget, however long the reader pauses, and nothing is lost after it resumes.
#[test]
fn backpressure_bounds_memory() {
    const TOTAL: usize = 32 << 20;
    const BUDGET: usize = 1 << 20;
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        flood_total: TOTAL,
        ..ServerOptions::default()
    });
    let mut spec = spec_for(&server, dir.path(), password_auth());
    spec.forward_buffer_bytes = BUDGET;
    spec.tunnel_buffer_bytes = BUDGET;
    spec.timeouts.forward_stall = Duration::from_secs(10);
    let tunnel = open_trusted(&server, &spec, Arc::new(NoPrompts)).unwrap();

    let mut stream = connect(tunnel.local_addr(&flood()).unwrap());
    assert!(
        eventually(Duration::from_secs(20), || tunnel.buffered_bytes()
            >= BUDGET - (64 << 10)),
        "the buffer should fill while the client does not read (at {})",
        tunnel.buffered_bytes()
    );
    thread::sleep(Duration::from_millis(1500)); // the sender keeps pushing; the buffer must not grow
    assert!(
        tunnel.peak_buffered_bytes() <= BUDGET,
        "peak {} > budget {BUDGET}",
        tunnel.peak_buffered_bytes()
    );
    assert!(
        Stats::get(&server.stats.flood_sent) < TOTAL,
        "the sender must have been slowed down"
    );

    read_flood(&mut stream, TOTAL, 64 << 10, Duration::ZERO);
    assert!(
        tunnel.peak_buffered_bytes() <= BUDGET,
        "peak {} > budget {BUDGET}",
        tunnel.peak_buffered_bytes()
    );
    assert!(tunnel.last_forward_error(&flood()).is_none());
}

/// A late-confirmed channel (the server confirms after the forward timeout) is closed, not leaked.
#[test]
fn forward_open_timeout_closes_late_channel() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        open_delay: Duration::from_millis(2500),
        ..ServerOptions::default()
    });
    let mut spec = spec_for(&server, dir.path(), password_auth());
    spec.timeouts.forward = Duration::from_secs(1);
    let tunnel = open_trusted(&server, &spec, Arc::new(NoPrompts)).unwrap();

    let started = Instant::now();
    let mut local = connect(tunnel.local_addr(&delayed()).unwrap());
    let mut byte = [0u8; 1];
    assert!(
        matches!(local.read(&mut byte), Ok(0) | Err(_)),
        "the local connection ends at the timeout"
    );
    assert!(
        started.elapsed() < Duration::from_millis(2300),
        "timeout must fire before the server confirms"
    );
    let error = tunnel.last_forward_error(&delayed()).unwrap();
    assert!(matches!(error, TunnelError::Timeout { .. }), "{error:?}");

    assert!(
        eventually(Duration::from_secs(8), || Stats::get(
            &server.stats.channels_closed
        ) >= 1),
        "the channel confirmed after the timeout must be closed"
    );
    assert!(tunnel.is_alive());
}

/// A peer that stops reading wedges russh's session loop on a blocked socket write; the watchdog closes it.
#[test]
fn stalled_socket_write_closes_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        window_size: u32::MAX / 2,
        ..ServerOptions::default()
    });
    let mut spec = spec_for(&server, dir.path(), password_auth());
    spec.timeouts.keepalive = Duration::from_secs(1); // blocked-write limit: 4 keepalive intervals
    let tunnel = open_trusted(&server, &spec, Arc::new(NoPrompts)).unwrap();

    let mut pusher = connect(tunnel.local_addr(&stall()).unwrap());
    pusher
        .set_write_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let writer = thread::spawn(move || {
        let chunk = vec![7u8; 1 << 20];
        for _ in 0..512 {
            if pusher.write_all(&chunk).is_err() {
                break;
            }
        }
    });
    let started = Instant::now();
    assert!(
        eventually(Duration::from_secs(30), || !tunnel.is_alive()),
        "a blocked write must not wedge the tunnel"
    );
    println!("stalled write detected after {:?}", started.elapsed());
    writer.join().unwrap();
}

// ------------------------------------------------------------------------------------------- concurrency

#[test]
fn tunnel_open_is_usable_from_many_threads_and_survives_server_restart_of_other_tunnels() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions::default());
    let tunnel = Arc::new(
        open_trusted(
            &server,
            &spec_for(&server, dir.path(), password_auth()),
            Arc::new(NoPrompts),
        )
        .unwrap(),
    );
    let done = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (tunnel, done) = (tunnel.clone(), done.clone());
            thread::spawn(move || {
                for _ in 0..10 {
                    echo_round_trip(tunnel.local_addr(&echo()).unwrap());
                    done.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    workers.into_iter().for_each(|w| w.join().unwrap());
    assert_eq!(done.load(Ordering::SeqCst), 80);
}

// ------------------------------------------------------------------------------------------------- N7

/// A fake ssh-agent that offers one identity and, when asked to sign, makes the SSH server vanish first.
#[cfg(unix)]
#[test]
fn agent_session_dying_mid_auth_is_a_connect_error_not_no_identity() {
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let server = Arc::new(TestServer::start(ServerOptions {
        plan: AuthPlan::PublicKey,
        ..ServerOptions::default()
    }));
    let (key, _) = generate_key(dir.path(), "agent_key", "ed25519");
    let blob = key.public_key().to_bytes().unwrap();

    let socket = dir.path().join("agent.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let asked_to_sign = Arc::new(AtomicBool::new(false));
    let agent = {
        let (server, asked_to_sign) = (server.clone(), asked_to_sign.clone());
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let frame = |payload: &[u8]| {
                let mut out = (payload.len() as u32).to_be_bytes().to_vec();
                out.extend_from_slice(payload);
                out
            };
            loop {
                let mut length = [0u8; 4];
                if stream.read_exact(&mut length).is_err() {
                    return;
                }
                let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
                if stream.read_exact(&mut body).is_err() {
                    return;
                }
                let reply = match body.first() {
                    // SSH_AGENTC_REQUEST_IDENTITIES -> SSH_AGENT_IDENTITIES_ANSWER with one key
                    Some(11) => {
                        let mut payload = vec![12];
                        payload.extend_from_slice(&1u32.to_be_bytes());
                        payload.extend_from_slice(&(blob.len() as u32).to_be_bytes());
                        payload.extend_from_slice(&blob);
                        payload.extend_from_slice(&4u32.to_be_bytes());
                        payload.extend_from_slice(b"test");
                        frame(&payload)
                    }
                    // SSH_AGENTC_SIGN_REQUEST: the connection to the SSH server dies first, then we fail.
                    Some(13) => {
                        asked_to_sign.store(true, Ordering::SeqCst);
                        server.drop_connections();
                        thread::sleep(Duration::from_millis(300));
                        frame(&[5]) // SSH_AGENT_FAILURE
                    }
                    _ => frame(&[5]),
                };
                if stream.write_all(&reply).is_err() {
                    return;
                }
            }
        })
    };

    let spec = spec_for(
        &server,
        dir.path(),
        Auth::Agent {
            socket: Some(socket),
        },
    );
    let error = err(open_trusted(&server, &spec, Arc::new(NoPrompts)));
    assert!(
        asked_to_sign.load(Ordering::SeqCst),
        "the agent must have been asked to sign"
    );
    assert!(
        matches!(error, TunnelError::Connect { .. } | TunnelError::Closed),
        "a dead transport must not look like an agent without identities: {error:?}"
    );
    assert!(!matches!(error, TunnelError::Auth(_)));
    drop(agent);
}

/// Measurement, not a gate: 50 MB streamed from the in-process server to a local client (the download path
/// through the per-forward pump). A full-duplex echo is deliberately not used: two russh session loops that
/// both block on writing to each other can deadlock, which says nothing about a real OpenSSH bastion.
/// `cargo test --release -p engine-net --test hardening -- --ignored --nocapture in_process_flood_throughput`
#[test]
#[ignore = "throughput measurement"]
fn in_process_flood_throughput() {
    const TOTAL: usize = 50 << 20;
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start(ServerOptions {
        flood_total: TOTAL,
        ..ServerOptions::default()
    });
    let tunnel = open_trusted(
        &server,
        &spec_for(&server, dir.path(), password_auth()),
        Arc::new(NoPrompts),
    )
    .unwrap();
    let mut stream = connect(tunnel.local_addr(&flood()).unwrap());
    let started = Instant::now();
    let mut buf = vec![0u8; 64 << 10];
    let mut received = 0;
    while received < TOTAL {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0);
        received += n;
    }
    let elapsed = started.elapsed();
    println!(
        "in-process download: 50 MB in {elapsed:?} = {:.1} MB/s ({})",
        50.0 / elapsed.as_secs_f64(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
}
