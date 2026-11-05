//! Jump hosts (ProxyJump chains) against in-process SSH servers: each hop is a `TestServer` that relays
//! `direct-tcpip` requests to the next one (no Docker, loopback only).

// Errors are cold-path values; their size does not matter here.
#![allow(clippy::result_large_err)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_net::{
    trust_host_key, Auth, AuthError, HopId, Jump, NoPrompts, Secret, Target, Tunnel, TunnelError,
    TunnelSpec,
};
use support::{
    eventually, generate_key, AuthPlan, HostKey, ServerOptions, Stats, TestServer, PORT_ECHO,
};

fn echo() -> Target {
    Target::new("echo.example.com", PORT_ECHO)
}

/// A server that accepts `password` and relays forwards to whatever is asked for.
fn hop(password: &str) -> TestServer {
    TestServer::start(ServerOptions {
        plan: AuthPlan::Password {
            password: password.into(),
        },
        relay: true,
        ..ServerOptions::default()
    })
}

fn jump(server: &TestServer, password: &str) -> Jump {
    let mut jump = Jump::new("127.0.0.1", "hopper", Auth::Password(Secret::new(password)));
    jump.port = server.addr.port();
    jump
}

/// `jumps` in dial order, then `bastion`.
fn chain(dir: &Path, jumps: Vec<Jump>, bastion: &TestServer, bastion_password: &str) -> TunnelSpec {
    let mut spec = TunnelSpec::new(
        "127.0.0.1",
        "tester",
        Auth::Password(Secret::new(bastion_password)),
        dir.join("app_known_hosts"),
    );
    spec.port = bastion.addr.port();
    spec.jumps = jumps;
    spec.user_known_hosts = None;
    spec.system_known_hosts = None;
    spec.targets = vec![echo()];
    spec
}

fn trust(spec: &TunnelSpec, server: &TestServer) {
    trust_host_key(
        spec,
        "127.0.0.1",
        server.addr.port(),
        &server.host_public_key,
    )
    .unwrap();
}

fn open(spec: &TunnelSpec) -> Result<Tunnel, TunnelError> {
    Tunnel::open(spec.clone(), Arc::new(NoPrompts))
}

fn err(result: Result<Tunnel, TunnelError>) -> TunnelError {
    match result {
        Ok(tunnel) => panic!("expected an error, got {tunnel:?}"),
        Err(error) => error,
    }
}

fn echo_works(addr: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reply = [0u8; 4];
    stream.write_all(b"ping").is_ok() && stream.read_exact(&mut reply).is_ok() && &reply == b"ping"
}

#[test]
fn two_jumps_then_the_bastion_each_with_its_own_credential() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (hop("pw-a"), hop("pw-b"), hop("pw-c"));
    let spec = chain(
        dir.path(),
        vec![jump(&a, "pw-a"), jump(&b, "pw-b")],
        &c,
        "pw-c",
    );
    for server in [&a, &b, &c] {
        trust(&spec, server);
    }
    let tunnel = open(&spec).unwrap();
    assert!(tunnel.is_alive());
    assert_eq!(tunnel.via(), format!("tester@127.0.0.1:{}", c.addr.port()));
    assert!(echo_works(tunnel.local_addr(&echo()).unwrap()));
    tunnel.probe(&echo()).unwrap();
    for server in [&a, &b, &c] {
        assert_eq!(Stats::get(&server.stats.connections), 1);
    }
    // A hop only ever sees its own credential.
    for (server, own) in [(&a, "pw-a"), (&b, "pw-b"), (&c, "pw-c")] {
        assert_eq!(*server.stats.passwords.lock().unwrap(), [own]);
    }
}

#[test]
fn a_jump_can_authenticate_with_a_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let a = TestServer::start(ServerOptions {
        plan: AuthPlan::PublicKey,
        relay: true,
        ..ServerOptions::default()
    });
    let c = hop("pw-c");
    generate_key(dir.path(), "jump_key", "ed25519");
    let mut jump_a = Jump::new(
        "127.0.0.1",
        "hopper",
        Auth::KeyFile {
            path: dir.path().join("jump_key"),
            passphrase: None,
        },
    );
    jump_a.port = a.addr.port();
    let spec = chain(dir.path(), vec![jump_a], &c, "pw-c");
    trust(&spec, &a);
    trust(&spec, &c);
    let tunnel = open(&spec).unwrap();
    assert!(echo_works(tunnel.local_addr(&echo()).unwrap()));
    assert!(a.stats.passwords.lock().unwrap().is_empty());
}

#[test]
fn an_unknown_key_on_a_later_hop_names_that_hop_and_trusting_it_writes_exactly_it() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (hop("pw-a"), hop("pw-b"), hop("pw-c"));
    let spec = chain(
        dir.path(),
        vec![jump(&a, "pw-a"), jump(&b, "pw-b")],
        &c,
        "pw-c",
    );
    trust(&spec, &a);

    // Hop 2: reported with its own host and port, and no credential was sent to it.
    let TunnelError::HostKeyUnknown {
        host,
        port,
        public_key,
        ..
    } = err(open(&spec))
    else {
        panic!("expected an unknown host key");
    };
    assert_eq!((host.as_str(), port), ("127.0.0.1", b.addr.port()));
    assert_eq!(public_key, b.host_public_key);
    assert_eq!(Stats::get(&b.stats.auth_started), 0);
    assert_eq!(Stats::get(&b.stats.auth_requests), 0);
    assert_eq!(
        Stats::get(&c.stats.connections),
        0,
        "nothing was dialled beyond the unknown hop"
    );
    // The first hop was verified and authenticated before the second was even dialled.
    assert_eq!(Stats::get(&a.stats.auth_success), 1);

    // Trusting what the error says (not spec.host / spec.port) is what makes the chain progress.
    trust_host_key(&spec, &host, port, &public_key).unwrap();
    let TunnelError::HostKeyUnknown {
        port, public_key, ..
    } = err(open(&spec))
    else {
        panic!("expected the bastion's key to be unknown");
    };
    assert_eq!(port, c.addr.port());
    assert_eq!(Stats::get(&c.stats.auth_requests), 0);
    trust_host_key(&spec, "127.0.0.1", port, &public_key).unwrap();
    let tunnel = open(&spec).unwrap();
    assert!(echo_works(tunnel.local_addr(&echo()).unwrap()));
    let recorded = std::fs::read_to_string(&spec.app_known_hosts).unwrap();
    assert_eq!(
        recorded.lines().count(),
        3,
        "exactly the three hops: {recorded}"
    );
}

#[test]
fn a_changed_key_on_a_later_hop_is_refused_before_any_credential() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (hop("pw-a"), hop("pw-b"), hop("pw-c"));
    let spec = chain(
        dir.path(),
        vec![jump(&a, "pw-a"), jump(&b, "pw-b")],
        &c,
        "pw-c",
    );
    trust(&spec, &a);
    trust(&spec, &c);
    // A different key of the same type is recorded for hop 2.
    let (_, other_public) = generate_key(dir.path(), "other", "ed25519");
    trust_host_key(&spec, "127.0.0.1", b.addr.port(), &other_public).unwrap();
    let error = err(open(&spec));
    let TunnelError::HostKeyMismatch { host, port, .. } = &error else {
        panic!("expected a changed host key, got {error:?}");
    };
    assert_eq!((host.as_str(), *port), ("127.0.0.1", b.addr.port()));
    assert_eq!(error.code(), "tunnel.host_key_mismatch");
    assert_eq!(Stats::get(&b.stats.auth_requests), 0);
    assert!(b.stats.passwords.lock().unwrap().is_empty());
}

#[test]
fn authentication_failures_name_the_failing_hop_and_hide_the_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (hop("pw-a"), hop("pw-b"), hop("pw-c"));
    for (jumps, bastion_password, expected, id, port) in [
        (
            vec![jump(&a, "pw-a"), jump(&b, "WRONG-jump-secret")],
            "pw-c",
            format!("jump 2 of 2 hopper@127.0.0.1:{}: ", b.addr.port()),
            HopId::Jump { index: 1, of: 2 },
            b.addr.port(),
        ),
        (
            vec![jump(&a, "WRONG-jump-secret"), jump(&b, "pw-b")],
            "pw-c",
            format!("jump 1 of 2 hopper@127.0.0.1:{}: ", a.addr.port()),
            HopId::Jump { index: 0, of: 2 },
            a.addr.port(),
        ),
        (
            vec![jump(&a, "pw-a"), jump(&b, "pw-b")],
            "WRONG-bastion-secret",
            format!("bastion tester@127.0.0.1:{}: ", c.addr.port()),
            HopId::Bastion,
            c.addr.port(),
        ),
    ] {
        let spec = chain(dir.path(), jumps, &c, bastion_password);
        for server in [&a, &b, &c] {
            trust(&spec, server);
        }
        let error = err(open(&spec));
        let shown = error.to_string();
        assert!(
            shown.starts_with(&expected),
            "{shown} (expected {expected})"
        );
        assert_eq!(error.code(), "tunnel.auth");
        // The position is structured too, so a caller can route a missing secret to the right hop.
        let hop = error.hop().expect("the failing hop is structured");
        assert_eq!(
            (hop.id, hop.host.as_str(), hop.port),
            (id, "127.0.0.1", port)
        );
        assert!(
            matches!(error.root(), TunnelError::Auth(AuthError::Rejected { .. })),
            "{error:?}"
        );
        let all = format!("{shown} {error:?}");
        assert!(!all.contains("WRONG-") && !all.contains("pw-"), "{all}");
    }
}

#[test]
fn a_jump_that_will_not_forward_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let closed = TestServer::start(ServerOptions {
        plan: AuthPlan::Password {
            password: "pw-a".into(),
        },
        relay: false,
        ..ServerOptions::default()
    });
    let c = hop("pw-c");
    let spec = chain(dir.path(), vec![jump(&closed, "pw-a")], &c, "pw-c");
    trust(&spec, &closed);
    trust(&spec, &c);
    let error = err(open(&spec));
    assert_eq!(error.code(), "tunnel.forward_refused");
    assert!(
        error.to_string().starts_with(&format!(
            "jump 1 of 1 hopper@127.0.0.1:{}: ",
            closed.addr.port()
        )),
        "{error}"
    );
}

/// Every hop is a session of its own: if any of them dies the tunnel is dead and its local port is closed.
#[test]
fn any_hop_dying_closes_the_tunnel_and_its_listeners() {
    for victim in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let servers = [hop("pw-a"), hop("pw-b"), hop("pw-c")];
        let spec = chain(
            dir.path(),
            vec![jump(&servers[0], "pw-a"), jump(&servers[1], "pw-b")],
            &servers[2],
            "pw-c",
        );
        for server in &servers {
            trust(&spec, server);
        }
        let tunnel = open(&spec).unwrap();
        let local = tunnel.local_addr(&echo()).unwrap();
        assert!(echo_works(local));
        servers[victim].drop_connections();
        assert!(
            eventually(Duration::from_secs(10), || !tunnel.is_alive()),
            "hop {victim} died but the tunnel still claims to be alive"
        );
        assert!(
            eventually(Duration::from_secs(5), || TcpStream::connect_timeout(
                &local,
                Duration::from_millis(500)
            )
            .is_err()),
            "hop {victim}: the local listener is still open"
        );
        assert!(matches!(tunnel.probe(&echo()), Err(TunnelError::Closed)));
    }
}

#[test]
fn dropping_the_tunnel_ends_every_session() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (hop("pw-a"), hop("pw-b"), hop("pw-c"));
    let spec = chain(
        dir.path(),
        vec![jump(&a, "pw-a"), jump(&b, "pw-b")],
        &c,
        "pw-c",
    );
    for server in [&a, &b, &c] {
        trust(&spec, server);
    }
    let tunnel = open(&spec).unwrap();
    assert!(echo_works(tunnel.local_addr(&echo()).unwrap()));
    drop(tunnel);
    for server in [&a, &b, &c] {
        assert!(
            eventually(Duration::from_secs(5), || Stats::get(
                &server.stats.connections_closed
            ) == 1),
            "a hop session outlived the tunnel"
        );
    }
}

#[test]
fn a_chain_whose_hop_is_not_an_ssh_server_fails_at_that_hop() {
    let dir = tempfile::tempdir().unwrap();
    let a = hop("pw-a");
    // The "bastion" is a plain TCP echo listener, not SSH.
    let impostor = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = impostor.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = impostor.accept() {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            std::thread::sleep(Duration::from_millis(300));
        }
    });
    let c = hop("pw-c");
    let mut spec = chain(dir.path(), vec![jump(&a, "pw-a")], &c, "pw-c");
    spec.port = port;
    trust(&spec, &a);
    let error = err(open(&spec));
    assert!(
        error
            .to_string()
            .starts_with(&format!("bastion tester@127.0.0.1:{port}: ")),
        "{error}"
    );
    assert!(matches!(
        error.root(),
        TunnelError::Connect { .. } | TunnelError::Timeout { .. }
    ));
}

#[test]
fn host_key_types_do_not_matter_to_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let a = TestServer::start(ServerOptions {
        host_key: HostKey::Rsa,
        plan: AuthPlan::Password {
            password: "pw-a".into(),
        },
        relay: true,
        ..ServerOptions::default()
    });
    let c = hop("pw-c");
    let spec = chain(dir.path(), vec![jump(&a, "pw-a")], &c, "pw-c");
    trust(&spec, &a);
    trust(&spec, &c);
    let tunnel = open(&spec).unwrap();
    assert!(echo_works(tunnel.local_addr(&echo()).unwrap()));
}
