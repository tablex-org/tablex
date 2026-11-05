//! The tunnel itself: one SSH session (behind any jump hosts and proxy), one loopback listener per target, one
//! `direct-tcpip` channel per accepted connection. A proxy-only tunnel ([`Tunnel::open_proxy`]) has no SSH
//! session: each accepted connection gets its own connection through the proxy, with the same buffering rules.
//!
//! Isolation and backpressure between forwards: russh re-opens the SSH receive window as soon as data arrives
//! and feeds every channel through a bounded queue that, when full, makes the *whole session loop wait*. Each
//! forward therefore drains its channel into its own byte budget (`TunnelSpec::forward_buffer_bytes`) so a slow
//! local reader does not hold up the other forwards. When that budget is full the forward stops draining: the
//! channel queue fills, the session loop waits, and the remote side is slowed down, exactly as one TCP
//! connection with a slow reader would be. A forward whose local client reads **nothing** for
//! `Timeouts::forward_stall` while its buffer is full is closed (`tunnel.forward_stalled`), which frees
//! everything else; a reader that is merely slow is never closed.
//!
//! The isolation holds below the budget only. While one forward is at budget and the remote keeps sending
//! to it, the session loop waits and **every** forward of this tunnel is slowed to that reader's pace (for a
//! dead reader: until the stall timeout closes it). Callers that need independent flow control per database
//! session open one tunnel per session.

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::client::{Handle, Msg};
use russh::{
    Channel, ChannelMsg, ChannelOpenFailure, ChannelReadHalf, ChannelWriteHalf, Disconnect,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::error::TunnelError;
use crate::proxy::{ProxyRoute, ProxySpec};
use crate::runtime;
use crate::session::{self, Client, KillSwitch};
use crate::spec::{CancelToken, Prompts, Target, TunnelSpec};

/// Per-direction copy buffer. Larger than tokio's 8 KiB default; matches the SSH channel packet size.
const COPY_BUFFER: usize = 32 * 1024;
/// What every forward may buffer regardless of the tunnel-wide budget (see `Shared::try_reserve`). The memory
/// bound of a tunnel is therefore `tunnel_buffer_bytes` + `max_concurrent_forwards` × this, plus the packets
/// queued inside russh per channel.
const GUARANTEED_FORWARD_BYTES: usize = 256 * 1024;
/// How long `Drop` waits for the listeners to be closed.
const DROP_WAIT: Duration = Duration::from_secs(2);
/// How long a channel confirmed after its open timed out is still waited for (to close it).
const LATE_CHANNEL_GRACE: Duration = Duration::from_secs(120);
/// A socket write blocked for this many keepalive intervals closes the session.
const STALLED_WRITE_KEEPALIVES: u32 = 4;

/// Where the bytes of a forward go.
enum Route {
    /// `direct-tcpip` channels of the last session; `upstream` are the sessions of the jump hosts that carry it.
    Ssh {
        session: Handle<Client>,
        upstream: Vec<Handle<Client>>,
    },
    /// One connection through the proxy per forward, bounded by `connect`.
    Proxy { proxy: ProxySpec, connect: Duration },
}

/// What a tunnel does with its forwards: the limits of [`TunnelSpec`] / [`ProxyRoute`].
struct Limits {
    max_forwards: usize,
    forward_timeout: Duration,
    forward_buffer_bytes: usize,
    tunnel_buffer_bytes: usize,
    stall: Duration,
}

/// State shared by the accept loops, forwards and the public handle.
struct Shared {
    route: Route,
    down: Arc<watch::Sender<bool>>,
    failures: Mutex<HashMap<Target, TunnelError>>,
    slots: Arc<Semaphore>,
    max_forwards: usize,
    forward_timeout: Duration,
    // Remote-to-local buffering (see the module docs).
    forward_limit: usize,
    tunnel_limit: usize,
    stall: Duration,
    epoch: Instant,
    queued_bytes: AtomicUsize,
    peak_bytes: AtomicUsize,
    /// Forwards currently waiting for room; lets `release` skip the wake-up when nobody waits.
    waiting: AtomicUsize,
    room: Notify,
}

/// Buffer accounting of one forward.
struct ForwardBuf {
    queued: AtomicUsize,
    /// Milliseconds since `Shared::epoch` at which the local client last consumed data.
    last_progress_ms: AtomicU64,
}

impl ForwardBuf {
    fn new() -> Self {
        Self {
            queued: AtomicUsize::new(0),
            last_progress_ms: AtomicU64::new(0),
        }
    }
}

/// Decrements `Shared::waiting` however the wait ends.
struct WaitGuard<'a>(&'a Shared);

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    fn new(route: Route, down: Arc<watch::Sender<bool>>, limits: &Limits) -> Self {
        Self {
            route,
            down,
            failures: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(limits.max_forwards)),
            max_forwards: limits.max_forwards,
            forward_timeout: limits.forward_timeout,
            forward_limit: limits.forward_buffer_bytes,
            tunnel_limit: limits.tunnel_buffer_bytes,
            stall: limits.stall,
            epoch: Instant::now(),
            queued_bytes: AtomicUsize::new(0),
            peak_bytes: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            room: Notify::new(),
        }
    }

    /// Any SSH session of the route has ended (a proxy-only route has none).
    fn session_closed(&self) -> bool {
        match &self.route {
            Route::Ssh { session, upstream } => {
                session.is_closed() || upstream.iter().any(Handle::is_closed)
            }
            Route::Proxy { .. } => false,
        }
    }

    fn record(&self, target: &Target, outcome: Option<TunnelError>) {
        if let Ok(mut failures) = self.failures.lock() {
            match outcome {
                Some(error) => {
                    failures.insert(target.clone(), error);
                }
                None => {
                    failures.remove(target);
                }
            }
        }
    }

    fn try_reserve(&self, forward: &ForwardBuf, len: usize) -> bool {
        let own = forward.queued.load(Ordering::SeqCst);
        let total = self.queued_bytes.fetch_add(len, Ordering::SeqCst) + len;
        if !fits(own, total, len, self.forward_limit, self.tunnel_limit) {
            self.queued_bytes.fetch_sub(len, Ordering::SeqCst);
            return false;
        }
        self.peak_bytes.fetch_max(total, Ordering::Relaxed);
        forward.queued.fetch_add(len, Ordering::SeqCst);
        true
    }

    /// The local client consumed `len` bytes.
    fn release(&self, forward: &ForwardBuf, len: usize) {
        forward
            .last_progress_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        forward.queued.fetch_sub(len, Ordering::SeqCst);
        self.queued_bytes.fetch_sub(len, Ordering::SeqCst);
        if self.waiting.load(Ordering::SeqCst) > 0 {
            self.room.notify_waiters();
        }
    }

    /// Waits until `len` more bytes fit in the buffers. This is the backpressure point: while it waits the
    /// caller does not drain its SSH channel. Fails only when this forward holds buffered data that its local
    /// client has not touched for the stall timeout.
    async fn wait_for_room(
        &self,
        forward: &ForwardBuf,
        len: usize,
        target: &Target,
    ) -> Result<(), ()> {
        if self.try_reserve(forward, len) {
            return Ok(());
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _waiting = WaitGuard(self);
        let since = Instant::now();
        loop {
            // Created (and thereby registered for `notify_waiters`) before the retry, so no release is missed.
            let notified = self.room.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_reserve(forward, len) {
                return Ok(());
            }
            // Only a forward that holds unread data of its own can be "stalled"; one that waits for the
            // tunnel-wide budget while empty just waits for the others to make room.
            let holding_data = forward.queued.load(Ordering::SeqCst) > 0;
            let last_progress = self.epoch
                + Duration::from_millis(forward.last_progress_ms.load(Ordering::Relaxed));
            let deadline = since.max(last_progress) + self.stall;
            if holding_data && Instant::now() >= deadline {
                self.record(
                    target,
                    Some(TunnelError::ForwardStalled {
                        host: target.host.clone(),
                        port: target.port,
                        seconds: self.stall.as_secs(),
                    }),
                );
                return Err(());
            }
            let wait = if holding_data {
                deadline.saturating_duration_since(Instant::now())
            } else {
                self.stall
            };
            let _ = tokio::time::timeout(wait, notified).await;
        }
    }
}

/// An open SSH session with a loopback listener per target. Dropping it closes the listeners, the forwarded
/// connections and the session.
///
/// `Send + Sync`; every method may be called from any thread, with or without an async runtime.
///
/// Once [`Tunnel::is_alive`] is false the local ports are closed for good. A caller that reconnects must open a
/// **new** tunnel and use its new addresses: never retry a database connection against an address obtained
/// from a dead tunnel (another process may have taken that port by then).
pub struct Tunnel {
    via: String,
    listeners: Vec<(Target, SocketAddr)>,
    shared: Arc<Shared>,
    /// The TCP socket of the first SSH hop; `None` for a proxy-only tunnel.
    kill: Option<KillSwitch>,
    tasks: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tunnel")
            .field("via", &self.via)
            .field("listeners", &self.listeners)
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl Tunnel {
    /// Connects, authenticates and starts forwarding. **Blocks the calling thread** until the tunnel is up or
    /// has failed (bounded by the spec's timeouts, plus any keyboard-interactive prompt time). Call it from a
    /// background thread; it needs no async runtime and, because the work runs on this crate's own runtime, it
    /// also works if called directly from async code (it then blocks that thread, so prefer
    /// [`Tunnel::open_async`] there).
    pub fn open(spec: TunnelSpec, prompts: Arc<dyn Prompts>) -> Result<Tunnel, TunnelError> {
        spec.validate()?;
        runtime::block_on(establish(spec, prompts, CancelToken::default()))?
    }

    /// Same as [`Tunnel::open`] but returns a plain `std::future::Future` that can be awaited on any executor
    /// (smol, gpui). The work runs on this crate's runtime. **Dropping the future abandons the open**: the
    /// in-flight handshake is aborted, the socket closed, and the [`CancelToken`] handed to the prompt
    /// callback is cancelled.
    pub async fn open_async(
        spec: TunnelSpec,
        prompts: Arc<dyn Prompts>,
    ) -> Result<Tunnel, TunnelError> {
        spec.validate()?;
        let cancel = CancelToken::default();
        runtime::run_abortable(establish(spec, prompts, cancel.clone()), cancel).await?
    }

    /// Starts a tunnel that goes through a SOCKS5 / HTTP CONNECT proxy only, with no SSH: every connection to
    /// a local listener is passed through the proxy to its target. Nothing is contacted until a client
    /// connects (use [`Tunnel::probe`] to test). **Blocks the calling thread** only to bind the listeners.
    pub fn open_proxy(route: ProxyRoute) -> Result<Tunnel, TunnelError> {
        route.validate()?;
        runtime::block_on(establish_proxy(route))?
    }

    /// [`Tunnel::open_proxy`] as a plain `std::future::Future`.
    pub async fn open_proxy_async(route: ProxyRoute) -> Result<Tunnel, TunnelError> {
        route.validate()?;
        runtime::run_abortable(establish_proxy(route), CancelToken::default()).await?
    }

    /// Remote data currently buffered in front of local sockets, across all forwards. Test support.
    #[doc(hidden)]
    pub fn buffered_bytes(&self) -> usize {
        self.shared.queued_bytes.load(Ordering::SeqCst)
    }

    /// Highest value [`Tunnel::buffered_bytes`] ever reached. Test support.
    #[doc(hidden)]
    pub fn peak_buffered_bytes(&self) -> usize {
        self.shared.peak_bytes.load(Ordering::Relaxed)
    }

    /// `user@host:port` of the bastion (`SOCKS5 proxy host:port` / `HTTP proxy host:port` for a proxy-only
    /// tunnel), for status text.
    pub fn via(&self) -> &str {
        &self.via
    }

    /// The loopback address that forwards to `target`, or `None` if it was not part of the spec.
    pub fn local_addr(&self, target: &Target) -> Option<SocketAddr> {
        self.listeners
            .iter()
            .find(|(t, _)| t == target)
            .map(|(_, addr)| *addr)
    }

    /// Every `(target, local address)` pair, one per distinct target in spec order.
    pub fn local_addrs(&self) -> impl Iterator<Item = (&Target, SocketAddr)> {
        self.listeners.iter().map(|(target, addr)| (target, *addr))
    }

    /// False once the SSH session has ended (server closed it, keepalives went unanswered, the socket failed,
    /// or the tunnel is shutting down). The listeners are closed at that point.
    pub fn is_alive(&self) -> bool {
        !*self.shared.down.borrow() && !self.shared.session_closed()
    }

    /// The most recent failure to forward to `target` (for example its port is closed, or too many forwards
    /// are open), cleared by the next successful forward to it. Lets the caller turn a bare "connection reset"
    /// from the database driver into `tunnel.forward_refused`.
    pub fn last_forward_error(&self, target: &Target) -> Option<TunnelError> {
        self.shared
            .failures
            .lock()
            .ok()
            .and_then(|failures| failures.get(target).cloned())
    }

    /// Opens and immediately closes a forwarding channel to `target`: checks that the bastion can reach it.
    /// Blocks the calling thread (bounded by the forward timeout).
    pub fn probe(&self, target: &Target) -> Result<(), TunnelError> {
        if !self.is_alive() {
            return Err(TunnelError::Closed);
        }
        let (shared, target) = (self.shared.clone(), target.clone());
        runtime::block_on(async move {
            let remote =
                open_remote(&shared, &target, SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
            remote.close().await;
            Ok(())
        })?
    }
}

async fn establish(
    spec: TunnelSpec,
    prompts: Arc<dyn Prompts>,
    cancel: CancelToken,
) -> Result<Tunnel, TunnelError> {
    let down = Arc::new(watch::channel(false).0);
    let session::Established {
        handle,
        upstream,
        kill,
        watch,
    } = session::establish(&spec, &prompts, down.clone(), &cancel).await?;
    let limits = Limits {
        max_forwards: spec.max_concurrent_forwards,
        forward_timeout: spec.timeouts.forward,
        forward_buffer_bytes: spec.forward_buffer_bytes,
        tunnel_buffer_bytes: spec.tunnel_buffer_bytes,
        stall: spec.timeouts.forward_stall,
    };
    let shared = Arc::new(Shared::new(
        Route::Ssh {
            session: handle,
            upstream,
        },
        down,
        &limits,
    ));
    let mut tunnel = Tunnel {
        via: format!("{}@{}:{}", spec.user, spec.host, spec.port),
        listeners: Vec::new(),
        shared,
        kill: Some(kill.clone()),
        tasks: Vec::new(),
    };
    let stalled_write_limit = spec.timeouts.keepalive * STALLED_WRITE_KEEPALIVES;
    tunnel.tasks.push(tokio::spawn(session::watchdog(
        watch,
        kill.clone(),
        stalled_write_limit,
    )));
    // A stalled forward may legitimately hold the session loop for the stall timeout, so the liveness
    // deadline is never shorter than that plus a margin.
    let liveness_deadline = spec
        .timeouts
        .outbound_liveness
        .max(spec.timeouts.forward_stall + spec.timeouts.keepalive * 2);
    let (probed, declared_dead) = (tunnel.shared.clone(), tunnel.shared.clone());
    tunnel.tasks.push(tokio::spawn(outbound_liveness(
        move || {
            let shared = probed.clone();
            // Answered inside russh's session loop without a packet on the wire: it completes exactly when
            // the loop takes work from its queue, however slow the link or the readers are. Every hop's
            // session is asked.
            async move {
                let Route::Ssh { session, upstream } = &shared.route else {
                    return false;
                };
                for handle in upstream.iter().chain([session]) {
                    if handle.best_supported_rsa_hash().await.is_err() {
                        return false;
                    }
                }
                true
            }
        },
        spec.timeouts.keepalive,
        liveness_deadline,
        move || {
            declared_dead.down.send_replace(true);
            kill.kill();
        },
    )));
    bind_listeners(&mut tunnel, spec.targets).await?;
    Ok(tunnel)
}

async fn establish_proxy(route: ProxyRoute) -> Result<Tunnel, TunnelError> {
    let limits = Limits {
        max_forwards: route.max_concurrent_forwards,
        forward_timeout: route.timeouts.forward,
        forward_buffer_bytes: route.forward_buffer_bytes,
        tunnel_buffer_bytes: route.tunnel_buffer_bytes,
        stall: route.timeouts.forward_stall,
    };
    let via = route.proxy.describe();
    let shared = Arc::new(Shared::new(
        Route::Proxy {
            proxy: route.proxy,
            connect: route.timeouts.connect,
        },
        Arc::new(watch::channel(false).0),
        &limits,
    ));
    let mut tunnel = Tunnel {
        via,
        listeners: Vec::new(),
        shared,
        kill: None,
        tasks: Vec::new(),
    };
    bind_listeners(&mut tunnel, route.targets).await?;
    Ok(tunnel)
}

/// One loopback listener per distinct target. On failure the tunnel is dropped by the caller, which closes
/// whatever was bound.
async fn bind_listeners(tunnel: &mut Tunnel, targets: Vec<Target>) -> Result<(), TunnelError> {
    for target in targets {
        if tunnel.listeners.iter().any(|(t, _)| *t == target) {
            continue;
        }
        let bind_error = |e: std::io::Error| TunnelError::Connect {
            host: "127.0.0.1".into(),
            port: 0,
            reason: format!("cannot bind a local forwarding port: {e}"),
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(bind_error)?;
        let addr = listener.local_addr().map_err(bind_error)?;
        tunnel.listeners.push((target.clone(), addr));
        tunnel.tasks.push(tokio::spawn(accept_loop(
            listener,
            target,
            tunnel.shared.clone(),
        )));
    }
    Ok(())
}

/// Resolves when the session ended or the tunnel was dropped (or the sender is gone).
async fn shut_down(down: &mut watch::Receiver<bool>) {
    let _ = down.wait_for(|down| *down).await;
}

async fn accept_loop(listener: TcpListener, target: Target, shared: Arc<Shared>) {
    let mut down = shared.down.subscribe();
    loop {
        tokio::select! {
            _ = shut_down(&mut down) => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    if shared.session_closed() {
                        break;
                    }
                    match shared.slots.clone().try_acquire_owned() {
                        Ok(permit) => {
                            tokio::spawn(forward_connection(stream, peer, target.clone(), shared.clone(), permit));
                        }
                        Err(_) => shared.record(&target, Some(TunnelError::ForwardRefused {
                            host: target.host.clone(),
                            port: target.port,
                            reason: format!("too many concurrent forwards (limit {})", shared.max_forwards),
                        })),
                    }
                }
                // Out of file descriptors and the like: back off instead of spinning.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            },
        }
    }
    // `listener` drops here: the loopback port is closed.
}

async fn forward_connection(
    local: TcpStream,
    peer: SocketAddr,
    target: Target,
    shared: Arc<Shared>,
    _permit: OwnedSemaphorePermit,
) {
    let _ = local.set_nodelay(true);
    let remote = match open_remote(&shared, &target, peer).await {
        Ok(remote) => {
            shared.record(&target, None);
            remote
        }
        Err(error) => {
            shared.record(&target, Some(error));
            return; // dropping `local` resets the database client's connection
        }
    };
    pump(local, remote, &target, &shared).await;
}

/// Declares the session dead when it stops taking work from its handle.
///
/// russh 0.64 keeps a channel whose peer closed it while window-blocked upload data was still pending, and
/// while any channel has pending data its session loop takes nothing from the `Handle` queue: no uplink data
/// on any forward, no new channel, no close, although keepalives still flow and the session looks alive.
/// Nothing in russh times that out. `probe` is a request that goes through the same queue and is answered by
/// the session loop itself (no network round trip), so one that is not answered within `deadline` means the
/// loop takes no work; `on_dead` then closes the socket and the listeners, and the caller reopens the tunnel.
/// `probe` resolves to `false` when the session is already closed.
async fn outbound_liveness<P, F>(
    probe: P,
    every: Duration,
    deadline: Duration,
    on_dead: impl FnOnce(),
) where
    P: Fn() -> F,
    F: std::future::Future<Output = bool>,
{
    loop {
        tokio::time::sleep(every).await;
        match tokio::time::timeout(deadline, probe()).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(_) => {
                on_dead();
                return;
            }
        }
    }
}

/// Whether `len` more bytes may be buffered for a forward that already holds `own`, when the tunnel would then
/// hold `total`. An empty forward always takes its next chunk and every forward may hold
/// [`GUARANTEED_FORWARD_BYTES`] whatever the others hold. So a forward that has to wait for room always holds
/// data of its own: either its client reads (progress) or it is closed as stalled, and nothing waits for
/// ever on another forward's backlog.
fn fits(own: usize, total: usize, len: usize, forward_limit: usize, tunnel_limit: usize) -> bool {
    own == 0
        || own + len <= GUARANTEED_FORWARD_BYTES
        || (own + len <= forward_limit && total <= tunnel_limit)
}

/// Writes `data`, reporting every partial write: each byte the local client takes is progress, however slowly
/// it reads.
async fn write_reporting<W: AsyncWrite + Unpin>(
    writer: &mut W,
    mut data: &[u8],
    mut taken: impl FnMut(usize),
) -> std::io::Result<()> {
    while !data.is_empty() {
        let written = writer.write(data).await?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        taken(written);
        data = &data[written..];
    }
    Ok(())
}

/// One chunk of remote data waiting for the local client. Boxed so SSH payloads and proxy reads share one
/// queue type; the box is one small allocation per chunk of up to 32 KiB.
type Chunk = Box<dyn AsRef<[u8]> + Send>;

/// The remote end of a forward: a `direct-tcpip` channel or a connection through the proxy.
struct Remote {
    rx: RemoteRx,
    tx: Pin<Box<dyn AsyncWrite + Send>>,
    /// SSH only: sends the channel close.
    channel: Option<ChannelWriteHalf<Msg>>,
}

enum RemoteRx {
    Channel(ChannelReadHalf),
    /// Plus the per-forward read buffer, reused by every read.
    Tcp(OwnedReadHalf, Vec<u8>),
}

enum Next {
    Data(Chunk),
    /// A channel message that carries no data for the client.
    Other,
    End,
}

impl RemoteRx {
    async fn next(&mut self) -> Next {
        match self {
            Self::Channel(rx) => match rx.wait().await {
                Some(ChannelMsg::Data { data }) => Next::Data(Box::new(data)),
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => Next::End,
                Some(_) => Next::Other,
            },
            Self::Tcp(rx, scratch) => match rx.read(scratch).await {
                Ok(0) | Err(_) => Next::End,
                // An exact-size copy: the buffer budget counts what is really held.
                Ok(read) => Next::Data(Box::new(scratch[..read].to_vec())),
            },
        }
    }
}

impl Remote {
    fn from_channel(channel: Channel<Msg>) -> Self {
        let (rx, tx) = channel.split();
        Self {
            rx: RemoteRx::Channel(rx),
            tx: Box::pin(tx.make_writer()),
            channel: Some(tx),
        }
    }

    fn from_tcp(stream: TcpStream) -> Self {
        let (rx, tx) = stream.into_split();
        Self {
            rx: RemoteRx::Tcp(rx, vec![0; COPY_BUFFER]),
            tx: Box::pin(tx),
            channel: None,
        }
    }

    /// Ends the forward towards the remote (a channel close; a proxy connection just closes on drop).
    async fn close(self) {
        if let Some(channel) = self.channel {
            let _ = channel.close().await;
        }
    }
}

/// Moves bytes both ways until either side finishes. See the module docs for the buffering rules.
async fn pump(local: TcpStream, remote: Remote, target: &Target, shared: &Arc<Shared>) {
    let mut down = shared.down.subscribe();
    let Remote {
        rx: mut remote_rx,
        tx: mut remote_tx,
        channel,
    } = remote;
    let (local_rx, mut local_tx) = local.into_split();

    let uplink = tokio::spawn(async move {
        let mut reader = BufReader::with_capacity(COPY_BUFFER, local_rx);
        let _ = tokio::io::copy_buf(&mut reader, &mut remote_tx).await;
        let _ = remote_tx.shutdown().await; // EOF to the remote; the other direction may continue
    });

    let buf = Arc::new(ForwardBuf::new());
    let (queue_tx, mut queue_rx) = mpsc::unbounded_channel::<Chunk>();
    let drain = {
        let (buf, shared, target) = (buf.clone(), shared.clone(), target.clone());
        async move {
            loop {
                match remote_rx.next().await {
                    Next::Data(data) => {
                        // Backpressure: while the buffers are full this does not return, so the remote is
                        // not drained and is slowed down.
                        shared
                            .wait_for_room(&buf, (*data).as_ref().len(), &target)
                            .await?;
                        let _ = queue_tx.send(data);
                    }
                    Next::End => return Ok(()),
                    Next::Other => {}
                }
            }
        }
    };
    let downlink = {
        let (buf, shared) = (buf.clone(), shared.clone());
        async move {
            while let Some(data) = queue_rx.recv().await {
                let sent = write_reporting(&mut local_tx, (*data).as_ref(), |taken| {
                    shared.release(&buf, taken)
                });
                if sent.await.is_err() {
                    return Err(());
                }
            }
            let _ = local_tx.shutdown().await;
            Ok(())
        }
    };

    tokio::select! {
        _ = shut_down(&mut down) => {}
        _ = async { tokio::try_join!(drain, downlink) } => {}
    }
    uplink.abort();
    // First give the bytes back: whatever was still queued no longer counts and others may now fit. Only
    // then tell the remote. `close` goes through russh's small command queue, which does not move while the
    // session loop waits on a full channel, so it must never sit between a finished forward and its release.
    shared
        .queued_bytes
        .fetch_sub(buf.queued.swap(0, Ordering::SeqCst), Ordering::SeqCst);
    if shared.waiting.load(Ordering::SeqCst) > 0 {
        shared.room.notify_waiters();
    }
    if let Some(channel) = channel {
        let close_within = shared.forward_timeout;
        tokio::spawn(async move {
            let _ = tokio::time::timeout(close_within, channel.close()).await;
        });
    }
}

/// A new connection to `target`: a `direct-tcpip` channel, or a connection negotiated through the proxy.
async fn open_remote(
    shared: &Arc<Shared>,
    target: &Target,
    originator: SocketAddr,
) -> Result<Remote, TunnelError> {
    match &shared.route {
        Route::Ssh { .. } => Ok(Remote::from_channel(
            open_channel(shared, target, originator).await?,
        )),
        Route::Proxy { proxy, connect } => {
            // A negotiation in flight ends with the tunnel, like a running forward.
            let mut down = shared.down.subscribe();
            tokio::select! {
                _ = shut_down(&mut down) => Err(TunnelError::Closed),
                answered = tokio::time::timeout(*connect, proxy.connect(&target.host, target.port)) => {
                    match answered {
                        Ok(stream) => Ok(Remote::from_tcp(stream?)),
                        Err(_) => Err(TunnelError::Timeout {
                            operation: "proxy connect",
                            seconds: connect.as_secs(),
                        }),
                    }
                }
            }
        }
    }
}

async fn open_channel(
    shared: &Arc<Shared>,
    target: &Target,
    originator: SocketAddr,
) -> Result<Channel<Msg>, TunnelError> {
    if !matches!(shared.route, Route::Ssh { .. }) {
        return Err(TunnelError::Closed);
    }
    let limit = shared.forward_timeout;
    let opening_shared = shared.clone();
    let (host, port) = (target.host.clone(), target.port);
    let mut opening = tokio::spawn(async move {
        let Route::Ssh { session, .. } = &opening_shared.route else {
            return Err(russh::Error::Disconnect);
        };
        session
            .channel_open_direct_tcpip(
                host,
                u32::from(port),
                originator.ip().to_string(),
                u32::from(originator.port()),
            )
            .await
    });
    match tokio::time::timeout(limit, &mut opening).await {
        Ok(Ok(Ok(channel))) => Ok(channel),
        Ok(Ok(Err(russh::Error::ChannelOpenFailure(failure)))) => {
            Err(TunnelError::ForwardRefused {
                host: target.host.clone(),
                port: target.port,
                reason: describe(&failure),
            })
        }
        Ok(_) => Err(TunnelError::Closed),
        Err(_) => {
            // The server may still confirm after we gave up. Close that channel when it arrives instead of
            // leaving a forward nobody uses open on the bastion.
            let abort = opening.abort_handle();
            tokio::spawn(async move {
                match tokio::time::timeout(LATE_CHANNEL_GRACE, opening).await {
                    Ok(Ok(Ok(channel))) => {
                        let _ = channel.close().await;
                    }
                    Ok(_) => {}
                    Err(_) => abort.abort(),
                }
            });
            Err(TunnelError::Timeout {
                operation: "SSH port forward",
                seconds: limit.as_secs(),
            })
        }
    }
}

pub(crate) fn describe(failure: &ChannelOpenFailure) -> String {
    match failure {
        ChannelOpenFailure::AdministrativelyProhibited => {
            "port forwarding is not permitted by the SSH server (AllowTcpForwarding / PermitOpen)"
                .into()
        }
        ChannelOpenFailure::ConnectFailed => {
            "the SSH server could not connect to the target (refused or unreachable)".into()
        }
        ChannelOpenFailure::ResourceShortage => "the SSH server is out of resources".into(),
        ChannelOpenFailure::UnknownChannelType => {
            "the SSH server does not support port forwarding".into()
        }
        ChannelOpenFailure::Other { code, reason } => {
            format!("the SSH server refused the channel (code {code}: {reason})")
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shared.down.send_replace(true);
        let tasks = std::mem::take(&mut self.tasks);
        for task in &tasks {
            task.abort();
        }
        let kill = self.kill.clone();
        let Ok(rt) = runtime::handle() else {
            if let Some(kill) = kill {
                kill.kill();
            }
            return;
        };
        let shared = self.shared.clone();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            for task in tasks {
                let _ = task.await; // the listener is closed once its task is gone
            }
            let _ = closed_tx.send(());
            // The goodbye goes through every hop's channel, so the last session's one is enough: killing the
            // first hop's socket ends the rest.
            if let Route::Ssh { session, .. } = &shared.route {
                let goodbye = session.disconnect(Disconnect::ByApplication, "", "en");
                let _ = tokio::time::timeout(Duration::from_secs(1), goodbye).await;
                tokio::time::sleep(Duration::from_millis(50)).await; // let the goodbye reach the wire
            }
            if let Some(kill) = kill {
                kill.kill();
            }
        });
        // Do not park a runtime worker if we are being dropped from inside the tunnel runtime itself.
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = closed_rx.recv_timeout(DROP_WAIT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Tunnel>();
    }

    #[test]
    fn a_forward_that_waits_for_room_always_holds_data_of_its_own() {
        const FORWARD: usize = 32 << 20;
        const TUNNEL: usize = 64 << 20;
        // Empty: the next chunk is always taken, even with the tunnel budget exhausted by others.
        assert!(fits(0, TUNNEL + 1, 32 << 10, FORWARD, TUNNEL));
        assert!(fits(0, TUNNEL * 4, FORWARD * 2, FORWARD, TUNNEL));
        // Within the guaranteed quota: still independent of the others.
        assert!(fits(
            GUARANTEED_FORWARD_BYTES - 10,
            TUNNEL * 4,
            10,
            FORWARD,
            TUNNEL
        ));
        // Beyond it both budgets apply.
        assert!(!fits(
            GUARANTEED_FORWARD_BYTES,
            TUNNEL + 1,
            1,
            FORWARD,
            TUNNEL
        ));
        assert!(fits(GUARANTEED_FORWARD_BYTES, TUNNEL, 1, FORWARD, TUNNEL));
        assert!(!fits(FORWARD, FORWARD + 1, 1, FORWARD, TUNNEL));
        assert!(fits(FORWARD - 1, FORWARD, 1, FORWARD, TUNNEL));
    }

    /// A writer that takes at most `step` bytes per call, like a socket whose reader is slow.
    struct Trickle {
        step: usize,
        taken: Vec<u8>,
    }

    impl AsyncWrite for Trickle {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            data: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let count = data.len().min(self.step);
            self.taken.extend_from_slice(&data[..count]);
            std::task::Poll::Ready(Ok(count))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn every_partial_write_is_reported_as_progress() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let data: Vec<u8> = (0..=255).collect();
        let mut reports = Vec::new();
        let mut slow = Trickle {
            step: 100,
            taken: Vec::new(),
        };
        runtime
            .block_on(write_reporting(&mut slow, &data, |taken| {
                reports.push(taken)
            }))
            .unwrap();
        assert_eq!(
            reports,
            [100, 100, 56],
            "one report per partial write, not one per chunk"
        );
        assert_eq!(slow.taken, data);

        let mut closed = Trickle {
            step: 0,
            taken: Vec::new(),
        };
        let error = runtime
            .block_on(write_reporting(&mut closed, &data, |_| {
                panic!("nothing was taken")
            }))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
    }

    fn liveness_outcome<F>(probe: impl Fn() -> F, wait: Duration) -> bool
    where
        F: std::future::Future<Output = bool>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let dead = std::sync::atomic::AtomicBool::new(false);
        runtime.block_on(async {
            let watching = outbound_liveness(
                probe,
                Duration::from_millis(20),
                Duration::from_millis(500),
                || dead.store(true, Ordering::SeqCst),
            );
            let _ = tokio::time::timeout(wait, watching).await;
        });
        dead.load(Ordering::SeqCst)
    }

    #[test]
    fn a_session_that_stops_taking_work_is_declared_dead_and_a_working_one_is_not() {
        let short = Duration::from_millis(300);
        // Answers every probe: never declared dead, however long it runs.
        assert!(!liveness_outcome(|| async { true }, short));
        // Slow answers well within the 500 ms deadline are fine too.
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            true
        };
        assert!(!liveness_outcome(slow, short));
        // A probe that never comes back: dead after one interval plus the deadline.
        assert!(liveness_outcome(
            std::future::pending::<bool>,
            Duration::from_secs(3)
        ));
        // A session that is already closed is someone else's business (no second kill).
        assert!(!liveness_outcome(|| async { false }, short));
    }

    #[test]
    fn forward_failures_have_readable_reasons() {
        assert!(describe(&ChannelOpenFailure::ConnectFailed).contains("refused or unreachable"));
        assert!(describe(&ChannelOpenFailure::AdministrativelyProhibited)
            .contains("AllowTcpForwarding"));
    }
}
