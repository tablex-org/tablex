//! The crate's own tokio runtime. The host application uses another executor (smol), so tunnels never run
//! on, or require, the caller's runtime. One small multi-thread runtime is shared by every tunnel and is
//! started lazily on first use.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

use crate::error::{ConfigError, TunnelError};
use crate::spec::CancelToken;

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

pub(crate) fn handle() -> Result<Handle, TunnelError> {
    let runtime = RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("engine-net")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
    });
    match runtime {
        Ok(runtime) => Ok(runtime.handle().clone()),
        Err(reason) => {
            Err(ConfigError::Invalid(format!("cannot start the tunnel runtime: {reason}")).into())
        }
    }
}

/// Runs `future` on the tunnel runtime and blocks the calling thread until it finishes. Uses a std channel,
/// so it works from any thread (it never needs, and never panics inside, a tokio context).
pub(crate) fn block_on<F>(future: F) -> Result<F::Output, TunnelError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    handle()?.spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv().map_err(|_| TunnelError::Closed)
}

/// Aborts the task and cancels the token when dropped: dropping an `open_async` future abandons the open.
struct AbandonOnDrop {
    task: tokio::task::AbortHandle,
    cancel: CancelToken,
}

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

/// Runs `future` on the tunnel runtime; the returned future is plain `std::future` and can be awaited on any
/// executor. Dropping it **aborts** the work (and cancels `cancel`, which the [`Prompts`](crate::Prompts)
/// callback can observe), so an abandoned open does not keep a half-open SSH session around.
pub(crate) async fn run_abortable<F>(
    future: F,
    cancel: CancelToken,
) -> Result<F::Output, TunnelError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = handle()?.spawn(future);
    let _abandon = AbandonOnDrop {
        task: task.abort_handle(),
        cancel,
    };
    task.await.map_err(|_| TunnelError::Closed)
}

/// Number of live tasks on the tunnel runtime (for leak tests).
pub(crate) fn alive_tasks() -> usize {
    handle().map(|h| h.metrics().num_alive_tasks()).unwrap_or(0)
}
