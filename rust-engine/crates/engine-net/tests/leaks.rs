//! A failed or abandoned open must leave nothing behind: no task on the tunnel runtime, no open socket.
//! Tests here share the process-wide runtime's task counter, so they run one at a time.

// Errors are cold-path values; their size does not matter here.
#![allow(clippy::result_large_err)]

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use engine_net::{
    alive_task_count, Auth, NoPrompts, Secret, Target, Tunnel, TunnelError, TunnelSpec,
};
use support::{eventually, poll_until_abandoned, AuthPlan, ServerOptions, TestServer};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn spec(port: u16, dir: &std::path::Path, auth: Auth) -> TunnelSpec {
    let mut spec = TunnelSpec::new("127.0.0.1", "tester", auth, dir.join("app_known_hosts"));
    spec.port = port;
    spec.user_known_hosts = None;
    spec.system_known_hosts = None;
    spec.targets
        .push(Target::new("echo.example.com", support::PORT_ECHO));
    spec.timeouts.connect = Duration::from_secs(1);
    spec
}

/// Task count once it has been stable for a moment (also starts the runtime).
fn settled_tasks() -> usize {
    let _ = Tunnel::open(
        spec(1, std::path::Path::new("."), Auth::KeyboardInteractive),
        Arc::new(NoPrompts),
    );
    let mut last = alive_task_count();
    let mut stable = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while stable < 5 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        let now = alive_task_count();
        stable = if now == last { stable + 1 } else { 0 };
        last = now;
    }
    last
}

fn assert_no_leftover_tasks(baseline: usize, what: &str) {
    assert!(
        eventually(Duration::from_secs(5), || alive_task_count() <= baseline),
        "{what}: {} tasks alive, expected at most {baseline}",
        alive_task_count()
    );
}

/// The peer sends its banner and then says nothing. After the client's connect timeout the socket must be
/// closed (the peer sees EOF) and no russh task may linger.
#[test]
fn server_that_stalls_after_banner_sees_eof_after_connect_timeout() {
    let _serial = serial();
    let baseline = settled_tasks();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (eof_tx, eof_rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .write_all(b"SSH-2.0-stalling-test-server\r\n")
            .unwrap();
        let mut sink = [0u8; 4096];
        loop {
            match stream.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {} // the client's own banner and KEXINIT
            }
        }
        let _ = eof_tx.send(Instant::now());
    });

    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let error = Tunnel::open(
        spec(port, dir.path(), Auth::Password(Secret::new("pw"))),
        Arc::new(NoPrompts),
    )
    .unwrap_err();
    let returned = Instant::now();
    assert!(matches!(error, TunnelError::Timeout { .. }), "{error:?}");
    assert!(returned - started >= Duration::from_millis(900));

    let eof_at = eof_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("the peer must see EOF / reset after the timeout");
    assert!(eof_at.saturating_duration_since(returned) < Duration::from_secs(2));
    peer.join().unwrap();
    assert_no_leftover_tasks(baseline, "after a handshake timeout");
}

#[test]
fn failed_and_abandoned_opens_leave_no_tasks_behind() {
    let _serial = serial();
    let baseline = settled_tasks();
    let dir = tempfile::tempdir().unwrap();

    // Wrong password.
    let server = TestServer::start(ServerOptions::default());
    let wrong = spec(
        server.addr.port(),
        dir.path(),
        Auth::Password(Secret::new("wrong")),
    );
    engine_net::trust_host_key(&wrong, &wrong.host, wrong.port, &server.host_public_key).unwrap();
    assert!(Tunnel::open(wrong, Arc::new(NoPrompts)).is_err());
    assert!(eventually(Duration::from_secs(5), || server
        .stats
        .connections_closed
        .load(std::sync::atomic::Ordering::SeqCst)
        == 1));
    assert_no_leftover_tasks(baseline, "after a rejected password");

    // Unknown host key.
    let fresh_dir = tempfile::tempdir().unwrap();
    let unknown = spec(
        server.addr.port(),
        fresh_dir.path(),
        Auth::Password(Secret::new("pw")),
    );
    assert!(matches!(
        Tunnel::open(unknown, Arc::new(NoPrompts)),
        Err(TunnelError::HostKeyUnknown { .. })
    ));
    assert_no_leftover_tasks(baseline, "after an unknown host key");

    // Keyboard-interactive nobody answers.
    let ki_server = TestServer::start(ServerOptions {
        plan: AuthPlan::Ki {
            rounds: vec![vec![("Code: ".into(), false)]],
            accept_last: "x".into(),
        },
        ..ServerOptions::default()
    });
    let declined = spec(ki_server.addr.port(), dir.path(), Auth::KeyboardInteractive);
    engine_net::trust_host_key(
        &declined,
        &declined.host,
        declined.port,
        &ki_server.host_public_key,
    )
    .unwrap();
    assert!(Tunnel::open(declined, Arc::new(NoPrompts)).is_err());
    assert_no_leftover_tasks(baseline, "after declined prompts");

    // Abandoned open_async.
    let abandoned = spec(ki_server.addr.port(), dir.path(), Auth::KeyboardInteractive);
    let abandon = Arc::new(AtomicBool::new(false));
    let worker = {
        let abandon = abandon.clone();
        thread::spawn(move || {
            poll_until_abandoned(Tunnel::open_async(abandoned, Arc::new(NoPrompts)), &abandon)
                .is_none()
        })
    };
    thread::sleep(Duration::from_millis(30));
    abandon.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = worker.join();
    assert_no_leftover_tasks(baseline, "after an abandoned open_async");
}
