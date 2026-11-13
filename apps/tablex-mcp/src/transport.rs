//! Running the server over a byte pipe (stdin / stdout in production, an in-memory duplex in the
//! tests), noticing that the client went away, and tearing the sessions down afterwards.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use rmcp::service::ServerInitializeError;
use rmcp::ServiceExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;

use engine_agent::ToolLayer;

use crate::server::{McpServer, MAX_CONCURRENT_CALLS};

/// The longest single message (one line of JSON) a client may send. rmcp reads a line into a
/// buffer that grows without bound; this caps it. Tool arguments are a statement of at most
/// 64 KiB, so this is far above anything legitimate.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("MCP handshake failed: {0}")]
    Initialize(Box<ServerInitializeError>),
    #[error("MCP service failed: {0}")]
    Service(#[from] tokio::task::JoinError),
    #[error("a client message was longer than {MAX_FRAME_BYTES} bytes; closing the connection")]
    FrameTooLarge,
    #[error(
        "shutdown did not finish in time: some database sessions or tunnels may not have been \
         closed cleanly"
    )]
    TeardownIncomplete,
}

/// Rolls back transactions nobody has called for the idle timeout, every `interval`, on the
/// blocking pool (a rollback waits for its server). The layer also does it at the start of every
/// call; this is for the model that went quiet. Ends when its task is aborted.
async fn sweep_idle_transactions(layer: Arc<ToolLayer>, interval: std::time::Duration) {
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        let layer = Arc::clone(&layer);
        let _ = tokio::task::spawn_blocking(move || layer.expire_idle_transactions()).await;
    }
}

/// A reader that cancels `gone` when the peer is gone (end of input or a read error), and ends
/// the connection with an error when a line grows past `max_line` bytes.
///
/// rmcp only learns of the closed input inside its loop and then waits for the running handlers
/// (up to 5 s) before it lets go, so a long statement would keep running after the client left.
/// Watching the pipe ourselves cancels the statements at once.
struct EofWatch<R> {
    inner: R,
    gone: CancellationToken,
    /// Bytes of the line being read so far.
    line: usize,
    max_line: usize,
    oversized: Arc<AtomicBool>,
}

impl<R> EofWatch<R> {
    fn new(inner: R, gone: CancellationToken, max_line: usize, oversized: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            gone,
            line: 0,
            max_line,
            oversized,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for EofWatch<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let wanted = buf.remaining() > 0;
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        let Poll::Ready(read) = &poll else {
            return poll;
        };
        if read.is_err() || (wanted && buf.filled().len() == before) {
            self.gone.cancel();
            return poll;
        }
        // The line counter: a line ends at `\n`; a chunk may hold the end of one line, whole
        // lines, and the start of the next.
        let mut segments = buf.filled()[before..].split(|byte| *byte == b'\n');
        let mut line = self.line + segments.next().map_or(0, <[u8]>::len);
        let mut longest = line;
        for segment in segments {
            line = segment.len();
            longest = longest.max(line);
        }
        self.line = line;
        if longest > self.max_line {
            self.oversized.store(true, Ordering::SeqCst);
            self.gone.cancel();
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP message too large",
            )));
        }
        poll
    }
}

impl McpServer {
    /// Serves MCP over `reader` / `writer` until the client closes the pipe or `stop` completes
    /// (a termination signal), then cancels what is still running and closes every session
    /// (disconnect and tunnel) of the tool layer. A client that leaves before the handshake is a
    /// clean end.
    ///
    /// The cleanup is bounded: it gives up when the server's shutdown deadline passes or when
    /// `stop` completes again (the next signal), and then returns
    /// [`ServeError::TeardownIncomplete`]. The signal that started the cleanup, or, when end of input
    /// started it, the first signal during it, lets it go on: a client that closes stdin and then
    /// sends SIGTERM does not cut a slow cancelled call short. A message longer than [`MAX_FRAME_BYTES`] ends the
    /// connection with [`ServeError::FrameTooLarge`] (after the same cleanup).
    pub async fn serve<R, W>(
        self,
        reader: R,
        writer: W,
        mut stop: impl AsyncFnMut(),
    ) -> Result<(), ServeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        self.serve_limited(reader, writer, &mut stop, MAX_FRAME_BYTES)
            .await
    }

    pub(crate) async fn serve_limited<R, W>(
        self,
        reader: R,
        writer: W,
        stop: &mut impl AsyncFnMut(),
        max_frame: usize,
    ) -> Result<(), ServeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let layer = Arc::clone(self.layer());
        let gone = self.gone().clone();
        let deadline = self.shutdown_deadline();
        let calls = self.calls();
        let oversized = Arc::new(AtomicBool::new(false));
        let mut signalled = false;
        let sweeper = tokio::spawn(sweep_idle_transactions(
            Arc::clone(&layer),
            self.sweep_interval(),
        ));
        let result = self
            .run(
                reader,
                writer,
                stop,
                max_frame,
                Arc::clone(&oversized),
                &mut signalled,
            )
            .await;
        gone.cancel();
        sweeper.abort();
        // `close_all` blocks (drivers disconnect, tunnels close, an open in flight finishes), and
        // a driver may hang: wait for it, but not past the deadline or a second signal. A hung
        // blocking thread cannot keep the process alive (see `main`).
        let closing = tokio::task::spawn_blocking(move || layer.close_all());
        let teardown = async {
            let closed = closing.await.is_ok();
            // A call that was cancelled is still on its way back: it has returned, and written
            // its audit line, once its permit is back. Without this wait the process could exit
            // between the cancel and that line.
            let returned = calls
                .acquire_many(MAX_CONCURRENT_CALLS as u32)
                .await
                .is_ok();
            closed && returned
        };
        let timeout = tokio::time::sleep(deadline);
        tokio::pin!(teardown, timeout);
        // End of input started the teardown: the first signal is the user's wish to stop, which
        // it already does, so it is let by; the next one ends the teardown.
        let mut spare_signals = usize::from(!signalled);
        let closed_cleanly = loop {
            tokio::select! {
                closed = &mut teardown => break closed,
                () = &mut timeout => break false,
                () = stop() => match spare_signals.checked_sub(1) {
                    Some(left) => spare_signals = left,
                    None => break false,
                },
            }
        };
        result?;
        if oversized.load(Ordering::SeqCst) {
            return Err(ServeError::FrameTooLarge);
        }
        if closed_cleanly {
            Ok(())
        } else {
            Err(ServeError::TeardownIncomplete)
        }
    }

    async fn run<R, W>(
        self,
        reader: R,
        writer: W,
        stop: &mut impl AsyncFnMut(),
        max_frame: usize,
        oversized: Arc<AtomicBool>,
        signalled: &mut bool,
    ) -> Result<(), ServeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let gone = self.gone().clone();
        let transport = (
            EofWatch::new(reader, gone.clone(), max_frame, oversized),
            writer,
        );
        let running = tokio::select! {
            started = ServiceExt::serve(self, transport) => match started {
                Ok(running) => running,
                Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
                Err(error) => return Err(ServeError::Initialize(Box::new(error))),
            },
            () = stop() => {
                *signalled = true;
                return Ok(());
            }
        };
        let shutdown = running.cancellation_token();
        tokio::select! {
            quit = running.waiting() => quit.map(drop).map_err(ServeError::from),
            () = stop() => {
                *signalled = true;
                gone.cancel();
                shutdown.cancel();
                Ok(())
            }
        }
    }
}

/// Termination requests: SIGTERM and SIGINT (Unix); Ctrl-C, Ctrl-Break, console close and system
/// shutdown (Windows). [`next`](Self::next) can be awaited again for a second request. A signal
/// that cannot be hooked is never delivered; the server then still ends on end of input.
///
/// `TerminateProcess` (Task Manager "End task", `kill -9`) cannot be handled on any platform.
pub struct Signals {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
    #[cfg(windows)]
    ctrl_break: Option<tokio::signal::windows::CtrlBreak>,
    #[cfg(windows)]
    ctrl_close: Option<tokio::signal::windows::CtrlClose>,
    #[cfg(windows)]
    ctrl_shutdown: Option<tokio::signal::windows::CtrlShutdown>,
}

impl Signals {
    /// Hooks the signals; call it inside the runtime, before serving.
    #[cfg(unix)]
    pub fn install() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        Self {
            term: signal(SignalKind::terminate()).ok(),
            int: signal(SignalKind::interrupt()).ok(),
        }
    }

    #[cfg(unix)]
    pub async fn next(&mut self) {
        tokio::select! {
            () = Self::unix_recv(&mut self.term) => {}
            () = Self::unix_recv(&mut self.int) => {}
        }
    }

    #[cfg(unix)]
    async fn unix_recv(signal: &mut Option<tokio::signal::unix::Signal>) {
        if let Some(signal) = signal {
            if signal.recv().await.is_some() {
                return;
            }
        }
        std::future::pending().await
    }

    #[cfg(windows)]
    pub fn install() -> Self {
        use tokio::signal::windows;
        Self {
            ctrl_c: windows::ctrl_c().ok(),
            ctrl_break: windows::ctrl_break().ok(),
            ctrl_close: windows::ctrl_close().ok(),
            ctrl_shutdown: windows::ctrl_shutdown().ok(),
        }
    }

    #[cfg(windows)]
    pub async fn next(&mut self) {
        // The console gives a process about 5 s after `ctrl_close` / `ctrl_shutdown` before it is
        // killed, and the shutdown deadline is 5 s as well: the console may end the process at the
        // deadline instead of the process exiting with its own message.
        macro_rules! recv {
            ($signal:expr) => {
                async {
                    if let Some(signal) = $signal {
                        if signal.recv().await.is_some() {
                            return;
                        }
                    }
                    std::future::pending::<()>().await
                }
            };
        }
        tokio::select! {
            () = recv!(&mut self.ctrl_c) => {}
            () = recv!(&mut self.ctrl_break) => {}
            () = recv!(&mut self.ctrl_close) => {}
            () = recv!(&mut self.ctrl_shutdown) => {}
        }
    }

    /// tokio has no signal API on other platforms.
    #[cfg(not(any(unix, windows)))]
    pub fn install() -> Self {
        Self {}
    }

    #[cfg(not(any(unix, windows)))]
    pub async fn next(&mut self) {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// A reader that yields the given chunks, then `end`.
    struct Script {
        chunks: std::collections::VecDeque<Vec<u8>>,
        error_at_end: bool,
    }

    impl AsyncRead for Script {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.chunks.pop_front() {
                Some(chunk) => {
                    buf.put_slice(&chunk);
                    Poll::Ready(Ok(()))
                }
                None if self.error_at_end => {
                    Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
                }
                None => Poll::Ready(Ok(())),
            }
        }
    }

    fn watch(
        chunks: &[&[u8]],
        error_at_end: bool,
        max_line: usize,
    ) -> (EofWatch<Script>, CancellationToken, Arc<AtomicBool>) {
        let gone = CancellationToken::new();
        let oversized = Arc::new(AtomicBool::new(false));
        let script = Script {
            chunks: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
            error_at_end,
        };
        (
            EofWatch::new(script, gone.clone(), max_line, Arc::clone(&oversized)),
            gone,
            oversized,
        )
    }

    #[tokio::test]
    async fn data_does_not_cancel_but_end_of_input_does() {
        let (mut reader, gone, _) = watch(&[b"{}\n"], false, 1024);
        let mut buf = [0u8; 16];
        assert_eq!(reader.read(&mut buf).await.unwrap(), 3);
        assert!(!gone.is_cancelled(), "data is not a disconnect");
        assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
        assert!(gone.is_cancelled(), "end of input is");
    }

    #[tokio::test]
    async fn a_read_error_cancels() {
        let (mut reader, gone, _) = watch(&[b"x"], true, 1024);
        let mut buf = [0u8; 16];
        assert_eq!(reader.read(&mut buf).await.unwrap(), 1);
        assert!(!gone.is_cancelled());
        assert!(reader.read(&mut buf).await.is_err());
        assert!(gone.is_cancelled());
    }

    #[tokio::test]
    async fn a_zero_capacity_read_is_not_an_end_of_input() {
        let (mut reader, gone, _) = watch(&[], false, 1024);
        assert_eq!(reader.read(&mut []).await.unwrap(), 0);
        assert!(!gone.is_cancelled());
    }

    #[tokio::test]
    async fn a_line_over_the_limit_fails_the_read_and_cancels() {
        // Two lines of 5 and 4 bytes, then 6 bytes without a newline, across chunk boundaries.
        let (mut reader, gone, oversized) = watch(&[b"12345\n12", b"34\nab", b"cdef"], false, 5);
        let mut buf = [0u8; 16];
        assert!(
            reader.read(&mut buf).await.is_ok(),
            "a line of exactly 5 is fine"
        );
        assert!(
            reader.read(&mut buf).await.is_ok(),
            "so is one split over chunks (4 then 2)"
        );
        assert!(!oversized.load(Ordering::SeqCst) && !gone.is_cancelled());
        assert!(reader.read(&mut buf).await.is_err(), "ab + cdef = 6 > 5");
        assert!(oversized.load(Ordering::SeqCst) && gone.is_cancelled());
    }

    #[tokio::test]
    async fn many_short_lines_in_one_chunk_are_never_oversized() {
        let (mut reader, gone, oversized) = watch(&[b"aaaa\nbbbb\ncccc\ndddd\n"], false, 5);
        let mut buf = [0u8; 64];
        assert!(reader.read(&mut buf).await.is_ok());
        assert!(!oversized.load(Ordering::SeqCst) && !gone.is_cancelled());
    }
}
