//! The session a Process List reads and acts through.
//!
//! **A session of its own**, opened from the connection's profile exactly as the Users & Privileges tab and the structure apply open
//! theirs (`secondary_session_spec`: the same credentials, TLS and SSH tunnel) and never a SQL tab's: a SQL tab may hold a manual
//! transaction, and a `KILL` or a `SELECT` joined into it would be part of it (and wait for its locks). It also keeps working when
//! every editor session is busy or blocked, which is when a Process List is wanted most.
//!
//! The tab keeps that one session for as long as it lives, so an auto refresh every 2 seconds is a query, not a login; it opens
//! when the first read or action needs it, goes after a failure (the next one connects again) and with the tab. Everything runs
//! off the UI thread, one request at a time.

use std::sync::{Arc, Mutex};

use super::super::accounts::DriverCatalog;
use super::super::structure_edit::apply::SessionSpec;
use crate::sql_account::catalog::{CatalogSession, ReadError, TextRows};

/// What a Process List asks of its session.
pub(in crate::app) trait ProcessSession: Send {
    /// A read, as text.
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError>;
    /// An action: runs, and drops what it answers.
    fn run(&mut self, sql: &str) -> Result<(), ReadError>;
}

impl ProcessSession for DriverCatalog {
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError> {
        CatalogSession::rows(self, sql)
    }

    fn run(&mut self, sql: &str) -> Result<(), ReadError> {
        DriverCatalog::run(self, sql)
    }
}

/// Test double: makes the session a tab asks for.
#[cfg(test)]
pub(in crate::app) type InjectedSession = dyn Fn() -> Box<dyn ProcessSession> + Send + Sync;

/// How to open the session, when the tab has none.
pub(in crate::app) enum SessionSource {
    Driver(Box<SessionSpec>),
    #[cfg(test)]
    Injected(Arc<InjectedSession>),
}

impl SessionSource {
    fn open(self) -> Result<Box<dyn ProcessSession>, ReadError> {
        match self {
            Self::Driver(spec) => Ok(Box::new(DriverCatalog::open(*spec)?)),
            #[cfg(test)]
            Self::Injected(make) => Ok(make()),
        }
    }
}

/// The tab's session, once it has one.
pub(in crate::app) type Slot = Arc<Mutex<Option<Box<dyn ProcessSession>>>>;

/// Runs `work` on the tab's session (opened from `source` if there is none), blocking: call it off the UI thread. A failed request
/// drops the session, so the next one starts from a new connection.
pub(in crate::app) fn with_session<T>(
    slot: &Slot,
    source: Result<SessionSource, String>,
    work: impl FnOnce(&mut dyn ProcessSession) -> Result<T, ReadError>,
) -> Result<T, ReadError> {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(source.map_err(ReadError::Failed)?.open()?);
    }
    let session = guard.as_mut().expect("opened above");
    let result = work(&mut **session);
    if result.is_err() {
        *guard = None;
    }
    result
}

/// Lets go of the session without waiting for the server (a disconnect is a network round trip): on a worker thread, unless a request
/// is using it, in which case that request's thread is the last to hold it and lets go.
pub(in crate::app) fn release(slot: &Slot) {
    let taken = slot.try_lock().ok().and_then(|mut guard| guard.take());
    if taken.is_some() {
        std::thread::spawn(move || drop(taken));
    }
}
