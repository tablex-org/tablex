//! The driver's single tokio runtime.
//!
//! One multi-thread runtime drives every connection's I/O task. It is created
//! on first use and torn down by `shutdown` (the host may then `dlclose` the
//! image, which must not leave worker threads running driver code); a later
//! `init`, or the next call, builds a fresh one, so shutdown -> init in the
//! same loaded image works.

use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tokio::runtime::{Builder, Handle, Runtime};

static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

/// Connection I/O is light and the calling threads do the waiting (`block_on`),
/// so two workers are enough.
const WORKER_THREADS: usize = 2;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn build() -> Result<Runtime, String> {
    Builder::new_multi_thread()
        .enable_all()
        .worker_threads(WORKER_THREADS)
        .thread_name("dbengine-postgres-worker")
        .build()
        .map_err(|error| format!("failed to create the PostgreSQL driver runtime: {error}"))
}

/// A handle to the runtime, creating it if needed.
pub(crate) fn handle() -> Result<Handle, String> {
    let mut slot = RUNTIME.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(build()?);
    }
    Ok(slot
        .as_ref()
        .expect("runtime was just created")
        .handle()
        .clone())
}

/// Runs `future` to completion on the calling thread, with this runtime
/// driving spawned tasks (connection I/O). The future need not be `Send`.
pub(crate) fn block_on<F: Future>(future: F) -> Result<F::Output, String> {
    Ok(handle()?.block_on(future))
}

/// Stops the runtime and waits (bounded) for its threads. Every connection is
/// already closed when the kit calls this.
pub(crate) fn stop() {
    let runtime = RUNTIME
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(runtime) = runtime {
        runtime.shutdown_timeout(SHUTDOWN_GRACE);
    }
}
