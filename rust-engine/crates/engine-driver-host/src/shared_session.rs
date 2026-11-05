use std::sync::{LockResult, Mutex, MutexGuard, TryLockResult};

use engine_driver_api::{MutationRequest, MutationResult, RowFields};

use crate::loader::{DriverOperation, DriverSession, LoaderError, QueryRunner, SessionAnchor};

/// Shares one driver session between UI inspection and serialized background
/// operations while preserving page-release ordering.
///
/// The session lock is held for the whole duration of navigator calls and
/// page acquisition (network I/O), so anything that runs on the UI thread must
/// use the lock-free [`Self::begin_operation`] / [`Self::query_runner`] /
/// [`Self::mutate`] / [`Self::describe_object`] instead of [`Self::lock`].
pub struct SharedDriverSession {
    session: Mutex<DriverSession>,
    anchor: SessionAnchor,
    pending_releases: Mutex<Vec<u64>>,
    /// Pages a lock-free mutation invalidated, forgotten by the next `lock`.
    pending_invalidations: Mutex<Vec<u64>>,
}

impl SharedDriverSession {
    pub fn new(session: DriverSession) -> Self {
        Self {
            anchor: session.anchor(),
            session: Mutex::new(session),
            pending_releases: Mutex::new(Vec::new()),
            pending_invalidations: Mutex::new(Vec::new()),
        }
    }

    /// Registers a host-allocated driver operation **without** taking the
    /// session lock (never blocks). The operation pins the driver generation
    /// until dropped and its id is the one to put in a request / cancel.
    pub fn begin_operation(&self) -> Result<DriverOperation, LoaderError> {
        self.anchor.begin_operation()
    }

    /// A [`QueryRunner`] **without** taking the session lock (never blocks).
    /// The runner registers a driver operation, so the driver cannot be
    /// retired or unloaded while any clone is alive; fails once the session
    /// was disconnected.
    pub fn query_runner(&self) -> Result<QueryRunner, LoaderError> {
        self.anchor.query_runner()
    }

    /// `dbengine_inspect_row` **without** the session lock (never waits for
    /// a busy session). Reads a page the driver already holds, so it is an
    /// in-memory call; a page released concurrently yields a driver error.
    pub fn inspect_row(&self, page_id: u64, row_index: u64) -> Result<RowFields, LoaderError> {
        self.anchor.inspect_row(page_id, row_index)
    }

    /// `dbengine_mutate` **without** the session lock, so a slow batch never
    /// queues navigator calls, describes or page adoption behind it. Runs
    /// under `operation` (from [`Self::begin_operation`] or a runner's
    /// [`QueryRunner::operation`]): its id replaces the request's, cancels the
    /// mutation, and the operation pins the driver generation for the call.
    ///
    /// A mutation creates no pages; it can only invalidate some, and that
    /// bookkeeping is applied under the lock only if it is free right now,
    /// else queued for the next [`Self::lock`] (never waits for it).
    pub fn mutate(
        &self,
        operation: &DriverOperation,
        request: MutationRequest,
    ) -> Result<MutationResult, LoaderError> {
        let result = self.anchor.mutate(operation, request)?;
        if !result.invalidated_page_ids.is_empty() {
            match self.session.try_lock() {
                Ok(mut session) => session.forget_pages(&result.invalidated_page_ids),
                Err(_) => self
                    .pending_invalidations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .extend_from_slice(&result.invalidated_page_ids),
            }
        }
        Ok(result)
    }

    /// `dbengine_describe_object` **without** the session lock, under its own
    /// registered operation (never waits for a busy session).
    pub fn describe_object(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, LoaderError> {
        self.anchor.describe_object(request)
    }

    /// Locks the session and flushes queued page releases before returning it.
    pub fn lock(&self) -> LockResult<MutexGuard<'_, DriverSession>> {
        let mut session = self.session.lock()?;
        let pending = {
            let mut pending_guard = self
                .pending_releases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *pending_guard)
        };

        for page_id in pending {
            let _ = session.release_page(page_id);
        }

        let invalidated = std::mem::take(
            &mut *self
                .pending_invalidations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        session.forget_pages(&invalidated);

        Ok(session)
    }

    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, DriverSession>> {
        self.session.try_lock()
    }

    pub fn queue_page_release(&self, page_id: u64) {
        let mut pending = self
            .pending_releases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.push(page_id);
    }
}
