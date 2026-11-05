//! Per-connection operation bookkeeping behind `operation.cancel`.
//!
//! An operation is *pending* from registration (before the backend waits for
//! its connection) until [`OperationGuard::activate`], then *active* until the
//! returned [`ActiveOperation`] drops. Cancelling a pending operation makes its
//! `activate` fail; cancelling the active one asks the backend's canceller to
//! stop the running statement (see `DriverBackend::cancel`) and is recorded so
//! the backend can tell a requested interrupt from a spontaneous one. A cancel
//! for an operation the connection has not seen yet is remembered (bounded) so
//! the operation starts cancelled when it registers. Once the connection is
//! closed (disconnect/shutdown) nothing can register or activate any more.
//!
//! Host ids are allocated before the request crosses to the driver, so two
//! requests on one connection can register in either order: an unknown id
//! older than one already registered may still be on its way. What tells a
//! late cancel (the operation is over) from an early one is the record of the
//! operations that finished, not the order of the ids.

use crate::backend::BackendError;
use engine_driver_api::OperationId;
use std::collections::{HashSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Early cancels remembered per connection; the oldest is evicted first.
pub(crate) const MAX_EARLY_CANCELS: usize = 64;
/// Finished operations remembered per connection, so a cancel that arrives
/// after its operation ended is not kept as an early one.
pub(crate) const MAX_FINISHED: usize = 256;

const CONNECTION_CLOSED: &str = "connection closed";
const OPERATION_CANCELLED: &str = "operation cancelled";

/// Why registering or activating an operation failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OperationError {
    /// The connection is being (or has been) disconnected.
    Closed,
    /// The host cancelled the operation before it started (while pending, or
    /// before it registered).
    Cancelled,
    Rejected(&'static str),
}

impl OperationError {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Closed => CONNECTION_CLOSED,
            Self::Cancelled => OPERATION_CANCELLED,
            Self::Rejected(message) => message,
        }
    }
}

#[derive(Default)]
pub(crate) struct OperationState {
    pub(crate) active: Option<OperationId>,
    pub(crate) pending: HashSet<OperationId>,
    pub(crate) cancelled: HashSet<OperationId>,
    pub(crate) early: VecDeque<OperationId>,
    /// Active operations a cancel was requested for.
    pub(crate) cancel_requested: HashSet<OperationId>,
    /// The operations that ended most recently, oldest first.
    pub(crate) finished: VecDeque<OperationId>,
    pub(crate) closed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CancelAction {
    Active,
    Pending,
    /// Unknown operation (remembered as an early cancel unless it finished).
    Ignored,
}

impl OperationState {
    pub(crate) fn register(&mut self, operation_id: OperationId) -> Result<(), OperationError> {
        if self.closed {
            return Err(OperationError::Closed);
        }
        if self.cancelled.remove(&operation_id) {
            return Err(OperationError::Cancelled);
        }
        if let Some(position) = self.early.iter().position(|id| *id == operation_id) {
            self.early.remove(position);
            return Err(OperationError::Cancelled);
        }
        if self.active == Some(operation_id) || !self.pending.insert(operation_id) {
            return Err(OperationError::Rejected("operation is already registered"));
        }
        Ok(())
    }

    pub(crate) fn activate(&mut self, operation_id: OperationId) -> Result<(), OperationError> {
        if self.closed {
            self.pending.remove(&operation_id);
            return Err(OperationError::Closed);
        }
        if self.cancelled.remove(&operation_id) {
            self.pending.remove(&operation_id);
            return Err(OperationError::Cancelled);
        }
        if self.active.is_some() {
            return Err(OperationError::Rejected(
                "connection already has an active operation",
            ));
        }
        if !self.pending.remove(&operation_id) {
            return Err(OperationError::Rejected("operation is not pending"));
        }
        self.active = Some(operation_id);
        Ok(())
    }

    /// The statement is done; the operation stays registered until `finish`.
    pub(crate) fn deactivate(&mut self, operation_id: OperationId) {
        if self.active == Some(operation_id) {
            self.active = None;
        }
    }

    pub(crate) fn finish(&mut self, operation_id: OperationId) {
        self.deactivate(operation_id);
        self.pending.remove(&operation_id);
        self.cancelled.remove(&operation_id);
        self.cancel_requested.remove(&operation_id);
        if self.finished.len() == MAX_FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(operation_id);
    }

    pub(crate) fn cancel(&mut self, operation_id: OperationId) -> CancelAction {
        if self.active == Some(operation_id) {
            self.cancel_requested.insert(operation_id);
            CancelAction::Active
        } else if self.pending.contains(&operation_id) {
            self.cancelled.insert(operation_id);
            CancelAction::Pending
        } else {
            // An operation that already ended never registers again; any other
            // unknown id may still be on its way, whatever its order.
            if !self.finished.contains(&operation_id) && !self.early.contains(&operation_id) {
                if self.early.len() == MAX_EARLY_CANCELS {
                    self.early.pop_front();
                }
                self.early.push_back(operation_id);
            }
            CancelAction::Ignored
        }
    }

    /// Closes the connection to new work (disconnect): nothing can register or
    /// activate afterwards, pending operations are cancelled, and the active
    /// one, if any, is returned (and marked cancel-requested) so the caller can
    /// interrupt it.
    pub(crate) fn cancel_all(&mut self) -> Option<OperationId> {
        self.closed = true;
        self.cancelled.extend(self.pending.iter().copied());
        if let Some(active) = self.active {
            self.cancel_requested.insert(active);
        }
        self.active
    }
}

/// The state is plain collections that are never left half-updated by a panic,
/// so a poisoned lock is safe to keep using.
pub(crate) fn lock_state(state: &Mutex<OperationState>) -> MutexGuard<'_, OperationState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Handle to one registered operation, passed to backend methods that can be
/// cancelled. Dropping it (including during a panic unwind) ends the
/// operation.
pub struct OperationGuard<'a> {
    state: &'a Mutex<OperationState>,
    operation_id: OperationId,
    closed_code: &'static str,
    cancelled_code: &'static str,
}

impl<'a> OperationGuard<'a> {
    pub(crate) fn register(
        state: &'a Mutex<OperationState>,
        operation_id: OperationId,
        closed_code: &'static str,
        cancelled_code: &'static str,
    ) -> Result<Self, OperationError> {
        lock_state(state).register(operation_id)?;
        Ok(Self {
            state,
            operation_id,
            closed_code,
            cancelled_code,
        })
    }

    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Marks the operation as running on the connection and takes ownership of
    /// the backend's connection lock guard `lock_guard` (a `std` or `tokio`
    /// `MutexGuard`/`OwnedMutexGuard`, a `RefCell` borrow, ...; no `Send` or
    /// `'static` bound).
    ///
    /// The returned [`ActiveOperation`] owns the guard and derefs to it. Its
    /// `Drop` ends the active phase *before* the guard is released, so the next
    /// operation can never be refused as "already active" and a late cancel can
    /// never hit a statement that is not this operation's: the type system
    /// enforces the drop order.
    ///
    /// Fails (dropping `lock_guard`) if the operation was cancelled while
    /// pending, if another operation is active on the connection, or if the
    /// connection is closed (`<op>.not_connected`, "connection closed").
    /// A cancel fails with the area's `<op>.cancelled`.
    pub fn activate<G>(&mut self, lock_guard: G) -> Result<ActiveOperation<'_, G>, BackendError> {
        lock_state(self.state)
            .activate(self.operation_id)
            .map_err(|error| match error {
                OperationError::Closed => {
                    BackendError::with_code(self.closed_code, error.message())
                }
                OperationError::Cancelled => {
                    BackendError::with_code(self.cancelled_code, error.message())
                }
                OperationError::Rejected(message) => BackendError::new(message),
            })?;
        Ok(ActiveOperation {
            state: self.state,
            operation_id: self.operation_id,
            lock_guard,
        })
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        lock_state(self.state).finish(self.operation_id);
    }
}

/// The connection's active operation together with the backend's connection
/// lock guard (see [`OperationGuard::activate`]). Derefs to the guard, which
/// derefs to the connection.
#[must_use = "dropping it immediately ends the active phase and releases the connection lock"]
pub struct ActiveOperation<'a, G> {
    state: &'a Mutex<OperationState>,
    operation_id: OperationId,
    // Dropped after `Drop::drop` has deactivated the operation.
    lock_guard: G,
}

impl<G> ActiveOperation<'_, G> {
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Whether the host asked to cancel this operation (or the connection is
    /// being disconnected). Use it to classify an interrupt error from the
    /// server: if it is `false` the user did not request the interruption (a
    /// server-side kill, a timeout), so report a retryable `query.interrupted`
    /// instead of a plain failure. Typical errors: Postgres SQLSTATE `57014`
    /// (query_canceled), MySQL error 1317 (ER_QUERY_INTERRUPTED), SQLite
    /// `SQLITE_INTERRUPT`.
    pub fn cancel_requested(&self) -> bool {
        lock_state(self.state)
            .cancel_requested
            .contains(&self.operation_id)
    }
}

impl<G> Deref for ActiveOperation<'_, G> {
    type Target = G;

    fn deref(&self) -> &G {
        &self.lock_guard
    }
}

impl<G> DerefMut for ActiveOperation<'_, G> {
    fn deref_mut(&mut self) -> &mut G {
        &mut self.lock_guard
    }
}

impl<G> Drop for ActiveOperation<'_, G> {
    fn drop(&mut self) {
        lock_state(self.state).deactivate(self.operation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_cancel_does_not_interrupt_active_operation() {
        let mut state = OperationState::default();
        state.register(OperationId(1)).expect("register active");
        state.activate(OperationId(1)).expect("activate operation");
        state.register(OperationId(2)).expect("register pending");

        assert_eq!(state.cancel(OperationId(2)), CancelAction::Pending);
        assert_eq!(state.active, Some(OperationId(1)));
        assert!(state.cancelled.contains(&OperationId(2)));
        assert_eq!(state.cancel(OperationId(1)), CancelAction::Active);
    }

    #[test]
    fn cancelled_pending_operation_cannot_activate() {
        let mut state = OperationState::default();
        state.register(OperationId(5)).unwrap();
        assert_eq!(state.cancel(OperationId(5)), CancelAction::Pending);
        assert_eq!(
            state.activate(OperationId(5)),
            Err(OperationError::Cancelled)
        );
        assert_eq!(state.cancel(OperationId(5)), CancelAction::Ignored);
    }

    #[test]
    fn an_early_cancel_holds_when_a_newer_operation_registered_first() {
        let mut state = OperationState::default();
        // Ids 7 and 8 were allocated in order, 8 reached the driver first.
        state.register(OperationId(8)).unwrap();
        assert_eq!(state.cancel(OperationId(7)), CancelAction::Ignored);
        assert_eq!(
            state.register(OperationId(7)),
            Err(OperationError::Cancelled),
            "the cancel that came first stops the operation when it arrives"
        );
    }

    #[test]
    fn duplicate_and_unknown_operations_are_rejected() {
        let mut state = OperationState::default();
        state.register(OperationId(1)).unwrap();
        assert_eq!(
            state.register(OperationId(1)),
            Err(OperationError::Rejected("operation is already registered"))
        );
        assert_eq!(
            state.activate(OperationId(9)),
            Err(OperationError::Rejected("operation is not pending"))
        );
        assert_eq!(state.cancel(OperationId(9)), CancelAction::Ignored);
    }

    #[test]
    fn guard_drop_finishes_the_operation() {
        let state = Mutex::new(OperationState::default());
        {
            let mut guard = OperationGuard::register(&state, OperationId(3), "c", "x").unwrap();
            let active = guard.activate(()).unwrap();
            assert_eq!(lock_state(&state).active, Some(OperationId(3)));
            drop(active);
            assert!(lock_state(&state).active.is_none(), "active phase is over");
        }
        let state = lock_state(&state);
        assert!(state.active.is_none() && state.pending.is_empty());
    }

    #[test]
    fn dropping_the_active_handle_lets_the_next_operation_activate() {
        let state = Mutex::new(OperationState::default());
        let mut first = OperationGuard::register(&state, OperationId(1), "c", "x").unwrap();
        let mut second = OperationGuard::register(&state, OperationId(2), "c", "x").unwrap();
        let active = first.activate(()).unwrap();
        assert!(second.activate(()).is_err());
        drop(active);
        // `first` has not been dropped yet: the connection is already free.
        assert!(second.activate(()).is_ok());
    }

    #[test]
    fn early_cancel_is_remembered_once_and_bounded() {
        let mut state = OperationState::default();
        assert_eq!(state.cancel(OperationId(7)), CancelAction::Ignored);
        assert_eq!(state.cancel(OperationId(7)), CancelAction::Ignored);
        assert_eq!(state.early.len(), 1);
        assert_eq!(
            state.register(OperationId(7)),
            Err(OperationError::Cancelled)
        );
        assert_eq!(state.register(OperationId(7)), Ok(()), "consumed once");
        for id in 0..(MAX_EARLY_CANCELS as u64 + 10) {
            state.cancel(OperationId(1000 + id));
        }
        assert_eq!(state.early.len(), MAX_EARLY_CANCELS);
        assert!(!state.early.contains(&OperationId(1000)), "oldest evicted");
    }

    #[test]
    fn cancel_all_blocks_pending_and_reports_the_active_operation() {
        let mut state = OperationState::default();
        state.register(OperationId(1)).unwrap();
        state.activate(OperationId(1)).unwrap();
        state.register(OperationId(2)).unwrap();
        assert_eq!(state.cancel_all(), Some(OperationId(1)));
        assert_eq!(state.activate(OperationId(2)), Err(OperationError::Closed));
    }

    #[test]
    fn active_operation_deactivates_before_its_lock_guard_is_released() {
        // A guard that records what the state looked like when it was dropped.
        struct Probe<'a>(
            &'a Mutex<OperationState>,
            &'a Mutex<Option<Option<OperationId>>>,
        );
        impl Drop for Probe<'_> {
            fn drop(&mut self) {
                *self.1.lock().unwrap() = Some(lock_state(self.0).active);
            }
        }
        let state = Mutex::new(OperationState::default());
        let seen = Mutex::new(None);
        let mut guard = OperationGuard::register(&state, OperationId(1), "c", "x").unwrap();
        drop(guard.activate(Probe(&state, &seen)).unwrap());
        assert_eq!(
            *seen.lock().unwrap(),
            Some(None),
            "still active when unlocked"
        );
    }

    #[test]
    fn cancel_requests_are_recorded_for_the_active_operation_only() {
        let state = Mutex::new(OperationState::default());
        let mut guard = OperationGuard::register(&state, OperationId(1), "c", "x").unwrap();
        let active = guard.activate(()).unwrap();
        assert!(!active.cancel_requested());
        assert_eq!(
            lock_state(&state).cancel(OperationId(1)),
            CancelAction::Active
        );
        assert!(active.cancel_requested());
        drop(active);
        drop(guard);
        assert!(lock_state(&state).cancel_requested.is_empty());
    }

    #[test]
    fn closed_connection_refuses_new_and_pending_operations() {
        let state = Mutex::new(OperationState::default());
        let mut pending =
            OperationGuard::register(&state, OperationId(1), "q.not_connected", "q.cancelled")
                .unwrap();
        assert_eq!(lock_state(&state).cancel_all(), None);
        assert_eq!(
            OperationGuard::register(&state, OperationId(2), "q.not_connected", "q.cancelled")
                .err(),
            Some(OperationError::Closed)
        );
        let error = pending
            .activate(())
            .err()
            .expect("closed connection refuses activate");
        assert_eq!(error.code.as_deref(), Some("q.not_connected"));
        assert_eq!(error.message, "connection closed");
    }

    #[test]
    fn cancel_all_marks_the_active_operation_cancel_requested() {
        let state = Mutex::new(OperationState::default());
        let mut guard = OperationGuard::register(&state, OperationId(1), "c", "x").unwrap();
        let active = guard.activate(()).unwrap();
        assert_eq!(lock_state(&state).cancel_all(), Some(OperationId(1)));
        assert!(active.cancel_requested());
    }

    #[test]
    fn cancelling_a_finished_id_is_not_remembered() {
        let mut state = OperationState::default();
        state.register(OperationId(5)).unwrap();
        state.activate(OperationId(5)).unwrap();
        state.finish(OperationId(5));
        assert_eq!(state.cancel(OperationId(5)), CancelAction::Ignored);
        assert!(state.early.is_empty());
        assert_eq!(state.register(OperationId(5)), Ok(()));
        // An older id that never registered may still be on its way: kept.
        assert_eq!(state.cancel(OperationId(3)), CancelAction::Ignored);
        assert_eq!(
            state.register(OperationId(3)),
            Err(OperationError::Cancelled)
        );
        // A newer id is remembered as a real early cancel.
        assert_eq!(state.cancel(OperationId(6)), CancelAction::Ignored);
        assert_eq!(
            state.register(OperationId(6)),
            Err(OperationError::Cancelled)
        );
    }

    #[test]
    fn cancel_before_start_is_the_areas_cancelled_code_not_a_failure() {
        let state = Mutex::new(OperationState::default());
        // Pending when the cancel arrives: `activate` reports the area code.
        let mut pending =
            OperationGuard::register(&state, OperationId(1), "q.closed", "q.cancelled").unwrap();
        assert_eq!(
            lock_state(&state).cancel(OperationId(1)),
            CancelAction::Pending
        );
        let error = pending.activate(()).err().expect("cancelled while pending");
        assert_eq!(error.code.as_deref(), Some("q.cancelled"));
        assert_eq!(error.message, "operation cancelled");

        // Remembered early cancel: `register` itself reports the cancel.
        assert_eq!(
            lock_state(&state).cancel(OperationId(9)),
            CancelAction::Ignored
        );
        let error = OperationGuard::register(&state, OperationId(9), "q.closed", "q.cancelled")
            .err()
            .expect("early cancel");
        assert_eq!(error, OperationError::Cancelled);
    }
}
