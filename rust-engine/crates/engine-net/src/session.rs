//! Connecting to the bastion, through its jump hosts and an optional proxy: TCP, SSH handshake per hop, host
//! key verification, and the socket-level safety nets (kill switch, stalled-write watchdog, transport-down
//! signal).

use std::io;
use std::net::Shutdown;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use russh::client::{self, Config, Handle, Handler};
use russh::keys::ssh_key::{Algorithm, HashAlg};
use russh::keys::PublicKeyOrCertificate;
use russh::{kex, Preferred};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{watch, Notify};

use crate::auth;
use crate::error::{ConfigError, TunnelError};
use crate::known_hosts::{HostKeyStatus, KnownHosts};
use crate::spec::{CancelToken, Hop, Prompts, TunnelSpec};
use crate::tunnel::describe;

/// Closes the TCP socket from outside russh. russh owns the stream inside a spawned session task; without
/// this, abandoning a half-finished handshake would leave that task and the socket alive until the peer
/// gives up.
#[derive(Clone)]
pub(crate) struct KillSwitch(Arc<std::net::TcpStream>);

impl KillSwitch {
    pub(crate) fn kill(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }

    /// True when the peer has closed (EOF) or reset the connection. Looks at the socket itself, so it works
    /// even while russh's session loop is not reading. Never consumes data.
    pub(crate) fn peer_closed(&self) -> bool {
        let mut byte = [0u8; 1];
        match self.0.peek(&mut byte) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => {
                e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::Interrupted
            }
        }
    }
}

/// Kills the socket when dropped unless disarmed: every early return of `establish` is covered.
struct KillGuard(Option<KillSwitch>);

impl KillGuard {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for KillGuard {
    fn drop(&mut self) {
        if let Some(switch) = &self.0 {
            switch.kill();
        }
    }
}

/// Transport health shared between the stream wrapper, the watchdog and the tunnel.
pub(crate) struct Watch {
    started: Instant,
    /// Milliseconds since `started` (+1) at which the current blocked write began; 0 when none is blocked.
    write_blocked_since: AtomicU64,
    blocked: Notify,
    down: Arc<watch::Sender<bool>>,
}

impl Watch {
    fn new(down: Arc<watch::Sender<bool>>) -> Self {
        Self {
            started: Instant::now(),
            write_blocked_since: AtomicU64::new(0),
            blocked: Notify::new(),
            down,
        }
    }

    fn write_blocked(&self) {
        let now = self.started.elapsed().as_millis() as u64 + 1;
        if self
            .write_blocked_since
            .compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.blocked.notify_one();
        }
    }

    fn write_progressed(&self) {
        self.write_blocked_since.store(0, Ordering::Release);
    }

    fn blocked_since(&self) -> Option<Instant> {
        match self.write_blocked_since.load(Ordering::Acquire) {
            0 => None,
            ms => Some(self.started + Duration::from_millis(ms - 1)),
        }
    }

    fn transport_down(&self) {
        self.down.send_replace(true);
    }
}

/// Closes the session when a socket write makes no progress for `limit`: a peer that stopped reading would
/// otherwise block russh's session loop forever (it cannot even run its keepalive timer then). Event driven:
/// idle until a write first blocks.
pub(crate) async fn watchdog(watch: Arc<Watch>, kill: KillSwitch, limit: Duration) {
    loop {
        watch.blocked.notified().await;
        while let Some(since) = watch.blocked_since() {
            let deadline = since + limit;
            if Instant::now() >= deadline {
                kill.kill();
                watch.transport_down();
                return;
            }
            tokio::time::sleep_until(deadline.into()).await;
        }
    }
}

/// The stream given to russh (the TCP socket of the first hop, the `direct-tcpip` channel of the later ones):
/// reports blocked writes and transport EOF / errors.
struct WatchedStream<S> {
    inner: S,
    watch: Arc<Watch>,
}

impl<S: AsyncRead + Unpin> AsyncRead for WatchedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let (before, wanted) = (buf.filled().len(), buf.remaining());
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        match &poll {
            Poll::Ready(Ok(())) if wanted > 0 && buf.filled().len() == before => {
                self.watch.transport_down()
            }
            Poll::Ready(Err(_)) => self.watch.transport_down(),
            _ => {}
        }
        poll
    }
}

impl<S> WatchedStream<S> {
    fn observe<T>(&self, poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        match &poll {
            Poll::Pending => self.watch.write_blocked(),
            Poll::Ready(Ok(_)) => self.watch.write_progressed(),
            Poll::Ready(Err(_)) => {
                self.watch.write_progressed();
                self.watch.transport_down();
            }
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WatchedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.observe(poll)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_flush(cx);
        self.observe(poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.observe(poll)
    }
}

/// russh callback object: verifies the host key *before* any credential is sent and reports session death.
pub(crate) struct Client {
    host: String,
    port: u16,
    known_hosts: Arc<KnownHosts>,
    /// Why the key was refused; russh itself only reports `UnknownKey`.
    verdict: Arc<Mutex<Option<TunnelError>>>,
    /// Flipped to `true` when the SSH session ends for any reason (and by `Tunnel::drop`).
    down: Arc<watch::Sender<bool>>,
}

impl Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            self.refuse(TunnelError::Connect {
                host: self.host.clone(),
                port: self.port,
                reason: "the server presented a certificate host key, which is not supported"
                    .into(),
            });
            return Ok(false);
        };
        let algorithm = key.algorithm().to_string();
        let sha256_fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let (host, port) = (self.host.clone(), self.port);
        match self.known_hosts.check(&host, port, key) {
            HostKeyStatus::Trusted => Ok(true),
            HostKeyStatus::Unknown => {
                let public_key = key.to_openssh().unwrap_or_default();
                self.refuse(TunnelError::HostKeyUnknown {
                    host,
                    port,
                    algorithm,
                    sha256_fingerprint,
                    public_key,
                });
                Ok(false)
            }
            HostKeyStatus::Changed { recorded_at } => {
                self.refuse(TunnelError::HostKeyMismatch {
                    host,
                    port,
                    algorithm,
                    sha256_fingerprint,
                    recorded_at,
                });
                Ok(false)
            }
            HostKeyStatus::TypeChanged {
                recorded_algorithms,
                location,
            } => {
                self.refuse(TunnelError::HostKeyTypeChanged {
                    host,
                    port,
                    presented_algorithm: algorithm,
                    sha256_fingerprint,
                    recorded_algorithms,
                    location,
                });
                Ok(false)
            }
            HostKeyStatus::Revoked => {
                self.refuse(TunnelError::HostKeyRevoked {
                    host,
                    port,
                    algorithm,
                    sha256_fingerprint,
                });
                Ok(false)
            }
        }
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        self.down.send_replace(true);
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(e) => Err(e),
        }
    }
}

impl Client {
    fn refuse(&self, error: TunnelError) {
        if let Ok(mut slot) = self.verdict.lock() {
            slot.get_or_insert(error);
        }
    }
}

/// russh options. Host key types already trusted for this host are moved to the front so a host recorded as
/// ssh-rsa is not reported "unknown" just because the server also has an ed25519 key. ECDH over the NIST
/// curves is appended for older servers; SHA-1 based `diffie-hellman-group14-sha1` and `ssh-rsa` signatures
/// are only offered when the spec opts in.
fn client_config(spec: &TunnelSpec, known: &[Algorithm]) -> Config {
    let mut preferred = Preferred::default();
    let mut kex_names = preferred.kex.to_vec();
    let mut extra = vec![
        kex::ECDH_SHA2_NISTP256,
        kex::ECDH_SHA2_NISTP384,
        kex::ECDH_SHA2_NISTP521,
    ];
    if spec.allow_legacy_algorithms {
        extra.push(kex::DH_G14_SHA1);
    } else {
        kex_names.retain(|name| *name != kex::DH_G14_SHA1);
    }
    for name in extra {
        if !kex_names.contains(&name) {
            kex_names.push(name);
        }
    }
    preferred.kex = kex_names.into();

    let same_family = |a: &Algorithm, b: &Algorithm| {
        matches!((a, b), (Algorithm::Rsa { .. }, Algorithm::Rsa { .. })) || a == b
    };
    let mut key_algorithms = preferred.key.to_vec();
    if spec.allow_legacy_algorithms {
        if !key_algorithms.contains(&Algorithm::Rsa { hash: None }) {
            key_algorithms.push(Algorithm::Rsa { hash: None });
        }
    } else {
        key_algorithms.retain(|a| *a != Algorithm::Rsa { hash: None });
    }
    key_algorithms.sort_by_key(|a| !known.iter().any(|k| same_family(a, k)));
    preferred.key = key_algorithms.into();

    Config {
        nodelay: true,
        keepalive_interval: Some(spec.timeouts.keepalive),
        keepalive_max: 3,
        // Packets queued inside russh per channel (about 2 MiB at the default packet size) once the
        // per-forward pump has stopped draining on purpose (backpressure). Kept small: this part is not
        // counted in the tunnel's byte budget.
        channel_buffer_size: 64,
        preferred,
        ..Config::default()
    }
}

fn transport_error(hop: &Hop<'_>, reason: impl std::fmt::Display) -> TunnelError {
    TunnelError::Connect {
        host: hop.host.to_owned(),
        port: hop.port,
        reason: reason.to_string(),
    }
}

fn load_known_hosts(spec: &TunnelSpec) -> Result<KnownHosts, ConfigError> {
    let mut known_hosts = KnownHosts::default();
    for read_only in [&spec.system_known_hosts, &spec.user_known_hosts]
        .into_iter()
        .flatten()
    {
        known_hosts.add_file(read_only)?;
    }
    known_hosts.add_file(&spec.app_known_hosts)?;
    Ok(known_hosts)
}

pub(crate) struct Established {
    /// The session of the bastion (the last hop).
    pub(crate) handle: Handle<Client>,
    /// The sessions of the jump hosts, first hop first. Each carries the transport of the next one, so they
    /// must stay open as long as `handle` is used.
    pub(crate) upstream: Vec<Handle<Client>>,
    /// Closes the TCP socket of the first hop (which carries every later hop).
    pub(crate) kill: KillSwitch,
    pub(crate) watch: Arc<Watch>,
}

/// SSH handshake (host key checked inside key exchange, before any credential) and authentication over
/// `stream`, for one hop.
#[allow(clippy::too_many_arguments)]
async fn connect_hop<S>(
    stream: S,
    spec: &TunnelSpec,
    hop: &Hop<'_>,
    known_hosts: &Arc<KnownHosts>,
    deadline: tokio::time::Instant,
    prompts: &Arc<dyn Prompts>,
    down: &Arc<watch::Sender<bool>>,
    kill: &KillSwitch,
    cancel: &CancelToken,
) -> Result<Handle<Client>, TunnelError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let known_algorithms = known_hosts.known_algorithms(hop.host, hop.port);
    let verdict = Arc::new(Mutex::new(None));
    let client = Client {
        host: hop.host.to_owned(),
        port: hop.port,
        known_hosts: known_hosts.clone(),
        verdict: verdict.clone(),
        down: down.clone(),
    };
    let config = Arc::new(client_config(spec, &known_algorithms));
    let limit = spec.timeouts.connect;
    let handshake =
        tokio::time::timeout_at(deadline, client::connect_stream(config, stream, client))
            .await
            .map_err(|_| TunnelError::Timeout {
                operation: "SSH connect",
                seconds: limit.as_secs(),
            })?;
    let mut handle = match handshake {
        Ok(handle) => handle,
        // A refused host key surfaces from russh as a generic error; the handler's verdict is the real cause.
        Err(generic) => {
            let cause = verdict.lock().ok().and_then(|mut v| v.take());
            return Err(cause.unwrap_or_else(|| transport_error(hop, generic)));
        }
    };
    auth::authenticate(&mut handle, spec, hop, prompts, cancel, down, kill).await?;
    Ok(handle)
}

/// Connects the first hop (through the proxy, if any), then every later hop through a `direct-tcpip` channel
/// of the one before, verifying each host key and authenticating each hop. `down` is signalled when any
/// session ends. Any failure (or the future being dropped) closes the socket, so no russh task outlives a
/// failed open.
pub(crate) async fn establish(
    spec: &TunnelSpec,
    prompts: &Arc<dyn Prompts>,
    down: Arc<watch::Sender<bool>>,
    cancel: &CancelToken,
) -> Result<Established, TunnelError> {
    let blocking_spec = spec.clone();
    let known_hosts = Arc::new(
        tokio::task::spawn_blocking(move || load_known_hosts(&blocking_spec))
            .await
            .map_err(|_| TunnelError::Closed)??,
    );
    let hops = spec.hops();
    let Some((first, later)) = hops.split_first() else {
        return Err(TunnelError::Closed);
    };

    let limit = spec.timeouts.connect;
    let timed_out = |operation: &'static str| TunnelError::Timeout {
        operation,
        seconds: limit.as_secs(),
    };
    let deadline = tokio::time::Instant::now() + limit;
    let dial = async {
        match &spec.proxy {
            Some(proxy) => proxy.connect(first.host, first.port).await,
            None => TcpStream::connect((first.host, first.port))
                .await
                .map_err(|e| transport_error(first, e)),
        }
    };
    let tcp = tokio::time::timeout_at(deadline, dial)
        .await
        .map_err(|_| {
            // A proxy that accepts the TCP connection and never answers is the proxy's fault, say so.
            first.wrap(timed_out(if spec.proxy.is_some() {
                "proxy connect"
            } else {
                "SSH connect"
            }))
        })?
        .map_err(|e| first.wrap(e))?;
    tcp.set_nodelay(true)
        .map_err(|e| first.wrap(transport_error(first, e)))?;
    let std_stream = tcp
        .into_std()
        .map_err(|e| first.wrap(transport_error(first, e)))?;
    let kill = KillSwitch(Arc::new(
        std_stream
            .try_clone()
            .map_err(|e| first.wrap(transport_error(first, e)))?,
    ));
    let guard = KillGuard(Some(kill.clone()));
    let tcp = TcpStream::from_std(std_stream).map_err(|e| first.wrap(transport_error(first, e)))?;

    let watch = Arc::new(Watch::new(down.clone()));
    let stream = WatchedStream {
        inner: tcp,
        watch: watch.clone(),
    };
    let mut handle = connect_hop(
        stream,
        spec,
        first,
        &known_hosts,
        deadline,
        prompts,
        &down,
        &kill,
        cancel,
    )
    .await
    .map_err(|e| first.wrap(e))?;

    // Each later hop is an SSH session inside a channel of the previous hop's session.
    let mut upstream = Vec::new();
    for (hop, via) in later.iter().zip(&hops) {
        let deadline = tokio::time::Instant::now() + limit;
        let opening = handle.channel_open_direct_tcpip(
            hop.host.to_owned(),
            u32::from(hop.port),
            "127.0.0.1",
            0,
        );
        let channel = match tokio::time::timeout_at(deadline, opening).await {
            Ok(Ok(channel)) => channel,
            Ok(Err(russh::Error::ChannelOpenFailure(failure))) => {
                return Err(via.wrap(TunnelError::ForwardRefused {
                    host: hop.host.to_owned(),
                    port: hop.port,
                    reason: describe(&failure),
                }))
            }
            Ok(Err(other)) => return Err(via.wrap(transport_error(via, other))),
            Err(_) => return Err(via.wrap(timed_out("SSH jump forward"))),
        };
        let stream = WatchedStream {
            inner: channel.into_stream(),
            watch: Arc::new(Watch::new(down.clone())),
        };
        let next = connect_hop(
            stream,
            spec,
            hop,
            &known_hosts,
            deadline,
            prompts,
            &down,
            &kill,
            cancel,
        )
        .await
        .map_err(|e| hop.wrap(e))?;
        upstream.push(std::mem::replace(&mut handle, next));
    }
    guard.disarm();
    Ok(Established {
        handle,
        upstream,
        kill,
        watch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Auth, Target};
    use russh::keys::ssh_key::EcdsaCurve;

    fn spec() -> TunnelSpec {
        let mut spec = TunnelSpec::new(
            "h.example.com",
            "u",
            Auth::KeyboardInteractive,
            "/tmp/kh".into(),
        );
        spec.targets.push(Target::new("db", 1));
        spec
    }

    #[test]
    fn known_host_key_types_are_negotiated_first() {
        let config = client_config(
            &spec(),
            &[
                Algorithm::Rsa { hash: None },
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP256,
                },
            ],
        );
        let order = config.preferred.key.to_vec();
        assert!(matches!(order[0], Algorithm::Ecdsa { .. }), "{order:?}");
        assert!(matches!(order[1], Algorithm::Rsa { .. }), "{order:?}");
        assert!(
            order.iter().position(|a| *a == Algorithm::Ed25519).unwrap() >= 3,
            "{order:?}"
        );
    }

    #[test]
    fn legacy_algorithms_are_opt_in() {
        let strict = client_config(&spec(), &[]);
        assert!(!strict.preferred.kex.contains(&kex::DH_G14_SHA1));
        assert!(!strict
            .preferred
            .key
            .contains(&Algorithm::Rsa { hash: None }));
        assert!(strict.preferred.kex.contains(&kex::ECDH_SHA2_NISTP256));
        assert_eq!(strict.preferred.kex[0], Preferred::default().kex[0]);

        let mut legacy_spec = spec();
        legacy_spec.allow_legacy_algorithms = true;
        let legacy = client_config(&legacy_spec, &[]);
        assert!(legacy.preferred.kex.contains(&kex::DH_G14_SHA1));
        assert!(legacy
            .preferred
            .key
            .contains(&Algorithm::Rsa { hash: None }));
        // Legacy methods are fallbacks, never first choice.
        assert_eq!(legacy.preferred.kex[0], Preferred::default().kex[0]);
        assert_ne!(legacy.preferred.key[0], Algorithm::Rsa { hash: None });
    }

    #[test]
    fn watch_tracks_one_blocked_write_episode() {
        let (down, _rx) = watch::channel(false);
        let watch = Watch::new(Arc::new(down));
        assert!(watch.blocked_since().is_none());
        watch.write_blocked();
        let first = watch.blocked_since().unwrap();
        watch.write_blocked();
        assert_eq!(
            watch.blocked_since().unwrap(),
            first,
            "a second Pending keeps the original start"
        );
        watch.write_progressed();
        assert!(watch.blocked_since().is_none());
    }
}
