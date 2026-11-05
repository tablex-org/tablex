//! Live tests against the `tablex-test-sshd` container (and the tablex-test-* databases behind it).
//!
//! ```sh
//! rust-engine/scripts/test-sshd.sh start
//! source "$(rust-engine/scripts/test-sshd.sh env)"      # bash/zsh
//! cargo test --manifest-path rust-engine/Cargo.toml -p engine-net --test live -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Never reads or writes `~/.ssh`: every spec uses `user_known_hosts: None` or a file inside a temp dir, and
//! the agent test runs its own private `ssh-agent`.

// Errors are cold-path values; their size does not matter here.
#![allow(clippy::result_large_err)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
#[cfg(unix)]
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use engine_net::{
    trust_host_key, Auth, AuthError, Jump, KeyboardInteractiveRequest, NoPrompts, Prompts,
    ProxyCredentials, ProxyKind, ProxyRoute, ProxySpec, Secret, Target, Tunnel, TunnelError,
    TunnelSpec,
};
use support::proxy::{Protocol, ProxyOptions, TestProxy};

// A public key that is not the container's (for the mismatch test). No private half exists.
const OTHER_ED25519: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOB2eAzhXfG/iR6rH2OG/O394S9h+IaW0uOSd5mgHD0p";

static SERIAL: Mutex<()> = Mutex::new(());

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set; run rust-engine/scripts/test-sshd.sh start and source its `env` file first")
    })
}

struct Fixture {
    dir: tempfile::TempDir,
    host: String,
    port: u16,
    user: String,
    password: String,
    passphrase: String,
    container: String,
    pg: Target,
    redis: Target,
    _serial: MutexGuard<'static, ()>,
}

fn target_from_env(name: &str) -> Target {
    let value = var(name);
    let (host, port) = value.rsplit_once(':').expect("host:port");
    Target::new(host, port.parse().expect("port"))
}

impl Fixture {
    /// Serialises the live tests: some of them pause or kill the container's SSH sessions.
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let container = var("TABLEX_TEST_SSHD_CONTAINER");
        assert!(
            container.starts_with("tablex-test-"),
            "refusing to touch container {container}"
        );
        Self {
            dir: tempfile::tempdir().unwrap(),
            host: var("TABLEX_TEST_SSHD_HOST"),
            port: var("TABLEX_TEST_SSHD_PORT").parse().unwrap(),
            user: var("TABLEX_TEST_SSHD_USER"),
            password: var("TABLEX_TEST_SSHD_PASSWORD"),
            passphrase: var("TABLEX_TEST_SSHD_PASSPHRASE"),
            container,
            pg: target_from_env("TABLEX_TEST_SSHD_PG"),
            redis: target_from_env("TABLEX_TEST_SSHD_REDIS"),
            _serial: serial,
        }
    }

    fn app_known_hosts(&self) -> PathBuf {
        self.dir.path().join("app_known_hosts")
    }

    fn spec(&self, auth: Auth) -> TunnelSpec {
        let mut spec = TunnelSpec::new(
            self.host.clone(),
            self.user.clone(),
            auth,
            self.app_known_hosts(),
        );
        spec.port = self.port;
        spec.user_known_hosts = None; // never the real ~/.ssh/known_hosts
        spec.system_known_hosts = None;
        spec.system_known_hosts = None;
        spec.targets = vec![self.pg.clone(), self.redis.clone()];
        spec
    }

    fn key(&self, name: &str) -> PathBuf {
        PathBuf::from(var(name))
    }

    fn key_auth(&self, name: &str, passphrase: Option<&str>) -> Auth {
        Auth::KeyFile {
            path: self.key(name),
            passphrase: passphrase.map(Secret::new),
        }
    }

    /// The first connection is expected to stop at the unknown host key; trust it (app file only) and retry.
    fn open_trusting(&self, spec: TunnelSpec) -> Result<Tunnel, TunnelError> {
        self.open_trusting_with(spec, Arc::new(NoPrompts))
    }

    fn open_trusting_with(
        &self,
        spec: TunnelSpec,
        prompts: Arc<dyn Prompts>,
    ) -> Result<Tunnel, TunnelError> {
        match Tunnel::open(spec.clone(), prompts.clone()) {
            Err(TunnelError::HostKeyUnknown { public_key, .. }) => {
                trust_host_key(&spec, &spec.host, spec.port, &public_key).unwrap();
                Tunnel::open(spec, prompts)
            }
            other => other,
        }
    }

    fn container_sh(&self, script: &str) -> String {
        let output = Command::new("docker")
            .args(["exec", &self.container, "sh", "-c", script])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn session_count(&self) -> usize {
        // One unprivileged `sshd-session: tablex` (older OpenSSH: `sshd: tablex`) process per SSH connection.
        let ps = self.container_sh("ps");
        ps.lines()
            .filter(|l| l.contains("sshd") && l.contains(": tablex") && !l.contains("[priv]"))
            .count()
    }

    fn wait_for_sessions(&self, expected: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.session_count() == expected {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(5)).expect("local forward accepts");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
}

/// PostgreSQL StartupMessage; returns the type byte of the server's first reply ('R' = authentication
/// request, 'E' = error), proof that a real PostgreSQL answered through the tunnel.
fn postgres_first_reply(addr: SocketAddr) -> u8 {
    let mut stream = connect(addr);
    let body = b"\x00\x03\x00\x00user\0tablex_ssh_test\0database\0postgres\0\0";
    stream
        .write_all(&((body.len() as u32 + 4).to_be_bytes()))
        .unwrap();
    stream.write_all(body).unwrap();
    let mut reply = [0u8; 1];
    stream.read_exact(&mut reply).unwrap();
    reply[0]
}

fn redis_ping(addr: SocketAddr) -> String {
    let mut stream = connect(addr);
    stream.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut reply = [0u8; 7];
    stream.read_exact(&mut reply).unwrap();
    String::from_utf8_lossy(&reply).into_owned()
}

fn assert_reaches_both_databases(fx: &Fixture, tunnel: &Tunnel) {
    assert_eq!(
        postgres_first_reply(tunnel.local_addr(&fx.pg).unwrap()),
        b'R'
    );
    assert_eq!(
        redis_ping(tunnel.local_addr(&fx.redis).unwrap()),
        "+PONG\r\n"
    );
}

fn expect_err(result: Result<Tunnel, TunnelError>) -> TunnelError {
    match result {
        Ok(tunnel) => panic!("expected an error, got a tunnel: {tunnel:?}"),
        Err(error) => error,
    }
}

// ----------------------------------------------------------------------------------------- authentication

/// Password auth, and the headline end to end: PostgreSQL and Redis protocol exchanges through the tunnel.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn password_auth_reaches_postgres_and_redis() {
    let fx = Fixture::new();
    let tunnel = fx
        .open_trusting(fx.spec(Auth::Password(Secret::new(&fx.password))))
        .unwrap();
    assert!(tunnel.is_alive());
    assert_eq!(tunnel.via(), format!("{}@{}:{}", fx.user, fx.host, fx.port));
    assert_reaches_both_databases(&fx, &tunnel);
    tunnel.probe(&fx.pg).unwrap();
}

/// Every key type and format authenticates: OpenSSH ed25519 / ecdsa / rsa and a PEM (PKCS#1) rsa key.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn key_files_of_every_type_authenticate() {
    let fx = Fixture::new();
    for name in [
        "TABLEX_TEST_SSHD_KEY_ED25519",
        "TABLEX_TEST_SSHD_KEY_ECDSA",
        "TABLEX_TEST_SSHD_KEY_RSA",
        "TABLEX_TEST_SSHD_KEY_RSA_PEM",
    ] {
        let tunnel = fx
            .open_trusting(fx.spec(fx.key_auth(name, None)))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            redis_ping(tunnel.local_addr(&fx.redis).unwrap()),
            "+PONG\r\n",
            "{name}"
        );
    }
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn encrypted_key_needs_the_right_passphrase() {
    let fx = Fixture::new();
    let tunnel = fx
        .open_trusting(
            fx.spec(fx.key_auth("TABLEX_TEST_SSHD_KEY_ED25519_ENC", Some(&fx.passphrase))),
        )
        .unwrap();
    assert_reaches_both_databases(&fx, &tunnel);

    let missing = expect_err(
        fx.open_trusting(fx.spec(fx.key_auth("TABLEX_TEST_SSHD_KEY_ED25519_ENC", None))),
    );
    assert!(
        matches!(
            missing,
            TunnelError::Auth(AuthError::PassphraseRequired { .. })
        ),
        "{missing:?}"
    );
    assert_eq!(missing.code(), "tunnel.auth");

    let wrong = expect_err(fx.open_trusting(fx.spec(fx.key_auth(
        "TABLEX_TEST_SSHD_KEY_ED25519_ENC",
        Some("wrong-passphrase-needle"),
    ))));
    assert!(
        matches!(wrong, TunnelError::Auth(AuthError::BadPassphrase { .. })),
        "{wrong:?}"
    );
    let shown = format!("{wrong} {wrong:?}");
    assert!(
        !shown.contains("wrong-passphrase-needle") && !shown.contains(&fx.passphrase),
        "{shown}"
    );
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn wrong_password_is_rejected_without_leaking_it() {
    let fx = Fixture::new();
    let error =
        expect_err(fx.open_trusting(fx.spec(Auth::Password(Secret::new("wrong-password-needle")))));
    assert!(
        matches!(error, TunnelError::Auth(AuthError::Rejected { .. })),
        "{error:?}"
    );
    assert_eq!(error.code(), "tunnel.auth");
    let shown = format!("{error} | {error:?} | {error:#?}");
    assert!(
        !shown.contains("wrong-password-needle") && !shown.contains(&fx.password),
        "{shown}"
    );
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn an_unauthorized_key_is_rejected() {
    let fx = Fixture::new();
    let stranger = fx.dir.path().join("stranger");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&stranger)
        .status()
        .unwrap();
    assert!(status.success());
    let error = expect_err(fx.open_trusting(fx.spec(Auth::KeyFile {
        path: stranger,
        passphrase: None,
    })));
    assert!(
        matches!(error, TunnelError::Auth(AuthError::Rejected { .. })),
        "{error:?}"
    );
}

struct ScriptedPrompts {
    answer: Option<String>,
    seen: Mutex<Vec<KeyboardInteractiveRequest>>,
    calls: AtomicUsize,
}

impl ScriptedPrompts {
    fn new(answer: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.map(str::to_owned),
            seen: Mutex::default(),
            calls: AtomicUsize::new(0),
        })
    }
}

impl Prompts for ScriptedPrompts {
    fn keyboard_interactive(&self, request: &KeyboardInteractiveRequest) -> Option<Vec<Secret>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request.clone());
        let answer = self.answer.as_ref()?;
        Some(
            request
                .prompts
                .iter()
                .map(|_| Secret::new(answer.clone()))
                .collect(),
        )
    }
}

/// Keyboard-interactive (PAM) answered through the callback; a wrong answer is rejected, no answer is a
/// distinct error.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn keyboard_interactive_is_answered_through_the_callback() {
    let fx = Fixture::new();
    let prompts = ScriptedPrompts::new(Some(&fx.password));
    let tunnel = fx
        .open_trusting_with(fx.spec(Auth::KeyboardInteractive), prompts.clone())
        .unwrap();
    assert_eq!(
        redis_ping(tunnel.local_addr(&fx.redis).unwrap()),
        "+PONG\r\n"
    );
    let seen = prompts.seen.lock().unwrap();
    assert!(
        seen[0].prompts[0].text.to_lowercase().contains("password"),
        "{seen:?}"
    );
    assert!(!seen[0].prompts[0].echo);
    drop(seen);

    let wrong = ScriptedPrompts::new(Some("wrong-kbd-needle"));
    let error = expect_err(fx.open_trusting_with(fx.spec(Auth::KeyboardInteractive), wrong));
    assert!(
        matches!(error, TunnelError::Auth(AuthError::Rejected { .. })),
        "{error:?}"
    );

    let silent = ScriptedPrompts::new(None);
    let error =
        expect_err(fx.open_trusting_with(fx.spec(Auth::KeyboardInteractive), silent.clone()));
    assert!(
        matches!(error, TunnelError::Auth(AuthError::PromptDeclined)),
        "{error:?}"
    );
    assert_eq!(silent.calls.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
struct PrivateAgent {
    child: Child,
    socket: PathBuf,
}

#[cfg(unix)]
impl PrivateAgent {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("agent.sock");
        let child = Command::new("ssh-agent")
            .args(["-D", "-a"])
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        Self { child, socket }
    }

    fn add(&self, key: &Path) {
        let status = Command::new("ssh-add")
            .arg(key)
            .env("SSH_AUTH_SOCK", &self.socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[cfg(unix)]
impl Drop for PrivateAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// ssh-agent auth against a private agent that holds only the throwaway key; an empty agent is reported.
#[cfg(unix)]
#[test]
#[ignore = "needs tablex-test-sshd"]
fn ssh_agent_authenticates_and_an_empty_agent_is_reported() {
    let fx = Fixture::new();
    let agent = PrivateAgent::start(fx.dir.path());

    let empty = expect_err(fx.open_trusting(fx.spec(Auth::Agent {
        socket: Some(agent.socket.clone()),
    })));
    assert!(
        matches!(empty, TunnelError::Auth(AuthError::NoAgentIdentity)),
        "{empty:?}"
    );

    agent.add(&fx.key("TABLEX_TEST_SSHD_KEY_ED25519"));
    let tunnel = fx
        .open_trusting(fx.spec(Auth::Agent {
            socket: Some(agent.socket.clone()),
        }))
        .unwrap();
    assert_reaches_both_databases(&fx, &tunnel);

    let missing = expect_err(fx.open_trusting(fx.spec(Auth::Agent {
        socket: Some(fx.dir.path().join("no-such.sock")),
    })));
    assert!(
        matches!(missing, TunnelError::Auth(AuthError::AgentUnavailable(_))),
        "{missing:?}"
    );
}

// ---------------------------------------------------------------------------------------------- host keys

fn host_pub_fields() -> (String, String) {
    let text = std::fs::read_to_string(var("TABLEX_TEST_SSHD_HOST_PUB")).unwrap();
    let mut fields = text.split_whitespace();
    (
        fields.next().unwrap().to_owned(),
        fields.next().unwrap().to_owned(),
    )
}

/// Unknown -> typed error with the OpenSSH fingerprint -> trust (app file only) -> reconnect works.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn unknown_host_key_then_trust_then_reconnect() {
    let fx = Fixture::new();
    let spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
    let error = expect_err(Tunnel::open(spec.clone(), Arc::new(NoPrompts)));
    let TunnelError::HostKeyUnknown {
        host,
        port,
        algorithm,
        sha256_fingerprint,
        public_key,
    } = error
    else {
        panic!("expected HostKeyUnknown, got {error:?}");
    };
    assert_eq!(
        (host.as_str(), port, algorithm.as_str()),
        (fx.host.as_str(), fx.port, "ssh-ed25519")
    );
    assert!(
        !fx.app_known_hosts().exists(),
        "nothing is written before the caller decides"
    );

    let (kind, blob) = host_pub_fields();
    assert_eq!(public_key, format!("{kind} {blob}"));
    let listed = Command::new("ssh-keygen")
        .arg("-lf")
        .arg(var("TABLEX_TEST_SSHD_HOST_PUB"))
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert_eq!(
        listed.split_whitespace().nth(1),
        Some(sha256_fingerprint.as_str()),
        "{listed}"
    );

    trust_host_key(&spec, &spec.host, spec.port, &public_key).unwrap();
    let recorded = std::fs::read_to_string(fx.app_known_hosts()).unwrap();
    assert!(
        recorded.starts_with("|1|") && recorded.ends_with(&format!(" {kind} {blob}\n")),
        "{recorded}"
    );
    assert!(
        !recorded.contains(&fx.host),
        "the file must not hold the host name in clear: {recorded}"
    );

    let tunnel = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap();
    assert_eq!(
        redis_ping(tunnel.local_addr(&fx.redis).unwrap()),
        "+PONG\r\n"
    );
}

/// A different key recorded for the host is a hard refusal; trusting cannot replace it either.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn changed_host_key_is_always_refused() {
    let fx = Fixture::new();
    std::fs::write(
        fx.app_known_hosts(),
        format!("[{}]:{} {OTHER_ED25519}\n", fx.host, fx.port),
    )
    .unwrap();
    let before = std::fs::read_to_string(fx.app_known_hosts()).unwrap();
    let spec = fx.spec(Auth::Password(Secret::new(&fx.password)));

    let error = expect_err(Tunnel::open(spec.clone(), Arc::new(NoPrompts)));
    assert!(
        matches!(&error, TunnelError::HostKeyMismatch { recorded_at, .. } if recorded_at.ends_with(":1")),
        "{error:?}"
    );
    assert_eq!(error.code(), "tunnel.host_key_mismatch");

    let (kind, blob) = host_pub_fields();
    let refused =
        trust_host_key(&spec, &spec.host, spec.port, &format!("{kind} {blob}")).unwrap_err();
    assert!(
        matches!(refused, TunnelError::HostKeyMismatch { .. }),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fx.app_known_hosts()).unwrap(),
        before,
        "the recorded key must stay"
    );
    // And no credential was sent: the failure is the host key, not authentication.
    assert!(Tunnel::open(spec, Arc::new(NoPrompts)).is_err());
}

/// The user's own known_hosts (read-only), in hashed form produced by real `ssh-keygen -H`, is honoured;
/// a `@revoked` line for the same key refuses the host; the file is never modified.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn user_known_hosts_hashed_and_revoked_entries_are_honoured_read_only() {
    let fx = Fixture::new();
    let (kind, blob) = host_pub_fields();
    let user_file = fx.dir.path().join("user_known_hosts");
    std::fs::write(
        &user_file,
        format!("[{}]:{} {kind} {blob}\n", fx.host, fx.port),
    )
    .unwrap();
    let hashed = Command::new("ssh-keygen")
        .args(["-q", "-H", "-f"])
        .arg(&user_file)
        .status()
        .unwrap();
    assert!(hashed.success());
    let text = std::fs::read_to_string(&user_file).unwrap();
    assert!(
        text.starts_with("|1|"),
        "ssh-keygen -H should have hashed the host: {text}"
    );

    let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
    spec.user_known_hosts = Some(user_file.clone());
    let tunnel = Tunnel::open(spec.clone(), Arc::new(NoPrompts))
        .expect("trusted through the hashed user entry");
    assert_eq!(
        redis_ping(tunnel.local_addr(&fx.redis).unwrap()),
        "+PONG\r\n"
    );
    drop(tunnel);
    assert!(
        !fx.app_known_hosts().exists(),
        "the app file is only written by trust_host_key"
    );

    let revoked = format!("@revoked [{}]:{} {kind} {blob}\n{text}", fx.host, fx.port);
    std::fs::write(&user_file, &revoked).unwrap();
    let error = expect_err(Tunnel::open(spec, Arc::new(NoPrompts)));
    assert!(
        matches!(error, TunnelError::HostKeyRevoked { .. }),
        "{error:?}"
    );
    assert_eq!(error.code(), "tunnel.host_key_mismatch");
    assert_eq!(
        std::fs::read_to_string(&user_file).unwrap(),
        revoked,
        "the user's file is read-only"
    );
}

// -------------------------------------------------------------------------------------------- forwarding

#[test]
#[ignore = "needs tablex-test-sshd"]
fn forward_to_a_closed_port_is_reported_as_refused() {
    let fx = Fixture::new();
    let closed = Target::new(fx.pg.host.clone(), 1);
    let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
    spec.targets = vec![closed.clone(), fx.pg.clone()];
    let tunnel = fx.open_trusting(spec).unwrap();

    let error = tunnel.probe(&closed).unwrap_err();
    assert!(
        matches!(error, TunnelError::ForwardRefused { port: 1, .. }),
        "{error:?}"
    );
    assert_eq!(error.code(), "tunnel.forward_refused");

    let mut stream = connect(tunnel.local_addr(&closed).unwrap());
    let mut buf = [0u8; 1];
    let read = stream.read(&mut buf);
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "the connection must be closed, got {read:?}"
    );
    assert!(matches!(
        tunnel.last_forward_error(&closed),
        Some(TunnelError::ForwardRefused { port: 1, .. })
    ));

    // The other target on the same session keeps working and clears the stale failure.
    assert_eq!(
        postgres_first_reply(tunnel.local_addr(&fx.pg).unwrap()),
        b'R'
    );
    assert!(
        tunnel.last_forward_error(&closed).is_some(),
        "failures are tracked per target"
    );
    assert!(tunnel.last_forward_error(&fx.pg).is_none());
    assert!(tunnel.is_alive());
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn two_targets_share_one_ssh_session() {
    let fx = Fixture::new();
    assert!(
        fx.wait_for_sessions(0),
        "leftover sessions from earlier tests"
    );
    let tunnel = fx
        .open_trusting(fx.spec(Auth::Password(Secret::new(&fx.password))))
        .unwrap();
    let addrs: Vec<_> = tunnel.local_addrs().map(|(t, a)| (t.clone(), a)).collect();
    assert_eq!(addrs.len(), 2);
    assert_ne!(
        addrs[0].1.port(),
        addrs[1].1.port(),
        "one listener per target"
    );
    assert!(addrs.iter().all(|(_, a)| a.ip().is_loopback()));
    assert_reaches_both_databases(&fx, &tunnel);
    assert!(tunnel.local_addr(&Target::new("not-in-spec", 1)).is_none());
    assert_eq!(
        fx.session_count(),
        1,
        "both targets must ride one SSH session:\n{}",
        fx.container_sh("ps")
    );
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn dropping_the_tunnel_closes_listeners_and_the_session() {
    let fx = Fixture::new();
    assert!(fx.wait_for_sessions(0));
    let tunnel = fx
        .open_trusting(fx.spec(Auth::Password(Secret::new(&fx.password))))
        .unwrap();
    let addr = tunnel.local_addr(&fx.redis).unwrap();
    let held = connect(addr);
    assert_eq!(fx.session_count(), 1);
    drop(tunnel);
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_err(),
        "the listener must be gone"
    );
    drop(held);
    assert!(fx.wait_for_sessions(0), "the SSH session must be closed");
}

/// The server drops the session: the tunnel notices at once and closes its listeners.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn a_killed_session_closes_the_listeners() {
    let fx = Fixture::new();
    assert!(fx.wait_for_sessions(0));
    let tunnel = fx
        .open_trusting(fx.spec(Auth::Password(Secret::new(&fx.password))))
        .unwrap();
    let addr = tunnel.local_addr(&fx.redis).unwrap();
    assert_eq!(redis_ping(addr), "+PONG\r\n");

    let started = Instant::now();
    fx.container_sh("pkill -f '[s]shd.*: tablex'");
    while tunnel.is_alive() && started.elapsed() < Duration::from_secs(10) {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!tunnel.is_alive(), "session death must be noticed");
    println!(
        "killed session noticed {:?} after the kill command was issued",
        started.elapsed()
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_err(),
        "listener must be closed"
    );
    assert!(matches!(tunnel.probe(&fx.pg), Err(TunnelError::Closed)));
}

struct Unpause(String);
impl Drop for Unpause {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["unpause", &self.0]).output();
    }
}

/// A silent peer (container frozen, TCP still open): keepalives go unanswered and the tunnel gives up.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn keepalive_detects_a_frozen_server_and_closes_the_listeners() {
    let fx = Fixture::new();
    let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
    spec.timeouts.keepalive = Duration::from_secs(1);
    let tunnel = fx.open_trusting(spec).unwrap();
    let addr = tunnel.local_addr(&fx.redis).unwrap();
    assert_eq!(redis_ping(addr), "+PONG\r\n");

    let _unpause = Unpause(fx.container.clone());
    assert!(Command::new("docker")
        .args(["pause", &fx.container])
        .output()
        .unwrap()
        .status
        .success());
    let started = Instant::now();
    while tunnel.is_alive() && started.elapsed() < Duration::from_secs(30) {
        thread::sleep(Duration::from_millis(100));
    }
    println!(
        "frozen server noticed after {:?} (keepalive 1s, 3 misses)",
        started.elapsed()
    );
    assert!(
        !tunnel.is_alive(),
        "keepalive must declare the session dead"
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_err(),
        "listener must be closed"
    );
}

/// 64 threads x 25 connections each, every one a fresh forwarded channel doing a real Redis PING.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn many_concurrent_forwarded_connections() {
    let fx = Fixture::new();
    let tunnel = Arc::new(
        fx.open_trusting(fx.spec(Auth::Password(Secret::new(&fx.password))))
            .unwrap(),
    );
    let addr = tunnel.local_addr(&fx.redis).unwrap();
    let started = Instant::now();
    let workers: Vec<_> = (0..64)
        .map(|_| {
            let tunnel = tunnel.clone();
            thread::spawn(move || {
                for _ in 0..25 {
                    assert_eq!(redis_ping(addr), "+PONG\r\n");
                }
                tunnel.is_alive()
            })
        })
        .collect();
    assert!(workers.into_iter().all(|w| w.join().unwrap()));
    let elapsed = started.elapsed();
    println!(
        "1600 forwarded connections (64 parallel): {elapsed:?} ({:.0} conn/s)",
        1600.0 / elapsed.as_secs_f64()
    );
}

/// Streams 50 MB through the tunnel to an echo server running in this process (reached from the container
/// via host.docker.internal) and back, verifying every byte.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn throughput_50mb_echo() {
    const TOTAL: usize = 50 * 1024 * 1024;
    let fx = Fixture::new();
    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut stream, _) = echo.accept().unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 || stream.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });

    let target = Target::new("host.docker.internal", echo_port);
    let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
    spec.targets = vec![target.clone()];
    let tunnel = fx.open_trusting(spec).unwrap();
    let mut writer = connect(tunnel.local_addr(&target).unwrap());
    let mut reader = writer.try_clone().unwrap();
    writer.set_read_timeout(None).unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();

    let pattern = |offset: usize| (offset.wrapping_mul(31).wrapping_add(offset >> 8)) as u8;
    let started = Instant::now();
    let sender = thread::spawn(move || {
        let mut chunk = vec![0u8; 64 * 1024];
        let mut sent = 0;
        while sent < TOTAL {
            let n = chunk.len().min(TOTAL - sent);
            chunk[..n]
                .iter_mut()
                .enumerate()
                .for_each(|(i, b)| *b = pattern(sent + i));
            writer.write_all(&chunk[..n]).unwrap();
            sent += n;
        }
        writer
    });
    let mut buf = vec![0u8; 64 * 1024];
    let mut received = 0;
    while received < TOTAL {
        let n = reader.read(&mut buf).expect("echo read");
        assert!(n > 0, "tunnel closed after {received} bytes");
        assert!(
            buf[..n]
                .iter()
                .enumerate()
                .all(|(i, b)| *b == pattern(received + i)),
            "corruption near byte {received}"
        );
        received += n;
    }
    let elapsed = started.elapsed();
    let _writer = sender.join().unwrap();
    let mb = TOTAL as f64 / (1024.0 * 1024.0);
    println!(
        "throughput: {mb:.0} MB out and {mb:.0} MB back in {elapsed:?} = {:.1} MB/s each way ({:.1} MB/s total), build: {}",
        mb / elapsed.as_secs_f64(),
        2.0 * mb / elapsed.as_secs_f64(),
        if cfg!(debug_assertions) { "debug" } else { "release" },
    );
    assert!(mb / elapsed.as_secs_f64() > 2.0, "throughput collapsed");
}

// ------------------------------------------------------------------ host-side servers for the wedge tests

fn flood_pattern(offset: usize) -> u8 {
    (offset.wrapping_mul(131) ^ (offset >> 9)) as u8
}

/// A server on this host (reached from the container via host.docker.internal) that sends `total`
/// patterned bytes to every connection and then closes it.
fn flood_server(total: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut chunk = vec![0u8; 64 * 1024];
                let mut sent = 0;
                while sent < total {
                    let n = chunk.len().min(total - sent);
                    chunk[..n]
                        .iter_mut()
                        .enumerate()
                        .for_each(|(i, b)| *b = flood_pattern(sent + i));
                    if stream.write_all(&chunk[..n]).is_err() {
                        return;
                    }
                    sent += n;
                }
            });
        }
    });
    port
}

/// Echoes every connection.
fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 || stream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// Accepts, reads nothing for `hold`, then closes: an upload to it becomes window-blocked and is then cut.
fn sink_then_close_server(hold: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                thread::sleep(hold);
                drop(stream);
            });
        }
    });
    port
}

fn try_echo(addr: SocketAddr, within: Duration) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, within) else {
        return false;
    };
    stream.set_read_timeout(Some(within)).unwrap();
    stream.set_write_timeout(Some(within)).unwrap();
    let mut reply = [0u8; 4];
    stream.write_all(b"ping").is_ok() && stream.read_exact(&mut reply).is_ok() && &reply == b"ping"
}

fn wait_until(within: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The whole tunnel budget is held by a reader that never reads while another forward is being sent data and
/// a third one uploads. Nothing may wait for ever on the dead reader's backlog, and its teardown must give
/// the budget back. Against a real OpenSSH server (an in-process russh server freezes itself here).
#[test]
#[ignore = "needs tablex-test-sshd"]
fn tunnel_budget_held_by_a_dead_reader_never_wedges_the_tunnel() {
    const BUDGET: usize = 1 << 20;
    const LIVE_TOTAL: usize = 4 << 20;
    let fx = Fixture::new();
    let dead_target = Target::new("host.docker.internal", flood_server(64 << 20));
    let live_target = Target::new("host.docker.internal", flood_server(LIVE_TOTAL));
    let echo_target = Target::new("host.docker.internal", echo_server());
    let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
    spec.targets = vec![
        dead_target.clone(),
        live_target.clone(),
        echo_target.clone(),
    ];
    spec.forward_buffer_bytes = BUDGET;
    spec.tunnel_buffer_bytes = BUDGET;
    spec.timeouts.forward_stall = Duration::from_secs(2);
    let tunnel = fx.open_trusting(spec).unwrap();
    let echo_addr = tunnel.local_addr(&echo_target).unwrap();

    let _dead = connect(tunnel.local_addr(&dead_target).unwrap());
    assert!(
        wait_until(Duration::from_secs(20), || tunnel.buffered_bytes()
            >= BUDGET - (64 << 10)),
        "the dead reader should fill the tunnel budget (at {})",
        tunnel.buffered_bytes()
    );

    // Started while the budget is exhausted: a live reader on another forward and an uploader.
    let mut live = connect(tunnel.local_addr(&live_target).unwrap());
    live.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let reader = thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        let mut received = 0;
        while received < LIVE_TOTAL {
            let n = live
                .read(&mut buf)
                .expect("the live forward must stay open");
            assert!(n > 0, "live forward closed after {received} bytes");
            assert!(
                buf[..n]
                    .iter()
                    .enumerate()
                    .all(|(i, b)| *b == flood_pattern(received + i)),
                "corruption near byte {received}"
            );
            received += n;
        }
    });
    let uploader =
        thread::spawn(move || (0..5).all(|_| try_echo(echo_addr, Duration::from_secs(20))));

    assert!(
        wait_until(Duration::from_secs(20), || matches!(
            tunnel.last_forward_error(&dead_target),
            Some(TunnelError::ForwardStalled { .. })
        )),
        "the dead reader must be closed as stalled: {:?}",
        tunnel.last_forward_error(&dead_target)
    );
    reader
        .join()
        .expect("the live reader must receive every byte");
    assert!(uploader.join().unwrap(), "the uploader must get its echoes");
    assert!(tunnel.is_alive());
    assert!(tunnel.last_forward_error(&live_target).is_none());
    assert!(
        wait_until(Duration::from_secs(5), || tunnel.buffered_bytes() == 0),
        "the closed forward must give its bytes back (still {})",
        tunnel.buffered_bytes()
    );
    assert!(
        try_echo(echo_addr, Duration::from_secs(5)),
        "the tunnel keeps serving new forwards"
    );
}

/// The russh defect behind `outbound_liveness`: the remote end of a forward goes away while our upload to it
/// is window-blocked. The tunnel must then either keep working or be declared dead within a bounded time —
/// never stay "alive" while no new forward can be opened.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn remote_close_during_window_blocked_upload_does_not_freeze_the_tunnel() {
    let fx = Fixture::new();
    let sink_target = Target::new(
        "host.docker.internal",
        sink_then_close_server(Duration::from_secs(3)),
    );
    let echo_target = Target::new("host.docker.internal", echo_server());
    let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
    spec.targets = vec![sink_target.clone(), echo_target.clone()];
    spec.timeouts.keepalive = Duration::from_secs(1);
    spec.timeouts.forward_stall = Duration::from_secs(2);
    spec.timeouts.outbound_liveness = Duration::from_secs(4);
    spec.timeouts.forward = Duration::from_secs(3);
    let tunnel = fx.open_trusting(spec).unwrap();
    let echo_addr = tunnel.local_addr(&echo_target).unwrap();
    assert!(
        try_echo(echo_addr, Duration::from_secs(5)),
        "the tunnel works before the upload"
    );

    // Upload far more than the sink (which reads nothing) and the SSH window can take, until it is cut.
    let mut upload = connect(tunnel.local_addr(&sink_target).unwrap());
    // Longer than the sink's 3 s hold, so it is the remote end that goes away first.
    upload
        .set_write_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    let uploader = thread::spawn(move || {
        let chunk = vec![0x5a_u8; 64 * 1024];
        let started = Instant::now();
        let mut sent = 0usize;
        while started.elapsed() < Duration::from_secs(8) && upload.write_all(&chunk).is_ok() {
            sent += chunk.len();
        }
        sent
    });
    let sent = uploader.join().unwrap();
    println!("uploaded {sent} bytes before the sink was cut");

    // Liveness probe every 1 s with a 4 s deadline: a wedge is declared within ~6 s.
    let started = Instant::now();
    let mut echoes = 0;
    let outcome = loop {
        if !tunnel.is_alive() {
            break "declared dead";
        }
        if try_echo(echo_addr, Duration::from_secs(4)) {
            echoes += 1;
            if echoes >= 3 {
                break "kept working";
            }
        } else {
            echoes = 0;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the tunnel looks alive but opens no forward {:?} after the remote close",
            started.elapsed()
        );
        thread::sleep(Duration::from_millis(200));
    };
    println!(
        "after a remote close during a window-blocked upload the tunnel {outcome} ({:?})",
        started.elapsed()
    );
    if outcome == "declared dead" {
        // A dead tunnel closes its listeners, so the driver fails fast and the app reconnects.
        assert!(wait_until(Duration::from_secs(5), || {
            TcpStream::connect_timeout(&echo_addr, Duration::from_millis(300)).is_err()
        }));
    }
}

// ======================================================================================= T3: jump hosts

/// The container as seen from itself: its own sshd on 127.0.0.1:22. With the jump on 127.0.0.1:52222 this is
/// a chain of two real OpenSSH sessions.
const INNER_SSHD: (&str, u16) = ("127.0.0.1", 22);

impl Fixture {
    fn jump(&self, auth: Auth) -> Jump {
        let mut jump = Jump::new(self.host.clone(), self.user.clone(), auth);
        jump.port = self.port;
        jump
    }

    /// A chain: `jump` (52222 from the host) then the same container's inner sshd, with PG and Redis behind.
    fn chain_spec(&self, jump_auth: Auth, bastion_auth: Auth) -> TunnelSpec {
        let mut spec = self.spec(bastion_auth);
        spec.host = INNER_SSHD.0.into();
        spec.port = INNER_SSHD.1;
        spec.jumps = vec![self.jump(jump_auth)];
        spec
    }

    /// Opens, trusting what each `HostKeyUnknown` names (host and port **of the error**), until it works.
    /// Returns the tunnel and the `host:port` pairs trusted, in order.
    fn open_chain_trusting(
        &self,
        spec: &TunnelSpec,
        prompts: Arc<dyn Prompts>,
    ) -> (Tunnel, Vec<(String, u16)>) {
        let mut trusted = Vec::new();
        for _ in 0..4 {
            match Tunnel::open(spec.clone(), prompts.clone()) {
                Err(TunnelError::HostKeyUnknown {
                    host,
                    port,
                    public_key,
                    ..
                }) => {
                    trust_host_key(spec, &host, port, &public_key).unwrap();
                    trusted.push((host, port));
                }
                Ok(tunnel) => return (tunnel, trusted),
                Err(other) => panic!("chain did not open: {other}"),
            }
        }
        panic!("too many unknown hosts: {trusted:?}");
    }
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn a_two_hop_chain_reaches_the_databases_and_reports_each_hops_unknown_key() {
    let fx = Fixture::new();
    let password = || Auth::Password(Secret::new(&fx.password));
    let spec = fx.chain_spec(password(), password());
    let (tunnel, trusted) = fx.open_chain_trusting(&spec, Arc::new(NoPrompts));
    // Hop 1 first, then hop 2 with its own host:port: the second report names 127.0.0.1:22, not the jump.
    assert_eq!(
        trusted,
        [
            (fx.host.clone(), fx.port),
            (INNER_SSHD.0.to_owned(), INNER_SSHD.1)
        ]
    );
    assert!(tunnel.is_alive());
    assert_eq!(tunnel.via(), format!("{}@127.0.0.1:22", fx.user));
    assert_reaches_both_databases(&fx, &tunnel);
    let recorded = std::fs::read_to_string(fx.app_known_hosts()).unwrap();
    assert_eq!(recorded.lines().count(), 2, "{recorded}");

    // Both hops are recorded now: the next open needs no question. Two sshd sessions exist for the chain.
    let again = Tunnel::open(spec, Arc::new(NoPrompts)).unwrap();
    assert_reaches_both_databases(&fx, &again);
    assert!(
        fx.wait_for_sessions(4),
        "two chains, two sshd sessions each"
    );
    drop((tunnel, again));
    assert!(
        fx.wait_for_sessions(0),
        "dropping the tunnels ends every hop"
    );
}

/// Every authentication method works on a jump (the bastion keeps using a password, so the method under test
/// is the only thing that changes).
#[test]
#[ignore = "needs tablex-test-sshd"]
fn every_auth_method_works_on_a_jump() {
    let fx = Fixture::new();
    let bastion = || Auth::Password(Secret::new(&fx.password));
    let mut methods: Vec<(&str, Auth, Arc<dyn Prompts>)> = vec![
        (
            "password",
            Auth::Password(Secret::new(&fx.password)),
            Arc::new(NoPrompts),
        ),
        (
            "ed25519",
            fx.key_auth("TABLEX_TEST_SSHD_KEY_ED25519", None),
            Arc::new(NoPrompts),
        ),
        (
            "ecdsa",
            fx.key_auth("TABLEX_TEST_SSHD_KEY_ECDSA", None),
            Arc::new(NoPrompts),
        ),
        (
            "rsa",
            fx.key_auth("TABLEX_TEST_SSHD_KEY_RSA", None),
            Arc::new(NoPrompts),
        ),
        (
            "encrypted ed25519",
            fx.key_auth("TABLEX_TEST_SSHD_KEY_ED25519_ENC", Some(&fx.passphrase)),
            Arc::new(NoPrompts),
        ),
        (
            "keyboard-interactive",
            Auth::KeyboardInteractive,
            ScriptedPrompts::new(Some(&fx.password)),
        ),
    ];
    #[cfg(unix)]
    let agent = {
        let agent = PrivateAgent::start(fx.dir.path());
        agent.add(&fx.key("TABLEX_TEST_SSHD_KEY_ED25519"));
        methods.push((
            "ssh-agent",
            Auth::Agent {
                socket: Some(agent.socket.clone()),
            },
            Arc::new(NoPrompts),
        ));
        agent
    };
    for (name, jump_auth, prompts) in methods {
        let spec = fx.chain_spec(jump_auth, bastion());
        let (tunnel, _) = fx.open_chain_trusting(&spec, prompts);
        assert_eq!(
            redis_ping(tunnel.local_addr(&fx.redis).unwrap()),
            "+PONG\r\n",
            "jump auth: {name}"
        );
    }
    #[cfg(unix)]
    drop(agent);
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn a_wrong_secret_on_either_hop_names_that_hop() {
    let fx = Fixture::new();
    let good = || Auth::Password(Secret::new(&fx.password));
    let bad = || Auth::Password(Secret::new("wrong-hop-needle"));
    for (spec, expected) in [
        (
            fx.chain_spec(bad(), good()),
            format!("jump 1 of 1 {}@{}:{}: ", fx.user, fx.host, fx.port),
        ),
        (
            fx.chain_spec(good(), bad()),
            format!("bastion {}@127.0.0.1:22: ", fx.user),
        ),
    ] {
        // Trust whatever is asked first so the credential check is reached.
        let mut last = None;
        for _ in 0..4 {
            match Tunnel::open(spec.clone(), Arc::new(NoPrompts)) {
                Err(TunnelError::HostKeyUnknown {
                    host,
                    port,
                    public_key,
                    ..
                }) => trust_host_key(&spec, &host, port, &public_key).unwrap(),
                other => {
                    last = Some(expect_err(other));
                    break;
                }
            }
        }
        let error = last.expect("an error");
        let shown = error.to_string();
        assert!(shown.starts_with(&expected), "{shown} (wanted {expected})");
        assert_eq!(error.code(), "tunnel.auth");
        assert!(!format!("{shown} {error:?}").contains("wrong-hop-needle"));
    }
}

/// A TCP relay in front of the jump that the test can cut, to kill hop 1 without touching hop 2's sshd.
struct Cutter {
    port: u16,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
}

impl Cutter {
    fn start(to: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sockets: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let kept = sockets.clone();
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let Ok(upstream) = TcpStream::connect(to) else {
                    continue;
                };
                {
                    let mut kept = kept.lock().unwrap();
                    kept.extend(client.try_clone());
                    kept.extend(upstream.try_clone());
                }
                let (mut c_in, mut u_out) =
                    (client.try_clone().unwrap(), upstream.try_clone().unwrap());
                let (mut u_in, mut c_out) = (upstream, client);
                thread::spawn(move || {
                    let _ = std::io::copy(&mut c_in, &mut u_out);
                });
                thread::spawn(move || {
                    let _ = std::io::copy(&mut u_in, &mut c_out);
                });
            }
        });
        Self { port, sockets }
    }

    fn cut(&self) {
        for socket in self.sockets.lock().unwrap().drain(..) {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[test]
#[ignore = "needs tablex-test-sshd"]
fn a_dead_hop_ends_the_tunnel_whichever_hop_it_is() {
    let fx = Fixture::new();
    assert!(fx.wait_for_sessions(0));
    let auth = || Auth::Password(Secret::new(&fx.password));

    // Hop 1 dies (its TCP connection is cut).
    let cutter = Cutter::start(format!("{}:{}", fx.host, fx.port).parse().unwrap());
    let mut spec = fx.chain_spec(auth(), auth());
    spec.jumps[0].port = cutter.port;
    spec.jumps[0].host = "127.0.0.1".into();
    let (tunnel, _) = fx.open_chain_trusting(&spec, Arc::new(NoPrompts));
    let addr = tunnel.local_addr(&fx.redis).unwrap();
    assert_eq!(redis_ping(addr), "+PONG\r\n");
    cutter.cut();
    assert!(
        wait_until(Duration::from_secs(10), || !tunnel.is_alive()),
        "hop 1 died"
    );
    assert!(wait_until(Duration::from_secs(5), || {
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err()
    }));
    assert!(matches!(tunnel.probe(&fx.pg), Err(TunnelError::Closed)));
    drop(tunnel);

    // Hop 2 dies (the newest sshd session in the container is the inner one).
    assert!(fx.wait_for_sessions(0));
    let spec = fx.chain_spec(auth(), auth());
    let (tunnel, _) = fx.open_chain_trusting(&spec, Arc::new(NoPrompts));
    assert!(fx.wait_for_sessions(2));
    fx.container_sh("pkill -n -f '[s]shd.*: tablex'");
    assert!(
        wait_until(Duration::from_secs(10), || !tunnel.is_alive()),
        "hop 2 died but the tunnel is alive"
    );
}

// ======================================================================================= T3: proxies

fn login_proxy(protocol: Protocol) -> TestProxy {
    TestProxy::start(ProxyOptions::new(protocol).with_login("proxy-user", "proxy-pw-needle"))
}

fn proxy_spec(proxy: &TestProxy, protocol: Protocol, password: &str) -> ProxySpec {
    let mut spec = ProxySpec::new(
        match protocol {
            Protocol::Socks5 => ProxyKind::Socks5,
            Protocol::Http => ProxyKind::HttpConnect,
        },
        "127.0.0.1",
        proxy.addr.port(),
    );
    spec.credentials = Some(ProxyCredentials {
        user: "proxy-user".into(),
        password: Secret::new(password),
    });
    spec
}

/// SSH to the real sshd through a proxy that wants a login; the databases behind answer through the tunnel.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn ssh_through_socks5_and_http_connect_proxies_with_a_login() {
    let fx = Fixture::new();
    for protocol in [Protocol::Socks5, Protocol::Http] {
        let proxy = login_proxy(protocol);
        let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
        spec.proxy = Some(proxy_spec(&proxy, protocol, "proxy-pw-needle"));
        let tunnel = fx
            .open_trusting(spec)
            .unwrap_or_else(|e| panic!("{protocol:?}: {e}"));
        assert_reaches_both_databases(&fx, &tunnel);
        // At most the unknown-host attempt and the real one (the host is recorded after the first protocol); the databases ride inside SSH, never through the proxy.
        let sshd = format!("127.0.0.1:{}", fx.port);
        assert!(
            (1..=2).contains(&proxy.tunnels()),
            "{protocol:?}: {:?}",
            proxy.requests()
        );
        assert!(
            proxy.requests().iter().all(|r| r.ends_with(&sshd)),
            "{:?}",
            proxy.requests()
        );
        drop(tunnel);

        // A wrong proxy password never reaches the SSH server.
        let mut spec = fx.spec(Auth::Password(Secret::new(&fx.password)));
        spec.proxy = Some(proxy_spec(&proxy, protocol, "WRONG-proxy-needle"));
        let error = expect_err(Tunnel::open(spec, Arc::new(NoPrompts)));
        assert_eq!(error.code(), "tunnel.proxy", "{error}");
        assert!(!format!("{error} {error:?}").contains("WRONG-proxy-needle"));
    }
}

/// The proxy carries the TCP connection to the first jump; the second hop and the databases ride inside SSH.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn a_proxy_in_front_of_a_two_hop_chain() {
    let fx = Fixture::new();
    let proxy = login_proxy(Protocol::Socks5);
    let auth = || Auth::Password(Secret::new(&fx.password));
    let mut spec = fx.chain_spec(auth(), auth());
    spec.proxy = Some(proxy_spec(&proxy, Protocol::Socks5, "proxy-pw-needle"));
    let (tunnel, trusted) = fx.open_chain_trusting(&spec, Arc::new(NoPrompts));
    assert_eq!(trusted.len(), 2);
    assert_reaches_both_databases(&fx, &tunnel);
    // Two unknown-host attempts and the real one: all to the first jump, none for hop 2 or the databases.
    let first_jump = format!("127.0.0.1:{}", fx.port);
    assert_eq!(proxy.requests().len(), 3, "{:?}", proxy.requests());
    assert!(proxy.requests().iter().all(|r| r.ends_with(&first_jump)));
}

/// No SSH: the in-process proxy connects to the test PostgreSQL / Redis containers published on the host.
#[test]
#[ignore = "needs tablex-test-sshd"]
fn proxy_only_route_reaches_redis_and_postgres() {
    let _fx = Fixture::new();
    let redis = Target::new("127.0.0.1", 56379);
    let pg = Target::new("127.0.0.1", 55432);
    for protocol in [Protocol::Socks5, Protocol::Http] {
        let proxy = login_proxy(protocol);
        let mut route = ProxyRoute::new(proxy_spec(&proxy, protocol, "proxy-pw-needle"));
        route.targets = vec![redis.clone(), pg.clone()];
        let tunnel = Tunnel::open_proxy(route.clone()).unwrap();
        assert!(tunnel
            .via()
            .ends_with(&format!("proxy 127.0.0.1:{}", proxy.addr.port())));
        assert_eq!(redis_ping(tunnel.local_addr(&redis).unwrap()), "+PONG\r\n");
        assert!(matches!(
            postgres_first_reply(tunnel.local_addr(&pg).unwrap()),
            b'R' | b'E'
        ));
        tunnel.probe(&redis).unwrap();
        assert!(tunnel.is_alive());
        drop(tunnel);

        // Wrong proxy password: the local port exists, the forward fails with tunnel.proxy.
        route.proxy = proxy_spec(&proxy, protocol, "WRONG-proxy-needle");
        let tunnel = Tunnel::open_proxy(route).unwrap();
        let error = tunnel.probe(&redis).unwrap_err();
        assert_eq!(error.code(), "tunnel.proxy", "{error}");
        assert!(!format!("{error} {error:?}").contains("WRONG-proxy-needle"));
    }
}
