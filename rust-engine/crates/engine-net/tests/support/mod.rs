//! In-process SSH server (russh's own server side) for tests that need to script the *server's* behaviour:
//! host key type, authentication rounds, forward targets that echo / flood / stall / confirm late. Listens on
//! 127.0.0.1 only and runs on its own runtime so it never shares threads with the code under test.
#![allow(dead_code)]

use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::pin;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

pub mod proxy;

use russh::keys::{decode_secret_key, PrivateKey};
use russh::server::{self, Auth, ChannelOpenHandle, Handler, Msg, Response, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, MethodKind, MethodSet};
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};

/// Forward targets understood by the test server (the port of the `direct-tcpip` request selects behaviour).
pub const PORT_ECHO: u16 = 7;
pub const PORT_FLOOD: u16 = 8;
pub const PORT_DELAYED: u16 = 9;
pub const PORT_SINK_STALL: u16 = 10;
/// Sends 4 MiB, then idles.
pub const PORT_FLOOD_SMALL: u16 = 11;
const SMALL_FLOOD_BYTES: usize = 4 << 20;

/// The byte the flood targets send at stream offset `offset` (so receivers can verify content).
pub fn flood_byte(offset: u64) -> u8 {
    (offset.wrapping_mul(31) ^ (offset >> 9)) as u8
}

#[derive(Clone, Copy)]
pub enum HostKey {
    Ed25519,
    Rsa,
}

#[derive(Clone)]
pub enum AuthPlan {
    /// `password` method; accepts exactly `password`.
    Password { password: String },
    /// `publickey` method; any offered key is accepted.
    PublicKey,
    /// `password` method; the server drops the connection while handling it.
    DropOnPassword,
    /// `password` succeeds only as a first factor (partial success), then one keyboard-interactive round
    /// whose single answer must equal `answer`.
    PasswordThenKi {
        password: String,
        prompt: (String, bool),
        answer: String,
    },
    /// Keyboard-interactive only; one prompt list per round; accepted when the last round's last answer equals
    /// `accept_last`.
    Ki {
        rounds: Vec<Vec<(String, bool)>>,
        accept_last: String,
    },
}

#[derive(Clone)]
pub struct ServerOptions {
    pub host_key: HostKey,
    pub plan: AuthPlan,
    /// Initial window the *server* grants the client for each channel (client-to-server data).
    pub window_size: u32,
    /// How long `PORT_DELAYED` waits before confirming the channel.
    pub open_delay: Duration,
    /// How much `PORT_FLOOD` sends before it stops.
    pub flood_total: usize,
    /// A `direct-tcpip` request for any other port opens a real TCP connection to the requested host and
    /// port and relays it: this server then works as a jump host in front of another server.
    pub relay: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            host_key: HostKey::Ed25519,
            plan: AuthPlan::Password {
                password: "pw".into(),
            },
            window_size: 2 << 20,
            open_delay: Duration::from_secs(2),
            flood_total: 256 << 20,
            relay: false,
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub connections: AtomicUsize,
    pub connections_closed: AtomicUsize,
    /// `authentication_banner` calls: the server saw the client start user authentication.
    pub auth_started: AtomicUsize,
    /// Every `auth_*` request of any method.
    pub auth_requests: AtomicUsize,
    pub auth_success: AtomicUsize,
    pub passwords: Mutex<Vec<String>>,
    /// Answers of every keyboard-interactive round, in order.
    pub ki_responses: Mutex<Vec<Vec<String>>>,
    pub channels_closed: AtomicUsize,
    pub flood_sent: AtomicUsize,
}

impl Stats {
    pub fn get(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }
}

pub struct TestServer {
    pub addr: SocketAddr,
    /// The host key in OpenSSH one-line form.
    pub host_public_key: String,
    pub stats: Arc<Stats>,
    connections: Arc<Mutex<Vec<std::net::TcpStream>>>,
    runtime: Option<Runtime>,
    _keys: tempfile::TempDir,
}

fn methods_for(plan: &AuthPlan) -> MethodSet {
    let mut methods = MethodSet::empty();
    match plan {
        AuthPlan::Password { .. } | AuthPlan::DropOnPassword => methods.push(MethodKind::Password),
        AuthPlan::PublicKey => methods.push(MethodKind::PublicKey),
        AuthPlan::Ki { .. } => methods.push(MethodKind::KeyboardInteractive),
        AuthPlan::PasswordThenKi { .. } => {
            methods.push(MethodKind::Password);
            methods.push(MethodKind::KeyboardInteractive);
        }
    }
    methods
}

pub fn generate_key(dir: &Path, name: &str, kind: &str) -> (PrivateKey, String) {
    let path = dir.join(name);
    let mut command = Command::new("ssh-keygen");
    command
        .args(["-q", "-t", kind, "-N", "", "-C", "", "-f"])
        .arg(&path);
    if kind == "rsa" {
        command.args(["-b", "2048"]);
    }
    assert!(command.status().expect("ssh-keygen").success());
    let key = decode_secret_key(&std::fs::read_to_string(&path).unwrap(), None).unwrap();
    let public = key.public_key().to_openssh().unwrap();
    (key, public)
}

impl TestServer {
    pub fn start(options: ServerOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (key, host_public_key) = generate_key(
            dir.path(),
            "host_key",
            match options.host_key {
                HostKey::Ed25519 => "ed25519",
                HostKey::Rsa => "rsa",
            },
        );
        let methods = methods_for(&options.plan);
        let config = Arc::new(server::Config {
            keys: vec![key],
            methods,
            auth_rejection_time: Duration::from_millis(1),
            auth_rejection_time_initial: Some(Duration::ZERO),
            window_size: options.window_size,
            inactivity_timeout: None,
            ..server::Config::default()
        });

        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let stats = Arc::new(Stats::default());
        let connections = Arc::new(Mutex::new(Vec::new()));
        let (accept_stats, accept_connections) = (stats.clone(), connections.clone());
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_stats.connections.fetch_add(1, Ordering::SeqCst);
                let handler = TestHandler::new(options.clone(), accept_stats.clone());
                let config = config.clone();
                // Keep a handle on the socket so the test can sever the connection from the server side.
                let Ok(std_stream) = stream.into_std() else {
                    continue;
                };
                let closer = std_stream.try_clone().ok();
                if let Some(clone) = closer.as_ref().and_then(|c| c.try_clone().ok()) {
                    accept_connections.lock().unwrap().push(clone);
                }
                let Ok(stream) = tokio::net::TcpStream::from_std(std_stream) else {
                    continue;
                };
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                    // The extra descriptors above would otherwise keep the socket open after the session ends.
                    if let Some(closer) = closer {
                        let _ = closer.shutdown(std::net::Shutdown::Both);
                    }
                });
            }
        });
        Self {
            addr,
            host_public_key,
            stats,
            connections,
            runtime: Some(runtime),
            _keys: dir,
        }
    }

    /// Abruptly closes every client connection (the server vanishes).
    pub fn drop_connections(&self) {
        for socket in self.connections.lock().unwrap().drain(..) {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

struct TestHandler {
    options: ServerOptions,
    stats: Arc<Stats>,
    ki_round: usize,
    methods: MethodSet,
    echo: HashSet<ChannelId>,
    stalled: HashSet<ChannelId>,
}

impl TestHandler {
    fn new(options: ServerOptions, stats: Arc<Stats>) -> Self {
        let methods = methods_for(&options.plan);
        Self {
            options,
            stats,
            ki_round: 0,
            methods,
            echo: HashSet::new(),
            stalled: HashSet::new(),
        }
    }

    /// A rejection that keeps the offered methods (an empty list makes russh hang up).
    fn reject(&self) -> Auth {
        Auth::Reject {
            proceed_with_methods: Some(self.methods.clone()),
            partial_success: false,
        }
    }

    fn prompts(round: &[(String, bool)]) -> Auth {
        Auth::Partial {
            name: "test".into(),
            instructions: "".into(),
            prompts: round
                .iter()
                .map(|(text, echo)| (text.clone().into(), *echo))
                .collect::<Vec<_>>()
                .into(),
        }
    }

    fn accept(&self) -> Auth {
        self.stats.auth_success.fetch_add(1, Ordering::SeqCst);
        Auth::Accept
    }
}

impl Drop for TestHandler {
    fn drop(&mut self) {
        self.stats.connections_closed.fetch_add(1, Ordering::SeqCst);
    }
}

impl Handler for TestHandler {
    type Error = russh::Error;

    async fn authentication_banner(&mut self) -> Result<Option<String>, Self::Error> {
        self.stats.auth_started.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.stats.auth_requests.fetch_add(1, Ordering::SeqCst);
        Ok(match self.options.plan {
            AuthPlan::PublicKey => self.accept(),
            _ => self.reject(),
        })
    }

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        self.stats.auth_requests.fetch_add(1, Ordering::SeqCst);
        Ok(self.reject())
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.stats.auth_requests.fetch_add(1, Ordering::SeqCst);
        self.stats
            .passwords
            .lock()
            .unwrap()
            .push(password.to_owned());
        match self.options.plan.clone() {
            AuthPlan::Password { password: expected } if expected == password => Ok(self.accept()),
            AuthPlan::DropOnPassword => Err(russh::Error::Disconnect),
            AuthPlan::PasswordThenKi {
                password: expected, ..
            } if expected == password => {
                let mut next = MethodSet::empty();
                next.push(MethodKind::KeyboardInteractive);
                Ok(Auth::Reject {
                    proceed_with_methods: Some(next),
                    partial_success: true,
                })
            }
            _ => Ok(self.reject()),
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        self.stats.auth_requests.fetch_add(1, Ordering::SeqCst);
        let rounds: Vec<Vec<(String, bool)>> = match &self.options.plan {
            AuthPlan::Ki { rounds, .. } => rounds.clone(),
            AuthPlan::PasswordThenKi { prompt, .. } => vec![vec![prompt.clone()]],
            _ => return Ok(self.reject()),
        };
        let Some(response) = response else {
            self.ki_round = 0;
            return Ok(Self::prompts(&rounds[0]));
        };
        let answers: Vec<String> = response
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();
        self.stats
            .ki_responses
            .lock()
            .unwrap()
            .push(answers.clone());
        self.ki_round += 1;
        if self.ki_round < rounds.len() {
            return Ok(Self::prompts(&rounds[self.ki_round]));
        }
        let expected = match &self.options.plan {
            AuthPlan::Ki { accept_last, .. } => accept_last.clone(),
            AuthPlan::PasswordThenKi { answer, .. } => answer.clone(),
            _ => unreachable!("handled above"),
        };
        if answers.last() == Some(&expected) {
            Ok(self.accept())
        } else {
            Ok(self.reject())
        }
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        match port as u16 {
            PORT_ECHO => {
                self.echo.insert(channel.id());
                reply.accept().await;
            }
            PORT_SINK_STALL => {
                self.stalled.insert(channel.id());
                reply.accept().await;
            }
            PORT_FLOOD | PORT_FLOOD_SMALL => {
                reply.accept().await;
                let stats = self.stats.clone();
                let total = if port as u16 == PORT_FLOOD {
                    self.options.flood_total
                } else {
                    SMALL_FLOOD_BYTES
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let mut writer = channel.make_writer();
                    let mut chunk = vec![0u8; 64 * 1024];
                    let mut offset = 0u64;
                    while (offset as usize) < total {
                        let n = chunk.len().min(total - offset as usize);
                        for (i, byte) in chunk[..n].iter_mut().enumerate() {
                            *byte = flood_byte(offset + i as u64);
                        }
                        if writer.write_all(&chunk[..n]).await.is_err() {
                            break;
                        }
                        offset += n as u64;
                        stats.flood_sent.store(offset as usize, Ordering::SeqCst);
                    }
                    // Keep the channel open: the client decides when it ends.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                });
            }
            PORT_DELAYED => {
                let delay = self.options.open_delay;
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    reply.accept().await;
                    // Keep the channel object alive; the client is expected to close it.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(channel);
                });
            }
            _ if self.options.relay => {
                match tokio::net::TcpStream::connect((host, port as u16)).await {
                    Ok(mut target) => {
                        reply.accept().await;
                        tokio::spawn(async move {
                            let mut stream = channel.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
                        });
                    }
                    Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
                }
            }
            _ => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.stalled.contains(&channel) {
            // Never return: the session loop stops reading the socket, like a wedged peer.
            std::future::pending::<()>().await;
        }
        if self.echo.contains(&channel) {
            session.data(channel, data.to_vec())?;
        }
        Ok(())
    }

    /// Like sshd: when the client half-closes, close the target side (here: the whole channel).
    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.stats.channels_closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Polls `condition` every 20 ms until it holds or `within` elapses.
pub fn eventually(within: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    condition()
}

/// Minimal executor that polls `future` until it is ready or `abandon` is set; in the latter case the future
/// is dropped (what a UI does when the user cancels) and `None` is returned.
pub fn poll_until_abandoned<F: Future>(future: F, abandon: &AtomicBool) -> Option<F::Output> {
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
            return Some(value);
        }
        if abandon.load(Ordering::SeqCst) {
            return None;
        }
        thread::park_timeout(Duration::from_millis(10));
    }
}
