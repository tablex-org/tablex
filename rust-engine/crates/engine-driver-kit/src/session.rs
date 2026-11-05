//! Connection and page registries (all keyed by `u64`, O(1) lookup).

use crate::backend::{BackendError, DriverBackend, Page};
use crate::operation::{lock_state, CancelAction, OperationError, OperationGuard, OperationState};
use engine_driver_api::{DriverError, OperationId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A connection plus its cancel bookkeeping.
pub(crate) struct ConnectionEntry<B: DriverBackend> {
    pub(crate) connection: B::Connection,
    canceller: B::Canceller,
    pub(crate) operations: Mutex<OperationState>,
}

impl<B: DriverBackend> ConnectionEntry<B> {
    pub(crate) fn new(connection: B::Connection) -> Result<Self, BackendError> {
        let canceller = B::canceller(&connection)?;
        Ok(Self {
            connection,
            canceller,
            operations: Mutex::new(OperationState::default()),
        })
    }

    /// Registers `operation_id`; failures carry `failed_code`, except a closed
    /// connection (`closed_code`) and an operation the host cancelled before it
    /// registered (`cancelled_code`).
    pub(crate) fn register_operation(
        &self,
        operation_id: OperationId,
        failed_code: &'static str,
        closed_code: &'static str,
        cancelled_code: &'static str,
    ) -> Result<OperationGuard<'_>, DriverError> {
        OperationGuard::register(&self.operations, operation_id, closed_code, cancelled_code)
            .map_err(|error| {
                let code = match error {
                    OperationError::Closed => closed_code,
                    OperationError::Cancelled => cancelled_code,
                    OperationError::Rejected(_) => failed_code,
                };
                DriverError {
                    code: code.to_owned(),
                    message: error.message().to_owned(),
                    retryable: false,
                }
            })
    }

    /// Two-phase cancel. Phase 1 decides under the state lock (this is the
    /// active-operation check: only the operation that is active *now* is
    /// interrupted); phase 2 calls the backend's canceller with no lock held, so
    /// a slow network cancel blocks nobody. Returns whether the operation was
    /// known (active or pending).
    pub(crate) fn cancel_operation(&self, operation_id: OperationId) -> Result<bool, BackendError> {
        let action = lock_state(&self.operations).cancel(operation_id);
        match action {
            CancelAction::Active => {
                B::cancel(&self.canceller, operation_id)?;
                Ok(true)
            }
            CancelAction::Pending => Ok(true),
            CancelAction::Ignored => Ok(false),
        }
    }

    /// Disconnect/shutdown: stop the active operation and keep pending ones
    /// from starting. Backend cancel errors are ignored (the connection is going
    /// away anyway).
    pub(crate) fn cancel_all(&self) {
        let active = lock_state(&self.operations).cancel_all();
        if let Some(operation_id) = active {
            let _ = B::cancel(&self.canceller, operation_id);
        }
    }
}

pub(crate) struct Session<B: DriverBackend> {
    pub(crate) connections: HashMap<u64, Arc<ConnectionEntry<B>>>,
    pub(crate) pages: HashMap<u64, Arc<Page<B::PageContext>>>,
    next_connection_id: u64,
    next_page_id: u64,
}

impl<B: DriverBackend> Session<B> {
    pub(crate) fn new() -> Self {
        Self {
            connections: HashMap::new(),
            pages: HashMap::new(),
            next_connection_id: 1,
            next_page_id: 1,
        }
    }

    pub(crate) fn insert_connection(&mut self, entry: Arc<ConnectionEntry<B>>) -> u64 {
        let id = self.next_connection_id;
        self.next_connection_id += 1;
        self.connections.insert(id, entry);
        id
    }

    pub(crate) fn next_page_id(&mut self) -> u64 {
        let id = self.next_page_id;
        self.next_page_id += 1;
        id
    }

    /// Removes every page of `connection_id` and returns them (ascending page
    /// id) so the caller can drop the buffers after releasing the session lock.
    pub(crate) fn remove_connection_pages(
        &mut self,
        connection_id: u64,
    ) -> Vec<Arc<Page<B::PageContext>>> {
        let mut removed = Vec::new();
        self.pages.retain(|_, page| {
            let keep = page.connection_id != connection_id;
            if !keep {
                removed.push(Arc::clone(page));
            }
            keep
        });
        removed.sort_unstable_by_key(|page| page.page_id);
        removed
    }
}
