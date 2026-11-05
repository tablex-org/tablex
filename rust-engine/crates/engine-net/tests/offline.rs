//! Tests that need no SSH server and no network: only loopback sockets this process owns.

// Errors are cold-path values; their size does not matter here.
#![allow(clippy::result_large_err)]

use std::future::Future;
use std::io::Write;
use std::net::TcpListener;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use engine_net::{
    Auth, ConfigError, Jump, NoPrompts, Secret, Target, Tunnel, TunnelError, TunnelSpec,
};

fn spec_for(port: u16) -> TunnelSpec {
    let mut spec = TunnelSpec::new(
        "127.0.0.1",
        "deploy",
        Auth::Password(Secret::new("offline-password-needle")),
        std::env::temp_dir().join("engine-net-offline-test-known-hosts"),
    );
    spec.port = port;
    spec.user_known_hosts = None;
    spec.system_known_hosts = None;
    spec.targets
        .push(Target::new("db.internal.example.com", 5432));
    spec.timeouts.connect = Duration::from_secs(1);
    spec
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn expect_err(result: Result<Tunnel, TunnelError>) -> TunnelError {
    match result {
        Ok(tunnel) => panic!("expected an error, got {tunnel:?}"),
        Err(error) => error,
    }
}

/// Minimal executor: proves `open_async` does not need tokio on the caller's side.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        thread::park();
    }
}

#[test]
fn connection_refused_is_a_connect_error_without_the_secret() {
    let started = Instant::now();
    let error = expect_err(Tunnel::open(spec_for(unused_port()), Arc::new(NoPrompts)));
    assert!(matches!(error, TunnelError::Connect { .. }), "{error:?}");
    assert_eq!(error.code(), "tunnel.connect");
    assert!(started.elapsed() < Duration::from_secs(5));
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains("offline-password-needle"), "{shown}");
}

#[test]
fn a_server_that_never_speaks_times_out() {
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let started = Instant::now();
    let error = expect_err(Tunnel::open(spec_for(port), Arc::new(NoPrompts)));
    assert!(matches!(error, TunnelError::Timeout { .. }), "{error:?}");
    assert_eq!(error.code(), "tunnel.timeout");
    assert!(
        started.elapsed() >= Duration::from_secs(1) && started.elapsed() < Duration::from_secs(5)
    );
    drop(silent);
}

#[test]
fn a_non_ssh_server_is_a_connect_error() {
    let imposter = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = imposter.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        if let Ok((mut stream, _)) = imposter.accept() {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            thread::sleep(Duration::from_millis(300));
        }
    });
    let error = expect_err(Tunnel::open(spec_for(port), Arc::new(NoPrompts)));
    assert!(
        matches!(
            error,
            TunnelError::Connect { .. } | TunnelError::Timeout { .. }
        ),
        "{error:?}"
    );
    server.join().unwrap();
}

#[test]
fn open_async_works_on_a_foreign_executor() {
    let error = expect_err(block_on(Tunnel::open_async(
        spec_for(unused_port()),
        Arc::new(NoPrompts),
    )));
    assert_eq!(error.code(), "tunnel.connect");
}

#[test]
fn open_does_not_need_tokio_but_tolerates_being_called_inside_it() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let port = unused_port();
    let from_blocking_thread = runtime.block_on(async move {
        tokio::task::spawn_blocking(move || Tunnel::open(spec_for(port), Arc::new(NoPrompts)))
            .await
            .unwrap()
    });
    assert_eq!(expect_err(from_blocking_thread).code(), "tunnel.connect");
}

fn chain_spec(jumps: &[(&str, u16)]) -> TunnelSpec {
    let mut spec = spec_for(unused_port());
    spec.jumps = jumps
        .iter()
        .map(|(host, port)| {
            let mut jump = Jump::new(
                *host,
                "hopper",
                Auth::Password(Secret::new("jump-password-needle")),
            );
            jump.port = *port;
            jump
        })
        .collect();
    spec
}

#[test]
fn invalid_chains_fail_before_any_network_use() {
    let started = Instant::now();
    // A repeated host:port, an empty host, port 0 and nine jumps: all typed config errors, nothing dialled.
    let repeated = chain_spec(&[("127.0.0.1", 9), ("127.0.0.1", 9)]);
    let empty_host = chain_spec(&[("", 22)]);
    let port_zero = chain_spec(&[("jump.example.com", 0)]);
    let nine: Vec<(String, u16)> = (0..9).map(|i| (format!("j{i}.example.com"), 22)).collect();
    let too_deep = chain_spec(
        &nine
            .iter()
            .map(|(h, p)| (h.as_str(), *p))
            .collect::<Vec<_>>(),
    );
    for (spec, needle) in [
        (repeated, "repeats"),
        (empty_host, "jump 1 of 1"),
        (port_zero, "port must not be 0"),
        (too_deep, "at most 8"),
    ] {
        let error = expect_err(Tunnel::open(spec, Arc::new(NoPrompts)));
        assert!(
            matches!(error, TunnelError::Config(ConfigError::Invalid(_))),
            "{error:?}"
        );
        assert_eq!(error.code(), "tunnel.config");
        assert!(error.to_string().contains(needle), "{needle}: {error}");
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("jump-password-needle"), "{shown}");
    }
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_dead_jump_is_reported_with_its_position_and_no_secret() {
    let dead = unused_port();
    let mut spec = chain_spec(&[("127.0.0.1", dead), ("jump2.example.com", 22)]);
    spec.user_known_hosts = None;
    let error = expect_err(Tunnel::open(spec, Arc::new(NoPrompts)));
    assert_eq!(error.code(), "tunnel.connect");
    let shown = error.to_string();
    assert!(
        shown.starts_with(&format!("jump 1 of 2 hopper@127.0.0.1:{dead}: ")),
        "{shown}"
    );
    assert!(
        matches!(error.root(), TunnelError::Connect { .. }),
        "{error:?}"
    );
    let debug = format!("{error:?}");
    assert!(!shown.contains("jump-password-needle") && !debug.contains("jump-password-needle"));
}

#[test]
fn invalid_specs_fail_validation_before_connecting() {
    let mut spec = spec_for(unused_port());
    spec.targets.clear();
    let error = expect_err(Tunnel::open(spec, Arc::new(NoPrompts)));
    assert_eq!(error.code(), "tunnel.config");
}

/// `Tunnel::open` is documented as blocking; called straight from async code it must still work (the work
/// runs on the crate's own runtime) instead of panicking with "cannot block inside a runtime".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_called_directly_in_async_context() {
    let error = expect_err(Tunnel::open(spec_for(unused_port()), Arc::new(NoPrompts)));
    assert_eq!(error.code(), "tunnel.connect");
}

#[tokio::test(flavor = "current_thread")]
async fn open_called_directly_in_a_current_thread_runtime() {
    let error = expect_err(Tunnel::open(spec_for(unused_port()), Arc::new(NoPrompts)));
    assert_eq!(error.code(), "tunnel.connect");
}
